//! MQTT 3.1.1 sessions: connect to the broker, subscribe to the request's topics, publish
//! what the user sends, and show what arrives.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{
    AsyncClient, ConnectionError, Incoming, MqttOptions, QoS, SubscribeFilter, SubscribeReasonCode,
    TlsConfiguration, Transport,
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

/// `req` is resolved. `ca_file` is the network settings' CA, trusted on top of the
/// system's for mqtts://. Dropping the sender of `outgoing` disconnects.
pub async fn session(
    req: Request,
    ca_file: String,
    mut outgoing: mpsc::UnboundedReceiver<Publish>,
    emit: impl Fn(Event),
) {
    let options = match options(&req, &ca_file) {
        Ok(o) => o,
        Err(e) => return emit(Event::Error(e)),
    };
    let (client, mut events) = AsyncClient::new(options, 16);
    let topics: Vec<SubscribeFilter> = req
        .mqtt
        .topics
        .iter()
        .map(|t| SubscribeFilter::new(t.filter.clone(), qos(t.qos)))
        .collect();
    let names: Vec<String> = topics.iter().map(|t| t.path.clone()).collect();
    // Queued: it goes out once the broker accepts the connection.
    if !topics.is_empty()
        && let Err(e) = client.subscribe_many(topics).await
    {
        return emit(Event::Error(e.to_string()));
    }
    let send = async {
        while let Some(p) = outgoing.recv().await {
            let shown = format!("[{}] {}", p.topic, p.payload);
            if let Err(e) = client
                .publish(p.topic, qos(p.qos), p.retain, p.payload)
                .await
            {
                return emit(Event::Error(e.to_string()));
            }
            emit(Event::Out(shown));
        }
        // The receiving side reports the goodbye once it has gone out.
        let _ = client.disconnect().await;
        std::future::pending().await
    };
    let receive = async {
        loop {
            match events.poll().await {
                Ok(rumqttc::Event::Incoming(Incoming::ConnAck(_))) => {
                    emit(Event::Open("connected".into()))
                }
                Ok(rumqttc::Event::Incoming(Incoming::Publish(p))) => emit(Event::In(format!(
                    "[{}] {}",
                    p.topic,
                    String::from_utf8_lossy(&p.payload)
                ))),
                Ok(rumqttc::Event::Incoming(Incoming::SubAck(ack))) => {
                    emit(Event::Info(subscribed(&names, &ack.return_codes)))
                }
                Ok(rumqttc::Event::Outgoing(rumqttc::Outgoing::Disconnect)) => {
                    return emit(Event::Closed("disconnected".into()));
                }
                Ok(_) => {}
                // Polling again would reconnect; someone testing a broker wants to see why
                // it failed instead.
                Err(e) => return emit(Event::Error(error(&e))),
            }
        }
    };
    tokio::select! {
        () = send => {}
        () = receive => {}
    }
}

fn qos(n: u8) -> QoS {
    match n {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    }
}

/// "subscribed: a/# (QoS 1), admin/# refused"
fn subscribed(names: &[String], codes: &[SubscribeReasonCode]) -> String {
    let each: Vec<String> = names
        .iter()
        .zip(codes)
        .map(|(name, code)| match code {
            SubscribeReasonCode::Success(q) => format!("{name} (QoS {})", *q as u8),
            SubscribeReasonCode::Failure => format!("{name} refused"),
        })
        .collect();
    format!("subscribed: {}", each.join(", "))
}

fn error(e: &ConnectionError) -> String {
    match e {
        ConnectionError::ConnectionRefused(code) => format!("broker refused: {code:?}"),
        e => e.to_string(),
    }
}

/// From `mqtt://host[:1883]` or `mqtts://host[:8883]` (`tcp://`, `ssl://` too).
fn options(req: &Request, ca_file: &str) -> Result<MqttOptions, String> {
    let url = req.url.trim();
    let (scheme, rest) = url.split_once("://").unwrap_or(("mqtt", url));
    let tls = match scheme.to_lowercase().as_str() {
        "mqtt" | "tcp" => false,
        "mqtts" | "ssl" => true,
        // ponytail: MQTT over WebSocket needs rumqttc's websocket feature; add it when a
        // broker is only reachable that way.
        other => return Err(format!("{other}:// isn't MQTT; use mqtt:// or mqtts://")),
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().map_err(|_| format!("bad port \"{port}\""))?;
            (host, port)
        }
        None => (authority, if tls { 8883 } else { 1883 }),
    };
    if host.is_empty() {
        return Err("the URL needs a broker, e.g. mqtt://localhost:1883".into());
    }
    let m = &req.mqtt;
    let id = match m.client_id.trim() {
        "" => {
            let mut b = [0u8; 4];
            getrandom::fill(&mut b).map_err(|e| e.to_string())?;
            format!("apitool-{:08x}", u32::from_le_bytes(b))
        }
        id => id.to_owned(),
    };
    let mut o = MqttOptions::new(id, host, port);
    o.set_keep_alive(Duration::from_secs(m.keep_alive_secs.into()))
        .set_clean_session(m.clean_session)
        .set_max_packet_size(MAX_PACKET, MAX_PACKET);
    match &req.auth {
        Auth::Basic { username, password } => {
            o.set_credentials(username, password);
        }
        Auth::None | Auth::Inherit => {}
        _ => return Err("MQTT signs in with a username and password: use Basic auth".into()),
    }
    if tls {
        o.set_transport(Transport::tls_with_config(tls_config(ca_file)?));
    }
    Ok(o)
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
    use rumqttc::{ConnAck, ConnectReturnCode, Packet, SubAck};

    use super::*;
    use crate::model::{Mqtt, Topic};

    /// Just enough broker for client "tester" signing in as u/p: answers the
    /// subscription (refusing "denied"), says hello on a/1, echoes what's published on
    /// a/echo and stops at DISCONNECT.
    pub fn broker() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = BytesMut::new();
            let mut next = |s: &mut TcpStream| loop {
                match Packet::read(&mut buf, MAX_PACKET) {
                    Ok(p) => return p,
                    Err(rumqttc::Error::InsufficientBytes(_)) => {
                        let mut chunk = [0; 4096];
                        let n = s.read(&mut chunk).unwrap();
                        assert!(n > 0, "the client hung up without DISCONNECT");
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) => panic!("{e:?}"),
                }
            };
            let send = |s: &mut TcpStream, p: Packet| {
                let mut out = BytesMut::new();
                p.write(&mut out, MAX_PACKET).unwrap();
                s.write_all(&out).unwrap();
            };
            let Packet::Connect(connect) = next(&mut s) else {
                panic!("CONNECT first");
            };
            let login = connect.login.expect("credentials from Basic auth");
            assert_eq!(
                (login.username.as_str(), login.password.as_str()),
                ("u", "p")
            );
            assert_eq!(connect.client_id, "tester");
            let ack = ConnAck::new(ConnectReturnCode::Success, false);
            send(&mut s, Packet::ConnAck(ack));
            let Packet::Subscribe(sub) = next(&mut s) else {
                panic!("SUBSCRIBE next");
            };
            let codes = sub.filters.iter().map(|f| match f.path.as_str() {
                "denied" => SubscribeReasonCode::Failure,
                _ => SubscribeReasonCode::Success(f.qos),
            });
            send(
                &mut s,
                Packet::SubAck(SubAck::new(sub.pkid, codes.collect())),
            );
            let hello = rumqttc::Publish::new("a/1", QoS::AtMostOnce, "hello");
            send(&mut s, Packet::Publish(hello));
            loop {
                match next(&mut s) {
                    Packet::Publish(p) => {
                        let text = String::from_utf8_lossy(&p.payload);
                        let echo = format!("echo {} {text}", p.topic);
                        let echo = rumqttc::Publish::new("a/echo", QoS::AtMostOnce, echo);
                        send(&mut s, Packet::Publish(echo));
                    }
                    Packet::Disconnect => break,
                    _ => {}
                }
            }
        });
        port
    }

    #[test]
    fn subscribes_publishes_and_says_goodbye() {
        let req = Request {
            method: "MQTT".into(),
            url: format!("mqtt://127.0.0.1:{}", broker()),
            auth: Auth::Basic {
                username: "u".into(),
                password: "p".into(),
            },
            mqtt: Mqtt {
                client_id: "tester".into(),
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
        rt.block_on(session(req, String::new(), rx, |e| {
            // Answer the hello, then hang up once the echo is back.
            match &e {
                Event::In(t) if t == "[a/1] hello" => {
                    let p = Publish {
                        topic: "cmd".into(),
                        qos: 0,
                        retain: false,
                        payload: "ping".into(),
                    };
                    tx.lock().unwrap().as_ref().unwrap().send(p).ok().unwrap();
                }
                Event::In(t) if t.starts_with("[a/echo]") => drop(tx.lock().unwrap().take()),
                _ => {}
            }
            events.lock().unwrap().push(e);
        }));
        assert_eq!(
            events.into_inner().unwrap(),
            [
                Event::Open("connected".into()),
                Event::Info("subscribed: a/# (QoS 1), denied refused".into()),
                Event::In("[a/1] hello".into()),
                Event::Out("[cmd] ping".into()),
                Event::In("[a/echo] echo cmd ping".into()),
                Event::Closed("disconnected".into()),
            ]
        );
    }

    #[test]
    fn a_bad_url_or_auth_says_so_before_connecting() {
        let bad = |url: &str, auth: Auth| {
            let req = Request {
                url: url.into(),
                auth,
                ..Default::default()
            };
            options(&req, "").err().unwrap()
        };
        assert!(bad("ws://x", Auth::None).contains("mqtt://"));
        assert!(bad("mqtt://x:port", Auth::None).contains("port"));
        let bearer = Auth::Bearer { token: "t".into() };
        assert!(bad("mqtt://x", bearer).contains("Basic"));
        // Without a scheme or port it's plain MQTT on 1883.
        let req = Request {
            url: "broker.test".into(),
            ..Default::default()
        };
        assert_eq!(
            options(&req, "").unwrap().broker_address(),
            ("broker.test".into(), 1883)
        );
    }
}
