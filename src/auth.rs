//! Auth schemes that need a round trip: OAuth 2.0 fetches a token before the request,
//! Digest answers the server's 401 challenge.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::http::error_chain;
use crate::model::{Grant, OAuth2};

/// Tokens by grant parameters. The workspace keeps them across runs (see `export`).
static TOKENS: LazyLock<Mutex<HashMap<String, Cached>>> = LazyLock::new(Default::default);
/// Tokens granted so far: a saver compares it with the count it last saved.
static GRANTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Cached {
    token: String,
    until: Instant,
    /// Gets the next token without signing in again, when the provider gave one.
    refresh: Option<String>,
}
/// Without `expires_in`, a token is reused this long; a 401 refetches it anyway.
const DEFAULT_TTL: Duration = Duration::from_secs(3600);
/// Refetch a bit early so a token doesn't expire in flight.
const MARGIN: Duration = Duration::from_secs(30);

fn cache_key(o: &OAuth2) -> String {
    format!(
        "{:?}\n{}\n{}\n{}\n{}",
        o.grant, o.token_url, o.client_id, o.scope, o.username
    )
}

/// A token as the workspace keeps it: expiry in Unix seconds, as an `Instant` means
/// nothing to the next process.
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    token: String,
    expires: u64,
    refresh: Option<String>,
}

fn unix_now() -> u64 {
    (SystemTime::now().duration_since(UNIX_EPOCH)).map_or(0, |d| d.as_secs())
}

/// The tokens worth keeping as JSON, so the next start doesn't sign in again: those still
/// valid, and expired ones with a refresh token.
pub fn export() -> String {
    let tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    let (now, unix) = (Instant::now(), unix_now());
    let kept: HashMap<&String, Kept> = (tokens.iter())
        .filter(|(_, c)| now < c.until || c.refresh.is_some())
        .map(|(k, c)| {
            let left = c.until.saturating_duration_since(now).as_secs();
            let (token, refresh) = (c.token.clone(), c.refresh.clone());
            let expires = unix + left;
            (
                k,
                Kept {
                    token,
                    expires,
                    refresh,
                },
            )
        })
        .collect();
    serde_json::to_string(&kept).unwrap_or_default()
}

/// Takes in what `export` wrote; tokens this process already has are fresher and stay.
pub fn import(json: &str) {
    let Ok(kept) = serde_json::from_str::<HashMap<String, Kept>>(json) else {
        return;
    };
    let (now, unix) = (Instant::now(), unix_now());
    let mut tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    for (key, k) in kept {
        tokens.entry(key).or_insert(Cached {
            token: k.token,
            until: now + Duration::from_secs(k.expires.saturating_sub(unix)),
            refresh: k.refresh,
        });
    }
}

/// In place of the tokens here: a sync merged this machine's into `json`.
pub fn replace(json: &str) {
    TOKENS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    import(json);
}

/// Goes up with each token granted; unchanged since the last save means nothing new.
pub fn grants() -> u64 {
    GRANTS.load(Ordering::Relaxed)
}

/// A still-valid cached token, without any network traffic (for "Copy as curl").
pub fn cached_token(o: &OAuth2) -> Option<String> {
    let tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    tokens
        .get(&cache_key(o))
        .filter(|c| Instant::now() < c.until)
        .map(|c| c.token.clone())
}

/// `fresh` skips the cache, after the API rejected the cached token.
pub async fn oauth2_token(
    client: &reqwest::Client,
    o: &OAuth2,
    fresh: bool,
) -> Result<String, String> {
    token(client, o, fresh, open_browser).await
}

/// `open` shows the sign-in page for the authorization code grant: the system browser, or
/// a stand-in in tests.
async fn token(
    client: &reqwest::Client,
    o: &OAuth2,
    fresh: bool,
    open: impl FnOnce(&str) -> Result<(), String>,
) -> Result<String, String> {
    if !fresh && let Some(token) = cached_token(o) {
        return Ok(token);
    }
    let key = cache_key(o);
    let cached = TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    if o.grant == Grant::Implicit {
        // No token endpoint and no refresh token: the redirect brings the token itself.
        let back = sign_in(o, open).await?;
        let ttl = (back.get("expires_in").and_then(|s| s.parse().ok()))
            .map_or(DEFAULT_TTL, Duration::from_secs);
        let token = back["access_token"].clone();
        return Ok(store(
            key,
            Granted {
                token,
                ttl,
                refresh: None,
            },
            None,
        ));
    }
    // Expired or rejected: the refresh token saves a sign-in. If it's refused too (revoked,
    // expired), the grant runs again from the start.
    if let Some(refresh) = cached.and_then(|c| c.refresh) {
        let mut form = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh.clone()),
            ("client_id", o.client_id.clone()),
        ];
        if !o.client_secret.is_empty() {
            form.push(("client_secret", o.client_secret.clone()));
        }
        if let Ok(got) = request_token(client, o, &form).await {
            return Ok(store(key, got, Some(refresh)));
        }
    }
    let grant = match o.grant {
        Grant::ClientCredentials => "client_credentials",
        Grant::Password => "password",
        Grant::AuthorizationCode => "authorization_code",
        Grant::Implicit => unreachable!("returned above"),
    };
    let (password, code) = (
        o.grant == Grant::Password,
        o.grant == Grant::AuthorizationCode,
    );
    let mut form = vec![
        ("grant_type", grant.to_owned()),
        ("client_id", o.client_id.clone()),
    ];
    // client_secret_post: accepted by the common providers (Keycloak, Entra ID, Auth0, Okta).
    for (key, value, needed) in [
        ("client_secret", &o.client_secret, true),
        // The authorization code grant asks for its scope when signing in.
        ("scope", &o.scope, !code),
        ("username", &o.username, password),
        ("password", &o.password, password),
    ] {
        if needed && !value.is_empty() {
            form.push((key, value.clone()));
        }
    }
    if code {
        let mut back = sign_in(o, open).await?;
        let take = |k: &str, back: &mut HashMap<String, String>| back.remove(k).unwrap_or_default();
        form.push(("code", take("code", &mut back)));
        form.push(("redirect_uri", take(REDIRECT, &mut back)));
        form.push(("code_verifier", take(VERIFIER, &mut back)));
    }
    let got = request_token(client, o, &form).await?;
    Ok(store(key, got, None))
}

/// A token response: the access token, how long it lasts, and a refresh token if any.
struct Granted {
    token: String,
    ttl: Duration,
    refresh: Option<String>,
}

/// Caches what was granted; a provider that doesn't rotate refresh tokens leaves `refresh`
/// as it was. Returns the access token.
fn store(key: String, got: Granted, refresh: Option<String>) -> String {
    let cached = Cached {
        token: got.token.clone(),
        until: Instant::now() + got.ttl.saturating_sub(MARGIN),
        refresh: got.refresh.or(refresh),
    };
    TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, cached);
    GRANTS.fetch_add(1, Ordering::Relaxed);
    got.token
}

async fn request_token(
    client: &reqwest::Client,
    o: &OAuth2,
    form: &[(&str, String)],
) -> Result<Granted, String> {
    let resp = client
        .post(o.token_url.trim())
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .map_err(|e| format!("OAuth 2.0 token request: {}", error_chain(&e)))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| error_chain(&e))?;
    let clip = |s: &str| s.chars().take(300).collect::<String>();
    if !status.is_success() {
        return Err(format!("OAuth 2.0 token request: {status} {}", clip(&text)));
    }
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| format!("OAuth 2.0 token response is not JSON: {}", clip(&text)))?;
    let token = json["access_token"]
        .as_str()
        .ok_or("OAuth 2.0 token response has no access_token")?
        .to_owned();
    let ttl = json["expires_in"]
        .as_u64()
        .map_or(DEFAULT_TTL, Duration::from_secs);
    let refresh = json["refresh_token"].as_str().map(str::to_owned);
    Ok(Granted {
        token,
        ttl,
        refresh,
    })
}

/// The `Authorization` value answering a `WWW-Authenticate: Digest …` challenge (RFC 7616).
pub fn digest(
    challenge: &str,
    method: &str,
    uri: &str,
    user: &str,
    pass: &str,
    cnonce: &str,
) -> Result<String, String> {
    let params = challenge_params(challenge.trim_start().get(6..).unwrap_or_default());
    let param = |k: &str| params.get(k).map(String::as_str);
    let nonce = param("nonce").ok_or("Digest challenge has no nonce")?;
    let realm = param("realm").unwrap_or_default();
    let algorithm = param("algorithm").unwrap_or("MD5");
    let (sha256, sess) = match algorithm.to_ascii_uppercase().as_str() {
        "MD5" => (false, false),
        "MD5-SESS" => (false, true),
        "SHA-256" => (true, false),
        "SHA-256-SESS" => (true, true),
        other => return Err(format!("Digest algorithm {other} isn't supported")),
    };
    let h = |s: String| -> String {
        use sha2::Digest as _;
        let bytes: Vec<u8> = if sha256 {
            sha2::Sha256::digest(s.as_bytes()).to_vec()
        } else {
            md5::Md5::digest(s.as_bytes()).to_vec()
        };
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    };
    let qop = match param("qop") {
        None => None,
        Some(q) if q.split(',').any(|q| q.trim() == "auth") => Some("auth"),
        Some(q) => return Err(format!("Digest qop \"{q}\" isn't supported (only auth)")),
    };
    let nc = "00000001";
    let mut ha1 = h(format!("{user}:{realm}:{pass}"));
    if sess {
        ha1 = h(format!("{ha1}:{nonce}:{cnonce}"));
    }
    let ha2 = h(format!("{method}:{uri}"));
    let response = match qop {
        Some(q) => h(format!("{ha1}:{nonce}:{nc}:{cnonce}:{q}:{ha2}")),
        None => h(format!("{ha1}:{nonce}:{ha2}")),
    };
    let mut out = format!(
        r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", algorithm={algorithm}, response="{response}""#
    );
    if let Some(q) = qop {
        let _ = write!(out, r#", qop={q}, nc={nc}, cnonce="{cnonce}""#);
    }
    if let Some(opaque) = param("opaque") {
        let _ = write!(out, r#", opaque="{opaque}""#);
    }
    Ok(out)
}

/// `realm="a, b", qop=auth` → map. Values may be quoted (with commas inside) or bare.
fn challenge_params(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        let key: String = std::iter::from_fn(|| chars.next_if(|c| *c != '=')).collect();
        if chars.next().is_none() {
            return out;
        }
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => value.extend(chars.next()),
                    c => value.push(c),
                }
            }
        } else {
            value = std::iter::from_fn(|| chars.next_if(|c| *c != ','))
                .collect::<String>()
                .trim()
                .to_owned();
        }
        out.insert(key.trim().to_ascii_lowercase(), value);
    }
}

/// Keys `sign_in` adds to what the browser brought back: what the token request must
/// repeat. Leading spaces: no provider parameter can be named like that.
const REDIRECT: &str = " redirect_uri";
const VERIFIER: &str = " code_verifier";

/// Signing in may take a password and a second factor.
const SIGN_IN_WAIT: Duration = Duration::from_secs(180);

/// Authorization code with PKCE (RFC 7636) over a loopback redirect (RFC 8252): listen on
/// this machine, send the browser to the provider, take the code it comes back with.
/// PKCE is always sent: a provider that doesn't know it ignores the extra parameters.
/// The implicit grant comes back the same way with `access_token` instead of `code`.
async fn sign_in(
    o: &OAuth2,
    open: impl FnOnce(&str) -> Result<(), String>,
) -> Result<HashMap<String, String>, String> {
    let implicit = o.grant == Grant::Implicit;
    let wanted = match o.redirect_uri.trim() {
        "" => "http://127.0.0.1:0/callback",
        uri => uri,
    };
    let mut redirect =
        reqwest::Url::parse(wanted).map_err(|e| format!("OAuth 2.0 redirect URI: {e}"))?;
    if redirect.scheme() != "http"
        || !matches!(redirect.host_str(), Some("127.0.0.1" | "localhost"))
    {
        return Err(
            "The OAuth 2.0 redirect URI must come back to this machine: http://127.0.0.1:PORT/…"
                .into(),
        );
    }
    let port = redirect.port().unwrap_or(80);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("Can't listen on port {port} for the OAuth 2.0 sign-in: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let _ = redirect.set_port(Some(port));
    let (verifier, state) = (random_token(32)?, random_token(16)?);
    let challenge = {
        use base64::Engine as _;
        use sha2::Digest as _;
        let hash = sha2::Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
    };
    let mut url =
        reqwest::Url::parse(o.auth_url.trim()).map_err(|e| format!("OAuth 2.0 auth URL: {e}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", if implicit { "token" } else { "code" })
        .append_pair("client_id", &o.client_id)
        .append_pair("redirect_uri", redirect.as_str())
        .append_pair("state", &state);
    if !implicit {
        url.query_pairs_mut()
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
    }
    if !o.scope.is_empty() {
        url.query_pairs_mut().append_pair("scope", &o.scope);
    }
    open(url.as_str())?;
    let wanted = if implicit { "access_token" } else { "code" };
    let came = callback(&listener, redirect.path(), &state, wanted);
    let mut back = tokio::time::timeout(SIGN_IN_WAIT, came)
        .await
        .map_err(|_| "No OAuth 2.0 sign-in came back from the browser within 3 minutes")??;
    back.insert(REDIRECT.into(), redirect.to_string());
    back.insert(VERIFIER.into(), verifier);
    Ok(back)
}

/// The implicit grant's token is in the URL fragment, which browsers never send: this page
/// sends it again as a query string. Without a fragment the provider left nothing to read.
const FRAGMENT_PAGE: &str = "<!doctype html><title>apitool</title><p style=\"font: 16px sans-serif\" id=m>Reading the sign-in…</p><script>if (location.hash.length > 1) location.replace(location.pathname + '?' + location.hash.slice(1)); else document.getElementById('m').textContent = 'Sign-in failed: the browser came back without a token.';</script>";

/// Answers the browser until a request to `path` brings `wanted` (the code or the token),
/// or the provider's error. Returns all of the redirect's parameters.
async fn callback(
    listener: &tokio::net::TcpListener,
    path: &str,
    state: &str,
    wanted: &str,
) -> Result<HashMap<String, String>, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        // The whole head: it may arrive in pieces, and closing on unread bytes resets the
        // connection, so the browser would show an error instead of the page.
        let (mut head, mut buf) = (Vec::new(), [0u8; 2048]);
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 64 << 10 {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let target = head.split_whitespace().nth(1).unwrap_or("/");
        let url = reqwest::Url::parse(&format!("http://localhost{target}"));
        let Some(url) = url.ok().filter(|u| u.path() == path) else {
            // The favicon, mostly.
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
            continue;
        };
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        if q.is_empty() && wanted == "access_token" {
            let page = FRAGMENT_PAGE;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            continue;
        }
        let result = match (q.get("error"), q.get(wanted)) {
            (Some(error), _) => Err(match q.get("error_description") {
                Some(d) => format!("{error}: {d}"),
                None => error.clone(),
            }),
            // Another sign-in's answer, e.g. from a stale tab: never take its code.
            _ if q.get("state").map(String::as_str) != Some(state) => {
                Err("the browser came back from a different sign-in; try again".to_owned())
            }
            (None, Some(_)) => Ok(q.clone()),
            (None, None) => Err(format!(
                "the browser came back without {}",
                if wanted == "code" {
                    "a code"
                } else {
                    "a token"
                }
            )),
        };
        let (status, text) = match &result {
            Ok(_) => (
                "200 OK",
                "Signed in. You can close this tab and go back to apitool.".to_owned(),
            ),
            Err(e) => ("400 Bad Request", format!("Sign-in failed: {e}")),
        };
        let text = text.replace('&', "&amp;").replace('<', "&lt;");
        let page = format!(
            "<!doctype html><title>apitool</title><p style=\"font: 16px sans-serif\">{text}</p>"
        );
        let reply = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        return result.map_err(|e| format!("OAuth 2.0 sign-in: {e}"));
    }
}

/// ponytail: the OS's own opener rather than a crate; covers macOS, Windows and Linux
/// desktops. A failure says where to sign in by hand.
pub fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = {
        // Not `cmd /c start`: cmd would split the URL at each `&`.
        let mut cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler");
        cmd
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url)
        .spawn()
        .map(drop)
        .map_err(|e| format!("Couldn't open a browser ({e}); open {url} by hand"))
}

/// Unguessable: a predictable PKCE verifier or state would defeat their purpose.
fn random_token(len: usize) -> Result<String, String> {
    use base64::Engine as _;
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes)
        .map_err(|e| format!("No randomness for the OAuth 2.0 sign-in: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub fn cnonce() -> String {
    let mut bytes = [0u8; 16];
    // A fixed cnonce only weakens replay protection; never worth failing the request over.
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use super::*;

    fn query(url: &str) -> HashMap<String, String> {
        let url = reqwest::Url::parse(url).unwrap();
        url.query_pairs().into_owned().collect()
    }

    fn client(rt: &tokio::runtime::Runtime) -> reqwest::Client {
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        rt.block_on(crate::net::build_client(net)).unwrap().http
    }

    /// Plays the browser: keeps the sign-in URL in `seen`, then comes back to its redirect
    /// URI with what `answer` makes of the state (after a favicon request, as browsers do).
    fn browser(
        seen: Arc<Mutex<String>>,
        answer: fn(&str) -> String,
    ) -> impl FnOnce(&str) -> Result<(), String> {
        move |url| {
            *seen.lock().unwrap() = url.to_owned();
            let q = query(url);
            let back = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
            let paths = [
                "/favicon.ico".to_owned(),
                format!("{}?{}", back.path(), answer(&q["state"])),
            ];
            std::thread::spawn(move || {
                for path in paths {
                    let mut s =
                        std::net::TcpStream::connect(("127.0.0.1", back.port().unwrap())).unwrap();
                    // In two pieces, as a request may arrive.
                    s.write_all(b"GET ").unwrap();
                    std::thread::sleep(Duration::from_millis(20));
                    write!(s, "{path} HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
                    let _ = s.read_to_string(&mut String::new());
                }
            });
            Ok(())
        }
    }

    fn code_grant(token_url: String) -> OAuth2 {
        OAuth2 {
            grant: Grant::AuthorizationCode,
            auth_url: "https://idp.test/authorize?tenant=x".into(),
            token_url,
            client_id: "app".into(),
            scope: "read".into(),
            ..Default::default()
        }
    }

    /// Signing in after every restart is what keeping tokens in the workspace saves: a
    /// valid token comes back as it was, an expired one only for its refresh token.
    #[test]
    fn tokens_come_back_in_the_next_process() {
        let grant = |url: &str| cache_key(&code_grant(format!("http://{url}/token")));
        let (valid, dead, stale) = (grant("valid.test"), grant("dead.test"), grant("stale.test"));
        let granted = |token: &str, secs: u64, refresh: Option<&str>| Granted {
            token: token.into(),
            ttl: Duration::from_secs(secs),
            refresh: refresh.map(Into::into),
        };
        store(valid.clone(), granted("t1", 3600, Some("r1")), None);
        store(dead.clone(), granted("old", 0, None), None);
        store(stale.clone(), granted("old", 0, Some("r2")), None);
        let json = export();
        // The next process: nothing in memory but what the workspace kept.
        let forget = || {
            let mut tokens = TOKENS.lock().unwrap();
            for key in [&valid, &dead, &stale] {
                tokens.remove(key);
            }
        };
        forget();
        import(&json);
        let tokens = TOKENS.lock().unwrap();
        let t1 = &tokens[&valid];
        assert_eq!(
            (t1.token.as_str(), t1.refresh.as_deref()),
            ("t1", Some("r1"))
        );
        let left = t1.until.saturating_duration_since(Instant::now());
        assert!(
            left > Duration::from_secs(3000) && left <= Duration::from_secs(3600),
            "{left:?}"
        );
        assert!(
            !tokens.contains_key(&dead),
            "expired, nothing to renew it with"
        );
        assert_eq!(tokens[&stale].refresh.as_deref(), Some("r2"), "renewable");
        assert!(tokens[&stale].until <= Instant::now(), "but expired");
        drop(tokens);
        // A token this process got meanwhile is newer than what was kept.
        store(valid.clone(), granted("t2", 3600, None), None);
        import(&json);
        assert_eq!(
            cached_token(&code_grant("http://valid.test/token".into())).as_deref(),
            Some("t2")
        );
        forget();
    }

    /// The provider gets a PKCE challenge; the token request must prove it with the
    /// verifier behind it, and bring the code the browser came back with.
    #[test]
    fn authorization_code_signs_in_through_the_browser_with_pkce() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let sign_in = seen.clone();
        let addr = crate::http::tests::serve(move |req| {
            use base64::Engine as _;
            use sha2::Digest as _;
            let body = req.split("\r\n\r\n").nth(1).unwrap_or_default();
            let form = query(&format!("http://x/?{body}"));
            let asked = query(&sign_in.lock().unwrap());
            let proof = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(form["code_verifier"].as_bytes()));
            let ok = form["grant_type"] == "authorization_code"
                && form["code"] == "c0de"
                && form["redirect_uri"] == asked["redirect_uri"]
                && asked["code_challenge_method"] == "S256"
                && proof == asked["code_challenge"]
                && !form.contains_key("scope");
            match ok {
                true => (
                    "200 OK\r\ncontent-type: application/json".into(),
                    r#"{"access_token":"t1","expires_in":3600}"#.into(),
                ),
                false => ("400 Bad Request".into(), format!("{form:?}\n{asked:?}")),
            }
        });
        let o = code_grant(format!("http://{addr}/token"));
        let client = client(&rt);
        let answer = |state: &str| format!("code=c0de&state={state}");
        let token1 = rt.block_on(token(&client, &o, true, browser(seen.clone(), answer)));
        assert_eq!(token1.as_deref(), Ok("t1"));
        let asked = seen.lock().unwrap().clone();
        assert!(
            asked.starts_with("https://idp.test/authorize?tenant=x&response_type=code"),
            "{asked}"
        );
        assert_eq!(query(&asked)["scope"], "read");
        // Until it expires, the token comes from the cache: no second sign-in.
        let again = rt.block_on(token(&client, &o, false, |_: &str| -> Result<(), String> {
            panic!("signed in twice")
        }));
        assert_eq!(again.as_deref(), Ok("t1"));
    }

    /// The implicit grant's token arrives in the redirect's fragment, which a browser never
    /// sends: the callback must hand out a page that resends it, and no token endpoint may
    /// be called (there is none, and no client secret to give it).
    #[test]
    fn implicit_grant_takes_the_token_from_the_redirect_fragment() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let addr = crate::http::tests::serve(move |_| {
            count.fetch_add(1, SeqCst);
            ("500 Nope".into(), String::new())
        });
        let o = OAuth2 {
            grant: Grant::Implicit,
            auth_url: "https://idp.test/authorize".into(),
            token_url: format!("http://{addr}/token"),
            client_id: "spa".into(),
            client_secret: "never-sent".into(),
            ..Default::default()
        };
        let seen = Arc::new(Mutex::new(String::new()));
        let first_page = Arc::new(Mutex::new(String::new()));
        let (url_seen, page_seen) = (seen.clone(), first_page.clone());
        let browser = move |url: &str| -> Result<(), String> {
            *url_seen.lock().unwrap() = url.to_owned();
            let q = query(url);
            let back = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
            let port = back.port().unwrap();
            let state = q["state"].clone();
            let path = back.path().to_owned();
            std::thread::spawn(move || {
                let get = |target: &str| {
                    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                    write!(s, "GET {target} HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
                    let mut reply = String::new();
                    let _ = s.read_to_string(&mut reply);
                    reply
                };
                // What the browser asks for first: the fragment stays behind.
                *page_seen.lock().unwrap() = get(&path);
                get(&format!(
                    "{path}?access_token=imp&token_type=bearer&expires_in=120&state={state}"
                ));
            });
            Ok(())
        };
        let client = client(&rt);
        let got = rt.block_on(token(&client, &o, true, browser));
        assert_eq!(got.as_deref(), Ok("imp"));
        let asked = query(&seen.lock().unwrap());
        assert_eq!(asked["response_type"], "token");
        assert!(!asked.contains_key("code_challenge"), "{asked:?}");
        assert!(first_page.lock().unwrap().contains("location.hash"));
        assert_eq!(hits.load(SeqCst), 0, "no token endpoint for this grant");
        assert_eq!(cached_token(&o).as_deref(), Some("imp"));
        TOKENS.lock().unwrap().remove(&cache_key(&o));
    }

    /// An expired token comes back through the refresh token, without the browser; once the
    /// provider refuses the refresh token, signing in again is the way back.
    #[test]
    fn an_expired_token_is_refreshed_without_signing_in_again() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let count = refreshes.clone();
        let addr = crate::http::tests::serve(move |req| {
            let body = req.split("\r\n\r\n").nth(1).unwrap_or_default();
            let form = query(&format!("http://x/?{body}"));
            let json = "200 OK\r\ncontent-type: application/json".to_owned();
            // Each token is born expired (expires_in 0), so every call needs a new one.
            match form["grant_type"].as_str() {
                "authorization_code" => (
                    json,
                    r#"{"access_token":"signed","expires_in":0,"refresh_token":"r1"}"#.into(),
                ),
                "refresh_token" if form["refresh_token"] == "r1" && form["client_id"] == "app" => {
                    match count.fetch_add(1, SeqCst) {
                        // No refresh_token in the answer: r1 stays in use.
                        0 | 1 => (
                            json,
                            r#"{"access_token":"refreshed","expires_in":0}"#.into(),
                        ),
                        _ => (
                            "400 Bad Request".into(),
                            r#"{"error":"invalid_grant"}"#.into(),
                        ),
                    }
                }
                _ => ("400 Bad Request".into(), format!("{form:?}")),
            }
        });
        let o = code_grant(format!("http://{addr}/token"));
        let client = client(&rt);
        let seen = Arc::new(Mutex::new(String::new()));
        let answer = |state: &str| format!("code=c0de&state={state}");
        let no_browser = |_: &str| -> Result<(), String> { panic!("signed in again") };

        let first = rt.block_on(token(&client, &o, false, browser(seen.clone(), answer)));
        assert_eq!(first.as_deref(), Ok("signed"));
        for _ in 0..2 {
            let next = rt.block_on(token(&client, &o, false, no_browser));
            assert_eq!(next.as_deref(), Ok("refreshed"));
        }
        let again = rt.block_on(token(&client, &o, false, browser(seen, answer)));
        assert_eq!(again.as_deref(), Ok("signed"), "refused refresh: sign in");
        assert_eq!(refreshes.load(SeqCst), 3);
    }

    #[test]
    fn a_foreign_or_refused_sign_in_never_reaches_the_token_endpoint() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let addr = crate::http::tests::serve(move |_| {
            count.fetch_add(1, SeqCst);
            ("200 OK".into(), r#"{"access_token":"t"}"#.into())
        });
        let o = code_grant(format!("http://{addr}/token"));
        let client = client(&rt);
        let seen = Arc::new(Mutex::new(String::new()));
        let stale = browser(seen.clone(), |_| "code=c0de&state=other".into());
        let err = rt.block_on(token(&client, &o, true, stale)).unwrap_err();
        assert!(err.contains("different sign-in"), "{err}");
        let refused = browser(seen, |_| {
            "error=access_denied&error_description=User+cancelled".into()
        });
        let err = rt.block_on(token(&client, &o, true, refused)).unwrap_err();
        assert!(err.contains("access_denied: User cancelled"), "{err}");
        assert_eq!(hits.load(SeqCst), 0);
        // A redirect to another machine is refused before any browser opens.
        let remote = OAuth2 {
            redirect_uri: "https://app.test/callback".into(),
            ..o
        };
        let err = rt.block_on(token(
            &client,
            &remote,
            true,
            |_: &str| -> Result<(), String> { panic!("opened a browser") },
        ));
        assert!(err.unwrap_err().contains("this machine"));
    }

    /// RFC 7616 §3.9.1. The RFC's printed MD5 response is a known erratum; this one was
    /// computed independently.
    #[test]
    fn digest_matches_rfc_7616_example() {
        let challenge = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=ALG, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let cnonce = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";
        for (alg, response) in [
            ("MD5", "8ca523f5e9506fed4657c9700eebdbec"),
            (
                "SHA-256",
                "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1",
            ),
        ] {
            let challenge = challenge.replace("ALG", alg);
            let header = digest(
                &challenge,
                "GET",
                "/dir/index.html",
                "Mufasa",
                "Circle of Life",
                cnonce,
            )
            .unwrap();
            assert!(
                header.contains(&format!(r#"response="{response}""#)),
                "{header}"
            );
            assert!(header.contains(r#"opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#));
            assert!(header.contains(&format!(r#"qop=auth, nc=00000001, cnonce="{cnonce}""#)));
        }
        assert!(digest(r#"Digest realm="r""#, "GET", "/", "u", "p", "c").is_err());
    }
}
