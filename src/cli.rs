//! Headless collection runner for CI and scheduled checks, like Postman's newman.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::runner::{self, RunPlan, Vars};
use crate::{net, store};

const USAGE: &str = "usage: apitool-cli <collection|folder|request.toml> [options]
       apitool-cli mcp [--workspace <dir>]
       apitool-cli docs [collection|folder] [--workspace <dir>] [-o <file.md>]

Runs every request under the path (relative to the current directory, or to the
workspace's collections/ folder) and exits with 1 if any request or test fails.

`mcp` serves the workspace to an LLM client (Model Context Protocol over stdio).
`docs` writes Markdown API docs (default: the whole collection, to stdout).

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
    if args.first().map(String::as_str) == Some("docs") {
        return docs(&args[1..]);
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
    // User paths are relative to where the command ran, so resolve them before
    // open_workspace changes the working directory.
    let absolute =
        |p: &PathBuf| std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()));
    let (target_abs, data) = (absolute(&target)?, data.as_ref().map(absolute).transpose()?);
    let junit = junit.as_ref().map(absolute).transpose()?;

    let ws = store::open_workspace(workspace)?;
    let requests = ws.load_requests_in(&scope(&ws, target_abs, &target))?;
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

/// A path as given on the command line, or else inside the collections folder.
fn scope(ws: &store::Workspace, absolute: PathBuf, given: &std::path::Path) -> PathBuf {
    if absolute.exists() {
        absolute
    } else {
        ws.collections().join(given)
    }
}

fn docs(args: &[String]) -> Result<bool, String> {
    let (mut target, mut workspace, mut output) = (None, None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--workspace" => workspace = Some(PathBuf::from(value()?)),
            "-o" | "--output" => output = Some(PathBuf::from(value()?)),
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            _ if target.is_none() => target = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let absolute =
        |p: &PathBuf| std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()));
    let output = output.as_ref().map(absolute).transpose()?;
    let target_abs = target.as_ref().map(absolute).transpose()?;
    let ws = store::open_workspace(workspace)?;
    let dir = match (target_abs, &target) {
        (Some(abs), Some(given)) => scope(&ws, abs, given),
        _ => ws.collections(),
    };
    let md = crate::docs::markdown(&ws, &dir)?;
    match output {
        Some(path) => {
            std::fs::write(&path, md).map_err(|e| format!("write {}: {e}", path.display()))?
        }
        None => print!("{md}"),
    }
    Ok(true)
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
        let write = |name: &str, status: u16| {
            let req = crate::model::Request {
                url: url.clone(),
                tests: format!(
                    "pm.test('status', function () {{ pm.response.to.have.status({status}); }});"
                ),
                ..Default::default()
            };
            std::fs::write(dir.join(name), toml::to_string(&req).unwrap()).unwrap();
        };
        write("a.toml", 200);
        let args = |target: &str| {
            vec![
                target.to_owned(),
                "--workspace".into(),
                ws.display().to_string(),
            ]
        };
        // A CI job relies on this: green only when every test passed.
        assert_eq!(run(args("smoke")), Ok(true));
        write("b.toml", 404);
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
