//! The one Import box: what was pasted, dropped or pointed to is recognised by its
//! shape, so nobody has to know which menu item their file belongs to.

use serde_json::Value;

pub use crate::postman::Import;

/// JSON, else YAML (OpenAPI specs mostly are).
pub fn value(text: &str) -> Result<Value, String> {
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(json) if text.trim_start().starts_with(['{', '[']) => Err(format!("not JSON: {json}")),
        Err(_) => serde_yaml_ng::from_str(text).map_err(|e| format!("not JSON or YAML: {e}")),
    }
}

pub fn parse(text: &str) -> Result<Import, String> {
    let v = value(text)?;
    if crate::openapi::is_spec(&v) {
        return Ok(crate::openapi::import(&v));
    }
    if crate::insomnia::is_insomnia(&v) {
        return Ok(crate::insomnia::import(&v));
    }
    if crate::har::is_har(&v) {
        return Ok(crate::har::import(&v));
    }
    if crate::postman3::is_v3(&v) {
        return crate::postman3::import(&v);
    }
    crate::postman::from_value(&v).map_err(|_| {
        "not something apitool imports: a Postman collection (v2.1 or v3), environment or \
         globals, an Insomnia export, an OpenAPI/Swagger spec or a HAR file"
            .to_owned()
    })
}

/// What an import added to the workspace.
#[derive(Default)]
pub struct Imported {
    /// The new top-level folders, as shown in the tree.
    pub folders: Vec<String>,
    pub requests: usize,
    /// The names given, which differ from the file's when one was taken.
    pub environments: Vec<String>,
    /// How many globals were added.
    pub globals: usize,
    /// What didn't come over as it was.
    pub warnings: Vec<String>,
}

/// `text` is the JSON or YAML. Nothing already there is replaced: a taken folder or
/// environment name gets " copy".
pub fn into_workspace(ws: &crate::store::Workspace, text: &str) -> Result<Imported, String> {
    add(ws, vec![parse(text)?], Vec::new())
}

/// A file, or a folder of Postman v3 YAML: one collection, or a Native Git workspace with
/// its environments and globals.
pub fn path_into_workspace(
    ws: &crate::store::Workspace,
    path: &std::path::Path,
) -> Result<Imported, String> {
    if path.is_dir() {
        let (imports, warnings) = crate::postman3::read_dir(path)?;
        return add(ws, imports, warnings);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    into_workspace(ws, &text)
}

fn add(
    ws: &crate::store::Workspace,
    imports: Vec<Import>,
    warnings: Vec<String>,
) -> Result<Imported, String> {
    let mut done = Imported {
        warnings,
        ..Default::default()
    };
    for import in imports {
        match import {
            Import::Collection {
                name,
                folders,
                requests,
                warnings,
                environments,
            } => {
                let dir = ws.add_tree(&name, &folders, &requests)?;
                done.folders.push(ws.display_name(&dir));
                done.requests += requests.len();
                for (env, shared, secret) in &environments {
                    done.environments.push(ws.add_env(env, shared, secret)?);
                }
                done.warnings.extend(warnings);
            }
            Import::Environment {
                name,
                shared,
                secret,
            } => done.environments.push(ws.add_env(&name, &shared, &secret)?),
            Import::Globals { shared, secret } => {
                let (mut own, mut own_secret) = ws.load_env(None)?;
                for (kv, is_secret) in (shared.into_iter().map(|kv| (kv, false)))
                    .chain(secret.into_iter().map(|kv| (kv, true)))
                {
                    if own.iter().chain(&own_secret).any(|o| o.key == kv.key) {
                        let what = format!("globals: {} is already set; kept as it was", kv.key);
                        done.warnings.push(what);
                        continue;
                    }
                    match is_secret {
                        true => own_secret.push(kv),
                        false => own.push(kv),
                    }
                    done.globals += 1;
                }
                ws.save_env(None, &own, &own_secret)?;
            }
        }
    }
    Ok(done)
}
