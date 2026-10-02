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
//! the database) is imported when opened. Secrets, history, cookies and UI state are never
//! exported; workspaces from before the database kept them in `*.secret.toml`,
//! `.history.jsonl`, `.cookies.json` and `.state.toml`, which are read in that first import.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::model::{Auth, Folder, Inherited, KeyValue, Request};

const DB: &str = "apitool.db";
/// The workspace directory may be a git repo of the exported tree; the database and the
/// per-machine files of older workspaces stay out of it.
const GITIGNORE: &str = "apitool.db\napitool.db-journal\n*.secret.toml\n.state.toml\n.history.jsonl\n.cookies.json\n*.tmp\n";
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
";

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
}

/// Clones share one connection: the GUI's mock server reads from its own thread.
#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
    db: Arc<Mutex<Connection>>,
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
            };
            let imported = ws.import(&root, true);
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
        };
        ws.ignore()?;
        Ok(ws)
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

    /// "users/get user" for a request, "users" for a folder, "" for the root.
    fn key(&self, path: &Path) -> String {
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

    /// The cookie jar as `Jar::to_json` wrote it; empty when there is none yet.
    pub fn load_cookies(&self) -> String {
        self.get("cookies").unwrap_or_default()
    }

    pub fn save_cookies(&self, json: &str) -> Result<(), String> {
        self.put("cookies", json)
    }

    /// Where requests and folders live, as paths (see the module docs).
    pub fn collections(&self) -> PathBuf {
        self.root.join("collections")
    }

    /// "folder/request" as shown in runner results.
    pub fn display_name(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.collections()).unwrap_or(path);
        rel.with_extension("").to_string_lossy().replace('\\', "/")
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
        let folders = rows("SELECT path, '' FROM folders WHERE path != ''");
        let requests = rows("SELECT path, method FROM requests");
        drop(db);
        match (folders, requests) {
            (Ok(folders), Ok(requests)) => self.nodes("", &folders, &requests),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("workspace database: {e}");
                Vec::new()
            }
        }
    }

    /// Folders first, then requests, each by name ignoring case.
    fn nodes(
        &self,
        parent: &str,
        folders: &[(String, String)],
        requests: &[(String, String)],
    ) -> Vec<Node> {
        let leaf = |key: &str| key.rsplit('/').next().unwrap_or(key).to_owned();
        let mut subfolders: Vec<Node> = folders
            .iter()
            .filter(|(key, _)| parent_of(key) == parent)
            .map(|(key, _)| Node::Folder {
                name: leaf(key),
                path: self.folder_at(key),
                children: self.nodes(key, folders, requests),
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
        subfolders
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

    /// `folder/name` (as shown by `display_name`) → its path. Every segment must be a valid
    /// name, which also keeps `..` out.
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

    /// Every request under `scope` (a folder or a single request), in tree order, with
    /// display names.
    pub fn load_requests_in(&self, scope: &Path) -> Result<Vec<(String, Request)>, String> {
        let mut paths = Vec::new();
        requests_in(&self.tree(), scope, &mut paths);
        if paths.is_empty() {
            return Err(format!("no requests under {}", self.display_name(scope)));
        }
        paths
            .iter()
            .map(|p| Ok((self.display_name(p), self.load_request(p)?)))
            .collect()
    }

    /// Creates `<dir>/<name>`, refusing to overwrite.
    pub fn create_request(&self, dir: &Path, name: &str) -> Result<PathBuf, String> {
        let path = dir.join(format!("{}.toml", valid_name(name)?));
        if self.exists(&path) {
            return Err(format!("\"{name}\" already exists"));
        }
        self.save_request(&path, &Request::default())?;
        Ok(path)
    }

    pub fn create_folder(&self, dir: &Path, name: &str) -> Result<PathBuf, String> {
        let path = dir.join(valid_folder_name(name)?);
        if self.exists(&path) {
            return Err(format!("\"{name}\" already exists"));
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
            true => path.with_file_name(format!("{}.toml", valid_name(name)?)),
            false => path.with_file_name(valid_folder_name(name)?),
        };
        if self.exists(&new) {
            return Err(format!("\"{}\" already exists", name.trim()));
        }
        let (old_key, new_key) = (self.key(path), self.key(&new));
        let mut db = self.db();
        let tx = sql(db.transaction())?;
        let tables: &[&str] = match request {
            true => &["requests"],
            false => &["folders", "requests"],
        };
        let rows = rows_of(request);
        for table in tables {
            let update = format!(
                "UPDATE {table} SET path = ?2 || substr(path, length(?1) + 1) WHERE {rows}"
            );
            sql(tx.execute(&update, [&old_key, &new_key]))?;
        }
        sql(tx.commit())?;
        Ok(new)
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
            true => &["requests"],
            false => &["folders", "requests"],
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
        sql(self.db().execute(
            "INSERT OR REPLACE INTO envs (name, shared, secret) VALUES (?1, ?2, ?3)",
            [env.unwrap_or(""), &keep(shared)?, &keep(secret)?],
        ))
        .map(drop)
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

    /// Reads the TOML tree under `dir` (see the module docs), replacing requests, folders
    /// and shared variables of the same name and keeping everything else, local secrets
    /// included. `first` also reads what workspaces from before the database kept per
    /// machine. Returns how many requests were read.
    pub fn import(&self, dir: &Path, first: bool) -> Result<usize, String> {
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
        if first {
            self.import_local(dir)?;
        }
        Ok(requests.len())
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
    /// holding `folders` and `requests`, keyed below it ("" is the folder itself).
    pub fn add_tree(
        &self,
        name: &str,
        folders: &[(String, Folder)],
        requests: &[(String, Request)],
    ) -> Result<PathBuf, String> {
        let dir = self.collections().join(valid_folder_name(name)?);
        let dir = match self.exists(&dir) {
            true => self.copy_of(&dir),
            false => dir,
        };
        let top = self.key(&dir);
        let under = |key: &str| -> Result<String, String> {
            for part in key.split('/').filter(|p| !p.is_empty()) {
                valid_name(part)?;
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

    /// Writes the TOML tree (see the module docs) under `dir`, for git: no secrets, history,
    /// cookies or UI state. Request, folder and environment files the database no longer
    /// has are removed so the tree matches it; other files (a .proto, a data file) stay.
    /// Returns how many requests were written.
    pub fn export(&self, dir: &Path) -> Result<usize, String> {
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
        let wanted: HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        prune(&collections, &wanted, &folders)?;
        for entry in fs::read_dir(&env_dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let secret = path
                .file_stem()
                .is_some_and(|s| s.to_string_lossy().ends_with(SECRET_SUFFIX));
            if is_request(&path) && !secret && !wanted.contains(path.as_path()) {
                fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))?;
            }
        }
        for dir in &folders {
            fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        fs::create_dir_all(&env_dir).map_err(|e| format!("create {}: {e}", env_dir.display()))?;
        for (path, text) in &files {
            write_atomic(path, text)?;
        }
        Ok(request_rows.len())
    }
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

/// "users/admin"; unlike `display_name`, a dot in a folder name isn't an extension.
pub fn folder_name(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    rel.to_string_lossy().replace('\\', "/")
}

/// Names become file names in an export, on macOS and Windows alike, so reject rather
/// than silently mangle.
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
fn valid_folder_name(name: &str) -> Result<&str, String> {
    let name = valid_name(name)?;
    match name.ends_with(".toml") {
        true => Err("a folder name can't end with .toml".into()),
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
            ws.create_request(&folder, "a/b").is_err(),
            "path separators are not names"
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
        fs::remove_dir_all(&root).unwrap();
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
        fs::remove_dir_all(&root).unwrap();
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
        fs::remove_dir_all(&root).unwrap();
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
        fs::remove_dir_all(&root).unwrap();
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
        fs::remove_dir_all(&root).unwrap();
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
        fs::remove_dir_all(&root).unwrap();
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
