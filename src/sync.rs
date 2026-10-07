//! Sync with a private repository on GitHub.com or GitLab.com: the workspace's TOML tree
//! (collections, environments, globals) goes up and comes down through their REST APIs, so
//! no git is needed. Secret values, history, cookies and OAuth tokens stay on this machine
//! unless Settings ticks them (`Share`); those go along encrypted with a passphrase, in
//! `.apitool/` in the repository and never in the workspace folder. UI state never goes.
//!
//! Each side is compared with the last sync by git blob hashes, without downloading
//! anything: what changed on one side only follows it, and what changed on both is a
//! conflict the user settles for all of them at once (the repository's or this machine's).
//! What goes along encrypted is compared record by record the same way (a variable set,
//! a history entry, a cookie, a token), so two machines' sends and logins merge.
//! Files in the repository that aren't the tree (a README) are left alone.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use reqwest::{Method, Url};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::KeyValue;
use crate::store::{HistoryEntry, Synced, Workspace};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Provider {
    #[default]
    GitHub,
    GitLab,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::GitHub => "GitHub.com",
            Provider::GitLab => "GitLab.com",
        }
    }

    fn api(self) -> &'static str {
        match self {
            Provider::GitHub => "https://api.github.com",
            Provider::GitLab => "https://gitlab.com/api/v4",
        }
    }
}

/// Where to sync, as Settings keeps it; the token is kept sealed, apart.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Remote {
    pub provider: Provider,
    /// `owner/name` (GitLab: `group/subgroup/name`); a pasted URL works too.
    pub repo: String,
    pub branch: String,
    pub share: Share,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            provider: Provider::GitHub,
            repo: String::new(),
            branch: "main".into(),
            share: Share::default(),
        }
    }
}

/// What goes along besides the tree, each only when ticked.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct Share {
    pub secrets: bool,
    pub history: bool,
    pub cookies: bool,
    pub tokens: bool,
}

impl Share {
    pub fn any(self) -> bool {
        self.secrets || self.history || self.cookies || self.tokens
    }
}

/// Record key → its value, for what goes along encrypted.
type Records = BTreeMap<String, Value>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// Keyed by environment ("" for globals): a variable's value isn't merged on its own.
    Secrets,
    /// Keyed by content: entries never change, so two machines' only add up.
    History,
    /// Keyed by domain, path and name.
    Cookies,
    /// Keyed as `auth` keys them.
    Tokens,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Secrets, Kind::History, Kind::Cookies, Kind::Tokens];

    /// Its file in the repository.
    fn path(self) -> &'static str {
        match self {
            Kind::Secrets => ".apitool/secrets.enc",
            Kind::History => ".apitool/history.enc",
            Kind::Cookies => ".apitool/cookies.enc",
            Kind::Tokens => ".apitool/oauth-tokens.enc",
        }
    }

    fn on(self, share: Share) -> bool {
        match self {
            Kind::Secrets => share.secrets,
            Kind::History => share.history,
            Kind::Cookies => share.cookies,
            Kind::Tokens => share.tokens,
        }
    }

    fn read(self, ws: &Workspace) -> Result<Records, String> {
        let json = |e: serde_json::Error| e.to_string();
        let mut out = Records::new();
        match self {
            Kind::Secrets => {
                for name in std::iter::once(String::new()).chain(ws.env_names()) {
                    let env = Some(name.as_str()).filter(|n| !n.is_empty());
                    let (_, secrets) = ws.load_env(env)?;
                    if !secrets.is_empty() {
                        out.insert(name, serde_json::to_value(secrets).map_err(json)?);
                    }
                }
            }
            Kind::History => {
                for entry in ws.load_history() {
                    let value = serde_json::to_value(entry).map_err(json)?;
                    out.insert(blob_hash(&value.to_string()), value);
                }
            }
            Kind::Cookies => {
                let jar: Vec<Value> = serde_json::from_str(&ws.load_cookies()).unwrap_or_default();
                for cookie in jar {
                    let raw = cookie["raw_cookie"].as_str().unwrap_or_default();
                    let name = raw.split(['=', ';']).next().unwrap_or_default().trim();
                    let key = format!("{}\t{}\t{name}", cookie["domain"], cookie["path"]);
                    out.insert(key, cookie);
                }
            }
            Kind::Tokens => {
                out = serde_json::from_str(&ws.load_tokens()).unwrap_or_default();
            }
        }
        Ok(out)
    }

    fn write(self, ws: &Workspace, records: &Records) -> Result<(), String> {
        let json = |e: serde_json::Error| e.to_string();
        match self {
            Kind::Secrets => {
                // Only into environments this machine has; a sync's tree brings new ones first.
                for name in std::iter::once(String::new()).chain(ws.env_names()) {
                    let env = Some(name.as_str()).filter(|n| !n.is_empty());
                    let (shared, secrets) = ws.load_env(env)?;
                    let wanted: Vec<KeyValue> = match records.get(&name) {
                        Some(v) => serde_json::from_value(v.clone()).map_err(json)?,
                        None => Vec::new(),
                    };
                    if wanted != secrets {
                        ws.save_env(env, &shared, &wanted)?;
                    }
                }
                Ok(())
            }
            Kind::History => {
                let mut entries = (records.values())
                    .map(|v| serde_json::from_value::<HistoryEntry>(v.clone()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(json)?;
                entries.sort_by_key(|e| e.at);
                ws.replace_history(&entries)
            }
            Kind::Cookies => {
                let jar: Vec<&Value> = records.values().collect();
                ws.save_cookies(&serde_json::to_string(&jar).map_err(json)?)
            }
            Kind::Tokens => ws.save_tokens(&serde_json::to_string(records).map_err(json)?),
        }
    }

    /// Takes these records from there (none: deleted there) into what's here now.
    fn apply(self, ws: &Workspace, pulled: &[(String, Option<Value>)]) -> Result<(), String> {
        let mut now = self.read(ws)?;
        for (key, value) in pulled {
            match value {
                Some(value) => now.insert(key.clone(), value.clone()),
                None => now.remove(key),
            };
        }
        self.write(ws, &now)
    }
}

fn record_hashes(records: &Records) -> Files {
    (records.iter())
        .map(|(key, value)| (key.clone(), blob_hash(&value.to_string())))
        .collect()
}

/// How a conflict on an environment's secret values is listed.
fn secrets_of(env: &str) -> String {
    let name = match env {
        "" => crate::i18n::t("Globals").to_owned(),
        env => env.to_owned(),
    };
    crate::i18n::tf("Secret values of {}", &[&name])
}

/// Marks what the passphrase encrypted.
const ENCRYPTED: &str = "apitool-encrypted-v1:";
/// PBKDF2-HMAC-SHA256 rounds, as OWASP advises.
const ROUNDS: u32 = 600_000;

/// The key a passphrase makes, the same on every machine; the repository salts it. Kept
/// for the process, as making it takes a noticeable part of a second.
fn passphrase_key(passphrase: &str, repo: &str) -> [u8; 32] {
    static LAST: Mutex<Option<(String, [u8; 32])>> = Mutex::new(None);
    let salt = format!("apitool sync {repo}");
    let id = format!("{salt}\n{passphrase}");
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((was, key)) = &*last
        && *was == id
    {
        return *key;
    }
    let mut key = [0u8; 32];
    let rounds = std::num::NonZeroU32::new(ROUNDS).expect("not zero");
    let algorithm = ring::pbkdf2::PBKDF2_HMAC_SHA256;
    ring::pbkdf2::derive(
        algorithm,
        rounds,
        salt.as_bytes(),
        passphrase.as_bytes(),
        &mut key,
    );
    *last = Some((id, key));
    key
}

fn cipher(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("a 32-byte key"))
}

fn encrypt(key: &[u8; 32], plain: &str) -> Result<String, String> {
    use base64::Engine as _;
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let mut sealed = plain.as_bytes().to_vec();
    let nonce_once = Nonce::assume_unique_for_key(nonce);
    (cipher(key).seal_in_place_append_tag(nonce_once, Aad::empty(), &mut sealed))
        .map_err(|_| "encrypting failed".to_owned())?;
    let blob = [&nonce[..], &sealed].concat();
    let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
    Ok(format!("{ENCRYPTED}{b64}\n"))
}

fn decrypt(key: &[u8; 32], text: &str, path: &str) -> Result<String, String> {
    use base64::Engine as _;
    let refused = || crate::i18n::tf("The passphrase doesn't open {} in the repository", &[&path]);
    let blob = (text.trim().strip_prefix(ENCRYPTED))
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .filter(|blob| blob.len() > NONCE_LEN)
        .ok_or_else(refused)?;
    let (nonce, sealed) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| refused())?;
    let mut sealed = sealed.to_vec();
    let plain =
        (cipher(key).open_in_place(nonce, Aad::empty(), &mut sealed)).map_err(|_| refused())?;
    String::from_utf8(plain.to_vec()).map_err(|e| e.to_string())
}

impl Remote {
    /// The repository's path, from `owner/name` or its web address.
    pub fn path(&self) -> String {
        let repo = self.repo.trim();
        let repo =
            (repo.split_once("://").map_or(repo, |(_, rest)| rest)).trim_start_matches("www.");
        let host = ["github.com/", "gitlab.com/"];
        let repo = host
            .iter()
            .fold(repo, |r, h| r.strip_prefix(h).unwrap_or(r));
        let repo = repo.trim_matches('/');
        repo.strip_suffix(".git").unwrap_or(repo).to_owned()
    }

    pub fn is_set(&self) -> bool {
        self.path().contains('/') && !self.branch.trim().is_empty()
    }

    /// The provider's page for a new token, filled in with what sync needs: signing in
    /// there is the login. An OAuth sign-in would need an app registered with each.
    pub fn token_page(&self) -> String {
        match self.provider {
            Provider::GitHub => {
                let path = self.path();
                let owner = path.split('/').next().unwrap_or_default();
                format!(
                    "https://github.com/settings/personal-access-tokens/new?name=apitool+sync&contents=write&target_name={owner}"
                )
            }
            Provider::GitLab => {
                "https://gitlab.com/-/user_settings/personal_access_tokens?name=apitool+sync&scopes=api".into()
            }
        }
    }
}

/// How the user settled the files both sides changed.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Resolution {
    UseRemote,
    KeepLocal,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Synced {
        pulled: usize,
        pushed: usize,
    },
    /// Changed on both sides; nothing was touched.
    Conflicts(Vec<String>),
}

/// Path (with '/', from the workspace folder) → git blob hash.
type Files = BTreeMap<String, String>;

/// How the repository's tree was at the last sync, which both sides then had.
#[derive(Serialize, Deserialize, Default)]
struct Base {
    files: Files,
    /// What went along encrypted, by its file's path.
    #[serde(default)]
    carried: BTreeMap<String, Seen>,
}

/// One kind at the last sync: its file's hash in the repository, and each record's.
#[derive(Serialize, Deserialize, Default, Clone)]
struct Seen {
    file: String,
    records: Files,
}

/// One kind going along in this sync.
struct Carried {
    kind: Kind,
    /// Record hashes there.
    there: Files,
    /// Downloaded only when the file changed there, the only time anything is taken.
    values: Records,
    pull: Vec<String>,
    conflicts: Vec<String>,
}

/// The hash git gives a file with this text.
pub fn blob_hash(text: &str) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(format!("blob {}\0", text.len()).as_bytes());
    ctx.update(text.as_bytes());
    let digest = ctx.finish();
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A file of the tree, as `export` writes it; anything else in the repository isn't
/// apitool's, and nothing may climb out of the workspace folder.
fn in_tree(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let plain = parts
        .iter()
        .all(|p| !p.is_empty() && *p != "." && *p != "..");
    let toml = path.ends_with(".toml") && !path.contains('\\');
    plain
        && toml
        && match parts[..] {
            ["globals.toml"] => true,
            ["environments", file] => !file.ends_with(".secret.toml"),
            ["collections", _, ..] => true,
            _ => false,
        }
}

/// What to pull, push and ask about, from each file's hash here, there and at the last
/// sync (none: it wasn't there).
fn decide(local: &Files, remote: &Files, base: &Files) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut pull, mut push, mut conflicts) = (Vec::new(), Vec::new(), Vec::new());
    let paths: BTreeSet<&String> = local
        .keys()
        .chain(remote.keys())
        .chain(base.keys())
        .collect();
    for path in paths {
        let (l, r, b) = (local.get(path), remote.get(path), base.get(path));
        if l == r {
            continue;
        }
        match (l == b, r == b) {
            (true, _) => pull.push(path.clone()),
            (false, true) => push.push(path.clone()),
            (false, false) => conflicts.push(path.clone()),
        }
    }
    (pull, push, conflicts)
}

/// Pulls what changed there, pushes what changed here; `resolution` settles what changed
/// on both. `http` should be the requests' client, for its proxy. `passphrase` encrypts
/// what `remote.share` ticks.
pub async fn sync(
    http: &reqwest::Client,
    ws: &Workspace,
    remote: &Remote,
    token: &str,
    passphrase: &str,
    resolution: Option<Resolution>,
) -> Result<Outcome, String> {
    let root = remote.provider.api();
    sync_at(http, ws, remote, token, passphrase, resolution, root).await
}

async fn sync_at(
    http: &reqwest::Client,
    ws: &Workspace,
    remote: &Remote,
    token: &str,
    passphrase: &str,
    resolution: Option<Resolution>,
    root: &str,
) -> Result<Outcome, String> {
    if ws.sync()? == Synced::Conflict {
        return Err(crate::i18n::t(
            "The workspace folder's files and apitool both changed: settle that first",
        )
        .into());
    }
    let kinds: Vec<Kind> = (Kind::ALL.into_iter())
        .filter(|k| k.on(remote.share))
        .collect();
    if !kinds.is_empty() && passphrase.is_empty() {
        return Err(crate::i18n::t("Give the passphrase in Settings > Sync first").into());
    }
    let api = Api {
        http,
        remote,
        repo: remote.path(),
        token,
        root,
    };
    let head = api.head().await?;
    let there = match &head {
        Some(head) => api.tree(head).await?,
        None => Files::new(),
    };
    let tree_there: Files = (there.iter())
        .filter(|(path, _)| in_tree(path))
        .map(|(path, hash)| (path.clone(), hash.clone()))
        .collect();
    let files = |ws: &Workspace| -> Result<BTreeMap<String, String>, String> {
        Ok(ws.tree_files()?.into_iter().collect())
    };
    let hashes = |files: &BTreeMap<String, String>| -> Files {
        (files.iter())
            .map(|(path, text)| (path.clone(), blob_hash(text)))
            .collect()
    };
    let here = hashes(&files(ws)?);
    let base: Base = (ws.remote_base())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let (mut pull, _, conflicts) = decide(&here, &tree_there, &base.files);

    // Read and opened before anything here changes: a wrong passphrase changes nothing.
    let key = match kinds.is_empty() {
        true => [0; 32],
        false => passphrase_key(passphrase, &api.repo),
    };
    let mut carried = Vec::new();
    for kind in kinds {
        let seen = base.carried.get(kind.path()).cloned().unwrap_or_default();
        let (theirs, values) = match there.get(kind.path()) {
            None => (Files::new(), Records::new()),
            // Unchanged there: its records are the ones seen at the last sync.
            Some(hash) if *hash == seen.file => (seen.records.clone(), Records::new()),
            Some(hash) => {
                let text = decrypt(&key, &api.blob(hash).await?, kind.path())?;
                let values: Records =
                    (serde_json::from_str(&text)).map_err(|e| format!("{}: {e}", kind.path()))?;
                (record_hashes(&values), values)
            }
        };
        let mine = record_hashes(&kind.read(ws)?);
        let (pull, _, conflicts) = decide(&mine, &theirs, &seen.records);
        carried.push(Carried {
            kind,
            there: theirs,
            values,
            pull,
            conflicts,
        });
    }
    // Secret values are asked about like files. History can't conflict, and cookies and
    // tokens changed on both keep this machine's: either side's works.
    let asked: Vec<String> = (conflicts.iter().cloned())
        .chain(
            (carried.iter().filter(|c| c.kind == Kind::Secrets))
                .flat_map(|c| c.conflicts.iter().map(|e| secrets_of(e))),
        )
        .collect();
    match (asked.is_empty(), resolution) {
        (true, _) | (false, Some(Resolution::KeepLocal)) => {}
        (false, Some(Resolution::UseRemote)) => {
            pull.extend(conflicts);
            for c in carried.iter_mut().filter(|c| c.kind == Kind::Secrets) {
                let both = std::mem::take(&mut c.conflicts);
                c.pull.extend(both);
            }
        }
        (false, None) => return Ok(Outcome::Conflicts(asked)),
    }
    for path in &pull {
        let file = path
            .split('/')
            .fold(ws.root.clone(), |p, part| p.join(part));
        let failed = |e: std::io::Error| format!("{}: {e}", file.display());
        match tree_there.get(path) {
            Some(hash) => {
                let text = api.blob(hash).await?;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir).map_err(failed)?;
                }
                std::fs::write(&file, text).map_err(failed)?;
            }
            None => match std::fs::remove_file(&file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(failed(e)),
                _ => remove_empty_dirs(&file, &ws.root),
            },
        }
    }
    // Read in like a git pull, and written back in apitool's own layout.
    if !pull.is_empty() {
        ws.sync()?;
    }
    let mut pulled = pull.len();
    for c in carried.iter().filter(|c| !c.pull.is_empty()) {
        let taken: Vec<(String, Option<Value>)> = (c.pull.iter())
            .map(|key| (key.clone(), c.values.get(key).cloned()))
            .collect();
        c.kind.apply(ws, &taken)?;
        pulled += 1;
    }
    // Whatever differs now is this machine's to push: its own changes, the conflicts it
    // keeps, and a pulled file apitool writes differently.
    let texts = files(ws)?;
    let here = hashes(&texts);
    let mut changes: Vec<(String, Option<String>)> = (here.keys().chain(tree_there.keys()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| here.get(*path) != tree_there.get(*path))
        .map(|path| (path.clone(), texts.get(path).cloned()))
        .collect();
    let mut seen = BTreeMap::new();
    for c in &carried {
        let path = c.kind.path();
        // Read again: what was taken, and whatever this machine did meanwhile.
        let mine = c.kind.read(ws)?;
        let records = record_hashes(&mine);
        let file = match (records == c.there, mine.is_empty()) {
            (true, _) => there.get(path).cloned().unwrap_or_default(),
            (false, true) => {
                changes.push((path.to_owned(), None));
                String::new()
            }
            (false, false) => {
                let json = serde_json::to_string(&mine).map_err(|e| e.to_string())?;
                let text = encrypt(&key, &json)?;
                let file = blob_hash(&text);
                changes.push((path.to_owned(), Some(text)));
                file
            }
        };
        seen.insert(path.to_owned(), Seen { file, records });
    }
    if !changes.is_empty() {
        api.commit(head.as_ref(), &there, &changes).await?;
    }
    let base = Base {
        files: here,
        carried: seen,
    };
    ws.set_remote_base(&serde_json::to_string(&base).map_err(|e| e.to_string())?)?;
    Ok(Outcome::Synced {
        pulled,
        pushed: changes.len(),
    })
}

/// A folder emptied by a pull goes too, as git leaves none behind.
fn remove_empty_dirs(file: &std::path::Path, root: &std::path::Path) {
    for dir in file.ancestors().skip(1).take_while(|d| *d != root) {
        if std::fs::remove_dir(dir).is_err() {
            return;
        }
    }
}

#[derive(Clone)]
struct Head {
    commit: String,
    /// GitHub: the commit's tree, which a new tree is built on.
    tree: String,
}

struct Api<'a> {
    http: &'a reqwest::Client,
    remote: &'a Remote,
    repo: String,
    token: &'a str,
    root: &'a str,
}

const MESSAGE: &str = "Sync from apitool";

impl Api<'_> {
    fn github(&self) -> bool {
        self.remote.provider == Provider::GitHub
    }

    /// The API address with these path segments, each escaped (a request's name may hold
    /// spaces; a GitLab project's path is one segment, slashes and all).
    fn url(&self, segments: &[&str]) -> Result<Url, String> {
        let mut url = Url::parse(self.root).map_err(|e| e.to_string())?;
        url.path_segments_mut()
            .map_err(|_| "not an API address".to_owned())?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    /// The repository's own segments: `repos/owner/name`, or `projects/group%2Fname`.
    fn at(&self, rest: &[&str]) -> Result<Url, String> {
        let mut segments: Vec<&str> = match self.github() {
            true => ["repos"].into_iter().chain(self.repo.split('/')).collect(),
            false => vec!["projects", &self.repo],
        };
        segments.extend(rest);
        self.url(&segments)
    }

    /// Status, body and GitLab's next page.
    async fn call(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> Result<(u16, String, Option<String>), String> {
        let mut req = self.http.request(method, url);
        req = match self.github() {
            true => (req.bearer_auth(self.token))
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28"),
            false => req.header("private-token", self.token),
        };
        if let Some(body) = body {
            req = (req.header("content-type", "application/json")).body(body.to_string());
        }
        let resp = (req.send().await).map_err(|e| crate::http::error_chain(&e))?;
        let status = resp.status().as_u16();
        let next = (resp.headers().get("x-next-page"))
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        let text = resp
            .text()
            .await
            .map_err(|e| crate::http::error_chain(&e))?;
        if status == 401 {
            let refused = crate::i18n::t(
                "{} refused the token: check it, and that it may read and write the repository",
            );
            return Err(crate::i18n::fill(refused, &[&self.remote.provider.name()]));
        }
        Ok((status, text, next))
    }

    /// What went wrong, in the API's words.
    fn failed(&self, status: u16, body: &str) -> String {
        let said = serde_json::from_str::<Value>(body).ok().and_then(|v| {
            let m = v.get("message").or_else(|| v.get("error"))?;
            Some(m.as_str().map_or_else(|| m.to_string(), str::to_owned))
        });
        let what = said.unwrap_or_else(|| crate::mcp::cut(body, 200).to_owned());
        format!("{} {status}: {what}", self.remote.provider.name())
    }

    fn json(&self, body: &str) -> Result<Value, String> {
        serde_json::from_str(body).map_err(|e| format!("{}: {e}", self.remote.provider.name()))
    }

    /// The branch's last commit; none for a repository without any yet.
    async fn head(&self) -> Result<Option<Head>, String> {
        let branch = self.remote.branch.trim();
        let (missing, project) = (
            crate::i18n::tf("{} has no branch {}", &[&self.repo, &branch]),
            crate::i18n::tf(
                "No repository {} on {}, or the token can't see it",
                &[&self.repo, &self.remote.provider.name()],
            ),
        );
        if self.github() {
            let (status, body, _) = self
                .call(Method::GET, self.at(&["commits", branch])?, None)
                .await?;
            return match status {
                200 => {
                    let v = self.json(&body)?;
                    Ok(Some(Head {
                        commit: v["sha"].as_str().unwrap_or_default().to_owned(),
                        tree: v["commit"]["tree"]["sha"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    }))
                }
                409 => Ok(None),
                404 => Err(project),
                422 => Err(missing),
                _ => Err(self.failed(status, &body)),
            };
        }
        let url = self.at(&["repository", "branches", branch])?;
        let (status, body, _) = self.call(Method::GET, url, None).await?;
        match status {
            200 => {
                let v = self.json(&body)?;
                let commit = v["commit"]["id"].as_str().unwrap_or_default().to_owned();
                Ok(Some(Head {
                    commit,
                    tree: String::new(),
                }))
            }
            // An empty project has no branches; a wrong path or token, no project.
            404 => {
                let (status, body, _) = self.call(Method::GET, self.at(&[])?, None).await?;
                match status {
                    200 if self.json(&body)?["empty_repo"] == true => Ok(None),
                    200 => Err(missing),
                    _ => Err(project),
                }
            }
            _ => Err(self.failed(status, &body)),
        }
    }

    /// The tree files there, by hash.
    async fn tree(&self, head: &Head) -> Result<Files, String> {
        let mut files = Files::new();
        let mut keep = |entries: &Value, hash: &str| {
            for e in entries.as_array().into_iter().flatten() {
                let path = e["path"].as_str().unwrap_or_default();
                let carried = Kind::ALL.iter().any(|k| k.path() == path);
                if e["type"] == "blob" && (in_tree(path) || carried) {
                    files.insert(
                        path.to_owned(),
                        e[hash].as_str().unwrap_or_default().to_owned(),
                    );
                }
            }
        };
        if self.github() {
            let mut url = self.at(&["git", "trees", &head.tree])?;
            url.query_pairs_mut().append_pair("recursive", "1");
            let (status, body, _) = self.call(Method::GET, url, None).await?;
            if status != 200 {
                return Err(self.failed(status, &body));
            }
            let v = self.json(&body)?;
            if v["truncated"] == true {
                return Err(format!("{}: too many files to list", self.repo));
            }
            keep(&v["tree"], "sha");
            return Ok(files);
        }
        let mut page = Some("1".to_owned());
        while let Some(n) = page {
            let mut url = self.at(&["repository", "tree"])?;
            (url.query_pairs_mut())
                .append_pair("ref", self.remote.branch.trim())
                .append_pair("recursive", "true")
                .append_pair("per_page", "100")
                .append_pair("page", &n);
            let (status, body, next) = self.call(Method::GET, url, None).await?;
            if status != 200 {
                return Err(self.failed(status, &body));
            }
            keep(&self.json(&body)?, "id");
            page = next;
        }
        Ok(files)
    }

    async fn blob(&self, hash: &str) -> Result<String, String> {
        let utf8 = |bytes: Vec<u8>| String::from_utf8(bytes).map_err(|e| e.to_string());
        if self.github() {
            let url = self.at(&["git", "blobs", hash])?;
            let (status, body, _) = self.call(Method::GET, url, None).await?;
            if status != 200 {
                return Err(self.failed(status, &body));
            }
            use base64::Engine as _;
            let b64: String = (self.json(&body)?["content"].as_str().unwrap_or_default())
                .split_whitespace()
                .collect();
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64);
            return utf8(bytes.map_err(|e| e.to_string())?);
        }
        let url = self.at(&["repository", "blobs", hash, "raw"])?;
        let (status, body, _) = self.call(Method::GET, url, None).await?;
        match status {
            200 => Ok(body),
            _ => Err(self.failed(status, &body)),
        }
    }

    /// One commit with every change: (path, new text; none deletes it).
    async fn commit(
        &self,
        head: Option<&Head>,
        there: &Files,
        changes: &[(String, Option<String>)],
    ) -> Result<(), String> {
        let moved = crate::i18n::t("The repository changed while this synced: sync again");
        if !self.github() {
            // GitLab can't be told which commit this builds on: look again just before.
            // ponytail: a push landing between the look and the commit is overwritten.
            if let Some(head) = head
                && self.head().await?.map(|h| h.commit) != Some(head.commit.clone())
            {
                return Err(moved.into());
            }
            let actions: Vec<Value> = (changes.iter())
                .map(|(path, text)| {
                    let action = match (text, there.contains_key(path)) {
                        (None, _) => "delete",
                        (Some(_), true) => "update",
                        (Some(_), false) => "create",
                    };
                    json!({ "action": action, "file_path": path, "content": text })
                })
                .collect();
            let body = json!({
                "branch": self.remote.branch.trim(),
                "commit_message": MESSAGE,
                "actions": actions,
            });
            let url = self.at(&["repository", "commits"])?;
            let (status, body, _) = self.call(Method::POST, url, Some(body)).await?;
            return match status {
                200 | 201 => Ok(()),
                _ => Err(self.failed(status, &body)),
            };
        }
        let mut changes = changes;
        let head = match head {
            Some(head) => head.clone(),
            // An empty repository has no tree to build on: its first file goes through
            // the contents API, which makes the first commit.
            None => {
                let Some(((path, Some(text)), rest)) = changes.split_first() else {
                    return Ok(());
                };
                use base64::Engine as _;
                let content = base64::engine::general_purpose::STANDARD.encode(text);
                let body = json!({ "message": MESSAGE, "content": content, "branch": self.remote.branch.trim() });
                let segments: Vec<&str> = ["contents"].into_iter().chain(path.split('/')).collect();
                let (status, body, _) = self
                    .call(Method::PUT, self.at(&segments)?, Some(body))
                    .await?;
                if status != 201 {
                    return Err(self.failed(status, &body));
                }
                changes = rest;
                let v = self.json(&body)?;
                let head = Head {
                    commit: v["commit"]["sha"].as_str().unwrap_or_default().to_owned(),
                    tree: v["commit"]["tree"]["sha"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                };
                if changes.is_empty() {
                    return Ok(());
                }
                head
            }
        };
        let entries: Vec<Value> = (changes.iter())
            .map(|(path, text)| match text {
                Some(text) => {
                    json!({ "path": path, "mode": "100644", "type": "blob", "content": text })
                }
                None => json!({ "path": path, "mode": "100644", "type": "blob", "sha": null }),
            })
            .collect();
        let tree = json!({ "base_tree": head.tree, "tree": entries });
        let url = self.at(&["git", "trees"])?;
        let (status, body, _) = self.call(Method::POST, url, Some(tree)).await?;
        if status != 201 {
            return Err(self.failed(status, &body));
        }
        let tree = self.json(&body)?["sha"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let commit = json!({ "message": MESSAGE, "tree": tree, "parents": [head.commit] });
        let url = self.at(&["git", "commits"])?;
        let (status, body, _) = self.call(Method::POST, url, Some(commit)).await?;
        if status != 201 {
            return Err(self.failed(status, &body));
        }
        let commit = self.json(&body)?["sha"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        // Not forced: a push made meanwhile makes this fail instead of being overwritten.
        let branch: Vec<&str> = ["git", "refs", "heads"]
            .into_iter()
            .chain(self.remote.branch.trim().split('/'))
            .collect();
        let update = json!({ "sha": commit, "force": false });
        let (status, body, _) = self
            .call(Method::PATCH, self.at(&branch)?, Some(update))
            .await?;
        match status {
            200 => Ok(()),
            409 | 422 => Err(moved.into()),
            _ => Err(self.failed(status, &body)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::model::{KeyValue, Request};

    /// A git repository in memory, behind GitHub's or GitLab's REST API.
    #[derive(Default)]
    struct Fake {
        blobs: HashMap<String, String>,
        /// Tree hash → its files (path → blob hash).
        trees: HashMap<String, Files>,
        /// Commit hash → its tree.
        commits: HashMap<String, String>,
        head: Option<String>,
        made: u32,
        /// The next branch update finds someone else's push first.
        raced: bool,
    }

    impl Fake {
        fn id(&mut self) -> String {
            self.made += 1;
            format!("{:040x}", self.made)
        }

        fn files(&self) -> Files {
            let tree = self.head.as_ref().map(|c| &self.commits[c]);
            tree.map(|t| self.trees[t].clone()).unwrap_or_default()
        }

        /// A commit of `files` on the head.
        fn commit(&mut self, files: Files) -> (String, String) {
            let (tree, commit) = (self.id(), self.id());
            self.trees.insert(tree.clone(), files);
            self.commits.insert(commit.clone(), tree.clone());
            self.head = Some(commit.clone());
            (commit, tree)
        }

        fn blob(&mut self, text: &str) -> String {
            let hash = blob_hash(text);
            self.blobs.insert(hash.clone(), text.to_owned());
            hash
        }

        fn text(&self, path: &str) -> Option<&String> {
            self.files().get(path).map(|h| &self.blobs[h])
        }

        fn github(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
            let path = path.strip_prefix("/repos/o/r/").unwrap_or_default();
            let parts: Vec<&str> = path.split('/').collect();
            match (method, &parts[..]) {
                ("GET", ["commits", "main"]) => match &self.head {
                    Some(c) => {
                        let tree = &self.commits[c];
                        (
                            200,
                            json!({ "sha": c, "commit": { "tree": { "sha": tree } } }),
                        )
                    }
                    None => (409, json!({ "message": "Git Repository is empty." })),
                },
                ("GET", ["git", "trees", tree]) => {
                    let entries: Vec<Value> = (self.trees[*tree].iter())
                        .map(|(p, h)| json!({ "path": p, "type": "blob", "sha": h }))
                        .collect();
                    (
                        200,
                        json!({ "sha": tree, "truncated": false, "tree": entries }),
                    )
                }
                ("GET", ["git", "blobs", hash]) => {
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&self.blobs[*hash]);
                    // GitHub wraps it at 60 columns.
                    let wrapped = b64
                        .as_bytes()
                        .chunks(60)
                        .map(|c| std::str::from_utf8(c).unwrap());
                    (
                        200,
                        json!({ "content": wrapped.collect::<Vec<_>>().join("\n"), "encoding": "base64" }),
                    )
                }
                ("PUT", ["contents", file @ ..]) if self.head.is_none() => {
                    use base64::Engine as _;
                    let text = base64::engine::general_purpose::STANDARD
                        .decode(body["content"].as_str().unwrap())
                        .unwrap();
                    let hash = self.blob(&String::from_utf8(text).unwrap());
                    let (commit, tree) = self.commit(Files::from([(file.join("/"), hash)]));
                    (
                        201,
                        json!({ "commit": { "sha": commit, "tree": { "sha": tree } } }),
                    )
                }
                ("POST", ["git", "trees"]) => {
                    let mut files = self.trees[body["base_tree"].as_str().unwrap()].clone();
                    for e in body["tree"].as_array().unwrap() {
                        let path = e["path"].as_str().unwrap().to_owned();
                        match e["content"].as_str() {
                            Some(text) => files.insert(path, self.blob(text)),
                            None => files.remove(&path),
                        };
                    }
                    let tree = self.id();
                    self.trees.insert(tree.clone(), files);
                    (201, json!({ "sha": tree }))
                }
                ("POST", ["git", "commits"]) => {
                    let commit = self.id();
                    let tree = body["tree"].as_str().unwrap().to_owned();
                    self.commits.insert(commit.clone(), tree);
                    let parent = body["parents"][0].as_str().unwrap().to_owned();
                    self.commits.insert(format!("{commit}^"), parent);
                    (201, json!({ "sha": commit }))
                }
                ("PATCH", ["git", "refs", "heads", "main"]) => {
                    let commit = body["sha"].as_str().unwrap();
                    let parent = &self.commits[&format!("{commit}^")];
                    if std::mem::take(&mut self.raced) || Some(parent) != self.head.as_ref() {
                        return (422, json!({ "message": "Update is not a fast forward" }));
                    }
                    self.head = Some(commit.to_owned());
                    (200, json!({}))
                }
                _ => (404, json!({ "message": "Not Found" })),
            }
        }

        fn gitlab(
            &mut self,
            method: &str,
            target: &str,
            body: &Value,
        ) -> (u16, Value, Option<String>) {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let path = path.strip_prefix("/api/v4/projects/o%2Fr").unwrap_or("?");
            let param = |name: &str| {
                (query.split('&').filter_map(|kv| kv.split_once('=')))
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_owned())
            };
            match (method, path) {
                ("GET", "") => (200, json!({ "empty_repo": self.head.is_none() }), None),
                ("GET", "/repository/branches/main") => match &self.head {
                    Some(c) => (200, json!({ "commit": { "id": c } }), None),
                    None => (404, json!({ "message": "404 Branch Not Found" }), None),
                },
                ("GET", "/repository/tree") => {
                    // Two to a page, to go through the pages.
                    let page: usize = param("page").unwrap().parse().unwrap();
                    let files: Vec<Value> = (self.files().into_iter())
                        .map(|(p, h)| json!({ "path": p, "type": "blob", "id": h }))
                        .collect();
                    let next = (page * 2 < files.len()).then(|| (page + 1).to_string());
                    let shown: Vec<Value> =
                        files.into_iter().skip((page - 1) * 2).take(2).collect();
                    (200, Value::Array(shown), next)
                }
                (_, p) if p.starts_with("/repository/blobs/") && p.ends_with("/raw") => {
                    let hash = &p["/repository/blobs/".len()..p.len() - "/raw".len()];
                    // Raw: `serve` writes a string reply as it is.
                    (200, json!(self.blobs[hash]), None)
                }
                ("POST", "/repository/commits") => {
                    let mut files = self.files();
                    for a in body["actions"].as_array().unwrap() {
                        let path = a["file_path"].as_str().unwrap().to_owned();
                        let exists = files.contains_key(&path);
                        match (a["action"].as_str().unwrap(), exists) {
                            ("delete", true) => files.remove(&path),
                            ("update", true) | ("create", false) => {
                                files.insert(path, self.blob(a["content"].as_str().unwrap()))
                            }
                            (action, _) => {
                                let msg = format!("A file with this name {action}: {path}");
                                return (400, json!({ "message": msg }), None);
                            }
                        };
                    }
                    self.commit(files);
                    (201, json!({}), None)
                }
                _ => (404, json!({ "message": "404 Not Found" }), None),
            }
        }
    }

    /// Serves `fake` as `provider`'s API; returns its root.
    fn serve(fake: Arc<Mutex<Fake>>, provider: Provider) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
                let end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    let n = conn.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                };
                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let length: usize = (head.lines())
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                while buf.len() < end + 4 + length {
                    let n = conn.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let raw = String::from_utf8_lossy(&buf[..end]).into_owned();
                let mut first = raw.split(' ');
                let (method, target) = (first.next().unwrap(), first.next().unwrap());
                let body = serde_json::from_slice(&buf[end + 4..]).unwrap_or(Value::Null);
                let mut fake = fake.lock().unwrap();
                let (status, reply, next) = match provider {
                    _ if !(head.contains("authorization: bearer t0k")
                        || head.contains("private-token: t0k")) =>
                    {
                        (401, json!({ "message": "Bad credentials" }), None)
                    }
                    Provider::GitHub => {
                        let path = target.split('?').next().unwrap().replace("%20", " ");
                        let (status, reply) = fake.github(method, &path, &body);
                        (status, reply, None)
                    }
                    Provider::GitLab => fake.gitlab(method, &target.replace("%20", " "), &body),
                };
                let reply = match reply {
                    Value::String(raw) => raw,
                    reply => reply.to_string(),
                };
                let next = next
                    .map(|n| format!("x-next-page: {n}\r\n"))
                    .unwrap_or_default();
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{next}content-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        match provider {
            Provider::GitHub => format!("http://{addr}"),
            Provider::GitLab => format!("http://{addr}/api/v4"),
        }
    }

    fn workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("apitool-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Workspace::open(dir).unwrap()
    }

    fn put(ws: &Workspace, name: &str, url: &str) {
        let req = Request {
            method: "GET".into(),
            url: url.into(),
            ..Default::default()
        };
        ws.save_request(&ws.request_path(name).unwrap(), &req)
            .unwrap();
    }

    fn url(ws: &Workspace, name: &str) -> String {
        ws.load_request(&ws.request_path(name).unwrap())
            .unwrap()
            .url
    }

    /// Two machines keep one workspace through the repository: what one changes reaches
    /// the other, changes to different requests merge, the same request changed on both
    /// is asked about, and secrets never leave.
    fn two_machines_share_a_workspace(provider: Provider) {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let root = serve(fake.clone(), provider);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let http = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let remote = Remote {
            provider,
            repo: "https://github.com/o/r.git".into(),
            ..Default::default()
        };
        let name = format!("{provider:?}");
        let (a, b) = (
            workspace(&format!("{name}-a")),
            workspace(&format!("{name}-b")),
        );
        let sync = |ws: &Workspace, resolution| {
            let r = rt.block_on(sync_at(&http, ws, &remote, "t0k", "", resolution, &root));
            r.unwrap()
        };
        let synced = |pulled, pushed| Outcome::Synced { pulled, pushed };

        put(&a, "users/list", "http://h.test/users");
        let secret = [KeyValue::new("token", "s3cret")];
        a.save_env(Some("dev"), &[KeyValue::new("host", "h.test")], &secret)
            .unwrap();
        assert_eq!(sync(&a, None), synced(0, 2), "into an empty repository");
        assert_eq!(sync(&b, None), synced(2, 0));
        assert_eq!(url(&b, "users/list"), "http://h.test/users");
        let (shared, secrets) = b.load_env(Some("dev")).unwrap();
        assert_eq!(
            (shared.len(), secrets.len()),
            (1, 0),
            "the secret stayed home"
        );
        assert!(
            fake.lock()
                .unwrap()
                .blobs
                .values()
                .all(|t| !t.contains("s3cret"))
        );
        assert_eq!(sync(&a, None), synced(0, 0), "nothing new");

        // Different requests: both get both.
        put(&a, "users/list", "http://h.test/users?page=1");
        put(&b, "users/new", "http://h.test/new");
        assert_eq!(sync(&a, None), synced(0, 1));
        assert_eq!(sync(&b, None), synced(1, 1));
        assert_eq!(sync(&a, None), synced(1, 0));
        assert_eq!(url(&b, "users/list"), "http://h.test/users?page=1");
        assert_eq!(url(&a, "users/new"), "http://h.test/new");

        // The same one: asked about, nothing touched until settled.
        put(&a, "users/list", "http://h.test/a");
        put(&b, "users/list", "http://h.test/b");
        assert_eq!(sync(&a, None), synced(0, 1));
        let asked = Outcome::Conflicts(vec!["collections/users/list.toml".into()]);
        assert_eq!(sync(&b, None), asked);
        assert_eq!(url(&b, "users/list"), "http://h.test/b");
        assert_eq!(sync(&b, Some(Resolution::UseRemote)), synced(1, 0));
        assert_eq!(url(&b, "users/list"), "http://h.test/a");
        put(&a, "users/list", "http://h.test/a2");
        put(&b, "users/list", "http://h.test/b2");
        assert_eq!(sync(&a, None), synced(0, 1));
        assert_eq!(sync(&b, Some(Resolution::KeepLocal)), synced(0, 1));
        assert_eq!(sync(&a, None), synced(1, 0));
        assert_eq!(url(&a, "users/list"), "http://h.test/b2");

        // A deletion travels, and its emptied folder goes with it rather than staying
        // behind as an empty folder in the other machine's tree.
        a.delete(&a.request_path("users/new").unwrap()).unwrap();
        a.delete(&a.request_path("users/list").unwrap()).unwrap();
        assert_eq!(sync(&a, None), synced(0, 2));
        assert_eq!(sync(&b, None), synced(2, 0));
        assert!(!b.exists(&b.request_path("users/new").unwrap()));
        assert!(!b.root.join("collections").join("users").exists());
        assert!(
            fake.lock()
                .unwrap()
                .text("collections/users/new.toml")
                .is_none()
        );
    }

    #[test]
    fn two_machines_share_a_workspace_on_github() {
        two_machines_share_a_workspace(Provider::GitHub);
    }

    #[test]
    fn two_machines_share_a_workspace_on_gitlab() {
        two_machines_share_a_workspace(Provider::GitLab);
    }

    /// Secret values, history, cookies and OAuth tokens stay home unless ticked. Ticked, they
    /// go encrypted and reach the other machine, where they merge: sends add up, a cleared
    /// history clears there too, secret values changed on both are asked about, tokens
    /// changed on both keep each machine's, and a wrong passphrase changes nothing.
    fn what_else_goes_along(provider: Provider) {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let root = serve(fake.clone(), provider);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let http = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let mut remote = Remote {
            provider,
            repo: "o/r".into(),
            ..Default::default()
        };
        let name = format!("{provider:?}-carry");
        let [a, b, c] = ["a", "b", "c"].map(|m| workspace(&format!("{name}-{m}")));
        let sync = |ws: &Workspace, remote: &Remote, passphrase: &str, resolution| {
            let sync = sync_at(&http, ws, remote, "t0k", passphrase, resolution, &root);
            rt.block_on(sync)
        };
        let synced = |pulled, pushed| Ok(Outcome::Synced { pulled, pushed });
        let leaked = || {
            let fake = fake.lock().unwrap();
            let marks = ["s3cret", "sent-a", "c00kie", "t0ken-a"];
            (fake.blobs.values()).any(|t| marks.iter().any(|m| t.contains(m)))
        };
        let sent = |url: &str| {
            let req = Request {
                url: url.into(),
                ..Default::default()
            };
            HistoryEntry::new("users/list".into(), 200, 5, req)
        };
        let urls = |ws: &Workspace| -> BTreeSet<String> {
            (ws.load_history().into_iter())
                .map(|e| e.request.url)
                .collect()
        };
        let shared = [KeyValue::new("host", "h.test")];
        let secret = |ws: &Workspace| ws.load_env(Some("dev")).unwrap().1;
        let token = |value: &str| {
            format!(r#"{{"grant":{{"token":"{value}","expires":4102444800,"refresh":null}}}}"#)
        };

        a.save_env(Some("dev"), &shared, &[KeyValue::new("token", "s3cret")])
            .unwrap();
        a.append_history(&sent("http://h.test/sent-a")).unwrap();
        let jar = crate::cookies::Jar::default();
        let site = Url::parse("http://h.test/").unwrap();
        jar.set(&site, "sid", "c00kie").unwrap();
        a.save_cookies(&jar.to_json().unwrap()).unwrap();
        a.save_tokens(&token("t0ken-a")).unwrap();
        assert_eq!(sync(&a, &remote, "", None), synced(0, 1), "the tree alone");
        assert!(!leaked());

        remote.share = Share {
            secrets: true,
            history: true,
            cookies: true,
            tokens: true,
        };
        let refused = sync(&a, &remote, "", None).unwrap_err();
        assert!(refused.contains("passphrase"), "{refused}");
        assert_eq!(sync(&a, &remote, "pass", None), synced(0, 4));
        assert!(!leaked(), "encrypted");
        assert_eq!(sync(&a, &remote, "pass", None), synced(0, 0), "nothing new");

        let refused = sync(&c, &remote, "wrong", None).unwrap_err();
        assert!(refused.contains("passphrase"), "{refused}");
        assert!(c.env_names().is_empty(), "not even the tree came down");

        assert_eq!(sync(&b, &remote, "pass", None), synced(5, 0));
        assert_eq!(secret(&b), [KeyValue::new("token", "s3cret")]);
        assert_eq!(urls(&b), BTreeSet::from(["http://h.test/sent-a".into()]));
        assert!(b.load_cookies().contains("c00kie"));
        assert!(b.load_tokens().contains("t0ken-a"));

        // Sends on both add up, and clearing the history clears it there too.
        b.append_history(&sent("http://h.test/sent-b")).unwrap();
        a.append_history(&sent("http://h.test/sent-a2")).unwrap();
        for ws in [&a, &b, &a] {
            sync(ws, &remote, "pass", None).unwrap();
        }
        assert_eq!(urls(&a).len(), 3);
        assert_eq!(urls(&a), urls(&b));
        a.clear_history().unwrap();
        for ws in [&a, &b] {
            sync(ws, &remote, "pass", None).unwrap();
        }
        assert!(urls(&b).is_empty());

        // Secret values changed on both: asked about. Tokens: each keeps its own.
        a.save_env(Some("dev"), &shared, &[KeyValue::new("token", "a2")])
            .unwrap();
        b.save_env(Some("dev"), &shared, &[KeyValue::new("token", "b2")])
            .unwrap();
        a.save_tokens(&token("t0ken-a2")).unwrap();
        b.save_tokens(&token("t0ken-b2")).unwrap();
        sync(&a, &remote, "pass", None).unwrap();
        let asked = Ok(Outcome::Conflicts(vec!["Secret values of dev".into()]));
        assert_eq!(sync(&b, &remote, "pass", None), asked);
        assert_eq!(secret(&b), [KeyValue::new("token", "b2")]);
        sync(&b, &remote, "pass", Some(Resolution::UseRemote)).unwrap();
        assert_eq!(secret(&b), [KeyValue::new("token", "a2")]);
        assert!(b.load_tokens().contains("t0ken-b2"));
    }

    #[test]
    fn what_else_goes_along_on_github() {
        what_else_goes_along(Provider::GitHub);
    }

    #[test]
    fn what_else_goes_along_on_gitlab() {
        what_else_goes_along(Provider::GitLab);
    }

    /// A push that lands while this one is on its way isn't overwritten.
    #[test]
    fn a_push_made_meanwhile_is_not_overwritten() {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let root = serve(fake.clone(), Provider::GitHub);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let http = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let remote = Remote {
            repo: "o/r".into(),
            ..Default::default()
        };
        let ws = workspace("raced");
        put(&ws, "a", "http://h.test/a");
        let sync = |token: &str| rt.block_on(sync_at(&http, &ws, &remote, token, "", None, &root));
        assert!(sync("t0k").is_ok());
        put(&ws, "a", "http://h.test/a2");
        fake.lock().unwrap().raced = true;
        let err = sync("t0k").unwrap_err();
        assert!(err.contains("changed while this synced"), "{err}");
        assert_eq!(
            sync("t0k"),
            Ok(Outcome::Synced {
                pulled: 0,
                pushed: 1
            }),
            "again, it goes"
        );
        assert!(sync("wrong").unwrap_err().contains("refused the token"));
    }

    #[test]
    fn only_the_tree_comes_down() {
        for path in [
            "collections/a/b.toml",
            "environments/dev.toml",
            "globals.toml",
        ] {
            assert!(in_tree(path), "{path}");
        }
        for path in [
            "README.md",
            "environments/dev.secret.toml",
            "environments/x/dev.toml",
            "collections/../../etc.toml",
            "collections/./a.toml",
            "collections//a.toml",
            "collections/a\\..\\b.toml",
            "apitool.db",
        ] {
            assert!(!in_tree(path), "{path}");
        }
        assert_eq!(
            blob_hash("hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        let remote = |repo: &str| Remote {
            repo: repo.into(),
            ..Default::default()
        };
        assert_eq!(remote("https://github.com/o/r.git").path(), "o/r");
        assert_eq!(remote("gitlab.com/g/sub/r/").path(), "g/sub/r");
        assert!(!remote("r").is_set());
    }
}
