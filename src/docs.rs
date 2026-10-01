//! API documentation as Markdown, generated from the requests under a folder: what
//! Postman's documentation view shows, in a form that pastes into a wiki or a README.

use std::fmt::Write as _;
use std::path::Path;

use crate::model::{Auth, Body, Grant, KeyValue, Request};
use crate::store::{Node, Workspace, folder_name};

/// `scope` is the collections root or a folder. Credentials are never written: auth
/// shows only its type (and OAuth's token URL).
pub fn markdown(ws: &Workspace, scope: &Path) -> Result<String, String> {
    let root = ws.collections();
    let tree = ws.tree();
    let (title, nodes) = if scope == root {
        ("API".to_owned(), &tree[..])
    } else {
        let children = find(&tree, scope).ok_or(format!("no folder at {}", scope.display()))?;
        (folder_name(&root, scope), children)
    };
    let mut out = format!("# {title}\n\n");
    if scope != root {
        text(&mut out, &ws.load_folder(scope)?.description);
    }
    if walk(ws, nodes, 2, &mut out)? == 0 {
        return Err(format!("no requests under {title}"));
    }
    Ok(out)
}

fn find<'a>(nodes: &'a [Node], dir: &Path) -> Option<&'a [Node]> {
    nodes.iter().find_map(|n| match n {
        Node::Folder { path, children, .. } if path == dir => Some(&children[..]),
        Node::Folder { path, children, .. } if dir.starts_with(path) => find(children, dir),
        _ => None,
    })
}

/// Requests before subfolders, so a folder's own requests sit right under its intro.
fn walk(ws: &Workspace, nodes: &[Node], level: usize, out: &mut String) -> Result<usize, String> {
    let h = "#".repeat(level.min(6));
    let mut count = 0;
    for node in nodes {
        if let Node::Request { name, path, .. } = node {
            request(out, &h, name, &ws.load_request(path)?);
            count += 1;
        }
    }
    for node in nodes {
        if let Node::Folder {
            name,
            path,
            children,
        } = node
        {
            let _ = write!(out, "{h} {name}\n\n");
            text(out, &ws.load_folder(path)?.description);
            count += walk(ws, children, level + 1, out)?;
        }
    }
    Ok(count)
}

fn request(out: &mut String, h: &str, name: &str, req: &Request) {
    let _ = write!(out, "{h} {name}\n\n`{} {}`", req.method, req.url);
    if !req.rpc.is_empty() {
        let _ = write!(out, " `{}` (proto `{}`)", req.rpc, req.proto);
    }
    out.push_str("\n\n");
    text(out, &req.description);
    let kind = match req.effective_auth() {
        Auth::Inherit => None,
        Auth::None => Some("none".to_owned()),
        Auth::Bearer { .. } => Some("Bearer token".into()),
        Auth::Basic { .. } => Some("Basic".into()),
        Auth::Digest { .. } => Some("Digest".into()),
        Auth::OAuth2(o) => {
            let grant = match o.grant {
                Grant::ClientCredentials => "client credentials",
                Grant::Password => "password",
            };
            Some(format!(
                "OAuth 2.0, {grant} grant, token URL `{}`",
                o.token_url
            ))
        }
    };
    if let Some(kind) = kind {
        let from = match (&req.auth, &req.inherited.auth) {
            (Auth::Inherit, Some((folder, _))) => format!(" (from folder \"{folder}\")"),
            _ => String::new(),
        };
        let _ = write!(out, "**Auth:** {kind}{from}\n\n");
    }
    table(out, "Query parameters", &req.params);
    table(out, "Headers", &req.headers);
    match &req.body {
        Body::None => {}
        Body::Json { text } => code(out, "Body (JSON)", "json", text),
        Body::Text { text } => code(out, "Body", "", text),
        Body::Form { fields } => table(out, "Body (form)", fields),
        Body::Multipart { parts } => table(out, "Body (multipart; `@path` uploads a file)", parts),
        Body::GraphQL { query, variables } => {
            code(out, "Query", "graphql", query);
            if !variables.trim().is_empty() {
                code(out, "Variables", "json", variables);
            }
        }
    }
    for ex in &req.examples {
        let ct = match ex.content_type.as_str() {
            "" => String::new(),
            ct => format!(" · `{ct}`"),
        };
        let lang = if ex.content_type.contains("json") {
            "json"
        } else {
            ""
        };
        let title = format!("Example: {} — {}{ct}", ex.name, ex.status);
        code(out, &title, lang, &ex.body);
    }
}

fn text(out: &mut String, s: &str) {
    if !s.trim().is_empty() {
        let _ = write!(out, "{}\n\n", s.trim());
    }
}

fn table(out: &mut String, title: &str, rows: &[KeyValue]) {
    let rows: Vec<_> = rows
        .iter()
        .filter(|r| r.enabled && !r.key.is_empty())
        .collect();
    if rows.is_empty() {
        return;
    }
    let cell = |s: &str| s.replace('|', "\\|").replace('\n', "<br>");
    let _ = write!(out, "**{title}**\n\n| Name | Value |\n| --- | --- |\n");
    for r in rows {
        let _ = writeln!(out, "| {} | {} |", cell(&r.key), cell(&r.value));
    }
    out.push('\n');
}

fn code(out: &mut String, title: &str, lang: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    // Longer than any backtick run inside, so a body containing ``` can't end the block.
    let run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(run.max(2) + 1);
    let _ = write!(
        out,
        "**{title}**\n\n{fence}{lang}\n{}\n{fence}\n\n",
        text.trim_end()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Example, Folder};

    #[test]
    fn docs_describe_every_request_without_leaking_credentials() {
        let root = std::env::temp_dir().join(format!("apitool-docs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = Workspace::open(root.clone()).unwrap();
        let api = ws.create_folder(&ws.collections(), "api").unwrap();
        let users = ws.create_folder(&api, "users").unwrap();
        ws.save_folder(
            &api,
            &Folder {
                description: "The public API.".into(),
                auth: Auth::Bearer {
                    token: "s3cret-token".into(),
                },
                ..Default::default()
            },
        )
        .unwrap();
        let mut off = KeyValue::new("X-Debug", "1");
        off.enabled = false;
        let get = Request {
            url: "{{base}}/users/{{id}}?expand=a|b".into(),
            description: "Fetches one user.".into(),
            headers: vec![KeyValue::new("Accept", "application/json"), off],
            examples: vec![Example {
                name: "found".into(),
                status: 200,
                content_type: "application/json".into(),
                body: "{\"note\": \"```\"}".into(),
            }],
            ..Default::default()
        };
        ws.save_request(&users.join("get user.toml"), &get).unwrap();
        let login = Request {
            method: "POST".into(),
            url: "{{base}}/login".into(),
            auth: Auth::Basic {
                username: "me".into(),
                password: "hunter2".into(),
            },
            body: Body::Json {
                text: "{\"remember\": true}".into(),
            },
            ..Default::default()
        };
        ws.save_request(&api.join("login.toml"), &login).unwrap();

        let md = markdown(&ws, &api).unwrap();
        assert!(
            !md.contains("s3cret-token") && !md.contains("hunter2"),
            "{md}"
        );
        // The folder's own request comes before its subfolder, at the same depth.
        let order: Vec<_> = [
            "# api",
            "The public API.",
            "## login",
            "## users",
            "### get user",
        ]
        .iter()
        .map(|s| {
            md.find(&format!("{s}\n"))
                .unwrap_or_else(|| panic!("{s}: {md}"))
        })
        .collect();
        assert!(order.is_sorted(), "{md}");
        assert!(md.contains("`GET {{base}}/users/{{id}}?expand=a|b`"));
        assert!(md.contains("**Auth:** Bearer token (from folder \"api\")"));
        assert!(md.contains("**Auth:** Basic\n"));
        // Disabled rows aren't part of the API; pipes would break the table.
        assert!(md.contains("| Accept | application/json |") && !md.contains("X-Debug"));
        assert!(md.contains("**Example: found — 200 · `application/json`**\n\n````json\n"));
        assert_eq!(markdown(&ws, &ws.collections()).unwrap()[..6], *"# API\n");
        assert!(markdown(&ws, &root.join("nope")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
