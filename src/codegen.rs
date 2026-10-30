//! Code snippets for a request, in the tools and languages people paste into, like
//! Postman's code panel. Built from the resolved request (variables filled in) through
//! `http::build`, so every snippet carries the headers, auth and body that Send would.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use crate::http::{OFFLINE, build, error_chain};
use crate::model::{Auth, Body, HttpVersion, KeyValue, Request, Settings, is_streaming};
use crate::store::Workspace;

type Generator = fn(&Wire) -> String;

/// Name shown in the picker, and the generator.
pub const TARGETS: &[(&str, Generator)] = &[
    ("cURL", curl),
    ("wget", wget),
    ("HTTPie", httpie),
    ("PowerShell", powershell),
    ("HTTP", raw),
    ("Python (requests)", python),
    ("JavaScript (fetch)", fetch),
    ("Node.js (axios)", axios),
    ("Go (net/http)", go),
    ("Java (HttpClient)", java),
    ("C# (HttpClient)", csharp),
    ("PHP (cURL)", php),
    ("Ruby (Net::HTTP)", ruby),
    ("Rust (reqwest)", rust),
    ("Swift (URLSession)", swift),
    ("Kotlin (OkHttp)", kotlin),
];

/// The snippet targets for a request of this kind: HTTP's, or a stream's own (SSE,
/// WebSocket, Socket.IO, MQTT, gRPC), in picker order.
pub fn targets(method: &str) -> Vec<&'static str> {
    match stream_targets(method) {
        Some(list) => list.iter().map(|(name, _)| *name).collect(),
        None => TARGETS.iter().map(|(name, _)| *name).collect(),
    }
}

/// The target shown for `method` when `preferred` was picked last: itself, else the same
/// language for this kind (Python (requests) → Python (websockets)), else the first.
pub fn pick(method: &str, preferred: &str) -> &'static str {
    let list = targets(method);
    let language = |name: &str| name.split(" (").next().unwrap_or(name).to_owned();
    (list.iter().find(|name| **name == preferred))
        .or_else(|| {
            list.iter()
                .find(|name| language(name) == language(preferred))
        })
        .unwrap_or(&list[0])
}

/// `req` must be resolved (`Request::resolved`).
pub fn generate(target: &str, req: Request) -> Result<String, String> {
    if let Some(list) = stream_targets(&req.method) {
        let (_, generator) = (list.iter().find(|(name, _)| *name == target))
            .ok_or_else(|| format!("no {target} snippet for {}", req.method))?;
        return Ok(generator(&Stream::new(req)?));
    }
    let (_, generator) = TARGETS
        .iter()
        .find(|(name, _)| *name == target)
        .ok_or_else(|| format!("no code generator named {target}"))?;
    Ok(generator(&Wire::new(req)?))
}

/// What goes on the wire, in a form every generator can render.
pub struct Wire {
    method: String,
    url: reqwest::Url,
    /// Lowercase names, auth and content type included (multipart's is left to each
    /// language, which picks its own boundary).
    headers: Vec<(String, String)>,
    body: Option<String>,
    /// A Binary body: the snippet reads this file, as Send does.
    file: Option<String>,
    parts: Vec<(String, Part)>,
    challenge: Option<Challenge>,
    settings: Settings,
}

/// Auth that answers the server's 401 challenge: done by the tool itself where it can, as
/// curl --digest and --ntlm do.
struct Challenge {
    /// "Digest" or "NTLM", as the snippets' comments name it.
    scheme: &'static str,
    user: String,
    pass: String,
    /// NTLM's, for the APIs that take it apart; `login` folds it in for the rest.
    domain: String,
}

impl Challenge {
    fn ntlm(&self) -> bool {
        self.scheme == "NTLM"
    }

    /// `DOMAIN\user`, as curl, wget, PowerShell, requests_ntlm and PHP take an NTLM login.
    fn login(&self) -> String {
        match self.domain.as_str() {
            "" => self.user.clone(),
            domain => format!("{domain}\\{}", self.user),
        }
    }
}

enum Part {
    Text(String),
    File(String),
}

impl Wire {
    fn new(mut req: Request) -> Result<Self, String> {
        // Taken out before `build`, which would open the file; typed as Send types it.
        let file = match std::mem::take(&mut req.body) {
            Body::File { path } if path.trim().is_empty() => {
                return Err("Pick a file to send as the body".into());
            }
            Body::File { path } => {
                let own = (req.headers.iter())
                    .any(|h| h.enabled && h.key.eq_ignore_ascii_case("content-type"));
                if !own {
                    let mime = mime_guess::from_path(&path).first_or_octet_stream();
                    req.headers
                        .push(KeyValue::new("Content-Type", mime.as_ref()));
                }
                Some(path)
            }
            body => {
                req.body = body;
                None
            }
        };
        let parts = match std::mem::take(&mut req.body) {
            Body::Multipart { parts } => parts
                .into_iter()
                .filter(|p| p.enabled && !p.key.is_empty())
                .map(|p| match p.value.strip_prefix('@') {
                    Some(path) => (p.key, Part::File(path.to_owned())),
                    None => (p.key, Part::Text(p.value)),
                })
                .collect(),
            body => {
                req.body = body;
                Vec::new()
            }
        };
        // OAuth 2.0 needs a token fetched by Send; Digest and NTLM the server's challenge.
        let mut challenge = None;
        match std::mem::take(&mut req.auth) {
            Auth::Digest { username, password } => {
                challenge = Some(Challenge {
                    scheme: "Digest",
                    user: username,
                    pass: password,
                    domain: String::new(),
                })
            }
            Auth::Ntlm(n) => {
                challenge = Some(Challenge {
                    scheme: "NTLM",
                    user: n.username,
                    pass: n.password,
                    domain: n.domain,
                })
            }
            Auth::OAuth2(o) => req.auth = fetched_token(&o),
            auth => req.auth = auth,
        }
        let settings = req.settings.clone();
        let wire = build(&OFFLINE, req)?.build().map_err(|e| error_chain(&e))?;
        let headers = wire
            .headers()
            .iter()
            .map(|(k, v)| {
                let v = String::from_utf8_lossy(v.as_bytes()).into_owned();
                (k.as_str().to_owned(), v)
            })
            .collect();
        let body = match wire.body().and_then(|b| b.as_bytes()) {
            // Typed bytes (Base64, hex) that aren't text: no snippet can hold them as a
            // literal, so nothing is made rather than something that sends other bytes.
            Some(b) if std::str::from_utf8(b).is_err() => {
                return Err("This body's bytes aren't text, which a snippet can't hold. Save them to a file and pick it under Binary > File.".into());
            }
            b => b.map(|b| String::from_utf8_lossy(b).into_owned()),
        };
        Ok(Self {
            method: wire.method().as_str().to_owned(),
            url: wire.url().clone(),
            headers,
            body,
            file,
            parts,
            challenge,
            settings,
        })
    }

    fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Headers except the content type, for languages that set it on the body.
    fn other_headers(&self) -> impl Iterator<Item = &(String, String)> {
        self.headers.iter().filter(|(k, _)| k != "content-type")
    }

    fn file_name(path: &str) -> &str {
        path.rsplit(['/', '\\']).next().unwrap_or(path)
    }

    fn follows(&self) -> bool {
        self.settings.follow_redirects
    }

    fn timeout_secs(&self) -> Option<String> {
        let ms = self.settings.timeout_ms;
        (ms > 0).then(|| match ms % 1000 {
            0 => (ms / 1000).to_string(),
            _ => format!("{}", ms as f64 / 1000.0),
        })
    }

    fn insecure(&self) -> bool {
        !self.settings.verify_tls
    }

    fn https(&self) -> bool {
        self.url.scheme() == "https"
    }
}

// --- String literals ---------------------------------------------------------------

/// POSIX shell single quotes.
fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A double-quoted literal with JSON escapes, which Python, JavaScript, Go, Java and C#
/// all read the same way.
fn dq(s: &str) -> String {
    serde_json::to_string(s).expect("strings serialize")
}

/// Swift and Rust: no `\b`/`\f`, and `\u{…}` instead of `\uXXXX`.
fn braced(s: &str) -> String {
    escaped(s, '"', |c| format!("\\u{{{:x}}}", c as u32))
}

/// Kotlin: no `\f`, and `$` starts a template.
fn kotlin_str(s: &str) -> String {
    escaped(s, '$', |c| format!("\\u{:04x}", c as u32))
}

/// A double-quoted literal escaping `"`, `\`, `extra`, the usual `\n\r\t`, and other
/// control characters through `unicode`.
fn escaped(s: &str, extra: char, unicode: fn(char) -> String) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c == extra => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&unicode(c)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn php_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn ps_str(s: &str) -> String {
    let mut out = String::from('\'');
    for c in s.chars() {
        // PowerShell also ends a quote at the typographic ones; doubling escapes any.
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Ruby interpolates `#{`, `#@` and `#$` inside double quotes.
fn ruby_str(s: &str) -> String {
    dq(s).replace('#', "\\#")
}

// --- Command-line tools ------------------------------------------------------------

/// POSIX shell quoting.
fn curl(w: &Wire) -> String {
    let mut out = String::from("curl");
    let s = &w.settings;
    // curl stays on a 3xx unless told; Send follows by default.
    if w.follows() {
        out.push_str(match s.follow_auth_header {
            true => " --location-trusted",
            false => " --location",
        });
        if s.max_redirects != Settings::default().max_redirects {
            let _ = write!(out, " --max-redirs {}", s.max_redirects);
        }
        if s.follow_original_method {
            out.push_str(" --post301 --post302 --post303");
        }
    }
    match w.method.as_str() {
        "GET" => {}
        // `-X HEAD` makes curl wait for a body that never comes.
        "HEAD" => out.push_str(" --head"),
        m => {
            let _ = write!(out, " -X {m}");
        }
    }
    if w.insecure() {
        out.push_str(" --insecure");
    }
    if s.disable_tls12 {
        out.push_str(" --tlsv1.3");
    }
    if s.disable_tls13 {
        out.push_str(" --tls-max 1.2");
    }
    // curl takes TLS 1.3 suites apart; a name apitool doesn't know goes with the others.
    let (tls13, tls12): (Vec<&str>, Vec<&str>) = (s.ciphers.iter().map(String::as_str))
        .partition(|n| crate::net::cipher(n).is_some_and(|c| c.1.tls13().is_some()));
    if !tls13.is_empty() {
        let _ = write!(out, " --tls13-ciphers {}", sh(&tls13.join(":")));
    }
    if !tls12.is_empty() {
        let _ = write!(out, " --ciphers {}", sh(&tls12.join(":")));
    }
    match (w.settings.http_version, w.https()) {
        (HttpVersion::Auto, _) => {}
        (HttpVersion::Http1, _) => out.push_str(" --http1.1"),
        (HttpVersion::Http2, true) => out.push_str(" --http2"),
        (HttpVersion::Http2, false) => out.push_str(" --http2-prior-knowledge"),
    }
    if let Some(s) = w.timeout_secs() {
        let _ = write!(out, " --max-time {s}");
    }
    if let Some(c) = &w.challenge {
        let flag = if c.ntlm() { "--ntlm" } else { "--digest" };
        let login = sh(&format!("{}:{}", c.login(), c.pass));
        let _ = write!(out, " {flag} -u {login}");
    }
    let _ = write!(out, " {}", sh(w.url.as_str()));
    for (k, v) in &w.headers {
        let _ = write!(out, " \\\n  -H {}", sh(&format!("{k}: {v}")));
    }
    if let Some(body) = &w.body {
        let _ = write!(out, " \\\n  --data-raw {}", sh(body));
    }
    if let Some(path) = &w.file {
        let _ = write!(out, " \\\n  --data-binary {}", sh(&format!("@{path}")));
    }
    for (key, part) in &w.parts {
        // --form-string sends text as is; -F would treat `;` and a leading `<` specially.
        let (flag, part) = match part {
            Part::File(path) => ("-F", format!("{key}=@{path}")),
            Part::Text(text) => ("--form-string", format!("{key}={text}")),
        };
        let _ = write!(out, " \\\n  {flag} {}", sh(&part));
    }
    out
}

fn wget(w: &Wire) -> String {
    let mut out = String::new();
    let multipart = !w.parts.is_empty();
    // wget has no multipart option: the body is written to a file first and sent from it.
    // printf's arguments are taken as they are, so values need no escaping beyond sh().
    if multipart {
        out.push_str("boundary=\"Boundary-$RANDOM$RANDOM\"\nbody=$(mktemp)\n{\n");
        let head = r#"--%s\r\nContent-Disposition: form-data; name="%s""#;
        for (key, p) in &w.parts {
            let _ = match p {
                Part::Text(text) => writeln!(
                    out,
                    "  printf -- '{head}\\r\\n\\r\\n%s\\r\\n' \"$boundary\" {} {}",
                    sh(key),
                    sh(text)
                ),
                Part::File(path) => writeln!(
                    out,
                    "  printf -- '{head}; filename=\"%s\"\\r\\n\\r\\n' \"$boundary\" {} {}\n  cat {}\n  printf '\\r\\n'",
                    sh(key),
                    sh(Wire::file_name(path)),
                    sh(path)
                ),
            };
        }
        out.push_str("  printf -- '--%s--\\r\\n' \"$boundary\"\n} > \"$body\"\n");
    }
    out.push_str("wget --no-verbose --output-document -");
    if w.method != "GET" {
        let _ = write!(out, " --method {}", w.method);
    }
    // wget follows up to 20 redirects on its own.
    match (w.follows(), w.settings.max_redirects) {
        (false, _) => out.push_str(" --max-redirect 0"),
        (true, 10) => {}
        (true, n) => {
            let _ = write!(out, " --max-redirect {n}");
        }
    }
    if w.insecure() {
        out.push_str(" --no-check-certificate");
    }
    if let Some(s) = w.timeout_secs() {
        let _ = write!(out, " --timeout {s}");
    }
    if let Some(c) = &w.challenge {
        let _ = write!(out, " --user {} --password {}", sh(&c.login()), sh(&c.pass));
    }
    for (k, v) in &w.headers {
        let _ = write!(out, " \\\n  --header {}", sh(&format!("{k}: {v}")));
    }
    if let Some(body) = &w.body {
        let _ = write!(out, " \\\n  --body-data {}", sh(body));
    }
    if let Some(path) = &w.file {
        let _ = write!(out, " \\\n  --body-file {}", sh(path));
    }
    if multipart {
        out.push_str(
            " \\\n  --header \"Content-Type: multipart/form-data; boundary=$boundary\" \\\n  --body-file \"$body\"",
        );
    }
    let _ = write!(out, " \\\n  {}", sh(w.url.as_str()));
    if multipart {
        out.push_str("\nrm \"$body\"");
    }
    out
}

fn httpie(w: &Wire) -> String {
    let mut out = String::from("http --ignore-stdin");
    if w.follows() {
        out.push_str(" --follow");
        if w.settings.max_redirects != 10 {
            let _ = write!(out, " --max-redirects {}", w.settings.max_redirects);
        }
    }
    if w.insecure() {
        out.push_str(" --verify no");
    }
    if let Some(s) = w.timeout_secs() {
        let _ = write!(out, " --timeout {s}");
    }
    if let Some(c) = &w.challenge {
        // NTLM comes from the httpie-ntlm plugin.
        let kind = if c.ntlm() { "ntlm" } else { "digest" };
        let login = sh(&format!("{}:{}", c.login(), c.pass));
        let _ = write!(out, " --auth-type {kind} --auth {login}");
    }
    if !w.parts.is_empty() {
        out.push_str(" --multipart");
    }
    let _ = write!(out, " {} {}", w.method, sh(w.url.as_str()));
    for (k, v) in &w.headers {
        let _ = write!(out, " \\\n  {}", sh(&format!("{k}:{v}")));
    }
    for (key, part) in &w.parts {
        let item = match part {
            Part::File(path) => format!("{key}@{path}"),
            Part::Text(text) => format!("{key}={text}"),
        };
        let _ = write!(out, " \\\n  {}", sh(&item));
    }
    if let Some(body) = &w.body {
        let _ = write!(out, " \\\n  --raw {}", sh(body));
    }
    // HTTPie reads a lone `@path` as the body, like stdin.
    if let Some(path) = &w.file {
        let _ = write!(out, " \\\n  {}", sh(&format!("@{path}")));
    }
    out
}

/// Splatted parameters; works in Windows PowerShell 5.1 unless a setting needs 7+.
fn powershell(w: &Wire) -> String {
    let mut out = String::new();
    let mut needs_7 = !w.parts.is_empty() || w.insecure();
    out.push_str("$params = @{\n");
    let _ = writeln!(out, "    Uri = {}", ps_str(w.url.as_str()));
    let _ = writeln!(out, "    Method = {}", ps_str(&w.method));
    let headers: Vec<_> = w.other_headers().collect();
    if !headers.is_empty() {
        out.push_str("    Headers = @{\n");
        for (k, v) in headers {
            let _ = writeln!(out, "        {} = {}", ps_str(k), ps_str(v));
        }
        out.push_str("    }\n");
    }
    if let Some(ct) = w.content_type() {
        let _ = writeln!(out, "    ContentType = {}", ps_str(ct));
    }
    if let Some(body) = &w.body {
        let _ = writeln!(out, "    Body = {}", ps_str(body));
    }
    if let Some(path) = &w.file {
        let _ = writeln!(out, "    InFile = {}", ps_str(path));
    }
    if !w.parts.is_empty() {
        // A plain hashtable sends the parts in no particular order.
        out.push_str("    Form = [ordered]@{\n");
        for (key, part) in &w.parts {
            let value = match part {
                Part::File(path) => format!("Get-Item -LiteralPath {}", ps_str(path)),
                Part::Text(text) => ps_str(text),
            };
            let _ = writeln!(out, "        {} = {value}", ps_str(key));
        }
        out.push_str("    }\n");
    }
    if !w.follows() {
        out.push_str("    MaximumRedirection = 0\n");
    } else if w.settings.max_redirects != 10 {
        let _ = writeln!(out, "    MaximumRedirection = {}", w.settings.max_redirects);
    }
    if w.insecure() {
        out.push_str("    SkipCertificateCheck = $true\n");
    }
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(
            out,
            "    TimeoutSec = {}",
            w.settings.timeout_ms.div_ceil(1000)
        );
    }
    if w.settings.http_version == HttpVersion::Http2 {
        needs_7 = true;
        out.push_str("    HttpVersion = '2.0'\n");
    }
    out.push_str("}\n");
    if let Some(c) = &w.challenge {
        let _ = write!(
            out,
            "$password = ConvertTo-SecureString {} -AsPlainText -Force\n\
             $params.Credential = New-Object PSCredential({}, $password)\n",
            ps_str(&c.pass),
            ps_str(&c.login())
        );
        // 7 won't send credentials over http:// without it, and 5.1 doesn't know it.
        if !w.https() {
            out.push_str(
                "if ($PSVersionTable.PSVersion.Major -ge 6) { $params.AllowUnencryptedAuthentication = $true }\n",
            );
        }
    }
    out.push_str("$response = Invoke-WebRequest @params -UseBasicParsing\n$response.Content");
    if needs_7 {
        out.insert_str(0, "# Needs PowerShell 7 or later.\n");
    }
    out
}

/// The request as it would appear on the wire.
fn raw(w: &Wire) -> String {
    let mut target = w.url.path().to_owned();
    if let Some(q) = w.url.query() {
        let _ = write!(target, "?{q}");
    }
    let host = match w.url.port() {
        Some(p) => format!("{}:{p}", w.url.host_str().unwrap_or_default()),
        None => w.url.host_str().unwrap_or_default().to_owned(),
    };
    let mut out = format!("{} {target} HTTP/1.1\nHost: {host}\n", w.method);
    for (k, v) in &w.headers {
        let _ = writeln!(out, "{k}: {v}");
    }
    if let Some(body) = &w.body {
        let _ = write!(out, "content-length: {}\n\n{body}", body.len());
    } else if let Some(path) = &w.file {
        let _ = write!(
            out,
            "content-length: <size of {path}>\n\n<contents of {path}>"
        );
    } else if !w.parts.is_empty() {
        let boundary = "----apitool";
        let _ = write!(
            out,
            "content-type: multipart/form-data; boundary={boundary}\n\n"
        );
        for (key, part) in &w.parts {
            let _ = writeln!(out, "--{boundary}");
            match part {
                Part::Text(text) => {
                    let _ = write!(
                        out,
                        "Content-Disposition: form-data; name=\"{key}\"\n\n{text}\n"
                    );
                }
                Part::File(path) => {
                    let name = Wire::file_name(path);
                    let _ = write!(
                        out,
                        "Content-Disposition: form-data; name=\"{key}\"; filename=\"{name}\"\n\n\
                         <contents of {path}>\n"
                    );
                }
            }
        }
        let _ = write!(out, "--{boundary}--");
    }
    out
}

// --- Languages ---------------------------------------------------------------------

fn python(w: &Wire) -> String {
    let mut out = String::from("import requests\n");
    match &w.challenge {
        Some(c) if c.ntlm() => out.push_str("from requests_ntlm import HttpNtlmAuth\n"),
        Some(_) => out.push_str("from requests.auth import HTTPDigestAuth\n"),
        None => {}
    }
    let _ = write!(out, "\nurl = {}\n", dq(w.url.as_str()));
    if !w.headers.is_empty() {
        out.push_str("headers = {\n");
        for (k, v) in &w.headers {
            let _ = writeln!(out, "    {}: {},", dq(k), dq(v));
        }
        out.push_str("}\n");
    }
    if let Some(body) = &w.body {
        let _ = writeln!(out, "payload = {}", dq(body));
    }
    // All parts go in `files`: with text alone in `data`, requests sends a urlencoded form.
    if !w.parts.is_empty() {
        out.push_str("files = [\n");
        for (key, part) in &w.parts {
            let value = match part {
                Part::Text(text) => format!("(None, {})", dq(text)),
                Part::File(path) => format!(
                    "({}, open({}, \"rb\"))",
                    dq(Wire::file_name(path)),
                    dq(path)
                ),
            };
            let _ = writeln!(out, "    ({}, {value}),", dq(key));
        }
        out.push_str("]\n");
    }
    if w.settings.http_version == HttpVersion::Http2 {
        out.push_str("# requests speaks HTTP/1.1 only.\n");
    }
    let mut args = vec![dq(&w.method), "url".to_owned()];
    if !w.headers.is_empty() {
        args.push("headers=headers".into());
    }
    if w.body.is_some() {
        // A str body would go out as Latin-1.
        args.push("data=payload.encode(\"utf-8\")".into());
    }
    if let Some(path) = &w.file {
        // A file object is streamed, not read into memory.
        args.push(format!("data=open({}, \"rb\")", dq(path)));
    }
    if !w.parts.is_empty() {
        args.push("files=files".into());
    }
    if let Some(c) = &w.challenge {
        let auth = if c.ntlm() {
            "HttpNtlmAuth"
        } else {
            "HTTPDigestAuth"
        };
        args.push(format!("auth={auth}({}, {})", dq(&c.login()), dq(&c.pass)));
    }
    if !w.follows() {
        args.push("allow_redirects=False".into());
    }
    if w.insecure() {
        args.push("verify=False".into());
    }
    if let Some(s) = w.timeout_secs() {
        args.push(format!("timeout={s}"));
    }
    let _ = write!(
        out,
        "\nresponse = requests.request({})\nprint(response.text)",
        args.join(", ")
    );
    out
}

/// Shared by fetch and axios: a FormData built from the parts, reading files from disk.
fn js_form(w: &Wire, out: &mut String) {
    out.push_str("const form = new FormData();\n");
    for (key, part) in &w.parts {
        match part {
            Part::Text(text) => {
                let _ = writeln!(out, "form.append({}, {});", dq(key), dq(text));
            }
            Part::File(path) => {
                let name = Wire::file_name(path);
                let _ = writeln!(
                    out,
                    "form.append({}, await openAsBlob({}), {});",
                    dq(key),
                    dq(path),
                    dq(name)
                );
            }
        }
    }
}

fn js_headers(headers: &[&(String, String)], indent: &str, out: &mut String) {
    let _ = writeln!(out, "{indent}headers: {{");
    for (k, v) in headers {
        let _ = writeln!(out, "{indent}  {}: {},", dq(k), dq(v));
    }
    let _ = writeln!(out, "{indent}}},");
}

/// An ES module (top-level await): a browser, Deno, or Node 18+ as `.mjs`.
fn fetch(w: &Wire) -> String {
    let mut out = String::new();
    if w.file.is_some() || w.parts.iter().any(|(_, p)| matches!(p, Part::File(_))) {
        out.push_str("import { openAsBlob } from \"node:fs\"; // Node 20+\n\n");
    }
    if !w.parts.is_empty() {
        js_form(w, &mut out);
        out.push('\n');
    }
    if w.insecure() {
        out.push_str(
            "// Node: run with NODE_TLS_REJECT_UNAUTHORIZED=0 to skip the certificate check.\n",
        );
    }
    if let Some(c) = &w.challenge {
        let _ = writeln!(
            out,
            "// {} auth isn't built into fetch; answer the 401 challenge yourself.",
            c.scheme
        );
    }
    let _ = writeln!(
        out,
        "const response = await fetch({}, {{",
        dq(w.url.as_str())
    );
    let _ = writeln!(out, "  method: {},", dq(&w.method));
    let headers: Vec<_> = w.headers.iter().collect();
    if !headers.is_empty() {
        js_headers(&headers, "  ", &mut out);
    }
    if let Some(body) = &w.body {
        let _ = writeln!(out, "  body: {},", dq(body));
    }
    if let Some(path) = &w.file {
        let _ = writeln!(out, "  body: await openAsBlob({}),", dq(path));
    }
    if !w.parts.is_empty() {
        out.push_str("  body: form,\n");
    }
    if !w.follows() {
        out.push_str("  redirect: \"manual\",\n");
    }
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(
            out,
            "  signal: AbortSignal.timeout({}),",
            w.settings.timeout_ms
        );
    }
    out.push_str("});\nconsole.log(await response.text());");
    out
}

fn axios(w: &Wire) -> String {
    let mut out = String::from("const axios = require(\"axios\");\n");
    if w.insecure() {
        out.push_str("const https = require(\"node:https\");\n");
    }
    if w.parts.iter().any(|(_, p)| matches!(p, Part::File(_))) {
        out.push_str("const { openAsBlob } = require(\"node:fs\"); // Node 20+\n");
    }
    if w.file.is_some() {
        out.push_str("const { readFile } = require(\"node:fs/promises\");\n");
    }
    out.push_str("\nasync function main() {\n");
    if !w.parts.is_empty() {
        let mut form = String::new();
        js_form(w, &mut form);
        for line in form.lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    out.push_str("  const response = await axios.request({\n");
    let _ = writeln!(out, "    method: {},", dq(&w.method.to_lowercase()));
    let _ = writeln!(out, "    url: {},", dq(w.url.as_str()));
    let headers: Vec<_> = w.headers.iter().collect();
    if !headers.is_empty() {
        js_headers(&headers, "    ", &mut out);
    }
    if let Some(body) = &w.body {
        let _ = writeln!(out, "    data: {},", dq(body));
    }
    if let Some(path) = &w.file {
        let _ = writeln!(out, "    data: await readFile({}),", dq(path));
    }
    if !w.parts.is_empty() {
        out.push_str("    data: form,\n");
    }
    if let Some(c) = &w.challenge {
        let _ = writeln!(
            out,
            "    // {} auth isn't built into axios; answer the 401 challenge yourself.",
            c.scheme
        );
    }
    match (w.follows(), w.settings.max_redirects) {
        (false, _) => out.push_str("    maxRedirects: 0,\n"),
        (true, 10) => {}
        (true, n) => {
            let _ = writeln!(out, "    maxRedirects: {n},");
        }
    }
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(out, "    timeout: {},", w.settings.timeout_ms);
    }
    if w.insecure() {
        out.push_str("    httpsAgent: new https.Agent({ rejectUnauthorized: false }),\n");
    }
    out.push_str("  });\n  console.log(response.data);\n}\n\nmain().catch(console.error);");
    out
}

fn go(w: &Wire) -> String {
    let files = w.parts.iter().any(|(_, p)| matches!(p, Part::File(_)));
    let mut imports = vec!["fmt", "io", "net/http"];
    if w.body.is_some() {
        imports.push("strings");
    }
    if !w.parts.is_empty() {
        imports.extend(["bytes", "mime/multipart"]);
    }
    if files {
        imports.extend(["os", "path/filepath"]);
    } else if w.file.is_some() {
        imports.push("os");
    }
    if w.settings.timeout_ms > 0 {
        imports.push("time");
    }
    if w.insecure() {
        imports.push("crypto/tls");
    }
    let limited = w.follows() && w.settings.max_redirects != 10;
    if limited {
        imports.push("errors");
    }
    imports.sort_unstable();
    let mut out = String::from("package main\n\nimport (\n");
    for i in imports {
        let _ = writeln!(out, "\t\"{i}\"");
    }
    out.push_str(")\n\nfunc main() {\n");
    let body = if let Some(body) = &w.body {
        let _ = writeln!(out, "\tpayload := strings.NewReader({})", dq(body));
        "payload"
    } else if let Some(path) = &w.file {
        let _ = write!(
            out,
            "\tpayload, err := os.Open({})\n\tif err != nil {{\n\t\tpanic(err)\n\t}}\n\tdefer payload.Close()\n",
            dq(path)
        );
        "payload"
    } else if !w.parts.is_empty() {
        out.push_str("\tpayload := &bytes.Buffer{}\n\twriter := multipart.NewWriter(payload)\n");
        for (key, part) in &w.parts {
            match part {
                Part::Text(text) => {
                    let _ = writeln!(out, "\t_ = writer.WriteField({}, {})", dq(key), dq(text));
                }
                // A block per file, so a second one can declare `file` and `err` again.
                Part::File(path) => {
                    let _ = write!(
                        out,
                        "\t{{\n\
                         \t\tfile, err := os.Open({path})\n\
                         \t\tif err != nil {{\n\t\t\tpanic(err)\n\t\t}}\n\
                         \t\tdefer file.Close()\n\
                         \t\tpart, err := writer.CreateFormFile({key}, filepath.Base({path}))\n\
                         \t\tif err != nil {{\n\t\t\tpanic(err)\n\t\t}}\n\
                         \t\tif _, err := io.Copy(part, file); err != nil {{\n\t\t\tpanic(err)\n\t\t}}\n\
                         \t}}\n",
                        path = dq(path),
                        key = dq(key)
                    );
                }
            }
        }
        out.push_str("\tif err := writer.Close(); err != nil {\n\t\tpanic(err)\n\t}\n");
        "payload"
    } else {
        "nil"
    };
    let _ = write!(
        out,
        "\treq, err := http.NewRequest({}, {}, {body})\n\
         \tif err != nil {{\n\t\tpanic(err)\n\t}}\n",
        dq(&w.method),
        dq(w.url.as_str())
    );
    for (k, v) in &w.headers {
        let _ = writeln!(out, "\treq.Header.Set({}, {})", dq(k), dq(v));
    }
    if !w.parts.is_empty() {
        out.push_str("\treq.Header.Set(\"Content-Type\", writer.FormDataContentType())\n");
    }
    if let Some(c) = &w.challenge {
        let _ = writeln!(
            out,
            "\t// {} auth isn't built into net/http; answer the 401 challenge yourself.",
            c.scheme
        );
    }
    if w.settings.http_version != HttpVersion::Auto {
        out.push_str("\t// net/http picks the HTTP version itself (HTTP/2 when TLS offers it).\n");
    }
    out.push_str("\tclient := &http.Client{");
    let mut fields = Vec::new();
    if w.settings.timeout_ms > 0 {
        fields.push(format!(
            "\n\t\tTimeout: {} * time.Millisecond,",
            w.settings.timeout_ms
        ));
    }
    if w.insecure() {
        fields.push(
            "\n\t\tTransport: &http.Transport{\n\t\t\tTLSClientConfig: &tls.Config{InsecureSkipVerify: true},\n\t\t},"
                .into(),
        );
    }
    if !w.follows() {
        fields.push(
            "\n\t\tCheckRedirect: func(req *http.Request, via []*http.Request) error {\n\t\t\treturn http.ErrUseLastResponse\n\t\t},"
                .into(),
        );
    } else if limited {
        fields.push(format!(
            "\n\t\tCheckRedirect: func(req *http.Request, via []*http.Request) error {{\n\
             \t\t\tif len(via) >= {n} {{\n\t\t\t\treturn errors.New(\"stopped after {n} redirects\")\n\t\t\t}}\n\
             \t\t\treturn nil\n\t\t}},",
            n = w.settings.max_redirects
        ));
    }
    for f in &fields {
        out.push_str(f);
    }
    if !fields.is_empty() {
        out.push_str("\n\t");
    }
    out.push_str(
        "}\n\tres, err := client.Do(req)\n\tif err != nil {\n\t\tpanic(err)\n\t}\n\
         \tdefer res.Body.Close()\n\tout, err := io.ReadAll(res.Body)\n\
         \tif err != nil {\n\t\tpanic(err)\n\t}\n\tfmt.Println(string(out))\n}\n",
    );
    out
}

/// java.net.http (JDK 11+), so no dependency.
fn java(w: &Wire) -> String {
    let mut out = String::from(
        "import java.net.URI;\nimport java.net.http.HttpClient;\n\
         import java.net.http.HttpRequest;\nimport java.net.http.HttpResponse;\n",
    );
    let multipart = !w.parts.is_empty();
    if multipart {
        out.push_str(
            "import java.io.ByteArrayOutputStream;\nimport java.io.IOException;\n\
             import java.nio.charset.StandardCharsets;\nimport java.nio.file.Files;\n",
        );
    }
    if w.file.is_some() || multipart {
        out.push_str("import java.nio.file.Path;\n");
    }
    if multipart {
        out.push_str("import java.util.UUID;\n");
    }
    if w.settings.timeout_ms > 0 {
        out.push_str("import java.time.Duration;\n");
    }
    out.push_str("\npublic class Main {\n");
    // HttpClient has no multipart builder, so the body is put together by hand.
    if multipart {
        out.push_str(
            "    static void part(ByteArrayOutputStream body, String boundary, String name,\n\
             \x20           String filename, byte[] content) throws IOException {\n\
             \x20       String head = \"--\" + boundary + \"\\r\\nContent-Disposition: form-data; name=\\\"\" + name + \"\\\"\";\n\
             \x20       if (filename != null) head += \"; filename=\\\"\" + filename + \"\\\"\";\n\
             \x20       body.write((head + \"\\r\\n\\r\\n\").getBytes(StandardCharsets.UTF_8));\n\
             \x20       body.write(content);\n\
             \x20       body.write(\"\\r\\n\".getBytes(StandardCharsets.UTF_8));\n\
             \x20   }\n\n",
        );
    }
    out.push_str("    public static void main(String[] args) throws Exception {\n");
    if multipart {
        out.push_str(
            "        String boundary = \"Boundary-\" + UUID.randomUUID();\n\
             \x20       ByteArrayOutputStream body = new ByteArrayOutputStream();\n",
        );
        for (key, p) in &w.parts {
            let _ = match p {
                Part::Text(text) => writeln!(
                    out,
                    "        part(body, boundary, {}, null, {}.getBytes(StandardCharsets.UTF_8));",
                    dq(key),
                    dq(text)
                ),
                Part::File(path) => writeln!(
                    out,
                    "        part(body, boundary, {}, {}, Files.readAllBytes(Path.of({})));",
                    dq(key),
                    dq(Wire::file_name(path)),
                    dq(path)
                ),
            };
        }
        out.push_str(
            "        body.write((\"--\" + boundary + \"--\\r\\n\").getBytes(StandardCharsets.UTF_8));\n",
        );
    }
    if w.insecure() {
        out.push_str("        // Skipping the certificate check needs a custom SSLContext.\n");
    }
    if let Some(c) = &w.challenge {
        let _ = writeln!(
            out,
            "        // {} auth isn't built into HttpClient; answer the 401 challenge yourself.",
            c.scheme
        );
    }
    out.push_str("        HttpClient client = HttpClient.newBuilder()\n");
    // HttpClient never follows unless told.
    if w.follows() {
        out.push_str("            .followRedirects(HttpClient.Redirect.NORMAL)\n");
    }
    match w.settings.http_version {
        HttpVersion::Auto => {}
        HttpVersion::Http1 => out.push_str("            .version(HttpClient.Version.HTTP_1_1)\n"),
        HttpVersion::Http2 => out.push_str("            .version(HttpClient.Version.HTTP_2)\n"),
    }
    out.push_str("            .build();\n");
    let publisher = match (&w.body, &w.file) {
        (Some(body), _) => format!("HttpRequest.BodyPublishers.ofString({})", dq(body)),
        (_, Some(path)) => format!("HttpRequest.BodyPublishers.ofFile(Path.of({}))", dq(path)),
        _ if multipart => "HttpRequest.BodyPublishers.ofByteArray(body.toByteArray())".into(),
        _ => "HttpRequest.BodyPublishers.noBody()".into(),
    };
    let _ = write!(
        out,
        "        HttpRequest request = HttpRequest.newBuilder()\n\
         \x20           .uri(URI.create({}))\n\
         \x20           .method({}, {publisher})\n",
        dq(w.url.as_str()),
        dq(&w.method)
    );
    for (k, v) in &w.headers {
        let _ = writeln!(out, "            .header({}, {})", dq(k), dq(v));
    }
    if multipart {
        out.push_str(
            "            .header(\"Content-Type\", \"multipart/form-data; boundary=\" + boundary)\n",
        );
    }
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(
            out,
            "            .timeout(Duration.ofMillis({}))",
            w.settings.timeout_ms
        );
    }
    out.push_str(
        "            .build();\n\
         \x20       HttpResponse<String> response = client.send(request, HttpResponse.BodyHandlers.ofString());\n\
         \x20       System.out.println(response.body());\n    }\n}\n",
    );
    out
}

/// Top-level statements (.NET 6+).
fn csharp(w: &Wire) -> String {
    let mut out = String::from("using System.Net;\nusing System.Net.Http.Headers;\n\n");
    out.push_str("var handler = new HttpClientHandler();\n");
    if !w.follows() {
        out.push_str("handler.AllowAutoRedirect = false;\n");
    } else if w.settings.max_redirects != 10 {
        let _ = writeln!(
            out,
            "handler.MaxAutomaticRedirections = {};",
            w.settings.max_redirects
        );
    }
    if w.insecure() {
        out.push_str(
            "handler.ServerCertificateCustomValidationCallback =\n    HttpClientHandler.DangerousAcceptAnyServerCertificateValidator;\n",
        );
    }
    if let Some(c) = &w.challenge {
        let domain = match c.domain.as_str() {
            "" => String::new(),
            domain => format!(", {}", dq(domain)),
        };
        let _ = writeln!(
            out,
            "handler.Credentials = new NetworkCredential({}, {}{domain});",
            dq(&c.user),
            dq(&c.pass)
        );
    }
    out.push_str("using var client = new HttpClient(handler);\n");
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(
            out,
            "client.Timeout = TimeSpan.FromMilliseconds({});",
            w.settings.timeout_ms
        );
    }
    let _ = writeln!(
        out,
        "var request = new HttpRequestMessage(new HttpMethod({}), {});",
        dq(&w.method),
        dq(w.url.as_str())
    );
    match w.settings.http_version {
        HttpVersion::Auto => {}
        HttpVersion::Http1 => out.push_str("request.Version = HttpVersion.Version11;\nrequest.VersionPolicy = HttpVersionPolicy.RequestVersionExact;\n"),
        HttpVersion::Http2 => out.push_str("request.Version = HttpVersion.Version20;\nrequest.VersionPolicy = HttpVersionPolicy.RequestVersionExact;\n"),
    }
    for (k, v) in w.other_headers() {
        let _ = writeln!(
            out,
            "request.Headers.TryAddWithoutValidation({}, {});",
            dq(k),
            dq(v)
        );
    }
    let content = match (&w.body, &w.file) {
        (Some(body), _) => Some(format!("new StringContent({})", dq(body))),
        (_, Some(path)) => Some(format!("new StreamContent(File.OpenRead({}))", dq(path))),
        _ => None,
    };
    if let Some(content) = content {
        let _ = writeln!(out, "request.Content = {content};");
        // StringContent says text/plain unless told otherwise.
        match w.content_type() {
            Some(ct) => {
                let _ = writeln!(
                    out,
                    "request.Content.Headers.ContentType = MediaTypeHeaderValue.Parse({});",
                    dq(ct)
                );
            }
            None => out.push_str("request.Content.Headers.ContentType = null;\n"),
        }
    }
    if !w.parts.is_empty() {
        out.push_str("var form = new MultipartFormDataContent();\n");
        for (key, part) in &w.parts {
            match part {
                Part::Text(text) => {
                    let _ = writeln!(
                        out,
                        "form.Add(new StringContent({}), {});",
                        dq(text),
                        dq(key)
                    );
                }
                Part::File(path) => {
                    let _ = writeln!(
                        out,
                        "form.Add(new StreamContent(File.OpenRead({})), {}, {});",
                        dq(path),
                        dq(key),
                        dq(Wire::file_name(path))
                    );
                }
            }
        }
        out.push_str("request.Content = form;\n");
    }
    out.push_str(
        "var response = await client.SendAsync(request);\n\
         Console.WriteLine(await response.Content.ReadAsStringAsync());\n",
    );
    out
}

fn php(w: &Wire) -> String {
    let mut out = String::from("<?php\n\n$curl = curl_init();\ncurl_setopt_array($curl, [\n");
    let opt = |out: &mut String, k: &str, v: String| {
        let _ = writeln!(out, "    {k} => {v},");
    };
    opt(&mut out, "CURLOPT_URL", php_str(w.url.as_str()));
    opt(&mut out, "CURLOPT_RETURNTRANSFER", "true".into());
    match w.method.as_str() {
        "GET" => {}
        "HEAD" => opt(&mut out, "CURLOPT_NOBODY", "true".into()),
        m => opt(&mut out, "CURLOPT_CUSTOMREQUEST", php_str(m)),
    }
    if w.follows() {
        opt(&mut out, "CURLOPT_FOLLOWLOCATION", "true".into());
        opt(
            &mut out,
            "CURLOPT_MAXREDIRS",
            w.settings.max_redirects.to_string(),
        );
    }
    if w.insecure() {
        opt(&mut out, "CURLOPT_SSL_VERIFYPEER", "false".into());
        opt(&mut out, "CURLOPT_SSL_VERIFYHOST", "0".into());
    }
    if w.settings.timeout_ms > 0 {
        opt(
            &mut out,
            "CURLOPT_TIMEOUT_MS",
            w.settings.timeout_ms.to_string(),
        );
    }
    match (w.settings.http_version, w.https()) {
        (HttpVersion::Auto, _) => {}
        (HttpVersion::Http1, _) => opt(
            &mut out,
            "CURLOPT_HTTP_VERSION",
            "CURL_HTTP_VERSION_1_1".into(),
        ),
        (HttpVersion::Http2, true) => opt(
            &mut out,
            "CURLOPT_HTTP_VERSION",
            "CURL_HTTP_VERSION_2_0".into(),
        ),
        (HttpVersion::Http2, false) => opt(
            &mut out,
            "CURLOPT_HTTP_VERSION",
            "CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE".into(),
        ),
    }
    if let Some(c) = &w.challenge {
        let kind = if c.ntlm() {
            "CURLAUTH_NTLM"
        } else {
            "CURLAUTH_DIGEST"
        };
        opt(&mut out, "CURLOPT_HTTPAUTH", kind.into());
        let login = format!("{}:{}", c.login(), c.pass);
        opt(&mut out, "CURLOPT_USERPWD", php_str(&login));
    }
    if !w.headers.is_empty() {
        out.push_str("    CURLOPT_HTTPHEADER => [\n");
        for (k, v) in &w.headers {
            let _ = writeln!(out, "        {},", php_str(&format!("{k}: {v}")));
        }
        out.push_str("    ],\n");
    }
    if let Some(body) = &w.body {
        opt(&mut out, "CURLOPT_POSTFIELDS", php_str(body));
    }
    if let Some(path) = &w.file {
        let read = format!("file_get_contents({})", php_str(path));
        opt(&mut out, "CURLOPT_POSTFIELDS", read);
    }
    if !w.parts.is_empty() {
        out.push_str("    CURLOPT_POSTFIELDS => [\n");
        for (key, part) in &w.parts {
            let value = match part {
                Part::Text(text) => php_str(text),
                Part::File(path) => format!("new CURLFile({})", php_str(path)),
            };
            let _ = writeln!(out, "        {} => {value},", php_str(key));
        }
        out.push_str("    ],\n");
    }
    out.push_str("]);\n$response = curl_exec($curl);\ncurl_close($curl);\necho $response;\n");
    out
}

fn ruby(w: &Wire) -> String {
    let mut out = String::from("require \"net/http\"\nrequire \"uri\"\n");
    if w.insecure() {
        out.push_str("require \"openssl\"\n");
    }
    let _ = write!(
        out,
        "\nuri = URI({})\nhttp = Net::HTTP.new(uri.host, uri.port)\nhttp.use_ssl = uri.scheme == \"https\"\n",
        ruby_str(w.url.as_str())
    );
    if w.insecure() {
        out.push_str("http.verify_mode = OpenSSL::SSL::VERIFY_NONE\n");
    }
    if let Some(s) = w.timeout_secs() {
        let _ = writeln!(out, "http.open_timeout = {s}\nhttp.read_timeout = {s}");
    }
    let has_body = w.body.is_some() || w.file.is_some() || !w.parts.is_empty();
    let _ = writeln!(
        out,
        "request = Net::HTTPGenericRequest.new({}, {has_body}, {}, uri.request_uri)",
        ruby_str(&w.method),
        w.method != "HEAD"
    );
    for (k, v) in &w.headers {
        let _ = writeln!(out, "request[{}] = {}", ruby_str(k), ruby_str(v));
    }
    if let Some(body) = &w.body {
        let _ = writeln!(out, "request.body = {}", ruby_str(body));
    }
    if let Some(path) = &w.file {
        let _ = writeln!(out, "request.body = File.binread({})", ruby_str(path));
    }
    if !w.parts.is_empty() {
        out.push_str("request.set_form([\n");
        for (key, part) in &w.parts {
            let value = match part {
                Part::Text(text) => ruby_str(text),
                Part::File(path) => format!("File.open({})", ruby_str(path)),
            };
            let _ = writeln!(out, "  [{}, {value}],", ruby_str(key));
        }
        out.push_str("], \"multipart/form-data\")\n");
    }
    match &w.challenge {
        Some(c) if c.ntlm() => {
            out.push_str("# NTLM auth: Net::HTTP needs the rubyntlm gem for the 401 challenge.\n")
        }
        Some(_) => out.push_str(
            "# Digest auth: Net::HTTP needs the net-http-digest_auth gem for the 401 challenge.\n",
        ),
        None => {}
    }
    if w.follows() {
        out.push_str(
            "# Net::HTTP doesn't follow redirects; read response[\"location\"] to go on.\n",
        );
    }
    out.push_str("response = http.request(request)\nputs response.body\n");
    out
}

fn rust(w: &Wire) -> String {
    let mut out = String::from("// Cargo.toml: tokio (feature \"full\") and reqwest");
    out.push_str(
        match (
            w.parts.is_empty(),
            w.parts.iter().any(|(_, p)| matches!(p, Part::File(_))),
        ) {
            (true, _) => ".\n",
            (false, false) => " (feature \"multipart\").\n",
            (false, true) => " (features \"multipart\" and \"stream\").\n",
        },
    );
    out.push_str("#[tokio::main]\nasync fn main() -> Result<(), Box<dyn std::error::Error>> {\n");
    out.push_str("    let client = reqwest::Client::builder()\n");
    match (w.follows(), w.settings.max_redirects) {
        (false, _) => out.push_str("        .redirect(reqwest::redirect::Policy::none())\n"),
        (true, 10) => {}
        (true, n) => {
            let _ = writeln!(
                out,
                "        .redirect(reqwest::redirect::Policy::limited({n}))"
            );
        }
    }
    if w.insecure() {
        out.push_str("        .danger_accept_invalid_certs(true)\n");
    }
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(
            out,
            "        .timeout(std::time::Duration::from_millis({}))",
            w.settings.timeout_ms
        );
    }
    match w.settings.http_version {
        HttpVersion::Auto => {}
        HttpVersion::Http1 => out.push_str("        .http1_only()\n"),
        HttpVersion::Http2 => out.push_str("        .http2_prior_knowledge()\n"),
    }
    out.push_str("        .build()?;\n");
    if !w.parts.is_empty() {
        out.push_str("    let form = reqwest::multipart::Form::new()");
        for (key, part) in &w.parts {
            match part {
                Part::Text(text) => {
                    let _ = write!(out, "\n        .text({}, {})", braced(key), braced(text));
                }
                Part::File(path) => {
                    let _ = write!(
                        out,
                        "\n        .file({}, {}).await?",
                        braced(key),
                        braced(path)
                    );
                }
            }
        }
        out.push_str(";\n");
    }
    let method = match w.method.as_str() {
        m @ ("GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS") => {
            format!("reqwest::Method::{m}")
        }
        m => format!("reqwest::Method::from_bytes(b{})?", braced(m)),
    };
    let _ = write!(
        out,
        "    let response = client\n        .request({method}, {})",
        braced(w.url.as_str())
    );
    for (k, v) in &w.headers {
        let _ = write!(out, "\n        .header({}, {})", braced(k), braced(v));
    }
    if let Some(body) = &w.body {
        let _ = write!(out, "\n        .body({})", braced(body));
    }
    if let Some(path) = &w.file {
        let _ = write!(
            out,
            "\n        .body(tokio::fs::read({}).await?)",
            braced(path)
        );
    }
    if !w.parts.is_empty() {
        out.push_str("\n        .multipart(form)");
    }
    if let Some(c) = &w.challenge {
        let _ = write!(
            out,
            "\n        // {} auth isn't built into reqwest; answer the 401 challenge yourself.",
            c.scheme
        );
    }
    out.push_str(
        "\n        .send()\n        .await?;\n    println!(\"{}\", response.text().await?);\n    Ok(())\n}\n",
    );
    out
}

/// A script (`swift main.swift`): top-level await.
fn swift(w: &Wire) -> String {
    // FoundationNetworking holds URLSession on Linux.
    let mut out = String::from(
        "import Foundation\n#if canImport(FoundationNetworking)\nimport FoundationNetworking\n#endif\n\n",
    );
    if w.insecure() || !w.follows() {
        out.push_str(
            "// Skipping redirects or the certificate check needs a URLSessionDelegate.\n",
        );
    }
    if let Some(c) = &w.challenge {
        let _ = writeln!(
            out,
            "// {} auth: answer the challenge in a URLSessionTaskDelegate.",
            c.scheme
        );
    }
    let _ = write!(
        out,
        "var request = URLRequest(url: URL(string: {})!)\nrequest.httpMethod = {}\n",
        braced(w.url.as_str()),
        braced(&w.method)
    );
    if let Some(s) = w.timeout_secs() {
        let _ = writeln!(out, "request.timeoutInterval = {s}");
    }
    for (k, v) in &w.headers {
        let _ = writeln!(
            out,
            "request.setValue({}, forHTTPHeaderField: {})",
            braced(v),
            braced(k)
        );
    }
    if let Some(body) = &w.body {
        let _ = writeln!(out, "request.httpBody = Data({}.utf8)", braced(body));
    }
    if let Some(path) = &w.file {
        let _ = writeln!(
            out,
            "request.httpBody = try Data(contentsOf: URL(fileURLWithPath: {}))",
            braced(path)
        );
    }
    // URLSession has no multipart builder, so the body is put together by hand.
    if !w.parts.is_empty() {
        out.push_str(
            "let boundary = \"Boundary-\\(UUID().uuidString)\"\n\
             var body = Data()\n\
             func part(_ name: String, _ filename: String?, _ content: Data) {\n\
             \x20   var head = \"--\\(boundary)\\r\\nContent-Disposition: form-data; name=\\\"\\(name)\\\"\"\n\
             \x20   if let filename { head += \"; filename=\\\"\\(filename)\\\"\" }\n\
             \x20   body.append(Data((head + \"\\r\\n\\r\\n\").utf8))\n\
             \x20   body.append(content)\n\
             \x20   body.append(Data(\"\\r\\n\".utf8))\n\
             }\n",
        );
        for (key, p) in &w.parts {
            let _ = match p {
                Part::Text(text) => {
                    writeln!(
                        out,
                        "part({}, nil, Data({}.utf8))",
                        braced(key),
                        braced(text)
                    )
                }
                Part::File(path) => writeln!(
                    out,
                    "part({}, {}, try Data(contentsOf: URL(fileURLWithPath: {})))",
                    braced(key),
                    braced(Wire::file_name(path)),
                    braced(path)
                ),
            };
        }
        out.push_str(
            "body.append(Data(\"--\\(boundary)--\\r\\n\".utf8))\n\
             request.setValue(\"multipart/form-data; boundary=\\(boundary)\", forHTTPHeaderField: \"Content-Type\")\n\
             request.httpBody = body\n",
        );
    }
    out.push_str(
        "let (data, _) = try await URLSession.shared.data(for: request)\n\
         print(String(decoding: data, as: UTF8.self))\n",
    );
    out
}

fn kotlin(w: &Wire) -> String {
    let mut imports = vec!["okhttp3.OkHttpClient", "okhttp3.Request"];
    let ct = w.content_type();
    // OkHttp throws on a POST, PUT or PATCH without a body.
    let empty = w.body.is_none()
        && w.file.is_none()
        && w.parts.is_empty()
        && matches!(w.method.as_str(), "POST" | "PUT" | "PATCH");
    if w.body.is_some() || empty {
        imports.push("okhttp3.RequestBody.Companion.toRequestBody");
    }
    if (w.body.is_some() || w.file.is_some()) && ct.is_some() {
        imports.push("okhttp3.MediaType.Companion.toMediaType");
    }
    if !w.parts.is_empty() {
        imports.push("okhttp3.MultipartBody");
    }
    if w.file.is_some() || w.parts.iter().any(|(_, p)| matches!(p, Part::File(_))) {
        imports.extend([
            "okhttp3.RequestBody.Companion.asRequestBody",
            "java.io.File",
        ]);
    }
    if w.settings.timeout_ms > 0 {
        imports.push("java.util.concurrent.TimeUnit");
    }
    if w.settings.http_version != HttpVersion::Auto {
        imports.push("okhttp3.Protocol");
    }
    imports.sort_unstable();
    let mut out = String::new();
    for i in imports {
        let _ = writeln!(out, "import {i}");
    }
    out.push('\n');
    if w.insecure() {
        out.push_str("// Skipping the certificate check needs a custom X509TrustManager.\n");
    }
    match &w.challenge {
        Some(c) if c.ntlm() => {
            out.push_str("// NTLM auth isn't built into OkHttp; add an Authenticator for it.\n")
        }
        Some(_) => out.push_str("// Digest auth: add an okhttp-digest Authenticator.\n"),
        None => {}
    }
    out.push_str("fun main() {\n    val client = OkHttpClient.Builder()\n");
    if !w.follows() {
        out.push_str("        .followRedirects(false)\n");
    }
    if w.settings.timeout_ms > 0 {
        let _ = writeln!(
            out,
            "        .callTimeout({}, TimeUnit.MILLISECONDS)",
            w.settings.timeout_ms
        );
    }
    match (w.settings.http_version, w.https()) {
        (HttpVersion::Auto, _) => {}
        (HttpVersion::Http1, _) => out.push_str("        .protocols(listOf(Protocol.HTTP_1_1))\n"),
        (HttpVersion::Http2, true) => {
            out.push_str("        .protocols(listOf(Protocol.HTTP_2, Protocol.HTTP_1_1))\n")
        }
        (HttpVersion::Http2, false) => {
            out.push_str("        .protocols(listOf(Protocol.H2_PRIOR_KNOWLEDGE))\n")
        }
    }
    out.push_str("        .build()\n");
    let media = match ct {
        Some(ct) => format!("{}.toMediaType()", kotlin_str(ct)),
        None => "null".into(),
    };
    let body = if let Some(body) = &w.body {
        // Bytes: a String body gets "; charset=utf-8" added to its content type.
        let _ = writeln!(
            out,
            "    val body = {}.toByteArray().toRequestBody({media})",
            kotlin_str(body)
        );
        "body"
    } else if let Some(path) = &w.file {
        let _ = writeln!(
            out,
            "    val body = File({}).asRequestBody({media})",
            kotlin_str(path)
        );
        "body"
    } else if !w.parts.is_empty() {
        out.push_str(
            "    val body = MultipartBody.Builder()\n        .setType(MultipartBody.FORM)\n",
        );
        for (key, part) in &w.parts {
            match part {
                Part::Text(text) => {
                    let _ = writeln!(
                        out,
                        "        .addFormDataPart({}, {})",
                        kotlin_str(key),
                        kotlin_str(text)
                    );
                }
                Part::File(path) => {
                    let _ = writeln!(
                        out,
                        "        .addFormDataPart({}, {}, File({}).asRequestBody())",
                        kotlin_str(key),
                        kotlin_str(Wire::file_name(path)),
                        kotlin_str(path)
                    );
                }
            }
        }
        out.push_str("        .build()\n");
        "body"
    } else if empty {
        "\"\".toRequestBody(null)"
    } else {
        "null"
    };
    let _ = write!(
        out,
        "    val request = Request.Builder()\n        .url({})\n        .method({}, {body})\n",
        kotlin_str(w.url.as_str()),
        kotlin_str(&w.method)
    );
    // OkHttp takes the content type from the body.
    for (k, v) in w.other_headers() {
        let _ = writeln!(
            out,
            "        .addHeader({}, {})",
            kotlin_str(k),
            kotlin_str(v)
        );
    }
    out.push_str(
        "        .build()\n    client.newCall(request).execute().use { response ->\n\
         \x20       println(response.body!!.string())\n    }\n}\n",
    );
    out
}

// --- Streams -------------------------------------------------------------------------

type StreamGenerator = fn(&Stream) -> String;

const SSE: &[(&str, StreamGenerator)] = &[
    ("cURL", sse_curl),
    ("JavaScript (fetch)", sse_fetch),
    ("Python (requests)", sse_python),
    ("Go (net/http)", sse_go),
];

const WEBSOCKET: &[(&str, StreamGenerator)] = &[
    ("websocat", ws_websocat),
    ("JavaScript (WebSocket)", ws_browser),
    ("Node.js (ws)", ws_node),
    ("Python (websockets)", ws_python),
    ("Go (gorilla/websocket)", ws_go),
];

const SOCKETIO: &[(&str, StreamGenerator)] = &[
    ("Node.js (socket.io-client)", sio_node),
    ("Python (python-socketio)", sio_python),
];

const MQTT: &[(&str, StreamGenerator)] = &[
    ("mosquitto", mqtt_mosquitto),
    ("Python (paho-mqtt)", mqtt_python),
    ("Node.js (mqtt)", mqtt_node),
    ("Go (paho.mqtt.golang)", mqtt_go),
];

const GRPC: &[(&str, StreamGenerator)] = &[
    ("grpcurl", grpc_grpcurl),
    ("Node.js (@grpc/grpc-js)", grpc_node),
    ("Python (grpcio)", grpc_python),
];

fn stream_targets(method: &str) -> Option<&'static [(&'static str, StreamGenerator)]> {
    Some(match method {
        "SSE" => SSE,
        "WS" => WEBSOCKET,
        "SOCKETIO" => SOCKETIO,
        "MQTT" => MQTT,
        "GRPC" => GRPC,
        _ => return None,
    })
}

/// What a stream's snippet is made from: the request, and the headers its handshake
/// carries (its own and its auth's, as Connect sends them).
pub struct Stream {
    req: Request,
    headers: Vec<(String, String)>,
    /// gRPC: the method's shape, when its descriptor is at hand.
    rpc: Option<crate::grpc::Rpc>,
}

impl Stream {
    fn new(mut req: Request) -> Result<Self, String> {
        if req.url.trim().is_empty() {
            return Err("URL is empty".into());
        }
        if let Auth::OAuth2(o) = &req.auth {
            req.auth = fetched_token(o);
        }
        let headers = match req.method.as_str() {
            // MQTT has no headers; it signs in with Basic auth's name and password.
            "MQTT" => Vec::new(),
            _ => {
                let handshake = Request {
                    body: Body::None,
                    ..req.clone()
                };
                let wire = build(&OFFLINE, handshake)?.build();
                let wire = wire.map_err(|e| error_chain(&e))?;
                (wire.headers().iter())
                    .map(|(k, v)| {
                        (
                            k.as_str().to_owned(),
                            String::from_utf8_lossy(v.as_bytes()).into_owned(),
                        )
                    })
                    .filter(|(k, _)| k != "content-type")
                    .collect()
            }
        };
        let rpc = (req.method == "GRPC")
            .then(|| {
                let imports = &req.settings.import_paths;
                crate::grpc::methods(&crate::grpc::source(&req.proto, &req.url, imports)).ok()
            })
            .flatten()
            .and_then(|list| list.into_iter().find(|r| r.name == req.rpc));
        Ok(Self { req, headers, rpc })
    }

    /// The URL as an HTTP one: what SSE, Socket.IO and gRPC connect to.
    fn http_url(&self) -> String {
        crate::http::wire_url(&self.req.url)
    }

    /// ws:// or wss://, however it was typed.
    fn ws_url(&self) -> String {
        let url = self.http_url();
        match url.strip_prefix("https://") {
            Some(rest) => format!("wss://{rest}"),
            None => url.replacen("http://", "ws://", 1),
        }
    }

    fn sse_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if !headers.iter().any(|(k, _)| k == "accept") {
            headers.insert(0, ("accept".into(), "text/event-stream".into()));
        }
        headers
    }

    fn body_text(&self) -> &str {
        match &self.req.body {
            Body::Json { text } | Body::Text { text } => text.trim(),
            _ => "",
        }
    }
}

/// OAuth 2.0's token is fetched by Send; a snippet can only show the last one.
fn fetched_token(o: &crate::model::OAuth2) -> Auth {
    let token =
        crate::auth::cached_token(o).unwrap_or_else(|| "<press Send once to fetch a token>".into());
    Auth::Bearer { token }
}

/// A message typed in the app while connected; a snippet sends this one in its place.
const MESSAGE: &str = "hello";

fn sse_curl(s: &Stream) -> String {
    // -N: print each event as it comes instead of buffering.
    let mut out = format!("curl -N {}", sh(&s.http_url()));
    for (k, v) in s.sse_headers() {
        let _ = write!(out, " \\\n  -H {}", sh(&format!("{k}: {v}")));
    }
    out
}

fn sse_fetch(s: &Stream) -> String {
    let mut out = format!(
        "const response = await fetch({}, {{\n  headers: {{\n",
        dq(&s.http_url())
    );
    for (k, v) in s.sse_headers() {
        let _ = writeln!(out, "    {}: {},", dq(&k), dq(&v));
    }
    out.push_str(
        "  },\n});\n\
         // EventSource can't send headers, so the body is read as it arrives (browsers,\n\
         // Node.js 18+).\n\
         const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();\n\
         for (;;) {\n  const { value, done } = await reader.read();\n  if (done) break;\n  \
         console.log(value);\n}\n",
    );
    out
}

fn sse_python(s: &Stream) -> String {
    let mut out = format!(
        "import requests\n\nurl = {}\nheaders = {{\n",
        dq(&s.http_url())
    );
    for (k, v) in s.sse_headers() {
        let _ = writeln!(out, "    {}: {},", dq(&k), dq(&v));
    }
    out.push_str(
        "}\nwith requests.get(url, headers=headers, stream=True) as response:\n    \
         for line in response.iter_lines(decode_unicode=True):\n        if line:\n            \
         print(line)\n",
    );
    out
}

fn sse_go(s: &Stream) -> String {
    let mut out = String::from(
        "package main\n\nimport (\n\t\"bufio\"\n\t\"fmt\"\n\t\"net/http\"\n)\n\nfunc main() {\n",
    );
    let _ = writeln!(
        out,
        "\treq, err := http.NewRequest(\"GET\", {}, nil)",
        dq(&s.http_url())
    );
    out.push_str("\tif err != nil {\n\t\tpanic(err)\n\t}\n");
    for (k, v) in s.sse_headers() {
        let _ = writeln!(out, "\treq.Header.Set({}, {})", dq(&k), dq(&v));
    }
    out.push_str(
        "\tresp, err := http.DefaultClient.Do(req)\n\tif err != nil {\n\t\tpanic(err)\n\t}\n\
         \tdefer resp.Body.Close()\n\tscanner := bufio.NewScanner(resp.Body)\n\
         \tfor scanner.Scan() {\n\t\tfmt.Println(scanner.Text())\n\t}\n}\n",
    );
    out
}

fn ws_websocat(s: &Stream) -> String {
    let mut out = String::from("# Each line typed is sent; what arrives is printed.\nwebsocat");
    for (k, v) in &s.headers {
        let _ = write!(out, " \\\n  -H {}", sh(&format!("{k}: {v}")));
    }
    let _ = write!(out, " \\\n  {}", sh(&s.ws_url()));
    out
}

fn ws_browser(s: &Stream) -> String {
    let mut out = String::new();
    if !s.headers.is_empty() {
        out.push_str("// A browser can't set headers on a WebSocket; these aren't sent:\n");
        for (k, v) in &s.headers {
            let _ = writeln!(out, "//   {k}: {v}");
        }
    }
    let _ = write!(
        out,
        "const socket = new WebSocket({});\n\
         socket.addEventListener(\"open\", () => socket.send({}));\n\
         socket.addEventListener(\"message\", (event) => console.log(event.data));\n",
        dq(&s.ws_url()),
        dq(MESSAGE)
    );
    out
}

fn ws_node(s: &Stream) -> String {
    let mut out = format!(
        "import WebSocket from \"ws\";\n\nconst socket = new WebSocket({}",
        dq(&s.ws_url())
    );
    if !s.headers.is_empty() {
        out.push_str(", {\n  headers: {\n");
        for (k, v) in &s.headers {
            let _ = writeln!(out, "    {}: {},", dq(k), dq(v));
        }
        out.push_str("  },\n}");
    }
    let _ = write!(
        out,
        ");\nsocket.on(\"open\", () => socket.send({}));\n\
         socket.on(\"message\", (data) => console.log(data.toString()));\n",
        dq(MESSAGE)
    );
    out
}

fn ws_python(s: &Stream) -> String {
    let mut out = String::from("import asyncio\n\nimport websockets\n\n\nasync def main():\n");
    let mut args = dq(&s.ws_url());
    if !s.headers.is_empty() {
        out.push_str("    headers = {\n");
        for (k, v) in &s.headers {
            let _ = writeln!(out, "        {}: {},", dq(k), dq(v));
        }
        out.push_str("    }\n");
        args.push_str(", additional_headers=headers");
    }
    let _ = write!(
        out,
        "    async with websockets.connect({args}) as socket:\n        \
         await socket.send({})\n        async for message in socket:\n            \
         print(message)\n\n\nasyncio.run(main())\n",
        dq(MESSAGE)
    );
    out
}

fn ws_go(s: &Stream) -> String {
    let mut out = String::from(
        "package main\n\nimport (\n\t\"fmt\"\n\t\"net/http\"\n\n\t\"github.com/gorilla/websocket\"\n)\n\n\
         func main() {\n\theader := http.Header{}\n",
    );
    for (k, v) in &s.headers {
        let _ = writeln!(out, "\theader.Set({}, {})", dq(k), dq(v));
    }
    let _ = write!(
        out,
        "\tconn, _, err := websocket.DefaultDialer.Dial({}, header)\n\
         \tif err != nil {{\n\t\tpanic(err)\n\t}}\n\tdefer conn.Close()\n\
         \tif err := conn.WriteMessage(websocket.TextMessage, []byte({})); err != nil {{\n\
         \t\tpanic(err)\n\t}}\n\tfor {{\n\t\t_, message, err := conn.ReadMessage()\n\
         \t\tif err != nil {{\n\t\t\tpanic(err)\n\t\t}}\n\t\tfmt.Println(string(message))\n\t}}\n}}\n",
        dq(&s.ws_url()),
        dq(MESSAGE)
    );
    out
}

/// Socket.IO's namespace is the URL's path; the server is what comes before it.
fn sio_parts(s: &Stream) -> (String, String) {
    match reqwest::Url::parse(&s.http_url()) {
        Ok(url) => {
            let ns = match url.path().trim_end_matches('/') {
                "" => "/".to_owned(),
                p => p.to_owned(),
            };
            (url.origin().ascii_serialization(), ns)
        }
        Err(_) => (s.http_url(), "/".into()),
    }
}

fn sio_node(s: &Stream) -> String {
    let sio = &s.req.socketio;
    // 2.x exports `io` only as the module itself, and has no auth or onAny.
    let mut out = match sio.v2 {
        true => "// A Socket.IO 2.x server takes only socket.io-client 2 (npm install \
                 socket.io-client@2).\nimport io from \"socket.io-client\";\n\n"
            .to_owned(),
        false => "import { io } from \"socket.io-client\";\n\n".to_owned(),
    };
    let _ = writeln!(out, "const socket = io({}, {{", dq(&s.http_url()));
    if !sio.path.trim().is_empty() {
        let _ = writeln!(out, "  path: {},", dq(&sio.handshake_path()));
    }
    out.push_str("  transports: [\"websocket\"],\n");
    if !s.headers.is_empty() {
        out.push_str("  extraHeaders: {\n");
        for (k, v) in &s.headers {
            let _ = writeln!(out, "    {}: {},", dq(k), dq(v));
        }
        out.push_str("  },\n");
    }
    // The JSON body is the CONNECT payload, as in the app; JSON is a JavaScript literal.
    if !sio.v2 && !s.body_text().is_empty() {
        let _ = writeln!(out, "  auth: {},", s.body_text());
    }
    let _ = write!(
        out,
        "}});\nsocket.on(\"connect\", () => {{\n  console.log(\"connected\", socket.id);\n  \
         socket.emit(\"message\", {});\n}});\n\
         socket.on(\"connect_error\", (err) => console.error(err.message));\n",
        dq(MESSAGE)
    );
    out.push_str(match sio.v2 {
        true => "socket.on(\"message\", (...args) => console.log(\"message\", ...args));\n",
        false => "socket.onAny((event, ...args) => console.log(event, ...args));\n",
    });
    out
}

fn sio_python(s: &Stream) -> String {
    let (server, ns) = sio_parts(s);
    let sio = &s.req.socketio;
    // 4.x has no auth and no "*" handler.
    let auth = if sio.v2 { "" } else { s.body_text() };
    let mut out = String::new();
    if sio.v2 {
        out.push_str(
            "# A Socket.IO 2.x server takes only python-socketio 4\n\
             # (pip install \"python-socketio[client]<5\").\n",
        );
    }
    if !auth.is_empty() {
        out.push_str("import json\n\n");
    }
    let (on, handler) = match sio.v2 {
        true => ("message", "message(*args):\n    print(\"message\", *args)"),
        false => ("*", "any_event(event, *args):\n    print(event, *args)"),
    };
    let _ = write!(
        out,
        "import socketio\n\nNAMESPACE = {}\nsio = socketio.Client()\n\n\n\
         @sio.on(\"connect\", namespace=NAMESPACE)\ndef connect():\n    print(\"connected\")\n    \
         sio.emit(\"message\", {}, namespace=NAMESPACE)\n\n\n\
         @sio.on({}, namespace=NAMESPACE)\ndef {handler}\n\n\nsio.connect(\n    {},\n    \
         namespaces=[NAMESPACE],\n    transports=[\"websocket\"],\n",
        dq(&ns),
        dq(MESSAGE),
        dq(on),
        dq(&server)
    );
    if !sio.path.trim().is_empty() {
        let path = sio.handshake_path();
        let _ = writeln!(out, "    socketio_path={},", dq(path.trim_matches('/')));
    }
    if !s.headers.is_empty() {
        out.push_str("    headers={\n");
        for (k, v) in &s.headers {
            let _ = writeln!(out, "        {}: {},", dq(k), dq(v));
        }
        out.push_str("    },\n");
    }
    if !auth.is_empty() {
        let _ = writeln!(out, "    auth=json.loads({}),", dq(auth));
    }
    out.push_str(")\nsio.wait()\n");
    out
}

/// The broker as each client names it: scheme (mqtt, mqtts, ws or wss), host, port, and
/// the path for WebSocket.
struct Broker {
    scheme: &'static str,
    host: String,
    port: u16,
    path: String,
}

impl Broker {
    fn of(url: &str) -> Self {
        let (scheme, rest) = url.trim().split_once("://").unwrap_or(("mqtt", url.trim()));
        let (scheme, port) = match scheme.to_lowercase().as_str() {
            "mqtts" | "ssl" => ("mqtts", 8883),
            "ws" => ("ws", 80),
            "wss" => ("wss", 443),
            _ => ("mqtt", 1883),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, p)) => (host, p.parse().unwrap_or(port)),
            None => (authority, port),
        };
        Self {
            scheme,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        }
    }

    fn tls(&self) -> bool {
        matches!(self.scheme, "mqtts" | "wss")
    }

    fn websocket(&self) -> bool {
        matches!(self.scheme, "ws" | "wss")
    }

    /// `scheme://host:port/path`, with the given name for each scheme.
    fn url(&self, names: [&str; 4]) -> String {
        let scheme = match self.scheme {
            "mqtt" => names[0],
            "mqtts" => names[1],
            "ws" => names[2],
            _ => names[3],
        };
        format!("{scheme}://{}:{}{}", self.host, self.port, self.path)
    }
}

/// Basic auth's name and password: what MQTT signs in with.
fn mqtt_login(s: &Stream) -> Option<(&str, &str)> {
    match &s.req.auth {
        Auth::Basic { username, password } => Some((username, password)),
        _ => None,
    }
}

fn mqtt_topics(s: &Stream) -> Vec<(&str, u8)> {
    (s.req.mqtt.topics.iter())
        .filter(|t| t.enabled && !t.filter.trim().is_empty())
        .map(|t| (t.filter.trim(), t.qos))
        .collect()
}

/// MQTT 5's user properties, sent with each publish like headers.
fn mqtt_properties(s: &Stream) -> Vec<(&str, &str)> {
    let m = &s.req.mqtt;
    (m.user_properties.iter())
        .filter(|p| m.v5 && p.enabled && !p.key.is_empty())
        .map(|p| (p.key.as_str(), p.value.as_str()))
        .collect()
}

/// MQTT 5's other publish properties, by their MQTT names.
fn mqtt_publish_extras(s: &Stream) -> Vec<(&'static str, String)> {
    let m = &s.req.mqtt;
    mqtt_extras(m.v5, &m.content_type, &m.response_topic, m.expiry_secs)
}

/// The will's MQTT 5 properties; none without a will.
fn mqtt_will_extras(s: &Stream) -> Vec<(&'static str, String)> {
    let m = &s.req.mqtt;
    let v5 = m.v5 && !m.will_topic.trim().is_empty();
    mqtt_extras(v5, &m.will_content_type, "", m.will_expiry_secs)
}

fn mqtt_extras(v5: bool, content: &str, reply: &str, expiry: u32) -> Vec<(&'static str, String)> {
    let expiry = if expiry > 0 {
        expiry.to_string()
    } else {
        String::new()
    };
    let all = [
        ("content-type", content.trim().to_owned()),
        ("response-topic", reply.trim().to_owned()),
        ("message-expiry-interval", expiry),
    ];
    (all.into_iter())
        .filter(|(_, v)| v5 && !v.is_empty())
        .collect()
}

/// `(name, value)` in a client's own spelling; the expiry is a number, the rest strings.
fn mqtt_extra(name: &str, value: &str, names: [&'static str; 3]) -> (&'static str, String) {
    match name {
        "content-type" => (names[0], dq(value)),
        "response-topic" => (names[1], dq(value)),
        _ => (names[2], value.to_owned()),
    }
}

const PAHO_NAMES: [&str; 3] = ["ContentType", "ResponseTopic", "MessageExpiryInterval"];
const MQTT_JS_NAMES: [&str; 3] = ["contentType", "responseTopic", "messageExpiryInterval"];

fn mqtt_mosquitto(s: &Stream) -> String {
    let (b, m) = (Broker::of(&s.req.url), &s.req.mqtt);
    if b.websocket() {
        return "# mosquitto_sub and mosquitto_pub don't connect over WebSocket; the Python, \
                Node.js and Go snippets do."
            .into();
    }
    let mut common = format!("-h {} -p {}", sh(&b.host), b.port);
    if m.v5 {
        common.push_str(" -V mqttv5");
    }
    if b.tls() {
        common.push_str(" --tls-use-os-certs");
    }
    if let Some((user, pass)) = mqtt_login(s) {
        let _ = write!(common, " -u {} -P {}", sh(user), sh(pass));
    }
    let mut out = String::new();
    let topics = mqtt_topics(s);
    if !topics.is_empty() {
        out.push_str("# Subscribe, printing each message's topic and payload.\n");
        let _ = write!(out, "mosquitto_sub {common} -v");
        if !m.client_id.trim().is_empty() {
            let _ = write!(out, " -i {}", sh(m.client_id.trim()));
            if !m.clean_session {
                out.push_str(" -c");
            }
        }
        let _ = write!(out, " -k {}", m.keep_alive_secs);
        if !m.will_topic.trim().is_empty() {
            let _ = write!(
                out,
                " --will-topic {} --will-payload {} --will-qos {}",
                sh(m.will_topic.trim()),
                sh(&m.will_payload),
                m.will_qos
            );
            if m.will_retain {
                out.push_str(" --will-retain");
            }
            for (k, v) in mqtt_will_extras(s) {
                let _ = write!(out, " -D will {k} {}", sh(&v));
            }
        }
        // One QoS for every topic: the highest asked for.
        let qos = topics.iter().map(|(_, q)| *q).max().unwrap_or(0);
        let _ = write!(out, " -q {qos}");
        for (filter, _) in &topics {
            let _ = write!(out, " -t {}", sh(filter));
        }
        out.push('\n');
    }
    if !m.topic.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("# Publish one message.\n");
        let _ = write!(
            out,
            "mosquitto_pub {common} -t {} -q {}",
            sh(m.topic.trim()),
            m.qos
        );
        if m.retain {
            out.push_str(" -r");
        }
        for (k, v) in mqtt_publish_extras(s) {
            let _ = write!(out, " -D publish {k} {}", sh(&v));
        }
        for (k, v) in mqtt_properties(s) {
            let _ = write!(out, " -D publish user-property {} {}", sh(k), sh(v));
        }
        let _ = writeln!(out, " -m {}", sh(MESSAGE));
    }
    if out.is_empty() {
        out.push_str("# Add a topic to subscribe to, or one to publish to.\n");
    }
    out
}

fn mqtt_python(s: &Stream) -> String {
    let (b, m) = (Broker::of(&s.req.url), &s.req.mqtt);
    let user_properties = mqtt_properties(s);
    let (extras, will_extras) = (mqtt_publish_extras(s), mqtt_will_extras(s));
    let publishes = !user_properties.is_empty() || !extras.is_empty();
    let mut out = String::from("import paho.mqtt.client as mqtt\n");
    if publishes || !will_extras.is_empty() {
        out.push_str(
            "from paho.mqtt.packettypes import PacketTypes\n\
             from paho.mqtt.properties import Properties\n",
        );
    }
    if publishes {
        out.push_str("\npublish_properties = Properties(PacketTypes.PUBLISH)\n");
        for (k, v) in &extras {
            let (k, v) = mqtt_extra(k, v, PAHO_NAMES);
            let _ = writeln!(out, "publish_properties.{k} = {v}");
        }
    }
    if !user_properties.is_empty() {
        out.push_str("publish_properties.UserProperty = [\n");
        for (k, v) in &user_properties {
            let _ = writeln!(out, "    ({}, {}),", dq(k), dq(v));
        }
        out.push_str("]\n");
    }
    if !will_extras.is_empty() {
        out.push_str("\nwill_properties = Properties(PacketTypes.WILLMESSAGE)\n");
        for (k, v) in &will_extras {
            let (k, v) = mqtt_extra(k, v, PAHO_NAMES);
            let _ = writeln!(out, "will_properties.{k} = {v}");
        }
    }
    out.push_str(
        "\n\ndef on_connect(client, userdata, flags, reason_code, properties):\n    \
         print(\"connected:\", reason_code)\n",
    );
    let topics = mqtt_topics(s);
    if !topics.is_empty() {
        let list: Vec<String> = topics
            .iter()
            .map(|(f, q)| format!("({}, {q})", dq(f)))
            .collect();
        let _ = writeln!(out, "    client.subscribe([{}])", list.join(", "));
    }
    if !m.topic.trim().is_empty() {
        let retain = if m.retain { "True" } else { "False" };
        let properties = match publishes {
            true => ", properties=publish_properties",
            false => "",
        };
        let _ = writeln!(
            out,
            "    client.publish({}, {}, qos={}, retain={retain}{properties})",
            dq(m.topic.trim()),
            dq(MESSAGE),
            m.qos
        );
    }
    out.push_str(
        "\n\ndef on_message(client, userdata, message):\n    \
         print(message.topic, message.payload.decode())\n\n\n\
         client = mqtt.Client(\n    mqtt.CallbackAPIVersion.VERSION2,\n",
    );
    if !m.client_id.trim().is_empty() {
        let _ = writeln!(out, "    client_id={},", dq(m.client_id.trim()));
    }
    match m.v5 {
        true => out.push_str("    protocol=mqtt.MQTTv5,\n"),
        // MQTT 5 says clean start at connect instead.
        false => {
            let clean = if m.clean_session { "True" } else { "False" };
            let _ = writeln!(out, "    clean_session={clean},");
        }
    }
    if b.websocket() {
        out.push_str("    transport=\"websockets\",\n");
    }
    out.push_str(")\n");
    if let Some((user, pass)) = mqtt_login(s) {
        let _ = writeln!(out, "client.username_pw_set({}, {})", dq(user), dq(pass));
    }
    if b.tls() {
        out.push_str("client.tls_set()\n");
    }
    if b.websocket() && !b.path.is_empty() {
        let _ = writeln!(out, "client.ws_set_options(path={})", dq(&b.path));
    }
    if !m.will_topic.trim().is_empty() {
        let retain = if m.will_retain { "True" } else { "False" };
        let properties = match will_extras.is_empty() {
            true => "",
            false => ", properties=will_properties",
        };
        let _ = writeln!(
            out,
            "client.will_set({}, {}, qos={}, retain={retain}{properties})",
            dq(m.will_topic.trim()),
            dq(&m.will_payload),
            m.will_qos
        );
    }
    let _ = write!(
        out,
        "client.on_connect = on_connect\nclient.on_message = on_message\n\
         client.connect({}, {}, keepalive={}",
        dq(&b.host),
        b.port,
        m.keep_alive_secs
    );
    if m.v5 {
        let clean = if m.clean_session { "True" } else { "False" };
        let _ = write!(out, ", clean_start={clean}");
    }
    out.push_str(")\nclient.loop_forever()\n");
    out
}

fn mqtt_node(s: &Stream) -> String {
    let (b, m) = (Broker::of(&s.req.url), &s.req.mqtt);
    let mut out = format!(
        "import mqtt from \"mqtt\";\n\nconst client = mqtt.connect({}, {{\n",
        dq(&b.url(["mqtt", "mqtts", "ws", "wss"]))
    );
    if !m.client_id.trim().is_empty() {
        let _ = writeln!(out, "  clientId: {},", dq(m.client_id.trim()));
    }
    if let Some((user, pass)) = mqtt_login(s) {
        let _ = writeln!(out, "  username: {},\n  password: {},", dq(user), dq(pass));
    }
    let _ = writeln!(
        out,
        "  protocolVersion: {},\n  keepalive: {},\n  clean: {},",
        if m.v5 { 5 } else { 4 },
        m.keep_alive_secs,
        m.clean_session
    );
    if !m.will_topic.trim().is_empty() {
        let extras: Vec<String> = (mqtt_will_extras(s).iter())
            .map(|(k, v)| mqtt_extra(k, v, MQTT_JS_NAMES))
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        let properties = match extras.is_empty() {
            true => String::new(),
            false => format!(", properties: {{ {} }}", extras.join(", ")),
        };
        let _ = writeln!(
            out,
            "  will: {{ topic: {}, payload: {}, qos: {}, retain: {}{properties} }},",
            dq(m.will_topic.trim()),
            dq(&m.will_payload),
            m.will_qos,
            m.will_retain
        );
    }
    out.push_str("});\nclient.on(\"connect\", () => {\n");
    let topics = mqtt_topics(s);
    if !topics.is_empty() {
        let list: Vec<String> = (topics.iter())
            .map(|(f, q)| format!("{}: {{ qos: {q} }}", dq(f)))
            .collect();
        let _ = writeln!(out, "  client.subscribe({{ {} }});", list.join(", "));
    }
    if !m.topic.trim().is_empty() {
        let mut options = format!("qos: {}, retain: {}", m.qos, m.retain);
        let mut properties: Vec<String> = (mqtt_publish_extras(s).iter())
            .map(|(k, v)| mqtt_extra(k, v, MQTT_JS_NAMES))
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        let user = mqtt_properties(s);
        if !user.is_empty() {
            let list: Vec<String> = (user.iter())
                .map(|(k, v)| format!("{}: {}", dq(k), dq(v)))
                .collect();
            properties.push(format!("userProperties: {{ {} }}", list.join(", ")));
        }
        if !properties.is_empty() {
            let _ = write!(options, ", properties: {{ {} }}", properties.join(", "));
        }
        let _ = writeln!(
            out,
            "  client.publish({}, {}, {{ {options} }});",
            dq(m.topic.trim()),
            dq(MESSAGE)
        );
    }
    out.push_str(
        "});\nclient.on(\"message\", (topic, payload) => console.log(topic, payload.toString()));\n\
         client.on(\"error\", (err) => console.error(err.message));\n",
    );
    out
}

fn mqtt_go(s: &Stream) -> String {
    let (b, m) = (Broker::of(&s.req.url), &s.req.mqtt);
    let mut out = String::new();
    if m.v5 {
        out.push_str(
            "// paho.mqtt.golang speaks MQTT 3.1.1; for MQTT 5 there is github.com/eclipse/paho.golang.\n",
        );
        for (k, v) in mqtt_properties(s) {
            let _ = writeln!(out, "// Left out, as MQTT 5 user property: {k}: {v}");
        }
        for (k, v) in mqtt_publish_extras(s) {
            let _ = writeln!(out, "// Left out, as MQTT 5 publish property: {k}: {v}");
        }
        for (k, v) in mqtt_will_extras(s) {
            let _ = writeln!(out, "// Left out, as MQTT 5 will property: {k}: {v}");
        }
    }
    let _ = write!(
        out,
        "package main\n\nimport (\n\t\"fmt\"\n\t\"time\"\n\n\tmqtt \"github.com/eclipse/paho.mqtt.golang\"\n)\n\n\
         func main() {{\n\topts := mqtt.NewClientOptions().AddBroker({})\n",
        dq(&b.url(["tcp", "ssl", "ws", "wss"]))
    );
    if !m.client_id.trim().is_empty() {
        let _ = writeln!(out, "\topts.SetClientID({})", dq(m.client_id.trim()));
    }
    if let Some((user, pass)) = mqtt_login(s) {
        let _ = writeln!(
            out,
            "\topts.SetUsername({})\n\topts.SetPassword({})",
            dq(user),
            dq(pass)
        );
    }
    let _ = writeln!(
        out,
        "\topts.SetKeepAlive({} * time.Second)\n\topts.SetCleanSession({})",
        m.keep_alive_secs, m.clean_session
    );
    if !m.will_topic.trim().is_empty() {
        let _ = writeln!(
            out,
            "\topts.SetWill({}, {}, {}, {})",
            dq(m.will_topic.trim()),
            dq(&m.will_payload),
            m.will_qos,
            m.will_retain
        );
    }
    out.push_str("\topts.OnConnect = func(c mqtt.Client) {\n");
    let topics = mqtt_topics(s);
    if !topics.is_empty() {
        let list: Vec<String> = topics
            .iter()
            .map(|(f, q)| format!("{}: {q}", dq(f)))
            .collect();
        let _ = writeln!(
            out,
            "\t\tc.SubscribeMultiple(map[string]byte{{{}}}, func(_ mqtt.Client, m mqtt.Message) {{\n\
             \t\t\tfmt.Println(m.Topic(), string(m.Payload()))\n\t\t}})",
            list.join(", ")
        );
    }
    if !m.topic.trim().is_empty() {
        let _ = writeln!(
            out,
            "\t\tc.Publish({}, {}, {}, {})",
            dq(m.topic.trim()),
            m.qos,
            m.retain,
            dq(MESSAGE)
        );
    }
    out.push_str(
        "\t}\n\tclient := mqtt.NewClient(opts)\n\
         \tif token := client.Connect(); token.Wait() && token.Error() != nil {\n\
         \t\tpanic(token.Error())\n\t}\n\tselect {}\n}\n",
    );
    out
}

/// gRPC's address (host:port, as its tools want it) and whether it is TLS.
fn grpc_address(s: &Stream) -> (String, bool) {
    match reqwest::Url::parse(&s.http_url()) {
        Ok(url) => {
            let host = url.host_str().unwrap_or_default();
            let port = url.port_or_known_default().unwrap_or(80);
            (format!("{host}:{port}"), url.scheme() == "https")
        }
        Err(_) => (s.req.url.trim().to_owned(), false),
    }
}

/// The messages to send, compact: the body, or for a client stream each item of its
/// array, as Connect sends them.
fn grpc_messages(s: &Stream) -> Vec<String> {
    let text = s.body_text();
    let streaming = s.rpc.as_ref().is_some_and(|r| r.client_streaming);
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Array(items)) if streaming => {
            items.iter().map(|v| v.to_string()).collect()
        }
        Ok(v) => vec![v.to_string()],
        Err(_) if text.is_empty() => vec!["{}".into()],
        Err(_) => vec![text.to_owned()],
    }
}

/// `package.Service/Method` as (service with its package, method).
fn grpc_names(s: &Stream) -> (&str, &str) {
    s.req
        .rpc
        .trim()
        .rsplit_once('/')
        .unwrap_or(("package.Service", "Method"))
}

fn grpc_grpcurl(s: &Stream) -> String {
    let (address, tls) = grpc_address(s);
    let mut out = String::from("grpcurl");
    if !tls {
        out.push_str(" -plaintext");
    }
    let proto = s.req.proto.trim();
    if !proto.is_empty() {
        let (dir, file) = proto.rsplit_once(['/', '\\']).unwrap_or((".", proto));
        let _ = write!(out, " \\\n  -import-path {} -proto {}", sh(dir), sh(file));
        let imports = s.req.settings.import_paths.iter().map(|p| p.trim());
        for path in imports.filter(|p| !p.is_empty()) {
            let _ = write!(out, " -import-path {}", sh(path));
        }
    }
    let name = s.req.settings.tls_server_name.trim();
    if tls && !name.is_empty() {
        let _ = write!(out, " \\\n  -authority {}", sh(name));
    }
    for (k, v) in &s.headers {
        let _ = write!(out, " \\\n  -H {}", sh(&format!("{k}: {v}")));
    }
    let _ = write!(
        out,
        " \\\n  -d {} \\\n  {} {}",
        sh(&grpc_messages(s).join("\n")),
        sh(&address),
        sh(s.req.rpc.trim())
    );
    out
}

fn grpc_node(s: &Stream) -> String {
    let (address, tls) = grpc_address(s);
    let (service, method) = grpc_names(s);
    let proto = s.req.proto.trim();
    let mut out = String::from(
        "import grpc from \"@grpc/grpc-js\";\nimport protoLoader from \"@grpc/proto-loader\";\n\n",
    );
    if proto.is_empty() {
        out.push_str("// No .proto in the request: save the service's .proto and name it here.\n");
    }
    let file = if proto.is_empty() {
        "service.proto"
    } else {
        proto
    };
    let _ = write!(
        out,
        "const definition = protoLoader.loadSync({}, {{\n  keepCase: true,\n  longs: String,\n  \
         enums: String,\n  defaults: true,\n  oneofs: true,\n}});\n\
         const proto = grpc.loadPackageDefinition(definition);\n\
         const client = new proto.{service}({}, grpc.credentials.{}());\n\
         const metadata = new grpc.Metadata();\n",
        dq(file),
        dq(&address),
        if tls { "createSsl" } else { "createInsecure" }
    );
    for (k, v) in &s.headers {
        let _ = writeln!(out, "metadata.add({}, {});", dq(k), dq(v));
    }
    let messages = grpc_messages(s);
    let (client, server) = s
        .rpc
        .as_ref()
        .map_or((false, false), |r| (r.client_streaming, r.server_streaming));
    let answer = "(err, response) => {\n  if (err) throw err;\n  console.log(response);\n}";
    let on_data = "call.on(\"data\", (message) => console.log(message));\n\
                   call.on(\"error\", (err) => console.error(err.message));\n";
    match (client, server) {
        (false, false) => {
            let _ = writeln!(out, "client.{method}({}, metadata, {answer});", messages[0]);
        }
        (false, true) => {
            let _ = write!(
                out,
                "const call = client.{method}({}, metadata);\n{on_data}",
                messages[0]
            );
        }
        (true, false) => {
            let _ = writeln!(out, "const call = client.{method}(metadata, {answer});");
        }
        (true, true) => {
            let _ = write!(out, "const call = client.{method}(metadata);\n{on_data}");
        }
    }
    if client {
        for m in &messages {
            let _ = writeln!(out, "call.write({m});");
        }
        out.push_str("call.end();\n");
    }
    out
}

fn grpc_python(s: &Stream) -> String {
    let (address, tls) = grpc_address(s);
    let (service, method) = grpc_names(s);
    let short = service.rsplit('.').next().unwrap_or(service);
    let proto = s.req.proto.trim();
    // protos_and_services only looks the file up along sys.path.
    let (dir, file) = match proto.rsplit_once(['/', '\\']) {
        Some((dir, file)) => (Some(dir), file),
        None => (None, proto),
    };
    let mut out = String::new();
    if dir.is_some() {
        out.push_str("import sys\n\n");
    }
    out.push_str(
        "import grpc\nfrom google.protobuf import json_format\n\n\
         # protos_and_services compiles the .proto at run time (pip install grpcio grpcio-tools).\n",
    );
    if let Some(dir) = dir {
        let _ = writeln!(out, "sys.path.append({})", dq(dir));
    }
    if proto.is_empty() {
        out.push_str("# No .proto in the request: save the service's .proto beside this script.\n");
    }
    let file = if proto.is_empty() {
        "service.proto"
    } else {
        file
    };
    let _ = writeln!(
        out,
        "protos, services = grpc.protos_and_services({})",
        dq(file)
    );
    match tls {
        true => {
            let _ = writeln!(
                out,
                "channel = grpc.secure_channel({}, grpc.ssl_channel_credentials())",
                dq(&address)
            );
        }
        false => {
            let _ = writeln!(out, "channel = grpc.insecure_channel({})", dq(&address));
        }
    }
    let _ = writeln!(out, "stub = services.{short}Stub(channel)");
    let list: Vec<String> = (s.headers.iter())
        .map(|(k, v)| format!("({}, {})", dq(k), dq(v)))
        .collect();
    let _ = writeln!(out, "metadata = [{}]", list.join(", "));
    let input = s.rpc.as_ref().map_or("Request", |r| r.input.as_str());
    let parse = |m: &str| format!("json_format.Parse({}, protos.{input}())", dq(m));
    let messages = grpc_messages(s);
    let (client, server) = s
        .rpc
        .as_ref()
        .map_or((false, false), |r| (r.client_streaming, r.server_streaming));
    let request = match client {
        true => {
            let list: Vec<String> = messages
                .iter()
                .map(|m| format!("    {},\n", parse(m)))
                .collect();
            let _ = writeln!(out, "requests = [\n{}]", list.concat());
            "iter(requests)".to_owned()
        }
        false => {
            let _ = writeln!(out, "request = {}", parse(&messages[0]));
            "request".to_owned()
        }
    };
    match server {
        true => {
            let _ = write!(
                out,
                "for response in stub.{method}({request}, metadata=metadata):\n    \
                 print(json_format.MessageToJson(response))\n"
            );
        }
        false => {
            let _ = write!(
                out,
                "response = stub.{method}({request}, metadata=metadata)\n\
                 print(json_format.MessageToJson(response))\n"
            );
        }
    }
    out
}

type Vars = std::collections::HashMap<String, String>;

/// A request under a folder as Send makes it: named, resolved, its first saved example.
struct Made {
    name: String,
    req: Request,
    example: Option<crate::model::Example>,
}

/// The requests under `dir`, resolved with `globals`, `env` and their folders' variables
/// in that precedence, as Send resolves them; and how many were left out: streams and
/// gRPC, which neither curl nor HAR speaks.
fn made_under(
    ws: &Workspace,
    dir: &Path,
    globals: &Vars,
    env: &Vars,
) -> Result<(Vec<Made>, usize), String> {
    let mut paths = Vec::new();
    crate::store::requests_in(&ws.tree(), dir, &mut paths);
    let (mut made, mut left) = (Vec::new(), 0);
    for path in paths {
        let req = ws.load_request(&path)?;
        if is_streaming(&req.method) || req.method == "GRPC" {
            left += 1;
            continue;
        }
        let vars = crate::runner::scoped(globals, env, &req.inherited.vars);
        made.push(Made {
            name: req.name.clone(),
            example: req.examples.first().cloned(),
            req: req.resolved(&vars).0,
        });
    }
    Ok((made, left))
}

/// The requests under `dir` as a shell script: a cURL command each, under a comment naming
/// it, which every API client's cURL import takes one at a time.
pub fn curl_script(
    ws: &Workspace,
    dir: &Path,
    globals: &Vars,
    env: &Vars,
) -> Result<(String, usize), String> {
    let (made, left) = made_under(ws, dir, globals, env)?;
    let mut out = String::new();
    for Made { name, req, .. } in made {
        // One that can't be made says why where it would be, not failing the rest.
        let command = Wire::new(req).map_or_else(|e| format!("# {e}"), |w| curl(&w));
        let _ = write!(out, "# {name}\n{command}\n\n");
    }
    Ok((out, left))
}

/// The requests under `dir` as a HAR 1.2 log, which Hoppscotch and Insomnia import: an
/// entry each, named in its comment as Insomnia reads it, answered by its first saved
/// example, else status 0 as a request never answered.
pub fn har(
    ws: &Workspace,
    dir: &Path,
    globals: &Vars,
    env: &Vars,
) -> Result<(String, usize), String> {
    let (made, mut left) = made_under(ws, dir, globals, env)?;
    let started = crate::fake::value("$isoTimestamp").unwrap_or_default();
    let pairs = |list: Vec<(String, String)>| -> Vec<Value> {
        (list.into_iter())
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect()
    };
    let mut entries = Vec::new();
    for Made { name, req, example } in made {
        let form = match &req.body {
            Body::Form { fields } => Some(fields.clone()),
            _ => None,
        };
        let Ok(w) = Wire::new(req) else {
            left += 1;
            continue;
        };
        let typed = w.header("content-type").unwrap_or_default().to_owned();
        let query = pairs(
            w.url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        );
        let mut request = json!({
            "method": w.method, "url": w.url.as_str(), "httpVersion": "HTTP/1.1",
            "cookies": [], "headers": pairs(w.headers.clone()), "queryString": query,
            "headersSize": -1, "bodySize": w.body.as_ref().map_or(0, String::len),
            "comment": name,
        });
        if let Some(text) = &w.body {
            request["postData"] = json!({ "mimeType": typed, "text": text });
            // A form's fields as well, as browsers write them.
            if let Some(fields) = form {
                let fields = fields.into_iter().map(|f| (f.key, f.value)).collect();
                request["postData"]["params"] = pairs(fields).into();
            }
        } else if !w.parts.is_empty() {
            let params: Vec<Value> = (w.parts.iter())
                .map(|(name, part)| match part {
                    Part::Text(value) => json!({ "name": name, "value": value }),
                    Part::File(path) => json!({ "name": name, "fileName": path }),
                })
                .collect();
            request["postData"] = json!({ "mimeType": "multipart/form-data", "params": params });
        }
        let response = match example {
            Some(e) => {
                let reason = e
                    .name
                    .strip_prefix(&e.status.to_string())
                    .unwrap_or(&e.name);
                json!({
                    "status": e.status, "statusText": reason.trim(), "httpVersion": "HTTP/1.1",
                    "cookies": [], "headers": [], "redirectURL": "",
                    "content": { "size": e.body.len(), "mimeType": e.content_type, "text": e.body },
                    "headersSize": -1, "bodySize": e.body.len(),
                })
            }
            None => json!({
                "status": 0, "statusText": "", "httpVersion": "HTTP/1.1", "cookies": [],
                "headers": [], "redirectURL": "", "content": { "size": 0, "mimeType": "" },
                "headersSize": -1, "bodySize": -1,
            }),
        };
        entries.push(json!({
            "startedDateTime": started, "time": 0, "request": request, "response": response,
            "cache": {}, "timings": { "send": 0, "wait": 0, "receive": 0 },
        }));
    }
    let log = json!({ "log": {
        "version": "1.2",
        "creator": { "name": "apitool", "version": env!("CARGO_PKG_VERSION") },
        "entries": entries,
    }});
    let text = serde_json::to_string_pretty(&log).map_err(|e| e.to_string())?;
    Ok((text, left))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KeyValue;

    /// A folder as HAR or cURL holds each request as Send makes it: the environment's
    /// variables over its folders' over the globals, inherited auth as its header, a form's
    /// fields too; in the shape Hoppscotch's HAR import checks (it rejects one without
    /// httpVersion, cookies, queryString or the sizes); and back through apitool's own
    /// imports as the same requests. Streams are counted, not made.
    #[test]
    fn a_folder_goes_out_as_har_and_curl_as_sent() {
        use crate::model::{Example, Folder};
        use std::collections::HashMap;
        let root = std::env::temp_dir().join(format!("apitool-har-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "api").unwrap();
        let folder = Folder {
            vars: vec![
                KeyValue::new("base", "https://folder.test"),
                KeyValue::new("page", "2"),
            ],
            auth: Auth::Bearer {
                token: "{{token}}".into(),
            },
            ..Default::default()
        };
        ws.save_folder(&api, &folder).unwrap();
        let items = Request {
            method: "POST".into(),
            url: "{{base}}/items?page={{page}}".into(),
            body: Body::Json {
                text: "{\"a\":1}".into(),
            },
            examples: vec![Example {
                name: "201 Created".into(),
                status: 201,
                content_type: "application/json".into(),
                body: "{\"id\":9}".into(),
            }],
            ..Default::default()
        };
        let login = Request {
            method: "POST".into(),
            url: "{{base}}/login".into(),
            body: Body::Form {
                fields: vec![KeyValue::new("user", "{{user}}")],
            },
            auth: Auth::None,
            ..Default::default()
        };
        let live = Request {
            method: "WS".into(),
            url: "wss://x.test".into(),
            ..Default::default()
        };
        // Bytes that aren't text: no command can hold them, which the export says instead
        // of failing.
        let raw = Request {
            method: "PUT".into(),
            url: "{{base}}/raw".into(),
            body: Body::Base64 {
                text: "/w==".into(),
            },
            ..Default::default()
        };
        for (name, req) in [
            ("api/items", items),
            ("api/live", live),
            ("api/login", login),
            ("api/raw", raw),
        ] {
            ws.save_request(&ws.request_path(name).unwrap(), &req)
                .unwrap();
        }
        let vars = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            (pairs.iter())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let globals = vars(&[("user", "me"), ("base", "https://global.test")]);
        let env = vars(&[("token", "t"), ("base", "https://env.test")]);

        let (text, left) = har(&ws, &api, &globals, &env).unwrap();
        assert_eq!(left, 2, "the WebSocket, and the bytes");
        let v: Value = serde_json::from_str(&text).unwrap();
        let entries = v["log"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            let r = &entry["request"];
            for key in [
                "httpVersion",
                "cookies",
                "headers",
                "queryString",
                "headersSize",
                "bodySize",
            ] {
                assert!(!r[key].is_null(), "{key}");
            }
        }
        assert_eq!(entries[0]["request"]["comment"], "items");
        let crate::postman::Import::Collection { requests, .. } = crate::har::import(&v) else {
            panic!("a collection")
        };
        let (_, first) = &requests[0];
        // The folder's base, set where the requests live, wins over the environment's.
        assert_eq!(first.url, "https://folder.test/items?page=2");
        let bearer = KeyValue::new("authorization", "Bearer t");
        assert!(first.headers.contains(&bearer), "{:?}", first.headers);
        assert_eq!(
            first.body,
            Body::Json {
                text: "{\"a\":1}".into()
            }
        );
        let answer = &first.examples[0];
        assert_eq!(
            (answer.name.as_str(), answer.body.as_str()),
            ("201 Created", "{\"id\":9}")
        );
        let (_, second) = &requests[1];
        assert_eq!(
            second.body,
            Body::Form {
                fields: vec![KeyValue::new("user", "me")]
            }
        );
        assert!(
            !second.headers.iter().any(|h| h.key == "authorization"),
            "its own: none"
        );

        let (script, left) = curl_script(&ws, &api, &globals, &env).unwrap();
        assert_eq!(left, 1);
        let commands: Vec<_> = script
            .split("\n\n")
            .filter(|c| !c.trim().is_empty())
            .collect();
        assert_eq!(commands.len(), 3, "{script}");
        assert!(commands[0].starts_with("# items\ncurl "), "{script}");
        assert!(
            commands[2].starts_with("# raw\n# This body's bytes aren't text"),
            "{script}"
        );
        let (comment, command) = commands[1].split_once('\n').unwrap();
        assert_eq!(comment, "# login");
        let back = crate::curl::from_curl(command).unwrap();
        assert_eq!(
            (back.method.as_str(), back.url.as_str()),
            ("POST", "https://folder.test/login")
        );
        assert_eq!(
            back.body,
            Body::Form {
                fields: vec![KeyValue::new("user", "me")]
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn string_literals_keep_quotes_escapes_and_interpolation_markers_literal() {
        // Every language must send this value byte for byte: quotes (typographic too), a
        // backslash, control characters, and what Kotlin, Ruby and shells would expand.
        let s = "a\"b\\c\n\t$x #{y} 'q' don’t é\u{7}";
        assert_eq!(serde_json::from_str::<String>(&dq(s)).unwrap(), s);
        assert_eq!(braced(s), r#""a\"b\\c\n\t$x #{y} 'q' don’t é\u{7}""#);
        assert_eq!(kotlin_str(s), r#""a\"b\\c\n\t\$x #{y} 'q' don’t é\u0007""#);
        assert_eq!(ruby_str(s), r#""a\"b\\c\n\t$x \#{y} 'q' don’t é\u0007""#);
        assert_eq!(php_str(s), "'a\"b\\\\c\n\t$x #{y} \\'q\\' don’t é\u{7}'");
        assert_eq!(ps_str(s), "'a\"b\\c\n\t$x #{y} ''q'' don’’t é\u{7}'");
        assert_eq!(sh(s), "'a\"b\\c\n\t$x #{y} '\\''q'\\'' don’t é\u{7}'");
    }

    /// The requests the snippets are checked with. `echo.test:8080` is swapped for a
    /// local server when they are run (see `APITOOL_SNIPPETS` below).
    fn cases() -> Vec<(&'static str, Request)> {
        let tricky = "it's \"quoted\" $HOME #{x} \\ back";
        vec![
            (
                "json",
                Request {
                    method: "POST".into(),
                    url: "http://echo.test:8080/echo?q=a%20b&x=1".into(),
                    headers: vec![KeyValue::new("X-Note", tricky)],
                    auth: Auth::Bearer {
                        token: "t0k".into(),
                    },
                    body: Body::Json {
                        text: "{\n  \"name\": \"O'Brien \\\"Q\\\" $x #{y} é\",\n  \"n\": 1\n}"
                            .into(),
                    },
                    ..Default::default()
                },
            ),
            (
                "form",
                Request {
                    method: "PUT".into(),
                    url: "http://echo.test:8080/form".into(),
                    auth: Auth::Basic {
                        username: "bob".into(),
                        password: "p:w".into(),
                    },
                    body: Body::Form {
                        fields: vec![KeyValue::new("a", "1"), KeyValue::new("b", "x y&z")],
                    },
                    ..Default::default()
                },
            ),
            (
                "multipart",
                Request {
                    method: "POST".into(),
                    url: "http://echo.test:8080/upload".into(),
                    body: Body::Multipart {
                        parts: vec![
                            KeyValue::new("note", tricky),
                            KeyValue::new("doc", "@upload.txt"),
                        ],
                    },
                    ..Default::default()
                },
            ),
            (
                "file",
                Request {
                    method: "POST".into(),
                    url: "http://echo.test:8080/raw".into(),
                    body: Body::File {
                        path: "upload.txt".into(),
                    },
                    ..Default::default()
                },
            ),
            (
                "settings",
                Request {
                    method: "DELETE".into(),
                    url: "http://echo.test:8080/item/7".into(),
                    headers: vec![KeyValue::new("Accept", "text/plain")],
                    settings: Settings {
                        http_version: HttpVersion::Http1,
                        follow_redirects: false,
                        verify_tls: false,
                        timeout_ms: 5000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            (
                "digest",
                Request {
                    url: "http://echo.test:8080/secret".into(),
                    auth: Auth::Digest {
                        username: "u".into(),
                        password: "p".into(),
                    },
                    ..Default::default()
                },
            ),
            ("ntlm", ntlm()),
        ]
    }

    fn ntlm() -> Request {
        Request {
            url: "http://echo.test:8080/intranet".into(),
            auth: Auth::Ntlm(crate::model::Ntlm {
                username: "u".into(),
                password: "p".into(),
                domain: "CORP".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// NTLM is a handshake no header can stand for: the tools that do it are told to,
    /// with the domain where each expects it, and the rest say they can't.
    #[test]
    fn ntlm_is_left_to_the_tools_that_do_the_handshake() {
        let code = |target: &str| generate(target, ntlm()).unwrap();
        let curl = code("cURL");
        assert!(curl.contains(r"--ntlm -u 'CORP\u:p'"), "{curl}");
        assert!(!curl.contains("uthorization"), "{curl}");
        let python = code("Python (requests)");
        assert!(
            python.contains(r#"auth=HttpNtlmAuth("CORP\\u", "p")"#),
            "{python}"
        );
        let cs = code("C# (HttpClient)");
        assert!(
            cs.contains(r#"NetworkCredential("u", "p", "CORP")"#),
            "{cs}"
        );
        let php = code("PHP (cURL)");
        assert!(php.contains("CURLAUTH_NTLM"), "{php}");
        let go = code("Go (net/http)");
        assert!(go.contains("NTLM auth isn't built into net/http"), "{go}");
    }

    #[test]
    fn every_target_renders_every_kind_of_request() {
        let dump = std::env::var_os("APITOOL_SNIPPETS").map(std::path::PathBuf::from);
        for (case, req) in cases() {
            for (name, _) in TARGETS {
                let code = generate(name, req.clone()).unwrap();
                assert!(code.contains("echo.test:8080"), "{name} {case}:\n{code}");
                if let Some(dir) = &dump {
                    let ext = match *name {
                        "cURL" | "wget" | "HTTPie" => "sh",
                        "PowerShell" => "ps1",
                        "HTTP" => "http",
                        "Python (requests)" => "py",
                        "JavaScript (fetch)" => "mjs",
                        "Node.js (axios)" => "cjs",
                        "Go (net/http)" => "go",
                        "Java (HttpClient)" => "java",
                        "C# (HttpClient)" => "cs",
                        "PHP (cURL)" => "php",
                        "Ruby (Net::HTTP)" => "rb",
                        "Rust (reqwest)" => "rs",
                        "Swift (URLSession)" => "swift",
                        "Kotlin (OkHttp)" => "kt",
                        other => panic!("no extension for {other}"),
                    };
                    // "C#" and "Node.js" would trip up file-based runners.
                    let file = name.split(' ').next().unwrap().to_lowercase();
                    let file = file.replace(|c: char| !c.is_ascii_alphanumeric(), "");
                    std::fs::create_dir_all(dir).unwrap();
                    std::fs::write(dir.join(format!("{case}.{file}.{ext}")), code).unwrap();
                }
            }
        }
    }

    /// Java and wget have no multipart helper: the body is put together by hand and sent,
    /// rather than pointing to another snippet (both also run in .github/snippets.py).
    #[test]
    fn java_and_wget_build_a_multipart_body_themselves() {
        let (_, req) = (cases().into_iter())
            .find(|(case, _)| *case == "multipart")
            .unwrap();
        let java = generate("Java (HttpClient)", req.clone()).unwrap();
        let file = r#"part(body, boundary, "doc", "upload.txt", Files.readAllBytes(Path.of("upload.txt")));"#;
        let header = r#".header("Content-Type", "multipart/form-data; boundary=" + boundary)"#;
        assert!(java.contains(file) && java.contains(header), "{java}");
        assert!(java.contains("ofByteArray(body.toByteArray())"), "{java}");

        let wget = generate("wget", req).unwrap();
        assert!(wget.contains(r#"--body-file "$body""#), "{wget}");
        // What it writes before calling wget, run as it is: a body a server can read.
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!("apitool-wget-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("upload.txt"), "hello file\n").unwrap();
            let build = wget.split("\nwget ").next().unwrap();
            let script = format!("{build}\nprintf %s \"$boundary\"; cat \"$body\"; rm \"$body\"");
            let out = std::process::Command::new("bash")
                .args(["-c", &script])
                .current_dir(&dir)
                .output()
                .unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            let out = String::from_utf8(out.stdout).unwrap();
            let (boundary, body) = out.split_at(out.find("--").unwrap());
            assert!(boundary.starts_with("Boundary-"), "{out}");
            let expected = format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\n\
                 it's \"quoted\" $HOME #{{x}} \\ back\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"upload.txt\"\r\n\r\n\
                 hello file\n\r\n--{boundary}--\r\n"
            );
            assert_eq!(body, expected);
        }
    }

    /// Each kind of stream renders every one of its targets, carrying what Connect would
    /// send: the URL as that client spells it, the handshake's headers, the login.
    #[test]
    fn every_stream_renders_what_connect_sends() {
        let dump = std::env::var_os("APITOOL_SNIPPETS").map(std::path::PathBuf::from);
        let dir = std::env::temp_dir().join(format!("apitool-codegen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proto = dir.join("chat.proto");
        std::fs::write(
            &proto,
            "syntax = \"proto3\";\npackage chat.v1;\nmessage Note { string text = 1; }\n\
             service Chat { rpc Talk(stream Note) returns (stream Note); }\n",
        )
        .unwrap();
        let token = Auth::Bearer {
            token: "t0k".into(),
        };
        let note = vec![KeyValue::new("X-Note", "it's")];
        let mqtt = crate::model::Mqtt {
            client_id: "probe".into(),
            v5: true,
            topics: vec![
                crate::model::Topic {
                    filter: "sensors/+".into(),
                    qos: 1,
                    ..Default::default()
                },
                crate::model::Topic {
                    filter: "muted/#".into(),
                    enabled: false,
                    ..Default::default()
                },
            ],
            topic: "out".into(),
            qos: 1,
            retain: true,
            user_properties: vec![KeyValue::new("trace", "42")],
            content_type: "text/csv".into(),
            response_topic: "replies/7".into(),
            expiry_secs: 90,
            will_topic: "gone".into(),
            will_content_type: "text/x-gone".into(),
            will_expiry_secs: 600,
            ..Default::default()
        };
        #[rustfmt::skip]
        let cases: Vec<(Request, &[&str])> = vec![
            (
                Request { method: "SSE".into(), url: "http://h.test:8080/events".into(), auth: token.clone(), ..Default::default() },
                &["http://h.test:8080/events", "text/event-stream", "Bearer t0k"],
            ),
            (
                Request { method: "WS".into(), url: "https://h.test/chat".into(), headers: note.clone(), ..Default::default() },
                &["wss://h.test/chat", "x-note", "it"],
            ),
            (
                Request {
                    method: "SOCKETIO".into(), url: "http://h.test:3000/admin".into(), auth: token.clone(),
                    body: Body::Json { text: r#"{"room": "a"}"#.into() }, ..Default::default()
                },
                &["h.test:3000", "/admin", "Bearer t0k", "room", "websocket"],
            ),
            (
                Request {
                    method: "MQTT".into(), url: "mqtts://broker.test".into(),
                    auth: Auth::Basic { username: "bob".into(), password: "pw".into() }, mqtt: mqtt.clone(), ..Default::default()
                },
                &["broker.test", "8883", "bob", "pw", "sensors/+", "out", "probe", "gone", "trace", "42",
                  "text/csv", "replies/7", "90", "text/x-gone", "600"],
            ),
            (
                Request {
                    method: "GRPC".into(), url: "http://h.test:50051".into(), headers: note,
                    proto: proto.to_string_lossy().into_owned(), rpc: "chat.v1.Chat/Talk".into(),
                    body: Body::Json { text: r#"[{"text": "ay"}, {"text": "bee"}]"#.into() }, ..Default::default()
                },
                &["h.test:50051", "x-note", "Talk", "ay", "bee"],
            ),
        ];
        for (req, pieces) in cases {
            for target in targets(&req.method) {
                let code = generate(target, req.clone()).unwrap();
                for piece in pieces {
                    assert!(code.contains(piece), "{target} lacks {piece}:\n{code}");
                }
                // A topic switched off isn't subscribed to.
                assert!(!code.contains("muted"), "{target}:\n{code}");
                if let Some(dir) = &dump {
                    let ext = match target.split(' ').next().unwrap() {
                        "cURL" | "websocat" | "mosquitto" | "grpcurl" => "sh",
                        "Python" => "py",
                        "JavaScript" | "Node.js" => "mjs",
                        "Go" => "go",
                        other => panic!("no extension for {other}"),
                    };
                    let file = target.replace(|c: char| !c.is_ascii_alphanumeric(), "");
                    std::fs::create_dir_all(dir).unwrap();
                    let name = format!("{}.{file}.{ext}", req.method.to_lowercase());
                    std::fs::write(dir.join(name), code).unwrap();
                }
            }
        }
        // paho takes them only as arguments; defined and left unused, nothing would go out.
        let req = |v5| Request {
            method: "MQTT".into(),
            url: "mqtt://broker.test".into(),
            mqtt: crate::model::Mqtt { v5, ..mqtt.clone() },
            ..Default::default()
        };
        let python = generate("Python (paho-mqtt)", req(true)).unwrap();
        assert!(
            python.contains(", properties=publish_properties)"),
            "{python}"
        );
        assert!(python.contains(", properties=will_properties)"), "{python}");
        // A 3.1.1 broker would refuse a packet carrying MQTT 5 properties.
        for target in targets("MQTT") {
            let code = generate(target, req(false)).unwrap();
            for v5_only in ["trace", "text/csv", "replies/7", "text/x-gone"] {
                assert!(!code.contains(v5_only), "{target} has {v5_only}:\n{code}");
            }
        }
        // The .proto says Talk streams both ways: every message is written, every answer read.
        let req = || Request {
            method: "GRPC".into(),
            url: "http://h.test:50051".into(),
            proto: proto.to_string_lossy().into_owned(),
            rpc: "chat.v1.Chat/Talk".into(),
            body: Body::Json {
                text: r#"[{"text": "a"}, {"text": "b"}]"#.into(),
            },
            ..Default::default()
        };
        let node = generate("Node.js (@grpc/grpc-js)", req()).unwrap();
        assert!(
            node.contains("call.write({\"text\":\"a\"});\ncall.write({\"text\":\"b\"});"),
            "{node}"
        );
        let python = generate("Python (grpcio)", req()).unwrap();
        assert!(
            python.contains("protos.Note()")
                && python.contains("for response in stub.Talk(iter(requests)")
                && python.contains(r#"protos_and_services("chat.proto")"#),
            "{python}"
        );
        let grpcurl = generate("grpcurl", req()).unwrap();
        assert!(grpcurl.contains("-plaintext"), "{grpcurl}");
        assert!(
            !grpcurl.contains("-authority"),
            "no TLS, no name: {grpcurl}"
        );
        // The request's import paths and TLS server name, as grpcurl's own flags.
        let mut tls = req();
        tls.url = "https://10.0.0.5:443".into();
        tls.settings.tls_server_name = "chat.internal".into();
        tls.settings.import_paths = vec!["../shared".into()];
        let grpcurl = generate("grpcurl", tls).unwrap();
        assert!(
            grpcurl.contains(" -import-path '../shared'")
                && grpcurl.contains("-authority 'chat.internal'")
                && !grpcurl.contains("-plaintext"),
            "{grpcurl}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A server mounted elsewhere needs the path in the snippet too, and a 2.x server a
    /// client of its era: the snippet names that major and leaves out what it lacks (the
    /// auth payload, catch-all listeners, socket.io-client's named `io` export).
    #[test]
    fn socketio_snippets_carry_the_path_and_the_client_a_v2_server_needs() {
        let req = |path: &str, v2| Request {
            method: "SOCKETIO".into(),
            url: "http://h.test:3000/admin".into(),
            body: Body::Json {
                text: r#"{"room": "a"}"#.into(),
            },
            socketio: crate::model::SocketIo {
                path: path.into(),
                v2,
            },
            ..Default::default()
        };
        let both = |req: Request| {
            let node = generate("Node.js (socket.io-client)", req.clone()).unwrap();
            (node, generate("Python (python-socketio)", req).unwrap())
        };
        let (node, python) = both(req("", false));
        assert!(!node.contains("path:"), "{node}");
        assert!(!python.contains("socketio_path"), "{python}");

        let (node, python) = both(req("rt", false));
        assert!(node.contains(r#"path: "/rt/","#), "{node}");
        assert!(node.contains("auth:") && node.contains("onAny"), "{node}");
        assert!(python.contains(r#"socketio_path="rt","#), "{python}");
        assert!(python.contains("auth=") && python.contains(r#"@sio.on("*""#));

        let (node, python) = both(req("rt", true));
        assert!(node.contains(r#"path: "/rt/","#), "{node}");
        assert!(node.contains("socket.io-client@2") && node.contains("import io from"));
        assert!(!node.contains("auth") && !node.contains("onAny"), "{node}");
        assert!(python.contains(r#"socketio_path="rt","#), "{python}");
        assert!(python.contains("python-socketio[client]<5"), "{python}");
        assert!(
            !python.contains("auth") && !python.contains(r#""*""#),
            "{python}"
        );
    }

    /// The picker keeps the language last picked as requests of other kinds are opened.
    #[test]
    fn the_last_picked_language_carries_across_kinds() {
        assert_eq!(pick("WS", "Python (requests)"), "Python (websockets)");
        assert_eq!(pick("GET", "Python (websockets)"), "Python (requests)");
        assert_eq!(pick("SSE", "cURL"), "cURL");
        assert_eq!(pick("MQTT", "cURL"), "mosquitto");
        assert_eq!(pick("POST", "Go (gorilla/websocket)"), "Go (net/http)");
        assert_eq!(pick("GET", ""), "cURL");
        assert!(
            generate(
                "cURL",
                Request {
                    method: "WS".into(),
                    url: "ws://h/".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
