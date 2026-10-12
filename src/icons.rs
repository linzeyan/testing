//! Interface icons: Lucide's SVGs (ISC, assets/icons/LICENSE), rasterized at the exact
//! pixel size they're drawn at and tinted where they're used, as the Craft apps do.
//! Font glyphs (egui-phosphor, before) were blurred by scaling and sat on the text's
//! baseline instead of the centre of their button.

use std::collections::HashMap;

use eframe::egui::emath::GuiRounding;
use eframe::egui::{self, Color32, Rect, Vec2};

/// One of the SVGs in assets/icons, by its Lucide name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Icon {
    name: &'static str,
    svg: &'static [u8],
}

macro_rules! icons {
    ($($id:ident $file:literal),* $(,)?) => {
        $(pub const $id: Icon = Icon {
            name: $file,
            svg: include_bytes!(concat!("../assets/icons/", $file, ".svg")),
        };)*
        #[cfg(test)]
        const ALL: &[Icon] = &[$($id),*];
    };
}

icons! {
    ARROW_DOWN "arrow-down",
    ARROW_UP "arrow-up",
    BOOKMARK "bookmark",
    CHEVRON_DOWN "chevron-down",
    CHEVRON_RIGHT "chevron-right",
    CODE "code-xml",
    COLUMNS "columns-2",
    COOKIE "cookie",
    COPY "copy",
    DOWNLOAD "download",
    ERASER "eraser",
    EXPORT "share",
    EXTERNAL "external-link",
    EYE "eye",
    FILE_PLUS "file-plus",
    FOLDER_OPEN "folder-open",
    GAUGE "gauge",
    GLOBE "globe",
    GRIP "grip-vertical",
    HISTORY "history",
    HOURGLASS "hourglass",
    LAYERS "layers",
    LAYERS_PLUS "layers-plus",
    LIST "list",
    MORE "ellipsis",
    PENCIL "pencil",
    PLAY "play",
    PLUS "plus",
    REFRESH "refresh-cw",
    RESET "rotate-ccw",
    SEARCH "search",
    SETTINGS "settings",
    SIDEBAR "panel-left",
    TRASH "trash-2",
    UPDATE "circle-arrow-up",
    WAND "wand-sparkles",
    WARNING "triangle-alert",
    WRAP "wrap-text",
    X "x",
    // Kinds of request.
    BOTH_WAYS "arrow-left-right",
    FUNCTION "square-function",
    GRAPH "waypoints",
    LIGHTNING "zap",
    RADIO "radio",
    RSS "rss",
}

/// Textures by (icon, pixels), kept in the context: an icon is drawn once per size.
type Cache = HashMap<(&'static str, u32), egui::TextureHandle>;

impl Icon {
    /// Its texture `size` points square on this screen, white for a tint to colour.
    pub fn texture(self, ctx: &egui::Context, size: f32) -> egui::load::SizedTexture {
        let px = (size * ctx.pixels_per_point()).round().max(1.0) as u32;
        let (key, id) = ((self.name, px), egui::Id::new("icon-textures"));
        let cached = ctx.data_mut(|d| d.get_temp_mut_or_default::<Cache>(id).get(&key).cloned());
        // Loaded outside `data_mut`: both lock the context.
        let texture = cached.unwrap_or_else(|| {
            let image = rasterize(self.svg, px);
            let texture = ctx.load_texture(self.name, image, egui::TextureOptions::LINEAR);
            ctx.data_mut(|d| {
                d.get_temp_mut_or_default::<Cache>(id)
                    .insert(key, texture.clone())
            });
            texture
        });
        egui::load::SizedTexture::new(texture.id(), Vec2::splat(size))
    }

    /// For a button's atoms: it tints the image as its text.
    pub fn image(self, ctx: &egui::Context, size: f32) -> egui::Image<'static> {
        egui::Image::from_texture(self.texture(ctx, size))
    }

    /// Paints it `size` points square in `color`, centred in `rect`.
    pub fn paint(self, ui: &egui::Ui, rect: Rect, size: f32, color: Color32) {
        let texture = self.texture(ui.ctx(), size);
        let ppp = ui.ctx().pixels_per_point();
        // On whole pixels, so each texel lands on one and the lines stay sharp.
        let side = (size * ppp).round().max(1.0) / ppp;
        let min = (rect.center() - Vec2::splat(side / 2.0)).round_to_pixels(ppp);
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let at = Rect::from_min_size(min, Vec2::splat(side));
        ui.painter().image(texture.id, at, uv, color);
    }
}

fn rasterize(svg: &[u8], px: u32) -> egui::ColorImage {
    use resvg::{tiny_skia, usvg};
    let text = String::from_utf8_lossy(svg).replace("currentColor", "#ffffff");
    let tree = usvg::Tree::from_str(&text, &usvg::Options::default()).expect("bundled icon");
    let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("at least a pixel");
    let scale = px as f32 / tree.size().width();
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    egui::ColorImage::from_rgba_premultiplied([px as usize; 2], pixmap.data())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled icon parses and draws something, white, at the sizes a 1x and a 2x
    /// screen ask for.
    #[test]
    fn every_icon_draws_white() {
        for icon in ALL {
            for px in [14, 28] {
                let image = rasterize(icon.svg, px);
                let lit: Vec<_> = image.pixels.iter().filter(|p| p.a() > 0).collect();
                assert!(lit.len() > (px * px / 20) as usize, "{} at {px}", icon.name);
                assert!(
                    lit.iter().all(|p| p.r() == p.a() && p.b() == p.a()),
                    "{}",
                    icon.name
                );
            }
        }
    }
}
