//! HAR 1.2 (what browser devtools' "Save all as HAR" and proxies write) as a collection: a
//! folder per host, a request per entry, the response it got as a saved example.

use std::collections::HashSet;

use serde_json::Value;

use crate::model::{Body, Example, Folder, KeyValue, Request};
use crate::postman::Import;
use crate::store::{copy_name, safe_name};

pub fn is_har(v: &Value) -> bool {
    v["log"]["entries"].is_array() || is_entry_request(v)
}

/// One request copied out of a HAR on its own (devtools' "Copy as HAR" on a request).
fn is_entry_request(v: &Value) -> bool {
    v["httpVersion"].is_string() && v["method"].is_string() && v["url"].is_string()
}

/// Headers the client works out itself: HTTP/2 pseudo-headers, the length of the body it
/// sends, and an Accept-Encoding listing codings it may not decode.
const DROPPED: &[&str] = &["host", "content-length", "connection", "accept-encoding"];

pub fn import(root: &Value) -> Import {
    let mut warnings = Vec::new();
    let mut folders = vec![(String::new(), Folder::default())];
    let mut requests = Vec::new();
    let mut keys = HashSet::new();
    let single = [serde_json::json!({ "request": root })];
    let entries = match is_entry_request(root) {
        true => &single[..],
        false => root["log"]["entries"]
            .as_array()
            .map_or(&[][..], Vec::as_slice),
    };
    for (i, entry) in entries.iter().enumerate() {
        let r = &entry["request"];
        let url = str_of(&r["url"]);
        let Ok(parsed) = reqwest::Url::parse(url) else {
            warnings.push(format!("entry {}: URL \"{url}\" left out", i + 1));
            continue;
        };
        let folder = safe_name(parsed.host_str().unwrap_or("untitled"));
        if !folders.iter().any(|(k, _)| *k == folder) {
            folders.push((folder.clone(), Folder::default()));
        }
        let method = str_of(&r["method"]).to_uppercase();
        let path = parsed.path().trim_matches('/').replace('/', " ");
        let name = safe_name(&format!("{method} {path}"));
        let key = std::iter::once(name.clone())
            .chain((1..).map(|n| copy_name(&name, n)))
            .map(|n| format!("{folder}/{n}"))
            .find(|k| keys.insert(k.clone()))
            .expect("some name is free");

        let mut req = Request {
            url: url.to_owned(),
            headers: pairs(&r["headers"])
                .into_iter()
                .filter(|h| !h.key.starts_with(':') && !DROPPED.contains(&&*h.key.to_lowercase()))
                .collect(),
            ..Default::default()
        };
        if crate::model::METHODS.contains(&method.as_str()) {
            req.method = method;
        } else {
            warnings.push(format!(
                "{key}: method {method} isn't supported; set to GET"
            ));
        }
        req.body = body(&key, &r["postData"], &mut warnings);
        if let Some(example) = example(&entry["response"]) {
            req.examples.push(example);
        }
        req.sync_params();
        requests.push((key, req));
    }
    let name = match folders.get(1) {
        Some((host, _)) => host.clone(),
        None => "HAR".to_owned(),
    };
    Import::Collection {
        name,
        folders,
        requests,
        warnings,
    }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn pairs(v: &Value) -> Vec<KeyValue> {
    let rows = v.as_array().into_iter().flatten();
    rows.map(|p| KeyValue::new(str_of(&p["name"]), str_of(&p["value"])))
        .collect()
}

fn body(key: &str, post: &Value, warnings: &mut Vec<String>) -> Body {
    let mime = str_of(&post["mimeType"]).to_lowercase();
    let text = str_of(&post["text"]).to_owned();
    // Browsers fill `params` for forms, but not always `text` or even `mimeType`, and
    // multipart files only come with their name: the bytes aren't in the HAR.
    let params = post["params"].as_array().map_or(&[][..], Vec::as_slice);
    let file = params.iter().any(|p| p["fileName"].is_string());
    if !params.is_empty() && (mime.starts_with("multipart/form-data") || file) {
        let mut parts = Vec::new();
        for p in params {
            let mut part = KeyValue::new(str_of(&p["name"]), str_of(&p["value"]));
            if let Some(file) = p["fileName"].as_str() {
                warnings.push(format!(
                    "{key}: file part \"{}\" needs its file picked again",
                    part.key
                ));
                part.value = format!("@{file}");
            }
            parts.push(part);
        }
        return Body::Multipart { parts };
    }
    if !params.is_empty()
        && (mime.starts_with("application/x-www-form-urlencoded") || text.is_empty())
    {
        return Body::Form {
            fields: pairs(&post["params"]),
        };
    }
    match (text.is_empty(), mime.contains("json")) {
        (true, _) => Body::None,
        (false, true) => Body::Json { text },
        (false, false) => Body::Text { text },
    }
}

/// The response as the request's saved example, so the mock server can replay the session.
/// Binary bodies (base64 in the HAR) are left out: an example holds text.
fn example(res: &Value) -> Option<Example> {
    let status = res["status"].as_u64().filter(|s| (100..600).contains(s))? as u16;
    let content = &res["content"];
    let body = match str_of(&content["encoding"]) {
        "base64" => String::new(),
        _ => str_of(&content["text"]).to_owned(),
    };
    Some(Example {
        name: format!("{status} {}", str_of(&res["statusText"]))
            .trim()
            .to_owned(),
        status,
        content_type: str_of(&content["mimeType"]).to_owned(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a Chrome export: what a developer replays must go out as the browser
    /// sent it, minus what the client computes, and the answer it got must be there to compare.
    #[test]
    fn a_browser_session_becomes_requests_with_their_responses() {
        let har = serde_json::json!({"log": {"version": "1.2", "entries": [
            {"request": {"method": "POST", "url": "https://api.example.com/v1/items?draft=1",
                "headers": [
                    {"name": ":authority", "value": "api.example.com"},
                    {"name": "Content-Type", "value": "application/json"},
                    {"name": "Content-Length", "value": "7"},
                    {"name": "Authorization", "value": "Bearer t"}],
                "postData": {"mimeType": "application/json", "text": "{\"a\":1}"}},
             "response": {"status": 201, "statusText": "Created",
                "content": {"mimeType": "application/json", "text": "{\"id\":9}"}}},
            {"request": {"method": "POST", "url": "https://api.example.com/v1/items",
                "headers": [],
                "postData": {"mimeType": "multipart/form-data; boundary=x", "params": [
                    {"name": "title", "value": "hi"},
                    {"name": "file", "fileName": "a.png"}]}},
             "response": {"status": 0, "content": {}}},
            {"request": {"method": "GET", "url": "https://cdn.example.com/logo.png", "headers": []},
             "response": {"status": 200, "statusText": "OK",
                "content": {"mimeType": "image/png", "text": "iVBO", "encoding": "base64"}}},
            {"request": {"method": "GET", "url": "not a url"}}
        ]}});
        assert!(is_har(&har));
        let Import::Collection {
            name,
            folders,
            requests,
            warnings,
        } = import(&har)
        else {
            panic!("a collection")
        };
        assert_eq!(name, "api.example.com");
        let folders: Vec<_> = folders.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(folders, ["", "api.example.com", "cdn.example.com"]);
        let keys: Vec<_> = requests.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "api.example.com/POST v1 items",
                "api.example.com/POST v1 items copy",
                "cdn.example.com/GET logo.png"
            ]
        );

        let first = &requests[0].1;
        let headers: Vec<_> = first.headers.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(headers, ["Content-Type", "Authorization"]);
        assert_eq!(first.params[0].key, "draft");
        assert_eq!(
            first.body,
            Body::Json {
                text: "{\"a\":1}".into()
            }
        );
        assert_eq!(first.examples[0].name, "201 Created");
        assert_eq!(first.examples[0].body, "{\"id\":9}");

        let Body::Multipart { parts } = &requests[1].1.body else {
            panic!("multipart")
        };
        assert_eq!(parts[1].value, "@a.png");
        assert!(requests[1].1.examples.is_empty(), "no answer, no example");
        // An image's base64 would be garbage as a text example.
        assert_eq!(requests[2].1.examples[0].body, "");
        assert_eq!(
            warnings,
            [
                "api.example.com/POST v1 items copy: file part \"file\" needs its file picked again",
                "entry 4: URL \"not a url\" left out"
            ]
        );
    }

    /// Devtools' "Copy as HAR" on one request gives the bare request object; its form has
    /// `params` but no `mimeType`.
    #[test]
    fn a_single_copied_request_comes_over_too() {
        let one = r#"{"method": "POST", "url": "https://x.test/login", "httpVersion": "HTTP/2",
            "postData": {"params": [{"name": "user", "value": "me"}]}}"#;
        let Ok(Import::Collection { requests, .. }) = crate::import::parse(one) else {
            panic!("a collection")
        };
        let (key, req) = &requests[0];
        assert_eq!(key, "x.test/POST login");
        assert_eq!(
            req.body,
            Body::Form {
                fields: vec![KeyValue::new("user", "me")]
            }
        );
    }
}
