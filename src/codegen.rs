//! Code snippets for a request, in the tools and languages people paste into, like
//! Postman's code panel. Built from the resolved request (variables filled in) through
//! `http::build`, so every snippet carries the headers, auth and body that Send would.

use std::fmt::Write as _;

use crate::http::{OFFLINE, build, error_chain};
use crate::model::{Auth, Body, HttpVersion, Request, Settings};

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

/// `req` must be resolved (`Request::resolved`).
pub fn generate(target: &str, req: Request) -> Result<String, String> {
    let (_, generator) = TARGETS
        .iter()
        .find(|(name, _)| *name == target)
        .ok_or_else(|| format!("no code generator named {target}"))?;
    if matches!(req.method.as_str(), "WS" | "GRPC" | "MQTT") {
        return Err("Code snippets are for HTTP requests, not WebSocket, gRPC or MQTT".into());
    }
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
    parts: Vec<(String, Part)>,
    /// Answered by the tool itself where it can, as curl --digest does.
    digest: Option<(String, String)>,
    settings: Settings,
}

enum Part {
    Text(String),
    File(String),
}

impl Wire {
    fn new(mut req: Request) -> Result<Self, String> {
        // ponytail: no generator writes a file body yet; add per language when asked for.
        if matches!(req.body, Body::File { .. }) {
            return Err("Code snippets don't cover a binary (file) body yet".into());
        }
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
        // OAuth 2.0 needs a token fetched by Send; Digest needs the server's challenge.
        let mut digest = None;
        match std::mem::take(&mut req.auth) {
            Auth::Digest { username, password } => digest = Some((username, password)),
            Auth::OAuth2(o) => {
                let token = crate::auth::cached_token(&o)
                    .unwrap_or_else(|| "<press Send once to fetch a token>".into());
                req.auth = Auth::Bearer { token };
            }
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
        let body = wire
            .body()
            .and_then(|b| b.as_bytes())
            .map(|b| String::from_utf8_lossy(b).into_owned());
        Ok(Self {
            method: wire.method().as_str().to_owned(),
            url: wire.url().clone(),
            headers,
            body,
            parts,
            digest,
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
    // curl stays on a 3xx unless told; Send follows by default.
    if w.follows() {
        out.push_str(" --location");
        if w.settings.max_redirects != Settings::default().max_redirects {
            let _ = write!(out, " --max-redirs {}", w.settings.max_redirects);
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
    match (w.settings.http_version, w.https()) {
        (HttpVersion::Auto, _) => {}
        (HttpVersion::Http1, _) => out.push_str(" --http1.1"),
        (HttpVersion::Http2, true) => out.push_str(" --http2"),
        (HttpVersion::Http2, false) => out.push_str(" --http2-prior-knowledge"),
    }
    if let Some(s) = w.timeout_secs() {
        let _ = write!(out, " --max-time {s}");
    }
    if let Some((user, pass)) = &w.digest {
        let _ = write!(out, " --digest -u {}", sh(&format!("{user}:{pass}")));
    }
    let _ = write!(out, " {}", sh(w.url.as_str()));
    for (k, v) in &w.headers {
        let _ = write!(out, " \\\n  -H {}", sh(&format!("{k}: {v}")));
    }
    if let Some(body) = &w.body {
        let _ = write!(out, " \\\n  --data-raw {}", sh(body));
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
    if !w.parts.is_empty() {
        out.push_str("# wget can't send multipart/form-data; use the cURL snippet.\n");
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
    if let Some((user, pass)) = &w.digest {
        let _ = write!(out, " --user {} --password {}", sh(user), sh(pass));
    }
    for (k, v) in &w.headers {
        let _ = write!(out, " \\\n  --header {}", sh(&format!("{k}: {v}")));
    }
    if let Some(body) = &w.body {
        let _ = write!(out, " \\\n  --body-data {}", sh(body));
    }
    let _ = write!(out, " \\\n  {}", sh(w.url.as_str()));
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
    if let Some((user, pass)) = &w.digest {
        let _ = write!(
            out,
            " --auth-type digest --auth {}",
            sh(&format!("{user}:{pass}"))
        );
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
    if !w.parts.is_empty() {
        out.push_str("    Form = @{\n");
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
    if let Some((user, pass)) = &w.digest {
        let _ = write!(
            out,
            "$password = ConvertTo-SecureString {} -AsPlainText -Force\n\
             $params.Credential = New-Object PSCredential({}, $password)\n",
            ps_str(pass),
            ps_str(user)
        );
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
    if w.digest.is_some() {
        out.push_str("from requests.auth import HTTPDigestAuth\n");
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
    if !w.parts.is_empty() {
        args.push("files=files".into());
    }
    if let Some((user, pass)) = &w.digest {
        args.push(format!("auth=HTTPDigestAuth({}, {})", dq(user), dq(pass)));
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
    if w.parts.iter().any(|(_, p)| matches!(p, Part::File(_))) {
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
    if w.digest.is_some() {
        out.push_str("// Digest auth isn't built into fetch; answer the 401 challenge yourself.\n");
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
    if !w.parts.is_empty() {
        out.push_str("    data: form,\n");
    }
    if w.digest.is_some() {
        out.push_str(
            "    // Digest auth isn't built into axios; answer the 401 challenge yourself.\n",
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
    if w.digest.is_some() {
        out.push_str(
            "\t// Digest auth isn't built into net/http; answer the 401 challenge yourself.\n",
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
    if w.settings.timeout_ms > 0 {
        out.push_str("import java.time.Duration;\n");
    }
    out.push_str(
        "\npublic class Main {\n    public static void main(String[] args) throws Exception {\n",
    );
    if !w.parts.is_empty() {
        out.push_str(
            "        // HttpClient has no multipart builder; see the Kotlin (OkHttp) snippet.\n",
        );
    }
    if w.insecure() {
        out.push_str("        // Skipping the certificate check needs a custom SSLContext.\n");
    }
    if w.digest.is_some() {
        out.push_str("        // Digest auth isn't built into HttpClient; answer the 401 challenge yourself.\n");
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
    let publisher = match &w.body {
        Some(body) => format!("HttpRequest.BodyPublishers.ofString({})", dq(body)),
        None => "HttpRequest.BodyPublishers.noBody()".into(),
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
    if let Some((user, pass)) = &w.digest {
        let _ = writeln!(
            out,
            "handler.Credentials = new NetworkCredential({}, {});",
            dq(user),
            dq(pass)
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
    if let Some(body) = &w.body {
        let _ = writeln!(out, "request.Content = new StringContent({});", dq(body));
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
    if let Some((user, pass)) = &w.digest {
        opt(&mut out, "CURLOPT_HTTPAUTH", "CURLAUTH_DIGEST".into());
        opt(
            &mut out,
            "CURLOPT_USERPWD",
            php_str(&format!("{user}:{pass}")),
        );
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
    let has_body = w.body.is_some() || !w.parts.is_empty();
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
    if w.digest.is_some() {
        out.push_str(
            "# Digest auth: Net::HTTP needs the net-http-digest_auth gem for the 401 challenge.\n",
        );
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
    if !w.parts.is_empty() {
        out.push_str("\n        .multipart(form)");
    }
    if w.digest.is_some() {
        out.push_str(
            "\n        // Digest auth isn't built into reqwest; answer the 401 challenge yourself.",
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
    if w.digest.is_some() {
        out.push_str("// Digest auth: answer the challenge in a URLSessionTaskDelegate.\n");
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
        && w.parts.is_empty()
        && matches!(w.method.as_str(), "POST" | "PUT" | "PATCH");
    if w.body.is_some() || empty {
        imports.push("okhttp3.RequestBody.Companion.toRequestBody");
        if ct.is_some() {
            imports.push("okhttp3.MediaType.Companion.toMediaType");
        }
    }
    if !w.parts.is_empty() {
        imports.push("okhttp3.MultipartBody");
    }
    if w.parts.iter().any(|(_, p)| matches!(p, Part::File(_))) {
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
    if w.digest.is_some() {
        out.push_str("// Digest auth: add an okhttp-digest Authenticator.\n");
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
    let body = if let Some(body) = &w.body {
        let media = match ct {
            Some(ct) => format!("{}.toMediaType()", kotlin_str(ct)),
            None => "null".into(),
        };
        let _ = writeln!(
            out,
            "    val body = {}.toRequestBody({media})",
            kotlin_str(body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KeyValue;

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
        ]
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
        let ws = Request {
            method: "WS".into(),
            url: "ws://h/".into(),
            ..Default::default()
        };
        assert!(generate("cURL", ws).is_err());
    }
}
