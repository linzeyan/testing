//! NATS sessions: connect to a server, keep its subscriptions in step with the request's
//! subjects, publish or ask what the user sends, and show what arrives.
//!
//! Always a direct connection: async-nats dials the server itself, so the app's proxy
//! settings don't apply, as they do for MQTT. TLS is the network settings' as for https.

use std::collections::HashMap;
use std::time::Duration;

use async_nats::{Client, ConnectOptions, HeaderMap};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::model::{Auth, Request};
use crate::net::Network;
use crate::stream::Event;

pub struct Publish {
    pub subject: String,
    /// Where a receiver should answer; unused by a request, which has its own inbox.
    pub reply: String,
    pub headers: Vec<(String, String)>,
    pub payload: String,
    /// Wait for the first reply instead of only publishing.
    pub request: bool,
}

/// What the app asks of a live session.
pub enum Command {
    Publish(Publish),
    /// The (subject, queue group) pairs to be subscribed to from now on.
    Subjects(Vec<(String, String)>),
}

/// What the tasks reading the connection hand to the session's one loop, which alone
/// calls `emit` (it needn't be `Send`).
enum Got {
    Message(String),
    Info(String),
    /// A subscription the server ended (no permission, or the connection closed).
    Ended(String),
}

/// `req` is resolved. `net` gives a TLS connection its settings. Dropping the sender of
/// `commands` disconnects.
pub async fn session(
    req: Request,
    net: Network,
    mut commands: mpsc::UnboundedReceiver<Command>,
    emit: impl Fn(Event),
) {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let client = match connect(&req, &net, events_tx).await {
        Ok(c) => c,
        Err(e) => return emit(Event::Error(e)),
    };
    let info = client.server_info();
    emit(Event::Open(format!(
        "connected to {} (nats-server {})",
        info.server_name, info.version
    )));
    let (got_tx, mut got) = mpsc::unbounded_channel();
    let mut subs = Subscriptions::default();
    let want = (req.nats.subjects.iter())
        .map(|s| (s.subject.clone(), s.queue.clone()))
        .collect();
    subs.update(&client, want, &got_tx, &emit).await;
    let tries = req.settings.reconnect_attempts;
    // The callback reports the first connection too, which Open has already said.
    let mut connected = true;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Publish(p)) => {
                    if let Err(e) = publish(&client, p, &got_tx, &emit).await {
                        emit(Event::Info(e));
                    }
                }
                Some(Command::Subjects(want)) => subs.update(&client, want, &got_tx, &emit).await,
                None => {
                    // What's queued goes out before the goodbye.
                    let _ = client.flush().await;
                    return emit(Event::Closed("disconnected".into()));
                }
            },
            Some(g) = got.recv() => match g {
                Got::Message(text) => emit(Event::In(text)),
                Got::Info(text) | Got::Ended(text) => emit(Event::Info(text)),
            },
            Some(event) = events.recv() => match event {
                async_nats::Event::Connected if !connected => {
                    connected = true;
                    // The client subscribes again by itself.
                    emit(Event::Info("reconnected".into()));
                }
                async_nats::Event::Connected => {}
                // async-nats would try for ever; someone testing a server wants to see it
                // went away, unless the request says to reconnect.
                async_nats::Event::Disconnected if tries == 0 => {
                    return emit(Event::Closed("the server closed the connection".into()));
                }
                async_nats::Event::Disconnected => {
                    connected = false;
                    let every = req.settings.reconnect_every().as_secs_f32();
                    emit(Event::Info(format!(
                        "disconnected; reconnecting every {every} s, up to {tries} times"
                    )));
                }
                async_nats::Event::ClientError(async_nats::ClientError::MaxReconnects)
                | async_nats::Event::Closed => {
                    return emit(Event::Closed("the server stayed unreachable".into()));
                }
                // "permissions violation for subscription to …" and the like.
                other => emit(Event::Info(other.to_string())),
            },
        }
    }
}

async fn connect(
    req: &Request,
    net: &Network,
    events: mpsc::UnboundedSender<async_nats::Event>,
) -> Result<Client, String> {
    let url = req.url.trim();
    if url.is_empty() {
        return Err("no server: nats://host:4222".into());
    }
    let n = &req.nats;
    // As for an HTTP request: its own timeout, else the network settings'.
    let wait = match req.settings.timeout_ms {
        0 => Duration::from_secs(net.timeout_secs.max(1)),
        ms => Duration::from_millis(ms),
    };
    let every = req.settings.reconnect_every();
    let name = if n.name.is_empty() {
        "apitool"
    } else {
        &n.name
    };
    let mut options = ConnectOptions::new()
        .name(name)
        .connection_timeout(wait)
        .request_timeout(Some(wait))
        .reconnect_delay_callback(move |_| every)
        .event_callback(move |event| {
            let events = events.clone();
            async move {
                let _ = events.send(event);
            }
        });
    if req.settings.reconnect_attempts > 0 {
        options = options.max_reconnects(req.settings.reconnect_attempts as usize);
    }
    options = match &req.auth {
        Auth::Basic { username, password } => {
            options.user_and_password(username.clone(), password.clone())
        }
        Auth::Bearer { token } => options.token(token.clone()),
        Auth::None | Auth::Inherit => options,
        _ => {
            return Err(
                "NATS signs in with a username and password (Basic), a token (Bearer), \
                 or a credentials file or NKey in Settings"
                    .into(),
            );
        }
    };
    if !n.creds.is_empty() {
        options = (options.credentials_file(&n.creds).await)
            .map_err(|e| format!("credentials file {}: {e}", n.creds))?;
    } else if !n.nkey.is_empty() {
        options = options.nkey(n.nkey.clone());
    }
    let insecure = net.insecure || !req.settings.verify_tls;
    let ring = rustls::crypto::ring::default_provider();
    let cert = net.cert_for(url);
    let tls = crate::mqtt::tls_config(net, cert, insecure, ring, rustls::DEFAULT_VERSIONS)?;
    options = options.tls_client_config(tls);
    // tls:// insists on TLS; a plain nats:// upgrades only when the server's INFO asks,
    // which giving a TLS config would otherwise turn into a demand.
    options = match url.starts_with("tls://") {
        true => options.require_tls(true),
        false => options.tls_optional(),
    };
    // Its message already carries its cause.
    (options.connect(url).await).map_err(|e| e.to_string())
}

/// The live subscriptions, each read by its own task; dropping one unsubscribes it.
#[derive(Default)]
struct Subscriptions {
    live: HashMap<(String, String), tokio::task::AbortHandle>,
}

impl Subscriptions {
    /// Unsubscribes what's no longer wanted and subscribes what's new, so a live
    /// connection follows the Subjects tab without reconnecting.
    async fn update(
        &mut self,
        client: &Client,
        want: Vec<(String, String)>,
        got: &mpsc::UnboundedSender<Got>,
        emit: &impl Fn(Event),
    ) {
        let gone: Vec<_> = (self.live.keys())
            .filter(|k| !want.contains(k))
            .cloned()
            .collect();
        for key in gone {
            if let Some(task) = self.live.remove(&key) {
                task.abort();
                emit(Event::Info(format!("unsubscribed: {}", named(&key))));
            }
        }
        for key in want {
            if self.live.contains_key(&key) {
                continue;
            }
            let (subject, queue) = key.clone();
            let sub = match queue.is_empty() {
                true => client.subscribe(subject).await,
                false => client.queue_subscribe(subject, queue).await,
            };
            let mut sub = match sub {
                Ok(sub) => sub,
                Err(e) => {
                    emit(Event::Info(format!("{}: {e}", named(&key))));
                    continue;
                }
            };
            emit(Event::Info(format!("subscribed: {}", named(&key))));
            let (got, name) = (got.clone(), named(&key));
            let task = tokio::spawn(async move {
                while let Some(m) = sub.next().await {
                    if got.send(Got::Message(shown(&m, None))).is_err() {
                        return;
                    }
                }
                let _ = got.send(Got::Ended(format!("subscription ended: {name}")));
            });
            self.live.insert(key, task.abort_handle());
        }
    }
}

impl Drop for Subscriptions {
    fn drop(&mut self) {
        for task in self.live.values() {
            task.abort();
        }
    }
}

/// "orders.> (queue workers)"
fn named((subject, queue): &(String, String)) -> String {
    match queue.is_empty() {
        true => subject.clone(),
        false => format!("{subject} (queue {queue})"),
    }
}

/// Err is for the log: a bad subject or header doesn't end the session.
async fn publish(
    client: &Client,
    p: Publish,
    got: &mpsc::UnboundedSender<Got>,
    emit: &impl Fn(Event),
) -> Result<(), String> {
    let mut headers = HeaderMap::new();
    for (k, v) in &p.headers {
        // Append: a name given twice is sent twice, as HTTP's are.
        headers.append(k.as_str(), v.as_str());
    }
    let mut tags = String::new();
    if p.request {
        tags += " · request";
    } else if !p.reply.is_empty() {
        tags += &format!(" · reply={}", p.reply);
    }
    for (k, v) in &p.headers {
        tags += &format!(" · {k}={v}");
    }
    let shown = format!("[{}{tags}] {}", p.subject, p.payload);
    let payload = bytes::Bytes::from(p.payload.into_bytes());
    if p.request {
        let mut request = async_nats::Request::new().payload(payload);
        if !headers.is_empty() {
            request = request.headers(headers);
        }
        let (client, got, subject) = (client.clone(), got.clone(), p.subject);
        emit(Event::Out(shown));
        // On its own task: the reply may take the whole timeout, and messages arrive
        // meanwhile.
        tokio::spawn(async move {
            let answer = match client.send_request(subject.clone(), request).await {
                Ok(m) => Got::Message(shown_reply(&m, &subject)),
                Err(e) => Got::Info(format!("request to {subject}: {e}")),
            };
            let _ = got.send(answer);
        });
        return Ok(());
    }
    let sent = match (p.reply.is_empty(), headers.is_empty()) {
        (true, true) => client.publish(p.subject, payload).await,
        (true, false) => (client.publish_with_headers(p.subject, headers, payload)).await,
        (false, true) => (client.publish_with_reply(p.subject, p.reply, payload)).await,
        (false, false) => {
            (client.publish_with_reply_and_headers(p.subject, p.reply, headers, payload)).await
        }
    };
    sent.map_err(|e| format!("publish: {e}"))?;
    emit(Event::Out(shown));
    Ok(())
}

/// A message as the log shows it: "[subject · reply=inbox · key=value] payload", the
/// prefix the log's pretty-printer splits off (as MQTT's).
fn shown(m: &async_nats::Message, subject: Option<&str>) -> String {
    let mut tags = String::new();
    if let Some(reply) = &m.reply {
        tags += &format!(" · reply={reply}");
    }
    if let Some(status) = m.status {
        tags += &format!(" · status={status}");
    }
    if let Some(headers) = &m.headers {
        let mut all: Vec<_> = headers.iter().collect();
        // A HashMap inside: sorted, so the same message reads the same each time.
        all.sort_by_key(|(k, _)| k.to_string());
        for (k, values) in all {
            for v in values {
                tags += &format!(" · {k}={}", v.as_str());
            }
        }
    }
    let subject = subject.map_or_else(|| m.subject.to_string(), str::to_owned);
    let body = match std::str::from_utf8(&m.payload) {
        Ok(text) => text.to_owned(),
        Err(_) => crate::stream::binary(&m.payload),
    };
    format!("[{subject}{tags}] {body}")
}

/// A request's answer arrives on a random inbox: named for what was asked instead.
fn shown_reply(m: &async_nats::Message, asked: &str) -> String {
    shown(m, Some(&format!("reply to {asked}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{KeyValue, Nats, NatsSubject};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// A NATS server small enough to read: INFO, CONNECT, PING/PONG, SUB/UNSUB and
    /// PUB/HPUB delivered to this one connection's own matching subscriptions (`*` and
    /// `>` wildcards), with headers and reply subjects passed along.
    /// A message to `svc.echo` is answered on its reply subject, as a service would.
    /// Returns the port. One connection per accept, each on its own thread.
    pub(crate) fn server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || serve(stream));
            }
        });
        port
    }

    fn serve(stream: TcpStream) {
        let mut out = stream.try_clone().unwrap();
        let info = r#"{"server_id":"T","server_name":"test","version":"2.12.0","host":"127.0.0.1","port":4222,"headers":true,"max_payload":1048576,"proto":1}"#;
        write!(out, "INFO {info}\r\n").unwrap();
        let mut input = BufReader::new(stream);
        // sid -> subject
        let mut subs: Vec<(String, String)> = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let words: Vec<&str> = line.split_whitespace().collect();
            let Some(op) = words.first() else { continue };
            match op.to_ascii_uppercase().as_str() {
                "CONNECT" => {}
                "PING" => out.write_all(b"PONG\r\n").unwrap(),
                "PONG" => {}
                // SUB subject [queue] sid
                "SUB" => subs.push((words[words.len() - 1].into(), words[1].into())),
                "UNSUB" => subs.retain(|(sid, _)| sid != words[1]),
                op @ ("PUB" | "HPUB") => {
                    let hpub = op == "HPUB";
                    // PUB subject [reply] bytes; HPUB subject [reply] header-bytes bytes
                    let sizes = if hpub { 2 } else { 1 };
                    let total: usize = words[words.len() - 1].parse().unwrap();
                    let reply = (words.len() == 2 + sizes + 1).then(|| words[2].to_owned());
                    let subject = words[1].to_owned();
                    let mut body = vec![0; total + 2];
                    input.read_exact(&mut body).unwrap();
                    body.truncate(total);
                    let head = if hpub { words[words.len() - 2] } else { "" };
                    let deliver =
                        |out: &mut TcpStream, sid: &str, to: &str, reply: Option<&str>| {
                            let reply = reply.map_or(String::new(), |r| format!(" {r}"));
                            match hpub {
                                true => write!(out, "HMSG {to} {sid}{reply} {head} {total}\r\n"),
                                false => write!(out, "MSG {to} {sid}{reply} {total}\r\n"),
                            }
                            .unwrap();
                            out.write_all(&body).unwrap();
                            out.write_all(b"\r\n").unwrap();
                        };
                    // Token by token: `*` is any one token, `>` whatever follows.
                    let matches = |pattern: &str, subject: &str| {
                        let (mut p, mut s) = (pattern.split('.'), subject.split('.'));
                        loop {
                            match (p.next(), s.next()) {
                                (Some(">"), Some(_)) => return true,
                                (Some("*"), Some(_)) => {}
                                (Some(a), Some(b)) if a == b => {}
                                (None, None) => return true,
                                _ => return false,
                            }
                        }
                    };
                    for (sid, pattern) in &subs {
                        if matches(pattern, &subject) {
                            deliver(&mut out, sid, &subject, reply.as_deref());
                        }
                    }
                    // The echo service: back to whoever asked, on the reply subject.
                    if subject == "svc.echo"
                        && let Some(reply) = &reply
                    {
                        for (sid, pattern) in &subs {
                            if matches(pattern, reply) {
                                deliver(&mut out, sid, reply, None);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn request(port: u16, subjects: &[&str]) -> Request {
        Request {
            method: "NATS".into(),
            url: format!("nats://127.0.0.1:{port}"),
            nats: Nats {
                subjects: (subjects.iter())
                    .map(|s| NatsSubject {
                        subject: (*s).into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Runs a session on its own runtime thread; returns the command sender and the
    /// events as they come.
    fn start(
        req: Request,
    ) -> (
        mpsc::UnboundedSender<Command>,
        std::sync::mpsc::Receiver<Event>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (events_tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(session(req, Network::default(), rx, move |e| {
                let _ = events_tx.send(e);
            }));
        });
        (tx, events)
    }

    fn next(events: &std::sync::mpsc::Receiver<Event>) -> Event {
        (events.recv_timeout(Duration::from_secs(60))).expect("an event within a minute")
    }

    fn publish(subject: &str, payload: &str) -> Publish {
        Publish {
            subject: subject.into(),
            reply: String::new(),
            headers: Vec::new(),
            payload: payload.into(),
            request: false,
        }
    }

    /// The reason to have it: what's published to a subject comes back on the wildcard
    /// subscription that covers it, headers and reply subject shown with it.
    #[test]
    fn a_publish_arrives_on_a_matching_subscription() {
        let (tx, events) = start(request(server(), &["orders.>"]));
        let first = next(&events);
        assert!(
            matches!(&first, Event::Open(s) if s.contains("nats-server 2.12.0")),
            "{first:?}"
        );
        assert_eq!(next(&events), Event::Info("subscribed: orders.>".into()));
        let p = Publish {
            reply: "answers".into(),
            headers: vec![("Trace".into(), "abc".into())],
            ..publish("orders.new", r#"{"id":1}"#)
        };
        tx.send(Command::Publish(p)).unwrap();
        let line = r#"[orders.new · reply=answers · Trace=abc] {"id":1}"#;
        assert_eq!(next(&events), Event::Out(line.into()));
        assert_eq!(next(&events), Event::In(line.into()));
        // Dropping the sender is Disconnect.
        drop(tx);
        assert_eq!(next(&events), Event::Closed("disconnected".into()));
    }

    /// A request waits for its reply and shows it under what was asked, not the inbox.
    #[test]
    fn a_request_shows_its_reply() {
        let (tx, events) = start(request(server(), &[]));
        assert!(matches!(next(&events), Event::Open(_)));
        let p = Publish {
            request: true,
            ..publish("svc.echo", "ping")
        };
        tx.send(Command::Publish(p)).unwrap();
        assert_eq!(
            next(&events),
            Event::Out("[svc.echo · request] ping".into())
        );
        assert_eq!(next(&events), Event::In("[reply to svc.echo] ping".into()));
    }

    /// Unticking a subject while connected stops its messages; the session stays up.
    #[test]
    fn the_subjects_follow_the_tab_while_connected() {
        let (tx, events) = start(request(server(), &["a"]));
        assert!(matches!(next(&events), Event::Open(_)));
        assert_eq!(next(&events), Event::Info("subscribed: a".into()));
        let want = vec![("b".to_owned(), "workers".to_owned())];
        tx.send(Command::Subjects(want)).unwrap();
        assert_eq!(next(&events), Event::Info("unsubscribed: a".into()));
        let subscribed = "subscribed: b (queue workers)";
        assert_eq!(next(&events), Event::Info(subscribed.into()));
        tx.send(Command::Publish(publish("a", "dropped"))).unwrap();
        tx.send(Command::Publish(publish("b", "kept"))).unwrap();
        assert_eq!(next(&events), Event::Out("[a] dropped".into()));
        assert_eq!(next(&events), Event::Out("[b] kept".into()));
        assert_eq!(next(&events), Event::In("[b] kept".into()));
    }

    /// Nobody listening is an error to read, not the end of the session.
    #[test]
    fn a_server_that_is_not_there_says_so() {
        // Bound and dropped: nothing listens there now.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (_tx, events) = start(request(port, &[]));
        assert!(matches!(next(&events), Event::Error(_)));
    }

    /// Only what NATS can sign in with: the others would be silently unused.
    #[test]
    fn an_auth_type_nats_lacks_is_refused() {
        let mut req = request(server(), &[]);
        req.auth = Auth::ApiKey {
            key: "k".into(),
            value: "v".into(),
            in_query: false,
        };
        let (_tx, events) = start(req);
        assert!(matches!(next(&events), Event::Error(e) if e.contains("Basic")));
        let _ = KeyValue::new("", "");
    }
}
