//! One request end to end: pre-request script → variable resolution → HTTP → tests.
//! Shared by the Send button and (later) the collection runner.

use std::collections::HashMap;

use crate::http;
use crate::model::{KeyValue, Request};
use crate::script::{self, Changes, Output, ScriptResponse, TestResult, WireRequest};

#[derive(Clone, Default)]
pub struct Vars {
    pub env: HashMap<String, String>,
    pub globals: HashMap<String, String>,
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
        Self { response: Err(error), tests: Vec::new(), logs: Vec::new(), env: Changes::new(), globals: Changes::new() }
    }
}

/// Must run on a multi-threaded tokio runtime: scripts are CPU-bound and use `block_in_place`.
pub async fn run(client: reqwest::Client, name: String, mut req: Request, mut vars: Vars) -> Outcome {
    let mut out = Outcome {
        response: Err(String::new()),
        tests: Vec::new(),
        logs: Vec::new(),
        env: Changes::new(),
        globals: Changes::new(),
    };
    let mut locals = HashMap::new();

    if !req.pre_request.trim().is_empty() {
        let wire = WireRequest {
            method: req.method.clone(),
            url: req.url.clone(),
            headers: req.headers.iter().filter(|h| h.enabled && !h.key.is_empty()).map(|h| (h.key.clone(), h.value.clone())).collect(),
        };
        let input = script::Input { name: &name, env: &vars.env, globals: &vars.globals, locals: &locals, request: &wire, response: None };
        let result = tokio::task::block_in_place(|| script::run(&req.pre_request, &input));
        let error = result.error.clone();
        let edited = absorb(&mut out, &mut vars, &mut locals, result);
        if let Some(e) = error {
            out.response = Err(format!("Pre-request script failed, request not sent:\n{e}"));
            return out;
        }
        if let Some(w) = edited {
            req.method = w.method;
            req.url = w.url;
            req.headers = w.headers.into_iter().map(|(k, v)| KeyValue::new(k, v)).collect();
        }
    }

    // Precedence like Postman: request-local > environment > globals.
    let mut merged = vars.globals.clone();
    merged.extend(vars.env.clone());
    merged.extend(locals.clone());
    let (wire, _) = req.resolved(&merged);
    let response = http::execute(client, wire).await;

    if let Ok(resp) = &response
        && !req.tests.trim().is_empty()
    {
        let wire = WireRequest { method: req.method.clone(), url: req.url.clone(), headers: Vec::new() };
        let sr = ScriptResponse {
            code: resp.status,
            status: &resp.reason,
            time: resp.elapsed.as_millis(),
            headers: &resp.headers,
            body: &resp.body,
        };
        let input = script::Input { name: &name, env: &vars.env, globals: &vars.globals, locals: &locals, request: &wire, response: Some(sr) };
        let result = tokio::task::block_in_place(|| script::run(&req.tests, &input));
        let error = result.error.clone();
        absorb(&mut out, &mut vars, &mut locals, result);
        if let Some(e) = error {
            out.tests.push(TestResult { name: "Script error".into(), passed: false, error: Some(e) });
        }
    }
    out.response = response;
    out
}

/// Folds one script's output into the running state; returns its edited request.
fn absorb(out: &mut Outcome, vars: &mut Vars, locals: &mut HashMap<String, String>, r: Output) -> Option<WireRequest> {
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
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let client = rt.block_on(build_client(Network { proxy: ProxyMode::None, ..Default::default() })).unwrap();
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
        };
        let out = rt.block_on(run(client, "t".into(), req, vars));

        let wire = out.response.unwrap().body.to_lowercase();
        // A variable set by the pre-request script resolves in the same request.
        assert!(wire.starts_with("get /users/7 http/1.1"), "{wire}");
        assert!(wire.contains("x-env: qa"), "{wire}");
        assert_eq!(out.env["id"], Some("7".into()));
        assert_eq!(out.globals["seen"], Some("200".into()));
        let names: Vec<_> = out.tests.iter().map(|t| (t.name.as_str(), t.passed)).collect();
        // A crashing test script still reports what passed, then the error last.
        assert_eq!(names, [("echoed", true), ("Script error", false)]);
    }

    #[test]
    fn failing_pre_request_script_blocks_the_send() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let client = rt.block_on(build_client(Network { proxy: ProxyMode::None, ..Default::default() })).unwrap();
        let req = Request { url: "http://127.0.0.1:9/".into(), pre_request: "throw new Error('nope')".into(), ..Default::default() };
        let out = rt.block_on(run(client, "t".into(), req, Vars::default()));
        let err = out.response.err().unwrap();
        assert!(err.contains("Pre-request script failed") && err.contains("nope"), "{err}");
    }
}
