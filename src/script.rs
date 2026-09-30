//! Postman-style scripts (`pm.test`, `pm.expect`, `pm.environment`, …) on QuickJS.
//!
//! Rust and JS exchange a single JSON document each way, so the `pm` API lives entirely
//! in the JS prelude below instead of being spread across many native bindings.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const CHAI: &str = include_str!("../vendor/chai.js");
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Serialize)]
pub struct ScriptResponse<'a> {
    pub code: u16,
    pub status: &'a str,
    pub time: u128,
    pub headers: &'a [(String, String)],
    pub body: &'a str,
}

#[derive(Serialize)]
pub struct Input<'a> {
    pub name: &'a str,
    pub env: &'a HashMap<String, String>,
    pub globals: &'a HashMap<String, String>,
    pub locals: &'a HashMap<String, String>,
    pub request: &'a WireRequest,
    pub response: Option<ScriptResponse<'a>>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// `None` values mean "unset".
pub type Changes = HashMap<String, Option<String>>;

#[derive(Deserialize, Default, Debug)]
pub struct Output {
    pub tests: Vec<TestResult>,
    pub logs: Vec<String>,
    pub env: Changes,
    pub globals: Changes,
    pub locals: Changes,
    pub request: Option<WireRequest>,
    /// Uncaught exception or syntax error; everything above is still what ran before it.
    #[serde(skip)]
    pub error: Option<String>,
}

const PRELUDE: &str = r#"
var __in = JSON.parse(__input);
var __out = { tests: [], logs: [], env: {}, globals: {}, locals: {}, request: null };
function __str(v) {
  if (typeof v === 'string') return v;
  try { var s = JSON.stringify(v); return s === undefined ? String(v) : s; } catch (e) { return String(v); }
}
var console = { log: function () { __out.logs.push(Array.prototype.map.call(arguments, __str).join(' ')); } };
console.info = console.warn = console.error = console.debug = console.log;
function __has(o, k) { return Object.prototype.hasOwnProperty.call(o, k); }
function __scope(store, changes) {
  return {
    get: function (k) { return __has(store, k) ? store[k] : undefined; },
    set: function (k, v) { v = __str(v); store[k] = v; changes[k] = v; },
    unset: function (k) { delete store[k]; changes[k] = null; },
    has: function (k) { return __has(store, k); },
    toObject: function () { return Object.assign({}, store); }
  };
}
var __req = __in.request;
function __findHeader(list, name) {
  name = String(name).toLowerCase();
  for (var i = 0; i < list.length; i++) if (String(list[i][0]).toLowerCase() === name) return i;
  return -1;
}
var pm = {
  info: { requestName: __in.name },
  environment: __scope(__in.env, __out.env),
  globals: __scope(__in.globals, __out.globals),
  expect: chai.expect,
  test: function (name, fn) {
    try { fn(); __out.tests.push({ name: String(name), passed: true }); }
    catch (e) { __out.tests.push({ name: String(name), passed: false, error: String(e && e.message || e) }); }
  },
  request: {
    method: __req.method,
    url: __req.url,
    headers: {
      get: function (n) { var i = __findHeader(__req.headers, n); return i < 0 ? undefined : __req.headers[i][1]; },
      has: function (n) { return __findHeader(__req.headers, n) >= 0; },
      add: function (h) { __req.headers.push([String(h.key), __str(h.value)]); },
      upsert: function (h) {
        var i = __findHeader(__req.headers, h.key);
        if (i < 0) __req.headers.push([String(h.key), __str(h.value)]); else __req.headers[i][1] = __str(h.value);
      },
      remove: function (n) { var i; while ((i = __findHeader(__req.headers, n)) >= 0) __req.headers.splice(i, 1); }
    }
  }
};
var __locals = __in.locals;
pm.variables = {
  get: function (k) {
    if (__has(__locals, k)) return __locals[k];
    if (__has(__in.env, k)) return __in.env[k];
    return __in.globals[k];
  },
  set: function (k, v) { v = __str(v); __locals[k] = v; __out.locals[k] = v; },
  unset: function (k) { delete __locals[k]; __out.locals[k] = null; },
  has: function (k) { return pm.variables.get(k) !== undefined; }
};
if (__in.response) {
  var __res = __in.response;
  var __resHeaders = {
    get: function (n) { var i = __findHeader(__res.headers, n); return i < 0 ? undefined : __res.headers[i][1]; },
    has: function (n) { return __findHeader(__res.headers, n) >= 0; }
  };
  pm.response = {
    code: __res.code,
    status: __res.status,
    responseTime: __res.time,
    headers: __resHeaders,
    text: function () { return __res.body; },
    json: function () { return JSON.parse(__res.body); },
    to: {
      have: {
        status: function (c) {
          if (typeof c === 'number') chai.expect(__res.code, 'status code').to.equal(c);
          else chai.expect(__res.status, 'status').to.equal(c);
        },
        header: function (n, v) {
          chai.expect(__resHeaders.has(n), 'header ' + n).to.equal(true);
          if (arguments.length > 1) chai.expect(__resHeaders.get(n), 'header ' + n).to.equal(v);
        },
        body: function (b) { chai.expect(__res.body).to.equal(b); },
        jsonBody: function (path) {
          var json = JSON.parse(__res.body);
          if (arguments.length > 0) chai.expect(json).to.have.nested.property(path);
        }
      },
      be: {}
    }
  };
  Object.defineProperty(pm.response.to.be, 'ok', {
    get: function () { chai.expect(__res.code, 'status code').to.be.within(200, 299); return true; }
  });
  Object.defineProperty(pm.response.to.be, 'json', {
    get: function () { JSON.parse(__res.body); return true; }
  });
}
"#;

const EPILOGUE: &str = r#"
__req.method = String(pm.request.method);
__req.url = String(pm.request.url);
__out.request = __req;
JSON.stringify(__out)
"#;

pub fn run(script: &str, input: &Input<'_>) -> Output {
    run_with_timeout(script, input, TIMEOUT)
}

fn run_with_timeout(script: &str, input: &Input<'_>, timeout: Duration) -> Output {
    match run_inner(script, input, timeout) {
        Ok(out) => out,
        Err(e) => Output { error: Some(e), ..Default::default() },
    }
}

fn run_inner(script: &str, input: &Input<'_>, timeout: Duration) -> Result<Output, String> {
    let json = serde_json::to_string(input).map_err(|e| e.to_string())?;
    let rt = rquickjs::Runtime::new().map_err(|e| e.to_string())?;
    rt.set_memory_limit(64 << 20);
    // A script with an endless loop must not hang the request forever.
    let deadline = Instant::now() + timeout;
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let ctx = rquickjs::Context::full(&rt).map_err(|e| e.to_string())?;
    ctx.with(|ctx| {
        let caught = |e: rquickjs::Error| exception(&ctx, e, deadline, timeout);
        ctx.globals().set("__input", json).map_err(caught)?;
        // chai's UMD wrapper looks for window/global/self and otherwise uses `this`, which
        // is undefined here; `self` is the one alias that doesn't imply a browser.
        ctx.eval::<(), _>("globalThis.self = globalThis;").map_err(caught)?;
        ctx.eval::<(), _>(CHAI).map_err(caught)?;
        ctx.eval::<(), _>(PRELUDE).map_err(caught)?;
        // The user's script may throw; keep whatever it recorded before that.
        let error = ctx.eval::<(), _>(script).err().map(|e| exception(&ctx, e, deadline, timeout));
        let out: String = ctx.eval(EPILOGUE).map_err(caught)?;
        let mut out: Output = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        out.error = error;
        Ok(out)
    })
}

fn exception(ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error, deadline: Instant, timeout: Duration) -> String {
    if Instant::now() > deadline {
        return format!("script timed out after {:.1} s", timeout.as_secs_f32());
    }
    if e.is_exception() {
        let value = ctx.catch();
        if let Some(x) = value.as_exception() {
            let message = x.message().unwrap_or_default();
            let line = x.stack().and_then(|s| s.lines().find(|l| l.contains("<eval>")).map(str::trim).map(str::to_owned));
            return match line {
                Some(at) => format!("{message} ({at})"),
                None => message,
            };
        }
        if let Some(s) = value.as_string().and_then(|s| s.to_string().ok()) {
            return s;
        }
    }
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> WireRequest {
        WireRequest { method: "GET".into(), url: "https://{{host}}/users".into(), headers: vec![] }
    }

    fn input<'a>(req: &'a WireRequest, env: &'a HashMap<String, String>, response: Option<ScriptResponse<'a>>) -> Input<'a> {
        static EMPTY: std::sync::LazyLock<HashMap<String, String>> = std::sync::LazyLock::new(HashMap::new);
        Input { name: "t", env, globals: &EMPTY, locals: &EMPTY, request: req, response }
    }

    #[test]
    fn postman_style_tests_run_against_the_response() {
        let (req, env) = (request(), HashMap::new());
        let headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        let body = r#"{"token":"abc","items":[1,2]}"#;
        let resp = ScriptResponse { code: 201, status: "Created", time: 12, headers: &headers, body };
        let out = run(
            r#"
            pm.test("status", function () { pm.response.to.have.status(201); });
            pm.test("ok", function () { pm.response.to.be.ok; });
            pm.test("json", function () { pm.expect(pm.response.json().items).to.have.lengthOf(2); });
            pm.test("fails", function () { pm.expect(pm.response.code).to.equal(200); });
            pm.environment.set("token", pm.response.json().token);
            console.log("got", pm.response.json().items);
            "#,
            &input(&req, &env, Some(resp)),
        );
        assert_eq!(out.error, None);
        let passed: Vec<_> = out.tests.iter().map(|t| (t.name.as_str(), t.passed)).collect();
        assert_eq!(passed, [("status", true), ("ok", true), ("json", true), ("fails", false)]);
        assert!(out.tests[3].error.as_deref().unwrap().contains("expected 201 to equal 200"));
        // Chaining: the token captured here is what the next request's {{token}} resolves to.
        assert_eq!(out.env["token"], Some("abc".into()));
        assert_eq!(out.logs, ["got [1,2]"]);
    }

    #[test]
    fn pre_request_script_can_modify_the_request() {
        let (req, env) = (request(), HashMap::from([("host".to_owned(), "api.test".to_owned())]));
        let out = run(
            r#"
            pm.request.headers.add({ key: "X-Ts", value: 42 });
            pm.request.url = pm.request.url + "?v=" + pm.environment.get("host");
            pm.variables.set("local", "1");
            "#,
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
        let r = out.request.unwrap();
        assert_eq!(r.url, "https://{{host}}/users?v=api.test");
        assert_eq!(r.headers, [("X-Ts".to_owned(), "42".to_owned())]);
        assert_eq!(out.locals["local"], Some("1".into()));
    }

    #[test]
    fn errors_keep_earlier_results_and_loops_time_out() {
        let (req, env) = (request(), HashMap::new());
        let out = run("console.log('before'); undefinedFn();", &input(&req, &env, None));
        assert_eq!(out.logs, ["before"]);
        assert!(out.error.unwrap().contains("undefinedFn"));

        let out = run("function (", &input(&req, &env, None));
        assert!(out.error.is_some(), "syntax errors must surface");

        let started = Instant::now();
        let out = run_with_timeout("while (true) {}", &input(&req, &env, None), Duration::from_millis(200));
        assert!(out.error.unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
