//! MCP server over stdio (`apitool-cli mcp`): lets an LLM client read and edit the
//! workspace and send requests through the same engine as the GUI. Hand-rolled JSON-RPC:
//! the protocol surface used here (initialize, tools/list, tools/call) is tiny.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::model::{self, Request};
use crate::net::{Clients, Network};
use crate::runner::{self, Info, RunPlan, Vars};
use crate::store::{Node, Workspace};

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Large bodies would flood the model's context; it can narrow the request instead.
const MAX_BODY: usize = 64 * 1024;
const MASK: &str = "•••";

const INSTRUCTIONS: &str = "apitool is a Postman-style API client. Requests are files in the \
user's workspace, addressed by path like \"users/get user\" (folders separated by '/'). \
Values may use {{variables}} from the active environment and globals, plus dynamic ones like \
{{$guid}} and {{$timestamp}}. Tests are Postman-compatible JavaScript (pm.test, pm.expect, \
pm.response.json(), pm.response.to.have.jsonSchema). Changes show up in the apitool window \
when it regains focus.";

pub fn serve(ws: Workspace, input: impl BufRead, mut out: impl Write) -> Result<(), String> {
    let mut server = Server::new(ws)?;
    for line in input.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}").map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

struct Server {
    ws: Workspace,
    rt: tokio::runtime::Runtime,
    /// Rebuilt when the network settings in the workspace state change (the GUI edits them).
    client: Option<(Network, Clients)>,
}

impl Server {
    fn new(ws: Workspace) -> Result<Self, String> {
        // Multi-thread: scripts run under block_in_place, which a current-thread runtime forbids.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            ws,
            rt,
            client: None,
        })
    }

    fn handle(&mut self, msg: &Value) -> Option<Value> {
        // Notifications (no id) and the client's replies (no method) get no answer.
        let id = msg.get("id")?.clone();
        let method = msg.get("method")?.as_str().unwrap_or_default();
        let result = match method {
            "initialize" => {
                let asked = msg["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or_default();
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "apitool", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => self.call(&msg["params"]),
            _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn call(&mut self, params: &Value) -> Value {
        let args = &params["arguments"];
        let result = match params["name"].as_str().unwrap_or_default() {
            "list_requests" => self.list_requests(),
            "get_request" => self.get_request(args),
            "save_request" => self.save_request(args),
            "delete_request" => self.delete_request(args),
            "list_environments" => self.list_environments(),
            "get_environment" => self.get_environment(args),
            "set_variables" => self.set_variables(args),
            "send_request" => self.send_request(args),
            "run_collection" => self.run_collection(args),
            other => Err(format!("unknown tool \"{other}\"")),
        };
        let (text, is_error) = match result {
            Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
            Err(e) => (e, true),
        };
        json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
    }

    fn client(&mut self) -> Result<Clients, String> {
        let net = self.ws.load_state().network;
        if let Some((cached, client)) = &self.client
            && *cached == net
        {
            return Ok(client.clone());
        }
        let client = self.rt.block_on(crate::net::build_client(net.clone()))?;
        self.client = Some((net, client.clone()));
        Ok(client)
    }

    /// `environment` names one; omitted means the one selected in the GUI.
    fn environment(&self, args: &Value) -> Result<Option<String>, String> {
        match args["environment"].as_str() {
            Some(name) if self.ws.env_names().iter().any(|n| n == name) => Ok(Some(name.into())),
            Some(name) => Err(format!(
                "unknown environment \"{name}\"; existing: {:?}",
                self.ws.env_names()
            )),
            None => Ok(self
                .ws
                .load_state()
                .active_env
                .filter(|e| self.ws.env_names().contains(e))),
        }
    }

    fn list_requests(&self) -> Result<Value, String> {
        fn walk(ws: &Workspace, nodes: &[Node], out: &mut Vec<Value>) {
            for node in nodes {
                match node {
                    Node::Folder { children, .. } => walk(ws, children, out),
                    Node::Request { path, method, .. } => {
                        let url = ws.load_request(path).map(|r| r.url).unwrap_or_default();
                        out.push(
                            json!({ "path": ws.display_name(path), "method": method, "url": url }),
                        );
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.ws, &self.ws.tree(), &mut out);
        Ok(Value::Array(out))
    }

    fn get_request(&self, args: &Value) -> Result<Value, String> {
        let path = self.ws.request_path(required(args, "path")?)?;
        let req = self.ws.load_request(&path)?;
        serde_json::to_value(req).map_err(|e| e.to_string())
    }

    fn save_request(&self, args: &Value) -> Result<Value, String> {
        let name = required(args, "path")?;
        let path = self.ws.request_path(name)?;
        let mut req: Request =
            serde_json::from_value(args["request"].clone()).map_err(|e| format!("request: {e}"))?;
        req.method = req.method.trim().to_uppercase();
        if !model::METHODS.contains(&req.method.as_str()) {
            return Err(format!("method must be one of {:?}", model::METHODS));
        }
        req.sync_params();
        let existed = self.ws.exists(&path);
        self.ws.save_request(&path, &req)?;
        Ok(json!({ "path": self.ws.display_name(&path), "created": !existed }))
    }

    fn delete_request(&self, args: &Value) -> Result<Value, String> {
        let path = self.ws.request_path(required(args, "path")?)?;
        if !self.ws.exists(&path) {
            return Err(format!("no request at {}", self.ws.display_name(&path)));
        }
        self.ws.delete(&path)?;
        Ok(json!({ "deleted": self.ws.display_name(&path) }))
    }

    fn list_environments(&self) -> Result<Value, String> {
        let globals: Vec<String> = self.ws.env_vars(None)?.into_keys().collect();
        Ok(json!({
            "active": self.environment(&Value::Null)?,
            "environments": self.ws.env_names(),
            "global_variables": globals,
        }))
    }

    fn get_environment(&self, args: &Value) -> Result<Value, String> {
        let env = if args["globals"].as_bool() == Some(true) {
            None
        } else {
            Some(
                self.environment(args)?
                    .ok_or("no environment is active; pass `environment`, or `globals: true`")?,
            )
        };
        let (shared, secret) = self.ws.load_env(env.as_deref())?;
        let reveal = args["reveal_secrets"].as_bool() == Some(true);
        let map = |kvs: Vec<model::KeyValue>, mask: bool| -> serde_json::Map<String, Value> {
            kvs.into_iter()
                .filter(|kv| kv.enabled)
                .map(|kv| {
                    (
                        kv.key,
                        Value::String(if mask { MASK.into() } else { kv.value }),
                    )
                })
                .collect()
        };
        Ok(json!({
            "name": env.unwrap_or_else(|| "globals".into()),
            "shared": map(shared, false),
            "secret": map(secret, !reveal),
        }))
    }

    fn set_variables(&self, args: &Value) -> Result<Value, String> {
        let env =
            if args["globals"].as_bool() == Some(true) {
                None
            } else {
                match args["environment"].as_str() {
                    // Naming a new environment creates it.
                    Some(name) => Some(name.to_owned()),
                    None => Some(self.environment(args)?.ok_or(
                        "no environment is active; pass `environment`, or `globals: true`",
                    )?),
                }
            };
        let vars = args["variables"]
            .as_object()
            .ok_or("`variables` must be an object of name → value (null removes)")?;
        let secret = args["secret"].as_bool() == Some(true);
        let (mut shared, mut secrets) = self.ws.load_env(env.as_deref())?;
        let target = if secret { &mut secrets } else { &mut shared };
        for (key, value) in vars {
            target.retain(|kv| &kv.key != key);
            match value {
                Value::Null => {}
                Value::String(s) => target.push(model::KeyValue::new(key.clone(), s.clone())),
                other => target.push(model::KeyValue::new(key.clone(), other.to_string())),
            }
        }
        self.ws.save_env(env.as_deref(), &shared, &secrets)?;
        Ok(json!({
            "environment": env.unwrap_or_else(|| "globals".into()),
            "updated": vars.keys().collect::<Vec<_>>(),
            "file": if secret { "secret (gitignored)" } else { "shared (committed)" },
        }))
    }

    fn vars(&self, env: Option<&str>) -> Result<Vars, String> {
        Ok(Vars {
            env: match env {
                Some(name) => self.ws.env_vars(Some(name))?,
                None => HashMap::new(),
            },
            globals: self.ws.env_vars(None)?,
            data: HashMap::new(),
        })
    }

    fn send_request(&mut self, args: &Value) -> Result<Value, String> {
        let (name, mut req) = match (args["path"].as_str(), args.get("request")) {
            (_, Some(inline)) if !inline.is_null() => {
                let req: Request =
                    serde_json::from_value(inline.clone()).map_err(|e| format!("request: {e}"))?;
                ("inline request".to_owned(), req)
            }
            (Some(path), _) => {
                let file = self.ws.request_path(path)?;
                (self.ws.display_name(&file), self.ws.load_request(&file)?)
            }
            _ => return Err("pass `path` of a saved request, or an inline `request`".into()),
        };
        req.method = req.method.trim().to_uppercase();
        if model::is_streaming(&req.method) {
            return Err("WebSocket/SSE requests are interactive; use the apitool window".into());
        }
        req.sync_params();
        let env = self.environment(args)?;
        let vars = self.vars(env.as_deref())?;
        let mut all = vars.globals.clone();
        all.extend(req.inherited.vars.clone());
        all.extend(vars.env.clone());
        let (_, missing) = req.resolved(&all);
        let client = self.client()?;
        let out = self
            .rt
            .block_on(runner::run(client, &Info::single(name), req, vars));
        // Same as the GUI: script writes persist, to globals when no environment is active.
        let mut globals = out.globals;
        match &env {
            Some(name) => self.ws.apply_changes(Some(name), &out.env)?,
            None => globals.extend(out.env),
        }
        self.ws.apply_changes(None, &globals)?;

        let mut result = json!({
            "environment": env,
            "tests": out.tests,
            "logs": out.logs,
        });
        if !missing.is_empty() {
            result["undefined_variables"] = json!(missing);
        }
        match out.response {
            Ok(r) => {
                let truncated = r.body.len() > MAX_BODY;
                let mut body = r.body;
                if truncated {
                    let cut = (0..=MAX_BODY)
                        .rev()
                        .find(|i| body.is_char_boundary(*i))
                        .unwrap_or(0);
                    body.truncate(cut);
                }
                result["status"] = json!(r.status);
                result["reason"] = json!(r.reason);
                result["time_ms"] = json!(r.elapsed.as_millis() as u64);
                result["headers"] = json!(r.headers);
                result["body"] = json!(body);
                if truncated {
                    result["body_truncated_at_bytes"] = json!(MAX_BODY);
                }
            }
            Err(e) => result["error"] = json!(e),
        }
        Ok(result)
    }

    fn run_collection(&mut self, args: &Value) -> Result<Value, String> {
        let scope = match args["path"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            None => self.ws.collections(),
            Some(p) => {
                let folder = self.ws.collections().join(p.trim_matches('/'));
                if self.ws.exists(&folder) {
                    folder
                } else {
                    self.ws.request_path(p)?
                }
            }
        };
        let requests = self.ws.load_requests_in(&scope)?;
        let data = match args["data_file"].as_str() {
            Some(p) => runner::load_data(std::path::Path::new(p))?,
            None => Vec::new(),
        };
        let env = self.environment(args)?;
        let plan = RunPlan {
            requests,
            data,
            iterations: args["iterations"].as_u64().unwrap_or(1).max(1) as usize,
            delay: std::time::Duration::ZERO,
        };
        let vars = self.vars(env.as_deref())?;
        let client = self.client()?;
        let mut items = Vec::new();
        let (env_changes, globals) =
            self.rt
                .block_on(runner::run_collection(client, plan, vars, |item| {
                    items.push(item)
                }));
        let mut globals = globals;
        match &env {
            Some(name) => self.ws.apply_changes(Some(name), &env_changes)?,
            None => globals.extend(env_changes),
        }
        self.ws.apply_changes(None, &globals)?;

        let failed = items.iter().filter(|i| i.failed()).count();
        let tests: usize = items.iter().map(|i| i.tests.len()).sum();
        let tests_failed = items
            .iter()
            .flat_map(|i| &i.tests)
            .filter(|t| !t.passed)
            .count();
        let items: Vec<Value> = items
            .into_iter()
            .map(|i| {
                let (status, time_ms, error) = match i.status {
                    Ok((code, ms)) => (json!(code), json!(ms as u64), Value::Null),
                    Err(e) => (Value::Null, Value::Null, json!(e)),
                };
                json!({
                    "iteration": i.iteration + 1,
                    "name": i.name,
                    "method": i.method,
                    "status": status,
                    "time_ms": time_ms,
                    "error": error,
                    "tests": i.tests,
                })
            })
            .collect();
        Ok(json!({
            "environment": env,
            "summary": {
                "requests": items.len(),
                "failed": failed,
                "tests": tests,
                "tests_failed": tests_failed,
            },
            "items": items,
        }))
    }
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing `{key}`"))
}

fn tools() -> Value {
    let path = json!({
        "type": "string",
        "description": "Request path inside the workspace, folders separated by '/', e.g. \"users/get user\"",
    });
    let environment = json!({
        "type": "string",
        "description": "Environment name; omit to use the one selected in the apitool window",
    });
    let kv = json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "value": { "type": "string" },
                "enabled": { "type": "boolean", "default": true },
                "description": { "type": "string", "description": "Documentation only; not sent" },
            },
            "required": ["key", "value"],
        },
    });
    let request = json!({
        "type": "object",
        "description": "A request. Only method and url are required.",
        "properties": {
            "method": {
                "type": "string",
                "enum": model::METHODS,
                "description": "GRAPHQL is a POST with a graphql body; GRPC needs proto + rpc; WS/SSE can be saved but only run in the window",
            },
            "url": { "type": "string", "description": "Full URL including any query string; {{variables}} allowed" },
            "params": { "description": "Query parameters (alternative to writing them into url)", "allOf": [kv] },
            "headers": kv,
            "body": {
                "type": "object",
                "description": "One of {\"type\":\"none\"}, {\"type\":\"json\",\"text\":\"...\"}, {\"type\":\"text\",\"text\":\"...\"}, {\"type\":\"form\",\"fields\":[{\"key\",\"value\"}]}, {\"type\":\"multipart\",\"parts\":[{\"key\",\"value\"}]}, (a value \"@path\" uploads that file), {\"type\":\"file\",\"path\":\"...\"} (the file's bytes as the body), {\"type\":\"graphql\",\"query\":\"...\",\"variables\":\"<JSON text>\"}",
            },
            "auth": {
                "type": "object",
                "description": "One of {\"type\":\"inherit\"} (the default: the auth set in the nearest folder's settings, else none), {\"type\":\"none\"}, {\"type\":\"bearer\",\"token\":\"...\"}, {\"type\":\"basic\",\"username\":\"...\",\"password\":\"...\"}, {\"type\":\"digest\",\"username\":\"...\",\"password\":\"...\"}, {\"type\":\"oauth2\",\"grant\":\"client_credentials\"|\"password\",\"token_url\":\"...\",\"client_id\":\"...\",\"client_secret\":\"...\",\"scope\":\"...\",\"username\":\"...\",\"password\":\"...\"} (the token is fetched and cached automatically), {\"type\":\"apikey\",\"key\":\"X-API-Key\",\"value\":\"...\",\"in_query\":false} (a header, or a query parameter when in_query is true)",
            },
            "pre_request": { "type": "string", "description": "JavaScript run before sending (Postman pm API)" },
            "tests": { "type": "string", "description": "JavaScript run on the response, e.g. pm.test(\"ok\", () => pm.response.to.have.status(200));" },
            "proto": { "type": "string", "description": "gRPC: .proto file path relative to the workspace" },
            "rpc": { "type": "string", "description": "gRPC: package.Service/Method" },
            "settings": {
                "type": "object",
                "description": "HTTP only; omit for the defaults",
                "properties": {
                    "http_version": { "type": "string", "enum": ["auto", "http1", "http2"], "default": "auto" },
                    "follow_redirects": { "type": "boolean", "default": true },
                    "max_redirects": { "type": "integer", "default": 10 },
                    "verify_tls": { "type": "boolean", "default": true },
                    "cookies": { "type": "boolean", "default": true, "description": "Send and store cookies" },
                    "timeout_ms": { "type": "integer", "default": 0, "description": "0 = the network settings' timeout" },
                },
            },
        },
        "required": ["method", "url"],
    });
    let read_only = json!({ "readOnlyHint": true });
    json!([
        {
            "name": "list_requests",
            "description": "List every saved request with its path, method and URL.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "get_request",
            "description": "Read a saved request in full (headers, body, auth, scripts).",
            "inputSchema": { "type": "object", "properties": { "path": path }, "required": ["path"] },
            "annotations": read_only,
        },
        {
            "name": "save_request",
            "description": "Create or overwrite a saved request; missing folders are created.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path, "request": request },
                "required": ["path", "request"],
            },
        },
        {
            "name": "delete_request",
            "description": "Delete a saved request file.",
            "inputSchema": { "type": "object", "properties": { "path": path }, "required": ["path"] },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "list_environments",
            "description": "List environments, the active one, and global variable names.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "get_environment",
            "description": "Read an environment's variables (or the globals). Secret values are masked unless reveal_secrets is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "environment": environment,
                    "globals": { "type": "boolean", "description": "Read the globals instead" },
                    "reveal_secrets": { "type": "boolean" },
                },
            },
            "annotations": read_only,
        },
        {
            "name": "set_variables",
            "description": "Set (or with null, remove) variables in an environment or the globals. Naming a new environment creates it. Shared values are committed to git; use secret: true for tokens and passwords.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "environment": environment,
                    "globals": { "type": "boolean", "description": "Write to the globals instead" },
                    "variables": { "type": "object", "additionalProperties": { "type": ["string", "null"] } },
                    "secret": { "type": "boolean", "description": "Store in the gitignored secret file" },
                },
                "required": ["variables"],
            },
        },
        {
            "name": "send_request",
            "description": "Send a saved request (path) or an inline one, running its pre-request script and tests. Returns status, headers, body, test results and console logs. Variables written by scripts are saved like in the app.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path, "request": request, "environment": environment },
            },
            "annotations": { "openWorldHint": true },
        },
        {
            "name": "run_collection",
            "description": "Run every request under a folder (or the whole workspace when path is omitted) in order, chaining variables, optionally once per row of a CSV/JSON data file. Returns per-request status and test results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder or request path; omit for everything" },
                    "environment": environment,
                    "iterations": { "type": "integer", "minimum": 1 },
                    "data_file": { "type": "string", "description": "CSV or JSON array file, relative to the workspace" },
                },
            },
            "annotations": { "openWorldHint": true },
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds JSON-RPC lines through `serve` and returns the parsed replies.
    fn session(ws: Workspace, messages: &[Value]) -> Vec<Value> {
        let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
        let mut out = Vec::new();
        serve(ws, input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn call(id: u64, name: &str, args: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": name, "arguments": args } })
    }

    fn text(reply: &Value) -> Value {
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into()))
    }

    #[test]
    fn an_llm_can_build_and_run_a_request_end_to_end() {
        let dir = std::env::temp_dir().join(format!("apitool-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::open(dir.clone()).unwrap();
        let url = crate::http::tests::echo_server();
        let host = url.trim_end_matches("/users");
        let replies = session(
            ws,
            &[
                json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                        "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": { "name": "t", "version": "1" } } }),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                call(
                    2,
                    "set_variables",
                    json!({ "environment": "dev",
                        "variables": { "host": host } }),
                ),
                call(
                    3,
                    "set_variables",
                    json!({ "environment": "dev", "secret": true,
                        "variables": { "token": "s3cret" } }),
                ),
                call(
                    4,
                    "save_request",
                    json!({ "path": "users/list", "request": {
                        "method": "get", "url": "{{host}}/users?page=2",
                        "auth": { "type": "bearer", "token": "{{token}}" },
                        "tests": "pm.test('ok', function () { pm.response.to.have.status(200); });\
                                  pm.environment.set('seen', 'yes');" } }),
                ),
                call(5, "list_requests", json!({})),
                call(
                    6,
                    "send_request",
                    json!({ "path": "users/list", "environment": "dev" }),
                ),
                call(7, "get_environment", json!({ "environment": "dev" })),
                call(
                    8,
                    "run_collection",
                    json!({ "path": "users", "environment": "dev", "iterations": 2 }),
                ),
                call(9, "get_request", json!({ "path": "../escape" })),
                json!({ "jsonrpc": "2.0", "id": 10, "method": "nope" }),
            ],
        );
        // The notification gets no reply, so reply i answers id i.
        assert_eq!(replies.len(), 11);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        let names: Vec<_> = replies[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"send_request") && names.contains(&"run_collection"));

        assert_eq!(text(&replies[4])["created"], true);
        assert_eq!(text(&replies[5])[0]["path"], "users/list");
        let sent = text(&replies[6]);
        let wire = sent["body"].as_str().unwrap().to_lowercase();
        assert!(wire.starts_with("get /users?page=2 "), "{wire}");
        assert!(
            wire.contains("authorization: bearer s3cret"),
            "secret resolved server-side"
        );
        assert_eq!(sent["tests"][0]["passed"], true);

        let env = text(&replies[7]);
        assert_eq!(
            env["secret"]["token"], MASK,
            "secrets stay out of the model's context"
        );
        assert_eq!(env["secret"]["seen"], MASK, "script writes were persisted");
        assert_eq!(env["shared"]["host"], host);

        let run = text(&replies[8]);
        assert_eq!(run["summary"]["requests"], 2);
        assert_eq!(run["summary"]["failed"], 0);

        assert_eq!(replies[9]["result"]["isError"], true);
        assert_eq!(replies[10]["error"]["code"], -32601);
    }
}
