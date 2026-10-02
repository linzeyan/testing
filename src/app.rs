use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};

use crate::cookies::Jar;
use crate::graphql::{self, Operation};
use crate::http;
use crate::loadtest::{self, Stats};
use crate::model::{self, Auth, Body, Example, Folder, Inherited, KeyValue, METHODS, Request};
use crate::net::{self, Network, ProxyMode};
use crate::runner::{self, Outcome, RunItem, RunPlan, Vars};
use crate::script::{Changes, TestResult};
use crate::store::{self, HistoryEntry, Node, State, Workspace};
use crate::stream::{self, Event};
use crate::varedit::{clip, var_edit};

const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SEND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
const DUPLICATE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);
const NEW_REQUEST: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
const FOCUS_URL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::L);
const FILTER_HINT: &str = "Filter by name";
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
    Stream(u64, Event),
    /// GraphQL introspection result for the URL it was fetched from.
    Schema(String, Result<graphql::Schema, String>),
}

/// Stream events kept per session; older ones scroll away so a chatty socket can't grow RAM.
const MAX_EVENTS: usize = 5000;

/// Built lazily on first use (a PAC file may need downloading) and replaced wholesale when
/// network settings change, so in-flight requests keep the client they started with.
type SharedClient = Arc<tokio::sync::OnceCell<Result<net::Clients, String>>>;
type Then = Box<dyn FnOnce(&mut App, &egui::Context)>;

#[derive(PartialEq, Clone, Copy)]
enum ReqTab {
    Params,
    Headers,
    Body,
    Auth,
    Scripts,
    Settings,
    Examples,
    Docs,
}

#[derive(PartialEq, Clone, Copy)]
enum FolderTab {
    Vars,
    Auth,
    Scripts,
    Docs,
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
        self.path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
    fn dirty(&self) -> bool {
        self.saved != self.draft
    }
}

/// One request in the tab bar. The active tab's request, response and load test live
/// in `App::open`/`response`/`load`, so the editor code stays single-request.
struct Tab {
    path: PathBuf,
    /// Opened by a plain click in the tree: the next click there reuses this tab instead
    /// of adding one. Editing or double-clicking keeps it.
    preview: bool,
    /// Background tabs only; also `None` for tabs restored at startup and not shown yet.
    // ponytail: background tabs keep their response; drop it on parking if RAM bites.
    parked: Option<Parked>,
}

impl Tab {
    fn new(path: PathBuf, preview: bool) -> Self {
        Self {
            path,
            preview,
            parked: None,
        }
    }
}

struct Parked {
    open: Open,
    response: Option<Shown>,
    load: Option<LoadView>,
}

struct Pending {
    path: PathBuf,
    /// As edited when sent, for history.
    request: Request,
    started: Instant,
    abort: tokio::task::AbortHandle,
}

/// A live (or just ended) WebSocket/SSE connection or gRPC stream for the open request.
struct StreamSession {
    id: u64,
    path: PathBuf,
    started: Instant,
    events: VecDeque<(Duration, Event)>,
    /// WebSocket and client-streaming gRPC; dropping it makes the task send a Close frame
    /// (WebSocket) or half-close (gRPC).
    outgoing: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// gRPC: (proto, method), to check messages before they are sent.
    grpc: Option<(String, String)>,
    live: bool,
    compose: String,
    abort: tokio::task::AbortHandle,
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Stops serving when dropped.
struct MockServer {
    url: String,
    folder: String,
    abort: tokio::task::AbortHandle,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Load-test pane for the open request; replaces the response pane while shown.
struct LoadView {
    vus: usize,
    secs: u64,
    stats: Option<Arc<std::sync::Mutex<Stats>>>,
    abort: Option<tokio::task::AbortHandle>,
}

impl Drop for LoadView {
    fn drop(&mut self) {
        if let Some(a) = &self.abort {
            a.abort();
        }
    }
}

/// GraphQL schema explorer: one schema at a time, remembered across requests because
/// requests to the same API share it.
#[derive(Default)]
struct Explorer {
    url: String,
    schema: Option<Result<graphql::Schema, String>>,
    loading: bool,
    filter: String,
}

struct ResponseView {
    head: http::Response,
    /// Body as displayed (pretty-printed when JSON); `head.body` is emptied to avoid a 2nd copy.
    text: String,
    raw_size: usize,
    line_starts: Vec<usize>,
    find: Find,
}

/// Find-in-body state. Lives in the view, so a new response starts a fresh search.
#[derive(Default)]
struct Find {
    query: String,
    /// The query `hits` were computed for; recomputed only when the query changes.
    searched: String,
    /// Byte offsets of matches in `ResponseView::text`.
    hits: Vec<usize>,
    current: usize,
    /// Scroll the body to `current` on the next frame.
    scroll: bool,
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
    DuplicateEnv(String),
}

enum Next {
    /// With a draft to put in place of the saved version (restoring from history).
    Open(PathBuf, Box<Request>),
    Close(PathBuf),
    Quit,
}

enum Dialog {
    Name {
        kind: NameKind,
        name: String,
        error: String,
    },
    Delete(PathBuf),
    Unsaved(Next),
}

impl Dialog {
    fn name(kind: NameKind, name: impl Into<String>) -> Self {
        Self::Name {
            kind,
            name: name.into(),
            error: String::new(),
        }
    }
}

struct EnvEditor {
    /// `None` edits the workspace-wide globals.
    env: Option<String>,
    shared: Vec<KeyValue>,
    secret: Vec<KeyValue>,
    /// What's on disk, so Close can tell whether it would throw edits away.
    saved: (Vec<KeyValue>, Vec<KeyValue>),
    error: String,
    confirm_delete: bool,
    confirm_discard: bool,
}

impl EnvEditor {
    fn dirty(&self) -> bool {
        (&self.shared, &self.secret) != (&self.saved.0, &self.saved.1)
    }
}

/// Edits a folder's `.folder.toml`.
struct FolderEditor {
    dir: PathBuf,
    name: String,
    folder: Folder,
    saved: Folder,
    /// What the folders above this one pass down.
    parent: Inherited,
    tab: FolderTab,
    error: String,
    confirm_discard: bool,
}

enum TreeAction {
    /// `true` (a double-click) opens a normal tab instead of a preview.
    Open(PathBuf, bool),
    Dialog(Dialog),
    Duplicate(PathBuf),
    Run(PathBuf),
    FolderSettings(PathBuf),
    CopyDocs(PathBuf),
    Mock(PathBuf),
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
    /// Oldest first.
    history: Vec<HistoryEntry>,
    show_history: bool,
    /// Sidebar filter: requests whose name contains it, and the folders leading to them.
    tree_filter: String,
    /// Outlives client rebuilds; saved to the workspace after responses.
    cookies: Arc<Jar>,
    cookie_manager: bool,
    /// The code snippet panel, and its language (kept in the workspace state).
    code: bool,
    code_lang: String,
    mock: Option<MockServer>,
    active_env: Option<String>,
    vars: HashMap<String, String>,
    /// Workspace-wide variables (globals.toml + globals.secret.toml), below any environment.
    globals: HashMap<String, String>,
    quick_look: bool,
    /// Focus to move to on the next frame, once the target widget exists.
    focus_request: Option<egui::Id>,
    open: Option<Open>,
    tabs: Vec<Tab>,
    req_tab: ReqTab,
    script_tab: ScriptTab,
    resp_tab: RespTab,
    response: Option<Shown>,
    /// At most one per request; tabs send independently.
    pending: Vec<Pending>,
    stream: Option<StreamSession>,
    /// Methods of the last `.proto` the gRPC picker looked at; recompiled only on change.
    grpc_methods: Rpcs,
    explorer: Explorer,
    load: Option<LoadView>,
    status: String,
    dialog: Option<Dialog>,
    env_editor: Option<EnvEditor>,
    folder_editor: Option<FolderEditor>,
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
            history: ws.load_history(),
            show_history: false,
            tree_filter: String::new(),
            cookies: Arc::new(Jar::load(&ws.cookies_path())),
            cookie_manager: false,
            code: false,
            code_lang: Some(state.code_lang)
                .filter(|l| crate::codegen::TARGETS.iter().any(|(n, _)| l == n))
                .unwrap_or_else(|| "cURL".into()),
            mock: None,
            ws,
            active_env: None,
            vars: HashMap::new(),
            globals: HashMap::new(),
            quick_look: false,
            focus_request: None,
            open: None,
            tabs: Vec::new(),
            req_tab: ReqTab::Params,
            script_tab: ScriptTab::Post,
            resp_tab: RespTab::Body,
            response: None,
            pending: Vec::new(),
            stream: None,
            grpc_methods: None,
            explorer: Explorer::default(),
            load: None,
            status: String::new(),
            dialog: None,
            env_editor: None,
            folder_editor: None,
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
        let tabs = state.tabs.into_iter().filter(|p| p.exists());
        app.tabs = tabs.map(|p| Tab::new(p, false)).collect();
        if let Some(path) = state.open.filter(|p| p.exists()) {
            app.activate(path, true);
        }
        app
    }

    fn save_state(&self) {
        self.ws.save_state(&State {
            active_env: self.active_env.clone(),
            open: self.open.as_ref().map(|o| o.path.clone()),
            tabs: self.tabs.iter().map(|t| t.path.clone()).collect(),
            network: self.network.clone(),
            code_lang: self.code_lang.clone(),
        });
    }

    fn apply_network(&mut self, network: Network, ctx: &egui::Context) {
        self.network = network;
        self.client = SharedClient::default();
        self.save_state();
        // Build right away so a bad proxy/PAC/cert shows up now, not on the next Send.
        let (cell, net, tx, ctx) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
            self.tx.clone(),
            ctx.clone(),
        );
        self.rt.spawn(async move {
            let msg = match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                Ok(_) => "Network settings applied".to_owned(),
                Err(e) => format!("Network settings: {e}"),
            };
            let _ = tx.send(Msg::Status(msg));
            ctx.request_repaint();
        });
    }

    fn set_env(&mut self, name: Option<String>) {
        self.active_env = name;
        self.reload_vars();
        self.save_state();
    }

    /// Re-reads globals and the active environment from disk. A broken file is shown in
    /// the status bar instead of quietly leaving every variable undefined.
    fn reload_vars(&mut self) {
        let mut load = |env: Option<&str>| {
            self.ws.env_vars(env).unwrap_or_else(|e| {
                self.status = format!("Variables not loaded: {e}");
                HashMap::new()
            })
        };
        let globals = load(None);
        let vars = self
            .active_env
            .clone()
            .map(|n| load(Some(&n)))
            .unwrap_or_default();
        (self.globals, self.vars) = (globals, vars);
    }

    /// `add` appends empty rows for these keys, e.g. the undefined names in the URL.
    fn open_env_editor(&mut self, env: Option<String>, add: &[String]) {
        match self.ws.load_env(env.as_deref()) {
            Ok((mut shared, secret)) => {
                let saved = (shared.clone(), secret.clone());
                for key in add {
                    shared.push(KeyValue::new(key.clone(), ""));
                }
                // Ready to type: the value of the first added row, else the blank row's key.
                let (row, col) = match add.len() {
                    0 => (shared.len(), 0),
                    n => (shared.len() - n, 1),
                };
                self.focus_request = Some(egui::Id::new(("env-shared", row, col)));
                self.env_editor = Some(EnvEditor {
                    env,
                    shared,
                    secret,
                    saved,
                    error: String::new(),
                    confirm_delete: false,
                    confirm_discard: false,
                });
            }
            Err(e) => self.status = e,
        }
    }

    fn reload(&mut self) {
        self.tree = self.ws.tree();
        self.envs = self.ws.env_names();
    }

    /// Picks up edits made outside the window (git pull, an MCP client, an editor) when the
    /// user comes back to it. Unsaved work is never replaced, only flagged.
    fn refresh_from_disk(&mut self) {
        self.reload();
        self.history = self.ws.load_history();
        if self
            .active_env
            .as_ref()
            .is_some_and(|e| !self.envs.contains(e))
        {
            self.active_env = None;
        }
        self.reload_vars();
        self.refresh_open();
    }

    /// The open request against its file: reloaded if it has no unsaved edits, else flagged.
    fn refresh_open(&mut self) {
        self.refresh_inherited();
        let Some(open) = &mut self.open else { return };
        match self.ws.load_request(&open.path) {
            Ok(disk) if disk == open.saved => {}
            Ok(disk) if !open.dirty() => {
                open.saved = disk.clone();
                open.draft = disk;
                self.status = format!("Reloaded {}: changed on disk", open.name());
            }
            Ok(_) => {
                self.status = format!(
                    "{} changed on disk; saving will overwrite that change",
                    open.name()
                )
            }
            Err(_) if !open.path.exists() => {
                self.status = format!("{} was deleted on disk; Save recreates it", open.name())
            }
            Err(e) => self.status = e,
        }
    }

    /// Folder settings apply to the open request at once, unsaved edits or not: they
    /// aren't part of its file.
    fn refresh_inherited(&mut self) {
        let Some(open) = &mut self.open else { return };
        match self.ws.inherited(&open.path) {
            Ok(inherited) => {
                open.saved.inherited = inherited.clone();
                open.draft.inherited = inherited;
            }
            Err(e) => self.status = e,
        }
    }

    fn tab_index(&self, path: &Path) -> Option<usize> {
        self.tabs.iter().position(|t| t.path == path)
    }

    /// Shows `path` in its tab, opening one if needed. A clean preview tab is reused, so
    /// clicking through the tree doesn't pile up tabs; `pin` makes the tab a normal one.
    fn activate(&mut self, path: PathBuf, pin: bool) {
        let current = self.open.as_ref().map(|o| o.path.clone());
        if current.as_ref() != Some(&path) {
            let from = current.as_ref().and_then(|p| self.tab_index(p));
            let parked = self.open.take().map(|open| Parked {
                open,
                response: self.response.take(),
                load: self.load.take(),
            });
            let clean = !parked.as_ref().is_some_and(|p| p.open.dirty());
            let reuse = from.filter(|&i| self.tabs[i].preview && clean);
            let to = match (self.tab_index(&path), reuse) {
                (Some(i), _) => i,
                (None, Some(i)) => {
                    self.tabs[i] = Tab::new(path.clone(), true);
                    i
                }
                (None, None) => {
                    let at = from.map_or(self.tabs.len(), |i| i + 1);
                    self.tabs.insert(at, Tab::new(path.clone(), true));
                    at
                }
            };
            // The tab left behind keeps its draft, response and load test (unless reused).
            if let (Some(f), Some(p)) = (from, parked)
                && self.tabs[f].path == p.open.path
            {
                self.tabs[f].parked = Some(p);
            }
            match self.tabs[to].parked.take() {
                Some(p) => {
                    (self.open, self.response, self.load) = (Some(p.open), p.response, p.load);
                    self.refresh_open();
                    self.save_state();
                }
                None => {
                    self.force_open(path.clone());
                    if self.open.is_none() {
                        // Unreadable file: the status bar says why; go back to where we were.
                        self.tabs.remove(to);
                        if let Some(back) = current.filter(|p| self.tab_index(p).is_some()) {
                            self.activate(back, false);
                        }
                        return;
                    }
                }
            }
        }
        if pin && let Some(i) = self.tab_index(&path) {
            self.tabs[i].preview = false;
        }
    }

    /// Asks first if the tab has unsaved edits, with it brought to the front.
    fn close_tab(&mut self, path: &Path) {
        let Some(i) = self.tab_index(path) else {
            return;
        };
        let dirty = match &self.tabs[i].parked {
            Some(p) => p.open.dirty(),
            None => self
                .open
                .as_ref()
                .is_some_and(|o| o.path == path && o.dirty()),
        };
        if dirty {
            self.activate(path.to_owned(), false);
            self.dialog = Some(Dialog::Unsaved(Next::Close(path.to_owned())));
        } else {
            self.drop_tab(i);
        }
    }

    /// Without asking. The tab to its right (else left) takes over if it was active.
    fn drop_tab(&mut self, i: usize) {
        let tab = self.tabs.remove(i);
        if self.stream.as_ref().is_some_and(|s| s.path == tab.path) {
            self.stream = None;
        }
        if self.open.as_ref().is_some_and(|o| o.path == tab.path) {
            (self.open, self.response, self.load) = (None, None, None);
            let next = self
                .tabs
                .get(i)
                .or(i.checked_sub(1).and_then(|j| self.tabs.get(j)));
            if let Some(next) = next.map(|t| t.path.clone()) {
                self.activate(next, false);
                return;
            }
        }
        self.save_state();
    }

    /// Requests with unsaved edits, in any tab.
    fn unsaved(&self) -> Vec<String> {
        let parked = self.tabs.iter().filter_map(|t| t.parked.as_ref());
        let opens = self.open.iter().chain(parked.map(|p| &p.open));
        opens.filter(|o| o.dirty()).map(Open::name).collect()
    }

    /// Returns false (the status bar says why) if any write failed.
    fn save_all(&mut self) -> bool {
        if self.open.as_ref().is_some_and(Open::dirty) && !self.save() {
            return false;
        }
        for p in self.tabs.iter_mut().filter_map(|t| t.parked.as_mut()) {
            if p.open.dirty() {
                if let Err(e) = self.ws.save_request(&p.open.path, &p.open.draft) {
                    self.status = e;
                    return false;
                }
                p.open.saved = p.open.draft.clone();
            }
        }
        true
    }

    /// The runner pane can't be left mid-run; says so in the status bar.
    fn runner_busy(&mut self) -> bool {
        let runner = self.runner.as_ref().and_then(|r| r.run.as_ref());
        let busy = runner.is_some_and(RunState::running);
        if busy {
            self.status = "The collection runner is still running; cancel it first.".into();
        }
        busy
    }

    /// Puts a request from history in its tab, asking first if that has unsaved edits.
    fn restore(&mut self, path: PathBuf, draft: Box<Request>) {
        self.activate(path.clone(), true);
        if self.open.as_ref().is_some_and(Open::dirty) {
            self.dialog = Some(Dialog::Unsaved(Next::Open(path, draft)));
        } else {
            self.force_open_with(path, draft);
        }
    }

    fn force_open_with(&mut self, path: PathBuf, mut draft: Box<Request>) {
        self.force_open(path.clone());
        if let Some(open) = &mut self.open
            && open.path == path
        {
            // Examples belong to the file, not to one send; folder settings to the folder.
            draft.examples = open.saved.examples.clone();
            draft.inherited = open.saved.inherited.clone();
            open.draft = *draft;
        }
    }

    fn save_cookies(&mut self) {
        if let Err(e) = self.cookies.save(&self.ws.cookies_path()) {
            self.status = e;
        }
    }

    fn record_history(&mut self, sent: Pending, outcome: &Outcome) {
        let (status, elapsed) = match &outcome.response {
            Ok(r) => (r.status, r.elapsed),
            Err(_) => (0, sent.started.elapsed()),
        };
        let path = self.ws.display_name(&sent.path);
        let entry = HistoryEntry::new(path, status, elapsed.as_millis() as u64, sent.request);
        if let Err(e) = self.ws.append_history(&entry) {
            self.status = e;
        }
        self.history.push(entry);
        if self.history.len() > store::MAX_HISTORY {
            self.history.remove(0);
        }
    }

    fn force_open(&mut self, path: PathBuf) {
        match self.ws.load_request(&path) {
            Ok(req) => {
                self.open = Some(Open {
                    path,
                    saved: req.clone(),
                    draft: req,
                });
                self.response = None;
                self.load = None;
                self.save_state();
            }
            Err(e) => self.status = e,
        }
    }

    /// Written to disk at once on top of the last saved version, so unsaved edits in the
    /// draft are neither saved along with it nor lost.
    fn save_example(&mut self, example: Example) {
        let Some(open) = &mut self.open else { return };
        let mut on_disk = open.saved.clone();
        on_disk.examples.push(example.clone());
        match self.ws.save_request(&open.path, &on_disk) {
            Ok(()) => {
                self.status = format!("Saved example \"{}\"", example.name);
                open.saved = on_disk;
                open.draft.examples.push(example);
                self.req_tab = ReqTab::Examples;
            }
            Err(e) => self.status = e,
        }
    }

    /// Returns false if the save failed, so callers never drop unsaved work.
    fn save(&mut self) -> bool {
        let Some(open) = &mut self.open else {
            return true;
        };
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
        if streams(&open.draft, &self.grpc_methods) {
            return self.connect(ctx);
        }
        if self.pending.iter().any(|p| p.path == open.path) {
            return;
        }
        let (cell, net) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
        );
        let vars = Vars {
            env: self.vars.clone(),
            globals: self.globals.clone(),
            data: HashMap::new(),
        };
        self.status.clear();
        let (path, name, req, tx, ctx) = (
            open.path.clone(),
            open.name(),
            open.draft.clone(),
            self.tx.clone(),
            ctx.clone(),
        );
        let task = self.rt.spawn(async move {
            let outcome = match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                Ok(client) => {
                    runner::run(client.clone(), &runner::Info::single(name), req, vars).await
                }
                Err(e) => Outcome::failed(format!("Network settings: {e}")),
            };
            let _ = tx.send(Msg::Response(path, Box::new(outcome)));
            ctx.request_repaint();
        });
        self.pending.push(Pending {
            path: open.path.clone(),
            request: open.draft.clone(),
            started: Instant::now(),
            abort: task.abort_handle(),
        });
    }

    /// Streams skip scripts: pre-request/tests are per-response, a stream has no single response.
    fn connect(&mut self, ctx: &egui::Context) {
        let Some(open) = &self.open else { return };
        self.next_run_id += 1;
        let id = self.next_run_id;
        let (req, _) = open.draft.resolved(&self.all_vars());
        let is_ws = req.method.eq_ignore_ascii_case("WS");
        let grpc = (req.method == "GRPC").then(|| (req.proto.clone(), req.rpc.clone()));
        let sends =
            is_ws || rpc_of(&open.draft, &self.grpc_methods).is_some_and(|r| r.client_streaming);
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cell, net, tx, ctx) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
            self.tx.clone(),
            ctx.clone(),
        );
        let task = self.rt.spawn(async move {
            let emit = |e| {
                let _ = tx.send(Msg::Stream(id, e));
                ctx.request_repaint();
            };
            match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                Ok(client) if is_ws => {
                    stream::websocket(client.http.clone(), req, out_rx, emit).await
                }
                Ok(client) if req.method == "GRPC" => {
                    crate::grpc::stream(client, req, out_rx, emit).await
                }
                Ok(client) => stream::sse(client.http.clone(), req, emit).await,
                Err(e) => emit(Event::Error(format!("Network settings: {e}"))),
            }
        });
        self.stream = Some(StreamSession {
            id,
            path: open.path.clone(),
            started: Instant::now(),
            events: VecDeque::new(),
            outgoing: sends.then_some(out_tx),
            grpc,
            live: true,
            compose: self
                .stream
                .take()
                .map(|s| s.compose.clone())
                .unwrap_or_default(),
            abort: task.abort_handle(),
        });
    }

    fn disconnect(&mut self) {
        let Some(s) = &mut self.stream else { return };
        match s.outgoing.take() {
            // gRPC half-closes and the server may still answer; pressing again cancels.
            Some(_) if s.grpc.is_some() => return,
            Some(_) => {}
            // SSE and server streams have no close handshake; dropping the connection is
            // how clients stop.
            None => {
                s.abort.abort();
                s.events
                    .push_back((s.started.elapsed(), Event::Closed("disconnected".into())));
            }
        }
        s.live = false;
    }

    fn cancel(&mut self) {
        let path = self.open.as_ref().map(|o| &o.path);
        if let Some(i) = self.pending.iter().position(|p| Some(&p.path) == path) {
            self.pending.remove(i).abort.abort();
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
                    if let Some(run) = self
                        .runner
                        .as_mut()
                        .and_then(|r| r.run.as_mut())
                        .filter(|r| r.id == id)
                    {
                        run.items.push(item);
                    }
                    continue;
                }
                Msg::RunDone(id, env, globals) => {
                    // Persist chained variables even if the runner pane was closed meanwhile.
                    self.apply_changes(env, globals);
                    self.save_cookies();
                    if let Some(run) = self
                        .runner
                        .as_mut()
                        .and_then(|r| r.run.as_mut())
                        .filter(|r| r.id == id)
                    {
                        run.finished = Some(run.started.elapsed());
                    }
                    continue;
                }
                Msg::Schema(url, result) => {
                    if self.explorer.url == url {
                        self.explorer.schema = Some(result);
                        self.explorer.loading = false;
                    }
                    continue;
                }
                Msg::Stream(id, event) => {
                    if let Some(s) = self.stream.as_mut().filter(|s| s.id == id) {
                        if matches!(event, Event::Closed(_) | Event::Error(_)) {
                            s.live = false;
                            s.outgoing = None;
                        }
                        if s.events.len() == MAX_EVENTS {
                            s.events.pop_front();
                        }
                        s.events.push_back((s.started.elapsed(), event));
                    }
                    continue;
                }
                Msg::Response(path, outcome) => (path, *outcome),
            };
            if let Some(i) = self.pending.iter().position(|p| p.path == path) {
                let sent = self.pending.remove(i);
                self.record_history(sent, &outcome);
            }
            // Variable writes apply even if the user switched away meanwhile.
            self.apply_changes(outcome.env, outcome.globals);
            self.save_cookies();
            if !outcome.tests.is_empty() {
                let passed = outcome.tests.iter().filter(|t| t.passed).count();
                self.status = format!("Tests: {passed}/{} passed", outcome.tests.len());
            }
            let failed = outcome.response.is_err() || outcome.tests.iter().any(|t| !t.passed);
            let to_tests = failed && !outcome.tests.is_empty();
            let shown = || Shown {
                result: outcome.response.map(into_view),
                tests: outcome.tests,
                logs: outcome.logs,
            };
            // Kept only while the request has a tab: bodies can be MBs and RAM is the constraint.
            if self.open.as_ref().is_some_and(|o| o.path == path) {
                self.resp_tab = if to_tests {
                    RespTab::Tests
                } else {
                    RespTab::Body
                };
                self.response = Some(shown());
            } else if let Some(t) = self.tabs.iter_mut().find(|t| t.path == path)
                && let Some(p) = &mut t.parked
            {
                p.response = Some(shown());
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
                    if let Err(e) = self.ws.apply_changes(Some(&name), &env) {
                        self.status = format!("Saving environment: {e}");
                    }
                }
                None => {
                    self.status =
                        "No environment selected: pm.environment.set was stored as a global".into();
                    globals.extend(env);
                }
            }
        }
        if let Err(e) = self.ws.apply_changes(None, &globals) {
            self.status = format!("Saving globals: {e}");
        }
        if !self.status.starts_with("Saving") {
            self.reload_vars();
        }
    }

    /// Everything `{{name}}` can resolve to, with the same precedence as the runner.
    fn all_vars(&self) -> HashMap<String, String> {
        let mut all = self.globals.clone();
        if let Some(open) = &self.open {
            all.extend(open.draft.inherited.vars.clone());
        }
        all.extend(self.vars.clone());
        all
    }

    fn submit_name(&mut self) {
        let Some(Dialog::Name { kind, name, error }) = &mut self.dialog else {
            return;
        };
        let name = name.trim().to_owned();
        let result = match kind {
            NameKind::NewRequest(dir) => self.ws.create_request(dir, &name).map(Some),
            NameKind::NewFolder(dir) => self.ws.create_folder(dir, &name).map(|_| None),
            // Never overwrite: an existing name would silently wipe that environment.
            NameKind::NewEnv | NameKind::DuplicateEnv(_) if self.envs.contains(&name) => {
                Err(format!("Environment \"{name}\" already exists"))
            }
            NameKind::NewEnv => self.ws.save_env(Some(&name), &[], &[]).map(|()| None),
            NameKind::DuplicateEnv(from) => self
                .ws
                .load_env(Some(from))
                .and_then(|(shared, secret)| self.ws.save_env(Some(&name), &shared, &secret))
                .map(|()| None),
            NameKind::Rename(old) => self.ws.rename(old, &name).map(|new| {
                // Keep tabs (and their unsaved drafts) pointing at the moved files.
                let moved = |p: &mut PathBuf| {
                    if *p == *old {
                        *p = new.clone();
                    } else if let Ok(rest) = p.strip_prefix(&*old) {
                        *p = new.join(rest);
                    }
                };
                let parked = self.tabs.iter_mut().filter_map(|t| t.parked.as_mut());
                let opens = self.open.iter_mut().chain(parked.map(|p| &mut p.open));
                opens.for_each(|o| moved(&mut o.path));
                self.tabs.iter_mut().for_each(|t| moved(&mut t.path));
                None
            }),
        };
        match result {
            Err(e) => *error = e,
            Ok(created) => {
                let new_env = matches!(kind, NameKind::NewEnv | NameKind::DuplicateEnv(_));
                self.dialog = None;
                self.reload();
                self.save_state();
                if new_env {
                    self.set_env(Some(name.clone()));
                    // A new environment is only useful once it has variables.
                    self.open_env_editor(Some(name), &[]);
                }
                if let Some(path) = created {
                    self.activate(path, true);
                }
            }
        }
    }
}

fn into_view(mut head: http::Response) -> ResponseView {
    let body = std::mem::take(&mut head.body);
    let raw_size = body.len();
    let looks_json = head.is_json() || body.trim_start().starts_with(['{', '[']);
    let text = looks_json
        .then(|| http::pretty_json(&body))
        .flatten()
        .unwrap_or(body);
    let line_starts = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .filter(|&i| i < text.len())
        .collect();
    ResponseView {
        head,
        text,
        raw_size,
        line_starts,
        find: Find::default(),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive();
        let regained = ui.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::WindowFocused(true)))
        });
        if regained {
            self.refresh_from_disk();
        }
        // Consume shortcuts before widgets see them, so Ctrl+Enter doesn't also insert a newline.
        if ui.input_mut(|i| i.consume_shortcut(&SAVE)) {
            // Save what the user is looking at, not the request hidden behind the editor.
            if self.env_editor.is_some() {
                self.save_env_editor();
            } else if self.folder_editor.is_some() {
                self.save_folder_editor();
            } else {
                self.save();
            }
        }
        if ui.input_mut(|i| i.consume_shortcut(&FIND)) {
            self.resp_tab = RespTab::Body;
            self.focus_request = Some(egui::Id::new("find"));
        }
        if ui.input_mut(|i| i.consume_shortcut(&SEND)) {
            match self.stream.as_mut().filter(|s| s.live) {
                Some(s) => {
                    if let Err(e) = s.send_compose() {
                        self.status = e;
                    }
                }
                None => self.send(ui.ctx()),
            }
        }
        if ui.input_mut(|i| i.consume_shortcut(&CLOSE_TAB))
            && self.dialog.is_none()
            && self.env_editor.is_none()
            && self.folder_editor.is_none()
            && let Some(path) = self.open.as_ref().map(|o| o.path.clone())
        {
            self.close_tab(&path);
        }
        if ui.input_mut(|i| i.consume_shortcut(&DUPLICATE))
            && self.dialog.is_none()
            && self.env_editor.is_none()
            && self.folder_editor.is_none()
            && let Some(path) = self.open.as_ref().map(|o| o.path.clone())
        {
            self.duplicate(&path);
        }
        if ui.input_mut(|i| i.consume_shortcut(&NEW_REQUEST))
            && self.dialog.is_none()
            && self.env_editor.is_none()
            && self.folder_editor.is_none()
        {
            let root = self.ws.collections();
            self.dialog = Some(Dialog::name(NameKind::NewRequest(root), ""));
        }
        if ui.input_mut(|i| i.consume_shortcut(&FOCUS_URL))
            && let Some(open) = &self.open
        {
            // Selected, as in a browser's address bar, so typing replaces it.
            let id = egui::Id::new("url");
            let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
            let end = egui::text::CCursor::new(open.draft.url.chars().count());
            let all = egui::text_selection::CCursorRange::two(egui::text::CCursor::new(0), end);
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), id);
            // Focused now, not via `focus_request`: an unfocused TextEdit collapses the
            // selection when it draws.
            ui.memory_mut(|m| m.request_focus(id));
        }
        if ui.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && !self.unsaved().is_empty()
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::Unsaved(Next::Quit));
        }

        self.status_bar(ui);
        egui::Panel::left("sidebar")
            .default_size(260.0)
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.main_area(ui));
        // Editing turns a preview tab into a normal one, as in VS Code.
        let dirty = self.open.as_ref().filter(|o| o.dirty());
        if let Some(i) = dirty.and_then(|o| self.tab_index(&o.path)) {
            self.tabs[i].preview = false;
        }
        self.dialog_ui(ui.ctx());
        self.env_editor_ui(ui.ctx());
        self.folder_editor_ui(ui.ctx());
        self.quick_look_ui(ui.ctx());
        self.cookie_manager_ui(ui.ctx());
        if let Some(id) = self.focus_request.take() {
            ui.memory_mut(|m| m.request_focus(id));
        }
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
                let note = match self.client.get() {
                    Some(Ok(c)) => c.note.as_deref(),
                    _ => None,
                };
                let mut label = RichText::new(format!("⚙ {proxy}"));
                if note.is_some() {
                    label = RichText::new(format!("⚙ {proxy} · direct")).color(ORANGE);
                }
                if self.network.insecure {
                    label = RichText::new(format!("⚙ {proxy} · TLS verify OFF")).color(RED);
                }
                if ui
                    .small_button(label)
                    .on_hover_text(note.unwrap_or("Network settings"))
                    .clicked()
                {
                    self.network_editor = Some(self.network.clone());
                }
                ui.separator();
                let mut stop_mock = false;
                if let Some(m) = &self.mock {
                    ui.colored_label(GREEN, "●");
                    let hover = format!(
                        "Answers with the saved examples of {}. Click to copy the URL.",
                        m.folder
                    );
                    if ui
                        .small_button(format!("Mock {}", m.url))
                        .on_hover_text(hover)
                        .clicked()
                    {
                        ui.ctx().copy_text(m.url.clone());
                        self.status = "Copied the mock server URL".into();
                    }
                    stop_mock = ui.small_button("Stop").clicked();
                    ui.separator();
                }
                if stop_mock {
                    self.mock = None;
                    self.status = "Mock server stopped".into();
                }
                if let Some(m) = memory_stats::memory_stats() {
                    ui.weak(format!("RAM {:.0} MB", mb(m.physical_mem)))
                        .on_hover_text(format!(
                            "private {:.0} MB\n{}",
                            mb(m.virtual_mem),
                            self.renderer
                        ));
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
            ui.strong("Environment");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("+ New")
                    .on_hover_text("New environment")
                    .clicked()
                {
                    self.dialog = Some(Dialog::name(NameKind::NewEnv, ""));
                }
                if ui
                    .small_button("Globals")
                    .on_hover_text("Variables available in every environment")
                    .clicked()
                {
                    self.open_env_editor(None, &[]);
                }
            });
        });
        ui.horizontal(|ui| {
            let label = self
                .active_env
                .clone()
                .unwrap_or_else(|| "No environment".into());
            let mut chosen = None;
            egui::ComboBox::from_id_salt("env")
                .selected_text(label)
                .width(150.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.active_env.is_none(), "No environment")
                        .clicked()
                    {
                        chosen = Some(None);
                    }
                    for name in &self.envs {
                        if ui
                            .selectable_label(self.active_env.as_ref() == Some(name), name)
                            .clicked()
                        {
                            chosen = Some(Some(name.clone()));
                        }
                    }
                });
            if let Some(env) = chosen {
                self.set_env(env);
            }
            if let Some(name) = self.active_env.clone()
                && ui
                    .small_button("Edit")
                    .on_hover_text("Edit this environment's variables")
                    .clicked()
            {
                self.open_env_editor(Some(name), &[]);
            }
            ui.toggle_value(&mut self.quick_look, "👁")
                .on_hover_text("Quick look: every variable in scope");
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.show_history,
                false,
                RichText::new("Collections").strong(),
            );
            ui.selectable_value(
                &mut self.show_history,
                true,
                RichText::new("History").strong(),
            );
        });
        if self.show_history {
            self.history_ui(ui);
            return;
        }
        ui.horizontal(|ui| {
            let root = self.ws.collections();
            if ui
                .small_button("+ Request")
                .on_hover_text(ui.ctx().format_shortcut(&NEW_REQUEST))
                .clicked()
            {
                self.dialog = Some(Dialog::name(NameKind::NewRequest(root.clone()), ""));
            }
            if ui.small_button("+ Folder").clicked() {
                self.dialog = Some(Dialog::name(NameKind::NewFolder(root.clone()), ""));
            }
            ui.menu_button("⋯", |ui| {
                if ui.button("Copy docs as Markdown").clicked() {
                    self.copy_docs(&root, ui.ctx());
                    ui.close();
                }
                if ui.button("Start mock server").clicked() {
                    self.start_mock(root.clone(), ui.ctx());
                    ui.close();
                }
            })
            .response
            .on_hover_text("The whole collection");
            if ui
                .small_button("▶ Run")
                .on_hover_text("Run the whole collection")
                .clicked()
            {
                self.open_runner(root);
            }
        });
        if !self.tree.is_empty() {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clear = !self.tree_filter.is_empty()
                        && ui.small_button("×").on_hover_text("Clear").clicked();
                    let filter = ui.add(
                        egui::TextEdit::singleline(&mut self.tree_filter)
                            // Not an auto id: the × appearing would change it and drop focus.
                            .id(egui::Id::new("tree-filter"))
                            .hint_text(FILTER_HINT)
                            .desired_width(f32::INFINITY),
                    );
                    // Escape leaves the box, and clears it like a search field.
                    if clear || (filter.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape))) {
                        self.tree_filter.clear();
                    }
                });
            });
        }
        let mut actions = Vec::new();
        let selected = self.open.as_ref().map(|o| o.path.as_path());
        let query = self.tree_filter.trim().to_lowercase();
        let found;
        let nodes = if query.is_empty() {
            &self.tree
        } else {
            found = filtered(&self.tree, &query);
            &found
        };
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                if self.tree.is_empty() {
                    ui.weak("No requests yet. Click \"+ Request\".");
                } else if nodes.is_empty() {
                    ui.weak("Nothing matches.");
                }
                tree_ui(ui, nodes, selected, !query.is_empty(), &mut actions);
            });
        for action in actions {
            match action {
                TreeAction::Open(path, pin) => {
                    if self.runner_busy() {
                        continue;
                    }
                    self.runner = None;
                    self.activate(path, pin);
                }
                TreeAction::Dialog(d) => self.dialog = Some(d),
                TreeAction::Duplicate(path) => self.duplicate(&path),
                TreeAction::Run(path) => self.open_runner(path),
                TreeAction::FolderSettings(dir) => self.open_folder_editor(dir),
                TreeAction::CopyDocs(dir) => self.copy_docs(&dir, ui.ctx()),
                TreeAction::Mock(dir) => self.start_mock(dir, ui.ctx()),
            }
        }
    }

    /// From the saved files, so unsaved edits aren't documented.
    /// Copies what's saved, like Postman: unsaved edits stay in the original's tab. A
    /// duplicated request opens, ready to change.
    fn duplicate(&mut self, path: &Path) {
        match self.ws.duplicate(path) {
            Ok(new) => {
                self.reload();
                self.status = format!("Duplicated as \"{}\"", self.ws.display_name(&new));
                if new.is_file() && !self.runner_busy() {
                    self.runner = None;
                    self.activate(new, true);
                }
            }
            Err(e) => self.status = e,
        }
    }

    fn copy_docs(&mut self, dir: &Path, ctx: &egui::Context) {
        match crate::docs::markdown(&self.ws, dir) {
            Ok(md) => {
                ctx.copy_text(md);
                self.status = "Copied the docs as Markdown".into();
            }
            Err(e) => self.status = e,
        }
    }

    /// Port 3000 when free, so a frontend can keep one base URL across restarts.
    fn start_mock(&mut self, dir: PathBuf, ctx: &egui::Context) {
        let requests = self.ws.load_requests_in(&dir).unwrap_or_default();
        if !requests.iter().any(|(_, r)| !r.examples.is_empty()) {
            self.status = "Nothing to mock yet: send a request, then \"Save as example\"".into();
            return;
        }
        self.mock = None;
        let bound = self.rt.block_on(async {
            match tokio::net::TcpListener::bind("127.0.0.1:3000").await {
                Ok(l) => Ok(l),
                Err(_) => tokio::net::TcpListener::bind("127.0.0.1:0").await,
            }
        });
        let listener = match bound.and_then(|l| Ok((l.local_addr()?, l))) {
            Ok(l) => l,
            Err(e) => return self.status = format!("Mock server: {e}"),
        };
        let (addr, listener) = listener;
        let folder = if dir == self.ws.collections() {
            "the whole collection".to_owned()
        } else {
            store::folder_name(&self.ws.collections(), &dir)
        };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let log = move |line: String| {
            let _ = tx.send(Msg::Status(format!("Mock: {line}")));
            ctx.request_repaint();
        };
        let task = self
            .rt
            .spawn(crate::mock::serve(self.ws.clone(), dir, listener, log));
        let url = format!("http://{addr}");
        self.status = format!("Mock server for {folder} at {url}");
        self.mock = Some(MockServer {
            url,
            folder,
            abort: task.abort_handle(),
        });
    }

    fn history_ui(&mut self, ui: &mut egui::Ui) {
        let mut restore = None;
        ui.horizontal(|ui| {
            ui.weak("Click to load what was sent.");
            if !self.history.is_empty() && ui.small_button("Clear").clicked() {
                match self.ws.clear_history() {
                    Ok(()) => self.history.clear(),
                    Err(e) => self.status = e,
                }
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                if self.history.is_empty() {
                    ui.weak("Requests you send show up here.");
                }
                for (i, e) in self.history.iter().enumerate().rev() {
                    let method = &e.request.method;
                    let row = ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{:<4}", short_method(method)))
                                .monospace()
                                .small()
                                .color(method_color(method)),
                        );
                        let status = match e.status {
                            0 => "ERR".to_owned(),
                            s => s.to_string(),
                        };
                        ui.label(
                            RichText::new(status)
                                .monospace()
                                .small()
                                .color(status_color(e.status)),
                        );
                        ui.add(
                            egui::Label::new(e.path.as_str())
                                .truncate()
                                .sense(egui::Sense::click()),
                        )
                    });
                    let note = if e.body_dropped {
                        "\nBody was too large to keep"
                    } else {
                        ""
                    };
                    let hover = format!("{}\n{} · {} ms{note}", e.request.url, ago(e.at), e.ms);
                    if row.inner.on_hover_text(hover).clicked() {
                        restore = Some(i);
                    }
                }
            });
        if let Some(i) = restore {
            let e = &self.history[i];
            match self.ws.request_path(&e.path) {
                Ok(path) if path.is_file() => {
                    let draft = Box::new(e.request.clone());
                    self.runner = None;
                    self.restore(path, draft);
                }
                _ => self.status = format!("\"{}\" no longer exists", e.path),
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let active = self.open.as_ref().map(|o| o.path.clone());
        let (mut show, mut close) = (None, None);
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for tab in &self.tabs {
                    let is_active = active.as_ref() == Some(&tab.path);
                    let open = match &tab.parked {
                        _ if is_active => self.open.as_ref(),
                        Some(p) => Some(&p.open),
                        None => None,
                    };
                    if let Some(o) = open {
                        let m = &o.draft.method;
                        let badge = RichText::new(short_method(m)).monospace().small();
                        ui.label(badge.color(method_color(m)));
                    }
                    let name = tab.path.file_stem().unwrap_or_default().to_string_lossy();
                    let mut text = RichText::new(match open.is_some_and(Open::dirty) {
                        true => format!("{name} ●"),
                        false => name.into_owned(),
                    });
                    if tab.preview {
                        text = text.italics();
                    }
                    let label = ui
                        .selectable_label(is_active, text)
                        .on_hover_text(self.ws.display_name(&tab.path));
                    if label.clicked() {
                        show = Some(tab.path.clone());
                    }
                    let x = ui
                        .small_button("×")
                        .on_hover_text(format!("Close ({})", ui.ctx().format_shortcut(&CLOSE_TAB)));
                    if x.clicked() || label.middle_clicked() {
                        close = Some(tab.path.clone());
                    }
                    ui.separator();
                }
            });
        });
        if let Some(path) = close {
            self.close_tab(&path);
        } else if let Some(path) = show
            && !self.runner_busy()
        {
            self.runner = None;
            self.activate(path, false);
        }
    }

    fn main_area(&mut self, ui: &mut egui::Ui) {
        if !self.tabs.is_empty() {
            egui::Panel::top("tabs").show(ui, |ui| self.tab_bar(ui));
        }
        if self.runner.is_some() {
            self.runner_ui(ui);
            return;
        }
        let all_vars = self.all_vars();
        let Some(open) = &mut self.open else {
            ui.centered_and_justified(|ui| {
                ui.weak("Select a request on the left, or create one with \"+ Request\".")
            });
            return;
        };
        let (mut send, mut save, mut cancel) = (false, false, false);
        let (mut toggle_load, mut start_load) = (false, false);
        let mut define: Option<Vec<String>> = None;
        let mut fetch_schema = false;
        let mut example = None;
        let pending = self.pending.iter().find(|p| p.path == open.path);
        // Before `streaming`: whether a gRPC method streams comes from its proto.
        if open.draft.method == "GRPC"
            && (self.grpc_methods.as_ref()).is_none_or(|(p, _)| *p != open.draft.proto)
        {
            let methods = crate::grpc::methods(&open.draft.proto);
            self.grpc_methods = Some((open.draft.proto.clone(), methods));
        }
        let streaming = streams(&open.draft, &self.grpc_methods);
        let session = self.stream.as_mut().filter(|s| s.path == open.path);
        let live = session.as_ref().is_some_and(|s| s.live);
        let half_close = session
            .as_ref()
            .is_some_and(|s| s.grpc.is_some() && s.outgoing.is_some());

        // As in Postman: beside both the request and its response, so edits show live.
        let mut lang_changed = false;
        if self.code {
            egui::Panel::right("code")
                .resizable(true)
                .default_size(380.0)
                .show(ui, |ui| {
                    let (wire, _) = open.draft.resolved(&all_vars);
                    let code = crate::codegen::generate(&self.code_lang, wire);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("code_lang")
                            .selected_text(&self.code_lang)
                            .show_ui(ui, |ui| {
                                for (name, _) in crate::codegen::TARGETS {
                                    let lang = &mut self.code_lang;
                                    let r = ui.selectable_value(lang, (*name).to_owned(), *name);
                                    lang_changed |= r.changed();
                                }
                            });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("✕").on_hover_text("Close").clicked() {
                                self.code = false;
                            }
                            if let Ok(code) = &code
                                && ui.add(primary("Copy")).clicked()
                            {
                                ui.ctx().copy_text(code.clone());
                                self.status = format!("Copied {} snippet", self.code_lang);
                            }
                        });
                    });
                    ui.weak("Variables are filled in; scripts don't run.");
                    ui.separator();
                    match code {
                        Ok(code) => {
                            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut code.as_str())
                                        .code_editor()
                                        .desired_width(f32::INFINITY),
                                );
                            });
                        }
                        Err(e) => {
                            ui.colored_label(ORANGE, e);
                        }
                    }
                });
        }

        egui::Panel::top("request")
            .resizable(true)
            .default_size(320.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.heading(open.name());
                    if open.dirty() {
                        ui.colored_label(ORANGE, "●")
                            .on_hover_text("Unsaved changes");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        save = ui
                            .add_enabled(open.dirty(), egui::Button::new("Save"))
                            .on_hover_text(ui.ctx().format_shortcut(&SAVE))
                            .clicked();
                        if !streaming {
                            toggle_load = ui
                                .selectable_label(self.load.is_some(), "⚡ Load test")
                                .clicked();
                        }
                        ui.toggle_value(&mut self.cookie_manager, "Cookies")
                            .on_hover_text("Cookies the server set, sent back automatically");
                        ui.toggle_value(&mut self.code, "</> Code")
                            .on_hover_text("This request as curl, Python, Go, … to copy");
                    });
                });
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("method")
                        .selected_text(
                            RichText::new(&open.draft.method)
                                .color(method_color(&open.draft.method))
                                .strong(),
                        )
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for m in METHODS {
                                let text = RichText::new(*m).color(method_color(m));
                                ui.selectable_value(&mut open.draft.method, (*m).to_owned(), text);
                            }
                        });
                    // GRAPHQL is a POST whose body is always a query; open the query editor.
                    if open.draft.method == "GRAPHQL"
                        && !matches!(open.draft.body, Body::GraphQL { .. })
                    {
                        open.draft.body = Body::GraphQL {
                            query: String::new(),
                            variables: String::new(),
                        };
                        self.req_tab = ReqTab::Body;
                    }
                    let button = [80.0, 22.0];
                    let width = ui.available_width() - button[0] - 8.0;
                    let url = var_edit(
                        ui,
                        egui::Id::new("url"),
                        &mut open.draft.url,
                        &all_vars,
                        egui::TextStyle::Monospace,
                        false,
                        |e| e.hint_text("https://{{host}}/path").desired_width(width),
                    );
                    // Pasting a curl command (e.g. devtools "Copy as cURL") imports it, like Postman.
                    if url.changed() && open.draft.url.trim_start().starts_with("curl ") {
                        match crate::curl::from_curl(&open.draft.url) {
                            Ok(r) => {
                                let d = &mut open.draft;
                                (d.method, d.url, d.params) = (r.method, r.url, r.params);
                                (d.headers, d.body, d.auth) = (r.headers, r.body, r.auth);
                                d.settings = r.settings;
                                self.status = "Imported curl command".into();
                            }
                            Err(e) => self.status = format!("curl import: {e}"),
                        }
                    } else if url.changed() {
                        open.draft.params_from_url();
                    }
                    if url.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        send = true;
                    }
                    if half_close {
                        cancel = ui
                            .add_sized(button, egui::Button::new("End stream"))
                            .on_hover_text("Stop sending; the server can still reply")
                            .clicked();
                    } else if pending.is_some() || live {
                        let label = if live { "Disconnect" } else { "Cancel" };
                        cancel = ui.add_sized(button, egui::Button::new(label)).clicked();
                    } else {
                        let label = RichText::new(if streaming { "Connect" } else { "Send" })
                            .strong()
                            .color(Color32::WHITE);
                        send |= ui
                            .add_sized(
                                button,
                                egui::Button::new(label).fill(Color32::from_rgb(40, 110, 200)),
                            )
                            .on_hover_text(ui.ctx().format_shortcut(&SEND))
                            .clicked();
                    }
                });
                if open.draft.method == "GRPC"
                    && let Some(e) = grpc_bar(ui, &mut open.draft, &mut self.grpc_methods)
                {
                    self.status = e;
                }
                let (_, missing) = open.draft.resolved(&all_vars);
                // A pre-request script may define them; only warn when nothing could.
                let scripted = !open.draft.pre_request.trim().is_empty()
                    || !open.draft.inherited.pre_request.is_empty();
                if !missing.is_empty() && !scripted {
                    ui.horizontal(|ui| {
                        ui.colored_label(ORANGE, format!("Undefined: {}", missing.join(", ")));
                        // Without an environment, Globals is where a value works right away.
                        let target = self.active_env.as_deref().unwrap_or("Globals");
                        if ui.small_button(format!("Define in {target}…")).clicked() {
                            define = Some(missing.clone());
                        }
                    });
                }
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let count = |kv: &[KeyValue]| {
                        kv.iter().filter(|p| p.enabled && !p.key.is_empty()).count()
                    };
                    let tab = |n: usize, name: &str| {
                        if n > 0 {
                            format!("{name} ({n})")
                        } else {
                            name.to_owned()
                        }
                    };
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Params,
                        tab(count(&open.draft.params), "Params"),
                    );
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Headers,
                        tab(count(&open.draft.headers), "Headers"),
                    );
                    let dot = |none: bool, name: &str| {
                        if none {
                            name.to_owned()
                        } else {
                            format!("{name} ●")
                        }
                    };
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Body,
                        if open.draft.method == "GRAPHQL" {
                            "Query".to_owned()
                        } else {
                            dot(matches!(open.draft.body, Body::None), "Body")
                        },
                    );
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Auth,
                        dot(
                            matches!(open.draft.effective_auth(), Auth::None | Auth::Inherit),
                            "Auth",
                        ),
                    );
                    let no_scripts = open.draft.pre_request.trim().is_empty()
                        && open.draft.tests.trim().is_empty();
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Scripts,
                        dot(no_scripts, "Scripts"),
                    );
                    // How a single HTTP exchange goes out; streams and gRPC don't use them.
                    if !matches!(open.draft.method.as_str(), "WS" | "SSE" | "GRPC") {
                        ui.selectable_value(
                            &mut self.req_tab,
                            ReqTab::Settings,
                            dot(open.draft.settings.is_default(), "Settings"),
                        );
                    } else if self.req_tab == ReqTab::Settings {
                        self.req_tab = ReqTab::Params;
                    }
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Docs,
                        dot(open.draft.description.trim().is_empty(), "Docs"),
                    );
                    let examples = open.draft.examples.len();
                    if examples > 0 {
                        ui.selectable_value(
                            &mut self.req_tab,
                            ReqTab::Examples,
                            tab(examples, "Examples"),
                        );
                    } else if self.req_tab == ReqTab::Examples {
                        self.req_tab = ReqTab::Params;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| match self.req_tab {
                        ReqTab::Params => {
                            if kv_table(ui, "params", &mut open.draft.params, &all_vars, true) {
                                open.draft.url_from_params();
                            }
                        }
                        ReqTab::Headers => {
                            kv_table(ui, "headers", &mut open.draft.headers, &all_vars, true);
                        }
                        ReqTab::Body => {
                            // GRAPHQL requests are always a query; no body type to pick.
                            if open.draft.method == "GRAPHQL"
                                && let Body::GraphQL { query, variables } = &mut open.draft.body
                            {
                                fetch_schema |= graphql_editor(
                                    ui,
                                    query,
                                    variables,
                                    &all_vars,
                                    &mut self.explorer,
                                );
                                return;
                            }
                            fetch_schema |= body_editor(
                                ui,
                                &mut open.draft.body,
                                &all_vars,
                                &mut self.explorer,
                            );
                        }
                        ReqTab::Auth => auth_editor(
                            ui,
                            &mut open.draft.auth,
                            &all_vars,
                            open.draft.inherited.auth.as_ref(),
                        ),
                        ReqTab::Scripts => scripts_editor(
                            ui,
                            &mut self.script_tab,
                            &mut open.draft.pre_request,
                            &mut open.draft.tests,
                            &open.draft.inherited,
                        ),
                        ReqTab::Settings => {
                            settings_editor(ui, &mut open.draft.settings, self.network.timeout_secs)
                        }
                        ReqTab::Examples => examples_editor(ui, &mut open.draft.examples),
                        ReqTab::Docs => docs_editor(ui, &mut open.draft.description),
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
            if let Some(view) = self.load.as_mut() {
                start_load = load_ui(ui, view);
                return;
            }
            if streaming {
                match session {
                    Some(s) => {
                        if let Err(e) = stream_ui(ui, s) {
                            self.status = e;
                        }
                    }
                    None if open.draft.method == "GRPC" => {
                        ui.weak("Press Connect to start the call. The body is the first message (for a client stream, an array is several, empty is none). Scripts don't run for streams.");
                    }
                    None => {
                        ui.weak("Press Connect to open the stream. Scripts don't run for WebSocket/SSE.");
                    }
                }
                return;
            }
            match &mut self.response {
                None => {
                    ui.weak(format!("Press Send or {} to see the response.", ui.ctx().format_shortcut(&SEND)));
                }
                Some(shown) => example = response_ui(ui, shown, &mut self.resp_tab),
            }
        });
        if let Some(example) = example {
            self.save_example(example);
        }
        if lang_changed {
            self.save_state();
        }

        if save {
            self.save();
        }
        if cancel {
            if live {
                self.disconnect()
            } else {
                self.cancel()
            }
        }
        if send {
            self.send(ui.ctx());
        }
        if fetch_schema {
            self.fetch_schema(ui.ctx());
        }
        if let Some(names) = define {
            self.open_env_editor(self.active_env.clone(), &names);
        }
        if toggle_load {
            self.load = match self.load.take() {
                Some(_) => None,
                None => Some(LoadView {
                    vus: 10,
                    secs: 30,
                    stats: None,
                    abort: None,
                }),
            };
        }
        if start_load {
            self.start_load(ui.ctx());
        }
    }

    /// Introspects the open request's endpoint with its own headers and auth.
    fn fetch_schema(&mut self, ctx: &egui::Context) {
        let Some(open) = &self.open else {
            return;
        };
        let (mut req, _) = open.draft.resolved(&self.all_vars());
        req.method = "POST".into();
        req.body = Body::GraphQL {
            query: graphql::INTROSPECTION.into(),
            variables: String::new(),
        };
        self.explorer.url = req.url.clone();
        self.explorer.loading = true;
        let (cell, net, tx, ctx) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
            self.tx.clone(),
            ctx.clone(),
        );
        self.rt.spawn(async move {
            let url = req.url.clone();
            let result = match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                Ok(client) => match runner::send(client, req).await {
                    Ok(r) if (200..300).contains(&r.status) => graphql::parse(&r.body),
                    Ok(r) => Err(format!(
                        "HTTP {} {}\n{}",
                        r.status,
                        r.reason,
                        clip(&r.body, 300)
                    )),
                    Err(e) => Err(e),
                },
                Err(e) => Err(format!("Network settings: {e}")),
            };
            let _ = tx.send(Msg::Schema(url, result));
            ctx.request_repaint();
        });
    }

    /// Variables are resolved once up front: every virtual user sends the identical request.
    fn start_load(&mut self, ctx: &egui::Context) {
        let (Some(open), Some(view)) = (&self.open, &self.load) else {
            return;
        };
        let (req, _) = open.draft.resolved(&self.all_vars());
        let stats = Arc::new(std::sync::Mutex::new(Stats::default()));
        let (vus, secs) = (view.vus, view.secs);
        let (cell, net, tx, s) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
            self.tx.clone(),
            stats.clone(),
        );
        let ctx = ctx.clone();
        let task = self.rt.spawn(async move {
            match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                Ok(client) => {
                    loadtest::run(client.clone(), req, vus, Duration::from_secs(secs), s).await
                }
                Err(e) => {
                    let _ = tx.send(Msg::Status(format!("Network settings: {e}")));
                    let mut s = s.lock().unwrap();
                    s.finished = Some(s.started.elapsed());
                }
            }
            ctx.request_repaint();
        });
        let view = self.load.as_mut().unwrap();
        view.stats = Some(stats);
        if let Some(old) = view.abort.replace(task.abort_handle()) {
            old.abort();
        }
    }

    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let unsaved = self.unsaved().join("\", \"");
        let Some(dialog) = &mut self.dialog else {
            return;
        };
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
                        NameKind::DuplicateEnv(_) => "Duplicate environment",
                    });
                    let edit = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("Name")
                            .desired_width(f32::INFINITY),
                    );
                    // Before refocusing: Enter is what made the field let go of focus.
                    let enter = enter_pressed(ui);
                    if !enter && ui.memory(|m| m.focused().is_none()) {
                        edit.request_focus();
                    }
                    if !error.is_empty() {
                        ui.colored_label(RED, error.as_str());
                    }
                    ui.horizontal(|ui| {
                        if ui.add(primary("OK")).clicked() || enter {
                            then = Some(Box::new(|app, _| app.submit_name()));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Delete(path) => {
                    ui.heading("Delete");
                    let what = if path.is_dir() {
                        "folder and everything in it"
                    } else {
                        "request"
                    };
                    ui.label(format!(
                        "Delete {what} \"{}\"?",
                        path.file_stem().unwrap_or_default().to_string_lossy()
                    ));
                    ui.horizontal(|ui| {
                        let delete = egui::Button::new(
                            RichText::new("Delete").strong().color(Color32::WHITE),
                        )
                        .fill(RED);
                        if ui.add(delete).clicked() || enter_pressed(ui) {
                            let path = path.clone();
                            then = Some(Box::new(move |app, _| {
                                app.dialog = None;
                                match app.ws.delete(&path) {
                                    // The delete was confirmed; its tabs close without asking.
                                    Ok(()) => {
                                        let active = app.open.as_ref().map(|o| o.path.clone());
                                        let gone = |p: &Path| p.starts_with(&path);
                                        app.tabs.retain(|t| {
                                            !gone(&t.path) || Some(&t.path) == active.as_ref()
                                        });
                                        if let Some(i) = active
                                            .filter(|p| gone(p))
                                            .and_then(|p| app.tab_index(&p))
                                        {
                                            app.drop_tab(i);
                                        }
                                        app.save_state();
                                    }
                                    Err(e) => app.status = e,
                                }
                                app.reload();
                            }));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Unsaved(next) => {
                    let quit = matches!(next, Next::Quit);
                    ui.heading("Unsaved changes");
                    let names = if quit { &unsaved } else { &open_name };
                    ui.label(format!("Save changes to \"{names}\"?"));
                    ui.horizontal(|ui| {
                        let save = ui.add(primary("Save")).clicked() || enter_pressed(ui);
                        let discard = ui.button("Discard").clicked();
                        cancel = ui.button("Cancel").clicked();
                        if save || discard {
                            then = Some(Box::new(move |app, ctx| {
                                if save && !(if quit { app.save_all() } else { app.save() }) {
                                    return; // keep the dialog; the status bar shows why
                                }
                                let Some(Dialog::Unsaved(next)) = app.dialog.take() else {
                                    return;
                                };
                                match next {
                                    Next::Open(path, draft) => app.force_open_with(path, draft),
                                    Next::Close(path) => {
                                        if let Some(i) = app.tab_index(&path) {
                                            app.drop_tab(i);
                                        }
                                    }
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
        let Some(ed) = &mut self.env_editor else {
            return;
        };
        let (mut save, mut delete, mut close, mut duplicate) = (false, false, false, false);
        let file = ed.env.clone().unwrap_or_else(|| "globals".into());
        // Only explicit buttons close this one: a stray click outside must not drop edits.
        egui::Modal::new(egui::Id::new("env-editor")).show(ctx, |ui| {
            ui.set_width(620.0);
            match &ed.env {
                Some(name) => ui.heading(format!("Environment: {name}")),
                None => ui.heading("Globals"),
            };
            if ed.env.is_none() {
                ui.weak("Available in every environment; an environment variable with the same name wins.");
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                ui.label(RichText::new("Shared").strong());
                ui.weak(format!("Saved to {file}.toml and committed to git."));
                kv_table(ui, "env-shared", &mut ed.shared, &HashMap::new(), false);
                ui.add_space(10.0);
                ui.label(RichText::new("Secret").strong());
                ui.weak(format!(
                    "Saved to {file}.secret.toml, which is gitignored. Overrides shared values; \
                     values set by scripts land here."
                ));
                kv_table(ui, "env-secret", &mut ed.secret, &HashMap::new(), false);
            });
            if !ed.error.is_empty() {
                ui.colored_label(RED, ed.error.as_str());
            }
            ui.separator();
            ui.horizontal(|ui| {
                save = ui
                    .add(primary("Save"))
                    .on_hover_text(ui.ctx().format_shortcut(&SAVE))
                    .clicked()
                    || enter_pressed(ui);
                let close_label = if ed.confirm_discard {
                    RichText::new("Discard changes").color(RED)
                } else {
                    RichText::new("Close")
                };
                close = ui.button(close_label).clicked() || escape_pressed(ui);
                if ed.env.is_some() {
                    duplicate = ui
                        .button("Duplicate…")
                        .on_hover_text("Save, then copy into a new environment (e.g. dev → prod)")
                        .clicked();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if ed.confirm_delete {
                            "Click again to delete"
                        } else {
                            "Delete environment"
                        };
                        if ui.button(RichText::new(label).color(RED)).clicked() {
                            delete = ed.confirm_delete;
                            ed.confirm_delete = true;
                        }
                    });
                }
            });
        });
        if save || duplicate {
            let from = ed.env.clone();
            if self.save_env_editor()
                && duplicate
                && let Some(from) = from
            {
                let name = format!("{from} copy");
                self.dialog = Some(Dialog::name(NameKind::DuplicateEnv(from), name));
            }
        } else if delete && let Some(name) = ed.env.clone() {
            match self.ws.delete_env(&name) {
                Ok(()) => {
                    self.env_editor = None;
                    if self.active_env.as_ref() == Some(&name) {
                        self.set_env(None);
                    }
                    self.reload();
                }
                Err(e) => ed.error = e,
            }
        } else if close {
            if ed.dirty() && !ed.confirm_discard {
                ed.confirm_discard = true;
            } else {
                self.env_editor = None;
            }
        }
    }

    /// Returns false (and shows why in the editor) if writing failed.
    fn save_env_editor(&mut self) -> bool {
        let Some(ed) = &mut self.env_editor else {
            return false;
        };
        match self.ws.save_env(ed.env.as_deref(), &ed.shared, &ed.secret) {
            Ok(()) => {
                let name = ed.env.clone().unwrap_or_else(|| "Globals".into());
                self.status = format!("Saved {name}");
                self.env_editor = None;
                self.reload_vars();
                true
            }
            Err(e) => {
                ed.error = e;
                false
            }
        }
    }

    fn open_folder_editor(&mut self, dir: PathBuf) {
        // A folder inherits from its parents exactly like a request in it would.
        let loaded = self.ws.load_folder(&dir);
        match loaded.and_then(|f| Ok((f, self.ws.inherited(&dir)?))) {
            Ok((folder, parent)) => {
                // Ready to type into the blank row, like the environment editor.
                self.focus_request = Some(egui::Id::new(("folder-vars", folder.vars.len(), 0)));
                self.folder_editor = Some(FolderEditor {
                    name: store::folder_name(&self.ws.collections(), &dir),
                    dir,
                    saved: folder.clone(),
                    folder,
                    parent,
                    tab: FolderTab::Vars,
                    error: String::new(),
                    confirm_discard: false,
                })
            }
            Err(e) => self.status = e,
        }
    }

    fn folder_editor_ui(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.folder_editor else {
            return;
        };
        let (mut save, mut close) = (false, false);
        let mut vars = self.globals.clone();
        vars.extend(ed.parent.vars.clone());
        let own = ed.folder.vars.iter().filter(|v| v.enabled);
        vars.extend(own.map(|v| (v.key.clone(), v.value.clone())));
        vars.extend(self.vars.clone());
        // Like the environment editor: only explicit buttons close it.
        egui::Modal::new(egui::Id::new("folder-editor")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.heading(format!("Folder: {}", ed.name));
            ui.weak(
                "Shared by every request in this folder and its subfolders. \
                 Saved to .folder.toml and committed to git.",
            );
            ui.horizontal(|ui| {
                let f = &ed.folder;
                let n = f
                    .vars
                    .iter()
                    .filter(|v| v.enabled && !v.key.is_empty())
                    .count();
                let vars_label = match n {
                    0 => "Variables".to_owned(),
                    n => format!("Variables ({n})"),
                };
                let dot = |set: bool, name: &str| match set {
                    true => format!("{name} ●"),
                    false => name.to_owned(),
                };
                let scripts = !f.pre_request.trim().is_empty() || !f.tests.trim().is_empty();
                let auth = dot(f.auth != Auth::Inherit, "Auth");
                ui.selectable_value(&mut ed.tab, FolderTab::Vars, vars_label);
                ui.selectable_value(&mut ed.tab, FolderTab::Auth, auth);
                ui.selectable_value(&mut ed.tab, FolderTab::Scripts, dot(scripts, "Scripts"));
                let docs = dot(!f.description.trim().is_empty(), "Docs");
                ui.selectable_value(&mut ed.tab, FolderTab::Docs, docs);
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| match ed.tab {
                    FolderTab::Vars => {
                        ui.weak(
                            "Below environments: an environment variable with the same name \
                             wins. Keep secrets in a secret environment.",
                        );
                        kv_table(
                            ui,
                            "folder-vars",
                            &mut ed.folder.vars,
                            &HashMap::new(),
                            false,
                        );
                    }
                    FolderTab::Auth => {
                        let parent = ed.parent.auth.as_ref();
                        auth_editor(ui, &mut ed.folder.auth, &vars, parent);
                        ui.weak("Requests set to \"Inherit from parent\" use this.");
                    }
                    FolderTab::Scripts => scripts_editor(
                        ui,
                        &mut self.script_tab,
                        &mut ed.folder.pre_request,
                        &mut ed.folder.tests,
                        &ed.parent,
                    ),
                    FolderTab::Docs => docs_editor(ui, &mut ed.folder.description),
                });
            if !ed.error.is_empty() {
                ui.colored_label(RED, ed.error.as_str());
            }
            ui.separator();
            ui.horizontal(|ui| {
                save = ui
                    .add(primary("Save"))
                    .on_hover_text(ui.ctx().format_shortcut(&SAVE))
                    .clicked()
                    || enter_pressed(ui);
                let close_label = if ed.confirm_discard {
                    RichText::new("Discard changes").color(RED)
                } else {
                    RichText::new("Close")
                };
                close = ui.button(close_label).clicked() || escape_pressed(ui);
            });
        });
        if save {
            self.save_folder_editor();
        } else if close {
            if ed.folder != ed.saved && !ed.confirm_discard {
                ed.confirm_discard = true;
            } else {
                self.folder_editor = None;
            }
        }
    }

    fn save_folder_editor(&mut self) {
        let Some(ed) = &mut self.folder_editor else {
            return;
        };
        match self.ws.save_folder(&ed.dir, &ed.folder) {
            Ok(()) => {
                self.status = format!("Saved folder {}", ed.name);
                self.folder_editor = None;
                self.refresh_inherited();
            }
            Err(e) => ed.error = e,
        }
    }

    fn cookie_manager_ui(&mut self, ctx: &egui::Context) {
        if !self.cookie_manager {
            return;
        }
        let rows = self.cookies.rows();
        let (mut open, mut remove, mut clear) = (true, None, false);
        egui::Window::new("Cookies")
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                if rows.is_empty() {
                    ui.weak("No cookies yet. Set-Cookie responses fill the jar.");
                    return;
                }
                ui.weak("Sent back to matching URLs, unless a request sets its own Cookie header.");
                egui::ScrollArea::vertical()
                    .max_height(400.0)
                    .show(ui, |ui| {
                        let mut domain = "";
                        for (i, r) in rows.iter().enumerate() {
                            if r.domain != domain {
                                ui.strong(&r.domain);
                                domain = &r.domain;
                            }
                            ui.horizontal(|ui| {
                                if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                                    remove = Some(i);
                                }
                                ui.monospace(format!("{}={}", r.name, clip(&r.value, 60)));
                                let expires = r.expires.as_deref().unwrap_or("session");
                                ui.weak(format!("{} · {expires}", r.path));
                            });
                        }
                    });
                clear = ui.button("Clear all").clicked();
            });
        if let Some(i) = remove {
            self.cookies.remove(&rows[i]);
        }
        if clear {
            self.cookies.clear();
        }
        if remove.is_some() || clear {
            self.save_cookies();
        }
        self.cookie_manager = open;
    }

    /// Every variable `{{name}}` can resolve to right now, and where it comes from.
    fn quick_look_ui(&mut self, ctx: &egui::Context) {
        if !self.quick_look {
            return;
        }
        let mut edit = None;
        let mut open = true;
        egui::Window::new("Variables in scope")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .default_pos([280.0, 60.0])
            .show(ctx, |ui| {
                let env = self.active_env.clone();
                let mut names: Vec<_> = self.all_vars().into_keys().collect();
                names.sort_by_key(|n| n.to_lowercase());
                if names.is_empty() {
                    ui.weak("No variables yet. Add some to an environment or to Globals.");
                }
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        egui::Grid::new("quick-look").striped(true).show(ui, |ui| {
                            for name in &names {
                                let folder = self.open.as_ref().map(|o| &o.draft.inherited.vars);
                                let (value, source) =
                                    match (self.vars.get(name), folder.and_then(|f| f.get(name))) {
                                        (Some(v), _) => (v, env.clone().unwrap_or_default()),
                                        (None, Some(v)) => (v, "Folder".to_owned()),
                                        _ => (&self.globals[name], "Globals".to_owned()),
                                    };
                                ui.monospace(name);
                                ui.label(RichText::new(clip(value, 60)).monospace())
                                    .on_hover_text(value.as_str());
                                ui.weak(source);
                                ui.end_row();
                            }
                        });
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if let Some(name) = &env
                        && ui.button(format!("Edit {name}")).clicked()
                    {
                        edit = Some(Some(name.clone()));
                    }
                    if ui.button("Edit Globals").clicked() {
                        edit = Some(None);
                    }
                });
                let dynamic: Vec<_> = model::DYNAMIC.iter().map(|(n, _)| *n).collect();
                ui.weak(format!("Always available: {}", dynamic.join(", ")));
            });
        self.quick_look = open;
        if let Some(env) = edit {
            self.open_env_editor(env, &[]);
        }
    }
}

impl App {
    fn open_runner(&mut self, scope: PathBuf) {
        if self
            .runner
            .as_ref()
            .and_then(|r| r.run.as_ref())
            .is_some_and(RunState::running)
        {
            self.status = "The collection runner is already running.".into();
            return;
        }
        let title = if scope == self.ws.collections() {
            "Whole collection".to_owned()
        } else {
            self.ws.display_name(&scope)
        };
        // Keep the previous settings when re-opening, which is how people iterate on a run.
        let prev = self.runner.take();
        self.runner = Some(RunnerView {
            scope,
            title,
            iterations: prev.as_ref().map_or(1, |p| p.iterations),
            data_path: prev
                .as_ref()
                .map(|p| p.data_path.clone())
                .unwrap_or_default(),
            delay_ms: prev.as_ref().map_or(0, |p| p.delay_ms),
            only_failures: false,
            error: String::new(),
            run: None,
        });
    }

    /// "Folder/Request" relative to the collections root, without the extension.
    fn start_run(&mut self, ctx: &egui::Context) {
        let Some(view) = &self.runner else { return };
        let mut paths = Vec::new();
        store::requests_in(&self.tree, &view.scope, &mut paths);
        let mut requests = Vec::new();
        let mut error = String::new();
        for path in &paths {
            match self.ws.load_request(path) {
                Ok(req) => requests.push((self.ws.display_name(path), req)),
                Err(e) => error = e,
            }
        }
        let data = match view.data_path.trim() {
            "" => Ok(Vec::new()),
            p => runner::load_data(Path::new(p)).and_then(|rows| {
                if rows.is_empty() {
                    Err(format!("{p}: no data rows"))
                } else {
                    Ok(rows)
                }
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
            self.status =
                "Note: the runner uses saved files; unsaved edits are not included.".into();
        }
        let count = if data.is_empty() {
            view.iterations.max(1)
        } else {
            data.len()
        };
        let plan = RunPlan {
            requests,
            data,
            iterations: view.iterations,
            delay: Duration::from_millis(view.delay_ms),
        };
        let total = plan.requests.len() * count;

        self.next_run_id += 1;
        let id = self.next_run_id;
        let (cell, net) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
        );
        let vars = Vars {
            env: self.vars.clone(),
            globals: self.globals.clone(),
            data: HashMap::new(),
        };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let task = self.rt.spawn(async move {
            let client = match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
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
        view.run = Some(RunState {
            id,
            started: Instant::now(),
            finished: None,
            total,
            items: Vec::new(),
            abort: task.abort_handle(),
        });
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
                start = ui
                    .add(egui::Button::new(label).fill(Color32::from_rgb(40, 110, 200)))
                    .clicked();
            }
            ui.checkbox(&mut view.only_failures, "Failures only");
        });
        ui.separator();

        if let Some(run) = &view.run {
            let done = run.items.len();
            let failed = run.items.iter().filter(|i| i.failed()).count();
            let (tests_passed, tests_total) = run.items.iter().fold((0, 0), |(p, t), i| {
                (
                    p + i.tests.iter().filter(|x| x.passed).count(),
                    t + i.tests.len(),
                )
            });
            let elapsed = run.finished.unwrap_or_else(|| run.started.elapsed());
            ui.add(
                egui::ProgressBar::new(done as f32 / run.total.max(1) as f32)
                    .text(format!("{done}/{}", run.total)),
            );
            ui.horizontal(|ui| {
                ui.label(format!("{:.1} s", elapsed.as_secs_f32()));
                ui.separator();
                ui.colored_label(
                    if failed == 0 { GREEN } else { RED },
                    format!("{failed} failed requests"),
                );
                ui.separator();
                let color = if tests_passed == tests_total {
                    GREEN
                } else {
                    RED
                };
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
            egui::ScrollArea::vertical()
                .id_salt("runner-results")
                .auto_shrink(false)
                .stick_to_bottom(running)
                .show(ui, |ui| {
                    for item in run
                        .items
                        .iter()
                        .filter(|i| !view.only_failures || i.failed())
                    {
                        run_item_ui(ui, item);
                    }
                });
        } else {
            ui.weak("Requests run in the order shown on the left, with the selected environment.");
        }

        if close {
            if let Some(run) = self
                .runner
                .as_ref()
                .and_then(|r| r.run.as_ref())
                .filter(|r| r.running())
            {
                run.abort.abort();
            }
            self.runner = None;
        } else if start {
            self.start_run(ui.ctx());
        }
    }

    fn network_editor_ui(&mut self, ctx: &egui::Context) {
        let Some(net) = &mut self.network_editor else {
            return;
        };
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
                ProxyMode::System => match net::system_auto_config() {
                    (Some(url), _) => {
                        ui.weak(format!("Windows is configured with a PAC script, which will be used:\n{url}"));
                    }
                    (None, true) => {
                        ui.weak(
                            "Windows has \"Automatically detect settings\" on: the PAC script is \
                             looked up via WPAD (DHCP, then DNS) before the first request. If \
                             what it finds isn't a usable script, requests go out directly.",
                        );
                    }
                    (None, false) => {
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
                apply = ui.add(primary("Apply")).clicked() || enter_pressed(ui);
                cancel = ui.button("Cancel").clicked() || escape_pressed(ui);
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

fn run_item_ui(ui: &mut egui::Ui, item: &RunItem) {
    ui.horizontal(|ui| {
        ui.weak(format!("#{:<3}", item.iteration + 1));
        ui.label(
            RichText::new(format!("{:<5}", short_method(&item.method)))
                .monospace()
                .small()
                .color(method_color(&item.method)),
        );
        ui.label(item.name.as_str());
        match &item.status {
            Ok((code, ms)) => {
                ui.label(
                    RichText::new(code.to_string())
                        .strong()
                        .color(status_color(*code)),
                );
                ui.weak(format!("{ms} ms"));
            }
            Err(_) => {
                ui.colored_label(RED, "ERROR");
            }
        }
        if !item.tests.is_empty() {
            let passed = item.tests.iter().filter(|t| t.passed).count();
            let color = if passed == item.tests.len() {
                GREEN
            } else {
                RED
            };
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

/// The tree with only the requests whose name contains `query` (lowercase), and the
/// folders leading to them. A folder whose own name matches keeps all it holds.
fn filtered(nodes: &[Node], query: &str) -> Vec<Node> {
    let hit = |name: &str| name.to_lowercase().contains(query);
    nodes
        .iter()
        .filter_map(|node| match node {
            Node::Folder { name, .. } if hit(name) => Some(node.clone()),
            Node::Folder {
                name,
                path,
                children,
            } => {
                let children = filtered(children, query);
                (!children.is_empty()).then(|| Node::Folder {
                    name: name.clone(),
                    path: path.clone(),
                    children,
                })
            }
            Node::Request { name, .. } => hit(name).then(|| node.clone()),
        })
        .collect()
}

/// `expand` opens every folder, so what a filter found is in view.
fn tree_ui(
    ui: &mut egui::Ui,
    nodes: &[Node],
    selected: Option<&Path>,
    expand: bool,
    actions: &mut Vec<TreeAction>,
) {
    for node in nodes {
        match node {
            Node::Folder {
                name,
                path,
                children,
            } => {
                let resp = egui::CollapsingHeader::new(name.as_str())
                    .id_salt(path)
                    // Reveal the open request on startup instead of hiding it in a collapsed folder.
                    .default_open(selected.is_some_and(|s| s.starts_with(path)))
                    .open(expand.then_some(true))
                    .show(ui, |ui| tree_ui(ui, children, selected, expand, actions));
                resp.header_response.context_menu(|ui| {
                    let mut item = |label: &str, a: TreeAction| {
                        if ui.button(label).clicked() {
                            actions.push(a);
                            ui.close();
                        }
                    };
                    let dialog = |kind| TreeAction::Dialog(Dialog::name(kind, ""));
                    item("New request", dialog(NameKind::NewRequest(path.clone())));
                    item("New folder", dialog(NameKind::NewFolder(path.clone())));
                    item(
                        "Rename",
                        TreeAction::Dialog(Dialog::name(
                            NameKind::Rename(path.clone()),
                            name.as_str(),
                        )),
                    );
                    item("Duplicate", TreeAction::Duplicate(path.clone()));
                    item("Delete", TreeAction::Dialog(Dialog::Delete(path.clone())));
                    ui.separator();
                    if ui.button("Folder settings…").clicked() {
                        actions.push(TreeAction::FolderSettings(path.clone()));
                        ui.close();
                    }
                    if ui.button("Copy docs as Markdown").clicked() {
                        actions.push(TreeAction::CopyDocs(path.clone()));
                        ui.close();
                    }
                    if ui.button("Start mock server").clicked() {
                        actions.push(TreeAction::Mock(path.clone()));
                        ui.close();
                    }
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
                        ui.label(
                            RichText::new(badge)
                                .monospace()
                                .small()
                                .color(method_color(method)),
                        );
                        ui.selectable_label(selected == Some(path.as_path()), name.as_str())
                    })
                    .inner;
                if resp.double_clicked() {
                    actions.push(TreeAction::Open(path.clone(), true));
                } else if resp.clicked() {
                    actions.push(TreeAction::Open(path.clone(), false));
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        actions.push(TreeAction::Dialog(Dialog::name(
                            NameKind::Rename(path.clone()),
                            name.as_str(),
                        )));
                        ui.close();
                    }
                    let duplicate = egui::Button::new("Duplicate")
                        .shortcut_text(ui.ctx().format_shortcut(&DUPLICATE));
                    if ui.add(duplicate).clicked() {
                        actions.push(TreeAction::Duplicate(path.clone()));
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
/// `describe` marks a request table: it gets a Description column (variables have none,
/// as in Postman) and Bulk Edit. Returns whether any row changed.
fn kv_table(
    ui: &mut egui::Ui,
    id: &str,
    rows: &mut Vec<KeyValue>,
    vars: &HashMap<String, String>,
    describe: bool,
) -> bool {
    let before = rows.clone();
    if describe {
        // The text lives in egui's memory while editing, so a half-typed line isn't
        // rewritten from the rows under the cursor.
        let bulk_id = egui::Id::new((id, "bulk"));
        let mut bulk: Option<String> = ui.data(|d| d.get_temp(bulk_id));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let label = if bulk.is_some() {
                "Key-Value Edit"
            } else {
                "Bulk Edit"
            };
            if ui.small_button(label).clicked() {
                bulk = match bulk {
                    Some(_) => None,
                    None => Some(to_bulk(rows)),
                };
            }
        });
        if let Some(text) = &mut bulk {
            // Rows changed elsewhere (the URL, another request) replace the text.
            if from_bulk(text, rows) != *rows {
                *text = to_bulk(rows);
            }
            let edited = ui.add(
                egui::TextEdit::multiline(text)
                    .code_editor()
                    .hint_text(BULK_HINT)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );
            if edited.changed() {
                *rows = from_bulk(text, rows);
            }
        }
        let shown = bulk.is_some();
        ui.data_mut(|d| match bulk {
            Some(text) => {
                d.insert_temp(bulk_id, text);
            }
            None => d.remove::<String>(bulk_id),
        });
        if shown {
            return *rows != before;
        }
    }
    let key_width = 200.0;
    let rest = (ui.available_width() - key_width - 90.0).max(120.0);
    let (value_width, desc_width) = match describe {
        true => (rest * 0.6, rest * 0.4 - ui.spacing().item_spacing.x),
        false => (rest, 0.0),
    };
    let mut remove = None;
    let mut blank = KeyValue::new("", "");
    let existing = rows.len();
    // Plain rows, not egui::Grid: Grid clamps a cell to last frame's column width, so
    // text fields that start narrow stay narrow forever.
    for (i, row) in rows
        .iter_mut()
        .chain(std::iter::once(&mut blank))
        .enumerate()
    {
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
            let style = egui::TextStyle::Body;
            var_edit(
                ui,
                egui::Id::new((id, i, 0)),
                &mut row.key,
                vars,
                style.clone(),
                false,
                |e| e.hint_text("Key").desired_width(key_width),
            );
            var_edit(
                ui,
                egui::Id::new((id, i, 1)),
                &mut row.value,
                vars,
                style,
                false,
                |e| e.hint_text("Value").desired_width(value_width),
            );
            if describe {
                ui.add(
                    egui::TextEdit::singleline(&mut row.description)
                        .id(egui::Id::new((id, i, 2)))
                        .hint_text("Description")
                        .desired_width(desc_width),
                );
            }
            if real && ui.small_button("🗑").on_hover_text("Remove").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        rows.remove(i);
    }
    if blank != KeyValue::new("", "") {
        rows.push(blank);
    }
    *rows != before
}

const BULK_HINT: &str = "key: value, one per line; // in front turns a line off";

/// Postman's bulk format: `key: value` per line, `//` in front of a disabled one.
fn to_bulk(rows: &[KeyValue]) -> String {
    let line = |r: &KeyValue| {
        let off = if r.enabled { "" } else { "//" };
        format!("{off}{}: {}", r.key, r.value)
    };
    rows.iter().map(line).collect::<Vec<_>>().join("\n")
}

/// Descriptions aren't in the text, so each line keeps the one its key had.
fn from_bulk(text: &str, old: &[KeyValue]) -> Vec<KeyValue> {
    let mut used = vec![false; old.len()];
    let lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    lines
        .map(|line| {
            let (enabled, line) = match line.strip_prefix("//") {
                Some(rest) => (false, rest.trim_start()),
                None => (true, line),
            };
            let (key, value) = line.split_once(':').unwrap_or((line, ""));
            let mut row = KeyValue::new(key.trim(), value.trim());
            row.enabled = enabled;
            if let Some(i) = (0..old.len()).find(|&i| !used[i] && old[i].key == row.key) {
                used[i] = true;
                row.description = old[i].description.clone();
            }
            row
        })
        .collect()
}

/// Returns true when the GraphQL explorer asked to fetch the schema.
fn body_editor(
    ui: &mut egui::Ui,
    body: &mut Body,
    vars: &HashMap<String, String>,
    explorer: &mut Explorer,
) -> bool {
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
            ("Multipart", Body::Multipart { parts: Vec::new() }),
            (
                "GraphQL",
                Body::GraphQL {
                    query: String::new(),
                    variables: String::new(),
                },
            ),
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
            code_editor(ui, "body", text, vars);
        }
        Body::Text { text } => code_editor(ui, "body", text, vars),
        Body::Form { fields } => {
            kv_table(ui, "form", fields, vars, true);
        }
        Body::Multipart { parts } => {
            ui.weak("A value starting with @ uploads that file, e.g. @files/photo.png (relative to the workspace). Or drop files here.");
            let dropped: Vec<PathBuf> = ui.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .map(|f| f.path().to_owned())
                    .collect()
            });
            let workspace = std::env::current_dir().unwrap_or_default();
            for path in dropped {
                let key = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                // Inside the workspace, keep it relative so it still works after a git clone.
                let path = path.strip_prefix(&workspace).unwrap_or(&path);
                parts.push(KeyValue::new(key, format!("@{}", path.display())));
            }
            kv_table(ui, "multipart", parts, vars, true);
            for p in parts.iter().filter(|p| p.enabled) {
                if let Some(file) = p.value.strip_prefix('@')
                    && !file.contains("{{")
                    && !Path::new(file).is_file()
                {
                    ui.colored_label(ORANGE, format!("File not found: {file}"));
                }
            }
        }
        Body::GraphQL { query, variables } => {
            return graphql_editor(ui, query, variables, vars, explorer);
        }
    }
    false
}

/// Query and variables on the left, schema explorer on the right. Returns true when
/// the user asked to fetch the schema.
fn graphql_editor(
    ui: &mut egui::Ui,
    query: &mut String,
    variables: &mut String,
    vars: &HashMap<String, String>,
    ex: &mut Explorer,
) -> bool {
    let mut fetch = false;
    let mut insert = None;
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        ui.label("Query");
        var_edit(
            ui,
            egui::Id::new("gql-query"),
            query,
            vars,
            egui::TextStyle::Monospace,
            true,
            |e| {
                e.code_editor()
                    .hint_text("Fetch the schema and click a field →\nor type a query here.")
                    .desired_rows(8)
                    .desired_width(f32::INFINITY)
            },
        );
        ui.horizontal(|ui| {
            ui.label("Variables (JSON)");
            if !variables.trim().is_empty()
                && let Err(e) = serde_json::from_str::<serde::de::IgnoredAny>(variables)
            {
                ui.colored_label(ORANGE, format!("Not valid JSON: {e}"));
            }
        });
        var_edit(
            ui,
            egui::Id::new("gql-variables"),
            variables,
            vars,
            egui::TextStyle::Monospace,
            true,
            |e| {
                e.code_editor()
                    .hint_text("{ \"id\": \"{{userId}}\" }")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
            },
        );

        let ui = &mut cols[1];
        ui.horizontal(|ui| {
            ui.strong("Schema");
            if ex.loading {
                ui.spinner();
            } else {
                let label = if ex.schema.is_some() {
                    "Refresh"
                } else {
                    "Fetch schema"
                };
                fetch = ui
                    .small_button(label)
                    .on_hover_text("Introspect using this request's URL, headers and auth")
                    .clicked();
            }
        });
        match &ex.schema {
            None => {
                ui.weak("Fetch the schema to browse its queries and mutations.");
            }
            Some(Err(e)) => {
                ui.colored_label(RED, e.as_str());
            }
            Some(Ok(schema)) => {
                ui.weak(format!("from {}", ex.url));
                ui.add(
                    egui::TextEdit::singleline(&mut ex.filter)
                        .hint_text("Filter fields")
                        .desired_width(f32::INFINITY),
                );
                let filter = ex.filter.to_lowercase();
                egui::ScrollArea::vertical()
                    .id_salt("gql-schema")
                    .show(ui, |ui| {
                        for (title, op, fields) in [
                            ("Query", Operation::Query, &schema.query),
                            ("Mutation", Operation::Mutation, &schema.mutation),
                        ] {
                            if fields.is_empty() {
                                continue;
                            }
                            egui::CollapsingHeader::new(format!("{title} ({})", fields.len()))
                                .default_open(true)
                                .show(ui, |ui| {
                                    for f in fields
                                        .iter()
                                        .filter(|f| f.name.to_lowercase().contains(&filter))
                                    {
                                        let args: Vec<_> = f
                                            .args
                                            .iter()
                                            .map(|(n, t)| format!("{n}: {t}"))
                                            .collect();
                                        let args = if args.is_empty() {
                                            String::new()
                                        } else {
                                            format!("({})", args.join(", "))
                                        };
                                        let label = format!("{}{args}: {}", f.name, f.ty);
                                        let mut hover = f.description.clone();
                                        if !hover.is_empty() {
                                            hover.push_str("\n\n");
                                        }
                                        hover.push_str(
                                            "Click to replace the query with this field.",
                                        );
                                        if ui
                                            .add(
                                                egui::Label::new(RichText::new(label).monospace())
                                                    .sense(egui::Sense::click()),
                                            )
                                            .on_hover_text(hover)
                                            .clicked()
                                        {
                                            insert = Some((op, f.clone()));
                                        }
                                    }
                                });
                        }
                    });
            }
        }
    });
    if let Some((op, field)) = insert
        && let Some(Ok(schema)) = &ex.schema
    {
        let (text, vars) = schema.operation(op, &field);
        *query = text;
        *variables = if vars.is_empty() {
            String::new()
        } else {
            serde_json::to_string_pretty(&serde_json::Value::Object(vars)).unwrap_or_default()
        };
    }
    fetch
}

fn code_editor(ui: &mut egui::Ui, id: &str, text: &mut String, vars: &HashMap<String, String>) {
    var_edit(
        ui,
        egui::Id::new(id),
        text,
        vars,
        egui::TextStyle::Monospace,
        true,
        |e| {
            e.code_editor()
                .desired_rows(12)
                .desired_width(f32::INFINITY)
        },
    );
}

/// `inherited`: what "Inherit from parent" currently resolves to, and from which folder.
fn auth_editor(
    ui: &mut egui::Ui,
    auth: &mut Auth,
    vars: &HashMap<String, String>,
    inherited: Option<&(String, Auth)>,
) {
    let (user, pass) = (String::new, String::new);
    let kinds = [
        ("Inherit from parent", Auth::Inherit),
        ("No auth", Auth::None),
        ("Bearer token", Auth::Bearer { token: user() }),
        (
            "Basic auth",
            Auth::Basic {
                username: user(),
                password: pass(),
            },
        ),
        (
            "Digest auth",
            Auth::Digest {
                username: user(),
                password: pass(),
            },
        ),
        ("OAuth 2.0", Auth::OAuth2(model::OAuth2::default())),
    ];
    let label_of = |a: &Auth| {
        let d = std::mem::discriminant(a);
        kinds
            .iter()
            .find(|(_, k)| std::mem::discriminant(k) == d)
            .map_or("", |(l, _)| *l)
    };
    let current = std::mem::discriminant(&*auth);
    egui::ComboBox::from_id_salt("auth")
        .selected_text(label_of(auth))
        .show_ui(ui, |ui| {
            for (label, kind) in &kinds {
                let same = std::mem::discriminant(kind) == current;
                if ui.selectable_label(same, *label).clicked() && !same {
                    *auth = kind.clone();
                }
            }
        });
    ui.add_space(4.0);
    let text = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
        ui.label(label);
        var_edit(
            ui,
            egui::Id::new(("auth", label)),
            value,
            vars,
            egui::TextStyle::Body,
            false,
            |e| e.hint_text(hint).desired_width(420.0),
        );
        ui.end_row();
    };
    let secret = |ui: &mut egui::Ui, label: &str, value: &mut String| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(value)
                .password(true)
                .desired_width(260.0),
        );
        ui.end_row();
    };
    egui::Grid::new("auth-fields")
        .num_columns(2)
        .spacing([8.0, 6.0])
        .show(ui, |ui| match auth {
            Auth::Inherit | Auth::None => {}
            Auth::Bearer { token } => text(ui, "Token", token, "{{token}}"),
            Auth::Basic { username, password } | Auth::Digest { username, password } => {
                text(ui, "Username", username, "");
                secret(ui, "Password", password);
            }
            Auth::OAuth2(o) => {
                ui.label("Grant");
                ui.horizontal(|ui| {
                    use model::Grant;
                    ui.selectable_value(
                        &mut o.grant,
                        Grant::ClientCredentials,
                        "Client credentials",
                    );
                    ui.selectable_value(&mut o.grant, Grant::Password, "Password");
                });
                ui.end_row();
                text(
                    ui,
                    "Token URL",
                    &mut o.token_url,
                    "https://login.example.com/oauth2/token",
                );
                text(ui, "Client ID", &mut o.client_id, "");
                secret(ui, "Client secret", &mut o.client_secret);
                text(ui, "Scope", &mut o.scope, "optional, space separated");
                if o.grant == model::Grant::Password {
                    text(ui, "Username", &mut o.username, "");
                    secret(ui, "Password", &mut o.password);
                }
            }
        });
    match auth {
        Auth::Inherit => {
            ui.weak(match inherited {
                Some((folder, a)) => format!("Uses {} from folder \"{folder}\".", label_of(a)),
                None => "No folder above sets auth, so none is sent.".to_owned(),
            });
        }
        Auth::None => {}
        Auth::OAuth2(_) => {
            ui.weak("The token is fetched on Send and reused until it expires or is rejected.");
        }
        _ => {
            ui.weak(
                "Tip: use {{variables}} from a secret environment so credentials never reach git.",
            );
        }
    }
}

fn docs_editor(ui: &mut egui::Ui, description: &mut String) {
    ui.weak(
        "Markdown, for the docs a folder's right-click menu copies (\"Copy docs as Markdown\").",
    );
    ui.add(
        egui::TextEdit::multiline(description)
            .hint_text("What this is for, when to use it, what comes back…")
            .desired_rows(12)
            .desired_width(f32::INFINITY),
    );
}

fn settings_editor(ui: &mut egui::Ui, s: &mut model::Settings, default_timeout_secs: u64) {
    use model::HttpVersion;
    egui::Grid::new("settings")
        .num_columns(3)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("HTTP version");
            let name = |v: HttpVersion| match v {
                HttpVersion::Auto => "Auto",
                HttpVersion::Http1 => "HTTP/1.1",
                HttpVersion::Http2 => "HTTP/2",
            };
            egui::ComboBox::from_id_salt("http-version")
                .selected_text(name(s.http_version))
                .show_ui(ui, |ui| {
                    for v in [HttpVersion::Auto, HttpVersion::Http1, HttpVersion::Http2] {
                        ui.selectable_value(&mut s.http_version, v, name(v));
                    }
                });
            ui.weak("Auto uses HTTP/2 when an https server offers it");
            ui.end_row();

            ui.label("Follow redirects");
            ui.checkbox(&mut s.follow_redirects, "");
            ui.weak("Off shows the 3xx response itself");
            ui.end_row();

            ui.label("Maximum redirects");
            ui.add_enabled(
                s.follow_redirects,
                egui::DragValue::new(&mut s.max_redirects).range(1..=50),
            );
            ui.end_row();

            ui.label("Verify TLS certificate");
            ui.checkbox(&mut s.verify_tls, "");
            if s.verify_tls {
                ui.weak("Off accepts any certificate, for this request only");
            } else {
                ui.colored_label(RED, "Any server certificate is accepted");
            }
            ui.end_row();

            ui.label("Cookie jar");
            ui.checkbox(&mut s.cookies, "");
            ui.weak("Send stored cookies and keep the ones the server sets");
            ui.end_row();

            ui.label("Timeout");
            let default = format!("Default ({default_timeout_secs} s)");
            ui.add(
                egui::DragValue::new(&mut s.timeout_ms)
                    .range(0..=3_600_000)
                    .speed(100)
                    .custom_formatter(move |v, _| match v {
                        0.0 => default.clone(),
                        v => format!("{v} ms"),
                    })
                    .custom_parser(|t| t.trim().trim_end_matches("ms").trim().parse().ok()),
            );
            ui.weak("0 uses the network settings' timeout");
            ui.end_row();
        });
    ui.add_space(8.0);
    if ui
        .add_enabled(!s.is_default(), egui::Button::new("Reset to defaults"))
        .clicked()
    {
        *s = model::Settings::default();
    }
}

const PRE_SNIPPETS: &[(&str, &str)] = &[
    (
        "Set a request header",
        "pm.request.headers.upsert({ key: \"X-Request-Id\", value: Date.now() });\n",
    ),
    (
        "Set an environment variable",
        "pm.environment.set(\"timestamp\", Date.now());\n",
    ),
    (
        "Log a variable",
        "console.log(pm.environment.get(\"host\"));\n",
    ),
];

const POST_SNIPPETS: &[(&str, &str)] = &[
    (
        "Status code is 200",
        "pm.test(\"Status code is 200\", function () {\n    pm.response.to.have.status(200);\n});\n",
    ),
    (
        "Response time is below 500 ms",
        "pm.test(\"Response time is below 500 ms\", function () {\n    pm.expect(pm.response.responseTime).to.be.below(500);\n});\n",
    ),
    (
        "JSON body has a property",
        "pm.test(\"Body has id\", function () {\n    const json = pm.response.json();\n    pm.expect(json).to.have.property(\"id\");\n});\n",
    ),
    (
        "Body matches a JSON schema",
        "const schema = {\n    type: \"object\",\n    required: [\"id\"],\n    properties: {\n        id: { type: \"integer\" }\n    }\n};\npm.test(\"Body matches the schema\", function () {\n    pm.response.to.have.jsonSchema(schema);\n});\n",
    ),
    (
        "Header is present",
        "pm.test(\"Content-Type is present\", function () {\n    pm.response.to.have.header(\"Content-Type\");\n});\n",
    ),
    (
        "Save a JSON value to the environment",
        "pm.environment.set(\"token\", pm.response.json().token);\n",
    ),
];

/// For a request or a folder; `inherited` lists the folder scripts that run first.
fn scripts_editor(
    ui: &mut egui::Ui,
    tab: &mut ScriptTab,
    pre_request: &mut String,
    tests: &mut String,
    inherited: &Inherited,
) {
    let mut insert = None;
    ui.horizontal(|ui| {
        let label = |name: &str, s: &str| {
            if s.trim().is_empty() {
                name.to_owned()
            } else {
                format!("{name} ●")
            }
        };
        ui.selectable_value(tab, ScriptTab::Pre, label("Pre-request", pre_request));
        ui.selectable_value(tab, ScriptTab::Post, label("Post-response", tests));
        ui.separator();
        let snippets = if *tab == ScriptTab::Pre {
            PRE_SNIPPETS
        } else {
            POST_SNIPPETS
        };
        ui.menu_button("Snippets", |ui| {
            for (name, code) in snippets {
                if ui.button(*name).clicked() {
                    insert = Some(*code);
                    ui.close();
                }
            }
        });
    });
    let above = match tab {
        ScriptTab::Pre => &inherited.pre_request,
        ScriptTab::Post => &inherited.tests,
    };
    if !above.is_empty() {
        let folders: Vec<&str> = above.iter().map(|(f, _)| f.as_str()).collect();
        ui.weak(format!("Folder scripts run first: {}", folders.join(", ")));
    }
    let (text, hint) = match tab {
        ScriptTab::Pre => (
            pre_request,
            "// Runs before the request is sent.\n// pm.request, pm.environment, pm.variables, console.log",
        ),
        ScriptTab::Post => (
            tests,
            "// Runs after the response arrives.\n// pm.test(name, fn), pm.expect(...), pm.response.json()",
        ),
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
    ui.add(
        egui::TextEdit::multiline(text)
            .code_editor()
            .hint_text(hint)
            .desired_rows(12)
            .desired_width(f32::INFINITY),
    );
}

const GREEN: Color32 = Color32::from_rgb(80, 180, 100);

/// Enter meant for a dialog's OK: pressed in a single-line field (which lets go of focus on
/// Enter) or with nothing focused. A multi-line editor or a focused button keeps it. Call
/// after the dialog's fields are drawn. Consumed, so a dialog opened by this one (New
/// environment → its editor) doesn't take the same press as its own OK.
fn enter_pressed(ui: &egui::Ui) -> bool {
    ui.memory(|m| m.focused().is_none())
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter))
}

fn escape_pressed(ui: &egui::Ui) -> bool {
    ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
}

/// The button Enter presses, filled so it's clear which one that is.
fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
        .fill(Color32::from_rgb(40, 110, 200))
}

/// The methods of a `.proto`, re-read when the path changes.
type Rpcs = Option<(String, Result<Vec<crate::grpc::Rpc>, String>)>;

fn rpc_of<'a>(req: &Request, rpcs: &'a Rpcs) -> Option<&'a crate::grpc::Rpc> {
    match rpcs {
        Some((proto, Ok(list))) if req.method == "GRPC" && *proto == req.proto => {
            list.iter().find(|r| r.name == req.rpc)
        }
        _ => None,
    }
}

/// WebSocket, SSE and streaming gRPC methods connect instead of sending.
fn streams(req: &Request, rpcs: &Rpcs) -> bool {
    model::is_streaming(&req.method)
        || rpc_of(req, rpcs).is_some_and(|r| r.client_streaming || r.server_streaming)
}

fn grpc_bar(ui: &mut egui::Ui, req: &mut Request, methods: &mut Rpcs) -> Option<String> {
    let mut error = None;
    ui.horizontal(|ui| {
        ui.label("Proto");
        ui.add(
            egui::TextEdit::singleline(&mut req.proto)
                .hint_text("protos/service.proto (relative to the workspace)")
                .desired_width(260.0),
        );
        let reload = ui
            .small_button("↻")
            .on_hover_text("Reload .proto")
            .clicked();
        if reload || methods.as_ref().is_none_or(|(p, _)| *p != req.proto) {
            *methods = Some((req.proto.clone(), crate::grpc::methods(&req.proto)));
        }
        let Some((_, list)) = methods else { return };
        match list {
            Ok(list) => {
                let shown = if req.rpc.is_empty() {
                    "Select method"
                } else {
                    &req.rpc
                };
                egui::ComboBox::from_id_salt("rpc")
                    .selected_text(shown)
                    .width(ui.available_width() - 110.0)
                    .show_ui(ui, |ui| {
                        for m in list.iter() {
                            let kind = match (m.client_streaming, m.server_streaming) {
                                (false, false) => "",
                                (false, true) => "  · server stream",
                                (true, false) => "  · client stream",
                                (true, true) => "  · bidi stream",
                            };
                            let label = format!("{}{kind}", m.name);
                            ui.selectable_value(&mut req.rpc, m.name.clone(), label);
                        }
                    });
                if ui
                    .add_enabled(!req.rpc.is_empty(), egui::Button::new("Fill body"))
                    .on_hover_text("Replace the body with an empty request message")
                    .clicked()
                {
                    match crate::grpc::template(&req.proto, &req.rpc) {
                        Ok(text) => req.body = Body::Json { text },
                        Err(e) => error = Some(e),
                    }
                }
            }
            Err(_) if req.proto.trim().is_empty() => {}
            Err(e) => {
                ui.colored_label(RED, e.lines().next().unwrap_or_default())
                    .on_hover_text(e.as_str());
            }
        }
    });
    error
}

/// Returns true when Start was pressed.
fn load_ui(ui: &mut egui::Ui, view: &mut LoadView) -> bool {
    let running = view
        .stats
        .as_ref()
        .is_some_and(|s| s.lock().unwrap().finished.is_none());
    let mut start = false;
    ui.horizontal(|ui| {
        ui.strong("Load test");
        ui.separator();
        ui.add_enabled_ui(!running, |ui| {
            ui.label("Virtual users");
            ui.add(egui::DragValue::new(&mut view.vus).range(1..=500));
            ui.label("Duration (s)");
            ui.add(egui::DragValue::new(&mut view.secs).range(1..=3600));
        });
        if running {
            if ui.button("Stop").clicked()
                && let Some(a) = view.abort.take()
            {
                a.abort();
                if let Some(s) = &view.stats {
                    let mut s = s.lock().unwrap();
                    s.finished = Some(s.started.elapsed());
                }
            }
        } else {
            start = ui.button("▶ Start").clicked();
        }
    });
    ui.weak("Sends the current draft (variables resolved once). Scripts are skipped.");
    ui.separator();
    let Some(stats) = &view.stats else {
        return start;
    };
    let s = stats.lock().unwrap();
    let elapsed = s.elapsed().as_secs_f64();
    if running {
        ui.add(egui::ProgressBar::new((elapsed / view.secs as f64) as f32).show_percentage());
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
    let errors = s.error_count();
    egui::Grid::new("load-stats").striped(true).show(ui, |ui| {
        let mut row = |k: &str, v: String| {
            ui.label(k);
            ui.monospace(v);
            ui.end_row();
        };
        row("Requests", s.count.to_string());
        row(
            "Throughput",
            format!("{:.1} req/s", s.count as f64 / elapsed.max(0.001)),
        );
        let pct = if s.count > 0 {
            errors as f64 * 100.0 / s.count as f64
        } else {
            0.0
        };
        row("Errors", format!("{errors} ({pct:.1}%)"));
        if s.count > 0 {
            row(
                "Mean",
                format!("{:.1} ms", s.sum_us as f64 / s.count as f64 / 1000.0),
            );
            for p in [50.0, 90.0, 95.0, 99.0] {
                row(&format!("p{p}"), format!("{:.1} ms", s.percentile_ms(p)));
            }
            row("Max", format!("{:.1} ms", s.max_us as f64 / 1000.0));
        }
    });
    if !s.statuses.is_empty() {
        ui.add_space(6.0);
        ui.strong("Status codes");
        for (code, n) in &s.statuses {
            ui.horizontal(|ui| {
                ui.colored_label(status_color(*code), code.to_string());
                ui.monospace(n.to_string());
            });
        }
    }
    if !s.errors.is_empty() {
        ui.add_space(6.0);
        ui.strong("Errors");
        for (msg, n) in &s.errors {
            ui.horizontal(|ui| {
                ui.monospace(n.to_string());
                ui.colored_label(RED, msg.lines().next().unwrap_or_default())
                    .on_hover_text(msg.as_str());
            });
        }
    }
    start
}

impl StreamSession {
    /// Err when a gRPC message doesn't fit the method: refused here, so a typo doesn't
    /// end the call.
    fn send_compose(&mut self) -> Result<(), String> {
        let Some(tx) = &self.outgoing else {
            return Ok(());
        };
        if self.compose.is_empty() {
            return Ok(());
        }
        if let Some((proto, rpc)) = &self.grpc {
            crate::grpc::check(proto, rpc, &self.compose)?;
        }
        let _ = tx.send(self.compose.clone());
        Ok(())
    }
}

fn stream_ui(ui: &mut egui::Ui, s: &mut StreamSession) -> Result<(), String> {
    let mut sent = Ok(());
    ui.horizontal(|ui| {
        if s.live {
            ui.spinner();
            ui.label(format!(
                "Connected {:.0} s",
                s.started.elapsed().as_secs_f32()
            ));
        } else {
            ui.weak("Not connected");
        }
        ui.weak(format!("· {} events", s.events.len()));
        if ui.small_button("Clear").clicked() {
            s.events.clear();
        }
    });
    if s.outgoing.is_some() {
        ui.horizontal(|ui| {
            let send_w = 70.0;
            ui.add(
                egui::TextEdit::multiline(&mut s.compose)
                    .desired_rows(2)
                    .font(egui::TextStyle::Monospace)
                    .hint_text(if s.grpc.is_some() {
                        "Message (JSON)"
                    } else {
                        "Message"
                    })
                    .desired_width(ui.available_width() - send_w - 8.0),
            );
            if ui
                .add_sized([send_w, 22.0], egui::Button::new("Send"))
                .on_hover_text(ui.ctx().format_shortcut(&SEND))
                .clicked()
            {
                sent = s.send_compose();
            }
        });
    }
    ui.separator();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for (at, event) in &s.events {
                let (badge, color, text) = match event {
                    Event::Open(t) => ("OPEN", GREEN, t),
                    Event::In(t) => ("IN", Color32::from_rgb(90, 160, 230), t),
                    Event::Out(t) => ("OUT", ORANGE, t),
                    Event::Closed(t) => ("CLOSED", Color32::GRAY, t),
                    Event::Error(t) => ("ERROR", RED, t),
                };
                ui.horizontal_top(|ui| {
                    ui.set_min_height(row_h);
                    ui.weak(format!("{:>8.3}", at.as_secs_f32()));
                    ui.add_sized(
                        [56.0, row_h],
                        egui::Label::new(RichText::new(badge).color(color).strong().monospace()),
                    );
                    // Selectable so payloads can be copied; clipped like the body viewer.
                    let text: String = text.chars().take(MAX_LINE).collect();
                    ui.add(
                        egui::Label::new(RichText::new(text).monospace())
                            .selectable(true)
                            .wrap(),
                    );
                });
            }
        });
    sent
}

/// Returns an example to save when the user asked for one.
fn response_ui(ui: &mut egui::Ui, shown: &mut Shown, tab: &mut RespTab) -> Option<Example> {
    let mut example = None;
    let passed = shown.tests.iter().filter(|t| t.passed).count();
    ui.horizontal(|ui| {
        match &shown.result {
            Ok(view) => {
                let h = &view.head;
                ui.label(
                    RichText::new(format!("{} {}", h.status, h.reason))
                        .strong()
                        .color(status_color(h.status)),
                );
                ui.weak(format!("{} ms", h.elapsed.as_millis()));
                ui.weak(human_size(view.raw_size));
                ui.weak(&h.version);
                ui.separator();
                ui.selectable_value(tab, RespTab::Body, "Body");
                ui.selectable_value(
                    tab,
                    RespTab::Headers,
                    format!("Headers ({})", h.headers.len()),
                );
            }
            Err(_) => {
                ui.colored_label(RED, "Request failed");
                ui.separator();
                ui.selectable_value(tab, RespTab::Body, "Error");
            }
        }
        if !shown.tests.is_empty() {
            let color = if passed == shown.tests.len() {
                GREEN
            } else {
                RED
            };
            let label =
                RichText::new(format!("Tests ({passed}/{})", shown.tests.len())).color(color);
            ui.selectable_value(tab, RespTab::Tests, label);
        }
        if !shown.logs.is_empty() {
            ui.selectable_value(
                tab,
                RespTab::Console,
                format!("Console ({})", shown.logs.len()),
            );
        }
        if let Ok(view) = &mut shown.result {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Copy").on_hover_text("Copy body").clicked() {
                    ui.ctx().copy_text(view.text.clone());
                }
                if ui
                    .small_button("Save as example")
                    .on_hover_text("Keep this response with the request")
                    .clicked()
                {
                    let h = &view.head;
                    let content_type = h
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    example = Some(Example {
                        name: format!("{} {}", h.status, h.reason).trim().to_owned(),
                        status: h.status,
                        content_type,
                        body: view.text.clone(),
                    });
                }
                if *tab == RespTab::Body {
                    find_bar(ui, view);
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
    match (current, &mut shown.result) {
        (RespTab::Tests, _) => {
            egui::ScrollArea::vertical()
                .id_salt("response-tests")
                .auto_shrink(false)
                .show(ui, |ui| {
                    for t in &shown.tests {
                        ui.horizontal(|ui| {
                            let (badge, color) = if t.passed {
                                ("PASS", GREEN)
                            } else {
                                ("FAIL", RED)
                            };
                            ui.label(RichText::new(badge).monospace().strong().color(color));
                            ui.add(egui::Label::new(t.name.as_str()).selectable(true));
                        });
                        if let Some(e) = &t.error {
                            ui.add(
                                egui::Label::new(RichText::new(e).monospace().weak())
                                    .selectable(true),
                            );
                        }
                    }
                });
        }
        (RespTab::Console, _) => {
            egui::ScrollArea::vertical()
                .id_salt("response-console")
                .auto_shrink(false)
                .show(ui, |ui| {
                    for line in &shown.logs {
                        ui.add(egui::Label::new(RichText::new(line).monospace()).selectable(true));
                    }
                });
        }
        (_, Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e.as_str()).monospace()).selectable(true));
        }
        (RespTab::Headers, Ok(view)) => {
            egui::ScrollArea::vertical()
                .id_salt("response-headers")
                .auto_shrink(false)
                .show(ui, |ui| {
                    egui::Grid::new("resp-headers")
                        .num_columns(2)
                        .striped(true)
                        .show(ui, |ui| {
                            for (k, v) in &view.head.headers {
                                ui.add(
                                    egui::Label::new(RichText::new(k).strong()).selectable(true),
                                );
                                ui.add(egui::Label::new(v.as_str()).selectable(true));
                                ui.end_row();
                            }
                        });
                });
        }
        (_, Ok(view)) => {
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
            let mut area = egui::ScrollArea::both()
                .id_salt("response-body")
                .auto_shrink(false);
            if std::mem::take(&mut view.find.scroll)
                && let Some(&at) = view.find.hits.get(view.find.current)
            {
                let row = view.line_starts.partition_point(|&s| s <= at) - 1;
                // A few lines of context above the hit.
                let pitch = row_height + ui.spacing().item_spacing.y;
                area = area.vertical_scroll_offset(row.saturating_sub(3) as f32 * pitch);
            }
            let view = &*view;
            area.show_rows(ui, row_height, view.line_starts.len(), |ui, rows| {
                for row in rows {
                    let start = view.line_starts[row];
                    let end = view
                        .line_starts
                        .get(row + 1)
                        .copied()
                        .unwrap_or(view.text.len());
                    let mut cut = end.min(start + MAX_LINE);
                    while !view.text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    let end = start + view.text[start..cut].trim_end().len();
                    ui.add(egui::Label::new(highlighted(ui, view, start..end)).extend());
                }
            });
        }
    }
    example
}

fn examples_editor(ui: &mut egui::Ui, examples: &mut Vec<Example>) {
    let mut remove = None;
    for (i, ex) in examples.iter_mut().enumerate() {
        let title =
            RichText::new(format!("{} · {}", ex.status, ex.name)).color(status_color(ex.status));
        egui::CollapsingHeader::new(title)
            .id_salt(("example", i))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut ex.name);
                    ui.label("Status");
                    ui.add(egui::DragValue::new(&mut ex.status).range(100..=599));
                    if ui
                        .small_button("🗑")
                        .on_hover_text("Delete example")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                });
                ui.add(
                    egui::TextEdit::multiline(&mut ex.body)
                        .code_editor()
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
            });
    }
    if let Some(i) = remove {
        examples.remove(i);
    }
}

/// The find bar, laid out right to left next to Copy.
fn find_bar(ui: &mut egui::Ui, view: &mut ResponseView) {
    let ResponseView { text, find, .. } = view;
    let next = ui.small_button("Next").on_hover_text("Enter").clicked();
    let prev = ui
        .small_button("Prev")
        .on_hover_text("Shift+Enter")
        .clicked();
    if !find.searched.is_empty() {
        let n = find.hits.len();
        ui.weak(format!("{}/{n}", if n == 0 { 0 } else { find.current + 1 }));
    }
    let edit = ui.add(
        egui::TextEdit::singleline(&mut find.query)
            .id(egui::Id::new("find"))
            .hint_text("Find")
            .desired_width(160.0),
    );
    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    if enter {
        edit.request_focus(); // keep typing / pressing Enter for the next hit
    }
    if find.query != find.searched {
        // ASCII-only case folding keeps byte offsets identical to `text`.
        let needle = find.query.to_ascii_lowercase();
        find.hits = if needle.is_empty() {
            Vec::new()
        } else {
            let hay = text.to_ascii_lowercase();
            hay.match_indices(&needle).map(|(i, _)| i).collect()
        };
        find.searched = find.query.clone();
        find.current = 0;
        find.scroll = !find.hits.is_empty();
    }
    let n = find.hits.len();
    if n > 0 {
        let shift = ui.input(|i| i.modifiers.shift);
        if next || (enter && !shift) {
            find.current = (find.current + 1) % n;
            find.scroll = true;
        } else if prev || (enter && shift) {
            find.current = (find.current + n - 1) % n;
            find.scroll = true;
        }
    }
}

/// One body line, with find hits painted over it.
fn highlighted(
    ui: &egui::Ui,
    view: &ResponseView,
    line: std::ops::Range<usize>,
) -> egui::WidgetText {
    let (text, find) = (&view.text, &view.find);
    let len = find.searched.len();
    let first = find.hits.partition_point(|&h| h + len <= line.start);
    if len == 0 || find.hits.get(first).is_none_or(|&h| h >= line.end) {
        return RichText::new(&text[line]).monospace().into();
    }
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let plain = egui::TextFormat::simple(font.clone(), ui.visuals().text_color());
    let mark = |current| egui::TextFormat {
        background: if current {
            ORANGE
        } else {
            Color32::from_rgb(240, 220, 90)
        },
        ..egui::TextFormat::simple(font.clone(), Color32::BLACK)
    };
    let mut job = egui::text::LayoutJob::default();
    let mut pos = line.start;
    for (k, &h) in find.hits.iter().enumerate().skip(first) {
        if h >= line.end {
            break;
        }
        let (from, to) = (h.max(pos), (h + len).min(line.end));
        job.append(&text[pos..from], 0.0, plain.clone());
        job.append(&text[from..to], 0.0, mark(k == find.current));
        pos = to;
    }
    job.append(&text[pos..line.end], 0.0, plain);
    job.into()
}

fn short_method(m: &str) -> &str {
    match m {
        "DELETE" => "DEL",
        "OPTIONS" => "OPT",
        "GRAPHQL" => "GQL",
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
        "GRAPHQL" => Color32::from_rgb(229, 53, 171),
        _ => Color32::GRAY,
    }
}

/// "5 min ago": local wall-clock time would need time zone data.
fn ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match now.saturating_sub(unix_secs) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86400),
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

#[cfg(test)]
mod ui_tests {
    use egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT, Queryable};

    use super::*;

    fn workspace(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("apitool-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Workspace::open(dir).unwrap()
    }

    fn harness(ws: Workspace) -> Harness<'static, App> {
        Harness::builder()
            .with_size([1200.0, 800.0])
            .build_eframe(|_| App::new(ws, "test".into()))
    }

    /// Set APITOOL_SHOTS=1 to dump what the test sees to /tmp/apitool-shots (needs a GPU).
    fn shot(h: &mut Harness<'_, App>, name: &str) {
        if std::env::var_os("APITOOL_SHOTS").is_none() {
            return;
        }
        std::fs::create_dir_all("/tmp/apitool-shots").unwrap();
        let img = h.render().unwrap();
        img.save(format!("/tmp/apitool-shots/{name}.png")).unwrap();
    }

    /// The nth text box, not counting the sidebar filter (there whenever the tree isn't
    /// empty), so the URL is 0 with a request open. The filter is told by its placeholder,
    /// which egui exposes only while it's empty.
    fn text_input<'h>(h: &'h Harness<'_, App>, nth: usize) -> egui_kittest::Node<'h> {
        h.get_all_by_role(Role::TextInput)
            .filter(|n| n.accesskit_node().placeholder() != Some(FILTER_HINT))
            .nth(nth)
            .unwrap()
    }

    fn type_into(h: &mut Harness<'_, App>, nth: usize, text: &str) {
        text_input(h, nth).click();
        h.run();
        text_input(h, nth).type_text(text);
        h.run();
    }

    #[test]
    fn new_environment_variable_resolves_in_the_url() {
        let mut h = harness(workspace("env"));
        h.run();
        shot(&mut h, "01-empty");
        // Keyboard only: Enter is OK in every dialog, Escape cancels.
        h.get_by_label("+ New").click();
        h.run();
        type_into(&mut h, 0, "dev");
        h.key_press(Key::Enter);
        h.run();
        assert!(h.state().dialog.is_none());
        assert!(
            h.state().env_editor.is_some(),
            "the same Enter must not also save and close the new editor"
        );
        shot(&mut h, "02-env-editor");
        type_into(&mut h, 0, "host");
        let addr = crate::http::tests::echo_server();
        let host = addr.trim_end_matches("/users").to_owned();
        type_into(&mut h, 1, &host);
        shot(&mut h, "03-env-typed");
        h.key_press(Key::Enter);
        h.run();
        assert!(h.state().env_editor.is_none());
        assert_eq!(h.state().vars.get("host"), Some(&host));

        h.get_by_label("+ Folder").click();
        h.run();
        h.key_press(Key::Escape);
        h.run();
        assert!(h.state().dialog.is_none() && h.state().tree.is_empty());

        h.get_by_label("+ Request").click();
        h.run();
        type_into(&mut h, 0, "r1");
        h.key_press(Key::Enter);
        h.run();
        // The URL bar is the first text field of the request editor.
        type_into(&mut h, 0, "{{host}}/x");
        assert_eq!(h.state().open.as_ref().unwrap().draft.url, "{{host}}/x");
        assert!(h.query_by_label_contains("Undefined").is_none());
        h.get_by_label("Send").click();
        wait(&mut h, |app| {
            app.pending.is_empty() && app.response.is_some()
        });
        shot(&mut h, "04-sent");
        let shown = h.state().response.as_ref().unwrap();
        let view = shown.result.as_ref().expect("request should succeed");
        assert!(
            view.text.to_lowercase().starts_with("get /x http/1.1"),
            "{}",
            view.text
        );
    }

    #[test]
    fn new_environment_opens_ready_to_type_and_never_drops_variables() {
        let mut h = harness(workspace("env-keys"));
        h.run();
        h.get_by_label("+ New").click();
        h.run();
        type_into(&mut h, 0, "dev");
        h.get_by_label("OK").click();
        h.run();
        // No click needed: the editor opens with the key field focused. One char per
        // frame, as a physical keyboard delivers them.
        let typed = |h: &mut Harness<'_, App>, text: &str| {
            for c in text.chars() {
                h.event(egui::Event::Text(c.to_string()));
                h.step();
            }
            h.run();
        };
        typed(&mut h, "host");
        h.key_press(Key::Tab);
        h.run();
        // The value through an IME, the way Windows TSF delivers committed text.
        for c in "abc".chars() {
            let c = c.to_string();
            h.event(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: c.clone(),
                active_range_chars: None,
            }));
            h.step();
            h.event(egui::Event::Ime(egui::ImeEvent::Commit(c)));
            h.step();
        }
        // Ctrl+S saves the environment, not the request behind the editor.
        h.key_press_modifiers(Modifiers::COMMAND, Key::S);
        h.run();
        assert!(h.state().env_editor.is_none());
        assert_eq!(h.state().vars.get("host").map(String::as_str), Some("abc"));

        // Close never silently throws typed variables away.
        h.get_by_label("Edit").click();
        h.run();
        typed(&mut h, "tmp");
        h.get_by_label("Close").click();
        h.run();
        assert!(h.state().env_editor.is_some(), "first Close only warns");
        h.get_by_label("Discard changes").click();
        h.run();
        assert!(h.state().env_editor.is_none());
        assert!(!h.state().vars.contains_key("tmp"));
    }

    #[test]
    fn folder_variables_reach_the_requests_inside_without_touching_their_edits() {
        let ws = workspace("folder");
        let dir = ws.create_folder(&ws.collections(), "api").unwrap();
        let path = ws.create_request(&dir, "r").unwrap();
        let req = Request {
            url: "{{base}}/x".into(),
            ..Default::default()
        };
        ws.save_request(&path, &req).unwrap();
        let mut h = harness(ws);
        h.run();
        h.get_by_label("api").click();
        h.run();
        h.get_by_label("r").click();
        h.run();
        h.get_by_label_contains("Undefined: base");
        let draft = KeyValue::new("X-Draft", "1");
        h.state_mut()
            .open
            .as_mut()
            .unwrap()
            .draft
            .headers
            .push(draft.clone());

        h.get_by_label("api").click_secondary();
        h.run();
        h.get_by_label("Folder settings…").click();
        h.run();
        // Opens ready to type, like the environment editor.
        for c in "base".chars() {
            h.event(egui::Event::Text(c.to_string()));
            h.step();
        }
        h.key_press(Key::Tab);
        h.run();
        let host = crate::http::tests::echo_server();
        h.event(egui::Event::Text(
            host.trim_end_matches("/users").to_owned(),
        ));
        h.run();
        h.key_press_modifiers(Modifiers::COMMAND, Key::S);
        h.run();

        assert!(h.state().folder_editor.is_none());
        assert!(h.query_by_label_contains("Undefined").is_none());
        let open = h.state().open.as_ref().unwrap();
        assert!(open.draft.headers.contains(&draft), "unsaved edits kept");
        h.get_by_label("Send").click();
        wait(&mut h, |app| app.response.is_some());
        let shown = h
            .state()
            .response
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        assert!(
            shown.text.to_lowercase().starts_with("get /x "),
            "{}",
            shown.text
        );
    }

    #[test]
    fn tabs_keep_unsaved_edits_and_ask_only_when_closing() {
        let ws = workspace("tabs");
        for name in ["a", "b", "c"] {
            ws.create_request(&ws.collections(), name).unwrap();
        }
        let b = ws.collections().join("b.toml");
        let mut h = harness(ws);
        h.run();
        let tabs = |h: &Harness<'_, App>| -> Vec<String> {
            let stem = |t: &Tab| t.path.file_stem().unwrap().to_string_lossy().into_owned();
            h.state().tabs.iter().map(stem).collect()
        };
        h.get_by_label("a").click();
        h.run();
        // A plain click previews: the next one reuses the tab instead of piling them up.
        h.get_all_by_label("b").next().unwrap().click();
        h.run();
        assert_eq!(tabs(&h), ["b"]);
        // Editing keeps the tab, and switching away never interrupts with a dialog.
        type_into(&mut h, 0, "http://edited");
        h.get_all_by_label("c").next().unwrap().click();
        h.run();
        assert_eq!(tabs(&h), ["b", "c"]);
        assert!(h.state().dialog.is_none());
        h.get_by_label("b ●").click();
        h.run();
        shot(&mut h, "13-tabs");
        assert_eq!(draft(&h).url, "http://edited");

        // Quitting must count edits in background tabs too.
        h.get_all_by_label("c").last().unwrap().click();
        h.run();
        assert_eq!(h.state().unsaved(), ["b"]);
        // Closing the edited tab asks, with it in front so Save saves what's shown.
        h.get_all_by_label("×").next().unwrap().click();
        h.run();
        assert_eq!(draft(&h).url, "http://edited");
        h.get_by_label("Discard").click();
        h.run();
        assert_eq!(tabs(&h), ["c"]);
        assert_eq!(
            h.state().ws.load_request(&b).unwrap().url,
            "",
            "file untouched"
        );
        // A clean tab closes without asking.
        h.key_press_modifiers(Modifiers::COMMAND, Key::W);
        h.run();
        assert!(h.state().tabs.is_empty() && h.state().open.is_none());
    }

    /// A workspace with one request `r` (and env `dev` with `host`), opened in the app.
    fn with_request(name: &str) -> Harness<'static, App> {
        let ws = workspace(name);
        ws.create_request(&ws.collections(), "r").unwrap();
        ws.save_env(Some("dev"), &[KeyValue::new("host", "127.0.0.1:1")], &[])
            .unwrap();
        ws.save_state(&State {
            active_env: Some("dev".into()),
            ..Default::default()
        });
        let mut h = harness(ws);
        h.run();
        h.get_by_label("r").click();
        h.run();
        h
    }

    fn draft<'h>(h: &'h Harness<'_, App>) -> &'h Request {
        &h.state().open.as_ref().unwrap().draft
    }

    #[test]
    fn duplicate_opens_a_copy_of_what_is_saved() {
        let mut h = with_request("duplicate");
        h.state_mut().open.as_mut().unwrap().draft.url = "http://unsaved.test".into();
        h.run();
        h.key_press_modifiers(Modifiers::COMMAND, Key::D);
        h.run();
        let app = h.state();
        let open = app.open.as_ref().unwrap();
        assert_eq!(app.ws.display_name(&open.path), "r copy");
        assert_eq!(open.draft.url, "", "unsaved edits stay with the original");
        assert_eq!(app.tabs.len(), 2);
        assert!(
            app.tabs[0].parked.as_ref().is_some_and(|p| p.open.dirty()),
            "the original keeps its edits"
        );
        assert!(app.status.contains("r copy"), "{}", app.status);
    }

    #[test]
    fn code_panel_shows_what_send_sends_and_keeps_the_language() {
        let mut h = with_request("code");
        h.state_mut().open.as_mut().unwrap().draft.url = "{{host}}/items".into();
        h.get_by_label("</> Code").click();
        h.run();
        let snippet = |h: &Harness<'_, App>| {
            let roles = [Role::MultilineTextInput, Role::TextInput];
            let nodes = roles.into_iter().flat_map(|r| h.get_all_by_role(r));
            nodes
                .filter_map(|n| n.value())
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Variables are filled in, as for Send.
        let code = snippet(&h);
        assert!(
            code.contains("curl --location 'http://127.0.0.1:1/items'"),
            "{code}"
        );
        let picker = egui_kittest::kittest::By::new()
            .role(Role::ComboBox)
            .value("cURL");
        h.get(picker).click();
        h.run();
        h.get_by_label("Go (net/http)").click();
        h.run();
        let code = snippet(&h);
        assert!(
            code.contains(r#"http.NewRequest("GET", "http://127.0.0.1:1/items", nil)"#),
            "{code}"
        );
        shot(&mut h, "code-panel");
        let mut h = harness(h.state().ws.clone());
        h.run();
        assert_eq!(h.state().code_lang, "Go (net/http)", "kept for next time");
    }

    #[test]
    fn sidebar_filter_finds_requests_inside_closed_folders() {
        let ws = workspace("filter");
        let root = ws.collections();
        let users = ws.create_folder(&root, "users").unwrap();
        ws.create_request(&users, "list users").unwrap();
        ws.create_request(&root, "health").unwrap();
        let mut h = harness(ws);
        h.run();
        assert!(
            h.query_by_label("list users").is_none(),
            "folders start closed"
        );
        h.get_all_by_role(Role::TextInput)
            .find(|n| n.accesskit_node().placeholder() == Some(FILTER_HINT))
            .unwrap()
            .click();
        h.run();
        // One key at a time: the × appearing after the first must not take the focus.
        for c in ["L", "I", "S", "T"] {
            h.event(egui::Event::Text(c.into()));
            h.run();
        }
        assert_eq!(h.state().tree_filter, "LIST");
        h.get_by_label("list users");
        assert!(h.query_by_label("health").is_none());
        // Escape clears it, as in a search field.
        h.key_press(Key::Escape);
        h.run();
        assert_eq!(h.state().tree_filter, "");
        h.get_by_label("health");
    }

    #[test]
    fn bulk_edit_takes_pasted_lines_and_keeps_descriptions() {
        let mut h = with_request("bulk");
        h.state_mut().open.as_mut().unwrap().draft.headers = vec![KeyValue {
            description: "who is asking".into(),
            ..KeyValue::new("x-user", "bob")
        }];
        h.run();
        h.get_by_label("Headers (1)").click();
        h.run();
        h.get_by_label("Bulk Edit").click();
        h.run();
        // The only multi-line box on the Headers tab.
        let editor = |h: &Harness<'_, App>| {
            let n = h.query_by_role(Role::MultilineTextInput);
            n.map(|n| n.value().unwrap_or_default())
        };
        assert_eq!(editor(&h).as_deref(), Some("x-user: bob"));
        // Replace it all with what devtools shows, one header switched off.
        h.get_by_role(Role::MultilineTextInput).click();
        h.run();
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        h.event(egui::Event::Paste("x-user: alice\n//x-debug: 1".into()));
        h.run();
        let user = KeyValue {
            description: "who is asking".into(),
            ..KeyValue::new("x-user", "alice")
        };
        let debug = KeyValue {
            enabled: false,
            ..KeyValue::new("x-debug", "1")
        };
        assert_eq!(draft(&h).headers, [user, debug]);
        h.get_by_label("Key-Value Edit").click();
        h.run();
        assert_eq!(editor(&h), None, "back to the table");
    }

    #[test]
    fn keyboard_reaches_new_request_and_the_url() {
        let mut h = with_request("keys");
        h.state_mut().open.as_mut().unwrap().draft.url = "http://old.test/x".into();
        h.run();
        // Like a browser's address bar: focused and selected, so typing replaces it.
        h.key_press_modifiers(Modifiers::COMMAND, Key::L);
        h.run();
        h.event(egui::Event::Text("http://new.test".into()));
        h.run();
        assert_eq!(draft(&h).url, "http://new.test");
        h.key_press_modifiers(Modifiers::COMMAND, Key::N);
        h.run();
        assert!(matches!(
            h.state().dialog,
            Some(Dialog::Name {
                kind: NameKind::NewRequest(_),
                ..
            })
        ));
    }

    #[test]
    fn history_brings_back_what_was_sent() {
        let mut h = with_request("history");
        let sent = crate::http::tests::echo_server();
        type_into(&mut h, 0, &sent);
        h.get_by_label("Send").click();
        wait(&mut h, |app| app.response.is_some());
        let entry = &h.state().history[0];
        assert_eq!((entry.path.as_str(), entry.status), ("r", 200));
        assert_eq!(h.state().ws.load_history(), h.state().history, "persisted");

        // Edit after sending, then go back to what was sent.
        type_into(&mut h, 0, "/later");
        h.get_by_label("History").click();
        h.run();
        // The sidebar row comes before the request heading of the same name.
        h.get_all_by_label("r").next().unwrap().click();
        h.run();
        h.get_by_label("Discard").click(); // the unsaved "/later" edit
        h.run();
        assert_eq!(draft(&h).url, sent);
    }

    #[test]
    fn cookies_from_a_login_are_sent_back_and_can_be_deleted() {
        let addr = crate::http::tests::serve(|req| match req.starts_with("GET /login ") {
            true => ("200 OK\r\nset-cookie: sid=abc; Path=/".into(), "ok".into()),
            false => ("200 OK\r\ncontent-type: text/plain".into(), req.to_owned()),
        });
        let mut h = with_request("cookies");
        let send = |h: &mut Harness<'_, App>, path: &str| {
            h.state_mut().open.as_mut().unwrap().draft.url = format!("http://{addr}{path}");
            h.state_mut().response = None;
            h.get_by_label("Send").click();
            wait(h, |app| app.response.is_some());
        };
        send(&mut h, "/login");
        send(&mut h, "/me");
        let shown = h
            .state()
            .response
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        assert!(
            shown.text.contains("\r\ncookie: sid=abc\r\n"),
            "{}",
            shown.text
        );
        // Survives a restart.
        let saved = std::fs::read_to_string(h.state().ws.cookies_path()).unwrap();
        assert!(saved.contains("sid"), "{saved}");

        h.get_by_label("Cookies").click();
        h.run();
        h.get_by_label("sid=abc");
        h.get_by_label("🗑").click();
        h.run();
        assert!(h.state().cookies.rows().is_empty());
        assert!(
            !std::fs::read_to_string(h.state().ws.cookies_path())
                .unwrap()
                .contains("sid")
        );
    }

    /// As if the open request had just been sent and answered 200 with this body.
    fn show_response(h: &mut Harness<'_, App>, content_type: &str, body: String) {
        h.state_mut().response = Some(Shown {
            result: Ok(into_view(http::Response {
                status: 200,
                reason: "OK".into(),
                version: "HTTP/1.1".into(),
                elapsed: Duration::ZERO,
                headers: vec![("content-type".into(), content_type.into())],
                body,
            })),
            tests: Vec::new(),
            logs: Vec::new(),
        });
        h.run();
    }

    #[test]
    fn saving_an_example_keeps_unsaved_edits_out_of_the_file() {
        let mut h = with_request("example");
        type_into(&mut h, 0, "http://x/unsaved");
        show_response(&mut h, "application/json", r#"{"id":1}"#.into());
        h.get_by_label("Save as example").click();
        h.run();
        let path = h.state().open.as_ref().unwrap().path.clone();
        let on_disk = h.state().ws.load_request(&path).unwrap();
        assert_eq!(on_disk.url, "", "the unsaved URL edit stays a draft");
        assert_eq!(
            on_disk.examples,
            [Example {
                name: "200 OK".into(),
                status: 200,
                content_type: "application/json".into(),
                body: "{\n  \"id\": 1\n}".into(),
            }]
        );
        let open = h.state().open.as_ref().unwrap();
        assert!(open.dirty() && open.draft.examples.len() == 1);
        h.get_by_label("Examples (1)");
    }

    #[test]
    fn find_in_response_counts_hits_and_scrolls_to_each() {
        let mut h = with_request("find");
        let body: String = (0..500)
            .map(|i| match i % 200 {
                7 => format!("NEEDLE {i}\n"),
                _ => "hay\n".into(),
            })
            .collect();
        show_response(&mut h, "text/plain", body);
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run();
        h.event(egui::Event::Text("needle".into())); // case-insensitive
        h.run();
        h.get_by_label("1/3");
        h.key_press(Key::Enter);
        h.run();
        h.get_by_label("2/3");
        // Only rows on screen exist, so the second hit being there means we scrolled to it.
        assert!(h.query_by_label("NEEDLE 207").is_some());
        assert!(h.query_by_label("NEEDLE 7").is_none());
        h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
        h.run();
        h.get_by_label("1/3");
        assert!(h.query_by_label("NEEDLE 7").is_some());
    }

    #[test]
    fn pasting_a_curl_command_into_the_url_imports_it() {
        let mut h = with_request("curl");
        text_input(&h, 0).click();
        h.run();
        h.event(egui::Event::Paste(
            "curl 'https://api.test/x?a=1' \\\n  -H 'accept: text/plain' \\\n  --data-raw 'k=v'"
                .into(),
        ));
        h.run();
        let d = draft(&h);
        assert_eq!(
            (d.method.as_str(), d.url.as_str()),
            ("POST", "https://api.test/x?a=1")
        );
        assert_eq!(d.headers, [KeyValue::new("accept", "text/plain")]);
        assert_eq!(
            d.body,
            Body::Form {
                fields: vec![KeyValue::new("k", "v")]
            }
        );
    }

    #[test]
    fn url_query_and_params_table_stay_in_sync() {
        let mut h = with_request("params");
        type_into(&mut h, 0, "http://x/a?page=2&q=b");
        assert_eq!(
            draft(&h).params,
            [KeyValue::new("page", "2"), KeyValue::new("q", "b")]
        );
        // Inputs: URL, then key/value/description per row, then the blank row.
        type_into(&mut h, 7, "debug");
        assert_eq!(draft(&h).url, "http://x/a?page=2&q=b&debug");
        // A description documents the row; the URL has no place for it.
        type_into(&mut h, 3, "1-based");
        assert_eq!(draft(&h).params[0].description, "1-based");
        assert_eq!(draft(&h).url, "http://x/a?page=2&q=b&debug");
        // Unticking a row drops it from the URL but keeps it in the table.
        h.get_all_by_role(Role::CheckBox).next().unwrap().click();
        h.run();
        shot(&mut h, "10-params-sync");
        assert_eq!(draft(&h).url, "http://x/a?q=b&debug");
        assert_eq!(draft(&h).params.len(), 3);
    }

    #[test]
    fn typing_double_braces_autocompletes_a_variable() {
        let mut h = with_request("complete");
        type_into(&mut h, 0, "{{ho");
        shot(&mut h, "11-autocomplete");
        h.key_press(Key::Enter);
        h.run();
        assert_eq!(draft(&h).url, "{{host}}");
        // Enter picked the suggestion; it must not also have sent the request.
        assert!(h.state().pending.is_empty());
        type_into(&mut h, 0, "/x");
        assert_eq!(draft(&h).url, "{{host}}/x", "cursor lands after the braces");
        shot(&mut h, "12-highlight");
    }

    #[test]
    fn undefined_variable_is_defined_from_the_request_in_one_step() {
        let mut h = with_request("define");
        type_into(&mut h, 0, "{{base}}/x");
        h.get_by_label("Define in dev…").click();
        h.run();
        shot(&mut h, "20-define");
        // The editor opens with the missing name added and its value focused.
        let value = h
            .get_all_by_role(Role::TextInput)
            .find(|n| n.is_focused())
            .expect("value field focused");
        value.type_text("http://example.test");
        h.run();
        modal_save(&mut h);
        h.run();
        assert_eq!(h.state().vars["base"], "http://example.test");
        assert_eq!(
            h.state().active_env.as_deref(),
            Some("dev"),
            "editing keeps the env"
        );
        assert!(h.query_by_label_contains("Undefined").is_none());
    }

    #[test]
    fn globals_persist_and_appear_in_quick_look() {
        let mut h = with_request("globals");
        h.get_by_label("Globals").click();
        h.run();
        // The modal's fields come after the request editor's: shared key/value, then secret.
        let inputs = h.get_all_by_role(Role::TextInput);
        let key = inputs
            .filter(|n| n.accesskit_node().placeholder() != Some(FILTER_HINT))
            .count()
            - 4;
        type_into(&mut h, key, "apiKey");
        type_into(&mut h, key + 1, "k-123");
        modal_save(&mut h);
        h.run();
        let root = h.state().ws.root.clone();
        assert_eq!(h.state().globals["apiKey"], "k-123");
        assert!(
            std::fs::read_to_string(root.join("globals.toml"))
                .unwrap()
                .contains("k-123")
        );
        h.get_by_label("👁").click();
        h.run();
        shot(&mut h, "21-quick-look");
        assert!(h.query_by_label("k-123").is_some());
        assert!(
            h.query_by_label("127.0.0.1:1").is_some(),
            "env vars listed too"
        );
    }

    #[test]
    fn grpc_streams_connect_take_messages_and_end() {
        let mut h = with_request("grpc-stream");
        let (proto, url) = crate::grpc::tests::greeter();
        let d = &mut h.state_mut().open.as_mut().unwrap().draft;
        (d.method, d.url, d.proto) = ("GRPC".into(), url, proto);
        d.rpc = "greet.v1.Greeter/Hello".into();
        d.body = Body::Json {
            text: r#"{"name": "ada"}"#.into(),
        };
        h.run();
        assert!(
            h.query_by_label("Connect").is_none(),
            "a unary method sends"
        );
        h.state_mut().open.as_mut().unwrap().draft.rpc = "greet.v1.Greeter/Chat".into();
        h.run();
        h.get_by_label("Connect").click();
        let replies = |app: &App| {
            let s = app.stream.as_ref().unwrap();
            s.events
                .iter()
                .filter(|(_, e)| matches!(e, Event::In(_)))
                .count()
        };
        wait_live(&mut h, |app| replies(app) == 1);

        // A typo is refused before it reaches the wire; the call goes on.
        h.state_mut().stream.as_mut().unwrap().compose = r#"{"nmae": "bob"}"#.into();
        h.get_by_label("Send").click();
        h.run_steps(2);
        assert!(h.state().status.contains("nmae"), "{}", h.state().status);
        h.state_mut().stream.as_mut().unwrap().compose = r#"{"name": "bob"}"#.into();
        h.get_by_label("Send").click();
        wait_live(&mut h, |app| replies(app) == 2);
        shot(&mut h, "32-grpc-stream");

        // Half-close: the server finishes its side and the call ends cleanly.
        h.get_by_label("End stream").click();
        wait(&mut h, |app| !app.stream.as_ref().unwrap().live);
        let events: Vec<_> = h
            .state()
            .stream
            .as_ref()
            .unwrap()
            .events
            .iter()
            .map(|(_, e)| e.clone())
            .collect();
        assert_eq!(
            events,
            [
                Event::Open("calling greet.v1.Greeter/Chat".into()),
                Event::Out(r#"{"name":"ada"}"#.into()),
                Event::In(r#"{"message":"hi ada x0"}"#.into()),
                Event::Out(r#"{"name": "bob"}"#.into()),
                Event::In(r#"{"message":"hi bob x0"}"#.into()),
                Event::Closed("OK".into()),
            ]
        );
    }

    #[test]
    fn graphql_schema_explorer_writes_the_query() {
        let mut h = with_request("gql");
        // The method picker, not the method badge on the tab.
        let picker = egui_kittest::kittest::By::new()
            .role(Role::ComboBox)
            .value("GET");
        h.get(picker).click();
        h.run();
        h.get_by_label("GRAPHQL").click();
        h.run();
        assert!(matches!(draft(&h).body, Body::GraphQL { .. }));
        assert!(
            h.query_by_label("Body").is_none(),
            "the Body tab reads Query"
        );
        let url = crate::http::tests::json_server(crate::graphql::tests::sample());
        type_into(&mut h, 0, &url);
        h.get_by_label("Fetch schema").click();
        wait(&mut h, |app| !app.explorer.loading);
        shot(&mut h, "30-graphql-schema");
        h.get_by_label("user(id: ID!): User").click();
        h.run();
        shot(&mut h, "31-graphql-inserted");
        let Body::GraphQL { query, variables } = &draft(&h).body else {
            panic!("body changed type")
        };
        assert!(query.starts_with("query User($id: ID!) {"), "{query}");
        assert_eq!(variables, "{\n  \"id\": null\n}");
    }

    /// The request editor has its own Save; the modal's is drawn last.
    #[test]
    fn edits_made_while_away_show_up_when_the_window_regains_focus() {
        let mut h = with_request("refocus");
        let path = h.state().open.as_ref().unwrap().path.clone();
        // What an MCP client or a git pull does while the user is in another window.
        let external = |h: &Harness<'_, App>, url: &str| {
            let ws = &h.state().ws;
            let req = Request {
                url: url.into(),
                ..Default::default()
            };
            ws.save_request(&path, &req).unwrap();
            ws.save_env(Some("dev"), &[KeyValue::new("host", url)], &[])
                .unwrap();
            ws.create_request(&ws.collections(), url.trim_start_matches("http://"))
                .unwrap();
        };
        external(&h, "http://first");
        h.event(egui::Event::WindowFocused(true));
        h.run();
        assert_eq!(draft(&h).url, "http://first");
        assert_eq!(h.state().vars["host"], "http://first");
        h.get_by_label("first");

        // Unsaved typing must survive; the user is told instead.
        type_into(&mut h, 0, "/mine");
        external(&h, "http://second");
        h.event(egui::Event::WindowFocused(true));
        h.run();
        assert_eq!(draft(&h).url, "http://first/mine");
        assert!(h.state().status.contains("changed on disk"));
        h.get_by_label("second");
    }

    fn modal_save(h: &mut Harness<'_, App>) {
        h.get_all_by_label("Save").last().unwrap().click();
        h.run();
    }

    fn wait(h: &mut Harness<'_, App>, done: impl Fn(&App) -> bool) {
        wait_live(h, done);
        h.run();
    }

    /// Without settling: a live stream's spinner keeps repainting, which `run` rejects.
    fn wait_live(h: &mut Harness<'_, App>, done: impl Fn(&App) -> bool) {
        for _ in 0..200 {
            h.step();
            if done(h.state()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for the app");
    }
}
