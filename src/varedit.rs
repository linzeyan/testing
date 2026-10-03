//! Text fields that understand `{{variables}}`: defined names are green, undefined red,
//! hovering shows the resolved text, and typing `{{` opens an autocomplete list
//! (↑/↓ to pick, Enter or click to insert, Esc to dismiss).

use std::collections::HashMap;

use eframe::egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use eframe::egui::{self, Color32, FontId, Id, Key, Modifiers, RichText, TextStyle};

use crate::model::{self, DYNAMIC};

pub const DEFINED: Color32 = Color32::from_rgb(80, 180, 100);
pub const UNDEFINED: Color32 = Color32::from_rgb(220, 80, 80);
const MAX_SUGGESTIONS: usize = 8;

#[derive(Clone, Copy, Default)]
struct Popup {
    /// Byte offset of the `{{` being completed.
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
            ui.button("Clear")
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
/// frames because the autocomplete state is keyed by it. `words` complete the whole field
/// outside `{{` (header names).
// A builder for one function would be more code than the long argument list.
#[allow(clippy::too_many_arguments)]
pub fn var_edit(
    ui: &mut egui::Ui,
    id: Id,
    text: &mut String,
    vars: &HashMap<String, String>,
    style: TextStyle,
    multiline: bool,
    words: &[(&str, &str)],
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
    let normal = ui.visuals().text_color();
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = highlight(buf.as_str(), vars, &font, normal);
        job.wrap.max_width = wrap_width;
        ui.painter().layout_job(job)
    };
    let edit = if multiline {
        egui::TextEdit::multiline(text)
    } else {
        egui::TextEdit::singleline(text)
    };
    let output = configure(edit.id(id).layouter(&mut layouter)).show(ui);
    let mut response = output.response.response;
    let focused = focused_before || response.has_focus();

    let cursor = output
        .cursor_range
        .filter(|r| r.is_empty())
        .map(|r| r.primary.index.0);
    let context = cursor
        .filter(|_| focused)
        .and_then(|c| completion_context(text, c));
    // Outside a `{{`, the field as a whole completes from `words`.
    let start = match &context {
        Some((start, ..)) => Some(*start),
        None if focused && cursor.is_some() && !words.is_empty() => Some(usize::MAX),
        None => None,
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
            suggestions = match &context {
                Some((_, _, prefix)) => suggest(prefix, vars),
                None => suggest_words(text, words),
            };
        }
    }
    let showing = !suggestions.is_empty();
    popup.selected = popup.selected.min(suggestions.len().saturating_sub(1));

    if showing {
        egui::Area::new(popup_id)
            .order(egui::Order::Foreground)
            .fixed_pos(response.rect.left_bottom())
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
        let index = match context {
            Some((start, cursor_byte, _)) => insert(text, start, cursor_byte, pick),
            None => {
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

    if !focused && text.contains("{{") {
        response = response.on_hover_ui(|ui| {
            let mut missing = Vec::new();
            let resolved = model::resolve(text, vars, &mut missing);
            ui.label(RichText::new(clip(&resolved, 400)).monospace());
            if !missing.is_empty() {
                ui.colored_label(UNDEFINED, format!("Undefined: {}", missing.join(", ")));
            }
        });
    }
    response
}

fn highlight(
    text: &str,
    vars: &HashMap<String, String>,
    font: &FontId,
    normal: Color32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let format = |color| TextFormat::simple(font.clone(), color);
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        job.append(&rest[..open], 0.0, format(normal));
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        let color = if is_known(after[..close].trim(), vars) {
            DEFINED
        } else {
            UNDEFINED
        };
        job.append(&rest[open..open + close + 4], 0.0, format(color));
        rest = &after[close + 2..];
    }
    job.append(rest, 0.0, format(normal));
    job
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
fn suggest_words(text: &str, words: &[(&str, &str)]) -> Vec<(String, String)> {
    let typed = text.trim().to_lowercase();
    if typed.is_empty() || words.iter().any(|(w, _)| w.to_lowercase() == typed) {
        return Vec::new();
    }
    let mut out: Vec<(String, String)> = words
        .iter()
        .filter(|(w, _)| w.to_lowercase().contains(&typed))
        .map(|(w, d)| (w.to_string(), d.to_string()))
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
