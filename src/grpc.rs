//! gRPC over plain reqwest: the `.proto` is compiled at runtime (protox) and messages
//! are converted to/from JSON with prost-reflect, so no codegen and no protoc install.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use http_body_util::BodyExt;
use prost::Message;
use prost_reflect::{
    DescriptorPool, DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, MethodDescriptor,
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::http::{self, Response, error_chain, header_list};
use crate::model::{Body, Request};
use crate::stream::Event;

/// Where a request's methods come from: its `.proto` file, then the folders its imports
/// are looked for in, a line each; or with no file set, what the server said over gRPC
/// reflection (`reflection:` and its URL). One string, so a change to any of it is a
/// change of source to whoever keeps methods by it.
pub fn source(proto: &str, url: &str, imports: &[String]) -> String {
    match proto.trim() {
        "" => format!("{REFLECTION}{}", url.trim().trim_end_matches('/')),
        p => {
            let imports = imports.iter().map(|i| i.trim()).filter(|i| !i.is_empty());
            std::iter::once(p)
                .chain(imports)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// A `source`'s file, the name errors give it.
fn file(source: &str) -> &str {
    source.lines().next().unwrap_or_default()
}

pub const REFLECTION: &str = "reflection:";

/// What servers answered over reflection, by URL, for as long as the app runs.
static REFLECTED: Mutex<Option<std::collections::HashMap<String, DescriptorPool>>> =
    Mutex::new(None);

fn reflected(url: &str) -> Option<DescriptorPool> {
    REFLECTED.lock().unwrap().as_ref()?.get(url).cloned()
}

/// Relative proto paths resolve against the working directory, which `main` sets to the
/// workspace root.
fn pool(proto: &str) -> Result<DescriptorPool, String> {
    if let Some(url) = proto.strip_prefix(REFLECTION) {
        return reflected(url).ok_or_else(|| {
            "No .proto file set: press ↻ to ask the server for its methods (gRPC reflection)"
                .to_owned()
        });
    }
    // ponytail: single-entry cache; enough for one request or a runner/load test hammering
    // the same RPC. Key includes mtime so edits to the .proto are picked up.
    static CACHE: Mutex<Option<(String, SystemTime, DescriptorPool)>> = Mutex::new(None);
    let source = proto.trim();
    let proto = file(source).trim();
    if proto.is_empty() {
        return Err("No .proto file set".into());
    }
    let path = std::path::absolute(proto).map_err(|e| format!("{proto}: {e}"))?;
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{proto}: {e}"))?;
    let mut cache = CACHE.lock().unwrap();
    if let Some((s, m, pool)) = &*cache
        && s == source
        && *m == modified
    {
        return Ok(pool.clone());
    }
    // The file's own directory and the workspace root cover both `import "b.proto"` and
    // `import "protos/b.proto"` styles; the request's import paths, the rest.
    let dir = path.parent().unwrap_or(Path::new("."));
    let imports = source.lines().skip(1).map(Path::new);
    let pool = protox::Compiler::new([dir, Path::new(".")].into_iter().chain(imports))
        .and_then(|mut c| {
            c.include_imports(true).open_file(&path)?;
            Ok(c.descriptor_pool())
        })
        .map_err(|e| format!("{proto}: {e}"))?;
    *cache = Some((source.to_owned(), modified, pool.clone()));
    Ok(pool)
}

/// A method in the picker. Streaming ones open a live call instead of a request.
#[derive(Clone, PartialEq, Debug)]
pub struct Rpc {
    /// `package.Service/Method`
    pub name: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
    /// The request message's name, without its package (for code snippets).
    pub input: String,
}

/// Every method in the file, for the method picker.
pub fn methods(proto: &str) -> Result<Vec<Rpc>, String> {
    let pool = pool(proto)?;
    Ok(pool
        .services()
        .flat_map(|s| {
            let service = s.full_name().to_owned();
            s.methods()
                .map(move |m| Rpc {
                    name: format!("{service}/{}", m.name()),
                    client_streaming: m.is_client_streaming(),
                    server_streaming: m.is_server_streaming(),
                    input: m.input().name().to_owned(),
                })
                .collect::<Vec<_>>()
        })
        .collect())
}

fn method(proto: &str, rpc: &str) -> Result<MethodDescriptor, String> {
    let pool = pool(proto)?;
    let (service, name) = rpc
        .trim()
        .rsplit_once('/')
        .ok_or_else(|| format!("Method \"{rpc}\" should look like package.Service/Method"))?;
    let service = pool
        .get_service_by_name(service)
        .ok_or_else(|| format!("Service {service} not found in {}", file(proto)))?;
    let method = service
        .methods()
        .find(|m| m.name() == name)
        .ok_or_else(|| format!("Method {name} not found in {}", service.full_name()))?;
    Ok(method)
}

/// The request message with every field set to a random value of its type, so it goes out
/// as is and shows its shape (zeros and empty strings showed the shape but tested nothing):
/// a field's name picks the kind of string (an email, an id, a URL…), an enum takes one of
/// its values, a list or map one entry, a oneof its first member.
pub fn template(proto: &str, rpc: &str) -> Result<String, String> {
    let value = sample(&method(proto, rpc)?.input(), 0);
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

/// Nested messages are filled this deep: one that holds itself would go on forever.
const SAMPLE_DEPTH: usize = 3;

fn sample(msg: &MessageDescriptor, depth: usize) -> Value {
    let fields = msg.fields().filter(|f| match f.containing_oneof() {
        // A proto3 `optional` is a oneof of its own.
        Some(o) if !o.is_synthetic() => o.fields().next().is_some_and(|x| x.number() == f.number()),
        _ => true,
    });
    let fields = fields.map(|f| {
        let value = match f.kind() {
            Kind::Message(entry) if f.is_map() => {
                let key = match sample_one(&entry.map_entry_key_field(), depth) {
                    Value::String(key) => key,
                    key => key.to_string(),
                };
                let value = sample_one(&entry.map_entry_value_field(), depth);
                Value::Object([(key, value)].into_iter().collect())
            }
            _ if f.is_list() => Value::Array(vec![sample_one(&f, depth)]),
            _ => sample_one(&f, depth),
        };
        (f.json_name().to_owned(), value)
    });
    Value::Object(fields.collect())
}

fn sample_one(f: &FieldDescriptor, depth: usize) -> Value {
    use crate::fake::below;
    use base64::Engine;
    match f.kind() {
        Kind::Double | Kind::Float => Value::from(below(100_000) as f64 / 100.0),
        Kind::Bool => Value::Bool(below(2) == 1),
        Kind::String => Value::String(text_for(f.name())),
        Kind::Bytes => {
            let bytes = text_for(f.name());
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        Kind::Enum(e) => {
            let values: Vec<_> = e.values().collect();
            let pick = values.get(below(values.len().max(1) as u64) as usize);
            pick.map_or(Value::Null, |v| Value::String(v.name().to_owned()))
        }
        Kind::Message(m) => match m.full_name() {
            // Well-known types have JSON forms of their own.
            "google.protobuf.Timestamp" => {
                Value::String(crate::fake::value("$isoTimestamp").unwrap_or_default())
            }
            "google.protobuf.Duration" => Value::String(format!("{}s", below(3600))),
            "google.protobuf.Struct" | "google.protobuf.Empty" => Value::Object(Default::default()),
            "google.protobuf.ListValue" => Value::Array(Vec::new()),
            "google.protobuf.Value" => Value::String(text_for("value")),
            "google.protobuf.FieldMask" => Value::String(String::new()),
            // Needs a type URL the pool can resolve; left out.
            "google.protobuf.Any" => Value::Null,
            // StringValue, Int32Value…: the value itself.
            name if name.starts_with("google.protobuf.") && name.ends_with("Value") => {
                (m.fields().next()).map_or(Value::Null, |inner| sample_one(&inner, depth))
            }
            _ if depth < SAMPLE_DEPTH => sample(&m, depth + 1),
            _ => Value::Null,
        },
        // Every integer kind.
        _ => Value::from(below(1000)),
    }
}

/// A string that suits a field named so: the dynamic variable for the first word of the
/// name that has one (`user_id` is an id, `home_city` a city), else a word. camelCase
/// (GraphQL's, JSON's) reads as snake_case: `firstName` is a first name, `userID` an id.
pub(crate) fn text_for(name: &str) -> String {
    const BY_WORD: &[(&str, &str)] = &[
        ("first_name", "$randomFirstName"),
        ("last_name", "$randomLastName"),
        ("email", "$randomEmail"),
        ("id", "$randomUUID"),
        ("uuid", "$randomUUID"),
        ("url", "$randomUrl"),
        ("uri", "$randomUrl"),
        ("phone", "$randomPhoneNumber"),
        ("username", "$randomUserName"),
        ("name", "$randomFullName"),
        ("city", "$randomCity"),
        ("country", "$randomCountry"),
        ("address", "$randomStreetAddress"),
        ("company", "$randomCompanyName"),
        ("ip", "$randomIP"),
        ("color", "$randomColor"),
        ("description", "$randomLoremSentence"),
        ("message", "$randomLoremSentence"),
        ("text", "$randomLoremSentence"),
    ];
    let mut snake = String::new();
    let mut lower = false;
    for c in name.chars() {
        if c.is_uppercase() && lower {
            snake.push('_');
        }
        lower = c.is_lowercase() || c.is_ascii_digit();
        snake.extend(c.to_lowercase());
    }
    let name = snake;
    let words: Vec<&str> = name.split('_').collect();
    let hit = |(word, _): &&(&str, &str)| match word.contains('_') {
        true => name.contains(word),
        false => words.contains(word),
    };
    let var = BY_WORD
        .iter()
        .find(hit)
        .map_or("$randomWord", |(_, var)| var);
    crate::fake::value(var).unwrap_or_default()
}

/// Length-prefixed message: 1 byte "compressed" flag + u32 big-endian length.
fn encode(method: &MethodDescriptor, json: Value) -> Result<Vec<u8>, String> {
    let msg = DynamicMessage::deserialize(method.input(), json)
        .map_err(|e| format!("Request message: {e}"))?;
    let payload = msg.encode_to_vec();
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// The request messages in the body: one JSON object or, for a client-streaming method, an
/// array of them (several messages, as grpcurl takes them). An empty body is `{}`, except
/// that a client stream may start with no messages.
fn body_messages(method: &MethodDescriptor, body: &Body) -> Result<Vec<Value>, String> {
    let text = match body {
        Body::Json { text } | Body::Text { text } => text.trim(),
        _ => "",
    };
    let value = match text {
        "" if method.is_client_streaming() => return Ok(Vec::new()),
        "" => return Ok(vec![Value::Object(Default::default())]),
        text => serde_json::from_str(text).map_err(|e| format!("Request message: {e}"))?,
    };
    match value {
        Value::Array(items) if method.is_client_streaming() => Ok(items),
        Value::Array(_) => Err(format!(
            "{} takes one message; only client-streaming methods take an array",
            method.name()
        )),
        one => Ok(vec![one]),
    }
}

/// Whether `json` is a valid request message, so the window can refuse a typo instead of
/// ending a live call with it.
pub fn check(proto: &str, rpc: &str, json: &str) -> Result<(), String> {
    let json = serde_json::from_str(json).map_err(|e| format!("Message: {e}"))?;
    encode(&method(proto, rpc)?, json).map(drop)
}

/// The HTTP/2 request for `req`: the method's path appended to the URL, its host the TLS
/// server name when one is set, which the client (`Clients::grpc_for`) finds where the
/// URL's host is.
fn wire(client: &reqwest::Client, req: Request) -> Result<reqwest::RequestBuilder, String> {
    let base = req.url.trim().trim_end_matches('/');
    let base = match crate::net::server_alias(&req.settings, base) {
        Some((name, _)) => {
            let mut url = reqwest::Url::parse(base).map_err(|e| format!("{base}: {e}"))?;
            (url.set_host(Some(&name))).map_err(|e| format!("TLS server name {name}: {e}"))?;
            url.to_string().trim_end_matches('/').to_owned()
        }
        None => base.to_owned(),
    };
    let url = format!("{base}/{}", req.rpc.trim());
    let deadline = req.settings.timeout_ms;
    let wire = Request {
        url,
        body: Body::None,
        ..req
    };
    let b = (http::build(client, wire)?)
        .header("content-type", "application/grpc")
        .header("te", "trailers");
    // The deadline, told to the server too, so it stops working on a call given up on.
    Ok(match deadline {
        0 => b,
        ms => b.header("grpc-timeout", format!("{ms}m")),
    })
}

/// `grpc-status` and `grpc-message` as a result.
fn status(code: Option<&str>, message: Option<&str>) -> Result<(), String> {
    match code.unwrap_or("") {
        "0" => Ok(()),
        code => Err(format!(
            "gRPC {} ({code}): {}",
            code_name(code),
            message.unwrap_or("")
        )),
    }
}

/// Takes the first response message off `buf` once it has fully arrived, as the request's
/// settings have it shown.
fn take_message(
    method: &MethodDescriptor,
    buf: &mut Vec<u8>,
    s: &crate::model::Settings,
) -> Option<Result<Value, String>> {
    let len = u32::from_be_bytes(buf.get(1..5)?.try_into().unwrap()) as usize;
    // The length prefix allows 4 GiB; buffering toward that would sink the machine.
    let max = s.max_message(crate::http::MAX_BODY);
    if len > max {
        return Some(Err(format!(
            "A {len}-byte response message is over the {} MB limit (Settings)",
            max >> 20
        )));
    }
    if buf.len() < 5 + len {
        return None;
    }
    let frame: Vec<u8> = buf.drain(..5 + len).collect();
    if frame[0] != 0 {
        return Some(Err("Compressed gRPC responses are not supported".into()));
    }
    let options = prost_reflect::SerializeOptions::new().skip_default_fields(!s.include_defaults);
    Some(
        DynamicMessage::decode(method.output(), &frame[5..])
            .map_err(|e| format!("Response message: {e}"))
            .and_then(|m| {
                (m.serialize_with_options(serde_json::value::Serializer, &options))
                    .map_err(|e| e.to_string())
            }),
    )
}

/// The request's method, asking the server over reflection first when there's no
/// `.proto` and it hasn't been asked yet.
async fn method_for(client: &reqwest::Client, req: &Request) -> Result<MethodDescriptor, String> {
    let source = source(&req.proto, &req.url, &req.settings.import_paths);
    if let Some(url) = source.strip_prefix(REFLECTION)
        && reflected(url).is_none()
    {
        reflect(client, req).await?;
    }
    method(&source, &req.rpc)
}

/// gRPC server reflection's messages (grpc/reflection/v1/reflection.proto), the fields
/// used only: listing services and fetching the files that define them.
mod reflection {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Request {
        #[prost(oneof = "Ask", tags = "4, 7")]
        pub ask: Option<Ask>,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Ask {
        #[prost(string, tag = "4")]
        FileContainingSymbol(String),
        #[prost(string, tag = "7")]
        ListServices(String),
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Response {
        #[prost(oneof = "Answer", tags = "4, 6, 7")]
        pub answer: Option<Answer>,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Answer {
        #[prost(message, tag = "4")]
        Files(Files),
        #[prost(message, tag = "6")]
        Services(Services),
        #[prost(message, tag = "7")]
        Error(Error),
    }
    /// Also how a FileDescriptorSet is laid out: its files are field 1.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Files {
        #[prost(bytes = "vec", repeated, tag = "1")]
        pub file_descriptor_proto: Vec<Vec<u8>>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Services {
        #[prost(message, repeated, tag = "1")]
        pub service: Vec<Service>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Service {
        #[prost(string, tag = "1")]
        pub name: String,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Error {
        #[prost(int32, tag = "1")]
        pub error_code: i32,
        #[prost(string, tag = "2")]
        pub error_message: String,
    }
}

/// One reflection exchange: every question in one request stream, the answers in order.
/// v1 first; servers from before it was final only speak v1alpha.
async fn ask(
    client: &reqwest::Client,
    req: &Request,
    asks: Vec<reflection::Ask>,
) -> Result<Vec<reflection::Answer>, String> {
    let mut frames = Vec::new();
    for ask in asks {
        let payload = reflection::Request { ask: Some(ask) }.encode_to_vec();
        frames.push(0);
        frames.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frames.extend_from_slice(&payload);
    }
    for version in ["v1", "v1alpha"] {
        let rpc = format!("grpc.reflection.{version}.ServerReflection/ServerReflectionInfo");
        let wire_req = Request { rpc, ..req.clone() };
        let resp = wire(client, wire_req)?
            .body(frames.clone())
            .send()
            .await
            .map_err(|e| error_chain(&e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {} (is this a gRPC endpoint?)", resp.status()));
        }
        let mut headers = header_list(resp.headers());
        let collected = reqwest::Body::from(resp)
            .collect()
            .await
            .map_err(|e| error_chain(&e))?;
        if let Some(t) = collected.trailers() {
            headers.extend(header_list(t));
        }
        let get = |k: &str| {
            headers
                .iter()
                .find(|(h, _)| h == k)
                .map(|(_, v)| v.as_str())
        };
        if get("grpc-status") == Some("12") && version == "v1" {
            continue; // UNIMPLEMENTED: try the older name
        }
        status(get("grpc-status"), get("grpc-message"))
            .map_err(|e| format!("Server reflection: {e}"))?;
        let mut buf = collected.to_bytes().to_vec();
        let mut answers = Vec::new();
        while buf.len() >= 5 {
            let len = u32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
            if buf.len() < 5 + len {
                break;
            }
            let frame: Vec<u8> = buf.drain(..5 + len).collect();
            let r = reflection::Response::decode(&frame[5..])
                .map_err(|e| format!("Server reflection: {e}"))?;
            match r.answer {
                Some(reflection::Answer::Error(e)) => {
                    return Err(format!("Server reflection: {}", e.error_message));
                }
                Some(a) => answers.push(a),
                None => {}
            }
        }
        return Ok(answers);
    }
    Err("Server reflection isn't enabled on this server".into())
}

/// Asks the server which services it has and the files describing them, and keeps them
/// as this URL's methods. Returns how many services there are.
pub async fn reflect(client: &reqwest::Client, req: &Request) -> Result<usize, String> {
    use reflection::{Answer, Ask};
    let listed = ask(client, req, vec![Ask::ListServices(String::new())]).await?;
    let services: Vec<String> = (listed.into_iter())
        .filter_map(|a| match a {
            Answer::Services(s) => Some(s.service),
            _ => None,
        })
        .flatten()
        .map(|s| s.name)
        // Its own service isn't one anybody means to call.
        .filter(|n| !n.starts_with("grpc.reflection."))
        .collect();
    let asks = services
        .iter()
        .cloned()
        .map(Ask::FileContainingSymbol)
        .collect();
    let mut files: Vec<Vec<u8>> = Vec::new();
    for answer in ask(client, req, asks).await? {
        if let Answer::Files(f) = answer {
            for file in f.file_descriptor_proto {
                // Each answer repeats the files its symbol's file imports.
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
    }
    let set = reflection::Files {
        file_descriptor_proto: files,
    };
    let pool = DescriptorPool::decode(set.encode_to_vec().as_slice())
        .map_err(|e| format!("Server reflection: {e}"))?;
    let url = req.url.trim().trim_end_matches('/').to_owned();
    (REFLECTED.lock().unwrap())
        .get_or_insert_default()
        .insert(url, pool);
    Ok(services.len())
}

/// A whole call, streams included, as one response: what the runner, CLI and MCP send.
/// Several replies come back as a JSON array.
pub async fn call(client: reqwest::Client, req: Request) -> Result<Response, String> {
    let method = method_for(&client, &req).await?;
    let mut frames = Vec::new();
    for msg in body_messages(&method, &req.body)? {
        frames.extend(encode(&method, msg)?);
    }
    let started = Instant::now();
    let settings = req.settings.clone();
    let resp = wire(&client, req)?
        .body(frames)
        .send()
        .await
        .map_err(|e| error_chain(&e))?;
    let status = resp.status();
    let version = format!("{:?}", resp.version());
    let mut headers = header_list(resp.headers());
    let collected = reqwest::Body::from(resp)
        .collect()
        .await
        .map_err(|e| error_chain(&e))?;
    // Trailers-only responses (typical for errors) carry grpc-status in the headers instead.
    if let Some(t) = collected.trailers() {
        headers.extend(header_list(t));
    }
    let elapsed = started.elapsed();
    let bytes = collected.to_bytes();
    let get = |k: &str| {
        headers
            .iter()
            .find(|(h, _)| h == k)
            .map(|(_, v)| v.as_str())
    };
    if !status.is_success() {
        return Err(format!("HTTP {status} (is this a gRPC endpoint?)"));
    }
    self::status(get("grpc-status"), get("grpc-message"))?;

    let (mut messages, mut buf) = (Vec::new(), bytes.to_vec());
    while let Some(msg) = take_message(&method, &mut buf, &settings) {
        messages.push(msg?);
    }
    if !buf.is_empty() {
        return Err("Truncated gRPC response frame".into());
    }
    let body = match messages.len() {
        1 => messages.pop().unwrap(),
        _ => Value::Array(messages),
    };
    headers.push(("content-type".into(), "application/json".into()));
    Ok(Response {
        status: status.as_u16(),
        reason: "OK".into(),
        version,
        elapsed,
        headers,
        body: body.to_string(),
        truncated: false,
        bytes: None,
        sent: Default::default(),
    })
}

/// A live call: the body's messages go first, then each `outgoing` one, and replies are
/// emitted as they arrive. Dropping the sender of `outgoing` half-closes, so the server
/// can finish its side; dropping the task cancels the call.
pub async fn stream(
    clients: &crate::net::Clients,
    mut req: Request,
    mut outgoing: mpsc::UnboundedReceiver<crate::stream::Frame>,
    emit: impl Fn(Event),
) {
    // The gRPC client speaks only HTTP/2; the token endpoint gets the regular one.
    if let Err(e) = http::with_token(&clients.http, &mut req, false).await {
        return emit(Event::Error(e));
    }
    let grpc = match clients.grpc_for(&req.settings, &req.url) {
        Ok(c) => c,
        Err(e) => return emit(Event::Error(e)),
    };
    let method = match method_for(&grpc, &req).await {
        Ok(m) => m,
        Err(e) => return emit(Event::Error(e)),
    };
    let first = match body_messages(&method, &req.body) {
        Ok(m) => m,
        Err(e) => return emit(Event::Error(e)),
    };
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let body = futures_util::stream::unfold(rx, |mut rx| async move {
        let frame = rx.recv().await?;
        Some((Ok::<_, std::convert::Infallible>(frame), rx))
    });
    let opened = format!("calling {}", req.rpc.trim());
    let settings = req.settings.clone();
    let send = match wire(&grpc, req) {
        // A stream lasts until someone ends it, not until the network timeout; a
        // deadline of its own (set by `wire`) ends it then.
        Ok(b) if settings.timeout_ms == 0 => b.timeout(Duration::MAX),
        Ok(b) => b,
        Err(e) => return emit(Event::Error(e)),
    };
    let send = send.body(reqwest::Body::wrap_stream(body)).send();
    emit(Event::Open(opened));
    let mut sender = Some(tx);
    for msg in first {
        if !relay(&method, &mut sender, Some(msg.to_string()), &emit) {
            return;
        }
    }
    if !method.is_client_streaming() {
        sender = None;
    }
    // Servers may hold their headers until the client is done (client streaming), so
    // messages keep flowing while waiting for the response.
    tokio::pin!(send);
    let resp = loop {
        tokio::select! {
            r = &mut send => break r,
            out = outgoing.recv(), if sender.is_some() => {
                if !relay(&method, &mut sender, out.map(|f| f.into_text()), &emit) {
                    return;
                }
            }
        }
    };
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status();
            return emit(Event::Error(format!(
                "HTTP {status} (is this a gRPC endpoint?)"
            )));
        }
        Err(e) => return emit(Event::Error(error_chain(&e))),
    };
    let head = resp.headers().clone();
    let (mut body, mut buf) = (reqwest::Body::from(resp), Vec::new());
    loop {
        tokio::select! {
            frame = body.frame() => {
                let trailers = match frame {
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(data) => {
                            buf.extend_from_slice(&data);
                            while let Some(msg) = take_message(&method, &mut buf, &settings) {
                                match msg {
                                    Ok(msg) => emit(Event::In(msg.to_string())),
                                    Err(e) => return emit(Event::Error(e)),
                                }
                            }
                            continue;
                        }
                        Err(frame) => frame.into_trailers().unwrap_or_default(),
                    },
                    Some(Err(e)) => return emit(Event::Error(error_chain(&e))),
                    // Trailers-only (typical for errors): the status is in the headers.
                    None => Default::default(),
                };
                let get = |k: &str| {
                    trailers.get(k).or(head.get(k)).and_then(|v| v.to_str().ok())
                };
                return emit(match status(get("grpc-status"), get("grpc-message")) {
                    Ok(()) => Event::Closed("OK".into()),
                    Err(e) => Event::Error(e),
                });
            }
            out = outgoing.recv(), if sender.is_some() => {
                if !relay(&method, &mut sender, out.map(|f| f.into_text()), &emit) {
                    return;
                }
            }
        }
    }
}

/// Queues one outgoing message, or half-closes on `None`. False when the call can't go on
/// (the error is emitted).
fn relay(
    method: &MethodDescriptor,
    sender: &mut Option<mpsc::UnboundedSender<Vec<u8>>>,
    out: Option<String>,
    emit: &impl Fn(Event),
) -> bool {
    let Some(text) = out else {
        *sender = None;
        return true;
    };
    let Some(tx) = sender else { return true };
    let frame = serde_json::from_str(&text)
        .map_err(|e| format!("Message: {e}"))
        .and_then(|json| encode(method, json));
    match frame {
        Ok(frame) => {
            let _ = tx.send(frame);
            emit(Event::Out(text));
            true
        }
        Err(e) => {
            emit(Event::Error(e));
            false
        }
    }
}

fn code_name(code: &str) -> &'static str {
    const NAMES: [&str; 17] = [
        "OK",
        "CANCELLED",
        "UNKNOWN",
        "INVALID_ARGUMENT",
        "DEADLINE_EXCEEDED",
        "NOT_FOUND",
        "ALREADY_EXISTS",
        "PERMISSION_DENIED",
        "RESOURCE_EXHAUSTED",
        "FAILED_PRECONDITION",
        "ABORTED",
        "OUT_OF_RANGE",
        "UNIMPLEMENTED",
        "INTERNAL",
        "UNAVAILABLE",
        "DATA_LOSS",
        "UNAUTHENTICATED",
    ];
    code.parse::<usize>()
        .ok()
        .and_then(|i| NAMES.get(i).copied())
        .unwrap_or("MISSING_STATUS")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const PROTO: &str = r#"
        syntax = "proto3";
        package greet.v1;
        service Greeter {
          rpc Hello (HelloRequest) returns (HelloReply);
          rpc Chat (stream HelloRequest) returns (stream HelloReply);
        }
        message HelloRequest { string name = 1; int32 times = 2; }
        message HelloReply { string message = 1; }
    "#;

    /// One file per caller: tests run in parallel, and a file being rewritten while another
    /// test compiles it would read half a proto.
    fn proto_file() -> String {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("apitool-grpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("greet-{n}.proto"));
        std::fs::write(&path, PROTO).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// (proto path, server URL) for the window's tests.
    pub fn greeter() -> (String, String) {
        let proto = proto_file();
        let url = server(pool(&proto).unwrap());
        (proto, url)
    }

    /// Minimal h2c gRPC server: answers each request message with "hi <name> x<times>" as
    /// soon as it arrives, or NOT_FOUND for name "nobody" as a trailers-only response.
    /// Returns the base URL.
    fn server(pool: DescriptorPool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (sock, _) = listener.accept().await.unwrap();
                let mut conn = h2::server::handshake(sock).await.unwrap();
                while let Some(Ok((req, mut respond))) = conn.accept().await {
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        let path = req.uri().path().to_owned();
                        // Reflection on the old name only, so a client must fall back to it.
                        if path.starts_with("/grpc.reflection.v1.") {
                            let head = ::http::Response::builder()
                                .header("content-type", "application/grpc")
                                .header("grpc-status", "12")
                                .body(())
                                .unwrap();
                            respond.send_response(head, true).unwrap();
                            return;
                        }
                        if path == "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo"
                        {
                            use reflection::{Answer, Ask};
                            let mut body = req.into_body();
                            let mut data = Vec::new();
                            while let Some(chunk) = body.data().await {
                                data.extend_from_slice(&chunk.unwrap());
                            }
                            let mut out = Vec::new();
                            while data.len() >= 5 {
                                let len =
                                    u32::from_be_bytes(data[1..5].try_into().unwrap()) as usize;
                                let frame: Vec<u8> = data.drain(..5 + len).collect();
                                let ask = reflection::Request::decode(&frame[5..]).unwrap().ask;
                                let answer = match ask.unwrap() {
                                    Ask::ListServices(_) => {
                                        Answer::Services(reflection::Services {
                                            service: [
                                                "greet.v1.Greeter",
                                                "grpc.reflection.v1alpha.ServerReflection",
                                            ]
                                            .map(|name| reflection::Service { name: name.into() })
                                            .to_vec(),
                                        })
                                    }
                                    Ask::FileContainingSymbol(symbol) => {
                                        assert_eq!(symbol, "greet.v1.Greeter");
                                        Answer::Files(reflection::Files {
                                            file_descriptor_proto: pool
                                                .files()
                                                .map(|f| f.file_descriptor_proto().encode_to_vec())
                                                .collect(),
                                        })
                                    }
                                };
                                let payload = reflection::Response {
                                    answer: Some(answer),
                                }
                                .encode_to_vec();
                                out.push(0);
                                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                                out.extend_from_slice(&payload);
                            }
                            let head = ::http::Response::builder()
                                .header("content-type", "application/grpc")
                                .body(())
                                .unwrap();
                            let mut stream = respond.send_response(head, false).unwrap();
                            stream.send_data(out.into(), false).unwrap();
                            let mut trailers = ::http::HeaderMap::new();
                            trailers.insert("grpc-status", "0".parse().unwrap());
                            stream.send_trailers(trailers).unwrap();
                            return;
                        }
                        assert!(path.starts_with("/greet.v1.Greeter/"));
                        assert_eq!(req.headers()["content-type"], "application/grpc");
                        let deadline = (req.headers().get("grpc-timeout"))
                            .map(|v| v.to_str().unwrap().to_owned());
                        let input = pool.get_message_by_name("greet.v1.HelloRequest").unwrap();
                        let output = pool.get_message_by_name("greet.v1.HelloReply").unwrap();
                        let head = |extra: Option<(&str, &str)>| {
                            let mut b = ::http::Response::builder()
                                .header("content-type", "application/grpc");
                            if let Some((k, v)) = extra {
                                b = b.header(k, v).header("grpc-message", "no such user");
                            }
                            b.body(()).unwrap()
                        };
                        let (mut body, mut data, mut stream) = (req.into_body(), Vec::new(), None);
                        while let Some(chunk) = body.data().await {
                            data.extend_from_slice(&chunk.unwrap());
                            while data.len() >= 5 {
                                let len = u32::from_be_bytes(data[1..5].try_into().unwrap());
                                let end = 5 + len as usize;
                                if data.len() < end {
                                    break;
                                }
                                let msg =
                                    DynamicMessage::decode(input.clone(), &data[5..end]).unwrap();
                                data.drain(..end);
                                let field = |n| msg.get_field_by_name(n).unwrap().into_owned();
                                let name = field("name").as_str().unwrap().to_owned();
                                let times = field("times").as_i32().unwrap();
                                if name == "nobody" {
                                    respond
                                        .send_response(head(Some(("grpc-status", "5"))), true)
                                        .unwrap();
                                    return;
                                }
                                let message = match name.as_str() {
                                    // Never answers, for a deadline to end.
                                    "never" => return std::future::pending().await,
                                    "deadline" => deadline.clone().unwrap_or_default(),
                                    // Proto3 leaves it out, as an empty string.
                                    "quiet" => String::new(),
                                    "big" => "x".repeat(17 << 20),
                                    _ => format!("hi {name} x{times}"),
                                };
                                let mut reply = DynamicMessage::new(output.clone());
                                reply.set_field_by_name(
                                    "message",
                                    prost_reflect::Value::String(message),
                                );
                                let payload = reply.encode_to_vec();
                                let mut frame = vec![0];
                                frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                                frame.extend_from_slice(&payload);
                                stream
                                    .get_or_insert_with(|| {
                                        respond.send_response(head(None), false).unwrap()
                                    })
                                    .send_data(frame.into(), false)
                                    .unwrap();
                            }
                        }
                        let mut trailers = ::http::HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        stream
                            .get_or_insert_with(|| {
                                respond.send_response(head(None), false).unwrap()
                            })
                            .send_trailers(trailers)
                            .unwrap();
                    });
                }
            });
        });
        format!("http://{addr}")
    }

    /// Fill body gives every field a random value of its type, and the message goes out
    /// as is: zeros and empty strings showed the shape but tested nothing.
    #[test]
    fn a_filled_body_has_a_value_of_each_type_and_goes_out_as_is() {
        const KINDS: &str = r#"syntax = "proto3";
package kinds;
import "google/protobuf/timestamp.proto";
enum Color { COLOR_UNSPECIFIED = 0; RED = 1; GREEN = 2; }
message Address { string city = 1; }
message Node { string label = 1; Node child = 2; }
message Order {
  string email = 1;
  string user_id = 2;
  int32 count = 3;
  int64 big = 4;
  double price = 5;
  bool paid = 6;
  bytes blob = 7;
  Color color = 8;
  repeated string tags = 9;
  map<string, int32> scores = 10;
  Address address = 11;
  oneof pick { string first = 12; int32 second = 13; }
  google.protobuf.Timestamp at = 14;
  Node tree = 15;
  optional string note = 16;
}
service Shop { rpc Place(Order) returns (Order); }
"#;
        let dir = std::env::temp_dir().join(format!("apitool-grpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kinds.proto");
        std::fs::write(&path, KINDS).unwrap();
        let proto = path.to_string_lossy().into_owned();
        let rpc = "kinds.Shop/Place";
        let text = template(&proto, rpc).unwrap();
        check(&proto, rpc, &text).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["email"].as_str().unwrap().contains('@'), "{text}");
        assert_eq!(
            v["userId"].as_str().unwrap().len(),
            36,
            "an id is a UUID: {text}"
        );
        assert!(v["count"].is_u64() && v["big"].is_u64(), "{text}");
        assert!(v["price"].is_f64() && v["paid"].is_boolean(), "{text}");
        let color = v["color"].as_str().unwrap();
        assert!(["COLOR_UNSPECIFIED", "RED", "GREEN"].contains(&color));
        assert_eq!(v["tags"].as_array().unwrap().len(), 1);
        assert_eq!(v["scores"].as_object().unwrap().len(), 1);
        assert!(!v["address"]["city"].as_str().unwrap().is_empty());
        assert!(
            v.get("first").is_some() && v.get("second").is_none(),
            "one of a oneof"
        );
        assert!(v["at"].as_str().unwrap().contains('T'), "{text}");
        assert!(v.get("note").is_some(), "a proto3 optional is filled too");
        // A message that holds itself stops a few levels down.
        assert!(v["tree"]["child"]["child"].is_object());
        assert!(v["tree"]["child"]["child"]["child"].is_null());
        assert_ne!(text, template(&proto, rpc).unwrap(), "different each time");
    }

    /// No .proto at hand: the server says what it has. Calls work from what it said, and
    /// the picker lists its methods (not reflection's own).
    #[test]
    fn without_a_proto_the_server_is_asked_over_reflection() {
        let proto = proto_file();
        let url = server(pool(&proto).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(crate::net::build_client(crate::net::Network::default()))
            .unwrap()
            .grpc;
        let req = Request {
            method: "GRPC".into(),
            url: format!("{url}/"),
            rpc: "greet.v1.Greeter/Hello".into(),
            body: Body::Json {
                text: r#"{"name": "ada", "times": 2}"#.into(),
            },
            ..Default::default()
        };
        let source = super::source("", &req.url, &[]);
        assert!(methods(&source).unwrap_err().contains("press ↻"));
        let resp = rt.block_on(call(client, req)).unwrap();
        assert_eq!(resp.body, r#"{"message":"hi ada x2"}"#);
        let names: Vec<_> = methods(&source)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["greet.v1.Greeter/Hello", "greet.v1.Greeter/Chat"]);
        assert!(
            template(&source, "greet.v1.Greeter/Hello")
                .unwrap()
                .contains("\"times\"")
        );
    }

    /// A .proto whose import lives elsewhere (a shared folder of a monorepo) compiles once
    /// that folder is an import path, and says what it couldn't find before. The paths
    /// are part of the source, so methods kept by it are looked up again when they change.
    #[test]
    fn imports_are_looked_for_in_the_import_paths() {
        let root = std::env::temp_dir().join(format!("apitool-imports-{}", std::process::id()));
        let (api, shared) = (root.join("api"), root.join("shared"));
        std::fs::create_dir_all(api.clone()).unwrap();
        std::fs::create_dir_all(shared.join("common")).unwrap();
        let common = "syntax = \"proto3\"; package common; message Name { string value = 1; }";
        std::fs::write(shared.join("common/name.proto"), common).unwrap();
        let main = r#"
            syntax = "proto3";
            package hello;
            import "common/name.proto";
            service Hi { rpc Say (common.Name) returns (common.Name); }
        "#;
        let file = api.join("hi.proto").to_string_lossy().into_owned();
        std::fs::write(&file, main).unwrap();

        let alone = source(&file, "", &[]);
        let err = methods(&alone).unwrap_err();
        assert!(err.contains("common/name.proto"), "{err}");
        let paths = [shared.to_string_lossy().into_owned(), "  ".into()];
        let with = source(&file, "", &paths);
        assert_ne!(with, alone, "another source");
        let names: Vec<_> = methods(&with)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["hello.Hi/Say"]);
        assert!(
            template(&with, "hello.Hi/Say")
                .unwrap()
                .contains("\"value\"")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A server reached by address whose certificate names another host: with that name as
    /// the TLS server name the call goes through, the name sent as SNI and :authority (as
    /// grpcurl's -authority), the certificate still checked against it. Without it the
    /// certificate doesn't fit the address and the call fails.
    #[test]
    fn a_tls_server_name_addresses_the_call_to_that_name() {
        use tokio::io::{AsyncRead, AsyncWrite};
        let own = rcgen::generate_simple_self_signed(vec!["grpc.test".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::try_from(own.signing_key.serialize_der());
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let mut config = (rustls::ServerConfig::builder_with_provider(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![own.cert.der().clone()], key.unwrap())
            .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let ca = std::env::temp_dir().join(format!("apitool-grpc-ca-{}.pem", std::process::id()));
        std::fs::write(&ca, own.cert.pem()).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        crate::net::install_provider();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        async fn answer(
            io: impl AsyncRead + AsyncWrite + Unpin,
            log: std::sync::Arc<Mutex<Vec<String>>>,
        ) {
            let Ok(mut conn) = h2::server::handshake(io).await else {
                return;
            };
            while let Some(Ok((req, mut respond))) = conn.accept().await {
                let authority = req.uri().authority().map(|a| a.to_string());
                log.lock()
                    .unwrap()
                    .push(format!("authority {}", authority.unwrap_or_default()));
                let head = ::http::Response::builder().header("content-type", "application/grpc");
                let mut stream = respond
                    .send_response(head.body(()).unwrap(), false)
                    .unwrap();
                // HelloReply { message: "hi" }
                let reply = [0, 0, 0, 0, 4, 0x0a, 2, b'h', b'i'];
                stream
                    .send_data(bytes::Bytes::copy_from_slice(&reply), false)
                    .unwrap();
                let mut trailers = ::http::HeaderMap::new();
                trailers.insert("grpc-status", "0".parse().unwrap());
                stream.send_trailers(trailers).unwrap();
            }
        }
        rt.spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let (acceptor, log) = (acceptor.clone(), log.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(s).await else {
                        return;
                    };
                    let sni = tls.get_ref().1.server_name().unwrap_or_default().to_owned();
                    log.lock().unwrap().push(format!("sni {sni}"));
                    answer(tls, log).await;
                });
            }
        });
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ca_file: ca.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let send = |name: &str| {
            let mut req = Request {
                method: "GRPC".into(),
                url: format!("https://127.0.0.1:{port}"),
                proto: proto_file(),
                rpc: "greet.v1.Greeter/Hello".into(),
                ..Default::default()
            };
            req.settings.tls_server_name = name.into();
            let client = clients.grpc_for(&req.settings, &req.url).unwrap();
            rt.block_on(call(client, req))
        };

        let refused = send("")
            .err()
            .expect("the address isn't the certificate's name");
        assert!(refused.to_lowercase().contains("certificate"), "{refused}");
        let resp = send("grpc.test").unwrap();
        assert_eq!(resp.body, r#"{"message":"hi"}"#);
        let seen = seen.lock().unwrap().clone();
        let named = [
            "sni grpc.test".to_owned(),
            format!("authority grpc.test:{port}"),
        ];
        assert!(seen.ends_with(&named), "{seen:?}");
        let _ = std::fs::remove_file(&ca);
    }

    #[test]
    fn unary_call_round_trips_json_and_surfaces_grpc_errors() {
        let proto = proto_file();
        let rpcs = methods(&proto).unwrap();
        let names: Vec<_> = rpcs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["greet.v1.Greeter/Hello", "greet.v1.Greeter/Chat"]);
        // The window opens a live call for a streaming method instead of sending.
        assert!(!rpcs[0].client_streaming && !rpcs[0].server_streaming);
        assert!(rpcs[1].client_streaming && rpcs[1].server_streaming);
        let tpl: serde_json::Value =
            serde_json::from_str(&template(&proto, "greet.v1.Greeter/Hello").unwrap()).unwrap();
        assert!(tpl["name"].as_str().is_some_and(|n| !n.is_empty()), "{tpl}");
        assert!(tpl["times"].is_u64(), "{tpl}");

        let url = server(pool(&proto).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().grpc;
        let req = |name: &str| Request {
            method: "GRPC".into(),
            url: url.clone(),
            proto: proto.clone(),
            rpc: "greet.v1.Greeter/Hello".into(),
            body: Body::Json {
                text: format!(r#"{{"name": "{name}", "times": 3}}"#),
            },
            ..Default::default()
        };
        let resp = rt.block_on(call(client.clone(), req("ada"))).unwrap();
        assert_eq!(resp.body, r#"{"message":"hi ada x3"}"#);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "grpc-status" && v == "0")
        );
        let Err(err) = rt.block_on(call(client, req("nobody"))) else {
            panic!("expected NOT_FOUND")
        };
        assert_eq!(err, "gRPC NOT_FOUND (5): no such user");
    }

    /// A call goes as its Settings tab says: the deadline ends it and reaches the server,
    /// the message size limit can be raised and lowered, and replies can show the fields
    /// proto3 leaves out.
    #[test]
    fn a_call_goes_as_its_settings_say() {
        use crate::model::Settings;
        let proto = proto_file();
        let url = server(pool(&proto).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let hello = |name: &str, settings: Settings| Request {
            method: "GRPC".into(),
            url: url.clone(),
            proto: proto.clone(),
            rpc: "greet.v1.Greeter/Hello".into(),
            body: Body::Json {
                text: format!(r#"{{"name": "{name}"}}"#),
            },
            settings,
            ..Default::default()
        };
        let send = |name: &str, settings: Settings| {
            let grpc = clients.grpc_for(&settings, &url).unwrap();
            rt.block_on(call(grpc, hello(name, settings)))
        };
        let deadline = |ms| Settings {
            timeout_ms: ms,
            ..Default::default()
        };

        assert_eq!(send("quiet", Settings::default()).unwrap().body, "{}");
        let defaults = Settings {
            include_defaults: true,
            ..Default::default()
        };
        assert_eq!(send("quiet", defaults).unwrap().body, r#"{"message":""}"#);

        let body = send("deadline", deadline(5000)).unwrap().body;
        assert_eq!(body, r#"{"message":"5000m"}"#);
        let err = send("never", deadline(300)).err().unwrap();
        assert!(err.contains("timed out"), "{err}");
        // A live call too, which otherwise waits for as long as the stream lasts.
        let events = Mutex::new(Vec::new());
        let (_tx, rx) = mpsc::unbounded_channel();
        let live = stream(&clients, hello("never", deadline(300)), rx, |e| {
            events.lock().unwrap().push(e)
        });
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), live).await })
            .expect("the deadline ended it");
        let events = events.into_inner().unwrap();
        assert!(
            matches!(events.last(), Some(Event::Error(e)) if e.contains("timed out")),
            "{events:?}"
        );

        let err = send("big", Settings::default()).err().unwrap();
        assert!(err.contains("over the 16 MB limit"), "{err}");
        let roomy = Settings {
            max_message_mb: 20,
            ..Default::default()
        };
        // {"message":"…"}
        assert_eq!(send("big", roomy).unwrap().body.len(), (17 << 20) + 14);
        let tight = Settings {
            max_message_mb: 1,
            ..Default::default()
        };
        let small = send("quiet", tight.clone()).map(|r| r.body);
        assert_eq!(small.as_deref(), Ok("{}"), "small ones still pass");
        let err = send("big", tight).err().unwrap();
        assert!(err.contains("over the 1 MB limit"), "{err}");
    }

    #[test]
    fn streaming_calls_send_and_receive_messages_as_they_go() {
        let proto = proto_file();
        let url = server(pool(&proto).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let chat = |body: &str| Request {
            method: "GRPC".into(),
            url: url.clone(),
            proto: proto.clone(),
            rpc: "greet.v1.Greeter/Chat".into(),
            body: Body::Json { text: body.into() },
            ..Default::default()
        };
        // The runner/CLI form: an array body is several messages, the replies an array.
        let resp = rt
            .block_on(call(
                clients.grpc.clone(),
                chat(r#"[{"name": "a"}, {"name": "b", "times": 2}]"#),
            ))
            .unwrap();
        assert_eq!(
            resp.body,
            r#"[{"message":"hi a x0"},{"message":"hi b x2"}]"#
        );

        // Live: each reply arrives while the call is still open, so the next message can
        // depend on it; letting go of the sender ends the client side and then the call.
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = Mutex::new(Some(tx));
        let events = Mutex::new(Vec::new());
        rt.block_on(stream(&clients, chat(r#"{"name": "ada"}"#), rx, |e| {
            match &e {
                Event::In(t) if t.contains("ada") => {
                    let next = r#"{"name": "bob", "times": 1}"#.to_owned();
                    let next = crate::stream::Frame::Text(next);
                    tx.lock().unwrap().as_ref().unwrap().send(next).unwrap();
                }
                Event::In(t) if t.contains("bob") => drop(tx.lock().unwrap().take()),
                _ => {}
            }
            events.lock().unwrap().push(e);
        }));
        assert_eq!(
            events.into_inner().unwrap(),
            [
                Event::Open("calling greet.v1.Greeter/Chat".into()),
                Event::Out(r#"{"name":"ada"}"#.into()),
                Event::In(r#"{"message":"hi ada x0"}"#.into()),
                Event::Out(r#"{"name": "bob", "times": 1}"#.into()),
                Event::In(r#"{"message":"hi bob x1"}"#.into()),
                Event::Closed("OK".into()),
            ]
        );
    }
}
