//! The GUI's log, so a problem on a machine nobody is watching (the VDI) leaves a trace:
//! one file rotated at 1 MB, the previous one kept as `.1`. Requests are logged by method
//! and URL with the query cut off; headers, bodies and tokens never are.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX: u64 = 1 << 20;

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
        // The libraries (wgpu, eframe, reqwest) talk a lot at Info; their warnings matter.
        let ours = m.target().starts_with("apitool");
        m.level()
            <= if ours {
                log::Level::Info
            } else {
                log::Level::Warn
            }
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
        let line = format!("{at} {:<5} {}: {}\n", r.level(), r.target(), r.args());
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
    let sink = file.map(|path| Mutex::new(Sink::new(path, MAX)));
    if log::set_boxed_logger(Box::new(Logger(sink))).is_err() {
        return;
    }
    log::set_max_level(log::LevelFilter::Info);
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
