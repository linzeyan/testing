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

pub fn is_known(name: &str, vars: &HashMap<String, String>) -> bool {
    vars.contains_key(name) || DYNAMIC.iter().any(|(n, _)| *n == name)
}

/// `configure` adds hint text, width, rows… to the TextEdit; `id` must be stable across
/// frames because the autocomplete state is keyed by it.
pub fn var_edit(
    ui: &mut egui::Ui,
    id: Id,
    text: &mut String,
    vars: &HashMap<String, String>,
    style: TextStyle,
    multiline: bool,
    configure: impl FnOnce(egui::TextEdit<'_>) -> egui::TextEdit<'_>,
) -> egui::Response {
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
    let mut suggestions = Vec::new();
    if let Some((start, _, prefix)) = &context {
        if popup.start != *start {
            popup = Popup {
                start: *start,
                ..Default::default()
            };
        }
        if !popup.dismissed {
            suggestions = suggest(prefix, vars);
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

    if accept
        && showing
        && let Some((start, cursor_byte, _)) = context
    {
        let index = insert(text, start, cursor_byte, &suggestions[popup.selected].0);
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

/// Defined variables first, then dynamic ones; prefix matches rank above substring matches.
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
        (!lower.starts_with(&prefix), n.starts_with('$'), lower)
    });
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
    }
}
