//! The workspace is a plain directory meant to be a git repo:
//!   collections/**/<name>.toml   one request per file, folders are directories
//!   collections/**/.folder.toml  variables, auth and scripts shared by a folder (committed)
//!   environments/<name>.toml      shared variables (committed)
//!   environments/<name>.secret.toml  secret variables (gitignored)
//!   globals.toml, globals.secret.toml  workspace-wide variables, same split
//!   .state.toml                   per-machine UI state (gitignored)
//!   .history.jsonl                requests sent from the app (gitignored)
//!   .cookies.json                 the app's cookie jar (gitignored)

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Auth, Folder, Inherited, KeyValue, Request};

const GITIGNORE: &str = "*.secret.toml\n.state.toml\n.history.jsonl\n.cookies.json\n*.tmp\n";
const SECRET_SUFFIX: &str = ".secret";
const HISTORY: &str = ".history.jsonl";
/// Starts with a dot, so the tree (and `valid_name`) never mistakes it for a request.
const FOLDER: &str = ".folder.toml";
pub const MAX_HISTORY: usize = 200;
/// History is for re-sending, and RAM is tight: bigger bodies are left out.
const MAX_HISTORY_BODY: usize = 32 * 1024;

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
        // Examples are saved with the file already; repeating them per send is waste.
        request.examples.clear();
        let body_size = serde_json::to_string(&request.body).map_or(0, |b| b.len());
        let body_dropped = body_size > MAX_HISTORY_BODY;
        if body_dropped {
            request.body = crate::model::Body::None;
        }
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self {
            at,
            path,
            status,
            ms,
            request,
            body_dropped,
        }
    }
}

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
}

#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

/// Opens the workspace (`dir`, else `APITOOL_WORKSPACE`, else `workspace/` next to the exe:
/// portable, so the tool can sit in a user folder on a VDI without installation) and makes
/// it the working directory, so relative paths in requests (.proto, data files) keep
/// working after a git clone on another machine.
pub fn open_workspace(dir: Option<PathBuf>) -> Result<Workspace, String> {
    let dir = dir
        .or_else(|| std::env::var_os("APITOOL_WORKSPACE").map(PathBuf::from))
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

/// Request files under `scope` (a folder or a single request), in tree order.
pub fn requests_in(nodes: &[Node], scope: &Path, out: &mut Vec<PathBuf>) {
    for node in nodes {
        match node {
            Node::Folder { children, .. } => requests_in(children, scope, out),
            Node::Request { path, .. } if path.starts_with(scope) => out.push(path.clone()),
            Node::Request { .. } => {}
        }
    }
}

impl Workspace {
    pub fn open(root: PathBuf) -> Result<Self, String> {
        let ws = Self { root };
        for dir in [ws.collections(), ws.environments()] {
            fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        // Checked on every open, so files added to the list later (history holds tokens)
        // are ignored in existing workspaces too.
        let gitignore = ws.root.join(".gitignore");
        let current = fs::read_to_string(&gitignore).unwrap_or_default();
        let missing: String = GITIGNORE
            .lines()
            .filter(|line| !current.lines().any(|c| c.trim() == *line))
            .map(|line| format!("{line}\n"))
            .collect();
        if !missing.is_empty() {
            let sep = if current.is_empty() || current.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            write_atomic(&gitignore, &format!("{current}{sep}{missing}"))?;
        }
        Ok(ws)
    }

    /// Oldest first, at most `MAX_HISTORY`; the file is trimmed here, so appends stay cheap.
    pub fn load_history(&self) -> Vec<HistoryEntry> {
        let path = self.root.join(HISTORY);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut entries: Vec<HistoryEntry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if entries.len() > MAX_HISTORY {
            entries.drain(..entries.len() - MAX_HISTORY);
            let text: String = entries
                .iter()
                .filter_map(|e| serde_json::to_string(e).ok())
                .map(|line| line + "\n")
                .collect();
            // Best effort: an untrimmed file is still a valid one.
            let _ = write_atomic(&path, &text);
        }
        entries
    }

    pub fn append_history(&self, entry: &HistoryEntry) -> Result<(), String> {
        use std::io::Write;
        let line = serde_json::to_string(entry).map_err(|e| e.to_string())?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(HISTORY))
            .and_then(|mut f| writeln!(f, "{line}"))
            .map_err(|e| format!("history: {e}"))
    }

    pub fn clear_history(&self) -> Result<(), String> {
        match fs::remove_file(self.root.join(HISTORY)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("history: {e}")),
            _ => Ok(()),
        }
    }

    pub fn cookies_path(&self) -> PathBuf {
        self.root.join(".cookies.json")
    }

    pub fn collections(&self) -> PathBuf {
        self.root.join("collections")
    }

    fn environments(&self) -> PathBuf {
        self.root.join("environments")
    }

    /// "folder/request" as shown in runner results.
    pub fn display_name(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.collections()).unwrap_or(path);
        rel.with_extension("").to_string_lossy().replace('\\', "/")
    }

    /// Variables belong to an environment, or (`None`) to the workspace-wide globals.
    fn vars_path(&self, env: Option<&str>, secret: bool) -> PathBuf {
        let suffix = if secret { SECRET_SUFFIX } else { "" };
        match env {
            Some(name) => self.environments().join(format!("{name}{suffix}.toml")),
            None => self.root.join(format!("globals{suffix}.toml")),
        }
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

    /// Script writes (`pm.environment.set` …) go to the gitignored secret file: like
    /// Postman's "current value" they stay on this machine. `None` values remove the key.
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
        scan(&self.collections())
    }

    pub fn load_request(&self, path: &Path) -> Result<Request, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut req: Request =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        req.sync_params();
        req.inherited = self.inherited(path)?;
        Ok(req)
    }

    /// A missing file is empty settings; a broken one is an error, like environments.
    pub fn load_folder(&self, dir: &Path) -> Result<Folder, String> {
        let path = dir.join(FOLDER);
        match fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Folder::default()),
            Err(e) => Err(format!("read {}: {e}", path.display())),
            Ok(text) => toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display())),
        }
    }

    pub fn save_folder(&self, dir: &Path, folder: &Folder) -> Result<(), String> {
        let path = dir.join(FOLDER);
        if *folder == Folder::default() {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("delete: {e}")),
                _ => Ok(()),
            };
        }
        let mut folder = folder.clone();
        folder.vars.retain(|v| !v.key.is_empty());
        write_atomic(
            &path,
            &toml::to_string_pretty(&folder).map_err(|e| e.to_string())?,
        )
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

    pub fn save_request(&self, path: &Path, req: &Request) -> Result<(), String> {
        write_atomic(
            path,
            &toml::to_string_pretty(req).map_err(|e| e.to_string())?,
        )
    }

    /// `folder/name` (as shown by `display_name`) → its file. Every segment must be a valid
    /// name, which also keeps `..` from escaping the workspace.
    pub fn request_path(&self, name: &str) -> Result<PathBuf, String> {
        let name = name.trim().trim_matches('/');
        let name = name.strip_suffix(".toml").unwrap_or(name);
        let (dirs, file) = name.rsplit_once('/').unwrap_or(("", name));
        let mut path = self.collections();
        for dir in dirs.split('/').filter(|d| !d.is_empty()) {
            path.push(valid_name(dir)?);
        }
        Ok(path.join(format!("{}.toml", valid_name(file)?)))
    }

    /// Every request under `scope` (a folder or a single request file), in tree order,
    /// with display names.
    pub fn load_requests_in(&self, scope: &Path) -> Result<Vec<(String, Request)>, String> {
        let mut paths = Vec::new();
        requests_in(&self.tree(), scope, &mut paths);
        if paths.is_empty() {
            return Err(format!("no requests under {}", scope.display()));
        }
        paths
            .iter()
            .map(|p| Ok((self.display_name(p), self.load_request(p)?)))
            .collect()
    }

    /// Creates `<dir>/<name>.toml`, refusing to overwrite.
    pub fn create_request(&self, dir: &Path, name: &str) -> Result<PathBuf, String> {
        let path = dir.join(format!("{}.toml", valid_name(name)?));
        if path.exists() {
            return Err(format!("\"{name}\" already exists"));
        }
        self.save_request(&path, &Request::default())?;
        Ok(path)
    }

    pub fn create_folder(&self, dir: &Path, name: &str) -> Result<PathBuf, String> {
        let path = dir.join(valid_name(name)?);
        fs::create_dir(&path).map_err(|e| format!("create folder: {e}"))?;
        Ok(path)
    }

    /// Renames a request file or folder in place; returns the new path.
    pub fn rename(&self, path: &Path, name: &str) -> Result<PathBuf, String> {
        let name = valid_name(name)?;
        let file_name = if path.is_dir() {
            name.to_owned()
        } else {
            format!("{name}.toml")
        };
        let new = path.with_file_name(file_name);
        if new.exists() {
            return Err(format!("\"{name}\" already exists"));
        }
        fs::rename(path, &new).map_err(|e| format!("rename: {e}"))?;
        Ok(new)
    }

    /// Copies a request or a whole folder next to itself as "<name> copy" ("copy 2", …).
    pub fn duplicate(&self, path: &Path) -> Result<PathBuf, String> {
        let dir = path.is_dir();
        // A dot in a folder name is not an extension.
        let stem = match dir {
            true => path.file_name(),
            false => path.file_stem(),
        };
        let stem = stem.unwrap_or_default().to_string_lossy();
        let new = (1..)
            .map(|n| match n {
                1 => format!("{stem} copy"),
                n => format!("{stem} copy {n}"),
            })
            .map(|name| match dir {
                true => path.with_file_name(name),
                false => path.with_file_name(format!("{name}.toml")),
            })
            .find(|p| !p.exists())
            .expect("some name is free");
        fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
            fs::create_dir(to)?;
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                match entry.file_type()?.is_dir() {
                    true => copy_dir(&entry.path(), &to.join(entry.file_name()))?,
                    false => drop(fs::copy(entry.path(), to.join(entry.file_name()))?),
                }
            }
            Ok(())
        }
        match dir {
            true => copy_dir(path, &new),
            false => fs::copy(path, &new).map(drop),
        }
        .map_err(|e| format!("duplicate: {e}"))?;
        Ok(new)
    }

    pub fn delete(&self, path: &Path) -> Result<(), String> {
        if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
        .map_err(|e| format!("delete: {e}"))
    }

    pub fn env_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.environments())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .filter_map(|p| p.file_stem()?.to_str().map(str::to_owned))
            .filter(|stem| !stem.ends_with(SECRET_SUFFIX))
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    }

    /// Returns (shared, secret) variables. A missing file is empty; a broken one is an
    /// error, because silently dropping it makes every `{{name}}` look undefined.
    pub fn load_env(&self, env: Option<&str>) -> Result<(Vec<KeyValue>, Vec<KeyValue>), String> {
        let read = |path: PathBuf| match fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("read {}: {e}", path.display())),
            Ok(text) => toml::from_str::<EnvFile>(&text)
                .map(|f| f.vars)
                .map_err(|e| format!("parse {}: {e}", path.display())),
        };
        Ok((
            read(self.vars_path(env, false))?,
            read(self.vars_path(env, true))?,
        ))
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
        let write = |path: PathBuf, vars: &[KeyValue]| {
            let vars = vars.iter().filter(|v| !v.key.is_empty()).cloned().collect();
            let text = toml::to_string_pretty(&EnvFile { vars }).map_err(|e| e.to_string())?;
            write_atomic(&path, &text)
        };
        write(self.vars_path(env, false), shared)?;
        let secret_path = self.vars_path(env, true);
        if secret.iter().any(|v| !v.key.is_empty()) {
            write(secret_path, secret)
        } else {
            // Don't leave empty secret files lying around.
            let _ = fs::remove_file(secret_path);
            Ok(())
        }
    }

    pub fn delete_env(&self, name: &str) -> Result<(), String> {
        for file in [
            format!("{name}.toml"),
            format!("{name}{SECRET_SUFFIX}.toml"),
        ] {
            let path = self.environments().join(file);
            if path.exists() {
                fs::remove_file(&path).map_err(|e| format!("delete: {e}"))?;
            }
        }
        Ok(())
    }

    pub fn load_state(&self) -> State {
        fs::read_to_string(self.root.join(".state.toml"))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, state: &State) {
        // Losing UI state is harmless; don't bother the user about it.
        if let Ok(text) = toml::to_string_pretty(state) {
            let _ = write_atomic(&self.root.join(".state.toml"), &text);
        }
    }
}

/// "users/admin"; unlike `display_name`, a dot in a folder name isn't an extension.
pub fn folder_name(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    rel.to_string_lossy().replace('\\', "/")
}

fn scan(dir: &Path) -> Vec<Node> {
    let mut folders = Vec::new();
    let mut requests = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            folders.push(Node::Folder {
                children: scan(&path),
                name,
                path,
            });
        } else if let Some(stem) = name.strip_suffix(".toml") {
            // Peek at the method for the tree badge; a broken file still shows up so it can be fixed.
            let method = fs::read_to_string(&path)
                .ok()
                .and_then(|t| toml::from_str::<Request>(&t).ok())
                .map_or_else(|| "?".into(), |r| r.method);
            requests.push(Node::Request {
                name: stem.to_owned(),
                path,
                method,
            });
        }
    }
    let key = |n: &Node| match n {
        Node::Folder { name, .. } | Node::Request { name, .. } => name.to_lowercase(),
    };
    folders.sort_by_key(key);
    requests.sort_by_key(key);
    folders.extend(requests);
    folders
}

/// Names become file names on both macOS and Windows, so reject rather than silently mangle.
fn valid_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') {
        return Err("name can't be empty or start/end with '.'".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| r#"<>:"/\|?*"#.contains(*c) || c.is_control())
    {
        return Err(format!("name can't contain '{c}'"));
    }
    Ok(name)
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

    #[test]
    fn workspace_round_trip() {
        let root = std::env::temp_dir().join(format!("apitool-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();

        let folder = ws.create_folder(&ws.collections(), "Users").unwrap();
        let path = ws.create_request(&folder, "Get user").unwrap();
        assert!(
            ws.create_request(&folder, "Get user").is_err(),
            "must not overwrite"
        );
        assert!(
            ws.create_request(&folder, "a/b").is_err(),
            "path separators are not names"
        );

        let renamed = ws.rename(&path, "Fetch user").unwrap();
        assert!(matches!(&ws.tree()[0], Node::Folder { children, .. }
            if matches!(&children[0], Node::Request { name, .. } if name == "Fetch user")));
        assert_eq!(ws.load_request(&renamed).unwrap(), Request::default());

        // Secrets must land in the gitignored file, never in the shared one.
        ws.save_env(
            Some("dev"),
            &[KeyValue::new("host", "x")],
            &[KeyValue::new("token", "s3cret")],
        )
        .unwrap();
        assert_eq!(ws.env_names(), ["dev"]);
        let shared = fs::read_to_string(root.join("environments/dev.toml")).unwrap();
        assert!(!shared.contains("s3cret"));
        assert!(
            fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .contains("*.secret.toml")
        );
        assert_eq!(
            ws.load_env(Some("dev")).unwrap().1,
            [KeyValue::new("token", "s3cret")]
        );

        // Script writes chain into the local (secret) file, for envs and globals alike.
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
        // Removing from the secret file never touches the committed shared value.
        assert_eq!(ws.env_vars(Some("dev")).unwrap()["host"], "x");
        assert!(!ws.env_names().contains(&"globals".to_owned()));

        // A broken file is reported, not treated as "no variables".
        fs::write(root.join("environments/dev.toml"), "vars = [").unwrap();
        assert!(ws.env_vars(Some("dev")).unwrap_err().contains("dev.toml"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn duplicates_sit_next_to_the_original_and_never_overwrite() {
        let root = std::env::temp_dir().join(format!("apitool-dup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
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
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn folder_settings_cascade_to_the_requests_below() {
        let root = std::env::temp_dir().join(format!("apitool-folders-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
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
            "settings files are not requests"
        );

        ws.save_folder(&admin, &Folder::default()).unwrap();
        assert!(!admin.join(FOLDER).exists(), "no empty settings files");
        // A broken file must not quietly drop the auth every request relies on.
        fs::write(api.join(FOLDER), "auth = [").unwrap();
        assert!(ws.load_request(&req).unwrap_err().contains(".folder.toml"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn history_is_gitignored_bounded_and_drops_huge_bodies() {
        let root = std::env::temp_dir().join(format!("apitool-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // A workspace from before history existed: its own rules are kept, ours added.
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitignore"), "custom").unwrap();
        let ws = Workspace::open(root.clone()).unwrap();
        let ignore = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(ignore.starts_with("custom\n") && ignore.contains("\n.history.jsonl\n"));
        Workspace::open(root.clone()).unwrap();
        assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), ignore);

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
        let lines = fs::read_to_string(root.join(HISTORY))
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, MAX_HISTORY, "file trimmed too");

        fs::remove_dir_all(&root).unwrap();
    }
}
