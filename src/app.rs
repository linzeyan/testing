use std::collections::{BTreeMap, HashMap, VecDeque};
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
/// Below this workbench width (logical px) the response goes under the request: each half
/// needs ~500 for the URL bar's buttons and the status line not to be cut off.
const NARROW: f32 = 1000.0;
const SEND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const FIND: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
const DUPLICATE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);
const NEW_REQUEST: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
const FOCUS_URL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::L);
const SWITCH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);
const FILTER_HINT: &str = "Filter by name";
/// Lines longer than this are clipped in the viewer; JSON is pretty-printed first so
/// only non-JSON minified bodies hit it.
const MAX_LINE: usize = 4096;
const RED: Color32 = Color32::from_rgb(220, 80, 80);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);
/// The raw body's languages and the Content-Type each sets; Text sends the default.
const RAW_TYPES: [(&str, &str); 4] = [
    ("Text", ""),
    ("XML", "application/xml"),
    ("HTML", "text/html"),
    ("JavaScript", "application/javascript"),
];
const ENV_COLORS: [(&str, Color32); 5] = [
    ("Red", RED),
    ("Orange", ORANGE),
    ("Green", Color32::from_rgb(80, 180, 100)),
    ("Blue", Color32::from_rgb(70, 130, 220)),
    ("Purple", Color32::from_rgb(160, 100, 220)),
];

enum Msg {
    Response(PathBuf, Box<Outcome>),
    Status(String),
    RunItem(u64, RunItem),
    RunDone(u64, Changes, Changes),
    /// GraphQL introspection result for the URL it was fetched from.
    Schema(String, Result<graphql::Schema, String>),
    /// gRPC reflection finished: how many services the server has.
    Reflected(Result<usize, String>),
}

/// Stream events kept per session, by count and by bytes; older ones scroll away.
const MAX_EVENTS: usize = 5000;
const MAX_EVENT_BYTES: usize = 8 << 20;
/// One message past this keeps its start: an MQTT message may be 1 MiB, a WebSocket one 64.
const MAX_EVENT_TEXT: usize = 64 << 10;

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
    Asserts,
    Settings,
    Examples,
    Docs,
    /// MQTT only.
    Topics,
    /// MQTT only: user properties.
    Properties,
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
    Cookies,
    Timeline,
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
    /// The response was over `MAX_PARKED_RESPONSE` and wasn't kept.
    dropped: bool,
    load: Option<LoadView>,
}

/// What a background tab's response may hold; ten tabs of 16 MiB bodies would be more
/// RAM than the VDI has free.
const MAX_PARKED_RESPONSE: usize = 1 << 20;

impl Parked {
    fn keep(&mut self, shown: Option<Shown>) {
        let size = |s: &Shown| match &s.result {
            Ok(view) => view.text.len(),
            Err(e) => e.len(),
        };
        self.dropped = shown
            .as_ref()
            .is_some_and(|s| size(s) > MAX_PARKED_RESPONSE);
        self.response = shown.filter(|_| !self.dropped);
    }
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
    path: PathBuf,
    started: Instant,
    /// Written by the connection's task, not passed through `Msg`: a queue would grow
    /// without bound while the window is minimized and nothing drains it.
    log: Arc<std::sync::Mutex<Log>>,
    /// The event shown whole below the list, counted from the first one ever logged, and
    /// its text as shown.
    selected: Option<(u64, String)>,
    /// WebSocket, MQTT and client-streaming gRPC; dropping it makes the task send a Close
    /// frame (WebSocket), DISCONNECT (MQTT) or half-close (gRPC).
    outgoing: Option<Outgoing>,
    /// gRPC: (proto, method), to check messages before they are sent.
    grpc: Option<(String, String)>,
    /// What the compose box takes, shown while it's empty.
    hint: &'static str,
    live: bool,
    compose: String,
    abort: tokio::task::AbortHandle,
}

enum Outgoing {
    Text(tokio::sync::mpsc::UnboundedSender<String>),
    Mqtt(tokio::sync::mpsc::UnboundedSender<crate::mqtt::Command>),
}

#[derive(Default)]
struct Log {
    events: VecDeque<(Duration, Event)>,
    bytes: usize,
    /// Events scrolled away or cleared, so selections can point past them.
    dropped: u64,
    /// Closed or Error has arrived.
    ended: bool,
}

impl Log {
    fn push(&mut self, at: Duration, mut event: Event) {
        let text = event_text(&mut event);
        crate::runner::clip(text, MAX_EVENT_TEXT);
        self.bytes += text.len();
        self.ended |= matches!(event, Event::Closed(_) | Event::Error(_));
        self.events.push_back((at, event));
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_EVENT_BYTES {
            let Some((_, mut gone)) = self.events.pop_front() else {
                break;
            };
            self.bytes -= event_text(&mut gone).len();
            self.dropped += 1;
        }
    }

    fn clear(&mut self) {
        self.dropped += self.events.len() as u64;
        self.events.clear();
        self.bytes = 0;
    }
}

fn event_text(e: &mut Event) -> &mut String {
    match e {
        Event::Open(t) | Event::Info(t) | Event::In(t) => t,
        Event::Out(t) | Event::Closed(t) | Event::Error(t) => t,
    }
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
    /// A JSON body's other form for the Pretty/Raw switch: as received while `text` is
    /// pretty, and the other way round. None when the body isn't valid JSON.
    other: Option<String>,
    pretty: bool,
    /// Colour JSON tokens (also when it's cut and no longer parses).
    json: bool,
    raw_size: usize,
    line_starts: Vec<usize>,
    /// Visual rows for word wrap at a given width in columns, see `wrap_rows`.
    wrapped: Option<(usize, Vec<Row>)>,
    /// Folded JSON blocks, opening line → closing line (0-based).
    folds: BTreeMap<usize, usize>,
    /// A binary body decoded for the preview, with its size in pixels; on first show.
    image: Option<Result<(egui::TextureHandle, [u32; 2]), String>>,
    find: Find,
    /// The JSON filter as typed, the one `text` shows the result of, and why it can't apply.
    filter: String,
    applied: String,
    filter_error: String,
    /// The body as shown before a filter replaced `text`.
    unfiltered: Option<String>,
}

impl Find {
    /// The options stay as they were, as in an editor's find.
    fn close(&mut self) {
        *self = Find {
            case: self.case,
            word: self.word,
            regex: self.regex,
            ..Default::default()
        };
    }
}

impl ResponseView {
    fn set_pretty(&mut self, pretty: bool) {
        if pretty == self.pretty {
            return;
        }
        let Some(other) = &mut self.other else { return };
        if let Some(body) = self.unfiltered.take() {
            self.text = body;
        }
        std::mem::swap(&mut self.text, other);
        self.pretty = pretty;
        self.apply_filter();
    }

    /// Shows what the filter selects in place of the body (pretty JSON); an empty filter
    /// brings the body back. Also re-derives what depends on `text`.
    fn apply_filter(&mut self) {
        self.applied = self.filter.clone();
        self.filter_error.clear();
        if let Some(body) = self.unfiltered.take() {
            self.text = body;
        }
        let query = self.filter.trim();
        if !query.is_empty() {
            let picked = serde_json::from_str(self.raw())
                .map_err(|e| e.to_string())
                .and_then(|json| crate::jsonpath::select(&json, query));
            match picked {
                Ok(v) => {
                    let shown = serde_json::to_string_pretty(&v).unwrap_or_default();
                    self.unfiltered = Some(std::mem::replace(&mut self.text, shown));
                }
                Err(e) => self.filter_error = e,
            }
        }
        self.line_starts = line_starts(&self.text);
        self.wrapped = None;
        self.folds.clear();
        self.find = Find::default();
    }

    /// The body as the server sent it.
    fn raw(&self) -> &str {
        match (&self.other, self.pretty) {
            (Some(raw), true) => raw,
            _ => self.unfiltered.as_deref().unwrap_or(&self.text),
        }
    }
}

/// One visual row of a wrapped body.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Row {
    start: usize,
    /// 1-based, on the first row of each line; 0 on the rows it wraps onto.
    line: u32,
    /// The row starts inside a JSON string, for colouring.
    in_string: bool,
}

/// Find-in-body state. Lives in the view, so a new response starts a fresh search.
#[derive(Default)]
struct Find {
    /// The find row shows under the response bar.
    open: bool,
    /// Put the cursor in the query on the next draw.
    focus: bool,
    query: String,
    case: bool,
    word: bool,
    regex: bool,
    /// The query and options `hits` were computed for; recomputed only when they change.
    searched: (String, bool, bool, bool),
    /// Byte ranges of matches in `ResponseView::text`.
    hits: Vec<std::ops::Range<usize>>,
    /// Why the pattern can't be used.
    error: Option<String>,
    current: usize,
    /// Scroll the body to `current` on the next frame.
    scroll: bool,
}

/// What the response pane shows for the last run of the open request.
struct Shown {
    result: Result<ResponseView, String>,
    tests: Vec<TestResult>,
    logs: Vec<String>,
    /// Its id in the request's response history.
    past: Option<i64>,
}

enum NameKind {
    NewRequest(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    /// The open request's draft, saved under a `folder/name`.
    SaveAs(PathBuf),
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
    /// Confirmed first: it replaces what was edited here since the last export.
    Import,
    /// Pasted JSON or a file path; `note` says what went wrong or didn't come over.
    Paste {
        text: String,
        note: String,
    },
    /// Where to save the response body; with `download`, where Send streams the next one.
    SaveBody {
        path: String,
        error: String,
        download: bool,
    },
    /// Ctrl+K: jump to a request or environment by typing part of its name.
    Switch {
        query: String,
        selected: usize,
    },
    /// Send asks for the request's `{{?name}}` values first, as Bruno does.
    Prompt {
        values: Vec<(String, String)>,
        download: Option<PathBuf>,
    },
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
    CopyPostman(PathBuf),
    Mock(PathBuf),
    /// Drag and drop: this request or folder into that folder.
    Move(PathBuf, PathBuf),
    /// What, next to which, after it (else before).
    Place(PathBuf, PathBuf, bool),
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
    /// At most MAX_RUN_ROWS, oldest first; the counters below cover the whole run.
    items: VecDeque<RunItem>,
    done: usize,
    failed: usize,
    /// Passed, run.
    tests: (usize, usize),
    abort: tokio::task::AbortHandle,
}

/// Results the runner pane keeps: a data file of 100 000 rows would otherwise hold every
/// row in RAM and lay all of them out each frame.
const MAX_RUN_ROWS: usize = 1000;

impl RunState {
    fn running(&self) -> bool {
        self.finished.is_none()
    }

    /// Past MAX_RUN_ROWS the oldest passing row goes: failures are what's looked for.
    fn push(&mut self, item: RunItem) {
        self.done += 1;
        self.failed += usize::from(item.failed());
        self.tests.0 += item.tests.iter().filter(|t| t.passed).count();
        self.tests.1 += item.tests.len();
        self.items.push_back(item);
        if self.items.len() > MAX_RUN_ROWS {
            let oldest = self.items.iter().position(|i| !i.failed()).unwrap_or(0);
            self.items.remove(oldest);
        }
    }
}

pub struct App {
    ws: Workspace,
    tree: Vec<Node>,
    /// Each request's latest kept status, shown on its tree row.
    statuses: HashMap<PathBuf, u16>,
    envs: Vec<String>,
    /// Oldest first.
    history: Vec<HistoryEntry>,
    show_history: bool,
    /// Sidebar filter: requests whose name contains it, and the folders leading to them.
    tree_filter: String,
    /// Set by a drop: the folders above it open on the next frame, so it stays in sight.
    reveal: Option<PathBuf>,
    /// Outlives client rebuilds; saved to the workspace after responses.
    cookies: Arc<Jar>,
    /// `auth::grants()` when OAuth tokens were last saved to the workspace.
    saved_grants: u64,
    cookie_manager: bool,
    /// The code snippet panel, and its language (kept in the workspace state).
    code: bool,
    code_lang: String,
    /// Word wrap in the response body, kept across restarts.
    wrap_response: bool,
    /// The response beside the request instead of under it (Postman's two-pane view).
    side_by_side: bool,
    /// Set each frame: the workbench is too narrow for side by side.
    narrow: bool,
    hide_sidebar: bool,
    env_colors: HashMap<String, [u8; 3]>,
    recent_filters: Vec<String>,
    raw_types: Vec<String>,
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
    /// `{{?name}}` answers: last ones offered again, kept only while the app runs (they're
    /// often secrets). `answers` is what the next Send uses, taken by it.
    prompted: HashMap<String, String>,
    answers: Option<HashMap<String, String>>,
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
        crate::auth::import(&ws.load_tokens());
        let mut app = Self {
            tree: ws.tree(),
            statuses: ws.last_statuses(),
            envs: ws.env_names(),
            history: ws.load_history(),
            show_history: false,
            tree_filter: String::new(),
            reveal: None,
            cookies: Arc::new(Jar::from_json(&ws.load_cookies())),
            saved_grants: crate::auth::grants(),
            cookie_manager: false,
            code: false,
            code_lang: Some(state.code_lang)
                .filter(|l| crate::codegen::TARGETS.iter().any(|(n, _)| l == n))
                .unwrap_or_else(|| "cURL".into()),
            wrap_response: state.wrap_response,
            side_by_side: !state.stacked,
            narrow: false,
            hide_sidebar: state.hide_sidebar,
            env_colors: state.env_colors.clone(),
            recent_filters: state.recent_filters.clone(),
            raw_types: state.raw_types.clone(),
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
            prompted: HashMap::new(),
            answers: None,
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
        let tabs = state.tabs.into_iter().filter(|p| app.ws.exists(p));
        app.tabs = tabs.map(|p| Tab::new(p, false)).collect();
        if let Some(path) = state.open.filter(|p| app.ws.exists(p)) {
            app.activate(path, true);
        }
        app
    }

    /// Offers Downloads/<request name>.<extension from the content type>.
    fn ask_save_body(&mut self) {
        let Some(Ok(view)) = self.response.as_ref().map(|s| &s.result) else {
            return;
        };
        let content_type = (view.head.headers.iter())
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map_or("", |(_, v)| v.as_str());
        let ext = match content_type {
            c if c.contains("json") => "json",
            c if c.contains("xml") => "xml",
            c if c.contains("html") => "html",
            c if c.starts_with("text/") => "txt",
            // image/png, image/svg+xml…
            c if c.starts_with("image/") => c[6..].split([';', '+']).next().unwrap_or("bin"),
            _ => "bin",
        };
        let name = self.open.as_ref().map(Open::name).unwrap_or_default();
        let file = format!("{}.{ext}", crate::store::safe_name(&name));
        self.dialog = Some(Dialog::SaveBody {
            path: self.downloads().join(file).display().to_string(),
            error: String::new(),
            download: false,
        });
    }

    /// Before Send, so no content type yet: the URL's file name if it has one
    /// (`/files/report.pdf`), else the request's name.
    fn ask_download(&mut self) {
        let Some(open) = &self.open else { return };
        let url = open.draft.url.split(['?', '#']).next().unwrap_or("");
        let last = url.rsplit('/').next().unwrap_or("");
        let file = match last.contains('.') && !last.contains("{{") {
            true => crate::store::safe_name(last),
            false => crate::store::safe_name(&open.name()),
        };
        self.dialog = Some(Dialog::SaveBody {
            path: self.downloads().join(file).display().to_string(),
            error: String::new(),
            download: true,
        });
    }

    fn downloads(&self) -> PathBuf {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        (home.map(|h| PathBuf::from(h).join("Downloads")))
            .filter(|d| d.is_dir())
            .unwrap_or_else(|| self.ws.root.clone())
    }

    fn save_body(&mut self, ctx: &egui::Context) {
        let Some(Dialog::SaveBody { path, download, .. }) = &self.dialog else {
            return;
        };
        let path = path.trim().to_owned();
        if *download {
            self.dialog = None;
            return self.send(ctx, Some(path.into()));
        }
        let Some(Ok(view)) = self.response.as_ref().map(|s| &s.result) else {
            self.dialog = None;
            return;
        };
        let body = view.head.bytes.as_deref().unwrap_or(view.raw().as_bytes());
        match std::fs::write(&path, body) {
            Ok(()) => {
                let cut = if view.head.truncated {
                    " (cut at 16 MiB)"
                } else {
                    ""
                };
                self.status = format!("Saved {}{cut} to {path}", human_size(body.len()));
                self.dialog = None;
            }
            Err(e) => {
                if let Some(Dialog::SaveBody { error, .. }) = &mut self.dialog {
                    *error = format!("{path}: {e}");
                }
            }
        }
    }

    /// HTML isn't drawn here: the browser shows it (relative links and assets won't load).
    fn open_html(&mut self) {
        let Some(Ok(view)) = self.response.as_ref().map(|s| &s.result) else {
            return;
        };
        let path = std::env::temp_dir().join("apitool-response.html");
        let opened = (std::fs::write(&path, view.raw()).map_err(|e| e.to_string()))
            .and_then(|()| crate::auth::open_browser(&path.display().to_string()));
        if let Err(e) = opened {
            self.status = e;
        }
    }

    fn save_state(&self) {
        self.ws.save_state(&State {
            active_env: self.active_env.clone(),
            open: self.open.as_ref().map(|o| o.path.clone()),
            tabs: self.tabs.iter().map(|t| t.path.clone()).collect(),
            network: self.network.clone(),
            code_lang: self.code_lang.clone(),
            wrap_response: self.wrap_response,
            stacked: !self.side_by_side,
            hide_sidebar: self.hide_sidebar,
            env_colors: self.env_colors.clone(),
            recent_filters: self.recent_filters.clone(),
            raw_types: self.raw_types.clone(),
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

    fn env_color(&self) -> Option<Color32> {
        let rgb = self
            .active_env
            .as_ref()
            .and_then(|e| self.env_colors.get(e))?;
        Some(Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
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
        self.statuses = self.ws.last_statuses();
        self.envs = self.ws.env_names();
    }

    fn export(&mut self) {
        let root = self.ws.root.clone();
        self.status = match self.ws.export(&root) {
            Ok(n) => format!("Exported {n} requests to {}", root.display()),
            Err(e) => format!("Export failed: {e}"),
        };
    }

    fn import(&mut self) {
        let root = self.ws.root.clone();
        match self.ws.import(&root, false) {
            Ok(n) => {
                self.status = format!("Imported {n} requests from {}", root.display());
                // May replace the status: an open request with unsaved edits is flagged.
                self.refresh_from_disk();
            }
            Err(e) => self.status = format!("Import failed: {e}"),
        }
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
            Err(_) if !self.ws.exists(&open.path) => {
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
            let parked = self.open.take().map(|open| {
                let mut p = Parked {
                    open,
                    response: None,
                    dropped: false,
                    load: self.load.take(),
                };
                p.keep(self.response.take());
                p
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
                    if p.dropped {
                        self.status = format!(
                            "The response was over {} and wasn't kept while the tab was in the background: send again to see it",
                            human_size(MAX_PARKED_RESPONSE)
                        );
                    }
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
        if self.tab_dirty(i) {
            self.activate(path.to_owned(), false);
            self.dialog = Some(Dialog::Unsaved(Next::Close(path.to_owned())));
        } else {
            self.drop_tab(i);
        }
    }

    fn tab_dirty(&self, i: usize) -> bool {
        let path = &self.tabs[i].path;
        match &self.tabs[i].parked {
            Some(p) => p.open.dirty(),
            None => (self.open.as_ref()).is_some_and(|o| &o.path == path && o.dirty()),
        }
    }

    /// Closes the given tabs except those with unsaved edits: one question per tab would
    /// turn "Close All" into a chore, so they stay open and the status bar says so.
    fn close_tabs(&mut self, paths: Vec<PathBuf>) {
        let mut kept = 0;
        for path in paths {
            match self.tab_index(&path) {
                Some(i) if self.tab_dirty(i) => kept += 1,
                Some(i) => self.drop_tab(i),
                None => {}
            }
        }
        if kept > 0 {
            let tabs = if kept == 1 { "tab" } else { "tabs" };
            self.status = format!("{kept} {tabs} with unsaved edits left open");
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
        let saved = self.cookies.to_json();
        if let Err(e) = saved.and_then(|json| self.ws.save_cookies(&json)) {
            self.status = e;
        }
    }

    fn save_tokens(&mut self) {
        let grants = crate::auth::grants();
        if grants == self.saved_grants {
            return;
        }
        self.saved_grants = grants;
        if let Err(e) = self.ws.save_tokens(&crate::auth::export()) {
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

    /// `download`: stream a successful body to this file instead of keeping it.
    fn send(&mut self, ctx: &egui::Context, download: Option<PathBuf>) {
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
        let answers = self.answers.take();
        let (_, missing) = open.draft.resolved(&self.all_vars());
        let asked: Vec<String> = missing.into_iter().filter(|n| n.starts_with('?')).collect();
        if !asked.is_empty() && answers.is_none() {
            let values = (asked.into_iter())
                .map(|n| {
                    let last = self.prompted.get(&n).cloned().unwrap_or_default();
                    (n, last)
                })
                .collect();
            self.dialog = Some(Dialog::Prompt { values, download });
            return;
        }
        let vars = Vars {
            env: self.vars.clone(),
            globals: self.globals.clone(),
            // This Send's own values, like a data row: never written to an environment.
            data: answers.unwrap_or_default(),
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
                    let info = runner::Info::single(name);
                    let run = runner::run(client.clone(), &info, req, vars);
                    match download {
                        Some(file) => {
                            crate::http::SINK
                                .scope(crate::http::Sink::File(file), run)
                                .await
                        }
                        None => run.await,
                    }
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
        let (req, _) = open.draft.resolved(&self.all_vars());
        let is_ws = req.method.eq_ignore_ascii_case("WS");
        let socketio = req.method == "SOCKETIO";
        let is_mqtt = req.method == "MQTT";
        let grpc = (req.method == "GRPC")
            .then(|| (crate::grpc::source(&req.proto, &req.url), req.rpc.clone()));
        let sends = is_ws
            || socketio
            || rpc_of(&open.draft, &self.grpc_methods).is_some_and(|r| r.client_streaming);
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (pub_tx, pub_rx) = tokio::sync::mpsc::unbounded_channel();
        let outgoing = match (is_mqtt, sends) {
            (true, _) => Some(Outgoing::Mqtt(pub_tx)),
            (false, true) => Some(Outgoing::Text(out_tx)),
            (false, false) => None,
        };
        let started = Instant::now();
        let log = Arc::new(std::sync::Mutex::new(Log::default()));
        let (cell, net, task_log, ctx) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
            log.clone(),
            ctx.clone(),
        );
        let task = self.rt.spawn(async move {
            let emit = |e| {
                task_log.lock().unwrap().push(started.elapsed(), e);
                ctx.request_repaint();
            };
            let tls = net.0.clone();
            match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                // MQTT isn't HTTP, but it takes the client's proxy choice (maybe from PAC).
                Ok(client) if is_mqtt => {
                    crate::mqtt::session(req, tls, client.route.clone(), pub_rx, emit).await
                }
                Ok(client) if req.method == "GRPC" => {
                    crate::grpc::stream(client, req, out_rx, emit).await
                }
                // Streams keep the default settings; only the host's certificate varies.
                Ok(client) => match client.for_settings(&Default::default(), &req.url) {
                    Ok(http) if is_ws => stream::websocket(http, req, out_rx, emit).await,
                    Ok(http) if socketio => stream::socketio(http, req, out_rx, emit).await,
                    Ok(http) if subscribes(&req) => stream::graphql(http, req, emit).await,
                    Ok(http) => stream::sse(http, req, emit).await,
                    Err(e) => emit(Event::Error(format!("Network settings: {e}"))),
                },
                Err(e) => emit(Event::Error(format!("Network settings: {e}"))),
            }
        });
        self.stream = Some(StreamSession {
            path: open.path.clone(),
            started,
            log,
            selected: None,
            outgoing,
            hint: match (&grpc, socketio) {
                (Some(_), _) => "Message (JSON)",
                (None, true) => "An event and its argument: chat {\"text\": \"hi\"}",
                (None, false) => "Message",
            },
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
                let mut log = s.log.lock().unwrap();
                log.push(s.started.elapsed(), Event::Closed("disconnected".into()));
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
        if let Some(s) = self.stream.as_mut().filter(|s| s.live)
            && s.log.lock().unwrap().ended
        {
            s.live = false;
            s.outgoing = None;
        }
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
                        run.push(item);
                    }
                    continue;
                }
                Msg::RunDone(id, env, globals) => {
                    // Persist chained variables even if the runner pane was closed meanwhile.
                    self.apply_changes(env, globals);
                    self.save_cookies();
                    self.save_tokens();
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
                Msg::Reflected(result) => {
                    // The picker asks again, and this time finds them.
                    self.grpc_methods = None;
                    self.status = match result {
                        Ok(n) => format!("The server has {n} services (gRPC reflection)"),
                        Err(e) => e,
                    };
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
            self.save_tokens();
            if !outcome.tests.is_empty() {
                let passed = outcome.tests.iter().filter(|t| t.passed).count();
                self.status = format!("Tests: {passed}/{} passed", outcome.tests.len());
            }
            let failed = outcome.response.is_err() || outcome.tests.iter().any(|t| !t.passed);
            let to_tests = failed && !outcome.tests.is_empty();
            let past = match &outcome.response {
                Ok(r) => {
                    self.statuses.insert(path.clone(), r.status);
                    (self.ws.add_response(&path, r))
                        .inspect_err(|e| self.status = format!("Keeping the response: {e}"))
                        .ok()
                }
                Err(_) => None,
            };
            let shown = || Shown {
                result: outcome.response.map(|r| shown_view(r, &self.raw_types)),
                tests: outcome.tests,
                logs: outcome.logs,
                past,
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
                p.keep(Some(shown()));
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

    /// Keeps tabs (and their unsaved drafts) pointing at moved or renamed files.
    fn follow_move(&mut self, old: &Path, new: &Path) {
        let moved = |p: &mut PathBuf| {
            if p == old {
                *p = new.to_owned();
            } else if let Ok(rest) = p.strip_prefix(old) {
                *p = new.join(rest);
            }
        };
        let parked = self.tabs.iter_mut().filter_map(|t| t.parked.as_mut());
        let opens = self.open.iter_mut().chain(parked.map(|p| &mut p.open));
        opens.for_each(|o| moved(&mut o.path));
        self.tabs.iter_mut().for_each(|t| moved(&mut t.path));
    }

    /// After a drag and drop in the tree; a reorder in place keeps the path.
    fn moved(&mut self, path: &Path, result: Result<PathBuf, String>) {
        match result {
            Ok(new) => {
                if new != path {
                    self.follow_move(path, &new);
                    self.status = format!("Moved to \"{}\"", self.ws.display_name(&new));
                }
                self.reload();
                self.save_state();
                self.reveal = Some(new);
            }
            Err(e) => self.status = e,
        }
    }

    /// Like Postman: the original keeps what was saved, and this tab, response included,
    /// becomes the new request.
    fn save_as(&mut self, old: &Path, name: &str) -> Result<PathBuf, String> {
        let new = self.ws.request_path(name)?;
        if self.ws.exists(&new) {
            return Err(format!("\"{}\" already exists", self.ws.display_name(&new)));
        }
        let Some(open) = self.open.as_mut().filter(|o| o.path == old) else {
            return Err("The request isn't open any more".into());
        };
        self.ws.save_request(&new, &open.draft)?;
        open.saved = open.draft.clone();
        self.follow_move(old, &new);
        // Another folder passes down other variables, auth and scripts.
        self.refresh_inherited();
        self.reveal = Some(new.clone());
        self.status = format!("Saved as \"{}\"", self.ws.display_name(&new));
        Ok(new)
    }

    fn submit_name(&mut self) {
        let Some(Dialog::Name { kind, name, .. }) = &mut self.dialog else {
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
            NameKind::Rename(old) => {
                let old = old.clone();
                self.ws.rename(&old, &name).map(|new| {
                    self.follow_move(&old, &new);
                    None
                })
            }
            NameKind::SaveAs(old) => {
                let old = old.clone();
                self.save_as(&old, &name).map(|_| None)
            }
        };
        let Some(Dialog::Name { kind, error, .. }) = &mut self.dialog else {
            return;
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

/// "application/json" from "application/json; charset=utf-8".
fn media_type(head: &http::Response) -> String {
    let value = (head.headers.iter())
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map_or("", |(_, v)| v.as_str());
    let media = value.split(';').next().unwrap_or_default();
    media.trim().to_ascii_lowercase()
}

/// As the user last chose for its content type: Raw if they switched to it.
fn shown_view(head: http::Response, raw_types: &[String]) -> ResponseView {
    let mut view = into_view(head);
    if raw_types.contains(&media_type(&view.head)) {
        view.set_pretty(false);
    }
    view
}

fn into_view(mut head: http::Response) -> ResponseView {
    let body = std::mem::take(&mut head.body);
    let raw_size = head.bytes.as_ref().map_or(body.len(), Vec::len);
    let json = head.is_json() || body.trim_start().starts_with(['{', '[']);
    let xml = !json && (head.headers.iter()).any(|(k, v)| k == "content-type" && v.contains("xml"));
    let pretty = match (json, xml) {
        (true, _) => http::pretty_json(&body),
        (_, true) => http::pretty_xml(&body),
        _ => None,
    };
    let (text, other) = match pretty {
        Some(pretty) => (pretty, Some(body)),
        None => (body, None),
    };
    ResponseView {
        head,
        line_starts: line_starts(&text),
        text,
        pretty: other.is_some(),
        other,
        json,
        raw_size,
        wrapped: None,
        folds: BTreeMap::new(),
        image: None,
        find: Find::default(),
        filter: String::new(),
        applied: String::new(),
        filter_error: String::new(),
        unfiltered: None,
    }
}

/// Bodies up to this size are filtered as the path is typed; past it, each run re-parses a
/// big body (a transient several times its size), so Enter applies the filter instead.
const LIVE_FILTER_MAX: usize = 1 << 20;

const MAX_RECENT_FILTERS: usize = 10;

/// To the front, once.
fn remember(recent: &mut Vec<String>, path: &str) {
    recent.retain(|r| r != path);
    recent.insert(0, path.to_owned());
    recent.truncate(MAX_RECENT_FILTERS);
}

fn filter_bar(ui: &mut egui::Ui, view: &mut ResponseView, recent: &mut Vec<String>) {
    ui.horizontal(|ui| {
        let edit = ui.add(
            egui::TextEdit::singleline(&mut view.filter)
                // Not an auto id: the × appearing would change it and drop focus.
                .id(egui::Id::new("json-filter"))
                .hint_text("Filter: $.items[*].id")
                .font(egui::TextStyle::Monospace)
                .desired_width(280.0),
        );
        let edit = edit.on_hover_text(
            "JSONPath: $, .key, ['key'], [0], [-1], [*], .*, ..key, [?(@.price < 10 && @.tag == 'x')]",
        );
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let live = view.raw_size <= LIVE_FILTER_MAX;
        if view.filter != view.applied && (live || enter) {
            view.apply_filter();
        }
        // Once typing is done: applied live, every prefix of a path would be kept too.
        let path = view.applied.trim();
        if edit.lost_focus() && !path.is_empty() && view.filter_error.is_empty() {
            remember(recent, path);
        }
        if !recent.is_empty() {
            ui.menu_button("Recent", |ui| {
                for r in recent.iter() {
                    if ui.button(RichText::new(r).monospace()).clicked() {
                        view.filter = r.clone();
                        view.apply_filter();
                    }
                }
            })
            .response
            .on_hover_text("Filters used before");
        }
        if !view.filter.is_empty()
            && ui.small_button("×").on_hover_text("Show the whole body").clicked()
        {
            view.filter.clear();
            view.apply_filter();
        }
        if !view.filter_error.is_empty() {
            ui.colored_label(ORANGE, view.filter_error.as_str());
        } else if view.filter != view.applied {
            ui.weak("Enter to apply");
        }
    });
}

/// The line that closes the `{`/`[` ending line `open`, found by indentation: pretty JSON
/// puts the closing bracket at the opening line's indent. None if `open` opens nothing.
fn fold_end(text: &str, line_starts: &[usize], open: usize) -> Option<usize> {
    let line = |i: usize| {
        let end = line_starts.get(i + 1).copied().unwrap_or(text.len());
        text[line_starts[i]..end].trim_end()
    };
    let indent = |l: &str| l.len() - l.trim_start().len();
    let head = line(open);
    if !head.ends_with(['{', '[']) {
        return None;
    }
    let depth = indent(head);
    (open + 1..line_starts.len())
        .take_while(|&i| indent(line(i)) >= depth)
        .find(|&i| indent(line(i)) == depth && line(i).trim_start().starts_with(['}', ']']))
}

/// Folded `(open, close)` lines as hidden ranges `(first, count)`: each fold hides the
/// lines after its opening one, its closing line included. Folds inside a folded one add
/// nothing.
fn hidden_ranges(folds: &BTreeMap<usize, usize>) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (&open, &close) in folds {
        if out.last().is_some_and(|&(first, n)| open < first + n) {
            continue;
        }
        out.push((open + 1, close - open));
    }
    out
}

/// The `shown`th visible line (or row), counting past hidden ranges.
fn unhide(hidden: &[(usize, usize)], shown: usize) -> usize {
    let mut i = shown;
    for &(first, n) in hidden {
        if i < first {
            break;
        }
        i += n;
    }
    i
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .filter(|&i| i < text.len())
        .collect()
}

/// Cuts each line into rows of `cols` columns, counting East Asian characters as two
/// (they're drawn twice as wide). Monospace makes this exact enough to keep `show_rows`,
/// which wrapped labels of varying height wouldn't allow. One pass over the text.
// ponytail: "wide" is everything from U+1100 up; emoji and combining marks are off by one.
fn wrap_rows(text: &str, line_starts: &[usize], cols: usize) -> Vec<Row> {
    let mut rows = Vec::with_capacity(line_starts.len());
    let (mut in_string, mut escaped) = (false, false);
    for (n, &start) in line_starts.iter().enumerate() {
        let end = line_starts.get(n + 1).copied().unwrap_or(text.len());
        let line = text[start..end].trim_end_matches(['\n', '\r']);
        rows.push(Row {
            start,
            line: n as u32 + 1,
            in_string,
        });
        let mut used = 0;
        // Where the row can break between words instead: the start of its last word, the
        // columns before it and the string state there. Never in the indentation.
        let (mut word, mut text, mut prev) = (None, false, '\0');
        for (i, c) in line.char_indices() {
            let w = if c as u32 >= 0x1100 { 2 } else { 1 };
            if c != ' ' && prev == ' ' && text {
                word = Some((i, used, in_string));
            }
            // A space may hang past the edge, as in a browser.
            if used + w > cols && c != ' ' {
                let (at, before, in_string) = word.take().unwrap_or((i, used, in_string));
                rows.push(Row {
                    start: start + at,
                    line: 0,
                    in_string,
                });
                used -= before;
            }
            used += w;
            text |= c != ' ';
            prev = c;
            match c {
                _ if escaped => escaped = false,
                '\\' if in_string => escaped = true,
                '"' => in_string = !in_string,
                _ => {}
            }
        }
        // Valid JSON has no raw newline in a string; don't let a stray quote colour the rest.
        in_string = false;
    }
    rows
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Token {
    Key,
    Str,
    Num,
    /// true, false, null
    Lit,
    Punct,
}

/// JSON tokens on one row as (end byte, kind), for colouring. Works a row at a time, so
/// it never looks past what's on screen; a string that runs on past the row counts as a
/// value, since the colon that would make it a key isn't in sight.
fn json_tokens(row: &str, in_string: bool) -> Vec<(usize, Token)> {
    let b = row.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let string_end = |mut i: usize| {
        while i < b.len() && b[i] != b'"' {
            i += if b[i] == b'\\' { 2 } else { 1 };
        }
        (i + 1).min(b.len())
    };
    if in_string {
        i = string_end(0);
        out.push((i, Token::Str));
    }
    while i < b.len() {
        let (end, kind) = match b[i] {
            b'"' => {
                let end = string_end(i + 1);
                let rest = row[end..].trim_start();
                (
                    end,
                    if rest.starts_with(':') {
                        Token::Key
                    } else {
                        Token::Str
                    },
                )
            }
            b'-' | b'0'..=b'9' => {
                let n = b[i..]
                    .iter()
                    .position(|c| !matches!(c, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'));
                (n.map_or(b.len(), |n| i + n), Token::Num)
            }
            b't' | b'f' | b'n' => {
                let n = b[i..].iter().position(|c| !c.is_ascii_alphabetic());
                (n.map_or(b.len(), |n| i + n), Token::Lit)
            }
            _ => {
                let n = b[i..]
                    .iter()
                    .position(|c| matches!(c, b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'));
                (n.map_or(b.len(), |n| i + n), Token::Punct)
            }
        };
        // Never stall on a byte none of the arms consumed.
        let end = end.max(i + 1).min(b.len());
        match out.last_mut() {
            Some((e, k)) if *k == kind => *e = end,
            _ => out.push((end, kind)),
        }
        i = end;
    }
    out
}

fn token_color(ui: &egui::Ui, t: Token) -> Color32 {
    let dark = ui.visuals().dark_mode;
    match (t, dark) {
        (Token::Key, true) => Color32::from_rgb(156, 210, 254),
        (Token::Key, false) => Color32::from_rgb(4, 81, 165),
        (Token::Str, true) => Color32::from_rgb(206, 145, 120),
        (Token::Str, false) => Color32::from_rgb(163, 21, 21),
        (Token::Num, true) => Color32::from_rgb(181, 206, 168),
        (Token::Num, false) => Color32::from_rgb(9, 134, 88),
        (Token::Lit, true) => Color32::from_rgb(86, 156, 214),
        (Token::Lit, false) => Color32::from_rgb(0, 0, 255),
        (Token::Punct, _) => ui.visuals().weak_text_color(),
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
            if let Some(Shown {
                result: Ok(view), ..
            }) = &mut self.response
            {
                (view.find.open, view.find.focus) = (true, true);
            }
        }
        if ui.input_mut(|i| i.consume_shortcut(&SEND)) {
            let vars = self.all_vars();
            let mqtt = self.open.as_ref().map(|o| &o.draft.mqtt);
            match self.stream.as_mut().filter(|s| s.live) {
                Some(s) => {
                    if let Err(e) = s.send_compose(mqtt, &vars) {
                        self.status = e;
                    }
                }
                None => self.send(ui.ctx(), None),
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
        if ui.input_mut(|i| i.consume_shortcut(&SWITCH))
            && self.dialog.is_none()
            && self.env_editor.is_none()
            && self.folder_editor.is_none()
        {
            self.dialog = Some(Dialog::Switch {
                query: String::new(),
                selected: 0,
            });
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

        // The active environment's colour across the top: hard to miss when it's prod.
        if let Some(color) = self.env_color() {
            egui::Panel::top("env-color")
                .exact_size(3.0)
                .frame(egui::Frame::NONE.fill(color))
                .show(ui, |_| {});
        }
        self.status_bar(ui);
        if !self.hide_sidebar {
            egui::Panel::left("sidebar")
                .default_size(260.0)
                .show(ui, |ui| self.sidebar(ui));
        }
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
                let mut sidebar = !self.hide_sidebar;
                if (ui.toggle_value(&mut sidebar, "Sidebar"))
                    .on_hover_text("Show or hide collections and history, for more room")
                    .changed()
                {
                    self.hide_sidebar = !sidebar;
                    self.save_state();
                }
                ui.separator();
                let hint = match self.narrow {
                    true => "The window is too narrow: the response is under the request until it is wider",
                    false => "The response beside the request instead of under it",
                };
                if ui
                    .toggle_value(&mut self.side_by_side, "Side by side")
                    .on_hover_text(hint)
                    .changed()
                {
                    self.save_state();
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
            let mut label = RichText::new(
                self.active_env
                    .clone()
                    .unwrap_or_else(|| "No environment".into()),
            );
            if let Some(color) = self.env_color() {
                label = label.color(color).strong();
            }
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
            if let Some(name) = self.active_env.clone() {
                let dot = RichText::new("●").color(self.env_color().unwrap_or(Color32::GRAY));
                ui.menu_button(dot, |ui| {
                    let mut pick = |ui: &mut egui::Ui, label: RichText, rgb: Option<[u8; 3]>| {
                        if ui.button(label).clicked() {
                            match rgb {
                                Some(rgb) => self.env_colors.insert(name.clone(), rgb),
                                None => self.env_colors.remove(&name),
                            };
                            self.save_state();
                            ui.close();
                        }
                    };
                    for (label, c) in ENV_COLORS {
                        pick(
                            ui,
                            RichText::new(format!("● {label}")).color(c),
                            Some([c.r(), c.g(), c.b()]),
                        );
                    }
                    pick(ui, RichText::new("None"), None);
                })
                .response
                .on_hover_text("Colour this environment, e.g. prod in red");
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
                if ui.button("Copy as Postman collection").clicked() {
                    self.copy_postman(&root, ui.ctx());
                    ui.close();
                }
                if ui.button("Start mock server").clicked() {
                    self.start_mock(root.clone(), ui.ctx());
                    ui.close();
                }
                ui.separator();
                if ui
                    .button("Import…")
                    .on_hover_text(
                        "A Postman collection or environment, or an OpenAPI/Swagger spec",
                    )
                    .clicked()
                {
                    self.dialog = Some(Dialog::Paste {
                        text: String::new(),
                        note: String::new(),
                    });
                    ui.close();
                }
                let export = ui.button("Export to files").on_hover_text(
                    "Writes collections/, environments/ and globals.toml into the workspace \
                     folder, for git. Secrets, history and cookies stay out.",
                );
                if export.clicked() {
                    self.export();
                    ui.close();
                }
                let import = ui
                    .button("Import from files…")
                    .on_hover_text("Reads those files back, e.g. after a git pull");
                if import.clicked() {
                    self.dialog = Some(Dialog::Import);
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
                    let switch = ui.ctx().format_shortcut(&SWITCH);
                    let filter = filter
                        .on_hover_text(format!("{switch} jumps to any request or environment"));
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
        let reveal = self.reveal.take();
        let expand =
            |p: &Path| !query.is_empty() || reveal.as_ref().is_some_and(|r| r.starts_with(p));
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                if self.tree.is_empty() {
                    ui.weak("No requests yet. Click \"+ Request\".");
                } else if nodes.is_empty() {
                    ui.weak("Nothing matches.");
                }
                tree_ui(ui, nodes, selected, &self.statuses, &expand, &mut actions);
                // The empty space under the tree takes a drop to the top level.
                let size = egui::vec2(ui.available_width(), ui.available_height().max(24.0));
                let rest = ui.allocate_response(size, egui::Sense::hover());
                drop_into(ui, &rest, &self.ws.collections(), &mut actions);
            });
        // What is being dragged follows the pointer.
        if let Some(path) = egui::DragAndDrop::payload::<PathBuf>(ui.ctx())
            && let Some(pos) = ui.ctx().pointer_interact_pos()
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("tree-drag"));
            ui.ctx().layer_painter(layer).text(
                pos + egui::vec2(14.0, 0.0),
                egui::Align2::LEFT_CENTER,
                path.file_stem().unwrap_or_default().to_string_lossy(),
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
        }
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
                TreeAction::CopyPostman(dir) => self.copy_postman(&dir, ui.ctx()),
                TreeAction::Mock(dir) => self.start_mock(dir, ui.ctx()),
                TreeAction::Move(path, folder) => {
                    let moved = self.ws.move_into(&path, &folder);
                    self.moved(&path, moved);
                }
                TreeAction::Place(path, target, after) => {
                    let moved = self.ws.place(&path, &target, after);
                    self.moved(&path, moved);
                }
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
                if crate::store::is_request(&new) && !self.runner_busy() {
                    self.runner = None;
                    self.activate(new, true);
                }
            }
            Err(e) => self.status = e,
        }
    }

    /// Closes the dialog when everything came over; else it stays to say what didn't.
    fn submit_import(&mut self, input: &str) {
        let result = self.import_text(input);
        let Some(Dialog::Paste { text, note }) = &mut self.dialog else {
            return;
        };
        match result {
            Ok(warnings) if warnings.is_empty() => self.dialog = None,
            Ok(warnings) => {
                const SHOWN: usize = 12;
                let mut lines: Vec<String> = warnings
                    .iter()
                    .take(SHOWN)
                    .map(|w| format!("• {w}"))
                    .collect();
                if warnings.len() > SHOWN {
                    lines.push(format!("… and {} more", warnings.len() - SHOWN));
                }
                text.clear();
                *note = format!(
                    "{}. Not carried over as it was:\n{}",
                    self.status,
                    lines.join("\n")
                );
            }
            Err(e) => *note = e,
        }
    }

    /// For Postman's Import > Raw text.
    fn copy_postman(&mut self, dir: &Path, ctx: &egui::Context) {
        self.status = match crate::postman::collection(&self.ws, dir) {
            Ok((json, count, skipped)) => {
                ctx.copy_text(json);
                let left = match skipped {
                    0 => String::new(),
                    n => format!("; left out {n} WebSocket/SSE/gRPC, which collections can't hold"),
                };
                format!("Copied {count} requests as a Postman collection{left}")
            }
            Err(e) => e,
        };
    }

    /// `input` is the JSON, or the path to its file (quoted, as Explorer's "Copy as path"
    /// gives it). What didn't come over as it was comes back to show the user.
    fn import_text(&mut self, input: &str) -> Result<Vec<String>, String> {
        let input = input.trim();
        // One line that isn't JSON is a path; YAML is never a single line worth importing.
        let text = match input.starts_with('{') || input.contains('\n') {
            true => input.to_owned(),
            false => {
                let path = input.trim_matches('"');
                std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
            }
        };
        let warnings = match crate::import::parse(&text)? {
            crate::postman::Import::Collection {
                name,
                folders,
                requests,
                warnings,
                environments,
            } => {
                let dir = self.ws.add_tree(&name, &folders, &requests)?;
                let name = self.ws.display_name(&dir);
                for (env, shared, secret) in &environments {
                    self.add_env(env, shared, secret)?;
                }
                self.status = format!("Imported {} requests into \"{name}\"", requests.len());
                match environments.len() {
                    0 => {}
                    1 => self.status += " and 1 environment",
                    n => self.status += &format!(" and {n} environments"),
                }
                warnings
            }
            crate::postman::Import::Environment {
                name,
                shared,
                secret,
            } => {
                let name = self.add_env(&name, &shared, &secret)?;
                self.status = format!("Imported environment \"{name}\"");
                Vec::new()
            }
        };
        self.reload();
        Ok(warnings)
    }

    /// Never replaces one: a same-named environment may hold this machine's secrets.
    fn add_env(
        &mut self,
        name: &str,
        shared: &[crate::model::KeyValue],
        secret: &[crate::model::KeyValue],
    ) -> Result<String, String> {
        let names = self.ws.env_names();
        let name = std::iter::once(name.to_owned())
            .chain((1..).map(|n| crate::store::copy_name(name, n)))
            .find(|n| !names.contains(n))
            .expect("some name is free");
        self.ws.save_env(Some(&name), shared, secret)?;
        Ok(name)
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
                Ok(path) if self.ws.exists(&path) => {
                    let draft = Box::new(e.request.clone());
                    self.runner = None;
                    self.restore(path, draft);
                }
                _ => self.status = format!("\"{}\" no longer exists", e.path),
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        enum Menu {
            Duplicate,
            Others,
            Right,
            All,
        }
        let active = self.open.as_ref().map(|o| o.path.clone());
        let (mut show, mut close, mut menu) = (None, None, None);
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
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
                    label.context_menu(|ui| {
                        let ctx = ui.ctx().clone();
                        let shortcut = |s| ctx.format_shortcut(s);
                        let item =
                            egui::Button::new("Duplicate Tab").shortcut_text(shortcut(&DUPLICATE));
                        if ui.add(item).clicked() {
                            menu = Some((i, Menu::Duplicate));
                        }
                        ui.separator();
                        let item =
                            egui::Button::new("Close Tab").shortcut_text(shortcut(&CLOSE_TAB));
                        if ui.add(item).clicked() {
                            close = Some(tab.path.clone());
                        }
                        for (label, action) in [
                            ("Close Other Tabs", Menu::Others),
                            ("Close Tabs to the Right", Menu::Right),
                            ("Close All Tabs", Menu::All),
                        ] {
                            if ui.button(label).clicked() {
                                menu = Some((i, action));
                            }
                        }
                    });
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
        if let Some((i, action)) = menu {
            let paths = self.tabs.iter().map(|t| t.path.clone());
            let paths: Vec<PathBuf> = match action {
                Menu::Duplicate => return self.duplicate(&self.tabs[i].path.clone()),
                Menu::Others => paths
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, p)| p)
                    .collect(),
                Menu::Right => paths.skip(i + 1).collect(),
                Menu::All => paths.collect(),
            };
            self.close_tabs(paths);
        } else if let Some(path) = close {
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
        let (mut download, mut save_as) = (false, false);
        let mut define: Option<Vec<String>> = None;
        let mut fetch_schema = false;
        let mut reflect = false;
        let mut example = None;
        let pending = self.pending.iter().find(|p| p.path == open.path);
        // The folders above the request, outermost first, for the breadcrumb.
        let root = self.ws.collections();
        let crumbs: Vec<(String, PathBuf)> = (open.path.ancestors().skip(1))
            .take_while(|d| d.starts_with(&root) && *d != root)
            .map(|d| {
                (
                    d.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    d.to_path_buf(),
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let mut folder_clicked: Option<PathBuf> = None;
        // Before `streaming`: whether a gRPC method streams comes from its proto.
        let grpc_source = (open.draft.method == "GRPC").then(|| {
            let url = open.draft.resolved(&all_vars).0.url;
            crate::grpc::source(&open.draft.proto, &url)
        });
        if let Some(source) = &grpc_source
            && (self.grpc_methods.as_ref()).is_none_or(|(p, _)| p != source)
        {
            let methods = crate::grpc::methods(source);
            self.grpc_methods = Some((source.clone(), methods));
        }
        let streaming = streams(&open.draft, &self.grpc_methods);
        let session = self.stream.as_mut().filter(|s| s.path == open.path);
        let live = session.as_ref().is_some_and(|s| s.live);
        let half_close = session
            .as_ref()
            .is_some_and(|s| s.grpc.is_some() && s.outgoing.is_some());

        // As in Postman: beside both the request and its response, so edits show live.
        let mut lang_changed = false;
        let mut save_file = false;
        let mut open_html = false;
        let wrap_before = self.wrap_response;
        let filters_before = self.recent_filters.first().cloned();
        let mut raw_changed = false;
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
                        Ok(code) if code.len() > crate::varedit::MAX_EDIT => {
                            ui.weak(format!(
                                "This snippet is {}: too big to show here. Copy still copies all of it.",
                                human_size(code.len())
                            ));
                        }
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

        // Too narrow for two columns of URL bar and JSON: stacked whatever the toggle says,
        // like yaak and insomnia; widening the window puts it back.
        self.narrow = ui.available_width() < NARROW;
        // Separate ids, so each layout keeps its own dragged size.
        let panel = match self.side_by_side && !self.narrow {
            true => egui::Panel::left("request-side").default_size(ui.available_width() / 2.0),
            false => egui::Panel::top("request").default_size(320.0),
        };
        panel.resizable(true).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (name, dir) in &crumbs {
                    let link = ui.link(RichText::new(name).weak());
                    if link.on_hover_text("Folder settings").clicked() {
                        folder_clicked = Some(dir.clone());
                    }
                    ui.weak("›");
                }
                ui.heading(open.name());
                if open.dirty() {
                    ui.colored_label(ORANGE, "●")
                        .on_hover_text("Unsaved changes");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Right to left: this lands to the right of Save.
                    let more = ui.button("⏷");
                    egui::Popup::menu(&more)
                        .id(egui::Id::new("save-more"))
                        .show(|ui| {
                            save_as = ui
                                .button("Save as…")
                                .on_hover_text(
                                    "Save these edits as a new request; this one stays as saved",
                                )
                                .clicked();
                        });
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
                // gRPC answers aren't read as an HTTP body, so they can't go to a file.
                let downloadable = !streaming && open.draft.method != "GRPC";
                let more = match downloadable {
                    true => 20.0 + ui.spacing().item_spacing.x,
                    false => 0.0,
                };
                let width = ui.available_width() - button[0] - 8.0 - more;
                let hint = match open.draft.method.as_str() {
                    "MQTT" => "mqtt://{{broker}}:1883",
                    "SOCKETIO" => "http://{{host}}:3000/namespace",
                    _ => "https://{{host}}/path",
                };
                let url = var_edit(
                    ui,
                    egui::Id::new("url"),
                    &mut open.draft.url,
                    &all_vars,
                    egui::TextStyle::Monospace,
                    false,
                    &[],
                    |e| e.hint_text(hint).desired_width(width),
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
                if url.changed() {
                    open.draft.path_vars_from_url();
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
                if downloadable {
                    let button = ui.add_enabled(
                        pending.is_none(),
                        egui::Button::new("⏷").min_size(egui::vec2(20.0, 22.0)),
                    );
                    egui::Popup::menu(&button)
                        .id(egui::Id::new("send-more"))
                        .show(|ui| {
                            download = ui
                                .button("Send and download…")
                                .on_hover_text("Save the response straight to a file, for big ones")
                                .clicked();
                        });
                }
            });
            if let Some(source) = &grpc_source {
                let (error, ask) = grpc_bar(ui, &mut open.draft, source, &mut self.grpc_methods);
                if let Some(e) = error {
                    self.status = e;
                }
                reflect |= ask;
            }
            let (_, mut missing) = open.draft.resolved(&all_vars);
            // `{{?name}}` is asked for on Send, not missing.
            missing.retain(|n| !n.starts_with('?'));
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
                let count =
                    |kv: &[KeyValue]| kv.iter().filter(|p| p.enabled && !p.key.is_empty()).count();
                let tab = |n: usize, name: &str| {
                    if n > 0 {
                        format!("{name} ({n})")
                    } else {
                        name.to_owned()
                    }
                };
                let dot = |none: bool, name: &str| {
                    if none {
                        name.to_owned()
                    } else {
                        format!("{name} ●")
                    }
                };
                // MQTT has no query, headers or body: topics take their place.
                let mqtt = open.draft.method == "MQTT";
                let http_only = [ReqTab::Params, ReqTab::Headers, ReqTab::Body];
                let mqtt_only = [ReqTab::Topics, ReqTab::Properties];
                if mqtt && http_only.contains(&self.req_tab) {
                    self.req_tab = ReqTab::Topics;
                } else if !mqtt && mqtt_only.contains(&self.req_tab) {
                    self.req_tab = ReqTab::Params;
                }
                if mqtt {
                    let topics = open.draft.mqtt.topics.iter();
                    let on = topics.filter(|t| t.enabled && !t.filter.is_empty()).count();
                    ui.selectable_value(&mut self.req_tab, ReqTab::Topics, tab(on, "Topics"));
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Properties,
                        tab(count(&open.draft.mqtt.user_properties), "Properties"),
                    );
                } else {
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Params,
                        tab(
                            count(&open.draft.params) + count(&open.draft.path_vars),
                            "Params",
                        ),
                    );
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Headers,
                        tab(count(&open.draft.headers), "Headers"),
                    );
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Body,
                        if open.draft.method == "GRAPHQL" {
                            "Query".to_owned()
                        } else {
                            dot(matches!(open.draft.body, Body::None), "Body")
                        },
                    );
                }
                ui.selectable_value(
                    &mut self.req_tab,
                    ReqTab::Auth,
                    dot(
                        matches!(open.draft.effective_auth(), Auth::None | Auth::Inherit),
                        "Auth",
                    ),
                );
                let no_scripts =
                    open.draft.pre_request.trim().is_empty() && open.draft.tests.trim().is_empty();
                ui.selectable_value(
                    &mut self.req_tab,
                    ReqTab::Scripts,
                    dot(no_scripts, "Scripts"),
                );
                ui.selectable_value(
                    &mut self.req_tab,
                    ReqTab::Asserts,
                    tab(count(&open.draft.asserts), "Asserts"),
                );
                // How a single HTTP exchange goes out, or MQTT's connection; other
                // streams and gRPC have none.
                if !matches!(
                    open.draft.method.as_str(),
                    "WS" | "SSE" | "GRPC" | "SOCKETIO"
                ) {
                    let default = match mqtt {
                        true => {
                            open.draft.mqtt.client_id.is_empty()
                                && !open.draft.mqtt.v5
                                && open.draft.mqtt.keep_alive_secs == 60
                                && open.draft.mqtt.clean_session
                                && open.draft.mqtt.will_topic.trim().is_empty()
                        }
                        false => open.draft.settings.is_default(),
                    };
                    ui.selectable_value(
                        &mut self.req_tab,
                        ReqTab::Settings,
                        dot(default, "Settings"),
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
                        path_vars_table(ui, &mut open.draft.path_vars, &all_vars);
                    }
                    ReqTab::Headers => {
                        kv_table(ui, "headers", &mut open.draft.headers, &all_vars, true);
                        let (wire, _) = open.draft.resolved(&all_vars);
                        auto_headers_ui(ui, http::auto_headers(wire, &open.draft.headers));
                    }
                    ReqTab::Body => {
                        // GRAPHQL requests are always a query; no body type to pick.
                        if open.draft.method == "GRAPHQL"
                            && let Body::GraphQL { query, variables } = &mut open.draft.body
                        {
                            fetch_schema |=
                                graphql_editor(ui, query, variables, &all_vars, &mut self.explorer);
                            return;
                        }
                        fetch_schema |= body_editor(
                            ui,
                            &mut open.draft.body,
                            &mut open.draft.headers,
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
                    ReqTab::Scripts => {
                        let json_response = match self.response.as_ref().map(|s| &s.result) {
                            Some(Ok(view))
                                if (view.head.headers.iter()).any(|(k, v)| {
                                    k.eq_ignore_ascii_case("content-type") && v.contains("json")
                                }) =>
                            {
                                Some(view.raw())
                            }
                            _ => None,
                        };
                        scripts_editor(
                            ui,
                            &mut self.script_tab,
                            &mut open.draft.pre_request,
                            &mut open.draft.tests,
                            &open.draft.inherited,
                            json_response,
                        )
                    }
                    ReqTab::Asserts => {
                        kv_table(ui, "asserts", &mut open.draft.asserts, &all_vars, false);
                        ui.add_space(8.0);
                        ui.weak(ASSERTS_HINT);
                    }
                    ReqTab::Settings if open.draft.method == "MQTT" => {
                        mqtt_settings(ui, &mut open.draft.mqtt)
                    }
                    ReqTab::Settings => {
                        settings_editor(ui, &mut open.draft.settings, self.network.timeout_secs)
                    }
                    ReqTab::Topics => {
                        if topics_editor(ui, &mut open.draft.mqtt.topics, live)
                            && let Some(s) = session.as_deref()
                        {
                            s.resubscribe(&open.draft, &all_vars);
                        }
                    }
                    ReqTab::Properties => {
                        let m = &mut open.draft.mqtt;
                        kv_table(
                            ui,
                            "mqtt-properties",
                            &mut m.user_properties,
                            &all_vars,
                            true,
                        );
                        ui.add_space(8.0);
                        ui.weak(match m.v5 {
                            true => "Sent with every message you publish.",
                            false => "User properties need MQTT 5.0 (Settings).",
                        });
                    }
                    ReqTab::Examples => examples_editor(ui, &mut open.draft.examples),
                    ReqTab::Docs => docs_editor(ui, &mut open.draft.description),
                });
        });

        // ponytail: read from SQLite every frame (an indexed query for at most
        // MAX_RESPONSES rows); cache it per request if it ever shows up in a profile.
        let mut past = Past {
            list: self.ws.responses(&open.path),
            pick: None,
            delete: None,
            clear: false,
        };
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
                        if let Err(e) = stream_ui(ui, s, &mut open.draft.mqtt, &all_vars) {
                            self.status = e;
                        }
                    }
                    None if open.draft.method == "MQTT" => {
                        ui.weak("Press Connect to reach the broker; it subscribes to the topics in the Topics tab. Scripts don't run for MQTT.");
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
                    ui.horizontal(|ui| {
                        ui.weak(format!("Press Send or {} to see the response.", ui.ctx().format_shortcut(&SEND)));
                        past_menu(ui, &mut past, None);
                    });
                    shortcut_list(ui);
                }
                Some(shown) => {
                    let pretty_before = shown.result.as_ref().ok().map(|v| v.pretty);
                    let wrap = &mut self.wrap_response;
                    let tab = &mut self.resp_tab;
                    let recent = &mut self.recent_filters;
                    example = response_ui(ui, shown, tab, wrap, &mut save_file, &mut open_html, &mut past, recent);
                    if let Ok(view) = &shown.result
                        && pretty_before != Some(view.pretty)
                    {
                        let media = media_type(&view.head);
                        self.raw_types.retain(|t| *t != media);
                        if !view.pretty {
                            self.raw_types.push(media);
                        }
                        raw_changed = true;
                    }
                }
            }
        });
        let path = self.open.as_ref().map(|o| o.path.clone());
        if past.clear
            && let Some(path) = &path
        {
            match self.ws.clear_responses(path) {
                Ok(()) => {
                    self.statuses.remove(path);
                    self.status = "Cleared this request's response history".into();
                }
                Err(e) => self.status = e,
            }
        }
        if let Some(id) = past.delete {
            match self.ws.delete_response(id) {
                Ok(()) => {
                    // Still on screen, but no longer one History can bring back.
                    if let Some(shown) = self.response.as_mut().filter(|s| s.past == Some(id)) {
                        shown.past = None;
                    }
                }
                Err(e) => self.status = e,
            }
        }
        if let Some(id) = past.pick {
            match self.ws.load_response(id) {
                // Tests and console output aren't kept: the response is what's looked back at.
                Ok(r) => {
                    self.response = Some(Shown {
                        result: Ok(shown_view(r, &self.raw_types)),
                        tests: Vec::new(),
                        logs: Vec::new(),
                        past: Some(id),
                    })
                }
                Err(e) => self.status = format!("Reading that response: {e}"),
            }
        }
        if let Some(example) = example {
            self.save_example(example);
        }
        if lang_changed
            || wrap_before != self.wrap_response
            || filters_before != self.recent_filters.first().cloned()
            || raw_changed
        {
            self.save_state();
        }
        if save_file {
            self.ask_save_body();
        }
        if open_html {
            self.open_html();
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
            self.send(ui.ctx(), None);
        }
        if download {
            self.ask_download();
        }
        if save_as && let Some(open) = &self.open {
            let copy = format!("{} copy", self.ws.display_name(&open.path));
            self.dialog = Some(Dialog::name(NameKind::SaveAs(open.path.clone()), copy));
        }
        if reflect {
            self.reflect(ui.ctx());
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
        if let Some(dir) = folder_clicked {
            self.reveal = Some(dir.clone());
            self.open_folder_editor(dir);
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

    fn reflect(&mut self, ctx: &egui::Context) {
        let Some(open) = &self.open else {
            return;
        };
        let (req, _) = open.draft.resolved(&self.all_vars());
        self.status = "Asking the server for its methods…".into();
        let (cell, net, tx, ctx) = (
            self.client.clone(),
            (self.network.clone(), self.cookies.clone()),
            self.tx.clone(),
            ctx.clone(),
        );
        self.rt.spawn(async move {
            let result = match cell
                .get_or_init(|| net::build_client_with_jar(net.0, net.1))
                .await
            {
                Ok(client) => match client.grpc_for(&req.url) {
                    Ok(grpc) => crate::grpc::reflect(&grpc, &req).await,
                    Err(e) => Err(format!("Network settings: {e}")),
                },
                Err(e) => Err(format!("Network settings: {e}")),
            };
            let _ = tx.send(Msg::Reflected(result));
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
        let root = self.ws.root.display().to_string();
        let mut cancel = false;
        let mut then: Option<Then> = None;
        let targets = match dialog {
            Dialog::Switch { .. } => switch_targets(&self.tree, &self.ws.collections(), &self.envs),
            _ => Vec::new(),
        };
        let mut modal = egui::Modal::new(egui::Id::new("dialog"));
        if let Dialog::Switch { .. } = dialog {
            // Pinned to the top: centred, it would jump as the list grows and shrinks.
            let area = egui::Modal::default_area(egui::Id::new("dialog"))
                .anchor(egui::Align2::CENTER_TOP, [0.0, 80.0]);
            modal = modal.area(area);
        }
        let modal = modal.show(ctx, |ui| {
            ui.set_width(360.0);
            match dialog {
                Dialog::Name { kind, name, error } => {
                    ui.heading(match kind {
                        NameKind::NewRequest(_) => "New request",
                        NameKind::NewFolder(_) => "New folder",
                        NameKind::Rename(_) => "Rename",
                        NameKind::SaveAs(_) => "Save as",
                        NameKind::NewEnv => "New environment",
                        NameKind::DuplicateEnv(_) => "Duplicate environment",
                    });
                    if let NameKind::SaveAs(_) = kind {
                        ui.weak("A folder in front puts it there: users/get user");
                    }
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
                    let what = if !crate::store::is_request(path) {
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
                Dialog::SaveBody {
                    path,
                    error,
                    download,
                } => {
                    ui.heading(match download {
                        true => "Send and download",
                        false => "Save response body",
                    });
                    if *download {
                        ui.weak("A successful response goes straight to this file, however big.");
                    }
                    let edit = ui.add(
                        egui::TextEdit::singleline(path)
                            .hint_text("File path")
                            .desired_width(f32::INFINITY),
                    );
                    let enter = enter_pressed(ui);
                    if !enter && ui.memory(|m| m.focused().is_none()) {
                        edit.request_focus();
                    }
                    if !error.is_empty() {
                        ui.colored_label(RED, error.as_str());
                    }
                    ui.horizontal(|ui| {
                        let label = if *download { "Send" } else { "Save" };
                        if ui.add(primary(label)).clicked() || enter {
                            then = Some(Box::new(|app, ctx| app.save_body(ctx)));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Prompt { values, .. } => {
                    ui.heading("Values for this request");
                    let mut enter = false;
                    egui::Grid::new("prompts").num_columns(2).show(ui, |ui| {
                        for (i, (name, value)) in values.iter_mut().enumerate() {
                            ui.label(name.trim_start_matches('?'));
                            let edit =
                                ui.add(egui::TextEdit::singleline(value).desired_width(240.0));
                            enter |= enter_pressed(ui);
                            if i == 0 && !enter && ui.memory(|m| m.focused().is_none()) {
                                edit.request_focus();
                            }
                            ui.end_row();
                        }
                    });
                    ui.weak("Asked by {{?name}}; kept until apitool closes, never saved.");
                    ui.horizontal(|ui| {
                        if ui.add(primary("Send")).clicked() || enter {
                            then = Some(Box::new(|app, ctx| {
                                let Some(Dialog::Prompt { values, download }) = app.dialog.take()
                                else {
                                    return;
                                };
                                app.prompted.extend(values.iter().cloned());
                                app.answers = Some(values.into_iter().collect());
                                app.send(ctx, download);
                            }));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Switch { query, selected } => {
                    // Taken before the field sees them: they'd move its cursor.
                    ui.input_mut(|i| {
                        if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                            *selected += 1;
                        }
                        if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                            *selected = selected.saturating_sub(1);
                        }
                    });
                    let edit = ui.add(
                        egui::TextEdit::singleline(query)
                            .hint_text("Go to a request, folder, environment or action")
                            .desired_width(f32::INFINITY),
                    );
                    let enter = enter_pressed(ui);
                    if !enter && ui.memory(|m| m.focused().is_none()) {
                        edit.request_focus();
                    }
                    let mut hits: Vec<_> = (targets.iter())
                        .filter_map(|t| fuzzy(&t.0, query).map(|rank| (rank, t)))
                        .collect();
                    hits.sort_by_key(|(rank, _)| *rank);
                    hits.truncate(12);
                    *selected = (*selected).min(hits.len().saturating_sub(1));
                    let mut go = None;
                    for (i, (_, (label, badge, target))) in hits.iter().enumerate() {
                        let row = ui.horizontal(|ui| {
                            let color = match target {
                                Go::Request(_) => method_color(badge),
                                Go::Folder(_) | Go::Env(_) | Go::Action(_) => {
                                    ui.visuals().weak_text_color()
                                }
                            };
                            ui.add_sized(
                                [44.0, 18.0],
                                egui::Label::new(
                                    RichText::new(badge.as_str())
                                        .monospace()
                                        .small()
                                        .color(color),
                                ),
                            );
                            ui.add(egui::Button::selectable(i == *selected, label.as_str()))
                        });
                        if row.inner.clicked() || (enter && i == *selected) {
                            go = Some(target.clone());
                        }
                    }
                    if hits.is_empty() {
                        ui.weak("Nothing matches.");
                    }
                    if let Some(go) = go {
                        then = Some(Box::new(move |app, _| {
                            app.dialog = None;
                            match go {
                                Go::Request(path) => {
                                    app.reveal = Some(path.clone());
                                    app.activate(path, true);
                                }
                                Go::Folder(dir) => {
                                    app.reveal = Some(dir.clone());
                                    app.open_folder_editor(dir);
                                }
                                Go::Env(name) => app.set_env(Some(name)),
                                Go::Action(action) => app.act(action),
                            }
                        }));
                    }
                }
                Dialog::Import => {
                    ui.heading("Import from files");
                    ui.label(format!(
                        "Read collections/, environments/ and globals.toml in {root}? \
                         Requests, folder settings and shared variables of the same name are \
                         replaced. Nothing is deleted, and secrets stay."
                    ));
                    ui.horizontal(|ui| {
                        if ui.add(primary("Import")).clicked() || enter_pressed(ui) {
                            then = Some(Box::new(|app, _| {
                                app.dialog = None;
                                app.import();
                            }));
                        }
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Paste { text, note } => {
                    ui.heading("Import a collection or spec");
                    ui.label(
                        "A Postman collection (v2.1) or environment, or an OpenAPI 3 / Swagger 2 \
                         spec (JSON or YAML): paste it or the file's path, or drop the file \
                         here. Nothing already here is replaced.",
                    );
                    // An exported collection is often past MAX_EDIT once pasted.
                    let id = egui::Id::new("postman-text");
                    let edit = match crate::varedit::too_big(ui, id, text) {
                        Some(note) => note,
                        None => {
                            egui::ScrollArea::vertical()
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::TextEdit::multiline(text)
                                            .id(id)
                                            .hint_text("JSON, YAML or a path")
                                            .code_editor()
                                            .desired_rows(4)
                                            .desired_width(f32::INFINITY),
                                    )
                                })
                                .inner
                        }
                    };
                    // Ready for Ctrl+V.
                    if ui.memory(|m| m.focused().is_none()) {
                        edit.request_focus();
                    }
                    if !note.is_empty() {
                        ui.colored_label(ORANGE, note.as_str());
                    }
                    let dropped = ui.input(|i| {
                        let file = i.raw.dropped_files.first();
                        file.map(|f| f.path().display().to_string())
                    });
                    ui.horizontal(|ui| {
                        let typed = !text.trim().is_empty();
                        let import = ui.add_enabled(typed, primary("Import")).clicked();
                        if let Some(input) = dropped.or(import.then(|| text.clone())) {
                            then = Some(Box::new(move |app, _| app.submit_import(&input)));
                        }
                        cancel = ui.button("Close").clicked();
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
                        // A folder's tests run on many responses, not the one shown.
                        None,
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
                // 120 names don't fit a line: name the common ones, autocomplete has the rest.
                ui.weak(format!(
                    "Always available: $guid, $timestamp, $randomInt, $randomEmail… \
                     ({} in all; type {{{{$ for the list)",
                    model::DYNAMIC.len()
                ));
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
            items: VecDeque::new(),
            done: 0,
            failed: 0,
            tests: (0, 0),
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
            let (done, failed, (tests_passed, tests_total)) = (run.done, run.failed, run.tests);
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
            if done > run.items.len() {
                ui.weak(format!(
                    "Showing {} of {done}: failures and the latest passes.",
                    run.items.len()
                ));
            }
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
                    field(ui, &mut net.proxy_url, "http://user:pass@proxy.corp:8080 or socks5h://host:1080");
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
            let pfx = |path: &str| path.to_lowercase().ends_with(".pfx") || path.to_lowercase().ends_with(".p12");
            field(ui, &mut net.client_cert, "Client certificate (.pem with key, or .pfx / .p12)");
            if pfx(&net.client_cert) {
                ui.add(
                    egui::TextEdit::singleline(&mut net.client_cert_password)
                        .password(true)
                        .hint_text("PFX password")
                        .desired_width(f32::INFINITY),
                );
            }
            let mut removed = None;
            for (i, c) in net.host_certs.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut c.host).hint_text("*.corp.com or host:8443").desired_width(150.0));
                    let width = if pfx(&c.cert) { 220.0 } else { 330.0 };
                    ui.add(egui::TextEdit::singleline(&mut c.cert).hint_text("Certificate; empty sends none").desired_width(width));
                    if pfx(&c.cert) {
                        ui.add(egui::TextEdit::singleline(&mut c.password).password(true).hint_text("PFX password").desired_width(100.0));
                    }
                    if ui.small_button("🗑").on_hover_text("Remove").clicked() {
                        removed = Some(i);
                    }
                });
            }
            if let Some(i) = removed {
                net.host_certs.remove(i);
            }
            if ui.button("+ Certificate for a host").on_hover_text("Checked in order before the one above").clicked() {
                net.host_certs.push(Default::default());
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

#[derive(Clone)]
enum Go {
    Request(PathBuf),
    /// Opens its settings.
    Folder(PathBuf),
    Env(String),
    Action(Action),
}

/// What a button somewhere already does, for whoever's hands are on the keyboard.
#[derive(Clone, Copy)]
enum Action {
    NewRequest,
    NewFolder,
    NewEnv,
    Globals,
    RunCollection,
    ImportAny,
    Network,
    Cookies,
    Sidebar,
    SideBySide,
}

const ACTIONS: [(&str, Action); 10] = [
    ("New request", Action::NewRequest),
    ("New folder", Action::NewFolder),
    ("New environment", Action::NewEnv),
    ("Edit globals", Action::Globals),
    ("Run collection", Action::RunCollection),
    ("Import (Postman, OpenAPI, Swagger)", Action::ImportAny),
    ("Network settings (proxy, certificates)", Action::Network),
    ("Cookies", Action::Cookies),
    ("Show or hide the sidebar", Action::Sidebar),
    ("Side by side or stacked", Action::SideBySide),
];

impl App {
    fn act(&mut self, action: Action) {
        let root = self.ws.collections();
        match action {
            Action::NewRequest => {
                self.dialog = Some(Dialog::name(NameKind::NewRequest(root), ""));
            }
            Action::NewFolder => self.dialog = Some(Dialog::name(NameKind::NewFolder(root), "")),
            Action::NewEnv => self.dialog = Some(Dialog::name(NameKind::NewEnv, "")),
            Action::Globals => self.open_env_editor(None, &[]),
            Action::RunCollection => self.open_runner(root),
            Action::ImportAny => {
                self.dialog = Some(Dialog::Paste {
                    text: String::new(),
                    note: String::new(),
                });
            }
            Action::Network => self.network_editor = Some(self.network.clone()),
            Action::Cookies => self.cookie_manager = true,
            Action::Sidebar => {
                self.hide_sidebar = !self.hide_sidebar;
                self.save_state();
            }
            Action::SideBySide => {
                self.side_by_side = !self.side_by_side;
                self.save_state();
            }
        }
    }
}

/// What Ctrl+K can jump to or do: (label, badge, target). Requests and folders are labelled by
/// their place in the tree, so two "get user"s in different folders can be told apart.
fn switch_targets(nodes: &[Node], root: &Path, envs: &[String]) -> Vec<(String, String, Go)> {
    fn walk(nodes: &[Node], root: &Path, out: &mut Vec<(String, String, Go)>) {
        let label = |path: &Path| {
            let rel = path.strip_prefix(root).unwrap_or(path).with_extension("");
            rel.to_string_lossy().replace('\\', "/")
        };
        for node in nodes {
            match node {
                Node::Folder { path, children, .. } => {
                    out.push((label(path), "DIR".to_owned(), Go::Folder(path.clone())));
                    walk(children, root, out);
                }
                Node::Request { path, method, .. } => {
                    let badge = short_method(method).to_owned();
                    out.push((label(path), badge, Go::Request(path.clone())));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, root, &mut out);
    out.extend((envs.iter()).map(|e| (e.clone(), "ENV".to_owned(), Go::Env(e.clone()))));
    out.extend(
        (ACTIONS.iter()).map(|(label, a)| ((*label).to_owned(), "CMD".to_owned(), Go::Action(*a))),
    );
    out
}

/// Case-insensitive: `query`'s letters in `label` in order, or None. Lower ranks first: a
/// plain substring before scattered letters, then the earlier match, then the shorter label.
fn fuzzy(label: &str, query: &str) -> Option<(bool, usize, usize)> {
    let (label, query) = (label.to_lowercase(), query.trim().to_lowercase());
    if let Some(at) = label.find(&query) {
        return Some((false, at, label.len()));
    }
    let mut rest = label.chars();
    let scattered = query.chars().filter(|c| !c.is_whitespace());
    scattered
        .clone()
        .all(|c| rest.any(|l| l == c))
        .then_some((true, 0, label.len()))
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

/// Folders for which `expand` is true are opened, so what a filter found or a drop moved
/// is in view.
fn tree_ui(
    ui: &mut egui::Ui,
    nodes: &[Node],
    selected: Option<&Path>,
    statuses: &HashMap<PathBuf, u16>,
    expand: &dyn Fn(&Path) -> bool,
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
                    .open(expand(path).then_some(true))
                    .show(ui, |ui| {
                        tree_ui(ui, children, selected, statuses, expand, actions)
                    });
                let header = &resp.header_response;
                header
                    .interact(egui::Sense::drag())
                    .dnd_set_drag_payload(path.clone());
                // The header's top edge is "before this folder"; the rest is "into it".
                let edge = header.rect.top() + header.rect.height() / 3.0;
                match ui.ctx().pointer_latest_pos().is_some_and(|p| p.y < edge) {
                    true => drop_beside(ui, header, path, actions),
                    false => drop_into(ui, header, path, actions),
                }
                header.context_menu(|ui| folder_menu(ui, path, name, actions));
                more_button(ui, header, path, |ui| folder_menu(ui, path, name, actions));
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
                        let row =
                            ui.selectable_label(selected == Some(path.as_path()), name.as_str());
                        if let Some(&status) = statuses.get(path) {
                            let text = RichText::new(status.to_string())
                                .small()
                                .color(status_color(status));
                            ui.label(text).on_hover_text("Last response");
                        }
                        row
                    })
                    .inner;
                if resp.double_clicked() {
                    actions.push(TreeAction::Open(path.clone(), true));
                } else if resp.clicked() {
                    actions.push(TreeAction::Open(path.clone(), false));
                }
                resp.interact(egui::Sense::drag())
                    .dnd_set_drag_payload(path.clone());
                drop_beside(ui, &resp, path, actions);
                resp.context_menu(|ui| request_menu(ui, path, name, actions));
                more_button(ui, &resp, path, |ui| request_menu(ui, path, name, actions));
            }
        }
    }
}

fn folder_menu(ui: &mut egui::Ui, path: &Path, name: &str, actions: &mut Vec<TreeAction>) {
    let mut item = |ui: &mut egui::Ui, label: &str, a: TreeAction| {
        if ui.button(label).clicked() {
            actions.push(a);
            ui.close();
        }
    };
    let dialog = |kind| TreeAction::Dialog(Dialog::name(kind, ""));
    let path = path.to_path_buf();
    item(
        ui,
        "New request",
        dialog(NameKind::NewRequest(path.clone())),
    );
    item(ui, "New folder", dialog(NameKind::NewFolder(path.clone())));
    let rename = Dialog::name(NameKind::Rename(path.clone()), name);
    item(ui, "Rename", TreeAction::Dialog(rename));
    item(ui, "Duplicate", TreeAction::Duplicate(path.clone()));
    item(
        ui,
        "Delete",
        TreeAction::Dialog(Dialog::Delete(path.clone())),
    );
    ui.separator();
    item(
        ui,
        "Folder settings…",
        TreeAction::FolderSettings(path.clone()),
    );
    item(
        ui,
        "Copy docs as Markdown",
        TreeAction::CopyDocs(path.clone()),
    );
    item(
        ui,
        "Copy as Postman collection",
        TreeAction::CopyPostman(path.clone()),
    );
    item(ui, "Start mock server", TreeAction::Mock(path.clone()));
    item(ui, "Run folder", TreeAction::Run(path));
}

fn request_menu(ui: &mut egui::Ui, path: &Path, name: &str, actions: &mut Vec<TreeAction>) {
    if ui.button("Rename").clicked() {
        let rename = Dialog::name(NameKind::Rename(path.to_path_buf()), name);
        actions.push(TreeAction::Dialog(rename));
        ui.close();
    }
    let duplicate =
        egui::Button::new("Duplicate").shortcut_text(ui.ctx().format_shortcut(&DUPLICATE));
    if ui.add(duplicate).clicked() {
        actions.push(TreeAction::Duplicate(path.to_path_buf()));
        ui.close();
    }
    if ui.button("Delete").clicked() {
        actions.push(TreeAction::Dialog(Dialog::Delete(path.to_path_buf())));
        ui.close();
    }
}

/// A "⋯" at the right end of a hovered tree row, opening the row's right-click menu:
/// right-clicking is easy to miss. The hover area is the full row width, so the pointer
/// can travel to the button.
fn more_button(
    ui: &mut egui::Ui,
    row: &egui::Response,
    path: &Path,
    menu: impl FnOnce(&mut egui::Ui),
) {
    let id = egui::Id::new(("tree-more", path));
    let line = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), row.rect.y_range());
    if !ui.rect_contains_pointer(line) && !egui::Popup::is_id_open(ui.ctx(), id) {
        return;
    }
    let size = egui::vec2(22.0, row.rect.height());
    let rect = egui::Rect::from_min_size(egui::pos2(line.right() - size.x, line.top()), size);
    let button = ui.put(rect, egui::Button::new("⋯").small());
    egui::Popup::menu(&button).id(id).show(menu);
}

/// A tree drop target: outlined while something is held over it; a release moves that into
/// `folder`.
fn drop_into(ui: &egui::Ui, resp: &egui::Response, folder: &Path, actions: &mut Vec<TreeAction>) {
    if resp.dnd_hover_payload::<PathBuf>().is_some() {
        let stroke = ui.visuals().selection.stroke;
        (ui.painter()).rect_stroke(resp.rect, 2.0, stroke, egui::StrokeKind::Inside);
    }
    if let Some(path) = resp.dnd_release_payload::<PathBuf>() {
        actions.push(TreeAction::Move((*path).clone(), folder.to_owned()));
    }
}

/// A tree drop target for arranging: a release over the row's upper half puts what is held
/// before `target` (in target's folder), over the lower half after it; a line shows where.
fn drop_beside(ui: &egui::Ui, resp: &egui::Response, target: &Path, actions: &mut Vec<TreeAction>) {
    let after = (ui.ctx().pointer_latest_pos()).is_some_and(|p| p.y > resp.rect.center().y);
    if resp.dnd_hover_payload::<PathBuf>().is_some() {
        let y = if after {
            resp.rect.bottom()
        } else {
            resp.rect.top()
        };
        let stroke = egui::Stroke::new(2.0, ui.visuals().selection.stroke.color);
        ui.painter().hline(ui.max_rect().x_range(), y, stroke);
    }
    if let Some(path) = resp.dnd_release_payload::<PathBuf>() {
        actions.push(TreeAction::Place((*path).clone(), target.to_owned(), after));
    }
}

const HEADER_NAMES: &[(&str, &str)] = &[
    ("Accept", "media types the client takes"),
    ("Accept-Encoding", "gzip, deflate, br"),
    ("Accept-Language", "en-US, zh-TW"),
    ("Authorization", "credentials (or use the Auth tab)"),
    ("Cache-Control", "no-cache, max-age=0"),
    ("Connection", "keep-alive, close"),
    ("Content-Disposition", "attachment; filename=…"),
    ("Content-Encoding", "gzip"),
    ("Content-Type", "media type of the body"),
    ("Cookie", "name=value; …"),
    ("If-Match", "ETag to match"),
    ("If-Modified-Since", "HTTP date"),
    ("If-None-Match", "ETag from an earlier response"),
    ("Origin", "for CORS"),
    ("Pragma", "no-cache"),
    ("Range", "bytes=0-1023"),
    ("Referer", "the page linking here"),
    ("User-Agent", "client name and version"),
    ("X-API-Key", "API key (or use the Auth tab)"),
    ("X-Correlation-ID", "id to trace a call across services"),
    ("X-Forwarded-For", "client IP behind a proxy"),
    ("X-Request-ID", "id for this request"),
    ("X-Requested-With", "XMLHttpRequest"),
];

const CONTENT_TYPES: &[(&str, &str)] = &[
    ("application/json", ""),
    ("application/xml", ""),
    ("application/x-www-form-urlencoded", ""),
    (
        "multipart/form-data",
        "set by the Body tab's form-data mode",
    ),
    ("text/plain", ""),
    ("text/html", ""),
    ("text/csv", ""),
    ("application/octet-stream", "raw bytes"),
];

/// What Send adds to the user's headers, greyed and folded away by default like Postman's
/// hidden headers: they explain the Timeline without crowding the table.
fn auto_headers_ui(ui: &mut egui::Ui, auto: Vec<(String, String, &str)>) {
    if auto.is_empty() {
        return;
    }
    ui.add_space(6.0);
    let title = RichText::new(format!("{} added on Send", auto.len())).weak();
    egui::CollapsingHeader::new(title)
        .id_salt("auto-headers")
        .show(ui, |ui| {
            egui::Grid::new("auto-headers-grid")
                .num_columns(3)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    for (name, value, from) in auto {
                        ui.weak(name);
                        ui.weak(clip(&value, 80)).on_hover_text(value);
                        ui.weak(format!("from {from}"));
                        ui.end_row();
                    }
                });
        });
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
    // The request's own headers table: other tables (params, form, folder headers…) get
    // no header names.
    let headers = id == "headers";
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
    // Narrower in a side-by-side pane, so the value keeps most of the room.
    let key_width = (ui.available_width() * 0.25).clamp(100.0, 200.0);
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
                if headers { HEADER_NAMES } else { &[] },
                |e| e.hint_text("Key").desired_width(key_width),
            );
            var_edit(
                ui,
                egui::Id::new((id, i, 1)),
                &mut row.value,
                vars,
                style,
                false,
                match headers && row.key.trim().eq_ignore_ascii_case("content-type") {
                    true => CONTENT_TYPES,
                    false => &[],
                },
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

/// Postman's Path Variables: one row per `/:name` in the URL. The names follow the URL, so
/// only values and descriptions are edited here, laid out like `kv_table`'s rows.
fn path_vars_table(ui: &mut egui::Ui, rows: &mut [KeyValue], vars: &HashMap<String, String>) {
    if rows.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.strong("Path Variables");
    // Narrower in a side-by-side pane, so the value keeps most of the room.
    let key_width = (ui.available_width() * 0.25).clamp(100.0, 200.0);
    let rest = (ui.available_width() - key_width - 90.0).max(120.0);
    for (i, row) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add_visible(false, egui::Checkbox::new(&mut true, ""));
            // A `&str` buffer is read-only but looks like the key fields above.
            ui.add(egui::TextEdit::singleline(&mut row.key.as_str()).desired_width(key_width));
            var_edit(
                ui,
                egui::Id::new(("path_vars", i, 1)),
                &mut row.value,
                vars,
                egui::TextStyle::Body,
                false,
                &[],
                |e| e.hint_text("Value").desired_width(rest * 0.6),
            );
            ui.add(
                egui::TextEdit::singleline(&mut row.description)
                    .id(egui::Id::new(("path_vars", i, 2)))
                    .hint_text("Description")
                    .desired_width(rest * 0.4 - ui.spacing().item_spacing.x),
            );
        });
    }
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
    headers: &mut Vec<KeyValue>,
    vars: &HashMap<String, String>,
    explorer: &mut Explorer,
) -> bool {
    ui.horizontal(|ui| {
        // ponytail: switching to None/Form drops the text; keep a per-mode stash if that bites.
        // JSON <-> Text keeps the text, since that switch is usually a content-type correction.
        // Built on a click only: the text moves over instead of being cloned every frame.
        type Make = fn(String) -> Body;
        let options: [(&str, Make); 7] = [
            ("None", |_| Body::None),
            ("JSON", |text| Body::Json { text }),
            ("Text", |text| Body::Text { text }),
            ("Form", |_| Body::Form { fields: Vec::new() }),
            ("Multipart", |_| Body::Multipart { parts: Vec::new() }),
            ("Binary", |_| Body::File {
                path: String::new(),
            }),
            ("GraphQL", |_| Body::GraphQL {
                query: String::new(),
                variables: String::new(),
            }),
        ];
        let kind = std::mem::discriminant(&*body);
        let mut chosen = None;
        for (label, make) in options {
            let current = std::mem::discriminant(&make(String::new())) == kind;
            if ui.selectable_label(current, label).clicked() && !current {
                chosen = Some(make);
            }
        }
        if let Some(make) = chosen {
            let text = match body {
                Body::Json { text } | Body::Text { text } => std::mem::take(text),
                _ => String::new(),
            };
            *body = make(text);
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
                // Parsing it each frame is what an uneditable body can do without.
                if !text.trim().is_empty()
                    && text.len() <= crate::varedit::MAX_EDIT
                    && let Err(e) = serde_json::from_str::<serde::de::IgnoredAny>(text)
                {
                    // Only a hint: `{{var}}` placeholders legitimately make the raw text invalid.
                    ui.colored_label(ORANGE, format!("Not valid JSON: {e}"));
                }
            });
            code_editor(ui, "body", text, vars);
        }
        Body::Text { text } => {
            // The language is the Content-Type header row, as Postman imports come in: one
            // place to see and change it.
            let typed =
                (headers.iter()).find(|h| h.enabled && h.key.eq_ignore_ascii_case("content-type"));
            let typed = typed.map(|h| h.value.to_lowercase()).unwrap_or_default();
            let shown = RAW_TYPES.iter().find(|(_, mime)| {
                !mime.is_empty() && typed.contains(&mime[mime.find('/').unwrap_or(0) + 1..])
            });
            let shown = shown.map_or("Text", |(label, _)| *label);
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("raw-type")
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        for (label, mime) in RAW_TYPES {
                            if ui.selectable_label(shown == label, label).clicked() {
                                headers.retain(|h| !h.key.eq_ignore_ascii_case("content-type"));
                                if !mime.is_empty() {
                                    headers.push(KeyValue::new("Content-Type", mime));
                                }
                            }
                        }
                    })
                    .response
                    .on_hover_text("Sets the Content-Type header");
                if shown == "XML"
                    && ui.small_button("Beautify").clicked()
                    && let Some(pretty) = http::pretty_xml(text)
                {
                    *text = pretty;
                }
            });
            code_editor(ui, "body", text, vars);
        }
        Body::Form { fields } => {
            kv_table(ui, "form", fields, vars, true);
        }
        Body::Multipart { parts } => {
            ui.weak("A value starting with @ uploads that file, e.g. @files/photo.png (relative to the workspace). Or drop files here.");
            for path in dropped_files(ui) {
                let key = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
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
        Body::File { path } => {
            ui.weak("The file's bytes are the body, as they are. Content-Type comes from the extension unless set under Headers. Or drop a file here.");
            if let Some(dropped) = dropped_files(ui).pop() {
                *path = dropped.display().to_string();
            }
            var_edit(
                ui,
                egui::Id::new("body-file"),
                path,
                vars,
                egui::TextStyle::Monospace,
                false,
                &[],
                |e| {
                    e.hint_text("path/to/file (relative to the workspace)")
                        .desired_width(f32::INFINITY)
                },
            );
            if !path.trim().is_empty() && !path.contains("{{") {
                match std::fs::metadata(&*path) {
                    Ok(m) if m.is_file() => {
                        let mime = mime_guess::from_path(&*path).first_or_octet_stream();
                        ui.weak(format!("{} · {mime}", human_size(m.len() as usize)));
                    }
                    _ => {
                        ui.colored_label(ORANGE, format!("File not found: {path}"));
                    }
                }
            }
        }
        Body::GraphQL { query, variables } => {
            return graphql_editor(ui, query, variables, vars, explorer);
        }
    }
    false
}

/// Files dropped on the window this frame, relative to the workspace when inside it so they
/// still work after a git clone. A file dropped while a dialog is up is the dialog's (Import
/// from Postman).
fn dropped_files(ui: &egui::Ui) -> Vec<PathBuf> {
    if ui.ctx().memory(|m| m.top_modal_layer().is_some()) {
        return Vec::new();
    }
    let workspace = std::env::current_dir().unwrap_or_default();
    ui.input(|i| {
        let files = i.raw.dropped_files.iter().map(|f| f.path().to_owned());
        files
            .map(|p| {
                p.strip_prefix(&workspace)
                    .map(Path::to_path_buf)
                    .unwrap_or(p)
            })
            .collect()
    })
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
            &[],
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
            &[],
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
        &[],
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
        (
            "API key",
            Auth::ApiKey {
                key: user(),
                value: pass(),
                in_query: false,
            },
        ),
        ("AWS Signature", Auth::AwsV4(model::AwsV4::default())),
        (
            "OAuth 1.0",
            Auth::OAuth1(model::OAuth1 {
                signature_method: "HMAC-SHA1".into(),
                consumer_key: user(),
                consumer_secret: pass(),
                ..Default::default()
            }),
        ),
        (
            "JWT Bearer",
            Auth::Jwt(model::Jwt {
                algorithm: "HS256".into(),
                secret: pass(),
                payload: "{\n  \"sub\": \"\",\n  \"iat\": {{$timestamp}}\n}".into(),
            }),
        ),
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
            &[],
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
            Auth::AwsV4(a) => {
                text(ui, "Access key", &mut a.access_key, "{{aws_access_key}}");
                secret(ui, "Secret key", &mut a.secret_key);
                text(ui, "Region", &mut a.region, "us-east-1");
                text(ui, "Service", &mut a.service, "execute-api, s3, lambda…");
                text(
                    ui,
                    "Session token",
                    &mut a.session_token,
                    "temporary credentials only",
                );
            }
            Auth::OAuth1(o) => {
                ui.label("Signature");
                egui::ComboBox::from_id_salt("oauth1-method")
                    .selected_text(o.signature_method.as_str())
                    .show_ui(ui, |ui| {
                        for m in crate::oauth1::METHODS {
                            ui.selectable_value(&mut o.signature_method, m.to_owned(), m);
                        }
                    });
                ui.end_row();
                text(ui, "Consumer key", &mut o.consumer_key, "");
                if o.signature_method.starts_with("RSA") {
                    text(
                        ui,
                        "Private key",
                        &mut o.consumer_secret,
                        "{{private_key}} (PEM)",
                    );
                } else {
                    secret(ui, "Consumer secret", &mut o.consumer_secret);
                }
                text(ui, "Access token", &mut o.token, "empty for two-legged");
                secret(ui, "Token secret", &mut o.token_secret);
                text(ui, "Realm", &mut o.realm, "optional");
            }
            Auth::Jwt(j) => {
                ui.label("Algorithm");
                egui::ComboBox::from_id_salt("jwt-alg")
                    .selected_text(j.algorithm.as_str())
                    .show_ui(ui, |ui| {
                        for alg in crate::jwt::ALGORITHMS {
                            ui.selectable_value(&mut j.algorithm, alg.to_owned(), alg);
                        }
                    });
                ui.end_row();
                if j.algorithm.starts_with("HS") {
                    secret(ui, "Secret", &mut j.secret);
                } else {
                    ui.label("Private key");
                    var_edit(
                        ui,
                        egui::Id::new(("auth", "jwt-key")),
                        &mut j.secret,
                        vars,
                        egui::TextStyle::Monospace,
                        true,
                        &[],
                        |e| {
                            e.hint_text("{{private_key}} or -----BEGIN PRIVATE KEY-----…")
                                .desired_rows(3)
                                .desired_width(420.0)
                        },
                    );
                    ui.end_row();
                }
                ui.label("Payload");
                var_edit(
                    ui,
                    egui::Id::new(("auth", "jwt-payload")),
                    &mut j.payload,
                    vars,
                    egui::TextStyle::Monospace,
                    true,
                    &[],
                    |e| e.desired_rows(4).desired_width(420.0),
                );
                ui.end_row();
            }
            Auth::ApiKey {
                key,
                value,
                in_query,
            } => {
                text(ui, "Key", key, "X-API-Key");
                text(ui, "Value", value, "{{api_key}}");
                ui.label("Add to");
                ui.horizontal(|ui| {
                    ui.selectable_value(in_query, false, "Header");
                    ui.selectable_value(in_query, true, "Query params");
                });
                ui.end_row();
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
                    ui.selectable_value(
                        &mut o.grant,
                        Grant::AuthorizationCode,
                        "Authorization code",
                    );
                    ui.selectable_value(&mut o.grant, Grant::Implicit, "Implicit");
                });
                ui.end_row();
                let browser = matches!(
                    o.grant,
                    model::Grant::AuthorizationCode | model::Grant::Implicit
                );
                let implicit = o.grant == model::Grant::Implicit;
                if browser {
                    text(
                        ui,
                        "Auth URL",
                        &mut o.auth_url,
                        "https://login.example.com/oauth2/authorize",
                    );
                }
                // The implicit grant has no token request, so nothing to send these to.
                if !implicit {
                    text(
                        ui,
                        "Token URL",
                        &mut o.token_url,
                        "https://login.example.com/oauth2/token",
                    );
                }
                text(ui, "Client ID", &mut o.client_id, "");
                if !implicit {
                    secret(ui, "Client secret", &mut o.client_secret);
                }
                text(ui, "Scope", &mut o.scope, "optional, space separated");
                if o.grant == model::Grant::Password {
                    text(ui, "Username", &mut o.username, "");
                    secret(ui, "Password", &mut o.password);
                }
                if browser {
                    text(
                        ui,
                        "Redirect URI",
                        &mut o.redirect_uri,
                        "http://127.0.0.1:<any free port>/callback",
                    );
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
        Auth::OAuth2(o) if o.grant == model::Grant::AuthorizationCode => {
            ui.weak(
                "Send opens your browser to sign in (with PKCE); the token is then reused until \
                 it expires or is rejected. Register the redirect URI with the provider; the \
                 client secret is only for confidential clients.",
            );
        }
        Auth::OAuth2(o) if o.grant == model::Grant::Implicit => {
            ui.weak(
                "Send opens your browser to sign in; the provider hands the token straight back \
                 to this machine. It is reused until it expires or is rejected, then you sign in \
                 again (the implicit grant has no refresh tokens).",
            );
        }
        Auth::OAuth2(_) => {
            ui.weak("The token is fetched on Send and reused until it expires or is rejected.");
        }
        Auth::OAuth1(_) => {
            ui.weak(
                "Each Send is signed over its method, URL, query and form body with a fresh \
                 nonce and timestamp. Get the access token from the provider's sign-in flow \
                 first and paste it here.",
            );
        }
        Auth::Jwt(_) => {
            ui.weak(
                "Signed on each Send and sent as Authorization: Bearer. Variables work in the \
                 payload ({{$timestamp}} for iat); keep the secret in a secret environment.",
            );
        }
        Auth::AwsV4(_) => {
            ui.weak(
                "Each Send is signed over its URL, headers and body. A signature lasts 15 \
                 minutes, so a copied snippet stops working after that. A body streamed from a \
                 file is sent as UNSIGNED-PAYLOAD, which S3 accepts.",
            );
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

/// True when a QoS was picked.
fn qos_box(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, qos: &mut u8) -> bool {
    let before = *qos;
    egui::ComboBox::from_id_salt(id)
        .width(64.0)
        .selected_text(format!("QoS {qos}"))
        .show_ui(ui, |ui| {
            for q in 0..=2u8 {
                ui.selectable_value(qos, q, format!("QoS {q}"));
            }
        })
        .response
        .on_hover_text("0: at most once · 1: at least once · 2: exactly once");
    *qos != before
}

/// True when an edit is complete (a box ticked, a QoS picked, a row removed, a filter
/// left): the moment a live connection should follow. Not every keystroke: "a/b" would
/// subscribe to "a" and "a/" on the way.
fn topics_editor(ui: &mut egui::Ui, topics: &mut Vec<model::Topic>, live: bool) -> bool {
    ui.weak(match live {
        true => "Connected: a change is (un)subscribed as soon as you finish it.",
        false => "Subscribed to on Connect. + matches one level, # everything below.",
    });
    let mut done = false;
    let mut remove = None;
    egui::Grid::new("topics")
        .num_columns(4)
        .spacing([8.0, 6.0])
        .show(ui, |ui| {
            for (i, t) in topics.iter_mut().enumerate() {
                done |= ui.checkbox(&mut t.enabled, "").changed();
                done |= ui
                    .add(
                        egui::TextEdit::singleline(&mut t.filter)
                            .hint_text("sensors/+/temperature")
                            .font(egui::TextStyle::Monospace)
                            .desired_width(320.0),
                    )
                    .lost_focus();
                done |= qos_box(ui, ("topic-qos", i), &mut t.qos);
                if ui.small_button("🗑").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        topics.remove(i);
        done = true;
    }
    if ui.small_button("+ Topic").clicked() {
        topics.push(model::Topic::default());
    }
    done
}

fn mqtt_settings(ui: &mut egui::Ui, m: &mut model::Mqtt) {
    egui::Grid::new("mqtt-settings")
        .num_columns(3)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("Version");
            egui::ComboBox::from_id_salt("mqtt-version")
                .width(80.0)
                .selected_text(if m.v5 { "5.0" } else { "3.1.1" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut m.v5, false, "3.1.1");
                    ui.selectable_value(&mut m.v5, true, "5.0");
                });
            ui.weak("5.0 says why a broker refuses or hangs up, and has user properties");
            ui.end_row();

            ui.label("Client ID");
            ui.add(
                egui::TextEdit::singleline(&mut m.client_id)
                    .hint_text("random")
                    .desired_width(240.0),
            );
            ui.weak("A broker drops the older of two connections with the same ID");
            ui.end_row();

            ui.label("Keep alive");
            ui.add(
                egui::DragValue::new(&mut m.keep_alive_secs)
                    .range(0..=3600)
                    .suffix(" s"),
            );
            ui.weak("0 sends no pings");
            ui.end_row();

            ui.label("Clean session");
            ui.checkbox(&mut m.clean_session, "");
            ui.weak("Off: the broker keeps subscriptions and queued messages for this client ID");
            ui.end_row();

            ui.label("Last will");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut m.will_topic)
                        .hint_text("devices/42/status")
                        .font(egui::TextStyle::Monospace)
                        .desired_width(240.0),
                );
                qos_box(ui, "will-qos", &mut m.will_qos);
                ui.checkbox(&mut m.will_retain, "Retain");
            });
            ui.weak("The broker publishes it if the connection drops without a goodbye");
            ui.end_row();

            ui.label("Will message");
            ui.add_enabled(
                !m.will_topic.trim().is_empty(),
                egui::TextEdit::singleline(&mut m.will_payload)
                    .hint_text("offline")
                    .desired_width(240.0),
            );
            ui.end_row();
        });
    ui.add_space(8.0);
    ui.weak("Username and password go in Auth (Basic). ws:// and wss:// carry MQTT over WebSocket (path as the broker says, often /mqtt). mqtts:// and wss:// use the certificate settings in Network settings, and the connection goes through its proxy.");
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

/// The empty response pane is where every reference app teaches its keys: it is the
/// biggest blank area, and it is in view exactly when nothing has been sent yet.
fn shortcut_list(ui: &mut egui::Ui) {
    ui.add_space(12.0);
    egui::Grid::new("shortcuts")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (what, key) in [
                ("Send request", &SEND),
                ("Save changes", &SAVE),
                ("Go to a request, folder or action", &SWITCH),
                ("New request", &NEW_REQUEST),
                ("Duplicate", &DUPLICATE),
                ("Select the URL", &FOCUS_URL),
                ("Find in the response", &FIND),
                ("Close tab", &CLOSE_TAB),
            ] {
                ui.weak(what);
                ui.weak(RichText::new(ui.ctx().format_shortcut(key)).monospace());
                ui.end_row();
            }
        });
}

const ASSERTS_HINT: &str = "Key: what to check, e.g. res.status, res.body.items.length, \
    res.body[0].id, res.headers['content-type'], res.responseTime.\n\
    Value: an operator and what to compare with, e.g. eq 200, neq, gt 0, gte, lt 500, lte, \
    in 200,201, notIn, contains ok, notContains, length 3, matches ^ok, notMatches, \
    startsWith, endsWith, between 1,10, isEmpty, isNotEmpty, isNull, isUndefined, isDefined, \
    isTruthy, isFalsy, isJson, isNumber, isString, isBoolean, isArray; no operator is eq. \
    {{variables}} work. Each row shows up under Tests.";

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
    (
        "Put a cookie in the jar",
        "pm.cookies.jar().set(\"https://\" + pm.environment.get(\"host\"), \"session\", \"abc\");\n",
    ),
    (
        "Fetch a token first",
        "pm.sendRequest({\n    url: pm.variables.replaceIn(\"{{baseUrl}}/token\"),\n    method: \"POST\",\n    body: { mode: \"raw\", raw: { id: pm.environment.get(\"clientId\") }, options: { raw: { language: \"json\" } } }\n}, function (err, res) {\n    if (err) throw err;\n    pm.environment.set(\"token\", res.json().token);\n});\n",
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
        "Cookie is set",
        "pm.test(\"Session cookie is set\", function () {\n    pm.expect(pm.cookies.has(\"session\")).to.be.true;\n});\n",
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
    json_response: Option<&str>,
) {
    let mut insert: Option<String> = None;
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
                    insert = Some((*code).to_owned());
                    ui.close();
                }
            }
            // Seeds a contract test from what came back: later sends fail when the shape
            // changes.
            if *tab == ScriptTab::Post
                && let Some(body) = json_response
            {
                ui.separator();
                if ui.button("Response matches its schema").clicked() {
                    insert = Some(schema_snippet(body));
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
    let (text, hint, id) = match tab {
        ScriptTab::Pre => (
            pre_request,
            "// Runs before the request is sent.\n// pm.request, pm.environment, pm.variables, console.log",
            "pre-request-script",
        ),
        ScriptTab::Post => (
            tests,
            "// Runs after the response arrives.\n// pm.test(name, fn), pm.expect(...), pm.response.json()",
            "tests-script",
        ),
    };
    if let Some(code) = insert {
        if !text.is_empty() {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
        }
        text.push_str(&code);
    }
    // A pasted library (lodash, moment) can be past MAX_EDIT.
    let id = egui::Id::new(id);
    if crate::varedit::too_big(ui, id, text).is_some() {
        return;
    }
    ui.add(
        egui::TextEdit::multiline(text)
            .id(id)
            .code_editor()
            .hint_text(hint)
            .desired_rows(12)
            .desired_width(f32::INFINITY),
    );
}

fn schema_snippet(body: &str) -> String {
    match serde_json::from_str(body) {
        Ok(v) => {
            let schema = serde_json::to_string_pretty(&crate::script::schema_of(&v))
                .unwrap_or_default()
                .replace('\n', "\n    ");
            format!(
                "pm.test(\"Response matches its schema\", function () {{\n    pm.response.to.have.jsonSchema({schema});\n}});\n"
            )
        }
        Err(e) => format!("// The response body isn't JSON: {e}\n"),
    }
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
    let reflected =
        |source: &str| req.proto.trim().is_empty() && source.starts_with(crate::grpc::REFLECTION);
    match rpcs {
        Some((source, Ok(list)))
            if req.method == "GRPC" && (*source == req.proto || reflected(source)) =>
        {
            list.iter().find(|r| r.name == req.rpc)
        }
        _ => None,
    }
}

/// WebSocket, SSE and streaming gRPC methods connect instead of sending.
fn streams(req: &Request, rpcs: &Rpcs) -> bool {
    model::is_streaming(&req.method)
        || rpc_of(req, rpcs).is_some_and(|r| r.client_streaming || r.server_streaming)
        || subscribes(req)
}

fn subscribes(req: &Request) -> bool {
    req.method == "GRAPHQL"
        && matches!(&req.body, Body::GraphQL { query, .. } if crate::graphql::is_subscription(query))
}

/// Returns an error to show, and whether to ask the server for its methods (reflection).
fn grpc_bar(
    ui: &mut egui::Ui,
    req: &mut Request,
    source: &str,
    methods: &mut Rpcs,
) -> (Option<String>, bool) {
    let (mut error, mut ask) = (None, false);
    let reflection = source.starts_with(crate::grpc::REFLECTION);
    ui.horizontal(|ui| {
        ui.label("Proto");
        ui.add(
            egui::TextEdit::singleline(&mut req.proto)
                .hint_text("protos/service.proto, or empty to ask the server")
                .desired_width(260.0),
        );
        let reload = ui
            .small_button("↻")
            .on_hover_text(match reflection {
                true => "Ask the server for its methods (gRPC reflection)",
                false => "Reload .proto",
            })
            .clicked();
        ask = reload && reflection;
        if reload || methods.as_ref().is_none_or(|(p, _)| p != source) {
            *methods = Some((source.to_owned(), crate::grpc::methods(source)));
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
                    match crate::grpc::template(source, &req.rpc) {
                        Ok(text) => req.body = Body::Json { text },
                        Err(e) => error = Some(e),
                    }
                }
            }
            Err(e) if reflection => {
                ui.weak(e.as_str());
            }
            Err(e) => {
                ui.colored_label(RED, e.lines().next().unwrap_or_default())
                    .on_hover_text(e.as_str());
            }
        }
    });
    (error, ask)
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
    /// end the call. MQTT publishes to the open request's topic, as it is now.
    fn send_compose(
        &mut self,
        mqtt: Option<&model::Mqtt>,
        vars: &HashMap<String, String>,
    ) -> Result<(), String> {
        match &self.outgoing {
            None => {}
            Some(Outgoing::Text(_)) if self.compose.is_empty() => {}
            Some(Outgoing::Text(tx)) => {
                if let Some((proto, rpc)) = &self.grpc {
                    crate::grpc::check(proto, rpc, &self.compose)?;
                }
                let _ = tx.send(self.compose.clone());
            }
            // An empty message is fine: retained, it clears what the broker keeps.
            Some(Outgoing::Mqtt(tx)) => {
                let m = mqtt.ok_or("no request is open")?;
                let topic = model::resolve(m.topic.trim(), vars, &mut Vec::new());
                if topic.is_empty() {
                    return Err("Publish needs a topic".into());
                }
                if topic.contains(['+', '#']) {
                    return Err("+ and # are for subscribing; publish to one topic".into());
                }
                let properties: Vec<_> = (m.user_properties.iter())
                    .filter(|p| p.enabled && !p.key.trim().is_empty())
                    .map(|p| {
                        let r = |s: &str| model::resolve(s, vars, &mut Vec::new());
                        (r(p.key.trim()), r(&p.value))
                    })
                    .collect();
                // ponytail: goes by the request's version now, not the connection's; they
                // differ only if it's switched while connected.
                if !m.v5 && !properties.is_empty() {
                    return Err("User properties need MQTT 5.0 (Settings)".into());
                }
                let _ = tx.send(crate::mqtt::Command::Publish(crate::mqtt::Publish {
                    topic,
                    qos: m.qos,
                    retain: m.retain,
                    payload: self.compose.clone(),
                    properties,
                }));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn events(&self) -> Vec<(Duration, Event)> {
        self.log.lock().unwrap().events.iter().cloned().collect()
    }

    /// MQTT: has the live connection follow the request's topics.
    fn resubscribe(&self, req: &Request, vars: &HashMap<String, String>) {
        if let Some(Outgoing::Mqtt(tx)) = &self.outgoing {
            let (req, _) = req.resolved(vars);
            let want = req.mqtt.topics.into_iter().map(|t| (t.filter, t.qos));
            let _ = tx.send(crate::mqtt::Command::Topics(want.collect()));
        }
    }
}

fn stream_ui(
    ui: &mut egui::Ui,
    s: &mut StreamSession,
    mqtt: &mut model::Mqtt,
    vars: &HashMap<String, String>,
) -> Result<(), String> {
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
        ui.weak(format!("· {} events", s.log.lock().unwrap().events.len()));
        if ui.small_button("Clear").clicked() {
            s.log.lock().unwrap().clear();
            s.selected = None;
        }
    });
    if matches!(s.outgoing, Some(Outgoing::Mqtt(_))) {
        // Part of the request, as in Postman, so it's there next time.
        ui.horizontal(|ui| {
            ui.label("Publish to");
            ui.add(
                egui::TextEdit::singleline(&mut mqtt.topic)
                    .hint_text("devices/42/cmd")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(240.0),
            );
            qos_box(ui, "publish-qos", &mut mqtt.qos);
            ui.checkbox(&mut mqtt.retain, "Retain")
                .on_hover_text("The broker keeps it for whoever subscribes later");
        });
    }
    if s.outgoing.is_some() {
        ui.horizontal(|ui| {
            let send_w = 70.0;
            let id = egui::Id::new("compose");
            if crate::varedit::too_big(ui, id, &mut s.compose).is_none() {
                ui.add(
                    egui::TextEdit::multiline(&mut s.compose)
                        .id(id)
                        .desired_rows(2)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(s.hint)
                        .desired_width(ui.available_width() - send_w - 8.0),
                );
            }
            if ui
                .add_sized([send_w, 22.0], egui::Button::new("Send"))
                .on_hover_text(ui.ctx().format_shortcut(&SEND))
                .clicked()
            {
                sent = s.send_compose(Some(mqtt), vars);
            }
        });
    }
    ui.separator();
    let log = s.log.lock().unwrap();
    // The selected event, whole, under the list; gone once it scrolls out of the log.
    let selected = s.selected.as_ref().map(|(n, _)| *n);
    let at = |n: u64| n.checked_sub(log.dropped).map(|i| i as usize);
    if selected.and_then(at).is_none_or(|i| i >= log.events.len()) {
        s.selected = None;
    }
    if let Some((_, text)) = &s.selected {
        egui::Panel::bottom("stream-detail")
            .resizable(true)
            .default_size(160.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(text.clone());
                    }
                    ui.weak(human_size(text.len()));
                });
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(text.as_str()).monospace())
                                .selectable(true)
                                .wrap(),
                        );
                    });
            });
    }
    // One line per event and only the rows in view laid out: 5000 wrapped messages
    // would cost gigabytes of text layout every frame.
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show_rows(ui, row_h, log.events.len(), |ui, rows| {
            for i in rows {
                let (at, event) = &log.events[i];
                let (badge, color, text) = match event {
                    Event::Open(t) => ("OPEN", GREEN, t),
                    Event::Info(t) => ("INFO", Color32::GRAY, t),
                    Event::In(t) => ("IN", Color32::from_rgb(90, 160, 230), t),
                    Event::Out(t) => ("OUT", ORANGE, t),
                    Event::Closed(t) => ("CLOSED", Color32::GRAY, t),
                    Event::Error(t) => ("ERROR", RED, t),
                };
                let n = log.dropped + i as u64;
                ui.horizontal(|ui| {
                    ui.set_height(row_h);
                    ui.weak(format!("{:>8.3}", at.as_secs_f32()));
                    ui.add_sized(
                        [56.0, row_h],
                        egui::Label::new(RichText::new(badge).color(color).strong().monospace()),
                    );
                    let line: String = text
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(300)
                        .collect();
                    let mut line = RichText::new(line).monospace();
                    if selected == Some(n) {
                        line = line.background_color(ui.visuals().selection.bg_fill);
                    }
                    let row = ui.add(
                        egui::Label::new(line)
                            .truncate()
                            .sense(egui::Sense::click()),
                    );
                    if row.on_hover_text("Click to see it whole").clicked() {
                        clicked = Some((n, text));
                    }
                });
            }
        });
    if let Some((n, text)) = clicked {
        s.selected = match selected == Some(n) {
            true => None,
            // Pretty when it's JSON; already capped at MAX_EVENT_TEXT.
            false => Some((n, http::pretty_json(text).unwrap_or_else(|| text.clone()))),
        };
    }
    sent
}

/// The open request's past responses, newest first, and what the user did with them.
struct Past {
    list: Vec<store::ResponseMeta>,
    pick: Option<i64>,
    delete: Option<i64>,
    clear: bool,
}

/// Postman's response history: earlier responses of this request, to look back at.
fn past_menu(ui: &mut egui::Ui, past: &mut Past, shown: Option<i64>) {
    if past.list.is_empty() {
        return;
    }
    let label = match past.list.iter().find(|m| Some(m.id) == shown) {
        Some(m) => format!("History: {}", ago(m.at)),
        None => "History".into(),
    };
    ui.menu_button(label, |ui| {
        for m in &past.list {
            let text = RichText::new(format!("{}  ·  {} ms  ·  {}", m.status, m.ms, ago(m.at)))
                .color(status_color(m.status));
            ui.horizontal(|ui| {
                if ui
                    // Room for "200 · 12345 ms · 59 min ago", so the ×s line up; the
                    // grow atom keeps the text at the left of the wider button.
                    .add(
                        egui::Button::selectable(shown == Some(m.id), (text, egui::Atom::grow()))
                            .min_size(egui::vec2(200.0, 0.0)),
                    )
                    .clicked()
                {
                    past.pick = Some(m.id);
                }
                if ui
                    .small_button("×")
                    .on_hover_text("Delete this response")
                    .clicked()
                {
                    past.delete = Some(m.id);
                }
            });
        }
        ui.separator();
        if ui.button("Clear history").clicked() {
            past.clear = true;
        }
    })
    .response
    .on_hover_text(format!(
        "Earlier responses to this request (the last {})",
        store::MAX_RESPONSES
    ));
}

/// Returns an example to save when the user asked for one.
/// `wrap` is the word-wrap switch; `save_file` and `open_html` are set when the user asks
/// to save the body or see it in a browser.
// Each is a separate App field borrowed for the pane; a struct for this one call would
// only rename them.
#[allow(clippy::too_many_arguments)]
fn response_ui(
    ui: &mut egui::Ui,
    shown: &mut Shown,
    tab: &mut RespTab,
    wrap: &mut bool,
    save_file: &mut bool,
    open_html: &mut bool,
    past: &mut Past,
    recent: &mut Vec<String>,
) -> Option<Example> {
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
                )
                .on_hover_text(status_meaning(h.status));
                let ms = ui.weak(format!("{} ms", h.elapsed.as_millis()));
                if let Some(waited) = h.sent.waited {
                    let hops = match h.sent.hops.len() {
                        0 => String::new(),
                        1 => ", the redirect included".into(),
                        n => format!(", {n} redirects included"),
                    };
                    let connect = match h.sent.connect {
                        Some(c) => format!("Connect {} ms (DNS, TCP, TLS)\n", c.as_millis()),
                        None => "Reused an open connection\n".into(),
                    };
                    let connected = waited.saturating_sub(h.sent.connect.unwrap_or_default());
                    ms.on_hover_text(format!(
                        "{connect}Waiting (TTFB) {} ms{hops}\nDownload {} ms",
                        connected.as_millis(),
                        h.elapsed.saturating_sub(waited).as_millis()
                    ));
                }
                // As received, before any re-indenting for display.
                let headers: usize = (h.headers.iter()).map(|(k, v)| k.len() + v.len() + 4).sum();
                let sizes = format!(
                    "Headers {}\nBody {}",
                    human_size(headers),
                    human_size(view.raw_size)
                );
                if h.truncated {
                    ui.colored_label(ORANGE, format!("{} (cut)", human_size(view.raw_size)))
                        .on_hover_text(format!(
                            "{sizes}\nOnly the first {} were kept, to save memory.",
                            human_size(view.raw_size)
                        ));
                } else {
                    ui.weak(human_size(view.raw_size)).on_hover_text(sizes);
                }
                ui.weak(&h.version);
                ui.separator();
                ui.selectable_value(tab, RespTab::Body, "Body");
                ui.selectable_value(
                    tab,
                    RespTab::Headers,
                    format!("Headers ({})", h.headers.len()),
                );
                let cookies = crate::cookies::from_response(&h.headers).len();
                if cookies > 0 {
                    ui.selectable_value(tab, RespTab::Cookies, format!("Cookies ({cookies})"))
                        .on_hover_text("What this response's Set-Cookie headers set");
                }
                // gRPC goes out its own way and isn't recorded.
                if !h.sent.method.is_empty() {
                    ui.selectable_value(tab, RespTab::Timeline, "Timeline")
                        .on_hover_text("What was sent, redirects, and what came back");
                }
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
        past_menu(ui, past, shown.past);
        if let Ok(view) = &mut shown.result {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let timeline = *tab == RespTab::Timeline;
                let binary = view.head.bytes.is_some();
                let what = if timeline {
                    "Copy the timeline"
                } else {
                    "Copy body"
                };
                if (timeline || !binary) && ui.small_button("Copy").on_hover_text(what).clicked() {
                    let text = match timeline {
                        true => view.head.timeline(),
                        false => view.text.clone(),
                    };
                    ui.ctx().copy_text(text);
                }
                if (ui.small_button("Save…"))
                    .on_hover_text("Save the body to a file, as received")
                    .clicked()
                {
                    *save_file = true;
                }
                let html = (view.head.headers.iter())
                    .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("html"));
                if html
                    && (ui.small_button("Open in browser"))
                        .on_hover_text("Show the HTML in your browser")
                        .clicked()
                {
                    *open_html = true;
                }
                // Examples hold text.
                if !binary
                    && ui
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
                        body: view.unfiltered.clone().unwrap_or_else(|| view.text.clone()),
                    });
                }
                if *tab == RespTab::Body && !binary {
                    ui.separator();
                    if (ui.selectable_label(*wrap, "Wrap"))
                        .on_hover_text("Wrap long lines")
                        .clicked()
                    {
                        *wrap = !*wrap;
                    }
                    // Right to left: Raw is added first to sit on the right.
                    if view.other.is_some() {
                        if (ui.selectable_label(!view.pretty, "Raw"))
                            .on_hover_text("As received")
                            .clicked()
                        {
                            view.set_pretty(false);
                        }
                        if ui.selectable_label(view.pretty, "Pretty").clicked() {
                            view.set_pretty(true);
                        }
                    }
                    let shortcut = ui.ctx().format_shortcut(&FIND);
                    if (ui.selectable_label(view.find.open, "Find"))
                        .on_hover_text(shortcut)
                        .clicked()
                    {
                        match view.find.open {
                            true => view.find.close(),
                            false => (view.find.open, view.find.focus) = (true, true),
                        }
                    }
                }
            });
        }
    });
    // A row of its own: in the bar it squeezed out the tabs left of it.
    if *tab == RespTab::Body
        && let Ok(view) = &mut shown.result
        && view.find.open
        && view.head.bytes.is_none()
    {
        ui.horizontal(|ui| find_bar(ui, view));
    }
    ui.separator();
    // A tab from the previous run may not exist in this one; fall back to the body.
    let current = match *tab {
        RespTab::Tests if shown.tests.is_empty() => RespTab::Body,
        RespTab::Console if shown.logs.is_empty() => RespTab::Body,
        RespTab::Headers if shown.result.is_err() => RespTab::Body,
        RespTab::Cookies
            if (shown.result.as_ref())
                .is_ok_and(|v| crate::cookies::from_response(&v.head.headers).is_empty())
                || shown.result.is_err() =>
        {
            RespTab::Body
        }
        RespTab::Timeline
            if (shown.result.as_ref()).is_ok_and(|v| v.head.sent.method.is_empty())
                || shown.result.is_err() =>
        {
            RespTab::Body
        }
        t => t,
    };
    match (current, &mut shown.result) {
        (RespTab::Tests, _) => {
            // None: all; else only passed (true) or failed (false) ones.
            let id = egui::Id::new("tests-filter");
            let mut only: Option<bool> = ui.data(|d| d.get_temp(id)).flatten();
            let passed = shown.tests.iter().filter(|t| t.passed).count();
            let failed = shown.tests.len() - passed;
            ui.horizontal(|ui| {
                ui.selectable_value(&mut only, None, format!("All ({})", shown.tests.len()));
                ui.selectable_value(&mut only, Some(true), format!("Passed ({passed})"));
                ui.selectable_value(&mut only, Some(false), format!("Failed ({failed})"));
            });
            ui.data_mut(|d| d.insert_temp(id, only));
            egui::ScrollArea::vertical()
                .id_salt("response-tests")
                .auto_shrink(false)
                .show(ui, |ui| {
                    for t in (shown.tests.iter()).filter(|t| only.is_none_or(|p| t.passed == p)) {
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
        (RespTab::Cookies, Ok(view)) => {
            egui::ScrollArea::both()
                .id_salt("response-cookies")
                .auto_shrink(false)
                .show(ui, |ui| {
                    egui::Grid::new("resp-cookies")
                        .num_columns(5)
                        .striped(true)
                        .show(ui, |ui| {
                            for title in ["Name", "Value", "Domain", "Path", "Expires"] {
                                ui.strong(title);
                            }
                            ui.end_row();
                            for c in crate::cookies::from_response(&view.head.headers) {
                                let expires = c.expires.as_deref().unwrap_or("session");
                                for cell in [&c.name, &c.value, &c.domain, &c.path] {
                                    ui.add(egui::Label::new(cell.as_str()).selectable(true));
                                }
                                ui.label(expires);
                                ui.end_row();
                            }
                        });
                });
        }
        (RespTab::Timeline, Ok(view)) => {
            egui::ScrollArea::both()
                .id_salt("response-timeline")
                .auto_shrink(false)
                .show(ui, |ui| {
                    let text = RichText::new(view.head.timeline()).monospace();
                    ui.add(egui::Label::new(text).selectable(true).extend());
                });
        }
        (_, Ok(view)) if view.head.bytes.is_some() => binary_body(ui, view),
        (_, Ok(view)) => {
            if view.other.is_some() {
                filter_bar(ui, view, recent);
            }
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font, '0'));
            let digits = view.line_starts.len().max(1).ilog10() as usize + 1;
            // Pretty JSON only: folding finds a block's end by its indentation.
            let foldable = view.json && (view.pretty || view.unfiltered.is_some());
            let arrow_w = if foldable { char_w * 1.5 } else { 0.0 };
            let spacing = ui.spacing().item_spacing.x;
            let arrow = if foldable { arrow_w + spacing } else { 0.0 };
            let gutter = (digits + 1) as f32 * char_w + spacing + arrow;
            if *wrap {
                let width = ui.available_width() - gutter - ui.spacing().scroll.bar_width;
                let cols = (width / char_w).floor().max(20.0) as usize;
                if view.wrapped.as_ref().is_none_or(|(c, _)| *c != cols) {
                    view.wrapped = Some((cols, wrap_rows(&view.text, &view.line_starts, cols)));
                }
            }
            let rows: &[Row] = match (&view.wrapped, *wrap) {
                (Some((_, rows)), true) => rows,
                _ => &[],
            };
            let mut area = match *wrap {
                true => egui::ScrollArea::vertical(),
                false => egui::ScrollArea::both(),
            }
            .id_salt("response-body")
            .auto_shrink(false);
            let mut scroll_to = None;
            if std::mem::take(&mut view.find.scroll)
                && let Some(at) = view.find.hits.get(view.find.current).map(|h| h.start)
            {
                let line = view.line_starts.partition_point(|&s| s <= at) - 1;
                // A hit inside a folded block opens it.
                view.folds
                    .retain(|&open, &mut close| !(open < line && line <= close));
                scroll_to = Some(
                    match *wrap {
                        true => rows.partition_point(|r| r.start <= at),
                        false => line + 1,
                    } - 1,
                );
            }
            // Folds as hidden lines, then as hidden rows when wrapped.
            let mut hidden = hidden_ranges(&view.folds);
            if *wrap {
                let row_of = |line: usize| match view.line_starts.get(line) {
                    Some(&s) => rows.partition_point(|r| r.start < s),
                    None => rows.len(),
                };
                for (first, n) in &mut hidden {
                    let row = row_of(*first);
                    (*first, *n) = (row, row_of(*first + *n) - row);
                }
            }
            let total = if *wrap {
                rows.len()
            } else {
                view.line_starts.len()
            };
            let count = total - hidden.iter().map(|&(_, n)| n).sum::<usize>();
            if let Some(row) = scroll_to {
                let before: usize = (hidden.iter())
                    .filter(|&&(first, n)| first + n <= row)
                    .map(|&(_, n)| n)
                    .sum();
                // A few lines of context above the hit.
                let pitch = row_height + ui.spacing().item_spacing.y;
                area = area.vertical_scroll_offset((row - before).saturating_sub(3) as f32 * pitch);
            }
            let weak = ui.visuals().weak_text_color();
            let mut toggle = None;
            area.show_rows(ui, row_height, count, |ui, range| {
                for i in range.map(|v| unhide(&hidden, v)) {
                    let (start, end, line, in_string) = match *wrap {
                        true => {
                            let end = rows.get(i + 1).map_or(view.text.len(), |r| r.start);
                            (rows[i].start, end, rows[i].line, rows[i].in_string)
                        }
                        false => {
                            let end = view.line_starts.get(i + 1).copied();
                            (
                                view.line_starts[i],
                                end.unwrap_or(view.text.len()),
                                i as u32 + 1,
                                false,
                            )
                        }
                    };
                    let mut cut = end.min(start + MAX_LINE);
                    while !view.text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    let end = start + view.text[start..cut].trim_end().len();
                    let folded = hidden
                        .binary_search_by_key(&(i + 1), |&(first, _)| first)
                        .is_ok();
                    ui.horizontal(|ui| {
                        let number = match line {
                            0 => String::new(),
                            n => n.to_string(),
                        };
                        ui.label(
                            RichText::new(format!("{number:>digits$}"))
                                .monospace()
                                .color(weak),
                        );
                        if foldable {
                            let size = egui::vec2(arrow_w, row_height);
                            let (rect, click) = ui.allocate_exact_size(size, egui::Sense::click());
                            let at = line as usize;
                            let opens = at > 0 && {
                                let end = view.line_starts.get(at).copied();
                                let text = &view.text
                                    [view.line_starts[at - 1]..end.unwrap_or(view.text.len())];
                                text.trim_end().ends_with(['{', '['])
                            };
                            if opens {
                                let (arrow, what) = match view.folds.contains_key(&(at - 1)) {
                                    true => ("⏵", "Unfold"),
                                    false => ("⏷", "Fold"),
                                };
                                click.widget_info(|| {
                                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, what)
                                });
                                let color = if click.hovered() {
                                    ui.visuals().text_color()
                                } else {
                                    weak
                                };
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    arrow,
                                    font.clone(),
                                    color,
                                );
                                if click.clicked() {
                                    toggle = Some(at - 1);
                                }
                            }
                        }
                        ui.add(
                            egui::Label::new(highlighted(ui, view, start..end, in_string)).extend(),
                        );
                        if folded {
                            let close = match view.text[start..end].ends_with('{') {
                                true => "… }",
                                false => "… ]",
                            };
                            ui.label(RichText::new(close).monospace().color(weak));
                        }
                    });
                }
            });
            if let Some(open) = toggle
                && view.folds.remove(&open).is_none()
                && let Some(close) = fold_end(&view.text, &view.line_starts, open)
            {
                view.folds.insert(open, close);
            }
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
                // Saved from a response, so up to 16 MiB.
                let id = egui::Id::new(("example-body", i));
                if crate::varedit::too_big(ui, id, &mut ex.body).is_none() {
                    ui.add(
                        egui::TextEdit::multiline(&mut ex.body)
                            .id(id)
                            .code_editor()
                            .desired_rows(8)
                            .desired_width(f32::INFINITY),
                    );
                }
            });
    }
    if let Some(i) = remove {
        examples.remove(i);
    }
}

/// Enough to step through: one letter in a 16 MiB body would otherwise keep millions of
/// 8-byte offsets.
const MAX_HITS: usize = 10_000;

/// The find row: query, options, count, stepping, close.
fn find_bar(ui: &mut egui::Ui, view: &mut ResponseView) {
    let ResponseView { text, find, .. } = view;
    let edit = ui.add(
        egui::TextEdit::singleline(&mut find.query)
            .id(egui::Id::new("find"))
            .hint_text("Find")
            .desired_width(220.0),
    );
    let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
    let (enter, escape) = (edit.lost_focus() && enter, edit.lost_focus() && escape);
    // After the field has drawn: a click on the Find button would otherwise count as a
    // click elsewhere and take the focus straight back.
    if enter || std::mem::take(&mut find.focus) {
        edit.request_focus(); // keep typing / pressing Enter for the next hit
    }
    ui.toggle_value(&mut find.case, "Aa")
        .on_hover_text("Match case");
    ui.toggle_value(&mut find.word, "W")
        .on_hover_text("Whole word");
    ui.toggle_value(&mut find.regex, ".*")
        .on_hover_text("Regular expression");
    let wanted = (find.query.clone(), find.case, find.word, find.regex);
    if wanted != find.searched {
        // Searched in place: lowercasing a copy of a 16 MiB body per keystroke was a
        // 16 MiB allocation each time.
        let pattern = match find.regex {
            true => find.query.clone(),
            false => regex::escape(&find.query),
        };
        // ASCII word boundaries: Unicode ones push the regex crate off its fast engine
        // for any non-ASCII body.
        let pattern = match find.word {
            true => format!(r"(?-u:\b)(?:{pattern})(?-u:\b)"),
            false => pattern,
        };
        let built = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!find.case)
            .build();
        (find.hits, find.error) = match built {
            _ if find.query.is_empty() => (Vec::new(), None),
            // Empty matches (`a*`) would mark every position and highlight nothing.
            Ok(re) => (
                (re.find_iter(text).filter(|m| !m.is_empty()))
                    .map(|m| m.range())
                    .take(MAX_HITS)
                    .collect(),
                None,
            ),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        find.searched = wanted;
        find.current = 0;
        find.scroll = !find.hits.is_empty();
    }
    let n = find.hits.len();
    if let Some(e) = &find.error {
        ui.colored_label(RED, "bad pattern").on_hover_text(e);
    } else if !find.query.is_empty() {
        let more = if n == MAX_HITS { "+" } else { "" };
        ui.weak(format!(
            "{}/{n}{more}",
            if n == 0 { 0 } else { find.current + 1 }
        ));
    }
    let prev = ui
        .small_button("Prev")
        .on_hover_text("Shift+Enter")
        .clicked();
    let next = ui.small_button("Next").on_hover_text("Enter").clicked();
    let close = ui.small_button("×").on_hover_text("Close (Esc)").clicked();
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
    if close || escape {
        find.close();
    }
}

/// A body that isn't text: shown when it's an image, else only its size.
fn binary_body(ui: &mut egui::Ui, view: &mut ResponseView) {
    let bytes = view.head.bytes.as_deref().unwrap_or_default();
    let ctx = ui.ctx().clone();
    match view.image.get_or_insert_with(|| decode_image(&ctx, bytes)) {
        Ok((texture, [w, h])) => {
            ui.weak(format!("{w} × {h}"));
            // As big as it is, unless that doesn't fit.
            let fit = ui.available_size();
            ui.add(
                egui::Image::from_texture(&*texture)
                    .fit_to_original_size(1.0)
                    .max_size(fit),
            );
        }
        Err(e) => {
            let size = human_size(bytes.len());
            ui.weak(format!(
                "{size} of binary data, not shown ({e}). Save… keeps it as received."
            ));
        }
    }
}

/// PNG and JPEG. Bigger than the GPU takes, the picture is scaled down to fit.
// ponytail: no GIF or WebP; enable those `image` features if APIs here serve them.
fn decode_image(
    ctx: &egui::Context,
    bytes: &[u8],
) -> Result<(egui::TextureHandle, [u32; 2]), String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    // A few bytes can claim a huge picture: 64 MiB decoded is 4096 × 4096.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 << 20);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let size = [img.width(), img.height()];
    let side = ctx.input(|i| i.max_texture_side) as u32;
    let img = match size[0].max(size[1]) > side {
        true => img.thumbnail(side, side),
        false => img,
    };
    let rgba = img.into_rgba8();
    let pixels = [rgba.width() as usize, rgba.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(pixels, rgba.as_raw());
    Ok((
        ctx.load_texture("response-image", color, Default::default()),
        size,
    ))
}

/// One body row, JSON tokens coloured and find hits painted over it.
fn highlighted(
    ui: &egui::Ui,
    view: &ResponseView,
    line: std::ops::Range<usize>,
    in_string: bool,
) -> egui::WidgetText {
    let (text, find) = (&view.text, &view.find);
    let mut hit = find.hits.partition_point(|h| h.end <= line.start);
    let no_hits = find.hits.get(hit).is_none_or(|h| h.start >= line.end);
    if !view.json && no_hits {
        return RichText::new(&text[line]).monospace().into();
    }
    let tokens: Vec<(usize, Option<Token>)> = match view.json {
        true => (json_tokens(&text[line.clone()], in_string).into_iter())
            .map(|(end, t)| (end, Some(t)))
            .collect(),
        false => vec![(line.len(), None)],
    };
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    let mut append = |range: std::ops::Range<usize>, color, background| {
        let format = egui::TextFormat {
            background,
            ..egui::TextFormat::simple(font.clone(), color)
        };
        job.append(&text[range], 0.0, format);
    };
    let mut pos = line.start;
    for (end, token) in tokens {
        let end = line.start + end;
        let color = token.map_or(ui.visuals().text_color(), |t| token_color(ui, t));
        while pos < end {
            match find.hits.get(hit) {
                Some(h) if h.start < end && h.end > pos => {
                    if h.start > pos {
                        append(pos..h.start, color, Color32::TRANSPARENT);
                        pos = h.start;
                    }
                    let to = h.end.min(end);
                    let mark = match hit == find.current {
                        true => ORANGE,
                        false => Color32::from_rgb(240, 220, 90),
                    };
                    let done = h.end <= to;
                    append(pos..to, Color32::BLACK, mark);
                    pos = to;
                    if done {
                        hit += 1;
                    }
                }
                _ => {
                    append(pos..end, color, Color32::TRANSPARENT);
                    pos = end;
                }
            }
        }
    }
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

/// What a status code means, for the hover on it, as Postman shows.
fn status_meaning(status: u16) -> &'static str {
    match status {
        200 => "OK: the request succeeded.",
        201 => "Created: the request succeeded and a new resource was created.",
        202 => "Accepted: received, but not acted on yet.",
        204 => "No Content: succeeded, with no body to return.",
        206 => "Partial Content: only the requested range is returned.",
        301 => "Moved Permanently: the resource has a new URL for good.",
        302 => "Found: the resource is at another URL for now.",
        303 => "See Other: get the result from another URL with GET.",
        304 => "Not Modified: the cached copy is still good.",
        307 => "Temporary Redirect: repeat the same request at another URL.",
        308 => "Permanent Redirect: repeat the same request at another URL, from now on.",
        400 => {
            "Bad Request: the server can't process the request as sent (syntax, framing, values)."
        }
        401 => "Unauthorized: credentials are missing or wrong.",
        403 => "Forbidden: the credentials are known but not allowed to do this.",
        404 => "Not Found: nothing at this URL.",
        405 => "Method Not Allowed: the URL exists but not for this method.",
        406 => "Not Acceptable: nothing matches the Accept headers.",
        408 => "Request Timeout: the server gave up waiting for the request.",
        409 => "Conflict: the request clashes with the resource's current state.",
        410 => "Gone: the resource was here and was removed for good.",
        413 => "Content Too Large: the body is bigger than the server accepts.",
        415 => "Unsupported Media Type: the server doesn't take this Content-Type.",
        422 => "Unprocessable Content: well-formed, but the values don't pass validation.",
        429 => "Too Many Requests: rate limited; see Retry-After.",
        500 => "Internal Server Error: the server failed while handling the request.",
        501 => "Not Implemented: the server doesn't support this method.",
        502 => "Bad Gateway: a proxy or gateway got a bad answer from upstream.",
        503 => "Service Unavailable: overloaded or down for maintenance; see Retry-After.",
        504 => "Gateway Timeout: a proxy or gateway got no answer from upstream in time.",
        _ => match status / 100 {
            1 => "Informational: the request was received and goes on.",
            2 => "Success: the request was received, understood and accepted.",
            3 => "Redirection: more action is needed to complete the request.",
            4 => "Client error: the request is wrong or can't be fulfilled.",
            _ => "Server error: the server failed to fulfil a valid request.",
        },
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
        // The Timeline shows the URL with the variable filled in, as it went out.
        h.get_by_label("Timeline").click();
        h.run();
        shot(&mut h, "45-timeline");
        let sent = format!("> GET http://{host}/x\n");
        assert!(h.query_by_label_contains(&sent).is_some(), "no {sent:?}");
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

        // The tree's "api", drawn before the breadcrumb's.
        h.get_all_by_label("api").next().unwrap().click_secondary();
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

    #[test]
    fn the_tab_menu_closes_in_bulk_but_never_drops_edits() {
        let ws = workspace("tabmenu");
        let dir = ws.collections();
        for name in ["a", "b", "c", "d"] {
            ws.create_request(&dir, name).unwrap();
        }
        let mut h = harness(ws);
        h.run();
        for name in ["a", "b", "c", "d", "b"] {
            h.state_mut()
                .activate(dir.join(format!("{name}.toml")), true);
            h.run();
        }
        h.state_mut().open.as_mut().unwrap().draft.url = "http://edited".into();
        h.run();
        let tabs = |h: &Harness<'_, App>| -> Vec<String> {
            let stem = |t: &Tab| t.path.file_stem().unwrap().to_string_lossy().into_owned();
            h.state().tabs.iter().map(stem).collect()
        };
        // The tree's label comes first, then the tab's (the active one's title is a third).
        let menu = |h: &mut Harness<'_, App>, tab: &str, item: &str| {
            h.get_all_by_label(tab).nth(1).unwrap().click_secondary();
            h.run();
            shot(h, "42-tab-menu");
            // A shortcut is part of its button's label.
            h.get(egui_kittest::kittest::By::new().label_contains(item))
                .click();
            h.run();
        };
        menu(&mut h, "c", "Close Tabs to the Right");
        assert_eq!(tabs(&h), ["a", "b", "c"]);
        // Bulk closing never asks, so the edited tab stays and the status bar says why.
        menu(&mut h, "c", "Close Other Tabs");
        assert_eq!(tabs(&h), ["b", "c"]);
        assert!(h.state().dialog.is_none());
        assert_eq!(h.state().status, "1 tab with unsaved edits left open");
        menu(&mut h, "c", "Duplicate Tab");
        assert!(tabs(&h).contains(&"c copy".to_owned()), "{:?}", tabs(&h));
        menu(&mut h, "c copy", "Close All Tabs");
        assert_eq!(tabs(&h), ["b"]);
        assert_eq!(draft(&h).url, "http://edited");
    }

    #[test]
    fn path_variables_appear_with_the_url_and_take_values() {
        let mut h = with_request("pathvars");
        type_into(&mut h, 0, "{{host}}/users/:id");
        assert!(h.query_by_label("Params (1)").is_some());
        // The name follows the URL; the field after it is the value.
        let inputs = h.get_all_by_role(Role::TextInput);
        let inputs = inputs.filter(|n| n.accesskit_node().placeholder() != Some(FILTER_HINT));
        let key = (inputs.map(|n| n.value()))
            .position(|v| v.as_deref() == Some("id"))
            .expect("a row for :id");
        type_into(&mut h, key + 1, "{{uid}}");
        shot(&mut h, "43-path-variables");
        assert_eq!(draft(&h).path_vars, [KeyValue::new("id", "{{uid}}")]);
        assert_eq!(
            draft(&h).url,
            "{{host}}/users/:id",
            "the URL keeps the name"
        );
        // Its value is resolved like the rest of the request.
        assert!(h.query_by_label("Undefined: uid").is_some());
    }

    #[test]
    fn side_by_side_puts_the_response_beside_the_request_and_is_kept() {
        let mut h = with_request("sidebyside");
        h.set_size(egui::vec2(1600.0, 900.0));
        h.run();
        h.run();
        let hint = "Press Send or Ctrl+Enter to see the response.";
        // Until something is sent, the response pane teaches the keys.
        assert!(
            h.query_by_label("Go to a request, folder or action")
                .is_some()
        );
        let rects =
            |h: &Harness<'_, App>| (h.get_by_label("Params").rect(), h.get_by_label(hint).rect());
        shot(&mut h, "44-side-by-side");
        let (p, r) = rects(&h);
        assert!(
            r.top() < p.top() && r.left() > p.right(),
            "side by side by default: {p:?} {r:?}"
        );
        h.get_by_label("Side by side").click();
        h.run();
        let (p, r) = rects(&h);
        assert!(
            r.top() > p.bottom(),
            "stacked: the response is under the request"
        );
        assert!(h.state().ws.load_state().stacked, "kept for the next start");

        // A narrow window stacks them whatever the toggle says.
        h.get_by_label("Side by side").click();
        h.run();
        h.set_size(egui::vec2(1200.0, 800.0));
        h.run();
        h.run();
        let (p, r) = rects(&h);
        assert!(h.state().side_by_side);
        assert!(r.top() > p.bottom(), "narrow: stacked {p:?} {r:?}");
        h.set_size(egui::vec2(1600.0, 900.0));
        h.run();
        h.run();
        let (p, r) = rects(&h);
        assert!(r.left() > p.right(), "wide again: side by side {p:?} {r:?}");
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

    /// The git round trip: export, a teammate's change arrives with a pull, import.
    #[test]
    fn export_and_import_go_through_the_collection_menu() {
        let mut h = with_request("export");
        let file = h.state().ws.root.join("collections/r.toml");
        // The collection's ⋯, drawn before the hovered tree row's.
        h.get_all_by_label("⋯").next().unwrap().click();
        h.run();
        h.get_by_label("Export to files").click();
        h.run();
        assert!(file.exists(), "{}", h.state().status);

        std::fs::write(&file, "url = 'http://pulled.test'\n").unwrap();
        h.get_all_by_label("⋯").next().unwrap().click();
        h.run();
        h.get_by_label("Import from files…").click();
        h.run();
        assert_eq!(draft(&h).url, "", "nothing changes before the confirmation");
        h.get_by_label("Import").click();
        h.run();
        assert_eq!(draft(&h).url, "http://pulled.test");
        assert!(h.state().dialog.is_none());
    }

    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    /// Dropping Postman's export on the dialog is the whole import; what didn't come over
    /// stays listed until the user has read it.
    #[test]
    fn a_postman_file_dropped_on_the_dialog_is_imported() {
        let mut h = with_request("postman");
        // The multipart editor behind the dialog takes dropped files too, but not this one.
        let parts = Body::Multipart { parts: Vec::new() };
        h.state_mut().open.as_mut().unwrap().draft.body = parts.clone();
        h.state_mut().req_tab = ReqTab::Body;
        let file = h.state().ws.root.join("shop.postman_collection.json");
        let shop = r#"{ "info": { "name": "Shop" }, "item": [
            { "name": "list", "request": { "method": "GET", "url": "{{base}}/items" } },
            { "name": "signed", "request": { "method": "GET", "url": "{{base}}/s",
                                             "auth": { "type": "ntlm", "ntlm": [] } } } ] }"#;
        std::fs::write(&file, shop).unwrap();
        h.get_all_by_label("⋯").next().unwrap().click();
        h.run();
        h.get_by_label("Import…").click();
        h.run();
        h.input_mut()
            .dropped_files
            .push(std::sync::Arc::new(Dropped(file)));
        h.run();
        let app = h.state();
        let shop = app.ws.collections().join("Shop");
        assert_eq!(app.ws.load_requests_in(&shop).unwrap().len(), 2);
        let Some(Dialog::Paste { note, .. }) = &app.dialog else {
            panic!("the dialog stays to say what didn't come over");
        };
        assert!(note.contains("signed: ntlm auth"), "{note}");
        assert_eq!(draft(&h).body, parts);

        // A second import of the same environment never replaces the first, which may
        // hold this machine's secrets by now.
        let env = r#"{ "name": "Prod", "values": [{ "key": "host", "value": "h" }] }"#;
        for _ in 0..2 {
            h.state_mut().dialog = Some(Dialog::Paste {
                text: env.into(),
                note: String::new(),
            });
            // The modal re-centres a frame after its content shrinks; a pointer click
            // must aim where the button ends up.
            h.run_steps(2);
            h.get_by_label("Import").click();
            h.run();
            let note = match &h.state().dialog {
                Some(Dialog::Paste { note, .. }) => Some(note.as_str()),
                _ => None,
            };
            assert_eq!(note, None, "nothing to report closes it");
        }
        assert_eq!(h.state().envs, ["dev", "Prod", "Prod copy"]);

        // Insomnia keeps its environments in the collection's export; they arrive with it,
        // under the same never-replace rule.
        let insomnia = "type: collection.insomnia.rest/5.0\nname: Ins\ncollection: []\n\
                        environments:\n  subEnvironments:\n    - name: Prod\n      data: {a: b}\n";
        h.state_mut().dialog = Some(Dialog::Paste {
            text: insomnia.into(),
            note: String::new(),
        });
        h.run_steps(2);
        h.get_by_label("Import").click();
        h.run();
        assert_eq!(h.state().envs, ["dev", "Prod", "Prod copy", "Prod copy 2"]);
        assert!(
            h.state().status.ends_with("and 1 environment"),
            "{}",
            h.state().status
        );
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
        let saved = h.state().ws.load_cookies();
        assert!(saved.contains("sid"), "{saved}");

        h.get_by_label("Cookies").click();
        h.run();
        h.get_by_label("sid=abc");
        h.get_by_label("🗑").click();
        h.run();
        assert!(h.state().cookies.rows().is_empty());
        assert!(!h.state().ws.load_cookies().contains("sid"));
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
                truncated: false,
                sent: Default::default(),
                bytes: None,
            })),
            tests: Vec::new(),
            logs: Vec::new(),
            past: None,
        });
        h.run();
    }

    #[test]
    fn dragging_in_the_tree_moves_requests_and_their_tabs_follow() {
        let ws = workspace("drag");
        let top = ws.collections();
        let api = ws.create_folder(&top, "api").unwrap();
        ws.create_request(&top, "r").unwrap();
        let mut h = harness(ws);
        h.run();
        h.state_mut().activate(top.join("r.toml"), true);
        h.state_mut().open.as_mut().unwrap().draft.url = "http://unsaved".into();
        h.run();
        let drag = |h: &mut Harness<'_, App>, from: egui::Pos2, to: egui::Pos2| {
            h.drag_at(from);
            h.run();
            // Past egui's drag threshold, then over the target.
            h.hover_at(from + egui::vec2(0.0, 12.0));
            h.run();
            h.hover_at(to);
            h.run();
            shot(h, "47-tree-drag");
            h.drop_at(to);
            h.run();
        };
        let r = h.get_all_by_label("r").next().unwrap().rect().center();
        let folder = h.get_by_label("api").rect().center();
        drag(&mut h, r, folder);
        let moved = api.join("r.toml");
        assert!(
            h.state().ws.load_request(&moved).is_ok(),
            "{}",
            h.state().status
        );
        // The tab follows the file, unsaved edits and all.
        assert_eq!(h.state().tabs[0].path, moved);
        assert_eq!(draft(&h).url, "http://unsaved");
        // And back out: the space under the tree is the top level.
        let r = h.get_all_by_label("r").next().unwrap().rect().center();
        drag(&mut h, r, egui::pos2(r.x, 600.0));
        assert!(h.state().ws.load_request(&top.join("r.toml")).is_ok());
        assert_eq!(h.state().open.as_ref().unwrap().path, top.join("r.toml"));
    }

    #[test]
    fn dragging_onto_a_row_edge_arranges_the_tree() {
        let ws = workspace("arrange");
        let top = ws.collections();
        ws.create_folder(&top, "api").unwrap();
        ws.create_request(&top, "a").unwrap();
        ws.create_request(&top, "b").unwrap();
        let mut h = harness(ws);
        h.run();
        let drag = |h: &mut Harness<'_, App>, from: egui::Pos2, to: egui::Pos2| {
            h.drag_at(from);
            h.run();
            h.hover_at(from + egui::vec2(0.0, 12.0));
            h.run();
            h.hover_at(to);
            h.run();
            h.drop_at(to);
            h.run();
        };
        let order = |h: &Harness<'_, App>| -> Vec<String> {
            (h.state().ws.tree().iter())
                .map(|n| n.path().file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        let row =
            |h: &Harness<'_, App>, name: &str| h.get_all_by_label(name).next().unwrap().rect();
        // b onto a's upper half: before a.
        let (b, a) = (row(&h, "b"), row(&h, "a"));
        drag(&mut h, b.center(), egui::pos2(a.center().x, a.top() + 2.0));
        assert_eq!(
            order(&h),
            ["api", "b.toml", "a.toml"],
            "{}",
            h.state().status
        );
        // a onto the folder header's top edge: before the folder, not into it.
        let (a, folder) = (row(&h, "a"), h.get_by_label("api").rect());
        drag(
            &mut h,
            a.center(),
            egui::pos2(folder.center().x, folder.top() + 1.0),
        );
        assert_eq!(order(&h), ["a.toml", "api", "b.toml"]);
        assert!(h.state().ws.exists(&top.join("a.toml")));
    }

    #[test]
    fn a_hovered_tree_row_offers_its_menu_without_a_right_click() {
        let ws = workspace("more");
        let top = ws.collections();
        ws.create_request(&top, "r").unwrap();
        let mut h = harness(ws);
        h.run();
        let row = h.get_all_by_label("r").next().unwrap().rect();
        // The collection header has its own ⋯; the row's is the one on the row's line.
        let on_row = |h: &Harness<'_, App>| {
            h.get_all_by_label("⋯")
                .filter(|n| n.rect().center().y.round() == row.center().y.round())
                .count()
        };
        assert_eq!(on_row(&h), 0, "only shown while hovered");
        h.hover_at(row.center());
        h.run();
        let more = h
            .get_all_by_label("⋯")
            .find(|n| n.rect().center().y.round() == row.center().y.round())
            .unwrap()
            .rect()
            .center();
        // Reaching for the button must not hide it.
        h.hover_at(more);
        h.run();
        assert_eq!(on_row(&h), 1);
        h.get_all_by_label("⋯")
            .find(|n| n.rect().center() == more)
            .unwrap()
            .click();
        h.run();
        shot(&mut h, "49-tree-more");
        h.get_by_label("Rename").click();
        h.run();
        assert!(matches!(
            &h.state().dialog,
            Some(Dialog::Name { kind: NameKind::Rename(p), .. }) if *p == top.join("r.toml")
        ));
    }

    #[test]
    fn header_names_and_content_types_complete_while_typing() {
        let mut h = with_request("hdr-complete");
        h.state_mut().req_tab = ReqTab::Headers;
        h.run();
        // URL is 0; then the blank row's key and value.
        type_into(&mut h, 1, "content-t");
        shot(&mut h, "50-header-complete");
        h.key_press(Key::Enter);
        h.run();
        assert_eq!(draft(&h).headers[0].key, "Content-Type");
        type_into(&mut h, 2, "json");
        h.get_by_label("application/json").click();
        h.run();
        assert_eq!(draft(&h).headers[0].value, "application/json");
        // Query parameters aren't headers.
        h.state_mut().req_tab = ReqTab::Params;
        h.run();
        type_into(&mut h, 1, "acc");
        assert!(h.query_by_label("Accept").is_none());
    }

    #[test]
    fn a_file_dropped_on_a_binary_body_becomes_the_body() {
        let mut h = with_request("binary-body");
        h.state_mut().req_tab = ReqTab::Body;
        h.run();
        h.get_by_label("Binary").click();
        h.run();
        let file = h.state().ws.root.join("photo.png");
        std::fs::write(&file, [0u8; 2048]).unwrap();
        h.input_mut()
            .dropped_files
            .push(std::sync::Arc::new(Dropped(file.clone())));
        h.run();
        shot(&mut h, "51-binary-body");
        let path = file.display().to_string();
        assert_eq!(draft(&h).body, Body::File { path });
        // What will go out, before Send.
        assert!(h.query_by_label_contains("image/png").is_some());
        std::fs::remove_file(&file).unwrap();
        h.run();
        assert!(h.query_by_label_contains("File not found").is_some());
    }

    #[test]
    fn the_headers_tab_lists_what_send_adds() {
        let mut h = with_request("auto-headers");
        let draft = &mut h.state_mut().open.as_mut().unwrap().draft;
        draft.url = "http://api.test/a".into();
        draft.auth = Auth::Bearer {
            token: "t0k".into(),
        };
        h.state_mut().req_tab = ReqTab::Headers;
        h.run();
        // Folded away until asked for: host, authorization, accept, accept-encoding.
        assert!(h.query_by_label("from Auth").is_none());
        h.get_by_label_contains("added on Send").click();
        h.run();
        shot(&mut h, "52-auto-headers");
        assert!(h.query_by_label("Bearer t0k").is_some());
        assert!(h.query_by_label("from Auth").is_some());
    }

    /// A body too big for RAM needs a way to disk: Send ▾ asks where (named after the
    /// URL's file), and the pane says where it went instead of showing it.
    #[test]
    fn send_and_download_puts_the_body_in_the_chosen_file() {
        let mut h = with_request("dl");
        let addr = crate::http::tests::serve(|_| ("200 OK".into(), "a,b\n1,2\n".into()));
        let url = format!("http://{addr}/files/report.csv?v=2");
        h.state_mut().open.as_mut().unwrap().draft.url = url;
        h.run();
        // Save's ⏷ comes first, on the row above.
        h.get_all_by_label("⏷").last().unwrap().click();
        h.run();
        h.get_by_label("Send and download…").click();
        h.run();
        shot(&mut h, "60-send-and-download");
        let file = h.state().ws.root.join("saved.csv");
        let Some(Dialog::SaveBody {
            path,
            download: true,
            ..
        }) = &mut h.state_mut().dialog
        else {
            panic!("the download dialog should be open");
        };
        assert!(path.ends_with("report.csv"), "named after the URL: {path}");
        *path = file.display().to_string();
        h.key_press(Key::Enter);
        wait(&mut h, |app| {
            app.pending.is_empty() && app.response.is_some()
        });
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a,b\n1,2\n");
        let view = h
            .state()
            .response
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        assert!(view.text.contains("saved.csv"), "{}", view.text);
    }

    /// An OAuth token outlives the window: saved to the workspace once granted, so the
    /// next start doesn't sign in again.
    #[test]
    fn a_granted_oauth_token_is_saved_to_the_workspace() {
        let mut h = with_request("oauth-keep");
        let provider = crate::http::tests::serve(|_| {
            let json = "200 OK\r\ncontent-type: application/json";
            (
                json.into(),
                r#"{"access_token":"gui-1","expires_in":3600}"#.into(),
            )
        });
        {
            let d = &mut h.state_mut().open.as_mut().unwrap().draft;
            d.url = crate::http::tests::echo_server();
            d.auth = Auth::OAuth2(crate::model::OAuth2 {
                grant: crate::model::Grant::ClientCredentials,
                token_url: format!("http://{provider}/token"),
                client_id: "gui".into(),
                ..Default::default()
            });
        }
        h.get_by_label("Send").click();
        wait(&mut h, |app| {
            app.pending.is_empty() && app.response.is_some()
        });
        let kept = h.state().ws.load_tokens();
        assert!(kept.contains("gui-1"), "{kept}");
    }

    /// Save as forks a variant without touching the original, and the copy picks up what
    /// its new folder passes down.
    #[test]
    fn save_as_forks_the_edits_and_leaves_the_original_as_saved() {
        let mut h = with_request("save-as");
        let ws = h.state().ws.clone();
        let team = ws.collections().join("team");
        let folder = crate::model::Folder {
            vars: vec![KeyValue::new("tenant", "t1")],
            ..Default::default()
        };
        ws.save_folder(&team, &folder).unwrap();
        h.state_mut().open.as_mut().unwrap().draft.url = "http://api.test/v2".into();
        h.run();
        h.get_all_by_label("⏷").next().unwrap().click();
        h.run();
        h.get_by_label("Save as…").click();
        h.run();
        let Some(Dialog::Name { name, .. }) = &mut h.state_mut().dialog else {
            panic!("the save-as dialog should be open");
        };
        assert_eq!(name, "r copy", "free by default, next to the original");
        *name = "team/r v2".into();
        h.key_press(Key::Enter);
        h.run();
        shot(&mut h, "61-saved-as");

        assert!(h.state().dialog.is_none());
        let copy = team.join("r v2.toml");
        assert_eq!(ws.load_request(&copy).unwrap().url, "http://api.test/v2");
        let original = ws.collections().join("r.toml");
        assert_eq!(ws.load_request(&original).unwrap().url, "", "kept as saved");
        let open = h.state().open.as_ref().unwrap();
        assert_eq!(open.path, copy, "the tab follows the copy");
        assert!(!open.dirty());
        assert_eq!(open.draft.inherited.vars.get("tenant").unwrap(), "t1");
        assert_eq!(h.state().tabs.len(), 1);

        // Never over another request.
        h.get_all_by_label("⏷").next().unwrap().click();
        h.run();
        h.get_by_label("Save as…").click();
        h.run();
        if let Some(Dialog::Name { name, .. }) = &mut h.state_mut().dialog {
            *name = "r".into();
        }
        h.key_press(Key::Enter);
        h.run();
        let Some(Dialog::Name { error, .. }) = &h.state().dialog else {
            panic!("the dialog should stay to say why");
        };
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(ws.load_request(&original).unwrap().url, "");
    }

    /// A 10 MB body took 3.6 GB to lay out for editing. Past MAX_EDIT it gets a note
    /// instead, and a paste that big goes in at the cursor without reaching the editor.
    #[test]
    fn a_body_too_big_to_edit_gets_a_note_and_a_big_paste_still_lands() {
        let mut h = with_request("big-body");
        h.state_mut().open.as_mut().unwrap().draft.body = Body::Json { text: "ab".into() };
        h.state_mut().req_tab = ReqTab::Body;
        h.run();
        let id = egui::Id::new("body");
        h.ctx.memory_mut(|m| m.request_focus(id));
        h.run();
        let mut state = egui::TextEdit::load_state(&h.ctx, id).expect("the editor has focus");
        let between = egui::text::CCursorRange::one(egui::text::CCursor::new(1));
        state.cursor.set_char_range(Some(between));
        state.store(&h.ctx, id);
        let big = "x".repeat(crate::varedit::MAX_EDIT);
        h.event(egui::Event::Paste(big.clone()));
        h.run();
        let Body::Json { text } = &draft(&h).body else {
            panic!("still a JSON body");
        };
        assert!(*text == format!("a{big}b"), "pasted at the cursor");
        assert!(h.query_by_label_contains("too big to edit").is_some());
        shot(&mut h, "62-too-big-to-edit");
        // The editor would have moved its cursor past what it inserted (and laid it all out).
        let state = egui::TextEdit::load_state(&h.ctx, id).unwrap();
        assert_eq!(
            state.cursor.char_range(),
            Some(between),
            "the editor never saw it"
        );

        h.get_all_by_label("Clear").last().unwrap().click();
        h.run();
        assert_eq!(
            draft(&h).body,
            Body::Json {
                text: String::new()
            }
        );
        assert!(h.query_by_label_contains("too big to edit").is_none());
    }

    /// A long data-driven run keeps a bounded list, but the counts cover every row and
    /// no failure is pushed out by passes.
    #[test]
    fn a_long_run_keeps_its_failures_and_counts_everything() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut run = RunState {
            id: 0,
            started: Instant::now(),
            finished: None,
            total: 0,
            items: VecDeque::new(),
            done: 0,
            failed: 0,
            tests: (0, 0),
            abort: rt.spawn(async {}).abort_handle(),
        };
        for i in 0..3 * MAX_RUN_ROWS {
            run.push(RunItem {
                iteration: i,
                name: "r".into(),
                method: "GET".into(),
                status: Ok((200, 1)),
                tests: vec![crate::script::TestResult {
                    name: "ok".into(),
                    passed: i % 100 != 0,
                    error: None,
                }],
            });
        }
        assert_eq!(run.items.len(), MAX_RUN_ROWS);
        assert_eq!((run.done, run.failed), (3 * MAX_RUN_ROWS, 30));
        assert_eq!(run.tests, (3 * MAX_RUN_ROWS - 30, 3 * MAX_RUN_ROWS));
        let failures = run.items.iter().filter(|i| i.failed()).count();
        assert_eq!(
            failures, 30,
            "the first failure, from row 0, is still there"
        );
        assert_eq!(run.items.back().unwrap().iteration, 3 * MAX_RUN_ROWS - 1);
    }

    #[test]
    fn fuzzy_ranks_whole_words_before_scattered_letters() {
        assert_eq!(fuzzy("users/get user", "adm"), None);
        let mut labels = ["admin/get user", "users/get", "orders/target"];
        labels.sort_by_key(|l| fuzzy(l, "get").unwrap());
        // Earlier first ("target" holds "get" late); the same place goes to the shorter.
        assert_eq!(labels, ["users/get", "admin/get user", "orders/target"]);
        assert!(fuzzy("admin/get user", "adm get").is_some_and(|r| r.0));
        assert!(fuzzy("orders/target", "ordtar").is_some_and(|r| r.0));
        assert!(fuzzy("orders/get", "get") < fuzzy("orders/gxext", "get"));
    }

    #[test]
    fn ctrl_k_jumps_to_a_request_folder_or_environment_from_the_keyboard() {
        let ws = workspace("switch");
        let top = ws.collections();
        for folder in ["admin", "users"] {
            let dir = ws.create_folder(&top, folder).unwrap();
            ws.create_request(&dir, "get user").unwrap();
        }
        ws.save_env(Some("prod"), &[], &[]).unwrap();
        let mut h = harness(ws);
        h.run();
        let switch = |h: &mut Harness<'_, App>, text: &str, downs: usize| {
            h.key_press_modifiers(Modifiers::COMMAND, Key::K);
            h.run();
            let hint = Some("Go to a request, folder, environment or action");
            (h.get_all_by_role(Role::TextInput))
                .find(|n| n.accesskit_node().placeholder() == hint)
                .unwrap()
                .type_text(text);
            h.run();
            for _ in 0..downs {
                h.key_press(Key::ArrowDown);
                h.run();
            }
            shot(h, "53-switcher");
            h.key_press(Key::Enter);
            h.run();
        };
        let open = |h: &Harness<'_, App>| h.state().open.as_ref().map(|o| o.path.clone());
        // Scattered letters across the folder and the name.
        switch(&mut h, "adm get", 0);
        assert_eq!(open(&h), Some(top.join("admin/get user.toml")));
        assert!(h.state().dialog.is_none());
        // Same name in two folders: the second one is a ↓ away.
        switch(&mut h, "get user", 1);
        assert_eq!(open(&h), Some(top.join("users/get user.toml")));
        switch(&mut h, "prod", 0);
        assert_eq!(h.state().active_env.as_deref(), Some("prod"));
        // Actions do what their buttons do, saved state included.
        switch(&mut h, "network", 0);
        assert!(h.state().network_editor.is_some());
        h.state_mut().network_editor = None;
        switch(&mut h, "side by", 0);
        assert!(!h.state().side_by_side && h.state().ws.load_state().stacked);
        switch(&mut h, "new req", 0);
        assert!(matches!(
            h.state().dialog,
            Some(Dialog::Name {
                kind: NameKind::NewRequest(_),
                ..
            })
        ));
        h.state_mut().dialog = None;
        // A folder opens its settings; it ranks above the longer "users/get user".
        switch(&mut h, "users", 0);
        let editing = h.state().folder_editor.as_ref().map(|f| f.dir.clone());
        assert_eq!(editing, Some(top.join("users")));
    }

    #[test]
    fn raw_sticks_to_the_content_type_it_was_chosen_for() {
        let mut h = with_request("raw-pref");
        let url = crate::http::tests::json_server(r#"{"a":1}"#.into());
        h.state_mut().open.as_mut().unwrap().draft.url = url;
        let pretty = |app: &App| match app.response.as_ref().map(|s| &s.result) {
            Some(Ok(view)) => Some(view.pretty),
            _ => None,
        };
        let send = |h: &mut Harness<'_, App>| {
            h.state_mut().response = None;
            h.get_by_label("Send").click();
            wait(h, |app| pretty(app).is_some());
        };
        send(&mut h);
        assert_eq!(pretty(h.state()), Some(true), "JSON opens pretty at first");
        h.get_by_label("Raw").click();
        h.run();
        let kept = vec!["application/json".to_owned()];
        assert_eq!(h.state().ws.load_state().raw_types, kept);
        send(&mut h);
        assert_eq!(
            pretty(h.state()),
            Some(false),
            "the next JSON response opens raw"
        );
        h.get_by_label("Pretty").click();
        h.run();
        send(&mut h);
        assert_eq!(pretty(h.state()), Some(true));
        assert!(h.state().ws.load_state().raw_types.is_empty());
        // A past response picked from History opens the same way as a new one.
        h.get_by_label("Raw").click();
        h.run();
        h.get_by_label_contains("History: ").click();
        h.run();
        let by = egui_kittest::kittest::By::new().label_contains("200  ·");
        h.query_all(by).next().unwrap().click();
        h.run();
        let shown = h.state().response.as_ref().unwrap();
        assert!(shown.past.is_some(), "a past response is shown");
        assert_eq!(pretty(h.state()), Some(false));
        // The type, not its parameters: a charset doesn't make another kind of body.
        let head = http::Response {
            status: 200,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            elapsed: Duration::ZERO,
            headers: vec![(
                "Content-Type".into(),
                "Application/JSON; charset=utf-8".into(),
            )],
            body: String::new(),
            truncated: false,
            sent: Default::default(),
            bytes: None,
        };
        assert_eq!(media_type(&head), "application/json");
    }

    #[test]
    fn a_filter_used_again_moves_to_the_front_and_the_list_stays_short() {
        let mut recent = Vec::new();
        for i in 0..12 {
            remember(&mut recent, &format!("$.f{i}"));
        }
        remember(&mut recent, "$.f5");
        assert_eq!(recent.len(), MAX_RECENT_FILTERS);
        assert_eq!(&recent[..3], ["$.f5", "$.f11", "$.f10"]);
        assert_eq!(recent.iter().filter(|r| *r == "$.f5").count(), 1);
    }

    #[test]
    fn a_json_filter_narrows_the_body_but_save_keeps_it_whole() {
        let mut h = with_request("json-filter");
        let body = r#"{"items":[{"id":1},{"id":2}]}"#;
        h.state_mut().response = Some(Shown {
            result: Ok(into_view(http::Response {
                status: 200,
                reason: "OK".into(),
                version: "HTTP/1.1".into(),
                elapsed: Duration::ZERO,
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.into(),
                truncated: false,
                sent: Default::default(),
                bytes: None,
            })),
            tests: Vec::new(),
            logs: Vec::new(),
            past: None,
        });
        h.run();
        let view = |h: &Harness<'_, App>| {
            let shown = h.state().response.as_ref().unwrap();
            let view = shown.result.as_ref().unwrap();
            (view.text.clone(), view.raw().to_owned())
        };
        let hint = Some("Filter: $.items[*].id");
        // Found by its placeholder, which stays until something is typed.
        for step in 0..2 {
            let field = (h.get_all_by_role(Role::TextInput))
                .find(|n| n.accesskit_node().placeholder() == hint)
                .unwrap();
            // In two goes: "$.items" applies too, but isn't what was meant.
            match step {
                0 => field.click(),
                _ => {
                    field.type_text("$.items");
                    h.run();
                    h.event(egui::Event::Text("[*].id".into()));
                }
            }
            h.run();
        }
        shot(&mut h, "54-json-filter");
        assert_eq!(view(&h), ("[\n  1,\n  2\n]".into(), body.into()));
        // Raw and back: still filtered, still the server's bytes underneath.
        h.get_by_label("Raw").click();
        h.run();
        assert_eq!(view(&h), ("[\n  1,\n  2\n]".into(), body.into()));
        h.get_by_label("Pretty").click();
        h.run();
        assert_eq!(view(&h).0, "[\n  1,\n  2\n]");
        // An example is the response, not the view of it.
        h.get_by_label("Save as example").click();
        h.run();
        assert!(draft(&h).examples[0].body.contains("\"items\""));
        // The filter's ×, drawn after the tab strip's.
        h.get_all_by_label("×").last().unwrap().click();
        h.run();
        assert!(view(&h).0.starts_with("{\n  \"items\""), "{}", view(&h).0);
        // Kept once typing was done (clicking Raw took the focus), not every prefix of it.
        let kept = vec!["$.items[*].id".to_owned()];
        assert_eq!(h.state().ws.load_state().recent_filters, kept);
        h.get_by_label("Recent").click();
        h.run();
        shot(&mut h, "67-recent-filters");
        h.get_by_label("$.items[*].id").click();
        h.run();
        assert_eq!(view(&h).0, "[\n  1,\n  2\n]");
    }

    /// `{{?name}}` is asked for on Send, goes into that request only, and is offered again
    /// next time, without ever landing in an environment.
    #[test]
    fn prompt_variables_are_asked_for_on_send() {
        let mut h = with_request("prompt");
        let addr = crate::http::tests::echo_server();
        let draft = &mut h.state_mut().open.as_mut().unwrap().draft;
        draft.url = format!("{addr}/{{{{?id}}}}");
        draft.headers = vec![KeyValue::new("X-Key", "{{ ?api key }}")];
        h.run();
        assert!(h.query_by_label_contains("Undefined").is_none());
        h.get_by_label("Send").click();
        h.run();
        let Some(Dialog::Prompt { values, .. }) = &mut h.state_mut().dialog else {
            panic!("Send asks first");
        };
        let names: Vec<_> = values.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, ["?id", "?api key"]);
        values[0].1 = "7".into();
        values[1].1 = "k".into();
        h.run_steps(2);
        // The dialog's Send, drawn after the request's.
        h.get_all_by_label("Send").last().unwrap().click();
        wait(&mut h, |app| {
            app.pending.is_empty() && app.response.is_some()
        });
        let shown = h.state().response.as_ref().unwrap();
        let text = shown.result.as_ref().unwrap().text.to_lowercase();
        assert!(
            text.contains("/7 http/1.1") && text.contains("x-key: k"),
            "{text}"
        );
        assert!(!h.state().vars.contains_key("?id"));

        h.get_by_label("Send").click();
        h.run();
        let Some(Dialog::Prompt { values, .. }) = &h.state().dialog else {
            panic!("asked again: a new value may be wanted");
        };
        assert_eq!(values[0].1, "7", "the last answer is offered");
    }

    /// The schema snippet adds to what the user wrote, and what it adds passes on the
    /// response it came from.
    #[test]
    fn a_schema_test_is_added_from_the_response() {
        let mut h = with_request("schema-test");
        let body = r#"{"items":[{"id":1},{"id":2}],"next":null}"#;
        let headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        h.state_mut().response = Some(Shown {
            result: Ok(into_view(http::Response {
                status: 200,
                reason: "OK".into(),
                version: "HTTP/1.1".into(),
                elapsed: Duration::ZERO,
                headers: headers.clone(),
                body: body.into(),
                truncated: false,
                sent: Default::default(),
                bytes: None,
            })),
            tests: Vec::new(),
            logs: Vec::new(),
            past: None,
        });
        h.state_mut().open.as_mut().unwrap().draft.tests = "// mine".into();
        (h.state_mut().req_tab, h.state_mut().script_tab) = (ReqTab::Scripts, ScriptTab::Post);
        h.run();
        h.get_by_label("Snippets").click();
        h.run();
        h.get_by_label("Response matches its schema").click();
        h.run();
        let tests = draft(&h).tests.clone();
        assert!(tests.starts_with("// mine\n\npm.test("), "{tests}");

        let (empty, wire) = (
            HashMap::new(),
            crate::script::WireRequest {
                method: "GET".into(),
                url: String::new(),
                headers: Vec::new(),
            },
        );
        let out = crate::script::run(
            &tests,
            &crate::script::Input {
                name: "t",
                iteration: 0,
                iteration_count: 1,
                data: &empty,
                env: &empty,
                collection: &empty,
                globals: &empty,
                locals: &empty,
                request: &wire,
                response: Some(crate::script::ScriptResponse {
                    code: 200,
                    status: "OK",
                    time: 1,
                    headers: &headers,
                    body,
                }),
                cookie_url: "",
                jar: None,
                client: None,
            },
        );
        assert_eq!(out.error, None);
        assert!(
            out.tests.len() == 1 && out.tests[0].passed,
            "{:?}",
            out.tests
        );
    }

    #[test]
    fn the_authorization_code_grant_asks_for_where_to_sign_in() {
        let mut h = with_request("oauth-code");
        h.state_mut().open.as_mut().unwrap().draft.auth = Auth::OAuth2(Default::default());
        h.state_mut().req_tab = ReqTab::Auth;
        h.run();
        assert!(h.query_by_label("Auth URL").is_none());
        h.get_by_label("Authorization code").click();
        h.run();
        shot(&mut h, "55-oauth-code");
        assert!(matches!(
            &draft(&h).auth,
            Auth::OAuth2(o) if o.grant == model::Grant::AuthorizationCode
        ));
        assert!(h.query_by_label("Auth URL").is_some());
        assert!(h.query_by_label("Redirect URI").is_some());
        assert!(h.query_by_label_contains("opens your browser").is_some());
    }

    #[test]
    fn the_sidebar_folds_away_and_prod_can_be_painted_red() {
        let ws = workspace("env-color");
        ws.create_request(&ws.collections(), "r").unwrap();
        ws.save_env(Some("prod"), &[], &[]).unwrap();
        let mut h = harness(ws);
        h.state_mut().set_env(Some("prod".into()));
        h.run();
        h.get_by_label("Sidebar").click();
        h.run();
        assert!(h.query_by_label("+ Request").is_none(), "the tree is gone");
        assert!(h.state().ws.load_state().hide_sidebar, "and stays gone");
        h.get_by_label("Sidebar").click();
        h.run();
        h.get_by_label("●").click();
        h.run();
        h.get_by_label("● Red").click();
        h.run();
        shot(&mut h, "58-env-color");
        assert_eq!(h.state().env_color(), Some(RED));
        let kept = h.state().ws.load_state().env_colors;
        assert_eq!(kept.get("prod"), Some(&[RED.r(), RED.g(), RED.b()]));
        // Other environments aren't painted.
        h.state_mut().set_env(None);
        assert_eq!(h.state().env_color(), None);
        h.state_mut().set_env(Some("prod".into()));
        h.run();
        h.get_by_label("●").click();
        h.run();
        h.get_by_label("None").click();
        h.run();
        assert_eq!(h.state().env_color(), None);
    }

    #[test]
    fn a_raw_body_says_its_type_and_xml_comes_out_indented() {
        let mut h = with_request("raw-xml");
        let text = "<a><b>1</b></a>".to_owned();
        h.state_mut().open.as_mut().unwrap().draft.body = Body::Text { text };
        h.state_mut().req_tab = ReqTab::Body;
        h.run();
        let content_type = |h: &Harness<'_, App>| {
            let headers = &draft(h).headers;
            let row = headers.iter().find(|r| r.key == "Content-Type");
            row.map(|r| r.value.clone())
        };
        let picker = |h: &Harness<'_, App>, value: &str| {
            (h.get_all_by_role(Role::ComboBox))
                .find(|n| n.accesskit_node().value().as_deref() == Some(value))
                .unwrap()
                .click();
        };
        picker(&h, "Text");
        h.run();
        h.get_by_label("XML").click();
        h.run();
        assert_eq!(content_type(&h).as_deref(), Some("application/xml"));
        h.get_by_label("Beautify").click();
        h.run();
        shot(&mut h, "59-raw-xml");
        let Body::Text { text } = &draft(&h).body else {
            panic!("not text")
        };
        assert_eq!(text, "<a>\n  <b>1</b>\n</a>");
        picker(&h, "XML");
        h.run();
        // The body mode row has a "Text" too; the list's comes last.
        h.get_all_by_label("Text").last().unwrap().click();
        h.run();
        assert_eq!(content_type(&h), None, "plain text needs no header");

        // An XML response reads indented; Raw is what came.
        let view = into_view(http::Response {
            status: 200,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            elapsed: Duration::ZERO,
            headers: vec![("content-type".into(), "text/xml".into())],
            body: "<a><b>1</b></a>".into(),
            truncated: false,
            sent: Default::default(),
            bytes: None,
        });
        assert_eq!(
            (view.text.as_str(), view.raw()),
            ("<a>\n  <b>1</b>\n</a>", "<a><b>1</b></a>")
        );
        assert!(!view.json, "no JSON colours on XML");
    }

    #[test]
    fn the_tests_tab_narrows_to_failures() {
        let mut h = with_request("tests-filter");
        let test = |name: &str, passed| TestResult {
            name: name.into(),
            passed,
            error: (!passed).then(|| "expected 1 to equal 2".into()),
        };
        h.state_mut().response = Some(Shown {
            result: Err("x".into()),
            tests: vec![
                test("status ok", true),
                test("has id", false),
                test("fast", true),
            ],
            logs: Vec::new(),
            past: None,
        });
        h.state_mut().resp_tab = RespTab::Tests;
        h.run();
        assert!(h.query_by_label("status ok").is_some());
        h.get_by_label("Failed (1)").click();
        h.run();
        shot(&mut h, "56-tests-filter");
        assert!(h.query_by_label("status ok").is_none());
        assert!(h.query_by_label("has id").is_some());
        h.get_by_label("All (3)").click();
        h.run();
        assert!(h.query_by_label("fast").is_some());
    }

    #[test]
    fn the_breadcrumb_leads_to_the_folders_above() {
        let ws = workspace("crumbs");
        let api = ws.create_folder(&ws.collections(), "api").unwrap();
        let users = ws.create_folder(&api, "users").unwrap();
        ws.create_request(&users, "get user").unwrap();
        let mut h = harness(ws);
        h.state_mut().activate(users.join("get user.toml"), true);
        h.run();
        shot(&mut h, "57-breadcrumb");
        // The tree is folded, so this "users" is the breadcrumb's.
        h.get_by_label("users").click();
        h.run();
        assert_eq!(
            h.state().folder_editor.as_ref().map(|e| e.dir.clone()),
            Some(users)
        );
    }

    #[test]
    fn the_cookies_tab_lists_what_the_response_sets() {
        let mut h = with_request("resp-cookies");
        let cookie = |v: &str| ("set-cookie".to_owned(), v.to_owned());
        let head = |headers| http::Response {
            status: 200,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            elapsed: Duration::ZERO,
            headers,
            body: String::new(),
            truncated: false,
            sent: Default::default(),
            bytes: None,
        };
        let headers = vec![
            cookie("sid=abc; Path=/; Max-Age=3600; HttpOnly"),
            ("content-type".into(), "text/plain".into()),
            cookie("theme=dark; Domain=example.com; Expires=Wed, 21 Oct 2037 07:28:00 GMT"),
        ];
        h.state_mut().response = Some(Shown {
            result: Ok(into_view(head(headers))),
            tests: Vec::new(),
            logs: Vec::new(),
            past: None,
        });
        h.run();
        h.get_by_label("Cookies (2)").click();
        h.run();
        shot(&mut h, "48-response-cookies");
        for cell in [
            "sid",
            "abc",
            "in 3600 s",
            "example.com",
            "2037-10-21 07:28 UTC",
        ] {
            assert!(h.query_by_label(cell).is_some(), "no {cell}");
        }
        // No Set-Cookie, no tab: it doesn't stay stuck on an empty one.
        h.state_mut().response.as_mut().unwrap().result = Ok(into_view(head(Vec::new())));
        h.run();
        assert!(h.query_by_label_contains("Cookies (").is_none());
        assert!(h.query_by_label("sid").is_none());
        assert!(
            h.query_by_label("Expires").is_none(),
            "back on the body, not an empty table"
        );
    }

    #[test]
    fn an_earlier_response_can_be_looked_back_at() {
        let mut h = with_request("past");
        let url = crate::http::tests::echo_server();
        let body = |app: &App| match app.response.as_ref().map(|s| &s.result) {
            Some(Ok(view)) => view.text.to_lowercase(),
            _ => String::new(),
        };
        for path in ["first", "second"] {
            h.state_mut().open.as_mut().unwrap().draft.url = format!("{url}/{path}");
            h.get_by_label("Send").click();
            wait(&mut h, |app| {
                body(app).starts_with(&format!("get /users/{path} "))
            });
        }
        h.get_by_label("History: just now").click();
        h.run();
        shot(&mut h, "46-response-history");
        // Newest first: the second entry is the first send.
        h.get_all_by_label_contains("200  ·  ")
            .nth(1)
            .unwrap()
            .click();
        h.run();
        assert!(
            body(h.state()).starts_with("get /users/first "),
            "{}",
            body(h.state())
        );
        // Kept on disk, so a restart still has them; Clear empties this request's list.
        let path = h.state().open.as_ref().unwrap().path.clone();
        assert_eq!(h.state().ws.responses(&path).len(), 2);
        // The tree row carries the last status, also after a restart.
        assert!(h.query_by_label("200").is_some());
        assert_eq!(h.state().ws.last_statuses().get(&path), Some(&200));
        // × deletes one: the first send, the one on screen, which stays shown but is no
        // longer one History can bring back.
        h.get_by_label("History: just now").click();
        h.run();
        // The last ×: the tab strip has its own, and the oldest row comes last.
        h.get_all_by_label("×").last().unwrap().click();
        h.run();
        let left = h.state().ws.responses(&path);
        assert_eq!(left.len(), 1);
        assert!(body(h.state()).starts_with("get /users/first "));
        assert_eq!(h.state().response.as_ref().unwrap().past, None);
        // Now plain "History", after the sidebar tab of that name.
        h.get_all_by_label("History").last().unwrap().click();
        h.run();
        h.get_by_label("Clear history").click();
        h.run();
        assert!(h.state().ws.responses(&path).is_empty());
        assert!(h.query_by_label_contains("History: ").is_none());
        assert!(h.query_by_label("200").is_none(), "no status left to show");
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

        // One letter in a big body: enough hits to step through, not millions of offsets.
        show_response(&mut h, "text/plain", "ab\n".repeat(2 * MAX_HITS));
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run();
        h.event(egui::Event::Text("a".into()));
        h.run();
        h.get_by_label(&format!("1/{MAX_HITS}+"));
    }

    #[test]
    fn find_can_match_case_whole_words_or_a_pattern() {
        let mut h = with_request("find-options");
        let body = "NEEDLE 7\nneedles 42\nhay 7\n";
        show_response(&mut h, "text/plain", body.into());
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run();
        let find = |h: &mut Harness<'_, App>, query: &str| {
            let shown = h.state_mut().response.as_mut().unwrap();
            shown.result.as_mut().unwrap().find.query = query.into();
            h.run();
        };
        find(&mut h, "needle");
        h.get_by_label("1/2");
        h.get_by_label("Aa").click();
        h.run();
        h.get_by_label("1/1"); // only "needles"
        h.get_by_label("Aa").click();
        h.get_by_label("W").click();
        h.run();
        h.get_by_label("1/1"); // only "NEEDLE": "needles" goes on
        h.get_by_label("W").click();
        h.get_by_label(".*").click();
        find(&mut h, r"\d+");
        h.get_by_label("1/3");
        shot(&mut h, "66-find-pattern");
        // Matches of nothing would be thousands of invisible hits.
        find(&mut h, "z*");
        h.get_by_label("0/0");
        find(&mut h, "(");
        h.get_by_label("bad pattern");
        // As text, the same query is just a bracket.
        h.get_by_label(".*").click();
        h.run();
        h.get_by_label("0/0");

        // Esc in the field closes the row, and its highlights with it; the Find button
        // opens it again.
        h.ctx.memory_mut(|m| m.request_focus(egui::Id::new("find")));
        h.run();
        h.key_press(Key::Escape);
        h.run();
        assert!(h.query_by_label("0/0").is_none() && h.query_by_label("Aa").is_none());
        let shown = h.state().response.as_ref().unwrap();
        let find = &shown.result.as_ref().unwrap().find;
        assert!(
            find.query.is_empty() && find.hits.is_empty(),
            "nothing left marked"
        );
        h.get_by_label("Find").click();
        h.run();
        h.get_by_label("Aa");
        assert!(
            h.ctx.memory(|m| m.has_focus(egui::Id::new("find"))),
            "ready to type"
        );
    }

    #[test]
    fn a_folded_json_block_hides_its_lines_until_find_needs_them() {
        let mut h = with_request("fold");
        let long = "z".repeat(400);
        let body =
            format!(r#"{{"a":{{"x":"NEEDLE","y":2}},"w":"{long}","list":[1,2],"b":"tail"}}"#);
        show_response(&mut h, "application/json", body);
        let visible = |h: &Harness<'_, App>, line: &str| h.query_by_label(line).is_some();
        // Lines that open a block get an arrow: the body, "a" and "list".
        assert_eq!(h.get_all_by_label("Fold").count(), 3);
        h.get_all_by_label("Fold").nth(1).unwrap().click();
        h.run();
        shot(&mut h, "63-json-fold");
        assert!(!visible(&h, r#"    "x": "NEEDLE","#));
        assert!(!visible(&h, "  },"), "the closing line folds away too");
        assert!(visible(&h, "… }") && visible(&h, r#"  "list": ["#));
        h.get_by_label("Unfold").click();
        h.run();
        assert!(visible(&h, r#"    "x": "NEEDLE","#));

        // Wrapped, a fold hides rows rather than lines: "w" takes several rows, so the
        // rows of "list" aren't at its line numbers.
        h.get_by_label("Wrap").click();
        h.run();
        h.get_all_by_label("Fold").nth(2).unwrap().click();
        h.run();
        assert!(!visible(&h, "    1,"));
        assert!(visible(&h, r#"  "list": ["#) && visible(&h, r#"  "b": "tail""#));
        h.get_all_by_label("Fold").next().unwrap().click();
        h.run();
        assert!(!visible(&h, r#"  "b": "tail""#));
        // Find jumps to a hit inside a fold and opens just the folds around it.
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run();
        h.event(egui::Event::Text("needle".into()));
        h.run();
        assert!(visible(&h, r#"    "x": "NEEDLE","#));
        assert_eq!(h.get_all_by_label("Unfold").count(), 1, "list stays folded");
    }

    #[test]
    fn folds_map_visible_rows_past_hidden_ones() {
        let text = "{\n  \"a\": [\n    1\n  ],\n  \"b\": {}\n}";
        let starts = line_starts(text);
        assert_eq!(fold_end(text, &starts, 0), Some(5));
        assert_eq!(fold_end(text, &starts, 1), Some(3));
        // `{}` closes on its own line: nothing to fold.
        assert_eq!(fold_end(text, &starts, 4), None);
        let folds = BTreeMap::from([(0, 5), (1, 3)]);
        // The inner fold is inside the outer one and adds nothing.
        assert_eq!(hidden_ranges(&folds), [(1, 5)]);
        let folds = BTreeMap::from([(1, 3)]);
        let hidden = hidden_ranges(&folds);
        assert_eq!(hidden, [(2, 2)]);
        let shown: Vec<_> = (0..4).map(|v| unhide(&hidden, v)).collect();
        assert_eq!(shown, [0, 1, 4, 5]);
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
        assert_eq!(h.state().globals["apiKey"], "k-123");
        let reopened = Workspace::open(h.state().ws.root.clone()).unwrap();
        assert_eq!(reopened.env_vars(None).unwrap()["apiKey"], "k-123");
        h.get_by_label("👁").click();
        h.run();
        shot(&mut h, "21-quick-look");
        assert!(h.query_by_label("k-123").is_some());
        assert!(
            h.query_by_label("127.0.0.1:1").is_some(),
            "env vars listed too"
        );
    }

    /// The request the test brokers in `mqtt::tests` expect, with its will through a
    /// variable.
    fn mqtt_request(name: &str, port: u16) -> Harness<'static, App> {
        let mut h = with_request(name);
        let d = &mut h.state_mut().open.as_mut().unwrap().draft;
        d.method = "MQTT".into();
        d.url = format!("mqtt://127.0.0.1:{port}");
        d.auth = Auth::Basic {
            username: "u".into(),
            password: "p".into(),
        };
        d.mqtt.will_topic = "status/{{device}}".into();
        d.mqtt.will_payload = "offline".into();
        (d.mqtt.will_qos, d.mqtt.will_retain) = (1, true);
        let topic = |filter: &str| model::Topic {
            filter: filter.into(),
            ..Default::default()
        };
        d.mqtt.topics = vec![topic("a/#"), topic("denied")];
        h.state_mut().vars.insert("device".into(), "tester".into());
        h
    }

    fn got(app: &App, text: &str) -> bool {
        let events = app.stream.as_ref().unwrap().events();
        (events.iter()).any(|(_, e)| matches!(e, Event::In(t) if t == text))
    }

    /// Topics subscribe on Connect, Send publishes where the request says, and a topic
    /// that can't be published to is refused before it reaches the broker.
    #[test]
    fn mqtt_subscribes_publishes_and_disconnects() {
        let mut h = mqtt_request("mqtt", crate::mqtt::tests::broker());
        h.run();
        assert!(h.query_by_label("Params").is_none(), "MQTT has no query");
        // Only the will is set, and the tab says so.
        h.get_by_label("Settings ●").click();
        h.run();
        h.state_mut().open.as_mut().unwrap().draft.mqtt.client_id = "tester".into();
        h.run();
        shot(&mut h, "65-mqtt-settings");
        h.get_by_label("Topics (2)").click();
        h.run();
        h.get_by_label("Connect").click();
        wait_live(&mut h, |app| got(app, "[a/1] hello"));

        h.state_mut().stream.as_mut().unwrap().compose = "ping".into();
        for (topic, why) in [("", "needs a topic"), ("cmd/#", "subscribing")] {
            h.state_mut().open.as_mut().unwrap().draft.mqtt.topic = topic.into();
            h.get_by_label("Send").click();
            h.run_steps(2);
            assert!(h.state().status.contains(why), "{}", h.state().status);
        }
        // 3.1.1 has nowhere to put them; dropping them silently would mislead.
        let m = &mut h.state_mut().open.as_mut().unwrap().draft.mqtt;
        (m.topic, m.user_properties) = ("cmd".into(), vec![KeyValue::new("unit", "C")]);
        h.get_by_label("Send").click();
        h.run_steps(2);
        assert!(h.state().status.contains("MQTT 5"), "{}", h.state().status);
        h.state_mut()
            .open
            .as_mut()
            .unwrap()
            .draft
            .mqtt
            .user_properties[0]
            .enabled = false;
        h.get_by_label("Send").click();
        wait_live(&mut h, |app| got(app, "[a/echo] echo cmd ping"));
        shot(&mut h, "33-mqtt");

        // Unticking a topic while connected unsubscribes it then and there.
        let ticks: Vec<_> = h.get_all_by_role(Role::CheckBox).collect();
        assert_eq!(
            ticks[1].accesskit_node().toggled(),
            Some(egui::accesskit::Toggled::True)
        );
        ticks[1].click();
        wait_live(&mut h, |app| {
            let events = app.stream.as_ref().unwrap().events();
            (events.iter()).any(|(_, e)| *e == Event::Info("unsubscribed: denied".into()))
        });

        h.get_by_label("Disconnect").click();
        wait(&mut h, |app| !app.stream.as_ref().unwrap().live);
        let events = h.state().stream.as_ref().unwrap().events();
        let (_, last) = events.last().unwrap();
        assert_eq!(*last, Event::Closed("disconnected".into()), "{events:?}");
    }

    /// MQTT 5 user properties go out with each publish, variables filled in; the broker
    /// echoes them back.
    #[test]
    fn mqtt_5_publishes_user_properties() {
        let mut h = mqtt_request("mqtt5", crate::mqtt::tests::broker_v5());
        let m = &mut h.state_mut().open.as_mut().unwrap().draft.mqtt;
        (m.client_id, m.v5, m.topic) = ("tester".into(), true, "cmd".into());
        let mut off = KeyValue::new("off", "x");
        off.enabled = false;
        m.user_properties = vec![KeyValue::new("from", "{{device}}"), off];
        h.run();
        h.get_by_label("Properties (1)").click();
        h.run();
        h.get_by_label("Connect").click();
        wait_live(&mut h, |app| got(app, "[a/1] hello"));
        h.state_mut().stream.as_mut().unwrap().compose = "ping".into();
        h.get_by_label("Send").click();
        wait_live(&mut h, |app| {
            got(app, "[a/echo · from=tester] echo cmd ping")
        });
        shot(&mut h, "67-mqtt-properties");
        h.get_by_label("Disconnect").click();
        wait(&mut h, |app| !app.stream.as_ref().unwrap().live);
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
            s.events()
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
            .events()
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

    /// Keys, strings, numbers and literals get their own colours, a quote inside a
    /// string doesn't end it, and a row that starts inside a string continues it.
    #[test]
    fn json_rows_are_tokenized_for_colour() {
        use Token::*;
        let row = r#"  "k\"ey": "v,1", "n": -1.5e3, "t": [true, null]"#;
        let kinds: Vec<(&str, Token)> = {
            let mut at = 0;
            (json_tokens(row, false).into_iter())
                .map(|(end, t)| (std::mem::replace(&mut at, end), end, t))
                .map(|(from, to, t)| (&row[from..to], t))
                .collect()
        };
        assert_eq!(
            kinds,
            [
                ("  ", Punct),
                (r#""k\"ey""#, Key),
                (": ", Punct),
                (r#""v,1""#, Str),
                (", ", Punct),
                (r#""n""#, Key),
                (": ", Punct),
                ("-1.5e3", Num),
                (", ", Punct),
                (r#""t""#, Key),
                (": [", Punct),
                ("true", Lit),
                (", ", Punct),
                ("null", Lit),
                ("]", Punct),
            ]
        );
        assert_eq!(json_tokens(r#"ue", 1"#, true)[0], (3, Str));
    }

    /// Wrapped rows keep `show_rows` usable: each holds at most `cols` columns (CJK
    /// counting two), only a line's first row is numbered, and a row cut inside a
    /// string knows it.
    #[test]
    fn long_lines_wrap_into_numbered_rows() {
        let text = "{\"a\": \"0123456789\"}\n中文中文中文\n";
        let rows = wrap_rows(text, &line_starts(text), 8);
        let shown: Vec<(&str, u32, bool)> = (0..rows.len())
            .map(|i| {
                let end = rows.get(i + 1).map_or(text.len(), |r| r.start);
                (
                    text[rows[i].start..end].trim_end(),
                    rows[i].line,
                    rows[i].in_string,
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("{\"a\":", 1, false),
                ("\"0123456", 0, false),
                ("789\"}", 0, true),
                ("中文中文", 2, false),
                ("中文", 0, false),
            ]
        );
        // Between words when there's room, not inside one; never after the indentation, and
        // not before a space (it hangs instead).
        let text = "aa bbbb cc\n    aaaaaaaa\naaaaaa bb\n";
        let rows = wrap_rows(text, &line_starts(text), 6);
        let starts: Vec<usize> = rows.iter().map(|r| r.start).collect();
        assert_eq!(starts, [0, 3, 8, 11, 17, 24, 31]);
        let text = "\"aa bbbbbb\"";
        let rows = wrap_rows(text, &line_starts(text), 6);
        assert_eq!(
            (rows[1].start, rows[1].in_string),
            (4, true),
            "a word inside a string"
        );
    }

    /// The response toolbar: Raw shows the body as received and Pretty brings the
    /// indented one back; Wrap is kept across restarts; Save… writes what the server
    /// sent to the path given, Enter confirming.
    #[test]
    fn response_body_switches_wraps_and_saves() {
        let mut h = with_request("resp-tools");
        let raw = r#"{"a":[1,2],"b":"x"}"#;
        show_response(&mut h, "application/json", raw.into());
        let text = |h: &Harness<'_, App>| {
            let shown = h.state().response.as_ref().unwrap();
            shown.result.as_ref().unwrap().text.clone()
        };
        assert!(text(&h).starts_with("{\n  \"a\": ["), "{}", text(&h));
        h.get_by_label("Raw").click();
        h.run();
        assert_eq!(text(&h), raw);
        h.get_by_label("Pretty").click();
        h.run();
        assert!(text(&h).starts_with("{\n"));

        let look = r#"{"id":42,"name":"中文名稱","tags":["a","b"],"ok":true,"none":null,"price":-1.5e3,"note":"a long value that runs well past the width of the pane so that wrapping has something to do, again and again and again and again and again and again and again"}"#;
        show_response(&mut h, "application/json", look.into());
        shot(&mut h, "40-response-pretty");
        h.get_by_label("Wrap").click();
        h.run();
        assert!(h.state().wrap_response);
        h.get_by_label("Raw").click();
        h.run();
        shot(&mut h, "41-response-raw-wrapped");
        show_response(&mut h, "application/json", raw.into());
        let ws = h.state().ws.clone();
        assert!(ws.load_state().wrap_response, "kept for the next start");

        h.get_by_label("Save…").click();
        h.run();
        let file = ws.root.join("body.json");
        let Some(Dialog::SaveBody { path, .. }) = &mut h.state_mut().dialog else {
            panic!("the save dialog should be open");
        };
        assert!(path.ends_with("r.json"), "named after the request: {path}");
        *path = file.display().to_string();
        h.key_press(Key::Enter);
        h.run();
        assert!(h.state().dialog.is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), raw, "as received");
    }

    /// An image response shows as one, and Save… writes the very bytes that came: read as
    /// text, they'd be mangled for good.
    #[test]
    fn an_image_response_is_previewed_and_saved_byte_for_byte() {
        let mut h = with_request("image");
        let img = image::RgbaImage::from_fn(120, 80, |x, y| {
            image::Rgba([(x * 2) as u8, (y * 3) as u8, 160, 255])
        });
        let mut png = Vec::new();
        (img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)).unwrap();
        let show = |h: &mut Harness<'_, App>, bytes: Vec<u8>| {
            h.state_mut().response = Some(Shown {
                result: Ok(into_view(http::Response {
                    status: 200,
                    reason: "OK".into(),
                    version: "HTTP/1.1".into(),
                    elapsed: Duration::ZERO,
                    headers: vec![("content-type".into(), "image/png".into())],
                    body: String::new(),
                    bytes: Some(bytes),
                    truncated: false,
                    sent: Default::default(),
                })),
                tests: Vec::new(),
                logs: Vec::new(),
                past: None,
            });
            h.run();
        };
        show(&mut h, png.clone());
        shot(&mut h, "64-image-preview");
        h.get_by_label("120 × 80");
        h.get_by_label(&human_size(png.len())); // the bar's size is the bytes'
        // The text tools have no text to work on.
        for gone in ["Copy", "Save as example", "Wrap"] {
            assert!(h.query_by_label(gone).is_none(), "{gone}");
        }
        h.get_by_label("Save…").click();
        h.run();
        let file = h.state().ws.root.join("body.png");
        let Some(Dialog::SaveBody { path, .. }) = &mut h.state_mut().dialog else {
            panic!("the save dialog should be open");
        };
        assert!(path.ends_with("r.png"), "named for its type: {path}");
        *path = file.display().to_string();
        h.key_press(Key::Enter);
        h.run();
        assert_eq!(std::fs::read(&file).unwrap(), png);

        // Not a picture it reads: its size, and how to keep it.
        show(&mut h, b"PK\x03\x04\xff\xff".to_vec());
        assert!(h.query_by_label_contains("6 B of binary data").is_some());
    }

    /// A small file can claim a huge picture: decoding one would take more RAM than a VDI
    /// has. This PNG of zeros is a few KB, and 70 MB decoded.
    #[test]
    fn an_image_too_big_to_decode_is_refused() {
        let img = image::GrayImage::new(8400, 8400);
        let mut png = Vec::new();
        (img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)).unwrap();
        let ctx = egui::Context::default();
        let err = decode_image(&ctx, &png).err().unwrap();
        assert!(err.contains("limit"), "{err}");
    }

    /// A chatty stream keeps a bounded log: big messages keep their start, and old
    /// events scroll away by bytes as well as by count.
    #[test]
    fn the_stream_log_is_capped_in_bytes() {
        let mut log = Log::default();
        for i in 0..400 {
            log.push(
                Duration::ZERO,
                Event::In(format!("{i} {}", "x".repeat(100_000))),
            );
        }
        assert!(log.bytes <= MAX_EVENT_BYTES, "{}", log.bytes);
        let kept = log.events.len() as u64;
        assert_eq!(log.dropped + kept, 400);
        let (_, Event::In(last)) = log.events.back().unwrap() else {
            panic!()
        };
        assert!(last.starts_with("399 xxx") && last.ends_with("bytes in all)"));
        assert!(last.len() < MAX_EVENT_TEXT + 32);
        log.push(Duration::ZERO, Event::Closed("bye".into()));
        assert!(log.ended);
    }

    /// Ten background tabs of big responses would be more than the VDI has free: a big
    /// one isn't kept, and coming back says why it's gone. A small one is kept.
    #[test]
    fn background_tabs_keep_only_small_responses() {
        let mut h = with_request("parked");
        let ws = h.state().ws.clone();
        let s = ws.create_request(&ws.collections(), "s").unwrap();
        let r = h.state().open.as_ref().unwrap().path.clone();
        h.state_mut().reload();
        // Pinned: a preview tab would be reused for `s` rather than parked.
        h.state_mut().activate(r.clone(), true);
        for (size, kept) in [(MAX_PARKED_RESPONSE + 1, false), (100, true)] {
            show_response(&mut h, "text/plain", "x".repeat(size));
            h.state_mut().activate(s.clone(), true);
            h.state_mut().activate(r.clone(), true);
            h.run();
            assert_eq!(h.state().response.is_some(), kept, "{size} bytes");
            assert_eq!(h.state().status.contains("wasn't kept"), !kept);
            h.state_mut().status.clear();
        }
    }

    /// Not a check: prints what a big response and a chatty stream cost in RAM, for
    /// comparing builds. Run alone so other tests don't share the process:
    /// `cargo test --release memory_footprint -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn memory_footprint() {
        use std::io::{Read as _, Write as _};
        let rss = || memory_stats::memory_stats().unwrap().physical_mem >> 20;
        // Generated while it's written, so the server holds no copy in this process.
        let serve =
            |head: &'static str,
             (first, chunk, n, tail): (&'static str, String, usize, &'static str)| {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let addr = listener.local_addr().unwrap();
                std::thread::spawn(move || {
                    let (mut s, _) = listener.accept().unwrap();
                    let _ = s.read(&mut [0; 8192]);
                    let len = first.len() + chunk.len() * n + tail.len();
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\n{head}content-length: {len}\r\nconnection: close\r\n\r\n{first}"
                    );
                    for _ in 0..n {
                        if s.write_all(chunk.as_bytes()).is_err() {
                            return;
                        }
                    }
                    let _ = s.write_all(tail.as_bytes());
                });
                format!("http://{addr}")
            };
        let settle = |h: &mut Harness<'_, App>, done: &dyn Fn(&App) -> bool| {
            let mut peak = rss();
            for _ in 0..3000 {
                h.step();
                peak = peak.max(rss());
                if done(h.state()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            for _ in 0..5 {
                h.step();
            }
            peak
        };

        let mut h = with_request("memory");
        println!("idle: {} MiB", rss());

        // JSON as "[" + objects + "{}]": 15 MB is shown pretty, 100 MB is cut at 16 MiB.
        let object = r#"{"id":12345,"name":"abcdefghijklmnopqrstuvwxyz","ok":true},"#;
        let chunk = object.repeat(64 * 1024 / object.len());
        for mb in [15, 100] {
            let n = mb * 1024 * 1024 / chunk.len();
            let body = ("[", chunk.clone(), n, "{}]");
            h.state_mut().open.as_mut().unwrap().draft.url =
                serve("content-type: application/json\r\n", body);
            h.get_by_label("Send").click();
            let peak = settle(&mut h, &|app| {
                app.pending.is_empty() && app.response.is_some()
            });
            println!("{mb} MB JSON response: peak {peak} MiB, then {} MiB", rss());
            h.state_mut().response = None;
            h.step();
        }

        // SSE: 20 000 events of 50 KB, 1 GB in all.
        let event = format!("data: {}\n\n", "x".repeat(50 * 1024));
        let url = serve(
            "content-type: text/event-stream\r\n",
            ("", event, 20_000, ""),
        );
        {
            let d = &mut h.state_mut().open.as_mut().unwrap().draft;
            (d.method, d.url) = ("SSE".into(), url);
        }
        h.step();
        h.get_by_label("Connect").click();
        let peak = settle(&mut h, &|app| app.stream.as_ref().is_some_and(|s| !s.live));
        let kept = h.state().stream.as_ref().unwrap().events().len();
        println!(
            "1 GB of SSE events: peak {peak} MiB, then {} MiB ({kept} events kept)",
            rss()
        );

        // Load test: 50 VUs for 3 s against a 4 MB body, each answer written as it goes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let chunk = [b'x'; 64 * 1024];
                    while s.read(&mut [0; 8192]).is_ok_and(|n| n > 0) {
                        let head = "HTTP/1.1 200 OK\r\ncontent-length: 4194304\r\n\r\n";
                        if s.write_all(head.as_bytes()).is_err() {
                            return;
                        }
                        for _ in 0..64 {
                            if s.write_all(&chunk).is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        let stats = Arc::new(std::sync::Mutex::new(crate::loadtest::Stats::default()));
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let rt = &h.state().rt;
        let client = rt.block_on(crate::net::build_client(net)).unwrap();
        let req = Request {
            url: format!("http://{addr}/"),
            ..Default::default()
        };
        let run = crate::loadtest::run(client, req, 50, Duration::from_secs(3), stats.clone());
        let task = rt.spawn(run);
        let mut peak = rss();
        while !task.is_finished() {
            peak = peak.max(rss());
            std::thread::sleep(Duration::from_millis(10));
        }
        let count = stats.lock().unwrap().count;
        println!(
            "load test, 50 VUs × 4 MB: peak {peak} MiB, then {} MiB ({count} requests)",
            rss()
        );

        // A 10 MB JSON request body (pasted, imported) shown in the body editor.
        {
            let d = &mut h.state_mut().open.as_mut().unwrap().draft;
            d.method = "POST".into();
            d.body = Body::Json {
                text: object.repeat(10 * 1024 * 1024 / object.len()),
            };
        }
        h.state_mut().req_tab = ReqTab::Body;
        let mut peak = rss();
        for _ in 0..10 {
            h.step();
            peak = peak.max(rss());
        }
        println!(
            "10 MB JSON body in the editor: peak {peak} MiB, then {} MiB",
            rss()
        );

        // The same 10 MB pasted into an empty body editor.
        let pasted = match &mut h.state_mut().open.as_mut().unwrap().draft.body {
            Body::Json { text } => std::mem::take(text),
            _ => unreachable!(),
        };
        h.step();
        h.ctx.memory_mut(|m| m.request_focus(egui::Id::new("body")));
        h.step();
        h.event(egui::Event::Paste(pasted));
        let mut peak = rss();
        for _ in 0..10 {
            h.step();
            peak = peak.max(rss());
        }
        println!(
            "10 MB pasted into the body editor: peak {peak} MiB, then {} MiB",
            rss()
        );

        // A 12-megapixel photo as a JPEG response, previewed.
        let photo = image::RgbImage::from_fn(4000, 3000, |x, y| {
            image::Rgb([(x ^ y) as u8, (x * y) as u8, (x + y) as u8])
        });
        let mut jpeg = Vec::new();
        (photo.write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        ))
        .unwrap();
        drop(photo);
        let size = jpeg.len();
        h.state_mut().response = None;
        h.state_mut().resp_tab = RespTab::Body;
        // The SSE run above left a stream view in place of the response.
        h.state_mut().open.as_mut().unwrap().draft.method = "GET".into();
        for _ in 0..5 {
            h.step();
        }
        let before = rss();
        h.state_mut().response = Some(Shown {
            result: Ok(into_view(http::Response {
                status: 200,
                reason: "OK".into(),
                version: "HTTP/1.1".into(),
                elapsed: Duration::ZERO,
                headers: vec![("content-type".into(), "image/jpeg".into())],
                body: String::new(),
                bytes: Some(jpeg),
                truncated: false,
                sent: Default::default(),
            })),
            tests: Vec::new(),
            logs: Vec::new(),
            past: None,
        });
        let peak = settle(&mut h, &|_| true);
        let shown = h
            .state()
            .response
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        assert!(
            matches!(shown.image, Some(Ok((_, [4000, 3000])))),
            "decoded"
        );
        println!(
            "{:.1} MB 4000 × 3000 JPEG previewed: from {before} MiB, peak {peak} MiB, then {} MiB",
            mb(size),
            rss()
        );

        // A script that kept a 50 MB response in a variable: what does each frame cost?
        h.state_mut().response = None;
        let frames = |h: &mut Harness<'_, App>| {
            let (start, mut peak) = (Instant::now(), rss());
            for _ in 0..30 {
                h.step();
                peak = peak.max(rss());
            }
            (start.elapsed() / 30, peak)
        };
        let (without, _) = frames(&mut h);
        h.state_mut()
            .vars
            .insert("big".into(), "x".repeat(50 << 20));
        let before = rss();
        let (with, peak) = frames(&mut h);
        println!(
            "50 MB variable: {:?} per frame (was {without:?}), from {before} MiB, peak {peak} MiB",
            with
        );
    }

    /// Without settling: a live stream's spinner keeps repainting, which `run` rejects.
    fn wait_live(h: &mut Harness<'_, App>, done: impl Fn(&App) -> bool) {
        for _ in 0..200 {
            h.step();
            if done(h.state()) {
                // Sessions write their events from another thread: what's awaited may
                // have arrived after this frame was drawn (e.g. in the frame that
                // connected, before the panel showed Send), so draw one that sees it.
                h.step();
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for the app");
    }
}
