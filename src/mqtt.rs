//! MQTT 3.1.1 and 5 sessions over TCP, TLS or WebSocket: connect to the broker, keep its
//! subscriptions in step with the request's topics, publish what the user sends, and show
//! what arrives.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use rumqttc::v5::mqttbytes::v5 as p5;
use rumqttc::{
    Incoming, Outgoing, SubscribeFilter, SubscribeReasonCode, TlsConfiguration, Transport, v5,
};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio::sync::mpsc;

use crate::model::{Auth, Request};
use crate::stream::Event;

/// ponytail: incoming and outgoing messages are capped at 1 MiB; raise it when a broker's
/// messages are bigger (each one is held whole in RAM).
const MAX_PACKET: usize = 1 << 20;

pub struct Publish {
    pub topic: String,
    pub qos: u8,
    pub retain: bool,
    pub payload: String,
}

/// What the app asks of a live session.
pub enum Command {
    Publish(Publish),
    /// The (filter, QoS) pairs to be subscribed to from now on.
    Topics(Vec<(String, u8)>),
}

/// `req` is resolved. `ca_file` is the network settings' CA, trusted on top of the
/// system's for mqtts:// and wss://. Dropping the sender of `commands` disconnects.
pub async fn session(
    req: Request,
    ca_file: String,
    mut commands: mpsc::UnboundedReceiver<Command>,
    emit: impl Fn(Event),
) {
    let (client, mut events) = match connect(&req, &ca_file) {
        Ok(c) => c,
        Err(e) => return emit(Event::Error(e)),
    };
    // SUBACK and UNSUBACK carry only codes; brokers answer in order, so the names wait
    // here in the order they went out.
    let acks = Acks::default();
    let mut have = Vec::new();
    let want = (req.mqtt.topics.iter())
        .map(|t| (t.filter.clone(), t.qos))
        .collect();
    // Queued: it goes out once the broker accepts the connection.
    if let Err(e) = update(&client, &acks, &mut have, want).await {
        return emit(Event::Error(e));
    }
    let send = async {
        while let Some(command) = commands.recv().await {
            let sent = match command {
                Command::Publish(p) => {
                    let shown = format!("[{}] {}", p.topic, p.payload);
                    client.publish(p).await.map(|()| emit(Event::Out(shown)))
                }
                Command::Topics(want) => update(&client, &acks, &mut have, want).await,
            };
            if let Err(e) = sent {
                return emit(Event::Error(e));
            }
        }
        // The receiving side reports the goodbye once it has gone out.
        client.disconnect().await;
        std::future::pending().await
    };
    let receive = async {
        loop {
            match events.poll().await {
                Ok(Got::Connected) => emit(Event::Open("connected".into())),
                Ok(Got::Message(text)) => emit(Event::In(text)),
                Ok(Got::SubAck(codes)) => {
                    let names = acks.subs.lock().unwrap().pop_front().unwrap_or_default();
                    emit(Event::Info(subscribed(&names, &codes)))
                }
                Ok(Got::UnsubAck) => {
                    if let Some(name) = acks.unsubs.lock().unwrap().pop_front() {
                        emit(Event::Info(format!("unsubscribed: {name}")))
                    }
                }
                Ok(Got::Gone(why)) => return emit(Event::Closed(why)),
                Ok(Got::Other) => {}
                // Polling again would reconnect; someone testing a broker wants to see why
                // it failed instead.
                Err(e) => return emit(Event::Error(e)),
            }
        }
    };
    tokio::select! {
        () = send => {}
        () = receive => {}
    }
}

#[derive(Default)]
struct Acks {
    subs: Mutex<VecDeque<Vec<String>>>,
    unsubs: Mutex<VecDeque<String>>,
}

/// Unsubscribes what's no longer wanted and subscribes what's new or has a new QoS, so
/// a live connection follows the Topics tab without reconnecting.
async fn update(
    client: &Client,
    acks: &Acks,
    have: &mut Vec<(String, u8)>,
    want: Vec<(String, u8)>,
) -> Result<(), String> {
    let (gone, new) = changes(have, &want);
    for filter in gone {
        acks.unsubs.lock().unwrap().push_back(filter.clone());
        client.unsubscribe(filter).await?;
    }
    if !new.is_empty() {
        let names = new.iter().map(|(f, _)| f.clone()).collect();
        acks.subs.lock().unwrap().push_back(names);
        client.subscribe(new).await?;
    }
    *have = want;
    Ok(())
}

/// (filters to unsubscribe, filters to subscribe). Subscribing again to a filter replaces
/// its QoS, so a QoS change needs no unsubscribe.
fn changes(have: &[(String, u8)], want: &[(String, u8)]) -> (Vec<String>, Vec<(String, u8)>) {
    let gone = (have.iter())
        .filter(|(f, _)| !want.iter().any(|(w, _)| w == f))
        .map(|(f, _)| f.clone())
        .collect();
    let new = (want.iter())
        .filter(|w| !have.contains(w))
        .cloned()
        .collect();
    (gone, new)
}

/// "subscribed: a/# (QoS 1), admin/# refused"
fn subscribed(names: &[String], codes: &[String]) -> String {
    let each: Vec<String> = (names.iter().zip(codes))
        .map(|(name, code)| format!("{name} {code}"))
        .collect();
    format!("subscribed: {}", each.join(", "))
}

/// The two protocol versions behind one face: rumqttc has a separate client per version
/// with the same shape but different types.
enum Client {
    V3(rumqttc::AsyncClient),
    V5(v5::AsyncClient),
}

// One per connection: the size difference costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
enum Events {
    V3(rumqttc::EventLoop),
    V5(v5::EventLoop),
}

enum Got {
    Connected,
    /// "[topic] payload"
    Message(String),
    /// Per filter: "(QoS 1)" or why it was refused.
    SubAck(Vec<String>),
    UnsubAck,
    Gone(String),
    Other,
}

impl Client {
    async fn subscribe(&self, topics: Vec<(String, u8)>) -> Result<(), String> {
        match self {
            Client::V3(c) => {
                let filters = (topics.into_iter()).map(|(f, q)| SubscribeFilter::new(f, qos3(q)));
                c.subscribe_many(filters).await.map_err(|e| e.to_string())
            }
            Client::V5(c) => {
                let filters = (topics.into_iter()).map(|(f, q)| p5::Filter::new(f, qos5(q)));
                c.subscribe_many(filters).await.map_err(|e| e.to_string())
            }
        }
    }

    async fn unsubscribe(&self, filter: String) -> Result<(), String> {
        match self {
            Client::V3(c) => c.unsubscribe(filter).await.map_err(|e| e.to_string()),
            Client::V5(c) => c.unsubscribe(filter).await.map_err(|e| e.to_string()),
        }
    }

    async fn publish(&self, p: Publish) -> Result<(), String> {
        match self {
            Client::V3(c) => (c.publish(p.topic, qos3(p.qos), p.retain, p.payload).await)
                .map_err(|e| e.to_string()),
            Client::V5(c) => (c.publish(p.topic, qos5(p.qos), p.retain, p.payload).await)
                .map_err(|e| e.to_string()),
        }
    }

    async fn disconnect(&self) {
        let _ = match self {
            Client::V3(c) => c.disconnect().await.map_err(|e| e.to_string()),
            Client::V5(c) => c.disconnect().await.map_err(|e| e.to_string()),
        };
    }
}

impl Events {
    async fn poll(&mut self) -> Result<Got, String> {
        use rumqttc::Event::{Incoming as In3, Outgoing as Out3};
        use v5::Event::{Incoming as In5, Outgoing as Out5};
        let got = match self {
            Events::V3(e) => match e.poll().await {
                Ok(In3(Incoming::ConnAck(_))) => Got::Connected,
                Ok(In3(Incoming::Publish(p))) => Got::Message(shown(&p.topic, &p.payload)),
                Ok(In3(Incoming::SubAck(ack))) => Got::SubAck(
                    (ack.return_codes.iter())
                        .map(|c| match c {
                            SubscribeReasonCode::Success(q) => format!("(QoS {})", *q as u8),
                            SubscribeReasonCode::Failure => "refused".into(),
                        })
                        .collect(),
                ),
                Ok(In3(Incoming::UnsubAck(_))) => Got::UnsubAck,
                Ok(Out3(Outgoing::Disconnect)) => Got::Gone("disconnected".into()),
                Ok(_) => Got::Other,
                Err(rumqttc::ConnectionError::ConnectionRefused(code)) => {
                    return Err(format!("broker refused: {code:?}"));
                }
                Err(e) => return Err(e.to_string()),
            },
            Events::V5(e) => match e.poll().await {
                Ok(In5(p5::Packet::ConnAck(_))) => Got::Connected,
                Ok(In5(p5::Packet::Publish(p))) => {
                    Got::Message(shown(&String::from_utf8_lossy(&p.topic), &p.payload))
                }
                Ok(In5(p5::Packet::SubAck(ack))) => Got::SubAck(
                    (ack.return_codes.iter())
                        .map(|c| match c {
                            p5::SubscribeReasonCode::Success(q) => format!("(QoS {})", *q as u8),
                            // v5 says why: NotAuthorized, TopicFilterInvalid, …
                            why => format!("refused ({why:?})"),
                        })
                        .collect(),
                ),
                Ok(In5(p5::Packet::UnsubAck(_))) => Got::UnsubAck,
                // v5 brokers may hang up with a reason, e.g. SessionTakenOver.
                Ok(In5(p5::Packet::Disconnect(d))) => {
                    let why = d.properties.and_then(|p| p.reason_string);
                    let why = why.map(|s| format!(": {s}")).unwrap_or_default();
                    Got::Gone(format!("broker disconnected: {:?}{why}", d.reason_code))
                }
                Ok(Out5(Outgoing::Disconnect)) => Got::Gone("disconnected".into()),
                Ok(_) => Got::Other,
                Err(v5::ConnectionError::ConnectionRefused(code)) => {
                    return Err(format!("broker refused: {code:?}"));
                }
                Err(e) => return Err(e.to_string()),
            },
        };
        Ok(got)
    }
}

fn shown(topic: &str, payload: &[u8]) -> String {
    format!("[{topic}] {}", String::from_utf8_lossy(payload))
}

fn qos3(n: u8) -> rumqttc::QoS {
    match n {
        0 => rumqttc::QoS::AtMostOnce,
        1 => rumqttc::QoS::AtLeastOnce,
        _ => rumqttc::QoS::ExactlyOnce,
    }
}

fn qos5(n: u8) -> v5::mqttbytes::QoS {
    match n {
        0 => v5::mqttbytes::QoS::AtMostOnce,
        1 => v5::mqttbytes::QoS::AtLeastOnce,
        _ => v5::mqttbytes::QoS::ExactlyOnce,
    }
}

#[derive(Debug, PartialEq)]
enum Wire {
    Tcp,
    Tls,
    Ws,
    Wss,
}

/// From `mqtt://host[:1883]`, `mqtts://host[:8883]` (`tcp://`, `ssl://` too),
/// `ws://host[:80]/path` or `wss://host[:443]/path`. The address is the host, or the
/// whole URL for WebSocket, which rumqttc requests as given.
fn target(url: &str) -> Result<(Wire, String, u16), String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").unwrap_or(("mqtt", url));
    let (wire, default_port) = match scheme.to_lowercase().as_str() {
        "mqtt" | "tcp" => (Wire::Tcp, 1883),
        "mqtts" | "ssl" => (Wire::Tls, 8883),
        "ws" => (Wire::Ws, 80),
        "wss" => (Wire::Wss, 443),
        other => {
            return Err(format!(
                "{other}:// isn't MQTT; use mqtt://, mqtts://, ws:// or wss://"
            ));
        }
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().map_err(|_| format!("bad port \"{port}\""))?;
            (host, port)
        }
        None => (authority, default_port),
    };
    if host.is_empty() {
        return Err("the URL needs a broker, e.g. mqtt://localhost:1883".into());
    }
    let address = match wire {
        Wire::Ws | Wire::Wss => format!("{}://{rest}", scheme.to_lowercase()),
        Wire::Tcp | Wire::Tls => host.to_owned(),
    };
    Ok((wire, address, port))
}

fn connect(req: &Request, ca_file: &str) -> Result<(Client, Events), String> {
    let (wire, address, port) = target(&req.url)?;
    let m = &req.mqtt;
    let id = match m.client_id.trim() {
        "" => {
            let mut b = [0u8; 4];
            getrandom::fill(&mut b).map_err(|e| e.to_string())?;
            format!("apitool-{:08x}", u32::from_le_bytes(b))
        }
        id => id.to_owned(),
    };
    let login = match &req.auth {
        Auth::Basic { username, password } => Some((username, password)),
        Auth::None | Auth::Inherit => None,
        _ => return Err("MQTT signs in with a username and password: use Basic auth".into()),
    };
    let transport = match wire {
        Wire::Tcp => Transport::Tcp,
        Wire::Tls => Transport::tls_with_config(tls_config(ca_file)?),
        Wire::Ws => Transport::Ws,
        Wire::Wss => Transport::wss_with_config(tls_config(ca_file)?),
    };
    let keep_alive = Duration::from_secs(m.keep_alive_secs.into());
    if m.v5 {
        let mut o = v5::MqttOptions::new(id, address, port);
        o.set_keep_alive(keep_alive)
            .set_clean_start(m.clean_session)
            .set_max_packet_size(Some(MAX_PACKET as u32))
            .set_transport(transport);
        // In v5 a session ends with the connection unless it's given a lifetime; "keep
        // the session" means what it does in 3.1.1: until the broker forgets it.
        if !m.clean_session {
            o.set_session_expiry_interval(Some(u32::MAX));
        }
        if let Some((user, password)) = login {
            o.set_credentials(user, password);
        }
        let (c, e) = v5::AsyncClient::new(o, 16);
        return Ok((Client::V5(c), Events::V5(e)));
    }
    let mut o = rumqttc::MqttOptions::new(id, address, port);
    o.set_keep_alive(keep_alive)
        .set_clean_session(m.clean_session)
        .set_max_packet_size(MAX_PACKET, MAX_PACKET)
        .set_transport(transport);
    if let Some((user, password)) = login {
        o.set_credentials(user, password);
    }
    let (c, e) = rumqttc::AsyncClient::new(o, 16);
    Ok((Client::V3(c), Events::V3(e)))
}

/// The system's trust store plus the CA file, as for https. ponytail: the network
/// settings' client certificate and "accept any certificate" aren't applied to MQTT yet.
fn tls_config(ca_file: &str) -> Result<TlsConfiguration, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let ca_file = ca_file.trim();
    let mut extra = Vec::new();
    if !ca_file.is_empty() {
        let pem = std::fs::read(ca_file).map_err(|e| format!("CA file {ca_file}: {e}"))?;
        for cert in CertificateDer::pem_slice_iter(&pem) {
            extra.push(cert.map_err(|e| format!("CA file {ca_file}: {e}"))?);
        }
    }
    let verifier =
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider.clone())
            .map_err(|e| format!("system certificates: {e}"))?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(config.into())
}

#[cfg(test)]
pub mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use bytes::BytesMut;
    use rumqttc::{ConnAck, ConnectReturnCode, Packet, QoS, SubAck, UnsubAck};

    use super::*;
    use crate::model::{Mqtt, Topic};

    /// How the fake broker reads and writes MQTT bytes: straight on TCP, or in binary
    /// WebSocket messages.
    trait Pipe {
        fn fill(&mut self, buf: &mut BytesMut);
        fn put(&mut self, bytes: &[u8]);
    }

    impl Pipe for TcpStream {
        fn fill(&mut self, buf: &mut BytesMut) {
            let mut chunk = [0; 4096];
            let n = self.read(&mut chunk).unwrap();
            assert!(n > 0, "the client hung up without DISCONNECT");
            buf.extend_from_slice(&chunk[..n]);
        }
        fn put(&mut self, bytes: &[u8]) {
            self.write_all(bytes).unwrap();
        }
    }

    impl Pipe for tungstenite::WebSocket<TcpStream> {
        fn fill(&mut self, buf: &mut BytesMut) {
            match self.read().unwrap() {
                tungstenite::Message::Binary(b) => buf.extend_from_slice(&b),
                m => panic!("MQTT rides in binary messages, got {m:?}"),
            }
        }
        fn put(&mut self, bytes: &[u8]) {
            self.send(tungstenite::Message::binary(bytes.to_vec()))
                .unwrap();
        }
    }

    /// Serves one connection on a free port. Over WebSocket it insists on the path
    /// /mqtt and the "mqtt" subprotocol, as brokers do.
    fn listen(ws: bool, serve: fn(&mut dyn Pipe)) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            if !ws {
                return serve(&mut { s });
            }
            use tungstenite::handshake::server::{Request, Response};
            // The callback's error type is tungstenite's, too big for clippy's taste.
            #[allow(clippy::result_large_err)]
            let mut ws = tungstenite::accept_hdr(s, |req: &Request, mut resp: Response| {
                assert_eq!(req.uri().path(), "/mqtt");
                assert_eq!(req.headers()["Sec-WebSocket-Protocol"], "mqtt");
                resp.headers_mut()
                    .insert("Sec-WebSocket-Protocol", "mqtt".parse().unwrap());
                Ok(resp)
            })
            .unwrap();
            serve(&mut ws)
        });
        port
    }

    /// Just enough MQTT 3.1.1 broker for client "tester" signing in as u/p: answers
    /// subscriptions (refusing "denied") and unsubscriptions, says hello on a/1 after the
    /// first subscription, echoes what's published on a/echo and stops at DISCONNECT.
    pub fn broker() -> u16 {
        listen(false, serve_v3)
    }

    fn serve_v3(s: &mut dyn Pipe) {
        let mut buf = BytesMut::new();
        let mut next = |s: &mut dyn Pipe| loop {
            match Packet::read(&mut buf, MAX_PACKET) {
                Ok(p) => return p,
                Err(rumqttc::Error::InsufficientBytes(_)) => s.fill(&mut buf),
                Err(e) => panic!("{e:?}"),
            }
        };
        let send = |s: &mut dyn Pipe, p: Packet| {
            let mut out = BytesMut::new();
            p.write(&mut out, MAX_PACKET).unwrap();
            s.put(&out);
        };
        let Packet::Connect(connect) = next(s) else {
            panic!("CONNECT first");
        };
        let login = connect.login.expect("credentials from Basic auth");
        assert_eq!(
            (login.username.as_str(), login.password.as_str()),
            ("u", "p")
        );
        assert_eq!(connect.client_id, "tester");
        send(
            s,
            Packet::ConnAck(ConnAck::new(ConnectReturnCode::Success, false)),
        );
        let mut hello = true;
        loop {
            match next(s) {
                Packet::Subscribe(sub) => {
                    let codes = sub.filters.iter().map(|f| match f.path.as_str() {
                        "denied" => SubscribeReasonCode::Failure,
                        _ => SubscribeReasonCode::Success(f.qos),
                    });
                    send(s, Packet::SubAck(SubAck::new(sub.pkid, codes.collect())));
                    if std::mem::take(&mut hello) {
                        let hello = rumqttc::Publish::new("a/1", QoS::AtMostOnce, "hello");
                        send(s, Packet::Publish(hello));
                    }
                }
                Packet::Unsubscribe(u) => send(s, Packet::UnsubAck(UnsubAck::new(u.pkid))),
                Packet::Publish(p) => {
                    let text = String::from_utf8_lossy(&p.payload);
                    let echo = format!("echo {} {text}", p.topic);
                    let echo = rumqttc::Publish::new("a/echo", QoS::AtMostOnce, echo);
                    send(s, Packet::Publish(echo));
                }
                Packet::Disconnect => break,
                _ => {}
            }
        }
    }

    /// `serve_v3` in MQTT 5, which says why it refuses "denied".
    fn serve_v5(s: &mut dyn Pipe) {
        use p5::Packet;
        let max = Some(MAX_PACKET as u32);
        let mut buf = BytesMut::new();
        let mut next = |s: &mut dyn Pipe| loop {
            match Packet::read(&mut buf, max) {
                Ok(p) => return p,
                Err(v5::mqttbytes::Error::InsufficientBytes(_)) => s.fill(&mut buf),
                Err(e) => panic!("{e:?}"),
            }
        };
        let send = |s: &mut dyn Pipe, p: Packet| {
            let mut out = BytesMut::new();
            p.write(&mut out, max).unwrap();
            s.put(&out);
        };
        let Packet::Connect(connect, _, login) = next(s) else {
            panic!("CONNECT first");
        };
        let login = login.expect("credentials from Basic auth");
        assert_eq!(
            (login.username.as_str(), login.password.as_str()),
            ("u", "p")
        );
        assert_eq!(connect.client_id, "tester");
        let ack = p5::ConnAck {
            session_present: false,
            code: p5::ConnectReturnCode::Success,
            properties: None,
        };
        send(s, Packet::ConnAck(ack));
        let mut hello = true;
        loop {
            match next(s) {
                Packet::Subscribe(sub) => {
                    let codes = sub.filters.iter().map(|f| match f.path.as_str() {
                        "denied" => p5::SubscribeReasonCode::NotAuthorized,
                        _ => p5::SubscribeReasonCode::Success(f.qos),
                    });
                    let ack = p5::SubAck {
                        pkid: sub.pkid,
                        return_codes: codes.collect(),
                        properties: None,
                    };
                    send(s, Packet::SubAck(ack));
                    if std::mem::take(&mut hello) {
                        let hello = p5::Publish::new("a/1", qos5(0), "hello", None);
                        send(s, Packet::Publish(hello));
                    }
                }
                Packet::Unsubscribe(u) => {
                    let ack = p5::UnsubAck {
                        pkid: u.pkid,
                        reasons: vec![p5::UnsubAckReason::Success; u.filters.len()],
                        properties: None,
                    };
                    send(s, Packet::UnsubAck(ack));
                }
                Packet::Publish(p) => {
                    let topic = String::from_utf8_lossy(&p.topic);
                    let text = String::from_utf8_lossy(&p.payload);
                    let echo = format!("echo {topic} {text}");
                    send(
                        s,
                        Packet::Publish(p5::Publish::new("a/echo", qos5(0), echo, None)),
                    );
                }
                Packet::Disconnect(_) => break,
                _ => {}
            }
        }
    }

    /// Subscribes to a/# and "denied"; answers the hello with a publish; once the echo is
    /// back, swaps "denied" for b/# while connected; hangs up once b/# is subscribed.
    fn exchange(url: String, v5: bool) -> Vec<Event> {
        let req = Request {
            method: "MQTT".into(),
            url,
            auth: Auth::Basic {
                username: "u".into(),
                password: "p".into(),
            },
            mqtt: Mqtt {
                client_id: "tester".into(),
                v5,
                topics: vec![
                    Topic {
                        filter: "a/#".into(),
                        qos: 1,
                        ..Default::default()
                    },
                    Topic {
                        filter: "denied".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = std::sync::Mutex::new(Some(tx));
        let events = std::sync::Mutex::new(Vec::new());
        let command = |c: Command| {
            let tx = tx.lock().unwrap();
            tx.as_ref().unwrap().send(c).ok().unwrap();
        };
        rt.block_on(session(req, String::new(), rx, |e| {
            match &e {
                Event::In(t) if t == "[a/1] hello" => command(Command::Publish(Publish {
                    topic: "cmd".into(),
                    qos: 0,
                    retain: false,
                    payload: "ping".into(),
                })),
                Event::In(t) if t.starts_with("[a/echo]") => {
                    command(Command::Topics(vec![("a/#".into(), 1), ("b/#".into(), 0)]))
                }
                Event::Info(t) if t.contains("b/#") => drop(tx.lock().unwrap().take()),
                _ => {}
            }
            events.lock().unwrap().push(e);
        }));
        events.into_inner().unwrap()
    }

    fn expected(refused: &str) -> Vec<Event> {
        vec![
            Event::Open("connected".into()),
            Event::Info(format!("subscribed: a/# (QoS 1), denied {refused}")),
            Event::In("[a/1] hello".into()),
            Event::Out("[cmd] ping".into()),
            Event::In("[a/echo] echo cmd ping".into()),
            // a/# is unchanged, so it's left alone.
            Event::Info("unsubscribed: denied".into()),
            Event::Info("subscribed: b/# (QoS 0)".into()),
            Event::Closed("disconnected".into()),
        ]
    }

    #[test]
    fn subscribes_publishes_follows_topic_changes_and_says_goodbye() {
        let url = format!("mqtt://127.0.0.1:{}", broker());
        assert_eq!(exchange(url, false), expected("refused"));
    }

    #[test]
    fn mqtt_5_says_why_a_subscription_is_refused() {
        let url = format!("mqtt://127.0.0.1:{}", listen(false, serve_v5));
        assert_eq!(exchange(url, true), expected("refused (NotAuthorized)"));
    }

    #[test]
    fn both_versions_ride_websockets() {
        let url = format!("ws://127.0.0.1:{}/mqtt", listen(true, serve_v3));
        assert_eq!(exchange(url, false), expected("refused"));
        let url = format!("ws://127.0.0.1:{}/mqtt", listen(true, serve_v5));
        assert_eq!(exchange(url, true), expected("refused (NotAuthorized)"));
    }

    #[test]
    fn a_qos_change_resubscribes_without_unsubscribing() {
        let t = |f: &str, q| (f.to_owned(), q);
        let (gone, new) = changes(&[t("a", 0), t("b", 1)], &[t("b", 2), t("c", 0)]);
        assert_eq!(gone, ["a"]);
        assert_eq!(new, [t("b", 2), t("c", 0)]);
        assert_eq!(changes(&[t("a", 0)], &[t("a", 0)]), (vec![], vec![]));
    }

    #[test]
    fn a_bad_url_or_auth_says_so_before_connecting() {
        let bad = |url: &str, auth: Auth| {
            let req = Request {
                url: url.into(),
                auth,
                ..Default::default()
            };
            connect(&req, "").err().unwrap()
        };
        assert!(bad("http://x", Auth::None).contains("mqtt://"));
        assert!(bad("mqtt://x:port", Auth::None).contains("port"));
        let bearer = Auth::Bearer { token: "t".into() };
        assert!(bad("mqtt://x", bearer).contains("Basic"));
        // Without a scheme or port it's plain MQTT on 1883; WebSocket keeps the URL.
        assert_eq!(
            target("broker.test").unwrap(),
            (Wire::Tcp, "broker.test".into(), 1883)
        );
        assert_eq!(
            target("WSS://b.test/mqtt").unwrap(),
            (Wire::Wss, "wss://b.test/mqtt".into(), 443)
        );
    }
}
