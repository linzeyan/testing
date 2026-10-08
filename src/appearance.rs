//! How the app looks, chosen in Settings: theme, fonts and text size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui::{self, FontFamily};
use resvg::usvg::fontdb;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
    /// A system font family for the interface; empty: the built-in one.
    pub ui_font: String,
    /// For bodies, scripts and snippets; empty: the built-in monospace.
    pub code_font: String,
    /// Body text in points; headings and small text keep their proportion to it.
    pub size: f32,
    pub language: crate::i18n::Lang,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            ui_font: String::new(),
            code_font: String::new(),
            size: DEFAULT_SIZE,
            language: crate::i18n::Lang::English,
        }
    }
}

/// egui's own body size.
pub const DEFAULT_SIZE: f32 = 13.0;
pub const SIZES: std::ops::RangeInclusive<f32> = 9.0..=24.0;

/// Corners of buttons, and of what egui rounds with them (fields, tabs, rows, checkboxes):
/// egui's 2 px read as square. Rounder makes a 14 px checkbox look like a radio button.
const RADIUS: u8 = 4;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Err names a chosen font that isn't installed; everything else still applies.
pub fn apply(ctx: &egui::Context, a: &Appearance) -> Result<(), String> {
    ctx.set_theme(match a.theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
    // On Windows `title_bar` does it: winit's way leaves the title bar stale.
    ctx.options_mut(|o| o.sync_window_theme = !cfg!(windows));
    let base = egui::Style::default().text_styles;
    let scale = a.size.clamp(*SIZES.start(), *SIZES.end()) / DEFAULT_SIZE;
    ctx.all_styles_mut(|style| {
        for (text_style, font) in &mut style.text_styles {
            if let Some(b) = base.get(text_style) {
                font.size = b.size * scale;
            }
        }
        let w = &mut style.visuals.widgets;
        for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            v.corner_radius = egui::CornerRadius::same(RADIUS);
        }
    });
    let mut fonts = egui::FontDefinitions::default();
    // Before the chosen fonts: it goes second in its family, and should stay behind egui's
    // own text font. Its code points are private-use, so it can't shadow text either way.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // Its glyphs are drawn for 16 px and read small beside 13 px text.
    if let Some(data) = fonts.font_data.get_mut("phosphor") {
        let data = Arc::make_mut(data);
        data.tweak.scale = 1.25;
        data.tweak.y_offset_factor = 0.08;
    }
    // Method badges draw icons in monospace rows.
    let mono = fonts.families.entry(FontFamily::Monospace).or_default();
    mono.push("phosphor".into());
    let mut missing = Vec::new();
    for (name, family) in [
        (&a.ui_font, FontFamily::Proportional),
        (&a.code_font, FontFamily::Monospace),
    ] {
        if name.is_empty() {
            continue;
        }
        match system_font(name) {
            Some(data) => {
                fonts.font_data.insert(name.clone(), Arc::new(data));
                // First, so it draws what it has; the built-in fonts fill in the rest.
                let list = fonts.families.entry(family).or_default();
                list.insert(0, name.clone());
            }
            None => missing.push(format!("Font \"{name}\" isn't installed")),
        }
    }
    // egui's fonts have no CJK glyphs: the OS's go last, behind whatever was chosen.
    if let Some((_, bytes)) = cjk() {
        let data = egui::FontData::from_static(bytes);
        fonts.font_data.insert("cjk".into(), Arc::new(data));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".into());
        }
    }
    ctx.set_fonts(fonts);
    match missing.is_empty() {
        true => Ok(()),
        false => Err(missing.join("; ")),
    }
}

/// Windows draws the title bar itself, and winit darkens it through an undocumented call
/// that shows only once the frame is repainted: white at start, black after a move (its
/// buttons gone meanwhile), a theme switch only after a focus change. The documented
/// attribute plus a caption repaint switches it at once.
#[cfg(windows)]
pub fn title_bar(frame: &eframe::Frame, dark: bool) {
    use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, GetForegroundWindow, WM_NCACTIVATE,
    };
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };
    let hwnd = h.hwnd.get() as HWND;
    let on = i32::from(dark);
    // SAFETY: eframe's live window, and a BOOL-sized value as the attribute expects.
    unsafe {
        let size = size_of::<i32>() as u32;
        let attr = DWMWA_USE_IMMERSIVE_DARK_MODE as u32;
        DwmSetWindowAttribute(hwnd, attr, (&raw const on).cast(), size);
        // Flipping the caption's active state redraws it; it ends as it was.
        let active = GetForegroundWindow() == hwnd;
        DefWindowProcW(hwnd, WM_NCACTIVATE, usize::from(!active), 0);
        DefWindowProcW(hwnd, WM_NCACTIVATE, usize::from(active), 0);
    }
}

/// Elsewhere egui's `sync_window_theme` keeps the title bar in step.
#[cfg(not(windows))]
pub fn title_bar(_: &eframe::Frame, _: bool) {}

/// The OS font that gives CJK text its glyphs, and where it is.
pub fn cjk() -> Option<(&'static str, &'static [u8])> {
    static CJK: OnceLock<Option<(&'static str, &'static [u8])>> = OnceLock::new();
    *CJK.get_or_init(|| {
        const CANDIDATES: &[&str] = &[
            r"C:\Windows\Fonts\msjh.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ];
        (CANDIDATES.iter()).find_map(|p| Some((*p, mapped(Path::new(p))?)))
    })
}

/// The installed font families, sorted, each with whether it is monospaced.
pub fn families() -> Vec<(String, bool)> {
    let mut all: HashMap<&str, bool> = HashMap::new();
    for face in system().faces() {
        if let Some((name, _)) = face.families.first() {
            *all.entry(name).or_default() |= face.monospaced;
        }
    }
    let mut list: Vec<(String, bool)> = all.into_iter().map(|(n, m)| (n.to_owned(), m)).collect();
    list.sort_by_key(|(n, _)| n.to_lowercase());
    list
}

/// Read once, when first needed: that's hundreds of files on Windows.
fn system() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// The family's regular face.
fn system_font(name: &str) -> Option<egui::FontData> {
    let db = system();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(name)],
        ..Default::default()
    };
    let (source, index) = db.face_source(db.query(&query)?)?;
    let path = match source {
        fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => p,
        fontdb::Source::Binary(_) => return None,
    };
    let mut data = egui::FontData::from_static(mapped(&path)?);
    data.index = index;
    Some(data)
}

/// Mapped rather than read, so glyph pages never drawn stay out of RAM (msjh.ttc is
/// ~20 MB). Each file once, kept for the life of the app, as fonts are.
fn mapped(path: &Path) -> Option<&'static [u8]> {
    static MAPS: Mutex<Option<HashMap<PathBuf, &'static [u8]>>> = Mutex::new(None);
    let mut maps = MAPS.lock().unwrap();
    let maps = maps.get_or_insert_default();
    if let Some(bytes) = maps.get(path) {
        return Some(bytes);
    }
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: system font files are not modified while the app runs.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    let bytes: &'static [u8] = Box::leak(Box::new(map));
    maps.insert(path.to_owned(), bytes);
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size is the body's; headings and small text keep their proportion, so a
    /// bigger size doesn't flatten the hierarchy.
    #[test]
    fn size_scales_every_text_style_and_theme_applies() {
        let ctx = egui::Context::default();
        let a = Appearance {
            theme: Theme::Light,
            size: 19.5,
            ..Default::default()
        };
        apply(&ctx, &a).unwrap();
        let size = |s: egui::TextStyle| ctx.style_of(egui::Theme::Light).text_styles[&s].size;
        assert_eq!(size(egui::TextStyle::Body), 19.5);
        assert_eq!(size(egui::TextStyle::Heading), 27.0);
        assert_eq!(size(egui::TextStyle::Small), 13.5);
        assert_eq!(
            ctx.options(|o| o.theme_preference),
            egui::ThemePreference::Light
        );
        // Both themes, so switching later keeps the size.
        let dark = ctx.style_of(egui::Theme::Dark).text_styles[&egui::TextStyle::Body].size;
        assert_eq!(dark, 19.5);
    }

    #[test]
    fn a_missing_font_is_named_and_the_rest_still_applies() {
        let ctx = egui::Context::default();
        let a = Appearance {
            code_font: "No Such Font 123".into(),
            size: 20.0,
            ..Default::default()
        };
        let e = apply(&ctx, &a).unwrap_err();
        assert!(e.contains("No Such Font 123"), "{e}");
        let body = &ctx.style_of(egui::Theme::Dark).text_styles[&egui::TextStyle::Body];
        assert_eq!(body.size, 20.0);
    }

    /// In both themes, so switching keeps them round.
    #[test]
    fn buttons_are_rounded_in_both_themes() {
        let ctx = egui::Context::default();
        apply(&ctx, &Appearance::default()).unwrap();
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            let style = ctx.style_of(theme);
            let w = &style.visuals.widgets;
            for v in [w.inactive, w.hovered, w.active, w.open] {
                assert_eq!(v.corner_radius, egui::CornerRadius::same(RADIUS));
            }
        }
    }

    /// A chosen font comes first in its family, ahead of the built-in one.
    #[test]
    fn an_installed_font_goes_first() {
        let Some((name, _)) = families().into_iter().find(|(_, mono)| *mono) else {
            panic!("no monospaced system font to try");
        };
        let ctx = egui::Context::default();
        let a = Appearance {
            code_font: name.clone(),
            ..Default::default()
        };
        apply(&ctx, &a).unwrap();
        // Fonts take effect at the next frame.
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
        let first = ctx.fonts(|f| f.definitions().families[&FontFamily::Monospace][0].clone());
        assert_eq!(first, name);
    }
}
