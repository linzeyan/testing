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
    /// Bruno's post-response vars: key a variable, value what to take from the response
    /// (`res.body.token`, as in `asserts`). Set in the environment before the scripts run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub response_vars: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Settings::is_default")]
    pub settings: Settings,
    #[serde(skip_serializing_if = "Mqtt::is_default")]
    pub mqtt: Mqtt,
    #[serde(skip_serializing_if = "SocketIo::is_default")]
    pub socketio: SocketIo,
    /// Messages kept to send again on a stream (WebSocket, Socket.IO, MQTT, gRPC), as
    /// Postman's saved messages.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
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
    // The fields below are left out at their defaults, so a request saved before they
    // existed saves unchanged.
    /// A 301/302/303 keeps the method and body instead of switching to GET.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub follow_original_method: bool,
    /// Authorization still goes along when a redirect leads to another host.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub follow_auth_header: bool,
    /// No Referer on the request a redirect leads to.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub remove_referer: bool,
    /// TLS 1.2 and 1.3 are the only versions rustls speaks; each can be turned off.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disable_tls12: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disable_tls13: bool,
    /// Cipher suites offered, in order, by OpenSSL's names as Postman keeps them (see
    /// `net::ciphers`); empty offers all. Names apitool doesn't have are kept but skipped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ciphers: Vec<String>,
    /// Accepts responses with malformed headers, as Postman's `insecureHTTPParser`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub insecure_parser: bool,
    /// The largest message a WebSocket, Socket.IO, gRPC or MQTT connection takes in, in
    /// MB; 0 is each one's own limit (see `max_message`).
    #[serde(skip_serializing_if = "is_zero")]
    pub max_message_mb: u32,
    /// gRPC: replies show the fields proto3 leaves out at their defaults (0, "", false).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub include_defaults: bool,
    /// WebSocket, Socket.IO, SSE, MQTT: how many times in a row a connection that ends
    /// without Disconnect is opened again; 0 never.
    #[serde(skip_serializing_if = "is_zero")]
    pub reconnect_attempts: u32,
    /// The wait before each of those; 0 is 5 s (see `reconnect_every`).
    #[serde(skip_serializing_if = "is_zero")]
    pub reconnect_interval_ms: u32,
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
            follow_original_method: false,
            follow_auth_header: false,
            remove_referer: false,
            disable_tls12: false,
            disable_tls13: false,
            ciphers: Vec::new(),
            insecure_parser: false,
            max_message_mb: 0,
            include_defaults: false,
            reconnect_attempts: 0,
            reconnect_interval_ms: 0,
        }
    }
}

/// The wait between reconnection attempts unless a request sets its own.
pub const RECONNECT_EVERY_MS: u64 = 5000;

impl Settings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The largest message taken in, in bytes; `default` when none is set.
    pub fn max_message(&self, default: usize) -> usize {
        match self.max_message_mb {
            0 => default,
            mb => (mb as usize) << 20,
        }
    }

    pub fn reconnect_every(&self) -> std::time::Duration {
        std::time::Duration::from_millis(match self.reconnect_interval_ms {
            0 => RECONNECT_EVERY_MS,
            ms => ms.into(),
        })
    }

    /// reqwest's redirect policy turns a POST into a GET on 301/302 and drops
    /// Authorization on another host, with no way to ask otherwise; for these settings
    /// apitool follows redirects itself (`http::follow`) and the client follows none.
    pub fn follows_itself(&self) -> bool {
        self.follow_redirects && (self.follow_original_method || self.follow_auth_header)
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
    /// MQTT 5 publish properties; empty or 0 isn't sent.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    /// Where the receiver should answer.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub response_topic: String,
    /// How long the broker holds a message nobody has taken yet; 0: until it's taken.
    #[serde(skip_serializing_if = "is_zero")]
    pub expiry_secs: u32,
    /// What the broker publishes for this client when it drops without a goodbye. No
    /// topic: no will.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub will_topic: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub will_payload: String,
    pub will_qos: u8,
    pub will_retain: bool,
    /// MQTT 5 will properties, as Postman has them.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub will_content_type: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub will_expiry_secs: u32,
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
            content_type: String::new(),
            response_topic: String::new(),
            expiry_secs: 0,
            will_topic: String::new(),
            will_payload: String::new(),
            will_qos: 0,
            will_retain: false,
            will_content_type: String::new(),
            will_expiry_secs: 0,
        }
    }
}

impl Mqtt {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A copy with the Settings tab's fields (how it connects) as a new request has them;
    /// the topics and what it publishes are kept.
    pub fn with_default_connection(&self) -> Mqtt {
        Mqtt {
            topics: self.topics.clone(),
            topic: self.topic.clone(),
            qos: self.qos,
            retain: self.retain,
            user_properties: self.user_properties.clone(),
            content_type: self.content_type.clone(),
            response_topic: self.response_topic.clone(),
            expiry_secs: self.expiry_secs,
            ..Default::default()
        }
    }
}

/// Socket.IO only: where the server mounts it and which client it expects, as in
/// Postman's Socket.IO request settings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct SocketIo {
    /// Empty: /socket.io/. The namespace is still the URL's path.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// A Socket.IO 2.x server (Engine.IO 3) instead of 3.x/4.x: v3 and v4 clients speak
    /// the same protocol, v2's differs.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub v2: bool,
}

impl SocketIo {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// As socket.io-client and python-socketio spell it: one leading and one trailing
    /// slash, which servers match on.
    pub fn handshake_path(&self) -> String {
        match self.path.trim() {
            "" => "/socket.io/".into(),
            p => format!("/{}/", p.trim_matches('/')).replace("//", "/"),
        }
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
            socketio: SocketIo::default(),
            asserts: Vec::new(),
            response_vars: Vec::new(),
            messages: Vec::new(),
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

fn is_zero(n: &u32) -> bool {
    *n == 0
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
    /// Bytes typed as Base64 or hex (`decode_base64`, `decode_hex`), decoded when sent: a
    /// binary body with no file to keep beside the request.
    Base64 {
        text: String,
    },
    Hex {
        text: String,
    },
    #[serde(rename = "graphql")]
    GraphQL {
        query: String,
        variables: String,
    },
}

impl Body {
    /// The body for a switch to the GRAPHQL method (`to_graphql`) or away from it: a query
    /// becomes the JSON it goes out as, and that JSON a query again. Its text is kept as
    /// typed, `{{variables}}` and all; anything else starts an empty query.
    pub fn graphql_switched(self, to_graphql: bool) -> Body {
        use crate::graphql::{members, object};
        let (query, variables) = match (self, to_graphql) {
            (Body::GraphQL { query, variables }, false) => {
                let variables = match variables.trim() {
                    "" => "{}".to_owned(),
                    v => v.replace('\n', "\n  "),
                };
                let query = serde_json::Value::from(query);
                let text = format!("{{\n  \"query\": {query},\n  \"variables\": {variables}\n}}");
                return Body::Json { text };
            }
            (body, false) | (body @ Body::GraphQL { .. }, true) => return body,
            (Body::Json { text }, true) => {
                let members = members(&text).unwrap_or_default();
                let take = |name: &str| members.iter().find(|m| m.0 == name).map(|m| m.1.trim());
                let query = take("query").and_then(|q| serde_json::from_str(q).ok());
                let variables = match take("variables") {
                    None | Some("{}" | "null") => String::new(),
                    Some(v) => object(&crate::graphql::members(v).unwrap_or_default()),
                };
                (query.unwrap_or_default(), variables)
            }
            (_, true) => Default::default(),
        };
        Body::GraphQL { query, variables }
    }

    /// The bytes a Base64 or hex body stands for; None for any other body.
    pub fn typed_bytes(&self) -> Option<Result<Vec<u8>, String>> {
        match self {
            Body::Base64 { text } => Some(decode_base64(text)),
            Body::Hex { text } => Some(decode_hex(text)),
            _ => None,
        }
    }

    /// A type picked and nothing put in it. Sent, saved and shown as no body, so clicking
    /// a body type changes nothing until something is typed.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Json { text } | Self::Text { text } => text.is_empty(),
            Self::Base64 { text } | Self::Hex { text } => text.is_empty(),
            Self::Form { fields } => fields.is_empty(),
            Self::Multipart { parts } => parts.is_empty(),
            Self::File { path } => path.is_empty(),
            Self::GraphQL { query, variables } => query.is_empty() && variables.is_empty(),
        }
    }
}

/// Base64 as it comes from anywhere: the standard or URL-safe alphabet, padded or not,
/// broken over lines.
pub fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let clean: String = (text.chars())
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    (base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean))
        .map_err(|e| format!("Not Base64: {e}"))
}

/// Hex as it's written: `deadbeef`, `de ad be ef`, `DE:AD`, `0xde, 0xad`.
pub fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = (text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ':' | '-')))
        .flat_map(|w| {
            let w = w
                .strip_prefix("0x")
                .or_else(|| w.strip_prefix("0X"))
                .unwrap_or(w);
            w.bytes()
        })
        .collect();
    if let Some(bad) = digits.iter().find(|b| !b.is_ascii_hexdigit()) {
        return Err(format!("Not hex: {:?} isn't a hex digit", *bad as char));
    }
    if digits.len() % 2 == 1 {
        return Err("Not hex: an odd number of digits".into());
    }
    let value = |d: u8| (d as char).to_digit(16).unwrap_or(0) as u8;
    Ok((digits.chunks(2))
        .map(|p| value(p[0]) << 4 | value(p[1]))
        .collect())
}

/// Bytes as hex pairs with spaces between, as `decode_hex` reads them back.
pub fn encode_hex(bytes: &[u8]) -> String {
    let pairs: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    pairs.join(" ")
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
    /// Signed per request over the method, URL and host, and the body if asked.
    #[serde(rename = "hawk")]
    Hawk(Hawk),
    /// Akamai's: signed per request over the method, URL, chosen headers and a POST's body.
    #[serde(rename = "edgegrid")]
    EdgeGrid(EdgeGrid),
    /// Windows sign-in (IIS on an intranet): a handshake on one connection, so it costs
    /// one extra round trip.
    #[serde(rename = "ntlm")]
    Ntlm(Ntlm),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Hawk {
    pub id: String,
    pub key: String,
    /// "sha256" or "sha1"; empty is sha256.
    pub algorithm: String,
    /// Postman's "User": kept for the round trip, never signed (Hawk has no such field).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Empty: a fresh one per send. Set only to reproduce a signature.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub nonce: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ext: String,
    /// Oz's application and delegation ids.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dlg: String,
    /// Unix seconds; empty is now.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub timestamp: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub payload_hash: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct EdgeGrid {
    pub access_token: String,
    pub client_token: String,
    pub client_secret: String,
    /// Empty: a fresh one per send.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub nonce: String,
    /// `20140321T19:34:21+0000`; empty is now.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub timestamp: String,
    /// Its host is signed instead of the URL's, for a request sent through another host.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Comma separated, in the order they're signed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub headers_to_sign: String,
    /// How much of a POST body is signed, in bytes; empty is 131072.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub max_body: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Ntlm {
    /// With `domain` empty, `DOMAIN\user` and `user@domain` are split, as in Postman.
    pub username: String,
    pub password: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workstation: String,
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
            Self::Hawk(h) => {
                *h == Hawk {
                    algorithm: h.algorithm.clone(),
                    payload_hash: h.payload_hash,
                    ..Default::default()
                }
            }
            Self::EdgeGrid(e) => *e == EdgeGrid::default(),
            Self::Ntlm(n) => *n == Ntlm::default(),
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
                Body::Base64 { text } => Body::Base64 { text: r(text) },
                Body::Hex { text } => Body::Hex { text: r(text) },
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
                Auth::Hawk(h) => Auth::Hawk(Hawk {
                    id: r(&h.id),
                    key: r(&h.key),
                    algorithm: h.algorithm.clone(),
                    user: r(&h.user),
                    nonce: r(&h.nonce),
                    ext: r(&h.ext),
                    app: r(&h.app),
                    dlg: r(&h.dlg),
                    timestamp: r(&h.timestamp),
                    payload_hash: h.payload_hash,
                }),
                Auth::EdgeGrid(e) => Auth::EdgeGrid(EdgeGrid {
                    access_token: r(&e.access_token),
                    client_token: r(&e.client_token),
                    client_secret: r(&e.client_secret),
                    nonce: r(&e.nonce),
                    timestamp: r(&e.timestamp),
                    base_url: r(&e.base_url),
                    headers_to_sign: r(&e.headers_to_sign),
                    max_body: r(&e.max_body),
                }),
                Auth::Ntlm(n) => Auth::Ntlm(Ntlm {
                    username: r(&n.username),
                    password: r(&n.password),
                    domain: r(&n.domain),
                    workstation: r(&n.workstation),
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
                content_type: r(self.mqtt.content_type.trim()),
                response_topic: r(self.mqtt.response_topic.trim()),
                will_topic: r(self.mqtt.will_topic.trim()),
                will_payload: r(&self.mqtt.will_payload),
                will_content_type: r(self.mqtt.will_content_type.trim()),
                ..self.mqtt.clone()
            },
            socketio: SocketIo {
                path: r(&self.socketio.path),
                ..self.socketio.clone()
            },
            asserts: Vec::new(),
            response_vars: Vec::new(),
            messages: Vec::new(),
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
            response_vars,
            settings,
            mqtt,
            socketio,
            messages,
            examples,
            inherited: _,
        } = self;
        (method, url, proto, rpc) == (&other.method, &other.url, &other.proto, &other.rpc)
            && (description, params, path_vars)
                == (&other.description, &other.params, &other.path_vars)
            && (headers, pre_request, tests) == (&other.headers, &other.pre_request, &other.tests)
            && (asserts, settings, mqtt) == (&other.asserts, &other.settings, &other.mqtt)
            && (examples, response_vars) == (&other.examples, &other.response_vars)
            && (messages, socketio) == (&other.messages, &other.socketio)
            && (body == &other.body || body.is_empty() && other.body.is_empty())
            && (auth == &other.auth || auth.is_unset() && other.auth.is_unset())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Switching a GraphQL request to POST shows the JSON that went out, and back gives the
    /// same query and variables: nothing typed is lost either way.
    #[test]
    fn a_query_survives_switching_the_method_away_and_back() {
        let gql = Body::GraphQL {
            query: "query U($id: ID!) { user(id: $id) { name } }".into(),
            variables: "{\n  \"id\": \"{{userId}}\"\n}".into(),
        };
        let json = gql.clone().graphql_switched(false);
        let Body::Json { text } = &json else {
            panic!("{json:?}")
        };
        let sent: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            sent["query"],
            "query U($id: ID!) { user(id: $id) { name } }"
        );
        assert_eq!(sent["variables"]["id"], "{{userId}}");
        assert_eq!(json.graphql_switched(true), gql);
        // A body that isn't a query starts an empty one; one already a query stays.
        let other = Body::Text { text: "x".into() }.graphql_switched(true);
        assert!(matches!(other, Body::GraphQL { query, .. } if query.is_empty()));
        assert_eq!(gql.clone().graphql_switched(true), gql);
    }

    /// Bytes pasted from anywhere decode: a JWT's URL-safe unpadded Base64, a PEM's wrapped
    /// lines, hex from a dump or a C array. A typo is refused, not sent as something else.
    #[test]
    fn typed_bytes_read_base64_and_hex_as_people_write_them() {
        let hi = b"Hi?>".to_vec();
        for b64 in ["SGk/Pg==", "SGk_Pg", "SGk/\nPg=="] {
            assert_eq!(decode_base64(b64), Ok(hi.clone()), "{b64}");
        }
        for hex in [
            "48693f3e",
            "48 69 3F 3E",
            "48:69:3f:3e",
            "0x48, 0x69, 0x3f, 0x3e",
        ] {
            assert_eq!(decode_hex(hex), Ok(hi.clone()), "{hex}");
        }
        assert_eq!(decode_hex(&encode_hex(&hi)), Ok(hi));
        assert!(decode_base64("SGk*").is_err());
        assert!(decode_hex("486").is_err(), "odd digits");
        assert!(decode_hex("48 zz").is_err());
        let body = Body::Hex {
            text: "00 ff".into(),
        };
        assert_eq!(body.typed_bytes(), Some(Ok(vec![0, 255])));
        assert_eq!(Body::Text { text: "00".into() }.typed_bytes(), None);
    }

    /// A request saved with settings from before the redirect, TLS and parser ones existed
    /// saves byte for byte as it did (the store keeps JSON, Sync writes TOML): new fields
    /// at their defaults are left out, so untouched requests show no diff.
    #[test]
    fn settings_untouched_by_newer_fields_save_as_before() {
        let s = Settings {
            timeout_ms: 1500,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"http_version":"auto","follow_redirects":true,"max_redirects":10,"verify_tls":true,"cookies":true,"timeout_ms":1500}"#
        );
        assert_eq!(
            toml::to_string(&s).unwrap(),
            "http_version = \"auto\"\nfollow_redirects = true\nmax_redirects = 10\nverify_tls = true\ncookies = true\ntimeout_ms = 1500\n"
        );
        assert!(Settings::default().is_default());
    }

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

    /// MCP and the will go out from the resolved request, not the editor's.
    #[test]
    fn mqtt_5_properties_fill_in_variables() {
        let req = Request {
            method: "MQTT".into(),
            mqtt: Mqtt {
                content_type: "{{type}}".into(),
                response_topic: "reply/{{id}}".into(),
                will_content_type: "{{type}}".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let vars = HashMap::from([
            ("type".to_owned(), "text/csv".to_owned()),
            ("id".to_owned(), "7".to_owned()),
        ]);
        let m = req.resolved(&vars).0.mqtt;
        assert_eq!(
            [m.content_type, m.response_topic, m.will_content_type],
            ["text/csv", "reply/7", "text/csv"]
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
            response_vars: vec![KeyValue::new("token", "res.body.access_token")],
            messages: vec!["{\"op\": \"subscribe\"}".into()],
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
                content_type: "application/json".into(),
                response_topic: "reply/{{device}}".into(),
                expiry_secs: 60,
                will_topic: "status/{{device}}".into(),
                will_payload: "offline".into(),
                will_qos: 2,
                will_retain: true,
                will_content_type: "text/plain".into(),
                will_expiry_secs: 300,
            },
            socketio: SocketIo {
                path: "/{{prefix}}/rt".into(),
                v2: true,
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

    /// Every Socket.IO request saved before these settings existed must stay byte for
    /// byte the same, or opening and saving it would show up in git. Set, they're an edit,
    /// take variables, and reach the wire.
    #[test]
    fn socketio_settings_stay_out_of_the_file_until_set() {
        let plain = Request {
            method: "SOCKETIO".into(),
            url: "http://h.test:3000/chat".into(),
            ..Default::default()
        };
        assert_eq!(
            toml::to_string_pretty(&plain).unwrap(),
            "method = \"SOCKETIO\"\nurl = \"http://h.test:3000/chat\"\n"
        );
        let set = Request {
            socketio: SocketIo {
                path: "/{{app}}/rt".into(),
                v2: true,
            },
            ..plain.clone()
        };
        assert!(!set.same_as(&plain));
        let vars = HashMap::from([("app".to_owned(), "shop".to_owned())]);
        let wire = set.resolved(&vars).0.socketio;
        assert_eq!(
            (wire.handshake_path().as_str(), wire.v2),
            ("/shop/rt/", true)
        );
        let path = |p: &str| {
            SocketIo {
                path: p.into(),
                v2: false,
            }
            .handshake_path()
        };
        assert_eq!(path(" "), "/socket.io/");
        assert_eq!(path("rt"), "/rt/");
        assert_eq!(path("/"), "/");
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

    /// Hawk, EdgeGrid and NTLM picked and left blank inherit and save as inherit, choices
    /// aside, like the other types; one typed field makes them a setting that survives a save.
    #[test]
    fn hawk_edgegrid_and_ntlm_left_blank_are_not_set() {
        let plain = Request::default();
        let with = |auth: Auth| Request {
            auth,
            ..Default::default()
        };
        let blank = [
            Auth::Hawk(Hawk {
                algorithm: "sha1".into(),
                payload_hash: true,
                ..Default::default()
            }),
            Auth::EdgeGrid(EdgeGrid::default()),
            Auth::Ntlm(Ntlm::default()),
        ];
        for auth in blank {
            let picked = with(auth);
            assert!(
                picked.same_as(&plain) && plain.same_as(&picked),
                "{picked:?}"
            );
            let saved = toml::to_string_pretty(&picked).unwrap();
            assert_eq!(saved, toml::to_string_pretty(&plain).unwrap());
        }
        let typed = [
            Auth::Hawk(Hawk {
                id: "id".into(),
                ..Default::default()
            }),
            Auth::EdgeGrid(EdgeGrid {
                headers_to_sign: "X-A".into(),
                ..Default::default()
            }),
            Auth::Ntlm(Ntlm {
                domain: "CORP".into(),
                ..Default::default()
            }),
        ];
        for auth in typed {
            let req = with(auth.clone());
            assert!(!req.same_as(&plain), "{req:?}");
            let saved = toml::to_string_pretty(&req).unwrap();
            assert_eq!(toml::from_str::<Request>(&saved).unwrap().auth, auth);
        }
    }
}
