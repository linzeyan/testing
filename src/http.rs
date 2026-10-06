use std::fmt::Write as _;
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;

use crate::model::{Auth, Body, KeyValue, Request};

/// What went out for one send, for the Timeline tab: the first request as the server got
/// it, then each redirect followed.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Sent {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// None: streamed from files (multipart), so never in memory.
    pub body: Option<String>,
    /// The status that sent it on, and where to.
    pub hops: Vec<(u16, String)>,
    /// The peer the response came from; the proxy's address when going through one.
    pub remote: Option<String>,
    /// Until the response headers were in, redirects included; the rest of the time went
    /// on the body. None from before it was kept, and for gRPC.
    #[serde(default)]
    pub waited: Option<Duration>,
    /// Part of `waited`: opening new connections, DNS, TCP, TLS and a proxy's CONNECT
    /// together. None when an open one was reused.
    #[serde(default)]
    pub connect: Option<Duration>,
    /// Part of `connect`: the TLS handshakes. None over plain HTTP, and from before it
    /// was kept.
    #[serde(default)]
    pub tls: Option<Duration>,
    /// Part of `connect`: looking the host up. None for an IP address, a reused
    /// connection, or a SOCKS proxy that resolves names itself.
    #[serde(default)]
    pub dns: Option<Duration>,
}

/// Only used to build requests, never to send: the code panel and the Headers tab rebuild
/// every frame, and a client loads root certificates and system proxies when made.
pub(crate) static OFFLINE: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    // Client::new panics without a provider, and nothing may have been sent yet.
    crate::net::install_provider();
    reqwest::Client::new()
});

/// The Timeline keeps this much of a request body.
const MAX_SENT_BODY: usize = 64 << 10;

/// Filled in during a send by the cookie jar and the redirect policy: reqwest calls both
/// from inside the send, and has no other way to say what it added.
#[derive(Default)]
pub struct Trace {
    pub cookie: Option<String>,
    pub hops: Vec<(u16, String)>,
    /// Filled in by the connector, see `net::TimeConnect`, `net::TimeDns` and
    /// `net::HelloAt`.
    pub connect: Option<Duration>,
    pub dns: Option<Duration>,
    pub tls: Option<Duration>,
    /// When the handshake of the connection being opened started.
    pub hello: Option<Instant>,
}

tokio::task_local! {
    static TRACE: std::cell::RefCell<Trace>;
    /// Where bodies go instead of RAM for the sends inside `SINK.scope`. A task-local like
    /// TRACE so the runner and its scripts don't carry it.
    pub static SINK: Sink;
}

pub enum Sink {
    /// "Send and download": a successful response's body goes to this file.
    File(std::path::PathBuf),
    /// The load test only counts statuses and times; each VU keeping a body would add up
    /// to gigabytes.
    Discard,
}

/// No-op outside a traced send (WebSocket, SSE, gRPC, the PAC fetch).
pub fn trace(f: impl FnOnce(&mut Trace)) {
    let _ = TRACE.try_with(|t| f(&mut t.borrow_mut()));
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub version: String,
    pub elapsed: Duration,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// The body as received when it isn't text (an image, a zip…); `body` is empty then.
    /// ponytail: not kept in history, whose responses are JSON.
    #[serde(skip)]
    pub bytes: Option<Vec<u8>>,
    /// The body went past `MAX_BODY` and only its start was kept.
    pub truncated: bool,
    pub sent: Sent,
}

/// What a response body may hold in RAM. ponytail: past it the rest isn't read; stream
/// it to a file instead if bodies this big need keeping whole.
pub const MAX_BODY: usize = 16 << 20;

impl Response {
    /// Like `curl -v`: what went out, each redirect, and what came back (the body is in
    /// the Body tab).
    pub fn timeline(&self) -> String {
        let s = &self.sent;
        let mut out = String::new();
        if !s.method.is_empty() {
            let _ = writeln!(out, "> {} {}", s.method, s.url);
            for (k, v) in &s.headers {
                let _ = writeln!(out, "> {k}: {v}");
            }
            match &s.body {
                None => out.push_str(">\n> (streamed from files, not kept)\n"),
                Some(b) if b.is_empty() => {}
                Some(b) => {
                    out.push_str(">\n");
                    for line in b.lines() {
                        let _ = writeln!(out, "> {line}");
                    }
                }
            }
        }
        for (status, url) in &s.hops {
            let _ = writeln!(out, "* {status}: redirected to {url}");
        }
        if let Some(remote) = &s.remote {
            let _ = writeln!(out, "* answered by {remote}");
        }
        let _ = writeln!(out, "< {} {} {}", self.version, self.status, self.reason);
        for (k, v) in &self.headers {
            let _ = writeln!(out, "< {k}: {v}");
        }
        let _ = write!(out, "* {} ms", self.elapsed.as_millis());
        out
    }

    pub fn is_json(&self) -> bool {
        self.headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.contains("json"))
    }
}

/// HTTP verb for a request; the pseudo-methods map to what goes on the wire.
pub fn wire_method(method: &str) -> Result<reqwest::Method, String> {
    match method.trim().to_uppercase().as_str() {
        "WS" | "SSE" | "SOCKETIO" => Ok(reqwest::Method::GET),
        "GRPC" | "GRAPHQL" => Ok(reqwest::Method::POST),
        // Else it would go out as an HTTP request with an "MQTT" verb.
        "MQTT" => Err("MQTT connects instead of sending: open it in the app and Connect".into()),
        m => reqwest::Method::from_bytes(m.as_bytes())
            .map_err(|_| format!("invalid method \"{method}\"")),
    }
}

/// Like Postman: a bare "localhost:8080/x" means http. WebSocket URLs are upgraded from
/// plain HTTP(S), which is what ws(s):// means on the wire.
pub fn wire_url(url: &str) -> String {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if url.contains("://") {
        url.to_owned()
    } else {
        format!("http://{url}")
    }
}

/// Builds the wire request for an already-resolved `Request` (see `Request::resolved`).
/// Shared by plain sends, SSE and WebSocket so all get the same auth/headers/proxy.
pub fn build(client: &reqwest::Client, req: Request) -> Result<reqwest::RequestBuilder, String> {
    let method = wire_method(&req.method)?;
    let url = req.url.trim();
    if url.is_empty() {
        return Err("URL is empty".into());
    }
    let url = wire_url(url);

    let pairs = |kv: &[KeyValue]| {
        kv.iter()
            .map(|p| (p.key.clone(), p.value.clone()))
            .collect::<Vec<_>>()
    };
    // The query lives in the URL itself (see `Request::url_from_params`); the params table is
    // only its editor, so it is not appended again here.
    let mut b = client.request(method, url);
    if req.settings.timeout_ms > 0 {
        b = b.timeout(Duration::from_millis(req.settings.timeout_ms));
    }
    for h in &req.headers {
        b = b.header(h.key.as_str(), h.value.as_str());
    }
    // What signs the finished request: what is signed is then exactly what goes out.
    type Signer = Box<dyn FnOnce(&mut reqwest::Request) -> Result<(), String>>;
    let mut sign_last: Option<Signer> = None;
    let now = std::time::SystemTime::now();
    b = match req.auth {
        // `resolved` has already made an API key a header or query parameter.
        Auth::None | Auth::Inherit | Auth::ApiKey { .. } => b,
        Auth::AwsV4(a) => {
            sign_last = Some(Box::new(move |w| crate::sigv4::sign(w, &a, now)));
            b
        }
        Auth::OAuth1(o) => {
            sign_last = Some(Box::new(move |w| crate::oauth1::sign(w, &o, now)));
            b
        }
        Auth::Bearer { token } => b.bearer_auth(token),
        Auth::Jwt(j) => b.bearer_auth(crate::jwt::sign(&j)?),
        Auth::Basic { username, password } => b.basic_auth(username, Some(password)),
        // Both need a round trip first; `execute` and `with_token` take care of it.
        Auth::Digest { .. } => {
            return Err(
                "Digest auth works for plain HTTP requests only, not WebSocket, SSE or gRPC".into(),
            );
        }
        Auth::OAuth2(_) => {
            return Err("internal: OAuth 2.0 token not fetched before building".into());
        }
    };
    // An explicit Content-Type header from the user always wins.
    let has_type = req
        .headers
        .iter()
        .any(|h| h.key.eq_ignore_ascii_case("content-type"));
    let typed = |b: reqwest::RequestBuilder, mime: &str| {
        if has_type {
            b
        } else {
            b.header(CONTENT_TYPE, mime)
        }
    };
    let b = match req.body {
        Body::None => b,
        Body::Json { text } => typed(b, "application/json").body(text),
        Body::Text { text } => typed(b, "text/plain; charset=utf-8").body(text),
        Body::Form { fields } => b.form(&pairs(&fields)),
        Body::Multipart { parts } => b.multipart(multipart(parts)?),
        // Streamed like multipart files, so a large upload never sits in memory.
        Body::File { path } => {
            if path.trim().is_empty() {
                return Err("Pick a file to send as the body".into());
            }
            let file = std::fs::File::open(&path).map_err(|e| format!("{path}: {e}"))?;
            let len = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            typed(b, mime.as_ref())
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(tokio::fs::File::from_std(file))
        }
        Body::GraphQL { query, variables } => {
            let variables: serde_json::Value = if variables.trim().is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&variables)
                    .map_err(|e| format!("GraphQL variables are not valid JSON: {e}"))?
            };
            let payload = serde_json::json!({ "query": query, "variables": variables });
            typed(b, "application/json").body(payload.to_string())
        }
    };
    let Some(sign) = sign_last else {
        return Ok(b);
    };
    let mut wire = b.build().map_err(|e| error_chain(&e))?;
    sign(&mut wire)?;
    Ok(reqwest::RequestBuilder::from_parts(client.clone(), wire))
}

/// Files are streamed from disk when the request is sent, not read into memory first.
fn multipart(parts: Vec<KeyValue>) -> Result<reqwest::multipart::Form, String> {
    use reqwest::multipart::Part;
    let mut form = reqwest::multipart::Form::new();
    for p in parts {
        let Some(path) = p.value.strip_prefix('@') else {
            form = form.text(p.key, p.value);
            continue;
        };
        let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let len = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let part = Part::stream_with_length(tokio::fs::File::from_std(file), len)
            .file_name(name)
            .mime_str(mime.as_ref())
            .map_err(|e| error_chain(&e))?;
        form = form.part(p.key, part);
    }
    Ok(form)
}

/// Swaps OAuth 2.0 auth for the bearer token it yields (cached, or fetched now).
pub async fn with_token(
    client: &reqwest::Client,
    req: &mut Request,
    fresh: bool,
) -> Result<(), String> {
    if let Auth::OAuth2(o) = &req.auth {
        let token = crate::auth::oauth2_token(client, o, fresh).await?;
        req.auth = Auth::Bearer { token };
    }
    Ok(())
}

/// Sends a resolved request, doing the extra round trip Digest and OAuth 2.0 need.
pub async fn execute(client: reqwest::Client, req: Request) -> Result<Response, String> {
    match req.auth.clone() {
        Auth::OAuth2(_) => {
            let mut first = req.clone();
            with_token(&client, &mut first, false).await?;
            let resp = send_once(&client, first).await?;
            if resp.status != 401 {
                return Ok(resp);
            }
            // The cached token was revoked or expired early: one retry with a new one.
            let mut retry = req;
            with_token(&client, &mut retry, true).await?;
            send_once(&client, retry).await
        }
        Auth::Digest { username, password } => {
            let mut req = Request {
                auth: Auth::None,
                ..req
            };
            let first = send_once(&client, req.clone()).await?;
            let challenge = first.headers.iter().find(|(k, v)| {
                k.eq_ignore_ascii_case("www-authenticate")
                    && v.trim_start()
                        .get(..6)
                        .is_some_and(|s| s.eq_ignore_ascii_case("digest"))
            });
            let (401, Some((_, challenge))) = (first.status, challenge) else {
                return Ok(first);
            };
            let wire = build(&client, req.clone())?
                .build()
                .map_err(|e| error_chain(&e))?;
            let uri = match wire.url().query() {
                Some(q) => format!("{}?{q}", wire.url().path()),
                None => wire.url().path().to_owned(),
            };
            let method = wire.method().as_str();
            let cnonce = crate::auth::cnonce();
            let value =
                crate::auth::digest(challenge, method, &uri, &username, &password, &cnonce)?;
            req.headers.push(KeyValue::new("Authorization", value));
            send_once(&client, req).await
        }
        _ => send_once(&client, req).await,
    }
}

async fn send_once(client: &reqwest::Client, req: Request) -> Result<Response, String> {
    let wire = build(client, req)?.build().map_err(|e| error_chain(&e))?;
    let body_len = wire.body().and_then(|b| b.as_bytes()).map(<[u8]>::len);
    let mut sent = Sent {
        method: wire.method().to_string(),
        url: wire.url().to_string(),
        headers: header_list(wire.headers()),
        body: wire.body().map_or(Some(String::new()), |b| {
            b.as_bytes().map(|b| {
                let mut text =
                    String::from_utf8_lossy(&b[..b.len().min(MAX_SENT_BODY)]).into_owned();
                if b.len() > MAX_SENT_BODY {
                    crate::runner::clip(&mut text, MAX_SENT_BODY);
                }
                text
            })
        }),
        ..Default::default()
    };
    let started = Instant::now();
    let send = async {
        let resp = client.execute(wire).await;
        (resp, TRACE.with(|t| t.take()))
    };
    let (resp, trace) = TRACE.scope(Default::default(), send).await;
    let resp = resp.map_err(|e| error_chain(&e))?;
    sent.waited = Some(started.elapsed());
    sent.added(trace.cookie, body_len);
    sent.hops = trace.hops;
    (sent.connect, sent.dns, sent.tls) = (trace.connect, trace.dns, trace.tls);
    sent.remote = resp.remote_addr().map(|a| a.to_string());
    let status = resp.status();
    let version = format!("{:?}", resp.version());
    let headers = header_list(resp.headers());
    let (body, bytes, truncated) = read_body(resp).await?;
    Ok(Response {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_owned(),
        version,
        elapsed: started.elapsed(),
        headers,
        body,
        bytes,
        truncated,
        sent,
    })
}

impl Sent {
    /// The headers reqwest and hyper add on the way out, as they add them.
    /// ponytail: mirrors reqwest 0.13's rules (checked by a test against what a server
    /// receives); revisit when upgrading reqwest.
    fn added(&mut self, cookie: Option<String>, body_len: Option<usize>) {
        let has = |h: &[(String, String)], name: &str| h.iter().any(|(k, _)| k == name);
        let mut auto = Vec::new();
        if let Ok(url) = reqwest::Url::parse(&self.url)
            && let Some(host) = url.host_str()
        {
            let host = match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            auto.push(("host".to_owned(), host));
        }
        auto.append(&mut self.headers);
        self.headers = auto;
        if !has(&self.headers, "accept") {
            self.headers.push(("accept".into(), "*/*".into()));
        }
        if !has(&self.headers, "user-agent") {
            let agent = crate::net::USER_AGENT.into();
            self.headers.push(("user-agent".into(), agent));
        }
        if !has(&self.headers, "accept-encoding") && !has(&self.headers, "range") {
            self.headers.push(("accept-encoding".into(), "gzip".into()));
        }
        if let Some(cookie) = cookie.filter(|_| !has(&self.headers, "cookie")) {
            self.headers.push(("cookie".into(), cookie));
        }
        if let Some(len) = body_len.filter(|n| *n > 0) {
            self.headers
                .push(("content-length".into(), len.to_string()));
        }
    }
}

/// What Send will add to the headers the user typed (`own`), with where each comes from, for
/// the Headers tab. `req` is the resolved request. Empty when it can't be built yet.
pub fn auto_headers(mut req: Request, own: &[KeyValue]) -> Vec<(String, String, &'static str)> {
    let mut later = None;
    match &req.auth {
        Auth::OAuth2(o) => {
            let token = crate::auth::cached_token(o).unwrap_or_else(|| "<fetched on Send>".into());
            req.auth = Auth::Bearer { token };
        }
        Auth::Digest { .. } => {
            later = Some((
                "authorization".to_owned(),
                "Digest … (answers the server's challenge)".to_owned(),
            ));
            req.auth = Auth::None;
        }
        _ => {}
    }
    let Ok(wire) = build(&OFFLINE, req).and_then(|b| b.build().map_err(|e| error_chain(&e))) else {
        return Vec::new();
    };
    let mut sent = Sent {
        url: wire.url().to_string(),
        headers: header_list(wire.headers()),
        ..Default::default()
    };
    sent.headers.extend(later);
    sent.added(
        None,
        wire.body().and_then(|b| b.as_bytes()).map(<[u8]>::len),
    );
    let own: Vec<String> = (own.iter().filter(|h| h.enabled))
        .map(|h| h.key.trim().to_lowercase())
        .collect();
    let auto = sent.headers.into_iter().filter(|(k, _)| !own.contains(k));
    auto.map(|(k, v)| {
        let from = match k.as_str() {
            "host" | "accept" | "user-agent" | "accept-encoding" => "HTTP client",
            "content-type" | "content-length" => "Body",
            // Authorization, or an API key's header.
            _ => "Auth",
        };
        // A fresh multipart boundary each build would flicker.
        let v = match v.split_once("boundary=") {
            Some((head, _)) => format!("{head}boundary=…"),
            None => v,
        };
        (k, v, from)
    })
    .collect()
}

/// Up to `MAX_BODY` bytes, decoded by the Content-Type's charset as `text()` would (Big5
/// and friends included). `text()` itself takes whatever arrives: a 1 GB download, or a
/// small gzip that inflates to one.
/// Text, bytes when it isn't text, and whether it was cut.
type ReadBody = (String, Option<Vec<u8>>, bool);

async fn read_body(mut resp: reqwest::Response) -> Result<ReadBody, String> {
    let sink = SINK.try_with(|s| match s {
        Sink::File(path) => Some(path.clone()),
        Sink::Discard => None,
    });
    match sink {
        // Only a success: an error body (or Digest's first 401) is small and worth reading.
        Ok(Some(path)) if resp.status().is_success() => {
            let n = download(resp, &path).await?;
            return Ok((
                format!("Saved {n} bytes to {}", path.display()),
                None,
                false,
            ));
        }
        // Still read to the end: that's part of the time, and frees the connection for reuse.
        Ok(None) => {
            while resp.chunk().await.map_err(|e| error_chain(&e))?.is_some() {}
            return Ok((String::new(), None, false));
        }
        _ => {}
    }
    let content_type = (resp.headers().get(CONTENT_TYPE))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let declared = (content_type.split(';'))
        .find_map(|p| p.trim().strip_prefix("charset="))
        .and_then(|c| encoding_rs::Encoding::for_label(c.trim_matches('"').as_bytes()));
    // Said to be text: decoded even when it doesn't fit its charset (a Big5 page without
    // a charset), as before. Anything else that isn't UTF-8 is binary.
    let text_type = declared.is_some() || content_type.starts_with("text/");
    let charset = declared.unwrap_or(encoding_rs::UTF_8);
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = resp.chunk().await.map_err(|e| error_chain(&e))? {
        let room = MAX_BODY - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = match String::from_utf8(bytes) {
        // The usual case, without a copy.
        Ok(text) if charset == encoding_rs::UTF_8 => text,
        Ok(text) => charset.decode(text.as_bytes()).0.into_owned(),
        // `error_len` None: only the end is short, where MAX_BODY cut a character.
        Err(e) if !text_type && e.utf8_error().error_len().is_some() => {
            return Ok((String::new(), Some(e.into_bytes()), truncated));
        }
        Err(e) => charset.decode(e.as_bytes()).0.into_owned(),
    };
    Ok((text, None, truncated))
}

/// Streams the body to `path`, whatever its size. A download that fails or is cancelled
/// (Cancel drops this mid-write) removes the file rather than leave a cut one that looks
/// whole.
async fn download(mut resp: reqwest::Response, path: &std::path::Path) -> Result<u64, String> {
    use tokio::io::AsyncWriteExt;
    /// Armed once the file is ours: a file that couldn't be created may be someone else's.
    struct Partial<'a>(&'a std::path::Path);
    impl Drop for Partial<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut file = tokio::fs::File::create(path).await.map_err(failed)?;
    let partial = Partial(path);
    let mut n = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| error_chain(&e))? {
        file.write_all(&chunk).await.map_err(failed)?;
        n += chunk.len() as u64;
    }
    file.flush().await.map_err(failed)?;
    std::mem::forget(partial);
    Ok(n)
}

pub fn header_list(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// XML indented two spaces a level, with `<a>text</a>` kept on one line. Tags are found by
/// their brackets, not parsed: meant for well-formed documents; None when one doesn't close.
/// ponytail: the token list costs about the text's size again; stream it if big XML bodies
/// get common.
pub fn pretty_xml(xml: &str) -> Option<String> {
    let mut tokens = Vec::new();
    let mut rest = xml.trim();
    while !rest.is_empty() {
        let end = if !rest.starts_with('<') {
            rest.find('<').unwrap_or(rest.len())
        } else if rest.starts_with("<!--") {
            rest.find("-->")? + 3
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>")? + 3
        } else {
            rest.find('>')? + 1
        };
        let token = rest[..end].trim();
        if !token.is_empty() {
            tokens.push(token);
        }
        rest = &rest[end..];
    }
    // An element's start tag: not text, a closing tag, `<?…?>`, `<!…>` or self-closing.
    let opens = |t: &str| {
        let after = t.strip_prefix('<').and_then(|t| t.chars().next());
        after.is_some_and(|c| !matches!(c, '/' | '?' | '!')) && !t.ends_with("/>")
    };
    let mut out = String::with_capacity(xml.len() + xml.len() / 4);
    let (mut depth, mut i) = (0usize, 0);
    while i < tokens.len() {
        let t = tokens[i];
        if t.starts_with("</") {
            depth = depth.saturating_sub(1);
        }
        out.extend(std::iter::repeat_n("  ", depth));
        out.push_str(t);
        if opens(t) {
            match (tokens.get(i + 1), tokens.get(i + 2)) {
                (Some(close), _) if close.starts_with("</") => {
                    out.push_str(close);
                    i += 1;
                }
                (Some(text), Some(close)) if !text.starts_with('<') && close.starts_with("</") => {
                    out.push_str(text);
                    out.push_str(close);
                    i += 2;
                }
                _ => depth += 1,
            }
        }
        out.push('\n');
        i += 1;
    }
    out.pop();
    Some(out)
}

/// Indented like `serde_json::to_string_pretty`, but without building a value tree: a
/// tree costs several times the text (a 100 MB body went past 1 GB). Numbers and escapes
/// stay exactly as the server wrote them.
pub fn pretty_json(body: &str) -> Option<String> {
    // Checks it's JSON without keeping anything.
    serde_json::from_str::<serde::de::IgnoredAny>(body).ok()?;
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 2);
    let mut depth = 0;
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.resize(out.len() + 2 * depth, b' ');
    };
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        match b {
            b'"' => {
                let start = i - 1;
                while bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                out.extend_from_slice(&bytes[start..i]);
            }
            b'{' | b'[' => {
                let next = bytes[i..].iter().position(|c| !c.is_ascii_whitespace());
                if let Some(n) = next.filter(|&n| matches!(bytes[i + n], b'}' | b']')) {
                    // {} and [] stay on one line.
                    out.extend_from_slice(&[b, bytes[i + n]]);
                    i += n + 1;
                } else {
                    out.push(b);
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            b'}' | b']' => {
                depth -= 1;
                newline(&mut out, depth);
                out.push(b);
            }
            b',' => {
                out.push(b',');
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b if b.is_ascii_whitespace() => {}
            b => out.push(b),
        }
    }
    String::from_utf8(out).ok()
}

/// reqwest's Display hides the cause; on a locked-down VDI the cause (proxy refused,
/// unknown issuer, …) is the only useful part.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let _ = write!(msg, "\n  caused by: {s}");
        source = s.source();
    }
    msg
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write};

    use super::*;

    /// Test server: `handle` turns the raw request into (status line plus any extra header
    /// lines, body).
    pub(crate) fn serve(
        handle: impl Fn(&str) -> (String, String) + Send + 'static,
    ) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let (head, body) = handle(&read_request(&mut stream));
                // `connection: close` so the client never reuses a socket we're about to drop.
                let head = format!(
                    "HTTP/1.1 {head}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        addr
    }

    /// The whole request: a streamed body can arrive after the headers.
    fn read_request(stream: &mut impl Read) -> String {
        let (mut req, mut buf) = (Vec::new(), [0; 16 * 1024]);
        loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            req.extend_from_slice(&buf[..n]);
            let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") else {
                if n == 0 {
                    break;
                }
                continue;
            };
            let head = String::from_utf8_lossy(&req[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if n == 0 || req.len() >= end + 4 + len {
                break;
            }
        }
        String::from_utf8_lossy(&req).into_owned()
    }

    /// Answers each request with the raw request it received, so tests can assert on
    /// exactly what went over the wire.
    pub(crate) fn echo_server() -> String {
        let addr = serve(|req| ("200 OK\r\ncontent-type: text/plain".into(), req.to_owned()));
        format!("{addr}/users") // no scheme on purpose: must default to http
    }

    /// Answers every request with the same JSON body.
    pub(crate) fn json_server(body: String) -> String {
        let addr = serve(move |_| {
            (
                "200 OK\r\ncontent-type: application/json".into(),
                body.clone(),
            )
        });
        format!("http://{addr}/graphql")
    }

    fn client(rt: &tokio::runtime::Runtime) -> reqwest::Client {
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        rt.block_on(crate::net::build_client(net)).unwrap().http
    }

    #[test]
    fn digest_auth_answers_the_servers_challenge() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let addr = serve(|req| {
            if req.contains("\r\nauthorization: Digest ") {
                return ("200 OK\r\ncontent-type: text/plain".into(), req.to_owned());
            }
            // Servers often offer Basic too; the Digest challenge must be the one picked.
            let challenge = "401 Unauthorized\r\nwww-authenticate: Basic realm=\"x\"\r\nwww-authenticate: Digest realm=\"r\", qop=\"auth,auth-int\", nonce=\"n1\", opaque=\"o\"";
            (challenge.into(), String::new())
        });
        let req = Request {
            url: format!("http://{addr}/a?b=1"),
            auth: Auth::Digest {
                username: "u".into(),
                password: "p".into(),
            },
            ..Default::default()
        };
        let resp = rt.block_on(execute(client(&rt), req)).unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.contains(r#"authorization: Digest username="u", realm="r", nonce="n1", uri="/a?b=1", algorithm=MD5, response=""#),
            "{}",
            resp.body
        );
        assert!(resp.body.contains(r#"opaque="o""#), "{}", resp.body);
    }

    #[test]
    fn oauth2_token_is_reused_and_renewed_after_a_401() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let rt = tokio::runtime::Runtime::new().unwrap();
        let fetched = std::sync::Arc::new(AtomicUsize::new(0));
        let count = fetched.clone();
        let addr = serve(move |req| {
            if req.starts_with("POST /token ") {
                if !req.contains("grant_type=client_credentials&client_id=app&client_secret=s3") {
                    return ("400 Bad Request".into(), req.to_owned());
                }
                let n = count.fetch_add(1, SeqCst) + 1;
                let token = format!(r#"{{"access_token":"t{n}","expires_in":3600}}"#);
                return ("200 OK\r\ncontent-type: application/json".into(), token);
            }
            // The first token gets revoked on the server before it expires.
            if req.contains("\r\nauthorization: Bearer t1\r\n") {
                return ("401 Unauthorized".into(), String::new());
            }
            ("200 OK\r\ncontent-type: text/plain".into(), req.to_owned())
        });
        let req = Request {
            url: format!("http://{addr}/api"),
            auth: Auth::OAuth2(crate::model::OAuth2 {
                token_url: format!("http://{addr}/token"),
                client_id: "app".into(),
                client_secret: "s3".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        for _ in 0..2 {
            let resp = rt.block_on(execute(client(&rt), req.clone())).unwrap();
            assert!(
                resp.body.contains("\r\nauthorization: Bearer t2\r\n"),
                "{}",
                resp.body
            );
        }
        assert_eq!(
            fetched.load(SeqCst),
            2,
            "t1 rejected, t2 fetched once and reused"
        );
    }

    #[test]
    fn execute_sends_params_headers_auth_and_json_body() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let req = Request {
            method: "post".into(),
            url: format!("{}?q=a b", echo_server()),
            headers: vec![KeyValue::new("X-Trace", "1")],
            body: Body::Json {
                text: "{\"n\":1}".into(),
            },
            auth: Auth::Bearer {
                token: "t0k".into(),
            },
            ..Default::default()
        };
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let resp = rt.block_on(execute(client, req)).unwrap();
        let wire = resp.body.to_lowercase();
        assert_eq!(resp.status, 200);
        assert!(wire.starts_with("post /users?q=a%20b http/1.1"), "{wire}");
        assert!(wire.contains("x-trace: 1"), "{wire}");
        assert!(wire.contains("authorization: bearer t0k"), "{wire}");
        assert!(wire.contains("content-type: application/json"), "{wire}");
        assert!(wire.ends_with("{\"n\":1}"), "{wire}");
        // The Timeline lists exactly what arrived, the headers reqwest adds included.
        assert_eq!(shown(&resp), received(&resp));
        assert_eq!(resp.sent.body.as_deref(), Some("{\"n\":1}"));
    }

    #[test]
    fn a_file_body_goes_out_as_its_bytes_typed_by_its_extension() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let path = std::env::temp_dir().join(format!("apitool-body-{}.png", std::process::id()));
        std::fs::write(&path, "not really a png").unwrap();
        let req = Request {
            method: "PUT".into(),
            url: echo_server(),
            body: Body::File {
                path: path.display().to_string(),
            },
            ..Default::default()
        };
        let resp = rt.block_on(execute(client(&rt), req)).unwrap();
        std::fs::remove_file(&path).unwrap();
        let wire = resp.body.to_lowercase();
        assert!(wire.contains("content-type: image/png"), "{wire}");
        assert!(wire.contains("content-length: 16"), "{wire}");
        assert!(wire.ends_with("\r\n\r\nnot really a png"), "{wire}");
        assert_eq!(shown(&resp), received(&resp));
    }

    /// The Headers tab's "added on Send" plus the user's own must be exactly what arrives.
    #[test]
    fn oauth1_signs_the_finished_request() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let req = Request {
            url: "http://x.test/a?b=1".into(),
            auth: Auth::OAuth1(crate::model::OAuth1 {
                consumer_key: "ck".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let wire = build(&client(&rt), req).unwrap().build().unwrap();
        let h = wire.headers()["authorization"].to_str().unwrap();
        assert!(h.starts_with(r#"OAuth oauth_consumer_key="ck""#), "{h}");
        assert!(h.contains(r#"oauth_signature_method="HMAC-SHA1""#), "{h}");
    }

    #[test]
    fn a_jwt_is_signed_from_the_resolved_claims_and_sent_as_bearer() {
        use base64::Engine as _;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let req = Request {
            url: echo_server(),
            auth: Auth::Jwt(crate::model::Jwt {
                algorithm: "HS256".into(),
                secret: "{{key}}".into(),
                payload: r#"{"sub": "{{user}}"}"#.into(),
            }),
            ..Default::default()
        };
        let vars = std::collections::HashMap::from([
            ("key".into(), "k".into()),
            ("user".into(), "ada".into()),
        ]);
        let (req, _) = req.resolved(&vars);
        let resp = rt.block_on(execute(client(&rt), req)).unwrap();
        let auth = (received(&resp).into_iter())
            .find_map(|(k, v)| (k == "authorization").then_some(v))
            .unwrap();
        let token = auth.strip_prefix("Bearer ").unwrap();
        let claims = token.split('.').nth(1).unwrap();
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims)
            .unwrap();
        assert_eq!(claims, br#"{"sub":"ada"}"#);
        let signed_with = |secret: &str| {
            crate::jwt::sign(&crate::model::Jwt {
                algorithm: "HS256".into(),
                secret: secret.into(),
                payload: r#"{"sub":"ada"}"#.into(),
            })
            .unwrap()
        };
        assert_eq!(
            token,
            signed_with("k"),
            "the secret's variable is resolved too"
        );
    }

    #[test]
    fn the_headers_tab_predicts_every_header_that_arrives() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let req = Request {
            method: "POST".into(),
            url: echo_server(),
            headers: vec![
                KeyValue::new("X-Trace", "1"),
                KeyValue::new("Accept", "text/csv"),
            ],
            body: Body::Json { text: "{}".into() },
            auth: Auth::Bearer { token: "t".into() },
            ..Default::default()
        };
        let auto = auto_headers(req.clone(), &req.headers);
        let mut predicted: Vec<String> = (auto.iter().map(|(k, ..)| k.clone()))
            .chain(req.headers.iter().map(|h| h.key.to_lowercase()))
            .collect();
        predicted.sort();
        let resp = rt.block_on(execute(client(&rt), req)).unwrap();
        let arrived: Vec<String> = received(&resp).into_iter().map(|(k, _)| k).collect();
        assert_eq!(predicted, arrived);
        let from = |name: &str| auto.iter().find(|(k, ..)| k == name).map(|(.., f)| *f);
        assert_eq!(from("authorization"), Some("Auth"));
        assert_eq!(from("content-type"), Some("Body"));
        assert_eq!(from("accept"), None, "the user's own replaces it");
        // A User-Agent by default (some APIs refuse requests without one); the request's
        // own replaces it.
        let agent = ("user-agent".to_owned(), crate::net::USER_AGENT.to_owned());
        assert!(received(&resp).contains(&agent), "{:?}", received(&resp));
        let own = Request {
            url: echo_server(),
            headers: vec![KeyValue::new("User-Agent", "probe")],
            ..Default::default()
        };
        let auto = auto_headers(own.clone(), &own.headers);
        assert!(!auto.iter().any(|(k, ..)| k == "user-agent"), "{auto:?}");
        let resp = rt.block_on(execute(client(&rt), own)).unwrap();
        let agents: Vec<_> = (received(&resp).into_iter())
            .filter(|(k, _)| k == "user-agent")
            .collect();
        assert_eq!(agents, [("user-agent".to_owned(), "probe".to_owned())]);
        let plain = Request {
            url: "http://x.test/a".into(),
            ..Default::default()
        };
        let auto = auto_headers(plain, &[]);
        assert!(
            auto.contains(&(agent.0, agent.1, "HTTP client")),
            "{auto:?}"
        );

        // A file body, built outside any runtime as the UI does.
        let path = std::env::temp_dir().join(format!("apitool-auto-{}.png", std::process::id()));
        std::fs::write(&path, "1234").unwrap();
        let file = Request {
            method: "PUT".into(),
            url: "http://x.test/a".into(),
            body: Body::File {
                path: path.display().to_string(),
            },
            ..Default::default()
        };
        let auto = auto_headers(file, &[]);
        std::fs::remove_file(&path).unwrap();
        assert!(
            auto.contains(&("content-type".into(), "image/png".into(), "Body")),
            "{auto:?}"
        );
        assert!(
            auto.contains(&("content-length".into(), "4".into(), "Body")),
            "{auto:?}"
        );

        // Shown every frame: a fresh boundary each time would flicker. Digest's header
        // only exists after the server's challenge.
        let form = Request {
            method: "POST".into(),
            url: "http://x.test/a".into(),
            body: Body::Multipart {
                parts: vec![KeyValue::new("a", "1")],
            },
            auth: Auth::Digest {
                username: "u".into(),
                password: "p".into(),
            },
            ..Default::default()
        };
        let auto = auto_headers(form, &[]);
        let value = |name: &str| auto.iter().find(|(k, ..)| k == name).unwrap().1.clone();
        assert_eq!(value("content-type"), "multipart/form-data; boundary=…");
        assert!(value("authorization").starts_with("Digest"));
    }

    #[test]
    fn xml_is_indented_with_leaves_on_one_line() {
        let xml = r#"<?xml version="1.0"?><a x="1"><b>text</b><c/><d></d><!-- <no> --><e><f>1</f></e></a>"#;
        let pretty = "<?xml version=\"1.0\"?>\n<a x=\"1\">\n  <b>text</b>\n  <c/>\n  <d></d>\n  <!-- <no> -->\n  <e>\n    <f>1</f>\n  </e>\n</a>";
        assert_eq!(pretty_xml(xml).as_deref(), Some(pretty));
        assert_eq!(
            pretty_xml(pretty).as_deref(),
            Some(pretty),
            "already pretty stays put"
        );
        assert_eq!(pretty_xml("<a><b"), None);
    }

    /// The header lines an echo server got, as (lowercase name, value), sorted.
    fn received(resp: &Response) -> Vec<(String, String)> {
        let head = resp.body.split("\r\n\r\n").next().unwrap();
        let mut lines: Vec<_> = (head.lines().skip(1))
            .map(|l| l.split_once(": ").unwrap())
            .map(|(k, v)| (k.to_lowercase(), v.to_owned()))
            .collect();
        lines.sort();
        lines
    }

    fn shown(resp: &Response) -> Vec<(String, String)> {
        let mut lines = resp.sent.headers.clone();
        lines.sort();
        lines
    }

    #[test]
    fn the_timeline_shows_redirects_and_jar_cookies_as_sent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let addr = serve(|req| {
            if req.starts_with("GET /a ") {
                (
                    "302 Found\r\nlocation: /b\r\nset-cookie: sid=1".into(),
                    String::new(),
                )
            } else if req.starts_with("GET /loop ") {
                ("302 Found\r\nlocation: /loop".into(), String::new())
            } else {
                ("200 OK".into(), req.to_owned())
            }
        });
        let client = client(&rt);
        let req = Request {
            url: format!("http://{addr}/a"),
            headers: vec![KeyValue::new("X-Trace", "1")],
            ..Default::default()
        };
        let first = rt.block_on(execute(client.clone(), req.clone())).unwrap();
        assert_eq!(first.sent.hops, [(302, format!("http://{addr}/b"))]);
        assert_eq!(first.sent.remote, Some(addr.to_string()));
        // The second time the jar has a cookie: it's in the Timeline because it was sent.
        let again = rt.block_on(execute(client.clone(), req)).unwrap();
        assert!(
            again
                .sent
                .headers
                .contains(&("cookie".into(), "sid=1".into()))
        );
        // The server saw the hop, which differs from the first request by a Referer only.
        let mut hop = received(&again);
        hop.retain(|(k, _)| k != "referer");
        assert_eq!(shown(&again), hop);
        let timeline = again.timeline();
        assert!(
            timeline.starts_with(&format!("> GET http://{addr}/a\n")),
            "{timeline}"
        );
        assert!(timeline.contains("\n> cookie: sid=1\n"), "{timeline}");
        assert!(timeline.contains("* 302: redirected to"), "{timeline}");
        // Noting hops must not lift the limit (10 by default).
        let looping = Request {
            url: format!("http://{addr}/loop"),
            ..Default::default()
        };
        let err = rt.block_on(execute(client, looping)).err().unwrap();
        assert!(err.contains("too many redirects"), "{err}");
    }

    #[test]
    fn multipart_uploads_files_with_name_and_type() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("apitool-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, "file body").unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let upload = |value: String| Request {
            method: "POST".into(),
            url: echo_server(),
            body: Body::Multipart {
                parts: vec![KeyValue::new("doc", value), KeyValue::new("note", "hi")],
            },
            ..Default::default()
        };

        let resp = rt.block_on(execute(
            client.clone(),
            upload(format!("@{}", file.display())),
        ));
        let wire = resp.unwrap().body;
        assert!(
            wire.to_lowercase()
                .contains("content-type: multipart/form-data; boundary="),
            "{wire}"
        );
        // What servers key on: field name, original file name, guessed type, the bytes.
        assert!(
            wire.contains(
                "name=\"doc\"; filename=\"hello.txt\"\r\nContent-Type: text/plain\r\n\r\nfile body\r\n"
            ),
            "{wire}"
        );
        assert!(wire.contains("name=\"note\"\r\n\r\nhi\r\n"), "{wire}");

        let missing = dir.join("missing.txt");
        let sent = rt.block_on(execute(client, upload(format!("@{}", missing.display()))));
        let Err(e) = sent else {
            panic!("a missing file must fail the send");
        };
        assert!(e.contains("missing.txt"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Serves `body` as raw bytes, which `serve` can't (it takes a String).
    fn serve_bytes(head: &'static str, body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_request(&mut s);
            let head = format!(
                "HTTP/1.1 200 OK\r\n{head}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        });
        format!("http://{addr}/")
    }

    /// What Postman's time hover splits: a server slow to start answering, and one slow
    /// to send the body, are told apart.
    #[test]
    fn the_wait_for_headers_is_told_from_the_body_download() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_request(&mut s);
            std::thread::sleep(Duration::from_millis(200));
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\n");
            std::thread::sleep(Duration::from_millis(300));
            let _ = s.write_all(b"ok");
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        // A process's first connection can take a second here (measured with a bare
        // std connect), which would land in the wait.
        get(&rt, serve_bytes("content-type: text/plain", b"x".to_vec()));
        let resp = get(&rt, format!("http://{addr}/"));
        let waited = resp.sent.waited.unwrap();
        // A new connection each time ("connection: close"), timed as part of the wait.
        assert!(
            resp.sent.connect.is_some_and(|c| c <= waited),
            "{resp:?}",
            resp = resp.sent
        );
        let ms = Duration::from_millis;
        assert!(ms(200) <= waited && waited < ms(450), "{waited:?}");
        let download = resp.elapsed - waited;
        assert!(download >= ms(250), "{download:?}");
    }

    /// The time hover's DNS line: a host name is looked up inside the connect, an IP
    /// address isn't looked up at all.
    #[test]
    fn a_host_name_lookup_is_timed_as_part_of_the_connect() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ip = serve_bytes("content-type: text/plain", b"x".to_vec());
        let by_ip = get(&rt, ip);
        assert!(
            by_ip.sent.connect.is_some() && by_ip.sent.dns.is_none(),
            "{:?}",
            by_ip.sent
        );
        let name = serve_bytes("content-type: text/plain", b"x".to_vec());
        let by_name = get(&rt, name.replace("127.0.0.1", "localhost"));
        let (dns, connect) = (by_name.sent.dns.unwrap(), by_name.sent.connect.unwrap());
        assert!(dns <= connect, "{:?}", by_name.sent);
    }

    /// The time hover's TCP and TLS lines: a server slow to answer the ClientHello shows
    /// as TLS time, while TCP (accepted by the OS backlog at once) stays short.
    #[test]
    fn a_slow_tls_handshake_is_timed_apart_from_tcp() {
        crate::net::install_provider();
        let own = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::try_from(own.signing_key.serialize_der());
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![own.cert.der().clone()], key.unwrap())
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // The ClientHello is in: everything from here on is the handshake.
            s.peek(&mut [0]).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            let mut conn = rustls::ServerConnection::new(config.into()).unwrap();
            let mut tls = rustls::Stream::new(&mut conn, &mut s);
            read_request(&mut tls);
            let _ = tls
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok");
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        // A process's first connection can take seconds here, which is TCP's to show.
        get(&rt, serve_bytes("content-type: text/plain", b"x".to_vec()));
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            insecure: true,
            ..Default::default()
        };
        let c = rt.block_on(crate::net::build_client(net)).unwrap().http;
        let req = Request {
            method: "GET".into(),
            // Not localhost: Windows tries ::1 first, and a refused connection there takes
            // hyper's 300 ms fallback to IPv4, which is TCP time too.
            url: format!("https://{addr}/"),
            ..Default::default()
        };
        let sent = rt.block_on(execute(c, req)).unwrap().sent;
        let (connect, tls) = (sent.connect.unwrap(), sent.tls.unwrap());
        let ms = Duration::from_millis;
        assert!(tls >= ms(300) && tls <= connect, "{sent:?}");
        let tcp = connect - tls - sent.dns.unwrap_or_default();
        assert!(tcp < ms(200), "{sent:?}");
    }

    /// A reused connection costs nothing to open; it must not show the last one's time.
    #[test]
    fn a_reused_connection_has_no_connect_time() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            for _ in 0..2 {
                read_request(&mut s);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
            }
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let c = client(&rt);
        let req = Request {
            method: "GET".into(),
            url: format!("http://{addr}/"),
            ..Default::default()
        };
        let first = rt.block_on(execute(c.clone(), req.clone())).unwrap();
        let second = rt.block_on(execute(c, req)).unwrap();
        assert!(first.sent.connect.is_some());
        assert_eq!(second.sent.connect, None);
    }

    fn get(rt: &tokio::runtime::Runtime, url: String) -> Response {
        let req = Request {
            method: "GET".into(),
            url,
            ..Default::default()
        };
        rt.block_on(execute(client(rt), req)).unwrap()
    }

    /// QUERY (a GET with a body), TRACE and CONNECT go out as themselves; CONNECT in the
    /// authority form a proxy expects.
    #[test]
    fn query_trace_and_connect_go_out_as_written() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let url = echo_server();
        let host = url.trim_end_matches("/users").to_owned();
        let send = |method: &str| {
            let req = Request {
                method: method.into(),
                url: url.clone(),
                body: Body::Json {
                    text: r#"{"q":1}"#.into(),
                },
                ..Default::default()
            };
            rt.block_on(execute(client(&rt), req)).unwrap()
        };
        let query = send("QUERY");
        assert!(
            query.body.starts_with("QUERY /users HTTP/1.1"),
            "{}",
            query.body
        );
        assert!(query.body.ends_with(r#"{"q":1}"#), "{}", query.body);
        assert!(send("TRACE").body.starts_with("TRACE /users HTTP/1.1"));
        // A 2xx turns a CONNECT into a tunnel, so the echo isn't read back: the request line
        // is in what was sent.
        let connect = send("CONNECT");
        assert_eq!(connect.status, 200);
        assert_eq!(connect.sent.method, "CONNECT");
        assert!(connect.timeline().contains(&host), "{}", connect.timeline());
    }

    /// A body past MAX_BODY keeps its start and says so, instead of taking whatever the
    /// server sends into a machine with under 1 GB free.
    #[test]
    fn a_huge_body_is_cut_and_marked() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = vec![b'a'; MAX_BODY + 1000];
        let resp = get(&rt, serve_bytes("content-type: text/plain", body));
        assert!(resp.truncated);
        assert_eq!(resp.body.len(), MAX_BODY);
        let resp = get(
            &rt,
            serve_bytes("content-type: text/plain", b"small".to_vec()),
        );
        assert!(!resp.truncated);
        assert_eq!(resp.body, "small");
    }

    /// An image read as text loses its bytes for good; Save… and the preview need them.
    #[test]
    fn a_binary_body_keeps_its_bytes_and_text_stays_text() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        let resp = get(&rt, serve_bytes("content-type: image/png", png.clone()));
        assert_eq!((resp.bytes, resp.body.as_str()), (Some(png.clone()), ""));
        // Said to be text: decoded as before, however wrong the bytes are.
        let resp = get(&rt, serve_bytes("content-type: text/plain", png));
        assert!(resp.bytes.is_none() && resp.body.starts_with('\u{FFFD}'));
        // Cut at MAX_BODY through a character: still text, not binary.
        let mut cut = vec![b'a'; MAX_BODY - 1];
        cut.extend("é".repeat(10).as_bytes());
        let resp = get(&rt, serve_bytes("content-type: application/json", cut));
        assert!(
            resp.truncated && resp.bytes.is_none(),
            "{:?}",
            resp.bytes.map(|b| b.len())
        );
    }

    /// "Send and download" is how a body past MAX_BODY is kept whole: it goes to disk as
    /// it arrives. An error answer stays on screen, where it can be read.
    #[test]
    fn a_download_keeps_the_whole_body_on_disk_not_in_ram() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = std::env::temp_dir().join(format!("apitool-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let client = client(&rt);
        let fetch = |url: String, to: &std::path::Path| {
            let req = Request {
                method: "GET".into(),
                url,
                ..Default::default()
            };
            let send = SINK.scope(Sink::File(to.to_owned()), execute(client.clone(), req));
            rt.block_on(send).unwrap()
        };
        let file = dir.join("big.bin");
        let body: Vec<u8> = (0..MAX_BODY + 1000).map(|i| i as u8).collect();
        let url = serve_bytes("content-type: application/octet-stream", body.clone());
        let resp = fetch(url, &file);
        assert!(!resp.truncated);
        assert!(
            resp.body.contains(&file.display().to_string()),
            "{}",
            resp.body
        );
        assert!(
            std::fs::read(&file).unwrap() == body,
            "every byte, past MAX_BODY"
        );

        let missing = dir.join("not-found.txt");
        let addr = serve(|_| ("404 Not Found".into(), "no such thing".into()));
        let resp = fetch(format!("http://{addr}/"), &missing);
        assert_eq!(resp.body, "no such thing");
        assert!(!missing.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cancel drops a download mid-write; the cut file left behind would look whole.
    #[test]
    fn a_cancelled_download_leaves_no_file() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let file = std::env::temp_dir().join(format!("apitool-cut-{}.bin", std::process::id()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_request(&mut s);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000000\r\n\r\n");
            let _ = s.write_all(&[7; 1000]);
            std::thread::sleep(Duration::from_secs(5));
        });
        let req = Request {
            method: "GET".into(),
            url: format!("http://{addr}/"),
            ..Default::default()
        };
        let send = SINK.scope(Sink::File(file.clone()), execute(client(&rt), req));
        let task = rt.spawn(send);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::fs::metadata(&file).map_or(true, |m| m.len() == 0) {
            assert!(
                std::time::Instant::now() < deadline,
                "the download should start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        task.abort();
        assert!(rt.block_on(task).is_err_and(|e| e.is_cancelled()));
        assert!(!file.exists(), "a cut file looks whole");
    }

    /// 50 load-test VUs each holding a 4 MB body peaked at 502 MiB; they only need the
    /// status, so the body is read through and dropped.
    #[test]
    fn a_discarded_body_is_read_but_not_kept() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let req = Request {
            method: "GET".into(),
            url: serve_bytes("content-type: text/plain", vec![b'a'; 1 << 20]),
            ..Default::default()
        };
        let send = SINK.scope(Sink::Discard, execute(client(&rt), req));
        let resp = rt.block_on(send).unwrap();
        assert_eq!((resp.status, resp.body.as_str()), (200, ""));
    }

    /// Reading in capped chunks must still decode like `text()` did: legacy Taiwanese
    /// APIs answer in Big5.
    #[test]
    fn the_charset_still_decodes_the_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let big5 = vec![0xA4, 0xA4, 0xA4, 0xE5]; // 中文
        let url = serve_bytes("content-type: text/plain; charset=Big5", big5);
        assert_eq!(get(&rt, url).body, "中文");
    }

    /// Same text as serde_json's pretty printer, without its value tree.
    #[test]
    fn pretty_json_matches_serde_json() {
        let doc = r#" {"a":[1,2.5,-3e2,{"b":null,"c":true}],"e":{},"f":[ ],
            "s":"q\"uo,te:{[]}\\","u":"中文é","n":[[[]]],"z":{"y":[{}]}} "#;
        let ours = pretty_json(doc).unwrap();
        let value: serde_json::Value = serde_json::from_str(doc).unwrap();
        let theirs = serde_json::to_string_pretty(&value).unwrap();
        // serde_json rewrites numbers (-3e2 becomes -300.0); ours keeps them as sent.
        assert_eq!(ours.replace("-3e2", "-300.0"), theirs);
        assert_eq!(pretty_json("[1,"), None, "not JSON: shown as it came");
        assert_eq!(pretty_json("\"x\"").unwrap(), "\"x\"");
    }
}
