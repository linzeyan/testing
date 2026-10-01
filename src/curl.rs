//! curl commands in and out: "Copy as curl" for sharing a request, and pasting a curl
//! command (e.g. from browser devtools) into the URL bar to import one.

use crate::http::{build, error_chain};
use crate::model::{Auth, Body, KeyValue, Request};

/// A curl command for an already-resolved request. Derived from `http::build`, so it
/// carries exactly the headers, auth and body that Send would. POSIX shell quoting.
pub fn to_curl(req: Request) -> Result<String, String> {
    // Client::new panics without a provider, and nothing may have been sent yet.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let wire = build(&reqwest::Client::new(), req)?
        .build()
        .map_err(|e| error_chain(&e))?;
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let mut out = String::from("curl");
    match wire.method().as_str() {
        "GET" => {}
        // `-X HEAD` makes curl wait for a body that never comes.
        "HEAD" => out.push_str(" --head"),
        m => out.push_str(&format!(" -X {m}")),
    }
    out.push(' ');
    out.push_str(&quote(wire.url().as_str()));
    for (k, v) in wire.headers() {
        let line = format!("{k}: {}", String::from_utf8_lossy(v.as_bytes()));
        out.push_str(&format!(" \\\n  -H {}", quote(&line)));
    }
    if let Some(body) = wire.body().and_then(|b| b.as_bytes()) {
        let body = String::from_utf8_lossy(body);
        out.push_str(&format!(" \\\n  --data-raw {}", quote(&body)));
    }
    Ok(out)
}

/// Options whose value we don't use but must skip, so it isn't taken for the URL.
const IGNORED_WITH_VALUE: &[&str] = &[
    "-o",
    "--output",
    "-x",
    "--proxy",
    "-m",
    "--max-time",
    "--connect-timeout",
    "--cacert",
    "--capath",
    "-E",
    "--cert",
    "--key",
    "--cert-type",
    "--key-type",
    "-w",
    "--write-out",
    "-T",
    "--upload-file",
    "--retry",
    "-c",
    "--cookie-jar",
    "-r",
    "--range",
    "--resolve",
    "-U",
    "--proxy-user",
    "-K",
    "--config",
    "-D",
    "--dump-header",
    "--max-redirs",
    "--limit-rate",
    "--interface",
    "-y",
    "-Y",
    "-z",
    "-C",
    "-t",
    "-Q",
    "-P",
];

/// Short options that take a value, which curl also accepts glued on (`-XPOST`).
const SHORT_WITH_VALUE: &str = "XHdubeAFoxmEwTcrUKDyYzCtQP";

/// Parses a curl command (bash quoting, `$'…'`, or Chrome's Windows "Copy as cURL (cmd)").
pub fn from_curl(cmd: &str) -> Result<Request, String> {
    let mut words = split(cmd)?.into_iter();
    if words.next().as_deref() != Some("curl") {
        return Err("not a curl command".into());
    }
    let mut req = Request::default();
    let (mut url, mut method, mut head, mut get) = (None, None, false, false);
    let mut data: Vec<String> = Vec::new();
    while let Some(word) = words.next() {
        // `-XPOST` → (`-X`, `POST`)
        let (flag, glued) = match word.strip_prefix('-') {
            Some(rest)
                if !rest.starts_with('-')
                    && rest.len() > 1
                    && SHORT_WITH_VALUE.contains(&rest[..1]) =>
            {
                (format!("-{}", &rest[..1]), Some(rest[1..].to_owned()))
            }
            _ => (word.clone(), None),
        };
        let mut value = || {
            glued
                .clone()
                .or_else(|| words.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "-X" | "--request" => method = Some(value()?.to_uppercase()),
            "-H" | "--header" => {
                let line = value()?;
                let (k, v) = line.split_once(':').unwrap_or((&line, ""));
                req.headers.push(KeyValue::new(k.trim(), v.trim()));
            }
            "-d" | "--data" | "--data-ascii" | "--data-binary" => {
                let v = value()?;
                if v.starts_with('@') {
                    return Err(format!(
                        "{flag} {v} reads a file; paste its contents instead"
                    ));
                }
                data.push(v);
            }
            "--data-raw" => data.push(value()?),
            // `name=content` encodes only the content; `content` / `=content` send it bare.
            "--data-urlencode" => {
                let v = value()?;
                data.push(match v.split_once('=') {
                    Some((name, content)) if !name.is_empty() => {
                        format!("{name}={}", urlencode(content))
                    }
                    Some((_, content)) => urlencode(content),
                    None => urlencode(&v),
                });
            }
            "-F" | "--form" | "--form-string" => {
                return Err("multipart forms (-F) aren't supported yet".into());
            }
            "-u" | "--user" => {
                let v = value()?;
                let (user, pass) = v.split_once(':').unwrap_or((&v, ""));
                req.auth = Auth::Basic {
                    username: user.into(),
                    password: pass.into(),
                };
            }
            "-A" | "--user-agent" => req.headers.push(KeyValue::new("User-Agent", value()?)),
            "-e" | "--referer" => req.headers.push(KeyValue::new("Referer", value()?)),
            // Without `=` it names a cookie file, which we can't read.
            "-b" | "--cookie" => {
                let v = value()?;
                if v.contains('=') {
                    req.headers.push(KeyValue::new("Cookie", v));
                }
            }
            "--url" => url = Some(value()?),
            "-I" | "--head" => head = true,
            "-G" | "--get" => get = true,
            f if IGNORED_WITH_VALUE.contains(&f) => {
                value()?;
            }
            f if f.starts_with('-') && f.len() > 1 => {} // -s, -L, --compressed, -k, …
            _ if url.is_none() => url = Some(word),
            _ => return Err(format!("unexpected argument \"{word}\"")),
        }
    }
    let url = url.ok_or("no URL in the curl command")?;
    let data = (!data.is_empty()).then(|| data.join("&"));
    req.url = match (&data, get) {
        (Some(d), true) => format!("{url}{}{d}", if url.contains('?') { '&' } else { '?' }),
        _ => url,
    };
    req.method = match (method, data.is_some() && !get) {
        (Some(m), _) => m,
        (None, _) if head => "HEAD".into(),
        (None, true) => "POST".into(),
        (None, false) => "GET".into(),
    };
    if let (Some(text), false) = (data, get) {
        let ct = req
            .headers
            .iter()
            .position(|h| h.key.eq_ignore_ascii_case("content-type"));
        let ct_value = ct.map(|i| req.headers[i].value.to_lowercase());
        req.body = match ct_value.as_deref() {
            Some(t) if t.contains("json") => Body::Json { text },
            // curl's default type for -d is form-urlencoded.
            None | Some("application/x-www-form-urlencoded") => match form_decode(&text) {
                Some(fields) => {
                    if let Some(i) = ct {
                        req.headers.remove(i);
                    }
                    Body::Form { fields }
                }
                None => {
                    if ct.is_none() {
                        req.headers.push(KeyValue::new(
                            "Content-Type",
                            "application/x-www-form-urlencoded",
                        ));
                    }
                    Body::Text { text }
                }
            },
            Some(_) => Body::Text { text },
        };
    }
    req.sync_params();
    Ok(req)
}

/// `a=1&b=x%20y` → fields, or None if it isn't form data (e.g. JSON sent without a type).
fn form_decode(text: &str) -> Option<Vec<KeyValue>> {
    if !text
        .split('&')
        .all(|p| p.contains('=') && !p.starts_with('='))
    {
        return None;
    }
    // ponytail: borrows Url's form decoder rather than adding the form_urlencoded crate.
    let url = reqwest::Url::parse(&format!("http://x/?{text}")).ok()?;
    Some(
        url.query_pairs()
            .map(|(k, v)| KeyValue::new(k, v))
            .collect(),
    )
}

fn urlencode(s: &str) -> String {
    let mut url = reqwest::Url::parse("http://x/").expect("static URL");
    url.query_pairs_mut().append_pair("", s);
    url.query().unwrap_or_default()[1..].to_owned()
}

/// Shell words. A single-line paste turns `\`+newline into `\`+space, so a backslash
/// before whitespace is taken as a line continuation.
fn split(cmd: &str) -> Result<Vec<String>, String> {
    let cmd = if cmd.contains("^\"") || cmd.contains("^\n") {
        uncaret(cmd)
    } else {
        cmd.to_owned()
    };
    let unterminated = |q: char| format!("unterminated {q} quote in the curl command");
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\\' => match chars.next() {
                Some(n) if n.is_whitespace() => words.extend(word.take()),
                Some(n) => word.get_or_insert_default().push(n),
                None => {}
            },
            '\'' => {
                let w = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => w.push(ch),
                        None => return Err(unterminated('\'')),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => w.push(n),
                            Some('\n') => {}
                            Some(n) => {
                                w.push('\\');
                                w.push(n);
                            }
                            None => return Err(unterminated('"')),
                        },
                        Some(ch) => w.push(ch),
                        None => return Err(unterminated('"')),
                    }
                }
            }
            // Bash ANSI-C quoting; Chrome uses it when a body contains quotes or newlines.
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                let w = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => w.push('\n'),
                            Some('t') => w.push('\t'),
                            Some('r') => w.push('\r'),
                            Some(n @ ('x' | 'u' | 'U')) => {
                                let len = match n {
                                    'x' => 2,
                                    'u' => 4,
                                    _ => 8,
                                };
                                let mut hex = String::new();
                                while hex.len() < len
                                    && let Some(h) = chars.next_if(char::is_ascii_hexdigit)
                                {
                                    hex.push(h);
                                }
                                let ch =
                                    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                                w.push(ch.ok_or("bad \\x/\\u escape in the curl command")?);
                            }
                            Some(n) => {
                                if !matches!(n, '\\' | '\'' | '"' | '?') {
                                    w.push('\\');
                                }
                                w.push(n);
                            }
                            None => return Err(unterminated('\'')),
                        },
                        Some(ch) => w.push(ch),
                        None => return Err(unterminated('\'')),
                    }
                }
            }
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// Undoes cmd.exe caret escaping: `^X` is X, and `^`+newline continues the line.
/// ponytail: cmd's `%VAR%` expansion isn't emulated; devtools escapes `%` anyway.
fn uncaret(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        if c != '^' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\r') => {
                chars.next(); // the \n of \r\n
            }
            Some('\n') | None => {}
            Some(n) => out.push(n),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_export_carries_auth_body_and_survives_shell_quoting() {
        let req = Request {
            method: "POST".into(),
            url: "api.test/users?q=a b".into(),
            headers: vec![KeyValue::new("X-Note", "it's")],
            body: Body::Json {
                text: r#"{"name":"O'Brien"}"#.into(),
            },
            auth: Auth::Bearer {
                token: "t0k".into(),
            },
            ..Default::default()
        };
        let exported = to_curl(req).unwrap();
        assert_eq!(
            exported,
            r#"curl -X POST 'http://api.test/users?q=a%20b' \
  -H 'x-note: it'\''s' \
  -H 'authorization: Bearer t0k' \
  -H 'content-type: application/json' \
  --data-raw '{"name":"O'\''Brien"}'"#
        );
        // Importing what we export must reproduce the same wire request, including after
        // a single-line paste flattens the line breaks.
        let pasted = exported.replace('\n', " ");
        assert_eq!(to_curl(from_curl(&pasted).unwrap()).unwrap(), exported);

        let head = Request {
            method: "HEAD".into(),
            url: "http://x/".into(),
            ..Default::default()
        };
        assert_eq!(to_curl(head).unwrap(), "curl --head 'http://x/'");
    }

    #[test]
    fn chrome_copy_as_curl_imports_on_both_platforms() {
        // DevTools "Copy as cURL (bash)": $'…' when the body has a quote.
        let bash = r#"curl 'https://api.test/v1/items?page=2' \
  -H 'accept: application/json' \
  -H 'content-type: application/json' \
  --data-raw $'{"note":"it\'s\\n"}' \
  --compressed"#;
        // DevTools "Copy as cURL (cmd)" on Windows: carets and \" inside ^"…^".
        let cmd = "curl ^\"https://api.test/v1/items?page=2^\" ^\n  -H ^\"accept: application/json^\" ^\n  -H ^\"content-type: application/json^\" ^\n  --data-raw ^\"^{^\\^\"note^\\^\":^\\^\"it's^\\^\\n^\\^\"^}^\" ^\n  --compressed";
        for text in [bash, cmd] {
            let req = from_curl(text).unwrap();
            assert_eq!(req.method, "POST", "{text}");
            assert_eq!(req.url, "https://api.test/v1/items?page=2");
            assert_eq!(req.params, [KeyValue::new("page", "2")]);
            assert_eq!(req.headers[0], KeyValue::new("accept", "application/json"));
            assert_eq!(
                req.body,
                Body::Json {
                    text: r#"{"note":"it's\n"}"#.into()
                },
                "{text}"
            );
        }
    }

    #[test]
    fn curl_options_map_to_method_auth_and_body() {
        let req =
            from_curl("curl -sSL -XPUT -u bob:pw -A probe http://h/x -d a=1 -d 'b=x%20y'").unwrap();
        assert_eq!(req.method, "PUT");
        assert_eq!(
            req.auth,
            Auth::Basic {
                username: "bob".into(),
                password: "pw".into()
            }
        );
        assert_eq!(req.headers, [KeyValue::new("User-Agent", "probe")]);
        assert_eq!(
            req.body,
            Body::Form {
                fields: vec![KeyValue::new("a", "1"), KeyValue::new("b", "x y")]
            }
        );
        // -G moves the data into the query string.
        let req = from_curl("curl -G http://h/s -d q=rust --data-urlencode 'tag=a b'").unwrap();
        assert_eq!(
            (req.method.as_str(), req.url.as_str()),
            ("GET", "http://h/s?q=rust&tag=a+b")
        );
        // JSON sent without a type keeps curl's form content type and the exact bytes.
        let req = from_curl(r#"curl http://h -d '{"a":1}'"#).unwrap();
        assert_eq!(
            req.body,
            Body::Text {
                text: r#"{"a":1}"#.into()
            }
        );
        assert!(
            from_curl("curl http://h -d @body.json")
                .unwrap_err()
                .contains("paste")
        );
        assert!(from_curl("wget http://h").is_err());
    }
}
