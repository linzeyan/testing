//! Insomnia exports: v4 JSON (a flat list of resources linked by `parentId`) and v5 YAML
//! (the same as a tree). Requests, folders, auth and scripts come over; the base
//! environment becomes the collection's variables, each sub-environment an environment.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::{Value, json};

use crate::model::{Auth, AwsV4, Body, Folder, Grant, KeyValue, OAuth1, OAuth2, Request};
use crate::postman::Import;
use crate::store::safe_name;

pub fn is_insomnia(v: &Value) -> bool {
    (v["__export_format"] == 4 && v["resources"].is_array())
        || str_of(&v["type"]).starts_with("collection.insomnia.rest/")
}

pub fn import(root: &Value) -> Import {
    let mut r = Reader::default();
    let root = templates(root, &mut r.tags);
    let (name, children, envs) = match root["resources"].as_array() {
        Some(resources) => v4(resources),
        None => (
            str_of(&root["name"]).to_owned(),
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
            "description": description_of(&root),
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
            n => safe_name(n),
        },
        folders: r.folders,
        requests: r.requests,
        warnings: r.warnings,
        environments,
    }
}

/// v4's flat list as v5's tree, children in the order Insomnia shows them.
fn v4(resources: &[Value]) -> (String, Value, (Value, Vec<Value>)) {
    let of_type = |t: &'static str| resources.iter().filter(move |r| r["_type"] == t);
    let Some(ws) = of_type("workspace").next() else {
        return (String::new(), json!([]), (Value::Null, Vec::new()));
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
    (name, tree(resources, &ws["_id"]), (base, subs))
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
            let base = safe_name(str_of(&child["name"]));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postman::Env;

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
           "authentication": {"type": "ntlm"}},
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
                "Top Level/Query: ntlm auth isn't supported; set to none",
                "template tag {% response 'body', 'req_2', '$.id' %} has no counterpart; left as it is",
            ]
        );
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
