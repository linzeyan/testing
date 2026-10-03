//! The XPath most people type into a response filter, over roxmltree: `/a/b`, `//b`,
//! `*`, `.`, `..`, `@attr`, `text()`, and predicates `[2]`, `[last()]`, `[@id]`,
//! `[@id='x']`, `[name]`, `[name='x']`, `[text()='x']`. Names match the local part, so a
//! SOAP response's prefixes don't matter. Functions and axes are refused with a message,
//! as `jsonpath` does.

use roxmltree::{Document, Node, ParsingOptions};

struct Step {
    /// After `//`: matches at any depth below.
    deep: bool,
    test: Test,
    preds: Vec<Pred>,
}

enum Test {
    /// `*` is None.
    Element(Option<String>),
    Attr(String),
    Text,
    Current,
    Parent,
}

enum Pred {
    /// 1-based, as in XPath.
    At(usize),
    Last,
    /// `[@a]`, `[child]`, `[text()]`, with `='v'` when compared.
    Has(Test, Option<String>),
}

/// The selected elements as their XML, then attributes and text as their values, one per
/// line in document order.
pub fn select(xml: &str, path: &str) -> Result<String, String> {
    let steps = parse(path)?;
    let options = ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = Document::parse_with_options(xml, options).map_err(|e| format!("XML: {e}"))?;
    let mut nodes = vec![doc.root()];
    let mut values: Option<Vec<&str>> = None;
    for (i, step) in steps.iter().enumerate() {
        if values.is_some() {
            return Err("XPath: @attr and text() can only come last".into());
        }
        let last = i + 1 == steps.len();
        match &step.test {
            Test::Attr(name) => {
                let found = (nodes.iter())
                    .flat_map(|n| context(*n, step.deep))
                    .filter_map(|n| attr(n, name))
                    .collect();
                values = Some(found);
            }
            Test::Text if last && step.preds.is_empty() => {
                let found = (nodes.iter())
                    .flat_map(|n| context(*n, step.deep))
                    .flat_map(|n| n.children().filter(Node::is_text))
                    .filter_map(|n| n.text())
                    .collect();
                values = Some(found);
            }
            _ => nodes = apply(&nodes, step),
        }
    }
    let out: Vec<&str> = match values {
        Some(v) => v,
        None => (nodes.iter())
            .map(|n| match n.is_element() {
                true => &xml[n.range()],
                false => n.text().unwrap_or_default(),
            })
            .collect(),
    };
    Ok(out.join("\n"))
}

/// The nodes whose children a step looks at: the node itself, or after `//` it and
/// every element below it (XPath's descendant-or-self).
fn context<'a, 'i>(n: Node<'a, 'i>, deep: bool) -> Vec<Node<'a, 'i>> {
    match deep {
        true => n
            .descendants()
            .filter(|d| d.is_element() || d.is_root())
            .collect(),
        false => vec![n],
    }
}

fn apply<'a, 'i>(nodes: &[Node<'a, 'i>], step: &Step) -> Vec<Node<'a, 'i>> {
    let mut out: Vec<Node> = Vec::new();
    for n in nodes {
        for parent in context(*n, step.deep) {
            // Positions count among one parent's matches, as `//b[1]` does in XPath.
            let matched: Vec<Node> = match &step.test {
                Test::Element(name) => (parent.children())
                    .filter(|c| c.is_element() && name_is(*c, name.as_deref()))
                    .collect(),
                Test::Text => parent.children().filter(Node::is_text).collect(),
                Test::Current => vec![parent],
                Test::Parent => parent.parent().into_iter().collect(),
                Test::Attr(_) => unreachable!("handled by select"),
            };
            out.extend(filter(matched, &step.preds));
        }
    }
    out.sort_by_key(|n| n.id().get());
    out.dedup_by_key(|n| n.id());
    out
}

fn filter<'a, 'i>(mut nodes: Vec<Node<'a, 'i>>, preds: &[Pred]) -> Vec<Node<'a, 'i>> {
    for p in preds {
        nodes = match p {
            Pred::At(i) => nodes.get(i - 1).copied().into_iter().collect(),
            Pred::Last => nodes.last().copied().into_iter().collect(),
            Pred::Has(test, want) => (nodes.into_iter())
                .filter(|n| {
                    let found: Vec<String> = match test {
                        Test::Attr(a) => attr(*n, a).map(str::to_owned).into_iter().collect(),
                        Test::Text => vec![text_of(*n)],
                        Test::Element(name) => (n.children())
                            .filter(|c| c.is_element() && name_is(*c, name.as_deref()))
                            .map(text_of)
                            .collect(),
                        Test::Current | Test::Parent => vec![text_of(*n)],
                    };
                    match want {
                        Some(v) => found.iter().any(|f| f == v),
                        None => !found.is_empty(),
                    }
                })
                .collect(),
        };
    }
    nodes
}

fn name_is(n: Node, name: Option<&str>) -> bool {
    name.is_none_or(|want| n.tag_name().name() == local(want))
}

fn attr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    (n.attributes())
        .find(|a| a.name() == local(name))
        .map(|a| a.value())
}

/// A string-value, as `[name='x']` compares it: all the text inside.
fn text_of(n: Node) -> String {
    n.descendants()
        .filter(Node::is_text)
        .filter_map(|t| t.text())
        .collect()
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn parse(path: &str) -> Result<Vec<Step>, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("XPath: empty".into());
    }
    // A relative path starts from the document, as a filter has nothing else to start at.
    let (mut rest, mut deep) = match path {
        p if p.starts_with("//") => (&p[2..], true),
        p => (p.strip_prefix('/').unwrap_or(p), false),
    };
    let mut steps = Vec::new();
    loop {
        let end = step_end(rest)?;
        steps.push(step(&rest[..end], deep)?);
        rest = &rest[end..];
        if rest.is_empty() {
            return Ok(steps);
        }
        (rest, deep) = match rest.strip_prefix("//") {
            Some(r) => (r, true),
            None => (&rest[1..], false),
        };
    }
}

/// Where the step starting `s` ends: at a `/` outside brackets and quotes.
fn step_end(s: &str) -> Result<usize, String> {
    let (mut depth, mut quote) = (0, None);
    for (i, c) in s.char_indices() {
        match (c, quote) {
            ('\'' | '"', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (_, Some(_)) => {}
            ('[', None) => depth += 1,
            (']', None) => depth -= 1,
            ('/', None) if depth == 0 => return Ok(i),
            _ => {}
        }
    }
    match (depth, quote) {
        (0, None) => Ok(s.len()),
        _ => Err("XPath: a [ or quote isn't closed".into()),
    }
}

fn step(s: &str, deep: bool) -> Result<Step, String> {
    let (head, mut preds_src) = match s.find('[') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let test = test(head.trim())?;
    let mut preds = Vec::new();
    while let Some(inner) = preds_src.strip_prefix('[') {
        let close = step_end_bracket(inner).ok_or("XPath: a [ isn't closed")?;
        preds.push(pred(inner[..close].trim())?);
        preds_src = inner[close + 1..].trim_start();
    }
    if !preds_src.is_empty() {
        return Err(format!("XPath: unexpected \"{preds_src}\""));
    }
    Ok(Step { deep, test, preds })
}

fn step_end_bracket(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (c, quote) {
            ('\'' | '"', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (']', None) => return Some(i),
            _ => {}
        }
    }
    None
}

fn test(s: &str) -> Result<Test, String> {
    let name_ok =
        |n: &str| !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || "_-.:".contains(c));
    Ok(match s {
        "." => Test::Current,
        ".." => Test::Parent,
        "*" => Test::Element(None),
        "text()" => Test::Text,
        a if a.starts_with('@') && name_ok(&a[1..]) => Test::Attr(a[1..].to_owned()),
        n if name_ok(n) => Test::Element(Some(n.to_owned())),
        other => {
            return Err(format!(
                "XPath: \"{other}\" isn't supported (names, *, ., .., @attr, text())"
            ));
        }
    })
}

fn pred(s: &str) -> Result<Pred, String> {
    if s == "last()" {
        return Ok(Pred::Last);
    }
    if let Ok(n) = s.parse::<usize>() {
        return match n {
            0 => Err("XPath: positions start at 1".into()),
            n => Ok(Pred::At(n)),
        };
    }
    let (lhs, want) = match s.split_once('=') {
        Some((l, r)) => {
            let r = r.trim();
            let quoted = (r.len() >= 2 && (r.starts_with('\'') && r.ends_with('\'')))
                || (r.len() >= 2 && r.starts_with('"') && r.ends_with('"'));
            if !quoted {
                return Err(format!("XPath: compare with a quoted value, as [{l}='x']"));
            }
            (l.trim(), Some(r[1..r.len() - 1].to_owned()))
        }
        None => (s, None),
    };
    match test(lhs)? {
        Test::Current | Test::Parent => Err(format!("XPath: [{s}] isn't supported")),
        t => Ok(Pred::Has(t, want)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOP: &str = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
  <soap:Body>
    <items count="3">
      <item id="a" kind="book"><name>Rust</name><price>10</price></item>
      <item id="b"><name>Go</name><price>8</price></item>
      <group><item id="c"><name>Zig</name></item></group>
    </items>
  </soap:Body>
</soap:Envelope>"#;

    fn at(path: &str) -> String {
        select(SHOP, path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn paths_pick_what_a_response_filter_needs() {
        assert_eq!(at("//item/@id"), "a\nb\nc");
        assert_eq!(at("/Envelope/Body/items/item/name/text()"), "Rust\nGo");
        // A prefix in the path is ignored like the document's.
        assert_eq!(at("/soap:Envelope/soap:Body/items/@count"), "3");
        assert_eq!(at("//item[2]/name/text()"), "Go");
        // Per parent, as in XPath: c is the first item in its group.
        assert_eq!(at("//item[1]/@id"), "a\nc");
        assert_eq!(at("//items/item[last()]/@id"), "b");
        assert_eq!(at("//item[@kind]/@id"), "a");
        assert_eq!(at("//item[@id='b']/price/text()"), "8");
        assert_eq!(at("//item[name='Zig']/@id"), "c");
        assert_eq!(at("//item[price][name='Go']/@id"), "b");
        assert_eq!(at("//name[text()='Rust']/../@id"), "a");
        assert_eq!(at("//group/*/@id"), "c");
        // Elements come out as written.
        assert_eq!(
            at("//group/item"),
            r#"<item id="c"><name>Zig</name></item>"#
        );
        assert_eq!(at("//nothing"), "");
    }

    #[test]
    fn what_isnt_supported_says_so() {
        let err = |p: &str| select(SHOP, p).unwrap_err();
        assert!(err("count(//item)").contains("isn't supported"));
        assert!(err("//item[@id=b]").contains("quoted"));
        assert!(err("//item[0]").contains("start at 1"));
        assert!(err("//item[@id").contains("isn't closed"));
        assert!(err("//@id/name").contains("come last"));
        assert!(select("<a>", "/a").unwrap_err().starts_with("XML:"));
    }
}
