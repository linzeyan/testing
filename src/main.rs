#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use eframe::egui;
use eframe::egui_wgpu::WgpuSetup;
use eframe::wgpu;

use apitool::{app, appearance, store};

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

    let ws = match store::open_workspace(None) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("cannot open workspace: {e}");
            std::process::exit(1);
        }
    };
    eframe::run_native(
        "apitool",
        options,
        Box::new(|cc| {
            // The app installs it with the fonts chosen in Settings.
            let font = appearance::cjk().map_or("none: CJK text will not render", |(p, _)| p);
            Ok(Box::new(app::App::new(
                ws,
                format!("{}\nCJK font: {font}", renderer_info(cc)),
            )))
        }),
    )
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
