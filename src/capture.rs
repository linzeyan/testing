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
use serde::{Deserialize, Serialize};
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
    /// The rules that changed it, a Map Remote's as the URL it went to.
    pub applied: Vec<Act>,
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
            applied: Vec::new(),
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

/// What the pane sets and the proxy reads for each exchange, so a change applies at once.
#[derive(Default)]
pub struct Controls {
    /// Decrypting's CA while it's on; tunnels pass untouched while it's None.
    pub decrypt: Mutex<Option<Arc<Ca>>>,
    pub rules: Mutex<Vec<Rule>>,
    /// Exchanges held at a breakpoint, oldest first, for the pane to let go or drop.
    pub paused: Mutex<Vec<Paused>>,
}

/// An exchange held at a breakpoint, as the text it goes on as.
pub struct Paused {
    pub id: u64,
    /// The answer, not the request.
    pub response: bool,
    /// Its first line, a header a line, a blank line, the body.
    pub text: String,
    done: tokio::sync::oneshot::Sender<String>,
}

impl Paused {
    /// Goes on as `text` says. Dropped instead, it's aborted: the app gets a 502.
    pub fn resume(self) {
        let _ = self.done.send(self.text);
    }
}

const ABORTED: &str = "Aborted at a breakpoint";

impl Controls {
    /// Held for the pane, which lets it go as edited (its first line, headers, body) or
    /// drops it (None).
    async fn pause(
        &self,
        flows: &Flows,
        id: u64,
        first: String,
        headers: &HeaderMap,
        body: &[u8],
        response: bool,
    ) -> Option<(String, HeaderMap, Bytes)> {
        let mut text = format!("{first}\n");
        for (name, value) in pairs(headers) {
            text.push_str(&format!("{name}: {value}\n"));
        }
        // A body that isn't text goes on as it was unless its note is typed over: as text
        // it would come back mangled.
        let note = format!("({} bytes of binary data, sent as they were)", body.len());
        let shown = std::str::from_utf8(body).ok();
        text = format!("{text}\n{}", shown.unwrap_or(&note));
        let (done, edited) = tokio::sync::oneshot::channel();
        let paused = Paused {
            id,
            response,
            text,
            done,
        };
        self.paused.lock().unwrap().push(paused);
        // The pane redraws, showing it.
        flows.update(id, |_| {});
        let edited = edited.await.ok()?;
        let (head, typed) = edited.split_once("\n\n").unwrap_or((edited.as_str(), ""));
        let mut lines = head.lines();
        let first = lines.next().unwrap_or_default().trim().to_owned();
        let mut headers = HeaderMap::new();
        for (name, value) in lines.filter_map(|line| line.split_once(':')) {
            let name = hyper::header::HeaderName::from_bytes(name.trim().as_bytes());
            if let (Ok(name), Ok(value)) = (name, HeaderValue::from_str(value.trim())) {
                headers.append(name, value);
            }
        }
        let body = match shown {
            None if typed.trim() == note => Bytes::copy_from_slice(body),
            _ => Bytes::from(typed.to_owned()),
        };
        Some((first, headers, body))
    }
}

/// Proxyman's tools, as a rule: what to do to an exchange whose URL `matches` it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub on: bool,
    pub url: String,
    pub act: Act,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Act {
    /// Answered from this file, never sent on.
    MapLocal(String),
    /// Sent to this server instead (scheme, host, port), the path and query kept.
    MapRemote(String),
    /// Set on the request; an empty value removes it.
    RequestHeader(String, String),
    /// Set on the answer the app gets; an empty value removes it.
    ResponseHeader(String, String),
    /// Held this long before going on, and the answer at most this fast (0: as fast as
    /// it comes).
    Throttle { delay_ms: u64, kib_per_s: u64 },
    /// Held for the pane to edit and let go: the request before it goes on, the answer
    /// before the app gets it.
    Breakpoint { request: bool, response: bool },
}

/// A rule's URL: found anywhere in the URL, `*` for any run of characters, case aside.
/// An empty one matches nothing, so a rule just added changes nothing yet.
pub fn matches(pattern: &str, url: &str) -> bool {
    let (pattern, url) = (pattern.trim().to_lowercase(), url.to_lowercase());
    let mut at = 0;
    for part in pattern.split('*').filter(|p| !p.is_empty()) {
        match url[at..].find(part) {
            Some(i) => at += i + part.len(),
            None => return false,
        }
    }
    !pattern.is_empty()
}

/// Serves until dropped, its tunnels with it. `clients` forwards: its proxy, CA file and
/// client certificates.
pub async fn serve(listener: TcpListener, clients: Clients, flows: Flows, controls: Arc<Controls>) {
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
        let (stopped, controls) = (stopped.clone(), controls.clone());
        connections.spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let (clients, flows) = (clients.clone(), flows.clone());
                let (stopped, controls) = (stopped.clone(), controls.clone());
                async move {
                    let ca = controls.decrypt.lock().unwrap().clone();
                    let res = match ca {
                        Some(ca) if req.method() == Method::CONNECT => {
                            intercept(req, ca, &clients, &flows, controls, stopped)
                        }
                        _ => handle(req, &clients, &flows, &controls, own, stopped).await,
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
    controls: &Controls,
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
    exchange(req, clients, flows, controls).await
}

/// A request with its absolute URL: forwarded, unless it came back around.
async fn exchange(
    req: Request<Incoming>,
    clients: &Clients,
    flows: &Flows,
    controls: &Controls,
) -> Response<Body> {
    let vias = req.headers().get_all(hyper::header::VIA);
    if vias
        .iter()
        .any(|v| v.as_bytes().ends_with(b"apitool-capture"))
    {
        return text(StatusCode::LOOP_DETECTED, LOOP);
    }
    forward(req, clients, flows, controls).await
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

    fn response(
        &self,
        frame: reqwest::Result<Frame<Bytes>>,
    ) -> Result<Frame<Bytes>, std::io::Error> {
        match frame {
            Ok(frame) => {
                if let Some(chunk) = frame.data_ref() {
                    self.flows.update(self.id, |f| f.response.take(chunk));
                }
                Ok(frame)
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

async fn forward(
    req: Request<Incoming>,
    clients: &Clients,
    flows: &Flows,
    controls: &Controls,
) -> Response<Body> {
    let rules = controls.rules.lock().unwrap().clone();
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
    let (mut target, mut local, mut pace, mut hold) = (url.clone(), None, (0, 0), (false, false));
    let (mut asked, mut answered, mut applied) = (Vec::new(), Vec::new(), Vec::new());
    for rule in rules.iter().filter(|r| r.on && matches(&r.url, &url)) {
        let act = match &rule.act {
            // The first one answers.
            Act::MapLocal(_) if local.is_some() => continue,
            Act::MapLocal(path) => {
                local = Some(path.clone());
                rule.act.clone()
            }
            Act::MapRemote(to) => match remap(&url, to) {
                Ok(to) => {
                    target = to;
                    Act::MapRemote(target.clone())
                }
                Err(e) => return fail(e),
            },
            Act::RequestHeader(name, value) => {
                asked.push((name.clone(), value.clone()));
                rule.act.clone()
            }
            Act::ResponseHeader(name, value) => {
                answered.push((name.clone(), value.clone()));
                rule.act.clone()
            }
            Act::Throttle {
                delay_ms,
                kib_per_s,
            } => {
                pace = (*delay_ms, *kib_per_s);
                rule.act.clone()
            }
            Act::Breakpoint { request, response } => {
                hold = (hold.0 || *request, hold.1 || *response);
                rule.act.clone()
            }
        };
        applied.push(act);
    }
    if !applied.is_empty() {
        flows.update(id, |f| f.applied = applied);
    }
    tokio::time::sleep(Duration::from_millis(pace.0)).await;
    let ok = |id| Answer {
        flows,
        id,
        started,
        rate: pace.1,
    };
    if let Some(path) = local {
        let body = match tokio::fs::read(&path).await {
            Ok(body) => body,
            Err(e) => return fail(format!("{path}: {e}")),
        };
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        let mut headers = HeaderMap::new();
        if let Ok(mime) = HeaderValue::from_str(mime.as_ref()) {
            headers.insert(hyper::header::CONTENT_TYPE, mime);
        }
        // Its length known, as a file server tells it: streamed, it went out chunked.
        headers.insert(hyper::header::CONTENT_LENGTH, body.len().into());
        set_headers(&mut headers, &answered);
        let (status, headers, body) = match hold.1 {
            true => match held(controls, flows, id, StatusCode::OK, headers, body.into()).await {
                Ok(answer) => answer,
                Err(e) => return fail(e),
            },
            false => (StatusCode::OK, headers, Bytes::from(body)),
        };
        return ok(id).send(status, headers, body.into());
    }
    let (mut method, mut headers) = (parts.method, parts.headers);
    // The one hop-by-hop header HTTP/2 lets through, which gRPC's servers ask for.
    let trailers = (headers.get(hyper::header::TE)).is_some_and(|te| te == "trailers");
    for name in HOP_BY_HOP.iter().chain(&["host"]) {
        headers.remove(*name);
    }
    if trailers {
        headers.insert(hyper::header::TE, HeaderValue::from_static("trailers"));
    }
    set_headers(&mut headers, &asked);
    // A GET's empty body stays absent: streamed, it would go out chunked.
    let sent = if hold.0 {
        let body = match body.collect().await {
            Ok(body) => body.to_bytes(),
            Err(e) => return fail(e.to_string()),
        };
        flows.update(id, |f| f.request.take(&body));
        // Counted again for what's typed.
        headers.remove(hyper::header::CONTENT_LENGTH);
        let first = format!("{method} {target}");
        let edited = controls
            .pause(flows, id, first, &headers, &body, false)
            .await;
        let Some((first, edited, body)) = edited else {
            return fail(ABORTED.to_owned());
        };
        let (verb, url) = first.split_once(' ').unwrap_or((first.as_str(), ""));
        method = match Method::from_bytes(verb.as_bytes()) {
            Ok(method) => method,
            Err(e) => return fail(format!("{verb}: {e}")),
        };
        (target, headers) = (url.trim().to_owned(), edited);
        Some(reqwest::Body::from(body))
    } else if body.is_end_stream() {
        None
    } else {
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
        Some(reqwest::Body::wrap_stream(chunks))
    };
    headers.append(hyper::header::VIA, HeaderValue::from_static(VIA));
    let client = match clients.forward_for(&target) {
        Ok(client) => client,
        Err(e) => return fail(e),
    };
    let mut send = client.request(method, &target).headers(headers);
    if let Some(sent) = sent {
        send = send.body(sent);
    }
    let res = match send.send().await {
        Ok(res) => res,
        Err(e) => return fail(error_chain(&e)),
    };
    let mut headers = HeaderMap::new();
    for (name, value) in res.headers() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            headers.append(name, value.clone());
        }
    }
    set_headers(&mut headers, &answered);
    let status = res.status();
    if !hold.1 {
        let body = Response::<reqwest::Body>::from(res).into_body();
        return ok(id).send(status, headers, body);
    }
    let body = match res.bytes().await {
        Ok(body) => body,
        Err(e) => return fail(error_chain(&e)),
    };
    match held(controls, flows, id, status, headers, body).await {
        Ok((status, headers, body)) => ok(id).send(status, headers, body.into()),
        Err(e) => fail(e),
    }
}

/// An answer held at a breakpoint, a server's or a mapped file's: as the pane lets it go,
/// or why the app gets none.
async fn held(
    controls: &Controls,
    flows: &Flows,
    id: u64,
    status: StatusCode,
    mut headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, HeaderMap, Bytes), String> {
    // Counted again for what's typed.
    headers.remove(hyper::header::CONTENT_LENGTH);
    let reason = status.canonical_reason().unwrap_or_default();
    let first = format!("{} {reason}", status.as_u16());
    let edited = (controls.pause(flows, id, first, &headers, &body, true)).await;
    let (first, mut headers, body) = edited.ok_or(ABORTED)?;
    let code = first.split_whitespace().next().unwrap_or_default();
    let Ok(status) = StatusCode::from_bytes(code.as_bytes()) else {
        return Err(format!("{code}: not a status"));
    };
    headers.insert(hyper::header::CONTENT_LENGTH, body.len().into());
    Ok((status, headers, body))
}

/// The answer as the app gets it, kept in its flow as it goes.
struct Answer<'a> {
    flows: &'a Flows,
    id: u64,
    started: Instant,
    /// KiB/s at most; 0, as fast as it comes.
    rate: u64,
}

impl Answer<'_> {
    fn send(self, status: StatusCode, headers: HeaderMap, body: reqwest::Body) -> Response<Body> {
        let shown = pairs(&headers);
        self.flows.update(self.id, |f| {
            (f.status, f.response.headers) = (Some(status.as_u16()), shown);
        });
        let tee = Tee {
            flows: self.flows.clone(),
            id: self.id,
            started: self.started,
        };
        let rate = self.rate;
        let paced = BodyStream::new(body).then(move |frame| async move {
            if let (1.., Ok(Some(chunk))) = (rate, frame.as_ref().map(Frame::data_ref)) {
                let took = chunk.len() as f64 / (rate as f64 * 1024.0);
                tokio::time::sleep(Duration::from_secs_f64(took)).await;
            }
            frame
        });
        let body = StreamBody::new(paced.map(move |frame| tee.response(frame)));
        let mut res = Response::new(body.boxed_unsync());
        (*res.status_mut(), *res.headers_mut()) = (status, headers);
        res
    }
}

/// Header rules in order: set, or removed when the value is empty. What can't be a header
/// is left out.
fn set_headers(headers: &mut HeaderMap, rules: &[(String, String)]) {
    for (name, value) in rules {
        let Ok(name) = hyper::header::HeaderName::from_bytes(name.trim().as_bytes()) else {
            continue;
        };
        if value.is_empty() {
            headers.remove(&name);
        } else if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    }
}

/// `url` sent to `to`'s server: its scheme, host and port, the path and query kept.
fn remap(url: &str, to: &str) -> Result<String, String> {
    let to = reqwest::Url::parse(to.trim()).map_err(|e| format!("Map Remote {to}: {e}"))?;
    let url = reqwest::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
    let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
    Ok(format!(
        "{}{}{query}",
        to.origin().ascii_serialization(),
        url.path()
    ))
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

/// This machine's address on its network, for other devices to reach the capture at: the
/// one it would send from to the internet. The socket is only pointed, nothing is sent.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-1: never answered, and any default route covers it.
    probe.connect("192.0.2.1:9").ok()?;
    probe.local_addr().ok().map(|a| a.ip())
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
    // On every interface, it's itself at any of this machine's addresses: then the
    // connection comes from the address it went to.
    let itself = |p: SocketAddr| {
        own.is_some_and(|own| {
            p == own
                || (own.ip().is_unspecified()
                    && p.port() == own.port()
                    && s.local_addr().is_ok_and(|l| l.ip() == p.ip()))
        })
    };
    if s.peer_addr().is_ok_and(itself) {
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
        let mut config = (rustls::ServerConfig::builder_with_provider(provider))
            .with_safe_default_protocol_versions()
            .and_then(|b| b.with_no_client_auth().with_single_cert(chain, key))
            .map_err(|e| fail(&e))?;
        // HTTP/2 when the app offers it, as gRPC must; else HTTP/1.1.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
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
    controls: Arc<Controls>,
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
            let (clients, flows, controls) = (clients.clone(), flows.clone(), controls.clone());
            // Inside the tunnel a request names only its path.
            let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
            let uri = format!("{origin}{path}").parse::<hyper::Uri>();
            async move {
                let res = match uri {
                    Ok(uri) => {
                        *req.uri_mut() = uri;
                        exchange(req, &clients, &flows, &controls).await
                    }
                    Err(e) => text(StatusCode::BAD_REQUEST, &e.to_string()),
                };
                Ok::<_, Infallible>(res)
            }
        });
        let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2".as_slice());
        let io = TokioIo::new(tls);
        let conn = async {
            if h2 {
                let exec = hyper_util::rt::TokioExecutor::new();
                (hyper::server::conn::http2::Builder::new(exec).serve_connection(io, service)).await
            } else {
                (hyper::server::conn::http1::Builder::new().serve_connection(io, service)).await
            }
        };
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
        proxy_with(rt, net, Controls::default())
    }

    fn proxy_with(
        rt: &tokio::runtime::Runtime,
        net: Network,
        controls: impl Into<Arc<Controls>>,
    ) -> (SocketAddr, Flows) {
        let clients = rt.block_on(crate::net::build_client(net)).unwrap();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let (addr, flows) = (listener.local_addr().unwrap(), Flows::new(|| {}));
        rt.spawn(serve(listener, clients, flows.clone(), controls.into()));
        (addr, flows)
    }

    fn through(addr: SocketAddr) -> reqwest::Client {
        let proxy = reqwest::Proxy::http(format!("http://{addr}")).unwrap();
        reqwest::Client::builder().proxy(proxy).build().unwrap()
    }

    /// A rule's URL is found anywhere, `*` stands for any run, and case doesn't matter; an
    /// empty one, a rule just added, matches nothing.
    #[test]
    fn a_rule_url_matches_anywhere_with_wildcards() {
        let url = "https://API.test/v1/users/7?full=1";
        for hit in ["api.test", "*/users/*", "https://*/v1/*full", "*"] {
            assert!(matches(hit, url), "{hit}");
        }
        for miss in ["", "  ", "/v2/", "*/users/*/posts", "full*v1"] {
            assert!(!matches(miss, url), "{miss}");
        }
    }

    /// Proxyman's tools as rules: a Map Local answers from its file and never reaches the
    /// server; a Map Remote goes to another server, the path and query kept; header rules
    /// set and remove on either side; a rule that's off does nothing. A flow says which
    /// changed it.
    #[test]
    fn rules_answer_locally_send_elsewhere_and_rewrite_headers() {
        let rt = runtime();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        let real = crate::http::tests::serve(move |req| {
            kept.lock().unwrap().push(req.to_lowercase());
            ("200 OK\r\nx-server: real".into(), "real".into())
        });
        let other = crate::http::tests::serve(|req| {
            let line = req.lines().next().unwrap_or_default();
            ("200 OK".into(), format!("other {line}"))
        });
        let file = std::env::temp_dir().join(format!("apitool-map-{}.json", std::process::id()));
        std::fs::write(&file, r#"{"mapped":true}"#).unwrap();
        let rule = |url: &str, act| Rule {
            on: true,
            url: url.into(),
            act,
        };
        let rules = vec![
            Rule {
                on: false,
                ..rule("*", Act::MapLocal("/nowhere".into()))
            },
            rule("*/local*", Act::MapLocal(file.display().to_string())),
            // The first that matches answers.
            rule("/local/", Act::MapLocal("/nowhere".into())),
            rule("/remote", Act::MapRemote(format!("http://{other}/ignored"))),
            rule("*", Act::RequestHeader("x-added".into(), "1".into())),
            rule("*", Act::RequestHeader("x-app".into(), String::new())),
            rule("*", Act::ResponseHeader("x-server".into(), String::new())),
            rule(
                "*",
                Act::ResponseHeader("access-control-allow-origin".into(), "*".into()),
            ),
        ];
        let controls = Controls {
            rules: Mutex::new(rules),
            ..Default::default()
        };
        let (addr, flows) = proxy_with(&rt, direct(), controls);
        let app = through(addr);
        let get = |path: &str| {
            let sent = app.get(format!("http://{real}{path}")).header("x-app", "1");
            rt.block_on(async {
                let res = sent.send().await.unwrap();
                let headers = res.headers().clone();
                (headers, res.text().await.unwrap())
            })
        };

        let (headers, body) = get("/local/a");
        assert_eq!(body, r#"{"mapped":true}"#);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert!(seen.lock().unwrap().is_empty(), "never reached the server");
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!(f.applied[0], Act::MapLocal(file.display().to_string()));

        let (_, body) = get("/remote?q=1");
        assert_eq!(body, "other GET /remote?q=1 HTTP/1.1");
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        let went = Act::MapRemote(format!("http://{other}/remote?q=1"));
        assert!(f.applied.contains(&went), "{:?}", f.applied);

        let (headers, body) = get("/plain");
        assert_eq!(body, "real");
        assert!(!headers.contains_key("x-server"));
        let asked = seen.lock().unwrap()[0].clone();
        assert!(
            asked.contains("x-added: 1") && !asked.contains("x-app"),
            "{asked}"
        );
        let _ = std::fs::remove_file(&file);
    }

    /// A throttle holds the request, then lets the answer through no faster than its rate:
    /// what a slow network does to an app.
    #[test]
    fn a_throttle_holds_the_request_and_slows_the_answer() {
        let rt = runtime();
        let server = crate::http::tests::serve(|_| ("200 OK".into(), "x".repeat(8 * 1024)));
        let throttle = Act::Throttle {
            delay_ms: 300,
            kib_per_s: 16,
        };
        let controls = Controls {
            rules: Mutex::new(vec![Rule {
                on: true,
                url: "/slow".into(),
                act: throttle,
            }]),
            ..Default::default()
        };
        let (addr, _) = proxy_with(&rt, direct(), controls);
        let app = through(addr);
        let get = |path: &str| {
            let sent = app.get(format!("http://{server}{path}"));
            let started = Instant::now();
            let body = rt.block_on(async { sent.send().await.unwrap().bytes().await.unwrap() });
            assert_eq!(body.len(), 8 * 1024);
            started.elapsed()
        };
        // Unthrottled first: a new binary's first connections are held by the firewall here,
        // which would pass for the throttle's doing.
        let quick = get("/warm");
        // 300 ms held, then 8 KiB at 16 KiB/s: half a second more.
        let took = get("/slow");
        assert!(
            took >= Duration::from_millis(780),
            "{took:?} ({quick:?} unthrottled)"
        );
    }

    /// A breakpoint holds the request until it's let go as edited, so the server gets
    /// what was typed; then the answer, edited too, is what the app gets. A binary body
    /// goes on as it was, not as text. Dropped instead, the app gets a 502 and the flow
    /// says why.
    #[test]
    fn a_breakpoint_holds_both_ways_for_an_edit_or_an_abort() {
        let rt = runtime();
        let server = crate::http::tests::serve(|req| ("200 OK".into(), req.to_owned()));
        let controls = Arc::new(Controls {
            rules: Mutex::new(vec![Rule {
                on: true,
                url: "/held".into(),
                act: Act::Breakpoint {
                    request: true,
                    response: true,
                },
            }]),
            ..Default::default()
        });
        let (addr, flows) = proxy_with(&rt, direct(), controls.clone());
        let app = through(addr);
        let held = |response: bool| held_one(&controls, response);
        let send = |sent: reqwest::RequestBuilder| {
            rt.spawn(async move {
                let res = sent.send().await.unwrap();
                (res.status().as_u16(), res.text().await.unwrap())
            })
        };

        let url = format!("http://{server}/held");
        let answer = send(app.post(&url).header("x-app", "1").body("from the app"));
        let mut p = held(false);
        assert!(p.text.starts_with(&format!("POST {url}\n")), "{}", p.text);
        assert!(p.text.contains("x-app: 1\n"), "{}", p.text);
        assert!(p.text.ends_with("\n\nfrom the app"), "{}", p.text);
        p.text = (p.text.replacen("/held", "/edited", 1))
            .replace("x-app: 1", "x-pane: 2")
            .replace("from the app", "from the pane");
        p.resume();
        let mut p = held(true);
        let seen = p.text.clone();
        assert!(seen.starts_with("200 OK\n"), "{seen}");
        let (head, _) = seen.split_once("\n\n").unwrap();
        assert!(!head.contains("content-length"), "counted again: {head}");
        assert!(seen.contains("POST /edited HTTP/1.1"), "{seen}");
        assert!(
            seen.contains("x-pane: 2") && !seen.contains("x-app"),
            "{seen}"
        );
        assert!(seen.ends_with("from the pane"), "{seen}");
        p.text = "201 Created\nx-answer: pane\n\nfrom the breakpoint".into();
        p.resume();
        let answer = rt.block_on(answer).unwrap();
        assert_eq!(answer, (201, "from the breakpoint".to_owned()));
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!(f.status, Some(201));
        assert_eq!(f.request.body, b"from the app", "as the app sent it");
        assert_eq!(f.response.body, b"from the breakpoint", "as the app got it");
        let length = ("content-length".to_owned(), "19".to_owned());
        assert!(
            f.response.headers.contains(&length),
            "{:?}",
            f.response.headers
        );
        assert!(matches!(f.applied[..], [Act::Breakpoint { .. }]));

        let answer = send(app.post(&url).body(vec![0xff, 0, 1]));
        let p = held(false);
        assert!(
            p.text
                .ends_with("\n\n(3 bytes of binary data, sent as they were)")
        );
        p.resume();
        let p = held(true);
        assert!(p.text.contains("content-length: 3"), "{}", p.text);
        p.resume();
        assert_eq!(rt.block_on(answer).unwrap().0, 200);

        let answer = send(app.get(&url));
        drop(held(false));
        let answer = rt.block_on(answer).unwrap();
        assert_eq!(answer, (502, ABORTED.to_owned()));
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!(f.error.as_deref(), Some(ABORTED));
    }

    /// The oldest exchange held, once it's the request or the answer as asked; long enough
    /// for the firewall here, which holds a new binary's first connections.
    fn held_one(controls: &Controls, response: bool) -> Paused {
        for _ in 0..3000 {
            let mut paused = controls.paused.lock().unwrap();
            if paused.first().is_some_and(|p| p.response == response) {
                return paused.remove(0);
            }
            drop(paused);
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("nothing held");
    }

    /// A file answered by Map Local stops at a response breakpoint as a server's answer
    /// does: what the app gets is what the pane let go. Nothing is sent anywhere, so the
    /// host needn't exist.
    #[test]
    fn a_mapped_file_stops_at_a_response_breakpoint() {
        let rt = runtime();
        let file = std::env::temp_dir().join(format!("apitool-mapped-{}.json", std::process::id()));
        std::fs::write(&file, "{\"from\":\"file\"}").unwrap();
        let rule = |act| Rule {
            on: true,
            url: "/mapped".into(),
            act,
        };
        let controls = Arc::new(Controls {
            rules: Mutex::new(vec![
                rule(Act::MapLocal(file.display().to_string())),
                rule(Act::Breakpoint {
                    request: false,
                    response: true,
                }),
            ]),
            ..Default::default()
        });
        let (addr, flows) = proxy_with(&rt, direct(), controls.clone());
        let app = through(addr);
        let answer = rt.spawn(async move {
            let res = app
                .get("http://mapped.invalid/mapped")
                .send()
                .await
                .unwrap();
            (res.status().as_u16(), res.text().await.unwrap())
        });
        let mut p = held_one(&controls, true);
        assert!(
            p.text
                .starts_with("200 OK\ncontent-type: application/json\n"),
            "{}",
            p.text
        );
        assert!(p.text.ends_with("\n\n{\"from\":\"file\"}"), "{}", p.text);
        p.text = p.text.replace("file", "pane");
        p.resume();
        let pane = "{\"from\":\"pane\"}";
        assert_eq!(rt.block_on(answer).unwrap(), (200, pane.to_owned()));
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!(f.response.body, pane.as_bytes());
        let _ = std::fs::remove_file(&file);
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

        // Its own address is only known once bound: ask it for the port it was given. On
        // every interface (other devices let in), 127.0.0.1 is itself too.
        for host in ["127.0.0.1", "0.0.0.0"] {
            let listener = rt.block_on(TcpListener::bind((host, 0))).unwrap();
            let own = SocketAddr::from(([127, 0, 0, 1], listener.local_addr().unwrap().port()));
            let clients = rt.block_on(crate::net::build_client(behind(format!("http://{own}"))));
            let flows = Flows::new(|| {});
            rt.spawn(serve(
                listener,
                clients.unwrap(),
                flows.clone(),
                Arc::default(),
            ));
            let refused = ping_through(own, &target);
            assert!(refused.starts_with("HTTP/1.1 502"), "{host}: {refused}");
            let f = flow_when(&flows, |f| f.error.is_some());
            assert!(
                f.error.as_deref().unwrap().contains("its own upstream"),
                "{host}: {f:?}"
            );
        }
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

    /// An HTTPS server speaking only HTTP/2, as gRPC's do: "pong" and a grpc-status
    /// trailer to each request, noting its version and te.
    fn h2_server(rt: &tokio::runtime::Runtime) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivateKeyDer::try_from(own.signing_key.serialize_der()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = (rustls::ServerConfig::builder_with_provider(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![own.cert.der().clone()], key)
            .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        rt.spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let (acceptor, log) = (acceptor.clone(), log.clone());
                tokio::spawn(async move {
                    let tls = acceptor.accept(s).await.unwrap();
                    let mut conn = h2::server::handshake(tls).await.unwrap();
                    while let Some(Ok((req, mut respond))) = conn.accept().await {
                        let te = req.headers().get("te").map_or("", |v| v.to_str().unwrap());
                        log.lock()
                            .unwrap()
                            .push(format!("{:?} te={te}", req.version()));
                        let head = Response::builder().header("content-type", "application/grpc");
                        let mut send = respond
                            .send_response(head.body(()).unwrap(), false)
                            .unwrap();
                        send.send_data(Bytes::from_static(b"pong"), false).unwrap();
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", HeaderValue::from_static("0"));
                        send.send_trailers(trailers).unwrap();
                    }
                });
            }
        });
        (addr, seen)
    }

    /// Decrypting, an app speaking HTTP/2 goes on in it, as gRPC must: its te: trailers
    /// reaches the server, and the server's trailers, the call's status, reach the app.
    #[test]
    fn grpc_goes_through_decrypted_over_http2() {
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
        let controls = Controls {
            decrypt: Mutex::new(Some(ca)),
            ..Default::default()
        };
        rt.spawn(serve(listener, clients, flows.clone(), Arc::new(controls)));
        let (server, seen) = h2_server(&rt);
        let app = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{addr}")).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(cert.as_bytes()).unwrap())
            .build()
            .unwrap();
        let url = format!("https://localhost:{}/echo.Echo/Ping", server.port());
        let (version, trailers, body) = rt.block_on(async {
            let call = app.post(&url).header("content-type", "application/grpc");
            let res = call
                .header("te", "trailers")
                .body("ping")
                .send()
                .await
                .unwrap();
            let version = res.version();
            let all = Response::<reqwest::Body>::from(res)
                .into_body()
                .collect()
                .await
                .unwrap();
            (
                version,
                all.trailers().cloned().unwrap_or_default(),
                all.to_bytes(),
            )
        });
        assert_eq!(version, reqwest::Version::HTTP_2);
        assert_eq!(body, "pong");
        assert_eq!(trailers.get("grpc-status").unwrap(), "0");
        assert_eq!(*seen.lock().unwrap(), ["HTTP/2.0 te=trailers"]);
        let f = flow_when(&flows, |f| f.elapsed.is_some());
        assert_eq!((f.url, f.response.body), (url, b"pong".to_vec()));
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
        let controls = Controls {
            decrypt: Mutex::new(Some(ca)),
            ..Default::default()
        };
        rt.spawn(serve(listener, clients, flows.clone(), Arc::new(controls)));
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
            applied: Vec::new(),
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
