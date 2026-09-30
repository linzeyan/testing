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
        self.headers.iter().any(|(k, v)| k == "content-type" && v.contains("json"))
    }
}

/// Sends an already-resolved request (see `Request::resolved`).
pub async fn execute(client: reqwest::Client, req: Request) -> Result<Response, String> {
    let method = reqwest::Method::from_bytes(req.method.trim().to_uppercase().as_bytes())
        .map_err(|_| format!("invalid method \"{}\"", req.method))?;
    let url = req.url.trim();
    if url.is_empty() {
        return Err("URL is empty".into());
    }
    // Like Postman: a bare "localhost:8080/x" means http.
    let url = if url.contains("://") { url.to_owned() } else { format!("http://{url}") };

    let pairs = |kv: &[KeyValue]| kv.iter().map(|p| (p.key.clone(), p.value.clone())).collect::<Vec<_>>();
    let mut b = client.request(method, url).query(&pairs(&req.params));
    for h in &req.headers {
        b = b.header(h.key.as_str(), h.value.as_str());
    }
    b = match req.auth {
        Auth::None => b,
        Auth::Bearer { token } => b.bearer_auth(token),
        Auth::Basic { username, password } => b.basic_auth(username, Some(password)),
    };
    // An explicit Content-Type header from the user always wins.
    let has_type = req.headers.iter().any(|h| h.key.eq_ignore_ascii_case("content-type"));
    let typed = |b: reqwest::RequestBuilder, mime: &str| if has_type { b } else { b.header(CONTENT_TYPE, mime) };
    b = match req.body {
        Body::None => b,
        Body::Json { text } => typed(b, "application/json").body(text),
        Body::Text { text } => typed(b, "text/plain; charset=utf-8").body(text),
        Body::Form { fields } => b.form(&pairs(&fields)),
    };

    let started = Instant::now();
    let resp = b.send().await.map_err(|e| error_chain(&e))?;
    let status = resp.status();
    let version = format!("{:?}", resp.version());
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
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

    /// Server that answers each request with the raw request it received, so tests
    /// can assert on exactly what went over the wire.
    pub(crate) fn echo_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = vec![0; 16 * 1024];
                let n = stream.read(&mut buf).unwrap();
                let body = &buf[..n];
                // `connection: close` so the client never reuses a socket we're about to drop.
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        format!("{addr}/users") // no scheme on purpose: must default to http
    }

    #[test]
    fn execute_sends_params_headers_auth_and_json_body() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let req = Request {
            method: "post".into(),
            url: echo_server(),
            params: vec![KeyValue::new("q", "a b")],
            headers: vec![KeyValue::new("X-Trace", "1")],
            body: Body::Json { text: "{\"n\":1}".into() },
            auth: Auth::Bearer { token: "t0k".into() },
            ..Default::default()
        };
        let net = crate::net::Network { proxy: crate::net::ProxyMode::None, ..Default::default() };
        let client = rt.block_on(crate::net::build_client(net)).unwrap();
        let resp = rt.block_on(execute(client, req)).unwrap();
        let wire = resp.body.to_lowercase();
        assert_eq!(resp.status, 200);
        assert!(wire.starts_with("post /users?q=a+b http/1.1"), "{wire}");
        assert!(wire.contains("x-trace: 1"), "{wire}");
        assert!(wire.contains("authorization: bearer t0k"), "{wire}");
        assert!(wire.contains("content-type: application/json"), "{wire}");
        assert!(wire.ends_with("{\"n\":1}"), "{wire}");
    }
}
