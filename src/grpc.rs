//! gRPC over plain reqwest: the `.proto` is compiled at runtime (protox) and messages
//! are converted to/from JSON with prost-reflect, so no codegen and no protoc install.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use http_body_util::BodyExt;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor, SerializeOptions};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::http::{self, Response, error_chain, header_list};
use crate::model::{Body, Request};
use crate::stream::Event;

/// Relative proto paths resolve against the working directory, which `main` sets to the
/// workspace root.
fn pool(proto: &str) -> Result<DescriptorPool, String> {
    // ponytail: single-entry cache; enough for one request or a runner/load test hammering
    // the same RPC. Key includes mtime so edits to the .proto are picked up.
    static CACHE: Mutex<Option<(PathBuf, SystemTime, DescriptorPool)>> = Mutex::new(None);
    let proto = proto.trim();
    if proto.is_empty() {
        return Err("No .proto file set".into());
    }
    let path = std::path::absolute(proto).map_err(|e| format!("{proto}: {e}"))?;
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{proto}: {e}"))?;
    let mut cache = CACHE.lock().unwrap();
    if let Some((p, m, pool)) = &*cache
        && *p == path
        && *m == modified
    {
        return Ok(pool.clone());
    }
    // The file's own directory and the workspace root cover both `import "b.proto"` and
    // `import "protos/b.proto"` styles.
    let dir = path.parent().unwrap_or(Path::new("."));
    let pool = protox::Compiler::new([dir, Path::new(".")])
        .and_then(|mut c| {
            c.include_imports(true).open_file(&path)?;
            Ok(c.descriptor_pool())
        })
        .map_err(|e| format!("{proto}: {e}"))?;
    *cache = Some((path, modified, pool.clone()));
    Ok(pool)
}

/// A method in the picker. Streaming ones open a live call instead of a request.
#[derive(Clone, PartialEq, Debug)]
pub struct Rpc {
    /// `package.Service/Method`
    pub name: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
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
        .ok_or_else(|| format!("Service {service} not found in {proto}"))?;
    let method = service
        .methods()
        .find(|m| m.name() == name)
        .ok_or_else(|| format!("Method {name} not found in {}", service.full_name()))?;
    Ok(method)
}

/// JSON skeleton of the request message with every field present, so users see the shape.
pub fn template(proto: &str, rpc: &str) -> Result<String, String> {
    let msg = DynamicMessage::new(method(proto, rpc)?.input());
    let mut out = serde_json::Serializer::pretty(Vec::new());
    msg.serialize_with_options(
        &mut out,
        &SerializeOptions::new().skip_default_fields(false),
    )
    .map_err(|e| e.to_string())?;
    Ok(String::from_utf8(out.into_inner()).unwrap_or_default())
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

/// The HTTP/2 request for `req`: the method's path appended to the URL.
fn wire(client: &reqwest::Client, req: Request) -> Result<reqwest::RequestBuilder, String> {
    let url = format!(
        "{}/{}",
        req.url.trim().trim_end_matches('/'),
        req.rpc.trim()
    );
    let wire = Request {
        url,
        body: Body::None,
        ..req
    };
    Ok(http::build(client, wire)?
        .header("content-type", "application/grpc")
        .header("te", "trailers"))
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

/// Takes the first response message off `buf` once it has fully arrived.
fn take_message(method: &MethodDescriptor, buf: &mut Vec<u8>) -> Option<Result<Value, String>> {
    let len = u32::from_be_bytes(buf.get(1..5)?.try_into().unwrap()) as usize;
    // The length prefix allows 4 GiB; buffering toward that would sink the machine.
    if len > crate::http::MAX_BODY {
        return Some(Err(format!(
            "A {len}-byte response message is over the 16 MiB limit"
        )));
    }
    if buf.len() < 5 + len {
        return None;
    }
    let frame: Vec<u8> = buf.drain(..5 + len).collect();
    if frame[0] != 0 {
        return Some(Err("Compressed gRPC responses are not supported".into()));
    }
    Some(
        DynamicMessage::decode(method.output(), &frame[5..])
            .map_err(|e| format!("Response message: {e}"))
            .and_then(|m| serde_json::to_value(&m).map_err(|e| e.to_string())),
    )
}

/// A whole call, streams included, as one response: what the runner, CLI and MCP send.
/// Several replies come back as a JSON array.
pub async fn call(client: reqwest::Client, req: Request) -> Result<Response, String> {
    let method = method(&req.proto, &req.rpc)?;
    let mut frames = Vec::new();
    for msg in body_messages(&method, &req.body)? {
        frames.extend(encode(&method, msg)?);
    }
    let started = Instant::now();
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
    while let Some(msg) = take_message(&method, &mut buf) {
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
    mut outgoing: mpsc::UnboundedReceiver<String>,
    emit: impl Fn(Event),
) {
    // The gRPC client speaks only HTTP/2; the token endpoint gets the regular one.
    if let Err(e) = http::with_token(&clients.http, &mut req, false).await {
        return emit(Event::Error(e));
    }
    let method = match method(&req.proto, &req.rpc) {
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
    let send = match wire(&clients.grpc, req) {
        // A stream lasts until someone ends it, not until the network timeout.
        Ok(b) => b
            .timeout(Duration::MAX)
            .body(reqwest::Body::wrap_stream(body))
            .send(),
        Err(e) => return emit(Event::Error(e)),
    };
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
                if !relay(&method, &mut sender, out, &emit) {
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
                            while let Some(msg) = take_message(&method, &mut buf) {
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
                if !relay(&method, &mut sender, out, &emit) {
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
                        assert!(req.uri().path().starts_with("/greet.v1.Greeter/"));
                        assert_eq!(req.headers()["content-type"], "application/grpc");
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
                                let mut reply = DynamicMessage::new(output.clone());
                                reply.set_field_by_name(
                                    "message",
                                    prost_reflect::Value::String(format!("hi {name} x{times}")),
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
        assert_eq!(tpl, serde_json::json!({"name": "", "times": 0}));

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
