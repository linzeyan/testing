//! The capture proxy, as Proxyman and Charles have: what apps send through it is forwarded
//! with the app's own network settings and kept as flows. HTTPS passes as a tunnel, seen
//! only as its host and byte counts, unless decrypting: then its TLS ends here, with a
//! certificate for its host from apitool's CA, and its requests are flows as any other.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, BodyStream, Full, StreamBody};
use hyper::body::{Body as _, Frame, Incoming};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::http::error_chain;
use crate::net::{Clients, Route};

/// Past these the oldest flows go and a body is cut: the VDI has little RAM to spare.
pub const MAX_FLOWS: usize = 1000;
pub const MAX_BODY: usize = 256 * 1024;

/// Added to what the proxy forwards; seeing it come back means the proxy is its own
/// upstream, which would loop until something ran out.
const VIA: &str = "1.1 apitool-capture";

/// Headers about one hop, not the request: the proxy's own connection to each side has its
/// own. Host comes from the URL.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub headers: Vec<(String, String)>,
    /// The first `MAX_BODY` bytes.
    pub body: Vec<u8>,
    /// All of it, as it went through.
    pub size: u64,
}

impl Message {
    fn take(&mut self, chunk: &[u8]) {
        let room = MAX_BODY.saturating_sub(self.body.len());
        self.body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        self.size += chunk.len() as u64;
    }

    /// Only the first `MAX_BODY` bytes were kept.
    pub fn cut(&self) -> bool {
        self.size > self.body.len() as u64
    }
}

#[derive(Clone, Debug)]
pub struct Flow {
    pub id: u64,
    pub at: SystemTime,
    pub method: String,
    /// Absolute; a tunnel's is its `host:port`.
    pub url: String,
    pub request: Message,
    /// None until it answers, and for what never does.
    pub status: Option<u16>,
    pub response: Message,
    /// None while it goes on.
    pub elapsed: Option<Duration>,
    pub error: Option<String>,
}

impl Flow {
    pub fn tunnel(&self) -> bool {
        self.method == "CONNECT"
    }
}

/// The flows so far, oldest first, and how the window is told to redraw.
#[derive(Clone)]
pub struct Flows {
    list: Arc<Mutex<Vec<Flow>>>,
    changed: Arc<dyn Fn() + Send + Sync>,
}

impl Flows {
    pub fn new(changed: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            list: Default::default(),
            changed: Arc::new(changed),
        }
    }

    /// Read under the lock: a copy each frame would be every kept body.
    pub fn with<R>(&self, f: impl FnOnce(&[Flow]) -> R) -> R {
        f(&self.list.lock().unwrap())
    }

    pub fn clear(&self) {
        self.list.lock().unwrap().clear();
    }

    fn start(&self, method: &str, url: &str, headers: &HeaderMap) -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut list = self.list.lock().unwrap();
        if list.len() >= MAX_FLOWS {
            let over = list.len() + 1 - MAX_FLOWS;
            list.drain(..over);
        }
        list.push(Flow {
            id,
            at: SystemTime::now(),
            method: method.to_owned(),
            url: url.to_owned(),
            request: Message {
                headers: pairs(headers),
                ..Default::default()
            },
            status: None,
            response: Message::default(),
            elapsed: None,
            error: None,
        });
        drop(list);
        (self.changed)();
        id
    }

    /// Gone already when cleared meanwhile.
    fn update(&self, id: u64, f: impl FnOnce(&mut Flow)) {
        let mut list = self.list.lock().unwrap();
        // Still going on: near the end.
        if let Some(flow) = list.iter_mut().rev().find(|flow| flow.id == id) {
            f(flow);
        }
        drop(list);
        (self.changed)();
    }
}

fn pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    let text = |v: &HeaderValue| String::from_utf8_lossy(v.as_bytes()).into_owned();
    (headers.iter())
        .map(|(k, v)| (k.as_str().to_owned(), text(v)))
        .collect()
}

type Body = UnsyncBoxBody<Bytes, std::io::Error>;

fn text(status: StatusCode, message: &str) -> Response<Body> {
    let body = Full::new(Bytes::from(message.to_owned())).map_err(|e| match e {});
    let mut res = Response::new(body.boxed_unsync());
    *res.status_mut() = status;
    res.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

/// Serves until dropped, its tunnels with it. `clients` forwards: its proxy, CA file and
/// client certificates.
pub async fn serve(listener: TcpListener, clients: Clients, flows: Flows, decrypt: Decrypt) {
    let own = listener.local_addr().ok();
    // Tunnels watch it: the sender goes when this does.
    let (_running, stopped) = tokio::sync::watch::channel(());
    let mut connections = tokio::task::JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Out of file handles, say: give the open ones time to close.
                log::warn!("capture: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (clients, flows) = (clients.clone(), flows.clone());
        let (stopped, decrypt) = (stopped.clone(), decrypt.clone());
        connections.spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let (clients, flows) = (clients.clone(), flows.clone());
                let (stopped, ca) = (stopped.clone(), decrypt.lock().unwrap().clone());
                async move {
                    let res = match ca {
                        Some(ca) if req.method() == Method::CONNECT => {
                            intercept(req, ca, &clients, &flows, stopped)
                        }
                        _ => handle(req, &clients, &flows, own, stopped).await,
                    };
                    Ok::<_, Infallible>(res)
                }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            if let Err(e) = conn.await {
                log::debug!("capture: {e}");
            }
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    clients: &Clients,
    flows: &Flows,
    own: Option<SocketAddr>,
    stopped: tokio::sync::watch::Receiver<()>,
) -> Response<Body> {
    if req.method() == Method::CONNECT {
        return tunnel(req, &clients.route, flows, own, stopped).await;
    }
    if req.uri().scheme().is_none() {
        let hint = "This is apitool's capture proxy: set it as the HTTP proxy, not as the server.";
        return text(StatusCode::BAD_REQUEST, hint);
    }
    exchange(req, clients, flows).await
}

/// A request with its absolute URL: forwarded, unless it came back around.
async fn exchange(req: Request<Incoming>, clients: &Clients, flows: &Flows) -> Response<Body> {
    let vias = req.headers().get_all(hyper::header::VIA);
    if vias
        .iter()
        .any(|v| v.as_bytes().ends_with(b"apitool-capture"))
    {
        return text(StatusCode::LOOP_DETECTED, LOOP);
    }
    forward(req, clients, flows).await
}

const LOOP: &str = "apitool's capture proxy would be its own upstream: in apitool's network \
    settings, pick another proxy or None";

/// Records into a flow as a body goes through, and when it's dropped (the body ended, or
/// a side hung up), how long the exchange took.
struct Tee {
    flows: Flows,
    id: u64,
    started: Instant,
}

impl Tee {
    fn request(&self, chunk: &Bytes) {
        self.flows.update(self.id, |f| f.request.take(chunk));
    }

    fn response(&self, chunk: reqwest::Result<Bytes>) -> Result<Frame<Bytes>, std::io::Error> {
        match chunk {
            Ok(chunk) => {
                self.flows.update(self.id, |f| f.response.take(&chunk));
                Ok(Frame::data(chunk))
            }
            Err(e) => {
                let e = error_chain(&e);
                self.flows.update(self.id, |f| f.error = Some(e.clone()));
                Err(std::io::Error::other(e))
            }
        }
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        self.flows.update(self.id, |f| {
            f.elapsed.get_or_insert(elapsed);
        });
    }
}

async fn forward(req: Request<Incoming>, clients: &Clients, flows: &Flows) -> Response<Body> {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let url = parts.uri.to_string();
    let id = flows.start(parts.method.as_str(), &url, &parts.headers);
    let fail = |e: String| {
        flows.update(id, |f| {
            (f.error, f.elapsed) = (Some(e.clone()), Some(started.elapsed()));
        });
        text(StatusCode::BAD_GATEWAY, &e)
    };
    let client = match clients.forward_for(&url) {
        Ok(client) => client,
        Err(e) => return fail(e),
    };
    let mut headers = parts.headers;
    for name in HOP_BY_HOP.iter().chain(&["host"]) {
        headers.remove(*name);
    }
    headers.append(hyper::header::VIA, HeaderValue::from_static(VIA));
    let mut send = client.request(parts.method, &url).headers(headers);
    // A GET's empty body stays absent: streamed, it would go out chunked.
    if !body.is_end_stream() {
        let tee = Tee {
            flows: flows.clone(),
            id,
            started,
        };
        let chunks = BodyStream::new(body).filter_map(move |frame| {
            let chunk = match frame {
                Ok(frame) => frame.into_data().ok().map(Ok),
                Err(e) => Some(Err(e)),
            };
            if let Some(Ok(chunk)) = &chunk {
                tee.request(chunk);
            }
            std::future::ready(chunk)
        });
        send = send.body(reqwest::Body::wrap_stream(chunks));
    }
    let res = match send.send().await {
        Ok(res) => res,
        Err(e) => return fail(error_chain(&e)),
    };
    let status = res.status();
    let mut back = Response::builder().status(status);
    for (name, value) in res.headers() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            back = back.header(name, value);
        }
    }
    let shown = pairs(res.headers());
    flows.update(id, |f| {
        (f.status, f.response.headers) = (Some(status.as_u16()), shown);
    });
    let tee = Tee {
        flows: flows.clone(),
        id,
        started,
    };
    let body = StreamBody::new(res.bytes_stream().map(move |chunk| tee.response(chunk)));
    match back.body(body.boxed_unsync()) {
        Ok(res) => res,
        Err(e) => fail(e.to_string()),
    }
}

/// CONNECT: the bytes go both ways untouched, counted when the tunnel closes.
async fn tunnel(
    req: Request<Incoming>,
    route: &Route,
    flows: &Flows,
    own: Option<SocketAddr>,
    mut stopped: tokio::sync::watch::Receiver<()>,
) -> Response<Body> {
    let started = Instant::now();
    let Some(target) = req.uri().authority().map(|a| a.to_string()) else {
        return text(StatusCode::BAD_REQUEST, "CONNECT takes host:port");
    };
    let id = flows.start("CONNECT", &target, req.headers());
    let mut upstream = match connect(route, &target, own).await {
        Ok(stream) => stream,
        Err(e) => {
            flows.update(id, |f| {
                (f.error, f.elapsed) = (Some(e.clone()), Some(started.elapsed()));
            });
            return text(StatusCode::BAD_GATEWAY, &e);
        }
    };
    flows.update(id, |f| f.status = Some(200));
    let flows = flows.clone();
    tokio::spawn(async move {
        let result = match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut app = TokioIo::new(upgraded);
                tokio::select! {
                    copied = tokio::io::copy_bidirectional(&mut app, &mut upstream) => {
                        copied.map_err(|e| e.to_string())
                    }
                    _ = stopped.changed() => Err("capture stopped".to_owned()),
                }
            }
            Err(e) => Err(e.to_string()),
        };
        flows.update(id, |f| {
            match result {
                Ok((up, down)) => (f.request.size, f.response.size) = (up, down),
                Err(e) => f.error = Some(e),
            }
            f.elapsed = Some(started.elapsed());
        });
    });
    Response::new(
        http_body_util::Empty::new()
            .map_err(|e| match e {})
            .boxed_unsync(),
    )
}

/// To `target` (`host:port`) the way the network settings send https there: directly, or
/// through an http:// proxy's own CONNECT.
async fn connect(
    route: &Route,
    target: &str,
    own: Option<SocketAddr>,
) -> Result<TcpStream, String> {
    let probe =
        reqwest::Url::parse(&format!("https://{target}")).map_err(|e| format!("{target}: {e}"))?;
    let Some((proxy, login)) = route.proxy_for(&probe) else {
        return (TcpStream::connect(target).await).map_err(|e| format!("{target}: {e}"));
    };
    let (Some(host), Some(port)) = (proxy.host_str(), proxy.port_or_known_default()) else {
        return Err(format!("proxy {proxy}: no host"));
    };
    if proxy.scheme() != "http" {
        return Err(format!(
            "tunnels go through http:// proxies only, not {proxy}"
        ));
    }
    let at = format!("proxy {host}:{port}");
    let mut s = (TcpStream::connect((host, port)).await).map_err(|e| format!("{at}: {e}"))?;
    if own.is_some_and(|own| s.peer_addr().is_ok_and(|p| p == own)) {
        return Err(LOOP.to_owned());
    }
    let auth = match login {
        Some((user, password)) => {
            let pair =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
            format!("Proxy-Authorization: Basic {pair}\r\n")
        }
        None => String::new(),
    };
    let ask = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n");
    s.write_all(ask.as_bytes())
        .await
        .map_err(|e| format!("{at}: {e}"))?;
    // A byte at a time: what comes after the head is the tunnel's, not this answer's.
    let (mut head, mut byte) = (Vec::new(), [0u8]);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err(format!("{at}: no end to its answer"));
        }
        s.read_exact(&mut byte)
            .await
            .map_err(|e| format!("{at}: {e}"))?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let line = head.lines().next().unwrap_or_default();
    match line.split_whitespace().nth(1) {
        Some("200") => Ok(s),
        _ => Err(format!("{at} refused the tunnel: {line}")),
    }
}

/// Decrypting's CA while it's on; tunnels pass untouched while it's None.
pub type Decrypt = Arc<Mutex<Option<Arc<Ca>>>>;

/// The CA that signs a certificate for each host an app opens HTTPS to. The app must trust
/// it, so it is made once and kept (see `generate` and `load`).
pub struct Ca {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    cert: CertificateDer<'static>,
    pem: String,
    /// One certificate per host, made on its first tunnel.
    hosts: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
}

impl Ca {
    /// A new CA, its certificate and key as PEM, for `load`.
    pub fn generate() -> Result<(String, String), String> {
        let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
        let mut params = ca_params();
        // Ten years: re-trusting a new CA in every app is the chore this saves.
        valid_for(&mut params, 10 * 365);
        let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    pub fn load(cert: &str, key: &str) -> Result<Self, String> {
        let bad = |e: &dyn std::fmt::Display| format!("apitool's capture CA: {e}");
        let key = rcgen::KeyPair::from_pem(key).map_err(|e| bad(&e))?;
        let der = CertificateDer::from_pem_slice(cert.as_bytes()).map_err(|e| bad(&e))?;
        Ok(Self {
            // Its name, key usages and key are all a signature needs of it: the same
            // params as `generate` made it with.
            issuer: rcgen::Issuer::new(ca_params(), key),
            cert: der,
            pem: cert.to_owned(),
            hosts: Mutex::default(),
        })
    }

    /// The certificate to export, for apps to trust.
    pub fn pem(&self) -> &str {
        &self.pem
    }

    fn config_for(&self, host: &str) -> Result<Arc<rustls::ServerConfig>, String> {
        let mut hosts = self.hosts.lock().unwrap();
        if let Some(config) = hosts.get(host) {
            return Ok(config.clone());
        }
        let fail = |e: &dyn std::fmt::Display| format!("a certificate for {host}: {e}");
        let key = rcgen::KeyPair::generate().map_err(|e| fail(&e))?;
        let mut params =
            rcgen::CertificateParams::new(vec![host.to_owned()]).map_err(|e| fail(&e))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, host);
        // What Apple's and Chrome's checks ask of a server certificate: a SAN, server auth,
        // at most 825 days.
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        valid_for(&mut params, 365);
        let cert = params.signed_by(&key, &self.issuer).map_err(|e| fail(&e))?;
        let chain = vec![cert.der().clone(), self.cert.clone()];
        let key = PrivateKeyDer::try_from(key.serialize_der()).map_err(|e| fail(&e))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // No ALPN: an app offering h2 hears nothing of it and speaks HTTP/1.1, all this
        // serves.
        let config = (rustls::ServerConfig::builder_with_provider(provider))
            .with_safe_default_protocol_versions()
            .and_then(|b| b.with_no_client_auth().with_single_cert(chain, key))
            .map_err(|e| fail(&e))?;
        let config = Arc::new(config);
        hosts.insert(host.to_owned(), config.clone());
        Ok(config)
    }
}

fn ca_params() -> rcgen::CertificateParams {
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    (params.distinguished_name).push(rcgen::DnType::CommonName, "apitool capture CA");
    (params.distinguished_name).push(rcgen::DnType::OrganizationName, "apitool");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    params
}

/// From a day ago, for a clock a little behind, for `days`.
fn valid_for(params: &mut rcgen::CertificateParams, days: u64) {
    let since = (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default();
    let now = rcgen::date_time_ymd(1970, 1, 1) + since;
    params.not_before = now - Duration::from_secs(86_400);
    params.not_after = now + Duration::from_secs(days * 86_400);
}

/// CONNECT while decrypting: the app's TLS ends here and each request inside goes on as
/// any other, its URL https://. A handshake the app broke off is a flow with why.
fn intercept(
    req: Request<Incoming>,
    ca: Arc<Ca>,
    clients: &Clients,
    flows: &Flows,
    mut stopped: tokio::sync::watch::Receiver<()>,
) -> Response<Body> {
    let Some(authority) = req.uri().authority().cloned() else {
        return text(StatusCode::BAD_REQUEST, "CONNECT takes host:port");
    };
    let (clients, flows) = (clients.clone(), flows.clone());
    tokio::spawn(async move {
        let started = Instant::now();
        let host = authority.host().trim_matches(['[', ']']);
        let accepted = async {
            let config = ca.config_for(host)?;
            let upgraded = (hyper::upgrade::on(req).await).map_err(|e| e.to_string())?;
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            (acceptor.accept(TokioIo::new(upgraded)).await)
                .map_err(|e| format!("{e} (does the app trust apitool's CA?)"))
        };
        let tls = match accepted.await {
            Ok(tls) => tls,
            Err(e) => {
                let id = flows.start("CONNECT", authority.as_str(), &HeaderMap::new());
                flows.update(id, |f| {
                    (f.error, f.elapsed) = (Some(e), Some(started.elapsed()));
                });
                return;
            }
        };
        let origin = match authority.port_u16() {
            Some(443) | None => format!("https://{}", authority.host()),
            Some(_) => format!("https://{authority}"),
        };
        let service = hyper::service::service_fn(move |mut req: Request<Incoming>| {
            let (clients, flows) = (clients.clone(), flows.clone());
            // Inside the tunnel a request names only its path.
            let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
            let uri = format!("{origin}{path}").parse::<hyper::Uri>();
            async move {
                let res = match uri {
                    Ok(uri) => {
                        *req.uri_mut() = uri;
                        exchange(req, &clients, &flows).await
                    }
                    Err(e) => text(StatusCode::BAD_REQUEST, &e.to_string()),
                };
                Ok::<_, Infallible>(res)
            }
        });
        let conn =
            hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), service);
        tokio::select! {
            served = conn => if let Err(e) = served {
                log::debug!("capture: {e}");
            },
            _ = stopped.changed() => {}
        }
    });
    Response::new(
        http_body_util::Empty::new()
            .map_err(|e| match e {})
            .boxed_unsync(),
    )
}

/// HAR 1.2, as browsers' devtools save it, of the flows that were HTTP exchanges.
pub fn har(flows: &[&Flow]) -> Value {
    let headers = |h: &[(String, String)]| -> Vec<Value> {
        h.iter()
            .map(|(k, v)| json!({"name": k, "value": v}))
            .collect()
    };
    let mime = |h: &[(String, String)]| {
        (h.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type")))
        .map_or("", |(_, v)| v.as_str())
        .to_owned()
    };
    // Text as it is; anything else base64, as HAR has it.
    let content = |m: &Message| match std::str::from_utf8(&m.body) {
        Ok(text) => json!({"mimeType": mime(&m.headers), "size": m.size, "text": text}),
        Err(_) => json!({
            "mimeType": mime(&m.headers),
            "size": m.size,
            "text": base64::engine::general_purpose::STANDARD.encode(&m.body),
            "encoding": "base64",
        }),
    };
    let entries: Vec<Value> = (flows.iter())
        .filter(|f| !f.tunnel())
        .map(|f| {
            let since = (f.at.duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default();
            let ms = f.elapsed.map_or(-1.0, |e| e.as_secs_f64() * 1000.0);
            let mut request = json!({
                "method": f.method,
                "url": f.url,
                "httpVersion": "HTTP/1.1",
                "headers": headers(&f.request.headers),
                "queryString": [],
                "cookies": [],
                "headersSize": -1,
                "bodySize": f.request.size,
            });
            if f.request.size > 0 {
                let post = content(&f.request);
                request["postData"] = json!({"mimeType": post["mimeType"], "text": post["text"]});
            }
            json!({
                "startedDateTime": crate::model::iso8601(since),
                "time": ms,
                "request": request,
                "response": {
                    "status": f.status.unwrap_or(0),
                    "statusText": "",
                    "httpVersion": "HTTP/1.1",
                    "headers": headers(&f.response.headers),
                    "cookies": [],
                    "content": content(&f.response),
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": f.response.size,
                },
                "cache": {},
                "timings": {"send": 0, "wait": ms, "receive": 0},
            })
        })
        .collect();
    json!({"log": {
        "version": "1.2",
        "creator": {"name": "apitool", "version": env!("CARGO_PKG_VERSION")},
        "entries": entries,
    }})
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::net::{Network, ProxyMode};

    fn runtime() -> tokio::runtime::Runtime {
        crate::net::install_provider();
        (tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build())
        .unwrap()
    }

    /// A capture proxy forwarding by `net`, and where it listens.
    fn proxy(rt: &tokio::runtime::Runtime, net: Network) -> (SocketAddr, Flows) {
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let (addr, flows) = (listener.local_addr().unwrap(), Flows::new(|| {}));
        rt.spawn(serve(listener, clients, flows.clone(), Decrypt::default()));
        (addr, flows)
    }

    fn direct() -> Network {
        Network {
            proxy: ProxyMode::None,
            ..Default::default()
        }
    }

    /// The flow once `done` holds for it: its end is noted as its body finishes going on.
    fn flow_when(flows: &Flows, done: impl Fn(&Flow) -> bool) -> Flow {
        for _ in 0..200 {
            if let Some(f) = flows.with(|l| l.last().filter(|f| done(f)).cloned()) {
                return f;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no such flow: {:?}", flows.with(|l| l.to_vec()));
    }

    /// An app pointed at the proxy gets the server's answer, and the exchange is kept as
    /// the server saw it: the proxy's own headers (its login, its hop) don't go on, a Via
    /// does, and the body arrives whole.
    #[test]
    fn an_app_through_the_proxy_gets_its_answer_and_the_exchange_is_kept() {
        let rt = runtime();
        let (addr, flows) = proxy(&rt, direct());
        let server = crate::http::tests::serve(|req| ("200 OK".into(), req.to_owned()));
        let app = reqwest::Client::builder()
            .proxy(reqwest::Proxy::http(format!("http://u:p@{addr}")).unwrap())
            .no_gzip()
            .build()
            .unwrap();
        let url = format!("http://{server}/items?draft=1");
        let sent = app.post(&url).header("x-app", "1").body("hello");
        let res = rt.block_on(async { sent.send().await.unwrap().text().await.unwrap() });
        let seen = res.to_lowercase();
        assert!(seen.starts_with("post /items?draft=1 http/1.1"), "{res}");
        assert!(
            seen.contains("via: 1.1 apitool-capture") && seen.contains("x-app: 1"),
            "{res}"
        );
        assert!(!seen.contains("proxy-authorization"), "{res}");
        // Nothing of apitool's own: the server sees the app, not a second client.
        assert!(
            !seen.contains("user-agent") && !seen.contains("accept-encoding"),
            "{res}"
        );
        assert!(res.ends_with("hello"), "{res}");

        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!(
            (f.method.as_str(), f.url.as_str(), f.status),
            ("POST", &*url, Some(200))
        );
        assert_eq!(f.request.body, b"hello");
        assert!(f.request.headers.contains(&("x-app".into(), "1".into())));
        assert_eq!(f.response.body, res.as_bytes());
        assert_eq!(f.response.size, res.len() as u64);
        assert!(f.error.is_none());
    }

    /// What isn't a proxy request is told so, and one the proxy sent itself (its upstream
    /// is itself) stops at once instead of going round: neither is a flow.
    #[test]
    fn the_proxy_refuses_to_loop_or_to_be_the_server() {
        let rt = runtime();
        let (addr, flows) = proxy(&rt, direct());
        let plain = reqwest::Client::builder().no_proxy().build().unwrap();
        let as_server = plain.get(format!("http://{addr}/"));
        let status = rt.block_on(async { as_server.send().await.unwrap().status() });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let app = reqwest::Client::builder()
            .proxy(reqwest::Proxy::http(format!("http://{addr}")).unwrap())
            .build()
            .unwrap();
        let looped = app.get("http://example.test/").header("via", VIA);
        let status = rt.block_on(async { looped.send().await.unwrap().status() });
        assert_eq!(status, StatusCode::LOOP_DETECTED);
        assert!(flows.with(<[Flow]>::is_empty));
    }

    /// An echo server: what it reads, it writes back, until the other side closes.
    fn echo() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0; 1024];
                    while let Ok(n @ 1..) = s.read(&mut buf) {
                        let _ = s.write_all(&buf[..n]);
                    }
                });
            }
        });
        addr
    }

    /// Through a CONNECT to `target`, "ping" goes and its echo comes back.
    fn ping_through(proxy: SocketAddr, target: &str) -> String {
        let mut s = std::net::TcpStream::connect(proxy).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s, "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") && s.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        if !head.starts_with("HTTP/1.1 200") {
            return head;
        }
        s.write_all(b"ping").unwrap();
        let mut back = [0; 4];
        s.read_exact(&mut back).unwrap();
        String::from_utf8_lossy(&back).into_owned()
    }

    /// HTTPS goes through untouched as a tunnel; its flow says where to and how much went
    /// each way once it closes.
    #[test]
    fn a_tunnel_carries_bytes_both_ways_and_counts_them() {
        let rt = runtime();
        let (addr, flows) = proxy(&rt, direct());
        let target = echo().to_string();
        assert_eq!(ping_through(addr, &target), "ping");
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert!(f.tunnel());
        assert_eq!((f.url.as_str(), f.status), (&*target, Some(200)));
        assert_eq!((f.request.size, f.response.size), (4, 4));
    }

    /// Behind a corporate proxy a tunnel opens through it, with its login; the proxy
    /// set as its own upstream is refused instead of tunnelling into itself.
    #[test]
    fn a_tunnel_goes_through_the_upstream_proxy_and_never_into_itself() {
        let rt = runtime();
        let target = echo().to_string();
        let corporate = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = corporate.local_addr().unwrap();
        let want = format!("CONNECT {target} ");
        std::thread::spawn(move || {
            let (mut s, _) = corporate.accept().unwrap();
            let head = crate::http::tests::read_request(&mut s);
            let login = head
                .to_lowercase()
                .contains("proxy-authorization: basic dtpw");
            if !(head.starts_with(&want) && login) {
                let _ = s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n");
                return;
            }
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .unwrap();
            let mut buf = [0; 4];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });
        let behind = |proxy_url: String| Network {
            proxy: ProxyMode::Manual,
            proxy_url,
            no_proxy: String::new(),
            ..Default::default()
        };
        let (addr, _) = proxy(&rt, behind(format!("http://u:p@{upstream}")));
        assert_eq!(ping_through(addr, &target), "ping");

        // Its own address is only known once bound: ask it for the port it was given.
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let own = listener.local_addr().unwrap();
        let clients = rt.block_on(crate::net::build_client(behind(format!("http://{own}"))));
        let flows = Flows::new(|| {});
        rt.spawn(serve(
            listener,
            clients.unwrap(),
            flows.clone(),
            Decrypt::default(),
        ));
        let refused = ping_through(own, &target);
        assert!(refused.starts_with("HTTP/1.1 502"), "{refused}");
        let f = flow_when(&flows, |f| f.error.is_some());
        assert!(
            f.error.as_deref().unwrap().contains("its own upstream"),
            "{f:?}"
        );
    }

    /// An HTTPS server with a certificate of its own, answering with the request line.
    fn tls_server() -> SocketAddr {
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivateKeyDer::try_from(own.signing_key.serialize_der()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = (rustls::ServerConfig::builder_with_provider(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![own.cert.der().clone()], key)
            .unwrap();
        let config = Arc::new(config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let config = config.clone();
                std::thread::spawn(move || {
                    let mut conn = rustls::ServerConnection::new(config).unwrap();
                    let mut tls = rustls::Stream::new(&mut conn, &mut s);
                    let req = crate::http::tests::read_request(&mut tls);
                    let line = req.lines().next().unwrap_or_default();
                    let head = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length";
                    let _ = write!(tls, "{head}: {}\r\n\r\n{line}", line.len());
                });
            }
        });
        addr
    }

    /// Decrypting, an app that trusts the CA reads its HTTPS answer through the proxy, and
    /// the exchange is a flow as any other, its URL https://. The CA is the one kept: loaded
    /// again, it signs for the same certificate apps were given. An app that doesn't trust
    /// it can't go on, and its tunnel says why.
    #[test]
    fn https_is_read_when_the_app_trusts_the_ca() {
        let rt = runtime();
        let (cert, key) = Ca::generate().unwrap();
        let ca = Arc::new(Ca::load(&cert, &key).unwrap());
        // The server's own certificate is self-signed: only apitool's side checks it.
        let net = Network {
            insecure: true,
            ..direct()
        };
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let (addr, flows) = (listener.local_addr().unwrap(), Flows::new(|| {}));
        let decrypt = Arc::new(Mutex::new(Some(ca)));
        rt.spawn(serve(listener, clients, flows.clone(), decrypt));
        let server = tls_server();
        let app = |trusted: bool| {
            let mut b = reqwest::Client::builder()
                .proxy(reqwest::Proxy::https(format!("http://{addr}")).unwrap());
            if trusted {
                b = b
                    .add_root_certificate(reqwest::Certificate::from_pem(cert.as_bytes()).unwrap());
            }
            b.build().unwrap()
        };
        let url = format!("https://localhost:{}/items?x=1", server.port());
        let res = rt.block_on(async {
            let res = app(true).get(&url).send().await.unwrap();
            res.text().await.unwrap()
        });
        assert_eq!(res, "GET /items?x=1 HTTP/1.1");
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!(
            (f.method.as_str(), f.url.as_str(), f.status),
            ("GET", &*url, Some(200))
        );
        assert_eq!(f.response.body, res.as_bytes());

        let refused = rt.block_on(app(false).get(&url).send());
        assert!(refused.is_err(), "{refused:?}");
        let f = flow_when(&flows, |f| f.tunnel() && f.error.is_some());
        assert_eq!(f.url, format!("localhost:{}", server.port()));
        assert!(f.error.unwrap().contains("trust apitool's CA"));
    }

    /// Past the cap the oldest flows go, the newest stay: a busy app left capturing
    /// overnight mustn't fill the memory.
    #[test]
    fn the_oldest_flows_go_past_the_cap() {
        let flows = Flows::new(|| {});
        let ids: Vec<u64> = (0..=MAX_FLOWS)
            .map(|i| flows.start("GET", &format!("http://a.test/{i}"), &HeaderMap::new()))
            .collect();
        let kept: Vec<u64> = flows.with(|l| l.iter().map(|f| f.id).collect());
        assert_eq!(kept, ids[1..]);
    }

    /// A kept body stops at its cap, its size still the whole: a large download mustn't
    /// fill the memory, nor read as smaller than it was.
    #[test]
    fn a_kept_body_stops_at_its_cap_but_counts_all() {
        let mut m = Message::default();
        m.take(&vec![b'a'; MAX_BODY - 1]);
        m.take(b"bc");
        assert_eq!((m.body.len(), m.size), (MAX_BODY, MAX_BODY as u64 + 1));
        assert!(m.cut() && m.body.ends_with(b"ab"));
    }

    /// The HAR imports back as the requests sent, their answers as examples; a tunnel,
    /// which had no request to replay, is left out.
    #[test]
    fn the_flows_save_as_a_har_that_imports_back() {
        let message = |headers: &[(&str, &str)], body: &[u8]| Message {
            headers: (headers.iter())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
            size: body.len() as u64,
        };
        let flow = |method: &str, url: &str, request, status, response| Flow {
            id: 1,
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(86_400),
            method: method.into(),
            url: url.into(),
            request,
            status,
            response,
            elapsed: Some(Duration::from_millis(12)),
            error: None,
        };
        let json = [("content-type", "application/json")];
        let post = flow(
            "POST",
            "http://api.test/items",
            message(&json, br#"{"a":1}"#),
            Some(201),
            message(&json, br#"{"id":9}"#),
        );
        let png = flow(
            "GET",
            "http://api.test/logo.png",
            message(&[], b""),
            Some(200),
            message(&[("content-type", "image/png")], &[0x89, b'P', 0xff]),
        );
        let tunnel = flow(
            "CONNECT",
            "api.test:443",
            message(&[], b""),
            Some(200),
            message(&[], b""),
        );
        let har = har(&[&post, &png, &tunnel]);
        assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 2);
        let entry = &har["log"]["entries"][0];
        assert_eq!(entry["startedDateTime"], "1970-01-02T00:00:00.000Z");
        assert_eq!(entry["time"], 12.0);
        assert_eq!(
            har["log"]["entries"][1]["response"]["content"]["encoding"],
            "base64"
        );

        let crate::postman::Import::Collection { requests, .. } = crate::har::import(&har) else {
            panic!("a collection")
        };
        let (_, req) = &requests[0];
        assert_eq!(
            (req.method.as_str(), req.url.as_str()),
            ("POST", "http://api.test/items")
        );
        assert_eq!(
            req.body,
            crate::model::Body::Json {
                text: r#"{"a":1}"#.into()
            }
        );
        assert_eq!(
            (req.examples[0].status, &*req.examples[0].body),
            (201, r#"{"id":9}"#)
        );
    }
}
