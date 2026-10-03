//! Postman collections (v2.1, and v2.0's auth) and environments as JSON: what Postman's
//! Import takes and its Export gives, to bring requests over from Postman and back.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::model::{
    self, Auth, AwsV4, Body, Example, Folder, Grant, Jwt, KeyValue, OAuth1, OAuth2, Request,
    Settings,
};
use crate::store::{Node, Workspace, folder_name, safe_name};

const SCHEMA: &str = "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";

/// An environment's name, shared and secret variables.
pub type Env = (String, Vec<KeyValue>, Vec<KeyValue>);

pub enum Import {
    /// Keys are below the collection's own folder, which is "".
    Collection {
        name: String,
        folders: Vec<(String, Folder)>,
        requests: Vec<(String, Request)>,
        /// What didn't come over as it was, one line each.
        warnings: Vec<String>,
        /// Saved next to the collection.
        environments: Vec<Env>,
    },
    Environment {
        name: String,
        shared: Vec<KeyValue>,
        secret: Vec<KeyValue>,
    },
}

pub fn from_value(v: &Value) -> Result<Import, String> {
    let v = v.clone();
    if let Some(values) = v["values"].as_array() {
        return Ok(environment(&v, values));
    }
    if !v["item"].is_array() {
        return Err("not a Postman collection or environment".into());
    }
    let mut c = Reader::default();
    c.group("", &v, &v["info"]["description"]);
    Ok(Import::Collection {
        name: safe_name(str_of(&v["info"]["name"])),
        folders: c.folders,
        requests: c.requests,
        warnings: c.warnings,
        environments: Vec::new(),
    })
}

#[derive(Default)]
struct Reader {
    folders: Vec<(String, Folder)>,
    requests: Vec<(String, Request)>,
    warnings: Vec<String>,
}

impl Reader {
    fn warn(&mut self, key: &str, what: impl std::fmt::Display) {
        let at = if key.is_empty() { "collection" } else { key };
        self.warnings.push(format!("{at}: {what}"));
    }

    /// An auth apitool doesn't have becomes none, rather than silently inheriting another.
    fn auth(&mut self, key: &str, v: &Value) -> Auth {
        auth(v).unwrap_or_else(|kind| {
            self.warn(key, format!("{kind} auth isn't supported; set to none"));
            Auth::None
        })
    }

    /// The collection itself, or a folder in it.
    fn group(&mut self, key: &str, v: &Value, description: &Value) {
        let folder = Folder {
            description: description_of(description),
            vars: rows(&v["variable"]),
            auth: self.auth(key, &v["auth"]),
            pre_request: script(&v["event"], "prerequest"),
            tests: script(&v["event"], "test"),
            order: Vec::new(),
        };
        let at = self.folders.len();
        self.folders.push((key.to_owned(), folder));
        let mut order = Vec::new();
        // Postman allows the same name twice; files (in an export) don't, nor do they
        // tell case apart on macOS and Windows.
        let mut taken = HashSet::new();
        for item in v["item"].as_array().into_iter().flatten() {
            let base = safe_name(str_of(&item["name"]));
            let name = (1..)
                .map(|n| match n {
                    1 => base.clone(),
                    n => format!("{base} {n}"),
                })
                .find(|n| taken.insert(n.to_lowercase()))
                .expect("some name is free");
            let child = match key.is_empty() {
                true => name.clone(),
                false => format!("{key}/{name}"),
            };
            match item["item"].is_array() {
                true => {
                    order.push(format!("{name}/"));
                    self.group(&child, item, &item["description"]);
                }
                false => {
                    order.push(name);
                    self.request(&child, item);
                }
            }
        }
        // Postman's order is the author's; the tree would otherwise sort by name.
        self.folders[at].1.order = Folder::order_of(order);
    }

    fn request(&mut self, key: &str, item: &Value) {
        let r = &item["request"];
        let mut req = Request::default();
        let method = str_of(&r["method"]).to_uppercase();
        if model::METHODS.contains(&method.as_str()) {
            req.method = method;
        } else if !method.is_empty() {
            self.warn(key, format!("method {method} isn't supported; set to GET"));
        }
        // A request may be just its URL.
        let url = if r.is_string() { r } else { &r["url"] };
        // ponytail: the raw URL, which Postman always writes; building one from host and
        // path parts waits for a collection without it.
        req.url = match url {
            Value::String(raw) => raw.clone(),
            v => str_of(&v["raw"]).to_owned(),
        };
        (req.params, req.path_vars) = (rows(&url["query"]), rows(&url["variable"]));
        req.headers = rows(&r["header"]);
        req.description = description_of(&r["description"]);
        req.body = self.body(key, &r["body"], &mut req.headers);
        if matches!(req.body, Body::GraphQL { .. }) {
            req.method = "GRAPHQL".into();
        }
        req.auth = self.auth(key, &r["auth"]);
        req.pre_request = script(&item["event"], "prerequest");
        req.tests = script(&item["event"], "test");
        req.settings = settings(&item["protocolProfileBehavior"]);
        let examples = item["response"].as_array().into_iter().flatten();
        req.examples = examples.map(example).collect();
        req.sync_params();
        self.requests.push((key.to_owned(), req));
    }

    fn body(&mut self, key: &str, b: &Value, headers: &mut Vec<KeyValue>) -> Body {
        let typed = |headers: &[KeyValue]| {
            let content_type = headers
                .iter()
                .find(|h| h.enabled && h.key.eq_ignore_ascii_case("content-type"));
            content_type.map(|h| h.value.clone())
        };
        match str_of(&b["mode"]) {
            "" => Body::None,
            "raw" => {
                let text = str_of(&b["raw"]).to_owned();
                let language = str_of(&b["options"]["raw"]["language"]);
                // Postman sends the Content-Type of the language it shows; here that's a header.
                let mime = match language {
                    "xml" => Some("application/xml"),
                    "html" => Some("text/html"),
                    "javascript" => Some("application/javascript"),
                    _ => None,
                };
                if let Some(mime) = mime.filter(|_| typed(headers).is_none()) {
                    headers.push(KeyValue::new("Content-Type", mime));
                }
                let json = language == "json" || typed(headers).is_some_and(|t| t.contains("json"));
                match (text.is_empty(), json) {
                    (true, _) => Body::None,
                    (false, true) => Body::Json { text },
                    (false, false) => Body::Text { text },
                }
            }
            "urlencoded" => Body::Form {
                fields: rows(&b["urlencoded"]),
            },
            "formdata" => {
                let rows_in = b["formdata"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let mut parts = rows(&b["formdata"]);
                for (part, row) in parts.iter_mut().zip(rows_in) {
                    if row["type"] != "file" {
                        continue;
                    }
                    let src = match &row["src"] {
                        Value::Array(files) => {
                            if files.len() > 1 {
                                let what =
                                    format!("part \"{}\" sends only its first file", part.key);
                                self.warn(key, what);
                            }
                            files.first().map_or("", str_of)
                        }
                        src => str_of(src),
                    };
                    if src.is_empty() {
                        self.warn(key, format!("file part \"{}\" has no file", part.key));
                    }
                    part.value = format!("@{src}");
                }
                Body::Multipart { parts }
            }
            "graphql" => Body::GraphQL {
                query: str_of(&b["graphql"]["query"]).into(),
                variables: str_of(&b["graphql"]["variables"]).into(),
            },
            "file" => Body::File {
                path: str_of(&b["file"]["src"]).into(),
            },
            mode => {
                self.warn(key, format!("a {mode} body isn't supported; left empty"));
                Body::None
            }
        }
    }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// Values may be numbers or booleans in Postman's JSON.
fn value_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A string, or `{ "content": … }`.
fn description_of(v: &Value) -> String {
    match v {
        Value::Object(_) => str_of(&v["content"]).into(),
        v => str_of(v).into(),
    }
}

/// Query, header, form and variable rows.
fn rows(v: &Value) -> Vec<KeyValue> {
    let rows = v.as_array().map(Vec::as_slice).unwrap_or_default();
    rows.iter()
        .map(|r| KeyValue {
            key: str_of(&r["key"]).into(),
            value: value_of(&r["value"]),
            enabled: r["disabled"] != true,
            description: description_of(&r["description"]),
        })
        .collect()
}

fn script(events: &Value, listen: &str) -> String {
    let events = events.as_array().map(Vec::as_slice).unwrap_or_default();
    let scripts: Vec<String> = events
        .iter()
        .filter(|e| e["listen"] == listen && e["disabled"] != true)
        .map(|e| match &e["script"]["exec"] {
            Value::Array(lines) => lines.iter().map(str_of).collect::<Vec<_>>().join("\n"),
            exec => str_of(exec).to_owned(),
        })
        .collect();
    scripts.join("\n")
}

/// `Err` names a kind apitool doesn't have.
fn auth(v: &Value) -> Result<Auth, String> {
    let p = |name| param(v, name);
    Ok(match str_of(&v["type"]) {
        "" | "inherit" => Auth::Inherit,
        "noauth" => Auth::None,
        "bearer" => Auth::Bearer { token: p("token") },
        "basic" => Auth::Basic {
            username: p("username"),
            password: p("password"),
        },
        "digest" => Auth::Digest {
            username: p("username"),
            password: p("password"),
        },
        "oauth2" => {
            let grant = match p("grant_type").as_str() {
                "client_credentials" => Grant::ClientCredentials,
                "password_credentials" => Grant::Password,
                "implicit" => Grant::Implicit,
                // "" is Postman's default.
                "" | "authorization_code" | "authorization_code_with_pkce" => {
                    Grant::AuthorizationCode
                }
                other => return Err(format!("OAuth 2.0 {other}")),
            };
            Auth::OAuth2(OAuth2 {
                grant,
                token_url: p("accessTokenUrl"),
                client_id: p("clientId"),
                client_secret: p("clientSecret"),
                scope: p("scope"),
                username: p("username"),
                password: p("password"),
                auth_url: p("authUrl"),
                redirect_uri: p("redirect_uri"),
            })
        }
        "apikey" => Auth::ApiKey {
            key: p("key"),
            value: p("value"),
            in_query: p("in") == "query",
        },
        // A presigned URL (auth data in the query) is a different signature: refused, so the
        // import says so instead of sending headers the server doesn't expect.
        "awsv4" if p("addAuthDataToQuery") == "true" => {
            return Err("awsv4 (auth data in the query)".into());
        }
        // Only the header form, which is all apitool sends.
        "oauth1" if p("addParamsToHeader") == "false" => {
            return Err("oauth1 (parameters in the body or query)".into());
        }
        "oauth1" => Auth::OAuth1(OAuth1 {
            consumer_secret: match p("signatureMethod").starts_with("RSA") {
                true => p("privateKey"),
                false => p("consumerSecret"),
            },
            signature_method: p("signatureMethod"),
            consumer_key: p("consumerKey"),
            token: p("token"),
            token_secret: p("tokenSecret"),
            realm: p("realm"),
        }),
        // Only the header form: a query-parameter token or base64 secret would go out wrong.
        "jwt" if p("addTokenTo") == "queryParam" => {
            return Err("jwt (token in the query)".into());
        }
        "jwt" if p("isSecretBase64Encoded") == "true" => {
            return Err("jwt (base64-encoded secret)".into());
        }
        "jwt" => {
            let algorithm = p("algorithm");
            Auth::Jwt(Jwt {
                secret: match algorithm.starts_with("HS") {
                    true => p("secret"),
                    false => p("privateKey"),
                },
                algorithm,
                payload: p("payload"),
            })
        }
        "awsv4" => Auth::AwsV4(AwsV4 {
            access_key: p("accessKey"),
            secret_key: p("secretKey"),
            region: p("region"),
            service: p("service"),
            session_token: p("sessionToken"),
        }),
        other => return Err(other.into()),
    })
}

/// An auth setting: v2.1 lists `{key, value}` rows, v2.0 has an object.
fn param(auth: &Value, name: &str) -> String {
    match &auth[str_of(&auth["type"])] {
        Value::Array(rows) => rows
            .iter()
            .find(|r| r["key"] == name)
            .map_or_else(String::new, |r| value_of(&r["value"])),
        settings => value_of(&settings[name]),
    }
}

fn settings(p: &Value) -> Settings {
    let mut s = Settings::default();
    if let Some(b) = p["followRedirects"].as_bool() {
        s.follow_redirects = b;
    }
    if let Some(n) = p["maxRedirects"].as_u64() {
        s.max_redirects = n as u32;
    }
    if let Some(b) = p["strictSSL"].as_bool() {
        s.verify_tls = b;
    }
    if let Some(b) = p["disableCookies"].as_bool() {
        s.cookies = !b;
    }
    s
}

fn example(r: &Value) -> Example {
    let content_type = rows(&r["header"])
        .into_iter()
        .find(|h| h.key.eq_ignore_ascii_case("content-type"));
    Example {
        name: str_of(&r["name"]).into(),
        status: r["code"].as_u64().unwrap_or(200) as u16,
        content_type: content_type.map(|h| h.value).unwrap_or_default(),
        body: str_of(&r["body"]).into(),
    }
}

/// Postman's secret type stays on this machine; the rest is shared.
fn environment(v: &Value, values: &[Value]) -> Import {
    let (mut shared, mut secret) = (Vec::new(), Vec::new());
    for row in values {
        let kv = KeyValue {
            key: str_of(&row["key"]).into(),
            value: value_of(&row["value"]),
            enabled: row["enabled"] != false,
            description: String::new(),
        };
        match row["type"] == "secret" {
            true => secret.push(kv),
            false => shared.push(kv),
        }
    }
    Import::Environment {
        name: safe_name(str_of(&v["name"])),
        shared,
        secret,
    }
}

/// The requests under `dir` (the collections root or a folder) as a Postman collection,
/// with how many requests it holds and how many were left out: Postman keeps WebSocket,
/// SSE and gRPC requests outside collections.
pub fn collection(ws: &Workspace, dir: &Path) -> Result<(String, usize, usize), String> {
    let root = ws.collections();
    let tree = ws.tree();
    let (name, nodes) = match dir == root {
        true => ("API".to_owned(), &tree[..]),
        false => {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            let nodes = crate::docs::find(&tree, dir)
                .ok_or_else(|| format!("no folder at {}", folder_name(&root, dir)))?;
            (name.into_owned(), nodes)
        }
    };
    let mut counts = (0, 0);
    let mut body = group(ws, &ws.load_folder(dir)?, nodes, &mut counts)?;
    let mut info = json!({ "name": name, "schema": SCHEMA });
    // A collection's description lives in its info.
    if let Some(description) = body.remove("description") {
        info["description"] = description;
    }
    let mut out = Map::from_iter([("info".to_owned(), info)]);
    out.extend(body);
    let text = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
    Ok((text, counts.0, counts.1))
}

fn group(
    ws: &Workspace,
    folder: &Folder,
    nodes: &[Node],
    counts: &mut (usize, usize),
) -> Result<Map<String, Value>, String> {
    let mut items = Vec::new();
    for node in nodes {
        match node {
            Node::Folder {
                name,
                path,
                children,
            } => {
                let inner = group(ws, &ws.load_folder(path)?, children, counts)?;
                let mut g = Map::from_iter([("name".to_owned(), json!(name))]);
                g.extend(inner);
                items.push(Value::Object(g));
            }
            Node::Request { method, .. }
                if matches!(method.as_str(), "WS" | "SSE" | "GRPC" | "MQTT" | "SOCKETIO") =>
            {
                counts.1 += 1
            }
            Node::Request { name, path, .. } => {
                items.push(item(name, &ws.load_request(path)?));
                counts.0 += 1;
            }
        }
    }
    let mut g = Map::new();
    g.insert("item".into(), Value::Array(items));
    if !folder.description.is_empty() {
        g.insert("description".into(), json!(folder.description));
    }
    if let Some(auth) = auth_json(&folder.auth) {
        g.insert("auth".into(), auth);
    }
    let events = events_json(&folder.pre_request, &folder.tests);
    if !events.is_empty() {
        g.insert("event".into(), Value::Array(events));
    }
    if !folder.vars.is_empty() {
        g.insert("variable".into(), rows_json(&folder.vars));
    }
    Ok(g)
}

fn item(name: &str, req: &Request) -> Value {
    let graphql = req.method == "GRAPHQL";
    let mut r = Map::new();
    let method = if graphql { "POST" } else { &req.method };
    r.insert("method".into(), json!(method));
    if let Some(auth) = auth_json(&req.auth) {
        r.insert("auth".into(), auth);
    }
    r.insert("header".into(), rows_json(&req.headers));
    if let Some(body) = body_json(&req.body) {
        r.insert("body".into(), body);
    }
    r.insert("url".into(), url_json(req));
    if !req.description.is_empty() {
        r.insert("description".into(), json!(req.description));
    }
    let mut item = Map::from_iter([("name".to_owned(), json!(name))]);
    let events = events_json(&req.pre_request, &req.tests);
    if !events.is_empty() {
        item.insert("event".into(), Value::Array(events));
    }
    let behavior = behavior_json(&req.settings);
    if !behavior.is_empty() {
        item.insert("protocolProfileBehavior".into(), Value::Object(behavior));
    }
    item.insert("request".into(), Value::Object(r));
    let examples = req.examples.iter().map(example_json);
    item.insert("response".into(), examples.collect());
    Value::Object(item)
}

fn rows_json(rows: &[KeyValue]) -> Value {
    let row = |kv: &KeyValue| {
        let mut r = json!({ "key": kv.key, "value": kv.value });
        if !kv.enabled {
            r["disabled"] = json!(true);
        }
        if !kv.description.is_empty() {
            r["description"] = json!(kv.description);
        }
        r
    };
    rows.iter().map(row).collect()
}

/// Postman reads the parts, not `raw`, so they're written the way its own exports have them.
fn url_json(req: &Request) -> Value {
    let raw = req.url.as_str();
    let base = raw.split(['?', '#']).next().unwrap_or_default();
    let (protocol, rest) = match base.split_once("://") {
        Some((p, rest)) => (Some(p), rest),
        None => (None, base),
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    };
    let mut url = json!({ "raw": raw, "host": host.split('.').collect::<Vec<_>>() });
    if let Some(protocol) = protocol {
        url["protocol"] = json!(protocol);
    }
    if let Some(port) = port {
        url["port"] = json!(port);
    }
    if !path.is_empty() {
        url["path"] = json!(path.split('/').collect::<Vec<_>>());
    }
    if !req.params.is_empty() {
        url["query"] = rows_json(&req.params);
    }
    if !req.path_vars.is_empty() {
        url["variable"] = rows_json(&req.path_vars);
    }
    url
}

fn body_json(body: &Body) -> Option<Value> {
    Some(match body {
        Body::None => return None,
        Body::Json { text } => {
            json!({ "mode": "raw", "raw": text, "options": { "raw": { "language": "json" } } })
        }
        Body::Text { text } => json!({ "mode": "raw", "raw": text }),
        Body::Form { fields } => json!({ "mode": "urlencoded", "urlencoded": rows_json(fields) }),
        Body::Multipart { parts } => {
            let mut rows = rows_json(parts);
            for (row, part) in rows.as_array_mut().into_iter().flatten().zip(parts) {
                match part.value.strip_prefix('@') {
                    Some(file) => {
                        row["type"] = json!("file");
                        row["src"] = json!(file);
                        if let Some(row) = row.as_object_mut() {
                            row.remove("value");
                        }
                    }
                    None => row["type"] = json!("text"),
                }
            }
            json!({ "mode": "formdata", "formdata": rows })
        }
        Body::File { path } => json!({ "mode": "file", "file": { "src": path } }),
        Body::GraphQL { query, variables } => {
            json!({ "mode": "graphql", "graphql": { "query": query, "variables": variables } })
        }
    })
}

fn auth_json(auth: &Auth) -> Option<Value> {
    let typed = |kind: &str, settings: &[(&str, &str)]| {
        let rows: Vec<Value> = settings
            .iter()
            .map(|(k, v)| json!({ "key": k, "value": v, "type": "string" }))
            .collect();
        json!({ "type": kind, kind: rows })
    };
    Some(match auth {
        Auth::Inherit => return None,
        Auth::None => json!({ "type": "noauth" }),
        Auth::Bearer { token } => typed("bearer", &[("token", token.as_str())]),
        Auth::Basic { username, password } => typed(
            "basic",
            &[
                ("username", username.as_str()),
                ("password", password.as_str()),
            ],
        ),
        Auth::Digest { username, password } => typed(
            "digest",
            &[
                ("username", username.as_str()),
                ("password", password.as_str()),
            ],
        ),
        Auth::ApiKey {
            key,
            value,
            in_query,
        } => typed(
            "apikey",
            &[
                ("key", key.as_str()),
                ("value", value.as_str()),
                ("in", if *in_query { "query" } else { "header" }),
            ],
        ),
        Auth::OAuth1(o) => typed(
            "oauth1",
            &[
                ("signatureMethod", o.signature_method.as_str()),
                ("consumerKey", o.consumer_key.as_str()),
                match o.signature_method.starts_with("RSA") {
                    true => ("privateKey", o.consumer_secret.as_str()),
                    false => ("consumerSecret", o.consumer_secret.as_str()),
                },
                ("token", o.token.as_str()),
                ("tokenSecret", o.token_secret.as_str()),
                ("realm", o.realm.as_str()),
                ("version", "1.0"),
                ("addParamsToHeader", "true"),
            ],
        ),
        Auth::Jwt(j) => typed(
            "jwt",
            &[
                ("algorithm", j.algorithm.as_str()),
                match j.algorithm.starts_with("HS") {
                    true => ("secret", j.secret.as_str()),
                    false => ("privateKey", j.secret.as_str()),
                },
                ("payload", j.payload.as_str()),
                ("addTokenTo", "header"),
                ("headerPrefix", "Bearer"),
            ],
        ),
        Auth::AwsV4(a) => typed(
            "awsv4",
            &[
                ("accessKey", a.access_key.as_str()),
                ("secretKey", a.secret_key.as_str()),
                ("region", a.region.as_str()),
                ("service", a.service.as_str()),
                ("sessionToken", a.session_token.as_str()),
            ],
        ),
        Auth::OAuth2(o) => {
            let grant = match o.grant {
                Grant::ClientCredentials => "client_credentials",
                Grant::Password => "password_credentials",
                Grant::AuthorizationCode => "authorization_code_with_pkce",
                Grant::Implicit => "implicit",
            };
            typed(
                "oauth2",
                &[
                    ("grant_type", grant),
                    ("accessTokenUrl", o.token_url.as_str()),
                    ("clientId", o.client_id.as_str()),
                    ("clientSecret", o.client_secret.as_str()),
                    ("scope", o.scope.as_str()),
                    ("username", o.username.as_str()),
                    ("password", o.password.as_str()),
                    ("authUrl", o.auth_url.as_str()),
                    ("redirect_uri", o.redirect_uri.as_str()),
                ],
            )
        }
    })
}

/// Lines split on '\n' alone, so a script's last newline comes back on import.
fn events_json(pre_request: &str, tests: &str) -> Vec<Value> {
    [("prerequest", pre_request), ("test", tests)]
        .into_iter()
        .filter(|(_, script)| !script.trim().is_empty())
        .map(|(listen, script)| {
            let exec: Vec<&str> = script.split('\n').collect();
            json!({ "listen": listen, "script": { "type": "text/javascript", "exec": exec } })
        })
        .collect()
}

/// ponytail: Postman has no per-request HTTP version or timeout, so those stay behind.
fn behavior_json(s: &Settings) -> Map<String, Value> {
    let d = Settings::default();
    let mut p = Map::new();
    if s.follow_redirects != d.follow_redirects {
        p.insert("followRedirects".into(), json!(s.follow_redirects));
    }
    if s.max_redirects != d.max_redirects {
        p.insert("maxRedirects".into(), json!(s.max_redirects));
    }
    if s.verify_tls != d.verify_tls {
        p.insert("strictSSL".into(), json!(s.verify_tls));
    }
    if s.cookies != d.cookies {
        p.insert("disableCookies".into(), json!(!s.cookies));
    }
    p
}

fn example_json(e: &Example) -> Value {
    let header = match e.content_type.is_empty() {
        true => json!([]),
        false => json!([{ "key": "Content-Type", "value": e.content_type }]),
    };
    json!({ "name": e.name, "code": e.status, "header": header, "body": e.body })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Shaped like Postman's own export, with what people actually put in collections.
    const SHOP: &str = r#"{
      "info": { "_postman_id": "1", "name": "Shop: API", "description": "The shop.",
                "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json" },
      "item": [
        { "name": "users", "description": { "content": "User endpoints", "type": "text/markdown" },
          "auth": { "type": "basic", "basic": { "username": "u", "password": "p" } },
          "item": [
            { "name": "get user",
              "event": [{ "listen": "test", "script": { "type": "text/javascript",
                "exec": ["pm.test('ok', function () {", "    pm.response.to.have.status(200);", "});"] } }],
              "request": { "method": "GET",
                "header": [{ "key": "Accept", "value": "application/json", "description": "always JSON" },
                           { "key": "X-Debug", "value": "1", "disabled": true }],
                "url": { "raw": "{{base}}/users/:id?fields=name", "host": ["{{base}}"], "path": ["users", ":id"],
                         "query": [{ "key": "fields", "value": "name", "description": "comma separated" },
                                   { "key": "page", "value": "2", "disabled": true }],
                         "variable": [{ "key": "id", "value": "{{userId}}" }] } },
              "response": [{ "name": "found", "code": 200, "status": "OK",
                             "header": [{ "key": "Content-Type", "value": "application/json" }],
                             "body": "{\"id\": 1}" }] },
            { "name": "Get User", "request": "{{base}}/users/me" }
          ] },
        { "name": "create",
          "protocolProfileBehavior": { "followRedirects": false, "strictSSL": false, "disableCookies": true },
          "request": { "method": "post", "url": "{{base}}/items", "header": [],
            "body": { "mode": "raw", "raw": "{\"a\": 1}", "options": { "raw": { "language": "json" } } },
            "auth": { "type": "apikey", "apikey": [{ "key": "key", "value": "X-Api-Key" },
                      { "key": "value", "value": "{{key}}" }, { "key": "in", "value": "header" }] } } },
        { "name": "upload/avatar",
          "request": { "method": "PUT", "url": { "raw": "{{base}}/avatar" },
            "body": { "mode": "formdata", "formdata": [{ "key": "note", "value": "hi", "type": "text" },
                      { "key": "file", "type": "file", "src": "/tmp/a.png" }] } } },
        { "name": "login",
          "request": { "method": "POST", "url": "{{base}}/login",
            "body": { "mode": "urlencoded", "urlencoded": [{ "key": "user", "value": "{{user}}" }] },
            "auth": { "type": "ntlm", "ntlm": [] } } },
        { "name": "feed", "request": { "method": "GET", "url": "{{base}}/feed?x=1",
            "auth": { "type": "apikey", "apikey": [{ "key": "key", "value": "api_key" },
                      { "key": "value", "value": "k" }, { "key": "in", "value": "query" }] } } },
        { "name": "me", "request": { "method": "POST", "url": "{{base}}/graphql",
            "body": { "mode": "graphql", "graphql": { "query": "{ me { id } }", "variables": "{}" } } } },
        { "name": "binary", "request": { "method": "POST", "url": "{{base}}/bin",
            "body": { "mode": "file", "file": { "src": "x.bin" } } } }
      ],
      "auth": { "type": "bearer", "bearer": [{ "key": "token", "value": "{{token}}", "type": "string" }] },
      "event": [{ "listen": "prerequest", "script": { "type": "text/javascript",
                  "exec": ["pm.variables.set('t', Date.now());"] } }],
      "variable": [{ "key": "base", "value": "https://shop.test" }, { "key": "retries", "value": 3 }]
    }"#;

    #[test]
    fn a_postman_export_comes_over_with_what_apitool_can_hold() {
        let Import::Collection {
            name,
            folders,
            requests,
            warnings,
            ..
        } = crate::import::parse(SHOP).unwrap()
        else {
            panic!("not a collection");
        };
        assert_eq!(
            name, "Shop- API",
            "a name that can't be a file name is cleaned up"
        );
        let folders: HashMap<_, _> = folders.into_iter().collect();
        // The author's order, not the tree's alphabetical one.
        let order = &folders[""].order;
        assert_eq!(
            (&order[..2], order.last().unwrap().as_str()),
            (&["users/".to_owned(), "create".to_owned()][..], "binary")
        );
        let top = &folders[""];
        assert_eq!(top.description, "The shop.");
        let retries = KeyValue::new("retries", "3");
        assert_eq!(
            top.vars,
            [KeyValue::new("base", "https://shop.test"), retries]
        );
        assert_eq!(
            top.auth,
            Auth::Bearer {
                token: "{{token}}".into()
            }
        );
        assert_eq!(top.pre_request, "pm.variables.set('t', Date.now());");
        // v2.0's object form of auth settings reads the same.
        let users = &folders["users"];
        assert_eq!(users.description, "User endpoints");
        let basic = Auth::Basic {
            username: "u".into(),
            password: "p".into(),
        };
        assert_eq!(users.auth, basic);

        let r: HashMap<_, _> = requests.into_iter().collect();
        let mut keys: Vec<_> = r.keys().map(String::as_str).collect();
        keys.sort();
        // Same name twice (case aside) and a '/' in a name: neither may clobber another.
        let expected = ["binary", "create", "feed", "login", "me", "upload-avatar"];
        assert_eq!(&keys[..6], expected);
        assert_eq!(&keys[6..], ["users/Get User 2", "users/get user"]);

        let get = &r["users/get user"];
        assert_eq!(get.url, "{{base}}/users/:id?fields=name");
        assert_eq!(get.path_vars, [KeyValue::new("id", "{{userId}}")]);
        let fields = KeyValue {
            description: "comma separated".into(),
            ..KeyValue::new("fields", "name")
        };
        let page = KeyValue {
            enabled: false,
            ..KeyValue::new("page", "2")
        };
        assert_eq!(get.params, [fields, page]);
        assert_eq!(get.headers[0].description, "always JSON");
        assert!(!get.headers[1].enabled);
        assert_eq!(
            get.auth,
            Auth::Inherit,
            "no auth of its own inherits, as in Postman"
        );
        assert!(
            get.tests
                .starts_with("pm.test('ok', function () {\n    pm.response")
        );
        assert_eq!(
            (
                get.examples[0].status,
                get.examples[0].content_type.as_str()
            ),
            (200, "application/json")
        );
        assert_eq!(r["users/Get User 2"].url, "{{base}}/users/me");

        let create = &r["create"];
        assert_eq!(create.method, "POST");
        assert_eq!(
            create.body,
            Body::Json {
                text: "{\"a\": 1}".into()
            }
        );
        assert!(create.headers.is_empty());
        let key = Auth::ApiKey {
            key: "X-Api-Key".into(),
            value: "{{key}}".into(),
            in_query: false,
        };
        assert_eq!(create.auth, key, "the key replaces the folder's auth");
        let s = &create.settings;
        assert!(!s.follow_redirects && !s.verify_tls && !s.cookies);
        // Stays an auth setting, so the URL isn't touched.
        assert_eq!(r["feed"].url, "{{base}}/feed?x=1");
        assert!(matches!(
            r["feed"].auth,
            Auth::ApiKey { in_query: true, .. }
        ));

        let upload = &r["upload-avatar"];
        let parts = vec![
            KeyValue::new("note", "hi"),
            KeyValue::new("file", "@/tmp/a.png"),
        ];
        assert_eq!(upload.body, Body::Multipart { parts });
        assert!(matches!(r["login"].body, Body::Form { .. }));
        assert_eq!(r["me"].method, "GRAPHQL");

        // What couldn't come over is said, not dropped quietly.
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("login: ntlm auth"), "{warnings:?}");
        let file = Body::File {
            path: "x.bin".into(),
        };
        assert_eq!(r["binary"].body, file);
        assert_eq!(body_json(&file).unwrap()["file"]["src"], "x.bin");
        assert_eq!(r["login"].auth, Auth::None);
    }

    #[test]
    fn environments_keep_secrets_on_the_secret_side() {
        let env = r#"{ "name": "Prod", "values": [
            { "key": "host", "value": "h", "type": "default", "enabled": true },
            { "key": "token", "value": "s", "type": "secret", "enabled": true },
            { "key": "old", "value": "o", "enabled": false } ],
          "_postman_variable_scope": "environment" }"#;
        let Import::Environment {
            name,
            shared,
            secret,
        } = crate::import::parse(env).unwrap()
        else {
            panic!("not an environment");
        };
        assert_eq!(name, "Prod");
        let old = KeyValue {
            enabled: false,
            ..KeyValue::new("old", "o")
        };
        assert_eq!(shared, [KeyValue::new("host", "h"), old]);
        assert_eq!(secret, [KeyValue::new("token", "s")]);
        assert!(crate::import::parse("{}").is_err() && crate::import::parse("not json").is_err());
    }

    /// What goes to Postman must come back the same: a team may move both ways.
    #[test]
    fn a_folder_goes_to_postman_and_back_unchanged() {
        let root = std::env::temp_dir().join(format!("apitool-postman-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "api").unwrap();
        let admin = ws.create_folder(&api, "admin").unwrap();
        let folder = Folder {
            description: "The API".into(),
            vars: vec![KeyValue::new("base", "https://x.test:8443/v1")],
            auth: Auth::OAuth2(OAuth2 {
                grant: Grant::Password,
                token_url: "{{base}}/token".into(),
                client_id: "c".into(),
                client_secret: "{{secret}}".into(),
                scope: "read".into(),
                username: "u".into(),
                password: "p".into(),
                ..Default::default()
            }),
            pre_request: "console.log(1);\n".into(),
            tests: "pm.test('x', () => {});".into(),
            order: Vec::new(),
        };
        ws.save_folder(&api, &folder).unwrap();
        let sub = Folder {
            auth: Auth::Digest {
                username: "d".into(),
                password: "{{pw}}".into(),
            },
            ..Default::default()
        };
        ws.save_folder(&admin, &sub).unwrap();
        let off = KeyValue {
            enabled: false,
            ..KeyValue::new("debug", "1")
        };
        let described = KeyValue {
            description: "who".into(),
            ..KeyValue::new("X-User", "{{u}}")
        };
        let requests = [
            (
                "api/list",
                Request {
                    url: "{{base}}/items?page=2".into(),
                    params: vec![KeyValue::new("page", "2"), off.clone()],
                    headers: vec![described, off.clone()],
                    description: "Lists **items**.".into(),
                    tests: "pm.test('ok', function () {\n});\n".into(),
                    settings: Settings {
                        follow_redirects: false,
                        max_redirects: 3,
                        verify_tls: false,
                        cookies: false,
                        ..Default::default()
                    },
                    examples: vec![Example {
                        name: "ok".into(),
                        status: 200,
                        content_type: "application/json".into(),
                        body: "[]".into(),
                    }],
                    ..Default::default()
                },
            ),
            (
                "api/admin/create",
                Request {
                    method: "POST".into(),
                    url: "http://localhost:3000/items/:kind".into(),
                    path_vars: vec![KeyValue {
                        description: "book or pen".into(),
                        ..KeyValue::new("kind", "{{kind}}")
                    }],
                    body: Body::Json {
                        text: "{\n  \"a\": 1\n}".into(),
                    },
                    auth: Auth::Bearer {
                        token: "{{t}}".into(),
                    },
                    ..Default::default()
                },
            ),
            (
                "api/admin/upload",
                Request {
                    method: "PUT".into(),
                    url: "{{base}}/files".into(),
                    body: Body::Multipart {
                        parts: vec![
                            KeyValue::new("note", "x"),
                            KeyValue::new("doc", "@files/a.txt"),
                            off.clone(),
                        ],
                    },
                    auth: Auth::None,
                    ..Default::default()
                },
            ),
            (
                "api/login",
                Request {
                    method: "POST".into(),
                    url: "{{base}}/login".into(),
                    body: Body::Form {
                        fields: vec![KeyValue::new("user", "{{u}}"), off],
                    },
                    auth: Auth::Basic {
                        username: "u".into(),
                        password: "p".into(),
                    },
                    ..Default::default()
                },
            ),
            (
                "api/note",
                Request {
                    method: "PATCH".into(),
                    url: "{{base}}/note".into(),
                    body: Body::Text {
                        text: "plain".into(),
                    },
                    auth: Auth::ApiKey {
                        key: "api_key".into(),
                        value: "{{k}}".into(),
                        in_query: true,
                    },
                    ..Default::default()
                },
            ),
            (
                "api/me",
                Request {
                    method: "GRAPHQL".into(),
                    url: "{{base}}/graphql".into(),
                    body: Body::GraphQL {
                        query: "{ me { id } }".into(),
                        variables: "{\"a\": 1}".into(),
                    },
                    auth: Auth::OAuth2(OAuth2 {
                        grant: Grant::AuthorizationCode,
                        auth_url: "{{base}}/authorize".into(),
                        token_url: "{{base}}/token".into(),
                        client_id: "spa".into(),
                        redirect_uri: "http://localhost:8765/cb".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            (
                "api/aws",
                Request {
                    method: "GET".into(),
                    url: "https://abc.execute-api.us-east-1.amazonaws.com/prod".into(),
                    auth: Auth::AwsV4(AwsV4 {
                        access_key: "{{ak}}".into(),
                        secret_key: "{{sk}}".into(),
                        region: "us-east-1".into(),
                        service: "execute-api".into(),
                        session_token: String::new(),
                    }),
                    ..Default::default()
                },
            ),
            (
                "api/jwt",
                Request {
                    url: "{{base}}/me".into(),
                    auth: Auth::Jwt(Jwt {
                        algorithm: "HS256".into(),
                        secret: "{{jwt_secret}}".into(),
                        payload: r#"{"sub": "ada"}"#.into(),
                    }),
                    ..Default::default()
                },
            ),
            (
                "api/oauth1",
                Request {
                    url: "{{base}}/me".into(),
                    auth: Auth::OAuth1(OAuth1 {
                        signature_method: "RSA-SHA256".into(),
                        consumer_key: "ck".into(),
                        consumer_secret: "{{private_key}}".into(),
                        token: "t".into(),
                        token_secret: "ts".into(),
                        realm: "r".into(),
                    }),
                    ..Default::default()
                },
            ),
            (
                "api/jwt-rs",
                Request {
                    url: "{{base}}/me".into(),
                    auth: Auth::Jwt(Jwt {
                        algorithm: "RS256".into(),
                        secret: "{{private_key}}".into(),
                        payload: "{}".into(),
                    }),
                    ..Default::default()
                },
            ),
            (
                "api/spa",
                Request {
                    method: "GET".into(),
                    url: "{{base}}/me".into(),
                    auth: Auth::OAuth2(OAuth2 {
                        grant: Grant::Implicit,
                        auth_url: "{{base}}/authorize".into(),
                        client_id: "spa".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            (
                "api/live",
                Request {
                    method: "WS".into(),
                    url: "ws://x".into(),
                    ..Default::default()
                },
            ),
        ];
        for (name, req) in &requests {
            ws.save_request(&ws.request_path(name).unwrap(), req)
                .unwrap();
        }

        let (json, count, skipped) = collection(&ws, &api).unwrap();
        assert_eq!(
            (count, skipped),
            (11, 1),
            "WebSocket has no place in a collection"
        );
        let Import::Collection {
            name,
            folders,
            requests: back,
            warnings,
            ..
        } = crate::import::parse(&json).unwrap()
        else {
            panic!("not a collection");
        };
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(name, "api");
        let folders: HashMap<_, _> = folders.into_iter().collect();
        assert_eq!(folders[""], folder);
        assert_eq!(folders["admin"], sub);
        let back: HashMap<_, _> = back.into_iter().collect();
        assert_eq!(back.len(), 11);
        for (name, _) in requests.iter().filter(|(n, _)| *n != "api/live") {
            let mut saved = ws.load_request(&ws.request_path(name).unwrap()).unwrap();
            saved.inherited = Default::default();
            assert_eq!(back[name.strip_prefix("api/").unwrap()], saved, "{name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
