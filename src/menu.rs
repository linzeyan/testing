//! The Mac's menu bar. Every item of the app's own is one of its keyboard shortcuts:
//! picked, or its keys pressed (the system hands those to the menu, not the window), it
//! comes back as that key press in the next frame's input, so there is one place that
//! says what a shortcut does and when it may (`App::shortcuts`).

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, ViewportCommand};
use muda::accelerator::{Accelerator, Code, Modifiers as Mods};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::i18n::t;

/// A row of one of the app's menus.
pub enum Row {
    Keys(&'static str, KeyboardShortcut),
    Separator,
}

/// What a picked item asks of the next frame.
#[derive(Clone, Copy)]
enum Does {
    Keys(KeyboardShortcut),
    // The clipboard goes through the window, which reads and writes it for egui.
    Cut,
    Copy,
    Paste,
}

struct Picked {
    does: HashMap<MenuId, Does>,
    events: Mutex<mpsc::Receiver<MenuId>>,
}

static PICKED: OnceLock<Picked> = OnceLock::new();

/// Puts the menu bar up, in the language set now: once, from the main thread, after the
/// window exists. `menus` are the app's own, File first: Edit goes after it.
pub fn install(ctx: &egui::Context, menus: Vec<(&'static str, Vec<Row>)>) {
    let mut does = HashMap::new();
    let mut item = |menu: &Submenu, text: &str, what: Does, keys: KeyboardShortcut| {
        let item = MenuItem::new(text, true, accelerator(&keys));
        does.insert(item.id().clone(), what);
        let _ = menu.append(&item);
    };
    let command = |key| KeyboardShortcut::new(Modifiers::COMMAND, key);
    let bar = Menu::new();

    let app = Submenu::new("apitool", true);
    let _ = app.append(&PredefinedMenuItem::about(None, None));
    let _ = app.append(&PredefinedMenuItem::separator());
    let settings = command(Key::Comma);
    item(&app, t("Settings…"), Does::Keys(settings), settings);
    let _ = app.append_items(&[
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::services(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::quit(None),
    ]);
    let _ = bar.append(&app);

    for (at, (name, rows)) in menus.into_iter().enumerate() {
        // Edit is the Mac's second menu, after File.
        if at == 1 {
            // Not the predefined items: those send cut:, copy: and paste: to the window's view,
            // which has none of them, so the items would be grey and their keys swallowed.
            let edit = Submenu::new(t("Edit"), true);
            let undo = command(Key::Z);
            let redo = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
            item(&edit, t("Undo"), Does::Keys(undo), undo);
            item(&edit, t("Redo"), Does::Keys(redo), redo);
            let _ = edit.append(&PredefinedMenuItem::separator());
            item(&edit, t("Cut"), Does::Cut, command(Key::X));
            item(&edit, t("Copy"), Does::Copy, command(Key::C));
            item(&edit, t("Paste"), Does::Paste, command(Key::V));
            let all = command(Key::A);
            item(&edit, t("Select all"), Does::Keys(all), all);
            let _ = bar.append(&edit);
        }
        let menu = Submenu::new(name, true);
        for row in rows {
            match row {
                Row::Keys(text, keys) => item(&menu, text, Does::Keys(keys), keys),
                Row::Separator => {
                    let _ = menu.append(&PredefinedMenuItem::separator());
                }
            }
        }
        let _ = bar.append(&menu);
    }

    let window = Submenu::new(t("Window"), true);
    let _ = window.append_items(&[
        &PredefinedMenuItem::minimize(None),
        &PredefinedMenuItem::maximize(None),
        &PredefinedMenuItem::fullscreen(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::bring_all_to_front(None),
    ]);
    let _ = bar.append(&window);
    window.set_as_windows_menu_for_nsapp();

    let (tx, rx) = mpsc::channel();
    let picked = Picked {
        does,
        events: Mutex::new(rx),
    };
    if PICKED.set(picked).is_err() {
        return;
    }
    let ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = tx.send(event.id);
        // An idle window draws no frame to take it in.
        ctx.request_repaint();
    }));
    bar.init_for_nsapp();
    // The NSMenu lives as long as the app: dropping the handle would take the items' ids.
    std::mem::forget(bar);
}

/// Hands what was picked since the last frame to this one, as the key presses they are.
pub fn take(ctx: &egui::Context, input: &mut egui::RawInput) {
    let Some(picked) = PICKED.get() else { return };
    let Ok(events) = picked.events.lock() else {
        return;
    };
    for id in events.try_iter() {
        match picked.does.get(&id) {
            Some(Does::Keys(keys)) => {
                log::info!("menu: {}", ctx.format_shortcut(keys));
                input.events.push(pressed(keys));
            }
            Some(Does::Cut) => ctx.send_viewport_cmd(ViewportCommand::RequestCut),
            Some(Does::Copy) => ctx.send_viewport_cmd(ViewportCommand::RequestCopy),
            Some(Does::Paste) => ctx.send_viewport_cmd(ViewportCommand::RequestPaste),
            None => {}
        }
    }
}

/// The key press a menu item stands for, as the window would have sent it.
pub fn pressed(keys: &KeyboardShortcut) -> egui::Event {
    let mut modifiers = keys.modifiers;
    // As a pressed ⌘ comes from the window on a Mac.
    modifiers.mac_cmd = modifiers.command;
    egui::Event::Key {
        key: keys.logical_key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// None for a key the menu has no name for: the item is still there, without its keys.
fn accelerator(keys: &KeyboardShortcut) -> Option<Accelerator> {
    let code = match keys.logical_key {
        Key::A => Code::KeyA,
        Key::C => Code::KeyC,
        Key::D => Code::KeyD,
        Key::E => Code::KeyE,
        Key::F => Code::KeyF,
        Key::G => Code::KeyG,
        Key::K => Code::KeyK,
        Key::L => Code::KeyL,
        Key::N => Code::KeyN,
        Key::O => Code::KeyO,
        Key::S => Code::KeyS,
        Key::T => Code::KeyT,
        Key::V => Code::KeyV,
        Key::W => Code::KeyW,
        Key::X => Code::KeyX,
        Key::Z => Code::KeyZ,
        Key::Enter => Code::Enter,
        Key::Comma => Code::Comma,
        Key::Slash => Code::Slash,
        Key::Backslash => Code::Backslash,
        _ => return None,
    };
    let mut mods = Mods::empty();
    let m = keys.modifiers;
    mods.set(Mods::META, m.command || m.mac_cmd);
    mods.set(Mods::SHIFT, m.shift);
    mods.set(Mods::ALT, m.alt);
    mods.set(Mods::CONTROL, m.ctrl);
    Some(Accelerator::new(mods, code))
}
