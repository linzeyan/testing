//! AWS Signature Version 4 (header form). Signs the finished wire request, so what is
//! signed is exactly what goes out: the URL as encoded, every header set by then, the body.

use std::time::SystemTime;

use ring::{digest, hmac};

use crate::model::AwsV4;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn mac(key: &[u8], data: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
        .as_ref()
        .to_vec()
}

/// RFC 3986 unreserved characters stay; everything else is %XX, upper case (AWS's rule).
pub(crate) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// "20150830T123600Z"
fn amz_date(at: SystemTime) -> String {
    let iso = crate::model::iso8601(
        at.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    );
    // 2015-08-30T12:36:00.000Z
    let (date, time) = (&iso[..10], &iso[11..19]);
    format!("{}T{}Z", date.replace('-', ""), time.replace(':', ""))
}

/// Adds `X-Amz-Date`, `X-Amz-Security-Token` (with a session token), `X-Amz-Content-Sha256`
/// (S3 only, which requires it) and `Authorization`.
pub fn sign(req: &mut reqwest::Request, a: &AwsV4, at: SystemTime) -> Result<(), String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let (region, service) = (a.region.trim(), a.service.trim());
    if a.access_key.trim().is_empty() || region.is_empty() || service.is_empty() {
        return Err("AWS Signature needs an access key, a region and a service".into());
    }
    let stamp = amz_date(at);
    let day = &stamp[..8];
    // No body hashes as empty; a body streamed from disk can't be hashed in advance, which
    // S3 accepts as UNSIGNED-PAYLOAD (other services may refuse it).
    let payload = match req.body() {
        None => sha256_hex(b""),
        Some(b) => b
            .as_bytes()
            .map_or("UNSIGNED-PAYLOAD".to_owned(), sha256_hex),
    };
    let s3 = service == "s3";
    let mut add = |name: &'static str, value: &str| -> Result<(), String> {
        let v = HeaderValue::from_str(value).map_err(|e| format!("{name}: {e}"))?;
        req.headers_mut().insert(HeaderName::from_static(name), v);
        Ok(())
    };
    add("x-amz-date", &stamp)?;
    if !a.session_token.trim().is_empty() {
        add("x-amz-security-token", a.session_token.trim())?;
    }
    if s3 {
        add("x-amz-content-sha256", &payload)?;
    }

    let url = req.url();
    // Other services check the path encoded twice (the SDKs encode the already-encoded
    // path); S3 once.
    let path = match url.path() {
        "" => "/".to_owned(),
        p if s3 => p
            .split('/')
            .map(|seg| {
                let raw = percent_decode(seg);
                uri_encode(&raw, false)
            })
            .collect::<Vec<_>>()
            .join("/"),
        p => uri_encode(p, true),
    };
    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k, false), uri_encode(&v, false)))
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let mut headers: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (k, v) in req.headers() {
        let v = String::from_utf8_lossy(v.as_bytes());
        let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
        headers.entry(k.as_str().to_owned()).or_default().push(v);
    }
    headers.entry("host".into()).or_insert_with(|| vec![host]);
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.join(",")))
        .collect();
    let signed = headers.keys().cloned().collect::<Vec<_>>().join(";");

    let canonical = format!(
        "{}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}",
        req.method().as_str()
    );
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let key = [day, region, service, "aws4_request"]
        .iter()
        .fold(format!("AWS4{}", a.secret_key).into_bytes(), |k, part| {
            mac(&k, part)
        });
    let signature = hex(&mac(&key, &to_sign));
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        a.access_key.trim()
    );
    let v = HeaderValue::from_str(&auth).map_err(|e| e.to_string())?;
    req.headers_mut().insert(reqwest::header::AUTHORIZATION, v);
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From AWS's SigV4 test suite (aws-sig-v4-test-suite): a server computes the same
    /// signature or answers 403, so these must match to the character.
    fn suite(method: &str, url: &str) -> String {
        let client = crate::http::OFFLINE.clone();
        let mut req = client
            .request(method.parse().unwrap(), url)
            .build()
            .unwrap();
        let a = AwsV4 {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
            service: "service".into(),
            session_token: String::new(),
        };
        // 2015-08-30T12:36:00Z
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160);
        sign(&mut req, &a, at).unwrap();
        assert_eq!(req.headers()["x-amz-date"], "20150830T123600Z");
        req.headers()["authorization"].to_str().unwrap().to_owned()
    }

    #[test]
    fn matches_the_aws_test_suite() {
        let cred = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=";
        for (method, url, sig) in [
            (
                "GET",
                "https://example.amazonaws.com/",
                "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            ),
            // get-vanilla-query-order-key-case: the query is sorted before signing.
            (
                "GET",
                "https://example.amazonaws.com/?Param2=value2&Param1=value1",
                "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
            ),
            // post-vanilla: no body hashes as the empty string.
            (
                "POST",
                "https://example.amazonaws.com/",
                "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            ),
        ] {
            assert_eq!(suite(method, url), format!("{cred}{sig}"), "{method} {url}");
        }
    }

    /// Send, snippets and the runner all build through `http::build`: signing must happen
    /// there, after the body is in place, or the server sees a signature over another body.
    #[test]
    fn requests_built_for_the_wire_are_signed_over_their_body() {
        let req = crate::model::Request {
            method: "POST".into(),
            url: "https://abc.execute-api.us-east-1.amazonaws.com/prod/items".into(),
            body: crate::model::Body::Json {
                text: "{\"a\":1}".into(),
            },
            auth: crate::model::Auth::AwsV4(AwsV4 {
                access_key: "AK".into(),
                secret_key: "SK".into(),
                region: "us-east-1".into(),
                service: "execute-api".into(),
                session_token: String::new(),
            }),
            ..Default::default()
        };
        let wire = crate::http::build(&crate::http::OFFLINE.clone(), req)
            .unwrap()
            .build()
            .unwrap();
        let auth = wire.headers()["authorization"].to_str().unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AK/"),
            "{auth}"
        );
        assert!(auth.contains("content-type;host;x-amz-date"), "{auth}");
        assert_eq!(wire.body().unwrap().as_bytes(), Some(&b"{\"a\":1}"[..]));
    }

    #[test]
    fn s3_and_session_tokens_add_their_headers() {
        let client = crate::http::OFFLINE.clone();
        let mut req = client
            .put("https://bucket.s3.amazonaws.com/a b.txt")
            .body("hi")
            .build()
            .unwrap();
        let a = AwsV4 {
            access_key: "AK".into(),
            secret_key: "SK".into(),
            region: "eu-west-1".into(),
            service: "s3".into(),
            session_token: "TOKEN".into(),
        };
        sign(&mut req, &a, SystemTime::now()).unwrap();
        let h = req.headers();
        assert_eq!(
            h["x-amz-content-sha256"],
            "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"
        );
        assert_eq!(h["x-amz-security-token"], "TOKEN");
        let auth = h["authorization"].to_str().unwrap();
        assert!(
            auth.contains(
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
            ),
            "{auth}"
        );
        let missing = AwsV4 {
            region: String::new(),
            ..a
        };
        assert!(sign(&mut req, &missing, SystemTime::now()).is_err());
    }
}
