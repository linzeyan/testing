//! Hawk (mozilla/hawk), header form. Like SigV4 it signs the finished wire request: the
//! method, path and query, host and port exactly as they go out, and the body if asked.

use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::{digest, hmac};

use crate::model::Hawk;

pub const ALGORITHMS: [&str; 2] = ["sha256", "sha1"];

pub fn sign(req: &mut reqwest::Request, h: &Hawk, at: SystemTime) -> Result<(), String> {
    let ts = match h.timestamp.trim() {
        "" => (at.duration_since(SystemTime::UNIX_EPOCH))
            .unwrap_or_default()
            .as_secs()
            .to_string(),
        ts => ts.to_owned(),
    };
    let nonce = match h.nonce.trim() {
        "" => {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        nonce => nonce.to_owned(),
    };
    sign_with(req, h, &ts, &nonce)
}

fn sign_with(req: &mut reqwest::Request, h: &Hawk, ts: &str, nonce: &str) -> Result<(), String> {
    let (mac_alg, hash_alg) = match h.algorithm.trim() {
        "" | "sha256" => (hmac::HMAC_SHA256, &digest::SHA256),
        "sha1" => (
            hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            &digest::SHA1_FOR_LEGACY_USE_ONLY,
        ),
        other => return Err(format!("Hawk algorithm \"{other}\" isn't supported")),
    };
    let id = h.id.trim();
    if id.is_empty() || h.key.is_empty() {
        return Err("Hawk needs an ID and a key".into());
    }
    // As Postman: only a body there is; one streamed from a file or multipart isn't in
    // memory, so it goes unhashed.
    let body = req.body().and_then(|b| b.as_bytes()).unwrap_or_default();
    let hash = match h.payload_hash && !body.is_empty() {
        true => {
            let content_type = (req.headers().get(reqwest::header::CONTENT_TYPE))
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            let content_type = content_type.split(';').next().unwrap_or_default();
            let mut d = digest::Context::new(hash_alg);
            let head = format!("hawk.1.payload\n{}\n", content_type.trim().to_lowercase());
            d.update(head.as_bytes());
            d.update(body);
            d.update(b"\n");
            STANDARD.encode(d.finish())
        }
        false => String::new(),
    };
    let url = req.url();
    let resource = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let mut normalized = format!(
        "hawk.1.header\n{ts}\n{nonce}\n{}\n{resource}\n{}\n{}\n{hash}\n{}\n",
        req.method().as_str().to_uppercase(),
        url.host_str().unwrap_or_default().to_lowercase(),
        url.port_or_known_default().unwrap_or_default(),
        h.ext.replace('\\', "\\\\").replace('\n', "\\n"),
    );
    if !h.app.is_empty() {
        normalized.push_str(&format!("{}\n{}\n", h.app, h.dlg));
    }
    let key = hmac::Key::new(mac_alg, h.key.as_bytes());
    let mac = STANDARD.encode(hmac::sign(&key, normalized.as_bytes()));

    let mut header = format!("Hawk id=\"{id}\", ts=\"{ts}\", nonce=\"{nonce}\"");
    if !hash.is_empty() {
        header.push_str(&format!(", hash=\"{hash}\""));
    }
    if !h.ext.is_empty() {
        let ext = h.ext.replace('\\', "\\\\").replace('"', "\\\"");
        header.push_str(&format!(", ext=\"{ext}\""));
    }
    header.push_str(&format!(", mac=\"{mac}\""));
    if !h.app.is_empty() {
        header.push_str(&format!(", app=\"{}\"", h.app));
        if !h.dlg.is_empty() {
            header.push_str(&format!(", dlg=\"{}\"", h.dlg));
        }
    }
    let value = reqwest::header::HeaderValue::from_str(&header).map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert(reqwest::header::AUTHORIZATION, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "Protocol Example" in Hawk's API.md: its credentials, timestamp and nonce.
    fn example(method: &str) -> (reqwest::Request, Hawk) {
        let url = "http://example.com:8000/resource/1?b=1&a=2";
        let req = reqwest::Request::new(method.parse().unwrap(), url.parse().unwrap());
        let h = Hawk {
            id: "dh37fgj492je".into(),
            key: "werxhqb98rpaxn39848xrunpaw3489ruxnpa98w4rxn".into(),
            algorithm: "sha256".into(),
            ext: "some-app-ext-data".into(),
            ..Default::default()
        };
        (req, h)
    }

    fn header(req: &reqwest::Request) -> &str {
        req.headers()["authorization"].to_str().unwrap()
    }

    #[test]
    fn the_specs_example_signs_to_its_mac() {
        let (mut req, h) = example("GET");
        sign_with(&mut req, &h, "1353832234", "j4h3g2").unwrap();
        assert_eq!(
            header(&req),
            r#"Hawk id="dh37fgj492je", ts="1353832234", nonce="j4h3g2", ext="some-app-ext-data", mac="6R4rV5iE+NPoym+WwjeHzjAGXUtLNIxmo1vpMofpLAE=""#
        );
    }

    /// The spec's payload validation example: the hash covers the content type (without
    /// its parameters) and the body, and the MAC covers the hash.
    #[test]
    fn the_payload_hash_matches_the_specs_example() {
        let (mut req, mut h) = example("POST");
        h.payload_hash = true;
        let plain = "text/plain; charset=utf-8".parse().unwrap();
        req.headers_mut()
            .insert(reqwest::header::CONTENT_TYPE, plain);
        *req.body_mut() = Some("Thank you for flying Hawk".into());
        sign_with(&mut req, &h, "1353832234", "j4h3g2").unwrap();
        assert_eq!(
            header(&req),
            r#"Hawk id="dh37fgj492je", ts="1353832234", nonce="j4h3g2", hash="Yi9LfIIFRtBEPt74PVmbTF/xVAwPn7ub15ePICfgnuY=", ext="some-app-ext-data", mac="aSe1DERmZuRl3pI36/9BdZmnErTw3sNzOOAUlfeKjVw=""#
        );

        h.payload_hash = false;
        sign_with(&mut req, &h, "1353832234", "j4h3g2").unwrap();
        assert!(!header(&req).contains("hash="), "{}", header(&req));
    }

    #[test]
    fn fresh_values_and_mistakes() {
        let (mut req, mut h) = example("GET");
        h.app = "app1".into();
        h.dlg = "dlg1".into();
        sign(&mut req, &h, SystemTime::UNIX_EPOCH).unwrap();
        let first = header(&req).to_owned();
        assert!(first.contains(r#"ts="0""#), "{first}");
        assert!(first.ends_with(r#", app="app1", dlg="dlg1""#), "{first}");
        sign(&mut req, &h, SystemTime::UNIX_EPOCH).unwrap();
        assert_ne!(header(&req), first, "a fresh nonce per send");

        h.algorithm = "md5".into();
        let err = sign(&mut req, &h, SystemTime::now()).unwrap_err();
        assert!(err.contains("isn't supported"), "{err}");
        h.algorithm = "sha1".into();
        h.key.clear();
        assert!(
            sign(&mut req, &h, SystemTime::now())
                .unwrap_err()
                .contains("key")
        );
    }
}
