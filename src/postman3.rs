//! Postman's v3 collections: the YAML that Native Git and the Postman CLI keep in a repo (a
//! folder per collection and folder, a `*.request.yaml` per request, `.resources/` beside
//! them), or the same tree as one document. Each becomes a v2.1 collection, so one reader
//! (`postman`) treats both formats alike; what v2.1 can't hold (stream messages, MQTT
//! topics) rides along under `_v3` and is set by `stream`.

use std::path::Path;

use serde_json::{Value, json};

use crate::model::{KeyValue, Request, Topic};
use crate::postman::{Import, from_value};

/// `$kind: collection` at the top, or children that are v3 requests. Postman's app model
/// nests it under `collection`, may leave out its `$kind` and spells some fields its own
/// way (`title`, `children`, `payload.variables`).
pub fn is_v3(v: &Value) -> bool {
    collection(v).is_some()
}

fn collection(v: &Value) -> Option<&Value> {
    let requests = |c: &Value| {
        children(c).is_some_and(|list| {
            list.iter()
                .any(|r| r["$kind"].as_str().is_some_and(|k| k.ends_with("-request")))
        })
    };
    [v, &v["collection"]]
        .into_iter()
        .find(|c| c["$kind"] == "collection" || requests(c))
}

pub fn import(v: &Value) -> Result<Import, String> {
    let c = collection(v).ok_or("not a Postman v3 collection")?;
    let mut warnings = Vec::new();
    let mut v2 = group(c, "", &mut warnings);
    v2["info"] = json!({ "name": name_of(c), "description": c["description"] });
    let mut import = from_value(&v2)?;
    if let Import::Collection { warnings: w, .. } = &mut import {
        warnings.append(w);
        *w = warnings;
    }
    Ok(import)
}

/// `dir` is a collection's folder, or a workspace (holding `postman/`, or that folder
/// itself) whose collections, environments and globals all come in.
pub fn read_dir(dir: &Path) -> Result<(Vec<Import>, Vec<String>), String> {
    let workspace = [dir.join("postman"), dir.to_owned()].into_iter().find(|d| {
        ["collections", "environments", "globals"]
            .iter()
            .any(|sub| d.join(sub).is_dir())
    });
    let Some(root) = workspace else {
        let c = tree(dir)?;
        if c["items"].as_array().is_none_or(Vec::is_empty) && !definition(dir).is_file() {
            return Err(format!(
                "{}: no Postman v3 collection here (no *.request.yaml or .resources/definition.yaml)",
                dir.display()
            ));
        }
        return Ok((vec![import(&c)?], Vec::new()));
    };
    let (mut imports, mut warnings) = (Vec::new(), Vec::new());
    for entry in entries(&root)? {
        let name = file_name(&entry);
        match name.as_str() {
            "collections" => {
                for c in entries(&entry)?.into_iter().filter(|p| p.is_dir()) {
                    imports.push(import(&tree(&c)?)?);
                }
            }
            "environments" | "globals" => {
                for file in entries(&entry)?.into_iter().filter(|p| p.is_file()) {
                    let mut v = crate::import::value(&read(&file)?)?;
                    if name == "globals" {
                        v["_postman_variable_scope"] = "globals".into();
                    }
                    imports.push(from_value(&v).map_err(|e| format!("{}: {e}", file.display()))?);
                }
            }
            _ if entry.is_dir() => warnings.push(match name.as_str() {
                "specs" => "specs/ left out: import each spec file on its own".to_owned(),
                _ => {
                    format!("{name}/ left out: only collections, environments and globals come in")
                }
            }),
            _ => {}
        }
    }
    Ok((imports, warnings))
}

fn definition(dir: &Path) -> std::path::PathBuf {
    dir.join(".resources").join("definition.yaml")
}

/// A folder on disk as a v3 collection document: its definition, then its folders and
/// requests in the order they say, examples read in from beside them.
fn tree(dir: &Path) -> Result<Value, String> {
    let def = definition(dir);
    let mut c = match def.is_file() {
        true => yaml(&def)?,
        false => Value::Null,
    };
    if !c.is_object() {
        c = json!({});
    }
    if c["name"].as_str().is_none_or(str::is_empty) {
        c["name"] = file_name(dir).into();
    }
    c["$kind"] = "collection".into();
    let mut items = Vec::new();
    for path in entries(dir)? {
        let name = file_name(&path);
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            items.push(tree(&path)?);
            continue;
        }
        let Some(stem) = [".request.yaml", ".request.yml"]
            .iter()
            .find_map(|ext| name.strip_suffix(ext))
        else {
            continue;
        };
        let mut r = yaml(&path)?;
        if r["name"].as_str().is_none_or(str::is_empty) {
            r["name"] = stem.into();
        }
        let examples = match r["examples"].as_str() {
            Some(rel) => dir.join(rel),
            None => dir
                .join(".resources")
                .join(format!("{stem}.resources"))
                .join("examples"),
        };
        r["examples"] = Value::Array(match examples.is_dir() {
            true => sorted(examples_in(&examples)?),
            false => Vec::new(),
        });
        items.push(r);
    }
    c["items"] = Value::Array(sorted(items));
    Ok(c)
}

fn examples_in(dir: &Path) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for path in entries(dir)? {
        let name = file_name(&path);
        if let Some(stem) = [".example.yaml", ".example.yml"]
            .iter()
            .find_map(|ext| name.strip_suffix(ext))
        {
            let mut e = yaml(&path)?;
            if e["name"].as_str().is_none_or(str::is_empty) {
                e["name"] = stem.into();
            }
            out.push(e);
        }
    }
    Ok(out)
}

/// By `order`; what has none goes last. Stable, so ties keep the file names' order.
fn sorted(mut items: Vec<Value>) -> Vec<Value> {
    items.sort_by(|a, b| {
        let at = |v: &Value| v["order"].as_f64().unwrap_or(f64::INFINITY);
        at(a).total_cmp(&at(b))
    });
    items
}

/// By name, so an import reads the same on every file system.
fn entries(dir: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let list = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut paths: Vec<_> = list.filter_map(|e| Some(e.ok()?.path())).collect();
    paths.sort();
    Ok(paths)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn yaml(p: &Path) -> Result<Value, String> {
    crate::import::value(&read(p)?).map_err(|e| format!("{}: {e}", p.display()))
}

fn name_of(v: &Value) -> &str {
    [&v["name"], &v["title"]]
        .into_iter()
        .find_map(Value::as_str)
        .unwrap_or_default()
}

fn children(v: &Value) -> Option<&Vec<Value>> {
    [&v["items"], &v["children"]]
        .into_iter()
        .find_map(Value::as_array)
}

fn join(at: &str, name: &str) -> String {
    match at {
        "" => name.to_owned(),
        at => format!("{at}/{name}"),
    }
}

/// A string as it is; JSON (GraphQL variables, a message) written out as text.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// Rows are v2.1's `{key, value, description, disabled}` already; a map (`Content-Type:
/// application/json`), as the CLI writes short lists, becomes them.
fn rows(v: &Value) -> Value {
    match v {
        Value::Object(map) => (map.iter())
            .map(|(k, v)| json!({ "key": k, "value": v }))
            .collect(),
        Value::Array(_) => v.clone(),
        _ => json!([]),
    }
}

fn group(c: &Value, at: &str, warnings: &mut Vec<String>) -> Value {
    let mut item = Vec::new();
    for child in children(c).into_iter().flatten() {
        let path = join(at, name_of(child));
        if children(child).is_some() || child["$kind"] == "collection" {
            item.push(group(child, &path, warnings));
            continue;
        }
        match request(child, &path, warnings) {
            Ok(r) => item.push(r),
            Err(e) => warnings.push(format!("{path}: {e}")),
        }
    }
    // Rows, or a map when no row has more than its key and value (as `migrate` writes).
    let vars = [&c["variables"], &c["payload"]["variables"]]
        .into_iter()
        .find(|v| v.is_array() || v.is_object());
    let at = if at.is_empty() { "collection" } else { at };
    json!({
        "name": name_of(c),
        "description": c["description"],
        "variable": vars.map_or(json!([]), rows),
        "auth": auth(&c["auth"], at, warnings),
        "event": events(&c["scripts"], at, warnings),
        "item": item,
    })
}

fn request(r: &Value, at: &str, warnings: &mut Vec<String>) -> Result<Value, String> {
    let kind = r["$kind"].as_str().unwrap_or("http-request");
    let url = text(&r["url"]);
    let (method, url, body) = match kind {
        "http-request" => (text(&r["method"]), url, body(&r["body"])),
        "graphql-request" => {
            let graphql = json!({ "query": text(&r["query"]), "variables": text(&r["variables"]) });
            (
                "POST".into(),
                url,
                json!({ "mode": "graphql", "graphql": graphql }),
            )
        }
        "grpc-request" => {
            let message = text(&r["message"]["content"]);
            let raw = json!({ "mode": "raw", "raw": message, "options": { "raw": { "language": "json" } } });
            let secure = r["settings"]["secureConnection"] == true;
            ("GRPC".into(), grpc_url(&url, secure), raw)
        }
        "websocket-request" => ("WS".into(), url, Value::Null),
        "socket.io-request" => ("SOCKETIO".into(), url, Value::Null),
        "mqtt-request" => ("MQTT".into(), url, Value::Null),
        other => {
            let what = other.trim_end_matches("-request");
            return Err(format!("{what} requests aren't supported; left out"));
        }
    };
    let headers = match kind {
        "grpc-request" => &r["metadata"],
        _ => &r["headers"],
    };
    let mut examples = Vec::new();
    for e in r["examples"].as_array().into_iter().flatten() {
        match example(e) {
            Some(e) => examples.push(e),
            None => warnings.push(format!(
                "{at}: example \"{}\" ({}) isn't kept",
                name_of(e),
                text(&e["$kind"])
            )),
        }
    }
    let mut item = json!({
        "name": name_of(r),
        "request": {
            "method": method,
            "description": r["description"],
            "url": {
                "raw": url,
                "query": rows(&r["queryParams"]),
                "variable": rows(&r["pathVariables"]),
            },
            "header": rows(headers),
            "body": body,
            "auth": auth(&r["auth"], at, warnings),
        },
        "event": events(&r["scripts"], at, warnings),
        // Its names are v2.1's: strictSSL, followRedirects, maxRedirects.
        "protocolProfileBehavior": r["settings"],
        "response": examples,
    });
    if kind != "http-request" && kind != "graphql-request" {
        item["_v3"] = r.clone();
    }
    Ok(item)
}

fn body(b: &Value) -> Value {
    let c = &b["content"];
    match b["type"].as_str().unwrap_or_default() {
        "" | "none" => Value::Null,
        t @ ("json" | "text" | "xml" | "html" | "javascript") => {
            json!({ "mode": "raw", "raw": text(c), "options": { "raw": { "language": t } } })
        }
        "urlencoded" => json!({ "mode": "urlencoded", "urlencoded": rows(c) }),
        "formdata" => json!({ "mode": "formdata", "formdata": rows(c) }),
        "file" => json!({ "mode": "file", "file": { "src": c["src"] } }),
        "graphql" => {
            let graphql = json!({ "query": text(&c["query"]), "variables": text(&c["variables"]) });
            json!({ "mode": "graphql", "graphql": graphql })
        }
        // The reader names what it can't take.
        other => json!({ "mode": other }),
    }
}

/// Postman's gRPC URL has no scheme (or grpc://, grpcs://); here it's the HTTP/2 base.
fn grpc_url(url: &str, secure: bool) -> String {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("grpcs://") {
        return format!("https://{rest}");
    }
    if let Some(rest) = url.strip_prefix("grpc://") {
        return format!("http://{rest}");
    }
    match (url.contains("://"), secure) {
        (true, _) => url.to_owned(),
        (false, true) => format!("https://{url}"),
        (false, false) => format!("http://{url}"),
    }
}

/// `{type, credentials}`, the credentials as rows or a map; a collection may list several
/// (multi-auth), of which the first is the default.
fn auth(a: &Value, at: &str, warnings: &mut Vec<String>) -> Value {
    let a = match a {
        Value::Array(all) => {
            if all.len() > 1 {
                let n = all.len();
                warnings.push(format!("{at}: only the first of {n} auths is kept"));
            }
            all.first().unwrap_or(&Value::Null)
        }
        a => a,
    };
    match a["type"].as_str() {
        // Rows are v2.1's form and a map v2.0's; `postman::param` reads either.
        Some(kind) => json!({ "type": kind, kind: a["credentials"] }),
        None => Value::Null,
    }
}

/// Before and after scripts, by whatever name each kind gives them (`http:beforeRequest`,
/// `beforeQuery`, `grpc:beforeInvoke`…).
fn events(scripts: &Value, at: &str, warnings: &mut Vec<String>) -> Value {
    let mut out = Vec::new();
    for s in scripts.as_array().into_iter().flatten() {
        let kind = text(&s["type"]);
        let kind = kind.rsplit(':').next().unwrap_or_default();
        // Postman keeps an empty entry for every script tab; nothing is lost with it.
        if s["code"]
            .as_str()
            .is_some_and(|code| code.trim().is_empty())
        {
            continue;
        }
        let listen = match kind {
            "beforeRequest" | "beforeQuery" | "beforeInvoke" => "prerequest",
            "afterResponse" => "test",
            other => {
                warnings.push(format!("{at}: the {other} script isn't kept"));
                continue;
            }
        };
        match s["code"].as_str() {
            Some(code) => out.push(json!({ "listen": listen, "script": { "exec": code } })),
            None => warnings.push(format!("{at}: a {kind} script kept in a file isn't read")),
        }
    }
    Value::Array(out)
}

/// An HTTP or GraphQL example as v2.1's saved response; `None` for a stream's.
fn example(e: &Value) -> Option<Value> {
    let r = &e["response"];
    // The app model wraps a GraphQL response in `http`.
    let r = if r["http"].is_object() { &r["http"] } else { r };
    let code = (r["statusCode"].as_u64())
        .or_else(|| r["status"].as_u64())
        .or_else(|| {
            r["status"]
                .as_str()?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })?;
    let body = match &r["body"] {
        b @ Value::Object(_) => text(&b["content"]),
        b => text(b),
    };
    Some(json!({ "name": name_of(e), "code": code, "header": rows(&r["headers"]), "body": body }))
}

/// What a stream request has beyond URL, headers, auth and body. `v` is the v3 request,
/// null for any other.
pub fn stream(req: &mut Request, v: &Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let messages = v["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    match v["$kind"].as_str().unwrap_or_default() {
        "websocket-request" => {
            // The app model's raw messages keep it under `payload`.
            let content = |m: &Value| match &m["content"] {
                Value::Null => text(&m["payload"]["content"]),
                c => text(c),
            };
            req.messages = messages.iter().map(content).collect();
        }
        "socket.io-request" => {
            req.messages = messages.iter().map(socketio_message).collect();
            let s = &v["settings"];
            req.socketio.path = text(&s["path"]);
            req.socketio.v2 = text(&s["version"])
                .trim_start_matches(['v', 'V'])
                .starts_with('2');
        }
        "grpc-request" => {
            // `package.Service.Method` (or with a slash) to `package.Service/Method`.
            let path = text(&v["methodPath"]);
            let path = path.trim_start_matches('/');
            req.rpc = match path.contains('/') {
                true => path.to_owned(),
                false => match path.rsplit_once('.') {
                    Some((service, method)) => format!("{service}/{method}"),
                    None => path.to_owned(),
                },
            };
        }
        "mqtt-request" => mqtt(req, v, messages, &mut warnings),
        _ => {}
    }
    warnings
}

/// `["event", arg…]`, which the compose box sends as it is.
fn socketio_message(m: &Value) -> String {
    let mut out = vec![Value::String(text(&m["eventName"]))];
    for arg in m["args"].as_array().into_iter().flatten() {
        let content = text(&arg["content"]);
        out.push(match arg["contentType"] == "json" {
            true => serde_json::from_str(&content).unwrap_or(Value::String(content)),
            false => Value::String(content),
        });
    }
    serde_json::to_string(&out).unwrap_or_default()
}

fn mqtt(req: &mut Request, v: &Value, messages: &[Value], warnings: &mut Vec<String>) {
    let m = &mut req.mqtt;
    let qos = |v: &Value| v.as_u64().unwrap_or(0).min(2) as u8;
    m.client_id = text(&v["clientId"]);
    m.v5 = v["version"] == 5;
    let s = &v["settings"];
    if let Some(secs) = s["keepAlive"].as_u64() {
        m.keep_alive_secs = secs.min(u16::MAX as u64) as u16;
    }
    if let Some(clean) = s["cleanSession"].as_bool() {
        m.clean_session = clean;
    }
    m.topics = (v["topics"].as_array().into_iter().flatten())
        .map(|t| Topic {
            filter: text(&t["name"]),
            qos: qos(&t["qos"]),
            enabled: t["subscribe"] != false,
        })
        .collect();
    let will = &v["lastWill"];
    m.will_topic = text(&will["topic"]);
    m.will_payload = text(&will["payload"]);
    m.will_qos = qos(&will["qos"]);
    m.will_retain = will["retain"] == true;
    m.will_content_type = text(&will["properties"]["contentType"]);
    let expiry = will["properties"]["messageExpiryInterval"]
        .as_u64()
        .unwrap_or(0);
    m.will_expiry_secs = expiry.min(u32::MAX as u64) as u32;
    m.user_properties = (v["properties"]["userProperties"]
        .as_array()
        .into_iter()
        .flatten())
    .map(|p| KeyValue::new(text(&p["key"]), text(&p["value"])))
    .collect();
    // Send publishes to one topic; a saved message keeps only its payload.
    if let Some(first) = messages.first() {
        m.topic = text(&first["topic"]);
        m.qos = qos(&first["qos"]);
        m.retain = first["retain"] == true;
    }
    let mut others: Vec<String> = (messages.iter())
        .map(|msg| text(&msg["topic"]))
        .filter(|t| *t != m.topic)
        .collect();
    others.dedup();
    if !others.is_empty() {
        warnings.push(format!(
            "saved messages publish to \"{}\"; their own topics ({}) aren't kept",
            m.topic,
            others.join(", ")
        ));
    }
    req.messages = messages.iter().map(|msg| text(&msg["content"])).collect();
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::model::{Auth, Body};
    use crate::store::Workspace;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apitool-v3-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    type Parts = (
        String,
        Vec<(String, crate::model::Folder)>,
        Vec<(String, Request)>,
        Vec<String>,
    );

    fn collection(dir: &Path) -> Parts {
        let (mut imports, _) = read_dir(dir).unwrap();
        let Import::Collection {
            name,
            folders,
            requests,
            warnings,
            ..
        } = imports.remove(0)
        else {
            panic!("a collection");
        };
        (name, folders, requests, warnings)
    }

    fn find<'a, T>(list: &'a [(String, T)], key: &str) -> &'a T {
        let found = list.iter().find(|(k, _)| k == key);
        &found.unwrap_or_else(|| panic!("no {key}")).1
    }

    /// As `postman collection migrate` (Postman CLI 1.73) writes a v2.1 collection: what a
    /// team keeps in git must come in as Postman would open it.
    #[test]
    fn a_native_git_collection_comes_in_as_postman_wrote_it() {
        let dir = temp("git");
        write(
            &dir,
            &[
                (
                    ".resources/definition.yaml",
                    r#"$kind: collection
name: Sample API
description: Collection docs
variables:
  - key: baseUrl
    value: https://api.example.com
    secret: false
  - key: off
    value: x
    disabled: true
scripts:
  - type: http:beforeRequest
    code: pm.variables.set('t', Date.now());
    language: text/javascript
  - type: http:afterResponse
    code: pm.test('ok', () => pm.response.to.have.status(200));
auth:
  - id: cf28b545
    type: bearer
    name: bearer auth
    credentials:
      token: "{{token}}"
"#,
                ),
                (
                    "Users/.resources/definition.yaml",
                    r#"$kind: collection
description: User folder
variables:
  team: red
scripts:
  - type: http:beforeRequest
    code: console.log('folder');
auth:
  - id: 11ba90d9
    type: basic
    credentials:
      username: u
      password: p
order: 1000
"#,
                ),
                (
                    "Users/Get user.request.yaml",
                    r#"$kind: http-request
url: "{{baseUrl}}/users/:id?verbose=true"
method: GET
headers:
  - key: Accept
    value: application/json
    description: want json
  - key: X-Off
    value: "1"
    disabled: true
queryParams:
  - key: verbose
    value: "true"
  - key: skip
    value: "1"
    disabled: true
pathVariables:
  - key: id
    value: "42"
scripts:
  - type: afterResponse
    code: pm.test('has id', () => {});
examples: ./.resources/Get user.resources/examples
order: 1000
"#,
                ),
                (
                    "Users/.resources/Get user.resources/examples/OK.example.yaml",
                    r#"$kind: http-example
request:
  url: "{{baseUrl}}/users/42"
  method: GET
response:
  statusCode: 201
  statusText: Created
  headers:
    Content-Type: application/json
  body:
    type: text
    content: '{"id":42}'
"#,
                ),
                (
                    "Users/Create user.request.yaml",
                    r#"$kind: http-request
url: "{{baseUrl}}/users"
method: POST
headers:
  Content-Type: application/json
body:
  type: json
  content: '{"name": "{{name}}"}'
order: 2000
"#,
                ),
                (
                    "Users/Login form.request.yaml",
                    r#"$kind: http-request
url: "{{baseUrl}}/login"
method: POST
body:
  type: urlencoded
  content:
    - key: user
      value: a
    - key: pass
      value: b
      disabled: true
auth:
  type: apikey
  credentials:
    key: X-Key
    value: "{{key}}"
    in: header
order: 3000
"#,
                ),
                (
                    "GraphQL query.request.yaml",
                    r#"$kind: http-request
url: "{{baseUrl}}/graphql"
method: POST
body:
  type: graphql
  content:
    query: "query($id: ID!) { user(id: $id) { name } }"
    variables: '{"id": "1"}'
order: 2000
"#,
                ),
                (
                    "Binary.request.yaml",
                    r#"$kind: http-request
url: "{{baseUrl}}/blob"
method: POST
body:
  type: file
  content:
    src: /tmp/blob.bin
order: 3000
"#,
                ),
            ],
        );
        let (name, folders, requests, warnings) = collection(&dir);
        assert_eq!(name, "Sample API");
        assert_eq!(warnings, Vec::<String>::new());

        let root = find(&folders, "");
        assert_eq!(root.description, "Collection docs");
        assert_eq!(root.vars.len(), 2);
        assert!(!root.vars[1].enabled, "a disabled variable stays off");
        assert_eq!(
            root.auth,
            Auth::Bearer {
                token: "{{token}}".into()
            }
        );
        assert!(
            root.pre_request.contains("pm.variables.set") && root.tests.contains("pm.test('ok'")
        );
        // Postman's order, not the tree's by-name one (Binary before GraphQL query).
        assert_eq!(root.order, ["Users/", "GraphQL query", "Binary"]);

        let users = find(&folders, "Users");
        assert_eq!(
            users.auth,
            Auth::Basic {
                username: "u".into(),
                password: "p".into()
            }
        );
        assert_eq!(users.pre_request, "console.log('folder');");
        assert_eq!(
            users.vars,
            [KeyValue::new("team", "red")],
            "variables as a map"
        );
        assert_eq!(users.order, ["Get user", "Create user", "Login form"]);

        let get = find(&requests, "Users/Get user");
        assert_eq!(get.method, "GET");
        let params: Vec<_> = get
            .params
            .iter()
            .map(|p| (p.key.as_str(), p.enabled))
            .collect();
        assert_eq!(params, [("verbose", true), ("skip", false)]);
        assert_eq!(
            (
                get.path_vars[0].key.as_str(),
                get.path_vars[0].value.as_str()
            ),
            ("id", "42")
        );
        assert_eq!(get.headers[0].description, "want json");
        assert!(!get.headers[1].enabled);
        assert!(get.tests.contains("has id"));
        let example = &get.examples[0];
        assert_eq!((example.name.as_str(), example.status), ("OK", 201));
        assert_eq!(
            (example.content_type.as_str(), example.body.as_str()),
            ("application/json", r#"{"id":42}"#)
        );

        let create = find(&requests, "Users/Create user");
        assert_eq!(
            create.headers[0].key, "Content-Type",
            "a header map is rows too"
        );
        assert_eq!(
            create.body,
            Body::Json {
                text: r#"{"name": "{{name}}"}"#.into()
            }
        );

        let login = find(&requests, "Users/Login form");
        let key = Auth::ApiKey {
            key: "X-Key".into(),
            value: "{{key}}".into(),
            in_query: false,
        };
        assert_eq!(login.auth, key, "credentials as a map");
        let Body::Form { fields } = &login.body else {
            panic!("a form")
        };
        assert!(fields[0].enabled && !fields[1].enabled);

        let graphql = find(&requests, "GraphQL query");
        assert_eq!(graphql.method, "GRAPHQL");
        assert!(matches!(&graphql.body, Body::GraphQL { query, variables }
            if query.starts_with("query($id") && variables == r#"{"id": "1"}"#));
        assert_eq!(
            find(&requests, "Binary").body,
            Body::File {
                path: "/tmp/blob.bin".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The kinds a v2.1 collection can't hold, as the CLI and the v3 schema write them.
    #[test]
    fn every_stream_kind_keeps_its_messages_and_settings() {
        let dir = temp("kinds");
        write(
            &dir,
            &[
                (
                    "A graphql.request.yaml",
                    r#"$kind: graphql-request
url: '{{baseUrl}}/graphql'
query: 'query Q { me { id } }'
variables: '{"id":"1"}'
auth:
  type: bearer
  credentials:
    - key: token
      value: tok
scripts:
  - type: beforeQuery
    code: console.log(1)
"#,
                ),
                (
                    "B grpc.request.yaml",
                    r#"$kind: grpc-request
url: localhost:50051
methodPath: pkg.Greeter.SayHello
message:
  content: '{"name":"x"}'
metadata:
  - key: x-md
    value: '1'
settings:
  secureConnection: true
"#,
                ),
                (
                    "C websocket.request.yaml",
                    r#"$kind: websocket-request
url: wss://echo.example/ws
messages:
  - name: Hello
    $kind: websocket-message
    content: '{"hello":1}'
  - name: Bye
    content: bye
"#,
                ),
                (
                    "D socketio.request.yaml",
                    r#"$kind: socket.io-request
url: http://localhost:3000/chat
messages:
  - name: Message
    eventName: chat
    args:
      - content: '{"a":1}'
        contentType: json
      - content: hi
        contentType: text
settings:
  path: /io
  version: v2
"#,
                ),
                (
                    "E mqtt.request.yaml",
                    r#"$kind: mqtt-request
url: mqtt://broker:1883
clientId: me
version: 5
topics:
  - name: scores/#
    qos: 1
    subscribe: true
  - name: news
    qos: 0
    subscribe: false
lastWill:
  topic: bye
  payload: gone
  qos: 2
  retain: true
  properties:
    messageExpiryInterval: 120
    contentType: text/plain
properties:
  userProperties:
    - key: team
      value: red
settings:
  keepAlive: 30
  cleanSession: false
messages:
  - topic: scores/1
    qos: 1
    retain: true
    content: '{"goal":1}'
  - topic: news
    content: hi
"#,
                ),
                (
                    "F mcp.request.yaml",
                    "$kind: mcp-request\ntransport: stdio\n",
                ),
            ],
        );
        let (_, _, requests, warnings) = collection(&dir);

        let graphql = find(&requests, "A graphql");
        assert_eq!(graphql.method, "GRAPHQL");
        assert_eq!(
            graphql.auth,
            Auth::Bearer {
                token: "tok".into()
            },
            "credentials as rows"
        );
        assert_eq!(graphql.pre_request, "console.log(1)");

        let grpc = find(&requests, "B grpc");
        assert_eq!(
            (grpc.method.as_str(), grpc.url.as_str()),
            ("GRPC", "https://localhost:50051")
        );
        assert_eq!(grpc.rpc, "pkg.Greeter/SayHello");
        assert_eq!(
            grpc.body,
            Body::Json {
                text: r#"{"name":"x"}"#.into()
            }
        );
        assert_eq!(grpc.headers[0].key, "x-md", "metadata goes out as headers");

        let ws = find(&requests, "C websocket");
        assert_eq!(ws.method, "WS");
        assert_eq!(ws.messages, [r#"{"hello":1}"#, "bye"]);

        let io = find(&requests, "D socketio");
        assert_eq!(io.method, "SOCKETIO");
        assert_eq!(io.messages, [r#"["chat",{"a":1},"hi"]"#]);
        assert_eq!((io.socketio.path.as_str(), io.socketio.v2), ("/io", true));

        let mqtt = find(&requests, "E mqtt");
        let m = &mqtt.mqtt;
        assert_eq!(mqtt.method, "MQTT");
        assert!(m.v5 && !m.clean_session && m.keep_alive_secs == 30 && m.client_id == "me");
        let topics: Vec<_> = m
            .topics
            .iter()
            .map(|t| (t.filter.as_str(), t.qos, t.enabled))
            .collect();
        assert_eq!(topics, [("scores/#", 1, true), ("news", 0, false)]);
        assert_eq!(
            (
                m.will_topic.as_str(),
                m.will_payload.as_str(),
                m.will_qos,
                m.will_retain,
                m.will_content_type.as_str(),
                m.will_expiry_secs
            ),
            ("bye", "gone", 2, true, "text/plain", 120)
        );
        assert_eq!(m.user_properties, [KeyValue::new("team", "red")]);
        assert_eq!((m.topic.as_str(), m.qos, m.retain), ("scores/1", 1, true));
        assert_eq!(mqtt.messages, [r#"{"goal":1}"#, "hi"]);

        assert!(
            !requests.iter().any(|(k, _)| k.starts_with("F mcp")),
            "no stand-in for an MCP request"
        );
        let said = |what: &str| warnings.iter().any(|w| w.contains(what));
        assert!(said("F mcp: mcp requests aren't supported"), "{warnings:?}");
        assert!(said("their own topics (news)"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Native Git workspace: every collection, its environments and the globals, the
    /// globals added beside the user's own rather than over them.
    #[test]
    fn a_workspace_brings_its_environments_and_globals_without_replacing_any() {
        let root = temp("ws-store");
        let ws = Workspace::open(root.clone()).unwrap();
        ws.set_var(None, "g2", "mine", false).unwrap();
        let dir = temp("ws-files");
        write(
            &dir,
            &[
                (
                    "postman/collections/Shop/List.request.yaml",
                    "$kind: http-request\nmethod: GET\nurl: https://x.test/items\n",
                ),
                (
                    "postman/collections/Chat/Live.request.yaml",
                    "$kind: websocket-request\nurl: wss://x.test/ws\n",
                ),
                (
                    "postman/environments/Dev.environment.yaml",
                    r#"name: Dev
values:
  - key: host
    value: dev.test
    enabled: true
    type: default
  - key: token
    value: s3cret
    enabled: true
    type: secret
"#,
                ),
                (
                    "postman/globals/workspace.globals.yaml",
                    "values:\n  - key: g1\n    value: one\n  - key: g2\n    value: theirs\n  - key: gs\n    value: hidden\n    type: secret\n",
                ),
                ("postman/specs/api.yaml", "openapi: 3.0.0\n"),
                (".postman/resources.yaml", "workspace: {}\n"),
            ],
        );
        let i = crate::import::path_into_workspace(&ws, &dir).unwrap();
        assert_eq!(i.folders, ["Chat", "Shop"]);
        assert_eq!(
            (i.requests, i.environments.as_slice(), i.globals),
            (2, ["Dev".to_owned()].as_slice(), 2)
        );
        let said = |what: &str| i.warnings.iter().any(|w| w.contains(what));
        assert!(
            said("specs/ left out") && said("g2 is already set"),
            "{:?}",
            i.warnings
        );

        let (globals, secret_globals) = ws.load_env(None).unwrap();
        assert_eq!(
            secret_globals[0].key, "gs",
            "a secret global stays on this machine"
        );
        let value = |key: &str| {
            globals
                .iter()
                .find(|kv| kv.key == key)
                .map(|kv| kv.value.clone())
        };
        assert_eq!(
            (value("g1"), value("g2")),
            (Some("one".into()), Some("mine".into()))
        );
        let (shared, secret) = ws.load_env(Some("Dev")).unwrap();
        assert_eq!(
            (shared[0].key.as_str(), secret[0].key.as_str()),
            ("host", "token"),
            "secret type stays local"
        );

        // Postman's v2.1 globals export is the same thing.
        let v21 = r#"{"name": "My Workspace Globals", "_postman_variable_scope": "globals",
                      "values": [{"key": "g", "value": "1", "enabled": true}]}"#;
        assert!(
            matches!(crate::import::parse(v21), Ok(Import::Globals { shared, .. }) if shared[0].key == "g")
        );
        let _ = (
            std::fs::remove_dir_all(&root),
            std::fs::remove_dir_all(&dir),
        );
    }

    /// One document, as Postman's app holds a collection (pasted or dropped as a file): its
    /// top may have no `$kind`, only children that are v3 requests.
    #[test]
    fn one_v3_document_from_postman_s_app_model_imports() {
        let text = r#"{"format": "postman-v3-internal", "collection": {
            "title": "Chat",
            "payload": {"variables": [{"key": "host", "value": "x.test"}]},
            "children": [
              {"$kind": "websocket-request", "name": "Room", "url": "wss://{{host}}/ws",
               "scripts": [{"type": "onIncomingMessage", "code": ""}],
               "messages": [{"content": "hi"}, {"type": "ws-raw-message", "payload": {"type": "json", "content": "{}"}}],
               "examples": [{"$kind": "websocket-example", "name": "Session", "response": {"events": []}}]},
              {"$kind": "graphql-request", "name": "Me", "url": "https://{{host}}/graphql",
               "query": "{ me { id } }", "variables": "{}",
               "examples": [{"$kind": "graphql-example", "name": "Ok",
                 "response": {"http": {"status": "200 OK", "body": "{\"data\":{}}",
                   "headers": [{"key": "content-type", "value": "application/json"}]}}}]}
            ]}}"#;
        let Ok(Import::Collection {
            name,
            folders,
            requests,
            warnings,
            ..
        }) = crate::import::parse(text)
        else {
            panic!("a collection");
        };
        assert_eq!(name, "Chat");
        assert_eq!(find(&folders, "").vars[0].key, "host");
        assert_eq!(find(&requests, "Room").messages, ["hi", "{}"]);
        let example = &find(&requests, "Me").examples[0];
        assert_eq!(
            (example.status, example.content_type.as_str()),
            (200, "application/json")
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("\"Session\" (websocket-example) isn't kept")),
            "{warnings:?}"
        );
        // Postman's empty script tabs aren't something lost.
        assert!(
            !warnings.iter().any(|w| w.contains("onIncomingMessage")),
            "{warnings:?}"
        );
    }
}
