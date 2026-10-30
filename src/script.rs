//! Postman-style scripts (`pm.test`, `pm.expect`, `pm.environment`, …) on QuickJS.
//!
//! Rust and JS exchange a single JSON document each way, so the `pm` API lives entirely
//! in the JS prelude below instead of being spread across many native bindings.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const CHAI: &str = include_str!("../vendor/chai.js");
/// What Postman's sandbox lets `require` load; evaluated only when a script asks, so the
/// other runs pay nothing for them.
const LIBS: &[(&str, &str)] = &[
    ("crypto-js", include_str!("../vendor/crypto-js.js")),
    ("lodash", include_str!("../vendor/lodash.min.js")),
    ("moment", include_str!("../vendor/moment.min.js")),
    // Not in Postman's sandbox, where it comes through `pm.require('npm:tweetnacl@…')`;
    // bundled for Ed25519 signing, which crypto-js lacks.
    ("tweetnacl", include_str!("../vendor/tweetnacl.min.js")),
    // 3.2.0 as CommonJS (see its end); its SHA-512 is set when loaded (`__setup`).
    ("@noble/ed25519", include_str!("../vendor/noble-ed25519.js")),
];
/// The user's script is evaluated under this file name, so its frames in a stack stand
/// apart from the prelude's (`eval_script`).
const FILE: &str = "script";
/// Top-level `await` needs an async function around the script; on the same line, so
/// error lines don't move (columns on line 1 move by its length).
const ASYNC_OPEN: &str = "(async function () { ";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WireRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// A JSON or text body's text, `{{variables}}` unresolved (`pm.request.body.raw`);
    /// None for no body or one of another kind.
    #[serde(default)]
    pub body: Option<String>,
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
  // As Node's console prints one: as JSON an Error is `{}`.
  if (v instanceof Error) return String(v) + (v.stack ? '\n' + String(v.stack).replace(/\s+$/, '') : '');
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
    },
    // Postman's RequestBody, raw mode only: a script signing the body reads it here and
    // writes the signed one back.
    body: {
      get mode() { return __req.body === null || __req.body === undefined ? undefined : 'raw'; },
      get raw() { return __req.body === null ? undefined : __req.body; },
      set raw(v) { __req.body = v === undefined || v === null ? '' : __str(v); },
      update: function (v) {
        if (v !== null && typeof v === 'object') {
          if (v.mode !== undefined && v.mode !== 'raw') throw new Error("pm.request.body.update: only mode 'raw' here");
          v = v.raw;
        }
        this.raw = v;
      },
      isEmpty: function () { return !__req.body; },
      toString: function () { return __req.body || ''; },
      toJSON: function () { return { mode: this.mode, raw: this.raw }; }
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
    if (__has(__in.collection, k)) return __in.collection[k];
    if (__has(__in.env, k)) return __in.env[k];
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
// Rejected promises nothing handles yet, as the runtime reports them: an async function
// that throws fails the script, as a throw at the top does, instead of its request going
// out without what it was to set.
var __rejected = [];
function __track(p, reason, handled) {
  var i = -1;
  for (var j = 0; j < __rejected.length; j++) if (__rejected[j][0] === p) i = j;
  if (handled) { if (i >= 0) __rejected.splice(i, 1); }
  else if (i < 0) __rejected.push([p, reason]);
}
// Postman's sandbox modules. uuid and tv4 stand on what's native here already.
var __modules = {
  chai: chai,
  uuid: { v4: function () { return __dynamic('$guid'); } },
  tv4: {
    error: null,
    validate: function (data, schema) {
      var e = __schemaErrors(JSON.stringify(schema), JSON.stringify(data));
      this.error = e ? { message: e } : null;
      return !e;
    },
    validateResult: function (data, schema) {
      var valid = this.validate(data, schema);
      return { valid: valid, error: this.error };
    }
  }
};
function __hex(b) {
  var s = '';
  for (var i = 0; i < b.length; i++) s += (b[i] < 16 ? '0' : '') + b[i].toString(16);
  return s;
}
function __unhex(h) {
  var b = new Uint8Array(h.length >> 1);
  for (var i = 0; i < b.length; i++) {
    var v = parseInt(h.substr(i * 2, 2), 16);
    if (isNaN(v)) return b.subarray(0, i); // as Node: up to the first bad pair
    b[i] = v;
  }
  return b;
}
// BufferSource (or a plain array) as bytes, without copying a typed array.
function __bytes(v) {
  if (v instanceof Uint8Array) return v;
  if (ArrayBuffer.isView(v)) return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
  if (v instanceof ArrayBuffer) return new Uint8Array(v);
  if (Array.isArray(v)) return Uint8Array.from(v);
  throw new TypeError('expected bytes (a Uint8Array, ArrayBuffer or DataView), got ' + typeof v);
}
function __utf8(s) {
  s = String(s);
  var out = [];
  for (var i = 0; i < s.length; i++) {
    var c = s.codePointAt(i);
    if (c > 0xffff) i++;
    if (c < 0x80) out.push(c);
    else if (c < 0x800) out.push(0xc0 | c >> 6, 0x80 | c & 63);
    else if (c < 0x10000) out.push(0xe0 | c >> 12, 0x80 | c >> 6 & 63, 0x80 | c & 63);
    else out.push(0xf0 | c >> 18, 0x80 | c >> 12 & 63, 0x80 | c >> 6 & 63, 0x80 | c & 63);
  }
  return Uint8Array.from(out);
}
function __unutf8(b) {
  var s = '';
  for (var i = 0; i < b.length;) {
    var c = b[i++], n = c >= 0xf0 ? 3 : c >= 0xe0 ? 2 : c >= 0xc0 ? 1 : 0;
    if (n) c &= 0x3f >> n;
    while (n-- > 0 && i < b.length) c = c << 6 | b[i++] & 63;
    s += String.fromCodePoint(c);
  }
  return s;
}
function __digestSync(alg, data) {
  var bytes = typeof data === 'string' ? __utf8(data) : __bytes(data);
  var h = __digest(String(alg).toUpperCase(), __hex(bytes));
  if (!h) throw new Error("digest: '" + alg + "' isn't supported here (SHA-256, SHA-384, SHA-512)");
  return __unhex(h);
}
// WebCrypto's random numbers and digests, as browsers and Node have them: tweetnacl,
// crypto-js and noble-ed25519 take their randomness here, and noble's async API its SHA-512.
var crypto = {
  getRandomValues: function (a) {
    var b = __bytes(a);
    if (b.length > 65536) throw new Error('getRandomValues: at most 65536 bytes at a time');
    b.set(__unhex(__random(b.length)));
    return a;
  },
  randomUUID: function () { return __dynamic('$guid'); },
  subtle: {
    digest: function (alg, data) {
      try { var h = __digestSync(typeof alg === 'string' ? alg : alg && alg.name, data); }
      catch (e) { return Promise.reject(e); }
      return Promise.resolve(h.buffer);
    }
  }
};
if (typeof TextEncoder === 'undefined') {
  var TextEncoder = function () {};
  TextEncoder.prototype.encoding = 'utf-8';
  TextEncoder.prototype.encode = function (s) { return __utf8(s === undefined ? '' : s); };
  var TextDecoder = function () {};
  TextDecoder.prototype.encoding = 'utf-8';
  TextDecoder.prototype.decode = function (b) { return b === undefined ? '' : __unutf8(__bytes(b)); };
}
// Node's Buffer, which Postman's sandbox has: the encodings signing scripts use.
var Buffer = class Buffer extends Uint8Array {
  static from(v, enc) {
    if (typeof v !== 'string') {
      if (v instanceof ArrayBuffer) return new Buffer(v, enc || 0, arguments.length > 2 ? arguments[2] : v.byteLength - (enc || 0));
      var src = __bytes(v), b = new Buffer(src.length);
      b.set(src);
      return b;
    }
    switch (String(enc || 'utf8').toLowerCase()) {
      case 'hex': return Buffer.from(__unhex(v));
      case 'base64': case 'base64url':
        var bin = atob(v.replace(/-/g, '+').replace(/_/g, '/'));
        return Buffer.from(Array.prototype.map.call(bin, function (c) { return c.charCodeAt(0); }));
      case 'latin1': case 'binary': case 'ascii':
        return Buffer.from(Array.prototype.map.call(v, function (c) { return c.charCodeAt(0) & 255; }));
      case 'utf8': case 'utf-8': return Buffer.from(__utf8(v));
      default: throw new Error('Buffer: unknown encoding ' + enc);
    }
  }
  static alloc(n, fill) { var b = new Buffer(n); if (fill !== undefined) b.fill(typeof fill === 'number' ? fill : Buffer.from(fill)[0]); return b; }
  static isBuffer(b) { return b instanceof Buffer; }
  static byteLength(s, enc) { return typeof s === 'string' ? Buffer.from(s, enc).length : __bytes(s).length; }
  static concat(list) {
    var n = 0, at = 0;
    list.forEach(function (b) { n += b.length; });
    var out = new Buffer(n);
    list.forEach(function (b) { out.set(b, at); at += b.length; });
    return out;
  }
  toString(enc) {
    switch (String(enc || 'utf8').toLowerCase()) {
      case 'hex': return __hex(this);
      case 'base64': case 'base64url':
        var s = btoa(String.fromCharCode.apply(null, Array.from(this)));
        return String(enc).toLowerCase() === 'base64' ? s : s.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
      case 'latin1': case 'binary': case 'ascii': return String.fromCharCode.apply(null, Array.from(this));
      default: return __unutf8(this);
    }
  }
  equals(o) { o = __bytes(o); return o.length === this.length && this.every(function (x, i) { return x === o[i]; }); }
  toJSON() { return { type: 'Buffer', data: Array.from(this) }; }
};
function __hashes() {
  var f = function (alg) { return function (m) { return __digestSync(alg, m); }; };
  return { sha256: f('SHA-256'), sha384: f('SHA-384'), sha512: f('SHA-512') };
}
// noble's hash modules, for code that sets `etc.sha512Sync` (v2) or `hashes.sha512` (v3).
['@noble/hashes/sha2', '@noble/hashes/sha256', '@noble/hashes/sha512'].forEach(function (n) {
  __modules[n] = __modules[n + '.js'] = __hashes();
});
// What a bundled module needs set once it's loaded.
var __setup = {
  '@noble/ed25519': function (ed) { ed.hashes.sha512 = __hashes().sha512; }
};
var __known = 'crypto-js, lodash, moment, tweetnacl, @noble/ed25519, @noble/hashes/sha2, uuid, tv4, chai, atob, btoa';
function require(name) {
  name = String(name);
  if (__has(__modules, name)) return __modules[name];
  var src = __lib(name);
  if (src === undefined || src === null) throw new Error("require: no module '" + name + "' here (" + __known + ")");
  var module = { exports: {} };
  // What a library probes for (crypto-js and tweetnacl try Node's `crypto`) is missing,
  // not an error: they do without it.
  var probe = function (n) { return __has(__modules, n) || __lib(n) != null ? require(n) : undefined; };
  new Function('module', 'exports', 'require', src)(module, module.exports, probe);
  if (__has(__setup, name)) __setup[name](module.exports);
  return __modules[name] = module.exports;
}
// Postman's package library: `npm:name@version` comes from the registry there. Here the
// name picks a bundled module, whatever the version.
pm.require = function (name) {
  var m = /^npm:(@?[^@]+)(@.*)?$/.exec(String(name));
  return require(m ? m[1] : name);
};
if (typeof btoa !== 'function') {
  var __b64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  // Latin-1 in and out, as in browsers: a byte per character.
  var btoa = function (s) {
    s = String(s);
    var out = '';
    for (var i = 0; i < s.length; i += 3) {
      var a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2);
      if (a > 255 || b > 255 || c > 255) throw new Error('btoa: characters outside Latin-1');
      var n = (a << 16) | ((b || 0) << 8) | (c || 0);
      out += __b64[n >> 18 & 63] + __b64[n >> 12 & 63] + (i + 1 < s.length ? __b64[n >> 6 & 63] : '=') + (i + 2 < s.length ? __b64[n & 63] : '=');
    }
    return out;
  };
  var atob = function (s) {
    s = String(s).replace(/[\s=]+/g, '');
    var out = '', bits = 0, n = 0;
    for (var i = 0; i < s.length; i++) {
      var v = __b64.indexOf(s[i]);
      if (v < 0) throw new Error('atob: not base64');
      n = (n << 6) | v; bits += 6;
      if (bits >= 8) { bits -= 8; out += String.fromCharCode(n >> bits & 255); }
    }
    return out;
  };
}
__modules.atob = atob;
__modules.btoa = btoa;
// The Assert table's right-hand side: a number, true/false/null, a quoted string, or text;
// `{{vars}}` resolved first.
function __literal(s) {
  s = pm.variables.replaceIn(String(s).trim());
  if (s === 'true') return true;
  if (s === 'false') return false;
  if (s === 'null') return null;
  if (s === 'undefined') return undefined;
  if (s !== '' && !isNaN(Number(s))) return Number(s);
  var q = s.charAt(0);
  if (s.length > 1 && (q === '"' || q === "'") && s.charAt(s.length - 1) === q) return s.slice(1, -1);
  return s;
}
function __list(s) {
  s = String(s).trim();
  if (s.charAt(0) === '[' && s.charAt(s.length - 1) === ']') s = s.slice(1, -1);
  return s.split(',').map(__literal);
}
function __regex(s) {
  s = pm.variables.replaceIn(String(s).trim());
  if (s.length > 1 && s.charAt(0) === '/' && s.charAt(s.length - 1) === '/') s = s.slice(1, -1);
  return new RegExp(s);
}
var __ops = (function (E) {
  return {
    eq: function (l, a) { E(l).to.eql(__literal(a)); },
    neq: function (l, a) { E(l).to.not.eql(__literal(a)); },
    gt: function (l, a) { E(l).to.be.above(__literal(a)); },
    gte: function (l, a) { E(l).to.be.at.least(__literal(a)); },
    lt: function (l, a) { E(l).to.be.below(__literal(a)); },
    lte: function (l, a) { E(l).to.be.at.most(__literal(a)); },
    in: function (l, a) { E(__list(a)).to.deep.include(l); },
    notIn: function (l, a) { E(__list(a)).to.not.deep.include(l); },
    contains: function (l, a) { E(l).to.include(__literal(a)); },
    notContains: function (l, a) { E(l).to.not.include(__literal(a)); },
    length: function (l, a) { E(l).to.have.lengthOf(__literal(a)); },
    matches: function (l, a) { E(String(l)).to.match(__regex(a)); },
    notMatches: function (l, a) { E(String(l)).to.not.match(__regex(a)); },
    startsWith: function (l, a) { var x = String(__literal(a)); E(String(l).slice(0, x.length), 'start').to.equal(x); },
    endsWith: function (l, a) { var x = String(__literal(a)); E(String(l).slice(-x.length), 'end').to.equal(x); },
    between: function (l, a) { var b = __list(a); E(l).to.be.within(b[0], b[1]); },
    isEmpty: function (l) { E(l).to.be.empty; },
    isNotEmpty: function (l) { E(l).to.not.be.empty; },
    isNull: function (l) { E(l).to.be.null; },
    isUndefined: function (l) { E(l).to.be.undefined; },
    isDefined: function (l) { E(l).to.not.be.undefined; },
    isTruthy: function (l) { E(l).to.be.ok; },
    isFalsy: function (l) { E(l).to.not.be.ok; },
    isJson: function (l) { E(l !== null && typeof l === 'object', 'expected ' + __str(l) + ' to be JSON').to.equal(true); },
    isNumber: function (l) { E(l).to.be.a('number'); },
    isString: function (l) { E(l).to.be.a('string'); },
    isBoolean: function (l) { E(l).to.be.a('boolean'); },
    isArray: function (l) { E(l).to.be.an('array'); }
  };
})(chai.expect);
// What `res` is to the Assert and Vars tables, as in Bruno: the body parsed if it's JSON.
function __resValue() {
  var text = __in.response.body, body;
  try { body = JSON.parse(text); } catch (e) { body = text; }
  var headers = {};
  __in.response.headers.forEach(function (h) { headers[String(h[0]).toLowerCase()] = h[1]; });
  return { status: __in.response.code, statusText: __in.response.status, responseTime: __in.response.time, headers: headers, body: body };
}
// Rows of [name, JS expression over `res`]: each sets that environment variable, as
// pm.environment.set would. Text stays as it is, anything else becomes its JSON. A row
// that throws or finds nothing is a failed test, so a missing token is seen right here.
function __vars(rows) {
  var res = __resValue();
  rows.forEach(function (row) {
    var value, error;
    try { value = new Function('res', 'return (' + row[1] + ');')(res); } catch (e) { error = e; }
    if (error === undefined && value === undefined) error = new Error(row[1] + ' is undefined');
    if (error !== undefined) {
      pm.test(row[0] + ' = ' + row[1], function () { throw error; });
      return;
    }
    pm.environment.set(row[0], typeof value === 'string' ? value : JSON.stringify(value));
  });
}
// Rows of [left, "op value"]; the left side is a JS expression over `res`, as in Bruno.
function __asserts(rows) {
  var res = __resValue();
  rows.forEach(function (row) {
    var spec = row[1], gap = spec.indexOf(' ');
    var op = gap < 0 ? spec : spec.slice(0, gap), arg = gap < 0 ? '' : spec.slice(gap + 1);
    if (!__has(__ops, op)) { op = 'eq'; arg = spec; }
    pm.test(row[0] + ' ' + spec, function () {
      __ops[op](new Function('res', 'return (' + row[0] + ');')(res), arg);
    });
  });
}
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
    rt.set_host_promise_rejection_tracker(Some(Box::new(|ctx, promise, reason, handled| {
        // Before the prelude there is no `__track`, and nothing of the user's to report.
        let track = ctx.globals().get::<_, rquickjs::Function>("__track");
        if track.is_ok_and(|f| f.call::<_, ()>((promise, reason, handled)).is_err()) {
            ctx.catch();
        }
    })));
    let ctx = rquickjs::Context::full(&rt).map_err(|e| e.to_string())?;
    ctx.with(|ctx| {
        let caught = |e: rquickjs::Error| exception(&ctx, e, deadline, timeout, 0);
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
                "__lib",
                rquickjs::Function::new(ctx.clone(), |name: String| {
                    LIBS.iter().find(|(n, _)| *n == name).map(|(_, src)| *src)
                })
                .map_err(caught)?,
            )
            .map_err(caught)?;
        ctx.globals()
            .set(
                "__dynamic",
                rquickjs::Function::new(ctx.clone(), |n: String| crate::fake::value(&n))
                    .map_err(caught)?,
            )
            .map_err(caught)?;
        // Bytes cross as hex: few and small (keys, signatures, digests of a body).
        ctx.globals()
            .set(
                "__random",
                rquickjs::Function::new(ctx.clone(), |n: usize| {
                    let mut buf = vec![0; n.min(65536)];
                    getrandom::fill(&mut buf).expect("the system's random source");
                    hex(&buf)
                })
                .map_err(caught)?,
            )
            .map_err(caught)?;
        ctx.globals()
            .set(
                "__digest",
                rquickjs::Function::new(ctx.clone(), digest).map_err(caught)?,
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
                let client = client
                    .as_ref()
                    .ok_or("pm.sendRequest isn't available here")?;
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
        // For `await pm.sendRequest(…)`; a rejection of it is tracked as any other.
        let (source, shift) = match AWAIT.is_match(script) {
            true => (format!("{ASYNC_OPEN}{script}\n}})();"), ASYNC_OPEN.len()),
            false => (script.to_owned(), 0),
        };
        let mut options = rquickjs::context::EvalOptions::default();
        options.filename = Some(FILE.into());
        // The user's script may throw; keep whatever it recorded before that.
        let mut error = (ctx.eval_with_options::<(), _>(source, options).err())
            .map(|e| exception(&ctx, e, deadline, timeout, shift));
        while ctx.execute_pending_job() {}
        if error.is_none() {
            let reason: rquickjs::Value = ctx
                .eval("__rejected.length ? __rejected[0][1] : undefined")
                .map_err(caught)?;
            if !reason.is_undefined() {
                error = Some(thrown(&reason, shift));
            }
        }
        let out: String = ctx.eval(EPILOGUE).map_err(caught)?;
        let mut out: Output = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        out.error = error;
        Ok(out)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `crypto.subtle.digest` and noble's hashes: hex in, hex out; "" for an algorithm
/// that isn't here.
fn digest(alg: String, data: String) -> String {
    use sha2::Digest;
    let bytes: Vec<u8> = (0..data.len() / 2)
        .filter_map(|i| u8::from_str_radix(data.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    match alg.as_str() {
        "SHA-256" => hex(&sha2::Sha256::digest(&bytes)),
        "SHA-384" => hex(&sha2::Sha384::digest(&bytes)),
        "SHA-512" => hex(&sha2::Sha512::digest(&bytes)),
        _ => String::new(),
    }
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

/// A JSON Schema that `v` matches: types, every object key required, array items merged
/// over all elements (a key only some elements have is optional).
pub fn schema_of(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};
    match v {
        Value::Null => json!({ "type": "null" }),
        Value::Bool(_) => json!({ "type": "boolean" }),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({ "type": "integer" }),
        Value::Number(_) => json!({ "type": "number" }),
        Value::String(_) => json!({ "type": "string" }),
        Value::Array(items) => {
            let mut s = json!({ "type": "array" });
            if let Some(merged) = items.iter().map(schema_of).reduce(merge_schemas) {
                s["items"] = merged;
            }
            s
        }
        Value::Object(o) => json!({
            "type": "object",
            "properties": o.iter().map(|(k, v)| (k.clone(), schema_of(v))).collect::<serde_json::Map<_, _>>(),
            "required": o.keys().collect::<Vec<_>>(),
        }),
    }
}

fn merge_schemas(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};
    if a == b {
        return a;
    }
    match (a["type"].as_str(), b["type"].as_str()) {
        (Some("object"), Some("object")) => {
            let (mut props, other) = (a["properties"].clone(), &b["properties"]);
            for (k, v) in other.as_object().into_iter().flatten() {
                props[k] = match props.get(k) {
                    Some(p) => merge_schemas(p.clone(), v.clone()),
                    None => v.clone(),
                };
            }
            let required: Vec<&Value> = (a["required"].as_array().into_iter().flatten())
                .filter(|k| b["required"].as_array().is_some_and(|r| r.contains(k)))
                .collect();
            json!({ "type": "object", "properties": props, "required": required })
        }
        (Some("array"), Some("array")) => match (a.get("items"), b.get("items")) {
            (Some(x), Some(y)) => {
                json!({ "type": "array", "items": merge_schemas(x.clone(), y.clone()) })
            }
            (Some(x), None) | (None, Some(x)) => json!({ "type": "array", "items": x }),
            (None, None) => json!({ "type": "array" }),
        },
        (Some("integer"), Some("number")) | (Some("number"), Some("integer")) => {
            json!({ "type": "number" })
        }
        // ponytail: mixed types (a value that is sometimes a string, sometimes null) allow
        // anything; an anyOf of the two if that proves too loose.
        _ => json!({}),
    }
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
    shift: usize,
) -> String {
    if Instant::now() > deadline {
        return format!("script timed out after {:.1} s", timeout.as_secs_f32());
    }
    match e.is_exception() {
        true => thrown(&ctx.catch(), shift),
        false => e.to_string(),
    }
}

/// What a script threw, as "TypeError: not a function (line 3:25)": QuickJS's messages
/// name neither the kind nor the place. The place is the innermost frame in the user's
/// script, the one to look at; `shift` is what the async wrapper added to line 1.
fn thrown(value: &rquickjs::Value<'_>, shift: usize) -> String {
    let Some(x) = value.as_exception() else {
        return match value.as_string().and_then(|s| s.to_string().ok()) {
            Some(s) => s,
            None => "script failed".into(),
        };
    };
    let message = x.message().unwrap_or_default();
    let text = match x.get::<_, String>("name") {
        Ok(name) if !name.is_empty() => format!("{name}: {message}"),
        _ => message,
    };
    let stack = x.stack().unwrap_or_default();
    let Some(at) = FRAME.captures(&stack) else {
        return text;
    };
    let (line, column) = (&at[1], at[2].parse::<usize>().unwrap_or_default());
    // QuickJS counts columns from 0 on line 1 and from 1 on the others.
    let column = match line {
        "1" => (column + 1).saturating_sub(shift),
        _ => column,
    };
    format!("{text} (line {line}:{column})")
}

static FRAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(&format!(r"\b{FILE}:(\d+):(\d+)")).expect("valid")
});

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> WireRequest {
        WireRequest {
            method: "GET".into(),
            url: "https://{{host}}/users".into(),
            headers: vec![],
            body: None,
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

    /// The Assert table: each operator Bruno offers must pass on what matches and fail on
    /// what doesn't, or a green row would mean nothing.
    #[test]
    fn assert_rows_check_the_response_without_js() {
        let (req, env) = (
            request(),
            HashMap::from([("want".to_owned(), "ann".to_owned())]),
        );
        let headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        let body = r#"{"name":"ann","items":[1,2,3],"none":null,"ok":true,"empty":[],"id":"a-17"}"#;
        let resp = ScriptResponse {
            code: 201,
            status: "Created",
            time: 40,
            headers: &headers,
            body,
        };
        let rows = serde_json::json!([
            ["res.status", "eq 201"],
            ["res.status", "201"],
            ["res.status", "in 200,201"],
            ["res.status", "notIn [400, 500]"],
            ["res.status", "between 200,299"],
            ["res.status", "neq 200"],
            ["res.responseTime", "lt 1000"],
            ["res.responseTime", "gte 40"],
            ["res.headers['content-type']", "contains json"],
            ["res.body.name", "eq {{want}}"],
            ["res.body.name", "eq 'ann'"],
            ["res.body.items", "length 3"],
            ["res.body.items", "contains 2"],
            ["res.body.items.length", "gt 2"],
            ["res.body.id", "matches /^a-\\d+$/"],
            ["res.body.id", "startsWith a-"],
            ["res.body.id", "endsWith 17"],
            ["res.body.none", "isNull"],
            ["res.body.missing", "isUndefined"],
            ["res.body.ok", "isBoolean"],
            ["res.body.empty", "isEmpty"],
            ["res.body", "isJson"],
            ["res.body.items", "isArray"],
            // These must fail.
            ["res.status", "eq 200"],
            ["res.body.items", "length 2"],
            ["res.body.name", "isNumber"],
            ["res.body.id", "notMatches ^a"],
            ["res.body.nope.deeper", "isDefined"],
            ["res.body.id", "startsWith 17"],
            ["res.body.id", "endsWith a-"],
        ]);
        let out = run(
            &format!("__asserts({rows});"),
            &input(&req, &env, Some(resp)),
        );
        assert_eq!(out.error, None);
        let failed: Vec<_> = out
            .tests
            .iter()
            .filter(|t| !t.passed)
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(
            failed,
            [
                "res.status eq 200",
                "res.body.items length 2",
                "res.body.name isNumber",
                "res.body.id notMatches ^a",
                "res.body.nope.deeper isDefined",
                "res.body.id startsWith 17",
                "res.body.id endsWith a-",
            ],
            "{:?}",
            out.tests
        );
        assert_eq!(out.tests.len(), 30);
        assert!(
            out.tests[23]
                .error
                .as_deref()
                .unwrap()
                .contains("expected 201 to deeply equal 200")
        );
    }

    /// Postman scripts sign requests with crypto-js and shape data with lodash and moment;
    /// copied in, they must run as they did there.
    #[test]
    fn scripts_require_postmans_libraries() {
        let (req, env) = (request(), HashMap::new());
        let out = run(
            r#"
            var CryptoJS = require("crypto-js");
            console.log(CryptoJS.HmacSHA256("data", "key").toString());
            console.log(CryptoJS.enc.Base64.stringify(CryptoJS.SHA256("abc")));
            console.log(require("lodash").chunk([1, 2, 3], 2));
            console.log(require("moment").utc(0).add(1, "day").format("YYYY-MM-DD"));
            console.log(/^[0-9a-f-]{36}$/.test(require("uuid").v4()));
            var tv4 = require("tv4");
            console.log(tv4.validate({ a: 1 }, { required: ["b"] }), !!tv4.error);
            console.log(btoa("hi:there"), atob("aGk6dGhlcmU="));
            console.log(require("lodash") === require("lodash"));
            require("left-pad");
            "#,
            &input(&req, &env, None),
        );
        assert_eq!(
            out.logs,
            [
                "5031fe3d989c6d1537a013fa6e739da23463fdaec3b70137d828e36ace221bd0",
                "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
                "[[1,2],[3]]",
                "1970-01-02",
                "true",
                "false true",
                "aGk6dGhlcmU= hi:there",
                "true",
            ]
        );
        assert!(out.error.unwrap().contains("no module 'left-pad'"));
    }

    /// Ed25519 request signing as Postman scripts do it: `pm.require` of an npm package,
    /// in an async function. `pm.require` was missing ("not a function"). RFC 8032's
    /// first vector (empty message) checks the signature itself.
    #[test]
    fn pm_require_signs_ed25519_with_tweetnacl() {
        let req = request();
        let env = HashMap::from([
            (
                "seed".to_owned(),
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60".to_owned(),
            ),
            (
                "public".to_owned(),
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a".to_owned(),
            ),
        ]);
        let out = run(
            r#"
            (async function () {
              const imported = pm.require('npm:tweetnacl@1.0.3');
              const nacl = imported.default || imported;
              const hex = k => new Uint8Array(pm.environment.get(k).match(/.{2}/g).map(x => parseInt(x, 16)));
              const pair = nacl.sign.keyPair.fromSeed(hex('seed'));
              const signature = nacl.sign.detached(new Uint8Array(), pair.secretKey);
              pm.variables.set('signature', btoa(String.fromCharCode(...signature)));
              pm.variables.set('same', pair.publicKey.every((b, i) => b === hex('public')[i]));
              pm.variables.set('verified', nacl.sign.detached.verify(new Uint8Array(), signature, pair.publicKey));
            })();
            "#,
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
        assert_eq!(
            out.locals["signature"].as_deref(),
            Some(
                "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQCw=="
            )
        );
        assert_eq!(out.locals["same"].as_deref(), Some("true"));
        assert_eq!(out.locals["verified"].as_deref(), Some("true"));

        // A package that isn't bundled says what is, rather than "not a function".
        let out = run(
            "pm.require('npm:@noble/curves@1.4.0');",
            &input(&req, &env, None),
        );
        let e = out.error.unwrap();
        assert!(
            e.contains("no module '@noble/curves'") && e.contains("@noble/ed25519"),
            "{e}"
        );
    }

    /// The 10-30 VDI script required `@noble/ed25519`, which wasn't bundled. Scripts for it
    /// come in its v2 style (`etc.sha512Sync` from `@noble/hashes`, `randomPrivateKey`) and
    /// its v3 one (`hashes.sha512`, async calls on WebCrypto); both must sign, and the
    /// helpers they use around it (Buffer, TextEncoder, crypto) must be there. RFC 8032's
    /// second vector (message 0x72) checks the signature itself.
    #[test]
    fn noble_ed25519_signs_in_its_v2_and_v3_styles() {
        let req = request();
        let env = HashMap::from([(
            "seed".to_owned(),
            "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb".to_owned(),
        )]);
        let want = "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
                    085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00";
        let v2 = r#"
            const ed = require('@noble/ed25519');
            const { sha512 } = require('@noble/hashes/sha512');
            ed.etc.sha512Sync = (...m) => sha512(ed.etc.concatBytes(...m));
            const seed = Buffer.from(pm.environment.get('seed'), 'hex');
            const sig = ed.sign(new Uint8Array([0x72]), seed);
            pm.variables.set('sig', Buffer.from(sig).toString('hex'));
            pm.variables.set('ok', ed.verify(sig, new Uint8Array([0x72]), ed.getPublicKey(seed)));
            pm.variables.set('fresh', ed.utils.randomPrivateKey().length);
        "#;
        let v3 = r#"
            const ed = pm.require('npm:@noble/ed25519@3.0.0');
            const seed = ed.etc.hexToBytes(pm.environment.get('seed'));
            const sig = await ed.signAsync(Uint8Array.of(0x72), seed);
            pm.variables.set('sig', ed.etc.bytesToHex(sig));
            const pub = await ed.getPublicKeyAsync(seed);
            pm.variables.set('ok', await ed.verifyAsync(sig, Uint8Array.of(0x72), pub));
            pm.variables.set('fresh', (await ed.keygenAsync()).secretKey.length);
        "#;
        for script in [v2, v3] {
            let out = run(script, &input(&req, &env, None));
            assert_eq!(out.error, None, "{script}");
            assert_eq!(out.locals["sig"].as_deref(), Some(want), "{script}");
            assert_eq!(out.locals["ok"].as_deref(), Some("true"));
            assert_eq!(out.locals["fresh"].as_deref(), Some("32"));
        }

        let out = run(
            r#"
            const bytes = new TextEncoder().encode('é€😀');
            console.log(Buffer.from(bytes).toString('hex'), new TextDecoder().decode(bytes));
            console.log(Buffer.from('aGk/Pz4+', 'base64').toString(), Buffer.from('hi??>>').toString('base64url'));
            console.log(Buffer.concat([Buffer.from('a'), Buffer.from([98])]).toString('latin1'));
            const r = crypto.getRandomValues(new Uint8Array(16));
            console.log(r.length, r.some(b => b !== 0));
            crypto.subtle.digest('SHA-256', new TextEncoder().encode('abc'))
              .then(h => console.log(Buffer.from(h).toString('hex')));
            "#,
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
        assert_eq!(
            out.logs,
            [
                "c3a9e282acf09f9880 é€😀",
                "hi??>> aGk_Pz4-",
                "ab",
                "16 true",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ]
        );
    }

    /// `pm.request.body` is what a script signs: it reads the raw body and writes the
    /// signed one back, which is what goes out.
    #[test]
    fn scripts_read_and_rewrite_the_raw_body() {
        let mut req = request();
        req.body = Some(r#"{"Data":"{{data}}"}"#.into());
        let env = HashMap::new();
        let out = run(
            r#"
            const body = JSON.parse(pm.request.body.raw);
            console.log(pm.request.body.mode, body.Data);
            body.Signature = 'c2ln';
            pm.request.body.update(JSON.stringify(body));
            "#,
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
        assert_eq!(out.logs, ["raw {{data}}"]);
        assert_eq!(
            out.request.unwrap().body.as_deref(),
            Some(r#"{"Data":"{{data}}","Signature":"c2ln"}"#)
        );
        // No body: no mode, and one can be set.
        let out = run(
            "console.log(String(pm.request.body.mode)); pm.request.body.raw = {a: 1};",
            &input(&request(), &env, None),
        );
        assert_eq!(out.logs, ["undefined"]);
        assert_eq!(out.request.unwrap().body.as_deref(), Some(r#"{"a":1}"#));
    }

    /// An async function that throws failed nothing: the request went out without the
    /// variables it was to set. And QuickJS's "not a function" named neither the kind nor
    /// the line, and a logged Error read `{}`.
    #[test]
    fn a_rejected_async_function_fails_the_script_with_the_line() {
        let (req, env) = (request(), HashMap::new());
        let async_throw = "(async function () {\n  console.error('failed:', new Error('no key'));\n  pm.nope();\n})();";
        let out = run(async_throw, &input(&req, &env, None));
        assert_eq!(
            out.error.as_deref(),
            Some("TypeError: not a function (line 3:6)")
        );
        let logged = &out.logs[0];
        assert!(logged.starts_with("failed: Error: no key\n"), "{logged}");
        assert!(logged.contains("script:2:"), "{logged}");

        // Columns count from 1 on line 1 too (QuickJS counts them from 0 there only), and
        // inside the top-level-await wrapper still from the script's own start.
        let out = run("pm.nope();", &input(&req, &env, None));
        assert_eq!(
            out.error.as_deref(),
            Some("TypeError: not a function (line 1:4)")
        );
        let out = run("await 1; pm.nope();", &input(&req, &env, None));
        assert_eq!(
            out.error.as_deref(),
            Some("TypeError: not a function (line 1:13)")
        );

        // Handled, even later, is no failure.
        let out = run(
            "var p = Promise.reject(new Error('x')); p.catch(function () {});",
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
        let out = run(
            "(async function () { try { await Promise.reject(1); } catch (e) {} })();",
            &input(&req, &env, None),
        );
        assert_eq!(out.error, None);
    }

    /// The generated schema seeds a contract test: the response it came from must pass,
    /// and one whose shape changed must not.
    #[test]
    fn a_schema_from_a_response_accepts_it_and_rejects_a_changed_shape() {
        let body = serde_json::json!({"items": [{"id": 1, "tag": "a"}, {"id": 2.5}], "next": null});
        let schema = schema_of(&body);
        assert_eq!(
            schema["properties"]["items"]["items"]["required"],
            serde_json::json!(["id"])
        );
        assert_eq!(
            schema["properties"]["items"]["items"]["properties"]["id"]["type"],
            "number"
        );
        let check = |v: serde_json::Value| schema_errors(schema.to_string(), v.to_string());
        assert_eq!(check(body.clone()), "");
        assert_ne!(
            check(serde_json::json!({"items": [{"id": "1"}], "next": null})),
            ""
        );
        assert_ne!(
            check(serde_json::json!({"items": []})),
            "",
            "next went missing"
        );
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
