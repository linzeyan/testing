//! NATS sessions: connect to a server, keep its subscriptions in step with the request's
//! subjects, publish or ask what the user sends, read JetStream streams and key-value
//! buckets, and show what arrives.
//!
//! Always a direct connection: async-nats dials the server itself, so the app's proxy
//! settings don't apply, as they do for MQTT. TLS is the network settings' as for https.

use std::collections::HashMap;
use std::time::Duration;

use async_nats::jetstream::{self, consumer, kv};
use async_nats::{Client, ConnectOptions, HeaderMap};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::model::{Auth, NatsFrom, NatsSend, Request};
use crate::net::Network;
use crate::stream::Event;

pub struct Publish {
    pub subject: String,
    /// Where a receiver should answer a plain publish; a request has its own inbox.
    pub reply: String,
    pub headers: Vec<(String, String)>,
    pub payload: String,
    pub send: NatsSend,
}

/// One row of the Subjects tab: a subscription, or with `stream` a read of that stream.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Sub {
    pub subject: String,
    pub queue: String,
    pub stream: String,
    pub from: NatsFrom,
}

pub enum KvOp {
    Get(String),
    Put(String, String),
    Delete(String),
    Keys,
    /// Every change to the keys matching this pattern, from now on.
    Watch(String),
    /// Ends this bucket's watches.
    Unwatch,
}

/// What the app asks of a live session.
pub enum Command {
    Publish(Publish),
    /// The rows to be subscribed to from now on.
    Subjects(Vec<Sub>),
    /// Lists the account's streams in the log.
    Streams,
    Kv {
        bucket: String,
        op: KvOp,
    },
}

/// What the tasks reading the connection hand to the session's one loop, which alone
/// calls `emit` (it needn't be `Send`).
enum Got {
    Message(String),
    Info(String),
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
    let mut watches = Tasks::default();
    let want = (req.nats.subjects.iter())
        .map(|s| Sub {
            subject: s.subject.clone(),
            queue: s.queue.clone(),
            stream: s.stream.clone(),
            from: s.from,
        })
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
                Some(Command::Streams) => {
                    tokio::spawn(streams(client.clone(), got_tx.clone()));
                }
                Some(Command::Kv { bucket, op }) => {
                    key_value(&client, bucket, op, &mut watches, &got_tx, &emit);
                }
                None => {
                    // What's queued goes out before the goodbye.
                    let _ = client.flush().await;
                    return emit(Event::Closed("disconnected".into()));
                }
            },
            Some(g) = got.recv() => match g {
                Got::Message(text) => emit(Event::In(text)),
                Got::Info(text) => emit(Event::Info(text)),
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

/// Tasks reading the connection, by what they read; one dropped or replaced is aborted,
/// which ends its subscription, consumer or watch.
struct Tasks<K>(HashMap<K, tokio::task::AbortHandle>);

impl<K> Default for Tasks<K> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<K> Drop for Tasks<K> {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

/// The live subscriptions and stream reads.
#[derive(Default)]
struct Subscriptions {
    live: Tasks<Sub>,
}

impl Subscriptions {
    /// Unsubscribes what's no longer wanted and subscribes what's new, so a live
    /// connection follows the Subjects tab without reconnecting.
    async fn update(
        &mut self,
        client: &Client,
        want: Vec<Sub>,
        got: &mpsc::UnboundedSender<Got>,
        emit: &impl Fn(Event),
    ) {
        let live = &mut self.live.0;
        let gone: Vec<_> = live.keys().filter(|k| !want.contains(k)).cloned().collect();
        for sub in gone {
            if let Some(task) = live.remove(&sub) {
                task.abort();
                emit(Event::Info(format!("unsubscribed: {}", named(&sub))));
            }
        }
        for sub in want {
            if live.contains_key(&sub) {
                continue;
            }
            let name = named(&sub);
            let task = match sub.stream.is_empty() {
                true => subscribe(client, &sub, got.clone()).await,
                false => read_stream(client, &sub, got.clone()).await,
            };
            match task {
                Ok(task) => {
                    emit(Event::Info(format!("subscribed: {name}")));
                    live.insert(sub, task);
                }
                Err(e) => emit(Event::Info(format!("{name}: {e}"))),
            }
        }
    }
}

async fn subscribe(
    client: &Client,
    sub: &Sub,
    got: mpsc::UnboundedSender<Got>,
) -> Result<tokio::task::AbortHandle, String> {
    let subject = sub.subject.clone();
    let subscriber = match sub.queue.is_empty() {
        true => client.subscribe(subject).await,
        false => client.queue_subscribe(subject, sub.queue.clone()).await,
    };
    let mut subscriber = subscriber.map_err(|e| e.to_string())?;
    let name = named(sub);
    let task = tokio::spawn(async move {
        while let Some(m) = subscriber.next().await {
            if got.send(Got::Message(shown(&m, None, ""))).is_err() {
                return;
            }
        }
        // No permission, or the connection closed.
        let _ = got.send(Got::Info(format!("subscription ended: {name}")));
    });
    Ok(task.abort_handle())
}

/// A stream's stored messages in order, through an ephemeral ordered consumer: nothing
/// to acknowledge, and it's gone with the connection.
async fn read_stream(
    client: &Client,
    sub: &Sub,
    got: mpsc::UnboundedSender<Got>,
) -> Result<tokio::task::AbortHandle, String> {
    let stream = (jetstream::new(client.clone()).get_stream(&sub.stream).await)
        .map_err(|e| e.to_string())?;
    let config = consumer::push::OrderedConfig {
        deliver_subject: client.new_inbox(),
        filter_subject: sub.subject.clone(),
        deliver_policy: match sub.from {
            NatsFrom::New => consumer::DeliverPolicy::New,
            NatsFrom::All => consumer::DeliverPolicy::All,
            NatsFrom::Last => consumer::DeliverPolicy::Last,
        },
        ..Default::default()
    };
    let consumer = (stream.create_consumer(config).await).map_err(|e| e.to_string())?;
    let mut messages = consumer.messages().await.map_err(|e| e.to_string())?;
    let name = named(sub);
    let stream = sub.stream.clone();
    let task = tokio::spawn(async move {
        while let Some(m) = messages.next().await {
            let line = match m {
                Ok(m) => {
                    let seq = m.info().map_or(0, |i| i.stream_sequence);
                    // Its reply subject is the consumer's ack address: noise here.
                    let mut plain = m.message.clone();
                    plain.reply = None;
                    Got::Message(shown(&plain, None, &format!(" · {stream} #{seq}")))
                }
                Err(e) => Got::Info(format!("{name}: {e}")),
            };
            if got.send(line).is_err() {
                return;
            }
        }
        let _ = got.send(Got::Info(format!("subscription ended: {name}")));
    });
    Ok(task.abort_handle())
}

/// "orders.> (queue workers)", "orders.> in ORDERS (all)"
fn named(sub: &Sub) -> String {
    let subject = if sub.subject.is_empty() {
        ">"
    } else {
        &sub.subject
    };
    if !sub.stream.is_empty() {
        let from = match sub.from {
            NatsFrom::New => "new",
            NatsFrom::All => "all",
            NatsFrom::Last => "last",
        };
        return format!("{subject} in {} ({from})", sub.stream);
    }
    match sub.queue.is_empty() {
        true => subject.to_owned(),
        false => format!("{subject} (queue {})", sub.queue),
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
    match p.send {
        NatsSend::Request => tags += " · request",
        NatsSend::JetStream => tags += " · jetstream",
        NatsSend::Publish if !p.reply.is_empty() => tags += &format!(" · reply={}", p.reply),
        NatsSend::Publish => {}
    }
    for (k, v) in &p.headers {
        tags += &format!(" · {k}={v}");
    }
    let shown = format!("[{}{tags}] {}", p.subject, p.payload);
    let payload = bytes::Bytes::from(p.payload.into_bytes());
    let (got, subject) = (got.clone(), p.subject);
    match p.send {
        NatsSend::Request => {
            let mut request = async_nats::Request::new().payload(payload);
            if !headers.is_empty() {
                request = request.headers(headers);
            }
            let client = client.clone();
            emit(Event::Out(shown));
            // On its own task: the reply may take the whole timeout, and messages arrive
            // meanwhile.
            tokio::spawn(async move {
                let answer = match client.send_request(subject.clone(), request).await {
                    // A request's answer arrives on a random inbox: named for what was
                    // asked instead.
                    Ok(m) => Got::Message(shown_as(&m, &format!("reply to {subject}"))),
                    Err(e) => Got::Info(format!("request to {subject}: {e}")),
                };
                let _ = got.send(answer);
            });
        }
        NatsSend::JetStream => {
            let js = jetstream::new(client.clone());
            emit(Event::Out(shown));
            tokio::spawn(async move {
                let ack = match js
                    .publish_with_headers(subject.clone(), headers, payload)
                    .await
                {
                    Ok(ack) => ack.await,
                    Err(e) => Err(e),
                };
                let said = match ack {
                    Ok(a) if a.duplicate => {
                        format!("{subject}: {} already has it as #{}", a.stream, a.sequence)
                    }
                    Ok(a) => format!("{subject}: stored in {} as #{}", a.stream, a.sequence),
                    // No stream takes the subject: nobody answers the publish.
                    Err(e) => format!("{subject}: not stored: {e}"),
                };
                let _ = got.send(Got::Info(said));
            });
        }
        NatsSend::Publish => {
            let sent = match (p.reply.is_empty(), headers.is_empty()) {
                (true, true) => client.publish(subject, payload).await,
                (true, false) => (client.publish_with_headers(subject, headers, payload)).await,
                (false, true) => (client.publish_with_reply(subject, p.reply, payload)).await,
                (false, false) => {
                    (client.publish_with_reply_and_headers(subject, p.reply, headers, payload))
                        .await
                }
            };
            sent.map_err(|e| format!("publish: {e}"))?;
            emit(Event::Out(shown));
        }
    }
    Ok(())
}

/// The account's streams, a line each: what the Subjects tab's Stream field can name.
async fn streams(client: Client, got: mpsc::UnboundedSender<Got>) {
    let js = jetstream::new(client);
    let mut all = js.streams();
    let mut count = 0;
    while let Some(stream) = all.next().await {
        let line = match stream {
            Ok(info) => {
                count += 1;
                format!(
                    "stream {}: {} · {} messages · {} bytes · #{} to #{}",
                    info.config.name,
                    info.config.subjects.join(", "),
                    info.state.messages,
                    info.state.bytes,
                    info.state.first_sequence,
                    info.state.last_sequence
                )
            }
            // JetStream off on the server, or no permission.
            Err(e) => format!("streams: {e}"),
        };
        if got.send(Got::Info(line)).is_err() {
            return;
        }
    }
    if count == 0 {
        let _ = got.send(Got::Info("streams: none".into()));
    }
}

/// One KV tab action; its result goes to the log. Each on a task of its own, so a slow
/// bucket doesn't hold up messages; a watch lives until Unwatch or the session's end.
fn key_value(
    client: &Client,
    bucket: String,
    op: KvOp,
    watches: &mut Tasks<(String, String)>,
    got: &mpsc::UnboundedSender<Got>,
    emit: &impl Fn(Event),
) {
    let (js, got) = (jetstream::new(client.clone()), got.clone());
    let pattern = match &op {
        KvOp::Unwatch => {
            let ended: Vec<_> = (watches.0.keys())
                .filter(|(b, _)| *b == bucket)
                .cloned()
                .collect();
            for key in ended {
                if let Some(task) = watches.0.remove(&key) {
                    task.abort();
                    emit(Event::Info(format!(
                        "kv {bucket}: stopped watching {}",
                        key.1
                    )));
                }
            }
            return;
        }
        KvOp::Watch(key) => {
            let pattern = if key.is_empty() { ">" } else { key.as_str() };
            if watches
                .0
                .contains_key(&(bucket.clone(), pattern.to_owned()))
            {
                return emit(Event::Info(format!(
                    "kv {bucket}: already watching {pattern}"
                )));
            }
            Some(pattern.to_owned())
        }
        _ => None,
    };
    let name = bucket.clone();
    let task = tokio::spawn(async move {
        let said = match js.get_key_value(bucket.clone()).await {
            Ok(store) => key_value_op(&store, &bucket, op, &got).await,
            Err(e) => Err(e.to_string()),
        };
        let line = match said {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(e) => Got::Info(format!("kv {bucket}: {e}")),
        };
        let _ = got.send(line);
    });
    if let Some(pattern) = pattern {
        watches.0.insert((name, pattern), task.abort_handle());
    }
}

async fn key_value_op(
    store: &kv::Store,
    bucket: &str,
    op: KvOp,
    got: &mpsc::UnboundedSender<Got>,
) -> Result<Option<Got>, String> {
    let e = |e: &dyn std::fmt::Display| e.to_string();
    Ok(Some(match op {
        KvOp::Get(key) => match store.entry(key.clone()).await.map_err(|x| e(&x))? {
            Some(entry) if entry.operation == kv::Operation::Put => {
                Got::Message(entry_shown(&entry))
            }
            // Deleted keeps a marker as its last revision.
            _ => Got::Info(format!("kv {bucket}: {key} has no value")),
        },
        KvOp::Put(key, value) => {
            let revision = (store.put(&key, value.into_bytes().into()).await).map_err(|x| e(&x))?;
            Got::Info(format!("kv {bucket}: put {key}, revision {revision}"))
        }
        KvOp::Delete(key) => {
            store.delete(&key).await.map_err(|x| e(&x))?;
            Got::Info(format!("kv {bucket}: deleted {key}"))
        }
        KvOp::Keys => {
            let mut keys = store.keys().await.map_err(|x| e(&x))?;
            let mut all = Vec::new();
            while let Some(key) = keys.next().await {
                all.push(key.map_err(|x| e(&x))?);
            }
            all.sort();
            match all.is_empty() {
                true => Got::Info(format!("kv {bucket}: no keys")),
                false => Got::Info(format!("kv {bucket}: keys: {}", all.join(", "))),
            }
        }
        KvOp::Watch(key) => {
            let pattern = if key.is_empty() { ">" } else { key.as_str() };
            let mut entries = store.watch(pattern).await.map_err(|x| e(&x))?;
            let _ = got.send(Got::Info(format!("kv {bucket}: watching {pattern}")));
            while let Some(entry) = entries.next().await {
                let line = match entry {
                    Ok(entry) => Got::Message(entry_shown(&entry)),
                    Err(x) => Got::Info(format!("kv {bucket}: {x}")),
                };
                if got.send(line).is_err() {
                    break;
                }
            }
            return Ok(None);
        }
        KvOp::Unwatch => return Ok(None),
    }))
}

/// "[kv config · feature.x · rev 3] on", a delete as "· deleted".
fn entry_shown(entry: &kv::Entry) -> String {
    let what = match entry.operation {
        kv::Operation::Put => "",
        kv::Operation::Delete => " · deleted",
        kv::Operation::Purge => " · purged",
    };
    format!(
        "[kv {} · {} · rev {}{what}] {}",
        entry.bucket,
        entry.key,
        entry.revision,
        text(&entry.value)
    )
}

fn text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => crate::stream::binary(bytes),
    }
}

/// A message as the log shows it: "[subject · reply=inbox · key=value] payload", the
/// prefix the log's pretty-printer splits off (as MQTT's). `subject` stands in for the
/// message's own, and `more` follows it.
fn shown(m: &async_nats::Message, subject: Option<&str>, more: &str) -> String {
    let mut tags = String::from(more);
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
    format!("[{subject}{tags}] {}", text(&m.payload))
}

fn shown_as(m: &async_nats::Message, subject: &str) -> String {
    shown(m, Some(subject), "")
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

    /// The nats-server the JetStream and key-value tests run against: the fake above
    /// would be a second implementation of both. The CI workflow installs the same.
    const REAL: &str = "2.15.3";

    /// A real nats-server with JetStream, gone when dropped.
    pub(crate) struct Real {
        pub port: u16,
        child: Option<std::process::Child>,
        container: Option<String>,
        store: std::path::PathBuf,
    }

    impl Drop for Real {
        fn drop(&mut self) {
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(name) = &self.container {
                let _ = std::process::Command::new("docker")
                    .args(["rm", "-f", name])
                    .output();
            }
            let _ = std::fs::remove_dir_all(&self.store);
        }
    }

    /// Starts one with these streams (name, subjects) and key-value buckets: the
    /// `nats-server` on PATH (CI), else the official image in docker (a dev machine).
    /// Neither is a failure, not a skip: the tests would pass having checked nothing.
    pub(crate) fn real(streams: &[(&str, &str)], buckets: &[&str]) -> Real {
        use std::process::{Command as Process, Stdio};
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let store =
            std::env::temp_dir().join(format!("apitool-nats-{}-{port}", std::process::id()));
        let mut server = Real {
            port,
            child: None,
            container: None,
            store,
        };
        let binary = Process::new("nats-server")
            .args(["-js", "-a", "127.0.0.1", "-p", &port.to_string(), "-sd"])
            .arg(&server.store)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match binary {
            Ok(child) => server.child = Some(child),
            Err(_) => {
                let name = format!("apitool-nats-{}-{port}", std::process::id());
                let publish = format!("127.0.0.1:{port}:4222");
                let image = format!("nats:{REAL}");
                let run = Process::new("docker")
                    .args([
                        "run", "--rm", "-d", "--name", &name, "-p", &publish, &image, "-js",
                    ])
                    .output();
                match run {
                    Ok(out) if out.status.success() => server.container = Some(name),
                    Ok(out) => panic!(
                        "no nats-server on PATH, and docker couldn't run {image}: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ),
                    Err(e) => panic!(
                        "the JetStream tests need nats-server {REAL} on PATH, or docker: {e}"
                    ),
                }
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let url = format!("nats://127.0.0.1:{port}");
        let ready = async {
            let client = async_nats::connect(&url).await.map_err(|e| e.to_string())?;
            let js = jetstream::new(client.clone());
            for (name, subjects) in streams {
                let config = jetstream::stream::Config {
                    name: (*name).into(),
                    subjects: vec![(*subjects).into()],
                    ..Default::default()
                };
                (js.get_or_create_stream(config).await).map_err(|e| e.to_string())?;
            }
            for bucket in buckets {
                let config = kv::Config {
                    bucket: (*bucket).into(),
                    history: 5,
                    ..Default::default()
                };
                (js.create_key_value(config).await).map_err(|e| e.to_string())?;
            }
            client.flush().await.map_err(|e| e.to_string())
        };
        // docker's port answers before the server inside does, and JetStream comes up a
        // moment after the server: the whole setup is tried again until it holds.
        let setup = async {
            let started = std::time::Instant::now();
            loop {
                let again = async {
                    let client = async_nats::connect(&url).await.map_err(|e| e.to_string())?;
                    let js = jetstream::new(client.clone());
                    (js.query_account().await).map_err(|e| e.to_string())?;
                    Ok::<_, String>(())
                };
                match again.await {
                    Ok(()) => break,
                    Err(e) if started.elapsed() > Duration::from_secs(60) => {
                        panic!("nats-server on {port} didn't come up: {e}")
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
            ready.await
        };
        if let Err(e) = rt.block_on(setup) {
            panic!("setting up nats-server on {port}: {e}");
        }
        server
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
            send: NatsSend::Publish,
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
            send: NatsSend::Request,
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
        let want = vec![Sub {
            subject: "b".into(),
            queue: "workers".into(),
            ..Default::default()
        }];
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

    /// JetStream against a real server: a publish says where it was stored, or that no
    /// stream took it; a stream row reads what's stored from its start, numbered; the
    /// streams can be listed; a stream that isn't there is a line in the log.
    #[test]
    fn a_stream_stores_a_publish_and_is_read_back() {
        let server = real(&[("ORDERS", "orders.>")], &[]);
        let (tx, events) = start(request(server.port, &[]));
        assert!(matches!(next(&events), Event::Open(_)));
        let stored = |subject: &str| Publish {
            send: NatsSend::JetStream,
            ..publish(subject, "one")
        };
        tx.send(Command::Publish(stored("orders.new"))).unwrap();
        let out = Event::Out("[orders.new · jetstream] one".into());
        assert_eq!(next(&events), out);
        let said = "orders.new: stored in ORDERS as #1";
        assert_eq!(next(&events), Event::Info(said.into()));
        tx.send(Command::Publish(stored("nobody"))).unwrap();
        assert!(matches!(next(&events), Event::Out(_)));
        let lost = next(&events);
        assert!(
            matches!(&lost, Event::Info(s) if s.starts_with("nobody: not stored: ")),
            "{lost:?}"
        );

        let row = |stream: &str| Sub {
            subject: "orders.>".into(),
            stream: stream.into(),
            from: NatsFrom::All,
            ..Default::default()
        };
        tx.send(Command::Subjects(vec![row("ORDERS")])).unwrap();
        let subscribed = "subscribed: orders.> in ORDERS (all)";
        assert_eq!(next(&events), Event::Info(subscribed.into()));
        // Stored before this session read the stream: what a subscription never sees.
        let read = "[orders.new · ORDERS #1] one";
        assert_eq!(next(&events), Event::In(read.into()));

        tx.send(Command::Streams).unwrap();
        let listed = next(&events);
        assert!(
            matches!(&listed, Event::Info(s) if s.starts_with("stream ORDERS: orders.> · 1 messages")),
            "{listed:?}"
        );
        tx.send(Command::Subjects(vec![row("ORDERS"), row("NOPE")]))
            .unwrap();
        let missing = next(&events);
        assert!(
            matches!(&missing, Event::Info(s) if s.starts_with("orders.> in NOPE (all): ")),
            "{missing:?}"
        );
    }

    /// A bucket against a real server: put, get, keys and delete say what they did, a
    /// watch shows each change as it happens until it's stopped, and a bucket that isn't
    /// there is a line in the log.
    #[test]
    fn a_bucket_is_written_read_and_watched() {
        let server = real(&[], &["config"]);
        let (tx, events) = start(request(server.port, &[]));
        assert!(matches!(next(&events), Event::Open(_)));
        let kv = |bucket: &str, op| {
            let bucket = bucket.to_owned();
            tx.send(Command::Kv { bucket, op }).unwrap();
        };
        let info = |s: &str| Event::Info(s.into());
        kv("config", KvOp::Watch(String::new()));
        assert_eq!(next(&events), info("kv config: watching >"));
        kv("config", KvOp::Watch(String::new()));
        assert_eq!(next(&events), info("kv config: already watching >"));

        // The put's answer and the watch's line race: both, in either order.
        kv("config", KvOp::Put("mode".into(), "dark".into()));
        let two = [next(&events), next(&events)];
        assert!(
            two.contains(&info("kv config: put mode, revision 1")),
            "{two:?}"
        );
        let entry = Event::In("[kv config · mode · rev 1] dark".into());
        assert!(two.contains(&entry), "{two:?}");
        kv("config", KvOp::Get("mode".into()));
        assert_eq!(next(&events), entry);
        kv("config", KvOp::Keys);
        assert_eq!(next(&events), info("kv config: keys: mode"));

        kv("config", KvOp::Delete("mode".into()));
        let two = [next(&events), next(&events)];
        assert!(two.contains(&info("kv config: deleted mode")), "{two:?}");
        let gone = Event::In("[kv config · mode · rev 2 · deleted] ".into());
        assert!(two.contains(&gone), "{two:?}");
        kv("config", KvOp::Get("mode".into()));
        assert_eq!(next(&events), info("kv config: mode has no value"));
        kv("config", KvOp::Keys);
        assert_eq!(next(&events), info("kv config: no keys"));

        kv("config", KvOp::Unwatch);
        assert_eq!(next(&events), info("kv config: stopped watching >"));
        kv("config", KvOp::Put("mode".into(), "light".into()));
        // Only the put's answer: the watch is over.
        assert_eq!(next(&events), info("kv config: put mode, revision 3"));
        kv("nope", KvOp::Keys);
        let missing = next(&events);
        assert!(
            matches!(&missing, Event::Info(s) if s.starts_with("kv nope: ")),
            "{missing:?}"
        );
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
