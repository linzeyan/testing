//! MCP server: lets an LLM client read and edit the workspace and send requests through
//! the same engine as the GUI. Over stdio (`apitool-cli mcp`), or inside the window over
//! HTTP on 127.0.0.1, where it can also operate the window (`apitool-cli mcp --window`
//! relays stdio there). Hand-rolled JSON-RPC: the protocol surface used here (initialize,
//! tools/list, tools/call) is tiny.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::model::{self, Request};
use crate::net::{Clients, Network};
use crate::runner::{self, Info, RunPlan, Vars};
use crate::store::{Node, Workspace};
use crate::stream::Event;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Large bodies would flood the model's context; it can narrow the request instead.
pub const MAX_BODY: usize = 64 * 1024;
const MASK: &str = "•••";

const INSTRUCTIONS: &str = "apitool is a Postman-style API client. Requests are kept in the \
user's workspace, addressed by path like \"users/get user\" (folders separated by '/'); \
folders hold variables, auth and scripts their requests inherit. Values may use {{variables}} \
from the active environment and globals, plus dynamic ones like {{$guid}} and {{$timestamp}}. \
Tests are Postman-compatible JavaScript (pm.test, pm.expect, pm.response.json(), \
pm.response.to.have.jsonSchema).";
const ON_DISK: &str = " Changes show up in the apitool window when it regains focus.";
const IN_WINDOW: &str = " This server runs inside the open apitool window, where the user \
watches: get_window shows what they see (the open request with its unsaved edits, its \
response, a live stream), and open_request, send_in_window and select_environment act there. \
What the other tools change shows in the window at once.";
/// The tools that change the workspace, so the window reloads after them.
const CHANGES: &[&str] = &[
    "save_request",
    "delete",
    "move",
    "save_folder",
    "import",
    "set_variables",
    "send_request",
    "run_collection",
];
/// How long send_in_window waits for a response.
const SEND_TIMEOUT: Duration = Duration::from_secs(120);
/// A stream's events beyond these are counted, not returned.
const MAX_EVENTS: usize = 200;
const MAX_EVENT_TEXT: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The open window, when the server runs inside it: answers its tools by name.
pub type Window = Box<dyn FnMut(&str, &Value) -> Result<Value, String> + Send>;

pub fn serve(ws: Workspace, input: impl BufRead, mut out: impl Write) -> Result<(), String> {
    crate::auth::import(&ws.load_tokens());
    let mut saved = crate::auth::grants();
    let mut server = Server::new(ws, None)?;
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
        // stdout is for JSON-RPC only.
        if crate::auth::grants() != saved {
            saved = crate::auth::grants();
            if let Err(e) = server.ws.save_tokens(&crate::auth::export()) {
                log::warn!("OAuth tokens not kept: {e}");
            }
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
    window: Option<Window>,
}

impl Server {
    fn new(ws: Workspace, window: Option<Window>) -> Result<Self, String> {
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
            window,
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
                let place = if self.window.is_some() {
                    IN_WINDOW
                } else {
                    ON_DISK
                };
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "apitool", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": format!("{INSTRUCTIONS}{place}"),
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools(self.window.is_some()) }),
            "tools/call" => self.call(&msg["params"]),
            _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn call(&mut self, params: &Value) -> Value {
        let args = &params["arguments"];
        let name = params["name"].as_str().unwrap_or_default();
        let result = match name {
            "list_requests" => self.list_requests(),
            "get_request" => self.get_request(args),
            "save_request" => self.save_request(args),
            "delete" => self.delete(args),
            "move" => self.move_item(args),
            "get_folder" => self.get_folder(args),
            "save_folder" => self.save_folder(args),
            "import" => self.import(args),
            "generate_code" => self.generate_code(args),
            "list_history" => self.list_history(args),
            "stream_request" => self.stream_request(args),
            "list_environments" => self.list_environments(),
            "get_environment" => self.get_environment(args),
            "set_variables" => self.set_variables(args),
            "send_request" => self.send_request(args),
            "run_collection" => self.run_collection(args),
            "get_window" | "open_request" | "select_environment" | "send_in_window"
                if self.window.is_some() =>
            {
                self.in_window(name, args)
            }
            other => Err(format!("unknown tool \"{other}\"")),
        };
        // Shown at once, not when the window next gets focus.
        if let Some(window) = &mut self.window
            && CHANGES.contains(&name)
        {
            let _ = window("refresh", &Value::Null);
        }
        let (text, is_error) = match result {
            Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
            Err(e) => (e, true),
        };
        json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
    }

    /// send_in_window presses Send, then waits for the response (or, for a stream, the
    /// `seconds` asked) and answers with the window as it is then.
    fn in_window(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let window = self.window.as_mut().expect("checked by the caller");
        if name != "send_in_window" {
            return window(name, args);
        }
        let sent = window("send", args)?;
        if sent["stream"] == true {
            let seconds = args["seconds"].as_f64().unwrap_or(2.0).clamp(0.1, 60.0);
            std::thread::sleep(Duration::from_secs_f64(seconds));
            return window("get_window", &Value::Null);
        }
        let deadline = Instant::now() + SEND_TIMEOUT;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let state = window("get_window", &Value::Null)?;
            if state["sending"] != true || Instant::now() > deadline {
                return Ok(state);
            }
        }
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

    /// `path` names a request, else a folder.
    fn item(&self, path: &str) -> Result<PathBuf, String> {
        let request = self.ws.request_path(path)?;
        if self.ws.exists(&request) {
            return Ok(request);
        }
        let folder = self.folder_path(path)?;
        match self.ws.exists(&folder) {
            true => Ok(folder),
            false => Err(format!("no request or folder at \"{path}\"")),
        }
    }

    /// A folder's path as typed ("users/admin"), escaped as request paths are; "" is the
    /// top level.
    fn folder_path(&self, name: &str) -> Result<PathBuf, String> {
        let name = name.trim().trim_matches('/');
        if name.is_empty() {
            return Ok(self.ws.collections());
        }
        let inside = self.ws.request_path(&format!("{name}/x"))?;
        Ok(inside.parent().map(PathBuf::from).unwrap_or_default())
    }

    fn delete(&self, args: &Value) -> Result<Value, String> {
        let path = self.item(required(args, "path")?)?;
        self.ws.delete(&path)?;
        Ok(json!({ "deleted": self.ws.display_name(&path) }))
    }

    fn move_item(&self, args: &Value) -> Result<Value, String> {
        let mut path = self.item(required(args, "path")?)?;
        if let Some(to) = args["folder"].as_str() {
            let folder = self.folder_path(to)?;
            if !self.ws.exists(&folder) {
                self.ws.save_folder(&folder, &model::Folder::default())?;
            }
            path = self.ws.move_into(&path, &folder)?;
        }
        if let Some(name) = args["name"].as_str().filter(|n| !n.trim().is_empty()) {
            path = self.ws.rename(&path, name)?;
        }
        Ok(json!({ "path": self.ws.display_name(&path) }))
    }

    fn get_folder(&self, args: &Value) -> Result<Value, String> {
        let dir = self.folder_path(required(args, "path")?)?;
        if !self.ws.exists(&dir) {
            return Err(format!("no folder at \"{}\"", self.ws.display_name(&dir)));
        }
        serde_json::to_value(self.ws.load_folder(&dir)?).map_err(|e| e.to_string())
    }

    fn save_folder(&self, args: &Value) -> Result<Value, String> {
        let dir = self.folder_path(required(args, "path")?)?;
        let mut folder: model::Folder =
            serde_json::from_value(args["folder"].clone()).map_err(|e| format!("folder: {e}"))?;
        let existed = self.ws.exists(&dir);
        // The order is the user's dragging; a model has no reason to know it.
        if existed && folder.order.is_empty() {
            folder.order = self.ws.load_folder(&dir)?.order;
        }
        self.ws.save_folder(&dir, &folder)?;
        Ok(json!({ "path": self.ws.display_name(&dir), "created": !existed }))
    }

    fn import(&self, args: &Value) -> Result<Value, String> {
        let text = match (args["text"].as_str(), args["file"].as_str()) {
            (Some(text), _) if !text.trim().is_empty() => text.to_owned(),
            (_, Some(file)) => std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?,
            _ => return Err("pass `text` (the JSON or YAML) or `file` (its path)".into()),
        };
        let i = crate::import::into_workspace(&self.ws, &text)?;
        Ok(json!({
            "folder": i.folder,
            "requests": i.requests,
            "environments": i.environments,
            "not_carried_over": i.warnings,
        }))
    }

    fn generate_code(&self, args: &Value) -> Result<Value, String> {
        let (_, req) = self.request_arg(args)?;
        let env = self.environment(args)?;
        let mut all = self.all_vars(&req, &self.vars(env.as_deref())?);
        // Secrets stay out of the model's context, as in get_environment: where one is the
        // value in effect, the snippet keeps its {{name}}.
        if args["reveal_secrets"].as_bool() != Some(true) {
            let scopes = env.as_deref().map(Some).into_iter().chain([None]);
            for scope in scopes {
                for kv in self.ws.load_env(scope)?.1 {
                    if all.get(&kv.key) == Some(&kv.value) {
                        all.insert(kv.key.clone(), format!("{{{{{}}}}}", kv.key));
                    }
                }
            }
        }
        let (req, missing) = req.resolved(&all);
        let target = crate::codegen::pick(&req.method, args["target"].as_str().unwrap_or(""));
        let targets = crate::codegen::targets(&req.method);
        let code = crate::codegen::generate(target, req)?;
        let mut out = json!({ "target": target, "code": code, "targets": targets });
        if !missing.is_empty() {
            out["undefined_variables"] = json!(missing);
        }
        Ok(out)
    }

    fn list_history(&self, args: &Value) -> Result<Value, String> {
        let limit = args["limit"].as_u64().unwrap_or(20) as usize;
        let full = args["full"].as_bool() == Some(true);
        let entries = self.ws.load_history();
        let items: Vec<Value> = (entries.into_iter().rev().take(limit))
            .map(|e| {
                let mut item = json!({
                    "at": model::iso8601(Duration::from_secs(e.at)),
                    "path": e.path,
                    "method": e.request.method,
                    "url": e.request.url,
                    "status": (e.status != 0).then_some(e.status),
                    "time_ms": e.ms,
                });
                if full {
                    item["request"] = json!(e.request);
                }
                item
            })
            .collect();
        Ok(Value::Array(items))
    }

    /// Connects a stream (SSE, WebSocket, Socket.IO, MQTT, gRPC streaming, a GraphQL
    /// subscription), sends `messages`, and returns what happened within `seconds`.
    fn stream_request(&mut self, args: &Value) -> Result<Value, String> {
        let (_, req) = self.request_arg(args)?;
        let env = self.environment(args)?;
        let (req, missing) = req.resolved(&self.all_vars(&req, &self.vars(env.as_deref())?));
        let streams = model::is_streaming(&req.method)
            || req.method == "GRPC"
            || crate::stream::subscribes(&req);
        if !streams {
            return Err("not a stream; send it with send_request".into());
        }
        let messages: Vec<String> = (args["messages"].as_array().into_iter().flatten())
            .map(|m| m.as_str().map_or_else(|| m.to_string(), str::to_owned))
            .collect();
        let listens = req.method == "SSE" || req.method == "GRAPHQL";
        if listens && !messages.is_empty() {
            return Err(format!("{} only listens; it sends no messages", req.method));
        }
        let (text_tx, text_rx) = tokio::sync::mpsc::unbounded_channel();
        let (mqtt_tx, mqtt_rx) = tokio::sync::mpsc::unbounded_channel();
        for payload in messages {
            if req.method != "MQTT" {
                let _ = text_tx.send(payload);
                continue;
            }
            let m = &req.mqtt;
            if m.topic.trim().is_empty() || m.topic.contains(['+', '#']) {
                return Err("MQTT publishes to the request's mqtt.topic: set one topic".into());
            }
            let properties = (m.user_properties.iter())
                .filter(|p| p.enabled && !p.key.trim().is_empty())
                .map(|p| (p.key.trim().to_owned(), p.value.clone()))
                .collect();
            let publish = crate::mqtt::Publish {
                topic: m.topic.trim().to_owned(),
                qos: m.qos,
                retain: m.retain,
                payload,
                properties,
            };
            let _ = mqtt_tx.send(crate::mqtt::Command::Publish(publish));
        }
        // gRPC answers a client stream once it's half-closed; the others stay open to listen.
        let text_tx = (req.method != "GRPC").then_some(text_tx);
        let seconds = args["seconds"].as_f64().unwrap_or(5.0).clamp(0.1, 60.0);
        let (client, net) = (self.client()?, self.ws.load_state().network);
        let log: Arc<Mutex<Vec<(Duration, Event)>>> = Arc::default();
        let opened = Arc::new(tokio::sync::Notify::new());
        let (task_log, task_opened, started) = (log.clone(), opened.clone(), Instant::now());
        let mut task = self.rt.spawn(async move {
            let emit = |e| {
                task_log.lock().unwrap().push((started.elapsed(), e));
                task_opened.notify_one();
            };
            crate::stream::connect(&client, net, req, text_rx, mqtt_rx, emit).await
        });
        self.rt.block_on(async {
            // Connecting (the first TLS setup alone can take a second) isn't listening time.
            let mut done = false;
            tokio::select! {
                _ = opened.notified() => {}
                _ = &mut task => done = true,
                _ = tokio::time::sleep(CONNECT_TIMEOUT) => {}
            }
            let listen = Duration::from_secs_f64(seconds);
            if !done && tokio::time::timeout(listen, &mut task).await.is_err() {
                // Dropping the senders closes WebSocket, Socket.IO and MQTT gracefully.
                drop((text_tx, mqtt_tx));
                if tokio::time::timeout(Duration::from_secs(1), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                }
            }
        });
        let log = log.lock().unwrap();
        let (mut events, mut size) = (Vec::new(), 0);
        for (at, event) in log.iter().take(MAX_EVENTS) {
            let item = event_json(*at, event);
            size += item["text"].as_str().map_or(0, str::len);
            if size > MAX_BODY {
                break;
            }
            events.push(item);
        }
        let mut out = json!({ "environment": env, "events": events });
        if log.len() > events.len() {
            out["events_left_out"] = json!(log.len() - events.len());
        }
        if !missing.is_empty() {
            out["undefined_variables"] = json!(missing);
        }
        Ok(out)
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
            "kept": if secret { "secret: this machine only" } else { "shared: exported for git" },
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

    /// The request a tool is about: a saved one by `path`, or an inline `request`.
    fn request_arg(&self, args: &Value) -> Result<(String, Request), String> {
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
        req.sync_params();
        Ok((name, req))
    }

    /// What `req`'s {{variables}} can name: the globals, its folders', the environment's.
    fn all_vars(&self, req: &Request, vars: &Vars) -> HashMap<String, String> {
        let mut all = vars.globals.clone();
        all.extend(req.inherited.vars.clone());
        all.extend(vars.env.clone());
        all
    }

    fn send_request(&mut self, args: &Value) -> Result<Value, String> {
        let (name, req) = self.request_arg(args)?;
        if model::is_streaming(&req.method) || crate::stream::subscribes(&req) {
            return Err("a stream: connect it with stream_request".into());
        }
        let env = self.environment(args)?;
        let vars = self.vars(env.as_deref())?;
        let (_, missing) = req.resolved(&self.all_vars(&req, &vars));
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
                let body = cut(&r.body, MAX_BODY);
                result["status"] = json!(r.status);
                result["reason"] = json!(r.reason);
                result["time_ms"] = json!(r.elapsed.as_millis() as u64);
                result["headers"] = json!(r.headers);
                result["body"] = json!(body);
                if body.len() < r.body.len() {
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

/// A stream event as tools report it, its text cut to `MAX_EVENT_TEXT`.
pub fn event_json(at: Duration, event: &Event) -> Value {
    let (kind, text) = match event {
        Event::Open(t) => ("open", t),
        Event::Info(t) => ("info", t),
        Event::In(t) => ("in", t),
        Event::Out(t) => ("out", t),
        Event::Closed(t) => ("closed", t),
        Event::Error(t) => ("error", t),
    };
    json!({ "ms": at.as_millis() as u64, "kind": kind, "text": cut(text, MAX_EVENT_TEXT) })
}

/// At most `max` bytes of `text`, cut at a character boundary.
pub fn cut(text: &str, max: usize) -> &str {
    let end = (0..=max.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    &text[..end]
}

/// Serves MCP's Streamable HTTP transport at `/mcp` (answering in JSON, one exchange per
/// connection) for the window, until `stop` is set. Any program on this machine can reach
/// 127.0.0.1, so a caller must send `token`; a web page's Origin is refused outright.
pub fn serve_http(
    listener: TcpListener,
    ws: Workspace,
    token: String,
    window: Window,
    stop: Arc<AtomicBool>,
) {
    let mut server = match Server::new(ws, Some(window)) {
        Ok(s) => s,
        Err(e) => return log::warn!("MCP: {e}"),
    };
    for conn in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // ponytail: one exchange at a time; a client sends a request and waits anyway.
        if let Ok(conn) = conn
            && let Err(e) = exchange(&mut server, &token, conn)
        {
            log::warn!("MCP: {e}");
        }
    }
}

fn exchange(server: &mut Server, token: &str, mut conn: TcpStream) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (method, target, headers, body) = read_request(&mut conn)?;
    let header = |name: &str| headers.get(name).map(String::as_str);
    let local = |origin: &str| {
        reqwest::Url::parse(origin)
            .is_ok_and(|u| matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
    };
    let (status, reply) = if header("origin").is_some_and(|o| !local(o)) {
        (403, None)
    } else if header("authorization") != Some(&format!("Bearer {token}")) {
        (401, None)
    } else if target.split('?').next() != Some("/mcp") {
        (404, None)
    } else if method != "POST" {
        // No server-initiated stream (GET) and no sessions to end (DELETE).
        (405, None)
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(msg) => match server.handle(&msg) {
                Some(reply) => (200, Some(reply)),
                None => (202, None),
            },
            Err(e) => (
                400,
                Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
            ),
        }
    };
    let body = reply.map(|r| r.to_string()).unwrap_or_default();
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    write!(
        conn,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nallow: POST\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    conn.flush()
}

type HttpRequest = (String, String, HashMap<String, String>, Vec<u8>);

/// The request line, lowercase headers and the body (by content-length).
fn read_request(conn: &mut TcpStream) -> std::io::Result<HttpRequest> {
    let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = conn.read(&mut chunk)?;
        if n == 0 || buf.len() > 64 * 1024 {
            return Err(bad("no request head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or_default().to_owned();
    let headers: HashMap<String, String> = (lines.filter_map(|l| l.split_once(':')))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > 16 << 20 {
        return Err(bad("body too large"));
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Err(bad("body cut short"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok((method, target, headers, body))
}

/// `apitool-cli mcp --window`: relays stdio to the window's endpoint, so a client that
/// only starts programs (Claude Desktop) can operate it too. The endpoint is looked up for
/// each message: the window takes a new port and token every time it starts.
pub fn relay(ws: &Workspace, input: impl BufRead, mut out: impl Write) -> Result<(), String> {
    for line in input.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|m| m.get("id").cloned());
        let reply = match post(ws, &line) {
            Ok(reply) => reply,
            // A notification gets no answer, not even an error.
            Err(e) => id.map(|id| error(id, -32000, &e).to_string()),
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}").map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn post(ws: &Workspace, message: &str) -> Result<Option<String>, String> {
    let off = "The apitool window isn't open, or Settings > MCP > \"Let MCP clients operate \
               this window\" is off";
    let state = ws.load_state();
    if state.mcp_addr.is_empty() {
        return Err(off.into());
    }
    let io = |e: std::io::Error| e.to_string();
    let mut conn = TcpStream::connect(&state.mcp_addr).map_err(|_| off.to_owned())?;
    // send_in_window waits for a response that long.
    let wait = SEND_TIMEOUT + Duration::from_secs(30);
    conn.set_read_timeout(Some(wait)).map_err(io)?;
    write!(
        conn,
        "POST /mcp HTTP/1.1\r\nhost: {}\r\nauthorization: Bearer {}\r\n\
         content-type: application/json\r\naccept: application/json, text/event-stream\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{message}",
        state.mcp_addr,
        state.mcp_token,
        message.len()
    )
    .map_err(io)?;
    let mut response = String::new();
    conn.read_to_string(&mut response).map_err(io)?;
    let (head, body) = response.split_once("\r\n\r\n").ok_or(off)?;
    match head.split(' ').nth(1) {
        Some("200") => Ok(Some(body.to_owned())),
        Some("202") => Ok(None),
        _ => Err(format!(
            "the apitool window answered {}",
            head.lines().next().unwrap_or_default()
        )),
    }
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing `{key}`"))
}

fn tools(window: bool) -> Value {
    let path = json!({
        "type": "string",
        "description": "Request path inside the workspace, folders separated by '/', e.g. \"users/get user\"",
    });
    let item = json!({
        "type": "string",
        "description": "Path of a request or a folder, e.g. \"users/get user\" or \"users\"",
    });
    let folder_path = json!({
        "type": "string",
        "description": "Folder path, folders separated by '/', e.g. \"users/admin\"",
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
                "description": "GRAPHQL is a POST with a graphql body; GRPC needs proto + rpc; streams (WS, SSE, SOCKETIO, MQTT, gRPC streaming methods, GraphQL subscriptions) run with stream_request",
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
                "description": "One of {\"type\":\"inherit\"} (the default: the auth set in the nearest folder's settings, else none), {\"type\":\"none\"}, {\"type\":\"bearer\",\"token\":\"...\"}, {\"type\":\"basic\",\"username\":\"...\",\"password\":\"...\"}, {\"type\":\"digest\",\"username\":\"...\",\"password\":\"...\"}, {\"type\":\"oauth2\",\"grant\":\"client_credentials\"|\"password\"|\"authorization_code\"|\"implicit\" (both open a browser to sign in; set auth_url, optionally redirect_uri; implicit needs no token_url),\"token_url\":\"...\",\"client_id\":\"...\",\"client_secret\":\"...\",\"scope\":\"...\",\"username\":\"...\",\"password\":\"...\"} (the token is fetched and cached automatically), {\"type\":\"apikey\",\"key\":\"X-API-Key\",\"value\":\"...\",\"in_query\":false} (a header, or a query parameter when in_query is true), {\"type\":\"awsv4\",\"access_key\":\"...\",\"secret_key\":\"...\",\"region\":\"us-east-1\",\"service\":\"execute-api\",\"session_token\":\"\"} (AWS Signature v4, signed on every send)",
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
    let mut list = json!([
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
            "name": "delete",
            "description": "Delete a saved request, or a folder with everything in it.",
            "inputSchema": { "type": "object", "properties": { "path": item }, "required": ["path"] },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "move",
            "description": "Move a request or folder into another folder (created if missing; \"\" is the top level), rename it, or both.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": item,
                    "folder": { "type": "string", "description": "Destination folder" },
                    "name": { "type": "string", "description": "New name" },
                },
                "required": ["path"],
            },
        },
        {
            "name": "get_folder",
            "description": "Read a folder's settings: description, variables, auth and scripts its requests inherit.",
            "inputSchema": { "type": "object", "properties": { "path": folder_path }, "required": ["path"] },
            "annotations": read_only,
        },
        {
            "name": "save_folder",
            "description": "Create a folder, or replace its settings. Requests whose auth is inherit use the folder's; its scripts run before each request's own.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": folder_path,
                    "folder": {
                        "type": "object",
                        "properties": {
                            "description": { "type": "string", "description": "Markdown, for the generated API docs" },
                            "vars": kv,
                            "auth": { "type": "object", "description": "As a request's auth" },
                            "pre_request": { "type": "string" },
                            "tests": { "type": "string" },
                        },
                    },
                },
                "required": ["path", "folder"],
            },
        },
        {
            "name": "import",
            "description": "Import a Postman collection or environment, an Insomnia export, an OpenAPI 3 / Swagger 2 spec or a HAR file, as a new top-level folder (environments as new environments). Nothing existing is replaced.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The JSON or YAML" },
                    "file": { "type": "string", "description": "Or the path of its file" },
                },
            },
        },
        {
            "name": "generate_code",
            "description": "A code snippet that makes the request (variables filled in; secrets left as {{name}} unless reveal_secrets). HTTP has cURL, wget, HTTPie, PowerShell, HTTP, Python, JavaScript, Node.js, Go, Java, C#, PHP, Ruby, Rust, Swift and Kotlin; streams have their own clients. The reply lists the request's targets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path,
                    "request": request,
                    "target": { "type": "string", "description": "e.g. \"cURL\", \"Python (requests)\"; a language alone (\"Go\") picks its client. Default: the first" },
                    "environment": environment,
                    "reveal_secrets": { "type": "boolean" },
                },
            },
            "annotations": read_only,
        },
        {
            "name": "list_history",
            "description": "The requests sent from the apitool window, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "default": 20 },
                    "full": { "type": "boolean", "description": "Include each request in full" },
                },
            },
            "annotations": read_only,
        },
        {
            "name": "stream_request",
            "description": "Connect a stream (SSE, WebSocket, Socket.IO, MQTT, a gRPC streaming method, a GraphQL subscription), send messages, and return the events (open, in, out, info, closed, error) within `seconds`. Scripts don't run on streams.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path,
                    "request": request,
                    "environment": environment,
                    "messages": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Sent once connected: WebSocket text, Socket.IO `event {json}`, gRPC JSON messages (then the stream is half-closed), MQTT payloads published to the request's topic",
                    },
                    "seconds": { "type": "number", "minimum": 0.1, "maximum": 60, "default": 5 },
                },
            },
            "annotations": { "openWorldHint": true },
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
            "description": "Set (or with null, remove) variables in an environment or the globals. Naming a new environment creates it. Shared values are exported with the workspace's files (for git); use secret: true for tokens and passwords.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "environment": environment,
                    "globals": { "type": "boolean", "description": "Write to the globals instead" },
                    "variables": { "type": "object", "additionalProperties": { "type": ["string", "null"] } },
                    "secret": { "type": "boolean", "description": "Keep on this machine only, sealed by the OS where it can; never exported" },
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
    ]);
    if !window {
        return list;
    }
    let in_window = json!([
        {
            "name": "get_window",
            "description": "What the apitool window shows: its tabs, the open request with any unsaved edits, the active environment, whether a send is under way, the open request's response (status, headers, body, test results, console logs) or its live stream's latest events.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "open_request",
            "description": "Open a saved request in a tab of the window. With `request`, the tab shows it as unsaved edits, for the user to look over and save.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path, "request": request },
                "required": ["path"],
            },
        },
        {
            "name": "send_in_window",
            "description": "Press Send (Connect, for a stream) in the window on the open request, or first open `path`; scripts run as for the user. Waits for the response and returns the window as get_window does; a stream is watched for `seconds`. With `message`, sends it on the open request's live stream instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path,
                    "message": { "type": "string", "description": "For a live WebSocket, Socket.IO, MQTT (published to the request's topic) or client-streaming gRPC connection" },
                    "seconds": { "type": "number", "minimum": 0.1, "maximum": 60, "default": 2 },
                },
            },
            "annotations": { "openWorldHint": true },
        },
        {
            "name": "select_environment",
            "description": "Make an environment the window's active one; null for none.",
            "inputSchema": {
                "type": "object",
                "properties": { "environment": { "type": ["string", "null"] } },
                "required": ["environment"],
            },
        },
    ]);
    let all = list.as_array_mut().expect("a list");
    all.extend(in_window.as_array().expect("a list").iter().cloned());
    list
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

    /// The rest of what the window does: import, folders and what they pass down,
    /// reorganising, snippets that keep secrets out, streams and the history.
    #[test]
    fn an_llm_can_organise_import_stream_and_look_back() {
        let dir = std::env::temp_dir().join(format!("apitool-mcp2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::open(dir.clone()).unwrap();
        let mut sent = Request {
            method: "GET".into(),
            url: "http://h.test/sent".into(),
            ..Default::default()
        };
        sent.sync_params();
        let entry = crate::store::HistoryEntry::new("sent".into(), 204, 12, sent);
        ws.append_history(&entry).unwrap();
        // A WebSocket server that echoes, until the client closes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ws_url = format!("ws://{}/chat", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(s).unwrap();
            while let Ok(msg) = socket.read() {
                if msg.is_text() {
                    let echo = format!("echo {}", msg.to_text().unwrap());
                    socket.send(tungstenite::Message::text(echo)).unwrap();
                }
            }
        });
        let collection = json!({
            "info": { "name": "Shop", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json" },
            "item": [{ "name": "list", "request": { "method": "GET", "url": "{{host}}/items" } }],
        });
        let replies = session(
            ws,
            &[
                call(0, "import", json!({ "text": collection.to_string() })),
                call(
                    1,
                    "save_folder",
                    json!({ "path": "Shop", "folder": {
                        "vars": [{ "key": "host", "value": "http://shop.test" }],
                        "auth": { "type": "bearer", "token": "{{token}}" } } }),
                ),
                call(
                    2,
                    "set_variables",
                    json!({ "environment": "dev", "secret": true, "variables": { "token": "s3cret" } }),
                ),
                call(
                    3,
                    "generate_code",
                    json!({ "path": "Shop/list", "target": "Python", "environment": "dev" }),
                ),
                call(
                    4,
                    "generate_code",
                    json!({ "path": "Shop/list", "environment": "dev", "reveal_secrets": true }),
                ),
                call(
                    5,
                    "move",
                    json!({ "path": "Shop/list", "folder": "Archive", "name": "all items" }),
                ),
                call(6, "get_folder", json!({ "path": "Shop" })),
                call(7, "delete", json!({ "path": "Archive" })),
                call(8, "list_requests", json!({})),
                call(
                    9,
                    "stream_request",
                    json!({ "request": { "method": "WS", "url": ws_url },
                            "messages": ["ping"], "seconds": 0.5 }),
                ),
                call(
                    10,
                    "stream_request",
                    json!({ "request": { "method": "GET", "url": "http://h.test" } }),
                ),
                call(11, "list_history", json!({})),
            ],
        );
        let imported = text(&replies[0]);
        assert_eq!(
            (imported["folder"].as_str(), imported["requests"].as_u64()),
            (Some("Shop"), Some(1))
        );
        assert_eq!(
            text(&replies[1])["created"],
            false,
            "the import made the folder"
        );

        // The folder's variable and auth reach its request; the secret stays a name.
        let code = text(&replies[3]);
        assert_eq!(code["target"], "Python (requests)");
        let snippet = code["code"].as_str().unwrap();
        assert!(snippet.contains("http://shop.test/items"), "{snippet}");
        assert!(
            snippet.contains("Bearer {{token}}") && !snippet.contains("s3cret"),
            "{snippet}"
        );
        let revealed = text(&replies[4]);
        assert_eq!(revealed["target"], "cURL");
        assert!(revealed["code"].as_str().unwrap().contains("Bearer s3cret"));

        assert_eq!(text(&replies[5])["path"], "Archive/all items");
        assert_eq!(
            text(&replies[6])["vars"][0]["key"],
            "host",
            "the folder kept its settings"
        );
        assert_eq!(text(&replies[7])["deleted"], "Archive");
        assert_eq!(
            text(&replies[8]),
            json!([]),
            "the folder went with its request"
        );

        let events = text(&replies[9])["events"].clone();
        let kinds: Vec<(&str, &str)> = (events.as_array().unwrap().iter())
            .map(|e| (e["kind"].as_str().unwrap(), e["text"].as_str().unwrap()))
            .collect();
        assert_eq!(
            kinds[..3],
            [("open", "connected"), ("out", "ping"), ("in", "echo ping")],
            "{kinds:?}"
        );
        assert_eq!(
            kinds.last().unwrap().0,
            "closed",
            "closed when the time was up: {kinds:?}"
        );
        assert_eq!(replies[10]["result"]["isError"], true);

        let history = text(&replies[11]);
        assert_eq!(history[0]["url"], "http://h.test/sent");
        assert_eq!(history[0]["status"], 204);
        assert!(history[0].get("request").is_none(), "only with full");
    }
}
