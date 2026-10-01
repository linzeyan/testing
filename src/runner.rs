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
}

impl Outcome {
    pub fn failed(error: String) -> Self {
        Self {
            response: Err(error),
            tests: Vec::new(),
            logs: Vec::new(),
            env: Changes::new(),
            globals: Changes::new(),
        }
    }
}

/// Must run on a multi-threaded tokio runtime: scripts are CPU-bound and use `block_in_place`.
pub async fn run(client: net::Clients, info: &Info, mut req: Request, mut vars: Vars) -> Outcome {
    let mut out = Outcome {
        response: Err(String::new()),
        tests: Vec::new(),
        logs: Vec::new(),
        env: Changes::new(),
        globals: Changes::new(),
    };
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

    for (folder, code) in scripts(&req.inherited.pre_request, &req.pre_request) {
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

    // Precedence like Postman: request-local > data row > environment > folder > globals.
    let mut merged = vars.globals.clone();
    merged.extend(collection.clone());
    merged.extend(vars.env.clone());
    merged.extend(vars.data.clone());
    merged.extend(locals.clone());
    let (wire, _) = req.resolved(&merged);
    let response = send(&client, wire).await;

    if let Ok(resp) = &response {
        for (folder, code) in scripts(&req.inherited.tests, &req.tests) {
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

/// Sends an already-resolved request with the client its protocol needs.
pub async fn send(client: &net::Clients, mut req: Request) -> Result<http::Response, String> {
    if req.method.eq_ignore_ascii_case("GRPC") {
        // The gRPC client speaks only HTTP/2; the token endpoint gets the regular one.
        http::with_token(&client.http, &mut req, false).await?;
        grpc::call(client.grpc.clone(), req).await
    } else {
        http::execute(client.http.clone(), req).await
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
        for (name, req) in &plan.requests {
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
            on_item(RunItem {
                iteration,
                name: name.clone(),
                method: req.method.clone(),
                status: out.response.map(|r| (r.status, r.elapsed.as_millis())),
                tests: out.tests,
            });
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
    out.env.extend(r.env);
    out.globals.extend(r.globals);
    // Tests from the pre-request script are unusual but legal; keep them in order.
    out.tests.extend(r.tests);
    out.logs.extend(r.logs);
    r.request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{Network, ProxyMode, build_client};

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
}
