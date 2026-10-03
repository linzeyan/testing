//! One request end to end: pre-request script → variable resolution → HTTP → tests,
//! and the collection runner that loops it over requests and data rows.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::model::{KeyValue, Request};
use crate::script::{self, Changes, Output, ScriptResponse, TestResult, WireRequest};
use crate::{grpc, http, net};

#[derive(Clone, Default)]
pub struct Vars {
    pub env: HashMap<String, String>,
    pub globals: HashMap<String, String>,
    /// Current data-file row; empty outside the collection runner.
    pub data: HashMap<String, String>,
}

/// Exposed to scripts as `pm.info`.
pub struct Info {
    pub name: String,
    pub iteration: usize,
    pub count: usize,
}

impl Info {
    pub fn single(name: String) -> Self {
        Self {
            name,
            iteration: 0,
            count: 1,
        }
    }
}

pub struct Outcome {
    pub response: Result<http::Response, String>,
    pub tests: Vec<TestResult>,
    pub logs: Vec<String>,
    /// Variable writes made by the scripts, to be applied (and persisted) by the caller.
    pub env: Changes,
    pub globals: Changes,
    /// A script's `setNextRequest`: Some(None) stops the run.
    pub next: Option<Option<String>>,
    /// A pre-request script's `skipRequest()`: nothing was sent.
    pub skipped: bool,
}

impl Outcome {
    pub fn failed(error: String) -> Self {
        Self {
            response: Err(error),
            tests: Vec::new(),
            logs: Vec::new(),
            env: Changes::new(),
            globals: Changes::new(),
            next: None,
            skipped: false,
        }
    }
}

/// Must run on a multi-threaded tokio runtime: scripts are CPU-bound and use `block_in_place`.
pub async fn run(client: net::Clients, info: &Info, mut req: Request, mut vars: Vars) -> Outcome {
    let mut out = Outcome::failed(String::new());
    let mut locals = HashMap::new();
    let collection = req.inherited.vars.clone();
    // Postman's order: outermost folder first, the request's own script last.
    let scripts = |inherited: &[(String, String)], own: &str| -> Vec<(String, String)> {
        let own = (String::new(), own.to_owned());
        let all = inherited.iter().cloned().chain(std::iter::once(own));
        all.filter(|(_, s)| !s.trim().is_empty()).collect()
    };
    let whose = |folder: &str| match folder {
        "" => String::new(),
        f => format!(" of folder \"{f}\""),
    };

    let jar = req.settings.cookies.then_some(&client.jar);
    for (folder, code) in scripts(&req.inherited.pre_request, &req.pre_request) {
        // What the request would go to now: scripts before this one may have changed it.
        let cookie_url = http::wire_url(&req.resolved(&merge(&vars, &collection, &locals)).0.url);
        let wire = WireRequest {
            method: req.method.clone(),
            url: req.url.clone(),
            headers: req
                .headers
                .iter()
                .filter(|h| h.enabled && !h.key.is_empty())
                .map(|h| (h.key.clone(), h.value.clone()))
                .collect(),
        };
        let input = script::Input {
            name: &info.name,
            iteration: info.iteration,
            iteration_count: info.count,
            data: &vars.data,
            env: &vars.env,
            collection: &collection,
            globals: &vars.globals,
            locals: &locals,
            request: &wire,
            response: None,
            cookie_url: &cookie_url,
            jar,
            client: Some(&client),
        };
        let result = tokio::task::block_in_place(|| script::run(&code, &input));
        let error = result.error.clone();
        let edited = absorb(&mut out, &mut vars, &mut locals, result);
        if let Some(e) = error {
            let whose = whose(&folder);
            out.response = Err(format!(
                "Pre-request script{whose} failed, request not sent:\n{e}"
            ));
            return out;
        }
        if out.skipped {
            let whose = whose(&folder);
            out.response = Err(format!(
                "Skipped by pm.execution.skipRequest() in the pre-request script{whose}"
            ));
            return out;
        }
        if let Some(w) = edited {
            req.method = w.method;
            req.url = w.url;
            req.headers = w
                .headers
                .into_iter()
                .map(|(k, v)| KeyValue::new(k, v))
                .collect();
        }
    }

    let (wire, _) = req.resolved(&merge(&vars, &collection, &locals));
    let cookie_url = http::wire_url(&wire.url);
    let response = send(&client, wire).await;

    if let Ok(resp) = &response {
        let mut tests = scripts(&req.inherited.tests, &req.tests);
        let asserts: Vec<_> = (req.asserts.iter())
            .filter(|a| a.enabled && !a.key.trim().is_empty())
            .map(|a| (a.key.trim(), a.value.trim()))
            .collect();
        if !asserts.is_empty() {
            // After the scripts, so a row can check what they computed into a variable.
            tests.push((
                String::new(),
                format!("__asserts({});", serde_json::json!(asserts)),
            ));
        }
        for (folder, code) in tests {
            let wire = WireRequest {
                method: req.method.clone(),
                url: req.url.clone(),
                headers: Vec::new(),
            };
            let sr = ScriptResponse {
                code: resp.status,
                status: &resp.reason,
                time: resp.elapsed.as_millis(),
                headers: &resp.headers,
                body: &resp.body,
            };
            let input = script::Input {
                name: &info.name,
                iteration: info.iteration,
                iteration_count: info.count,
                data: &vars.data,
                env: &vars.env,
                collection: &collection,
                globals: &vars.globals,
                locals: &locals,
                request: &wire,
                response: Some(sr),
                cookie_url: &cookie_url,
                jar,
                client: Some(&client),
            };
            let result = tokio::task::block_in_place(|| script::run(&code, &input));
            let error = result.error.clone();
            absorb(&mut out, &mut vars, &mut locals, result);
            if let Some(e) = error {
                out.tests.push(TestResult {
                    name: format!("Script error{}", whose(&folder)),
                    passed: false,
                    error: Some(e),
                });
            }
        }
    }
    out.response = response;
    out
}

/// Precedence like Postman: request-local > data row > environment > folder > globals.
fn merge(
    vars: &Vars,
    collection: &HashMap<String, String>,
    locals: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = vars.globals.clone();
    merged.extend(collection.clone());
    merged.extend(vars.env.clone());
    merged.extend(vars.data.clone());
    merged.extend(locals.clone());
    merged
}

/// Sends an already-resolved request with the client its protocol needs.
pub async fn send(client: &net::Clients, mut req: Request) -> Result<http::Response, String> {
    if req.method.eq_ignore_ascii_case("GRPC") {
        // The gRPC client speaks only HTTP/2; the token endpoint gets the regular one.
        http::with_token(&client.http, &mut req, false).await?;
        grpc::call(client.grpc.clone(), req).await
    } else {
        http::execute(client.for_settings(&req.settings)?, req).await
    }
}

/// One row of the collection runner's results. Bodies are deliberately not kept:
/// a long data-driven run must not accumulate megabytes per request.
pub struct RunItem {
    pub iteration: usize,
    pub name: String,
    pub method: String,
    /// (status code, elapsed ms) or the request error.
    pub status: Result<(u16, u128), String>,
    pub tests: Vec<TestResult>,
}

impl RunItem {
    pub fn failed(&self) -> bool {
        self.status.is_err() || self.tests.iter().any(|t| !t.passed)
    }
}

pub struct RunPlan {
    pub requests: Vec<(String, Request)>,
    /// Data rows; when empty, `iterations` plain iterations run instead.
    pub data: Vec<HashMap<String, String>>,
    pub iterations: usize,
    pub delay: Duration,
}

/// Runs every request for every iteration, chaining variable writes between requests.
/// Returns the accumulated env/global changes for the caller to persist.
pub async fn run_collection(
    client: net::Clients,
    plan: RunPlan,
    mut vars: Vars,
    mut on_item: impl FnMut(RunItem),
) -> (Changes, Changes) {
    let (mut env, mut globals) = (Changes::new(), Changes::new());
    let count = if plan.data.is_empty() {
        plan.iterations.max(1)
    } else {
        plan.data.len()
    };
    let mut first = true;
    for iteration in 0..count {
        vars.data = plan.data.get(iteration).cloned().unwrap_or_default();
        let mut at = 0;
        while let Some((name, req)) = plan.requests.get(at) {
            at += 1;
            if !first && !plan.delay.is_zero() {
                tokio::time::sleep(plan.delay).await;
            }
            first = false;
            let info = Info {
                name: name.clone(),
                iteration,
                count,
            };
            let out = run(client.clone(), &info, req.clone(), vars.clone()).await;
            for (k, v) in &out.env {
                apply_one(&mut vars.env, k, v);
            }
            for (k, v) in &out.globals {
                apply_one(&mut vars.globals, k, v);
            }
            env.extend(out.env);
            globals.extend(out.globals);
            let mut tests = out.tests;
            // Postman matches the request's name; a folder path works too, for names that
            // repeat across folders.
            match out.next {
                None => {}
                Some(None) => at = plan.requests.len(),
                Some(Some(next)) => {
                    let found = plan.requests.iter().position(|(key, _)| {
                        key == &next || key.rsplit('/').next() == Some(next.as_str())
                    });
                    at = found.unwrap_or_else(|| {
                        tests.push(TestResult {
                            name: "setNextRequest".into(),
                            passed: false,
                            error: Some(format!(
                                "no request named \"{next}\" in this run; it stops here"
                            )),
                        });
                        plan.requests.len()
                    });
                }
            }
            // Postman leaves skipped requests out of the results too.
            if !out.skipped || tests.iter().any(|t| !t.passed) {
                on_item(RunItem {
                    iteration,
                    name: name.clone(),
                    method: req.method.clone(),
                    status: out.response.map(|r| (r.status, r.elapsed.as_millis())),
                    tests,
                });
            }
        }
    }
    (env, globals)
}

fn apply_one(map: &mut HashMap<String, String>, k: &str, v: &Option<String>) {
    match v {
        Some(v) => map.insert(k.to_owned(), v.clone()),
        None => map.remove(k),
    };
}

/// Reads a runner data file: CSV with a header row, or a JSON array of objects.
/// Non-string JSON values are passed on as their JSON text, like Postman.
// ponytail: every row is kept as a map, about 16 times the file (100 000 rows: 65 MiB from
// CSV, 105 MiB from JSON). Read rows as the run reaches them if files with millions come.
pub fn load_data(path: &Path) -> Result<Vec<HashMap<String, String>>, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    let is_json = path
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("json"));
    if is_json {
        let rows: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&text)
            .map_err(|e| format!("{shown}: expected an array of objects: {e}"))?;
        return Ok(rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            if let serde_json::Value::String(s) = v {
                                s
                            } else {
                                v.to_string()
                            },
                        )
                    })
                    .collect()
            })
            .collect());
    }
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::Headers)
        .from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| format!("{shown}: {e}"))?
        .clone();
    reader
        .records()
        .map(|rec| {
            let rec = rec.map_err(|e| format!("{shown}: {e}"))?;
            Ok(headers
                .iter()
                .zip(rec.iter())
                .map(|(h, v)| (h.to_owned(), v.to_owned()))
                .collect())
        })
        .collect()
}

/// Folds one script's output into the running state; returns its edited request.
fn absorb(
    out: &mut Outcome,
    vars: &mut Vars,
    locals: &mut HashMap<String, String>,
    r: Output,
) -> Option<WireRequest> {
    fn apply(map: &mut HashMap<String, String>, changes: &Changes) {
        for (k, v) in changes {
            match v {
                Some(v) => map.insert(k.clone(), v.clone()),
                None => map.remove(k),
            };
        }
    }
    apply(&mut vars.env, &r.env);
    apply(&mut vars.globals, &r.globals);
    apply(locals, &r.locals);
    if let Some(next) = r.next {
        out.next = Some(next.name);
    }
    out.skipped |= r.skip;
    out.env.extend(r.env);
    out.globals.extend(r.globals);
    // Tests from the pre-request script are unusual but legal; keep them in order.
    out.tests.extend(r.tests.into_iter().map(|mut t| {
        if let Some(e) = &mut t.error {
            clip(e, MAX_LOG_LINE);
        }
        t
    }));
    for mut line in r.logs {
        match out.logs.len().cmp(&MAX_LOGS) {
            std::cmp::Ordering::Less => {
                clip(&mut line, MAX_LOG_LINE);
                out.logs.push(line);
            }
            std::cmp::Ordering::Equal => out.logs.push("… later lines not kept".into()),
            std::cmp::Ordering::Greater => break,
        }
    }
    r.request
}

/// What one console line or test failure keeps: `console.log(pm.response.text())` is a
/// common habit, chai quotes whole values in its failures, and the UI lays out all of it.
const MAX_LOG_LINE: usize = 4096;
const MAX_LOGS: usize = 1000;

/// Cuts `text` to at most `max` bytes (on a character boundary) and says how long it was.
pub fn clip(text: &mut String, max: usize) {
    if text.len() > max {
        let all = text.len();
        text.truncate(text.floor_char_boundary(max));
        text.push_str(&format!("… ({all} bytes in all)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{Network, ProxyMode, build_client};

    /// Scripts see the jar Send uses: a test reads the session cookie a login just set,
    /// and a cookie a pre-request script puts in the jar goes out with that same request.
    #[test]
    fn scripts_read_and_write_the_cookie_jar_send_uses() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(build_client(Network {
                proxy: ProxyMode::None,
                ..Default::default()
            }))
            .unwrap();
        let addr = crate::http::tests::serve(|raw| match raw.split(' ').nth(1).unwrap_or("") {
            "/login" => (
                "200 OK\r\nset-cookie: sid=s3cr3t; Path=/".into(),
                String::new(),
            ),
            _ => ("200 OK".into(), raw.to_lowercase()),
        });
        let run = |path: &str, pre: &str, tests: &str| {
            let req = Request {
                url: format!("http://{{{{host}}}}{path}"),
                pre_request: pre.into(),
                tests: tests.into(),
                ..Default::default()
            };
            let vars = Vars {
                env: HashMap::from([("host".to_owned(), addr.to_string())]),
                ..Default::default()
            };
            rt.block_on(super::run(
                client.clone(),
                &Info::single("t".into()),
                req,
                vars,
            ))
        };
        let out = run(
            "/login",
            "",
            r#"pm.test("sid", function () {
                 pm.expect(pm.cookies.get("sid")).to.equal("s3cr3t");
                 pm.expect(pm.cookies.has("nope")).to.equal(false);
               });"#,
        );
        assert!(out.tests.iter().all(|t| t.passed), "{:?}", out.tests);
        assert_eq!(out.tests.len(), 1);

        let out = run(
            "/echo",
            r#"var url = "http://" + pm.environment.get("host") + "/";
               pm.cookies.jar().set(url, "extra", "1", function (err, c) {
                 if (err) throw err;
                 console.log(c.name);
               });
               pm.cookies.jar().unset(url, "sid");"#,
            "",
        );
        assert_eq!(out.logs, ["extra"]);
        let body = out.response.unwrap().body;
        assert!(body.contains("cookie: extra=1"), "{body}");
        assert!(
            !body.contains("sid="),
            "unset in the jar before sending: {body}"
        );
    }

    /// The usual reason for `pm.sendRequest`: fetch a token before the request that needs
    /// it, in Postman's callback form and with `await`; both see the answer before the
    /// script ends, or the variable would be set too late.
    #[test]
    fn scripts_send_requests_of_their_own() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt.block_on(build_client(Network::default())).unwrap();
        let addr = crate::http::tests::serve(|raw| match raw.split(' ').nth(1).unwrap_or("") {
            // The token endpoint wants the JSON it was sent, labelled as JSON.
            "/token"
                if raw
                    .to_lowercase()
                    .contains("content-type: application/json")
                    && raw.ends_with(r#"{"id":1}"#)
                    && raw.to_lowercase().contains("x-client: app") =>
            {
                ("200 OK".into(), r#"{"token":"t1"}"#.into())
            }
            _ => ("200 OK".into(), raw.to_lowercase()),
        });
        let req = Request {
            url: format!("http://{addr}/api"),
            headers: vec![KeyValue::new("Authorization", "Bearer {{token}}")],
            pre_request: format!(
                r#"pm.sendRequest({{ url: "http://{addr}/token", method: "POST",
                     header: {{ "X-Client": "app" }},
                     body: {{ mode: "raw", raw: {{ id: 1 }}, options: {{ raw: {{ language: "json" }} }} }}
                   }}, function (err, res) {{
                     if (err) throw err;
                     pm.environment.set("token", res.json().token);
                   }});
                   pm.sendRequest("http://127.0.0.1:1/", function (err) {{ console.log("down", !!err); }});"#
            ),
            tests: format!(
                r#"const res = await pm.sendRequest({{ url: "http://{addr}/echo",
                     header: [{{ key: "X-Seen", value: "yes" }}, {{ key: "X-Off", value: "1", disabled: true }}] }});
                   pm.test("echo", () => {{
                     pm.expect(res.code).to.equal(200);
                     pm.expect(res.text()).to.include("x-seen: yes").and.not.include("x-off");
                   }});"#
            ),
            ..Default::default()
        };
        let out = rt.block_on(super::run(
            client,
            &Info::single("t".into()),
            req,
            Vars::default(),
        ));
        assert_eq!(out.env["token"], Some("t1".into()));
        assert_eq!(out.logs, ["down true"]);
        let body = out.response.unwrap().body;
        assert!(body.contains("authorization: bearer t1"), "{body}");
        assert_eq!(out.tests.len(), 1, "{:?}", out.tests);
        assert!(out.tests[0].passed, "{:?}", out.tests);
    }

    #[test]
    fn request_settings_change_how_it_goes_out() {
        use crate::model::{HttpVersion, Settings};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(build_client(Network {
                proxy: ProxyMode::None,
                ..Default::default()
            }))
            .unwrap();
        let addr = crate::http::tests::serve(|raw| {
            let none = String::new();
            match raw.split(' ').nth(1).unwrap_or("") {
                "/start" => ("302 Found\r\nlocation: /end".into(), none),
                "/loop" => ("302 Found\r\nlocation: /loop".into(), none),
                "/login" => ("200 OK\r\nset-cookie: sid=1; Path=/".into(), none),
                "/slow" => {
                    std::thread::sleep(Duration::from_millis(500));
                    ("200 OK".into(), none)
                }
                _ => ("200 OK".into(), raw.to_lowercase()),
            }
        });
        let h2c = h2c_server();
        let send = |url: String, settings: Settings| {
            let req = Request {
                url,
                settings,
                ..Default::default()
            };
            rt.block_on(super::send(&client, req))
        };
        let at = |path: &str| format!("http://{addr}{path}");
        let d = Settings::default();

        assert_eq!(send(at("/start"), d.clone()).unwrap().status, 200);
        let stay = Settings {
            follow_redirects: false,
            ..d.clone()
        };
        assert_eq!(
            send(at("/start"), stay).unwrap().status,
            302,
            "the 3xx itself"
        );
        let few = Settings {
            max_redirects: 3,
            ..d.clone()
        };
        let err = send(at("/loop"), few).err().expect("a redirect loop fails");
        assert!(err.contains("too many redirects"), "{err}");

        send(at("/login"), d.clone()).unwrap();
        assert!(
            send(at("/echo"), d.clone())
                .unwrap()
                .body
                .contains("cookie: sid=1")
        );
        let jarless = Settings {
            cookies: false,
            ..d.clone()
        };
        assert!(!send(at("/echo"), jarless).unwrap().body.contains("cookie:"));

        let h1 = Settings {
            http_version: HttpVersion::Http1,
            ..d.clone()
        };
        assert_eq!(send(at("/echo"), h1).unwrap().version, "HTTP/1.1");
        let h2 = Settings {
            http_version: HttpVersion::Http2,
            ..d.clone()
        };
        assert_eq!(send(h2c, h2).unwrap().version, "HTTP/2.0");

        // Last: the server sleeps through it before taking another request.
        let quick = Settings {
            timeout_ms: 100,
            ..d.clone()
        };
        let err = send(at("/slow"), quick).err().expect("too slow");
        assert!(err.contains("timed out"), "{err}");
    }

    /// Answers every request with an empty 200 over cleartext HTTP/2.
    fn h2c_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (sock, _) = listener.accept().await.unwrap();
                let mut conn = h2::server::handshake(sock).await.unwrap();
                while let Some(Ok((_, mut respond))) = conn.accept().await {
                    let ok = ::http::Response::builder().body(()).unwrap();
                    respond.send_response(ok, true).unwrap();
                }
            });
        });
        format!("http://{addr}/")
    }

    #[test]
    fn scripts_chain_variables_into_the_request_and_capture_results() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(build_client(Network {
                proxy: ProxyMode::None,
                ..Default::default()
            }))
            .unwrap();
        let addr = crate::http::tests::echo_server();
        let req = Request {
            url: "{{base}}/{{id}}".into(),
            pre_request: r#"
                pm.environment.set("id", 7);
                pm.request.headers.upsert({ key: "X-Env", value: pm.environment.get("stage") });
            "#
            .into(),
            tests: r#"
                pm.test("echoed", function () { pm.expect(pm.response.text()).to.include("GET /users/7 "); });
                pm.globals.set("seen", pm.response.code);
                boom();
            "#
            .into(),
            ..Default::default()
        };
        let vars = Vars {
            env: HashMap::from([("stage".to_owned(), "qa".to_owned())]),
            globals: HashMap::from([("base".to_owned(), addr)]),
            data: HashMap::new(),
        };
        let out = rt.block_on(run(client, &Info::single("t".into()), req, vars));

        let wire = out.response.unwrap().body.to_lowercase();
        // A variable set by the pre-request script resolves in the same request.
        assert!(wire.starts_with("get /users/7 http/1.1"), "{wire}");
        assert!(wire.contains("x-env: qa"), "{wire}");
        assert_eq!(out.env["id"], Some("7".into()));
        assert_eq!(out.globals["seen"], Some("200".into()));
        let names: Vec<_> = out
            .tests
            .iter()
            .map(|t| (t.name.as_str(), t.passed))
            .collect();
        // A crashing test script still reports what passed, then the error last.
        assert_eq!(names, [("echoed", true), ("Script error", false)]);
    }

    #[test]
    fn collection_run_chains_requests_and_iterates_data_rows() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(build_client(Network {
                proxy: ProxyMode::None,
                ..Default::default()
            }))
            .unwrap();
        let base = crate::http::tests::echo_server();
        let login = Request {
            url: format!("{base}/login/{{{{user}}}}"),
            // The echo server returns the request line, e.g. "GET /users/login/ann HTTP/1.1".
            tests: r#"pm.environment.set("session", pm.response.text().split(" ")[1].split("/").pop());"#.into(),
            ..Default::default()
        };
        let profile = Request {
            url: format!("{base}/me"),
            headers: vec![KeyValue::new("X-Session", "{{session}}")],
            tests: r#"
                pm.test("session sent", function () {
                    pm.expect(pm.response.text().toLowerCase()).to.include("x-session: " + pm.iterationData.get("user"));
                });
                pm.test("iteration", function () { pm.expect(pm.info.iteration).to.be.below(pm.info.iterationCount); });
            "#
            .into(),
            ..Default::default()
        };
        let plan = RunPlan {
            requests: vec![("Login".into(), login), ("Profile".into(), profile)],
            data: vec![
                HashMap::from([("user".to_owned(), "ann".to_owned())]),
                HashMap::from([("user".to_owned(), "bob".to_owned())]),
            ],
            iterations: 99, // ignored: data rows decide the count
            delay: Duration::ZERO,
        };
        let mut items = Vec::new();
        let (env, _) = rt.block_on(run_collection(client, plan, Vars::default(), |item| {
            items.push(item)
        }));

        let order: Vec<_> = items
            .iter()
            .map(|i| (i.iteration, i.name.as_str()))
            .collect();
        assert_eq!(
            order,
            [(0, "Login"), (0, "Profile"), (1, "Login"), (1, "Profile")]
        );
        for item in &items {
            assert!(item.status.is_ok(), "{}: {:?}", item.name, item.status);
            assert!(
                item.tests.iter().all(|t| t.passed),
                "{}: {:?}",
                item.name,
                item.tests
            );
        }
        // The last write wins and is handed back for persisting.
        assert_eq!(env["session"], Some("bob".into()));
    }

    /// The request's Assert rows run after its tests script, with the variables it set.
    #[test]
    fn assert_rows_become_test_results() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt.block_on(build_client(Network::default())).unwrap();
        let base = crate::http::tests::echo_server();
        let mut off = KeyValue::new("res.status", "eq 404");
        off.enabled = false;
        let req = Request {
            url: format!("{base}/x"),
            tests: r#"pm.environment.set("code", pm.response.code);"#.into(),
            asserts: vec![KeyValue::new("res.status", "eq {{code}}"), off],
            ..Default::default()
        };
        let out = rt.block_on(super::run(
            client,
            &Info::single("t".into()),
            req,
            Vars::default(),
        ));
        let names: Vec<_> = out
            .tests
            .iter()
            .map(|t| (t.name.as_str(), t.passed))
            .collect();
        assert_eq!(names, [("res.status eq {{code}}", true)]);
    }

    /// Postman's flow control: jump over a request, poll one until it's done, stop the
    /// run, and leave out a request a pre-request script skips (it never goes out).
    #[test]
    fn scripts_choose_the_next_request_or_skip_their_own() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt.block_on(build_client(Network::default())).unwrap();
        let base = crate::http::tests::echo_server();
        let req = |pre: &str, tests: &str| Request {
            url: format!("{base}/x"),
            pre_request: pre.into(),
            tests: tests.into(),
            ..Default::default()
        };
        let poll = r#"var n = Number(pm.environment.get("n") || 0) + 1;
            pm.environment.set("n", n);
            pm.execution.setNextRequest(n < 3 ? "poll" : null);"#;
        let plan = RunPlan {
            requests: vec![
                (
                    "skipped".into(),
                    req(
                        "pm.execution.skipRequest();",
                        "pm.environment.set('sent', 1);",
                    ),
                ),
                (
                    "start".into(),
                    req("", r#"postman.setNextRequest("poll");"#),
                ),
                ("jumped over".into(), req("", "")),
                ("jobs/poll".into(), req("", poll)),
                ("after the stop".into(), req("", "")),
            ],
            data: vec![],
            iterations: 1,
            delay: Duration::ZERO,
        };
        let mut items = Vec::new();
        let (env, _) = rt.block_on(run_collection(
            client.clone(),
            plan,
            Vars::default(),
            |item| items.push(item),
        ));
        let order: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(order, ["start", "jobs/poll", "jobs/poll", "jobs/poll"]);
        assert!(items.iter().all(|i| !i.failed()));
        assert_eq!(env.get("sent"), None, "a skipped request's tests don't run");

        let plan = RunPlan {
            requests: vec![
                (
                    "a".into(),
                    req("", r#"pm.execution.setNextRequest("nope");"#),
                ),
                ("b".into(), req("", "")),
            ],
            data: vec![],
            iterations: 1,
            delay: Duration::ZERO,
        };
        let mut items = Vec::new();
        rt.block_on(run_collection(client, plan, Vars::default(), |item| {
            items.push(item)
        }));
        assert_eq!(items.len(), 1, "an unknown name stops the run");
        assert!(
            items[0].tests[0]
                .error
                .as_deref()
                .unwrap()
                .contains("\"nope\"")
        );
    }

    #[test]
    fn folder_scripts_wrap_the_request_and_folder_settings_apply() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(build_client(Network {
                proxy: ProxyMode::None,
                ..Default::default()
            }))
            .unwrap();
        let s = |v: &str| v.to_owned();
        let mut req = Request {
            url: "{{base}}/x".into(),
            pre_request: "console.log('own');".into(),
            tests: r#"pm.test("own", function () {
                pm.expect(pm.collectionVariables.get("stage")).to.equal("folder");
            });"#
                .into(),
            ..Default::default()
        };
        req.inherited = crate::model::Inherited {
            vars: HashMap::from([
                (s("base"), crate::http::tests::echo_server()),
                (s("stage"), s("folder")),
            ]),
            auth: Some((
                s("api"),
                crate::model::Auth::Bearer {
                    token: s("{{token}}"),
                },
            )),
            pre_request: vec![
                (s("api"), s("console.log('outer');")),
                (
                    s("api/admin"),
                    s("console.log('inner'); \
                       pm.request.headers.upsert({ key: 'X-Stage', value: pm.variables.get('stage') });"),
                ),
            ],
            tests: vec![(s("api"), s("pm.collectionVariables.set('x', 1);"))],
        };
        let vars = Vars {
            env: HashMap::from([(s("token"), s("s3cret")), (s("stage"), s("env"))]),
            ..Default::default()
        };
        let out = rt.block_on(run(client, &Info::single("t".into()), req, vars));

        assert_eq!(out.logs, ["outer", "inner", "own"]);
        let wire = out.response.unwrap().body.to_lowercase();
        assert!(wire.starts_with("get /users/x "), "{wire}");
        assert!(wire.contains("authorization: bearer s3cret"), "{wire}");
        assert!(
            wire.contains("x-stage: env"),
            "environment beats folder: {wire}"
        );
        let tests: Vec<_> = out
            .tests
            .iter()
            .map(|t| (t.name.as_str(), t.passed))
            .collect();
        // Folder tests run first, and an error says whose script it was.
        assert_eq!(
            tests,
            [("Script error of folder \"api\"", false), ("own", true)]
        );
        assert!(out.tests[0].error.as_deref().unwrap().contains("read-only"));
    }

    #[test]
    fn data_files_load_from_csv_and_json() {
        let dir = std::env::temp_dir().join(format!("apitool-data-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("users.csv");
        std::fs::write(
            &csv,
            "user, note\nann,\"hello, world\"\nbob,\"multi\nline\"\n",
        )
        .unwrap();
        let rows = load_data(&csv).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["note"], "hello, world");
        assert_eq!(rows[1]["note"], "multi\nline");

        let json = dir.join("users.json");
        std::fs::write(&json, r#"[{"user":"ann","age":30,"tags":["a"]}]"#).unwrap();
        let rows = load_data(&json).unwrap();
        assert_eq!(rows[0]["user"], "ann");
        assert_eq!(rows[0]["age"], "30");
        assert_eq!(rows[0]["tags"], r#"["a"]"#);

        std::fs::write(&json, r#"{"not":"an array"}"#).unwrap();
        assert!(load_data(&json).unwrap_err().contains("array of objects"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failing_pre_request_script_blocks_the_send() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let client = rt
            .block_on(build_client(Network {
                proxy: ProxyMode::None,
                ..Default::default()
            }))
            .unwrap();
        let req = Request {
            url: "http://127.0.0.1:9/".into(),
            pre_request: "throw new Error('nope')".into(),
            ..Default::default()
        };
        let out = rt.block_on(run(client, &Info::single("t".into()), req, Vars::default()));
        let err = out.response.err().unwrap();
        assert!(
            err.contains("Pre-request script failed") && err.contains("nope"),
            "{err}"
        );
    }

    /// `console.log(pm.response.text())` on a big body must not become a giant line in
    /// the console, nor a flood of lines an endless list.
    #[test]
    fn script_logs_and_failures_are_clipped() {
        let mut out = Outcome::failed(String::new());
        let r = Output {
            logs: vec!["é".repeat(10_000); MAX_LOGS + 50],
            tests: vec![TestResult {
                name: "t".into(),
                passed: false,
                error: Some("y".repeat(100_000)),
            }],
            ..Default::default()
        };
        absorb(&mut out, &mut Vars::default(), &mut HashMap::new(), r);
        assert_eq!(out.logs.len(), MAX_LOGS + 1);
        assert!(out.logs[0].len() < MAX_LOG_LINE + 32 && out.logs[0].ends_with("bytes in all)"));
        assert_eq!(out.logs[MAX_LOGS], "… later lines not kept");
        assert!(out.tests[0].error.as_ref().unwrap().len() < MAX_LOG_LINE + 32);
    }
}
