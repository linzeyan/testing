//! Text fields that understand `{{variables}}`: defined names are green, undefined red,
//! hovering shows the resolved text, and typing `{{` opens an autocomplete list
//! (↑/↓ to pick, Enter or click to insert, Esc to dismiss). Right-click edits the
//! selection or stores it as a variable.

use std::collections::HashMap;

use eframe::egui::text::{CCursor, CCursorRange, LayoutJob};
use eframe::egui::{self, Color32, FontId, Id, Key, Modifiers, PointerButton, RichText, TextStyle};

use crate::i18n::{t, tf};
use crate::model::{self, DYNAMIC};
use crate::syntax::{self, Lang};

// As app.rs GREEN: readable on both themes.
pub const DEFINED: Color32 = Color32::from_rgb(40, 150, 70);
pub const UNDEFINED: Color32 = Color32::from_rgb(220, 80, 80);
const MAX_SUGGESTIONS: usize = 8;

/// What completes besides `{{variables}}`.
pub enum Complete<'a> {
    None,
    /// The whole field, from these words with their descriptions (header names).
    Words(&'a [(&'a str, &'static str)]),
    /// The word at the cursor: given the text and the cursor's byte offset, where that
    /// word starts and what can go there, with a description each (GraphQL fields).
    At(&'a CompleteAt<'a>),
}

pub type CompleteAt<'a> = dyn Fn(&str, usize) -> Option<(usize, Vec<(String, String)>)> + 'a;

/// Where the right-click menu can store a selection: the active environment, if any, then
/// the globals (`None`), each with its variable names.
pub type Scopes = Vec<(Option<String>, Vec<String>)>;

/// What the right-click menu leaves for the window, which owns the environments and the
/// status bar.
#[derive(Clone)]
pub enum Picked {
    /// Store `value` as `name` in `env` (`None`: the globals); without a name, ask for one.
    Set {
        env: Option<String>,
        name: Option<String>,
        value: String,
    },
    Status(String),
}

const SCOPES: &str = "var-scopes";
const PICKED: &str = "var-picked";

/// Set each frame before the fields are drawn: their menus can't see the window's state.
pub fn offer(ctx: &egui::Context, scopes: Scopes) {
    ctx.data_mut(|d| d.insert_temp(Id::new(SCOPES), scopes));
}

pub fn picked(ctx: &egui::Context) -> Option<Picked> {
    // In an Option: `remove_temp` wants a Default.
    ctx.data_mut(|d| d.remove_temp::<Option<Picked>>(Id::new(PICKED)))
        .flatten()
}

#[derive(Clone, Copy, Default)]
struct Popup {
    /// Byte offset of the `{{` or word being completed.
    start: usize,
    selected: usize,
    visible: bool,
    dismissed: bool,
}

/// Text past this gets no editor: egui lays out every character at about 300 bytes each
/// (a 10 MB body took 3.6 GB), which a machine with under 1 GB free can't hold.
pub const MAX_EDIT: usize = 128 * 1024;

/// For text too big to edit: a note with its start and a Clear button, in place of the
/// editor. A paste that would make it too big is taken here, before the editor lays it
/// out. `None` when the text can be edited as usual.
pub fn too_big(ui: &mut egui::Ui, id: Id, text: &mut String) -> Option<egui::Response> {
    let mut pasted = false;
    if ui.memory(|m| m.has_focus(id)) {
        let paste = ui.input_mut(|i| {
            let big = |e: &egui::Event| matches!(e, egui::Event::Paste(s) if text.len() + s.len() > MAX_EDIT);
            let at = i.events.iter().position(big)?;
            match i.events.remove(at) {
                egui::Event::Paste(s) => Some(s),
                _ => None,
            }
        });
        if let Some(s) = paste {
            let chars = egui::TextEdit::load_state(ui.ctx(), id)
                .and_then(|s| s.cursor.char_range())
                .map(|r| r.as_sorted_char_range());
            let byte = |c: usize| text.char_indices().nth(c).map_or(text.len(), |(b, _)| b);
            let range = chars.map_or(text.len()..text.len(), |r| byte(r.start.0)..byte(r.end.0));
            text.replace_range(range, &s);
            pasted = true;
        }
    }
    if text.len() <= MAX_EDIT {
        return None;
    }
    let size = match text.len() {
        n if n < 1 << 20 => format!("{} KB", n >> 10),
        n => format!("{:.1} MB", n as f64 / 1_048_576.0),
    };
    let start: String = text.chars().take(300).collect();
    let mut response = ui
        .group(|ui| {
            ui.label(format!(
                "{size}: too big to edit here, laying it out would take more memory than \
                 this machine may have. It's kept and sent as it is."
            ));
            ui.label(
                RichText::new(format!("{}…", start.trim_end()))
                    .monospace()
                    .weak(),
            );
            ui.button(t("Clear"))
        })
        .inner;
    if response.clicked() {
        text.clear();
        response.mark_changed();
    }
    if pasted {
        response.mark_changed();
    }
    Some(response)
}

pub fn is_known(name: &str, vars: &HashMap<String, String>) -> bool {
    vars.contains_key(name) || DYNAMIC.iter().any(|(n, _)| *n == name)
}

/// `configure` adds hint text, width, rows… to the TextEdit; `id` must be stable across
/// frames because the autocomplete state is keyed by it. `complete` says what completes
/// outside `{{`. `lang` colours the text as code under the `{{var}}`s.
// A builder for one function would be more code than the long argument list.
#[allow(clippy::too_many_arguments)]
pub fn var_edit(
    ui: &mut egui::Ui,
    id: Id,
    text: &mut String,
    vars: &HashMap<String, String>,
    style: TextStyle,
    multiline: bool,
    lang: Option<Lang>,
    complete: Complete<'_>,
    configure: impl FnOnce(egui::TextEdit<'_>) -> egui::TextEdit<'_>,
) -> egui::Response {
    if let Some(response) = too_big(ui, id, text) {
        return response;
    }
    let popup_id = id.with("vars-popup");
    let focused_before = ui.memory(|m| m.has_focus(id));
    let mut popup: Popup = ui.data(|d| d.get_temp(popup_id)).unwrap_or_default();
    // Keys must be taken before the TextEdit sees them, or Enter would end editing.
    let mut accept = false;
    if focused_before && popup.visible {
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                popup.selected += 1;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                popup.selected = popup.selected.saturating_sub(1);
            }
            accept = i.consume_key(Modifiers::NONE, Key::Enter);
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                popup.dismissed = true;
            }
        });
    }

    let font = style.resolve(ui.style());
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = highlight(buf.as_str(), vars, &font, lang, ui.visuals());
        job.wrap.max_width = wrap_width;
        ui.painter().layout_job(job)
    };
    let right_click = ui.input(|i| i.pointer.button_pressed(PointerButton::Secondary));
    let selected = right_click
        .then(|| {
            egui::TextEdit::load_state(ui.ctx(), id)?
                .cursor
                .char_range()
        })
        .flatten()
        .filter(|r| !r.is_empty());
    let edit = if multiline {
        egui::TextEdit::multiline(text)
    } else {
        egui::TextEdit::singleline(text)
    };
    let output = configure(edit.id(id).layouter(&mut layouter)).show(ui);
    let mut response = output.response.response;
    let focused = focused_before || response.has_focus();

    // egui moves the cursor to a right-click as to a left one, dropping the selection the
    // menu is for. It's put back, and kept for the menu: a click on a menu item takes focus
    // from the field, and egui collapses an unfocused field's selection.
    let kept = id.with("menu-selection");
    if right_click && response.hovered() {
        if let Some(range) = selected {
            let mut state = output.state.clone();
            state.cursor.set_char_range(Some(range));
            state.store(ui.ctx(), id);
        }
        let chars = selected
            .map(|r| r.as_sorted_char_range())
            .map(|r| [r.start.0, r.end.0]);
        ui.data_mut(|d| d.insert_temp(kept, chars));
    }

    let cursor = output
        .cursor_range
        .filter(|r| r.is_empty())
        .map(|r| r.primary.index.0);
    let context = cursor
        .filter(|_| focused)
        .and_then(|c| completion_context(text, c));
    // Outside a `{{`: the word at the cursor, as (its start, the cursor, what can go there).
    let word = match (&context, &complete, cursor) {
        (None, Complete::At(at), Some(c)) if focused => {
            let byte = text.char_indices().nth(c).map_or(text.len(), |(i, _)| i);
            at(text, byte).map(|(start, mut list)| {
                list.truncate(MAX_SUGGESTIONS);
                (start, byte, list)
            })
        }
        _ => None,
    };
    // Else the field as a whole completes from `words`.
    let start = match (&context, &word, &complete) {
        (Some((start, ..)), ..) | (None, Some((start, ..)), _) => Some(*start),
        (None, None, Complete::Words(w)) if focused && cursor.is_some() && !w.is_empty() => {
            Some(usize::MAX)
        }
        _ => None,
    };
    let mut suggestions = Vec::new();
    if let Some(start) = start {
        if popup.start != start {
            popup = Popup {
                start,
                ..Default::default()
            };
        }
        if !popup.dismissed {
            suggestions = match (&context, &word, &complete) {
                (Some((_, _, prefix)), ..) => suggest(prefix, vars),
                (None, Some((_, _, list)), _) => list.clone(),
                (None, None, Complete::Words(w)) => suggest_words(text, w),
                _ => Vec::new(),
            };
        }
    }
    let showing = !suggestions.is_empty();
    popup.selected = popup.selected.min(suggestions.len().saturating_sub(1));

    // A word mid-text gets its list under the cursor; a whole field under the field.
    let below = match (&word, &output.cursor_range) {
        (Some(_), Some(r)) => {
            let at = output.galley.pos_from_cursor(r.primary);
            output.galley_pos + at.left_bottom().to_vec2()
        }
        _ => response.rect.left_bottom(),
    };
    if showing {
        egui::Area::new(popup_id)
            .order(egui::Order::Foreground)
            .fixed_pos(below)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(240.0);
                    for (i, (name, value)) in suggestions.iter().enumerate() {
                        let item = ui.horizontal(|ui| {
                            let r = ui.selectable_label(
                                i == popup.selected,
                                RichText::new(name).monospace(),
                            );
                            ui.weak(clip(value, 40));
                            r
                        });
                        if item.inner.clicked() {
                            popup.selected = i;
                            accept = true;
                        }
                    }
                });
            });
    }

    if accept && showing {
        let pick = &suggestions[popup.selected].0;
        let index = match (context, word) {
            (Some((start, cursor_byte, _)), _) => insert(text, start, cursor_byte, pick),
            (None, Some((start, cursor_byte, _))) => {
                text.replace_range(start..cursor_byte, pick);
                text[..start + pick.len()].chars().count()
            }
            (None, None) => {
                pick.clone_into(text);
                text.chars().count()
            }
        };
        let mut state = output.state;
        state
            .cursor
            .set_char_range(Some(CCursorRange::one(CCursor::new(index))));
        state.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
        response.mark_changed();
        popup = Popup::default();
    } else {
        popup.visible = showing;
    }
    if focused {
        ui.data_mut(|d| d.insert_temp(popup_id, popup));
    } else {
        ui.data_mut(|d| d.remove::<Popup>(popup_id));
    }

    let mut edited = false;
    response.context_menu(|ui| {
        let chars = ui.data(|d| d.get_temp(kept)).flatten();
        edited = selection_menu(ui, id, text, chars);
    });
    if edited {
        response.mark_changed();
    }

    // A URL's preview also fills its path variables, which only the caller has.
    if !focused && text.contains("{{") && lang != Some(Lang::Url) {
        response = response.on_hover_ui(|ui| {
            let mut missing = Vec::new();
            let resolved = model::resolve(text, vars, &mut missing);
            missing.retain(|n| !n.starts_with('?'));
            ui.label(RichText::new(clip(&resolved, 400)).monospace());
            if !missing.is_empty() {
                ui.colored_label(UNDEFINED, format!("Undefined: {}", missing.join(", ")));
            }
        });
    }
    response
}

/// The right-click menu, over the selection as char indices (start, end). Returns whether
/// the text changed.
fn selection_menu(ui: &mut egui::Ui, id: Id, text: &mut String, chars: Option<[usize; 2]>) -> bool {
    let byte = |c: usize| text.char_indices().nth(c).map_or(text.len(), |(b, _)| b);
    let bytes = chars.map(|[a, b]| byte(a)..byte(b));
    let selected = bytes.clone().map(|r| text[r].to_owned());
    let has = selected.is_some();
    let ctx = ui.ctx().clone();
    // Back in the field with `chars` selected, as a native menu leaves it.
    let refocus = |chars: Option<[usize; 2]>| {
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
        if let Some([a, b]) = chars {
            let range = CCursorRange::two(CCursor::new(a), CCursor::new(b));
            state.cursor.set_char_range(Some(range));
        }
        state.store(&ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
        ctx.request_repaint();
    };
    let cut = ui.add_enabled(has, egui::Button::new(t("Cut"))).clicked();
    let copy = ui.add_enabled(has, egui::Button::new(t("Copy"))).clicked();
    // Only the platform reads the clipboard: it answers with a paste event, which the
    // field, focused again with the selection, takes like Ctrl+V.
    if ui.button(t("Paste")).clicked() {
        refocus(chars);
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
    }
    if ui.button(t("Select all")).clicked() {
        refocus(Some([0, text.chars().count()]));
    }
    ui.separator();
    let encode = (ui.add_enabled(has, egui::Button::new(t("Encode URI component")))).clicked();
    let decode = (ui.add_enabled(has, egui::Button::new(t("Decode URI component")))).clicked();
    let scopes: Scopes = ctx
        .data(|d| d.get_temp(Id::new(SCOPES)))
        .unwrap_or_default();
    if !scopes.is_empty() {
        ui.separator();
    }
    // Like Postman, the selection stays as it is rather than becoming `{{name}}`: the same
    // action from a read-only response can't replace anything, and the value is often
    // wanted as is (a token for other requests) without this request turning unsaved.
    for (env, names) in scopes {
        let scope = env.as_deref().unwrap_or(t("Globals"));
        let mut name = None;
        ui.add_enabled_ui(has, |ui| {
            ui.menu_button(tf("Set as variable in {}", &[&scope]), |ui| {
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .show(ui, |ui| {
                        for n in &names {
                            if ui.button(n.as_str()).clicked() {
                                name = Some(Some(n.clone()));
                            }
                        }
                    });
                if !names.is_empty() {
                    ui.separator();
                }
                if ui.button(t("New variable…")).clicked() {
                    name = Some(None);
                }
            })
        });
        if let (Some(name), Some(value)) = (name, selected.clone()) {
            hand(&ctx, Picked::Set { env, name, value });
        }
    }

    let Some(selected) = selected else {
        return false;
    };
    if cut || copy {
        ctx.copy_text(selected.clone());
    }
    let new = if cut {
        String::new()
    } else if encode {
        encode_component(&selected)
    } else if let Some(decoded) = decode.then(|| decode_component(&selected)) {
        let Some(decoded) = decoded else {
            let why = t("Not decoded: the selection isn't valid URI encoding");
            hand(&ctx, Picked::Status(why.into()));
            return false;
        };
        decoded
    } else {
        if copy {
            refocus(chars);
        }
        return false;
    };
    let (Some([start, _]), Some(bytes)) = (chars, bytes) else {
        return false;
    };
    text.replace_range(bytes, &new);
    // The replacement stays selected, so a decode can be undone by an encode.
    refocus(Some([start, start + new.chars().count()]));
    true
}

fn hand(ctx: &egui::Context, picked: Picked) {
    ctx.data_mut(|d| d.insert_temp(Id::new(PICKED), Some(picked)));
}

/// As JavaScript's encodeURIComponent: UTF-8 bytes as `%XX`, except letters, digits and
/// `-_.!~*'()`. (sigv4's `uri_encode` is RFC 3986's set, which escapes `!*'()` too.)
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(b as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// As JavaScript's decodeURIComponent, which throws where this is `None`: a `%` without
/// two hex digits after it, or bytes that aren't UTF-8. `+` stays `+`.
fn decode_component(s: &str) -> Option<String> {
    let hex = |c: Option<u8>| (c? as char).to_digit(16);
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'%' => (hex(bytes.next())? * 16 + hex(bytes.next())?) as u8,
            b => b,
        });
    }
    String::from_utf8(out).ok()
}

fn highlight(
    text: &str,
    vars: &HashMap<String, String>,
    font: &FontId,
    lang: Option<Lang>,
    visuals: &egui::Visuals,
) -> LayoutJob {
    let mut colors = vec![visuals.text_color(); text.len()];
    if let Some(lang) = lang {
        syntax::paint(&mut colors, text, lang, visuals);
    }
    // Variables over the code: `"{{token}}"` reads as a variable, not as a string.
    let mut at = 0;
    while let Some(open) = text[at..].find("{{").map(|n| at + n) {
        let Some(close) = text[open + 2..].find("}}").map(|n| open + 2 + n) else {
            break;
        };
        let color = if is_known(text[open + 2..close].trim(), vars) {
            DEFINED
        } else {
            UNDEFINED
        };
        colors[open..close + 2].fill(color);
        at = close + 2;
    }
    syntax::job(text, &colors, font)
}

/// When the cursor (a char index) sits inside an unfinished `{{name`, returns
/// (byte offset of `{{`, byte offset of the cursor, typed prefix).
fn completion_context(text: &str, cursor: usize) -> Option<(usize, usize, String)> {
    let byte = text
        .char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i);
    let before = &text[..byte];
    let open = before.rfind("{{")?;
    let prefix = &before[open + 2..];
    let valid = prefix
        .chars()
        .all(|c| c.is_alphanumeric() || "_-.$".contains(c));
    valid.then(|| (open, byte, prefix.to_owned()))
}

/// Defined variables first, then dynamic ones; within each, prefix matches rank above
/// substring matches.
fn suggest(prefix: &str, vars: &HashMap<String, String>) -> Vec<(String, String)> {
    let prefix = prefix.to_lowercase();
    let mut out: Vec<(String, String)> = vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .chain(DYNAMIC.iter().map(|(n, d)| (n.to_string(), d.to_string())))
        .filter(|(n, _)| n.to_lowercase().contains(&prefix))
        .collect();
    out.sort_by_key(|(n, _)| {
        let lower = n.to_lowercase();
        // With ~120 dynamic names nearly any letter is a substring hit, so `{{email` has
        // to reach `$randomEmail` as a prefix match or it falls past the cut.
        let bare = lower.trim_start_matches('$');
        let starts = [&lower[..], bare, bare.trim_start_matches("random")]
            .iter()
            .any(|w| w.starts_with(&prefix));
        // The user's own names come first: one of them is what's meant far more often.
        (n.starts_with('$'), !starts, lower)
    });
    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Words containing what's typed, those starting with it first, else in list order. Nothing
/// for an empty field (the list would cover the rows below) or a finished word.
fn suggest_words(text: &str, words: &[(&str, &'static str)]) -> Vec<(String, String)> {
    let typed = text.trim().to_lowercase();
    if typed.is_empty() || words.iter().any(|(w, _)| w.to_lowercase() == typed) {
        return Vec::new();
    }
    let mut out: Vec<(String, String)> = words
        .iter()
        .filter(|(w, _)| w.to_lowercase().contains(&typed))
        .map(|(w, d)| (w.to_string(), t(d).to_owned()))
        .collect();
    out.sort_by_key(|(w, _)| !w.to_lowercase().starts_with(&typed));
    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Replaces the typed prefix with `name}}` (reusing a `}}` already after the cursor) and
/// returns the char index just past the closing braces.
fn insert(text: &mut String, open: usize, cursor: usize, name: &str) -> usize {
    let close = if text[cursor..].starts_with("}}") {
        ""
    } else {
        "}}"
    };
    text.replace_range(open + 2..cursor, &format!("{name}{close}"));
    text[..open + 2 + name.len() + 2].chars().count()
}

pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_triggers_only_inside_an_open_placeholder() {
        let text = "https://{{ho";
        assert_eq!(
            completion_context(text, text.chars().count()),
            Some((8, 12, "ho".into()))
        );
        assert_eq!(completion_context("{{host}}/x", 10), None);
        assert_eq!(completion_context("{{a b", 5), None);
        // Char index, not byte index: CJK before the placeholder must not shift it.
        assert_eq!(
            completion_context("名稱{{to", 6),
            Some((6, 10, "to".into()))
        );
    }

    /// A variable inside a JSON string shows as a variable, defined or not, with the
    /// string's colour on either side of it; an unclosed `{{` is just text.
    #[test]
    fn variables_are_coloured_over_the_code() {
        let vars = HashMap::from([("v".to_owned(), "1".to_owned())]);
        let visuals = egui::Visuals::dark();
        let text = r#"{"a":"x{{v}}y{{w}}","b":"{{"}"#;
        let job = highlight(
            text,
            &vars,
            &FontId::monospace(12.0),
            Some(Lang::Json),
            &visuals,
        );
        let runs: Vec<(&str, Color32)> = (job.sections.iter())
            .map(|s| {
                (
                    &text[s.byte_range.start.0..s.byte_range.end.0],
                    s.format.color,
                )
            })
            .collect();
        let str = syntax::color(syntax::Kind::Str, &visuals);
        let key = syntax::color(syntax::Kind::Key, &visuals);
        assert_eq!(
            runs[1..8],
            [
                ("\"a\"", key),
                (":", visuals.weak_text_color()),
                ("\"x", str),
                ("{{v}}", DEFINED),
                ("y", str),
                ("{{w}}", UNDEFINED),
                ("\"", str),
            ]
        );
        assert_eq!(runs[runs.len() - 2], ("\"{{\"", str));
    }

    /// What a browser's encodeURIComponent/decodeURIComponent give, so a value encoded here
    /// is what a server or a pm.* script expects; where JavaScript throws, nothing changes.
    #[test]
    fn uri_components_encode_and_decode_as_javascript_does() {
        let plain = "a b&c/d?=#+:@,;$é€😀-_.!~*'()";
        let encoded =
            "a%20b%26c%2Fd%3F%3D%23%2B%3A%40%2C%3B%24%C3%A9%E2%82%AC%F0%9F%98%80-_.!~*'()";
        assert_eq!(encode_component(plain), encoded);
        assert_eq!(decode_component(encoded).as_deref(), Some(plain));
        // Lower-case hex, reserved characters and a literal `+` decode as JavaScript does.
        assert_eq!(decode_component("%c3%a9%2F+").as_deref(), Some("é/+"));
        for bad in ["%", "50%", "%2", "%zz", "%+1", "%E2%82", "%C3%28"] {
            assert_eq!(decode_component(bad), None, "{bad}");
        }
    }

    #[test]
    fn insert_completes_the_name_and_reuses_closing_braces() {
        let mut t = "x/{{ho/y".to_owned();
        let at = insert(&mut t, 2, 6, "host");
        assert_eq!((t.as_str(), at), ("x/{{host}}/y", 10));
        let mut t = "{{ho}}".to_owned();
        assert_eq!(insert(&mut t, 0, 4, "host"), 8);
        assert_eq!(t, "{{host}}");
    }

    #[test]
    fn suggestions_rank_defined_prefix_matches_first() {
        let vars = HashMap::from([
            ("token".to_owned(), "abc".to_owned()),
            ("authToken".to_owned(), "x".to_owned()),
        ]);
        let names: Vec<_> = suggest("t", &vars).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names[..2], ["token".to_owned(), "authToken".to_owned()]);
        assert!(names.contains(&"$timestamp".to_owned()));
        let names: Vec<_> = suggest("email", &vars)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names[0], "$randomEmail", "{names:?}");
    }

    #[test]
    fn words_complete_by_substring_and_go_quiet_once_typed_out() {
        let words = [("Accept", ""), ("X-Content-Id", ""), ("Content-Type", "")];
        let names = |t| -> Vec<_> {
            suggest_words(t, &words)
                .into_iter()
                .map(|(w, _)| w)
                .collect()
        };
        assert_eq!(names("content"), ["Content-Type", "X-Content-Id"]);
        assert_eq!(names("type"), ["Content-Type"]);
        assert!(names("").is_empty());
        assert!(
            names("content-type").is_empty(),
            "a finished word needs no list"
        );
    }
}
