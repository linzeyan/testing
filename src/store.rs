//! The workspace is a plain directory meant to be a git repo:
//!   collections/**/<name>.toml   one request per file, folders are directories
//!   environments/<name>.toml      shared variables (committed)
//!   environments/<name>.secret.toml  secret variables (gitignored)
//!   globals.toml, globals.secret.toml  workspace-wide variables, same split
//!   .state.toml                   per-machine UI state (gitignored)

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{KeyValue, Request};

const GITIGNORE: &str = "*.secret.toml\n.state.toml\n*.tmp\n";
const SECRET_SUFFIX: &str = ".secret";

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
    pub open: Option<PathBuf>,
    pub network: crate::net::Network,
}

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
        let gitignore = ws.root.join(".gitignore");
        if !gitignore.exists() {
            write_atomic(&gitignore, GITIGNORE)?;
        }
        Ok(ws)
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
        Ok(req)
    }

    pub fn save_request(&self, path: &Path, req: &Request) -> Result<(), String> {
        write_atomic(
            path,
            &toml::to_string_pretty(req).map_err(|e| e.to_string())?,
        )
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
}
