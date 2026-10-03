//! The JSONPath most people type into a response filter: `$`, `.key`, `['key']`, `[0]`,
//! `[-1]`, `[*]`, `.*`, `..key`, and filters such as `[?(@.price < 10 && @.tag == 'x')]`.
//! Slices, regexes and functions are refused with a message rather than half-supported.

use serde_json::Value;

enum Step {
    Key(String),
    Index(i64),
    All,
    /// `..key`, or `..*` (None): every match at any depth.
    Descend(Option<String>),
    /// `[?(...)]`: the children it holds for.
    Filter(Expr),
}

enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    /// `@.x` alone: the path finds something.
    Exists(Operand),
    Cmp(Operand, Op, Operand),
}

enum Operand {
    /// `@...`, from the child being tested.
    Here(Vec<Step>),
    /// `$...`, from the document.
    Root(Vec<Step>),
    Literal(Value),
}

#[derive(Clone, Copy)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// What `path` selects in `json`: the value itself for a path that can only select one
/// (no `*` or `..`), else an array of every match.
pub fn select(json: &Value, path: &str) -> Result<Value, String> {
    let (steps, many) = parse(path)?;
    let found = run(json, json, &steps);
    Ok(match (many, found.as_slice()) {
        (false, [one]) => (*one).clone(),
        _ => Value::Array(found.into_iter().cloned().collect()),
    })
}

fn run<'a>(root: &'a Value, from: &'a Value, steps: &'a [Step]) -> Vec<&'a Value> {
    let mut found = vec![from];
    for step in steps {
        found = found
            .into_iter()
            .flat_map(|v| apply(root, v, step))
            .collect();
    }
    found
}

fn apply<'a>(root: &'a Value, v: &'a Value, step: &'a Step) -> Vec<&'a Value> {
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
        Step::Filter(e) => (children(v).into_iter())
            .filter(|c| holds(root, c, e))
            .collect(),
    }
}

fn holds(root: &Value, at: &Value, e: &Expr) -> bool {
    match e {
        Expr::Or(es) => es.iter().any(|e| holds(root, at, e)),
        Expr::And(es) => es.iter().all(|e| holds(root, at, e)),
        Expr::Not(e) => !holds(root, at, e),
        Expr::Exists(o) => value(root, at, o).is_some(),
        Expr::Cmp(a, op, b) => compare(value(root, at, a), *op, value(root, at, b)),
    }
}

fn value<'a>(root: &'a Value, at: &'a Value, o: &'a Operand) -> Option<&'a Value> {
    match o {
        Operand::Here(steps) => run(root, at, steps).into_iter().next(),
        Operand::Root(steps) => run(root, root, steps).into_iter().next(),
        Operand::Literal(v) => Some(v),
    }
}

/// As RFC 9535: numbers compare by value (10 == 10.0), strings by code point, and
/// nothing orders a number against a string. A missing value equals only another one.
fn compare(a: Option<&Value>, op: Op, b: Option<&Value>) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let order = match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => {
            (x.as_f64()).and_then(|x| y.as_f64().and_then(|y| x.partial_cmp(&y)))
        }
        (Some(Value::String(x)), Some(Value::String(y))) => Some(x.cmp(y)),
        _ => None,
    };
    let equal = match (a, b) {
        (Some(Value::Number(_)), Some(Value::Number(_))) => order == Some(Equal),
        _ => a == b,
    };
    match op {
        Op::Eq => equal,
        Op::Ne => !equal,
        Op::Lt => order == Some(Less),
        Op::Le => matches!(order, Some(Less | Equal)),
        Op::Gt => order == Some(Greater),
        Op::Ge => matches!(order, Some(Greater | Equal)),
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
            let close = closing(after).ok_or("`[` without `]`")?;
            let inside = after[..close].trim();
            if inside == "*" {
                steps.push(Step::All);
                many = true;
            } else if let Some(expr) = inside.strip_prefix('?') {
                steps.push(Step::Filter(filter(expr)?));
                many = true;
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

/// The `]` closing a `[`, past brackets and quoted text inside it.
fn closing(s: &str) -> Option<usize> {
    let (mut depth, mut quote) = (0usize, None);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ']') if depth == 0 => return Some(i),
            (None, ')' | ']') => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Offsets of the characters of `s` outside quotes and brackets; an opening or closing
/// bracket at the top counts as outside.
fn top(s: &str) -> Vec<usize> {
    let (mut depth, mut quote, mut out) = (0usize, None, Vec::new());
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[') => {
                if depth == 0 {
                    out.push(i);
                }
                depth += 1;
            }
            (None, ')' | ']') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(i);
                }
            }
            _ if depth == 0 => out.push(i),
            _ => {}
        }
    }
    out
}

/// `s` cut at each `sep` outside quotes and brackets.
fn split_top<'a>(s: &'a str, sep: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut from = 0;
    for i in top(s) {
        if i >= from && s[i..].starts_with(sep) {
            parts.push(&s[from..i]);
            from = i + sep.len();
        }
    }
    parts.push(&s[from..]);
    parts
}

fn filter(src: &str) -> Result<Expr, String> {
    let s = src.trim();
    for (sep, all) in [("||", false), ("&&", true)] {
        let parts = split_top(s, sep);
        if parts.len() > 1 {
            let es = parts.into_iter().map(filter).collect::<Result<_, _>>()?;
            return Ok(if all { Expr::And(es) } else { Expr::Or(es) });
        }
    }
    if s.starts_with('(') && top(s) == [0, s.len() - 1] {
        return filter(&s[1..s.len() - 1]);
    }
    if let Some(e) = s.strip_prefix('!').filter(|e| !e.starts_with('=')) {
        return Ok(Expr::Not(Box::new(filter(e)?)));
    }
    let ops = [
        ("==", Op::Eq),
        ("!=", Op::Ne),
        ("<=", Op::Le),
        (">=", Op::Ge),
        ("<", Op::Lt),
        (">", Op::Gt),
    ];
    for (sym, op) in ops {
        if let Some(i) = top(s).into_iter().find(|&i| s[i..].starts_with(sym)) {
            let (a, b) = (operand(&s[..i])?, operand(&s[i + sym.len()..])?);
            return Ok(Expr::Cmp(a, op, b));
        }
    }
    match operand(s)? {
        Operand::Literal(_) => Err(format!("`{s}` is a value, not a condition")),
        path => Ok(Expr::Exists(path)),
    }
}

fn operand(s: &str) -> Result<Operand, String> {
    let s = s.trim();
    let path = |p: &str| -> Result<Vec<Step>, String> {
        // Whatever isn't a path step (`=~`, a function's parentheses) would read as a key.
        if (top(p).into_iter()).any(|i| " =!<>~()&|".contains(&p[i..i + 1])) {
            return Err(format!(
                "`{s}`: only ==, !=, <, <=, >, >=, &&, || and ! are supported"
            ));
        }
        Ok(parse(p)?.0)
    };
    if let Some(p) = s.strip_prefix('@') {
        return match p.is_empty() || p.starts_with(['.', '[']) {
            true => Ok(Operand::Here(path(p)?)),
            false => Err(format!("`{s}` isn't a path")),
        };
    }
    if s.starts_with('$') {
        return Ok(Operand::Root(path(s)?));
    }
    if let Some(text) = s.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return Ok(Operand::Literal(Value::String(text.to_owned())));
    }
    serde_json::from_str(s).map(Operand::Literal).map_err(|_| {
        format!("`{s}` isn't @.path, $.path, a number, a 'string', true, false or null")
    })
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
    fn filters_keep_the_elements_their_condition_holds_for() {
        let doc = json!({ "limit": 15, "items": [
            { "id": 1, "price": 10, "tag": "a", "owner": { "name": "ann" } },
            { "id": 2, "price": 20, "tag": "b]" },
            { "id": 3, "price": 10.0, "tag": "a", "sold": true }
        ]});
        let ids = |cond: &str| select(&doc, &format!("$.items[?({cond})].id")).unwrap();
        assert_eq!(ids("@.price == 10"), json!([1, 3]), "10 and 10.0 are equal");
        assert_eq!(ids("@.price > 10"), json!([2]));
        assert_eq!(ids("@.price >= 20"), json!([2]));
        assert_eq!(ids("@.price < 20"), json!([1, 3]));
        assert_eq!(ids("@.price <= 10"), json!([1, 3]));
        assert_eq!(
            ids("@.price <= $.limit"),
            json!([1, 3]),
            "against the document"
        );
        assert_eq!(ids("@.tag == 'b]'"), json!([2]), "a ] inside quotes");
        assert_eq!(ids("@.tag != \"a\""), json!([2]));
        assert_eq!(ids("@.owner.name"), json!([1]), "exists");
        assert_eq!(ids("!@.sold"), json!([1, 2]));
        assert_eq!(ids("@.tag == 'a' && @.price < 15 && !@.sold"), json!([1]));
        assert_eq!(ids("@.id == 2 || (@.sold && @.price == 10)"), json!([2, 3]));
        assert_eq!(
            ids("@.missing != 'x'"),
            json!([1, 2, 3]),
            "absent equals no value"
        );
        let both = ids("@.missing == @.absent");
        assert_eq!(
            both,
            json!([1, 2, 3]),
            "but another absent one, as RFC 9535 has it"
        );
        assert_eq!(ids("@.price > 'a'"), json!([]), "no order across types");
        let tags = select(&doc, "$.items[?@.id == 2].tag").unwrap();
        assert_eq!(tags, json!(["b]"]), "parentheses are optional");
        assert_eq!(select(&doc, "$.items[?(@.id > 9)]").unwrap(), json!([]));
    }

    #[test]
    fn unsupported_syntax_is_refused_not_misread() {
        let doc = json!({ "a": [1] });
        for unsupported in [
            "$.a[?(@ =~ /1/)]",
            "$.a[?(length(@) > 0)]",
            "$.a[?(@.x in [1])]",
        ] {
            assert!(select(&doc, unsupported).is_err(), "{unsupported}");
        }
        assert!(
            select(&doc, "$.a[?(1)]").is_err(),
            "a value isn't a condition"
        );
        assert!(select(&doc, "$.a[?()]").is_err());
        assert!(select(&doc, "$.a[0:1]").is_err());
        assert!(select(&doc, "$.a[0").is_err());
        assert!(select(&doc, "$.").is_err());
    }
}
