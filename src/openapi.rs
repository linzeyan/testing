//! OpenAPI 3.x and Swagger 2.0 specs (JSON or YAML) as a collection: one request per
//! operation, a folder per tag, `{{baseUrl}}` from the first server, auth from the
//! security schemes, bodies and saved examples from the schemas' examples (or a sample
//! built from the schema), so Send and the mock server work right after the import.
//! And back: a folder's requests as an OpenAPI 3.1 document (`export`).

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::model::{Auth, Body, Example, Folder, Grant, KeyValue, OAuth2, Request};
use crate::postman::Import;
use crate::store::{Node, Workspace, copy_name, escape_name, folder_name};

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace", "query",
];
/// Schemas may refer to themselves (a tree node's children); samples stop this deep.
const MAX_DEPTH: usize = 8;

pub fn is_spec(v: &Value) -> bool {
    v["openapi"].is_string() || v["swagger"].is_string()
}

struct Spec<'a> {
    root: &'a Value,
    v3: bool,
    warnings: Vec<String>,
    keys: HashSet<String>,
}

pub fn import(root: &Value) -> Import {
    let mut s = Spec {
        root,
        v3: root["openapi"].is_string(),
        warnings: Vec::new(),
        keys: HashSet::new(),
    };
    let base = s.base_url();
    let mut top = Folder {
        description: str_of(&root["info"]["description"]).to_owned(),
        vars: vec![KeyValue::new("baseUrl", base)],
        ..Default::default()
    };
    if let Some(auth) = s.security(&root["security"], "") {
        top.auth = auth;
    }
    let mut folders = vec![(String::new(), top)];
    let mut tags = HashSet::new();
    for tag in root["tags"].as_array().into_iter().flatten() {
        let name = escape_name(str_of(&tag["name"]));
        if tags.insert(name.clone()) {
            let description = str_of(&tag["description"]).to_owned();
            folders.push((
                name,
                Folder {
                    description,
                    ..Default::default()
                },
            ));
        }
    }

    let mut requests = Vec::new();
    for (path, item) in root["paths"].as_object().into_iter().flatten() {
        let item = s.deref(item);
        for method in METHODS {
            let op = &item[*method];
            if !op.is_object() {
                continue;
            }
            let folder = op["tags"][0].as_str().map(escape_name).unwrap_or_default();
            if !folder.is_empty() && tags.insert(folder.clone()) {
                folders.push((folder.clone(), Folder::default()));
            }
            let name = [&op["summary"], &op["operationId"]]
                .into_iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .find(|n| !n.is_empty())
                .map_or_else(
                    || {
                        format!(
                            "{} {}",
                            method.to_uppercase(),
                            path.trim_matches('/').replace('/', " ")
                        )
                    },
                    str::to_owned,
                );
            let key = s.unique(&folder, &escape_name(&name));
            let req = s.request(method, path, item, op, &key);
            requests.push((key, req));
        }
    }
    let name = match str_of(&root["info"]["title"]).trim() {
        "" => "API".to_owned(),
        t => escape_name(t),
    };
    Import::Collection {
        name,
        folders,
        requests,
        warnings: s.warnings,
        environments: Vec::new(),
    }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// A parameter or field value as text: strings as they are, the rest as JSON.
fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

impl<'a> Spec<'a> {
    fn warn(&mut self, at: &str, what: impl std::fmt::Display) {
        let at = if at.is_empty() { "collection" } else { at };
        self.warnings.push(format!("{at}: {what}"));
    }

    /// `name`, or "name copy", … when the folder already has one (operations often share
    /// a summary).
    fn unique(&mut self, folder: &str, name: &str) -> String {
        let key = |n: &str| match folder {
            "" => n.to_owned(),
            f => format!("{f}/{n}"),
        };
        let free = std::iter::once(name.to_owned())
            .chain((1..).map(|n| copy_name(name, n)))
            .find(|n| !self.keys.contains(&key(n)))
            .expect("some name is free");
        let key = key(&free);
        self.keys.insert(key.clone());
        key
    }

    /// Follows local `$ref`s (`#/components/…`); an external one is left as it is.
    fn deref<'v>(&self, mut v: &'v Value) -> &'v Value
    where
        'a: 'v,
    {
        for _ in 0..16 {
            let Some(r) = v["$ref"].as_str() else {
                return v;
            };
            let Some(target) = r.strip_prefix('#').and_then(|p| self.root.pointer(p)) else {
                return v;
            };
            v = target;
        }
        v
    }

    fn base_url(&mut self) -> String {
        let r = self.root;
        let url = if self.v3 {
            let server = &r["servers"][0];
            let mut url = str_of(&server["url"]).trim_end_matches('/').to_owned();
            for (name, var) in server["variables"].as_object().into_iter().flatten() {
                url = url.replace(&format!("{{{name}}}"), &text(&var["default"]));
            }
            url
        } else {
            let schemes: Vec<&str> = r["schemes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let scheme = match schemes.contains(&"https") || schemes.is_empty() {
                true => "https",
                false => schemes[0],
            };
            match str_of(&r["host"]) {
                "" => str_of(&r["basePath"]).trim_end_matches('/').to_owned(),
                host => format!(
                    "{scheme}://{host}{}",
                    str_of(&r["basePath"]).trim_end_matches('/')
                ),
            }
        };
        if !url.contains("://") {
            self.warn(
                "",
                format!(
                    "the server URL \"{url}\" isn't absolute; set baseUrl in the folder settings"
                ),
            );
        }
        url
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        item: &'a Value,
        op: &'a Value,
        key: &str,
    ) -> Request {
        // Path-level parameters apply to every operation; the operation's own win.
        let mut params: Vec<&Value> = Vec::new();
        for p in [&op["parameters"], &item["parameters"]] {
            for p in p.as_array().into_iter().flatten() {
                let p = self.deref(p);
                let same = |q: &&Value| q["name"] == p["name"] && q["in"] == p["in"];
                if !params.iter().any(same) {
                    params.push(p);
                }
            }
        }
        let mut req = Request {
            method: method.to_uppercase(),
            ..Default::default()
        };
        let mut description = [&op["description"], &op["summary"]]
            .into_iter()
            .filter_map(Value::as_str)
            .find(|d| !d.trim().is_empty())
            .unwrap_or_default()
            .to_owned();
        if op["deprecated"] == true {
            description = format!("**Deprecated.** {description}").trim().to_owned();
        }
        req.description = description;

        // `{id}` becomes apitool's (and Postman's) `:id`.
        let mut url = String::from("{{baseUrl}}");
        let mut rest = path;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            url.push_str(&rest[..open]);
            url.push(':');
            url.push_str(&rest[open + 1..open + close]);
            rest = &rest[open + close + 1..];
        }
        url.push_str(rest);

        let mut query = Vec::new();
        let mut form = Vec::new();
        let mut body_schema = None;
        for p in params {
            let name = str_of(&p["name"]).to_owned();
            let value = self.param_value(p);
            let mut row = KeyValue::new(&name, value);
            row.description = str_of(&p["description"]).to_owned();
            match str_of(&p["in"]) {
                "path" => req.path_vars.push(row),
                "query" => {
                    // Optional ones are listed but off, so Send sends what is required.
                    row.enabled = p["required"] == true;
                    query.push(row);
                }
                "header" => req.headers.push(row),
                "body" => body_schema = Some(&p["schema"]),
                "formData" => {
                    if p["type"] == "file" {
                        row.value = String::new();
                        row.description = format!("a file: @path/to/file. {}", row.description)
                            .trim()
                            .to_owned();
                    }
                    form.push((row, p["type"] == "file"));
                }
                other => self.warn(key, format!("{other} parameter \"{name}\" left out")),
            }
        }
        if !query.is_empty() {
            let typed: Vec<String> = (query.iter())
                .filter(|q| q.enabled)
                .map(|q| format!("{}={}", q.key, q.value))
                .collect();
            if !typed.is_empty() {
                url = format!("{url}?{}", typed.join("&"));
            }
            req.params = query;
        }
        req.url = url;

        if self.v3 {
            let body = self.deref(&op["requestBody"]);
            if let Some(content) = body["content"].as_object() {
                self.body_v3(&mut req, content, key);
            }
        } else if let Some(schema) = body_schema {
            let sample = self.sample(schema, 0);
            req.body = Body::Json {
                text: serde_json::to_string_pretty(&sample).unwrap_or_default(),
            };
        } else if !form.is_empty() {
            let consumes = op["consumes"]
                .as_array()
                .or(self.root["consumes"].as_array());
            let multipart = form.iter().any(|(_, file)| *file)
                || consumes.is_some_and(|c| c.iter().any(|t| t == "multipart/form-data"));
            let fields = form.into_iter().map(|(row, _)| row).collect();
            req.body = match multipart {
                true => Body::Multipart { parts: fields },
                false => Body::Form { fields },
            };
        }

        if !op["security"].is_null()
            && let Some(auth) = self.security(&op["security"], key)
        {
            req.auth = auth;
        }
        req.examples = self.examples(op);
        req
    }

    fn param_value(&self, p: &Value) -> String {
        if !p["example"].is_null() {
            return text(&p["example"]);
        }
        if let Some(first) = p["examples"].as_object().and_then(|e| e.values().next()) {
            return text(&self.deref(first)["value"]);
        }
        let schema = match p["schema"].is_null() {
            true => p, // Swagger 2 puts type/default/enum on the parameter itself
            false => self.deref(&p["schema"]),
        };
        for k in ["example", "default"] {
            if !schema[k].is_null() {
                return text(&schema[k]);
            }
        }
        text(&schema["enum"][0])
    }

    fn body_v3(&mut self, req: &mut Request, content: &Map<String, Value>, key: &str) {
        let pick = |want: &dyn Fn(&str) -> bool| content.iter().find(|(t, _)| want(t));
        let json = pick(&|t| t.contains("json"));
        let urlencoded = pick(&|t| t == "application/x-www-form-urlencoded");
        let multipart = pick(&|t| t.starts_with("multipart/"));
        let text_like = pick(&|t| t.contains("xml") || t.starts_with("text/"));
        if let Some((mime, media)) = json {
            req.body = Body::Json {
                text: serde_json::to_string_pretty(&self.media_sample(media)).unwrap_or_default(),
            };
            if mime != "application/json" {
                req.headers.push(KeyValue::new("Content-Type", mime));
            }
        } else if let Some((mime, media)) = urlencoded.or(multipart) {
            let schema = self.deref(&media["schema"]);
            let sample = self.sample(schema, 0);
            let mut fields = Vec::new();
            for (name, prop) in schema["properties"].as_object().into_iter().flatten() {
                let prop = self.deref(prop);
                let file = prop["format"] == "binary" || prop["format"] == "base64";
                let mut row = KeyValue::new(
                    name,
                    if file {
                        String::new()
                    } else {
                        text(&sample[name])
                    },
                );
                row.description = match file {
                    true => "a file: @path/to/file".into(),
                    false => str_of(&prop["description"]).to_owned(),
                };
                fields.push(row);
            }
            req.body = match mime.starts_with("multipart/") {
                true => Body::Multipart { parts: fields },
                false => Body::Form { fields },
            };
        } else if let Some((mime, media)) = text_like {
            let sample = self.media_sample(media);
            req.body = Body::Text {
                text: match sample {
                    Value::String(s) => s,
                    v => v.to_string(),
                },
            };
            req.headers.push(KeyValue::new("Content-Type", mime));
        } else if let Some(mime) = content.keys().next() {
            self.warn(
                key,
                format!("a {mime} body isn't filled in; pick a file in Body"),
            );
            req.headers.push(KeyValue::new("Content-Type", mime));
        }
    }

    /// A media type's example, its first named example, else a sample of its schema.
    fn media_sample(&self, media: &Value) -> Value {
        if !media["example"].is_null() {
            return media["example"].clone();
        }
        if let Some(first) = media["examples"]
            .as_object()
            .and_then(|e| e.values().next())
        {
            return self.deref(first)["value"].clone();
        }
        self.sample(&media["schema"], 0)
    }

    /// A value shaped like `schema`: its example or default where it gives one, else a
    /// placeholder of the right type, as Postman and Swagger UI fill in a body.
    fn sample(&self, schema: &Value, depth: usize) -> Value {
        let s = self.deref(schema);
        if depth > MAX_DEPTH {
            return Value::Null;
        }
        for k in ["example", "default", "const"] {
            if !s[k].is_null() {
                return s[k].clone();
            }
        }
        if let Some(first) = s["enum"].as_array().and_then(|e| e.first()) {
            return first.clone();
        }
        if let Some(all) = s["allOf"].as_array() {
            let mut merged = Map::new();
            for part in all {
                if let Value::Object(o) = self.sample(part, depth + 1) {
                    merged.extend(o);
                }
            }
            return Value::Object(merged);
        }
        for k in ["oneOf", "anyOf"] {
            if let Some(first) = s[k].as_array().and_then(|a| a.first()) {
                return self.sample(first, depth + 1);
            }
        }
        // 3.1 allows a list of types: the first that isn't null.
        let ty = match &s["type"] {
            Value::Array(types) => types
                .iter()
                .filter_map(Value::as_str)
                .find(|t| *t != "null"),
            t => t.as_str(),
        };
        match ty {
            Some("object") | None if s["properties"].is_object() || ty.is_some() => {
                let mut o = Map::new();
                for (name, prop) in s["properties"].as_object().into_iter().flatten() {
                    // The server fills those in.
                    if self.deref(prop)["readOnly"] == true {
                        continue;
                    }
                    o.insert(name.clone(), self.sample(prop, depth + 1));
                }
                Value::Object(o)
            }
            Some("array") => json!([self.sample(&s["items"], depth + 1)]),
            Some("integer") | Some("number") => json!(0),
            Some("boolean") => json!(true),
            Some("string") => json!(match str_of(&s["format"]) {
                "date-time" => "2024-01-01T00:00:00Z",
                "date" => "2024-01-01",
                "email" => "user@example.com",
                "uuid" => "3fa85f64-5717-4562-b3fc-2c963f66afa6",
                "uri" | "url" => "https://example.com",
                "ipv4" => "192.0.2.1",
                _ => "string",
            }),
            _ => Value::Null,
        }
    }

    /// Saved responses: every example the spec gives, else one sample for the first 2xx
    /// with a schema, so the mock server answers out of the box.
    fn examples(&self, op: &Value) -> Vec<Example> {
        let mut out = Vec::new();
        let mut sampled = None;
        for (code, resp) in op["responses"].as_object().into_iter().flatten() {
            let Ok(status) = code.parse::<u16>() else {
                continue; // "default", "2XX"
            };
            let resp = self.deref(resp);
            let what = str_of(&resp["description"]);
            let name = format!("{code} {what}").trim().to_owned();
            let medias: Vec<(&str, &Value)> = match self.v3 {
                true => (resp["content"].as_object().into_iter().flatten())
                    .map(|(t, m)| (t.as_str(), m))
                    .collect(),
                // Swagger 2: `examples` by MIME type, one `schema`.
                false => {
                    for (mime, ex) in resp["examples"].as_object().into_iter().flatten() {
                        out.push(example(&name, status, mime, ex));
                    }
                    if !resp["schema"].is_null() && (200..300).contains(&status) {
                        sampled.get_or_insert((
                            name.clone(),
                            status,
                            "application/json",
                            &resp["schema"],
                        ));
                    }
                    Vec::new()
                }
            };
            for (mime, media) in medias {
                if !media["example"].is_null() {
                    out.push(example(&name, status, mime, &media["example"]));
                }
                for (label, ex) in media["examples"].as_object().into_iter().flatten() {
                    let ex = self.deref(ex);
                    out.push(example(
                        &format!("{name} ({label})"),
                        status,
                        mime,
                        &ex["value"],
                    ));
                }
                if !media["schema"].is_null() && (200..300).contains(&status) {
                    sampled.get_or_insert((name.clone(), status, mime, &media["schema"]));
                }
            }
        }
        if out.is_empty()
            && let Some((name, status, mime, schema)) = sampled
        {
            out.push(example(&name, status, mime, &self.sample(schema, 0)));
        }
        out
    }

    /// The first security requirement that maps to an auth apitool has; `[]` means none.
    fn security(&mut self, reqs: &Value, key: &str) -> Option<Auth> {
        let reqs = reqs.as_array()?;
        if reqs.is_empty()
            || reqs
                .iter()
                .any(|r| r.as_object().is_some_and(Map::is_empty))
        {
            return Some(Auth::None);
        }
        let schemes = match self.v3 {
            true => &self.root["components"]["securitySchemes"],
            false => &self.root["securityDefinitions"],
        };
        for req in reqs {
            for (name, scopes) in req.as_object().into_iter().flatten() {
                let scheme = self.deref(&schemes[name]);
                let scope = (scopes.as_array().into_iter().flatten())
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                match self.auth(scheme, scope) {
                    Ok(auth) => return Some(auth),
                    Err(why) => self.warn(key, format!("security \"{name}\": {why}")),
                }
            }
        }
        None
    }

    fn auth(&self, s: &Value, scope: String) -> Result<Auth, String> {
        let var = |n: &str| format!("{{{{{n}}}}}");
        let oauth = |grant, auth_url: &Value, token_url: &Value| {
            Auth::OAuth2(OAuth2 {
                grant,
                auth_url: text(auth_url),
                token_url: text(token_url),
                client_id: var("clientId"),
                client_secret: match grant {
                    Grant::Implicit => String::new(),
                    _ => var("clientSecret"),
                },
                scope: scope.clone(),
                ..Default::default()
            })
        };
        Ok(
            match (
                str_of(&s["type"]),
                str_of(&s["scheme"]).to_lowercase().as_str(),
            ) {
                ("http", "bearer") => Auth::Bearer {
                    token: var("bearerToken"),
                },
                ("http", "basic") | ("basic", _) => Auth::Basic {
                    username: var("username"),
                    password: var("password"),
                },
                ("http", "digest") => Auth::Digest {
                    username: var("username"),
                    password: var("password"),
                },
                ("apiKey", _) => match str_of(&s["in"]) {
                    place @ ("header" | "query") => Auth::ApiKey {
                        key: str_of(&s["name"]).to_owned(),
                        value: var("apiKey"),
                        in_query: place == "query",
                    },
                    other => return Err(format!("an API key in a {other} isn't supported")),
                },
                ("oauth2", _) if self.v3 => {
                    let f = &s["flows"];
                    if f["clientCredentials"].is_object() {
                        oauth(
                            Grant::ClientCredentials,
                            &Value::Null,
                            &f["clientCredentials"]["tokenUrl"],
                        )
                    } else if f["authorizationCode"].is_object() {
                        let c = &f["authorizationCode"];
                        oauth(
                            Grant::AuthorizationCode,
                            &c["authorizationUrl"],
                            &c["tokenUrl"],
                        )
                    } else if f["password"].is_object() {
                        oauth(Grant::Password, &Value::Null, &f["password"]["tokenUrl"])
                    } else if f["implicit"].is_object() {
                        oauth(
                            Grant::Implicit,
                            &f["implicit"]["authorizationUrl"],
                            &Value::Null,
                        )
                    } else {
                        return Err("no OAuth 2.0 flow apitool has".into());
                    }
                }
                ("oauth2", _) => match str_of(&s["flow"]) {
                    "application" => oauth(Grant::ClientCredentials, &Value::Null, &s["tokenUrl"]),
                    "accessCode" => oauth(
                        Grant::AuthorizationCode,
                        &s["authorizationUrl"],
                        &s["tokenUrl"],
                    ),
                    "password" => oauth(Grant::Password, &Value::Null, &s["tokenUrl"]),
                    "implicit" => oauth(Grant::Implicit, &s["authorizationUrl"], &Value::Null),
                    other => return Err(format!("OAuth 2.0 flow \"{other}\"")),
                },
                (ty, _) => return Err(format!("{ty} isn't supported")),
            },
        )
    }
}

/// The requests under `scope` (a folder, or the collections root) as an OpenAPI 3.1
/// document, read back by `import` as the same requests: a path per URL (`:id` and
/// `{{var}}` segments as path parameters), the server as the URL's start (`{{baseUrl}}` a
/// server variable defaulting to the folder's value), a tag per subfolder, off query
/// parameters as optional, bodies and saved examples as examples with a schema read off
/// them. Like the Markdown docs it holds no credentials: auth is its security scheme only.
/// Also returns how many were left out: streams, gRPC, and a second request to a method
/// and path already taken.
pub fn export(ws: &Workspace, scope: &Path) -> Result<(Value, usize), String> {
    let root = ws.collections();
    let tree = ws.tree();
    let (title, nodes) = match scope == root {
        true => ("API".to_owned(), &tree[..]),
        false => {
            let nodes = crate::docs::find(&tree, scope)
                .ok_or_else(|| format!("no folder at {}", scope.display()))?;
            (folder_name(&root, scope), nodes)
        }
    };
    let mut info = json!({ "title": title, "version": "1.0.0" });
    if scope != root {
        let about = ws.load_folder(scope)?.description;
        if !about.trim().is_empty() {
            info["description"] = about.trim().into();
        }
    }
    let mut out = Out::default();
    out.walk(ws, nodes, "")?;
    if out.count == 0 {
        return Err(format!(
            "no requests under {title} that OpenAPI can describe"
        ));
    }
    // The server most requests use is the document's (the first such on a tie); a
    // request elsewhere names its own.
    let most = (0..out.servers.len()).max_by_key(|&i| (out.servers[i].1.len(), Reverse(i)));
    let mut doc = json!({ "openapi": "3.1.0", "info": info });
    if let Some(i) = most {
        doc["servers"] = json!([out.servers[i].0]);
    }
    if !out.tags.is_empty() {
        doc["tags"] = out.tags.into();
    }
    doc["paths"] = out.paths.into();
    for (i, (server, ops)) in out.servers.into_iter().enumerate() {
        if Some(i) == most {
            continue;
        }
        for (path, method) in ops {
            doc["paths"][&path][&method]["servers"] = json!([server.clone()]);
        }
    }
    if !out.schemes.is_empty() {
        let schemes: Map<String, Value> = out.schemes.into_iter().collect();
        doc["components"] = json!({ "securitySchemes": schemes });
    }
    Ok((doc, out.left))
}

#[derive(Default)]
struct Out {
    paths: Map<String, Value>,
    /// Each server, with the (path, method) of the requests on it.
    servers: Vec<(Value, Vec<(String, String)>)>,
    tags: Vec<Value>,
    /// Security schemes by name, each written once however many requests use it.
    schemes: Vec<(String, Value)>,
    count: usize,
    left: usize,
}

impl Out {
    fn walk(&mut self, ws: &Workspace, nodes: &[Node], tag: &str) -> Result<(), String> {
        for node in nodes {
            match node {
                Node::Request { name, path, .. } => {
                    self.operation(&ws.load_request(path)?, name, tag)
                }
                Node::Folder {
                    name,
                    path,
                    children,
                } => {
                    let tag = match tag {
                        "" => name.clone(),
                        outer => format!("{outer} / {name}"),
                    };
                    let mut entry = json!({ "name": tag });
                    let about = ws.load_folder(path)?.description;
                    if !about.trim().is_empty() {
                        entry["description"] = about.trim().into();
                    }
                    self.tags.push(entry);
                    self.walk(ws, children, &tag)?;
                }
            }
        }
        Ok(())
    }

    fn operation(&mut self, req: &Request, name: &str, tag: &str) {
        let method = match req.method.as_str() {
            "GRAPHQL" => "post".to_owned(),
            "SSE" => "get".to_owned(),
            m => m.to_lowercase(),
        };
        let (start, path) = server_and_path(&req.url);
        let (path, names) = path_template(path);
        // QUERY is OpenAPI 3.2's.
        if method == "query"
            || !METHODS.contains(&method.as_str())
            || self
                .paths
                .get(&path)
                .is_some_and(|item| !item[&method].is_null())
        {
            self.left += 1;
            return;
        }
        let mut op = json!({ "summary": name });
        if !tag.is_empty() {
            op["tags"] = json!([tag]);
        }
        if !req.description.trim().is_empty() {
            op["description"] = req.description.trim().into();
        }
        let mut params = Vec::new();
        for n in &names {
            let row = req.path_vars.iter().find(|r| r.key == *n);
            let value = (row.map(|r| r.value.as_str()))
                .or(req.inherited.vars.get(n).map(String::as_str))
                .unwrap_or_default();
            let about = row.map_or("", |r| r.description.as_str());
            params.push(param(n, "path", true, value, about));
        }
        // OpenAPI ignores these as header parameters: the body and security say them.
        let implied = ["accept", "content-type", "authorization"];
        for (rows, place) in [(&req.params, "query"), (&req.headers, "header")] {
            for r in rows.iter().filter(|r| !r.key.trim().is_empty()) {
                if place == "query" || !implied.contains(&r.key.to_lowercase().as_str()) {
                    // Off is optional, as `import` turns optional ones off.
                    params.push(param(&r.key, place, r.enabled, &r.value, &r.description));
                }
            }
        }
        if !params.is_empty() {
            op["parameters"] = params.into();
        }
        let body = match &req.body {
            b @ Body::GraphQL { .. } => b.clone().graphql_switched(false),
            b => b.clone(),
        };
        if let Some(body) = request_body(&body, &req.headers) {
            op["requestBody"] = body;
        }
        let responses = responses(&req.examples);
        if !responses.is_empty() {
            op["responses"] = responses.into();
        }
        if let Some((name, scheme, scopes)) = scheme(req.effective_auth()) {
            let name = self.scheme_name(name, scheme);
            op["security"] = json!([{ name: scopes }]);
        }
        if !start.is_empty() {
            let server = server(start, &req.inherited.vars);
            let at = (path.clone(), method.clone());
            match self.servers.iter_mut().find(|(s, _)| *s == server) {
                Some((_, ops)) => ops.push(at),
                None => self.servers.push((server, vec![at])),
            }
        }
        let item = self.paths.entry(path).or_insert_with(|| json!({}));
        item[&method] = op;
        self.count += 1;
    }

    /// `base`, or `base2`, … when a different scheme already has that name.
    fn scheme_name(&mut self, base: &str, scheme: Value) -> String {
        if let Some((name, _)) = self.schemes.iter().find(|(_, s)| *s == scheme) {
            return name.clone();
        }
        let taken = |n: &String| self.schemes.iter().any(|(m, _)| m == n);
        let name = (std::iter::once(base.to_owned()))
            .chain((2..).map(|n| format!("{base}{n}")))
            .find(|n| !taken(n))
            .expect("some name is free");
        self.schemes.push((name.clone(), scheme));
        name
    }
}

/// `{{baseUrl}}/users/:id?x=1` → (`{{baseUrl}}`, `/users/:id`): the path starts at the
/// first '/' after the scheme's `://`.
fn server_and_path(url: &str) -> (&str, &str) {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let host = url.find("://").map_or(0, |i| i + 3);
    match url[host..].find('/') {
        Some(i) => url.split_at(host + i),
        None => (url, "/"),
    }
}

/// `/users/:id/{{version}}` → `/users/{id}/{version}`, and the names in order.
fn path_template(path: &str) -> (String, Vec<String>) {
    let mut names = Vec::new();
    let segments: Vec<String> = (path.split('/'))
        .map(|seg| match crate::model::path_var(seg) {
            Some(name) => {
                names.push(name.to_owned());
                format!("{{{name}}}")
            }
            None => single_braces(seg, &mut names),
        })
        .collect();
    (segments.join("/"), names)
}

/// `{{name}}` → `{name}`, OpenAPI's template, with each name pushed to `names`.
fn single_braces(text: &str, names: &mut Vec<String>) -> String {
    let (mut out, mut rest) = (String::new(), text);
    while let Some(open) = rest.find("{{") {
        let Some(len) = rest[open + 2..].find("}}") else {
            break;
        };
        let name = rest[open + 2..open + 2 + len].trim();
        out.push_str(&rest[..open]);
        out.push_str(&format!("{{{name}}}"));
        names.push(name.to_owned());
        rest = &rest[open + 2 + len + 2..];
    }
    out + rest
}

fn server(start: &str, vars: &HashMap<String, String>) -> Value {
    // Sent over http when it names no scheme; a variable may hold one.
    let start = match start.contains("://") || start.starts_with("{{") {
        true => start.to_owned(),
        false => format!("http://{start}"),
    };
    let mut names = Vec::new();
    let mut s = json!({ "url": single_braces(&start, &mut names) });
    if !names.is_empty() {
        let defaults: Map<String, Value> = (names.into_iter())
            .map(|n| {
                let value = vars.get(&n).cloned().unwrap_or_default();
                (n, json!({ "default": value }))
            })
            .collect();
        s["variables"] = defaults.into();
    }
    s
}

fn param(name: &str, place: &str, required: bool, value: &str, about: &str) -> Value {
    let mut p = json!({
        "name": name,
        "in": place,
        "required": required,
        "schema": { "type": "string" },
    });
    if !value.is_empty() {
        p["example"] = value.into();
    }
    if !about.trim().is_empty() {
        p["description"] = about.trim().into();
    }
    p
}

fn request_body(body: &Body, headers: &[KeyValue]) -> Option<Value> {
    let declared = (headers.iter())
        .find(|h| h.enabled && h.key.eq_ignore_ascii_case("content-type"))
        .map(|h| h.value.trim())
        .filter(|v| !v.is_empty());
    let mime = |default: &str| declared.unwrap_or(default).to_owned();
    let binary = json!({ "schema": { "type": "string", "format": "binary" } });
    let (mime, media) = match body {
        Body::None | Body::GraphQL { .. } => return None,
        Body::Json { text } => (
            mime("application/json"),
            match serde_json::from_str::<Value>(text) {
                Ok(v) => json!({ "schema": schema_of(&v), "example": v }),
                // `{{variables}}` where a value goes: the text as it is.
                Err(_) => json!({ "example": text }),
            },
        ),
        Body::Text { text } => (
            mime("text/plain"),
            json!({ "schema": { "type": "string" }, "example": text }),
        ),
        Body::Form { fields } => (
            "application/x-www-form-urlencoded".into(),
            form(fields, false),
        ),
        Body::Multipart { parts } => ("multipart/form-data".into(), form(parts, true)),
        Body::File { .. } | Body::Base64 { .. } | Body::Hex { .. } => {
            (mime("application/octet-stream"), binary)
        }
    };
    Some(json!({ "content": { mime: media } }))
}

/// Fields as an object's properties, each value its example; a multipart `@path` is a file.
/// The ones on are required.
fn form(fields: &[KeyValue], files: bool) -> Value {
    let (mut properties, mut required) = (Map::new(), Vec::new());
    for f in fields.iter().filter(|f| !f.key.is_empty()) {
        let mut p = match files && f.value.starts_with('@') {
            true => json!({ "type": "string", "format": "binary" }),
            false => json!({ "type": "string", "example": f.value }),
        };
        if !f.description.trim().is_empty() {
            p["description"] = f.description.trim().into();
        }
        properties.insert(f.key.clone(), p);
        if f.enabled {
            required.push(f.key.clone());
        }
    }
    json!({ "schema": { "type": "object", "properties": properties, "required": required } })
}

/// Saved examples by status, then content type: one is the `example`, several are
/// `examples` by name. The first one's name describes the status.
fn responses(examples: &[Example]) -> Map<String, Value> {
    let mut groups: Vec<((u16, &str), Vec<&Example>)> = Vec::new();
    for ex in examples {
        let mime = ex.content_type.split(';').next().unwrap_or_default().trim();
        let key = (ex.status, if mime.is_empty() { "text/plain" } else { mime });
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(ex),
            None => groups.push((key, vec![ex])),
        }
    }
    let mut out = Map::new();
    for ((status, mime), list) in groups {
        let value = |ex: &Example| match mime.contains("json") {
            true => serde_json::from_str(&ex.body).unwrap_or_else(|_| ex.body.clone().into()),
            false => Value::String(ex.body.clone()),
        };
        let resp = (out.entry(status.to_string()))
            .or_insert_with(|| json!({ "description": list[0].name }));
        resp["content"][mime] = match &list[..] {
            [one] => {
                let v = value(one);
                json!({ "schema": schema_of(&v), "example": v })
            }
            many => {
                let named: Map<String, Value> = (many.iter())
                    .map(|ex| (ex.name.clone(), json!({ "value": value(ex) })))
                    .collect();
                json!({ "examples": named })
            }
        };
    }
    out
}

/// A schema read off a value: its types, an object's properties, an array's first item.
fn schema_of(v: &Value) -> Value {
    match v {
        Value::Null => json!({ "type": "null" }),
        Value::Bool(_) => json!({ "type": "boolean" }),
        Value::Number(n) if n.is_f64() => json!({ "type": "number" }),
        Value::Number(_) => json!({ "type": "integer" }),
        Value::String(_) => json!({ "type": "string" }),
        Value::Array(items) => {
            json!({ "type": "array", "items": items.first().map_or(json!({}), schema_of) })
        }
        Value::Object(o) => {
            let properties: Map<String, Value> =
                (o.iter()).map(|(k, v)| (k.clone(), schema_of(v))).collect();
            json!({ "type": "object", "properties": properties })
        }
    }
}

/// The security scheme for an auth (a name for it, the scheme, the scopes asked for), for
/// those OpenAPI has: no credentials, only how they're sent.
fn scheme(auth: &Auth) -> Option<(&'static str, Value, Vec<String>)> {
    let http = |scheme: &str| json!({ "type": "http", "scheme": scheme });
    Some(match auth {
        Auth::Bearer { .. } => ("bearerAuth", http("bearer"), Vec::new()),
        Auth::Jwt(_) => {
            let jwt = json!({ "type": "http", "scheme": "bearer", "bearerFormat": "JWT" });
            ("bearerAuth", jwt, Vec::new())
        }
        Auth::Basic { .. } => ("basicAuth", http("basic"), Vec::new()),
        Auth::Digest { .. } => ("digestAuth", http("digest"), Vec::new()),
        Auth::ApiKey { key, in_query, .. } => {
            let place = if *in_query { "query" } else { "header" };
            let key = json!({ "type": "apiKey", "name": key, "in": place });
            ("apiKey", key, Vec::new())
        }
        Auth::OAuth2(o) => {
            let scopes: Vec<String> = o.scope.split_whitespace().map(str::to_owned).collect();
            let (flow, mut f) = match o.grant {
                Grant::ClientCredentials => {
                    ("clientCredentials", json!({ "tokenUrl": o.token_url }))
                }
                Grant::Password => ("password", json!({ "tokenUrl": o.token_url })),
                Grant::AuthorizationCode => (
                    "authorizationCode",
                    json!({ "authorizationUrl": o.auth_url, "tokenUrl": o.token_url }),
                ),
                Grant::Implicit => ("implicit", json!({ "authorizationUrl": o.auth_url })),
            };
            let named: Map<String, Value> =
                (scopes.iter()).map(|s| (s.clone(), json!(""))).collect();
            f["scopes"] = named.into();
            let oauth = json!({ "type": "oauth2", "flows": { flow: f } });
            ("oauth2", oauth, scopes)
        }
        // OAuth 1, the signed ones (AWS, Hawk, EdgeGrid) and NTLM have none.
        _ => return None,
    })
}

fn example(name: &str, status: u16, mime: &str, body: &Value) -> Example {
    Example {
        name: name.to_owned(),
        status,
        content_type: mime.to_owned(),
        body: match body {
            Value::String(s) => s.clone(),
            v => serde_json::to_string_pretty(v).unwrap_or_default(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Parts = (
        String,
        Vec<(String, Folder)>,
        Vec<(String, Request)>,
        Vec<String>,
    );

    fn collection(spec: &str) -> Parts {
        let v = crate::import::value(spec).unwrap();
        assert!(is_spec(&v));
        match import(&v) {
            Import::Collection {
                name,
                folders,
                requests,
                warnings,
                ..
            } => (name, folders, requests, warnings),
            _ => panic!("a collection"),
        }
    }

    const PETS: &str = r##"
openapi: 3.0.3
info: { title: Pet Store, description: Pets. }
servers:
  - url: https://{region}.pets.test/v1
    variables: { region: { default: eu } }
security: [ { bearer: [] } ]
tags: [ { name: pets, description: Everything about pets } ]
paths:
  /pets/{petId}:
    parameters:
      - { name: petId, in: path, required: true, schema: { type: integer, example: 7 } }
    get:
      tags: [pets]
      summary: Get a pet
      parameters:
        - { name: fields, in: query, schema: { type: string, default: name } }
        - { name: X-Trace, in: header, schema: { type: string } }
      responses:
        "200":
          description: The pet
          content:
            application/json:
              schema: { $ref: "#/components/schemas/Pet" }
  /pets:
    post:
      tags: [pets]
      summary: Add a pet
      security: [ { oauth: [pets.write] } ]
      requestBody:
        content:
          application/json:
            schema: { $ref: "#/components/schemas/Pet" }
      responses:
        "201":
          description: Created
          content:
            application/json:
              examples:
                rex: { value: { id: 1, name: Rex } }
  /health:
    get:
      summary: Health
      security: []
      responses: { 204: { description: Up } }
  /upload:
    post:
      summary: Upload
      requestBody:
        content:
          multipart/form-data:
            schema:
              type: object
              properties:
                note: { type: string, example: hi }
                file: { type: string, format: binary }
      responses: { "200": { description: OK } }
components:
  securitySchemes:
    bearer: { type: http, scheme: bearer }
    oauth:
      type: oauth2
      flows: { clientCredentials: { tokenUrl: "https://auth.pets.test/token", scopes: {} } }
  schemas:
    Pet:
      type: object
      required: [name]
      properties:
        id: { type: integer, readOnly: true }
        name: { type: string, example: Tom }
        tags: { type: array, items: { type: string } }
        born: { type: string, format: date }
        parent: { $ref: "#/components/schemas/Pet" }
"##;

    /// What a spec gives is what Send needs: the URL with its path variable and required
    /// query, the server as {{baseUrl}}, a body to edit, auth, and examples to mock with.
    #[test]
    fn an_openapi_3_spec_becomes_requests_ready_to_send() {
        let (name, folders, requests, warnings) = collection(PETS);
        assert_eq!(name, "Pet Store");
        assert!(warnings.is_empty(), "{warnings:?}");
        let top = &folders[0].1;
        assert_eq!(
            top.vars,
            [KeyValue::new("baseUrl", "https://eu.pets.test/v1")]
        );
        assert_eq!(
            top.auth,
            Auth::Bearer {
                token: "{{bearerToken}}".into()
            }
        );
        assert_eq!(folders[1].0, "pets");
        assert_eq!(folders[1].1.description, "Everything about pets");

        let r: std::collections::HashMap<_, _> = requests.into_iter().collect();
        let get = &r["pets/Get a pet"];
        // Optional query parameters are listed but not sent.
        assert_eq!(get.url, "{{baseUrl}}/pets/:petId");
        assert_eq!(get.path_vars, [KeyValue::new("petId", "7")]);
        assert_eq!(
            (
                get.params[0].key.as_str(),
                get.params[0].value.as_str(),
                get.params[0].enabled
            ),
            ("fields", "name", false)
        );
        assert_eq!(get.headers[0].key, "X-Trace");
        assert_eq!(get.auth, Auth::Inherit);
        // No example given: one sampled from the schema, so the mock server has an answer.
        assert_eq!(get.examples.len(), 1);
        let pet: Value = serde_json::from_str(&get.examples[0].body).unwrap();
        assert_eq!(pet["name"], "Tom");
        assert!(pet.get("id").is_none(), "read-only fields are the server's");

        let add = &r["pets/Add a pet"];
        let Body::Json { text } = &add.body else {
            panic!("{:?}", add.body)
        };
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["born"], "2024-01-01");
        assert_eq!(body["tags"], json!(["string"]));
        assert!(
            body["parent"].is_object(),
            "self-reference stops, not overflows"
        );
        let Auth::OAuth2(o) = &add.auth else {
            panic!("{:?}", add.auth)
        };
        assert_eq!(
            (o.grant, o.token_url.as_str(), o.scope.as_str()),
            (
                Grant::ClientCredentials,
                "https://auth.pets.test/token",
                "pets.write"
            )
        );
        assert_eq!(add.examples[0].name, "201 Created (rex)");
        assert_eq!(add.examples[0].status, 201);

        assert_eq!(r["Health"].auth, Auth::None, "security: [] means none");
        let Body::Multipart { parts } = &r["Upload"].body else {
            panic!()
        };
        assert_eq!(parts[0], KeyValue::new("note", "hi"));
        assert!(parts[1].description.contains("@path"));
    }

    #[test]
    fn a_swagger_2_spec_comes_over_too() {
        let spec = r#"{
          "swagger": "2.0",
          "info": { "title": "Old" },
          "host": "old.test", "basePath": "/api", "schemes": ["http", "https"],
          "securityDefinitions": { "key": { "type": "apiKey", "in": "header", "name": "X-Key" } },
          "paths": {
            "/items": {
              "post": {
                "operationId": "addItem",
                "security": [{ "key": [] }],
                "parameters": [{ "in": "body", "name": "item", "schema": { "type": "object", "properties": { "n": { "type": "integer" } } } }],
                "responses": { "200": { "description": "ok", "examples": { "application/json": { "n": 1 } } } }
              },
              "put": {
                "parameters": [
                  { "in": "formData", "name": "f", "type": "file" },
                  { "in": "cookie", "name": "c", "type": "string" }
                ],
                "responses": {}
              }
            }
          }
        }"#;
        let (_, folders, requests, warnings) = collection(spec);
        assert_eq!(folders[0].1.vars[0].value, "https://old.test/api");
        let r: std::collections::HashMap<_, _> = requests.into_iter().collect();
        let add = &r["addItem"];
        assert_eq!(
            add.body,
            Body::Json {
                text: "{\n  \"n\": 0\n}".into()
            }
        );
        assert_eq!(
            add.auth,
            Auth::ApiKey {
                key: "X-Key".into(),
                value: "{{apiKey}}".into(),
                in_query: false
            }
        );
        assert_eq!(add.examples[0].body, "{\n  \"n\": 1\n}");
        assert!(matches!(r["PUT items"].body, Body::Multipart { .. }));
        assert_eq!(warnings, ["PUT items: cookie parameter \"c\" left out"]);
    }

    /// A folder exported and imported again gives back its requests: the server as
    /// {{baseUrl}} with the folder's value, path and query parameters (off ones optional),
    /// bodies, examples and auth, but never a credential. What OpenAPI can't hold is
    /// counted, not dropped unsaid, and a request elsewhere keeps its own server.
    #[test]
    fn an_exported_folder_imports_back_as_the_same_requests() {
        let root = std::env::temp_dir().join(format!("apitool-oas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let shop = ws.create_folder(&ws.collections(), "shop").unwrap();
        let folder = Folder {
            description: "The shop.".into(),
            vars: vec![KeyValue::new("baseUrl", "https://shop.test/v1")],
            auth: Auth::Bearer {
                token: "s3cret".into(),
            },
            ..Default::default()
        };
        ws.save_folder(&shop, &folder).unwrap();
        let orders = ws.create_folder(&shop, "orders").unwrap();
        let mut debug = KeyValue::new("debug", "1");
        debug.enabled = false;
        let json = |name: &str, status, body: &str| Example {
            name: name.into(),
            status,
            content_type: "application/json".into(),
            body: body.into(),
        };
        let get = Request {
            url: "{{baseUrl}}/orders/:id?expand=items".into(),
            path_vars: vec![KeyValue::new("id", "7")],
            params: vec![KeyValue::new("expand", "items"), debug],
            examples: vec![json("found", 200, r#"{"id": 7}"#), json("gone", 404, "{}")],
            ..Default::default()
        };
        ws.save_request(&orders.join("get order.toml"), &get)
            .unwrap();
        let create = Request {
            method: "POST".into(),
            url: "{{baseUrl}}/orders".into(),
            body: Body::Json {
                text: r#"{"sku": "a1", "qty": 2}"#.into(),
            },
            auth: Auth::Basic {
                username: "me".into(),
                password: "hunter2".into(),
            },
            ..Default::default()
        };
        ws.save_request(&orders.join("create order.toml"), &create)
            .unwrap();
        // Not for OpenAPI: a stream, and a second GET of a path already taken.
        let live = Request {
            method: "WS".into(),
            url: "{{baseUrl}}/live".into(),
            ..Default::default()
        };
        ws.save_request(&shop.join("live.toml"), &live).unwrap();
        ws.save_request(&orders.join("get order again.toml"), &get)
            .unwrap();
        let ping = Request {
            url: "https://status.test/ping".into(),
            ..Default::default()
        };
        ws.save_request(&shop.join("ping.toml"), &ping).unwrap();

        let (doc, left) = export(&ws, &shop).unwrap();
        let text = doc.to_string();
        assert!(
            !text.contains("s3cret") && !text.contains("hunter2"),
            "{text}"
        );
        assert_eq!(left, 2);
        let base = json!({ "url": "{baseUrl}", "variables": { "baseUrl": { "default": "https://shop.test/v1" } } });
        assert_eq!(doc["servers"], json!([base]));
        assert_eq!(
            doc["paths"]["/ping"]["get"]["servers"][0]["url"],
            "https://status.test"
        );
        // OpenAPI's templating, which tools other than `import` read.
        let mut paths: Vec<_> = doc["paths"].as_object().unwrap().keys().collect();
        paths.sort();
        assert_eq!(paths, ["/orders", "/orders/{id}", "/ping"]);

        let Import::Collection {
            folders,
            requests,
            warnings,
            ..
        } = import(&doc)
        else {
            panic!("not a collection")
        };
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(folders[0].1.vars, folder.vars);
        assert_eq!(folders[0].1.description, "The shop.");
        let r: HashMap<_, _> = requests.into_iter().collect();
        let back = &r["orders/get order"];
        assert_eq!(back.url, get.url);
        assert_eq!(
            (&back.path_vars, &back.params),
            (&get.path_vars, &get.params)
        );
        let statuses: Vec<_> = back.examples.iter().map(|e| e.status).collect();
        assert_eq!(statuses, [200, 404]);
        assert_eq!(back.examples[0].body, "{\n  \"id\": 7\n}");
        assert!(matches!(back.auth, Auth::Bearer { .. }), "{:?}", back.auth);
        let made = &r["orders/create order"];
        let Body::Json { text } = &made.body else {
            panic!("{:?}", made.body)
        };
        let sent: Value = serde_json::from_str(text).unwrap();
        assert_eq!(sent, json!({ "sku": "a1", "qty": 2 }));
        assert!(matches!(made.auth, Auth::Basic { .. }), "{:?}", made.auth);
        let _ = std::fs::remove_dir_all(&root);
    }
}
