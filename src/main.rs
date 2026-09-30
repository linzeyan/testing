#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod grpc;
mod http;
mod model;
mod net;
mod runner;
mod script;
mod store;
mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use eframe::egui_wgpu::WgpuSetup;
use eframe::wgpu;

fn main() -> eframe::Result {
    let use_glow = std::env::args().any(|a| a == "--glow");
    let mut options = eframe::NativeOptions {
        renderer: if use_glow {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("apitool"),
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

    let ws = match store::Workspace::open(workspace_dir()) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("cannot open workspace: {e}");
            std::process::exit(1);
        }
    };
    // Relative paths in requests (.proto files, data files) then resolve inside the
    // workspace, so they keep working after a git clone on another machine.
    if let Err(e) = std::env::set_current_dir(&ws.root) {
        eprintln!("cannot enter workspace {}: {e}", ws.root.display());
        std::process::exit(1);
    }
    eframe::run_native(
        "apitool",
        options,
        Box::new(|cc| {
            let font = install_cjk_font(&cc.egui_ctx).unwrap_or("none: CJK text will not render");
            Ok(Box::new(app::App::new(
                ws,
                format!("{}\nCJK font: {font}", renderer_info(cc)),
            )))
        }),
    )
}

/// Portable by default: the workspace lives next to the exe, so the whole tool can sit
/// in a user folder on a VDI without installation. `APITOOL_WORKSPACE` points elsewhere
/// (e.g. an existing git clone).
fn workspace_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("APITOOL_WORKSPACE") {
        // Absolute because main changes the working directory to the workspace.
        return std::path::absolute(dir).unwrap_or_default();
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));
    exe_dir.unwrap_or_default().join("workspace")
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

/// egui's bundled fonts have no CJK glyphs. Borrow the OS font instead of embedding one;
/// mmap keeps untouched glyph pages out of RSS (msjh.ttc is ~20 MB).
fn install_cjk_font(ctx: &egui::Context) -> Option<&'static str> {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msjh.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    let (path, bytes) = CANDIDATES.iter().find_map(|p| {
        let file = std::fs::File::open(p).ok()?;
        // SAFETY: system font files are not modified while the app runs.
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        Some((*p, &**Box::leak(Box::new(map))))
    })?;
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), Arc::new(egui::FontData::from_static(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
    Some(path)
}
