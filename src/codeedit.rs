//! What the code editors (bodies, scripts, GraphQL, JWT parts) do beyond egui's TextEdit,
//! with VS Code's keys: toggle comment, format, move, copy and delete lines, indent the
//! selected lines, keep the indent on Enter, copy or cut the line with nothing selected,
//! and find and replace in a box at the editor's top right.
//!
//! The text commands are plain functions over a string and a selection in chars (egui's
//! unit), so they are tested without a window. `take_keys` runs before the app's own
//! shortcuts: egui matches a shortcut ignoring extra Shift and Alt, so ⌘⇧K would also be
//! ⌘K (Switch) and ⌘⌥F would be ⌘F (find in the response) if the app looked first.

use std::collections::HashMap;
use std::ops::Range;

use eframe::egui::{
    self, Color32, Event, Id, Key, Modifiers, text::CCursor, text::CCursorRange, text::LayoutJob,
};

use crate::app::{find_options, icon_button};
use crate::i18n::{t, tf};
use crate::icons as icon;
use crate::syntax::Lang;

/// Enough to step through: one letter in a 16 MiB body would otherwise keep millions of
/// 8-byte offsets. The response's find stops there too.
pub const MAX_HITS: usize = 10_000;

/// A selection in chars, `start <= end`, as the commands take and give it back.
pub type Sel = (usize, usize);

/// A command's result: the new text and where the selection goes.
#[derive(Debug, PartialEq)]
pub struct Edit {
    pub text: String,
    pub sel: Sel,
}

fn byte_of(text: &str, char: usize) -> usize {
    text.char_indices().nth(char).map_or(text.len(), |(b, _)| b)
}

fn char_of(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
}

/// The bytes of the whole lines a selection touches, without the last one's newline. A
/// selection that ends at a line's very start leaves that line out, as in VS Code: it is
/// what a drag down to the next line's start selects.
fn lines_of(text: &str, (start, end): (usize, usize)) -> Range<usize> {
    let end = match end > start && text[..end].ends_with('\n') {
        true => end - 1,
        false => end,
    };
    let from = text[..start].rfind('\n').map_or(0, |n| n + 1);
    let to = text[end..].find('\n').map_or(text.len(), |n| end + n);
    from..to
}

fn bytes(text: &str, (start, end): Sel) -> (usize, usize) {
    (byte_of(text, start), byte_of(text, end))
}

/// Applies non-overlapping replacements (byte ranges in `text`, in order) and maps the
/// selection through them: a position after a replacement moves with it, one inside it
/// goes to its end. Text inserted right at a position goes before it (the text there
/// moved), except at the start of a selection: a selection from a line's start still
/// takes the whole line, a comment marker put there included.
fn apply(text: &str, sel: Sel, edits: &[(Range<usize>, String)]) -> Edit {
    let (start, end) = bytes(text, sel);
    let map = |p: usize, keeps_inserts: bool| -> usize {
        let mut shift: isize = 0;
        for (r, with) in edits {
            let insert_here = r.is_empty() && r.start == p;
            if insert_here {
                if !keeps_inserts {
                    shift += with.len() as isize;
                }
            } else if r.end <= p {
                shift += with.len() as isize - r.len() as isize;
            } else if r.start < p {
                return (r.start as isize + shift) as usize + with.len();
            }
        }
        (p as isize + shift) as usize
    };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (r, with) in edits {
        out.push_str(&text[at..r.start]);
        out.push_str(with);
        at = r.end;
    }
    out.push_str(&text[at..]);
    let (start, end) = (map(start, start != end), map(end, false));
    Edit {
        sel: (char_of(&out, start), char_of(&out, end)),
        text: out,
    }
}

/// The line starts in a byte range of whole lines.
fn starts(text: &str, lines: &Range<usize>) -> Vec<usize> {
    let mut out = vec![lines.start];
    out.extend(
        text[lines.clone()]
            .match_indices('\n')
            .map(|(n, _)| lines.start + n + 1),
    );
    out
}

fn line_at(text: &str, start: usize) -> &str {
    let end = text[start..].find('\n').map_or(text.len(), |n| start + n);
    &text[start..end]
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// One level of indentation as the text has it: a tab if lines start with tabs, else the
/// smallest indent of any line (2 to 8 spaces), else the language's usual one.
pub fn unit(text: &str, lang: Option<Lang>) -> String {
    let mut least = usize::MAX;
    for line in text.lines() {
        let indent = indent_of(line);
        if indent.starts_with('\t') {
            return "\t".into();
        }
        if !indent.is_empty() && line.len() > indent.len() {
            least = least.min(indent.len());
        }
    }
    match least {
        2..=8 => " ".repeat(least),
        _ if matches!(lang, Some(Lang::Json | Lang::GraphQl | Lang::Xml)) => "  ".into(),
        _ => "    ".into(),
    }
}

/// How a language comments: a line prefix, and a block's ends. JSON has neither.
pub fn comments(
    lang: Option<Lang>,
) -> (Option<&'static str>, Option<(&'static str, &'static str)>) {
    match lang {
        Some(Lang::JavaScript | Lang::CLike | Lang::Go) => (Some("//"), Some(("/*", "*/"))),
        Some(Lang::Python | Lang::Ruby | Lang::Shell | Lang::GraphQl) => (Some("#"), None),
        Some(Lang::Xml) => (None, Some(("<!--", "-->"))),
        _ => (None, None),
    }
}

/// ⌘/: comments the selected lines out, at the shallowest indent among them, or back in
/// when every non-blank one already is.
pub fn toggle_line_comment(text: &str, sel: Sel, prefix: &str) -> Edit {
    let lines = lines_of(text, bytes(text, sel));
    let starts = starts(text, &lines);
    let filled: Vec<usize> = (starts.iter().copied())
        .filter(|&s| !line_at(text, s).trim().is_empty())
        .collect();
    if filled.is_empty() {
        return apply(text, sel, &[]);
    }
    let commented = filled
        .iter()
        .all(|&s| line_at(text, s).trim_start().starts_with(prefix));
    let edits: Vec<(Range<usize>, String)> = if commented {
        (filled.iter())
            .map(|&s| {
                let line = line_at(text, s);
                let at = s + indent_of(line).len();
                let mut end = at + prefix.len();
                if text[end..].starts_with(' ') {
                    end += 1;
                }
                (at..end, String::new())
            })
            .collect()
    } else {
        let depth = (filled.iter())
            .map(|&s| indent_of(line_at(text, s)).len())
            .min()
            .unwrap_or(0);
        (filled.iter())
            .map(|&s| (s + depth..s + depth, format!("{prefix} ")))
            .collect()
    };
    apply(text, sel, &edits)
}

/// ⇧⌥A: wraps the selection in a block comment, or unwraps one that is.
pub fn toggle_block_comment(text: &str, sel: Sel, (open, close): (&str, &str)) -> Edit {
    let (start, end) = bytes(text, sel);
    let inner = &text[start..end];
    let trimmed = inner.trim();
    if trimmed.starts_with(open)
        && trimmed.ends_with(close)
        && trimmed.len() >= open.len() + close.len()
    {
        let lead = inner.len() - inner.trim_start().len();
        let o = start + lead;
        let c = start + lead + trimmed.len() - close.len();
        let pad = |r: Range<usize>, inside_start: bool| -> Range<usize> {
            match inside_start {
                true if text[r.end..].starts_with(' ') => r.start..r.end + 1,
                false if r.start > 0 && text[..r.start].ends_with(' ') => r.start - 1..r.end,
                _ => r,
            }
        };
        let first = pad(o..o + open.len(), true);
        let last = pad(c..c + close.len(), false);
        let last = last.start.max(first.end)..last.end;
        return apply(text, sel, &[(first, String::new()), (last, String::new())]);
    }
    let edits = [
        (start..start, format!("{open} ")),
        (end..end, format!(" {close}")),
    ];
    let mut out = apply(text, sel, &edits);
    // The selection stays on what was selected, inside the markers.
    out.sel = (
        sel.0 + open.chars().count() + 1,
        sel.1 + open.chars().count() + 1,
    );
    out
}

/// ⌥↑ / ⌥↓: the selected lines swap with the line above or below.
pub fn move_lines(text: &str, sel: Sel, up: bool) -> Option<Edit> {
    let lines = lines_of(text, bytes(text, sel));
    let block = &text[lines.clone()];
    let (start, end) = bytes(text, sel);
    let (out, shift): (String, isize) = if up {
        let prev_end = lines.start.checked_sub(1)?;
        let prev_start = text[..prev_end].rfind('\n').map_or(0, |n| n + 1);
        let prev = &text[prev_start..prev_end];
        let out = format!(
            "{}{block}\n{prev}{}",
            &text[..prev_start],
            &text[lines.end..]
        );
        (out, -(prev.len() as isize + 1))
    } else {
        if lines.end == text.len() {
            return None;
        }
        let next_start = lines.end + 1;
        let next_end = text[next_start..]
            .find('\n')
            .map_or(text.len(), |n| next_start + n);
        let next = &text[next_start..next_end];
        let out = format!(
            "{}{next}\n{block}{}",
            &text[..lines.start],
            &text[next_end..]
        );
        (out, next.len() as isize + 1)
    };
    let at = |p: usize| char_of(&out, (p as isize + shift) as usize);
    Some(Edit {
        sel: (at(start), at(end)),
        text: out,
    })
}

/// ⇧⌥↑ / ⇧⌥↓: the selected lines again above or below; the selection is on the copy.
pub fn copy_lines(text: &str, sel: Sel, up: bool) -> Edit {
    let lines = lines_of(text, bytes(text, sel));
    let block = text[lines.clone()].to_owned();
    if up {
        // Inserted above, the same text: the selection now reads the copy.
        let mut out = apply(
            text,
            sel,
            &[(lines.start..lines.start, format!("{block}\n"))],
        );
        out.sel = sel;
        out
    } else {
        // Below: the selection follows to the copy.
        let n = block.chars().count() + 1;
        let mut out = apply(text, sel, &[(lines.end..lines.end, format!("\n{block}"))]);
        out.sel = (sel.0 + n, sel.1 + n);
        out
    }
}

/// ⇧⌘K: the selected lines go, newline and all; the cursor is where they were.
pub fn delete_lines(text: &str, sel: Sel) -> Edit {
    let lines = lines_of(text, bytes(text, sel));
    let gone = match lines.end < text.len() {
        true => lines.start..lines.end + 1,
        // The last line: the newline before it goes instead.
        false => lines.start.saturating_sub(1)..lines.end,
    };
    let out = format!("{}{}", &text[..gone.start], &text[gone.end..]);
    let at = char_of(&out, lines.start.min(out.len()));
    Edit {
        text: out,
        sel: (at, at),
    }
}

/// ⌘] or Tab over several lines: each non-blank one a level deeper.
pub fn indent(text: &str, sel: Sel, unit: &str) -> Edit {
    let lines = lines_of(text, bytes(text, sel));
    let edits: Vec<(Range<usize>, String)> = (starts(text, &lines).into_iter())
        .filter(|&s| !line_at(text, s).trim().is_empty())
        .map(|s| (s..s, unit.to_owned()))
        .collect();
    apply(text, sel, &edits)
}

/// ⌘[ or ⇧Tab: each selected line a level shallower, as far as it goes.
pub fn outdent(text: &str, sel: Sel, unit: &str) -> Edit {
    let lines = lines_of(text, bytes(text, sel));
    let width = unit.len().max(1);
    let edits: Vec<(Range<usize>, String)> = (starts(text, &lines).into_iter())
        .filter_map(|s| {
            let indent = indent_of(line_at(text, s));
            let n = match indent.starts_with('\t') {
                true => 1,
                false => indent.len().min(width),
            };
            (n > 0).then(|| (s..s + n, String::new()))
        })
        .collect();
    apply(text, sel, &edits)
}

/// Tab with no line break in the selection: spaces to the next indent stop (a tab where
/// the text indents with tabs) in place of the selection.
pub fn tab(text: &str, sel: Sel, unit: &str) -> Edit {
    let (start, end) = bytes(text, sel);
    let with = match unit {
        "\t" => "\t".to_owned(),
        _ => {
            let column = text[..start]
                .rsplit('\n')
                .next()
                .unwrap_or("")
                .chars()
                .count();
            " ".repeat(unit.len() - column % unit.len())
        }
    };
    let n = with.chars().count();
    let mut out = apply(text, sel, &[(start..end, with)]);
    out.sel = (sel.0 + n, sel.0 + n);
    out
}

/// Enter: the new line starts at this one's indent, a level deeper after an opening
/// bracket; between a pair of brackets the closing one goes to a line of its own.
pub fn newline(text: &str, sel: Sel, unit: &str) -> Edit {
    let (start, end) = bytes(text, sel);
    let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
    let indent = indent_of(&text[line_start..start]).to_owned();
    let before = text[..start].trim_end_matches([' ', '\t']).chars().last();
    let after = text[end..].trim_start_matches([' ', '\t']).chars().next();
    let opens = matches!(before, Some('{' | '[' | '('));
    let pair = matches!(
        (before, after),
        (Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')'))
    );
    let deeper = match opens {
        true => format!("{indent}{unit}"),
        false => indent.clone(),
    };
    let inserted = match pair {
        true => format!("\n{deeper}\n{indent}"),
        false => format!("\n{deeper}"),
    };
    // Spaces left at the cursor would trail on the line above.
    let end = end + (text[end..].len() - text[end..].trim_start_matches([' ', '\t']).len());
    let at = sel.0 + 1 + deeper.chars().count();
    let mut out = apply(text, sel, &[(start..end, inserted)]);
    out.sel = (at, at);
    out
}

/// What ⌘C or ⌘X with nothing selected take: the cursor's line, newline included.
pub fn line_span(text: &str, at: usize) -> Range<usize> {
    let b = byte_of(text, at);
    let lines = lines_of(text, (b, b));
    match lines.end < text.len() {
        true => lines.start..lines.end + 1,
        false => lines,
    }
}

/// The find box's pattern as a regex: escaped unless it is one, whole words by ASCII
/// boundaries (Unicode ones push the regex crate off its fast engine on non-ASCII text).
pub fn pattern(
    query: &str,
    case: bool,
    word: bool,
    regex: bool,
) -> Result<regex::Regex, regex::Error> {
    let pattern = match regex {
        true => query.to_owned(),
        false => regex::escape(query),
    };
    let pattern = match word {
        true => format!(r"(?-u:\b)(?:{pattern})(?-u:\b)"),
        false => pattern,
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case)
        .build()
}

/// Every non-empty match, up to `MAX_HITS`: empty ones (`a*`) would mark every position.
pub fn hits(re: &regex::Regex, text: &str) -> Vec<Range<usize>> {
    (re.find_iter(text).filter(|m| !m.is_empty()))
        .map(|m| m.range())
        .take(MAX_HITS)
        .collect()
}

/// Replaces the matches at `ranges` (from `hits`), `$1` and `${name}` expanded in a
/// regular expression's replacement.
pub fn replace(
    text: &str,
    re: &regex::Regex,
    ranges: &[Range<usize>],
    with: &str,
    regex: bool,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in ranges {
        out.push_str(&text[at..r.start]);
        match (regex, re.captures_at(text, r.start)) {
            (true, Some(caps)) if caps.get(0).is_some_and(|m| m.range() == *r) => {
                caps.expand(with, &mut out);
            }
            _ => out.push_str(with),
        }
        at = r.end;
    }
    out.push_str(&text[at..]);
    out
}

/// Format (⇧⌥F) by language: JSON, XML and GraphQL as their Beautify buttons do,
/// JavaScript and TypeScript with js-beautify.
pub fn format(text: &str, lang: Option<Lang>, unit: &str) -> Result<String, String> {
    match lang {
        Some(Lang::Json) => {
            crate::http::pretty_json(text).ok_or_else(|| t("Not valid JSON").to_owned())
        }
        Some(Lang::Xml) => {
            crate::http::pretty_xml(text).ok_or_else(|| t("Not valid XML").to_owned())
        }
        Some(Lang::GraphQl) => crate::graphql::pretty(text)
            .ok_or_else(|| t("This query can't be formatted").to_owned()),
        Some(Lang::JavaScript) => crate::script::beautify(text, unit),
        _ => Err(t("Nothing formats this kind of text").to_owned()),
    }
}

/// A key the editor acts on, kept from `take_keys` until it draws.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmd {
    Find,
    Replace,
    Comment,
    DeleteLines,
}

/// The find box of one editor.
#[derive(Clone, Default)]
struct Find {
    open: bool,
    replace: bool,
    query: String,
    with: String,
    case: bool,
    word: bool,
    regex: bool,
    current: usize,
    /// Put the cursor in the query on the next draw.
    focus: bool,
    /// Select the current match in the editor and bring it into view.
    show: bool,
}

/// Which editor each focusable part belongs to: the editor itself and its find fields.
fn owners(ctx: &egui::Context) -> HashMap<Id, Id> {
    ctx.data(|d| d.get_temp(Id::new("code-edit-owners")))
        .unwrap_or_default()
}

fn own(ctx: &egui::Context, part: Id, editor: Id) {
    let mut map = owners(ctx);
    if map.get(&part) != Some(&editor) {
        map.insert(part, editor);
        ctx.data_mut(|d| d.insert_temp(Id::new("code-edit-owners"), map));
    }
}

/// Removes a key press with exactly these modifiers: egui's own matching would take
/// ⌥↑ for ⇧⌥↑ too.
fn take(i: &mut egui::InputState, mods: Modifiers, key: Key) -> bool {
    let at = i.events.iter().position(|e| {
        matches!(e, Event::Key { key: k, pressed: true, modifiers, .. }
            if *k == key && modifiers.matches_exact(mods))
    });
    at.map(|n| i.events.remove(n)).is_some()
}

const SHIFT_ALT: Modifiers = Modifiers::SHIFT.plus(Modifiers::ALT);

/// VS Code's replace key: ⌥⌘F on a Mac, Ctrl+H elsewhere (Ctrl+H deletes a character in a
/// Mac text field).
/// `replace_key` as the tooltips show keys.
fn replace_label(ctx: &egui::Context) -> String {
    let (mods, key) = replace_key(ctx);
    ctx.format_shortcut(&egui::KeyboardShortcut::new(mods, key))
}

pub fn replace_key(ctx: &egui::Context) -> (Modifiers, Key) {
    match ctx.os() {
        egui::os::OperatingSystem::Mac => (Modifiers::COMMAND.plus(Modifiers::ALT), Key::F),
        _ => (Modifiers::CTRL, Key::H),
    }
}

/// Before the app's shortcuts: while a code editor (or its find box) has the focus, the
/// keys it shares with them are its own.
pub fn take_keys(ctx: &egui::Context) {
    let Some(focused) = ctx.memory(|m| m.focused()) else {
        return;
    };
    let Some(editor) = owners(ctx).get(&focused).copied() else {
        return;
    };
    let in_editor = focused == editor;
    let (rmods, rkey) = replace_key(ctx);
    let cmds = ctx.input_mut(|i| {
        let mut cmds = Vec::new();
        // The longer chords first: ⌥⌘F also reads as ⌘F.
        if take(i, rmods, rkey) {
            cmds.push(Cmd::Replace);
        }
        if take(i, Modifiers::COMMAND, Key::F) {
            cmds.push(Cmd::Find);
        }
        if in_editor && take(i, Modifiers::COMMAND, Key::Slash) {
            cmds.push(Cmd::Comment);
        }
        if in_editor && take(i, Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::K) {
            cmds.push(Cmd::DeleteLines);
        }
        cmds
    });
    if !cmds.is_empty() {
        ctx.data_mut(|d| d.insert_temp(editor.with("cmds"), cmds));
    }
}

fn selection(ctx: &egui::Context, id: Id, text: &str) -> Sel {
    let n = text.chars().count();
    let range = egui::TextEdit::load_state(ctx, id).and_then(|s| s.cursor.char_range());
    range.map_or((n, n), |r| {
        let r = r.as_sorted_char_range();
        (r.start.0.min(n), r.end.0.min(n))
    })
}

fn select(ctx: &egui::Context, id: Id, (start, end): Sel) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(start),
        CCursor::new(end),
    )));
    state.store(ctx, id);
}

/// Just before the editor `id` shows `text`: its keys, a command from `take_keys`, a copy
/// or cut of the line. True when the text changed.
pub fn before(ui: &mut egui::Ui, id: Id, text: &mut String, lang: Option<Lang>) -> bool {
    let ctx = ui.ctx().clone();
    own(&ctx, id, id);
    let cmds: Vec<Cmd> = ctx
        .data_mut(|d| d.remove_temp(id.with("cmds")))
        .unwrap_or_default();
    let focused = ctx.memory(|m| m.has_focus(id));
    let mut find: Find = ctx
        .data(|d| d.get_temp(id.with("find")))
        .unwrap_or_default();
    let asked = cmds.iter().any(|c| matches!(c, Cmd::Find | Cmd::Replace));
    for cmd in &cmds {
        if let Cmd::Find | Cmd::Replace = cmd {
            // The selection, if it is on one line, is what to look for.
            let sel = selection(&ctx, id, text);
            let picked = text
                .chars()
                .skip(sel.0)
                .take(sel.1 - sel.0)
                .collect::<String>();
            if focused && !picked.is_empty() && !picked.contains('\n') {
                find.query = picked;
            }
            (find.open, find.focus) = (true, true);
            find.replace |= *cmd == Cmd::Replace;
        }
    }
    if asked {
        ctx.data_mut(|d| d.insert_temp(id.with("find"), find));
    }
    if !focused {
        return false;
    }
    let sel = selection(&ctx, id, text);
    let unit = || unit(text, lang);
    let (line, block) = comments(lang);
    let mut edit: Option<Edit> = None;
    let mut status: Option<String> = None;
    for cmd in cmds {
        match (cmd, line) {
            (Cmd::Comment, Some(prefix)) => edit = Some(toggle_line_comment(text, sel, prefix)),
            (Cmd::DeleteLines, _) => edit = Some(delete_lines(text, sel)),
            _ => {}
        }
    }
    let multiline = text
        .chars()
        .skip(sel.0)
        .take(sel.1 - sel.0)
        .any(|c| c == '\n');
    ctx.input_mut(|i| {
        if edit.is_some() {
            return;
        }
        // Longer chords before the ones they contain.
        edit = if take(i, SHIFT_ALT, Key::ArrowUp) {
            Some(copy_lines(text, sel, true))
        } else if take(i, SHIFT_ALT, Key::ArrowDown) {
            Some(copy_lines(text, sel, false))
        } else if take(i, Modifiers::ALT, Key::ArrowUp) {
            move_lines(text, sel, true)
        } else if take(i, Modifiers::ALT, Key::ArrowDown) {
            move_lines(text, sel, false)
        } else if take(i, SHIFT_ALT, Key::A) {
            block.map(|b| toggle_block_comment(text, sel, b))
        } else if take(i, SHIFT_ALT, Key::F) {
            match format(text, lang, &unit()) {
                Ok(pretty) if pretty != *text => Some(Edit {
                    sel: (0, 0),
                    text: pretty,
                }),
                Ok(_) => None,
                Err(e) => {
                    status = Some(tf("Not formatted: {}", &[&e]));
                    None
                }
            }
        } else if take(i, Modifiers::COMMAND, Key::CloseBracket) {
            Some(indent(text, sel, &unit()))
        } else if take(i, Modifiers::COMMAND, Key::OpenBracket)
            || take(i, Modifiers::SHIFT, Key::Tab)
        {
            Some(outdent(text, sel, &unit()))
        } else if take(i, Modifiers::NONE, Key::Tab) {
            Some(match multiline {
                true => indent(text, sel, &unit()),
                false => tab(text, sel, &unit()),
            })
        } else if take(i, Modifiers::NONE, Key::Enter) {
            Some(newline(text, sel, &unit()))
        } else {
            None
        };
        // ⌘C / ⌘X with nothing selected: the whole line, as VS Code does.
        if edit.is_none() && sel.0 == sel.1 {
            let at = i
                .events
                .iter()
                .position(|e| matches!(e, Event::Copy | Event::Cut));
            if let Some(n) = at {
                let cut = matches!(i.events.remove(n), Event::Cut);
                let span = line_span(text, sel.0);
                let line = text[span.clone()].to_owned();
                i.events.retain(|e| !matches!(e, Event::Copy | Event::Cut));
                edit = cut.then(|| {
                    let out = format!("{}{}", &text[..span.start], &text[span.end..]);
                    let at = char_of(&out, span.start.min(out.len()));
                    Edit {
                        text: out,
                        sel: (at, at),
                    }
                });
                ctx.copy_text(line);
            }
        }
    });
    if let Some(s) = status {
        ctx.data_mut(|d| d.insert_temp(Id::new("code-edit-status"), s));
    }
    match edit {
        Some(Edit { text: new, sel }) if new != *text => {
            *text = new;
            select(&ctx, id, sel);
            true
        }
        _ => false,
    }
}

/// A message a command left for the status bar (a format that failed), taken once.
pub fn status(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp(Id::new("code-edit-status")))
}

/// The find box's matches in `text`, and which one is current, for `mark`.
fn found(ctx: &egui::Context, id: Id, text: &str) -> Option<(Vec<Range<usize>>, usize)> {
    let find: Find = ctx.data(|d| d.get_temp(id.with("find")))?;
    if !find.open || find.query.is_empty() {
        return None;
    }
    let re = pattern(&find.query, find.case, find.word, find.regex).ok()?;
    Some((hits(&re, text), find.current))
}

/// In the editor's layouter: the find box's matches get a background, the current one
/// a stronger one.
pub fn mark(job: &mut LayoutJob, ui: &egui::Ui, id: Id, text: &str) {
    use egui::text::{ByteIndex, LayoutSection};
    let Some((hits, current)) = found(ui.ctx(), id, text) else {
        return;
    };
    let (all, now) = match ui.visuals().dark_mode {
        true => (
            Color32::from_rgba_unmultiplied(200, 160, 40, 70),
            Color32::from_rgba_unmultiplied(230, 120, 20, 160),
        ),
        false => (
            Color32::from_rgba_unmultiplied(250, 200, 60, 110),
            Color32::from_rgba_unmultiplied(245, 140, 30, 170),
        ),
    };
    let mut sections = Vec::with_capacity(job.sections.len() + hits.len() * 2);
    for s in std::mem::take(&mut job.sections) {
        let (first, end) = (s.byte_range.start.0, s.byte_range.end.0);
        let mut at = first;
        let piece = |r: Range<usize>, leading_space: f32, format: egui::TextFormat| LayoutSection {
            leading_space,
            byte_range: ByteIndex(r.start)..ByteIndex(r.end),
            format,
        };
        // The hits that overlap this section, cut to it.
        for (n, h) in hits.iter().enumerate() {
            if h.end <= at || h.start >= end {
                continue;
            }
            let (from, to) = (h.start.max(at), h.end.min(end));
            let lead = |at: usize| if at == first { s.leading_space } else { 0.0 };
            if from > at {
                sections.push(piece(at..from, lead(at), s.format.clone()));
            }
            let mut format = s.format.clone();
            format.background = if n == current { now } else { all };
            sections.push(piece(from..to, lead(from), format));
            at = to;
        }
        if at < end || first == end {
            let lead = if at == first { s.leading_space } else { 0.0 };
            sections.push(piece(at..end, lead, s.format));
        }
    }
    job.sections = sections;
}

/// After the editor `id` showed `text` (`output`): the find box over its top right,
/// replacing in `text`, and the current match brought into view. True when replacing
/// changed the text.
pub fn after(
    ui: &mut egui::Ui,
    id: Id,
    text: &mut String,
    output: &egui::text_edit::TextEditOutput,
) -> bool {
    let ctx = ui.ctx().clone();
    let Some(mut find) = ctx.data(|d| d.get_temp::<Find>(id.with("find"))) else {
        return false;
    };
    if !find.open {
        return false;
    }
    let query_id = id.with("find-query");
    let with_id = id.with("find-with");
    own(&ctx, query_id, id);
    own(&ctx, with_id, id);
    let re = pattern(&find.query, find.case, find.word, find.regex);
    let found = match (&re, find.query.is_empty()) {
        (Ok(re), false) => hits(re, text),
        _ => Vec::new(),
    };
    if find.current >= found.len() {
        find.current = 0;
    }
    let rect = output.response.response.rect.intersect(ui.clip_rect());
    let mut changed = false;
    let (mut close, mut step): (bool, isize) = (false, 0);
    egui::Area::new(id.with("find-box"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::RIGHT_TOP)
        .fixed_pos(rect.right_top() + egui::vec2(-6.0, 4.0))
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let chevron = match find.replace {
                        true => icon::CHEVRON_DOWN,
                        false => icon::CHEVRON_RIGHT,
                    };
                    if icon_button(ui, chevron, t("Toggle replace"), None)
                        .on_hover_text(tf("Toggle replace ({})", &[&replace_label(ui.ctx())]))
                        .clicked()
                    {
                        find.replace = !find.replace;
                    }
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut find.query)
                            .id(query_id)
                            .hint_text(t("Find"))
                            .desired_width(180.0),
                    );
                    let (enter, escape, shift) = ui.input(|i| {
                        (
                            i.key_pressed(Key::Enter),
                            i.key_pressed(Key::Escape),
                            i.modifiers.shift,
                        )
                    });
                    if edit.lost_focus() && enter {
                        step = if shift { -1 } else { 1 };
                        edit.request_focus();
                    }
                    if std::mem::take(&mut find.focus) {
                        edit.request_focus();
                        let mut state =
                            egui::TextEdit::load_state(ui.ctx(), query_id).unwrap_or_default();
                        let n = find.query.chars().count();
                        state.cursor.set_char_range(Some(CCursorRange::two(
                            CCursor::new(0),
                            CCursor::new(n),
                        )));
                        state.store(ui.ctx(), query_id);
                    }
                    close |= edit.lost_focus() && escape;
                    if edit.changed() {
                        (find.current, find.show) = (0, true);
                    }
                    if find_options(ui, &mut find.case, &mut find.word, &mut find.regex) {
                        (find.current, find.show) = (0, true);
                    }
                    match &re {
                        Err(e) if !find.query.is_empty() => {
                            ui.colored_label(Color32::from_rgb(200, 60, 60), t("bad pattern"))
                                .on_hover_text(e.to_string());
                        }
                        _ if find.query.is_empty() => {}
                        _ => {
                            let n = found.len();
                            let more = if n == MAX_HITS { "+" } else { "" };
                            let at = if n == 0 { 0 } else { find.current + 1 };
                            ui.weak(format!("{at}/{n}{more}"));
                        }
                    }
                    let prev = icon_button(ui, icon::ARROW_UP, t("Prev"), None)
                        .on_hover_text(tf("Previous match ({})", &[&"Shift+Enter"]));
                    let next = icon_button(ui, icon::ARROW_DOWN, t("Next"), None)
                        .on_hover_text(tf("Next match ({})", &[&"Enter"]));
                    step = match (prev.clicked(), next.clicked()) {
                        (true, _) => -1,
                        (_, true) => 1,
                        _ => step,
                    };
                    close |= icon_button(ui, icon::X, t("Close"), None)
                        .on_hover_text(t("Close (Esc)"))
                        .clicked();
                });
                if find.replace {
                    ui.horizontal(|ui| {
                        // Under the query, past the chevron.
                        let side = ui.spacing().interact_size.y;
                        ui.add_space(side + ui.spacing().item_spacing.x);
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut find.with)
                                .id(with_id)
                                .hint_text(t("Replace"))
                                .desired_width(180.0),
                        );
                        let (enter, escape) =
                            ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                        close |= edit.lost_focus() && escape;
                        let one = ui
                            .small_button(t("Replace"))
                            .on_hover_text(t("Replace this match (Enter)"))
                            .clicked()
                            || (edit.lost_focus() && enter);
                        if edit.lost_focus() && enter {
                            edit.request_focus();
                        }
                        let all = ui.small_button(t("Replace all")).clicked();
                        if let (Ok(re), false) = (&re, found.is_empty()) {
                            if all {
                                *text = replace(text, re, &found, &find.with, find.regex);
                                (changed, find.current) = (true, 0);
                            } else if one {
                                let r = found[find.current].clone();
                                *text = replace(text, re, &[r], &find.with, find.regex);
                                // The next match is now at this index.
                                (changed, find.show) = (true, true);
                            }
                        }
                    });
                }
            });
        });
    let n = found.len();
    if n > 0 && step != 0 {
        find.current = (find.current as isize + step).rem_euclid(n as isize) as usize;
        find.show = true;
    }
    if find.show && !changed && n > 0 {
        find.show = false;
        let r = &found[find.current.min(n - 1)];
        let (start, end) = (char_of(text, r.start), char_of(text, r.end));
        select(&ctx, id, (start, end));
        let top = output.galley.pos_from_cursor(CCursor::new(start));
        let bottom = output.galley.pos_from_cursor(CCursor::new(end));
        let at = top.union(bottom).translate(output.galley_pos.to_vec2());
        ui.scroll_to_rect(at, Some(egui::Align::Center));
    }
    if close {
        find.open = false;
        ctx.memory_mut(|m| m.request_focus(id));
    }
    ctx.data_mut(|d| d.insert_temp(id.with("find"), find));
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` marks the selection's ends in `with` (one bar: a cursor).
    fn sel_of(with: &str) -> (String, Sel) {
        let text = with.replace('|', "");
        let bars: Vec<usize> = with
            .chars()
            .enumerate()
            .filter(|(_, c)| *c == '|')
            .map(|(i, _)| i)
            .collect();
        let sel = match bars[..] {
            [a] => (a, a),
            [a, b] => (a, b - 1),
            _ => panic!("one or two bars: {with}"),
        };
        (text, sel)
    }

    fn shown(e: &Edit) -> String {
        let mut out: Vec<char> = e.text.chars().collect();
        out.insert(e.sel.1, '|');
        if e.sel.0 != e.sel.1 {
            out.insert(e.sel.0, '|');
        }
        out.into_iter().collect()
    }

    fn run(f: impl Fn(&str, Sel) -> Edit, with: &str) -> String {
        let (text, sel) = sel_of(with);
        shown(&f(&text, sel))
    }

    /// ⌘/ comments the lines out at their shallowest indent and back in, blank lines
    /// left alone, the cursor staying on its text.
    #[test]
    fn comment_toggles_on_the_selected_lines() {
        let c = |t: &str, s| toggle_line_comment(t, s, "//");
        assert_eq!(run(c, "a|b\nc"), "// a|b\nc");
        assert_eq!(run(c, "// a|b\nc"), "a|b\nc");
        assert_eq!(run(c, "  |x\n\n    y|\nz"), "  |// x\n\n  //   y|\nz");
        assert_eq!(run(c, "  |// x\n\n  //   y|\nz"), "  |x\n\n    y|\nz");
        // Mixed: some lines aren't, so all become comments.
        assert_eq!(run(c, "|// a\nb|"), "|// // a\n// b|");
        // A selection ending at a line's start leaves that line out.
        assert_eq!(run(c, "|a\n|b"), "|// a\n|b");
        let h = |t: &str, s| toggle_line_comment(t, s, "#");
        assert_eq!(run(h, "query { a| }"), "# query { a| }");
    }

    #[test]
    fn block_comments_wrap_and_unwrap_the_selection() {
        let b = |t: &str, s| toggle_block_comment(t, s, ("/*", "*/"));
        assert_eq!(run(b, "f(|x, y|)"), "f(/* |x, y| */)");
        assert_eq!(run(b, "f(|/* x, y */|)"), "f(|x, y|)");
    }

    /// ⌥↑/⌥↓ swap with the neighbour, ⇧⌥↑/⇧⌥↓ copy, ⇧⌘K deletes; the first and last
    /// lines can't move past the ends.
    #[test]
    fn lines_move_copy_and_go() {
        let up = |t: &str, s| move_lines(t, s, true).unwrap();
        let down = |t: &str, s| move_lines(t, s, false).unwrap();
        assert_eq!(run(up, "a\nb|b\nc"), "b|b\na\nc");
        assert_eq!(run(down, "a\nb|b\nc"), "a\nc\nb|b");
        assert_eq!(run(down, "|a\nb|\nc"), "c\n|a\nb|");
        assert!(move_lines("a\nb", (0, 0), true).is_none());
        assert!(move_lines("a\nb", (3, 3), false).is_none());
        let copy_down = |t: &str, s| copy_lines(t, s, false);
        let copy_up = |t: &str, s| copy_lines(t, s, true);
        assert_eq!(run(copy_down, "a\nb|b\nc"), "a\nbb\nb|b\nc");
        assert_eq!(run(copy_up, "a\nb|b\nc"), "a\nb|b\nbb\nc");
        assert_eq!(run(delete_lines, "a\nb|b\nc"), "a\n|c");
        assert_eq!(run(delete_lines, "a\nb|b"), "a|");
        assert_eq!(run(delete_lines, "o|nly"), "|");
    }

    /// Tab over lines indents them, ⇧Tab and ⌘[ take a level off, Tab elsewhere goes to
    /// the next stop with spaces; the unit is what the text already uses.
    #[test]
    fn indenting_follows_the_texts_own_unit() {
        assert_eq!(unit("{\n  \"a\": 1\n}", Some(Lang::Json)), "  ");
        assert_eq!(unit("if (x) {\n\ty();\n}", None), "\t");
        assert_eq!(unit("x();", Some(Lang::JavaScript)), "    ");
        let ind = |t: &str, s| indent(t, s, "  ");
        assert_eq!(run(ind, "|a\n\nb|"), "|  a\n\n  b|");
        let out = |t: &str, s| outdent(t, s, "  ");
        assert_eq!(run(out, "   |a\n b|"), " |a\nb|");
        let tb = |t: &str, s| tab(t, s, "    ");
        assert_eq!(run(tb, "ab|"), "ab  |");
        assert_eq!(run(tb, "|x"), "    |x");
    }

    /// Enter keeps the line's indent, goes a level deeper after `{`, and between `{}`
    /// puts the `}` on its own line.
    #[test]
    fn enter_keeps_the_indent() {
        let nl = |t: &str, s| newline(t, s, "  ");
        assert_eq!(run(nl, "  a|"), "  a\n  |");
        assert_eq!(run(nl, "  f({|"), "  f({\n    |");
        assert_eq!(run(nl, "x = {|}"), "x = {\n  |\n}");
        assert_eq!(run(nl, "a|  b"), "a\n|b");
    }

    /// ⌘C with nothing selected takes the line and its newline.
    #[test]
    fn a_bare_cursor_copies_its_line() {
        assert_eq!(&"a\nbc\nd"[line_span("a\nbc\nd", 3)], "bc\n");
        assert_eq!(&"a\nbc"[line_span("a\nbc", 3)], "bc");
    }

    /// Replace expands `$1` only for a regular expression, and skips empty matches.
    #[test]
    fn replace_expands_groups_in_regex_mode() {
        let re = pattern(r"(\w+)@x", true, false, true).unwrap();
        let text = "a@x b@x";
        let found = hits(&re, text);
        assert_eq!(replace(text, &re, &found, "<$1>", true), "<a> <b>");
        let re = pattern("$1", true, false, false).unwrap();
        assert_eq!(
            replace("a $1", &re, &hits(&re, "a $1"), "$x", false),
            "a $x"
        );
        let re = pattern("a*", true, false, true).unwrap();
        assert_eq!(&"baab"[hits(&re, "baab")[0].clone()], "aa");
    }

    /// Format: JSON as Beautify does, JavaScript (and TypeScript) by js-beautify at the
    /// text's indent, `{{vars}}` kept whole.
    #[test]
    fn format_lays_out_json_and_scripts() {
        assert_eq!(
            format("{\"a\":[1]}", Some(Lang::Json), "  ").unwrap(),
            "{\n  \"a\": [\n    1\n  ]\n}"
        );
        let js = format(
            "const x=(a:number)=>{if(a){return \"{{v}}\"+{{n}}}};",
            Some(Lang::JavaScript),
            "  ",
        )
        .unwrap();
        assert_eq!(
            js,
            "const x = (a: number) => {\n  if (a) {\n    return \"{{v}}\" + {{n}}\n  }\n};"
        );
        assert!(format("x", None, "  ").is_err());
    }
}
