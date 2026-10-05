//! Colours for code: request bodies, the GraphQL query, scripts and generated snippets. One
//! small scanner serves every language here, told only its keywords, comments and quotes; a
//! grammar engine (syntect) would add megabytes for colours nobody reads more closely.

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId, Galley, TextBuffer, TextStyle, Ui, Visuals};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A string before a colon: an object key.
    Key,
    Str,
    Num,
    /// true, false, null and the like.
    Lit,
    Keyword,
    Comment,
    Punct,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Json,
    JavaScript,
    GraphQl,
    Xml,
    Python,
    Go,
    Ruby,
    Shell,
    /// Java, C#, Kotlin, Swift, Rust, PHP: close enough to share a list.
    CLike,
}

/// The language of a snippet target (`codegen::TARGETS`); none for raw HTTP.
pub fn of_target(target: &str) -> Option<Lang> {
    let first = target.split(' ').next().unwrap_or_default();
    Some(match first {
        "cURL" | "wget" | "HTTPie" | "PowerShell" => Lang::Shell,
        "Python" => Lang::Python,
        "JavaScript" | "Node.js" => Lang::JavaScript,
        "Go" => Lang::Go,
        "Ruby" => Lang::Ruby,
        "HTTP" => return None,
        _ => Lang::CLike,
    })
}

struct Spec {
    keywords: &'static [&'static str],
    literals: &'static [&'static str],
    /// Line comments; `#` only starts one at the start of a line or after a space, so a
    /// URL's `#fragment` stays a URL.
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    quotes: &'static [u8],
    /// XML: the name after `<` or `</` is a tag.
    tags: bool,
}

const JS_WORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "of",
    "return",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "while",
    "yield",
    "require",
];

fn spec(lang: Lang) -> Spec {
    let c = |keywords, literals| Spec {
        keywords,
        literals,
        line: &["//"],
        block: Some(("/*", "*/")),
        quotes: b"\"'`",
        tags: false,
    };
    match lang {
        Lang::Json => Spec {
            line: &[],
            block: None,
            quotes: b"\"",
            ..c(&[], &["true", "false", "null"])
        },
        Lang::JavaScript => c(JS_WORDS, &["true", "false", "null", "undefined"]),
        Lang::GraphQl => Spec {
            line: &["#"],
            block: None,
            quotes: b"\"",
            ..c(
                &[
                    "query",
                    "mutation",
                    "subscription",
                    "fragment",
                    "on",
                    "type",
                    "input",
                    "enum",
                    "schema",
                    "interface",
                    "union",
                    "scalar",
                    "extend",
                    "implements",
                ],
                &["true", "false", "null"],
            )
        },
        Lang::Xml => Spec {
            line: &[],
            block: Some(("<!--", "-->")),
            quotes: b"\"'",
            tags: true,
            ..c(&[], &[])
        },
        Lang::Python => Spec {
            line: &["#"],
            block: None,
            quotes: b"\"'",
            ..c(
                &[
                    "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                    "del", "elif", "else", "except", "finally", "for", "from", "if", "import",
                    "in", "is", "lambda", "not", "or", "pass", "raise", "return", "try", "while",
                    "with", "yield",
                ],
                &["True", "False", "None"],
            )
        },
        Lang::Go => c(
            &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "for",
                "func",
                "go",
                "if",
                "import",
                "interface",
                "map",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "type",
                "var",
            ],
            &["true", "false", "nil"],
        ),
        Lang::Ruby => Spec {
            line: &["#"],
            block: None,
            quotes: b"\"'",
            ..c(
                &[
                    "begin", "class", "def", "do", "else", "elsif", "end", "if", "module",
                    "require", "rescue", "return", "then", "unless", "while", "yield",
                ],
                &["true", "false", "nil"],
            )
        },
        Lang::Shell => Spec {
            line: &["#"],
            block: None,
            quotes: b"\"'",
            ..c(
                &[
                    "curl", "wget", "http", "if", "then", "else", "fi", "for", "do", "done",
                    "export",
                ],
                &["true", "false", "$true", "$false", "$null"],
            )
        },
        Lang::CLike => c(
            &[
                "async",
                "await",
                "catch",
                "class",
                "const",
                "else",
                "final",
                "fn",
                "for",
                "fun",
                "func",
                "if",
                "import",
                "let",
                "match",
                "mut",
                "namespace",
                "new",
                "package",
                "private",
                "pub",
                "public",
                "return",
                "static",
                "throws",
                "try",
                "use",
                "using",
                "val",
                "var",
                "void",
                "while",
                "function",
                "echo",
            ],
            &["true", "false", "null", "nil", "None", "Some", "Ok", "Err"],
        ),
    }
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$')
}

/// The coloured spans of `text`, in order; what's between them is plain. Every span starts
/// and ends at an ASCII byte or the end, so on a char boundary.
pub fn tokens(text: &str, lang: Lang) -> Vec<(Range<usize>, Kind)> {
    let spec = spec(lang);
    let b = text.as_bytes();
    let mut out: Vec<(Range<usize>, Kind)> = Vec::new();
    let mut i = 0;
    let line_end = |i: usize| text[i..].find('\n').map_or(b.len(), |n| i + n);
    while i < b.len() {
        let rest = &text[i..];
        let after_space = i == 0 || b[i - 1].is_ascii_whitespace();
        let (end, kind) = if (spec.line.iter())
            .any(|c| rest.starts_with(c) && (*c != "#" || after_space))
        {
            (line_end(i), Some(Kind::Comment))
        } else if let Some((open, close)) = spec.block.filter(|(open, _)| rest.starts_with(open)) {
            let body = &rest[open.len()..];
            let end = body
                .find(close)
                .map_or(b.len(), |n| i + open.len() + n + close.len());
            (end, Some(Kind::Comment))
        } else if spec.quotes.contains(&b[i]) {
            let quote = b[i];
            let mut j = i + 1;
            // Only a backtick string runs on past the line.
            while j < b.len() && b[j] != quote && (quote == b'`' || b[j] != b'\n') {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(b.len());
            let key = text[end..].trim_start_matches([' ', '\t']).starts_with(':');
            (
                end,
                Some(if key && !spec.tags {
                    Kind::Key
                } else {
                    Kind::Str
                }),
            )
        } else if b[i].is_ascii_digit() {
            // A digit inside a word never gets here: the word arm took the whole word.
            let n = b[i..]
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'.'));
            (n.map_or(b.len(), |n| i + n), Some(Kind::Num))
        } else if ident(b[i]) {
            let n = b[i..].iter().position(|c| !ident(*c));
            let end = n.map_or(b.len(), |n| i + n);
            let word = &text[i..end];
            let tag = spec.tags && (text[..i].ends_with('<') || text[..i].ends_with("</"));
            let kind = match () {
                _ if tag => Some(Kind::Keyword),
                _ if spec.literals.contains(&word) => Some(Kind::Lit),
                _ if spec.keywords.contains(&word) => Some(Kind::Keyword),
                _ => None,
            };
            (end, kind)
        } else if b[i].is_ascii_punctuation() {
            (i + 1, Some(Kind::Punct))
        } else {
            (i + 1, None)
        };
        // Never stall on a byte none of the arms consumed.
        let end = end.max(i + 1).min(b.len());
        if let Some(kind) = kind {
            match out.last_mut() {
                Some((r, k)) if *k == kind && r.end == i => r.end = end,
                _ => out.push((i..end, kind)),
            }
        }
        i = end;
        // Past a multi-byte char: a span must not start inside one.
        while i < b.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
    out
}

/// Readable on both themes; JSON's colours as VS Code's.
pub fn color(kind: Kind, visuals: &Visuals) -> Color32 {
    let rgb = |dark: (u8, u8, u8), light: (u8, u8, u8)| {
        let (r, g, b) = if visuals.dark_mode { dark } else { light };
        Color32::from_rgb(r, g, b)
    };
    match kind {
        Kind::Key => rgb((156, 210, 254), (4, 81, 165)),
        Kind::Str => rgb((206, 145, 120), (163, 21, 21)),
        Kind::Num => rgb((181, 206, 168), (9, 134, 88)),
        Kind::Lit => rgb((86, 156, 214), (0, 0, 255)),
        Kind::Keyword => rgb((197, 134, 192), (175, 0, 219)),
        Kind::Comment => rgb((106, 153, 85), (0, 128, 0)),
        Kind::Punct => visuals.weak_text_color(),
    }
}

/// Colours `lang`'s tokens into `colors`, one per byte of `text`. A colour per byte lets
/// the `{{var}}` colours go on top without splitting spans; at `MAX_EDIT` it's 512 KB.
pub fn paint(colors: &mut [Color32], text: &str, lang: Lang, visuals: &Visuals) {
    for (r, kind) in tokens(text, lang) {
        colors[r].fill(color(kind, visuals));
    }
}

/// `text` in `font`, a section per run of one colour. Runs change colour only at span
/// edges, which are char boundaries.
pub fn job(text: &str, colors: &[Color32], font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    if text.is_empty() {
        // An empty field still needs a section: it sets the height of the cursor's row.
        job.append(
            "",
            0.0,
            TextFormat::simple(font.clone(), Color32::PLACEHOLDER),
        );
    }
    let mut start = 0;
    for end in 1..=text.len() {
        if end == text.len() || colors[end] != colors[start] {
            let format = TextFormat::simple(font.clone(), colors[start]);
            job.append(&text[start..end], 0.0, format);
            start = end;
        }
    }
    job
}

/// A layouter for a `TextEdit` of code with no `{{vars}}` in it: scripts, snippets.
pub fn layouter(lang: Option<Lang>) -> impl FnMut(&Ui, &dyn TextBuffer, f32) -> Arc<Galley> {
    move |ui, buf, wrap_width| {
        let text = buf.as_str();
        let mut colors = vec![ui.visuals().text_color(); text.len()];
        if let Some(lang) = lang {
            paint(&mut colors, text, lang, ui.visuals());
        }
        let mut job = job(text, &colors, &TextStyle::Monospace.resolve(ui.style()));
        job.wrap.max_width = wrap_width;
        ui.painter().layout_job(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each span as (its text, kind), for reading the expectations.
    fn spans(text: &str, lang: Lang) -> Vec<(&str, Kind)> {
        let t = tokens(text, lang);
        t.into_iter()
            .map(|(r, k)| (&text[r], k))
            .filter(|(_, k)| *k != Kind::Punct)
            .collect()
    }

    #[test]
    fn json_keys_values_and_literals() {
        use Kind::*;
        assert_eq!(
            spans(r#"{"na\"me": "名字", "n": -1.5, "ok": true}"#, Lang::Json),
            [
                (r#""na\"me""#, Key),
                ("\"名字\"", Str),
                ("\"n\"", Key),
                ("1.5", Num),
                ("\"ok\"", Key),
                ("true", Lit)
            ]
        );
    }

    #[test]
    fn keywords_strings_and_comments_by_language() {
        use Kind::*;
        assert_eq!(
            spans("const x = 'a'; // why", Lang::JavaScript),
            [("const", Keyword), ("'a'", Str), ("// why", Comment)]
        );
        assert_eq!(
            spans("query Q($id: ID!) { user(id: $id) } # all", Lang::GraphQl),
            [("query", Keyword), ("# all", Comment)]
        );
        assert_eq!(
            spans("# note\nimport os\nx = None", Lang::Python),
            [("# note", Comment), ("import", Keyword), ("None", Lit)]
        );
        assert_eq!(
            spans("<a href=\"x\"><!-- c --></a>", Lang::Xml),
            [
                ("a", Keyword),
                ("\"x\"", Str),
                ("<!-- c -->", Comment),
                ("a", Keyword)
            ]
        );
        // Words that are keywords elsewhere, or inside a word, are not.
        assert_eq!(spans("format = iffy", Lang::Python), []);
    }

    /// A URL's fragment and a number inside a word aren't what they'd be on their own.
    #[test]
    fn a_hash_in_a_url_is_no_comment() {
        use Kind::*;
        assert_eq!(
            spans("curl https://x.test/#a 'b' # sent", Lang::Shell),
            [("curl", Keyword), ("'b'", Str), ("# sent", Comment)]
        );
        assert_eq!(spans("v2 = x1", Lang::Python), []);
    }

    #[test]
    fn spans_are_on_char_boundaries_and_unclosed_ones_end_with_the_text() {
        for text in ["\"名", "/* 註", "a 中 1", "`多\n行"] {
            for lang in [Lang::Json, Lang::JavaScript, Lang::Python] {
                for (r, _) in tokens(text, lang) {
                    assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
                    assert!(r.end <= text.len());
                }
            }
        }
    }
}
