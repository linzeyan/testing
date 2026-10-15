//! The connections the OS lists, as Sniffnet shows them, read with the OS's own tools
//! rather than captured packets: capturing needs a driver installed (Npcap on Windows),
//! which the VDI doesn't allow. So there are no byte counts.

use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Sorted by process, then its sockets.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conn {
    pub process: String,
    pub pid: u32,
    /// TCP or UDP.
    pub proto: String,
    pub local: String,
    /// Empty when there's none: a listener, most UDP sockets.
    pub remote: String,
    /// As the OS words it; empty for UDP.
    pub state: String,
}

/// Every TCP and UDP socket the OS lets this user see: on macOS and Linux, without root,
/// only the user's own processes'. Blocks while the tools run.
pub fn list() -> Result<Vec<Conn>, String> {
    #[cfg(unix)]
    let mut list = lsof(&run("lsof", &["-nP", "-i", "-F", "pcPnT", "+c", "0"])?);
    #[cfg(windows)]
    let mut list = {
        let names = tasklist(&run("tasklist", &["/fo", "csv", "/nh"])?);
        netstat(&run("netstat", &["-ano"])?, &names)
    };
    // lsof lists a socket once per descriptor, and a listener on both IPv4 and IPv6 as
    // the same `*:port`.
    list.sort();
    list.dedup();
    Ok(list)
}

/// Listed again this often while shown: lsof takes some 60 ms here.
const EVERY: Duration = Duration::from_secs(2);

/// The listing kept fresh while it's shown, each run off the UI thread.
#[derive(Default)]
pub struct Table {
    /// None before the first listing is in.
    pub list: Option<Result<Vec<Conn>, String>>,
    coming: Option<mpsc::Receiver<Result<Vec<Conn>, String>>>,
    /// When the last listing started.
    at: Option<Instant>,
}

impl Table {
    /// Takes in a listing that's ready and starts the next, which waits until it's due;
    /// `ready` runs as it's in, to redraw. The wait is the thread's, not a timed redraw:
    /// no frame is drawn just to see whether it's time.
    pub fn refresh(&mut self, ready: impl FnOnce() + Send + 'static) {
        if let Some(coming) = &self.coming {
            match coming.try_recv() {
                Ok(list) => (self.list, self.coming) = (Some(list), None),
                Err(mpsc::TryRecvError::Disconnected) => self.coming = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.coming.is_none() {
            let wait = self
                .at
                .map_or(Duration::ZERO, |at| EVERY.saturating_sub(at.elapsed()));
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                std::thread::sleep(wait);
                let _ = tx.send(list());
                ready();
            });
            (self.coming, self.at) = (Some(rx), Some(Instant::now() + wait));
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashing up over the app.
        command.creation_flags(0x0800_0000);
    }
    let out = command.output().map_err(|e| format!("{program}: {e}"))?;
    // lsof exits 1 when it couldn't read some socket, having listed the rest.
    let error = String::from_utf8_lossy(&out.stderr);
    if out.stdout.is_empty() && !error.trim().is_empty() {
        return Err(format!("{program}: {}", error.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `lsof -F pcPnT`: a process's lines (p, c), then each of its sockets' (f, P, n, T).
#[cfg(any(unix, test))]
fn lsof(out: &str) -> Vec<Conn> {
    let (mut list, mut pid, mut process) = (Vec::<Conn>::new(), 0, String::new());
    for line in out.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match (tag, list.last_mut()) {
            ('p', _) => pid = value.parse().unwrap_or_default(),
            ('c', _) => process = value.to_owned(),
            ('f', _) => list.push(Conn {
                process: process.clone(),
                pid,
                ..Default::default()
            }),
            ('P', Some(conn)) => conn.proto = value.to_owned(),
            ('n', Some(conn)) => {
                let (local, remote) = value.split_once("->").unwrap_or((value, ""));
                (conn.local, conn.remote) = (local.to_owned(), remote.to_owned());
            }
            ('T', Some(conn)) => {
                if let Some(state) = value.strip_prefix("ST=") {
                    conn.state = state.to_owned();
                }
            }
            _ => {}
        }
    }
    list
}

/// `netstat -ano`'s rows, each process named from `tasklist`.
#[cfg(any(windows, test))]
fn netstat(out: &str, names: &std::collections::HashMap<u32, String>) -> Vec<Conn> {
    let row = |line: &str| {
        let cols: Vec<_> = line.split_whitespace().collect();
        let (proto, local, remote, state, pid) = match cols[..] {
            [proto @ "TCP", local, remote, state, pid] => (proto, local, remote, state, pid),
            [proto @ "UDP", local, remote, pid] => (proto, local, remote, "", pid),
            _ => return None,
        };
        let pid = pid.parse().ok()?;
        let remote = match remote {
            "*:*" | "0.0.0.0:0" | "[::]:0" => "",
            remote => remote,
        };
        Some(Conn {
            process: names.get(&pid).cloned().unwrap_or_default(),
            pid,
            proto: proto.to_owned(),
            local: local.to_owned(),
            remote: remote.to_owned(),
            state: state.to_owned(),
        })
    };
    out.lines().filter_map(row).collect()
}

/// `tasklist /fo csv /nh`: "name","pid",… a process a line.
#[cfg(any(windows, test))]
fn tasklist(out: &str) -> std::collections::HashMap<u32, String> {
    let mut rows = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(out.as_bytes());
    let named = |row: csv::StringRecord| Some((row.get(1)?.parse().ok()?, row.get(0)?.to_owned()));
    rows.records().flatten().filter_map(named).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(process: &str, pid: u32, proto: &str, ends: [&str; 2], state: &str) -> Conn {
        Conn {
            process: process.into(),
            pid,
            proto: proto.into(),
            local: ends[0].into(),
            remote: ends[1].into(),
            state: state.into(),
        }
    }

    /// lsof's fields: each socket belongs to the process listed before it; a listener has
    /// no remote end and UDP no state.
    #[test]
    fn lsof_fields_read_as_each_process_sockets() {
        let out = "p653\ncrapportd\nf12\nPTCP\nn*:63127\nTST=LISTEN\nTQR=0\nTQS=0\n\
                   f23\nPTCP\nn[fe80::1]:63127->[fe80::2]:56757\nTST=ESTABLISHED\n\
                   p666\ncGoogle Chrome Helper\nf24\nPUDP\nn*:3722\n";
        assert_eq!(
            lsof(out),
            [
                conn("rapportd", 653, "TCP", ["*:63127", ""], "LISTEN"),
                conn(
                    "rapportd",
                    653,
                    "TCP",
                    ["[fe80::1]:63127", "[fe80::2]:56757"],
                    "ESTABLISHED"
                ),
                conn("Google Chrome Helper", 666, "UDP", ["*:3722", ""], ""),
            ]
        );
    }

    /// netstat's rows: TCP with a state, UDP without; a remote end of all zeros is none;
    /// the header and blank lines aren't rows. Each process is named from tasklist.
    #[test]
    fn netstat_rows_read_with_their_process_names() {
        let tasks = "\"System Idle Process\",\"0\",\"Services\",\"0\",\"8 K\"\r\n\
                     \"chrome.exe\",\"4242\",\"Console\",\"1\",\"123,456 K\"\r\n";
        let out = "\r\nActive Connections\r\n\r\n  \
            Proto  Local Address          Foreign Address        State           PID\r\n  \
            TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1012\r\n  \
            TCP    10.0.0.5:50123         140.82.112.3:443       ESTABLISHED     4242\r\n  \
            UDP    [::]:5353              *:*                                    4242\r\n";
        assert_eq!(
            netstat(out, &tasklist(tasks)),
            [
                conn("", 1012, "TCP", ["0.0.0.0:135", ""], "LISTENING"),
                conn(
                    "chrome.exe",
                    4242,
                    "TCP",
                    ["10.0.0.5:50123", "140.82.112.3:443"],
                    "ESTABLISHED"
                ),
                conn("chrome.exe", 4242, "UDP", ["[::]:5353", ""], ""),
            ]
        );
    }

    /// The tools as this OS has them: a listener and a connection this process opens are
    /// listed as its own. Not on Linux, whose CI image may lack lsof.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn this_process_sockets_are_listed_as_its_own() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        // The same socket twice, as lsof lists it: once.
        let _twin = listener.try_clone().unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let _client = std::net::TcpStream::connect(&addr).unwrap();
        let list = list().unwrap();
        let ours: Vec<_> = (list.iter())
            .filter(|c| c.pid == std::process::id())
            .collect();
        let listening = |c: &&&Conn| c.local == addr && c.remote.is_empty();
        let listeners: Vec<_> = ours.iter().filter(listening).collect();
        assert_eq!(listeners.len(), 1, "{ours:?}");
        assert!(listeners[0].state.starts_with("LISTEN"), "{ours:?}");
        let connected = |c: &&Conn| c.remote == addr && c.state == "ESTABLISHED";
        assert!(ours.iter().any(connected), "{ours:?}");
        assert!(
            ours.iter().all(|c| c.process.starts_with("apitool")),
            "{ours:?}"
        );
        assert!(
            ours.iter().all(|c| c.proto == "TCP" || c.proto == "UDP"),
            "{ours:?}"
        );
    }

    /// While shown the table is listed again every couple of seconds, so a socket opened
    /// after the first listing shows up unasked.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_socket_opened_later_shows_up() {
        let mut table = Table::default();
        let wait = |table: &mut Table, until: &dyn Fn(&Table) -> bool| {
            for _ in 0..1500 {
                table.refresh(|| {});
                if until(table) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("never listed: {:?}", table.list);
        };
        wait(&mut table, &|t| t.list.is_some());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let has = |t: &Table| matches!(&t.list, Some(Ok(l)) if l.iter().any(|c| c.local == addr));
        assert!(!has(&table));
        wait(&mut table, &has);
    }
}
