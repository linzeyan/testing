//! Headless collection runner for CI and scheduled checks, like Postman's newman.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::runner::{self, RunPlan, Vars};
use crate::{net, store};

const USAGE: &str = "usage: apitool-cli <folder|request> [options]
       apitool-cli mcp [--workspace <dir>]
       apitool-cli docs [folder] [--workspace <dir>] [-o <file.md>]
       apitool-cli mock [folder] [--workspace <dir>] [--port <n>]

Runs every request under the folder or request, named as in the tree (`users`,
`users/get user`; `.` is the whole collection), and exits with 1 if any request or
test fails.

`mcp` serves the workspace to an LLM client (Model Context Protocol over stdio).
`docs` writes Markdown API docs (default: the whole collection, to stdout).
`mock` answers HTTP calls with the saved examples (default port 3000, localhost only).

options:
  --workspace <dir>      workspace (default: $APITOOL_WORKSPACE, else workspace/ next to the exe)
  -e, --env <name>       environment to use
  -d, --data <file>      CSV or JSON data file; one iteration per row
  -n, --iterations <n>   iterations without a data file (default 1)
  --delay <ms>           pause between requests
  --junit <file>         also write a JUnit XML report (for CI test dashboards)
";

/// Exit code: 0 all passed, 1 something failed, 2 could not run.
pub fn main() -> i32 {
    match run(std::env::args().skip(1).collect()) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

fn run(args: Vec<String>) -> Result<bool, String> {
    if args.first().map(String::as_str) == Some("mcp") {
        let workspace = match &args[1..] {
            [] => None,
            [flag, dir] if flag == "--workspace" => Some(PathBuf::from(dir)),
            _ => return Err("usage: apitool-cli mcp [--workspace <dir>]".into()),
        };
        // stdout carries only JSON-RPC; diagnostics go to stderr via `main`.
        let ws = store::open_workspace(workspace)?;
        crate::mcp::serve(ws, std::io::stdin().lock(), std::io::stdout().lock())?;
        return Ok(true);
    }
    match args.first().map(String::as_str) {
        Some("docs") => return docs(&args[1..]),
        Some("mock") => return mock(&args[1..]),
        _ => {}
    }
    let (mut target, mut workspace, mut env, mut data) = (None, None, None, None);
    let (mut iterations, mut delay, mut junit) = (1usize, 0u64, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(true);
            }
            "--workspace" => workspace = Some(PathBuf::from(value()?)),
            "-e" | "--env" => env = Some(value()?),
            "-d" | "--data" => data = Some(PathBuf::from(value()?)),
            "--junit" => junit = Some(PathBuf::from(value()?)),
            "-n" | "--iterations" => {
                iterations = value()?
                    .parse()
                    .map_err(|_| format!("{arg} needs a number"))?
            }
            "--delay" => {
                delay = value()?
                    .parse()
                    .map_err(|_| format!("{arg} needs a number"))?
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            _ if target.is_none() => target = Some(PathBuf::from(&arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let target = target.ok_or_else(|| USAGE.to_owned())?;
    let data = data.as_deref().map(absolute).transpose()?;
    let junit = junit.as_deref().map(absolute).transpose()?;

    let ws = store::open_workspace(workspace)?;
    let requests = ws.load_requests_in(&scope(&ws, &target))?;
    let env = match env {
        Some(name) if !ws.env_names().contains(&name) => {
            return Err(format!("unknown environment \"{name}\""));
        }
        Some(name) => ws.env_vars(Some(&name))?,
        None => HashMap::new(),
    };
    let globals = ws.env_vars(None)?;
    let data = match data {
        Some(p) => runner::load_data(&p)?,
        None => Vec::new(),
    };

    // Multi-thread: scripts run under block_in_place, which a current-thread runtime forbids.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let client = rt.block_on(net::build_client(ws.load_state().network))?;
    let plan = RunPlan {
        requests,
        data,
        iterations,
        delay: Duration::from_millis(delay),
    };
    let vars = Vars {
        env,
        globals,
        data: HashMap::new(),
    };
    let (mut total, mut failed, mut tests, mut tests_failed) = (0, 0, 0, 0);
    let mut suites = String::new();
    rt.block_on(runner::run_collection(client, plan, vars, |item| {
        suites.push_str(&junit_suite(&item));
        total += 1;
        failed += item.failed() as usize;
        let mark = if item.failed() { "FAIL" } else { "ok  " };
        let status = match &item.status {
            Ok((code, ms)) => format!("{code} {ms} ms"),
            Err(e) => e.lines().next().unwrap_or_default().to_owned(),
        };
        println!(
            "{mark} #{} {} {} — {status}",
            item.iteration + 1,
            item.method,
            item.name
        );
        for t in &item.tests {
            tests += 1;
            if t.passed {
                println!("       ✓ {}", t.name);
            } else {
                tests_failed += 1;
                let why = t.error.as_deref().unwrap_or_default();
                println!("       ✗ {} — {why}", t.name);
            }
        }
    }));
    println!("\n{total} requests, {failed} failed; {tests} tests, {tests_failed} failed");
    if let Some(path) = junit {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"apitool\">\n{suites}</testsuites>\n"
        );
        std::fs::write(&path, xml).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(failed == 0)
}

/// A request ("users/get user", ".toml" or not) or else a folder of the collection.
fn scope(ws: &store::Workspace, given: &Path) -> PathBuf {
    let request = ws.request_path(&given.to_string_lossy()).ok();
    let request = request.filter(|p| ws.exists(p));
    request.unwrap_or_else(|| ws.collections().join(given))
}

/// `[collection|folder]` plus `--option value` pairs, for `docs` and `mock`.
fn folder_args(
    args: &[String],
    options: &[&str],
) -> Result<(Option<PathBuf>, HashMap<String, String>), String> {
    let (mut target, mut values) = (None, HashMap::new());
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if options.contains(&arg.as_str()) {
            let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
            values.insert(arg.clone(), value.clone());
        } else if arg.starts_with('-') {
            return Err(format!("unknown option {arg}\n\n{USAGE}"));
        } else if target.is_none() {
            target = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected argument {arg}"));
        }
    }
    Ok((target, values))
}

/// The workspace and the folder named on the command line (default: the whole
/// collection). Make other relative paths absolute first: this changes directory.
fn open_folder(
    target: Option<PathBuf>,
    values: &HashMap<String, String>,
) -> Result<(store::Workspace, PathBuf), String> {
    let ws = store::open_workspace(values.get("--workspace").map(PathBuf::from))?;
    let dir = match target {
        Some(given) => scope(&ws, &given),
        None => ws.collections(),
    };
    Ok((ws, dir))
}

/// User paths are relative to where the command ran; resolve them before
/// `open_workspace` changes the working directory.
fn absolute(p: &Path) -> Result<PathBuf, String> {
    std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn docs(args: &[String]) -> Result<bool, String> {
    let (target, values) = folder_args(args, &["--workspace", "-o"])?;
    let output = values.get("-o").map(|p| absolute(p.as_ref())).transpose()?;
    let (ws, dir) = open_folder(target, &values)?;
    let md = crate::docs::markdown(&ws, &dir)?;
    match output {
        Some(path) => {
            std::fs::write(&path, md).map_err(|e| format!("write {}: {e}", path.display()))?
        }
        None => print!("{md}"),
    }
    Ok(true)
}

fn mock(args: &[String]) -> Result<bool, String> {
    let (target, values) = folder_args(args, &["--workspace", "--port"])?;
    let port: u16 = match values.get("--port") {
        Some(p) => p.parse().map_err(|_| "--port needs a number".to_owned())?,
        None => 3000,
    };
    let (ws, dir) = open_folder(target, &values)?;
    let requests = ws.load_requests_in(&dir)?;
    let mocked = requests
        .iter()
        .filter(|(_, r)| !r.examples.is_empty())
        .count();
    if mocked == 0 {
        return Err("no saved examples to serve: send a request, then \"Save as example\"".into());
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| format!("port {port}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        println!("Mocking {mocked} requests at http://{addr} (Ctrl+C stops)");
        crate::mock::serve(ws, dir, listener, |line| println!("{line}")).await;
        Ok(true)
    })
}

/// One `<testsuite>` per request run, one `<testcase>` per `pm.test`, like newman's
/// JUnit reporter. A request that got no response is a single erroring testcase.
fn junit_suite(item: &runner::RunItem) -> String {
    let name = esc(&format!("{} #{}", item.name, item.iteration + 1));
    let (time, cases) = match &item.status {
        Err(e) => (
            0.0,
            format!(
                "    <testcase name=\"{name}\" classname=\"{name}\"><error message=\"{}\"/></testcase>\n",
                esc(e)
            ),
        ),
        Ok((_, ms)) => {
            let cases: String = item
                .tests
                .iter()
                .map(|t| {
                    let failure = if t.passed {
                        String::new()
                    } else {
                        let why = t.error.as_deref().unwrap_or("failed");
                        format!("<failure message=\"{}\"/>", esc(why))
                    };
                    format!(
                        "    <testcase name=\"{}\" classname=\"{name}\">{failure}</testcase>\n",
                        esc(&t.name)
                    )
                })
                .collect();
            (*ms as f64 / 1000.0, cases)
        }
    };
    let count = item.tests.len().max(item.status.is_err() as usize);
    let failures = item.tests.iter().filter(|t| !t.passed).count();
    let errors = item.status.is_err() as usize;
    format!(
        "  <testsuite name=\"{name}\" tests=\"{count}\" failures=\"{failures}\" errors=\"{errors}\" time=\"{time:.3}\">\n{cases}  </testsuite>\n"
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_reflects_test_results() {
        let ws = std::env::temp_dir().join(format!("apitool-cli-{}", std::process::id()));
        let dir = ws.join("collections/smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let url = crate::http::tests::echo_server();
        let request = |status: u16| crate::model::Request {
            url: url.clone(),
            tests: format!(
                "pm.test('status', function () {{ pm.response.to.have.status({status}); }});"
            ),
            ..Default::default()
        };
        // A CI checkout has the exported files and no database yet.
        let text = toml::to_string(&request(200)).unwrap();
        std::fs::write(dir.join("a.toml"), text).unwrap();
        let args = |target: &str| {
            vec![
                target.to_owned(),
                "--workspace".into(),
                ws.display().to_string(),
            ]
        };
        // A CI job relies on this: green only when every test passed.
        assert_eq!(run(args("smoke")), Ok(true));
        let workspace = store::Workspace::open(ws.clone()).unwrap();
        let b = workspace.request_path("smoke/b").unwrap();
        workspace.save_request(&b, &request(404)).unwrap();
        assert_eq!(
            run(args("smoke/b")),
            Ok(false),
            "a single request by its tree name"
        );
        let report = ws.join("report.xml");
        let mut with_junit = args("smoke");
        with_junit.extend(["--junit".into(), report.display().to_string()]);
        assert_eq!(run(with_junit), Ok(false));
        // CI dashboards count these attributes and show the failure message.
        let xml = std::fs::read_to_string(&report).unwrap();
        assert_eq!(xml.matches("<testcase ").count(), 2, "{xml}");
        assert_eq!(xml.matches("<failure message=").count(), 1, "{xml}");
        assert!(xml.contains("name=\"smoke/b #1\""), "{xml}");
        assert!(run(args("missing")).unwrap_err().contains("no requests"));
    }
}
