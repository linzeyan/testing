use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "GRAPHQL", "WS", "SSE", "GRPC",
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
    /// Markdown, for the generated API docs.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Body::is_none")]
    pub body: Body,
    #[serde(skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    /// JavaScript run before sending (may edit the request and variables).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pre_request: String,
    /// JavaScript run on the response (`pm.test`, variable capture).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tests: String,
    /// Saved responses, for reference and documentation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Example>,
    /// From the folders above; filled in when loaded from the workspace, never saved.
    #[serde(skip)]
    pub inherited: Inherited,
}

/// A folder's `.folder.toml`: what every request below it shares, like a Postman
/// collection or folder.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Folder {
    /// Markdown, for the generated API docs.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<KeyValue>,
    /// Used by requests (and subfolders) whose auth is `inherit`.
    #[serde(skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    /// Runs before the request's own pre-request script.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pre_request: String,
    /// Runs before the request's own tests.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tests: String,
}

/// A request's folders folded together, outermost first. Scripts carry their folder's
/// name so an error says where it came from.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Inherited {
    /// Inner folders override outer ones.
    pub vars: HashMap<String, String>,
    /// From the nearest folder that sets one, with that folder's name.
    pub auth: Option<(String, Auth)>,
    pub pre_request: Vec<(String, String)>,
    pub tests: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Example {
    pub name: String,
    pub status: u16,
    /// ponytail: only the content type is kept from the headers; keep more when a
    /// consumer (mock server) needs them.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    pub body: String,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            method: "GET".into(),
            url: String::new(),
            proto: String::new(),
            rpc: String::new(),
            description: String::new(),
            params: Vec::new(),
            headers: Vec::new(),
            body: Body::None,
            auth: Auth::Inherit,
            pre_request: String::new(),
            tests: String::new(),
            examples: Vec::new(),
            inherited: Inherited::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
    /// What the row is for; documentation only, never sent.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
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
            description: String::new(),
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
    /// multipart/form-data. A value starting with `@` uploads that file, as in `curl -F`.
    /// ponytail: so a literal text value can't start with `@`; add a per-part flag if needed.
    Multipart {
        parts: Vec<KeyValue>,
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
    /// From the enclosing folder, else none; what new requests start with, as in Postman.
    #[default]
    Inherit,
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    /// Answers the server's 401 challenge, so it costs one extra round trip.
    Digest {
        username: String,
        password: String,
    },
    #[serde(rename = "oauth2")]
    OAuth2(OAuth2),
}

/// OAuth 2.0 grants that need no browser. ponytail: authorization code (browser + local
/// redirect) is left out until someone needs it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct OAuth2 {
    pub grant: Grant,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    /// Password grant only.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    #[default]
    ClientCredentials,
    Password,
}

impl Auth {
    fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }
}

/// Postman's dynamic variables: a fresh value on every use, no definition needed.
pub const DYNAMIC: &[(&str, &str)] = &[
    ("$guid", "random UUID v4"),
    ("$randomUUID", "random UUID v4"),
    ("$timestamp", "Unix time, seconds"),
    ("$isoTimestamp", "current UTC time, ISO 8601"),
    ("$randomInt", "random integer 0-1000"),
];

fn dynamic(name: &str) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let random = || {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("OS random source");
        b
    };
    Some(match name {
        "$guid" | "$randomUUID" => {
            let mut b = random();
            b[6] = (b[6] & 0x0f) | 0x40; // version 4
            b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
            let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )
        }
        "$timestamp" => now.as_secs().to_string(),
        "$isoTimestamp" => iso8601(now),
        "$randomInt" => (u32::from_le_bytes(random()[..4].try_into().unwrap()) % 1001).to_string(),
        _ => return None,
    })
}

/// UTC "YYYY-MM-DDTHH:MM:SS.mmmZ" without a date library (days-to-civil, H. Hinnant).
fn iso8601(since_epoch: std::time::Duration) -> String {
    let secs = since_epoch.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since_epoch.subsec_millis()
    )
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
        match vars.get(name).cloned().or_else(|| dynamic(name)) {
            Some(v) => out.push_str(&v),
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

/// Splits a URL into (before `?`, query, `#fragment`); query excludes the `?`.
fn split_url(url: &str) -> (&str, &str, &str) {
    let (rest, fragment) = match url.find('#') {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    match rest.split_once('?') {
        Some((base, query)) => (base, query, fragment),
        None => (rest, "", fragment),
    }
}

impl Request {
    /// Rebuilds the params table from the URL's query string. The URL is what gets sent;
    /// disabled rows exist only in the table, as in Postman, so they are kept at the end.
    pub fn params_from_url(&mut self) {
        let (_, query, _) = split_url(&self.url);
        let mut params: Vec<KeyValue> = query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                KeyValue::new(k, v)
            })
            .collect();
        // The URL has no room for descriptions; keep each one with its row: same key, or
        // else the same place (that row's key is being edited).
        let old: Vec<&KeyValue> = self.params.iter().filter(|p| p.enabled).collect();
        let keys: Vec<String> = params.iter().map(|p| p.key.clone()).collect();
        for (i, p) in params.iter_mut().enumerate() {
            let same_key = old.iter().find(|o| o.key == p.key);
            let same_place = old.get(i).filter(|o| !keys.contains(&o.key));
            if let Some(o) = same_key.or(same_place) {
                p.description = o.description.clone();
            }
        }
        params.extend(self.params.iter().filter(|p| !p.enabled).cloned());
        self.params = params;
    }

    /// Rewrites the URL's query string from the enabled params rows (text kept as typed;
    /// encoding happens when the URL is parsed for sending).
    pub fn url_from_params(&mut self) {
        let (base, _, fragment) = split_url(&self.url);
        let query: Vec<String> = self
            .params
            .iter()
            .filter(|p| p.enabled && !(p.key.is_empty() && p.value.is_empty()))
            .map(|p| match p.value.as_str() {
                "" => p.key.clone(),
                v => format!("{}={v}", p.key),
            })
            .collect();
        let query = if query.is_empty() {
            String::new()
        } else {
            format!("?{}", query.join("&"))
        };
        self.url = format!("{base}{query}{fragment}");
    }

    /// Makes the URL and the params table agree. Files and MCP clients may set either one,
    /// so a URL without a query string takes its query from the enabled params.
    pub fn sync_params(&mut self) {
        let has_query = !split_url(&self.url).1.is_empty();
        if !has_query && self.params.iter().any(|p| p.enabled && !p.key.is_empty()) {
            self.url_from_params();
        } else {
            self.params_from_url();
        }
    }

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
            description: String::new(),
            params: kv(&self.params, &mut r),
            headers: kv(&self.headers, &mut r),
            body: match &self.body {
                Body::None => Body::None,
                Body::Json { text } => Body::Json { text: r(text) },
                Body::Text { text } => Body::Text { text: r(text) },
                Body::Form { fields } => Body::Form {
                    fields: kv(fields, &mut r),
                },
                Body::Multipart { parts } => Body::Multipart {
                    parts: kv(parts, &mut r),
                },
                Body::GraphQL { query, variables } => Body::GraphQL {
                    query: r(query),
                    variables: r(variables),
                },
            },
            auth: match self.effective_auth() {
                Auth::None | Auth::Inherit => Auth::None,
                Auth::Bearer { token } => Auth::Bearer { token: r(token) },
                Auth::Basic { username, password } => Auth::Basic {
                    username: r(username),
                    password: r(password),
                },
                Auth::Digest { username, password } => Auth::Digest {
                    username: r(username),
                    password: r(password),
                },
                Auth::OAuth2(o) => Auth::OAuth2(OAuth2 {
                    grant: o.grant,
                    token_url: r(&o.token_url),
                    client_id: r(&o.client_id),
                    client_secret: r(&o.client_secret),
                    scope: r(&o.scope),
                    username: r(&o.username),
                    password: r(&o.password),
                }),
            },
            // Scripts have already run by the time a request is resolved for the wire.
            pre_request: String::new(),
            tests: String::new(),
            examples: Vec::new(),
            inherited: Inherited::default(),
        };
        (req, missing)
    }

    /// The auth that applies: its own, or the inherited one.
    pub fn effective_auth(&self) -> &Auth {
        match (&self.auth, &self.inherited.auth) {
            (Auth::Inherit, Some((_, auth))) => auth,
            (auth, _) => auth,
        }
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
    fn dynamic_variables_need_no_definition() {
        let mut missing = Vec::new();
        let vars = HashMap::new();
        let id = resolve("{{$guid}}", &vars, &mut missing);
        // Shape of a v4 UUID: version nibble 4, variant nibble 8-b.
        assert_eq!((id.len(), &id[14..15]), (36, "4"), "{id}");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "{id}");
        assert_ne!(
            id,
            resolve("{{$guid}}", &vars, &mut missing),
            "fresh per use"
        );
        assert_eq!(
            iso8601(std::time::Duration::from_millis(951_782_400_123)),
            "2000-02-29T00:00:00.123Z"
        );
        let ts: u64 = resolve("{{$timestamp}}", &vars, &mut missing)
            .parse()
            .unwrap();
        assert!(ts > 1_700_000_000);
        assert!(missing.is_empty());
        // A user-defined variable with the same name wins.
        let own = HashMap::from([("$timestamp".to_owned(), "fixed".to_owned())]);
        assert_eq!(resolve("{{$timestamp}}", &own, &mut missing), "fixed");
    }

    #[test]
    fn url_and_params_stay_in_sync_both_ways() {
        let mut off = KeyValue::new("debug", "1");
        off.enabled = false;
        let mut req = Request {
            url: "{{host}}/users?page=2&q={{term}}&flag#top".into(),
            params: vec![off.clone()],
            ..Default::default()
        };
        // Typing in the URL updates the table, keeping the disabled row.
        req.params_from_url();
        assert_eq!(
            req.params,
            [
                KeyValue::new("page", "2"),
                KeyValue::new("q", "{{term}}"),
                KeyValue::new("flag", ""),
                off.clone()
            ]
        );
        // Editing the table rewrites the query; disabled rows never reach the URL.
        req.params[0].value = "3".into();
        req.params.remove(1);
        req.url_from_params();
        assert_eq!(req.url, "{{host}}/users?page=3&flag#top");
        // A file (or an MCP client) that only sets params gets them into the URL.
        let mut from_file = Request {
            url: "http://x/a".into(),
            params: vec![KeyValue::new("k", "v"), off],
            ..Default::default()
        };
        from_file.sync_params();
        assert_eq!(from_file.url, "http://x/a?k=v");
        assert_eq!(from_file.params.len(), 2);
    }

    #[test]
    fn param_descriptions_survive_url_edits() {
        let described = |k: &str, v: &str, d: &str| KeyValue {
            description: d.into(),
            ..KeyValue::new(k, v)
        };
        // Loading a file rebuilds the table from the URL; that must not wipe descriptions.
        let mut req = Request {
            url: "http://x/a?page=1&size=10".into(),
            params: vec![
                described("page", "1", "1-based"),
                described("size", "10", "max 100"),
            ],
            ..Default::default()
        };
        req.sync_params();
        assert_eq!(req.params[0].description, "1-based");
        // Typing a value, reordering, and renaming a key in place keep them too.
        for url in [
            "http://x/a?page=2&size=10",
            "http://x/a?size=10&page=2",
            "http://x/a?size=10&p=2",
        ] {
            req.url = url.into();
            req.params_from_url();
        }
        assert_eq!(
            req.params,
            [
                described("size", "10", "max 100"),
                described("p", "2", "1-based")
            ]
        );
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
            description: "Fetches **one** user.\n".into(),
            params: vec![KeyValue {
                description: "1-based".into(),
                ..KeyValue::new("page", "2")
            }],
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
            examples: vec![Example {
                name: "found".into(),
                status: 200,
                content_type: "application/json".into(),
                body: "{\n  \"id\": 1\n}".into(),
            }],
            // Derived from the folders on load, never written.
            inherited: Inherited::default(),
        };
        let text = toml::to_string_pretty(&req).unwrap();
        assert_eq!(toml::from_str::<Request>(&text).unwrap(), req, "{text}");
        let form = Request {
            body: Body::Form {
                fields: vec![KeyValue::new("a", "b")],
            },
            // An explicit "No auth" must not come back as "inherit" from the folder.
            auth: Auth::None,
            ..Default::default()
        };
        assert_eq!(
            toml::from_str::<Request>(&toml::to_string_pretty(&form).unwrap()).unwrap(),
            form
        );
    }
}
