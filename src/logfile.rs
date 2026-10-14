//! The GUI's log, so a problem on a machine nobody is watching (the VDI) leaves a trace:
//! one file rotated at 1 MB, the previous one kept as `.1`. Requests are logged by method
//! and URL with the query cut off; headers, bodies and tokens never are. Each line says
//! which source line wrote it.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX: u64 = 1 << 20;

/// How much apitool writes (Settings > General). Debug and Trace also let the network
/// libraries write theirs at Debug: their connection, TLS and HTTP/2 steps are what finds
/// a proxy or certificate problem. Never theirs at Trace, which dumps raw frames.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl Level {
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    fn filter(self) -> log::LevelFilter {
        match self {
            Level::Error => log::LevelFilter::Error,
            Level::Warn => log::LevelFilter::Warn,
            Level::Info => log::LevelFilter::Info,
            Level::Debug => log::LevelFilter::Debug,
            Level::Trace => log::LevelFilter::Trace,
        }
    }
}

/// apitool's own level, a `log::LevelFilter` as a number.
static OURS: AtomicUsize = AtomicUsize::new(log::LevelFilter::Info as usize);
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Where the network's steps happen; what else the libraries say (wgpu, eframe) stays at
/// warnings, as it comes every frame.
const NETWORK: &[&str] = &["reqwest", "hyper", "hyper_util", "rustls", "h2"];

/// Takes effect at once, for every thread.
pub fn set_level(level: Level) {
    OURS.store(level.filter() as usize, Ordering::Relaxed);
    log::set_max_level(level.filter());
}

fn ours() -> log::LevelFilter {
    let n = OURS.load(Ordering::Relaxed);
    log::LevelFilter::iter()
        .nth(n)
        .unwrap_or(log::LevelFilter::Info)
}

/// The log file, once `init` was given one.
pub fn path() -> Option<&'static PathBuf> {
    PATH.get()
}

/// Whether a record at `level` from `target` goes in, at apitool's level `ours`.
fn wanted(target: &str, level: log::Level, ours: log::LevelFilter) -> bool {
    if target.starts_with("apitool") {
        return level <= ours;
    }
    let crate_name = target.split("::").next().unwrap_or(target);
    let network = NETWORK.contains(&crate_name) && ours >= log::LevelFilter::Debug;
    level
        <= match network {
            true => log::Level::Debug,
            false => log::Level::Warn,
        }
}

/// `src/http.rs:42` for apitool; a library's path after its `src/`, as the registry's
/// absolute one says nothing more.
fn place(r: &log::Record) -> String {
    let (Some(file), Some(line)) = (r.file(), r.line()) else {
        return String::new();
    };
    let file = file.replace('\\', "/");
    let file = match file.rfind("/src/") {
        Some(at) if !file.starts_with("src/") => &file[at + 5..],
        _ => &file[..],
    };
    format!(" {file}:{line}")
}

struct Sink {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max: u64,
}

impl Sink {
    fn new(path: PathBuf, max: u64) -> Self {
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        Self {
            path,
            file: None,
            size,
            max,
        }
    }

    fn write(&mut self, line: &str) {
        let len = line.len() as u64;
        if self.size + len > self.max {
            // Closed first: Windows won't rename a file this process holds open.
            self.file = None;
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
            self.size = 0;
        }
        if self.file.is_none() {
            let open = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path);
            self.file = open.ok();
        }
        if let Some(f) = &mut self.file
            && f.write_all(line.as_bytes()).is_ok()
        {
            self.size += len;
        }
    }
}

struct Logger(Option<Mutex<Sink>>);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        wanted(m.target(), m.level(), ours())
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        // What used to be eprintln!: still on stderr, for the CLI and a terminal.
        if r.level() <= log::Level::Warn && r.target().starts_with("apitool") {
            eprintln!("{}", r.args());
        }
        let Some(sink) = &self.0 else { return };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let at = crate::model::iso8601(now);
        let text = r.args().to_string();
        // apitool cuts queries where it logs a URL; a library doesn't know to.
        let text = match r.target().starts_with("apitool") {
            true => Cow::Borrowed(&text[..]),
            false => redact(&text),
        };
        let (level, target, place) = (r.level(), r.target(), place(r));
        let line = format!("{at} {level:<5} {target}{place}: {text}\n");
        sink.lock().unwrap_or_else(|e| e.into_inner()).write(&line);
    }

    fn flush(&self) {}
}

/// `file`: where the log goes; None for stderr only (the CLI). Panics are logged with a
/// backtrace when there is a file. Only the first call counts.
pub fn init(file: Option<PathBuf>) {
    let panics = file.is_some();
    if let Some(dir) = file.as_ref().and_then(|f| f.parent()) {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Some(file) = &file {
        let _ = PATH.set(file.clone());
    }
    let sink = file.map(|path| Mutex::new(Sink::new(path, MAX)));
    if log::set_boxed_logger(Box::new(Logger(sink))).is_err() {
        return;
    }
    log::set_max_level(ours());
    if panics {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let trace = std::backtrace::Backtrace::force_capture();
            log::error!("{info}\n{trace}");
            default(info);
        }));
    }
}

/// URL queries carry API keys and tokens as often as headers do.
pub fn redact(text: &str) -> Cow<'_, str> {
    static QUERY: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"\?[^\s)\]"']+"#).unwrap());
    QUERY.replace_all(text, "?…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log may never grow past twice its cap, and the latest lines are in the file.
    #[test]
    fn the_log_rotates_at_its_cap() {
        let dir = std::env::temp_dir().join(format!("apitool-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("apitool.log");
        let mut sink = Sink::new(path.clone(), 25);
        for line in ["one 678901\n", "two 678901\n", "three 8901\n"] {
            sink.write(line);
        }
        sink.file = None;
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(path.clone()), "three 8901\n");
        assert_eq!(
            read(path.with_extension("log.1")),
            "one 678901\ntwo 678901\n"
        );
        // Opened again, it counts what is already there.
        assert_eq!(Sink::new(path, 25).size, 11);
    }

    /// Debug opens the network libraries' steps, which find proxy and TLS problems, but
    /// never their Trace (raw frames) nor the chatter of the rest (wgpu, every frame).
    #[test]
    fn the_level_lets_in_apitool_and_the_network_libraries_only() {
        use log::Level::*;
        use log::LevelFilter as F;
        assert!(wanted("apitool::http", Info, F::Info));
        assert!(!wanted("apitool::http", Debug, F::Info));
        assert!(wanted("apitool::http", Trace, F::Trace));
        assert!(!wanted("apitool", Warn, F::Error));
        assert!(!wanted("hyper_util::client", Debug, F::Info));
        assert!(wanted("hyper_util::client", Debug, F::Debug));
        assert!(!wanted("rustls::client", Trace, F::Trace));
        assert!(!wanted("wgpu_core::device", Debug, F::Trace));
        assert!(wanted("wgpu_core::device", Warn, F::Info));
    }

    #[test]
    fn a_line_says_which_source_line_wrote_it() {
        let ours = log::Record::builder()
            .file(Some("src/http.rs"))
            .line(Some(42))
            .build();
        assert_eq!(place(&ours), " src/http.rs:42");
        let theirs = log::Record::builder()
            .file(Some(
                "/Users/x/.cargo/registry/src/index/hyper-util-0.1.17/src/client/legacy/connect/http.rs",
            ))
            .line(Some(7))
            .build();
        assert_eq!(place(&theirs), " client/legacy/connect/http.rs:7");
    }

    #[test]
    fn queries_are_left_out() {
        let e = "error sending request for url (https://x.test/a?token=s3cret&b=1)";
        assert_eq!(
            redact(e),
            "error sending request for url (https://x.test/a?…)"
        );
        assert_eq!(redact("{{host}}/x?key={{key}} sent"), "{{host}}/x?… sent");
    }
}
