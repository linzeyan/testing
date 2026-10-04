//! A local mock server answering with the saved examples of the requests under a folder,
//! like Postman's mock servers, so a frontend can be built before its backend exists.
//!
//! ponytail: minimal HTTP/1.1 (one request per connection, `connection: close`, no chunked
//! request bodies); enough for browsers and API clients talking to a local mock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::model::{Example, Request};
use crate::store::Workspace;

/// Calls handled at once. Each loads every request in scope, so a burst (a load test aimed
/// at the mock) mustn't multiply that without limit; the rest wait to be accepted.
const MAX_CONNECTIONS: usize = 16;
/// A client that connects and says nothing would otherwise keep its slot for good.
/// Shorter in tests, which wait it out.
const READ_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });

/// Serves until the task is dropped. Requests are re-read from disk for every call, so a
/// saved edit (a new example) is live at once. `log` gets one line per call.
pub async fn serve(
    ws: Workspace,
    scope: PathBuf,
    listener: TcpListener,
    log: impl Fn(String) + Clone + Send + 'static,
) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    while let Ok(slot) = slots.clone().acquire_owned().await
        && let Ok((stream, _)) = listener.accept().await
    {
        let (ws, scope, log) = (ws.clone(), scope.clone(), log.clone());
        tokio::spawn(async move {
            let _slot = slot;
            if let Some(line) = handle(&ws, &scope, stream).await {
                log(line);
            }
        });
    }
}

async fn handle(ws: &Workspace, scope: &std::path::Path, mut stream: TcpStream) -> Option<String> {
    let read = tokio::time::timeout(READ_TIMEOUT, read_head(&mut stream));
    let (method, target, headers) = read.await.ok()??;
    let header = |name: &str| headers.get(name).map(String::as_str);
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let (status, content_type, body, note) =
        if method == "OPTIONS" && header("access-control-request-method").is_some() {
            (
                204,
                String::new(),
                String::new(),
                "CORS preflight".to_owned(),
            )
        } else {
            let requests = ws.load_requests_in(scope).unwrap_or_default();
            let want = header("x-mock-response-name").or(header("x-mock-response-code"));
            match find(&requests, &method, path, want) {
                Some((name, ex)) => {
                    // Dynamic variables ({{$guid}}, {{$timestamp}}) give fresh values per call.
                    let body = crate::model::resolve(&ex.body, &HashMap::new(), &mut Vec::new());
                    let note = format!("{name}: {}", ex.name);
                    (ex.status, ex.content_type.clone(), body, note)
                }
                None => {
                    let known: Vec<String> = requests
                        .iter()
                        .filter(|(_, r)| !r.examples.is_empty())
                        .map(|(_, r)| format!("{} {}", r.method, r.url))
                        .collect();
                    let error = match want {
                        Some(w) => format!("no example \"{w}\" for {method} {path}"),
                        None => format!("no saved example matches {method} {path}"),
                    };
                    let body = serde_json::json!({ "error": error, "mocked": known }).to_string();
                    (404, "application/json".into(), body, "no match".into())
                }
            }
        };
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    let allow_headers = header("access-control-request-headers").unwrap_or("*");
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\
         access-control-allow-origin: *\r\naccess-control-allow-methods: *\r\n\
         access-control-allow-headers: {allow_headers}\r\n",
        body.len()
    );
    if !content_type.is_empty() {
        head.push_str(&format!("content-type: {content_type}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.ok()?;
    stream.write_all(body.as_bytes()).await.ok()?;
    let _ = stream.shutdown().await;
    Some(format!("{method} {target} → {status} ({note})"))
}

/// Request line and lowercase headers. The body is read and dropped: closing a socket with
/// unread data resets it, and the client would see an error instead of the response.
async fn read_head(stream: &mut TcpStream) -> Option<(String, String, HashMap<String, String>)> {
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        let n = stream.read(&mut chunk).await.ok().filter(|&n| n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_uppercase(), first.next()?.to_owned());
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut left = length.saturating_sub(buf.len() - end - 4);
    while left > 0 {
        let n = stream.read(&mut chunk).await.ok().filter(|&n| n > 0)?;
        left = left.saturating_sub(n);
    }
    Some((method, target, headers))
}

/// The example answering `method path`. Among requests whose URL path matches, the most
/// specific wins (most literal segments, so `/users/me` beats `/users/{{id}}`). `want`
/// picks an example by name or status code, as Postman's `x-mock-response-*` headers do.
pub fn find<'a>(
    requests: &'a [(String, Request)],
    method: &str,
    path: &str,
    want: Option<&str>,
) -> Option<(&'a str, &'a Example)> {
    let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (name, req) = requests
        .iter()
        .filter(|(_, r)| !r.examples.is_empty())
        .filter(|(_, r)| {
            let m = r.method.to_uppercase();
            m == method || (m == "GRAPHQL" && method == "POST")
        })
        .filter_map(|(name, r)| {
            let (pattern, anchored) = pattern(&r.url);
            let literal = matches(&pattern, anchored, &path)?;
            Some(((literal, pattern.len()), (name, r)))
        })
        .max_by_key(|(score, _)| *score)?
        .1;
    let ex = match want {
        Some(w) => req
            .examples
            .iter()
            .find(|e| e.name == w || e.status.to_string() == w),
        None => req.examples.first(),
    }?;
    Some((name.as_str(), ex))
}

/// Path segments of a request URL, and whether they must match the whole path. A URL that
/// starts with a variable (`{{base}}/users`) may expand to any host and path prefix, so it
/// matches the end of the path instead.
fn pattern(url: &str) -> (Vec<&str>, bool) {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let (path, anchored) = match url.split_once("://") {
        Some((_, rest)) => (rest.find('/').map_or("", |i| &rest[i..]), true),
        None => (
            url.find('/').map_or("", |i| &url[i..]),
            !url.starts_with("{{"),
        ),
    };
    (
        path.split('/').filter(|s| !s.is_empty()).collect(),
        anchored,
    )
}

/// Number of literal segments if `path` matches; `{{var}}` and `:name` match any segment.
fn matches(pattern: &[&str], anchored: bool, path: &[&str]) -> Option<usize> {
    if pattern.len() > path.len() || (anchored && pattern.len() != path.len()) {
        return None;
    }
    let tail = &path[path.len() - pattern.len()..];
    let mut literal = 0;
    for (p, s) in pattern.iter().zip(tail) {
        if p.contains("{{") || p.starts_with(':') {
            continue;
        }
        if p != s {
            return None;
        }
        literal += 1;
    }
    Some(literal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, url: &str, examples: &[(&str, u16)]) -> Request {
        Request {
            method: method.into(),
            url: url.into(),
            examples: examples
                .iter()
                .map(|(name, status)| Example {
                    name: (*name).into(),
                    status: *status,
                    content_type: "application/json".into(),
                    body: format!("{{\"example\": \"{name}\"}}"),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn requests_match_by_method_and_most_specific_path() {
        let requests = vec![
            (
                "user".into(),
                req(
                    "GET",
                    "{{base}}/users/{{id}}?x=1",
                    &[("found", 200), ("gone", 404)],
                ),
            ),
            ("me".into(), req("GET", "{{base}}/users/me", &[("me", 200)])),
            (
                "create".into(),
                req("POST", "{{base}}/users", &[("created", 201)]),
            ),
            (
                "orders".into(),
                req("GET", "http://shop.test/api/orders", &[("orders", 200)]),
            ),
            ("no examples".into(), req("GET", "{{base}}/health", &[])),
        ];
        let hit = |method: &str, path: &str, want: Option<&str>| {
            find(&requests, method, path, want).map(|(_, e)| e.name.as_str())
        };
        assert_eq!(hit("GET", "/users/7", None), Some("found"));
        assert_eq!(
            hit("GET", "/users/me", None),
            Some("me"),
            "literal beats variable"
        );
        // {{base}} may carry a path prefix, e.g. http://host/api/v1.
        assert_eq!(hit("GET", "/api/v1/users/7", None), Some("found"));
        assert_eq!(hit("POST", "/users", None), Some("created"));
        assert_eq!(hit("DELETE", "/users/7", None), None);
        // A literal host means the path is known exactly.
        assert_eq!(hit("GET", "/api/orders", None), Some("orders"));
        assert_eq!(hit("GET", "/v2/api/orders", None), None);
        assert_eq!(hit("GET", "/health", None), None, "nothing to answer with");
        assert_eq!(hit("GET", "/users/7", Some("404")), Some("gone"));
        assert_eq!(hit("GET", "/users/7", Some("gone")), Some("gone"));
        assert_eq!(hit("GET", "/users/7", Some("teapot")), None);
    }

    #[test]
    fn serves_examples_with_cors_over_http() {
        let root = std::env::temp_dir().join(format!("apitool-mock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let mut user = req("GET", "{{base}}/users/{{id}}", &[("found", 200)]);
        user.examples[0].body = r#"{"id": "{{$guid}}"}"#.into();
        ws.save_request(&ws.collections().join("user.toml"), &user)
            .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let addr = rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(serve(ws.clone(), ws.collections(), listener, |_| {}));
            addr
        });
        crate::net::install_provider();
        let client = reqwest::Client::new();
        rt.block_on(async {
            let resp = client
                .post(format!("http://{addr}/users/7"))
                .body("x".repeat(100_000))
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                404,
                "POST isn't mocked; the upload is drained"
            );
            let resp = client
                .get(format!("http://{addr}/users/7"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let h = resp.headers();
            assert_eq!(h["content-type"], "application/json");
            assert_eq!(h["access-control-allow-origin"], "*");
            let body = resp.text().await.unwrap();
            assert!(
                body.len() == 46 && !body.contains("{{"),
                "fresh guid: {body}"
            );
            let preflight = client
                .request(reqwest::Method::OPTIONS, format!("http://{addr}/users/7"))
                .header("origin", "http://localhost:5173")
                .header("access-control-request-method", "GET")
                .header("access-control-request-headers", "authorization")
                .send()
                .await
                .unwrap();
            assert_eq!(preflight.status(), 204);
            assert_eq!(
                preflight.headers()["access-control-allow-headers"],
                "authorization"
            );
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn calls_at_once_are_capped_and_a_silent_client_lets_go() {
        let root = std::env::temp_dir().join(format!("apitool-mock-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let user = req("GET", "{{base}}/users/{{id}}", &[("found", 200)]);
        ws.save_request(&ws.collections().join("user.toml"), &user)
            .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(serve(ws.clone(), ws.collections(), listener, |_| {}));
            let call = |addr| async move {
                let mut s = TcpStream::connect(addr).await.unwrap();
                s.write_all(b"GET /users/7 HTTP/1.1\r\nhost: x\r\n\r\n")
                    .await
                    .unwrap();
                let mut reply = String::new();
                let read = tokio::time::timeout(READ_TIMEOUT * 3, s.read_to_string(&mut reply));
                read.await.expect("served").unwrap();
                reply
            };
            // The first connection to a new binary can take a second to set up (the macOS
            // firewall); not timed, and not asked anything.
            drop(TcpStream::connect(addr).await.unwrap());
            let started = tokio::time::Instant::now();
            let mut silent = Vec::new();
            for _ in 0..MAX_CONNECTIONS {
                silent.push(TcpStream::connect(addr).await.unwrap());
            }
            let reply = call(addr).await;
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            assert!(
                started.elapsed() >= READ_TIMEOUT,
                "waited for a silent client to time out"
            );
        });
        let _ = std::fs::remove_dir_all(&root);
    }
}
