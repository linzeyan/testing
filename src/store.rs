//! The workspace is a directory holding `apitool.db`, one SQLite file with the requests,
//! folders, environments (secrets included), history, cookies and UI state.
//!
//! Requests and folders are keyed by their place in the tree ("users/get user"). Callers
//! name them by the path they would have as files under `collections/`, which is where
//! `export` writes them: this TOML tree, for git.
//!   collections/**/<name>.toml   one request per file, folders are directories
//!   collections/**/.folder.toml  variables, auth and scripts shared by a folder
//!   environments/<name>.toml      shared variables
//!   globals.toml                  workspace-wide shared variables
//! A directory with the tree and no database yet (a fresh clone, or a workspace from before
//! the database) is imported when opened; after that `sync` keeps the tree and the database
//! in step, either way. Secrets, history, cookies and UI state are never
//! exported; workspaces from before the database kept them in `*.secret.toml`,
//! `.history.jsonl`, `.cookies.json` and `.state.toml`, which are read in that first import.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::i18n::{t, tf};
use crate::model::{Auth, Folder, Inherited, KeyValue, Request};

const DB: &str = "apitool.db";
/// The workspace directory may be a git repo of the exported tree; the database and the
/// per-machine files of older workspaces stay out of it.
const GITIGNORE: &str = "apitool.db\napitool.db-journal\n*.secret.toml\n.state.toml\n.history.jsonl\n.cookies.json\n*.tmp\napitool.log*\n";
const SECRET_SUFFIX: &str = ".secret";
const HISTORY: &str = ".history.jsonl";
/// Starts with a dot, so it is never taken for a request.
const FOLDER: &str = ".folder.toml";
pub const MAX_HISTORY: usize = 200;
/// History is for re-sending, and RAM is tight: bigger bodies are left out.
const MAX_HISTORY_BODY: usize = 32 * 1024;

/// Rows hold JSON. `path` is the tree path; the root folder is "" and globals are the
/// environment named "".
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS folders (path TEXT PRIMARY KEY, settings TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS requests (path TEXT PRIMARY KEY, method TEXT NOT NULL, request TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS envs (name TEXT PRIMARY KEY, shared TEXT NOT NULL, secret TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS history (id INTEGER PRIMARY KEY, entry TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS responses (id INTEGER PRIMARY KEY, path TEXT NOT NULL, at INTEGER NOT NULL, status INTEGER NOT NULL, ms INTEGER NOT NULL, response TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS responses_path ON responses (path);
";
/// Past responses kept per request, newest first; older ones are deleted.
pub const MAX_RESPONSES: usize = 10;
/// A past response keeps this much of its body: they live on disk, but one is read back
/// whole when picked.
const MAX_RESPONSE_BODY: usize = 1 << 20;

/// One past response of a request, without its body.
pub struct ResponseMeta {
    pub id: i64,
    /// Unix seconds.
    pub at: u64,
    pub status: u16,
    pub ms: u64,
}

/// One request sent from the app, as it was edited at the time (variables unresolved).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// Unix seconds.
    pub at: u64,
    /// As shown in the tree, e.g. `users/get user`.
    pub path: String,
    /// 0 when there was no response.
    pub status: u16,
    pub ms: u64,
    pub request: Request,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_dropped: bool,
}

impl HistoryEntry {
    pub fn new(path: String, status: u16, ms: u64, mut request: Request) -> Self {
        // Examples are saved with the request already; repeating them per send is waste.
        request.examples.clear();
        let body_size = serde_json::to_string(&request.body).map_or(0, |b| b.len());
        let body_dropped = body_size > MAX_HISTORY_BODY;
        if body_dropped {
            request.body = crate::model::Body::None;
        }
        Self {
            at: unix_now(),
            path,
            status,
            ms,
            request,
            body_dropped,
        }
    }
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Clone)]
pub enum Node {
    Folder {
        name: String,
        path: PathBuf,
        children: Vec<Node>,
    },
    Request {
        name: String,
        path: PathBuf,
        method: String,
    },
}

impl Node {
    pub fn path(&self) -> &Path {
        match self {
            Node::Folder { path, .. } | Node::Request { path, .. } => path,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct EnvFile {
    #[serde(default)]
    vars: Vec<KeyValue>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct State {
    pub active_env: Option<String>,
    /// The active tab.
    pub open: Option<PathBuf>,
    /// Every open tab, in order.
    pub tabs: Vec<PathBuf>,
    pub network: crate::net::Network,
    /// The language the code panel shows.
    pub code_lang: String,
    pub wrap_response: bool,
    /// Response under the request. Off (side by side) unless chosen, as in bruno, insomnia
    /// and yaak: wide screens have the room, and long JSON reads better tall.
    pub stacked: bool,
    pub hide_sidebar: bool,
    /// By environment name. ponytail: a renamed or deleted environment leaves its entry
    /// behind; harmless, prune it if the list ever matters.
    pub env_colors: HashMap<String, [u8; 3]>,
    /// JSON filters that applied, newest first.
    pub recent_filters: Vec<String>,
    /// Content types last switched to Raw: their responses open that way.
    pub raw_types: Vec<String>,
    pub appearance: crate::appearance::Appearance,
    pub updates: crate::update::Updates,
    /// MCP clients may operate the window (Settings).
    pub mcp_window: bool,
    /// Where the window serves MCP while it runs, for `apitool-cli mcp --window`: its port
    /// on 127.0.0.1 (0 while it doesn't) and the token a caller must send, new each start.
    pub mcp_port: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mcp_token: String,
}

/// Clones share one connection: the GUI's mock server reads from its own thread.
#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
    db: Arc<Mutex<Connection>>,
    /// The tree files' sizes and times as `disk` last read them, and their fingerprint.
    seen: Arc<Mutex<Option<(u64, String)>>>,
}

/// `$var/apitool`, or `~/default/apitool` when `var` isn't set (or is relative, which the
/// XDG spec says to ignore). None on Windows, where everything stays beside the exe.
fn xdg(var: &str, default: &str) -> Option<PathBuf> {
    match cfg!(windows) {
        true => None,
        false => xdg_in(|name| std::env::var_os(name), var, default),
    }
}

fn xdg_in(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    var: &str,
    default: &str,
) -> Option<PathBuf> {
    let set = env(var).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = set.or_else(|| env("HOME").map(|home| Path::new(&home).join(default)))?;
    Some(base.join("apitool"))
}

/// Where the log goes on macOS and Linux (`$XDG_STATE_HOME/apitool`); None on Windows.
pub fn state_dir() -> Option<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state")
}

/// For files that may be lost (a response handed to the browser): `$XDG_CACHE_HOME/apitool`
/// on macOS and Linux, the temp folder on Windows.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").unwrap_or_else(std::env::temp_dir)
}

/// Opens the workspace and makes it the working directory, so relative paths in requests
/// (.proto, data files) keep working after a git clone on another machine. It is `dir`,
/// else `APITOOL_WORKSPACE`, else `$XDG_DATA_HOME/apitool` on macOS and Linux, else
/// `workspace/` next to the exe: on Windows the tool is portable, so it can sit in a user
/// folder on a VDI without installation.
pub fn open_workspace(dir: Option<PathBuf>) -> Result<Workspace, String> {
    let dir = (dir.or_else(|| std::env::var_os("APITOOL_WORKSPACE").map(PathBuf::from)))
        .or_else(|| xdg("XDG_DATA_HOME", ".local/share"))
        .unwrap_or_else(|| {
            let exe = std::env::current_exe().unwrap_or_default();
            exe.parent().unwrap_or(Path::new(".")).join("workspace")
        });
    // Absolute before changing directory, or a relative root would point elsewhere after.
    let dir = std::path::absolute(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let ws = Workspace::open(dir)?;
    std::env::set_current_dir(&ws.root)
        .map_err(|e| format!("cannot enter workspace {}: {e}", ws.root.display()))?;
    Ok(ws)
}

/// Requests under `scope` (a folder or a single request), in tree order.
pub fn requests_in(nodes: &[Node], scope: &Path, out: &mut Vec<PathBuf>) {
    for node in nodes {
        match node {
            Node::Folder { children, .. } => requests_in(children, scope, out),
            Node::Request { path, .. } if path.starts_with(scope) => out.push(path.clone()),
            Node::Request { .. } => {}
        }
    }
}

fn sql<T>(r: rusqlite::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("workspace database: {e}"))
}

fn connect(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    // No WAL: it needs shared memory, which network folders (a VDI's redirected home)
    // don't give. The app and `apitool-cli mcp` take turns through the busy timeout.
    sql(conn.busy_timeout(Duration::from_secs(5)))?;
    sql(conn.execute_batch(SCHEMA))?;
    Ok(conn)
}

/// A child's entry in its folder's `order`.
fn tag(path: &Path) -> String {
    match is_request(path) {
        true => path.file_stem(),
        false => path.file_name(),
    }
    .unwrap_or_default()
    .to_string_lossy()
    .into_owned()
        + if is_request(path) { "" } else { "/" }
}

/// A request's path ends in `.toml`; a folder's never does.
pub fn is_request(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "toml")
}

fn parent_of(key: &str) -> &str {
    key.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The rows of the request or folder `?1`. A folder may share its name with a request
/// next to it, so only a folder takes what's inside.
fn rows_of(request: bool) -> &'static str {
    match request {
        true => "path = ?1",
        false => "(path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')",
    }
}

/// "<name> copy", then "<name> copy 2", …
pub fn copy_name(name: &str, n: usize) -> String {
    match n {
        1 => format!("{name} copy"),
        n => format!("{name} copy {n}"),
    }
}

fn folder_json(folder: &Folder) -> Result<String, String> {
    let mut folder = folder.clone();
    folder.vars.retain(|v| !v.key.is_empty());
    match folder == Folder::default() {
        true => Ok(String::new()),
        false => to_json(&folder),
    }
}

/// Adds the folders of `key` ("a/b" → "a", "a/b") that aren't there yet.
fn ensure_folders(db: &Connection, key: &str) -> Result<(), String> {
    let mut prefix = String::new();
    for part in key.split('/').filter(|p| !p.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        sql(db.execute(
            "INSERT OR IGNORE INTO folders (path, settings) VALUES (?1, '')",
            [&prefix],
        ))?;
    }
    Ok(())
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}

impl Workspace {
    pub fn open(root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
        let path = root.join(DB);
        if !path.exists() {
            // Built under another name and renamed when complete, so an import that fails
            // is tried again next time instead of leaving an empty workspace behind.
            let new = root.join(format!("{DB}.new"));
            let _ = fs::remove_file(&new);
            let ws = Self {
                root: root.clone(),
                db: Arc::new(Mutex::new(connect(&new)?)),
                seen: Default::default(),
            };
            let imported = ws.import(&root, true).and_then(|_| {
                // In step with what was just read, so the first `sync` doesn't ask.
                ws.put(SYNCED, &ws.disk(&tree_stats(&root))?)
            });
            drop(ws); // Windows can't rename an open file.
            if let Err(e) = imported {
                let _ = fs::remove_file(&new);
                return Err(format!("import {}: {e}", root.display()));
            }
            fs::rename(&new, &path).map_err(|e| format!("create {}: {e}", path.display()))?;
        }
        let ws = Self {
            db: Arc::new(Mutex::new(connect(&path)?)),
            root,
            seen: Default::default(),
        };
        ws.ignore()?;
        ws.seal_secrets();
        ws.tidy();
        Ok(ws)
    }

    /// A request deleted outside the app (git, the file manager) leaves its past responses
    /// behind, and SQLite keeps the file at its largest size until it is vacuumed. Best
    /// effort: the other process (`apitool-cli mcp`) may hold the file.
    fn tidy(&self) {
        let db = self.db();
        let pragma = |p: &str| db.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0));
        let orphans = "DELETE FROM responses WHERE path NOT IN (SELECT path FROM requests)";
        let tidied = (db.execute(orphans, []))
            .and_then(|_| Ok((pragma("freelist_count")?, pragma("page_count")?)))
            // Rewriting the whole file pays off once a quarter of it is free.
            .and_then(|(free, all)| match free * 4 > all {
                true => db.execute_batch("VACUUM"),
                false => Ok(()),
            });
        if let Err(e) = tidied {
            log::warn!("tidying the workspace: {e}");
        }
    }

    /// Seals secrets stored plain: from before sealing, or from a system without it. A
    /// keychain that fails is reported and tried again next time; the workspace opens.
    fn seal_secrets(&self) {
        let db = self.db();
        let plain = (db.prepare("SELECT name, secret FROM envs WHERE secret NOT LIKE 'sealed:%'"))
            .and_then(|mut q| {
                let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<(String, String)>>>()
            });
        for (name, secret) in plain.unwrap_or_default() {
            match crate::vault::seal(&secret) {
                Ok(sealed) if sealed != secret => {
                    let update = "UPDATE envs SET secret = ?2 WHERE name = ?1";
                    if let Err(e) = db.execute(update, [&name, &sealed]) {
                        log::warn!("sealing secrets: {e}");
                    }
                }
                Ok(_) => {}
                Err(e) => return log::warn!("sealing secrets: {e}"),
            }
        }
    }

    /// Checked on every open, so rules added later reach existing workspaces too.
    fn ignore(&self) -> Result<(), String> {
        let gitignore = self.root.join(".gitignore");
        let current = fs::read_to_string(&gitignore).unwrap_or_default();
        let missing: String = GITIGNORE
            .lines()
            .filter(|line| !current.lines().any(|c| c.trim() == *line))
            .map(|line| format!("{line}\n"))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let sep = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        write_atomic(&gitignore, &format!("{current}{sep}{missing}"))
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// "users/get user" for a request, "users" for a folder, "" for the root: segments
    /// escaped (`escape_name`), so it goes back through `request_path` unchanged.
    pub fn key(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.collections()).unwrap_or(path);
        let rel = match is_request(path) {
            true => rel.with_extension(""),
            false => rel.to_owned(),
        };
        rel.to_string_lossy().replace('\\', "/")
    }

    fn folder_at(&self, key: &str) -> PathBuf {
        let parts = key.split('/').filter(|p| !p.is_empty());
        parts.fold(self.collections(), |path, part| path.join(part))
    }

    fn request_at(&self, key: &str) -> PathBuf {
        let (dir, name) = key.rsplit_once('/').unwrap_or(("", key));
        self.folder_at(dir).join(format!("{name}.toml"))
    }

    fn get(&self, key: &str) -> Option<String> {
        let db = self.db();
        let row = db.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0));
        row.ok()
    }

    fn put(&self, key: &str, value: &str) -> Result<(), String> {
        sql(self.db().execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            [key, value],
        ))
        .map(drop)
    }

    /// Oldest first, at most `MAX_HISTORY` (appending trims).
    pub fn load_history(&self) -> Vec<HistoryEntry> {
        let db = self.db();
        let rows = db
            .prepare("SELECT entry FROM history ORDER BY id")
            .and_then(|mut q| {
                let rows = q.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            });
        let rows = rows.unwrap_or_default();
        rows.iter()
            .filter_map(|e| serde_json::from_str(e).ok())
            .collect()
    }

    pub fn append_history(&self, entry: &HistoryEntry) -> Result<(), String> {
        let json = to_json(entry)?;
        let db = self.db();
        sql(db.execute("INSERT INTO history (entry) VALUES (?1)", [&json]))?;
        sql(db.execute(
            "DELETE FROM history WHERE id <= (SELECT max(id) FROM history) - ?1",
            [MAX_HISTORY as i64],
        ))
        .map(drop)
    }

    pub fn clear_history(&self) -> Result<(), String> {
        sql(self.db().execute("DELETE FROM history", [])).map(drop)
    }

    /// Keeps a response of the request at `path`; returns its id.
    pub fn add_response(&self, path: &Path, resp: &crate::http::Response) -> Result<i64, String> {
        let json = match resp.body.len() > MAX_RESPONSE_BODY {
            false => to_json(resp)?,
            true => to_json(&crate::http::Response {
                body: resp.body[..resp.body.floor_char_boundary(MAX_RESPONSE_BODY)].to_owned(),
                bytes: None,
                truncated: true,
                headers: resp.headers.clone(),
                sent: resp.sent.clone(),
                reason: resp.reason.clone(),
                version: resp.version.clone(),
                ..*resp
            })?,
        };
        let key = self.key(path);
        let at = unix_now() as i64;
        let ms = resp.elapsed.as_millis() as i64;
        let db = self.db();
        sql(db.execute(
            "INSERT INTO responses (path, at, status, ms, response) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![key, at, resp.status, ms, json],
        ))?;
        let id = db.last_insert_rowid();
        sql(db.execute(
            "DELETE FROM responses WHERE path = ?1 AND id NOT IN \
             (SELECT id FROM responses WHERE path = ?1 ORDER BY id DESC LIMIT ?2)",
            rusqlite::params![key, MAX_RESPONSES as i64],
        ))?;
        Ok(id)
    }

    /// Newest first.
    pub fn responses(&self, path: &Path) -> Vec<ResponseMeta> {
        let db = self.db();
        let rows = db
            .prepare("SELECT id, at, status, ms FROM responses WHERE path = ?1 ORDER BY id DESC")
            .and_then(|mut q| {
                let rows = q.query_map([self.key(path)], |r| {
                    Ok(ResponseMeta {
                        id: r.get(0)?,
                        at: r.get::<_, i64>(1)? as u64,
                        status: r.get(2)?,
                        ms: r.get::<_, i64>(3)? as u64,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            });
        rows.unwrap_or_default()
    }

    /// Each request's latest kept status, for the tree.
    pub fn last_statuses(&self) -> HashMap<PathBuf, u16> {
        let db = self.db();
        let sql = "SELECT path, status FROM responses \
                   WHERE id IN (SELECT MAX(id) FROM responses GROUP BY path)";
        let rows = db.prepare(sql).and_then(|mut q| {
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u16>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        });
        let root = self.collections();
        (rows.unwrap_or_default().into_iter())
            .map(|(key, status)| (root.join(format!("{key}.toml")), status))
            .collect()
    }

    pub fn load_response(&self, id: i64) -> Result<crate::http::Response, String> {
        let db = self.db();
        let json: String = sql(db.query_row(
            "SELECT response FROM responses WHERE id = ?1",
            [id],
            |r| r.get(0),
        ))?;
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    pub fn delete_response(&self, id: i64) -> Result<(), String> {
        let db = self.db();
        sql(db.execute("DELETE FROM responses WHERE id = ?1", [id])).map(drop)
    }

    pub fn clear_responses(&self, path: &Path) -> Result<(), String> {
        let db = self.db();
        sql(db.execute("DELETE FROM responses WHERE path = ?1", [self.key(path)])).map(drop)
    }

    /// The cookie jar as `Jar::to_json` wrote it; empty when there is none yet.
    pub fn load_cookies(&self) -> String {
        self.get("cookies").unwrap_or_default()
    }

    pub fn save_cookies(&self, json: &str) -> Result<(), String> {
        self.put("cookies", json)
    }

    /// OAuth tokens as `auth::export` wrote them; empty when there are none yet.
    pub fn load_tokens(&self) -> String {
        self.get("oauth-tokens").unwrap_or_default()
    }

    pub fn save_tokens(&self, json: &str) -> Result<(), String> {
        self.put("oauth-tokens", json)
    }

    /// Where requests and folders live, as paths (see the module docs).
    pub fn collections(&self) -> PathBuf {
        self.root.join("collections")
    }

    /// "folder/request" as people read it (runner results, the status bar); `key` is the
    /// form that goes back through `request_path`.
    pub fn display_name(&self, path: &Path) -> String {
        unescape_path(&self.key(path))
    }

    /// Whether a request (`….toml`) or folder is in the workspace.
    pub fn exists(&self, path: &Path) -> bool {
        let key = self.key(path);
        let table = match is_request(path) {
            true => "requests",
            false if key.is_empty() => return true,
            false => "folders",
        };
        let query = format!("SELECT 1 FROM {table} WHERE path = ?1");
        self.db().query_row(&query, [&key], |_| Ok(())).is_ok()
    }

    /// Enabled variables of an environment (or the globals); secret values override shared ones.
    pub fn env_vars(&self, env: Option<&str>) -> Result<HashMap<String, String>, String> {
        let (shared, secret) = self.load_env(env)?;
        Ok(shared
            .into_iter()
            .chain(secret)
            .filter(|kv| kv.enabled)
            .map(|kv| (kv.key, kv.value))
            .collect())
    }

    /// Script writes (`pm.environment.set` …) go to the secret side: like Postman's
    /// "current value" they stay on this machine. `None` values remove the key.
    pub fn apply_changes(
        &self,
        env: Option<&str>,
        changes: &HashMap<String, Option<String>>,
    ) -> Result<(), String> {
        if changes.is_empty() {
            return Ok(());
        }
        let (shared, mut secret) = self.load_env(env)?;
        for (key, value) in changes {
            secret.retain(|kv| &kv.key != key);
            if let Some(v) = value {
                secret.push(KeyValue::new(key.clone(), v.clone()));
            }
        }
        self.save_env(env, &shared, &secret)
    }

    pub fn tree(&self) -> Vec<Node> {
        let db = self.db();
        let rows = |query: &str| -> rusqlite::Result<Vec<(String, String)>> {
            let mut q = db.prepare(query)?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        };
        let folders = rows("SELECT path, settings FROM folders");
        let requests = rows("SELECT path, method FROM requests");
        drop(db);
        match (folders, requests) {
            (Ok(mut folders), Ok(requests)) => {
                let orders = (folders.iter_mut())
                    .filter_map(|(key, settings)| {
                        let f = serde_json::from_str::<Folder>(settings).ok();
                        Some((key.clone(), f?.order)).filter(|(_, o)| !o.is_empty())
                    })
                    .collect();
                folders.retain(|(key, _)| !key.is_empty());
                self.nodes("", &folders, &requests, &orders)
            }
            (Err(e), _) | (_, Err(e)) => {
                log::error!("workspace database: {e}");
                Vec::new()
            }
        }
    }

    /// The folder's own order if it has one; else (and for what it doesn't list) folders
    /// first, then requests, each by name ignoring case.
    fn nodes(
        &self,
        parent: &str,
        folders: &[(String, String)],
        requests: &[(String, String)],
        orders: &HashMap<String, Vec<String>>,
    ) -> Vec<Node> {
        let leaf = |key: &str| unescape_name(key.rsplit('/').next().unwrap_or(key));
        let mut subfolders: Vec<Node> = folders
            .iter()
            .filter(|(key, _)| parent_of(key) == parent)
            .map(|(key, _)| Node::Folder {
                name: leaf(key),
                path: self.folder_at(key),
                children: self.nodes(key, folders, requests, orders),
            })
            .collect();
        let mut here: Vec<Node> = requests
            .iter()
            .filter(|(key, _)| parent_of(key) == parent)
            .map(|(key, method)| Node::Request {
                name: leaf(key),
                path: self.request_at(key),
                method: method.clone(),
            })
            .collect();
        let name = |n: &Node| match n {
            Node::Folder { name, .. } | Node::Request { name, .. } => name.to_lowercase(),
        };
        subfolders.sort_by_key(name);
        here.sort_by_key(name);
        subfolders.extend(here);
        if let Some(order) = orders.get(parent) {
            // Stable, so the unlisted follow in the default order.
            let at = |n: &Node| order.iter().position(|t| *t == tag(n.path()));
            subfolders.sort_by_key(|n| at(n).unwrap_or(usize::MAX));
        }
        subfolders
    }

    /// Puts `path` just before (or after) `target`, first moving it into target's folder;
    /// returns its new path.
    pub fn place(&self, path: &Path, target: &Path, after: bool) -> Result<PathBuf, String> {
        let dir = target.parent().ok_or("nothing to place it next to")?;
        let new = self.move_into(path, dir)?;
        if new == target {
            return Ok(new);
        }
        let tree = self.tree();
        let siblings = match dir == self.collections() {
            true => &tree[..],
            false => crate::docs::find(&tree, dir).ok_or("the folder is gone")?,
        };
        let mut order: Vec<String> = siblings.iter().map(|n| tag(n.path())).collect();
        order.retain(|t| *t != tag(&new));
        let at = (order.iter().position(|t| *t == tag(target))).ok_or("the target is gone")?;
        order.insert(at + after as usize, tag(&new));
        let mut folder = self.load_folder(dir)?;
        folder.order = Folder::order_of(order);
        self.save_folder(dir, &folder)?;
        Ok(new)
    }

    pub fn load_request(&self, path: &Path) -> Result<Request, String> {
        let key = self.key(path);
        let json: Option<String> = sql(self
            .db()
            .query_row(
                "SELECT request FROM requests WHERE path = ?1",
                [&key],
                |r| r.get(0),
            )
            .optional())?;
        let json = json.ok_or_else(|| format!("no request \"{key}\""))?;
        let mut req: Request =
            serde_json::from_str(&json).map_err(|e| format!("request \"{key}\": {e}"))?;
        req.sync_params();
        req.inherited = self.inherited(path)?;
        Ok(req)
    }

    /// A folder without settings has the defaults.
    pub fn load_folder(&self, dir: &Path) -> Result<Folder, String> {
        let key = self.key(dir);
        let json: Option<String> = sql(self
            .db()
            .query_row(
                "SELECT settings FROM folders WHERE path = ?1",
                [&key],
                |r| r.get(0),
            )
            .optional())?;
        match json.as_deref() {
            None | Some("") => Ok(Folder::default()),
            Some(json) => serde_json::from_str(json).map_err(|e| format!("folder \"{key}\": {e}")),
        }
    }

    pub fn save_folder(&self, dir: &Path, folder: &Folder) -> Result<(), String> {
        let json = folder_json(folder)?;
        let key = self.key(dir);
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        ensure_folders(&tx, &key)?;
        sql(tx.execute(
            "INSERT OR REPLACE INTO folders (path, settings) VALUES (?1, ?2)",
            [&key, &json],
        ))?;
        sql(tx.commit())
    }

    /// The settings of every folder between `collections/` and the request at `path`.
    pub fn inherited(&self, path: &Path) -> Result<Inherited, String> {
        let root = self.collections();
        let mut dirs: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .take_while(|d| *d != root && d.starts_with(&root))
            .collect();
        dirs.reverse();
        let mut out = Inherited::default();
        for dir in dirs {
            let f = self.load_folder(dir)?;
            let name = folder_name(&root, dir);
            let vars = f
                .vars
                .into_iter()
                .filter(|v| v.enabled && !v.key.is_empty());
            out.vars.extend(vars.map(|v| (v.key, v.value)));
            if f.auth != Auth::Inherit {
                out.auth = Some((name.clone(), f.auth));
            }
            if !f.pre_request.trim().is_empty() {
                out.pre_request.push((name.clone(), f.pre_request));
            }
            if !f.tests.trim().is_empty() {
                out.tests.push((name, f.tests));
            }
        }
        Ok(out)
    }

    /// Creates the folders above it as needed.
    pub fn save_request(&self, path: &Path, req: &Request) -> Result<(), String> {
        let key = self.key(path);
        let json = to_json(req)?;
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        ensure_folders(&tx, parent_of(&key))?;
        sql(tx.execute(
            "INSERT OR REPLACE INTO requests (path, method, request) VALUES (?1, ?2, ?3)",
            [&key, &req.method, &json],
        ))?;
        sql(tx.commit())
    }

    /// `folder/name` (a `key`, or typed with plain names) → its path. Segments are escaped
    /// as needed, which also keeps `..` out.
    pub fn request_path(&self, name: &str) -> Result<PathBuf, String> {
        let name = name.trim().trim_matches('/');
        let name = name.strip_suffix(".toml").unwrap_or(name);
        let (dirs, file) = name.rsplit_once('/').unwrap_or(("", name));
        let mut path = self.collections();
        let normal = |s: &str| segment(&unescape_name(s));
        for dir in dirs.split('/').filter(|d| !d.is_empty()) {
            path.push(normal(dir)?);
        }
        Ok(path.join(format!("{}.toml", normal(file)?)))
    }

    /// Every request under `scope` (a folder or a single request), in tree order, with
    /// display names.
    pub fn load_requests_in(&self, scope: &Path) -> Result<Vec<(String, Request)>, String> {
        let mut paths = Vec::new();
        requests_in(&self.tree(), scope, &mut paths);
        if paths.is_empty() {
            return Err(tf("no requests under {}", &[&self.display_name(scope)]));
        }
        paths
            .iter()
            .map(|p| Ok((self.display_name(p), self.load_request(p)?)))
            .collect()
    }

    /// Creates `<dir>/<name>`, refusing to overwrite.
    pub fn create_request(&self, dir: &Path, name: &str) -> Result<PathBuf, String> {
        let path = request_in(dir, name)?;
        if self.exists(&path) {
            return Err(tf("\"{}\" already exists", &[&name]));
        }
        self.save_request(&path, &Request::default())?;
        Ok(path)
    }

    pub fn create_folder(&self, dir: &Path, name: &str) -> Result<PathBuf, String> {
        let path = dir.join(folder_segment(name)?);
        if self.exists(&path) {
            return Err(tf("\"{}\" already exists", &[&name]));
        }
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        ensure_folders(&tx, &self.key(&path))?;
        sql(tx.commit())?;
        Ok(path)
    }

    /// Renames a request or folder in place; returns the new path.
    pub fn rename(&self, path: &Path, name: &str) -> Result<PathBuf, String> {
        let request = is_request(path);
        let new = match request {
            true => path.with_file_name(format!("{}.toml", segment(name)?)),
            false => path.with_file_name(folder_segment(name)?),
        };
        if self.exists(&new) {
            return Err(tf("\"{}\" already exists", &[&name.trim()]));
        }
        self.relocate(path, &new)?;
        // Keep its place among its siblings.
        let dir = path.parent().unwrap_or(path);
        let mut folder = self.load_folder(dir)?;
        if let Some(t) = folder.order.iter_mut().find(|t| **t == tag(path)) {
            *t = tag(&new);
            self.save_folder(dir, &folder)?;
        }
        Ok(new)
    }

    /// Moves a request or folder into `folder` (the collections root included), keeping
    /// its name; returns the new path.
    pub fn move_into(&self, path: &Path, folder: &Path) -> Result<PathBuf, String> {
        let name = path.file_name().unwrap_or_default();
        let new = folder.join(name);
        if folder.starts_with(path) {
            return Err(t("A folder can't go inside itself").into());
        }
        if new == path {
            return Ok(new);
        }
        if self.exists(&new) {
            let name = Path::new(name)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            return Err(tf("\"{}\" already exists there", &[&name]));
        }
        self.relocate(path, &new)?;
        Ok(new)
    }

    /// Re-keys a request (or a folder and everything in it) from `path` to `new`.
    fn relocate(&self, path: &Path, new: &Path) -> Result<(), String> {
        let request = is_request(path);
        let (old_key, new_key) = (self.key(path), self.key(new));
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        let tables: &[&str] = match request {
            true => &["requests", "responses"],
            false => &["folders", "requests", "responses"],
        };
        let rows = rows_of(request);
        for table in tables {
            let update = format!(
                "UPDATE {table} SET path = ?2 || substr(path, length(?1) + 1) WHERE {rows}"
            );
            sql(tx.execute(&update, [&old_key, &new_key]))?;
        }
        sql(tx.commit())
    }

    /// The first free "<name> copy", "<name> copy 2", … next to `path`.
    fn copy_of(&self, path: &Path) -> PathBuf {
        let request = is_request(path);
        // A dot in a folder name is not an extension.
        let stem = match request {
            true => path.file_stem(),
            false => path.file_name(),
        };
        let stem = stem.unwrap_or_default().to_string_lossy();
        (1..)
            .map(|n| copy_name(&stem, n))
            .map(|name| match request {
                true => path.with_file_name(format!("{name}.toml")),
                false => path.with_file_name(name),
            })
            .find(|p| !self.exists(p))
            .expect("some name is free")
    }

    /// Copies a request or a whole folder next to itself as "<name> copy" ("copy 2", …).
    pub fn duplicate(&self, path: &Path) -> Result<PathBuf, String> {
        let request = is_request(path);
        let new = self.copy_of(path);
        let (old_key, new_key) = (self.key(path), self.key(&new));
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        let tables: &[(&str, &str)] = match request {
            true => &[("requests", "method, request")],
            false => &[("folders", "settings"), ("requests", "method, request")],
        };
        let rows = rows_of(request);
        for (table, columns) in tables {
            let copy = format!(
                "INSERT INTO {table} (path, {columns}) \
                 SELECT ?2 || substr(path, length(?1) + 1), {columns} FROM {table} WHERE {rows}"
            );
            sql(tx.execute(&copy, [&old_key, &new_key]))?;
        }
        sql(tx.commit())?;
        Ok(new)
    }

    pub fn delete(&self, path: &Path) -> Result<(), String> {
        let key = self.key(path);
        let request = is_request(path);
        let tables: &[&str] = match request {
            true => &["requests", "responses"],
            false => &["folders", "requests", "responses"],
        };
        let rows = rows_of(request);
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        for table in tables {
            let delete = format!("DELETE FROM {table} WHERE {rows}");
            sql(tx.execute(&delete, [&key]))?;
        }
        sql(tx.commit())
    }

    pub fn env_names(&self) -> Vec<String> {
        let db = self.db();
        let names = db
            .prepare("SELECT name FROM envs WHERE name != ''")
            .and_then(|mut q| {
                let rows = q.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            });
        let mut names = names.unwrap_or_default();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// Returns (shared, secret) variables; an environment that isn't there has none.
    pub fn load_env(&self, env: Option<&str>) -> Result<(Vec<KeyValue>, Vec<KeyValue>), String> {
        let name = env.unwrap_or("");
        let row: Option<(String, String)> = sql(self
            .db()
            .query_row(
                "SELECT shared, secret FROM envs WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional())?;
        let Some((shared, secret)) = row else {
            return Ok((Vec::new(), Vec::new()));
        };
        let parse = |json: &str| {
            serde_json::from_str(json).map_err(|e| format!("environment \"{name}\": {e}"))
        };
        let secret = crate::vault::open(&secret)
            .map_err(|e| format!("environment \"{name}\": its secrets can't be opened: {e}"))?;
        Ok((parse(&shared)?, parse(&secret)?))
    }

    pub fn save_env(
        &self,
        env: Option<&str>,
        shared: &[KeyValue],
        secret: &[KeyValue],
    ) -> Result<(), String> {
        if let Some(name) = env {
            valid_name(name)?;
        }
        let keep = |vars: &[KeyValue]| {
            let vars: Vec<&KeyValue> = vars.iter().filter(|v| !v.key.is_empty()).collect();
            to_json(&vars)
        };
        let secret = crate::vault::seal(&keep(secret)?)
            .map_err(|e| format!("secrets aren't saved unencrypted, and sealing failed: {e}"))?;
        sql(self.db().execute(
            "INSERT OR REPLACE INTO envs (name, shared, secret) VALUES (?1, ?2, ?3)",
            [env.unwrap_or(""), &keep(shared)?, &secret],
        ))
        .map(drop)
    }

    /// Saves a new environment, never over one: a same-named environment may hold this
    /// machine's secrets. Returns the name given ("<name> copy" when taken).
    pub fn add_env(
        &self,
        name: &str,
        shared: &[KeyValue],
        secret: &[KeyValue],
    ) -> Result<String, String> {
        let names = self.env_names();
        let name = std::iter::once(name.to_owned())
            .chain((1..).map(|n| copy_name(name, n)))
            .find(|n| !names.contains(n))
            .expect("some name is free");
        self.save_env(Some(&name), shared, secret)?;
        Ok(name)
    }

    pub fn delete_env(&self, name: &str) -> Result<(), String> {
        sql(self
            .db()
            .execute("DELETE FROM envs WHERE name = ?1", [name]))
        .map(drop)
    }

    pub fn load_state(&self) -> State {
        let state = self.get("state");
        state
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, state: &State) {
        // Losing UI state is harmless; don't bother the user about it.
        if let Ok(json) = to_json(state) {
            let _ = self.put("state", &json);
        }
    }

    /// Reads the TOML tree under `dir` (see the module docs) in place of the requests,
    /// folders and environments here: what it doesn't have is deleted, except the globals
    /// when there's no globals.toml. Secrets stay. `first` also reads what workspaces from
    /// before the database kept per machine. Returns how many requests were read.
    fn import(&self, dir: &Path, first: bool) -> Result<usize, String> {
        let mut folders = Vec::new();
        let mut requests = Vec::new();
        read_tree(&dir.join("collections"), "", &mut folders, &mut requests)?;
        let mut envs = vec![(String::new(), dir.join("globals.toml"))];
        let env_dir = dir.join("environments");
        for entry in fs::read_dir(&env_dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if is_request(&path) && !stem.ends_with(SECRET_SUFFIX) {
                envs.push((stem.to_owned(), path.clone()));
            }
        }
        let env_names: HashSet<String> = envs.iter().map(|(name, _)| name.clone()).collect();
        let mut vars = Vec::new();
        for (name, path) in envs {
            let shared = read_vars(&path)?;
            let secret = match (first, &shared) {
                (true, _) => read_vars(&path.with_file_name(format!(
                    "{}{SECRET_SUFFIX}.toml",
                    path.file_stem().unwrap_or_default().to_string_lossy()
                )))?,
                // Secrets are never exported: keep this machine's.
                (false, Some(_)) => {
                    let env = Some(name.as_str()).filter(|n| !n.is_empty());
                    Some(self.load_env(env)?.1)
                }
                (false, None) => None,
            };
            // Globals without a file have nothing to replace.
            if shared.is_some() || secret.is_some() {
                vars.push((name, shared.unwrap_or_default(), secret.unwrap_or_default()));
            }
        }
        self.put_tree(&folders, &requests)?;
        for (name, shared, secret) in vars {
            let env = Some(name.as_str()).filter(|n| !n.is_empty());
            self.save_env(env, &shared, &secret)?;
        }
        let folder_keys = folders.iter().map(|(k, _)| k.clone()).collect();
        let request_keys = requests.iter().map(|(k, _)| k.clone()).collect();
        self.keep_only(&folder_keys, &request_keys, &env_names)?;
        if first {
            self.import_local(dir)?;
        }
        Ok(requests.len())
    }

    /// Deletes the folders, requests and environments not named: they were deleted where
    /// the tree came from. The root folder and the globals always stay.
    fn keep_only(
        &self,
        folders: &HashSet<String>,
        requests: &HashSet<String>,
        envs: &HashSet<String>,
    ) -> Result<(), String> {
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        for (table, column, keep) in [
            ("folders", "path", folders),
            ("requests", "path", requests),
            ("envs", "name", envs),
        ] {
            let rows: Vec<String> = {
                let mut q = sql(tx.prepare(&format!("SELECT {column} FROM {table}")))?;
                let rows = sql(q.query_map([], |r| r.get(0)))?;
                sql(rows.collect())?
            };
            for row in rows.iter().filter(|r| !r.is_empty() && !keep.contains(*r)) {
                let delete = format!("DELETE FROM {table} WHERE {column} = ?1");
                sql(tx.execute(&delete, [row]))?;
            }
        }
        sql(tx.commit())
    }

    /// Folders (settings as JSON, "" for none) and requests, all or none.
    fn put_tree(
        &self,
        folders: &[(String, String)],
        requests: &[(String, Request)],
    ) -> Result<(), String> {
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        for (key, settings) in folders {
            ensure_folders(&tx, key)?;
            sql(tx.execute(
                "INSERT OR REPLACE INTO folders (path, settings) VALUES (?1, ?2)",
                [key, settings],
            ))?;
        }
        for (key, req) in requests {
            ensure_folders(&tx, parent_of(key))?;
            sql(tx.execute(
                "INSERT OR REPLACE INTO requests (path, method, request) VALUES (?1, ?2, ?3)",
                [key, &req.method, &to_json(req)?],
            ))?;
        }
        sql(tx.commit())
    }

    /// Adds a top-level folder `name` ("<name> copy" when taken, so nothing is replaced)
    /// holding `folders` and `requests`, keyed below it ("" is the folder itself) by
    /// segments made with `escape_name`.
    pub fn add_tree(
        &self,
        name: &str,
        folders: &[(String, Folder)],
        requests: &[(String, Request)],
    ) -> Result<PathBuf, String> {
        let dir = self.collections().join(folder_segment(name)?);
        let dir = match self.exists(&dir) {
            true => self.copy_of(&dir),
            false => dir,
        };
        let top = self.key(&dir);
        let under = |key: &str| -> Result<String, String> {
            for part in key.split('/').filter(|p| !p.is_empty()) {
                if *part != escape_name(&unescape_name(part)) {
                    return Err(format!("\"{part}\" isn't an escaped name"));
                }
            }
            Ok(match key.is_empty() {
                true => top.clone(),
                false => format!("{top}/{key}"),
            })
        };
        let folders: Vec<(String, String)> = folders
            .iter()
            .map(|(key, f)| Ok((under(key)?, folder_json(f)?)))
            .collect::<Result<_, String>>()?;
        let requests: Vec<(String, Request)> = requests
            .iter()
            .map(|(key, r)| Ok((under(key)?, r.clone())))
            .collect::<Result<_, String>>()?;
        self.put_tree(&folders, &requests)?;
        Ok(dir)
    }

    /// History, cookies and UI state of a workspace from before the database.
    fn import_local(&self, dir: &Path) -> Result<(), String> {
        let read = |name: &str| match fs::read_to_string(dir.join(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read {name}: {e}")),
            Ok(text) => Ok(Some(text)),
        };
        if let Some(text) = read(HISTORY)? {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            for line in &lines[lines.len().saturating_sub(MAX_HISTORY)..] {
                sql(self
                    .db()
                    .execute("INSERT INTO history (entry) VALUES (?1)", [line]))?;
            }
        }
        if let Some(json) = read(".cookies.json")? {
            self.save_cookies(&json)?;
        }
        // A state that no longer parses is only UI state: start fresh rather than refuse.
        if let Some(state) = read(".state.toml")?.and_then(|t| toml::from_str::<State>(&t).ok()) {
            self.save_state(&state);
        }
        Ok(())
    }

    /// Brings the database and the TOML tree in the workspace folder into step, so the
    /// folder can go through git: whichever side changed since they last were in step wins,
    /// the database by writing the tree, the files (a git pull) by being read in. When both
    /// changed, nothing is touched and `resolve` takes the user's pick.
    pub fn sync(&self) -> Result<Synced, String> {
        let plan = self.plan(&self.root)?;
        let files = tree_stats(&self.root);
        let mine = fingerprint(&self.root, &plan.files);
        let disk = self.disk(&files)?;
        // Never in step before: an empty folder takes the database, a tree it doesn't
        // match has to be asked about.
        let last = self
            .get(SYNCED)
            .unwrap_or_else(|| fingerprint(&self.root, &[]));
        if mine == disk {
            // Runs on every focus change: nothing to write, not even the fingerprint again.
            return match disk == last {
                true => Ok(Synced::Same),
                false => self.put(SYNCED, &disk).map(|_| Synced::Same),
            };
        }
        let synced = if disk == last || files.is_empty() {
            // A tree gone altogether is a folder deleted or moved by mistake far more
            // often than everything deleted on purpose elsewhere: write it again.
            self.write(&self.root, &plan)?;
            Synced::Exported
        } else if mine == last {
            self.import(&self.root, false)?;
            // In the tree's own layout, so the next look finds both sides the same.
            self.export(&self.root)?;
            Synced::Imported
        } else {
            return Ok(Synced::Conflict);
        };
        self.put(SYNCED, &self.disk(&tree_stats(&self.root))?)?;
        Ok(synced)
    }

    /// After `Synced::Conflict`: the files replace the database, or the other way round.
    pub fn resolve(&self, use_files: bool) -> Result<(), String> {
        if use_files {
            self.import(&self.root, false)?;
        }
        self.export(&self.root)?;
        self.put(SYNCED, &self.disk(&tree_stats(&self.root))?)
    }

    /// The `fingerprint` of the tree files, read only when one changed size or time since
    /// the last look: `sync` runs on every focus change, and reading a thousand requests
    /// from a network folder took over a second each time.
    // ponytail: an edit that keeps the size within one time step of the file system (2 s
    // on FAT) is missed until the next change; hash the files' contents if that bites.
    fn disk(&self, files: &[(PathBuf, fs::Metadata)]) -> Result<String, String> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        for (path, meta) in files {
            (path, meta.len(), meta.modified().ok()).hash(&mut hasher);
        }
        let stamp = hasher.finish();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, known)) = &*seen
            && *at == stamp
        {
            return Ok(known.clone());
        }
        let texts: Vec<(PathBuf, String)> = (files.iter())
            .map(|(path, _)| Ok((path.clone(), read_text(path)?)))
            .collect::<Result<_, String>>()?;
        let known = fingerprint(&self.root, &texts);
        *seen = Some((stamp, known.clone()));
        Ok(known)
    }

    /// Writes the TOML tree (see the module docs) under `dir`, for git: no secrets, history,
    /// cookies or UI state. Request, folder and environment files the database no longer
    /// has are removed so the tree matches it; other files (a .proto, a data file) stay.
    /// Returns how many requests were written.
    fn export(&self, dir: &Path) -> Result<usize, String> {
        let plan = self.plan(dir)?;
        self.write(dir, &plan)?;
        Ok(plan.requests)
    }

    fn plan(&self, dir: &Path) -> Result<Plan, String> {
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        let mut folders: HashSet<PathBuf> = HashSet::new();
        let collections = dir.join("collections");
        folders.insert(collections.clone());
        let place = |key: &str| {
            let parts = key.split('/').filter(|p| !p.is_empty());
            parts.fold(collections.clone(), |path, part| path.join(part))
        };
        let (folder_rows, request_rows, env_names) = {
            let db = self.db();
            let rows = |query: &str| -> Result<Vec<(String, String)>, String> {
                let mut q = sql(db.prepare(query))?;
                let rows = sql(q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))))?;
                sql(rows.collect())
            };
            (
                rows("SELECT path, settings FROM folders")?,
                rows("SELECT path, request FROM requests")?,
                rows("SELECT name, shared FROM envs")?,
            )
        };
        for (key, settings) in &folder_rows {
            let at = place(key);
            if !settings.is_empty() {
                let folder: Folder =
                    serde_json::from_str(settings).map_err(|e| format!("folder \"{key}\": {e}"))?;
                let text = toml::to_string_pretty(&folder).map_err(|e| e.to_string())?;
                files.push((at.join(FOLDER), text));
            }
            folders.extend(
                at.ancestors()
                    .take_while(|a| a.starts_with(&collections))
                    .map(Path::to_path_buf),
            );
        }
        for (key, json) in &request_rows {
            let req: Request =
                serde_json::from_str(json).map_err(|e| format!("request \"{key}\": {e}"))?;
            let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
            let text = toml::to_string_pretty(&req).map_err(|e| e.to_string())?;
            files.push((place(parent).join(format!("{name}.toml")), text));
            folders.extend(
                place(parent)
                    .ancestors()
                    .take_while(|a| a.starts_with(&collections))
                    .map(Path::to_path_buf),
            );
        }
        let env_dir = dir.join("environments");
        for (name, shared) in &env_names {
            let vars: Vec<KeyValue> =
                serde_json::from_str(shared).map_err(|e| format!("environment \"{name}\": {e}"))?;
            let text = toml::to_string_pretty(&EnvFile { vars }).map_err(|e| e.to_string())?;
            let path = match name.is_empty() {
                true => dir.join("globals.toml"),
                false => env_dir.join(format!("{name}.toml")),
            };
            files.push((path, text));
        }
        Ok(Plan {
            files,
            folders,
            requests: request_rows.len(),
        })
    }

    fn write(&self, dir: &Path, plan: &Plan) -> Result<(), String> {
        let Plan { files, folders, .. } = plan;
        let collections = dir.join("collections");
        let env_dir = dir.join("environments");
        let wanted: HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        prune(&collections, &wanted, folders)?;
        for entry in fs::read_dir(&env_dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let secret = path
                .file_stem()
                .is_some_and(|s| s.to_string_lossy().ends_with(SECRET_SUFFIX));
            if is_request(&path) && !secret && !wanted.contains(path.as_path()) {
                fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))?;
            }
        }
        for dir in folders {
            fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        fs::create_dir_all(&env_dir).map_err(|e| format!("create {}: {e}", env_dir.display()))?;
        for (path, text) in files {
            // Unchanged files keep their time stamp, and git sees nothing to look at.
            if fs::read_to_string(path).is_ok_and(|old| old.replace("\r\n", "\n") == *text) {
                continue;
            }
            write_atomic(path, text)?;
        }
        Ok(())
    }
}

/// What `export` writes: each file with its text, and the folders, empty ones included.
struct Plan {
    files: Vec<(PathBuf, String)>,
    folders: HashSet<PathBuf>,
    requests: usize,
}

#[derive(Debug, PartialEq)]
pub enum Synced {
    Same,
    Exported,
    Imported,
    /// Both changed since they were last in step.
    Conflict,
}

/// The kv entry holding the tree's `fingerprint` when it last matched the database.
const SYNCED: &str = "synced";

/// The tree files under `root` that `export` writes and prunes, unread.
fn tree_stats(root: &Path) -> Vec<(PathBuf, fs::Metadata)> {
    // Windows hands a folder's metadata over with its listing; asking per file would cost a
    // round trip each in a network folder. Links are followed, as `is_dir` did.
    fn stat(entry: fs::DirEntry) -> Option<(PathBuf, fs::Metadata)> {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) if m.is_symlink() => fs::metadata(&path),
            m => m,
        };
        meta.ok().map(|m| (path, m))
    }
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, fs::Metadata)>) {
        for (path, meta) in fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(stat)
        {
            if meta.is_dir() {
                walk(&path, out);
            } else if is_request(&path) {
                out.push((path, meta));
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("collections"), &mut out);
    for (path, meta) in fs::read_dir(root.join("environments"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(stat)
    {
        let secret =
            (path.file_stem()).is_some_and(|s| s.to_string_lossy().ends_with(SECRET_SUFFIX));
        if is_request(&path) && !secret {
            out.push((path, meta));
        }
    }
    let globals = root.join("globals.toml");
    if let Ok(meta) = fs::metadata(&globals) {
        out.push((globals, meta));
    }
    out
}

fn read_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// One value for a set of tree files, whatever order they're listed in. Line endings don't
/// count: git may check files out with CRLF.
fn fingerprint(root: &Path, files: &[(PathBuf, String)]) -> String {
    use std::hash::{Hash, Hasher};
    let mut files: Vec<(String, String)> = (files.iter())
        .map(|(path, text)| {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel = rel.to_string_lossy().replace('\\', "/");
            (rel, text.replace("\r\n", "\n"))
        })
        .collect();
    files.sort();
    // ponytail: SipHash with fixed keys, stable within a Rust release; a new one may
    // change it, which only reads the unchanged tree back in once.
    let mut hasher = std::hash::DefaultHasher::new();
    files.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Folders and requests under `dir`, whose tree path is `key`.
fn read_tree(
    dir: &Path,
    key: &str,
    folders: &mut Vec<(String, String)>,
    requests: &mut Vec<(String, Request)>,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
        Ok(entries) => entries,
    };
    let settings = match fs::read_to_string(dir.join(FOLDER)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", dir.join(FOLDER).display())),
        Ok(text) => {
            let folder: Folder = toml::from_str(&text)
                .map_err(|e| format!("parse {}: {e}", dir.join(FOLDER).display()))?;
            to_json(&folder)?
        }
    };
    folders.push((key.to_owned(), settings));
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let child = |name: &str| match key.is_empty() {
            true => name.to_owned(),
            false => format!("{key}/{name}"),
        };
        if path.is_dir() {
            read_tree(&path, &child(&name), folders, requests)?;
        } else if let Some(stem) = name.strip_suffix(".toml") {
            let text =
                fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            let req: Request =
                toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
            requests.push((child(stem), req));
        }
    }
    Ok(())
}

/// `None` when the file isn't there.
fn read_vars(path: &Path) -> Result<Option<Vec<KeyValue>>, String> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
        Ok(text) => toml::from_str::<EnvFile>(&text)
            .map(|f| Some(f.vars))
            .map_err(|e| format!("parse {}: {e}", path.display())),
    }
}

/// Removes `.toml` files under `dir` that aren't `wanted`, then folders that end up empty
/// and aren't `folders`.
fn prune(dir: &Path, wanted: &HashSet<&Path>, folders: &HashSet<PathBuf>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
        Ok(entries) => entries,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune(&path, wanted, folders)?;
        } else if is_request(&path) && !wanted.contains(path.as_path()) {
            fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))?;
        }
    }
    let empty = fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none());
    if empty && !folders.contains(dir) {
        fs::remove_dir(dir).map_err(|e| format!("delete {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// "users/admin" as people read it.
pub fn folder_name(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    unescape_path(&rel.to_string_lossy().replace('\\', "/"))
}

/// Characters a file name can't hold on Windows or macOS, plus '%', the escape itself.
const UNSAFE: &str = r#"<>:"/\|?*%"#;

/// A request or folder name as one segment of its key and of its file name in an export:
/// what a file name can't hold is %-escaped, so any name (Postman's "/api/v1/users", "a:b")
/// keeps its exact text. Undone by `unescape_name`.
pub fn escape_name(name: &str) -> String {
    let name = name.trim();
    let last = name.chars().count().saturating_sub(1);
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        // A leading dot hides the file, a trailing one Windows drops.
        let edge_dot = c == '.' && (i == 0 || i == last);
        if UNSAFE.contains(c) || c.is_control() || edge_dot {
            for b in c.encode_utf8(&mut [0; 4]).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The name a key segment stands for, see `escape_name`. A '%' not followed by two hex
/// digits is kept, so names from before the escaping read as they were.
pub fn unescape_name(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// "a/b" tree keys (and runner names) as people read them: each segment unescaped.
pub fn unescape_path(key: &str) -> String {
    let parts: Vec<String> = key.split('/').map(unescape_name).collect();
    parts.join("/")
}

/// Where a request named `name` goes in `dir`.
pub fn request_in(dir: &Path, name: &str) -> Result<PathBuf, String> {
    Ok(dir.join(format!("{}.toml", segment(name)?)))
}

/// A name typed or imported, as a key segment.
fn segment(name: &str) -> Result<String, String> {
    match escape_name(name) {
        s if s.is_empty() => Err(t("name can't be empty").into()),
        s => Ok(s),
    }
}

/// Environment names become file names in an export, on macOS and Windows alike, so
/// reject rather than silently mangle.
fn valid_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') {
        return Err(t("name can't be empty or start/end with '.'").into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| r#"<>:"/\|?*"#.contains(*c) || c.is_control())
    {
        return Err(tf("name can't contain '{}'", &[&c]));
    }
    Ok(name)
}

/// A name from elsewhere (Postman allows any), made valid: what a file name can't hold
/// becomes '-'.
pub fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| match r#"<>:"/\|?*"#.contains(c) || c.is_control() {
            true => '-',
            false => c,
        })
        .collect();
    match name.trim().trim_matches('.').trim() {
        "" => "untitled".into(),
        name => name.into(),
    }
}

/// A folder named like a request file would be read back as one.
fn folder_segment(name: &str) -> Result<String, String> {
    let name = segment(name)?;
    match name.ends_with(".toml") {
        true => Err(t("a folder name can't end with .toml").into()),
        false => Ok(name),
    }
}

/// Write-then-rename so a crash mid-save never leaves a truncated file in the repo.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The XDG variable when it's an absolute path, else its default under home. Not on
    /// Windows, which has no XDG and where `/data` isn't absolute.
    #[test]
    #[cfg(not(windows))]
    fn xdg_directories_follow_the_spec() {
        fn dir(vars: &[(&str, &str)]) -> Option<PathBuf> {
            let env = |name: &str| (vars.iter().find(|(n, _)| *n == name)).map(|(_, v)| v.into());
            xdg_in(env, "XDG_DATA_HOME", ".local/share")
        }
        let home = ("HOME", "/home/u");
        assert_eq!(dir(&[home]), Some("/home/u/.local/share/apitool".into()));
        let set = [home, ("XDG_DATA_HOME", "/data")];
        assert_eq!(dir(&set), Some("/data/apitool".into()));
        let relative = [home, ("XDG_DATA_HOME", "data")];
        assert_eq!(dir(&relative), Some("/home/u/.local/share/apitool".into()));
        assert_eq!(dir(&[]), None);
    }

    // ponytail: teardown is best-effort (`let _ = remove_dir_all`): Windows won't delete
    // apitool.db while the test's Workspace still holds it, so a Windows run leaves its
    // dir in %TEMP%. Drop every handle before the delete if that ever matters.
    fn fresh(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("apitool-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn workspace_round_trip() {
        let root = fresh("store");
        let ws = Workspace::open(root.clone()).unwrap();

        let folder = ws.create_folder(&ws.collections(), "Users").unwrap();
        let path = ws.create_request(&folder, "Get user").unwrap();
        assert!(
            ws.create_request(&folder, "Get user").is_err(),
            "must not overwrite"
        );
        assert!(
            ws.create_request(&folder, " ").is_err(),
            "a name can't be blank"
        );

        let renamed = ws.rename(&path, "Fetch user").unwrap();
        assert!(matches!(&ws.tree()[0], Node::Folder { children, .. }
            if matches!(&children[0], Node::Request { name, .. } if name == "Fetch user")));
        assert_eq!(ws.load_request(&renamed).unwrap(), Request::default());
        assert!(!ws.exists(&path) && ws.exists(&renamed));

        // Renaming a folder carries what's inside.
        let moved = ws.rename(&folder, "People").unwrap();
        assert_eq!(
            ws.load_requests_in(&moved).unwrap()[0].0,
            "People/Fetch user"
        );

        ws.save_env(
            Some("dev"),
            &[KeyValue::new("host", "x")],
            &[KeyValue::new("token", "s3cret")],
        )
        .unwrap();
        assert_eq!(ws.env_names(), ["dev"]);
        assert_eq!(
            ws.load_env(Some("dev")).unwrap().1,
            [KeyValue::new("token", "s3cret")]
        );

        // Script writes chain into the local (secret) side, for envs and globals alike.
        let changes = HashMap::from([
            ("token".to_owned(), Some("new".to_owned())),
            ("host".to_owned(), None),
        ]);
        ws.apply_changes(Some("dev"), &changes).unwrap();
        ws.apply_changes(None, &changes).unwrap();
        for env in [Some("dev"), None] {
            assert_eq!(
                ws.env_vars(env).unwrap().get("token").map(String::as_str),
                Some("new")
            );
        }
        // Removing from the secret side never touches the shared value.
        assert_eq!(ws.env_vars(Some("dev")).unwrap()["host"], "x");
        assert!(ws.env_names() == ["dev"], "globals aren't an environment");

        // A second handle (the MCP server next to the app) sees the same data.
        let other = Workspace::open(root.clone()).unwrap();
        assert_eq!(other.env_vars(Some("dev")).unwrap()["token"], "new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_escaped_into_keys_and_come_back_unchanged() {
        for name in [
            "/api/v1/users",
            r#"a:b "c" <d> | e? * f\g"#,
            "50% off",
            ".hidden.",
            "é/ü",
        ] {
            let seg = escape_name(name);
            assert!(
                !seg.contains(['/', '\\', ':', '"', '<', '>', '|', '?', '*']),
                "{seg}"
            );
            assert!(!seg.starts_with('.') && !seg.ends_with('.'), "{seg}");
            assert_eq!(unescape_name(&seg), name);
        }
        // Names from before the escaping read as they were.
        assert_eq!(unescape_name("100%"), "100%");
        assert_eq!(unescape_name("a%zz"), "a%zz");
    }

    /// Postman names hold '/' ("/api/v0/home/feed"): it must neither split the key nor be
    /// replaced, here, in an export, or in the history that leads back to the request.
    #[test]
    fn a_name_with_slashes_survives_the_tree_files_and_history() {
        let root = fresh("slash-names");
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "v0: api").unwrap();
        let feed = ws.create_request(&api, "/api/v0/home/feed").unwrap();
        assert_eq!(feed.parent().unwrap(), api, "one level down, not four");
        assert_eq!(ws.display_name(&feed), "v0: api//api/v0/home/feed");
        let tree = ws.tree();
        let Node::Folder { name, children, .. } = &tree[0] else {
            panic!("folder")
        };
        assert_eq!(name, "v0: api");
        assert!(matches!(&children[0], Node::Request { name, .. } if name == "/api/v0/home/feed"));
        // What history keeps leads back to the same request.
        assert_eq!(ws.request_path(&ws.key(&feed)).unwrap(), feed);
        let renamed = ws.rename(&feed, "/api/v1/home/feed?x=1").unwrap();
        assert_eq!(ws.display_name(&renamed), "v0: api//api/v1/home/feed?x=1");

        let out = root.join("export");
        ws.export(&out).unwrap();
        let clone = Workspace::open(out).unwrap();
        let names: Vec<String> = (clone.load_requests_in(&clone.collections()).unwrap())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["v0: api//api/v1/home/feed?x=1"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicates_sit_next_to_the_original_and_never_overwrite() {
        let root = fresh("dup");
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "api.v2").unwrap();
        let get = ws.create_request(&api, "get").unwrap();
        let req = Request {
            url: "{{base}}/x".into(),
            ..Default::default()
        };
        ws.save_request(&get, &req).unwrap();
        ws.save_folder(
            &api,
            &Folder {
                description: "settings come along".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let copy = ws.duplicate(&get).unwrap();
        assert_eq!(ws.display_name(&copy), "api.v2/get copy");
        assert_eq!(ws.load_request(&copy).unwrap().url, "{{base}}/x");
        let again = ws.duplicate(&get).unwrap();
        assert_eq!(ws.display_name(&again), "api.v2/get copy 2");

        let folder = ws.duplicate(&api).unwrap();
        assert_eq!(folder.file_name().unwrap(), "api.v2 copy");
        assert_eq!(
            ws.load_folder(&folder).unwrap().description,
            "settings come along"
        );
        let copied = ws.load_requests_in(&folder).unwrap();
        let names: Vec<_> = copied.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "api.v2 copy/get",
                "api.v2 copy/get copy",
                "api.v2 copy/get copy 2"
            ]
        );
        // Deleting the copy leaves the original whole.
        ws.delete(&folder).unwrap();
        assert_eq!(ws.load_requests_in(&api).unwrap().len(), 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_request_and_a_folder_may_share_a_name() {
        let root = fresh("same-name");
        let ws = Workspace::open(root.clone()).unwrap();
        let folder = ws.create_folder(&ws.collections(), "users").unwrap();
        ws.create_request(&folder, "inside").unwrap();
        let request = ws.create_request(&ws.collections(), "users").unwrap();
        let copy = ws.duplicate(&request).unwrap();
        let renamed = ws.rename(&request, "people").unwrap();
        ws.delete(&renamed).unwrap();
        ws.delete(&copy).unwrap();
        let left = ws.load_requests_in(&ws.collections()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, "users/inside", "the folder keeps what's inside");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folder_settings_cascade_to_the_requests_below() {
        let root = fresh("folders");
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "api").unwrap();
        let admin = ws.create_folder(&api, "admin.v2").unwrap();
        let req = ws.create_request(&admin, "r").unwrap();
        let bearer = Auth::Bearer {
            token: "{{token}}".into(),
        };
        let outer = Folder {
            vars: vec![KeyValue::new("base", "outer"), KeyValue::new("page", "1")],
            auth: bearer.clone(),
            pre_request: "outer()".into(),
            ..Default::default()
        };
        ws.save_folder(&api, &outer).unwrap();
        let inner = Folder {
            vars: vec![KeyValue::new("base", "inner")],
            pre_request: "inner()".into(),
            ..Default::default()
        };
        ws.save_folder(&admin, &inner).unwrap();

        let got = ws.load_request(&req).unwrap().inherited;
        // The inner folder wins; one that sets no auth passes its parent's down.
        assert_eq!(got.vars["base"], "inner");
        assert_eq!(got.vars["page"], "1");
        assert_eq!(got.auth, Some(("api".into(), bearer)));
        // Outermost first, as Postman runs collection then folder scripts. A dot in a
        // folder name is not an extension.
        let pre: Vec<_> = got
            .pre_request
            .iter()
            .map(|(f, s)| (f.as_str(), s.as_str()))
            .collect();
        assert_eq!(pre, [("api", "outer()"), ("api/admin.v2", "inner()")]);
        assert!(
            matches!(&ws.tree()[0], Node::Folder { children, .. } if children.len() == 1),
            "settings are not requests"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn history_is_bounded_and_drops_huge_bodies() {
        let root = fresh("history");
        let ws = Workspace::open(root.clone()).unwrap();
        let big = Request {
            body: crate::model::Body::Text {
                text: "x".repeat(MAX_HISTORY_BODY + 1),
            },
            ..Default::default()
        };
        let entry = HistoryEntry::new("big".into(), 200, 5, big);
        assert!(entry.body_dropped && entry.request.body == crate::model::Body::None);
        for i in 0..MAX_HISTORY + 5 {
            let mut e = entry.clone();
            e.path = i.to_string();
            ws.append_history(&e).unwrap();
        }
        let loaded = ws.load_history();
        assert_eq!(loaded.len(), MAX_HISTORY);
        assert_eq!(loaded[0].path, "5", "oldest entries go first");
        ws.clear_history().unwrap();
        assert!(ws.load_history().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn past_responses_are_bounded_and_follow_their_request() {
        let root = fresh("responses");
        let ws = Workspace::open(root.clone()).unwrap();
        let path = ws.create_request(&ws.collections(), "r").unwrap();
        let resp = |status: u16, body: String| crate::http::Response {
            status,
            reason: String::new(),
            version: "HTTP/1.1".into(),
            elapsed: std::time::Duration::from_millis(7),
            headers: Vec::new(),
            body,
            truncated: false,
            sent: Default::default(),
            bytes: None,
        };
        let big = ws
            .add_response(&path, &resp(500, "é".repeat(MAX_RESPONSE_BODY)))
            .unwrap();
        // A big body keeps its start, cut on a character boundary, and says so.
        let kept = ws.load_response(big).unwrap();
        assert!(kept.truncated && kept.body.len() <= MAX_RESPONSE_BODY && kept.status == 500);
        for i in 0..MAX_RESPONSES {
            ws.add_response(&path, &resp(200 + i as u16, String::new()))
                .unwrap();
        }
        let list = ws.responses(&path);
        assert_eq!(list.len(), MAX_RESPONSES, "the oldest goes");
        assert_eq!((list[0].status, list[0].ms), (209, 7), "newest first");
        assert_eq!(
            ws.last_statuses().get(&path),
            Some(&209),
            "the latest, for the tree"
        );
        assert!(ws.load_response(big).is_err());
        // Renaming keeps them with the request; deleting takes them along.
        let renamed = ws.rename(&path, "s").unwrap();
        assert!(ws.responses(&path).is_empty());
        assert_eq!(ws.responses(&renamed).len(), MAX_RESPONSES);
        ws.delete(&renamed).unwrap();
        let again = ws.create_request(&ws.collections(), "s").unwrap();
        assert!(
            ws.responses(&again).is_empty(),
            "a new request starts fresh"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_reads_the_files_only_when_their_sizes_or_times_change() {
        let root = fresh("stat");
        let ws = Workspace::open(root.clone()).unwrap();
        let path = ws.create_request(&ws.collections(), "r").unwrap();
        let url = |url: &str| Request {
            url: url.into(),
            ..Default::default()
        };
        ws.save_request(&path, &url("http://aaaa")).unwrap();
        assert_eq!(ws.sync().unwrap(), Synced::Exported);
        let touch = |at| {
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(at).unwrap();
        };
        // Same size, time put back: proof the file isn't read when nothing says it changed.
        let at = fs::metadata(&path).unwrap().modified().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("aaaa", "bbbb")).unwrap();
        touch(at);
        assert_eq!(ws.sync().unwrap(), Synced::Same);
        // A real edit moves the time.
        touch(at + Duration::from_secs(1));
        assert_eq!(ws.sync().unwrap(), Synced::Imported);
        assert_eq!(ws.load_request(&path).unwrap().url, "http://bbbb");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reopening_drops_orphaned_responses_and_gives_the_space_back() {
        let root = fresh("tidy");
        let ws = Workspace::open(root.clone()).unwrap();
        let gone = ws.create_request(&ws.collections(), "gone").unwrap();
        let kept = ws.create_request(&ws.collections(), "kept").unwrap();
        let resp = crate::http::Response {
            status: 200,
            reason: String::new(),
            version: "HTTP/1.1".into(),
            elapsed: std::time::Duration::from_millis(7),
            headers: Vec::new(),
            body: "x".repeat(MAX_RESPONSE_BODY),
            truncated: false,
            sent: Default::default(),
            bytes: None,
        };
        for _ in 0..MAX_RESPONSES {
            ws.add_response(&gone, &resp).unwrap();
        }
        ws.add_response(&kept, &resp).unwrap();
        ws.sync().unwrap();
        // A git pull deletes the request; its responses stay with nothing leading to them.
        fs::remove_file(&gone).unwrap();
        assert_eq!(ws.sync().unwrap(), Synced::Imported);
        let size = || fs::metadata(root.join(DB)).unwrap().len();
        let before = size();
        drop(ws);
        let ws = Workspace::open(root.clone()).unwrap();
        assert!(ws.responses(&gone).is_empty());
        assert_eq!(ws.responses(&kept).len(), 1);
        assert!(size() < before / 2, "{before} -> {} bytes", size());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn moving_takes_contents_along_and_never_overwrites() {
        let root = fresh("move");
        let ws = Workspace::open(root.clone()).unwrap();
        let top = ws.collections();
        let api = ws.create_folder(&top, "api").unwrap();
        let v1 = ws.create_folder(&top, "v1").unwrap();
        let r = ws.create_request(&top, "r").unwrap();
        let req = Request {
            url: "http://x".into(),
            ..Default::default()
        };
        ws.save_request(&r, &req).unwrap();
        let r = ws.move_into(&r, &v1).unwrap();
        assert_eq!(r, v1.join("r.toml"));
        assert_eq!(ws.load_request(&r).unwrap().url, "http://x");
        // A folder moves with everything in it.
        let v1 = ws.move_into(&v1, &api).unwrap();
        assert_eq!(ws.load_request(&v1.join("r.toml")).unwrap().url, "http://x");
        assert!(ws.move_into(&api, &v1).is_err(), "not into itself");
        // A same-named request there stays as it is.
        let other = ws.create_request(&top, "r").unwrap();
        let err = ws.move_into(&other, &v1).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(ws.load_request(&v1.join("r.toml")).unwrap().url, "http://x");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dragged_order_sticks_through_renames_moves_and_export() {
        let root = fresh("order");
        let ws = Workspace::open(root.clone()).unwrap();
        let top = ws.collections();
        let names =
            |nodes: &[Node]| -> Vec<String> { nodes.iter().map(|n| tag(n.path())).collect() };
        let api = ws.create_folder(&top, "api").unwrap();
        let a = ws.create_request(&top, "a").unwrap();
        let b = ws.create_request(&top, "b").unwrap();
        // A request named like a folder is a different entry.
        ws.create_request(&top, "api").unwrap();
        assert_eq!(names(&ws.tree()), ["api/", "a", "api", "b"]);

        ws.place(&b, &a, false).unwrap();
        ws.place(&api, &b, true).unwrap();
        assert_eq!(names(&ws.tree()), ["b", "api/", "a", "api"]);
        // A rename keeps the place; a new request goes after what was arranged.
        let c = ws.rename(&a, "c").unwrap();
        ws.create_request(&top, "0 new").unwrap();
        assert_eq!(names(&ws.tree()), ["b", "api/", "c", "api", "0 new"]);

        // Into another folder, at a spot among what is there.
        let inner = ws.create_request(&api, "x").unwrap();
        let moved = ws.place(&c, &inner, false).unwrap();
        assert_eq!(moved, api.join("c.toml"));
        let Node::Folder { children, .. } = &ws.tree()[1] else {
            panic!("api")
        };
        assert_eq!(names(children), ["c", "x"]);
        // That is the default order, so nothing is stored.
        assert!(ws.load_folder(&api).unwrap().order.is_empty());
        ws.place(&moved, &inner, true).unwrap();
        let Node::Folder { children, .. } = &ws.tree()[1] else {
            panic!("api")
        };
        assert_eq!(names(children), ["x", "c"]);

        let out = root.join("export");
        ws.export(&out).unwrap();
        let clone = Workspace::open(out).unwrap();
        assert_eq!(names(&clone.tree()), ["b", "api/", "api", "0 new"]);
        let Node::Folder { children, .. } = &clone.tree()[1] else {
            panic!("api")
        };
        assert_eq!(names(children), ["x", "c"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secrets_are_sealed_in_the_database_and_old_plain_ones_on_open() {
        let root = fresh("sealed");
        let ws = Workspace::open(root.clone()).unwrap();
        let column = |ws: &Workspace, env: &str| -> String {
            let q = "SELECT secret FROM envs WHERE name = ?1";
            ws.db().query_row(q, [env], |r| r.get(0)).unwrap()
        };
        let seals = cfg!(any(windows, target_os = "macos"));
        ws.save_env(Some("dev"), &[], &[KeyValue::new("token", "s3cret")])
            .unwrap();
        assert_eq!(column(&ws, "dev").starts_with(crate::vault::PREFIX), seals);
        if seals {
            assert!(!column(&ws, "dev").contains("s3cret"));
        }
        assert_eq!(
            ws.load_env(Some("dev")).unwrap().1,
            [KeyValue::new("token", "s3cret")]
        );

        // A row from before sealing is read as it was, and sealed the next time it opens.
        let old = r#"[{"key":"pw","value":"hunter2","enabled":true}]"#;
        let insert = "INSERT INTO envs (name, shared, secret) VALUES ('old', '[]', ?1)";
        ws.db().execute(insert, [old]).unwrap();
        assert_eq!(ws.load_env(Some("old")).unwrap().1[0].value, "hunter2");
        drop(ws);
        let ws = Workspace::open(root.clone()).unwrap();
        assert_eq!(column(&ws, "old").starts_with(crate::vault::PREFIX), seals);
        assert_eq!(ws.load_env(Some("old")).unwrap().1[0].value, "hunter2");

        // What can't be opened is an error, never an empty list a save would then keep.
        let broken = format!("{}AAAA", crate::vault::PREFIX);
        ws.db()
            .execute("UPDATE envs SET secret = ?1 WHERE name = 'old'", [&broken])
            .unwrap();
        let err = ws.load_env(Some("old")).unwrap_err();
        assert!(err.contains("can't be opened"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn export_is_a_git_tree_without_secrets_that_imports_back() {
        let root = fresh("export");
        let ws = Workspace::open(root.clone()).unwrap();
        let users = ws.create_folder(&ws.collections(), "users").unwrap();
        ws.save_folder(
            &users,
            &Folder {
                vars: vec![KeyValue::new("page", "1")],
                ..Default::default()
            },
        )
        .unwrap();
        let get = ws.request_path("users/get").unwrap();
        let req = Request {
            url: "{{host}}/users".into(),
            ..Default::default()
        };
        ws.save_request(&get, &req).unwrap();
        ws.create_folder(&ws.collections(), "empty").unwrap();
        ws.save_env(
            Some("dev"),
            &[KeyValue::new("host", "h")],
            &[KeyValue::new("token", "s3cret")],
        )
        .unwrap();
        ws.save_env(None, &[KeyValue::new("g", "1")], &[]).unwrap();

        let out = root.join("export");
        // A file the requests use, and one left from a request deleted since.
        fs::create_dir_all(out.join("collections/users")).unwrap();
        fs::write(out.join("collections/users/schema.proto"), "x").unwrap();
        fs::write(out.join("collections/users/gone.toml"), "url = 'x'").unwrap();
        assert_eq!(ws.export(&out).unwrap(), 1);
        let read = |p: &str| fs::read_to_string(out.join(p)).unwrap();
        assert!(read("collections/users/get.toml").contains("{{host}}/users"));
        assert!(read("collections/users/.folder.toml").contains("page"));
        assert!(out.join("collections/empty").is_dir(), "empty folders too");
        assert!(read("environments/dev.toml").contains("host"));
        assert!(read("globals.toml").contains("g"));
        assert!(out.join("collections/users/schema.proto").exists());
        assert!(!out.join("collections/users/gone.toml").exists());
        let everything: String = ["collections/users/get.toml", "environments/dev.toml"]
            .iter()
            .map(|p| read(p))
            .collect();
        assert!(
            !everything.contains("s3cret"),
            "secrets stay on this machine"
        );

        // A fresh clone of that tree opens with the same requests.
        let clone = Workspace::open(out.clone()).unwrap();
        let names: Vec<_> = clone.load_requests_in(&clone.collections()).unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].0, "users/get");
        assert_eq!(names[0].1.inherited.vars["page"], "1");
        assert!(clone.exists(&clone.collections().join("empty")));
        assert_eq!(clone.env_vars(Some("dev")).unwrap()["host"], "h");

        // Importing a teammate's change keeps this machine's secrets.
        fs::write(
            out.join("environments/dev.toml"),
            "[[vars]]\nkey = 'host'\nvalue = 'h2'\nenabled = true\n",
        )
        .unwrap();
        // A tree without globals has nothing to say about them.
        fs::remove_file(out.join("globals.toml")).unwrap();
        ws.import(&out, false).unwrap();
        let dev = ws.env_vars(Some("dev")).unwrap();
        assert_eq!(
            (dev["host"].as_str(), dev["token"].as_str()),
            ("h2", "s3cret")
        );
        assert_eq!(ws.env_vars(None).unwrap()["g"], "1");
        let _ = fs::remove_dir_all(&root);
    }

    /// The workspace folder goes through git, so it follows the database, and a pull
    /// comes back into it, deletions included. Changes on both sides wait for the user.
    #[test]
    fn sync_carries_changes_either_way_and_asks_when_both_changed() {
        let root = fresh("sync");
        let ws = Workspace::open(root.clone()).unwrap();
        let dir = ws.create_folder(&ws.collections(), "a").unwrap();
        let r = ws.create_request(&dir, "r").unwrap();
        ws.save_env(Some("dev"), &[KeyValue::new("host", "h")], &[])
            .unwrap();
        let file = root.join("collections/a/r.toml");
        let read = || fs::read_to_string(&file).unwrap();
        let url = |url: &str| Request {
            url: url.into(),
            ..Default::default()
        };

        assert_eq!(
            ws.sync().unwrap(),
            Synced::Exported,
            "an empty folder takes it all"
        );
        assert!(root.join("environments/dev.toml").exists());
        assert_eq!(ws.sync().unwrap(), Synced::Same);
        ws.save_request(&r, &url("http://mine")).unwrap();
        assert_eq!(ws.sync().unwrap(), Synced::Exported);
        assert!(read().contains("http://mine"));

        // A pull changes the request and deletes the environment.
        fs::write(&file, "url = 'http://pulled'\n").unwrap();
        fs::remove_file(root.join("environments/dev.toml")).unwrap();
        assert_eq!(ws.sync().unwrap(), Synced::Imported);
        assert_eq!(ws.load_request(&r).unwrap().url, "http://pulled");
        assert!(ws.env_names().is_empty());
        assert_eq!(
            ws.sync().unwrap(),
            Synced::Same,
            "the pulled file in our layout"
        );

        fs::write(&file, "url = 'http://theirs'\n").unwrap();
        ws.save_request(&r, &url("http://ours")).unwrap();
        assert_eq!(ws.sync().unwrap(), Synced::Conflict);
        assert!(
            read().contains("theirs"),
            "nothing moves until the user picks"
        );
        assert_eq!(ws.load_request(&r).unwrap().url, "http://ours");
        ws.resolve(true).unwrap();
        assert_eq!(ws.load_request(&r).unwrap().url, "http://theirs");
        assert_eq!(ws.sync().unwrap(), Synced::Same);

        // The whole tree gone is written again, not read as everything deleted.
        fs::remove_dir_all(root.join("collections")).unwrap();
        assert_eq!(ws.sync().unwrap(), Synced::Exported);
        assert!(read().contains("theirs"));

        // A fresh clone, written by hand: in step on open, so nothing to ask.
        let clone = root.join("clone");
        fs::create_dir_all(clone.join("collections")).unwrap();
        fs::write(clone.join("collections/x.toml"), "url='http://x'").unwrap();
        let cloned = Workspace::open(clone.clone()).unwrap();
        assert_eq!(cloned.sync().unwrap(), Synced::Exported);
        let x = fs::read_to_string(clone.join("collections/x.toml")).unwrap();
        assert!(x.contains("url = \"http://x\""), "{x}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_from_before_the_database_opens_with_everything() {
        let root = fresh("migrate");
        let write = |p: &str, text: &str| {
            let path = root.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            "collections/api/list.toml",
            "method = 'POST'\nurl = 'http://x/list'\n",
        );
        write("collections/api/.folder.toml", "description = 'the API'\n");
        write(
            "environments/dev.toml",
            "[[vars]]\nkey = 'host'\nvalue = 'h'\nenabled = true\n",
        );
        write(
            "environments/dev.secret.toml",
            "[[vars]]\nkey = 'token'\nvalue = 's'\nenabled = true\n",
        );
        let entry = HistoryEntry::new("api/list".into(), 200, 1, Request::default());
        write(
            HISTORY,
            &format!("{}\n", serde_json::to_string(&entry).unwrap()),
        );
        write(".cookies.json", "[]");
        write(".state.toml", "active_env = 'dev'\n");

        let ws = Workspace::open(root.clone()).unwrap();
        let list = ws.request_path("api/list").unwrap();
        assert_eq!(ws.load_request(&list).unwrap().url, "http://x/list");
        assert!(matches!(&ws.tree()[0], Node::Folder { children, .. }
            if matches!(&children[0], Node::Request { method, .. } if method == "POST")));
        assert_eq!(
            ws.load_folder(&ws.collections().join("api"))
                .unwrap()
                .description,
            "the API"
        );
        let dev = ws.env_vars(Some("dev")).unwrap();
        assert_eq!((dev["host"].as_str(), dev["token"].as_str()), ("h", "s"));
        assert_eq!(ws.load_history(), [entry]);
        assert_eq!(ws.load_cookies(), "[]");
        assert_eq!(ws.load_state().active_env.as_deref(), Some("dev"));
        drop(ws);

        // Once the database exists, the files are no longer read.
        write("collections/api/list.toml", "url = 'http://changed'\n");
        let ws = Workspace::open(root.clone()).unwrap();
        assert_eq!(ws.load_request(&list).unwrap().url, "http://x/list");
        drop(ws);

        // A broken file stops the first open loudly, and the next open tries again.
        let broken = fresh("migrate-broken");
        fs::create_dir_all(broken.join("collections")).unwrap();
        fs::write(broken.join("collections/bad.toml"), "url = [").unwrap();
        let e = Workspace::open(broken.clone()).err().unwrap();
        assert!(e.contains("bad.toml"), "{e}");
        assert!(!broken.join(DB).exists());
        fs::write(broken.join("collections/bad.toml"), "url = 'fixed'").unwrap();
        assert!(Workspace::open(broken.clone()).is_ok());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&broken).unwrap();
    }
}
