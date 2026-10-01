//! Auth schemes that need a round trip: OAuth 2.0 fetches a token before the request,
//! Digest answers the server's 401 challenge.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::http::error_chain;
use crate::model::{Grant, OAuth2};

/// Tokens by grant parameters, for the life of the process (GUI session, CLI run, MCP server).
static TOKENS: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(Default::default);
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

/// A still-valid cached token, without any network traffic (for "Copy as curl").
pub fn cached_token(o: &OAuth2) -> Option<String> {
    let tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    tokens
        .get(&cache_key(o))
        .filter(|(_, until)| Instant::now() < *until)
        .map(|(t, _)| t.clone())
}

/// `fresh` skips the cache, after the API rejected the cached token.
pub async fn oauth2_token(
    client: &reqwest::Client,
    o: &OAuth2,
    fresh: bool,
) -> Result<String, String> {
    if !fresh && let Some(token) = cached_token(o) {
        return Ok(token);
    }
    let grant = match o.grant {
        Grant::ClientCredentials => "client_credentials",
        Grant::Password => "password",
    };
    let mut form = vec![("grant_type", grant), ("client_id", &o.client_id)];
    // client_secret_post: accepted by the common providers (Keycloak, Entra ID, Auth0, Okta).
    for (key, value) in [
        ("client_secret", &o.client_secret),
        ("scope", &o.scope),
        ("username", &o.username),
        ("password", &o.password),
    ] {
        let needed = o.grant == Grant::Password || !matches!(key, "username" | "password");
        if needed && !value.is_empty() {
            form.push((key, value));
        }
    }
    let resp = client
        .post(o.token_url.trim())
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
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
    let until = Instant::now() + ttl.saturating_sub(MARGIN);
    TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cache_key(o), (token.clone(), until));
    Ok(token)
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

pub fn cnonce() -> String {
    let mut bytes = [0u8; 16];
    // A fixed cnonce only weakens replay protection; never worth failing the request over.
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
