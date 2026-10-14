//! Streaming requests: Server-Sent Events and WebSocket (gRPC streams live in `grpc`).
//! Both go through the shared reqwest client, so proxy/PAC/CA/client-certificate
//! settings apply as for plain HTTP.

use futures_util::{SinkExt, StreamExt};
use reqwest::header::ACCEPT;
use reqwest_websocket::{CloseCode, Message, Upgrade};
use tokio::sync::mpsc;

use crate::http::{self, error_chain};
use crate::model::{Body, KeyValue, Request};

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Open(String),
    /// About the session, not a message: what an MQTT broker subscribed to.
    Info(String),
    In(String),
    Out(String),
    Closed(String),
    Error(String),
}

/// What goes out on a WebSocket, Socket.IO or gRPC stream; only WebSocket sends bytes.
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

impl Frame {
    pub fn into_text(self) -> String {
        match self {
            Frame::Text(t) => t,
            Frame::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
        }
    }
}

/// A binary message as the log shows it: its size, then its bytes in hex.
fn binary(b: &[u8]) -> String {
    // The app keeps 64 KiB of an event's text; spelling out more is wasted work.
    let shown = crate::model::encode_hex(&b[..b.len().min(32 << 10)]);
    format!("<binary, {} bytes> {shown}", b.len())
}

/// tungstenite's own limit on a message taken in, kept unless the request sets one.
pub const WS_MAX_MESSAGE: usize = 64 << 20;

fn ws_config(s: &crate::model::Settings) -> tungstenite::protocol::WebSocketConfig {
    // A frame is never larger than the message it carries.
    let max = Some(s.max_message(WS_MAX_MESSAGE));
    let config = tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size(max).max_frame_size(max)
}

/// A GraphQL query that subscribes: it streams over WebSocket instead of sending once.
pub fn subscribes(req: &Request) -> bool {
    req.method == "GRAPHQL"
        && matches!(&req.body, Body::GraphQL { query, .. } if crate::graphql::is_subscription(query))
}

/// Connects a resolved stream request of any kind and runs it until either side ends it.
/// `text` carries what is sent on WebSocket, Socket.IO and gRPC streams, `mqtt` an MQTT
/// session's publishes; dropping a sender disconnects gracefully. A connection that ends
/// otherwise opens again as many times in a row as the request's settings say.
pub async fn connect(
    clients: &crate::net::Clients,
    net: crate::net::Network,
    req: Request,
    text: mpsc::UnboundedReceiver<Frame>,
    mqtt: mpsc::UnboundedReceiver<crate::mqtt::Command>,
    emit: impl Fn(Event),
) {
    // Sizes only: what a message says is the user's. The app logs opening, closing and
    // errors itself.
    let emit = |e: Event| {
        match &e {
            Event::In(m) => log::trace!("received {} B", m.len()),
            Event::Out(m) => log::trace!("sent {} B", m.len()),
            Event::Info(m) => log::debug!("{}", crate::logfile::redact(m)),
            Event::Open(_) | Event::Closed(_) | Event::Error(_) => {}
        }
        emit(e)
    };
    log::debug!(
        "{} stream to {}",
        req.method,
        crate::logfile::redact(&req.url)
    );
    match req.method.as_str() {
        // MQTT isn't HTTP, but it takes the client's proxy choice (maybe from PAC).
        "MQTT" => crate::mqtt::session(req, net, clients.route.clone(), mqtt, emit).await,
        "GRPC" => crate::grpc::stream(clients, req, text, emit).await,
        method => {
            let (ws, sio) = (method.eq_ignore_ascii_case("WS"), method == "SOCKETIO");
            let upgrades = ws || sio || subscribes(&req);
            let http = match upgrades {
                true => clients.websocket_for(&req.settings, &req.url),
                false => clients.for_settings(&req.settings, &req.url),
            };
            let http = match http {
                Ok(http) => http,
                Err(e) => {
                    return emit(Event::Error(crate::i18n::tf("Network settings: {}", &[&e])));
                }
            };
            let (tries, every) = (
                req.settings.reconnect_attempts,
                req.settings.reconnect_every(),
            );
            let (mut text, mut tried, mut last_id) = (text, 0, String::new());
            loop {
                // How it ended waits until it's known whether the connection opens again:
                // the window takes Closed and Error as the end of the session.
                let end = std::sync::Mutex::new(None);
                let opened = std::sync::atomic::AtomicBool::new(false);
                let relay = |e: Event| match e {
                    Event::Closed(_) | Event::Error(_) => *end.lock().unwrap() = Some(e),
                    e => {
                        if matches!(e, Event::Open(_)) {
                            opened.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        emit(e)
                    }
                };
                let (http, mut req) = (http.clone(), req.clone());
                if ws {
                    websocket(http, req, &mut text, relay).await
                } else if sio {
                    socketio(http, req, &mut text, relay).await
                } else if upgrades {
                    graphql(http, req, relay).await
                } else {
                    // Picks up where the stream left off, as EventSource does.
                    if !last_id.is_empty() {
                        req.headers.push(KeyValue::new("Last-Event-ID", &last_id));
                    }
                    sse(http, req, &mut last_id, relay).await
                }
                let end = end.into_inner().unwrap();
                let end = end.unwrap_or(Event::Closed("connection closed".into()));
                // Counted in a row: a connection that opened starts the count over.
                if opened.into_inner() {
                    tried = 0;
                }
                // Disconnect lets go of the sender; SSE's ends the task instead.
                let asked = (ws || sio) && text.is_closed();
                if asked || tried >= tries {
                    return emit(end);
                }
                tried += 1;
                let why = match &end {
                    Event::Closed(why) | Event::Error(why) => why.as_str(),
                    _ => "",
                };
                emit(Event::Info(reconnecting(why, tried, tries, every)));
                tokio::time::sleep(every).await;
            }
        }
    }
}

/// What the log says between a connection that ended and the next attempt.
pub fn reconnecting(why: &str, tried: u32, tries: u32, every: std::time::Duration) -> String {
    let secs = every.as_secs_f32();
    format!("{why}; reconnecting in {secs} s ({tried} of {tries})")
}

/// Reads an event stream until the server ends it; `emit` receives every event, and
/// `last_id` the last event ID the server set.
pub async fn sse(
    client: reqwest::Client,
    mut req: Request,
    last_id: &mut String,
    emit: impl Fn(Event),
) {
    if let Err(e) = http::with_token(&client, &mut req, false).await {
        return emit(Event::Error(e));
    }
    let wants_accept = !req
        .headers
        .iter()
        .any(|h| h.key.eq_ignore_ascii_case("accept"));
    // Some servers hold the head back until their first event, so there's no limit on
    // the wait unless the request sets one.
    let wait = match req.settings.timeout_ms {
        0 => std::time::Duration::MAX,
        ms => std::time::Duration::from_millis(ms),
    };
    let mut b = match http::build(&client, req) {
        // The client's timeout covers the whole body; a stream lasts until someone ends it.
        Ok(b) => b.timeout(std::time::Duration::MAX),
        Err(e) => return emit(Event::Error(e)),
    };
    if wants_accept {
        b = b.header(ACCEPT, "text/event-stream");
    }
    let resp = match tokio::time::timeout(wait, b.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return emit(Event::Error(error_chain(&e))),
        Err(_) => {
            let ms = wait.as_millis();
            return emit(Event::Error(format!("no answer within {ms} ms")));
        }
    };
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return emit(Event::Error(format!("{status}\n{body}")));
    }
    emit(Event::Open(format!("{status} {content_type}")));
    let mut parser = SseParser::default();
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                (parser.feed(&bytes).into_iter()).for_each(|e| emit(Event::In(e)));
                if !parser.id.is_empty() {
                    *last_id = std::mem::take(&mut parser.id);
                }
            }
            Err(e) => return emit(Event::Error(error_chain(&e))),
        }
    }
    emit(Event::Closed("stream ended by server".into()));
}

/// Connects, then relays `outgoing` messages until either side closes. Dropping the
/// sender of `outgoing` closes the socket gracefully.
pub async fn websocket(
    client: reqwest::Client,
    mut req: Request,
    outgoing: &mut mpsc::UnboundedReceiver<Frame>,
    emit: impl Fn(Event),
) {
    if let Err(e) = http::with_token(&client, &mut req, false).await {
        return emit(Event::Error(e));
    }
    let config = ws_config(&req.settings);
    let b = match http::build(&client, req) {
        Ok(b) => b,
        Err(e) => return emit(Event::Error(e)),
    };
    let socket = match b.upgrade().web_socket_config(config).send().await {
        Ok(resp) => resp.into_websocket().await,
        Err(e) => Err(e),
    };
    let socket = match socket {
        Ok(s) => s,
        Err(e) => return emit(Event::Error(error_chain(&e))),
    };
    emit(Event::Open("connected".into()));
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            out = outgoing.recv() => match out {
                Some(frame) => {
                    let (message, shown) = match frame {
                        Frame::Text(t) => (Message::Text(t.clone()), t),
                        Frame::Binary(b) => (Message::Binary(b.clone().into()), binary(&b)),
                    };
                    if let Err(e) = sink.send(message).await {
                        return emit(Event::Error(error_chain(&e)));
                    }
                    emit(Event::Out(shown));
                }
                None => {
                    hang_up(&mut sink, &mut stream).await;
                    return emit(Event::Closed("disconnected".into()));
                }
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(t))) => emit(Event::In(t)),
                Some(Ok(Message::Binary(b))) => emit(Event::In(binary(&b))),
                Some(Ok(Message::Close { code, reason })) => {
                    return emit(Event::Closed(format!("closed by server ({code:?}) {reason}")));
                }
                Some(Ok(_)) => {} // ping/pong are answered by the protocol layer
                Some(Err(e)) => return emit(Event::Error(error_chain(&e))),
                None => return emit(Event::Closed("connection closed".into())),
            },
        }
    }
}

/// Sends a Close and reads on until the server's, as the closing handshake has it. A socket
/// dropped with the server's last messages unread in it is reset, and on Windows a reset
/// makes the server lose what it hadn't read yet: the Close, or Socket.IO's disconnect.
/// Half a second at most, for a server that never answers.
async fn hang_up<K, S, E>(sink: &mut K, stream: &mut S)
where
    K: futures_util::Sink<Message> + Unpin,
    S: futures_util::Stream<Item = Result<Message, E>> + Unpin,
{
    let close = Message::Close {
        code: CloseCode::Normal,
        reason: String::new(),
    };
    if sink.send(close).await.is_err() {
        return;
    }
    let answered = async {
        while let Some(Ok(m)) = stream.next().await {
            if matches!(m, Message::Close { .. }) {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), answered).await;
}

/// A GraphQL subscription: graphql-transport-ws (the graphql-ws library, what servers
/// speak today), or subscriptions-transport-ws when the server only picks that one. Each
/// result arrives as a message; the server's `complete` ends the session.
pub async fn graphql(client: reqwest::Client, req: Request, emit: impl Fn(Event)) {
    use serde_json::{Value, json};
    let Body::GraphQL { query, variables } = &req.body else {
        return emit(Event::Error("not a GraphQL request".into()));
    };
    let variables: Value = match variables.trim() {
        "" => Value::Null,
        v => match serde_json::from_str(v) {
            Ok(v) => v,
            Err(e) => return emit(Event::Error(format!("Variables aren't JSON: {e}"))),
        },
    };
    let payload = json!({ "query": query, "variables": variables });
    // The handshake is a GET carrying the request's headers and auth, not its body.
    let mut upgrade = Request {
        method: "WS".into(),
        body: Body::None,
        ..req.clone()
    };
    if let Err(e) = http::with_token(&client, &mut upgrade, false).await {
        return emit(Event::Error(e));
    }
    let b = match http::build(&client, upgrade) {
        Ok(b) => b,
        Err(e) => return emit(Event::Error(e)),
    };
    let sent = b
        .upgrade()
        .protocols(["graphql-transport-ws", "graphql-ws"])
        .web_socket_config(ws_config(&req.settings))
        .send()
        .await;
    let socket = match sent {
        Ok(resp) => resp.into_websocket().await,
        Err(e) => Err(e),
    };
    let socket = match socket {
        Ok(s) => s,
        Err(e) => return emit(Event::Error(error_chain(&e))),
    };
    let legacy = socket.protocol() == Some("graphql-ws");
    let protocol = if legacy {
        "graphql-ws"
    } else {
        "graphql-transport-ws"
    };
    emit(Event::Open(format!("connected ({protocol})")));
    let (mut sink, mut stream) = socket.split();
    macro_rules! send {
        ($v:expr) => {{
            let text = $v.to_string();
            if let Err(e) = sink.send(Message::Text(text.clone())).await {
                return emit(Event::Error(error_chain(&e)));
            }
            emit(Event::Out(text));
        }};
    }
    send!(json!({ "type": "connection_init", "payload": {} }));
    while let Some(incoming) = stream.next().await {
        let text = match incoming {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close { code, reason }) => {
                return emit(Event::Closed(format!(
                    "closed by server ({code:?}) {reason}"
                )));
            }
            Ok(_) => continue,
            Err(e) => return emit(Event::Error(error_chain(&e))),
        };
        let msg: Value = serde_json::from_str(&text).unwrap_or_default();
        let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
        match msg["type"].as_str().unwrap_or_default() {
            "connection_ack" => {
                let kind = if legacy { "start" } else { "subscribe" };
                send!(json!({ "id": "1", "type": kind, "payload": payload }));
            }
            "next" | "data" => emit(Event::In(pretty(&msg["payload"]))),
            "ping" => send!(json!({ "type": "pong" })),
            "pong" | "ka" => {}
            "error" | "connection_error" => {
                return emit(Event::Error(pretty(&msg["payload"])));
            }
            "complete" => {
                hang_up(&mut sink, &mut stream).await;
                return emit(Event::Closed("subscription complete".into()));
            }
            _ => emit(Event::In(text)),
        }
    }
    emit(Event::Closed("connection closed".into()))
}

/// Socket.IO 3/4 (Engine.IO 4), or 2 (Engine.IO 3) when `socketio.v2`, over its WebSocket
/// transport at `socketio.path`. The URL's path is the namespace, a JSON body the CONNECT
/// auth payload (v3+ only). Each outgoing message is an event: `name {"json": "arg"}`, or
/// the wire's own `["name", arg, …]` array.
// ponytail: long-polling isn't spoken, only the websocket transport; add it when a server
// turns websocket-only clients away.
pub async fn socketio(
    client: reqwest::Client,
    req: Request,
    outgoing: &mut mpsc::UnboundedReceiver<Frame>,
    emit: impl Fn(Event),
) {
    let mut url = match reqwest::Url::parse(&http::wire_url(&req.url)) {
        Ok(u) => u,
        Err(e) => return emit(Event::Error(format!("URL: {e}"))),
    };
    let ns = match url.path().trim_end_matches('/') {
        "" => String::new(),
        p => p.to_owned(),
    };
    let v2 = req.socketio.v2;
    url.set_path(&req.socketio.handshake_path());
    url.query_pairs_mut()
        .append_pair("EIO", if v2 { "3" } else { "4" })
        .append_pair("transport", "websocket");
    // Socket.IO 2's CONNECT has no payload; its servers read the handshake's query.
    let auth = match &req.body {
        Body::Json { text } if !v2 && !text.trim().is_empty() => text.trim().to_owned(),
        _ => String::new(),
    };
    let mut upgrade = Request {
        method: "WS".into(),
        url: url.to_string(),
        body: Body::None,
        ..req
    };
    if let Err(e) = http::with_token(&client, &mut upgrade, false).await {
        return emit(Event::Error(e));
    }
    let config = ws_config(&upgrade.settings);
    let b = match http::build(&client, upgrade) {
        Ok(b) => b,
        Err(e) => return emit(Event::Error(e)),
    };
    let socket = match b.upgrade().web_socket_config(config).send().await {
        Ok(resp) => resp.into_websocket().await,
        Err(e) => Err(e),
    };
    let socket = match socket {
        Ok(s) => s,
        Err(e) => return emit(Event::Error(error_chain(&e))),
    };
    let (mut sink, mut stream) = socket.split();
    // A namespace other than "/" is named in every packet, followed by a comma.
    let prefix = match ns.is_empty() {
        true => String::new(),
        false => format!("{ns},"),
    };
    // Engine.IO 3 has the client ping every pingInterval, or the server hangs up; from 4
    // on the server pings instead.
    let mut pings: Option<tokio::time::Interval> = None;
    // A server ignores events sent before it has confirmed the namespace, and MCP queues
    // its messages before the connection is up: hold them until the CONNECT comes back.
    let mut joined = false;
    let mut pending = Vec::new();
    loop {
        tokio::select! {
            _ = async { pings.as_mut().unwrap().tick().await }, if pings.is_some() => {
                if let Err(e) = sink.send(Message::Text("2".into())).await {
                    return emit(Event::Error(error_chain(&e)));
                }
            },
            out = outgoing.recv() => match out {
                Some(frame) => pending.push(frame.into_text()),
                None => {
                    let _ = sink.send(Message::Text(format!("41{prefix}"))).await;
                    hang_up(&mut sink, &mut stream).await;
                    return emit(Event::Closed("disconnected".into()));
                }
            },
            incoming = stream.next() => {
                let text = match incoming {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close { code, reason })) => {
                        return emit(Event::Closed(format!("closed by server ({code:?}) {reason}")));
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return emit(Event::Error(error_chain(&e))),
                    None => return emit(Event::Closed("connection closed".into())),
                };
                // Engine.IO: 0 open, 2 ping, 4 a Socket.IO packet.
                match text.as_bytes().first() {
                    Some(b'0') => {
                        if v2 {
                            let open: serde_json::Value =
                                serde_json::from_str(&text[1..]).unwrap_or_default();
                            let every = open["pingInterval"].as_u64().unwrap_or(25_000);
                            let every = std::time::Duration::from_millis(every.max(1));
                            let first = tokio::time::Instant::now() + every;
                            pings = Some(tokio::time::interval_at(first, every));
                        }
                        // Socket.IO 2 joins "/" unasked, and the server says so with a 40.
                        if !(v2 && ns.is_empty()) {
                            let connect = format!("40{prefix}{auth}");
                            if let Err(e) = sink.send(Message::Text(connect)).await {
                                return emit(Event::Error(error_chain(&e)));
                            }
                        }
                    }
                    Some(b'2') => {
                        if let Err(e) = sink.send(Message::Text("3".into())).await {
                            return emit(Event::Error(error_chain(&e)));
                        }
                    }
                    // That unasked "/" isn't the namespace asked for.
                    Some(b'4') if v2 && !ns.is_empty() && text == "40" => {}
                    Some(b'4') => match socketio_packet(&text[1..]) {
                        Packet::Connected => {
                            joined = true;
                            let at = if ns.is_empty() { "/" } else { &ns };
                            emit(Event::Open(format!("connected to {at}")));
                        }
                        Packet::Event(e) => emit(Event::In(e)),
                        Packet::Refused(why) => return emit(Event::Error(format!("connection refused: {why}"))),
                        Packet::Disconnected => return emit(Event::Closed("disconnected by server".into())),
                    },
                    _ => {}
                }
            }
        }
        if !joined {
            continue;
        }
        for text in pending.drain(..) {
            let args = match event_args(&text) {
                Ok(a) => a,
                Err(e) => {
                    emit(Event::Error(e));
                    continue;
                }
            };
            if let Err(e) = sink.send(Message::Text(format!("42{prefix}{args}"))).await {
                return emit(Event::Error(error_chain(&e)));
            }
            emit(Event::Out(args));
        }
    }
}

enum Packet {
    Connected,
    Event(String),
    Refused(String),
    Disconnected,
}

/// A Socket.IO packet past its Engine.IO `4`: type, `/namespace,`, ack id, JSON.
fn socketio_packet(p: &str) -> Packet {
    let (kind, rest) = p.split_at(p.len().min(1));
    let rest = match rest.strip_prefix('/') {
        Some(r) => r.split_once(',').map_or("", |(_, data)| data),
        None => rest,
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (id, data) = rest.split_at(digits);
    match kind {
        "0" => Packet::Connected,
        "1" => Packet::Disconnected,
        "4" => Packet::Refused(data.to_owned()),
        // `["name", args…]`: shown as the name, then the arguments.
        "2" => match serde_json::from_str::<Vec<serde_json::Value>>(data) {
            Ok(v) if !v.is_empty() => {
                let name = v[0]
                    .as_str()
                    .map_or_else(|| v[0].to_string(), str::to_owned);
                let args: Vec<String> = v[1..].iter().map(|a| a.to_string()).collect();
                Packet::Event(format!("{name} {}", args.join(" ")).trim_end().to_owned())
            }
            _ => Packet::Event(data.to_owned()),
        },
        "3" => Packet::Event(format!("ack {id} {data}")),
        _ => Packet::Event(p.to_owned()),
    }
}

/// What the compose box holds, as the JSON array an event packet carries.
fn event_args(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.starts_with('[') {
        let v: Vec<serde_json::Value> =
            serde_json::from_str(text).map_err(|e| format!("not a JSON array: {e}"))?;
        if !v.first().is_some_and(serde_json::Value::is_string) {
            return Err("the array starts with the event name".into());
        }
        return Ok(serde_json::to_string(&v).unwrap_or_default());
    }
    let (name, arg) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if name.is_empty() {
        return Err("type an event name, then its argument: chat {\"text\": \"hi\"}".into());
    }
    let mut args = vec![serde_json::Value::String(name.to_owned())];
    let arg = arg.trim();
    if !arg.is_empty() {
        // Text that isn't JSON goes as a string, as `socket.emit("chat", "hi")` would.
        args.push(serde_json::from_str(arg).unwrap_or_else(|_| arg.into()));
    }
    Ok(serde_json::to_string(&args).unwrap_or_default())
}

/// Incremental `text/event-stream` parser. Works on bytes so multi-byte characters
/// split across network chunks decode correctly.
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
    /// Dropping the rest of a line that went past `MAX_SSE`.
    skipping: bool,
    event: String,
    data: Vec<String>,
    data_len: usize,
    /// The last `id:` since it was taken: what a reconnection sends as Last-Event-ID.
    id: String,
}

/// A line, or one event's data, past this keeps its start: a server that never sends a
/// newline would otherwise fill RAM.
const MAX_SSE: usize = 1 << 20;

impl SseParser {
    /// Returns one display string per dispatched event.
    fn feed(&mut self, mut chunk: &[u8]) -> Vec<String> {
        if self.skipping {
            let Some(nl) = chunk.iter().position(|&b| b == b'\n') else {
                return Vec::new();
            };
            self.skipping = false;
            self.buf.push(b'\n');
            chunk = &chunk[nl + 1..];
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = self.data.join("\n");
                    out.push(match self.event.as_str() {
                        "" | "message" => data,
                        event => format!("[{event}] {data}"),
                    });
                }
                self.data.clear();
                self.data_len = 0;
                self.event.clear();
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / keep-alive
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" if self.data_len < MAX_SSE => {
                    self.data_len += value.len();
                    self.data.push(value.to_owned());
                }
                "event" => self.event = value.to_owned(),
                // The spec ignores an ID with a NUL in it.
                "id" if !value.contains('\0') => self.id = value.to_owned(),
                _ => {} // retry: the request's settings say when to reconnect
            }
        }
        if self.buf.len() > MAX_SSE {
            self.buf.truncate(MAX_SSE);
            self.skipping = true;
        }
        out
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_chunking_multiline_and_named_events() {
        let mut p = SseParser::default();
        let stream = "data: first\n\n: keep-alive\n\nevent: update\ndata: line1\r\ndata: line2\r\n\r\ndata: 中文\n\n";
        let bytes = stream.as_bytes();
        // Split inside the multi-byte character to prove byte-level buffering.
        let cut = stream.find("中").unwrap() + 1;
        let mut events = p.feed(&bytes[..10]);
        events.extend(p.feed(&bytes[10..cut]));
        events.extend(p.feed(&bytes[cut..]));
        assert_eq!(events, ["first", "[update] line1\nline2", "中文"]);
    }

    #[test]
    fn sse_reads_events_from_a_live_stream() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let n = s.read(&mut buf).unwrap();
            assert!(
                String::from_utf8_lossy(&buf[..n])
                    .to_lowercase()
                    .contains("accept: text/event-stream")
            );
            s.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
            for i in 0..3 {
                if i == 2 {
                    // Past the 1 s network timeout: a stream must outlive it.
                    std::thread::sleep(std::time::Duration::from_millis(1200));
                }
                s.write_all(format!("data: tick {i}\n\n").as_bytes())
                    .unwrap();
                s.flush().unwrap();
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            timeout_secs: 1,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let req = Request {
            method: "SSE".into(),
            url: format!("{addr}/events"),
            ..Default::default()
        };
        let events = std::sync::Mutex::new(Vec::new());
        rt.block_on(sse(client, req, &mut String::new(), |e| {
            events.lock().unwrap().push(e)
        }));
        let events = events.into_inner().unwrap();
        assert!(
            matches!(&events[0], Event::Open(s) if s.contains("text/event-stream")),
            "{events:?}"
        );
        assert_eq!(
            events[1..4],
            [
                Event::In("tick 0".into()),
                Event::In("tick 1".into()),
                Event::In("tick 2".into())
            ]
        );
        assert!(
            matches!(events.last(), Some(Event::Closed(_))),
            "{events:?}"
        );
    }

    /// Hanging up waits for the server's Close: a socket dropped with the server's last
    /// messages unread in it is reset, and on Windows a reset makes the server lose what
    /// it hadn't read yet, the Close (or Socket.IO's disconnect) included.
    #[test]
    fn hanging_up_finishes_the_closing_handshake() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let mut got = Vec::new();
            let end = loop {
                match ws.read() {
                    Ok(m) if m.to_text().is_ok_and(|t| t == "bye") => {
                        got.push("bye".to_owned());
                        // Still talking as the client hangs up, then slow to read on.
                        for _ in 0..200 {
                            let _ = ws.send(tungstenite::Message::text("x".repeat(1024)));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    Ok(m) if m.is_close() => got.push("close".to_owned()),
                    Ok(_) => {}
                    Err(e) => break e,
                }
            };
            (got, matches!(end, tungstenite::Error::ConnectionClosed))
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let req = Request {
            method: "WS".into(),
            url: format!("ws://{addr}/"),
            ..Default::default()
        };
        // Queued and hung up before connecting, as MCP's stream_request does.
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Frame::Text("bye".into())).unwrap();
        drop(tx);
        rt.block_on(websocket(client, req, &mut rx, |_| {}));
        drop(rt);
        let (got, clean) = server.join().unwrap();
        assert_eq!(got, ["bye", "close"]);
        assert!(clean, "the closing handshake finished");
    }

    #[test]
    fn websocket_echoes_and_closes_when_sender_drops() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            ws.send(tungstenite::Message::text("hello")).unwrap();
            while let Ok(msg) = ws.read() {
                if msg.is_text() {
                    ws.send(tungstenite::Message::text(format!(
                        "echo {}",
                        msg.to_text().unwrap()
                    )))
                    .unwrap();
                }
                if msg.is_binary() {
                    ws.send(msg).unwrap();
                }
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let req = Request {
            method: "WS".into(),
            url: format!("ws://{addr}/socket"),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let tx = std::sync::Mutex::new(Some(tx));
        let send = |f: Frame| tx.lock().unwrap().as_ref().unwrap().send(f).unwrap();
        rt.block_on(websocket(client, req, &mut rx, |e| {
            // Reply once the server's echo arrives, then hang up by dropping the sender.
            match &e {
                Event::In(t) if t == "hello" => send(Frame::Text("ping".into())),
                // A binary message goes out as one, bytes intact: the server echoes it back.
                Event::In(t) if t == "echo ping" => send(Frame::Binary(vec![0, 0xbe, 0xef])),
                // Whatever comes back to it: one sent as text would leave this waiting forever.
                Event::In(_) => drop(tx.lock().unwrap().take()),
                _ => {}
            }
            events.lock().unwrap().push(e);
        }));
        let events = events.into_inner().unwrap();
        assert_eq!(
            events,
            [
                Event::Open("connected".into()),
                Event::In("hello".into()),
                Event::Out("ping".into()),
                Event::In("echo ping".into()),
                Event::Out("<binary, 3 bytes> 00 be ef".into()),
                Event::In("<binary, 3 bytes> 00 be ef".into()),
                Event::Closed("disconnected".into()),
            ]
        );
    }

    /// Servers behind Cloudflare (wss://sports-api.polymarket.com/ws) offer h2 by ALPN: a
    /// client offering it too gets HTTP/2, and the upgrade fails with "the server responded
    /// with a different http version". The WebSocket client offers only HTTP/1.1.
    #[test]
    fn a_websocket_over_tls_stays_on_http_1_1_when_the_server_offers_h2() {
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::try_from(own.signing_key.serialize_der());
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![own.cert.der().clone()], key.unwrap())
            .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let config = std::sync::Arc::new(config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                // A client that took h2 sends its preface, not a handshake: this fails.
                if let Ok(mut ws) = tungstenite::accept(rustls::StreamOwned::new(conn, s)) {
                    ws.send(tungstenite::Message::text("hello")).unwrap();
                    let _ = ws.read();
                }
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            insecure: true,
            ..Default::default()
        };
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let req = Request {
            method: "WS".into(),
            url: format!("wss://{addr}/ws"),
            ..Default::default()
        };
        let first_event = |client: reqwest::Client| {
            let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
            let tx = std::sync::Mutex::new(Some(tx));
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(websocket(client, req.clone(), &mut rx, |e| {
                drop(tx.lock().unwrap().take()); // hang up after the first event
                events.lock().unwrap().push(e);
            }));
            events.into_inner().unwrap().remove(0)
        };
        // The everyday client does take h2 here (this server then hangs up on its preface;
        // a real one answers in HTTP/2), so the test does reach the failure.
        let plain = first_event(clients.http.clone());
        assert!(matches!(&plain, Event::Error(_)), "{plain:?}");
        let ws = clients.websocket_for(&req.settings, &req.url).unwrap();
        assert_eq!(first_event(ws), Event::Open("connected".into()));
    }

    /// A connection the server drops opens again as the settings say, while the window
    /// sees no end until nothing more is tried; Disconnect ends it for good. SSE picks up
    /// after the last event ID it was given, as EventSource does.
    #[test]
    fn a_dropped_stream_reconnects_and_sse_resumes_where_it_left_off() {
        use std::io::{Read, Write};
        // The first connection says "one" and drops without a Close; the second, "two".
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (n, s) in listener.incoming().take(2).enumerate() {
                let mut ws = tungstenite::accept(s.unwrap()).unwrap();
                ws.send(tungstenite::Message::text(["one", "two"][n]))
                    .unwrap();
                while n == 1 && ws.read().is_ok() {}
            }
        });
        // Each connection one event, then the stream ends; returns the Last-Event-ID lines.
        let sse_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sse_addr = sse_listener.local_addr().unwrap();
        let resumed = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for (n, s) in sse_listener.incoming().take(2).enumerate() {
                let mut s = s.unwrap();
                let mut buf = [0; 4096];
                let n_read = s.read(&mut buf).unwrap();
                let head = String::from_utf8_lossy(&buf[..n_read]).to_lowercase();
                asked.push(
                    head.lines()
                        .find(|l| l.starts_with("last-event-id"))
                        .map(str::to_owned),
                );
                let event: &[u8] = [&b"id: 7\ndata: a\n\n"[..], b"data: b\n\n"][n];
                let ok = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                s.write_all(&[&ok[..], event].concat()).unwrap();
            }
            asked
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let clients = rt.block_on(crate::net::build_client(net.clone())).unwrap();
        let settings = crate::model::Settings {
            reconnect_attempts: 1,
            reconnect_interval_ms: 100,
            ..Default::default()
        };
        // Hangs up on "two".
        let events = |req: Request| {
            let (tx, rx) = mpsc::unbounded_channel::<Frame>();
            let (_mqtt, mqtt_rx) = mpsc::unbounded_channel();
            let tx = std::sync::Mutex::new(Some(tx));
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(connect(&clients, net.clone(), req, rx, mqtt_rx, |e| {
                if matches!(&e, Event::In(t) if t == "two") {
                    drop(tx.lock().unwrap().take());
                }
                events.lock().unwrap().push(e);
            }));
            events.into_inner().unwrap()
        };
        let again = |t: &str| t.ends_with("; reconnecting in 0.1 s (1 of 1)");

        let ws = Request {
            method: "WS".into(),
            url: format!("ws://{addr}/"),
            settings: settings.clone(),
            ..Default::default()
        };
        let got = events(ws);
        assert!(
            matches!(&got[..], [
                Event::Open(_),
                Event::In(one),
                Event::Info(info),
                Event::Open(_),
                Event::In(two),
                Event::Closed(end),
            ] if one == "one" && again(info) && two == "two" && end == "disconnected"),
            "{got:?}"
        );

        let sse = Request {
            method: "SSE".into(),
            url: format!("http://{sse_addr}/"),
            settings,
            ..Default::default()
        };
        // The server is gone by the third try, which ends it.
        let got = events(sse);
        assert!(
            matches!(&got[..], [
                Event::Open(_),
                Event::In(a),
                Event::Info(first),
                Event::Open(_),
                Event::In(b),
                Event::Info(second),
                Event::Error(_),
            ] if a == "a" && b == "b" && again(first) && again(second)),
            "{got:?}"
        );
        let asked = resumed.join().unwrap();
        assert_eq!(asked, [None, Some("last-event-id: 7".to_owned())]);
    }

    /// A stream connects the way its Settings tab says: they used to stop at plain HTTP,
    /// so a self-signed wss:// server couldn't be reached however the request was set.
    #[test]
    fn streams_connect_the_way_their_settings_say() {
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::try_from(own.signing_key.serialize_der());
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![own.cert.der().clone()], key.unwrap())
            .unwrap();
        let config = std::sync::Arc::new(config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Says a 2 MiB message once connected.
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                if let Ok(mut ws) = tungstenite::accept(rustls::StreamOwned::new(conn, s)) {
                    let _ = ws.send(tungstenite::Message::text("x".repeat(2 << 20)));
                    while ws.read().is_ok() {}
                }
            }
        });
        // Takes the request and never answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let quiet = silent.local_addr().unwrap();
        std::thread::spawn(move || {
            let held: Vec<_> = silent.incoming().take(1).collect();
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(held);
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let clients = rt.block_on(crate::net::build_client(net.clone())).unwrap();
        // Every event up to the first message, then hangs up.
        let events = |req: Request| {
            let (tx, rx) = mpsc::unbounded_channel::<Frame>();
            let (_mqtt, mqtt_rx) = mpsc::unbounded_channel();
            let tx = std::sync::Mutex::new(Some(tx));
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(connect(&clients, net.clone(), req, rx, mqtt_rx, |e| {
                if matches!(e, Event::In(_)) {
                    drop(tx.lock().unwrap().take());
                }
                events.lock().unwrap().push(e);
            }));
            events.into_inner().unwrap()
        };
        let ws = |settings: crate::model::Settings| Request {
            method: "WS".into(),
            url: format!("wss://{addr}/"),
            settings,
            ..Default::default()
        };
        let unchecked = crate::model::Settings {
            verify_tls: false,
            ..Default::default()
        };

        let got = events(ws(Default::default()));
        assert!(
            matches!(got.as_slice(), [Event::Error(e)] if e.contains("certificate")),
            "{got:?}"
        );
        let got = events(ws(unchecked.clone()));
        assert!(
            matches!(&got[..], [Event::Open(_), Event::In(t), ..] if t.len() == 2 << 20),
            "{got:?}"
        );
        // SSE too: past the certificate, this server turns a plain GET away.
        let sse = Request {
            method: "SSE".into(),
            url: format!("https://{addr}/"),
            settings: unchecked.clone(),
            ..Default::default()
        };
        let got = events(sse);
        assert!(
            matches!(got.as_slice(), [Event::Error(e)] if !e.contains("certificate")),
            "{got:?}"
        );
        let small = crate::model::Settings {
            max_message_mb: 1,
            ..unchecked
        };
        let got = events(ws(small));
        assert!(
            matches!(got.as_slice(), [Event::Open(_), Event::Error(e)] if e.contains("too long")),
            "{got:?}"
        );

        let sse = Request {
            method: "SSE".into(),
            url: format!("http://{quiet}/events"),
            settings: crate::model::Settings {
                timeout_ms: 300,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            events(sse),
            [Event::Error("no answer within 300 ms".into())]
        );
    }

    /// The handshake against a server that insists on its subprotocol: init, ack,
    /// subscribe with the query and variables, results until it completes; a ping
    /// mid-stream is answered or the server would drop the connection. Run against both
    /// protocols, which name their messages differently.
    // tungstenite's handshake callback type fixes the error type the lint objects to.
    #[allow(clippy::result_large_err)]
    #[test]
    fn graphql_subscriptions_stream_results_until_complete() {
        use serde_json::{Value, json};
        use tungstenite::handshake::server::{Request as Hs, Response};
        for (proto, subscribe, next) in [
            ("graphql-transport-ws", "subscribe", "next"),
            ("graphql-ws", "start", "data"),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (s, _) = listener.accept().unwrap();
                let pick = |req: &Hs, mut resp: Response| {
                    let offered = req.headers()["sec-websocket-protocol"].to_str().unwrap();
                    assert!(offered.contains(proto), "{offered}");
                    resp.headers_mut()
                        .insert("sec-websocket-protocol", proto.parse().unwrap());
                    Ok(resp)
                };
                let mut ws = tungstenite::accept_hdr(s, pick).unwrap();
                let read = |ws: &mut tungstenite::WebSocket<_>| -> Value {
                    serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap()
                };
                let send = |ws: &mut tungstenite::WebSocket<_>, v: Value| {
                    ws.send(tungstenite::Message::text(v.to_string())).unwrap()
                };
                assert_eq!(read(&mut ws)["type"], "connection_init");
                send(&mut ws, json!({"type": "connection_ack"}));
                let sub = read(&mut ws);
                assert_eq!(sub["type"], subscribe);
                assert_eq!(sub["payload"]["variables"]["room"], "a");
                let id = sub["id"].clone();
                send(
                    &mut ws,
                    json!({"id": id, "type": next, "payload": {"data": {"n": 1}}}),
                );
                if proto == "graphql-transport-ws" {
                    send(&mut ws, json!({"type": "ping"}));
                    assert_eq!(read(&mut ws)["type"], "pong");
                } else {
                    send(&mut ws, json!({"type": "ka"}));
                }
                send(
                    &mut ws,
                    json!({"id": id, "type": next, "payload": {"data": {"n": 2}}}),
                );
                send(&mut ws, json!({"id": id, "type": "complete"}));
                let _ = ws.read();
            });
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let client = rt
                .block_on(crate::net::build_client(crate::net::Network::default()))
                .unwrap()
                .http;
            let req = Request {
                method: "GRAPHQL".into(),
                url: format!("http://{addr}/graphql"),
                body: Body::GraphQL {
                    query: "subscription ($room: String) { messages(room: $room) { n } }".into(),
                    variables: r#"{"room": "a"}"#.into(),
                },
                ..Default::default()
            };
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(graphql(client, req, |e| events.lock().unwrap().push(e)));
            server.join().unwrap();
            let events = events.into_inner().unwrap();
            let ins: Vec<_> = (events.iter())
                .filter_map(|e| match e {
                    Event::In(t) => Some(t.replace([' ', '\n'], "")),
                    _ => None,
                })
                .collect();
            assert_eq!(
                ins,
                [r#"{"data":{"n":1}}"#, r#"{"data":{"n":2}}"#],
                "{proto}"
            );
            assert_eq!(events[0], Event::Open(format!("connected ({proto})")));
            assert_eq!(
                events.last(),
                Some(&Event::Closed("subscription complete".into()))
            );
        }
    }

    #[test]
    fn subscriptions_are_told_from_queries() {
        use crate::graphql::is_subscription;
        assert!(is_subscription("# live\n  subscription OnMsg { m }"));
        assert!(is_subscription("subscription{ m }"));
        assert!(!is_subscription("query { subscriptionCount }"));
        assert!(!is_subscription("subscriptions { x }"));
        assert!(!is_subscription("{ m }"));
    }

    /// The Engine.IO/Socket.IO exchange a socket.io 4 server has: open, CONNECT to the
    /// namespace with the auth payload, answer pings, events both ways, DISCONNECT when the
    /// user hangs up.
    // tungstenite's handshake callback type fixes the error type the lint objects to.
    #[allow(clippy::result_large_err)]
    #[test]
    fn socketio_connects_to_a_namespace_and_trades_events() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let check = |req: &tungstenite::handshake::server::Request, resp| {
                let uri = req.uri().to_string();
                assert!(
                    uri.starts_with("/socket.io/?") && uri.contains("EIO=4"),
                    "{uri}"
                );
                assert!(
                    uri.contains("transport=websocket") && uri.contains("v=1"),
                    "{uri}"
                );
                Ok(resp)
            };
            let mut ws = tungstenite::accept_hdr(s, check).unwrap();
            let mut got = Vec::new();
            let send = |ws: &mut tungstenite::WebSocket<_>, t: &str| {
                ws.send(tungstenite::Message::text(t)).unwrap()
            };
            send(
                &mut ws,
                r#"0{"sid":"e1","pingInterval":25000,"pingTimeout":20000}"#,
            );
            while let Ok(msg) = ws.read() {
                let t = msg.to_text().unwrap_or_default().to_owned();
                got.push(t.clone());
                match t.as_str() {
                    t if t.starts_with("40/chat,") => {
                        send(&mut ws, r#"40/chat,{"sid":"s1"}"#);
                        send(&mut ws, "2");
                    }
                    "3" => send(&mut ws, r#"42/chat,["welcome",{"n":1},"x"]"#),
                    t if t.starts_with("42/chat,") => send(&mut ws, r#"43/chat,7["ok"]"#),
                    _ => {}
                }
            }
            got
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(crate::net::build_client(crate::net::Network::default()))
            .unwrap()
            .http;
        let req = Request {
            method: "SOCKETIO".into(),
            url: format!("http://{addr}/chat?v=1"),
            body: Body::Json {
                text: r#"{"token": "t"}"#.into(),
            },
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tx = std::sync::Mutex::new(Some(tx));
        let events = std::sync::Mutex::new(Vec::new());
        rt.block_on(socketio(client, req, &mut rx, |e| {
            match &e {
                Event::In(t) if t.starts_with("welcome") => {
                    let tx = tx.lock().unwrap();
                    tx.as_ref()
                        .unwrap()
                        .send(Frame::Text(r#"say {"text": "hi"}"#.into()))
                        .unwrap();
                }
                Event::In(t) if t.starts_with("ack") => drop(tx.lock().unwrap().take()),
                _ => {}
            }
            events.lock().unwrap().push(e);
        }));
        let got = server.join().unwrap();
        assert_eq!(
            got,
            [
                r#"40/chat,{"token": "t"}"#,
                "3",
                r#"42/chat,["say",{"text":"hi"}]"#,
                "41/chat,",
                ""
            ]
        );
        assert_eq!(
            events.into_inner().unwrap(),
            [
                Event::Open("connected to /chat".into()),
                Event::In(r#"welcome {"n":1} "x""#.into()),
                Event::Out(r#"["say",{"text":"hi"}]"#.into()),
                Event::In(r#"ack 7 ["ok"]"#.into()),
                Event::Closed("disconnected".into()),
            ]
        );
        assert_eq!(event_args("ping").unwrap(), r#"["ping"]"#);
        assert_eq!(event_args("say hi there").unwrap(), r#"["say","hi there"]"#);
        assert!(event_args("[1, 2]").is_err() && event_args("  ").is_err());
    }

    /// MCP's stream_request queues its messages before the socket is even open; a server
    /// drops events that come before its CONNECT reply, so they must wait for it.
    // tungstenite's handshake callback type fixes the error type the lint objects to.
    #[allow(clippy::result_large_err)]
    #[test]
    fn socketio_holds_queued_events_until_the_namespace_is_joined() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let open = r#"0{"sid":"e1","pingInterval":25000,"pingTimeout":20000}"#;
            ws.send(tungstenite::Message::text(open)).unwrap();
            let mut got = Vec::new();
            while let Ok(msg) = ws.read() {
                let t = msg.to_text().unwrap_or_default().to_owned();
                if t.starts_with("40/chat,") {
                    // Slow to confirm, as a server checking auth is.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    ws.send(tungstenite::Message::text(r#"40/chat,{"sid":"s1"}"#))
                        .unwrap();
                }
                got.push(t);
            }
            got
        });
        let (rt, client) = sio_runtime();
        let req = Request {
            method: "SOCKETIO".into(),
            url: format!("http://{addr}/chat"),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Frame::Text("first".into())).unwrap();
        tx.send(Frame::Text("second 2".into())).unwrap();
        let tx = std::sync::Mutex::new(Some(tx));
        let events = std::sync::Mutex::new(Vec::new());
        rt.block_on(socketio(client, req, &mut rx, |e| {
            if matches!(&e, Event::Out(a) if a.contains("second")) {
                drop(tx.lock().unwrap().take());
            }
            events.lock().unwrap().push(e);
        }));
        assert_eq!(
            server.join().unwrap(),
            [
                "40/chat,",
                r#"42/chat,["first"]"#,
                r#"42/chat,["second",2]"#,
                "41/chat,",
                ""
            ]
        );
        assert_eq!(
            events.into_inner().unwrap(),
            [
                Event::Open("connected to /chat".into()),
                Event::Out(r#"["first"]"#.into()),
                Event::Out(r#"["second",2]"#.into()),
                Event::Closed("disconnected".into()),
            ]
        );
    }

    /// What a server answers a handshake at a path it doesn't serve.
    fn not_found() -> tungstenite::handshake::server::ErrorResponse {
        let mut resp = tungstenite::handshake::server::ErrorResponse::new(None);
        *resp.status_mut() = tungstenite::http::StatusCode::NOT_FOUND;
        // Else the client may keep this connection, which the server then drops, for its
        // next handshake.
        resp.headers_mut()
            .insert("connection", "close".parse().unwrap());
        resp
    }

    fn sio_runtime() -> (tokio::runtime::Runtime, reqwest::Client) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network::default();
        let client = rt.block_on(crate::net::build_client(net)).unwrap().http;
        (rt, client)
    }

    /// A server mounted elsewhere (`path: "/rt"` on socket.io's side) turns /socket.io/
    /// away: the default can't reach it, the path set can, and the namespace still comes
    /// from the URL.
    // tungstenite's handshake callback type fixes the error type the lint objects to.
    #[allow(clippy::result_large_err)]
    #[test]
    fn socketio_handshakes_at_the_path_set_and_nowhere_else() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for s in listener.incoming().take(2).flatten() {
                let mut uri = String::new();
                let mounted = |req: &tungstenite::handshake::server::Request, resp| {
                    uri = req.uri().to_string();
                    match req.uri().path() {
                        "/rt/" => Ok(resp),
                        _ => Err(not_found()),
                    }
                };
                let accepted = tungstenite::accept_hdr(s, mounted).ok();
                seen.push(uri);
                let Some(mut ws) = accepted else { continue };
                let open = r#"0{"sid":"e1","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#;
                ws.send(tungstenite::Message::text(open)).unwrap();
                while let Ok(msg) = ws.read() {
                    let t = msg.to_text().unwrap_or_default().to_owned();
                    if t == "40/chat," {
                        let ok = r#"40/chat,{"sid":"s1"}"#;
                        ws.send(tungstenite::Message::text(ok)).unwrap();
                    }
                    seen.push(t);
                }
            }
            seen
        });
        let (rt, client) = sio_runtime();
        let connect = |path: &str| {
            let req = Request {
                method: "SOCKETIO".into(),
                url: format!("http://{addr}/chat"),
                socketio: crate::model::SocketIo {
                    path: path.into(),
                    v2: false,
                },
                ..Default::default()
            };
            let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
            let tx = std::sync::Mutex::new(Some(tx));
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(socketio(client.clone(), req, &mut rx, |e| {
                drop(tx.lock().unwrap().take()); // hang up once connected
                events.lock().unwrap().push(e);
            }));
            events.into_inner().unwrap()
        };
        let default = connect("");
        assert!(
            matches!(&default[..], [Event::Error(e)] if e.contains("404")),
            "{default:?}"
        );
        assert_eq!(
            connect("rt"),
            [
                Event::Open("connected to /chat".into()),
                Event::Closed("disconnected".into())
            ]
        );
        assert_eq!(
            server.join().unwrap(),
            [
                "/socket.io/?EIO=4&transport=websocket",
                "/rt/?EIO=4&transport=websocket",
                "40/chat,",
                "41/chat,",
                ""
            ]
        );
    }

    /// A Socket.IO 2 server (Engine.IO 3) mounted at /rt/ (with `path: "/rt"`). It says
    /// 40 for "/" unasked, answers the client's pings, and drops a client that misses
    /// them for pingInterval + pingTimeout, as engine.io 3 does; after two pings it
    /// greets `ns` and acks an event. Returns the pings and, apart from them, what the
    /// client sent.
    // tungstenite's handshake callback type fixes the error type the lint objects to.
    #[allow(clippy::result_large_err)]
    pub fn socketio_v2_server(
        ns: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::thread::JoinHandle<(usize, Vec<String>)>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mounted = |req: &tungstenite::handshake::server::Request, resp| {
                let eio3 = req.uri().query().is_some_and(|q| q.contains("EIO=3"));
                match req.uri().path() {
                    "/rt/" if eio3 => Ok(resp),
                    _ => Err(not_found()),
                }
            };
            let mut ws = tungstenite::accept_hdr(s, mounted).unwrap();
            let missed = std::time::Duration::from_millis(100 + 400);
            ws.get_mut().set_read_timeout(Some(missed)).unwrap();
            let send = |ws: &mut tungstenite::WebSocket<_>, t: &str| {
                ws.send(tungstenite::Message::text(t)).unwrap()
            };
            let prefix = match ns {
                "" => String::new(),
                ns => format!("{ns},"),
            };
            send(
                &mut ws,
                r#"0{"sid":"e1","upgrades":[],"pingInterval":100,"pingTimeout":400}"#,
            );
            send(&mut ws, "40");
            let (mut pings, mut got) = (0, Vec::new());
            loop {
                let t = match ws.read() {
                    Ok(msg) => msg.to_text().unwrap_or_default().to_owned(),
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        got.push("<dropped: no ping in time>".into());
                        break;
                    }
                    Err(_) => break,
                };
                match t.as_str() {
                    "2" => {
                        send(&mut ws, "3");
                        pings += 1;
                        if pings == 2 {
                            send(&mut ws, &format!(r#"42{prefix}["welcome",{{"n":1}}]"#));
                        }
                        continue;
                    }
                    t if !ns.is_empty() && t == format!("40{prefix}") => send(&mut ws, t),
                    t if t.starts_with(&format!("42{prefix}[")) => {
                        send(&mut ws, &format!(r#"43{prefix}7["ok"]"#))
                    }
                    _ => {}
                }
                got.push(t);
            }
            (pings, got)
        });
        (addr, server)
    }

    /// Socket.IO 2: the client pings or is dropped, is in "/" without asking, asks for any
    /// other namespace with a bare CONNECT (no auth payload: the JSON body isn't sent),
    /// and frames events and acks as v3+ does.
    #[test]
    fn socketio_v2_pings_and_connects_the_v2_way() {
        for ns in ["/chat", ""] {
            let (addr, server) = socketio_v2_server(ns);
            let (rt, client) = sio_runtime();
            let req = Request {
                method: "SOCKETIO".into(),
                url: format!("http://{addr}{ns}"),
                body: Body::Json {
                    text: r#"{"token": "t"}"#.into(),
                },
                socketio: crate::model::SocketIo {
                    path: "/rt".into(),
                    v2: true,
                },
                ..Default::default()
            };
            let (tx, mut rx) = mpsc::unbounded_channel();
            let tx = std::sync::Mutex::new(Some(tx));
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(socketio(client, req, &mut rx, |e| {
                match &e {
                    Event::In(t) if t.starts_with("welcome") => {
                        let tx = tx.lock().unwrap();
                        tx.as_ref()
                            .unwrap()
                            .send(Frame::Text(r#"say {"text": "hi"}"#.into()))
                            .unwrap();
                    }
                    Event::In(t) if t.starts_with("ack") => drop(tx.lock().unwrap().take()),
                    _ => {}
                }
                events.lock().unwrap().push(e);
            }));
            let (pings, got) = server.join().unwrap();
            let prefix = if ns.is_empty() { "" } else { "/chat," };
            let mut sent = vec![
                format!(r#"42{prefix}["say",{{"text":"hi"}}]"#),
                format!("41{prefix}"),
                String::new(),
            ];
            if !ns.is_empty() {
                sent.insert(0, "40/chat,".into());
            }
            assert_eq!(got, sent, "{ns}");
            assert!(pings >= 2, "{ns}: {pings} pings");
            let at = if ns.is_empty() { "/" } else { ns };
            assert_eq!(
                events.into_inner().unwrap(),
                [
                    Event::Open(format!("connected to {at}")),
                    Event::In(r#"welcome {"n":1}"#.into()),
                    Event::Out(r#"["say",{"text":"hi"}]"#.into()),
                    Event::In(r#"ack 7 ["ok"]"#.into()),
                    Event::Closed("disconnected".into()),
                ],
                "{ns}"
            );
        }
    }

    /// A server that never ends a line can't fill RAM: the line keeps its first MiB and
    /// the stream carries on after the next newline.
    #[test]
    fn an_endless_line_is_capped_and_the_stream_goes_on() {
        let mut p = SseParser::default();
        let mut events = p.feed(b"data: ");
        for _ in 0..40 {
            events.extend(p.feed(&[b'x'; 64 * 1024]));
            assert!(p.buf.len() <= MAX_SSE);
        }
        events.extend(p.feed(b"xx\n\ndata: next\n\n"));
        assert_eq!(events.len(), 2);
        assert!(events[0].len() < MAX_SSE && events[0].starts_with("xxx"));
        assert_eq!(events[1], "next");
    }
}
