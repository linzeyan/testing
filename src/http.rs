use std::fmt::Write as _;
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;

use crate::model::{Auth, Body, KeyValue, Request};

pub struct Response {
    pub status: u16,
    pub reason: String,
    pub version: String,
    pub elapsed: Duration,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn is_json(&self) -> bool {
        self.headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.contains("json"))
    }
}

/// HTTP verb for a request; the pseudo-methods map to what goes on the wire.
pub fn wire_method(method: &str) -> Result<reqwest::Method, String> {
    match method.trim().to_uppercase().as_str() {
        "WS" | "SSE" => Ok(reqwest::Method::GET),
        "GRPC" | "GRAPHQL" => Ok(reqwest::Method::POST),
        m => reqwest::Method::from_bytes(m.as_bytes())
            .map_err(|_| format!("invalid method \"{method}\"")),
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
    // Like Postman: a bare "localhost:8080/x" means http. WebSocket URLs are upgraded
    // from plain HTTP(S), which is what ws(s):// means on the wire.
    let url = if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if url.contains("://") {
        url.to_owned()
    } else {
        format!("http://{url}")
    };

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
    b = match req.auth {
        Auth::None | Auth::Inherit => b,
        Auth::Bearer { token } => b.bearer_auth(token),
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
    Ok(match req.body {
        Body::None => b,
        Body::Json { text } => typed(b, "application/json").body(text),
        Body::Text { text } => typed(b, "text/plain; charset=utf-8").body(text),
        Body::Form { fields } => b.form(&pairs(&fields)),
        Body::Multipart { parts } => b.multipart(multipart(parts)?),
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
    })
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
    let b = build(client, req)?;
    let started = Instant::now();
    let resp = b.send().await.map_err(|e| error_chain(&e))?;
    let status = resp.status();
    let version = format!("{:?}", resp.version());
    let headers = header_list(resp.headers());
    let body = resp.text().await.map_err(|e| error_chain(&e))?;
    Ok(Response {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_owned(),
        version,
        elapsed: started.elapsed(),
        headers,
        body,
    })
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

pub fn pretty_json(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    serde_json::to_string_pretty(&value).ok()
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
    fn read_request(stream: &mut std::net::TcpStream) -> String {
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
}
