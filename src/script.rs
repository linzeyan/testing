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
    pub iteration: usize,
    pub iteration_count: usize,
    /// Current row of the runner's data file (`pm.iterationData`).
    pub data: &'a HashMap<String, String>,
    pub env: &'a HashMap<String, String>,
    /// Folder variables (`pm.collectionVariables`).
    pub collection: &'a HashMap<String, String>,
    pub globals: &'a HashMap<String, String>,
    pub locals: &'a HashMap<String, String>,
    pub request: &'a WireRequest,
    pub response: Option<ScriptResponse<'a>>,
    /// The resolved URL: `pm.cookies` lists what the jar sends to it.
    pub cookie_url: &'a str,
    /// The jar Send uses; `pm.cookies.jar()` writes go straight into it. None: no jar
    /// (cookies turned off), and `pm.cookies` is empty.
    #[serde(skip)]
    pub jar: Option<&'a std::sync::Arc<crate::cookies::Jar>>,
    /// What `pm.sendRequest` sends with. None: not available (it says so when called).
    #[serde(skip)]
    pub client: Option<&'a crate::net::Clients>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// `None` values mean "unset".
pub type Changes = HashMap<String, Option<String>>;

#[derive(Deserialize, Debug, PartialEq)]
pub struct Next {
    /// None: stop the run (`setNextRequest(null)`).
    pub name: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
pub struct Output {
    pub tests: Vec<TestResult>,
    pub logs: Vec<String>,
    pub env: Changes,
    pub globals: Changes,
    pub locals: Changes,
    pub request: Option<WireRequest>,
    /// `pm.execution.setNextRequest`: where the collection runner goes after this request.
    pub next: Option<Next>,
    /// `pm.execution.skipRequest()` in a pre-request script: don't send it.
    #[serde(default)]
    pub skip: bool,
    /// Uncaught exception or syntax error; everything above is still what ran before it.
    #[serde(skip)]
    pub error: Option<String>,
}

const PRELUDE: &str = r#"
chai.Assertion.addMethod('jsonSchema', function (schema) {
  var errors = __schemaErrors(JSON.stringify(schema), JSON.stringify(this._obj));
  this.assert(!errors, 'expected value to match JSON schema:\n' + errors, 'expected value not to match JSON schema');
});
var __in = JSON.parse(__input);
var __out = { tests: [], logs: [], env: {}, globals: {}, locals: {}, request: null, next: null, skip: false };
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
  info: { requestName: __in.name, iteration: __in.iteration, iterationCount: __in.iteration_count },
  iterationData: {
    get: function (k) { return __has(__in.data, k) ? __in.data[k] : undefined; },
    has: function (k) { return __has(__in.data, k); },
    toObject: function () { return Object.assign({}, __in.data); }
  },
  environment: __scope(__in.env, __out.env),
  globals: __scope(__in.globals, __out.globals),
  // Read-only: they live in a committed .folder.toml, which a script shouldn't rewrite.
  collectionVariables: (function (s) {
    s.set = s.unset = function () {
      throw new Error('pm.collectionVariables is read-only here (edit them in Folder settings); use pm.environment.set');
    };
    return s;
  })(__scope(__in.collection, {})),
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
// Only the collection runner follows these; a single Send has no next request.
pm.execution = {
  setNextRequest: function (n) { __out.next = { name: n === null || n === undefined ? null : String(n) }; },
  skipRequest: function () { __out.skip = true; }
};
var postman = { setNextRequest: pm.execution.setNextRequest };
var __locals = __in.locals;
pm.variables = {
  get: function (k) {
    if (__has(__locals, k)) return __locals[k];
    if (__has(__in.data, k)) return __in.data[k];
    if (__has(__in.env, k)) return __in.env[k];
    if (__has(__in.collection, k)) return __in.collection[k];
    return __in.globals[k];
  },
  set: function (k, v) { v = __str(v); __locals[k] = v; __out.locals[k] = v; },
  unset: function (k) { delete __locals[k]; __out.locals[k] = null; },
  has: function (k) { return pm.variables.get(k) !== undefined; },
  replaceIn: function (t) {
    return String(t).replace(/\{\{([^{}]+)\}\}/g, function (m, k) {
      var v = pm.variables.get(k);
      if (v === undefined || v === null) v = __dynamic(k);
      return v === undefined || v === null ? m : v;
    });
  }
};
function __jarCall(op, url, name, value) {
  var r = JSON.parse(__jar(op, String(url), name === undefined ? '' : String(name), value === undefined ? '' : __str(value)));
  if (r.error) throw new Error(r.error);
  return r.ok;
}
function __cookies() { return __in.cookie_url ? __jarCall('get', __in.cookie_url) : []; }
pm.cookies = {
  get: function (n) { var l = __cookies(); for (var i = 0; i < l.length; i++) if (l[i].name === n) return l[i].value; return undefined; },
  has: function (n) { return pm.cookies.get(n) !== undefined; },
  toObject: function () { var o = {}; __cookies().forEach(function (c) { o[c.name] = c.value; }); return o; },
  all: function () { return __cookies(); },
  count: function () { return __cookies().length; },
  // Postman's jar API: callbacks are (error, result) and are called before returning.
  jar: function () {
    function call(cb, f) {
      var v;
      try { v = f(); } catch (e) { if (cb) return cb(e); throw e; }
      if (cb) cb(null, v);
    }
    return {
      get: function (url, name, cb) {
        call(cb, function () {
          var l = __jarCall('get', url);
          for (var i = 0; i < l.length; i++) if (l[i].name === name) return l[i].value;
          return undefined;
        });
      },
      getAll: function (url, cb) { call(cb, function () { return __jarCall('get', url); }); },
      set: function (url, name, value, cb) {
        // set(url, {name, value}, cb) as well as set(url, name, value, cb).
        if (name !== null && typeof name === 'object') { cb = value; value = name.value; name = name.name; }
        call(cb, function () { __jarCall('set', url, name, value); return { name: String(name), value: __str(value) }; });
      },
      unset: function (url, name, cb) { call(cb, function () { __jarCall('unset', url, name); }); },
      clear: function (url, cb) { call(cb, function () { __jarCall('clear', url); }); }
    };
  }
};
function __response(r) {
  var headers = {
    get: function (n) { var i = __findHeader(r.headers, n); return i < 0 ? undefined : r.headers[i][1]; },
    has: function (n) { return __findHeader(r.headers, n) >= 0; },
    toObject: function () { var o = {}; r.headers.forEach(function (h) { o[h[0]] = h[1]; }); return o; }
  };
  return {
    code: r.code, status: r.status, responseTime: r.time, headers: headers,
    text: function () { return r.body; },
    json: function () { return JSON.parse(r.body); }
  };
}
// Postman's request shapes: a URL, or { url, method, header (list or object), body }.
// The call blocks until the answer is in; the callback runs before it returns, and
// without one it gives a promise, for `await`.
pm.sendRequest = function (req, cb) {
  if (typeof req === 'string') req = { url: req };
  var headers = [], h = req.header || req.headers || [];
  if (Array.isArray(h)) h.forEach(function (x) {
    if (typeof x === 'string') { var i = x.indexOf(':'); headers.push([x.slice(0, i).trim(), x.slice(i + 1).trim()]); }
    else if (!x.disabled) headers.push([String(x.key), __str(x.value)]);
  });
  else Object.keys(h).forEach(function (k) { headers.push([k, __str(h[k])]); });
  var b = req.body || {}, body = null;
  function rows(l) { return (l || []).filter(function (x) { return !x.disabled; }).map(function (x) { return [String(x.key), __str(x.value)]; }); }
  if (b.mode === 'raw') body = { raw: __str(b.raw === undefined ? '' : b.raw), json: !!(b.options && b.options.raw && b.options.raw.language === 'json') };
  else if (b.mode === 'urlencoded') body = { form: rows(b.urlencoded) };
  else if (b.mode === 'formdata') body = { multipart: rows(b.formdata) };
  else if (b.mode === 'graphql') body = { raw: JSON.stringify({ query: b.graphql.query, variables: b.graphql.variables }), json: true };
  var r = JSON.parse(__send(JSON.stringify({ method: String(req.method || 'GET').toUpperCase(), url: String(req.url), headers: headers, body: body })));
  var err = r.error === undefined ? null : new Error(r.error), res = err ? null : __response(r);
  if (cb) { cb(err, res); return; }
  return err ? Promise.reject(err) : Promise.resolve(res);
};
var __asyncError;
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
        },
        jsonSchema: function (schema) { chai.expect(JSON.parse(__res.body)).to.have.jsonSchema(schema); }
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
        Err(e) => Output {
            error: Some(e),
            ..Default::default()
        },
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
        ctx.eval::<(), _>("globalThis.self = globalThis;")
            .map_err(caught)?;
        ctx.eval::<(), _>(CHAI).map_err(caught)?;
        ctx.globals()
            .set(
                "__schemaErrors",
                rquickjs::Function::new(ctx.clone(), schema_errors).map_err(caught)?,
            )
            .map_err(caught)?;
        ctx.globals()
            .set(
                "__dynamic",
                rquickjs::Function::new(ctx.clone(), |n: String| crate::fake::value(&n))
                    .map_err(caught)?,
            )
            .map_err(caught)?;
        let jar = input.jar.cloned();
        let jar_call = move |op: String, url: String, name: String, value: String| -> String {
            let reply = (|| {
                let jar = jar
                    .as_ref()
                    .ok_or("cookies are turned off for this request")?;
                let url =
                    reqwest::Url::parse(&url).map_err(|e| format!("cookie URL {url}: {e}"))?;
                Ok::<_, String>(match op.as_str() {
                    "get" => serde_json::json!(
                        (jar.for_url(&url).into_iter())
                            .map(
                                |(name, value)| serde_json::json!({ "name": name, "value": value })
                            )
                            .collect::<Vec<_>>()
                    ),
                    "set" => {
                        jar.set(&url, &name, &value)?;
                        serde_json::Value::Null
                    }
                    "unset" => {
                        jar.unset(&url, Some(&name));
                        serde_json::Value::Null
                    }
                    _ => {
                        jar.unset(&url, None);
                        serde_json::Value::Null
                    }
                })
            })();
            match reply {
                Ok(ok) => serde_json::json!({ "ok": ok }).to_string(),
                Err(e) => serde_json::json!({ "error": e }).to_string(),
            }
        };
        ctx.globals()
            .set(
                "__jar",
                rquickjs::Function::new(ctx.clone(), jar_call).map_err(caught)?,
            )
            .map_err(caught)?;
        let client = input.client.cloned();
        let send = move |spec: String| -> String {
            let reply = (|| {
                let client = client.as_ref().ok_or("pm.sendRequest isn't available here")?;
                let req = send_request(&spec)?;
                let handle = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
                // Scripts run under `block_in_place`, where blocking on the runtime is allowed.
                let r = handle.block_on(crate::runner::send(client, req))?;
                Ok::<_, String>(serde_json::json!({
                    "code": r.status, "status": r.reason, "time": r.elapsed.as_millis() as u64,
                    "headers": r.headers, "body": r.body,
                }))
            })();
            match reply {
                Ok(r) => r.to_string(),
                Err(e) => serde_json::json!({ "error": e }).to_string(),
            }
        };
        ctx.globals()
            .set(
                "__send",
                rquickjs::Function::new(ctx.clone(), send).map_err(caught)?,
            )
            .map_err(caught)?;
        ctx.eval::<(), _>(PRELUDE).map_err(caught)?;
        // Top-level `await` (for `await pm.sendRequest(…)`) needs an async function around
        // the script; on the same line, so error positions don't move.
        let wrapped;
        let script = match AWAIT.is_match(script) {
            true => {
                wrapped = format!(
                    "(async function () {{ {script}\n}})().catch(function (e) {{ __asyncError = e; }});"
                );
                &wrapped
            }
            false => script,
        };
        // The user's script may throw; keep whatever it recorded before that.
        let mut error = ctx
            .eval::<(), _>(script)
            .err()
            .map(|e| exception(&ctx, e, deadline, timeout));
        while ctx.execute_pending_job() {}
        if error.is_none() {
            let async_error: rquickjs::Value = ctx.globals().get("__asyncError").map_err(caught)?;
            if !async_error.is_undefined() {
                error = Some(match async_error.as_exception() {
                    Some(x) => x.message().unwrap_or_default(),
                    None => async_error
                        .as_string()
                        .and_then(|s| s.to_string().ok())
                        .unwrap_or_else(|| "script failed".into()),
                });
            }
        }
        let out: String = ctx.eval(EPILOGUE).map_err(caught)?;
        let mut out: Output = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        out.error = error;
        Ok(out)
    })
}

static AWAIT: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\bawait\b").expect("valid"));

/// The request `pm.sendRequest` normalised in JS, as one apitool sends.
fn send_request(spec: &str) -> Result<crate::model::Request, String> {
    use crate::model::{Body, KeyValue, Request};
    let v: serde_json::Value = serde_json::from_str(spec).map_err(|e| e.to_string())?;
    let rows = |v: &serde_json::Value| -> Vec<KeyValue> {
        let pairs = v.as_array().into_iter().flatten();
        pairs
            .map(|p| {
                KeyValue::new(
                    p[0].as_str().unwrap_or_default(),
                    p[1].as_str().unwrap_or_default(),
                )
            })
            .collect()
    };
    let b = &v["body"];
    let body = if let Some(raw) = b["raw"].as_str() {
        match b["json"] == true {
            true => Body::Json { text: raw.into() },
            false => Body::Text { text: raw.into() },
        }
    } else if b["form"].is_array() {
        Body::Form {
            fields: rows(&b["form"]),
        }
    } else if b["multipart"].is_array() {
        Body::Multipart {
            parts: rows(&b["multipart"]),
        }
    } else {
        Body::None
    };
    let url = v["url"].as_str().unwrap_or_default();
    if url.is_empty() || url == "undefined" {
        return Err("pm.sendRequest needs a URL".into());
    }
    Ok(Request {
        method: v["method"].as_str().unwrap_or("GET").to_owned(),
        url: url.to_owned(),
        headers: rows(&v["headers"]),
        body,
        ..Default::default()
    })
}

/// Native because JSON Schema validation in JS would mean vendoring Ajv (~120 KB) into
/// every script run. Returns "" when valid, else one line per violation.
fn schema_errors(schema: String, instance: String) -> String {
    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s);
    let (Ok(schema), Ok(instance)) = (parse(&schema), parse(&instance)) else {
        return "value is not JSON".into();
    };
    let validator = match jsonschema::validator_for(&schema) {
        Ok(v) => v,
        Err(e) => return format!("invalid schema: {e}"),
    };
    validator
        .iter_errors(&instance)
        .map(|e| format!("{} {e}", e.instance_path()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn exception(
    ctx: &rquickjs::Ctx<'_>,
    e: rquickjs::Error,
    deadline: Instant,
    timeout: Duration,
) -> String {
    if Instant::now() > deadline {
        return format!("script timed out after {:.1} s", timeout.as_secs_f32());
    }
    if e.is_exception() {
        let value = ctx.catch();
        if let Some(x) = value.as_exception() {
            let message = x.message().unwrap_or_default();
            let line = x.stack().and_then(|s| {
                s.lines()
                    .find(|l| l.contains("<eval>"))
                    .map(str::trim)
                    .map(str::to_owned)
            });
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
        WireRequest {
            method: "GET".into(),
            url: "https://{{host}}/users".into(),
            headers: vec![],
        }
    }

    fn input<'a>(
        req: &'a WireRequest,
        env: &'a HashMap<String, String>,
        response: Option<ScriptResponse<'a>>,
    ) -> Input<'a> {
        static EMPTY: std::sync::LazyLock<HashMap<String, String>> =
            std::sync::LazyLock::new(HashMap::new);
        Input {
            name: "t",
            iteration: 0,
            iteration_count: 1,
            data: &EMPTY,
            env,
            collection: &EMPTY,
            globals: &EMPTY,
            locals: &EMPTY,
            request: req,
            response,
            cookie_url: "",
            jar: None,
            client: None,
        }
    }

    #[test]
    fn postman_style_tests_run_against_the_response() {
        let (req, env) = (request(), HashMap::new());
        let headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        let body = r#"{"token":"abc","items":[1,2]}"#;
        let resp = ScriptResponse {
            code: 201,
            status: "Created",
            time: 12,
            headers: &headers,
            body,
        };
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
        let passed: Vec<_> = out
            .tests
            .iter()
            .map(|t| (t.name.as_str(), t.passed))
            .collect();
        assert_eq!(
            passed,
            [
                ("status", true),
                ("ok", true),
                ("json", true),
                ("fails", false)
            ]
        );
        assert!(
            out.tests[3]
                .error
                .as_deref()
                .unwrap()
                .contains("expected 201 to equal 200")
        );
        // Chaining: the token captured here is what the next request's {{token}} resolves to.
        assert_eq!(out.env["token"], Some("abc".into()));
        assert_eq!(out.logs, ["got [1,2]"]);
    }

    #[test]
    fn json_schema_contract_checks_report_the_violating_path() {
        let (req, env) = (request(), HashMap::new());
        let body = r#"{"id": 7, "tags": ["a", 3]}"#;
        let resp = ScriptResponse {
            code: 200,
            status: "OK",
            time: 1,
            headers: &[],
            body,
        };
        let out = run(
            r#"
            var schema = { type: "object", required: ["id", "tags"],
              properties: { id: { type: "integer" }, tags: { type: "array", items: { type: "string" } } } };
            pm.test("contract", function () { pm.response.to.have.jsonSchema(schema); });
            pm.test("id only", function () {
              pm.expect(pm.response.json()).to.have.jsonSchema({ required: ["id"] });
            });
            pm.test("bad schema", function () { pm.response.to.have.jsonSchema({ type: 12 }); });
            "#,
            &input(&req, &env, Some(resp)),
        );
        assert_eq!(out.error, None);
        let passed: Vec<_> = out.tests.iter().map(|t| t.passed).collect();
        assert_eq!(passed, [false, true, false]);
        let why = out.tests[0].error.as_deref().unwrap();
        assert!(why.contains("/tags/1") && why.contains("string"), "{why}");
        assert!(
            out.tests[2]
                .error
                .as_deref()
                .unwrap()
                .contains("invalid schema")
        );
    }

    #[test]
    fn pre_request_script_can_modify_the_request() {
        let (req, env) = (
            request(),
            HashMap::from([("host".to_owned(), "api.test".to_owned())]),
        );
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
    fn replace_in_resolves_variables_and_dynamic_ones() {
        // Postman scripts get a dynamic value into JS this way, e.g. to log or reuse it.
        let (req, env) = (
            request(),
            HashMap::from([("host".to_owned(), "api.test".to_owned())]),
        );
        let out = run(
            r#"console.log(pm.variables.replaceIn("{{host}}|{{$randomEmail}}|{{nope}}"));"#,
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
        let parts: Vec<_> = out.logs[0].split('|').collect();
        assert_eq!(parts[0], "api.test");
        assert!(
            parts[1].contains('@') && !parts[1].contains("{{"),
            "{parts:?}"
        );
        assert_eq!(parts[2], "{{nope}}", "unknown names stay verbatim");
    }

    #[test]
    fn errors_keep_earlier_results_and_loops_time_out() {
        let (req, env) = (request(), HashMap::new());
        let out = run(
            "console.log('before'); undefinedFn();",
            &input(&req, &env, None),
        );
        assert_eq!(out.logs, ["before"]);
        assert!(out.error.unwrap().contains("undefinedFn"));

        let out = run("function (", &input(&req, &env, None));
        assert!(out.error.is_some(), "syntax errors must surface");

        let started = Instant::now();
        let out = run_with_timeout(
            "while (true) {}",
            &input(&req, &env, None),
            Duration::from_millis(200),
        );
        assert!(out.error.unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
