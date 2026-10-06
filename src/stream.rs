//! Streaming requests: Server-Sent Events and WebSocket (gRPC streams live in `grpc`).
//! Both go through the shared reqwest client, so proxy/PAC/CA/client-certificate
//! settings apply as for plain HTTP.

use futures_util::{SinkExt, StreamExt};
use reqwest::header::ACCEPT;
use reqwest_websocket::{CloseCode, Message, Upgrade};
use tokio::sync::mpsc;

use crate::http::{self, error_chain};
use crate::model::{Body, Request};

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

/// A GraphQL query that subscribes: it streams over WebSocket instead of sending once.
pub fn subscribes(req: &Request) -> bool {
    req.method == "GRAPHQL"
        && matches!(&req.body, Body::GraphQL { query, .. } if crate::graphql::is_subscription(query))
}

/// Connects a resolved stream request of any kind and runs it until either side ends it.
/// `text` carries what is sent on WebSocket, Socket.IO and gRPC streams, `mqtt` an MQTT
/// session's publishes; dropping a sender disconnects gracefully.
pub async fn connect(
    clients: &crate::net::Clients,
    net: crate::net::Network,
    req: Request,
    text: mpsc::UnboundedReceiver<String>,
    mqtt: mpsc::UnboundedReceiver<crate::mqtt::Command>,
    emit: impl Fn(Event),
) {
    match req.method.as_str() {
        // MQTT isn't HTTP, but it takes the client's proxy choice (maybe from PAC).
        "MQTT" => crate::mqtt::session(req, net, clients.route.clone(), mqtt, emit).await,
        "GRPC" => crate::grpc::stream(clients, req, text, emit).await,
        method => {
            let (ws, sio) = (method.eq_ignore_ascii_case("WS"), method == "SOCKETIO");
            let upgrades = ws || sio || subscribes(&req);
            // Streams keep the default settings; only the host's certificate varies.
            let http = match upgrades {
                true => clients.websocket_for(&req.url),
                false => clients.for_settings(&Default::default(), &req.url),
            };
            match http {
                Ok(http) if ws => websocket(http, req, text, emit).await,
                Ok(http) if sio => socketio(http, req, text, emit).await,
                Ok(http) if upgrades => graphql(http, req, emit).await,
                Ok(http) => sse(http, req, emit).await,
                Err(e) => emit(Event::Error(crate::i18n::tf("Network settings: {}", &[&e]))),
            }
        }
    }
}

/// Reads an event stream until the server ends it; `emit` receives every event.
pub async fn sse(client: reqwest::Client, mut req: Request, emit: impl Fn(Event)) {
    if let Err(e) = http::with_token(&client, &mut req, false).await {
        return emit(Event::Error(e));
    }
    let wants_accept = !req
        .headers
        .iter()
        .any(|h| h.key.eq_ignore_ascii_case("accept"));
    let mut b = match http::build(&client, req) {
        // The client's timeout covers the whole body; a stream lasts until someone ends it.
        Ok(b) => b.timeout(std::time::Duration::MAX),
        Err(e) => return emit(Event::Error(e)),
    };
    if wants_accept {
        b = b.header(ACCEPT, "text/event-stream");
    }
    let resp = match b.send().await {
        Ok(r) => r,
        Err(e) => return emit(Event::Error(error_chain(&e))),
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
            Ok(bytes) => parser
                .feed(&bytes)
                .into_iter()
                .for_each(|e| emit(Event::In(e))),
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
    mut outgoing: mpsc::UnboundedReceiver<String>,
    emit: impl Fn(Event),
) {
    if let Err(e) = http::with_token(&client, &mut req, false).await {
        return emit(Event::Error(e));
    }
    let b = match http::build(&client, req) {
        Ok(b) => b,
        Err(e) => return emit(Event::Error(e)),
    };
    let socket = match b.upgrade().send().await {
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
                Some(text) => {
                    if let Err(e) = sink.send(Message::Text(text.clone())).await {
                        return emit(Event::Error(error_chain(&e)));
                    }
                    emit(Event::Out(text));
                }
                None => {
                    let _ = sink.send(Message::Close { code: CloseCode::Normal, reason: String::new() }).await;
                    return emit(Event::Closed("disconnected".into()));
                }
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(t))) => emit(Event::In(t)),
                Some(Ok(Message::Binary(b))) => emit(Event::In(format!("<binary, {} bytes>", b.len()))),
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
                let _ = sink
                    .send(Message::Close {
                        code: CloseCode::Normal,
                        reason: String::new(),
                    })
                    .await;
                return emit(Event::Closed("subscription complete".into()));
            }
            _ => emit(Event::In(text)),
        }
    }
    emit(Event::Closed("connection closed".into()))
}

/// Socket.IO 4 (Engine.IO 4) over its WebSocket transport. The URL's path is the
/// namespace, a JSON body the CONNECT auth payload. Each outgoing message is an event:
/// `name {"json": "arg"}`, or the wire's own `["name", arg, …]` array.
// ponytail: the handshake path is always /socket.io/ and long-polling isn't spoken; add a
// path setting when a server mounts it elsewhere.
pub async fn socketio(
    client: reqwest::Client,
    req: Request,
    mut outgoing: mpsc::UnboundedReceiver<String>,
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
    url.set_path("/socket.io/");
    url.query_pairs_mut()
        .append_pair("EIO", "4")
        .append_pair("transport", "websocket");
    let auth = match &req.body {
        Body::Json { text } if !text.trim().is_empty() => text.trim().to_owned(),
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
    let b = match http::build(&client, upgrade) {
        Ok(b) => b,
        Err(e) => return emit(Event::Error(e)),
    };
    let socket = match b.upgrade().send().await {
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
    loop {
        tokio::select! {
            out = outgoing.recv() => match out {
                Some(text) => {
                    let args = match event_args(&text) {
                        Ok(a) => a,
                        Err(e) => { emit(Event::Error(e)); continue; }
                    };
                    if let Err(e) = sink.send(Message::Text(format!("42{prefix}{args}"))).await {
                        return emit(Event::Error(error_chain(&e)));
                    }
                    emit(Event::Out(args));
                }
                None => {
                    let _ = sink.send(Message::Text(format!("41{prefix}"))).await;
                    let _ = sink.send(Message::Close { code: CloseCode::Normal, reason: String::new() }).await;
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
                        let connect = format!("40{prefix}{auth}");
                        if let Err(e) = sink.send(Message::Text(connect)).await {
                            return emit(Event::Error(error_chain(&e)));
                        }
                    }
                    Some(b'2') => {
                        if let Err(e) = sink.send(Message::Text("3".into())).await {
                            return emit(Event::Error(error_chain(&e)));
                        }
                    }
                    Some(b'4') => match socketio_packet(&text[1..]) {
                        Packet::Connected => {
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
                _ => {} // id / retry don't change what we display
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
mod tests {
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
        rt.block_on(sse(client, req, |e| events.lock().unwrap().push(e)));
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
        let (tx, rx) = mpsc::unbounded_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let tx = std::sync::Mutex::new(Some(tx));
        rt.block_on(websocket(client, req, rx, |e| {
            // Reply once the server's echo arrives, then hang up by dropping the sender.
            match &e {
                Event::In(t) if t == "hello" => tx
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .send("ping".into())
                    .unwrap(),
                Event::In(t) if t == "echo ping" => drop(tx.lock().unwrap().take()),
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
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            let tx = std::sync::Mutex::new(Some(tx));
            let events = std::sync::Mutex::new(Vec::new());
            rt.block_on(websocket(client, req.clone(), rx, |e| {
                drop(tx.lock().unwrap().take()); // hang up after the first event
                events.lock().unwrap().push(e);
            }));
            events.into_inner().unwrap().remove(0)
        };
        // The everyday client does take h2 here (this server then hangs up on its preface;
        // a real one answers in HTTP/2), so the test does reach the failure.
        let plain = first_event(clients.http.clone());
        assert!(matches!(&plain, Event::Error(_)), "{plain:?}");
        let ws = clients.websocket_for(&req.url).unwrap();
        assert_eq!(first_event(ws), Event::Open("connected".into()));
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
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = std::sync::Mutex::new(Some(tx));
        let events = std::sync::Mutex::new(Vec::new());
        rt.block_on(socketio(client, req, rx, |e| {
            match &e {
                Event::In(t) if t.starts_with("welcome") => {
                    let tx = tx.lock().unwrap();
                    tx.as_ref()
                        .unwrap()
                        .send(r#"say {"text": "hi"}"#.into())
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
