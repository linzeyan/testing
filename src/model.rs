use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "WS", "SSE", "GRPC",
];

/// Methods whose response is a stream of messages rather than one body.
pub fn is_streaming(method: &str) -> bool {
    matches!(method, "WS" | "SSE")
}

/// One request per file on disk, so field order and `skip_serializing_if` matter:
/// they keep git diffs minimal.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Request {
    pub method: String,
    pub url: String,
    /// gRPC only: `.proto` file (relative to the workspace, so it syncs via git) and the
    /// fully-qualified method, `package.Service/Method`. The JSON body is the message.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proto: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub rpc: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Body::is_none")]
    pub body: Body,
    #[serde(skip_serializing_if = "Auth::is_none")]
    pub auth: Auth,
    /// JavaScript run before sending (may edit the request and variables).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pre_request: String,
    /// JavaScript run on the response (`pm.test`, variable capture).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tests: String,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            method: "GET".into(),
            url: String::new(),
            proto: String::new(),
            rpc: String::new(),
            params: Vec::new(),
            headers: Vec::new(),
            body: Body::None,
            auth: Auth::None,
            pre_request: String::new(),
            tests: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

fn is_enabled(b: &bool) -> bool {
    *b
}

impl KeyValue {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            enabled: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Body {
    #[default]
    None,
    Json {
        text: String,
    },
    Text {
        text: String,
    },
    Form {
        fields: Vec<KeyValue>,
    },
    #[serde(rename = "graphql")]
    GraphQL {
        query: String,
        variables: String,
    },
}

impl Body {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Auth {
    #[default]
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
}

impl Auth {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Replaces `{{name}}` with its value. Unknown names are left verbatim (so the server
/// error shows what was wrong) and reported in `missing`.
pub fn resolve(s: &str, vars: &HashMap<String, String>, missing: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        match vars.get(name) {
            Some(v) => out.push_str(v),
            None => {
                out.push_str(&rest[start..start + 2 + end + 2]);
                if !missing.iter().any(|m| m == name) {
                    missing.push(name.to_owned());
                }
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

impl Request {
    /// A copy with variables substituted and disabled rows dropped: exactly what goes on the wire.
    pub fn resolved(&self, vars: &HashMap<String, String>) -> (Request, Vec<String>) {
        fn kv(list: &[KeyValue], r: &mut dyn FnMut(&str) -> String) -> Vec<KeyValue> {
            list.iter()
                .filter(|p| p.enabled && !p.key.is_empty())
                .map(|p| KeyValue::new(r(&p.key), r(&p.value)))
                .collect()
        }
        let mut missing = Vec::new();
        let mut r = |s: &str| resolve(s, vars, &mut missing);
        let req = Request {
            method: self.method.clone(),
            url: r(&self.url),
            proto: r(&self.proto),
            rpc: r(&self.rpc),
            params: kv(&self.params, &mut r),
            headers: kv(&self.headers, &mut r),
            body: match &self.body {
                Body::None => Body::None,
                Body::Json { text } => Body::Json { text: r(text) },
                Body::Text { text } => Body::Text { text: r(text) },
                Body::Form { fields } => Body::Form {
                    fields: kv(fields, &mut r),
                },
                Body::GraphQL { query, variables } => Body::GraphQL {
                    query: r(query),
                    variables: r(variables),
                },
            },
            auth: match &self.auth {
                Auth::None => Auth::None,
                Auth::Bearer { token } => Auth::Bearer { token: r(token) },
                Auth::Basic { username, password } => Auth::Basic {
                    username: r(username),
                    password: r(password),
                },
            },
            // Scripts have already run by the time a request is resolved for the wire.
            pre_request: String::new(),
            tests: String::new(),
        };
        (req, missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_substitutes_and_reports_missing() {
        let vars = HashMap::from([("host".to_owned(), "api.test".to_owned())]);
        let mut missing = Vec::new();
        let out = resolve("https://{{ host }}/{{id}}/{{id}}?x={{", &vars, &mut missing);
        // Unknown vars stay visible in the URL so the failure is self-explanatory.
        assert_eq!(out, "https://api.test/{{id}}/{{id}}?x={{");
        assert_eq!(missing, ["id"]);
    }

    #[test]
    fn resolved_drops_disabled_rows() {
        let mut off = KeyValue::new("debug", "1");
        off.enabled = false;
        let req = Request {
            params: vec![off, KeyValue::new("q", "{{v}}")],
            ..Default::default()
        };
        let vars = HashMap::from([("v".to_owned(), "rust".to_owned())]);
        let (r, _) = req.resolved(&vars);
        assert_eq!(r.params, [KeyValue::new("q", "rust")]);
    }

    #[test]
    fn toml_round_trip_keeps_every_field() {
        // Files are the source of truth (and synced via git): losing a field on save is data loss.
        let mut off = KeyValue::new("X-Off", "1");
        off.enabled = false;
        let req = Request {
            method: "POST".into(),
            url: "https://{{host}}/users".into(),
            proto: "protos/users.proto".into(),
            rpc: "users.v1.Users/Get".into(),
            params: vec![KeyValue::new("page", "2")],
            headers: vec![off],
            body: Body::Json {
                text: "{\n  \"name\": \"測試\"\n}".into(),
            },
            auth: Auth::Basic {
                username: "u".into(),
                password: "{{pw}}".into(),
            },
            pre_request: "pm.environment.set(\"ts\", Date.now());".into(),
            tests: "pm.test(\"ok\", function () {\n    pm.response.to.have.status(200);\n});\n"
                .into(),
        };
        let text = toml::to_string_pretty(&req).unwrap();
        assert_eq!(toml::from_str::<Request>(&text).unwrap(), req, "{text}");
        let form = Request {
            body: Body::Form {
                fields: vec![KeyValue::new("a", "b")],
            },
            ..Default::default()
        };
        assert_eq!(
            toml::from_str::<Request>(&toml::to_string_pretty(&form).unwrap()).unwrap(),
            form
        );
    }
}
