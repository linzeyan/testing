//! Akamai EdgeGrid (EG1-HMAC-SHA256). Like SigV4 it signs the finished wire request: the
//! method, scheme, host, path and query, the chosen headers, and a POST's body.

use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::{digest, hmac};

use crate::model::EdgeGrid;

/// What the reference clients (and Postman) sign of a POST body unless told otherwise.
const MAX_BODY: usize = 128 << 10;

pub fn sign(req: &mut reqwest::Request, e: &EdgeGrid, at: SystemTime) -> Result<(), String> {
    let timestamp = match e.timestamp.trim() {
        // 2014-03-21T19:34:21.000Z → 20140321T19:34:21+0000
        "" => {
            let iso = crate::model::iso8601(
                (at.duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default(),
            );
            format!("{}T{}+0000", iso[..10].replace('-', ""), &iso[11..19])
        }
        t => t.to_owned(),
    };
    let nonce = match e.nonce.trim() {
        // A random UUID, which Akamai recommends.
        "" => {
            let mut b = [0u8; 16];
            getrandom::fill(&mut b).map_err(|e| e.to_string())?;
            (b[6], b[8]) = (b[6] & 0x0f | 0x40, b[8] & 0x3f | 0x80);
            let h: String = b.iter().map(|b| format!("{b:02x}")).collect();
            format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )
        }
        n => n.to_owned(),
    };
    sign_with(req, e, &timestamp, &nonce)
}

fn sign_with(
    req: &mut reqwest::Request,
    e: &EdgeGrid,
    timestamp: &str,
    nonce: &str,
) -> Result<(), String> {
    let (access, client, secret) = (
        e.access_token.trim(),
        e.client_token.trim(),
        e.client_secret.trim(),
    );
    if access.is_empty() || client.is_empty() || secret.is_empty() {
        return Err("EdgeGrid needs an access token, a client token and a client secret".into());
    }
    let max_body = match e.max_body.trim() {
        "" => MAX_BODY,
        n => {
            (n.parse()).map_err(|_| format!("EdgeGrid max body \"{n}\" isn't a number of bytes"))?
        }
    };
    let host_of = |url: &reqwest::Url| {
        let host = url.host_str().unwrap_or_default();
        url.port()
            .map_or_else(|| host.to_owned(), |p| format!("{host}:{p}"))
    };
    let url = req.url();
    let host = match e.base_url.trim() {
        "" => host_of(url),
        base => host_of(
            &reqwest::Url::parse(&crate::http::wire_url(base))
                .map_err(|err| format!("EdgeGrid base URL \"{base}\": {err}"))?,
        ),
    };
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    // In the order given; one not sent is left out, and runs of spaces count as one.
    let headers: Vec<String> = (e.headers_to_sign.split(','))
        .map(|name| name.trim().to_lowercase())
        .filter_map(|name| {
            let value = req.headers().get(name.as_str())?.to_str().ok()?;
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            (!value.is_empty()).then(|| format!("{name}:{value}"))
        })
        .collect();
    // Only a POST's body is signed, at most `max_body` of it; one streamed from a file
    // isn't in memory, so it goes unsigned.
    let body = req.body().and_then(|b| b.as_bytes()).unwrap_or_default();
    let content_hash = match req.method() == reqwest::Method::POST && !body.is_empty() {
        true => {
            let signed = &body[..body.len().min(max_body)];
            STANDARD.encode(digest::digest(&digest::SHA256, signed))
        }
        false => String::new(),
    };
    let auth = format!(
        "EG1-HMAC-SHA256 client_token={client};access_token={access};timestamp={timestamp};nonce={nonce};"
    );
    let data = [
        req.method().as_str(),
        url.scheme(),
        &host,
        &path,
        &headers.join("\t"),
        &content_hash,
        &auth,
    ]
    .join("\t");
    let mac = |key: &[u8], data: &str| {
        STANDARD.encode(hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, key),
            data.as_bytes(),
        ))
    };
    // The signing key is the base64 text, not the bytes it stands for.
    let signing_key = mac(secret.as_bytes(), timestamp);
    let header = format!("{auth}signature={}", mac(signing_key.as_bytes(), &data));
    let value = reqwest::header::HeaderValue::from_str(&header).map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert(reqwest::header::AUTHORIZATION, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cases from the reference clients' shared suite (AkamaiOPEN-edgegrid-python's
    /// testdata.json and testcases.json): an Akamai server computes the same signature or
    /// refuses the request, so each must match to the character.
    fn signed(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
        let url =
            format!("https://akaa-baseurl-xxxxxxxxxxx-xxxxxxxxxxxxx.luna.akamaiapis.net{path}");
        let mut req = reqwest::Request::new(method.parse().unwrap(), url.parse().unwrap());
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap();
            (req.headers_mut()).insert(name, reqwest::header::HeaderValue::from_str(v).unwrap());
        }
        if !body.is_empty() {
            *req.body_mut() = Some(body.to_owned().into());
        }
        let e = EdgeGrid {
            access_token: "akab-access-token-xxx-xxxxxxxxxxxxxxxx".into(),
            client_token: "akab-client-token-xxx-xxxxxxxxxxxxxxxx".into(),
            client_secret: "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx=".into(),
            headers_to_sign: "X-Test1, X-Test2,X-Test3".into(),
            max_body: "2048".into(),
            ..Default::default()
        };
        let (timestamp, nonce) = (
            "20140321T19:34:21+0000",
            "nonce-xx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
        );
        sign_with(&mut req, &e, timestamp, nonce).unwrap();
        let header = req.headers()["authorization"].to_str().unwrap();
        let prefix = "EG1-HMAC-SHA256 client_token=akab-client-token-xxx-xxxxxxxxxxxxxxxx;access_token=akab-access-token-xxx-xxxxxxxxxxxxxxxx;timestamp=20140321T19:34:21+0000;nonce=nonce-xx-xxxx-xxxx-xxxx-xxxxxxxxxxxx;signature=";
        header.strip_prefix(prefix).unwrap().to_owned()
    }

    #[test]
    fn the_reference_suite_signs_alike() {
        type Headers<'a> = &'a [(&'a str, &'a str)];
        // GETs, so no body.
        let cases: &[(&str, &str, Headers, &str)] = &[
            (
                "simple GET",
                "/",
                &[],
                "tL+y4hxyHxgWVD30X3pWnGKHcPzmrIF+LThiAOhMxYU=",
            ),
            (
                "GET with querystring",
                "/testapi/v1/t1?p1=1&p2=2",
                &[],
                "hKDH1UlnQySSHjvIcZpDMbQHihTQ0XyVAKZaApabdeA=",
            ),
            (
                "Simple header signing with GET",
                "/testapi/v1/t4",
                &[("X-Test1", "test-simple-header")],
                "8F9AybcRw+PLxnvT+H0JRkjROrrUgsxJTnRXMzqvcwY=",
            ),
            (
                "Header containing spaces",
                "/testapi/v1/t4",
                &[("X-Test1", "\"     test-header-with-spaces     \"")],
                "ucq2AbjCNtobHfCTuS38fdkl5UDdWHZhQX46fYR8CqI=",
            ),
            (
                "Header with leading and interior spaces",
                "/testapi/v1/t4",
                &[("X-Test1", "     first-thing      second-thing")],
                "WtnneL539UadAAOJwnsXvPqT4Kt6z7HMgBEwAFpt3+c=",
            ),
            (
                "Headers out of order",
                "/testapi/v1/t4",
                &[("X-Test2", "t2"), ("X-Test1", "t1"), ("X-Test3", "t3")],
                "Wus73Nx8jOYM+kkBFF2q8D1EATRIMr0WLWwpLBgkBqY=",
            ),
            (
                "Extra header",
                "/testapi/v1/t5",
                &[
                    ("X-Test2", "t2"),
                    ("X-Test1", "t1"),
                    ("X-Test3", "t3"),
                    ("X-Extra", "this won't be included"),
                ],
                "Knd/jc0A5Ghhizjayr0AUUvl2MZjBpS3FDSzvtq4Ixc=",
            ),
            (
                "GET with query params and separator in path",
                "/testapi/v1/configs/111;222;333?from=12345&limit=200000",
                &[],
                "pmQF7Is2+O4r/mMojPR4yeF58BrempNNoBX5/DT0Fxs=",
            ),
        ];
        for (name, path, headers, signature) in cases {
            assert_eq!(signed("GET", path, headers, ""), *signature, "{name}");
        }
    }

    /// Only a POST's body is signed, and only its first `max_body` bytes.
    #[test]
    fn bodies_are_signed_for_post_only_and_up_to_max_body() {
        let post = |body: &str| signed("POST", "/testapi/v1/t3", &[], body);
        assert_eq!(
            post("datadatadatadatadatadatadatadata"),
            "hXm4iCxtpN22m4cbZb4lVLW5rhX8Ca82vCFqXzSTPe4="
        );
        let cut = "6Q6PiTipLae6n4GsSIDTCJ54bEbHUBp+4MUXrbQCBoY=";
        assert_eq!(post(&"d".repeat(2049)), cut, "POST too large");
        assert_eq!(post(&"d".repeat(2048)), cut, "POST length equals max_body");
        let empty = signed("POST", "/testapi/v1/t6", &[], "");
        assert_eq!(empty, "1gEDxeQGD5GovIkJJGcBaKnZ+VaPtrc4qBUHixjsPCQ=");
        let put = signed("PUT", "/testapi/v1/t6", &[], &"P".repeat(31));
        assert_eq!(put, "GNBWEYSEWOLtu+7dD52da2C39aX/Jchpon3K/AmBqBU=");
        let patch = signed("PATCH", "/testapi/v1/t6", &[], &"P".repeat(31));
        assert_eq!(patch, "JIl05ImY1AOnMtmw+9LKgaFA8mnzsEKabbnHmI8LsQ4=");
    }

    #[test]
    fn the_base_url_host_is_signed_and_mistakes_say_so() {
        let e = EdgeGrid {
            access_token: "a".into(),
            client_token: "c".into(),
            client_secret: "s".into(),
            ..Default::default()
        };
        let at = |url: &str, e: &EdgeGrid| {
            let mut req = reqwest::Request::new(reqwest::Method::GET, url.parse().unwrap());
            sign_with(&mut req, e, "t", "n")
                .map(|()| req.headers()["authorization"].to_str().unwrap().to_owned())
        };
        // Sent to a local stand-in, signed as if to Akamai's host.
        let through = EdgeGrid {
            base_url: "https://akab-host.luna.akamaiapis.net/".into(),
            ..e.clone()
        };
        let direct = at("https://akab-host.luna.akamaiapis.net/x", &e).unwrap();
        assert_eq!(at("https://127.0.0.1:8443/x", &through).unwrap(), direct);
        assert_ne!(at("https://127.0.0.1:8443/x", &e).unwrap(), direct);

        let bad = EdgeGrid {
            max_body: "lots".into(),
            ..e.clone()
        };
        assert!(at("https://h/", &bad).unwrap_err().contains("max body"));
        let missing = EdgeGrid {
            client_secret: " ".into(),
            ..e
        };
        assert!(
            at("https://h/", &missing)
                .unwrap_err()
                .contains("client secret")
        );
    }
}
