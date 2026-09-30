use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};

use crate::http;
use crate::model::{Auth, Body, KeyValue, METHODS, Request};
use crate::net::{self, Network, ProxyMode};
use crate::store::{Node, State, Workspace};

const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SEND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
/// Lines longer than this are clipped in the viewer; JSON is pretty-printed first so
/// only non-JSON minified bodies hit it.
const MAX_LINE: usize = 4096;
const RED: Color32 = Color32::from_rgb(220, 80, 80);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);

enum Msg {
    Response(PathBuf, Result<http::Response, String>),
    Status(String),
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
}

#[derive(PartialEq, Clone, Copy)]
enum RespTab {
    Body,
    Headers,
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
}

pub struct App {
    ws: Workspace,
    tree: Vec<Node>,
    envs: Vec<String>,
    active_env: Option<String>,
    vars: HashMap<String, String>,
    open: Option<Open>,
    req_tab: ReqTab,
    resp_tab: RespTab,
    response: Option<Result<ResponseView, String>>,
    pending: Option<Pending>,
    status: String,
    dialog: Option<Dialog>,
    env_editor: Option<EnvEditor>,
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
            open: None,
            req_tab: ReqTab::Params,
            resp_tab: RespTab::Body,
            response: None,
            pending: None,
            status: String::new(),
            dialog: None,
            env_editor: None,
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
        let (req, missing) = open.draft.resolved(&self.vars);
        self.status = if missing.is_empty() {
            String::new()
        } else {
            format!("Undefined variables sent as-is: {}", missing.join(", "))
        };
        let (path, tx, ctx) = (open.path.clone(), self.tx.clone(), ctx.clone());
        let task = self.rt.spawn(async move {
            let result = match cell.get_or_init(|| net::build_client(net)).await {
                Ok(client) => http::execute(client.clone(), req).await,
                Err(e) => Err(format!("Network settings: {e}")),
            };
            let _ = tx.send(Msg::Response(path, result));
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
            let (path, result) = match msg {
                Msg::Status(s) => {
                    self.status = s;
                    continue;
                }
                Msg::Response(path, result) => (path, result),
            };
            if self.pending.as_ref().is_some_and(|p| p.path == path) {
                self.pending = None;
            }
            // Only the open request's response is kept: bodies can be MBs and RAM is the constraint.
            if self.open.as_ref().is_some_and(|o| o.path == path) {
                self.response = Some(result.map(into_view));
                self.resp_tab = RespTab::Body;
            }
        }
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
                self.dialog = Some(Dialog::name(NameKind::NewFolder(root), ""));
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
                TreeAction::Open(path) => self.request_open(path),
                TreeAction::Dialog(d) => self.dialog = Some(d),
            }
        }
    }

    fn main_area(&mut self, ui: &mut egui::Ui) {
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
            let (_, missing) = open.draft.resolved(&self.vars);
            if !missing.is_empty() {
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
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.req_tab {
                ReqTab::Params => kv_table(ui, "params", &mut open.draft.params),
                ReqTab::Headers => kv_table(ui, "headers", &mut open.draft.headers),
                ReqTab::Body => body_editor(ui, &mut open.draft.body),
                ReqTab::Auth => auth_editor(ui, &mut open.draft.auth),
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
                Some(Err(e)) => {
                    ui.colored_label(RED, "Request failed");
                    ui.add(egui::Label::new(RichText::new(e).monospace()).selectable(true));
                }
                Some(Ok(view)) => response_ui(ui, view, &mut self.resp_tab),
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

fn response_ui(ui: &mut egui::Ui, view: &ResponseView, tab: &mut RespTab) {
    let h = &view.head;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} {}", h.status, h.reason)).strong().color(status_color(h.status)));
        ui.weak(format!("{} ms", h.elapsed.as_millis()));
        ui.weak(human_size(view.raw_size));
        ui.weak(&h.version);
        ui.separator();
        ui.selectable_value(tab, RespTab::Body, "Body");
        ui.selectable_value(tab, RespTab::Headers, format!("Headers ({})", h.headers.len()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Copy").on_hover_text("Copy body").clicked() {
                ui.ctx().copy_text(view.text.clone());
            }
        });
    });
    ui.separator();
    match tab {
        RespTab::Body => {
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
        RespTab::Headers => {
            egui::ScrollArea::vertical().id_salt("response-headers").auto_shrink(false).show(ui, |ui| {
                egui::Grid::new("resp-headers").num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in &h.headers {
                        ui.add(egui::Label::new(RichText::new(k).strong()).selectable(true));
                        ui.add(egui::Label::new(v.as_str()).selectable(true));
                        ui.end_row();
                    }
                });
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
        "GET" => Color32::from_rgb(80, 180, 100),
        "POST" => ORANGE,
        "PUT" => Color32::from_rgb(70, 140, 230),
        "PATCH" => Color32::from_rgb(170, 110, 220),
        "DELETE" => RED,
        _ => Color32::GRAY,
    }
}

fn status_color(status: u16) -> Color32 {
    match status {
        200..=299 => Color32::from_rgb(80, 180, 100),
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
