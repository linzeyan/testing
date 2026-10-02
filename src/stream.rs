//! Streaming requests: Server-Sent Events and WebSocket (gRPC streams live in `grpc`).
//! Both go through the shared reqwest client, so proxy/PAC/CA/client-certificate
//! settings apply as for plain HTTP.

use futures_util::{SinkExt, StreamExt};
use reqwest::header::ACCEPT;
use reqwest_websocket::{CloseCode, Message, Upgrade};
use tokio::sync::mpsc;

use crate::http::{self, error_chain};
use crate::model::Request;

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
