//! OAuth 1.0a (RFC 5849), header form. Like SigV4 it signs the finished wire request: the
//! method, the URL, its query and a form body, exactly as they go out.

use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::hmac;

use crate::model::OAuth1;
use crate::sigv4::uri_encode;

pub const METHODS: [&str; 6] = [
    "HMAC-SHA1",
    "HMAC-SHA256",
    "HMAC-SHA512",
    "RSA-SHA256",
    "RSA-SHA512",
    "PLAINTEXT",
];

pub fn sign(req: &mut reqwest::Request, o: &OAuth1, at: SystemTime) -> Result<(), String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let stamp = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    sign_with(req, o, stamp.as_secs(), &nonce)
}

fn sign_with(req: &mut reqwest::Request, o: &OAuth1, at: u64, nonce: &str) -> Result<(), String> {
    let method = match o.signature_method.trim() {
        "" => "HMAC-SHA1",
        m => m,
    };
    if o.consumer_key.trim().is_empty() {
        return Err("OAuth 1.0 needs a consumer key".into());
    }
    let mut oauth = vec![
        ("oauth_consumer_key", o.consumer_key.trim().to_owned()),
        ("oauth_nonce", nonce.to_owned()),
        ("oauth_signature_method", method.to_owned()),
        ("oauth_timestamp", at.to_string()),
        ("oauth_version", "1.0".to_owned()),
    ];
    if !o.token.trim().is_empty() {
        oauth.push(("oauth_token", o.token.trim().to_owned()));
    }

    let url = req.url();
    let base_url = format!(
        "{}://{}{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port().map(|p| format!(":{p}")).unwrap_or_default(),
        url.path()
    );
    let mut params: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    let form = (req.headers().get(reqwest::header::CONTENT_TYPE)).is_some_and(|t| {
        t.as_bytes()
            .starts_with(b"application/x-www-form-urlencoded")
    });
    if let Some(body) = req.body().and_then(|b| b.as_bytes()).filter(|_| form) {
        // ponytail: borrows Url's form decoder, as curl.rs does.
        let body = String::from_utf8_lossy(body);
        let parsed =
            reqwest::Url::parse(&format!("http://x/?{body}")).map_err(|e| e.to_string())?;
        params.extend(parsed.query_pairs().map(|(k, v)| (k.into(), v.into())));
    }
    params.extend(oauth.iter().map(|(k, v)| (k.to_string(), v.clone())));
    let mut params: Vec<(String, String)> = (params.iter())
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    params.sort();
    let params = (params.iter().map(|(k, v)| format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join("&");
    let base = format!(
        "{}&{}&{}",
        req.method().as_str(),
        uri_encode(&base_url, false),
        uri_encode(&params, false)
    );

    let key = format!(
        "{}&{}",
        uri_encode(&o.consumer_secret, false),
        uri_encode(&o.token_secret, false)
    );
    let hmac = |a| {
        STANDARD.encode(hmac::sign(
            &hmac::Key::new(a, key.as_bytes()),
            base.as_bytes(),
        ))
    };
    let signature = match method {
        "HMAC-SHA1" => hmac(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY),
        "HMAC-SHA256" => hmac(hmac::HMAC_SHA256),
        "HMAC-SHA512" => hmac(hmac::HMAC_SHA512),
        // The consumer secret holds the PEM private key; JWT's RS* is the same signature.
        "RSA-SHA256" | "RSA-SHA512" => {
            let jwt = if method == "RSA-SHA256" {
                "RS256"
            } else {
                "RS512"
            };
            STANDARD.encode(crate::jwt::signature_of(
                jwt,
                &o.consumer_secret,
                base.as_bytes(),
            )?)
        }
        "PLAINTEXT" => key,
        other => {
            return Err(format!(
                "OAuth 1.0 signature method \"{other}\" isn't supported"
            ));
        }
    };
    oauth.push(("oauth_signature", signature));

    let mut header = String::from("OAuth ");
    if !o.realm.trim().is_empty() {
        header.push_str(&format!(
            "realm=\"{}\", ",
            uri_encode(o.realm.trim(), false)
        ));
    }
    let fields: Vec<String> = (oauth.iter())
        .map(|(k, v)| format!("{k}=\"{}\"", uri_encode(v, false)))
        .collect();
    header.push_str(&fields.join(", "));
    let value = reqwest::header::HeaderValue::from_str(&header).map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert(reqwest::header::AUTHORIZATION, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twitter's documented example ("Creating a signature"), the de facto test vector.
    fn twitter(method: &str) -> (reqwest::Request, OAuth1) {
        let url = "https://api.twitter.com/1.1/statuses/update.json?include_entities=true";
        let mut req = reqwest::Request::new(reqwest::Method::POST, url.parse().unwrap());
        let form = "application/x-www-form-urlencoded".parse().unwrap();
        req.headers_mut()
            .insert(reqwest::header::CONTENT_TYPE, form);
        *req.body_mut() =
            Some("status=Hello+Ladies+%2B+Gentlemen%2C+a+signed+OAuth+request%21".into());
        let o = OAuth1 {
            signature_method: method.into(),
            consumer_key: "xvz1evFS4wEEPTGEFPHBog".into(),
            consumer_secret: "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw".into(),
            token: "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".into(),
            token_secret: "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE".into(),
            realm: String::new(),
        };
        (req, o)
    }

    fn header(req: &reqwest::Request) -> String {
        req.headers()["authorization"].to_str().unwrap().to_owned()
    }

    #[test]
    fn hmac_sha1_matches_twitters_example() {
        let (mut req, o) = twitter("HMAC-SHA1");
        sign_with(
            &mut req,
            &o,
            1318622958,
            "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg",
        )
        .unwrap();
        let h = header(&req);
        // The query and the form body are both signed: without either it would differ.
        assert!(
            h.contains(r#"oauth_signature="hCtSmYh%2BiHYCEqBWrE7C7hYmtUk%3D""#),
            "{h}"
        );
        assert!(
            h.starts_with("OAuth oauth_consumer_key=\"xvz1evFS4wEEPTGEFPHBog\""),
            "{h}"
        );
        assert!(h.contains(r#"oauth_token="370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb""#));
    }

    #[test]
    fn other_methods_and_mistakes() {
        let (mut req, mut o) = twitter("PLAINTEXT");
        o.realm = "Photos".into();
        o.token_secret = "t s".into();
        sign_with(&mut req, &o, 1, "n").unwrap();
        let h = header(&req);
        assert!(h.starts_with(r#"OAuth realm="Photos", "#), "{h}");
        // The key is the two secrets, encoded, then encoded again for the header.
        assert!(
            h.contains(
                r#"oauth_signature="kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw%26t%2520s""#
            ),
            "{h}"
        );

        let (mut req, mut o) = twitter("HMAC-SHA256");
        o.token = String::new();
        sign_with(&mut req, &o, 1, "n").unwrap();
        assert!(
            !header(&req).contains("oauth_token"),
            "two-legged sends no token"
        );

        let (mut req, o) = twitter("RSA-SHA256");
        let err = sign_with(&mut req, &o, 1, "n").unwrap_err();
        assert!(err.contains("PEM private key"), "{err}");
        let (mut req, o) = twitter("RSA-SHA1");
        assert!(
            sign_with(&mut req, &o, 1, "n")
                .unwrap_err()
                .contains("isn't supported")
        );
        let (mut req, mut o) = twitter("");
        o.consumer_key = " ".into();
        assert!(
            sign_with(&mut req, &o, 1, "n")
                .unwrap_err()
                .contains("consumer key")
        );
    }
}
