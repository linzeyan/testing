//! MQTT 3.1.1 and 5 sessions over TCP, TLS or WebSocket: connect to the broker, keep its
//! subscriptions in step with the request's topics, publish what the user sends, and show
//! what arrives.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use rumqttc::v5::mqttbytes::v5 as p5;
use rumqttc::{
    Incoming, Outgoing, Proxy, ProxyAuth, ProxyType, SubscribeFilter, SubscribeReasonCode,
    TlsConfiguration, Transport, v5,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::mpsc;

use crate::model::{Auth, Request};
use crate::net::{Network, Route};
use crate::stream::Event;

/// The largest message either way unless the request's settings raise it: each one is
/// held whole in RAM.
pub const MAX_PACKET: usize = 1 << 20;

pub struct Publish {
    pub topic: String,
    pub qos: u8,
    pub retain: bool,
    pub payload: String,
    /// MQTT 5 user properties; the app refuses these and the three below for a 3.1.1
    /// connection.
    pub properties: Vec<(String, String)>,
    pub content_type: String,
    pub response_topic: String,
    /// 0: none.
    pub expiry_secs: u32,
}

impl Publish {
    fn v5(&self) -> p5::PublishProperties {
        let set = |s: &String| (!s.is_empty()).then(|| s.clone());
        p5::PublishProperties {
            message_expiry_interval: (self.expiry_secs > 0).then_some(self.expiry_secs),
            content_type: set(&self.content_type),
            response_topic: set(&self.response_topic),
            user_properties: self.properties.clone(),
            ..Default::default()
        }
    }
}

/// What the app asks of a live session.
pub enum Command {
    Publish(Publish),
    /// The (filter, QoS) pairs to be subscribed to from now on.
    Topics(Vec<(String, u8)>),
}

/// `req` is resolved. `net` gives mqtts:// and wss:// their TLS settings and `route` the
/// proxy, as for https. Dropping the sender of `commands` disconnects.
pub async fn session(
    req: Request,
    net: Network,
    route: Route,
    mut commands: mpsc::UnboundedReceiver<Command>,
    emit: impl Fn(Event),
) {
    let (client, mut events) = match connect(&req, &net, &route) {
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
    // Disconnect was asked for: the goodbye ends the session, whatever the settings say.
    let asked = std::sync::atomic::AtomicBool::new(false);
    // A broker that forgot this client (clean session) forgot its subscriptions too.
    let resubscribe = tokio::sync::Notify::new();
    let send = async {
        loop {
            let sent = tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Publish(p)) => {
                        let shown = shown(&p.topic, Some(&p.v5()), p.payload.as_bytes());
                        client.publish(p).await.map(|()| emit(Event::Out(shown)))
                    }
                    Some(Command::Topics(want)) => update(&client, &acks, &mut have, want).await,
                    None => break,
                },
                () = resubscribe.notified() => {
                    let want = std::mem::take(&mut have);
                    update(&client, &acks, &mut have, want).await
                }
            };
            if let Err(e) = sent {
                return emit(Event::Error(e));
            }
        }
        asked.store(true, std::sync::atomic::Ordering::Relaxed);
        // The receiving side reports the goodbye once it has gone out.
        client.disconnect().await;
        std::future::pending().await
    };
    let (tries, every) = (
        req.settings.reconnect_attempts,
        req.settings.reconnect_every(),
    );
    let gives_up = |tried| asked.load(std::sync::atomic::Ordering::Relaxed) || tried >= tries;
    let receive = async {
        // In a row: a connection that opens starts the count over.
        let (mut tried, mut reconnected) = (0, false);
        loop {
            match events.poll().await {
                Ok(Got::Connected) => {
                    if std::mem::take(&mut reconnected) {
                        resubscribe.notify_one();
                    }
                    tried = 0;
                    emit(Event::Open("connected".into()))
                }
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
                Ok(Got::Gone(why)) if gives_up(tried) => return emit(Event::Closed(why)),
                Ok(Got::Other) => {}
                // Polling again would reconnect; someone testing a broker wants to see why
                // it failed instead, unless the request says to reconnect.
                Err(e) if gives_up(tried) => return emit(Event::Error(e)),
                Ok(Got::Gone(why)) | Err(why) => {
                    tried += 1;
                    emit(Event::Info(crate::stream::reconnecting(
                        &why, tried, tries, every,
                    )));
                    tokio::time::sleep(every).await;
                    reconnected = true;
                }
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
    /// "[topic · key=value] payload"
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
            Client::V5(c) => {
                let properties = p.v5();
                let (topic, qos, payload) = (p.topic, qos5(p.qos), p.payload);
                (c.publish_with_properties(topic, qos, p.retain, payload, properties)
                    .await)
                    .map_err(|e| e.to_string())
            }
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
                Ok(In3(Incoming::Publish(p))) => Got::Message(shown(&p.topic, None, &p.payload)),
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
                Ok(In5(p5::Packet::Publish(p))) => Got::Message(shown(
                    &String::from_utf8_lossy(&p.topic),
                    p.properties.as_ref(),
                    &p.payload,
                )),
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

/// User properties go in the brackets, so the one-line list shows them too.
fn shown(topic: &str, properties: Option<&p5::PublishProperties>, payload: &[u8]) -> String {
    let mut tags = String::new();
    if let Some(p) = properties {
        let expiry = p.message_expiry_interval.map(|s| format!("{s}s"));
        let named = [
            ("content-type", p.content_type.clone()),
            ("response-topic", p.response_topic.clone()),
            ("expiry", expiry),
        ];
        let named = named
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_owned(), v?)));
        for (k, v) in named.chain(p.user_properties.iter().cloned()) {
            tags += &format!(" · {k}={v}");
        }
    }
    format!("[{topic}{tags}] {}", String::from_utf8_lossy(payload))
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

fn connect(req: &Request, net: &Network, route: &Route) -> Result<(Client, Events), String> {
    let (wire, address, port) = target(&req.url)?;
    let insecure = net.insecure || !req.settings.verify_tls;
    // The proxy's own TLS, if it's an https:// one, is checked as the broker's would be.
    let tls = |url: &str| {
        let ring = rustls::crypto::ring::default_provider();
        let config = tls_config(
            net,
            net.cert_for(url),
            insecure,
            ring,
            rustls::DEFAULT_VERSIONS,
        );
        config.map(|c| TlsConfiguration::Rustls(Arc::new(c)))
    };
    let proxy = proxy(route, &wire, &address, port, &tls)?;
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
    let will = m.will_topic.as_str();
    if will.contains(['+', '#']) {
        return Err("+ and # are for subscribing; a last will goes to one topic".into());
    }
    let will_extras = !m.will_content_type.is_empty() || m.will_expiry_secs > 0;
    if !m.v5 && !will.is_empty() && will_extras {
        return Err("the will's content type and expiry need MQTT 5.0".into());
    }
    let transport = match wire {
        Wire::Tcp => Transport::Tcp,
        Wire::Tls => Transport::tls_with_config(tls(&req.url)?),
        Wire::Ws => Transport::Ws,
        Wire::Wss => Transport::wss_with_config(tls(&req.url)?),
    };
    let keep_alive = Duration::from_secs(m.keep_alive_secs.into());
    let max = req.settings.max_message(MAX_PACKET);
    // As for an HTTP request: its own timeout, else the network settings'. rumqttc counts
    // whole seconds.
    let wait = match req.settings.timeout_ms {
        0 => net.timeout_secs.max(1),
        ms => ms.div_ceil(1000),
    };
    if m.v5 {
        let mut o = v5::MqttOptions::new(id, address, port);
        o.set_keep_alive(keep_alive)
            .set_clean_start(m.clean_session)
            .set_max_packet_size(Some(u32::try_from(max).unwrap_or(u32::MAX)))
            .set_connection_timeout(wait)
            .set_transport(transport);
        if let Some(p) = proxy {
            o.set_proxy(p);
        }
        if !will.is_empty() {
            let (payload, qos) = (m.will_payload.clone(), qos5(m.will_qos));
            let properties = will_extras.then(|| p5::LastWillProperties {
                delay_interval: None,
                payload_format_indicator: None,
                message_expiry_interval: (m.will_expiry_secs > 0).then_some(m.will_expiry_secs),
                content_type: (!m.will_content_type.is_empty())
                    .then(|| m.will_content_type.clone()),
                response_topic: None,
                correlation_data: None,
                user_properties: Vec::new(),
            });
            let will = p5::LastWill::new(will, payload, qos, m.will_retain, properties);
            o.set_last_will(will);
        }
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
        .set_max_packet_size(max, max)
        .set_transport(transport);
    if let Some(p) = proxy {
        o.set_proxy(p);
    }
    if !will.is_empty() {
        let (payload, qos) = (m.will_payload.clone(), qos3(m.will_qos));
        o.set_last_will(rumqttc::LastWill::new(will, payload, qos, m.will_retain));
    }
    if let Some((user, password)) = login {
        o.set_credentials(user, password);
    }
    let (c, mut e) = rumqttc::AsyncClient::new(o, 16);
    let mut n = rumqttc::NetworkOptions::new();
    n.set_connection_timeout(wait);
    e.set_network_options(n);
    Ok((Client::V3(c), Events::V3(e)))
}

/// The proxy the settings pick for the broker, asked as for an https URL when the broker
/// speaks TLS, else as for http. Tunnelled with CONNECT, over TLS to an https:// proxy.
fn proxy(
    route: &Route,
    wire: &Wire,
    address: &str,
    port: u16,
    tls: &dyn Fn(&str) -> Result<TlsConfiguration, String>,
) -> Result<Option<Proxy>, String> {
    let (scheme, host) = match wire {
        Wire::Tcp => ("http", Some(address.to_owned())),
        Wire::Tls => ("https", Some(address.to_owned())),
        Wire::Ws | Wire::Wss => {
            let url = reqwest::Url::parse(address).ok();
            let host = url.as_ref().and_then(|u| u.host_str().map(str::to_owned));
            (if *wire == Wire::Ws { "http" } else { "https" }, host)
        }
    };
    let probe = host.and_then(|h| reqwest::Url::parse(&format!("{scheme}://{h}:{port}")).ok());
    let Some((url, login)) = probe.and_then(|p| route.proxy_for(&p)) else {
        return Ok(None);
    };
    let refused = || format!("MQTT can only go through an http:// or https:// proxy, not {url}");
    let (Some(addr), Some(proxy_port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(refused());
    };
    let ty = match url.scheme() {
        "http" => ProxyType::Http,
        "https" => ProxyType::Https(tls(url.as_str())?),
        _ => return Err(refused()),
    };
    Ok(Some(Proxy {
        ty,
        auth: match login {
            Some((username, password)) => ProxyAuth::Basic { username, password },
            None => ProxyAuth::None,
        },
        addr: addr.to_owned(),
        port: proxy_port,
    }))
}

/// As for https: the system's trust store plus the CA file, the client certificate, and
/// `insecure` for "accept any certificate". Also how an HTTP request with its own TLS
/// versions or cipher suites connects (`net::custom_tls`), hence `provider` and `versions`.
pub(crate) fn tls_config(
    net: &Network,
    (cert, password): (&str, &str),
    insecure: bool,
    provider: rustls::crypto::CryptoProvider,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(provider);
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .map_err(|e| e.to_string())?
        .dangerous();
    let builder = if insecure {
        builder.with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
    } else {
        let ca_file = net.ca_file.trim();
        let mut extra = Vec::new();
        if !ca_file.is_empty() {
            let pem = std::fs::read(ca_file).map_err(|e| format!("CA file {ca_file}: {e}"))?;
            for cert in CertificateDer::pem_slice_iter(&pem) {
                extra.push(cert.map_err(|e| format!("CA file {ca_file}: {e}"))?);
            }
        }
        let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider)
            .map_err(|e| format!("system certificates: {e}"))?;
        builder.with_custom_certificate_verifier(Arc::new(verifier))
    };
    let cert = cert.trim();
    if cert.is_empty() {
        return Ok(builder.with_no_client_auth());
    }
    let pem = crate::net::identity_pem(std::path::Path::new(cert), password)?;
    let bad = |e: String| format!("client certificate {cert}: {e}");
    let chain = (CertificateDer::pem_slice_iter(&pem))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| bad(e.to_string()))?;
    if chain.is_empty() {
        return Err(bad("no certificate in it".into()));
    }
    let key = PrivateKeyDer::from_pem_slice(&pem).map_err(|e| bad(e.to_string()))?;
    (builder.with_client_auth_cert(chain, key)).map_err(|e| bad(e.to_string()))
}

/// "Accept any certificate", as reqwest has it: the certificate isn't checked, the
/// handshake's signatures still are.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.0.signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.0.signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
pub mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use base64::Engine as _;
    use bytes::BytesMut;
    use rumqttc::{ConnAck, ConnectReturnCode, Packet, QoS, SubAck, UnsubAck};

    use super::*;
    use crate::model::{Mqtt, Topic};

    /// How the fake broker reads and writes MQTT bytes: straight on TCP, or in binary
    /// WebSocket messages.
    pub(crate) trait Pipe {
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

    impl Pipe for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
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

    /// Serves one connection over TLS. A failed handshake is the client's to report.
    pub(crate) fn listen_tls(config: Arc<rustls::ServerConfig>, serve: fn(&mut dyn Pipe)) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut conn = rustls::ServerConnection::new(config).unwrap();
            while conn.is_handshaking() {
                if conn.complete_io(&mut s).is_err() {
                    return;
                }
            }
            serve(&mut rustls::StreamOwned::new(conn, s))
        });
        port
    }

    /// A broker with a self-signed certificate that wants a client certificate signed by
    /// `client_ca`.
    pub(crate) fn tls_broker(client_ca: &CertificateDer<'static>) -> Arc<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(client_ca.clone()).unwrap();
        let clients = rustls::server::WebPkiClientVerifier::builder_with_provider(
            roots.into(),
            provider.clone(),
        )
        .build()
        .unwrap();
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivateKeyDer::try_from(own.signing_key.serialize_der()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(clients)
            .with_single_cert(vec![own.cert.der().clone()], key)
            .unwrap();
        Arc::new(config)
    }

    /// An HTTP proxy that lets u (password p@ss) CONNECT to broker.test:1883 and then is
    /// that broker.
    fn proxy_to_broker(serve: fn(&mut dyn Pipe)) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || tunnel(listener.accept().unwrap().0, serve));
        port
    }

    /// `proxy_to_broker` spoken over TLS, as an https:// proxy is, with a self-signed
    /// certificate for localhost (returned, for the client's CA file).
    fn tls_proxy_to_broker(serve: fn(&mut dyn Pipe)) -> (u16, String) {
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivateKeyDer::try_from(own.signing_key.serialize_der()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![own.cert.der().clone()], key)
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            while conn.is_handshaking() {
                if conn.complete_io(&mut s).is_err() {
                    return;
                }
            }
            tunnel(rustls::StreamOwned::new(conn, s), serve)
        });
        (port, own.cert.pem())
    }

    /// The proxy's side once a client is in: CONNECT (checked), then the broker.
    fn tunnel<S: Read + Write + Pipe>(mut s: S, serve: fn(&mut dyn Pipe)) {
        // Byte by byte: what follows the request is MQTT.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            s.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("u:p@ss");
        assert!(head.starts_with("CONNECT broker.test:1883 "), "{head}");
        assert!(head.contains(&format!("Basic {auth}\r\n")), "{head}");
        s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .unwrap();
        s.flush().unwrap();
        serve(&mut s)
    }

    /// Just enough MQTT 3.1.1 broker for client "tester" signing in as u/p, with "offline"
    /// as its retained QoS 1 will on status/tester: answers
    /// subscriptions (refusing "denied") and unsubscriptions, says hello on a/1 after the
    /// first subscription, echoes what's published on a/echo and stops at DISCONNECT.
    pub fn broker() -> u16 {
        listen(false, serve_v3)
    }

    /// `broker` in MQTT 5, echoing user properties back.
    pub fn broker_v5() -> u16 {
        listen(false, serve_v5)
    }

    pub(crate) fn serve_v3(s: &mut dyn Pipe) {
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
        let will = rumqttc::LastWill::new("status/tester", "offline", QoS::AtLeastOnce, true);
        assert_eq!(connect.last_will, Some(will));
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
        let Packet::Connect(connect, will, login) = next(s) else {
            panic!("CONNECT first");
        };
        let login = login.expect("credentials from Basic auth");
        assert_eq!(
            (login.username.as_str(), login.password.as_str()),
            ("u", "p")
        );
        assert_eq!(connect.client_id, "tester");
        let properties = p5::LastWillProperties {
            delay_interval: None,
            payload_format_indicator: None,
            message_expiry_interval: Some(300),
            content_type: Some("text/plain".into()),
            response_topic: None,
            correlation_data: None,
            user_properties: Vec::new(),
        };
        let offline =
            p5::LastWill::new("status/tester", "offline", qos5(1), true, Some(properties));
        assert_eq!(will, Some(offline));
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
                    let echo = p5::Publish::new("a/echo", qos5(0), echo, p.properties);
                    send(s, Packet::Publish(echo));
                }
                Packet::Disconnect(_) => break,
                _ => {}
            }
        }
    }

    pub(crate) fn request(url: String, v5: bool) -> Request {
        Request {
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
                will_topic: "status/tester".into(),
                will_payload: "offline".into(),
                will_qos: 1,
                will_retain: true,
                // 3.1.1 has nowhere to put them.
                will_content_type: if v5 {
                    "text/plain".into()
                } else {
                    String::new()
                },
                will_expiry_secs: if v5 { 300 } else { 0 },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Subscribes to a/# and "denied"; answers the hello with a publish (with a user
    /// property, content type, response topic and expiry in MQTT 5); once the echo is back, swaps "denied" for b/# while
    /// connected; hangs up once b/# is subscribed.
    pub(crate) fn exchange(req: Request, net: Network) -> Vec<Event> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let route = rt
            .block_on(crate::net::build_client(net.clone()))
            .unwrap()
            .route;
        let v5 = req.mqtt.v5;
        let properties = match v5 {
            true => vec![("unit".to_owned(), "°C".to_owned())],
            false => Vec::new(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = std::sync::Mutex::new(Some(tx));
        let events = std::sync::Mutex::new(Vec::new());
        let command = |c: Command| {
            let tx = tx.lock().unwrap();
            tx.as_ref().unwrap().send(c).ok().unwrap();
        };
        rt.block_on(session(req, net, route, rx, |e| {
            match &e {
                Event::In(t) if t == "[a/1] hello" => command(Command::Publish(Publish {
                    topic: "cmd".into(),
                    qos: 0,
                    retain: false,
                    payload: "ping".into(),
                    properties: properties.clone(),
                    content_type: if v5 {
                        "text/plain".into()
                    } else {
                        String::new()
                    },
                    response_topic: if v5 { "reply/1".into() } else { String::new() },
                    expiry_secs: if v5 { 60 } else { 0 },
                })),
                Event::In(t) if t.starts_with("[a/echo") => {
                    command(Command::Topics(vec![("a/#".into(), 1), ("b/#".into(), 0)]))
                }
                Event::Info(t) if t.contains("b/#") => drop(tx.lock().unwrap().take()),
                _ => {}
            }
            events.lock().unwrap().push(e);
        }));
        events.into_inner().unwrap()
    }

    /// MQTT 5's refusal says why, and its messages carry their properties both ways.
    pub(crate) fn expected(v5: bool) -> Vec<Event> {
        let (refused, unit) = match v5 {
            true => (
                "refused (NotAuthorized)",
                " · content-type=text/plain · response-topic=reply/1 · expiry=60s · unit=°C",
            ),
            false => ("refused", ""),
        };
        vec![
            Event::Open("connected".into()),
            Event::Info(format!("subscribed: a/# (QoS 1), denied {refused}")),
            Event::In("[a/1] hello".into()),
            Event::Out(format!("[cmd{unit}] ping")),
            Event::In(format!("[a/echo{unit}] echo cmd ping")),
            // a/# is unchanged, so it's left alone.
            Event::Info("unsubscribed: denied".into()),
            Event::Info("subscribed: b/# (QoS 0)".into()),
            Event::Closed("disconnected".into()),
        ]
    }

    #[test]
    fn subscribes_publishes_follows_topic_changes_and_says_goodbye() {
        let url = format!("mqtt://127.0.0.1:{}", broker());
        assert_eq!(
            exchange(request(url.clone(), false), Network::default()),
            expected(false)
        );
        // Dropped silently, the will would go out without what it was meant to say.
        let mut req = request(url, false);
        req.mqtt.will_expiry_secs = 300;
        let events = exchange(req, Network::default());
        assert!(
            matches!(events.as_slice(), [Event::Error(e)] if e.contains("MQTT 5")),
            "{events:?}"
        );
    }

    #[test]
    fn mqtt_5_says_why_a_subscription_is_refused_and_carries_user_properties() {
        let url = format!("mqtt://127.0.0.1:{}", listen(false, serve_v5));
        assert_eq!(
            exchange(request(url, true), Network::default()),
            expected(true)
        );
    }

    #[test]
    fn both_versions_ride_websockets() {
        let url = format!("ws://127.0.0.1:{}/mqtt", listen(true, serve_v3));
        assert_eq!(
            exchange(request(url, false), Network::default()),
            expected(false)
        );
        let url = format!("ws://127.0.0.1:{}/mqtt", listen(true, serve_v5));
        assert_eq!(
            exchange(request(url, true), Network::default()),
            expected(true)
        );
    }

    #[test]
    fn mqtts_presents_the_client_certificate_and_can_skip_verifying_the_broker() {
        let client = rcgen::generate_simple_self_signed(vec!["client.test".into()]).unwrap();
        let path = std::env::temp_dir().join(format!("apitool-mqtt-{}.pem", std::process::id()));
        std::fs::write(
            &path,
            client.signing_key.serialize_pem() + &client.cert.pem(),
        )
        .unwrap();
        let net = Network {
            client_cert: path.display().to_string(),
            ..Default::default()
        };
        let broker = tls_broker(client.cert.der());
        let unverified = |net: &Network| {
            let url = format!("mqtts://127.0.0.1:{}", listen_tls(broker.clone(), serve_v3));
            let mut req = request(url, false);
            req.settings.verify_tls = false;
            exchange(req, net.clone())
        };
        assert_eq!(unverified(&net), expected(false));
        let for_host = |host: &str| Network {
            host_certs: vec![crate::net::HostCert {
                host: host.into(),
                cert: path.display().to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(unverified(&for_host("127.0.0.1")), expected(false));
        let events = unverified(&for_host("broker.corp.test"));
        assert!(matches!(events.as_slice(), [Event::Error(_)]), "{events:?}");
        // The broker turns away a client without a certificate.
        let events = unverified(&Network::default());
        assert!(matches!(events.as_slice(), [Event::Error(_)]), "{events:?}");
        // Verified, the self-signed broker is refused.
        let url = format!("mqtts://127.0.0.1:{}", listen_tls(broker.clone(), serve_v3));
        let events = exchange(request(url, false), net);
        assert!(matches!(events.as_slice(), [Event::Error(_)]), "{events:?}");
        std::fs::remove_file(&path).unwrap();
    }

    /// Here rather than in net.rs for the TLS server that checks client certificates.
    #[test]
    fn https_presents_the_certificate_chosen_for_its_host() {
        let client = rcgen::generate_simple_self_signed(vec!["client.test".into()]).unwrap();
        let path = std::env::temp_dir().join(format!("apitool-https-{}.pem", std::process::id()));
        std::fs::write(
            &path,
            client.signing_key.serialize_pem() + &client.cert.pem(),
        )
        .unwrap();
        let pem = path.display().to_string();
        let server = tls_broker(client.cert.der());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let get = |net: Network| {
            let serve: fn(&mut dyn Pipe) = |s| {
                s.fill(&mut BytesMut::new());
                s.put(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            };
            let url = format!("https://127.0.0.1:{}/", listen_tls(server.clone(), serve));
            let settings = crate::model::Settings {
                verify_tls: false,
                ..Default::default()
            };
            rt.block_on(async {
                let clients = crate::net::build_client(net).await?;
                let resp = clients
                    .for_settings(&settings, &url)?
                    .get(&url)
                    .send()
                    .await;
                resp.map(|r| r.status().as_u16()).map_err(|e| e.to_string())
            })
        };
        let host = |host: &str, cert: &str| Network {
            host_certs: vec![crate::net::HostCert {
                host: host.into(),
                cert: cert.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(get(host("127.0.0.1", &pem)), Ok(200));
        assert_eq!(get(host("127.0.0.1:443", &pem)).ok(), None);
        // Another host's certificate stays with that host.
        assert_eq!(get(host("api.corp.test", &pem)).ok(), None);
        // A matching entry without a certificate withholds the default one.
        let withheld = Network {
            client_cert: pem.clone(),
            ..host("127.0.0.1", "")
        };
        assert_eq!(get(withheld).ok(), None);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_proxy_settings_carry_the_connection() {
        // broker.test doesn't resolve: only the proxy can reach it.
        let via = |serve, v5| {
            let net = Network {
                proxy: crate::net::ProxyMode::Manual,
                proxy_url: format!("http://u:p%40ss@127.0.0.1:{}", proxy_to_broker(serve)),
                ..Default::default()
            };
            exchange(request("mqtt://broker.test:1883".into(), v5), net)
        };
        assert_eq!(via(serve_v3, false), expected(false));
        assert_eq!(via(serve_v5, true), expected(true));

        // An https:// proxy is reached over TLS, its certificate checked against the CA file
        // like any server's, then tunnelled as before.
        let via_tls = |serve, v5| {
            let (port, cert) = tls_proxy_to_broker(serve);
            let ca = std::env::temp_dir().join(format!("apitool-mqtt-proxy-{port}.pem"));
            std::fs::write(&ca, cert).unwrap();
            let net = Network {
                proxy: crate::net::ProxyMode::Manual,
                proxy_url: format!("https://u:p%40ss@localhost:{port}"),
                ca_file: ca.display().to_string(),
                ..Default::default()
            };
            let events = exchange(request("mqtt://broker.test:1883".into(), v5), net);
            std::fs::remove_file(&ca).unwrap();
            events
        };
        assert_eq!(via_tls(serve_v3, false), expected(false));
        assert_eq!(via_tls(serve_v5, true), expected(true));
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
            connect(&req, &Network::default(), &Route::Direct)
                .err()
                .unwrap()
        };
        assert!(bad("http://x", Auth::None).contains("mqtt://"));
        assert!(bad("mqtt://x:port", Auth::None).contains("port"));
        let bearer = Auth::Bearer { token: "t".into() };
        assert!(bad("mqtt://x", bearer).contains("Basic"));
        let will = Request {
            url: "mqtt://x".into(),
            mqtt: Mqtt {
                will_topic: "status/#".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = connect(&will, &Network::default(), &Route::Direct).err();
        assert!(err.unwrap().contains("last will"));
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

    /// A broker that drops the connection is reconnected to as the settings say, the count
    /// starting over once one opens, and the topics subscribed to again: with a clean
    /// session it forgot them, and messages for them would never come.
    #[test]
    fn a_dropped_connection_reconnects_and_subscribes_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = std::thread::spawn(move || {
            let mut subscribed = Vec::new();
            for (n, s) in listener.incoming().take(3).enumerate() {
                let mut s = s.unwrap();
                // A client that never subscribes again fails the test instead of hanging it.
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = BytesMut::new();
                let mut next = |s: &mut TcpStream| loop {
                    match Packet::read(&mut buf, MAX_PACKET) {
                        Ok(p) => return p,
                        Err(rumqttc::Error::InsufficientBytes(_)) => s.fill(&mut buf),
                        Err(e) => panic!("{e:?}"),
                    }
                };
                let send = |s: &mut TcpStream, p: Packet| {
                    let mut out = BytesMut::new();
                    p.write(&mut out, MAX_PACKET).unwrap();
                    s.put(&out);
                };
                let Packet::Connect(_) = next(&mut s) else {
                    panic!("CONNECT first");
                };
                let ok = ConnAck::new(ConnectReturnCode::Success, false);
                send(&mut s, Packet::ConnAck(ok));
                let Packet::Subscribe(sub) = next(&mut s) else {
                    panic!("SUBSCRIBE next");
                };
                subscribed.push(
                    (sub.filters.iter())
                        .map(|f| f.path.clone())
                        .collect::<Vec<_>>(),
                );
                let codes = (sub.filters.iter()).map(|f| SubscribeReasonCode::Success(f.qos));
                send(
                    &mut s,
                    Packet::SubAck(SubAck::new(sub.pkid, codes.collect())),
                );
                // The first two drop without a goodbye, once the SUBACK is read: dropped
                // with it, rumqttc reports the drop first.
                if n < 2 {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let hello = rumqttc::Publish::new("a/1", QoS::AtMostOnce, "hello");
                send(&mut s, Packet::Publish(hello));
                loop {
                    match next(&mut s) {
                        Packet::Disconnect => break,
                        // Subscribing twice would show here.
                        Packet::Subscribe(sub) => {
                            subscribed.push(vec![sub.filters[0].path.clone()])
                        }
                        _ => {}
                    }
                }
            }
            subscribed
        });
        let mut req = request(format!("mqtt://127.0.0.1:{port}"), false);
        req.mqtt.topics.truncate(1);
        (
            req.settings.reconnect_attempts,
            req.settings.reconnect_interval_ms,
        ) = (1, 100);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let tx = std::sync::Mutex::new(Some(tx));
        let events = std::sync::Mutex::new(Vec::new());
        rt.block_on(session(req, Network::default(), Route::Direct, rx, |e| {
            if matches!(e, Event::In(_)) {
                drop(tx.lock().unwrap().take());
            }
            events.lock().unwrap().push(e);
        }));
        let events = events.into_inner().unwrap();
        let subscribed = |e: &Event| *e == Event::Info("subscribed: a/# (QoS 1)".into());
        assert!(
            matches!(&events[..], [
                Event::Open(_),
                s1,
                Event::Info(again1),
                Event::Open(_),
                s2,
                Event::Info(again2),
                Event::Open(_),
                s3,
                Event::In(hello),
                Event::Closed(end),
            ] if [s1, s2, s3].into_iter().all(subscribed)
                && [again1, again2].iter().all(|t| t.ends_with("reconnecting in 0.1 s (1 of 1)"))
                && hello == "[a/1] hello" && end == "disconnected"),
            "{events:?}"
        );
        assert_eq!(broker.join().unwrap(), [["a/#"], ["a/#"], ["a/#"]]);
    }

    /// Connecting waits as long as an HTTP request would unless the request says; the
    /// largest message, both ways, is 1 MiB unless it says otherwise.
    #[test]
    fn the_settings_tab_sets_the_connect_timeout_and_largest_message() {
        let net = Network {
            timeout_secs: 7,
            ..Default::default()
        };
        let options = |req: &Request| match connect(req, &net, &Route::Direct).unwrap().1 {
            Events::V3(e) => (
                e.network_options.connection_timeout(),
                e.mqtt_options.max_packet_size(),
            ),
            Events::V5(e) => (
                e.options.connection_timeout(),
                e.options.max_packet_size().unwrap() as usize,
            ),
        };
        for v5 in [false, true] {
            let mut req = request("mqtt://127.0.0.1:1".into(), v5);
            assert_eq!(options(&req), (7, MAX_PACKET), "v5: {v5}");
            (req.settings.timeout_ms, req.settings.max_message_mb) = (1500, 4);
            assert_eq!(options(&req), (2, 4 << 20), "v5: {v5}");
        }
    }
}
