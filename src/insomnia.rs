//! Insomnia exports: v4 JSON (a flat list of resources linked by `parentId`) and v5 YAML
//! (the same as a tree). Requests, folders, auth and scripts come over; the base
//! environment becomes the collection's variables, each sub-environment an environment.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::{Value, json};

use crate::model::{
    Auth, AwsV4, Body, Folder, Grant, Hawk, KeyValue, Ntlm, OAuth1, OAuth2, Request,
};
use crate::postman::Import;
use crate::store::{Node, Workspace, escape_name, folder_name, safe_name, unescape_name};

pub fn is_insomnia(v: &Value) -> bool {
    (v["__export_format"] == 4 && v["resources"].is_array())
        || str_of(&v["type"]).starts_with("collection.insomnia.rest/")
}

pub fn import(root: &Value) -> Import {
    let mut r = Reader::default();
    let root = templates(root, &mut r.tags);
    let (name, description, children, envs) = match root["resources"].as_array() {
        Some(resources) => v4(resources),
        None => (
            str_of(&root["name"]).to_owned(),
            description_of(&root),
            root["collection"].clone(),
            {
                let base = &root["environments"];
                let subs = base["subEnvironments"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                (base.clone(), subs)
            },
        ),
    };
    let (base, subs) = envs;
    r.group(
        "",
        &json!({
            "children": children,
            "environment": base["data"],
            "description": description,
            "authentication": root["authentication"],
            "headers": root["headers"],
        }),
    );
    let environments = subs
        .iter()
        .map(|e| {
            let (shared, secret) = match e["isPrivate"] == true {
                true => (Vec::new(), vars(&e["data"])),
                false => (vars(&e["data"]), Vec::new()),
            };
            (safe_name(str_of(&e["name"])), shared, secret)
        })
        .collect();
    let mut tags: Vec<_> = r.tags.into_iter().collect();
    tags.sort();
    r.warnings.extend(
        tags.into_iter()
            .map(|t| format!("template tag {{% {t} %}} has no counterpart; left as it is")),
    );
    Import::Collection {
        name: match name.trim() {
            "" => "Insomnia".to_owned(),
            n => escape_name(n),
        },
        folders: r.folders,
        requests: r.requests,
        warnings: r.warnings,
        environments,
    }
}

/// v4's flat list as v5's tree, children in the order Insomnia shows them; the workspace's
/// name and description.
fn v4(resources: &[Value]) -> (String, String, Value, (Value, Vec<Value>)) {
    let of_type = |t: &'static str| resources.iter().filter(move |r| r["_type"] == t);
    let Some(ws) = of_type("workspace").next() else {
        return (
            String::new(),
            String::new(),
            json!([]),
            (Value::Null, Vec::new()),
        );
    };
    let base = of_type("environment")
        .find(|e| e["parentId"] == ws["_id"])
        .cloned()
        .unwrap_or_default();
    let subs = of_type("environment")
        .filter(|e| e["parentId"] == base["_id"] && !base["_id"].is_null())
        .cloned()
        .collect();
    fn tree(resources: &[Value], parent: &Value) -> Value {
        let mut children: Vec<Value> = resources
            .iter()
            .filter(|r| r["parentId"] == *parent && r["_type"] != "environment")
            .cloned()
            .collect();
        children.sort_by(|a, b| {
            let key = |r: &Value| r["metaSortKey"].as_f64().unwrap_or_default();
            key(a).total_cmp(&key(b))
        });
        for c in &mut children {
            if c["_type"] == "request_group" {
                c["children"] = tree(resources, &c["_id"]);
            }
        }
        Value::Array(children)
    }
    let name = str_of(&ws["name"]).to_owned();
    (
        name,
        description_of(ws),
        tree(resources, &ws["_id"]),
        (base, subs),
    )
}

#[derive(Default)]
struct Reader {
    folders: Vec<(String, Folder)>,
    requests: Vec<(String, Request)>,
    warnings: Vec<String>,
    /// Template tags left untranslated, reported once each.
    tags: HashSet<String>,
}

impl Reader {
    fn warn(&mut self, key: &str, what: impl std::fmt::Display) {
        let at = if key.is_empty() { "collection" } else { key };
        self.warnings.push(format!("{at}: {what}"));
    }

    fn auth(&mut self, key: &str, v: &Value) -> Auth {
        auth(v).unwrap_or_else(|kind| {
            self.warn(key, format!("{kind} auth isn't supported; set to none"));
            Auth::None
        })
    }

    fn group(&mut self, key: &str, v: &Value) {
        let mut folder = Folder {
            description: description_of(v),
            vars: vars(&v["environment"]),
            auth: self.auth(key, &v["authentication"]),
            pre_request: script(v, "preRequestScript", "preRequest"),
            tests: script(v, "afterResponseScript", "afterResponse"),
            order: Vec::new(),
            ..Default::default()
        };
        // Insomnia folders can carry headers for their requests; apitool's don't.
        if headers(&v["headers"]).iter().any(|h| h.enabled) {
            self.warn(
                key,
                "folder headers aren't supported; add them to its requests",
            );
        }
        folder.vars.retain(|v| !v.key.is_empty());
        let at = self.folders.len();
        self.folders.push((key.to_owned(), folder));
        let mut order = Vec::new();
        let mut taken = HashSet::new();
        for child in v["children"].as_array().into_iter().flatten() {
            let base = escape_name(str_of(&child["name"]));
            let name = (1..)
                .map(|n| match n {
                    1 => base.clone(),
                    n => format!("{base} {n}"),
                })
                .find(|n| taken.insert(n.to_lowercase()))
                .expect("some name is free");
            let child_key = match key.is_empty() {
                true => name.clone(),
                false => format!("{key}/{name}"),
            };
            let kind = str_of(&child["_type"]);
            if child["children"].is_array() {
                order.push(format!("{name}/"));
                self.group(&child_key, child);
            } else if kind == "grpc_request" || child["protoMethodName"].is_string() {
                // The proto files live outside the export.
                self.warn(&child_key, "gRPC requests aren't imported; left out");
            } else if kind == "websocket_request"
                || (kind.is_empty() && !child["method"].is_string() && child["url"].is_string())
            {
                let req = Request {
                    name: str_of(&child["name"]).trim().to_owned(),
                    method: "WS".into(),
                    url: str_of(&child["url"]).to_owned(),
                    headers: headers(&child["headers"]),
                    auth: self.auth(&child_key, &child["authentication"]),
                    description: description_of(child),
                    ..Default::default()
                };
                order.push(name);
                self.requests.push((child_key, req));
            } else if kind == "request" || kind.is_empty() {
                order.push(name);
                self.request(&child_key, child);
            }
        }
        // Children come sorted by Insomnia's own order (metaSortKey); keep it.
        self.folders[at].1.order = Folder::order_of(order);
    }

    fn request(&mut self, key: &str, v: &Value) {
        let mut req = Request {
            name: str_of(&v["name"]).trim().to_owned(),
            url: str_of(&v["url"]).to_owned(),
            params: rows(&v["parameters"]),
            path_vars: rows(&v["pathParameters"]),
            headers: headers(&v["headers"]),
            description: description_of(v),
            pre_request: script(v, "preRequestScript", "preRequest"),
            tests: script(v, "afterResponseScript", "afterResponse"),
            ..Default::default()
        };
        let method = str_of(&v["method"]).to_uppercase();
        if crate::model::METHODS.contains(&method.as_str()) {
            req.method = method;
        } else {
            self.warn(key, format!("method {method} isn't supported; set to GET"));
        }
        req.body = self.body(key, &v["body"], &mut req.headers);
        if matches!(req.body, Body::GraphQL { .. }) {
            req.method = "GRAPHQL".into();
        }
        req.auth = self.auth(key, &v["authentication"]);
        req.sync_params();
        self.requests.push((key.to_owned(), req));
    }

    fn body(&mut self, key: &str, b: &Value, headers: &mut Vec<KeyValue>) -> Body {
        let mime = str_of(&b["mimeType"]).trim().to_lowercase();
        let mime = mime.split(';').next().unwrap_or_default();
        let text = str_of(&b["text"]).to_owned();
        match mime {
            "application/x-www-form-urlencoded" => Body::Form {
                fields: rows(&b["params"]),
            },
            "multipart/form-data" => {
                let params = b["params"].as_array().map_or(&[][..], Vec::as_slice);
                let mut parts = rows(&b["params"]);
                for (part, p) in parts.iter_mut().zip(params) {
                    if p["type"] == "file" {
                        part.value = format!("@{}", str_of(&p["fileName"]));
                    }
                }
                Body::Multipart { parts }
            }
            // An editor marker: Insomnia sends it as a JSON envelope.
            "application/graphql" => {
                let envelope: Value = serde_json::from_str(&text).unwrap_or_default();
                match envelope["query"].as_str() {
                    Some(query) => Body::GraphQL {
                        query: query.to_owned(),
                        variables: match &envelope["variables"] {
                            Value::Null => String::new(),
                            Value::String(s) => s.clone(),
                            v => serde_json::to_string_pretty(v).unwrap_or_default(),
                        },
                    },
                    None => Body::GraphQL {
                        query: text,
                        variables: String::new(),
                    },
                }
            }
            "application/octet-stream" => Body::File {
                path: str_of(&b["fileName"]).to_owned(),
            },
            _ if text.is_empty() => Body::None,
            m if m == "application/json" || m.ends_with("+json") => Body::Json { text },
            m => {
                let typed = headers
                    .iter()
                    .any(|h| h.key.eq_ignore_ascii_case("content-type"));
                if !m.is_empty() && !typed {
                    headers.push(KeyValue::new("Content-Type", m));
                }
                if m.is_empty() && b["mimeType"].is_string() {
                    self.warn(key, "body has no content type; sent as text");
                }
                Body::Text { text }
            }
        }
    }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// v4 has it on the resource, v5 under `meta`.
fn description_of(v: &Value) -> String {
    match str_of(&v["description"]) {
        "" => str_of(&v["meta"]["description"]).to_owned(),
        d => d.to_owned(),
    }
}

/// Insomnia's script API mirrors Postman's under another name.
fn script(v: &Value, v4: &str, v5: &str) -> String {
    let s = match str_of(&v[v4]) {
        "" => str_of(&v["scripts"][v5]),
        s => s,
    };
    s.replace("insomnia.", "pm.")
}

fn rows(v: &Value) -> Vec<KeyValue> {
    let rows = v.as_array().into_iter().flatten();
    rows.map(|p| KeyValue {
        enabled: p["disabled"] != true,
        description: str_of(&p["description"]).to_owned(),
        ..KeyValue::new(str_of(&p["name"]), text(&p["value"]))
    })
    .collect()
}

fn headers(v: &Value) -> Vec<KeyValue> {
    let mut rows = rows(v);
    rows.retain(|h| !h.key.is_empty() || !h.value.is_empty());
    rows
}

fn vars(data: &Value) -> Vec<KeyValue> {
    let data = data.as_object().into_iter().flatten();
    data.map(|(k, v)| KeyValue::new(k, text(v))).collect()
}

/// `{}` or no auth inherits, as Insomnia's "Inherit from parent" does; `disabled` is none.
fn auth(a: &Value) -> Result<Auth, String> {
    let s = |k: &str| str_of(&a[k]).to_owned();
    if !a.is_object() || a.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(Auth::Inherit);
    }
    if a["disabled"] == true {
        return Ok(Auth::None);
    }
    Ok(match str_of(&a["type"]) {
        "" | "none" => Auth::None,
        "basic" => Auth::Basic {
            username: s("username"),
            password: s("password"),
        },
        "digest" => Auth::Digest {
            username: s("username"),
            password: s("password"),
        },
        "bearer" => match str_of(&a["prefix"]).trim() {
            "" | "Bearer" => Auth::Bearer { token: s("token") },
            prefix => Auth::ApiKey {
                key: "Authorization".into(),
                value: format!("{prefix} {}", s("token")),
                in_query: false,
            },
        },
        "apikey" if a["addTo"] != "cookie" => Auth::ApiKey {
            key: s("key"),
            value: s("value"),
            in_query: a["addTo"] == "queryParams",
        },
        "oauth2" => Auth::OAuth2(OAuth2 {
            grant: match str_of(&a["grantType"]) {
                "password" => Grant::Password,
                "authorization_code" => Grant::AuthorizationCode,
                "implicit" => Grant::Implicit,
                "client_credentials" => Grant::ClientCredentials,
                g => return Err(format!("OAuth 2 {g}")),
            },
            token_url: s("accessTokenUrl"),
            client_id: s("clientId"),
            client_secret: s("clientSecret"),
            scope: s("scope"),
            username: s("username"),
            password: s("password"),
            auth_url: s("authorizationUrl"),
            redirect_uri: s("redirectUrl"),
            // Insomnia's "Credentials in body" is off unless ticked: the header.
            basic_auth: a["credentialsInBody"] != true,
        }),
        "iam" => Auth::AwsV4(AwsV4 {
            access_key: s("accessKeyId"),
            secret_key: s("secretAccessKey"),
            region: s("region"),
            service: s("service"),
            session_token: s("sessionToken"),
        }),
        "oauth1" => Auth::OAuth1(OAuth1 {
            consumer_secret: match str_of(&a["signatureMethod"]).starts_with("RSA") {
                true => s("privateKey"),
                false => s("consumerSecret"),
            },
            signature_method: s("signatureMethod"),
            consumer_key: s("consumerKey"),
            token: s("tokenKey"),
            token_secret: s("tokenSecret"),
            realm: s("realm"),
        }),
        "hawk" => Auth::Hawk(Hawk {
            id: s("id"),
            key: s("key"),
            algorithm: s("algorithm"),
            ext: s("ext"),
            payload_hash: a["validatePayload"] == true,
            ..Default::default()
        }),
        // Insomnia has no domain field: `DOMAIN\user` in the name, which NTLM splits.
        "ntlm" => Auth::Ntlm(Ntlm {
            username: s("username"),
            password: s("password"),
            ..Default::default()
        }),
        "apikey" => return Err("cookie API key".into()),
        kind => return Err(kind.to_owned()),
    })
}

static VARIABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{\{\s*(?:_\.([\w.-]+)|_\[\s*['"]([^'"]+)['"]\s*\]|([\w-][\w.-]*))\s*\}\}"#)
        .expect("valid")
});
static TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\{%\s*(\w+)\s*((?:'[^']*'|"[^"]*"|[^'"%])*?)\s*%\}"#).expect("valid")
});

/// Insomnia's Nunjucks (`{{ _.host }}`, `{% uuid %}`) as apitool's `{{host}}` and dynamic
/// variables. Tags with no counterpart (`{% response %}`, filters) stay as typed, so the
/// user sees what to redo instead of a value that quietly means something else.
fn templates(v: &Value, unknown: &mut HashSet<String>) -> Value {
    match v {
        Value::String(s) => Value::String(template(s, unknown)),
        Value::Array(a) => Value::Array(a.iter().map(|v| templates(v, unknown)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (template(k, unknown), templates(v, unknown)))
                .collect(),
        ),
        v => v.clone(),
    }
}

fn template(s: &str, unknown: &mut HashSet<String>) -> String {
    if !s.contains("{{") && !s.contains("{%") {
        return s.to_owned();
    }
    let s = VARIABLE.replace_all(s, |c: &Captures| {
        let name = (1..=3).find_map(|i| c.get(i)).expect("one group matches");
        format!("{{{{{}}}}}", name.as_str())
    });
    TAG.replace_all(&s, |c: &Captures| {
        let (name, args) = (&c[1], c[2].trim().trim_matches(['\'', '"']));
        let dynamic = match name {
            "uuid" => Some("$guid".to_owned()),
            "now" | "timestamp" => match args.split(',').next().unwrap_or_default().trim() {
                "" | "iso-8601" if name == "now" => Some("$isoTimestamp".to_owned()),
                "unix" | "seconds" | "s" => Some("$timestamp".to_owned()),
                "" if name == "timestamp" => Some("$timestamp".to_owned()),
                _ => None,
            },
            "faker" => Some(format!("${args}")).filter(|n| crate::fake::value(n).is_some()),
            // Asked at send time here too.
            "prompt" => {
                let title = args.split(',').next().unwrap_or_default();
                let title = title.trim().trim_matches(['\'', '"']).trim();
                (!title.is_empty()).then(|| format!("?{title}"))
            }
            _ => None,
        };
        dynamic.map_or_else(
            || {
                unknown.insert(
                    c[0].trim_start_matches("{%")
                        .trim_end_matches("%}")
                        .trim()
                        .to_owned(),
                );
                c[0].to_owned()
            },
            |n| format!("{{{{{n}}}}}"),
        )
    })
    .into_owned()
}

/// The requests under `dir` (a folder, or the collections root) as an Insomnia v4 export,
/// which Insomnia, Hoppscotch, Bruno, Yaak and Restfox all import; and how many were left
/// out: SSE, MQTT and Socket.IO have no Insomnia counterpart, and gRPC's proto files stay
/// here. The folder's variables are the base environment. A v4 workspace holds no auth or
/// scripts, so the folder's go on each item directly in it, which Insomnia runs the same.
pub fn export(ws: &Workspace, dir: &Path) -> Result<(String, usize), String> {
    let root = ws.collections();
    let tree = ws.tree();
    let (name, nodes) = match dir == root {
        true => ("API".to_owned(), &tree[..]),
        false => {
            let nodes = crate::docs::find(&tree, dir)
                .ok_or_else(|| format!("no folder at {}", folder_name(&root, dir)))?;
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            (unescape_name(&name), nodes)
        }
    };
    let top = ws.load_folder(dir)?;
    let mut out = Out {
        resources: vec![
            json!({ "_id": "wrk_1", "_type": "workspace", "parentId": null, "name": name,
                    "description": top.description, "scope": "collection" }),
            json!({ "_id": "env_1", "_type": "environment", "parentId": "wrk_1",
                    "name": "Base Environment", "data": data(&top.vars) }),
        ],
        left: 0,
    };
    out.walk(ws, nodes, "wrk_1", Some(&top))?;
    let export = json!({ "_type": "export", "__export_format": 4, "__export_source": "apitool",
                         "resources": out.resources });
    let text = serde_json::to_string_pretty(&export).map_err(|e| e.to_string())?;
    Ok((text, out.left))
}

struct Out {
    resources: Vec<Value>,
    left: usize,
}

impl Out {
    /// `top` is the exported folder, for the items directly in it.
    fn walk(
        &mut self,
        ws: &Workspace,
        nodes: &[Node],
        parent: &str,
        top: Option<&Folder>,
    ) -> Result<(), String> {
        for (order, node) in nodes.iter().enumerate() {
            match node {
                Node::Folder {
                    name,
                    path,
                    children,
                    ..
                } => {
                    let mut f = ws.load_folder(path)?;
                    if let Some(top) = top {
                        adopt(top, &mut f.auth, &mut f.pre_request, &mut f.tests);
                    }
                    let id = format!("fld_{}", self.resources.len());
                    self.resources.push(json!({
                        "_id": id, "_type": "request_group", "parentId": parent, "name": name,
                        "description": f.description, "environment": data(&f.vars),
                        "authentication": auth_out(&f.auth),
                        "preRequestScript": script_out(&f.pre_request),
                        "afterResponseScript": script_out(&f.tests), "metaSortKey": order,
                    }));
                    self.walk(ws, children, &id, None)?;
                }
                Node::Request { method, .. }
                    if matches!(method.as_str(), "SSE" | "GRPC" | "MQTT" | "SOCKETIO") =>
                {
                    self.left += 1
                }
                Node::Request { name, path, .. } => {
                    let mut req = ws.load_request(path)?;
                    if let Some(top) = top {
                        adopt(top, &mut req.auth, &mut req.pre_request, &mut req.tests);
                    }
                    let id = format!("req_{}", self.resources.len());
                    self.resources
                        .push(request_out(&id, parent, name, &req, order));
                }
            }
        }
        Ok(())
    }
}

/// What an item directly in the exported folder has of it: its auth where the item's is
/// inherited, and its scripts, run before the item's own as a folder's are.
fn adopt(top: &Folder, auth: &mut Auth, pre_request: &mut String, tests: &mut String) {
    if *auth == Auth::Inherit {
        *auth = top.auth.clone();
    }
    for (outer, own) in [(&top.pre_request, pre_request), (&top.tests, tests)] {
        if outer.trim().is_empty() {
            continue;
        }
        *own = match own.trim().is_empty() {
            true => outer.clone(),
            false => format!("{outer}\n{own}"),
        };
    }
}

fn request_out(id: &str, parent: &str, name: &str, req: &Request, order: usize) -> Value {
    let mut r = json!({
        "_id": id, "parentId": parent, "name": name, "description": req.description,
        "metaSortKey": order, "headers": rows_out(&req.headers),
        "authentication": auth_out(&req.auth),
    });
    match req.method.as_str() {
        // Its query stays in the URL: Insomnia's own exports have it there.
        "WS" => {
            r["_type"] = json!("websocket_request");
            r["url"] = json!(nunjucks(&req.url));
        }
        method => {
            r["_type"] = json!("request");
            r["method"] = json!(if method == "GRAPHQL" { "POST" } else { method });
            // The query is in `parameters`, off ones too.
            r["url"] = json!(nunjucks(req.url.split('?').next().unwrap_or_default()));
            r["parameters"] = rows_out(&req.params);
            r["pathParameters"] = rows_out(&req.path_vars);
            r["body"] = body_out(&req.body, &req.headers);
            r["preRequestScript"] = json!(script_out(&req.pre_request));
            r["afterResponseScript"] = json!(script_out(&req.tests));
        }
    }
    r
}

fn rows_out(rows: &[KeyValue]) -> Value {
    let row = |kv: &KeyValue| {
        let mut r = json!({ "name": nunjucks(&kv.key), "value": nunjucks(&kv.value) });
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

/// An environment's data has no off switch: off variables stay here.
fn data(vars: &[KeyValue]) -> Value {
    let on = vars.iter().filter(|v| v.enabled && !v.key.is_empty());
    Value::Object(
        on.map(|v| (v.key.clone(), json!(nunjucks(&v.value))))
            .collect(),
    )
}

fn body_out(body: &Body, headers: &[KeyValue]) -> Value {
    let typed = |mime: &str, text: &str| json!({ "mimeType": mime, "text": nunjucks(text) });
    // Insomnia types a text body by its own field, and sends that as the Content-Type.
    let declared = (headers.iter())
        .find(|h| h.enabled && h.key.eq_ignore_ascii_case("content-type"))
        .map_or("text/plain", |h| h.value.as_str());
    match body {
        Body::None => json!({}),
        Body::Json { text } => typed("application/json", text),
        Body::Text { text } => typed(declared, text),
        Body::Form { fields } => {
            json!({ "mimeType": "application/x-www-form-urlencoded", "params": rows_out(fields) })
        }
        Body::Multipart { parts } => {
            let mut params = rows_out(parts);
            for (row, part) in params.as_array_mut().into_iter().flatten().zip(parts) {
                if let Some(file) = part.value.strip_prefix('@') {
                    row["type"] = json!("file");
                    row["fileName"] = json!(file);
                    row["value"] = json!("");
                }
            }
            json!({ "mimeType": "multipart/form-data", "params": params })
        }
        Body::File { path } => json!({ "mimeType": "application/octet-stream", "fileName": path }),
        Body::GraphQL { query, variables } => {
            // An object as Insomnia writes it; as typed when it isn't JSON yet ({{vars}}).
            let variables = match variables.trim() {
                "" => json!({}),
                v => serde_json::from_str(v).unwrap_or_else(|_| json!(v)),
            };
            let envelope = json!({ "query": query, "variables": variables });
            typed("application/graphql", &envelope.to_string())
        }
        // ponytail: Insomnia has no body of typed bytes; text ones go as text, the rest is
        // left out, as for Postman.
        typed_bytes @ (Body::Base64 { .. } | Body::Hex { .. }) => {
            match typed_bytes.typed_bytes().and_then(|b| b.ok()) {
                Some(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => typed(declared, &text),
                    Err(_) => json!({}),
                },
                None => json!({}),
            }
        }
    }
}

/// `auth` backwards; `{}` inherits.
fn auth_out(a: &Auth) -> Value {
    let n = |s: &str| json!(nunjucks(s));
    match a {
        Auth::Inherit => json!({}),
        Auth::None => json!({ "type": "none" }),
        Auth::Basic { username, password } => {
            json!({ "type": "basic", "username": n(username), "password": n(password) })
        }
        Auth::Digest { username, password } => {
            json!({ "type": "digest", "username": n(username), "password": n(password) })
        }
        Auth::Bearer { token } => json!({ "type": "bearer", "token": n(token) }),
        Auth::ApiKey {
            key,
            value,
            in_query,
        } => json!({
            "type": "apikey", "key": n(key), "value": n(value),
            "addTo": if *in_query { "queryParams" } else { "header" },
        }),
        Auth::OAuth2(o) => json!({
            "type": "oauth2",
            "grantType": match o.grant {
                Grant::ClientCredentials => "client_credentials",
                Grant::Password => "password",
                Grant::AuthorizationCode => "authorization_code",
                Grant::Implicit => "implicit",
            },
            "accessTokenUrl": n(&o.token_url), "authorizationUrl": n(&o.auth_url),
            "clientId": n(&o.client_id), "clientSecret": n(&o.client_secret),
            "scope": n(&o.scope), "username": n(&o.username), "password": n(&o.password),
            "redirectUrl": n(&o.redirect_uri), "credentialsInBody": !o.basic_auth,
        }),
        Auth::AwsV4(a) => json!({
            "type": "iam", "accessKeyId": n(&a.access_key), "secretAccessKey": n(&a.secret_key),
            "region": n(&a.region), "service": n(&a.service), "sessionToken": n(&a.session_token),
        }),
        Auth::OAuth1(o) => {
            let secret = match o.signature_method.starts_with("RSA") {
                true => "privateKey",
                false => "consumerSecret",
            };
            let mut v = json!({
                "type": "oauth1", "signatureMethod": o.signature_method,
                "consumerKey": n(&o.consumer_key), "tokenKey": n(&o.token),
                "tokenSecret": n(&o.token_secret), "realm": n(&o.realm),
            });
            v[secret] = n(&o.consumer_secret);
            v
        }
        Auth::Hawk(h) => json!({
            "type": "hawk", "id": n(&h.id), "key": n(&h.key), "algorithm": h.algorithm,
            "ext": n(&h.ext), "validatePayload": h.payload_hash,
        }),
        // Insomnia has no domain field: it reads `DOMAIN\user` in the name.
        Auth::Ntlm(x) => {
            let user = match x.domain.as_str() {
                "" => x.username.clone(),
                domain => format!("{domain}\\{}", x.username),
            };
            json!({ "type": "ntlm", "username": n(&user), "password": n(&x.password) })
        }
        // ponytail: Insomnia has no JWT or EdgeGrid auth; such a request goes without.
        Auth::Jwt(_) | Auth::EdgeGrid(_) => json!({ "type": "none" }),
    }
}

static API_VARIABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([^{}]+?)\s*\}\}").expect("valid"));
static PM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bpm\.").expect("valid"));

fn script_out(s: &str) -> String {
    PM.replace_all(s, "insomnia.").into_owned()
}

/// `template` backwards: `{{name}}` as `{{ _.name }}` (bracketed unless it's a plain name,
/// which Nunjucks would read as an expression), dynamic variables as their tags.
fn nunjucks(s: &str) -> String {
    if !s.contains("{{") {
        return s.to_owned();
    }
    API_VARIABLE
        .replace_all(s, |c: &Captures| {
            let name = &c[1];
            let plain = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            match name {
                "$guid" | "$randomUUID" => "{% uuid 'v4' %}".to_owned(),
                "$timestamp" => "{% now 'unix' %}".to_owned(),
                "$isoTimestamp" => "{% now 'iso-8601' %}".to_owned(),
                n if n.starts_with('$') && crate::fake::value(n).is_some() => {
                    format!("{{% faker '{}' %}}", &n[1..])
                }
                n if n.starts_with('?') => format!("{{% prompt '{}' %}}", &n[1..]),
                n if plain => format!("{{{{ _.{n} }}}}"),
                n => format!("{{{{ _['{n}'] }}}}"),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postman::Env;

    /// Insomnia sends the client in a Basic header unless "Credentials in body" is ticked:
    /// an import that put it in the form could fail where the request worked.
    #[test]
    fn oauth2_sends_the_client_as_insomnia_does() {
        let basic = |a: Value| match auth(&a) {
            Ok(Auth::OAuth2(o)) => o.basic_auth,
            other => panic!("{other:?}"),
        };
        let a = json!({ "type": "oauth2", "grantType": "client_credentials" });
        assert!(basic(a.clone()));
        let mut body = a;
        body["credentialsInBody"] = true.into();
        assert!(!basic(body));
    }

    type Parts = (
        Vec<(String, Folder)>,
        Vec<(String, Request)>,
        Vec<String>,
        Vec<Env>,
    );

    fn collection(text: &str) -> Parts {
        let Ok(Import::Collection {
            folders,
            requests,
            warnings,
            environments,
            ..
        }) = crate::import::parse(text)
        else {
            panic!("a collection")
        };
        (folders, requests, warnings, environments)
    }

    /// Trimmed from yaak's fixture of a real Insomnia 10 export: the request has to send
    /// what it sent from Insomnia, with `{{ _.BASE_URL }}` from the environment it was in.
    #[test]
    fn a_v4_export_becomes_a_collection_with_its_environments() {
        let v4 = r#"{"_type": "export", "__export_format": 4, "resources": [
          {"_id": "req_1", "parentId": "fld_1", "_type": "request", "metaSortKey": 2,
           "name": "New Request", "description": "Docs", "method": "GET",
           "url": "{{ _.BASE_URL }}/foo/:id",
           "parameters": [{"name": "query", "value": "qqq"}, {"name": "off", "value": "1", "disabled": true}],
           "pathParameters": [{"name": "id", "value": "iii"}],
           "headers": [{"name": "X-Key", "value": "{{ _['api key'] }}"}, {"name": "", "value": ""},
                       {"name": "X-Otp", "value": "{% prompt 'One-time code', 'Code', '', '', true %}"}],
           "authentication": {"type": "bearer", "token": "{% uuid 'v4' %}"},
           "body": {"mimeType": "application/xml", "text": "<a>{% response 'body', 'req_2', '$.id' %}</a>"},
           "preRequestScript": "insomnia.environment.set('a', 1);"},
          {"_id": "req_0", "parentId": "fld_1", "_type": "request", "metaSortKey": 1,
           "name": "Query", "method": "POST", "url": "https://x.test/graphql",
           "body": {"mimeType": "application/graphql", "text": "{\"query\":\"{ me }\",\"variables\":{\"a\":1}}"},
           "authentication": {"type": "asap"}},
          {"_id": "grpc_1", "parentId": "fld_1", "_type": "grpc_request", "name": "Hello",
           "protoMethodName": "/hello.Hello/Say"},
          {"_id": "fld_1", "parentId": "wrk_1", "_type": "request_group", "name": "Top Level",
           "environment": {"TOKEN": "t"}, "authentication": {"type": "basic", "username": "u", "password": "p"}},
          {"_id": "wrk_1", "parentId": null, "_type": "workspace", "name": "Dummy"},
          {"_id": "jar_1", "parentId": "wrk_1", "_type": "cookie_jar", "name": "Default Jar"},
          {"_id": "env_1", "parentId": "wrk_1", "_type": "environment", "name": "Base Environment",
           "data": {"BASE_VAR": "hello", "PORT": 8080}},
          {"_id": "env_2", "parentId": "env_1", "_type": "environment", "name": "Production",
           "data": {"BASE_URL": "https://api.example.com"}},
          {"_id": "env_3", "parentId": "env_1", "_type": "environment", "name": "Mine",
           "isPrivate": true, "data": {"BASE_URL": "http://localhost"}}
        ]}"#;
        let (folders, requests, warnings, envs) = collection(v4);
        assert_eq!(
            folders[0].1.vars,
            [
                KeyValue::new("BASE_VAR", "hello"),
                KeyValue::new("PORT", "8080")
            ]
        );
        assert_eq!(folders[1].0, "Top Level");
        assert_eq!(folders[1].1.vars, [KeyValue::new("TOKEN", "t")]);
        assert!(matches!(folders[1].1.auth, Auth::Basic { .. }));
        let keys: Vec<_> = requests.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["Top Level/Query", "Top Level/New Request"],
            "Insomnia's order"
        );

        let (_, gql) = &requests[0];
        assert_eq!(gql.method, "GRAPHQL");
        assert_eq!(
            gql.body,
            Body::GraphQL {
                query: "{ me }".into(),
                variables: "{\n  \"a\": 1\n}".into()
            }
        );
        assert_eq!(
            gql.auth,
            Auth::None,
            "unsupported auth must not inherit Basic instead"
        );

        let (_, req) = &requests[1];
        assert_eq!(req.url, "{{BASE_URL}}/foo/:id?query=qqq");
        assert!(!req.params[1].enabled);
        assert_eq!(req.path_vars, [KeyValue::new("id", "iii")]);
        assert_eq!(
            req.headers,
            [
                KeyValue::new("X-Key", "{{api key}}"),
                KeyValue::new("X-Otp", "{{?One-time code}}"),
                KeyValue::new("Content-Type", "application/xml")
            ]
        );
        assert_eq!(
            req.auth,
            Auth::Bearer {
                token: "{{$guid}}".into()
            }
        );
        assert_eq!(req.pre_request, "pm.environment.set('a', 1);");
        assert_eq!(req.description, "Docs");

        assert_eq!(
            envs,
            [
                (
                    "Production".into(),
                    vec![KeyValue::new("BASE_URL", "https://api.example.com")],
                    vec![]
                ),
                (
                    "Mine".into(),
                    vec![],
                    vec![KeyValue::new("BASE_URL", "http://localhost")]
                ),
            ]
        );
        assert_eq!(
            warnings,
            [
                "Top Level/Hello: gRPC requests aren't imported; left out",
                "Top Level/Query: asap auth isn't supported; set to none",
                "template tag {% response 'body', 'req_2', '$.id' %} has no counterpart; left as it is",
            ]
        );
    }

    /// Insomnia's Hawk and NTLM, as its exports write them.
    #[test]
    fn hawk_and_ntlm_come_over() {
        let hawk = json!({"type": "hawk", "id": "dh37fgj492je", "key": "k", "algorithm": "sha1",
                          "ext": "x", "validatePayload": true});
        let expected = Hawk {
            id: "dh37fgj492je".into(),
            key: "k".into(),
            algorithm: "sha1".into(),
            ext: "x".into(),
            payload_hash: true,
            ..Default::default()
        };
        assert_eq!(auth(&hawk), Ok(Auth::Hawk(expected)));
        let ntlm = json!({"type": "ntlm", "username": "CORP\\u", "password": "p"});
        let expected = Ntlm {
            username: "CORP\\u".into(),
            password: "p".into(),
            ..Default::default()
        };
        assert_eq!(auth(&ntlm), Ok(Auth::Ntlm(expected)));
    }

    #[test]
    fn a_v5_yaml_export_comes_over_too() {
        let v5 = r#"type: collection.insomnia.rest/5.0
name: Dummy
meta:
  id: wrk_1
  description: The API
collection:
  - name: Top Level
    meta:
      id: fld_1
    children:
      - url: "{{ _.BASE_URL }}/items"
        name: Add
        meta:
          id: req_1
          description: Adds one
        method: POST
        body:
          mimeType: multipart/form-data
          params:
            - name: file
              type: file
              fileName: /tmp/a.png
            - name: title
              value: hi
        authentication:
          type: apikey
          key: k
          value: v
          addTo: queryParams
        scripts:
          afterResponse: insomnia.test('ok', () => {});
  - url: wss://echo.websocket.org
    name: Socket
    meta:
      id: ws-req_1
environments:
  name: Base Environment
  data:
    BASE_URL: https://api.example.com
  subEnvironments:
    - name: Staging
      data:
        BASE_URL: https://staging.example.com
"#;
        let (folders, requests, warnings, envs) = collection(v5);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(folders[0].1.description, "The API");
        assert_eq!(
            folders[0].1.vars,
            [KeyValue::new("BASE_URL", "https://api.example.com")]
        );
        let (key, add) = &requests[0];
        assert_eq!(key, "Top Level/Add");
        assert_eq!(add.description, "Adds one");
        let Body::Multipart { parts } = &add.body else {
            panic!("multipart")
        };
        assert_eq!(parts[0].value, "@/tmp/a.png");
        assert_eq!(
            add.auth,
            Auth::ApiKey {
                key: "k".into(),
                value: "v".into(),
                in_query: true
            }
        );
        assert_eq!(add.tests, "pm.test('ok', () => {});");
        assert_eq!(
            (requests[1].1.method.as_str(), requests[1].1.url.as_str()),
            ("WS", "wss://echo.websocket.org")
        );
        assert_eq!(envs[0].0, "Staging");
    }

    /// What goes to Insomnia must come back the same, in the shape the other apps check:
    /// Bruno wants the export marker and a workspace, Hoppscotch roots at parentId null.
    /// The folder's auth and scripts land on the items directly in it (a v4 workspace holds
    /// none), so each still runs with them; Nunjucks names a variable with a space by
    /// brackets, a dotted one would read as a path.
    #[test]
    fn a_folder_goes_to_insomnia_and_back() {
        use std::collections::HashMap;
        let root = std::env::temp_dir().join(format!("apitool-insomnia-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "api").unwrap();
        let admin = ws.create_folder(&api, "admin").unwrap();
        let top = Folder {
            description: "The API".into(),
            vars: vec![
                KeyValue::new("base", "https://x.test"),
                KeyValue::new("user id", "7"),
            ],
            auth: Auth::Bearer {
                token: "{{token}}".into(),
            },
            pre_request: "pm.environment.set('a', 1);".into(),
            ..Default::default()
        };
        ws.save_folder(&api, &top).unwrap();
        let sub = Folder {
            vars: vec![KeyValue::new("role", "admin")],
            auth: Auth::Basic {
                username: "u".into(),
                password: "{{pw}}".into(),
            },
            ..Default::default()
        };
        ws.save_folder(&admin, &sub).unwrap();
        let off = KeyValue {
            enabled: false,
            ..KeyValue::new("debug", "1")
        };
        let requests = [
            (
                "api/list",
                Request {
                    url: "{{base}}/items/:id?page=2".into(),
                    params: vec![KeyValue::new("page", "2"), off.clone()],
                    path_vars: vec![KeyValue::new("id", "{{user id}}")],
                    headers: vec![
                        KeyValue {
                            description: "who".into(),
                            ..KeyValue::new("X-Id", "{{$guid}}")
                        },
                        off.clone(),
                    ],
                    description: "Lists **items**.".into(),
                    tests: "pm.test('ok', () => {});".into(),
                    ..Default::default()
                },
            ),
            (
                "api/note",
                Request {
                    method: "POST".into(),
                    url: "{{base}}/notes".into(),
                    headers: vec![KeyValue::new("Content-Type", "application/xml")],
                    body: Body::Text {
                        text: "<n code=\"{{?Code}}\"/>".into(),
                    },
                    auth: Auth::ApiKey {
                        key: "k".into(),
                        value: "{{key}}".into(),
                        in_query: true,
                    },
                    pre_request: "console.log(2);".into(),
                    ..Default::default()
                },
            ),
            (
                "api/query",
                Request {
                    method: "GRAPHQL".into(),
                    url: "{{base}}/graphql".into(),
                    body: Body::GraphQL {
                        query: "{ me }".into(),
                        variables: "{\n  \"a\": 1\n}".into(),
                    },
                    ..Default::default()
                },
            ),
            (
                "api/live",
                Request {
                    method: "WS".into(),
                    url: "wss://x.test/live".into(),
                    ..Default::default()
                },
            ),
            (
                "api/feed",
                Request {
                    method: "SSE".into(),
                    url: "{{base}}/feed".into(),
                    ..Default::default()
                },
            ),
            (
                "api/admin/upload",
                Request {
                    method: "POST".into(),
                    url: "{{base}}/files".into(),
                    body: Body::Multipart {
                        parts: vec![
                            KeyValue::new("file", "@/tmp/a.png"),
                            KeyValue::new("title", "{{role}}"),
                        ],
                    },
                    ..Default::default()
                },
            ),
            (
                "api/admin/login",
                Request {
                    method: "POST".into(),
                    url: "{{base}}/login".into(),
                    body: Body::Form {
                        fields: vec![KeyValue::new("user", "me"), off.clone()],
                    },
                    auth: Auth::None,
                    ..Default::default()
                },
            ),
        ];
        for (name, req) in &requests {
            ws.save_request(&ws.request_path(name).unwrap(), req)
                .unwrap();
        }
        let (json, left) = export(&ws, &api).unwrap();
        assert_eq!(left, 1, "SSE has no Insomnia counterpart");
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            (v["_type"].as_str(), v["__export_format"].as_u64()),
            (Some("export"), Some(4))
        );
        let workspace = &v["resources"][0];
        assert_eq!(workspace["_type"], "workspace");
        assert_eq!(workspace["parentId"], Value::Null);
        assert!(json.contains("{{ _['user id'] }}"), "{json}");
        // The query only in `parameters`: Insomnia adds those to the URL, so it would go twice.
        assert!(
            json.contains(r#""url": "{{ _.base }}/items/:id","#),
            "{json}"
        );
        assert!(json.contains("insomnia.test('ok'"), "{json}");
        // Insomnia's own tags and a variables object, which apitool reads back either way.
        assert!(json.contains("{% uuid 'v4' %}") && json.contains("{% prompt 'Code' %}"));
        assert!(json.contains(r#"\"variables\":{\"a\":1}"#), "{json}");

        let (folders, back, warnings, _) = collection(&json);
        assert!(warnings.is_empty(), "{warnings:?}");
        let folders: HashMap<_, _> = folders.into_iter().collect();
        assert_eq!(folders[""].description, "The API");
        assert_eq!(folders[""].vars, top.vars);
        let mut sub_out = sub.clone();
        adopt(
            &top,
            &mut sub_out.auth,
            &mut sub_out.pre_request,
            &mut sub_out.tests,
        );
        assert_eq!(folders["admin"], sub_out);
        let back: HashMap<_, _> = back.into_iter().collect();
        assert_eq!(back.len(), 6);
        // As the folder's, not through `adopt` alone: inherited auth, its script before theirs.
        assert_eq!(back["list"].auth, top.auth);
        assert_eq!(
            back["note"].pre_request,
            "pm.environment.set('a', 1);\nconsole.log(2);"
        );
        assert_eq!(folders["admin"].pre_request, top.pre_request);
        for (name, _) in requests.iter().filter(|(n, _)| *n != "api/feed") {
            let mut want = ws.load_request(&ws.request_path(name).unwrap()).unwrap();
            (want.inherited, want.updated) = Default::default();
            if !name.starts_with("api/admin/") {
                adopt(&top, &mut want.auth, &mut want.pre_request, &mut want.tests);
            }
            // Insomnia's WebSocket requests have no scripts.
            if want.method == "WS" {
                (want.pre_request, want.tests) = Default::default();
            }
            assert_eq!(back[name.strip_prefix("api/").unwrap()], want, "{name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oauth1_takes_the_private_key_for_rsa() {
        let a = serde_json::json!({
            "type": "oauth1", "signatureMethod": "RSA-SHA256", "consumerKey": "ck",
            "consumerSecret": "unused", "privateKey": "PEM", "tokenKey": "t", "tokenSecret": "ts",
        });
        let Auth::OAuth1(o) = auth(&a).unwrap() else {
            panic!("oauth1")
        };
        assert_eq!(
            (o.consumer_key, o.consumer_secret, o.token, o.token_secret),
            ("ck".into(), "PEM".into(), "t".into(), "ts".into())
        );
        let mut hmac = a.clone();
        hmac["signatureMethod"] = "HMAC-SHA1".into();
        let Auth::OAuth1(o) = auth(&hmac).unwrap() else {
            panic!("oauth1")
        };
        assert_eq!(o.consumer_secret, "unused");
    }
}
