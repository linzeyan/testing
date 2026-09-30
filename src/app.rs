use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};

use crate::http;
use crate::model::{Auth, Body, KeyValue, METHODS, Request};
use crate::net::{self, Network, ProxyMode};
use crate::runner::{self, Outcome, RunItem, RunPlan, Vars};
use crate::script::{Changes, TestResult};
use crate::store::{Node, State, Workspace};

const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SEND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
/// Lines longer than this are clipped in the viewer; JSON is pretty-printed first so
/// only non-JSON minified bodies hit it.
const MAX_LINE: usize = 4096;
const RED: Color32 = Color32::from_rgb(220, 80, 80);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);

enum Msg {
    Response(PathBuf, Box<Outcome>),
    Status(String),
    RunItem(u64, RunItem),
    RunDone(u64, Changes, Changes),
}

/// Built lazily on first use (a PAC file may need downloading) and replaced wholesale when
/// network settings change, so in-flight requests keep the client they started with.
type SharedClient = Arc<tokio::sync::OnceCell<Result<reqwest::Client, String>>>;
type Then = Box<dyn FnOnce(&mut App, &egui::Context)>;

#[derive(PartialEq, Clone, Copy)]
enum ReqTab {
    Params,
    Headers,
    Body,
    Auth,
    Scripts,
}

#[derive(PartialEq, Clone, Copy)]
enum ScriptTab {
    Pre,
    Post,
}

#[derive(PartialEq, Clone, Copy)]
enum RespTab {
    Body,
    Headers,
    Tests,
    Console,
}

struct Open {
    path: PathBuf,
    saved: Request,
    draft: Request,
}

impl Open {
    fn name(&self) -> String {
        self.path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    }
    fn dirty(&self) -> bool {
        self.saved != self.draft
    }
}

struct Pending {
    path: PathBuf,
    started: Instant,
    abort: tokio::task::AbortHandle,
}

struct ResponseView {
    head: http::Response,
    /// Body as displayed (pretty-printed when JSON); `head.body` is emptied to avoid a 2nd copy.
    text: String,
    raw_size: usize,
    line_starts: Vec<usize>,
}

/// What the response pane shows for the last run of the open request.
struct Shown {
    result: Result<ResponseView, String>,
    tests: Vec<TestResult>,
    logs: Vec<String>,
}

enum NameKind {
    NewRequest(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    NewEnv,
}

enum Next {
    Open(PathBuf),
    Quit,
}

enum Dialog {
    Name { kind: NameKind, name: String, error: String },
    Delete(PathBuf),
    Unsaved(Next),
}

impl Dialog {
    fn name(kind: NameKind, name: impl Into<String>) -> Self {
        Self::Name { kind, name: name.into(), error: String::new() }
    }
}

struct EnvEditor {
    name: String,
    shared: Vec<KeyValue>,
    secret: Vec<KeyValue>,
    error: String,
    confirm_delete: bool,
}

enum TreeAction {
    Open(PathBuf),
    Dialog(Dialog),
    Run(PathBuf),
}

/// Collection runner pane. Settings persist while the pane is open; results are summaries only.
struct RunnerView {
    scope: PathBuf,
    title: String,
    iterations: usize,
    data_path: String,
    delay_ms: u64,
    only_failures: bool,
    error: String,
    run: Option<RunState>,
}

struct RunState {
    id: u64,
    started: Instant,
    finished: Option<Duration>,
    total: usize,
    items: Vec<RunItem>,
    abort: tokio::task::AbortHandle,
}

impl RunState {
    fn running(&self) -> bool {
        self.finished.is_none()
    }
}

pub struct App {
    ws: Workspace,
    tree: Vec<Node>,
    envs: Vec<String>,
    active_env: Option<String>,
    vars: HashMap<String, String>,
    /// `pm.globals`: session-only, like Postman's globals without a workspace sync.
    globals: HashMap<String, String>,
    open: Option<Open>,
    req_tab: ReqTab,
    script_tab: ScriptTab,
    resp_tab: RespTab,
    response: Option<Shown>,
    pending: Option<Pending>,
    status: String,
    dialog: Option<Dialog>,
    env_editor: Option<EnvEditor>,
    runner: Option<RunnerView>,
    next_run_id: u64,
    network: Network,
    network_editor: Option<Network>,
    allow_close: bool,
    rt: tokio::runtime::Runtime,
    client: SharedClient,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    renderer: String,
}

impl App {
    pub fn new(ws: Workspace, renderer: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = ws.load_state();
        let mut app = Self {
            tree: ws.tree(),
            envs: ws.env_names(),
            ws,
            active_env: None,
            vars: HashMap::new(),
            globals: HashMap::new(),
            open: None,
            req_tab: ReqTab::Params,
            script_tab: ScriptTab::Post,
            resp_tab: RespTab::Body,
            response: None,
            pending: None,
            status: String::new(),
            dialog: None,
            env_editor: None,
            runner: None,
            next_run_id: 0,
            network: state.network,
            network_editor: None,
            allow_close: false,
            // One worker is plenty for interactive use; each tokio worker costs a stack.
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("tokio runtime"),
            client: SharedClient::default(),
            tx,
            rx,
            renderer,
        };
        let env = state.active_env.filter(|e| app.envs.contains(e));
        app.set_env(env);
        if let Some(path) = state.open.filter(|p| p.exists()) {
            app.force_open(path);
        }
        app
    }

    fn save_state(&self) {
        self.ws.save_state(&State {
            active_env: self.active_env.clone(),
            open: self.open.as_ref().map(|o| o.path.clone()),
            network: self.network.clone(),
        });
    }

    fn apply_network(&mut self, network: Network, ctx: &egui::Context) {
        self.network = network;
        self.client = SharedClient::default();
        self.save_state();
        // Build right away so a bad proxy/PAC/cert shows up now, not on the next Send.
        let (cell, net, tx, ctx) = (self.client.clone(), self.network.clone(), self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let msg = match cell.get_or_init(|| net::build_client(net)).await {
                Ok(_) => "Network settings applied".to_owned(),
                Err(e) => format!("Network settings: {e}"),
            };
            let _ = tx.send(Msg::Status(msg));
            ctx.request_repaint();
        });
    }

    fn set_env(&mut self, name: Option<String>) {
        self.vars.clear();
        if let Some(name) = &name {
            let (shared, secret) = self.ws.load_env(name);
            // Secret values override shared ones with the same key.
            for kv in shared.into_iter().chain(secret).filter(|kv| kv.enabled) {
                self.vars.insert(kv.key, kv.value);
            }
        }
        self.active_env = name;
        self.save_state();
    }

    fn reload(&mut self) {
        self.tree = self.ws.tree();
        self.envs = self.ws.env_names();
    }

    fn request_open(&mut self, path: PathBuf) {
        if self.open.as_ref().is_some_and(|o| o.path == path) {
            return;
        }
        if self.open.as_ref().is_some_and(Open::dirty) {
            self.dialog = Some(Dialog::Unsaved(Next::Open(path)));
        } else {
            self.force_open(path);
        }
    }

    fn force_open(&mut self, path: PathBuf) {
        match self.ws.load_request(&path) {
            Ok(req) => {
                self.open = Some(Open { path, saved: req.clone(), draft: req });
                self.response = None;
                self.save_state();
            }
            Err(e) => self.status = e,
        }
    }

    /// Returns false if the save failed, so callers never drop unsaved work.
    fn save(&mut self) -> bool {
        let Some(open) = &mut self.open else { return true };
        match self.ws.save_request(&open.path, &open.draft) {
            Ok(()) => {
                open.saved = open.draft.clone();
                self.status = format!("Saved {}", open.name());
                self.tree = self.ws.tree(); // method badge may have changed
                true
            }
            Err(e) => {
                self.status = e;
                false
            }
        }
    }

    fn send(&mut self, ctx: &egui::Context) {
        let Some(open) = &self.open else { return };
        if self.pending.is_some() {
            return;
        }
        let (cell, net) = (self.client.clone(), self.network.clone());
        let vars = Vars { env: self.vars.clone(), globals: self.globals.clone(), data: HashMap::new() };
        self.status.clear();
        let (path, name, req, tx, ctx) = (open.path.clone(), open.name(), open.draft.clone(), self.tx.clone(), ctx.clone());
        let task = self.rt.spawn(async move {
            let outcome = match cell.get_or_init(|| net::build_client(net)).await {
                Ok(client) => runner::run(client.clone(), &runner::Info::single(name), req, vars).await,
                Err(e) => Outcome::failed(format!("Network settings: {e}")),
            };
            let _ = tx.send(Msg::Response(path, Box::new(outcome)));
            ctx.request_repaint();
        });
        self.pending = Some(Pending { path: open.path.clone(), started: Instant::now(), abort: task.abort_handle() });
    }

    fn cancel(&mut self) {
        if let Some(p) = self.pending.take() {
            p.abort.abort();
            self.status = "Request cancelled".into();
        }
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            let (path, outcome) = match msg {
                Msg::Status(s) => {
                    self.status = s;
                    continue;
                }
                Msg::RunItem(id, item) => {
                    if let Some(run) = self.runner.as_mut().and_then(|r| r.run.as_mut()).filter(|r| r.id == id) {
                        run.items.push(item);
                    }
                    continue;
                }
                Msg::RunDone(id, env, globals) => {
                    // Persist chained variables even if the runner pane was closed meanwhile.
                    self.apply_changes(env, globals);
                    if let Some(run) = self.runner.as_mut().and_then(|r| r.run.as_mut()).filter(|r| r.id == id) {
                        run.finished = Some(run.started.elapsed());
                    }
                    continue;
                }
                Msg::Response(path, outcome) => (path, *outcome),
            };
            if self.pending.as_ref().is_some_and(|p| p.path == path) {
                self.pending = None;
            }
            // Variable writes apply even if the user switched away meanwhile.
            self.apply_changes(outcome.env, outcome.globals);
            if !outcome.tests.is_empty() {
                let passed = outcome.tests.iter().filter(|t| t.passed).count();
                self.status = format!("Tests: {passed}/{} passed", outcome.tests.len());
            }
            // Only the open request's response is kept: bodies can be MBs and RAM is the constraint.
            if self.open.as_ref().is_some_and(|o| o.path == path) {
                let failed = outcome.response.is_err() || outcome.tests.iter().any(|t| !t.passed);
                self.resp_tab = if failed && !outcome.tests.is_empty() { RespTab::Tests } else { RespTab::Body };
                self.response = Some(Shown { result: outcome.response.map(into_view), tests: outcome.tests, logs: outcome.logs });
            }
        }
    }

    /// `pm.environment.set` lands in the gitignored secret file: like Postman's "current
    /// value" it stays on this machine, and it survives restarts so chained tokens keep working.
    fn apply_changes(&mut self, env: Changes, globals: Changes) {
        let mut globals = globals;
        if !env.is_empty() {
            match self.active_env.clone() {
                Some(name) => {
                    let (shared, mut secret) = self.ws.load_env(&name);
                    for (key, value) in &env {
                        secret.retain(|kv| &kv.key != key);
                        if let Some(v) = value {
                            secret.push(KeyValue::new(key.clone(), v.clone()));
                        }
                    }
                    match self.ws.save_env(&name, &shared, &secret) {
                        Ok(()) => self.set_env(Some(name)),
                        Err(e) => self.status = format!("Saving environment: {e}"),
                    }
                }
                None => {
                    self.status = "No environment selected: pm.environment.set was stored as a global".into();
                    globals.extend(env);
                }
            }
        }
        for (key, value) in globals {
            match value {
                Some(v) => self.globals.insert(key, v),
                None => self.globals.remove(&key),
            };
        }
    }

    /// Everything `{{name}}` can resolve to, with the same precedence as the runner.
    fn all_vars(&self) -> HashMap<String, String> {
        let mut all = self.globals.clone();
        all.extend(self.vars.clone());
        all
    }

    fn submit_name(&mut self) {
        let Some(Dialog::Name { kind, name, error }) = &mut self.dialog else { return };
        let name = name.trim().to_owned();
        let result = match kind {
            NameKind::NewRequest(dir) => self.ws.create_request(dir, &name).map(Some),
            NameKind::NewFolder(dir) => self.ws.create_folder(dir, &name).map(|_| None),
            NameKind::NewEnv => self.ws.save_env(&name, &[], &[]).map(|()| None),
            NameKind::Rename(old) => self.ws.rename(old, &name).map(|new| {
                // Keep the open request (and its unsaved draft) pointing at the moved file.
                if let Some(open) = &mut self.open {
                    if open.path == *old {
                        open.path = new.clone();
                    } else if let Ok(rest) = open.path.strip_prefix(&*old) {
                        open.path = new.join(rest);
                    }
                }
                None
            }),
        };
        match result {
            Err(e) => *error = e,
            Ok(created) => {
                let new_env = matches!(kind, NameKind::NewEnv);
                self.dialog = None;
                self.reload();
                self.save_state();
                if new_env {
                    self.set_env(Some(name));
                }
                if let Some(path) = created {
                    self.request_open(path);
                }
            }
        }
    }
}

fn into_view(mut head: http::Response) -> ResponseView {
    let body = std::mem::take(&mut head.body);
    let raw_size = body.len();
    let looks_json = head.is_json() || body.trim_start().starts_with(['{', '[']);
    let text = looks_json.then(|| http::pretty_json(&body)).flatten().unwrap_or(body);
    let line_starts = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .filter(|&i| i < text.len())
        .collect();
    ResponseView { head, text, raw_size, line_starts }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive();
        // Consume shortcuts before widgets see them, so Ctrl+Enter doesn't also insert a newline.
        if ui.input_mut(|i| i.consume_shortcut(&SAVE)) {
            self.save();
        }
        if ui.input_mut(|i| i.consume_shortcut(&SEND)) {
            self.send(ui.ctx());
        }
        if ui.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.open.as_ref().is_some_and(Open::dirty)
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::Unsaved(Next::Quit));
        }

        self.status_bar(ui);
        egui::Panel::left("sidebar").default_size(260.0).show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.main_area(ui));
        self.dialog_ui(ui.ctx());
        self.env_editor_ui(ui.ctx());
        self.network_editor_ui(ui.ctx());
    }
}

impl App {
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let proxy = match self.network.proxy {
                    ProxyMode::System => "System proxy",
                    ProxyMode::None => "No proxy",
                    ProxyMode::Manual => "Manual proxy",
                    ProxyMode::Pac => "PAC",
                };
                let mut label = RichText::new(format!("⚙ {proxy}"));
                if self.network.insecure {
                    label = RichText::new(format!("⚙ {proxy} · TLS verify OFF")).color(RED);
                }
                if ui.small_button(label).on_hover_text("Network settings").clicked() {
                    self.network_editor = Some(self.network.clone());
                }
                ui.separator();
                if let Some(m) = memory_stats::memory_stats() {
                    ui.weak(format!("RAM {:.0} MB", mb(m.physical_mem)))
                        .on_hover_text(format!("private {:.0} MB\n{}", mb(m.virtual_mem), self.renderer));
                    ui.separator();
                }
                ui.weak(self.ws.root.display().to_string());
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(&self.status);
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let label = self.active_env.clone().unwrap_or_else(|| "No environment".into());
            let mut chosen = None;
            egui::ComboBox::from_id_salt("env").selected_text(label).width(140.0).show_ui(ui, |ui| {
                if ui.selectable_label(self.active_env.is_none(), "No environment").clicked() {
                    chosen = Some(None);
                }
                for name in &self.envs {
                    if ui.selectable_label(self.active_env.as_ref() == Some(name), name).clicked() {
                        chosen = Some(Some(name.clone()));
                    }
                }
            });
            if let Some(env) = chosen {
                self.set_env(env);
            }
            if let Some(name) = self.active_env.clone()
                && ui.small_button("Edit").on_hover_text("Edit variables").clicked()
            {
                let (shared, secret) = self.ws.load_env(&name);
                self.env_editor = Some(EnvEditor { name, shared, secret, error: String::new(), confirm_delete: false });
            }
            if ui.small_button("+").on_hover_text("New environment").clicked() {
                self.dialog = Some(Dialog::name(NameKind::NewEnv, ""));
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Collections");
            let root = self.ws.collections();
            if ui.small_button("+ Request").clicked() {
                self.dialog = Some(Dialog::name(NameKind::NewRequest(root.clone()), ""));
            }
            if ui.small_button("+ Folder").clicked() {
                self.dialog = Some(Dialog::name(NameKind::NewFolder(root.clone()), ""));
            }
            if ui.small_button("▶ Run").on_hover_text("Run the whole collection").clicked() {
                self.open_runner(root);
            }
        });
        let mut actions = Vec::new();
        let selected = self.open.as_ref().map(|o| o.path.as_path());
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if self.tree.is_empty() {
                ui.weak("No requests yet. Click \"+ Request\".");
            }
            tree_ui(ui, &self.tree, selected, &mut actions);
        });
        for action in actions {
            match action {
                TreeAction::Open(path) => {
                    if self.runner.as_ref().and_then(|r| r.run.as_ref()).is_some_and(RunState::running) {
                        self.status = "The collection runner is still running; cancel it first.".into();
                        continue;
                    }
                    self.runner = None;
                    self.request_open(path);
                }
                TreeAction::Dialog(d) => self.dialog = Some(d),
                TreeAction::Run(path) => self.open_runner(path),
            }
        }
    }

    fn main_area(&mut self, ui: &mut egui::Ui) {
        if self.runner.is_some() {
            self.runner_ui(ui);
            return;
        }
        let all_vars = self.all_vars();
        let Some(open) = &mut self.open else {
            ui.centered_and_justified(|ui| ui.weak("Select a request on the left, or create one with \"+ Request\"."));
            return;
        };
        let (mut send, mut save, mut cancel) = (false, false, false);
        let pending = self.pending.as_ref().filter(|p| p.path == open.path);

        egui::Panel::top("request").resizable(true).default_size(320.0).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(open.name());
                if open.dirty() {
                    ui.colored_label(ORANGE, "●").on_hover_text("Unsaved changes");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    save = ui
                        .add_enabled(open.dirty(), egui::Button::new("Save"))
                        .on_hover_text(ui.ctx().format_shortcut(&SAVE))
                        .clicked();
                });
            });
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("method")
                    .selected_text(RichText::new(&open.draft.method).color(method_color(&open.draft.method)).strong())
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for m in METHODS {
                            let text = RichText::new(*m).color(method_color(m));
                            ui.selectable_value(&mut open.draft.method, (*m).to_owned(), text);
                        }
                    });
                let button = [80.0, 22.0];
                let url = ui.add(
                    egui::TextEdit::singleline(&mut open.draft.url)
                        .hint_text("https://{{host}}/path")
                        .font(egui::TextStyle::Monospace)
                        .desired_width(ui.available_width() - button[0] - 8.0),
                );
                if url.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    send = true;
                }
                if pending.is_some() {
                    cancel = ui.add_sized(button, egui::Button::new("Cancel")).clicked();
                } else {
                    let label = RichText::new("Send").strong().color(Color32::WHITE);
                    send |= ui
                        .add_sized(button, egui::Button::new(label).fill(Color32::from_rgb(40, 110, 200)))
                        .on_hover_text(ui.ctx().format_shortcut(&SEND))
                        .clicked();
                }
            });
            let (_, missing) = open.draft.resolved(&all_vars);
            // A pre-request script may define them; only warn when nothing could.
            if !missing.is_empty() && open.draft.pre_request.trim().is_empty() {
                let hint = if self.active_env.is_none() { " (no environment selected)" } else { "" };
                ui.colored_label(ORANGE, format!("Undefined: {}{hint}", missing.join(", ")));
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let count = |kv: &[KeyValue]| kv.iter().filter(|p| p.enabled && !p.key.is_empty()).count();
                let tab = |n: usize, name: &str| if n > 0 { format!("{name} ({n})") } else { name.to_owned() };
                ui.selectable_value(&mut self.req_tab, ReqTab::Params, tab(count(&open.draft.params), "Params"));
                ui.selectable_value(&mut self.req_tab, ReqTab::Headers, tab(count(&open.draft.headers), "Headers"));
                let dot = |none: bool, name: &str| if none { name.to_owned() } else { format!("{name} ●") };
                ui.selectable_value(&mut self.req_tab, ReqTab::Body, dot(matches!(open.draft.body, Body::None), "Body"));
                ui.selectable_value(&mut self.req_tab, ReqTab::Auth, dot(matches!(open.draft.auth, Auth::None), "Auth"));
                let no_scripts = open.draft.pre_request.trim().is_empty() && open.draft.tests.trim().is_empty();
                ui.selectable_value(&mut self.req_tab, ReqTab::Scripts, dot(no_scripts, "Scripts"));
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.req_tab {
                ReqTab::Params => kv_table(ui, "params", &mut open.draft.params),
                ReqTab::Headers => kv_table(ui, "headers", &mut open.draft.headers),
                ReqTab::Body => body_editor(ui, &mut open.draft.body),
                ReqTab::Auth => auth_editor(ui, &mut open.draft.auth),
                ReqTab::Scripts => scripts_editor(ui, &mut self.script_tab, &mut open.draft),
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(p) = pending {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Sending… {:.1} s", p.started.elapsed().as_secs_f32()));
                });
                ui.ctx().request_repaint_after(Duration::from_millis(100));
                return;
            }
            match &self.response {
                None => {
                    ui.weak(format!("Press Send or {} to see the response.", ui.ctx().format_shortcut(&SEND)));
                }
                Some(shown) => response_ui(ui, shown, &mut self.resp_tab),
            }
        });

        if save {
            self.save();
        }
        if cancel {
            self.cancel();
        }
        if send {
            self.send(ui.ctx());
        }
    }

    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let open_name = self.open.as_ref().map(Open::name).unwrap_or_default();
        let mut cancel = false;
        let mut then: Option<Then> = None;
        let modal = egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
            ui.set_width(360.0);
            match dialog {
                Dialog::Name { kind, name, error } => {
                    ui.heading(match kind {
                        NameKind::NewRequest(_) => "New request",
                        NameKind::NewFolder(_) => "New folder",
                        NameKind::Rename(_) => "Rename",
                        NameKind::NewEnv => "New environment",
                    });
                    let edit = ui.add(egui::TextEdit::singleline(name).hint_text("Name").desired_width(f32::INFINITY));
                    if ui.memory(|m| m.focused().is_none()) {
                        edit.request_focus();
                    }
                    if !error.is_empty() {
                        ui.colored_label(RED, error.as_str());
                    }
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || enter {
                            then = Some(Box::new(|app, _| app.submit_name()));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Delete(path) => {
                    ui.heading("Delete");
                    let what = if path.is_dir() { "folder and everything in it" } else { "request" };
                    ui.label(format!("Delete {what} \"{}\"?", path.file_stem().unwrap_or_default().to_string_lossy()));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Delete").color(RED)).clicked() {
                            let path = path.clone();
                            then = Some(Box::new(move |app, _| {
                                app.dialog = None;
                                match app.ws.delete(&path) {
                                    Ok(()) if app.open.as_ref().is_some_and(|o| o.path.starts_with(&path)) => {
                                        app.open = None;
                                        app.response = None;
                                        app.save_state();
                                    }
                                    Ok(()) => {}
                                    Err(e) => app.status = e,
                                }
                                app.reload();
                            }));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Unsaved(_) => {
                    ui.heading("Unsaved changes");
                    ui.label(format!("Save changes to \"{open_name}\"?"));
                    ui.horizontal(|ui| {
                        let save = ui.button("Save").clicked();
                        let discard = ui.button("Discard").clicked();
                        cancel = ui.button("Cancel").clicked();
                        if save || discard {
                            then = Some(Box::new(move |app, ctx| {
                                if save && !app.save() {
                                    return; // keep the dialog; the status bar shows why
                                }
                                let Some(Dialog::Unsaved(next)) = app.dialog.take() else { return };
                                match next {
                                    Next::Open(path) => app.force_open(path),
                                    Next::Quit => {
                                        app.allow_close = true;
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                    }
                                }
                            }));
                        }
                    });
                }
            }
        });
        if cancel || modal.should_close() {
            self.dialog = None;
        }
        if let Some(f) = then {
            f(self, ctx);
        }
    }

    fn env_editor_ui(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.env_editor else { return };
        let (mut save, mut delete, mut close) = (false, false, false);
        // Only explicit buttons close this one: a stray click outside must not drop edits.
        egui::Modal::new(egui::Id::new("env-editor")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.heading(format!("Environment: {}", ed.name));
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                ui.label(RichText::new("Shared").strong());
                ui.weak("Saved to environments/<name>.toml and committed to git.");
                kv_table(ui, "env-shared", &mut ed.shared);
                ui.add_space(10.0);
                ui.label(RichText::new("Secret").strong());
                ui.weak("Saved to <name>.secret.toml, which is gitignored. Overrides shared values.");
                kv_table(ui, "env-secret", &mut ed.secret);
            });
            if !ed.error.is_empty() {
                ui.colored_label(RED, ed.error.as_str());
            }
            ui.separator();
            ui.horizontal(|ui| {
                save = ui.button("Save").clicked();
                close = ui.button("Close").clicked();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if ed.confirm_delete { "Click again to delete" } else { "Delete environment" };
                    if ui.button(RichText::new(label).color(RED)).clicked() {
                        delete = ed.confirm_delete;
                        ed.confirm_delete = true;
                    }
                });
            });
        });
        if save {
            match self.ws.save_env(&ed.name, &ed.shared, &ed.secret) {
                Ok(()) => {
                    let name = ed.name.clone();
                    self.env_editor = None;
                    self.set_env(Some(name));
                }
                Err(e) => ed.error = e,
            }
        } else if delete {
            match self.ws.delete_env(&ed.name) {
                Ok(()) => {
                    self.env_editor = None;
                    self.set_env(None);
                    self.reload();
                }
                Err(e) => ed.error = e,
            }
        } else if close {
            self.env_editor = None;
        }
    }
}

impl App {
    fn open_runner(&mut self, scope: PathBuf) {
        if self.runner.as_ref().and_then(|r| r.run.as_ref()).is_some_and(RunState::running) {
            self.status = "The collection runner is already running.".into();
            return;
        }
        let title = if scope == self.ws.collections() {
            "Whole collection".to_owned()
        } else {
            self.display_name(&scope)
        };
        // Keep the previous settings when re-opening, which is how people iterate on a run.
        let prev = self.runner.take();
        self.runner = Some(RunnerView {
            scope,
            title,
            iterations: prev.as_ref().map_or(1, |p| p.iterations),
            data_path: prev.as_ref().map(|p| p.data_path.clone()).unwrap_or_default(),
            delay_ms: prev.as_ref().map_or(0, |p| p.delay_ms),
            only_failures: false,
            error: String::new(),
            run: None,
        });
    }

    /// "Folder/Request" relative to the collections root, without the extension.
    fn display_name(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.ws.collections()).unwrap_or(path);
        rel.with_extension("").to_string_lossy().replace('\\', "/")
    }

    fn start_run(&mut self, ctx: &egui::Context) {
        let Some(view) = &self.runner else { return };
        let mut paths = Vec::new();
        requests_in(&self.tree, &view.scope, &mut paths);
        let mut requests = Vec::new();
        let mut error = String::new();
        for path in &paths {
            match self.ws.load_request(path) {
                Ok(req) => requests.push((self.display_name(path), req)),
                Err(e) => error = e,
            }
        }
        let data = match view.data_path.trim() {
            "" => Ok(Vec::new()),
            p => runner::load_data(Path::new(p)).and_then(|rows| {
                if rows.is_empty() { Err(format!("{p}: no data rows")) } else { Ok(rows) }
            }),
        };
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                error = e;
                Vec::new()
            }
        };
        if requests.is_empty() && error.is_empty() {
            error = "No requests to run here.".into();
        }
        let view = self.runner.as_mut().expect("runner open");
        view.error = error;
        if !view.error.is_empty() {
            return;
        }
        if self.open.as_ref().is_some_and(Open::dirty) {
            self.status = "Note: the runner uses saved files; unsaved edits are not included.".into();
        }
        let count = if data.is_empty() { view.iterations.max(1) } else { data.len() };
        let plan = RunPlan { requests, data, iterations: view.iterations, delay: Duration::from_millis(view.delay_ms) };
        let total = plan.requests.len() * count;

        self.next_run_id += 1;
        let id = self.next_run_id;
        let (cell, net) = (self.client.clone(), self.network.clone());
        let vars = Vars { env: self.vars.clone(), globals: self.globals.clone(), data: HashMap::new() };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let task = self.rt.spawn(async move {
            let client = match cell.get_or_init(|| net::build_client(net)).await {
                Ok(c) => c.clone(),
                Err(e) => {
                    let _ = tx.send(Msg::Status(format!("Network settings: {e}")));
                    let _ = tx.send(Msg::RunDone(id, Changes::new(), Changes::new()));
                    ctx.request_repaint();
                    return;
                }
            };
            let (item_tx, item_ctx) = (tx.clone(), ctx.clone());
            let (env, globals) = runner::run_collection(client, plan, vars, move |item| {
                let _ = item_tx.send(Msg::RunItem(id, item));
                item_ctx.request_repaint();
            })
            .await;
            let _ = tx.send(Msg::RunDone(id, env, globals));
            ctx.request_repaint();
        });
        view.run = Some(RunState { id, started: Instant::now(), finished: None, total, items: Vec::new(), abort: task.abort_handle() });
    }

    fn runner_ui(&mut self, ui: &mut egui::Ui) {
        let (mut start, mut close) = (false, false);
        let Some(view) = &mut self.runner else { return };
        let running = view.run.as_ref().is_some_and(RunState::running);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(format!("Run: {}", view.title));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("Close").clicked();
            });
        });
        ui.add_enabled_ui(!running, |ui| {
            egui::Grid::new("runner-settings").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Data file");
                ui.add(
                    egui::TextEdit::singleline(&mut view.data_path)
                        .hint_text("Optional CSV (with header row) or JSON array; one iteration per row")
                        .desired_width(480.0),
                );
                ui.end_row();
                ui.label("Iterations");
                let has_data = !view.data_path.trim().is_empty();
                ui.add_enabled(!has_data, egui::DragValue::new(&mut view.iterations).range(1..=10_000))
                    .on_disabled_hover_text("Set by the number of data rows");
                ui.end_row();
                ui.label("Delay");
                ui.add(egui::DragValue::new(&mut view.delay_ms).range(0..=60_000).suffix(" ms"));
                ui.end_row();
            });
        });
        if !view.error.is_empty() {
            ui.colored_label(RED, view.error.as_str());
        }
        ui.horizontal(|ui| {
            if running {
                if ui.button("Cancel").clicked() {
                    let run = view.run.as_mut().expect("running");
                    run.abort.abort();
                    run.finished = Some(run.started.elapsed());
                    self.status = "Run cancelled; variable changes from it were discarded.".into();
                }
            } else {
                let label = RichText::new("▶ Run").strong().color(Color32::WHITE);
                start = ui.add(egui::Button::new(label).fill(Color32::from_rgb(40, 110, 200))).clicked();
            }
            ui.checkbox(&mut view.only_failures, "Failures only");
        });
        ui.separator();

        if let Some(run) = &view.run {
            let done = run.items.len();
            let failed = run.items.iter().filter(|i| item_failed(i)).count();
            let (tests_passed, tests_total) = run.items.iter().fold((0, 0), |(p, t), i| {
                (p + i.tests.iter().filter(|x| x.passed).count(), t + i.tests.len())
            });
            let elapsed = run.finished.unwrap_or_else(|| run.started.elapsed());
            ui.add(egui::ProgressBar::new(done as f32 / run.total.max(1) as f32).text(format!("{done}/{}", run.total)));
            ui.horizontal(|ui| {
                ui.label(format!("{:.1} s", elapsed.as_secs_f32()));
                ui.separator();
                ui.colored_label(if failed == 0 { GREEN } else { RED }, format!("{failed} failed requests"));
                ui.separator();
                let color = if tests_passed == tests_total { GREEN } else { RED };
                ui.colored_label(color, format!("Tests {tests_passed}/{tests_total}"));
                if run.finished.is_some() && done < run.total {
                    ui.separator();
                    ui.colored_label(ORANGE, "cancelled");
                }
            });
            if running {
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            ui.separator();
            // ponytail: plain ScrollArea renders every row; switch to show_rows if runs reach tens of thousands.
            egui::ScrollArea::vertical().id_salt("runner-results").auto_shrink(false).stick_to_bottom(running).show(ui, |ui| {
                for item in run.items.iter().filter(|i| !view.only_failures || item_failed(i)) {
                    run_item_ui(ui, item);
                }
            });
        } else {
            ui.weak("Requests run in the order shown on the left, with the selected environment.");
        }

        if close {
            if let Some(run) = self.runner.as_ref().and_then(|r| r.run.as_ref()).filter(|r| r.running()) {
                run.abort.abort();
            }
            self.runner = None;
        } else if start {
            self.start_run(ui.ctx());
        }
    }

    fn network_editor_ui(&mut self, ctx: &egui::Context) {
        let Some(net) = &mut self.network_editor else { return };
        let (mut apply, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::new("network")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading("Network");
            ui.weak("Stored on this machine only (.state.toml), never committed.");
            ui.add_space(6.0);
            let field = |ui: &mut egui::Ui, text: &mut String, hint: &str| {
                ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(f32::INFINITY));
            };

            ui.label(RichText::new("Proxy").strong());
            ui.horizontal(|ui| {
                ui.radio_value(&mut net.proxy, ProxyMode::System, "System");
                ui.radio_value(&mut net.proxy, ProxyMode::None, "None");
                ui.radio_value(&mut net.proxy, ProxyMode::Manual, "Manual");
                ui.radio_value(&mut net.proxy, ProxyMode::Pac, "PAC script");
            });
            match net.proxy {
                ProxyMode::System => match net::system_pac_url() {
                    Some(url) => {
                        ui.weak(format!("Windows is configured with a PAC script, which will be used:\n{url}"));
                    }
                    None => {
                        ui.weak("Uses the OS proxy settings and HTTP(S)_PROXY / NO_PROXY variables.");
                    }
                },
                ProxyMode::None => {
                    ui.weak("Connect directly, ignoring OS settings.");
                }
                ProxyMode::Manual => {
                    field(ui, &mut net.proxy_url, "http://user:pass@proxy.corp:8080");
                    field(ui, &mut net.no_proxy, "Bypass: localhost,127.0.0.1,.corp.local");
                }
                ProxyMode::Pac => {
                    field(ui, &mut net.pac_url, r"http://wpad/proxy.pac  or  C:\path\proxy.pac");
                }
            }

            ui.add_space(8.0);
            ui.label(RichText::new("Certificates").strong());
            ui.weak("The OS certificate store is always trusted; these are added on top.");
            field(ui, &mut net.ca_file, "Extra CA bundle (PEM file path)");
            field(ui, &mut net.client_cert, "Client certificate (.pem with key, or .pfx / .p12)");
            if net.client_cert.to_lowercase().ends_with(".pfx") || net.client_cert.to_lowercase().ends_with(".p12") {
                ui.add(
                    egui::TextEdit::singleline(&mut net.client_cert_password)
                        .password(true)
                        .hint_text("PFX password")
                        .desired_width(f32::INFINITY),
                );
            }
            ui.checkbox(&mut net.insecure, "Skip TLS certificate verification");
            if net.insecure {
                ui.colored_label(RED, "Any server certificate is accepted. Use only to diagnose CA problems.");
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Timeout");
                ui.add(egui::DragValue::new(&mut net.timeout_secs).range(1..=3600).suffix(" s"));
            });
            ui.separator();
            ui.horizontal(|ui| {
                apply = ui.button("Apply").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if apply {
            let net = self.network_editor.take().expect("editor open");
            self.apply_network(net, ctx);
        } else if cancel {
            self.network_editor = None;
        }
    }
}

fn requests_in(nodes: &[Node], scope: &Path, out: &mut Vec<PathBuf>) {
    for node in nodes {
        match node {
            Node::Folder { children, .. } => requests_in(children, scope, out),
            Node::Request { path, .. } if path.starts_with(scope) => out.push(path.clone()),
            Node::Request { .. } => {}
        }
    }
}

fn item_failed(item: &RunItem) -> bool {
    item.status.is_err() || item.tests.iter().any(|t| !t.passed)
}

fn run_item_ui(ui: &mut egui::Ui, item: &RunItem) {
    ui.horizontal(|ui| {
        ui.weak(format!("#{:<3}", item.iteration + 1));
        ui.label(RichText::new(format!("{:<5}", short_method(&item.method))).monospace().small().color(method_color(&item.method)));
        ui.label(item.name.as_str());
        match &item.status {
            Ok((code, ms)) => {
                ui.label(RichText::new(code.to_string()).strong().color(status_color(*code)));
                ui.weak(format!("{ms} ms"));
            }
            Err(_) => {
                ui.colored_label(RED, "ERROR");
            }
        }
        if !item.tests.is_empty() {
            let passed = item.tests.iter().filter(|t| t.passed).count();
            let color = if passed == item.tests.len() { GREEN } else { RED };
            ui.colored_label(color, format!("{passed}/{} tests", item.tests.len()));
        }
    });
    let indent = 28.0;
    if let Err(e) = &item.status {
        ui.horizontal(|ui| {
            ui.add_space(indent);
            ui.add(egui::Label::new(RichText::new(e).monospace().color(RED)).selectable(true));
        });
    }
    for t in item.tests.iter().filter(|t| !t.passed) {
        ui.horizontal(|ui| {
            ui.add_space(indent);
            let text = format!("FAIL {}: {}", t.name, t.error.as_deref().unwrap_or(""));
            ui.add(egui::Label::new(RichText::new(text).monospace().color(RED)).selectable(true));
        });
    }
}

fn tree_ui(ui: &mut egui::Ui, nodes: &[Node], selected: Option<&Path>, actions: &mut Vec<TreeAction>) {
    for node in nodes {
        match node {
            Node::Folder { name, path, children } => {
                let resp = egui::CollapsingHeader::new(name.as_str())
                    .id_salt(path)
                    // Reveal the open request on startup instead of hiding it in a collapsed folder.
                    .default_open(selected.is_some_and(|s| s.starts_with(path)))
                    .show(ui, |ui| tree_ui(ui, children, selected, actions));
                resp.header_response.context_menu(|ui| {
                    let mut item = |label: &str, d: Dialog| {
                        if ui.button(label).clicked() {
                            actions.push(TreeAction::Dialog(d));
                            ui.close();
                        }
                    };
                    item("New request", Dialog::name(NameKind::NewRequest(path.clone()), ""));
                    item("New folder", Dialog::name(NameKind::NewFolder(path.clone()), ""));
                    item("Rename", Dialog::name(NameKind::Rename(path.clone()), name.as_str()));
                    item("Delete", Dialog::Delete(path.clone()));
                    ui.separator();
                    if ui.button("Run folder").clicked() {
                        actions.push(TreeAction::Run(path.clone()));
                        ui.close();
                    }
                });
            }
            Node::Request { name, path, method } => {
                let resp = ui
                    .horizontal(|ui| {
                        let badge = format!("{:<5}", short_method(method));
                        ui.label(RichText::new(badge).monospace().small().color(method_color(method)));
                        ui.selectable_label(selected == Some(path.as_path()), name.as_str())
                    })
                    .inner;
                if resp.clicked() {
                    actions.push(TreeAction::Open(path.clone()));
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        actions.push(TreeAction::Dialog(Dialog::name(NameKind::Rename(path.clone()), name.as_str())));
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        actions.push(TreeAction::Dialog(Dialog::Delete(path.clone())));
                        ui.close();
                    }
                });
            }
        }
    }
}

/// Key/value grid with a trailing blank row: typing into it creates a new row, like Postman.
fn kv_table(ui: &mut egui::Ui, id: &str, rows: &mut Vec<KeyValue>) {
    let key_width = 200.0;
    let value_width = (ui.available_width() - key_width - 90.0).max(120.0);
    let mut remove = None;
    let mut blank = KeyValue::new("", "");
    let existing = rows.len();
    // Plain rows, not egui::Grid: Grid clamps a cell to last frame's column width, so
    // text fields that start narrow stay narrow forever.
    for (i, row) in rows.iter_mut().chain(std::iter::once(&mut blank)).enumerate() {
        ui.horizontal(|ui| {
            let real = i < existing;
            if real {
                ui.checkbox(&mut row.enabled, "");
            } else {
                // Invisible twin keeps the blank row aligned with real rows.
                ui.add_visible(false, egui::Checkbox::new(&mut true, ""));
            }
            // Ids are by row index, so the blank row's editor becomes row `existing` after it's
            // promoted and keeps keyboard focus mid-typing.
            let key = egui::TextEdit::singleline(&mut row.key).id(egui::Id::new((id, i, 0)));
            ui.add(key.hint_text("Key").desired_width(key_width));
            let value = egui::TextEdit::singleline(&mut row.value).id(egui::Id::new((id, i, 1)));
            ui.add(value.hint_text("Value").desired_width(value_width));
            if real && ui.small_button("🗑").on_hover_text("Remove").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        rows.remove(i);
    }
    if !blank.key.is_empty() || !blank.value.is_empty() {
        rows.push(blank);
    }
}

fn body_editor(ui: &mut egui::Ui, body: &mut Body) {
    ui.horizontal(|ui| {
        // ponytail: switching to None/Form drops the text; keep a per-mode stash if that bites.
        // JSON <-> Text keeps the text, since that switch is usually a content-type correction.
        let kind = std::mem::discriminant(&*body);
        let text = match &*body {
            Body::Json { text } | Body::Text { text } => text.clone(),
            _ => String::new(),
        };
        let options = [
            ("None", Body::None),
            ("JSON", Body::Json { text: text.clone() }),
            ("Text", Body::Text { text }),
            ("Form", Body::Form { fields: Vec::new() }),
        ];
        let mut chosen = None;
        for (label, option) in options {
            let current = std::mem::discriminant(&option) == kind;
            if ui.selectable_label(current, label).clicked() && !current {
                chosen = Some(option);
            }
        }
        if let Some(option) = chosen {
            *body = option;
        }
    });
    ui.add_space(4.0);
    match body {
        Body::None => {
            ui.weak("This request has no body.");
        }
        Body::Json { text } => {
            ui.horizontal(|ui| {
                if ui.small_button("Beautify").clicked()
                    && let Some(pretty) = http::pretty_json(text)
                {
                    *text = pretty;
                }
                if !text.trim().is_empty()
                    && let Err(e) = serde_json::from_str::<serde::de::IgnoredAny>(text)
                {
                    // Only a hint: `{{var}}` placeholders legitimately make the raw text invalid.
                    ui.colored_label(ORANGE, format!("Not valid JSON: {e}"));
                }
            });
            code_editor(ui, text);
        }
        Body::Text { text } => code_editor(ui, text),
        Body::Form { fields } => kv_table(ui, "form", fields),
    }
}

fn code_editor(ui: &mut egui::Ui, text: &mut String) {
    ui.add(egui::TextEdit::multiline(text).code_editor().desired_rows(12).desired_width(f32::INFINITY));
}

fn auth_editor(ui: &mut egui::Ui, auth: &mut Auth) {
    let label = match auth {
        Auth::None => "No auth",
        Auth::Bearer { .. } => "Bearer token",
        Auth::Basic { .. } => "Basic auth",
    };
    egui::ComboBox::from_id_salt("auth").selected_text(label).show_ui(ui, |ui| {
        if ui.selectable_label(matches!(auth, Auth::None), "No auth").clicked() {
            *auth = Auth::None;
        }
        if ui.selectable_label(matches!(auth, Auth::Bearer { .. }), "Bearer token").clicked() && !matches!(auth, Auth::Bearer { .. }) {
            *auth = Auth::Bearer { token: String::new() };
        }
        if ui.selectable_label(matches!(auth, Auth::Basic { .. }), "Basic auth").clicked() && !matches!(auth, Auth::Basic { .. }) {
            *auth = Auth::Basic { username: String::new(), password: String::new() };
        }
    });
    ui.add_space(4.0);
    egui::Grid::new("auth-fields").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| match auth {
        Auth::None => {}
        Auth::Bearer { token } => {
            ui.label("Token");
            ui.add(egui::TextEdit::singleline(token).hint_text("{{token}}").desired_width(420.0));
            ui.end_row();
        }
        Auth::Basic { username, password } => {
            ui.label("Username");
            ui.add(egui::TextEdit::singleline(username).desired_width(260.0));
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(password).password(true).desired_width(260.0));
            ui.end_row();
        }
    });
    if !matches!(auth, Auth::None) {
        ui.weak("Tip: use {{variables}} from a secret environment so credentials never reach git.");
    }
}

const PRE_SNIPPETS: &[(&str, &str)] = &[
    ("Set a request header", "pm.request.headers.upsert({ key: \"X-Request-Id\", value: Date.now() });\n"),
    ("Set an environment variable", "pm.environment.set(\"timestamp\", Date.now());\n"),
    ("Log a variable", "console.log(pm.environment.get(\"host\"));\n"),
];

const POST_SNIPPETS: &[(&str, &str)] = &[
    ("Status code is 200", "pm.test(\"Status code is 200\", function () {\n    pm.response.to.have.status(200);\n});\n"),
    ("Response time is below 500 ms", "pm.test(\"Response time is below 500 ms\", function () {\n    pm.expect(pm.response.responseTime).to.be.below(500);\n});\n"),
    ("JSON body has a property", "pm.test(\"Body has id\", function () {\n    const json = pm.response.json();\n    pm.expect(json).to.have.property(\"id\");\n});\n"),
    ("Header is present", "pm.test(\"Content-Type is present\", function () {\n    pm.response.to.have.header(\"Content-Type\");\n});\n"),
    ("Save a JSON value to the environment", "pm.environment.set(\"token\", pm.response.json().token);\n"),
];

fn scripts_editor(ui: &mut egui::Ui, tab: &mut ScriptTab, req: &mut Request) {
    let mut insert = None;
    ui.horizontal(|ui| {
        let label = |name: &str, s: &str| if s.trim().is_empty() { name.to_owned() } else { format!("{name} ●") };
        ui.selectable_value(tab, ScriptTab::Pre, label("Pre-request", &req.pre_request));
        ui.selectable_value(tab, ScriptTab::Post, label("Post-response", &req.tests));
        ui.separator();
        let snippets = if *tab == ScriptTab::Pre { PRE_SNIPPETS } else { POST_SNIPPETS };
        ui.menu_button("Snippets", |ui| {
            for (name, code) in snippets {
                if ui.button(*name).clicked() {
                    insert = Some(*code);
                    ui.close();
                }
            }
        });
    });
    let (text, hint) = match tab {
        ScriptTab::Pre => (&mut req.pre_request, "// Runs before the request is sent.\n// pm.request, pm.environment, pm.variables, console.log"),
        ScriptTab::Post => (&mut req.tests, "// Runs after the response arrives.\n// pm.test(name, fn), pm.expect(...), pm.response.json()"),
    };
    if let Some(code) = insert {
        if !text.is_empty() {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
        }
        text.push_str(code);
    }
    ui.add(egui::TextEdit::multiline(text).code_editor().hint_text(hint).desired_rows(12).desired_width(f32::INFINITY));
}

const GREEN: Color32 = Color32::from_rgb(80, 180, 100);

fn response_ui(ui: &mut egui::Ui, shown: &Shown, tab: &mut RespTab) {
    let passed = shown.tests.iter().filter(|t| t.passed).count();
    ui.horizontal(|ui| {
        match &shown.result {
            Ok(view) => {
                let h = &view.head;
                ui.label(RichText::new(format!("{} {}", h.status, h.reason)).strong().color(status_color(h.status)));
                ui.weak(format!("{} ms", h.elapsed.as_millis()));
                ui.weak(human_size(view.raw_size));
                ui.weak(&h.version);
                ui.separator();
                ui.selectable_value(tab, RespTab::Body, "Body");
                ui.selectable_value(tab, RespTab::Headers, format!("Headers ({})", h.headers.len()));
            }
            Err(_) => {
                ui.colored_label(RED, "Request failed");
                ui.separator();
                ui.selectable_value(tab, RespTab::Body, "Error");
            }
        }
        if !shown.tests.is_empty() {
            let color = if passed == shown.tests.len() { GREEN } else { RED };
            let label = RichText::new(format!("Tests ({passed}/{})", shown.tests.len())).color(color);
            ui.selectable_value(tab, RespTab::Tests, label);
        }
        if !shown.logs.is_empty() {
            ui.selectable_value(tab, RespTab::Console, format!("Console ({})", shown.logs.len()));
        }
        if let Ok(view) = &shown.result {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Copy").on_hover_text("Copy body").clicked() {
                    ui.ctx().copy_text(view.text.clone());
                }
            });
        }
    });
    ui.separator();
    // A tab from the previous run may not exist in this one; fall back to the body.
    let current = match *tab {
        RespTab::Tests if shown.tests.is_empty() => RespTab::Body,
        RespTab::Console if shown.logs.is_empty() => RespTab::Body,
        RespTab::Headers if shown.result.is_err() => RespTab::Body,
        t => t,
    };
    match (current, &shown.result) {
        (RespTab::Tests, _) => {
            egui::ScrollArea::vertical().id_salt("response-tests").auto_shrink(false).show(ui, |ui| {
                for t in &shown.tests {
                    ui.horizontal(|ui| {
                        let (badge, color) = if t.passed { ("PASS", GREEN) } else { ("FAIL", RED) };
                        ui.label(RichText::new(badge).monospace().strong().color(color));
                        ui.add(egui::Label::new(t.name.as_str()).selectable(true));
                    });
                    if let Some(e) = &t.error {
                        ui.add(egui::Label::new(RichText::new(e).monospace().weak()).selectable(true));
                    }
                }
            });
        }
        (RespTab::Console, _) => {
            egui::ScrollArea::vertical().id_salt("response-console").auto_shrink(false).show(ui, |ui| {
                for line in &shown.logs {
                    ui.add(egui::Label::new(RichText::new(line).monospace()).selectable(true));
                }
            });
        }
        (_, Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).monospace()).selectable(true));
        }
        (RespTab::Headers, Ok(view)) => {
            egui::ScrollArea::vertical().id_salt("response-headers").auto_shrink(false).show(ui, |ui| {
                egui::Grid::new("resp-headers").num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in &view.head.headers {
                        ui.add(egui::Label::new(RichText::new(k).strong()).selectable(true));
                        ui.add(egui::Label::new(v.as_str()).selectable(true));
                        ui.end_row();
                    }
                });
            });
        }
        (_, Ok(view)) => {
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both()
                .id_salt("response-body")
                .auto_shrink(false)
                .show_rows(ui, row_height, view.line_starts.len(), |ui, rows| {
                    for row in rows {
                        let start = view.line_starts[row];
                        let end = view.line_starts.get(row + 1).copied().unwrap_or(view.text.len());
                        let mut cut = end.min(start + MAX_LINE);
                        while !view.text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        let line = RichText::new(view.text[start..cut].trim_end()).monospace();
                        ui.add(egui::Label::new(line).extend());
                    }
                });
        }
    }
}

fn short_method(m: &str) -> &str {
    match m {
        "DELETE" => "DEL",
        "OPTIONS" => "OPT",
        m => m,
    }
}

fn method_color(m: &str) -> Color32 {
    match m {
        "GET" => GREEN,
        "POST" => ORANGE,
        "PUT" => Color32::from_rgb(70, 140, 230),
        "PATCH" => Color32::from_rgb(170, 110, 220),
        "DELETE" => RED,
        _ => Color32::GRAY,
    }
}

fn status_color(status: u16) -> Color32 {
    match status {
        200..=299 => GREEN,
        300..=399 => Color32::from_rgb(70, 140, 230),
        400..=499 => ORANGE,
        _ => RED,
    }
}

fn human_size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", mb(bytes)),
    }
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}
