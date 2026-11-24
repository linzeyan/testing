#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use eframe::egui;
use eframe::egui_wgpu::WgpuSetup;
use eframe::wgpu;

use apitool::{app, appearance, logfile, store, update};

fn main() -> eframe::Result {
    let use_glow = std::env::args().any(|a| a == "--glow");
    let mut options = eframe::NativeOptions {
        renderer: if use_glow {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(app::WINDOW_SIZE)
            .with_min_inner_size([720.0, 480.0])
            .with_title("apitool")
            .with_icon(Arc::new(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
                    .expect("the bundled icon is a valid PNG"),
            )),
        ..Default::default()
    };
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        // Vulkan/GL drivers inside Citrix are the usual crash source; DX12 always has WARP.
        // WGPU_BACKEND still overrides this for field debugging.
        let native = if cfg!(windows) {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::PRIMARY
        };
        setup.instance_descriptor.backends = wgpu::Backends::from_env().unwrap_or(native);
        setup.native_adapter_selector = Some(Arc::new(select_adapter));
        // The default `Performance` hint makes the DX12/Vulkan allocator reserve 128 MB+64 MB
        // blocks up front; under WARP that is all system RAM, which the VDI can't spare.
        let base = setup.device_descriptor.clone();
        setup.device_descriptor = Arc::new(move |adapter| wgpu::DeviceDescriptor {
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..base(adapter)
        });
    }

    // The Mac's way since Big Sur: no title, the tab strip in the title bar's row and the
    // sidebar running to the top, under the traffic lights (`App::in_title_bar`).
    if cfg!(target_os = "macos") {
        options.viewport = (options.viewport)
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false);
    }

    let ws = match store::open_workspace(None) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("cannot open workspace: {e}");
            std::process::exit(1);
        }
    };
    // Restored maximized, the window is made at the maximized size rather than its saved
    // one. Windows reports the size it was made at in a message that winit hands on only
    // after eframe has set up, and eframe then sizes its surface to it and draws at once:
    // a frame laid out for the maximized window went into a surface the saved size and was
    // stretched to fill it, the tree in its corner enlarged and blurred for a moment (VM,
    // 175%; on a 100% VDI, 1200×800 stretched to 1920×1017). The saved size comes back as
    // where it un-maximizes to once it's shown (`appearance::set_restored_rect`).
    #[cfg(windows)]
    {
        options.window_builder = Some(Box::new(|b: egui::ViewportBuilder| {
            match (b.maximized, maximized_inner_size()) {
                (Some(true), Some(size)) => b.with_inner_size(size),
                _ => b,
            }
        }));
    }
    let logs = store::state_dir().unwrap_or_else(|| ws.root.clone());
    logfile::init(Some(logs.join("apitool.log")));
    // Beside the log rather than eframe's %APPDATA%: on Windows the tool is portable.
    options.persistence_path = Some(logs.join(".window.ron"));
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let version = env!("CARGO_PKG_VERSION");
    log::info!(
        "apitool {version} on {os} {arch}, workspace {}",
        ws.root.display()
    );
    // On its own thread: an idle window draws no frames, and a leak shows while idle too.
    std::thread::spawn(|| {
        loop {
            if let Some(m) = memory_stats::memory_stats() {
                let ram = m.physical_mem >> 20;
                // Private bytes on Windows; elsewhere it's the address space, which says
                // nothing.
                match cfg!(windows) {
                    true => log::info!("memory: RAM {ram} MB, private {} MB", m.virtual_mem >> 20),
                    false => log::info!("memory: RAM {ram} MB"),
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
    let result = eframe::run_native(
        "apitool",
        options,
        Box::new(|cc| {
            // The app installs it with the fonts chosen in Settings.
            let font = appearance::cjk().map_or("none: CJK text will not render", |(p, _)| p);
            let renderer = format!("{}\nCJK font: {font}", renderer_info(cc));
            log::info!("{}", renderer.replace('\n', ", "));
            cc.egui_ctx.add_plugin(app::OpenLinks);
            let mut app = app::App::new(ws, renderer);
            app.in_title_bar = cfg!(target_os = "macos");
            app.auto_update();
            app.restore_window(cc.storage);
            app.apply_appearance(&cc.egui_ctx);
            #[cfg(target_os = "macos")]
            app.install_menu(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    // Not at the close request: eframe saves the window on a thread it joins only as it
    // shuts down, so starting the new apitool then could beat the save.
    if update::RESTART.load(std::sync::atomic::Ordering::Relaxed)
        && let Err(e) = update::restart()
    {
        log::error!("restarting: {e}");
    }
    result
}

fn renderer_info(cc: &eframe::CreationContext<'_>) -> String {
    if let Some(rs) = &cc.wgpu_render_state {
        let i = rs.adapter.get_info();
        format!("wgpu {:?} / {:?} / {}", i.backend, i.device_type, i.name)
    } else if let Some(gl) = &cc.gl {
        use eframe::glow::HasContext as _;
        // SAFETY: querying a string on the context eframe just made current.
        format!("glow / {}", unsafe {
            gl.get_parameter_string(eframe::glow::RENDERER)
        })
    } else {
        "unknown renderer".into()
    }
}

/// The client area of a window maximized on the primary monitor, in points: its work area
/// less the title bar.
#[cfg(windows)]
fn maximized_inner_size() -> Option<egui::Vec2> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SM_CYCAPTION, SPI_GETWORKAREA, SystemParametersInfoW,
    };
    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: SPI_GETWORKAREA writes one RECT into the pointer given.
    unsafe {
        if SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) == 0 {
            return None;
        }
        let dpi = GetDpiForSystem();
        let caption = GetSystemMetricsForDpi(SM_CYCAPTION, dpi);
        let scale = dpi as f32 / 96.0;
        let (w, h) = (work.right - work.left, work.bottom - work.top - caption);
        Some(egui::vec2(w as f32 / scale, h as f32 / scale))
    }
}

/// Prefer a real GPU, but accept a software adapter (WARP on Windows) instead of failing:
/// a VDI without a vGPU only exposes "Microsoft Basic Render Driver".
fn select_adapter(
    adapters: &[wgpu::Adapter],
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    adapters
        .iter()
        .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
        .min_by_key(|a| a.get_info().device_type == wgpu::DeviceType::Cpu)
        .cloned()
        .ok_or_else(|| format!("no usable adapter among {} found", adapters.len()))
}
