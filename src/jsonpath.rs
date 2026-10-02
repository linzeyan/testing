//! The JSONPath most people type into a response filter: `$`, `.key`, `['key']`, `[0]`,
//! `[-1]`, `[*]`, `.*` and `..key`. Filters (`[?(...)]`) and slices are refused with a
//! message rather than half-supported.

use serde_json::Value;

enum Step {
    Key(String),
    Index(i64),
    All,
    /// `..key`, or `..*` (None): every match at any depth.
    Descend(Option<String>),
}

/// What `path` selects in `json`: the value itself for a path that can only select one
/// (no `*` or `..`), else an array of every match.
pub fn select(json: &Value, path: &str) -> Result<Value, String> {
    let (steps, many) = parse(path)?;
    let mut found = vec![json];
    for step in &steps {
        found = found.into_iter().flat_map(|v| apply(v, step)).collect();
    }
    Ok(match (many, found.as_slice()) {
        (false, [one]) => (*one).clone(),
        _ => Value::Array(found.into_iter().cloned().collect()),
    })
}

fn apply<'a>(v: &'a Value, step: &Step) -> Vec<&'a Value> {
    match step {
        Step::Key(k) => v.as_object().and_then(|o| o.get(k)).into_iter().collect(),
        Step::Index(i) => {
            let Some(a) = v.as_array() else {
                return Vec::new();
            };
            let i = if *i < 0 { a.len() as i64 + i } else { *i };
            usize::try_from(i)
                .ok()
                .and_then(|i| a.get(i))
                .into_iter()
                .collect()
        }
        Step::All => children(v),
        Step::Descend(name) => {
            let mut out = Vec::new();
            descend(v, name.as_deref(), &mut out);
            out
        }
    }
}

fn children(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.values().collect(),
        _ => Vec::new(),
    }
}

fn descend<'a>(v: &'a Value, name: Option<&str>, out: &mut Vec<&'a Value>) {
    match name {
        Some(n) => out.extend(v.as_object().and_then(|o| o.get(n))),
        None => out.extend(children(v)),
    }
    for c in children(v) {
        descend(c, name, out);
    }
}

/// The steps, and whether the path can select more than one value.
fn parse(path: &str) -> Result<(Vec<Step>, bool), String> {
    let path = path.trim();
    let mut rest = path.strip_prefix('$').unwrap_or(path);
    // `items[0]` reads as `$.items[0]`.
    let bare = format!(".{rest}");
    if !rest.is_empty() && !rest.starts_with(['.', '[']) {
        rest = &bare;
    }
    let (mut steps, mut many) = (Vec::new(), false);
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("..") {
            let (name, tail) = name(after);
            if name.is_empty() {
                return Err("`..` needs a name or `*` after it".into());
            }
            steps.push(Step::Descend((name != "*").then(|| name.to_owned())));
            (rest, many) = (tail, true);
        } else if let Some(after) = rest.strip_prefix('.') {
            let (name, tail) = name(after);
            match name {
                "" => return Err("`.` needs a name after it".into()),
                "*" => {
                    steps.push(Step::All);
                    many = true;
                }
                _ => steps.push(Step::Key(name.to_owned())),
            }
            rest = tail;
        } else if let Some(after) = rest.strip_prefix('[') {
            let close = after.find(']').ok_or("`[` without `]`")?;
            let inside = after[..close].trim();
            if inside == "*" {
                steps.push(Step::All);
                many = true;
            } else if inside.starts_with('?') {
                return Err("Filters like [?(@.x > 1)] aren't supported".into());
            } else {
                let quoted = (inside.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                    .or_else(|| inside.strip_prefix('"').and_then(|s| s.strip_suffix('"')));
                steps.push(match (quoted, inside.parse::<i64>()) {
                    (Some(key), _) => Step::Key(key.to_owned()),
                    (None, Ok(i)) => Step::Index(i),
                    (None, Err(_)) => {
                        return Err(format!("[{inside}] isn't an index, '*' or a 'key'"));
                    }
                });
            }
            rest = &after[close + 1..];
        } else {
            return Err(format!("Unexpected `{rest}`"));
        }
    }
    Ok((steps, many))
}

/// A dotted name runs to the next `.` or `[`.
fn name(s: &str) -> (&str, &str) {
    let end = s.find(['.', '[']).unwrap_or(s.len());
    (s[..end].trim(), &s[end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_select_what_people_expect() {
        let doc = json!({
            "items": [
                { "id": 1, "tags": ["a"], "owner": { "id": 9 } },
                { "id": 2, "tags": [] }
            ],
            "next page": "p2"
        });
        let q = |p: &str| select(&doc, p).unwrap();
        assert_eq!(q("$"), doc);
        assert_eq!(q("$.items[0].id"), json!(1));
        assert_eq!(
            q("items[-1].id"),
            json!(2),
            "no `$.`, and counting from the end"
        );
        assert_eq!(q("$['next page']"), json!("p2"));
        assert_eq!(q("$.items[*].id"), json!([1, 2]));
        assert_eq!(q("$..id"), json!([1, 9, 2]), "any depth, document order");
        assert_eq!(q("$.items[0].*").as_array().unwrap().len(), 3);
        // A wildcard path stays a list even when one value matches.
        assert_eq!(q("$.items[*].owner.id"), json!([9]));
        // A definite path that misses is an empty list, not an error.
        assert_eq!(q("$.items[5]"), json!([]));
    }

    #[test]
    fn unsupported_syntax_is_refused_not_misread() {
        let doc = json!({ "a": [1] });
        assert!(
            select(&doc, "$.a[?(@ > 0)]")
                .unwrap_err()
                .contains("Filters")
        );
        assert!(select(&doc, "$.a[0:1]").is_err());
        assert!(select(&doc, "$.a[0").is_err());
        assert!(select(&doc, "$.").is_err());
    }
}
