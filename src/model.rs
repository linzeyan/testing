use std::collections::HashMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// QUERY is the safe, body-carrying GET of draft-ietf-httpbis-safe-method-w-body.
pub const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "QUERY", "TRACE", "CONNECT",
    "GRAPHQL", "WS", "SSE", "GRPC", "MQTT", "SOCKETIO",
];

/// Methods whose response is a stream of messages rather than one body.
pub fn is_streaming(method: &str) -> bool {
    matches!(method, "WS" | "SSE" | "MQTT" | "SOCKETIO")
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
    /// Values for the URL's `/:name` segments, as in Postman; the keys follow the URL.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path_vars: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Body::is_empty")]
    pub body: Body,
    #[serde(skip_serializing_if = "Auth::is_unset")]
    pub auth: Auth,
    /// JavaScript run before sending (may edit the request and variables).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pre_request: String,
    /// JavaScript run on the response (`pm.test`, variable capture).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tests: String,
    /// Bruno-style checks without JS: key `res.status`, value `eq 200` (the operator
    /// first; none means `eq`). Each row is a test result.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub asserts: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Settings::is_default")]
    pub settings: Settings,
    #[serde(skip_serializing_if = "Mqtt::is_default")]
    pub mqtt: Mqtt,
    /// Saved responses, for reference and documentation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Example>,
    /// From the folders above; filled in when loaded from the workspace, never saved.
    #[serde(skip)]
    pub inherited: Inherited,
}

/// How one HTTP request goes out, like Postman's per-request Settings tab. The defaults
/// are what every other request does.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    pub http_version: HttpVersion,
    pub follow_redirects: bool,
    pub max_redirects: u32,
    /// Off skips certificate checks for this request only (the network setting does it
    /// for all).
    pub verify_tls: bool,
    /// Send and store cookies with the cookie jar.
    pub cookies: bool,
    /// 0 is the network settings' timeout.
    pub timeout_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            http_version: HttpVersion::Auto,
            follow_redirects: true,
            max_redirects: 10,
            verify_tls: true,
            cookies: true,
            timeout_ms: 0,
        }
    }
}

impl Settings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// MQTT only: the connection, what it subscribes to, and where Send publishes, as in
/// Postman's MQTT request.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Mqtt {
    /// Empty: a new random one per connection. A broker drops the older of two
    /// connections with the same ID.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    /// MQTT 5 instead of 3.1.1: reason codes on refusals and broker hang-ups.
    pub v5: bool,
    /// 0 sends no pings.
    pub keep_alive_secs: u16,
    /// Off: the broker keeps this client ID's subscriptions and queued messages between
    /// connections.
    pub clean_session: bool,
    /// Subscribed to on Connect, and kept in step while connected.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<Topic>,
    /// Where Send publishes.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub topic: String,
    pub qos: u8,
    /// The broker keeps the last retained message for whoever subscribes later.
    pub retain: bool,
    /// MQTT 5 user properties sent with every publish, like headers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<KeyValue>,
    /// What the broker publishes for this client when it drops without a goodbye. No
    /// topic: no will.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub will_topic: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub will_payload: String,
    pub will_qos: u8,
    pub will_retain: bool,
}

impl Default for Mqtt {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            v5: false,
            keep_alive_secs: 60,
            clean_session: true,
            topics: Vec::new(),
            topic: String::new(),
            qos: 0,
            retain: false,
            user_properties: Vec::new(),
            will_topic: String::new(),
            will_payload: String::new(),
            will_qos: 0,
            will_retain: false,
        }
    }
}

impl Mqtt {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Topic {
    /// `+` matches one level, `#` everything below.
    pub filter: String,
    pub qos: u8,
    #[serde(skip_serializing_if = "is_enabled")]
    pub enabled: bool,
}

impl Default for Topic {
    fn default() -> Self {
        Self {
            filter: String::new(),
            qos: 0,
            enabled: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum HttpVersion {
    /// HTTP/2 when the server offers it over TLS, else HTTP/1.1.
    #[default]
    Auto,
    Http1,
    Http2,
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
    #[serde(skip_serializing_if = "Auth::is_unset")]
    pub auth: Auth,
    /// Runs before the request's own pre-request script.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub pre_request: String,
    /// Runs before the request's own tests.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tests: String,
    /// The children as arranged by dragging: request names, and folder names ending in
    /// `/` (a folder and a request may share a name). Unlisted children follow in the
    /// default order, so a new or renamed-elsewhere item never vanishes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
}

impl Folder {
    /// `children` (entries as in `order`) as an `order`: empty when it is the default one,
    /// so a folder never rearranged keeps a clean .folder.toml.
    pub fn order_of(children: Vec<String>) -> Vec<String> {
        let mut default = children.clone();
        default.sort_by_key(|t| (!t.ends_with('/'), t.trim_end_matches('/').to_lowercase()));
        if default == children {
            Vec::new()
        } else {
            children
        }
    }
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
            path_vars: Vec::new(),
            headers: Vec::new(),
            body: Body::None,
            auth: Auth::Inherit,
            pre_request: String::new(),
            tests: String::new(),
            settings: Settings::default(),
            mqtt: Mqtt::default(),
            asserts: Vec::new(),
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
    /// A file's bytes as they are, streamed from disk when sent.
    File {
        path: String,
    },
    #[serde(rename = "graphql")]
    GraphQL {
        query: String,
        variables: String,
    },
}

impl Body {
    /// A type picked and nothing put in it. Sent, saved and shown as no body, so clicking
    /// a body type changes nothing until something is typed.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Json { text } | Self::Text { text } => text.is_empty(),
            Self::Form { fields } => fields.is_empty(),
            Self::Multipart { parts } => parts.is_empty(),
            Self::File { path } => path.is_empty(),
            Self::GraphQL { query, variables } => query.is_empty() && variables.is_empty(),
        }
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
    /// A key/value pair sent as a header, or as a query parameter.
    #[serde(rename = "apikey")]
    ApiKey {
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        in_query: bool,
    },
    /// Signed per request with the secret key; the signature covers the body and expires
    /// in 15 minutes.
    #[serde(rename = "awsv4")]
    AwsV4(AwsV4),
    /// Signed per request over the method, URL, query and form body.
    #[serde(rename = "oauth1")]
    OAuth1(OAuth1),
    /// A token signed per request and sent as a Bearer token.
    #[serde(rename = "jwt")]
    Jwt(Jwt),
}

/// ponytail: no callback/verifier fields, so the three-legged token dance is done
/// elsewhere and its access token pasted here; add them when someone runs the dance here.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct OAuth1 {
    /// One of `oauth1::METHODS`; empty is HMAC-SHA1.
    pub signature_method: String,
    pub consumer_key: String,
    /// For RSA-*, the PEM private key.
    pub consumer_secret: String,
    /// Empty for two-legged requests.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub token_secret: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub realm: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Jwt {
    /// One of `jwt::ALGORITHMS`.
    pub algorithm: String,
    /// The HMAC secret for HS*, a PEM private key for the rest.
    pub secret: String,
    /// The claims, as JSON.
    pub payload: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct AwsV4 {
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    /// e.g. execute-api, s3, lambda.
    pub service: String,
    /// Temporary credentials (STS) only.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_token: String,
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
    /// Authorization code and implicit: where the browser signs in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth_url: String,
    /// Authorization code and implicit: a loopback URL the provider sends the browser back to.
    /// Empty: `http://127.0.0.1:<a free port>/callback`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub redirect_uri: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    #[default]
    ClientCredentials,
    Password,
    /// Signs in through the system browser, with PKCE.
    AuthorizationCode,
    /// Signs in through the browser too, but the token comes straight back in the redirect
    /// (its fragment): older single-page-app providers that have no token endpoint for us.
    Implicit,
}

/// The claims a JWT Bearer auth starts with when picked.
pub const JWT_CLAIMS: &str = "{\n  \"sub\": \"\",\n  \"iat\": {{$timestamp}}\n}";

static INHERIT: Auth = Auth::Inherit;

impl Auth {
    /// Inherit, or a type picked with every field as picking it fills them (choices such as
    /// the grant or algorithm aside): nothing set yet, so it inherits, and saves as inherit.
    pub fn is_unset(&self) -> bool {
        match self {
            Self::Inherit => true,
            Self::None => false,
            Self::Bearer { token } => token.is_empty(),
            Self::Basic { username, password } | Self::Digest { username, password } => {
                username.is_empty() && password.is_empty()
            }
            Self::OAuth2(o) => {
                *o == OAuth2 {
                    grant: o.grant,
                    ..Default::default()
                }
            }
            Self::ApiKey { key, value, .. } => key.is_empty() && value.is_empty(),
            Self::AwsV4(a) => *a == AwsV4::default(),
            Self::OAuth1(o) => {
                *o == OAuth1 {
                    signature_method: o.signature_method.clone(),
                    ..Default::default()
                }
            }
            Self::Jwt(j) => j.secret.is_empty() && j.payload == JWT_CLAIMS,
        }
    }
}

/// Postman's dynamic variables: a fresh value on every use, no definition needed.
pub use crate::fake::DYNAMIC;

/// UTC "YYYY-MM-DDTHH:MM:SS.mmmZ" without a date library (days-to-civil, H. Hinnant).
pub(crate) fn iso8601(since_epoch: std::time::Duration) -> String {
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
        match vars.get(name).cloned().or_else(|| crate::fake::value(name)) {
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

/// The variable a whole path segment names: Postman's `:id`, or OpenAPI's `{id}` so a path
/// copied from Swagger UI works as pasted. `{{id}}` is an environment variable instead.
pub fn path_var(segment: &str) -> Option<&str> {
    let name = match segment.strip_prefix(':') {
        Some(name) => name,
        None => {
            (segment.strip_prefix('{')?.strip_suffix('}')).filter(|n| !n.contains(['{', '}']))?
        }
    };
    (!name.is_empty()).then_some(name)
}

/// Byte ranges of the path variable segments. A port's ':' never starts a segment, so
/// `host:8080` is not taken for one.
pub fn path_var_spans(url: &str) -> Vec<Range<usize>> {
    let (base, _, _) = split_url(url);
    let mut spans = Vec::new();
    let mut at = 0;
    for (i, segment) in base.split('/').enumerate() {
        if i > 0 && path_var(segment).is_some() {
            spans.push(at..at + segment.len());
        }
        at += segment.len() + 1;
    }
    spans
}

/// The names of the path variables, in order.
fn path_names(url: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in path_var_spans(url)
        .into_iter()
        .filter_map(|r| path_var(&url[r]))
    {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// `/:name` or `/{name}` as a whole path segment becomes `/value`.
pub fn fill(url: &str, name: &str, value: &str) -> String {
    let mut url = url.to_owned();
    for segment in [format!("/:{name}"), format!("/{{{name}}}")] {
        let mut out = String::new();
        let mut rest = url.as_str();
        while let Some(i) = rest.find(&segment) {
            let after = &rest[i + segment.len()..];
            let whole = after
                .chars()
                .next()
                .is_none_or(|c| matches!(c, '/' | '?' | '#'));
            out.push_str(&rest[..i]);
            out.push('/');
            out.push_str(if whole { value } else { &segment[1..] });
            rest = after;
        }
        out.push_str(rest);
        url = out;
    }
    url
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

    /// Rebuilds the path variable rows from the URL's `/:name` and `/{name}` segments. A
    /// row keeps its value and description by name, or else by place (its name is being
    /// edited).
    pub fn path_vars_from_url(&mut self) {
        let names = path_names(&self.url);
        let old = std::mem::take(&mut self.path_vars);
        self.path_vars = (names.iter().enumerate())
            .map(|(i, name)| {
                let same_name = old.iter().find(|o| &o.key == name);
                let same_place = old.get(i).filter(|o| !names.contains(&o.key));
                match same_name.or(same_place) {
                    Some(o) => KeyValue {
                        key: name.clone(),
                        ..o.clone()
                    },
                    None => KeyValue::new(name, ""),
                }
            })
            .collect();
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
        self.path_vars_from_url();
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
        // An empty value leaves `:name` in the URL, which shows what was left out.
        let url = (self.path_vars.iter())
            .filter(|p| !p.value.is_empty())
            .fold(self.url.clone(), |url, p| fill(&url, &p.key, &p.value));
        let mut req = Request {
            method: self.method.clone(),
            url: r(&url),
            proto: r(&self.proto),
            rpc: r(&self.rpc),
            description: String::new(),
            params: kv(&self.params, &mut r),
            path_vars: Vec::new(),
            headers: kv(&self.headers, &mut r),
            body: match &self.body {
                b if b.is_empty() => Body::None,
                Body::None => Body::None,
                Body::Json { text } => Body::Json { text: r(text) },
                Body::Text { text } => Body::Text { text: r(text) },
                Body::Form { fields } => Body::Form {
                    fields: kv(fields, &mut r),
                },
                Body::Multipart { parts } => Body::Multipart {
                    parts: kv(parts, &mut r),
                },
                Body::File { path } => Body::File { path: r(path) },
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
                    auth_url: r(&o.auth_url),
                    redirect_uri: r(&o.redirect_uri),
                }),
                // Becomes a header or a query parameter below.
                Auth::ApiKey { .. } => Auth::None,
                Auth::AwsV4(a) => Auth::AwsV4(AwsV4 {
                    access_key: r(&a.access_key),
                    secret_key: r(&a.secret_key),
                    region: r(&a.region),
                    service: r(&a.service),
                    session_token: r(&a.session_token),
                }),
                Auth::OAuth1(o) => Auth::OAuth1(OAuth1 {
                    signature_method: o.signature_method.clone(),
                    consumer_key: r(&o.consumer_key),
                    consumer_secret: r(&o.consumer_secret),
                    token: r(&o.token),
                    token_secret: r(&o.token_secret),
                    realm: r(&o.realm),
                }),
                Auth::Jwt(j) => Auth::Jwt(Jwt {
                    algorithm: j.algorithm.clone(),
                    secret: r(&j.secret),
                    payload: r(&j.payload),
                }),
            },
            // Scripts have already run by the time a request is resolved for the wire.
            pre_request: String::new(),
            tests: String::new(),
            settings: self.settings.clone(),
            mqtt: Mqtt {
                client_id: r(&self.mqtt.client_id),
                topics: (self.mqtt.topics.iter())
                    .filter(|t| t.enabled && !t.filter.trim().is_empty())
                    .map(|t| Topic {
                        filter: r(t.filter.trim()),
                        ..t.clone()
                    })
                    .collect(),
                topic: r(&self.mqtt.topic),
                will_topic: r(self.mqtt.will_topic.trim()),
                will_payload: r(&self.mqtt.will_payload),
                ..self.mqtt.clone()
            },
            asserts: Vec::new(),
            examples: Vec::new(),
            inherited: Inherited::default(),
        };
        if let Auth::ApiKey {
            key,
            value,
            in_query,
        } = self.effective_auth()
            && !key.trim().is_empty()
        {
            let (key, value) = (r(key.trim()), r(value));
            if *in_query {
                // Kept as typed, like the params table; encoding happens when sending.
                let (base, query, fragment) = split_url(&req.url);
                let query = match query {
                    "" => format!("{key}={value}"),
                    q => format!("{q}&{key}={value}"),
                };
                req.url = format!("{base}?{query}{fragment}");
                req.params.push(KeyValue::new(key, value));
            } else {
                req.headers.push(KeyValue::new(key, value));
            }
        }
        (req, missing)
    }

    /// The auth that applies: its own, or the inherited one when its own is unset.
    pub fn effective_auth(&self) -> &Auth {
        match (&self.auth, &self.inherited.auth) {
            (own, inherited) if own.is_unset() => inherited.as_ref().map_or(&INHERIT, |(_, a)| a),
            (own, _) => own,
        }
    }

    /// Whether saving either would store the same: an empty body is no body and an unset
    /// auth is inherit (they're saved that way), so picking a type alone isn't an edit.
    /// `inherited` comes from the folders, never from the file, and isn't compared.
    pub fn same_as(&self, other: &Request) -> bool {
        let Request {
            method,
            url,
            proto,
            rpc,
            description,
            params,
            path_vars,
            headers,
            body,
            auth,
            pre_request,
            tests,
            asserts,
            settings,
            mqtt,
            examples,
            inherited: _,
        } = self;
        (method, url, proto, rpc) == (&other.method, &other.url, &other.proto, &other.rpc)
            && (description, params, path_vars)
                == (&other.description, &other.params, &other.path_vars)
            && (headers, pre_request, tests) == (&other.headers, &other.pre_request, &other.tests)
            && (asserts, settings, mqtt) == (&other.asserts, &other.settings, &other.mqtt)
            && examples == &other.examples
            && (body == &other.body || body.is_empty() && other.body.is_empty())
            && (auth == &other.auth || auth.is_unset() && other.auth.is_unset())
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
    fn path_variables_follow_the_url_and_fill_it_on_send() {
        let mut req = Request {
            url: "http://localhost:8080/orgs/:org/users/:id?x=:no#:no".into(),
            ..Default::default()
        };
        // The port and the query are not path segments.
        req.sync_params();
        let keys: Vec<_> = req.path_vars.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["org", "id"]);
        req.path_vars[0].value = "acme".into();
        req.path_vars[1] = KeyValue {
            description: "numeric".into(),
            ..KeyValue::new("id", "{{uid}}")
        };
        // Renaming a segment in the URL keeps what was typed for it; a new one starts empty.
        req.url = "http://localhost:8080/orgs/:org/users/:user/:tab?x=:no#:no".into();
        req.path_vars_from_url();
        let user = KeyValue {
            description: "numeric".into(),
            ..KeyValue::new("user", "{{uid}}")
        };
        assert_eq!(req.path_vars[1], user);
        assert_eq!(req.path_vars[2], KeyValue::new("tab", ""));
        // Sending fills whole segments only; an empty value leaves `:tab` to show the gap.
        let vars = HashMap::from([("uid".to_owned(), "7".to_owned())]);
        let (wire, missing) = req.resolved(&vars);
        assert_eq!(
            wire.url,
            "http://localhost:8080/orgs/acme/users/7/:tab?x=:no#:no"
        );
        assert!(missing.is_empty());
        assert_eq!(
            fill("http://h/:id/:idx/:id", "id", "7"),
            "http://h/7/:idx/7"
        );
    }

    /// A path copied from Swagger UI (`/orders/{id}`) must work as pasted, without
    /// mistaking a `{{var}}` for one or filling a `{id}` that is only part of a segment.
    #[test]
    fn openapi_style_path_variables_work_as_pasted() {
        let url = "{{base}}/orders/{id}/{{v}}/{id}.json/:line?q={id}";
        let mut req = Request {
            url: url.into(),
            ..Default::default()
        };
        req.sync_params();
        let keys: Vec<_> = req.path_vars.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["id", "line"]);
        let spans: Vec<_> = (path_var_spans(url).into_iter()).map(|r| &url[r]).collect();
        assert_eq!(spans, ["{id}", ":line"], "what the URL bar colours");
        req.path_vars[0].value = "7".into();
        req.path_vars[1].value = "2".into();
        let vars = HashMap::from([("base".into(), "http://h".into()), ("v".into(), "x".into())]);
        assert_eq!(
            req.resolved(&vars).0.url,
            "http://h/orders/7/x/{id}.json/2?q={id}"
        );
        assert_eq!(path_var("{}"), None);
        assert_eq!(path_var(":"), None);
    }

    #[test]
    fn an_api_key_goes_out_as_a_header_or_a_query_parameter() {
        let vars = HashMap::from([("k".to_owned(), "s3cr3t".to_owned())]);
        let mut req = Request {
            url: "http://x/a?page=1#top".into(),
            auth: Auth::ApiKey {
                key: "X-Key".into(),
                value: "{{k}}".into(),
                in_query: false,
            },
            ..Default::default()
        };
        let (wire, _) = req.resolved(&vars);
        assert_eq!(wire.headers, [KeyValue::new("X-Key", "s3cr3t")]);
        assert_eq!(
            (wire.url.as_str(), &wire.auth),
            ("http://x/a?page=1#top", &Auth::None)
        );
        // In the query it joins the others, before the fragment.
        req.auth = Auth::ApiKey {
            key: "key".into(),
            value: "{{k}}".into(),
            in_query: true,
        };
        let (wire, _) = req.resolved(&vars);
        assert_eq!(wire.url, "http://x/a?page=1&key=s3cr3t#top");
        assert!(wire.headers.is_empty());
        req.url = "http://x/a".into();
        assert_eq!(req.resolved(&vars).0.url, "http://x/a?key=s3cr3t");
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
            url: "https://{{host}}/users/:id".into(),
            proto: "protos/users.proto".into(),
            rpc: "users.v1.Users/Get".into(),
            description: "Fetches **one** user.\n".into(),
            params: vec![KeyValue {
                description: "1-based".into(),
                ..KeyValue::new("page", "2")
            }],
            path_vars: vec![KeyValue {
                description: "user ID".into(),
                ..KeyValue::new("id", "{{uid}}")
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
            asserts: vec![KeyValue::new("res.body.items", "length 3")],
            settings: Settings {
                http_version: HttpVersion::Http1,
                follow_redirects: false,
                timeout_ms: 1500,
                ..Default::default()
            },
            mqtt: Mqtt {
                client_id: "{{device}}".into(),
                v5: true,
                keep_alive_secs: 0,
                clean_session: false,
                topics: vec![Topic {
                    filter: "sensors/+/temp".into(),
                    qos: 2,
                    enabled: false,
                }],
                topic: "cmd".into(),
                qos: 1,
                retain: true,
                user_properties: vec![KeyValue::new("trace", "{{id}}")],
                will_topic: "status/{{device}}".into(),
                will_payload: "offline".into(),
                will_qos: 2,
                will_retain: true,
            },
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

    /// A body or auth type picked and left empty is the same request as none picked: not
    /// sent, not saved, and not an edit. Anything typed into it makes it real.
    #[test]
    fn an_empty_body_and_a_blank_auth_are_not_set() {
        let plain = Request::default();
        let with = |body: Body, auth: Auth| Request {
            body,
            auth,
            ..Default::default()
        };
        let empty = [
            Body::Json { text: "".into() },
            Body::Text { text: "".into() },
            Body::Form { fields: vec![] },
            Body::Multipart { parts: vec![] },
            Body::File { path: "".into() },
        ];
        let blank = [
            Auth::Bearer { token: "".into() },
            Auth::Basic {
                username: "".into(),
                password: "".into(),
            },
            Auth::OAuth2(OAuth2 {
                grant: Grant::Password,
                ..Default::default()
            }),
            Auth::Jwt(Jwt {
                algorithm: "RS256".into(),
                secret: "".into(),
                payload: JWT_CLAIMS.into(),
            }),
        ];
        for (body, auth) in empty.into_iter().zip(blank) {
            let picked = with(body, auth);
            assert!(
                picked.same_as(&plain) && plain.same_as(&picked),
                "{picked:?}"
            );
            let saved = toml::to_string_pretty(&picked).unwrap();
            assert_eq!(saved, toml::to_string_pretty(&plain).unwrap());
            assert_eq!(picked.resolved(&HashMap::new()).0.body, Body::None);
        }

        let typed = [
            with(Body::Json { text: "{}".into() }, Auth::Inherit),
            with(Body::Text { text: " ".into() }, Auth::Inherit),
            with(Body::None, Auth::Bearer { token: "t".into() }),
            // An explicit "No auth" is a choice, unlike a blank one.
            with(Body::None, Auth::None),
            // Claims edited before the secret: not blank, or the edit couldn't be saved.
            with(
                Body::None,
                Auth::Jwt(Jwt {
                    payload: "{}".into(),
                    ..Default::default()
                }),
            ),
        ];
        for req in typed {
            assert!(!req.same_as(&plain), "{req:?}");
        }

        // Blank inherits what the folders set, as no auth picked would.
        let mut req = with(Body::None, Auth::Bearer { token: "".into() });
        assert_eq!(req.effective_auth(), &Auth::Inherit);
        let folder = Auth::Bearer {
            token: "folder".into(),
        };
        req.inherited.auth = Some(("api".into(), folder.clone()));
        assert_eq!(req.effective_auth(), &folder);
    }
}
