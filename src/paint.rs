//! Whether a finished frame is worth drawing.
//!
//! eframe draws and presents every frame it runs, and it runs one for every pointer move
//! and another after it (egui asks for a second once the pointer has moved). Most come to
//! the picture the window already shows. On a GPU that costs little; a software renderer
//! (WARP, on a VDI with no vGPU) rasterises the whole window on the CPU for each of them,
//! on every core: moving the pointer over the window was a CPU spike for as long as it
//! moved. `App::wants_paint`, the hook added to the vendored eframe, asks here.
//!
//! A frame that did change mostly changes a little: a row lit under the pointer, a tooltip.
//! On a software renderer that one is drawn over the last picture, which egui-wgpu keeps
//! (vendored for that), only where it changed: the rest of the window isn't rasterised
//! again. The richer the window, the more that saves.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use eframe::egui;

/// The least time between two drawn frames on a software renderer: 30 a second. A drag or
/// a scroll changes the picture every frame, and there every frame is the CPU's.
pub const SOFTWARE_GAP: Duration = Duration::from_millis(33);

/// The least time between two frames that aren't drawn: what a drawn one waits for the
/// screen at 60 Hz. Nothing else holds them apart: with no present to wait in, eframe ran
/// four frames for each move of the pointer (the move and the mouse's raw motion, each
/// followed by the one egui asks for), 200 a second on the Windows VM.
pub const PACE: Duration = Duration::from_millis(16);

/// Frames run and frames drawn since the start, for the log: a CPU complaint from a
/// machine out of reach is told apart by how many of each there were.
static FRAMES: AtomicU64 = AtomicU64::new(0);
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// (frames run, frames drawn) since the start.
pub fn counts() -> (u64, u64) {
    (FRAMES.load(Relaxed), DRAWN.load(Relaxed))
}

/// What a picture was drawn into. When it changes the last picture may be gone (a resized
/// or restored surface starts empty), so the next frame is drawn whatever it comes to.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    size: Option<egui::Vec2>,
    scale: f32,
    minimized: Option<bool>,
    maximized: Option<bool>,
    fullscreen: Option<bool>,
    focused: Option<bool>,
    occluded: Option<bool>,
}

impl Target {
    pub fn of(input: &egui::InputState) -> Self {
        let window = input.viewport();
        Self {
            // The window's, or (headless, in tests) the one egui lays out in.
            size: Some(window.inner_rect.unwrap_or(input.viewport_rect()).size()),
            scale: input.pixels_per_point,
            minimized: window.minimized,
            maximized: window.maximized,
            fullscreen: window.fullscreen,
            focused: window.focused,
            occluded: window.occluded,
        }
    }
}

/// What the window shows, as far as the app can tell.
#[derive(Default)]
pub struct Painted {
    /// Draw every frame, as eframe does by itself.
    pub all: bool,
    /// Draw a changed frame only where it changed, over the kept picture.
    pub part: bool,
    /// What the window is cleared to before anything is drawn.
    pub clear: egui::Color32,
    /// The least time between two drawn frames; None draws a changed one at once.
    pub gap: Option<Duration>,
    /// Set by the renderer when a frame couldn't be shown (the surface was outdated, lost
    /// or timed out): what is held here as on screen isn't.
    pub failed: Arc<AtomicBool>,
    shown: Option<(u64, Target)>,
    /// The last frame drawn, when drawing in part: what a new one is told apart from.
    last: Vec<egui::ClippedPrimitive>,
    /// When the last drawn frame went to the renderer, and when the last frame of either
    /// kind left here.
    drawn: Option<Instant>,
    left: Option<Instant>,
    /// What the last frame was told to wait.
    waited: Duration,
}

impl Painted {
    /// Whether to draw the frame that came to `primitives`, how, and how long to wait
    /// first: `gap` before a drawn one, `gap` or `PACE` after one that isn't. `must`: it
    /// is drawn whole whatever is said here. `kept`: the last picture drawn is kept, so it
    /// may be drawn over in part.
    pub fn judge(
        &mut self,
        primitives: &[egui::ClippedPrimitive],
        target: Target,
        must: bool,
        kept: bool,
        now: Instant,
    ) -> (eframe::Paint, Duration) {
        FRAMES.fetch_add(1, Relaxed);
        let frame = picture(primitives).map(|p| (p, target.clone()));
        let lost = self.failed.swap(false, Relaxed);
        // What is left of `least` since `then`.
        let rest = |least: Duration, then: Option<Instant>| {
            then.map_or(Duration::ZERO, |t| {
                least.saturating_sub(now.saturating_duration_since(t))
            })
        };
        if !must && !lost && !self.all && frame.is_some() && frame == self.shown {
            let wait = rest(self.gap.unwrap_or(PACE), self.left);
            (self.left, self.waited) = (Some(now + wait), wait);
            return (eframe::Paint::Skip, wait);
        }
        let wait = self.gap.map_or(Duration::ZERO, |gap| rest(gap, self.drawn));
        let paint = match self.part && !self.all {
            false => eframe::Paint::All,
            true => {
                // Over the last picture only when it is the one on screen, at this size.
                let same = (self.shown.as_ref()).is_some_and(|(_, t)| *t == target);
                let over = kept && !must && !lost && same && frame.is_some();
                let part = (over)
                    .then(|| changed(&self.last, primitives, &target))
                    .flatten()
                    .map(|rect| only(primitives, rect, self.clear));
                self.last = match frame.is_some() {
                    true => primitives.to_vec(),
                    false => Vec::new(),
                };
                part.map_or(eframe::Paint::Keep, eframe::Paint::Part)
            }
        };
        (self.shown, self.drawn, self.left) = (frame, Some(now + wait), Some(now + wait));
        self.waited = wait;
        DRAWN.fetch_add(1, Relaxed);
        (paint, wait)
    }

    /// What the last frame waited here: eframe counts it into that frame's time.
    pub fn waited(&self) -> Duration {
        self.waited
    }
}

/// Where `new` draws other than `old` did, in points, out to whole pixels: drawing `new`
/// there over the picture of `old` comes to the picture of `new`. None when that is most
/// of the window (drawing it all costs no more) or can't be told.
///
/// egui runs a frame's shapes together into one mesh per clip and texture, so a lit row
/// is a few triangles changed inside the mesh of its whole panel. Meshes alike at the
/// start and at the end of the frame are passed over; in those between, so are triangles
/// alike at their start and end (alike: the same three vertices, wherever they sit in the
/// vertex list, as a shape added earlier shifts the rest). What is left, in both frames,
/// is the change: every triangle outside its bounds is in both, in the same order.
pub fn changed(
    old: &[egui::ClippedPrimitive],
    new: &[egui::ClippedPrimitive],
    target: &Target,
) -> Option<egui::Rect> {
    use egui::epaint::Primitive;
    fn mesh(p: &egui::ClippedPrimitive) -> Option<&egui::Mesh> {
        match &p.primitive {
            Primitive::Mesh(m) => Some(m),
            Primitive::Callback(_) => None,
        }
    }
    let alike = |a: &egui::ClippedPrimitive, b: &egui::ClippedPrimitive| match (mesh(a), mesh(b)) {
        (Some(x), Some(y)) => {
            a.clip_rect == b.clip_rect
                && x.texture_id == y.texture_id
                && x.indices == y.indices
                && bytemuck::cast_slice::<_, u8>(&x.vertices)
                    == bytemuck::cast_slice::<_, u8>(&y.vertices)
        }
        _ => false,
    };
    let head = old.iter().zip(new).take_while(|(a, b)| alike(a, b)).count();
    let tail = (old[head..].iter().rev())
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| alike(a, b))
        .count();
    let (old, new) = (&old[head..old.len() - tail], &new[head..new.len() - tail]);
    let mut rect = egui::Rect::NOTHING;
    // The bounds of these triangles of `m`, cut to the clip they're drawn in.
    let mut add = |p: &egui::ClippedPrimitive, m: &egui::Mesh, triangles: &[u32]| {
        let mut bounds = egui::Rect::NOTHING;
        for &i in triangles {
            bounds.extend_with(m.vertices[i as usize].pos);
        }
        rect = rect.union(bounds.intersect(p.clip_rect));
    };
    let paired = old.len() == new.len()
        && old.iter().zip(new).all(|(a, b)| {
            a.clip_rect == b.clip_rect
                && matches!((mesh(a), mesh(b)), (Some(x), Some(y)) if x.texture_id == y.texture_id)
        });
    if paired {
        for (a, b) in old.iter().zip(new) {
            let (x, y) = (mesh(a)?, mesh(b)?);
            let same = |i: usize, j: usize| {
                (0..3).all(|k| {
                    let (u, v) = (x.indices[i * 3 + k], y.indices[j * 3 + k]);
                    bytemuck::bytes_of(&x.vertices[u as usize])
                        == bytemuck::bytes_of(&y.vertices[v as usize])
                })
            };
            let (n, m) = (x.indices.len() / 3, y.indices.len() / 3);
            let first = (0..n.min(m)).take_while(|&t| same(t, t)).count();
            let last = (0..n.min(m) - first)
                .take_while(|&t| same(n - 1 - t, m - 1 - t))
                .count();
            add(a, x, &x.indices[first * 3..(n - last) * 3]);
            add(b, y, &y.indices[first * 3..(m - last) * 3]);
        }
    } else {
        for p in old.iter().chain(new) {
            let m = mesh(p)?;
            add(p, m, &m.indices);
        }
    }
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, target.size?);
    let rect = rect.intersect(screen);
    if !rect.is_positive() {
        // Nothing visible changed: a clip moved over nothing, say. Rare; drawn whole.
        return None;
    }
    // Out to whole pixels, so each pixel is redrawn whole or not at all.
    let px = target.scale;
    let rect = egui::Rect::from_min_max(
        (rect.min.to_vec2() * px).floor().to_pos2() / px,
        (rect.max.to_vec2() * px).ceil().to_pos2() / px,
    );
    (rect.area() < screen.area() * 0.5).then_some(rect)
}

/// `primitives` drawn only inside `rect`, over what is there: the rect is first cleared
/// to `clear`, as the whole window is before a frame, then every primitive that reaches
/// into it is drawn cut to it. A scissor cut to whole pixels covers the same pixels inside
/// the rect as the uncut one does, so they come out as a whole frame's would.
pub fn only(
    primitives: &[egui::ClippedPrimitive],
    rect: egui::Rect,
    clear: egui::Color32,
) -> Vec<egui::ClippedPrimitive> {
    let mut ground = egui::Mesh::default();
    ground.add_colored_rect(rect, clear);
    let mut part = vec![egui::ClippedPrimitive {
        clip_rect: rect,
        primitive: egui::epaint::Primitive::Mesh(ground),
    }];
    for p in primitives {
        let clip = p.clip_rect.intersect(rect);
        if !clip.is_positive() {
            continue;
        }
        // Only the triangles that reach into it: the rest would be set up and cut away
        // by the scissor, every one of them on the CPU under WARP.
        let primitive = match &p.primitive {
            egui::epaint::Primitive::Mesh(m) => {
                let mut indices = Vec::new();
                for t in m.indices.chunks_exact(3) {
                    let mut bounds = egui::Rect::NOTHING;
                    t.iter()
                        .for_each(|&i| bounds.extend_with(m.vertices[i as usize].pos));
                    if bounds.intersects(clip) {
                        indices.extend_from_slice(t);
                    }
                }
                if indices.is_empty() {
                    continue;
                }
                egui::epaint::Primitive::Mesh(egui::Mesh {
                    indices,
                    vertices: m.vertices.clone(),
                    texture_id: m.texture_id,
                })
            }
            other => other.clone(),
        };
        part.push(egui::ClippedPrimitive {
            clip_rect: clip,
            primitive,
        });
    }
    part
}

/// For tests: whether drawing `new` inside `rect` over the picture of `old` comes to the
/// picture of `new`. It does when every triangle that reaches outside the rect (by its
/// bounds, cut to its clip and out to whole pixels at `scale`) is in both, in the same
/// order: outside the rect the two frames lay the same triangles over each other.
#[cfg(test)]
pub(crate) fn check_part(
    old: &[egui::ClippedPrimitive],
    new: &[egui::ClippedPrimitive],
    rect: egui::Rect,
    scale: f32,
) -> Result<(), String> {
    type Triangle = (egui::Rect, egui::TextureId, [[u8; 20]; 3]);
    let outside = |frame: &[egui::ClippedPrimitive]| -> Vec<Triangle> {
        let mut out = Vec::new();
        for p in frame {
            let egui::epaint::Primitive::Mesh(m) = &p.primitive else {
                continue;
            };
            for t in m.indices.chunks(3) {
                let v = [t[0], t[1], t[2]].map(|i| m.vertices[i as usize]);
                let mut bounds = egui::Rect::NOTHING;
                v.iter().for_each(|v| bounds.extend_with(v.pos));
                let cut = bounds.intersect(p.clip_rect);
                if !cut.is_positive() {
                    continue;
                }
                let px = egui::Rect::from_min_max(
                    (cut.min.to_vec2() * scale).floor().to_pos2() / scale,
                    (cut.max.to_vec2() * scale).ceil().to_pos2() / scale,
                );
                if !rect.contains_rect(px) {
                    let bytes = v.map(bytemuck::cast::<_, [u8; 20]>);
                    out.push((p.clip_rect, m.texture_id, bytes));
                }
            }
        }
        out
    };
    let (a, b) = (outside(old), outside(new));
    match a.iter().zip(&b).position(|(x, y)| x != y) {
        None if a.len() == b.len() => Ok(()),
        at => Err(format!(
            "outside {rect:?}: {} triangles before, {} after, first unlike at {at:?}",
            a.len(),
            b.len()
        )),
    }
}

/// A number for what a frame draws: the same for the same triangles in the same clips.
/// None with a paint callback in it, which only its owner could compare.
fn picture(primitives: &[egui::ClippedPrimitive]) -> Option<u64> {
    struct Meshes<'a>(&'a [egui::ClippedPrimitive]);
    impl Hash for Meshes<'_> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            for clipped in self.0 {
                let clip = clipped.clip_rect;
                state.write(bytemuck::bytes_of(&[clip.min, clip.max]));
                if let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive {
                    mesh.texture_id.hash(state);
                    state.write(bytemuck::cast_slice(&mesh.indices));
                    state.write(bytemuck::cast_slice(&mesh.vertices));
                }
            }
        }
    }
    let callback =
        |p: &egui::ClippedPrimitive| matches!(p.primitive, egui::epaint::Primitive::Callback(_));
    (!primitives.iter().any(callback)).then(|| egui::util::hash(Meshes(primitives)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drew((paint, _): (eframe::Paint, Duration)) -> bool {
        !matches!(paint, eframe::Paint::Skip)
    }

    fn drew_wait((paint, wait): (eframe::Paint, Duration)) -> (bool, Duration) {
        (!matches!(paint, eframe::Paint::Skip), wait)
    }

    fn target(width: f32) -> Target {
        Target {
            size: Some(egui::vec2(width, 600.0)),
            scale: 1.0,
            minimized: Some(false),
            maximized: Some(false),
            fullscreen: Some(false),
            focused: Some(true),
            occluded: Some(false),
        }
    }

    fn mesh(x: f32) -> Vec<egui::ClippedPrimitive> {
        let mut mesh = egui::Mesh::default();
        mesh.add_colored_rect(
            egui::Rect::from_min_size(egui::pos2(x, 0.0), egui::vec2(10.0, 10.0)),
            egui::Color32::RED,
        );
        vec![egui::ClippedPrimitive {
            clip_rect: egui::Rect::EVERYTHING,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }]
    }

    #[test]
    fn the_same_picture_is_drawn_once_and_a_changed_one_again() {
        let mut painted = Painted::default();
        let now = Instant::now();
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(800.0),
            false,
            false,
            now
        )));
        assert!(!drew(painted.judge(
            &mesh(0.0),
            target(800.0),
            false,
            false,
            now
        )));
        assert!(drew(painted.judge(
            &mesh(1.0),
            target(800.0),
            false,
            false,
            now
        )));
        assert!(!drew(painted.judge(
            &mesh(1.0),
            target(800.0),
            false,
            false,
            now
        )));
        // Back to an earlier picture is a change too: it isn't what the window shows.
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(800.0),
            false,
            false,
            now
        )));
    }

    /// A resized, restored or uncovered window may have lost what was drawn in it, and a
    /// frame that failed to show never got there: the same picture is drawn again.
    #[test]
    fn a_picture_that_may_be_gone_is_drawn_again() {
        let mut painted = Painted::default();
        let now = Instant::now();
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(800.0),
            false,
            false,
            now
        )));
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            false,
            false,
            now
        )));
        let mut restored = target(801.0);
        restored.minimized = Some(true);
        assert!(drew(painted.judge(&mesh(0.0), restored, false, false, now)));
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            false,
            false,
            now
        )));
        assert!(!drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            false,
            false,
            now
        )));
        painted.failed.store(true, Relaxed);
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            false,
            false,
            now
        )));
        assert!(!drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            false,
            false,
            now
        )));
        // eframe draws it anyway (a texture changed under the same triangles).
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            true,
            false,
            now
        )));
        painted.all = true;
        assert!(drew(painted.judge(
            &mesh(0.0),
            target(801.0),
            false,
            false,
            now
        )));
    }

    /// On a software renderer changed frames are held to `gap` apart, and the wait is
    /// counted from when the last one was really drawn, after its own wait.
    #[test]
    fn changed_frames_keep_their_distance() {
        let mut painted = Painted {
            gap: Some(SOFTWARE_GAP),
            ..Default::default()
        };
        let start = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(
            drew_wait(painted.judge(&mesh(0.0), target(800.0), false, false, start)),
            (true, ms(0))
        );
        let at = start + ms(10);
        assert_eq!(
            drew_wait(painted.judge(&mesh(1.0), target(800.0), false, false, at)),
            (true, ms(23))
        );
        // Drawn at 33 ms; the next, 7 ms after that, waits for 66 ms.
        let at = start + ms(40);
        assert_eq!(
            drew_wait(painted.judge(&mesh(2.0), target(800.0), false, false, at)),
            (true, ms(26))
        );
        let at = start + ms(200);
        assert_eq!(
            drew_wait(painted.judge(&mesh(3.0), target(800.0), false, false, at)),
            (true, ms(0))
        );
    }

    /// Frames that draw nothing don't run back to back: each waits out what is left of
    /// `PACE` (of `gap` on a software renderer) since the frame before it.
    #[test]
    fn undrawn_frames_keep_their_distance_too() {
        let ms = Duration::from_millis;
        for (gap, pace) in [(None, PACE), (Some(SOFTWARE_GAP), SOFTWARE_GAP)] {
            let mut painted = Painted {
                gap,
                ..Default::default()
            };
            let start = Instant::now();
            assert_eq!(
                drew_wait(painted.judge(&mesh(0.0), target(800.0), false, false, start)),
                (true, ms(0))
            );
            let at = start + ms(2);
            assert_eq!(
                drew_wait(painted.judge(&mesh(0.0), target(800.0), false, false, at)),
                (false, pace - ms(2))
            );
            // That one left at `pace`; the next comes 1 ms after it did.
            let at = start + pace + ms(1);
            assert_eq!(
                drew_wait(painted.judge(&mesh(0.0), target(800.0), false, false, at)),
                (false, pace - ms(1))
            );
            // After a pause there is nothing to wait for.
            let at = start + ms(500);
            assert_eq!(
                drew_wait(painted.judge(&mesh(0.0), target(800.0), false, false, at)),
                (false, ms(0))
            );
        }
        // Drawing every frame, as eframe by itself: the present does the waiting.
        let mut painted = Painted {
            all: true,
            ..Default::default()
        };
        let start = Instant::now();
        painted.judge(&mesh(0.0), target(800.0), false, false, start);
        assert_eq!(
            drew_wait(painted.judge(&mesh(0.0), target(800.0), false, false, start)),
            (true, ms(0))
        );
    }

    /// A panel's mesh of rects (a row each), in one clip, as egui runs them together.
    fn panel(rows: &[(f32, egui::Color32)], clip: egui::Rect) -> egui::ClippedPrimitive {
        let mut mesh = egui::Mesh::default();
        for &(y, color) in rows {
            let row = egui::Rect::from_min_size(egui::pos2(0.0, y), egui::vec2(300.0, 20.0));
            mesh.add_colored_rect(row, color);
        }
        egui::ClippedPrimitive {
            clip_rect: clip,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }
    }

    /// What a drawn frame came to after `before`: the rect drawn in part, if it was.
    fn part_after(
        before: &[egui::ClippedPrimitive],
        after: &[egui::ClippedPrimitive],
    ) -> Option<egui::Rect> {
        let mut painted = Painted::default();
        (painted.part, painted.clear) = (true, egui::Color32::BLACK);
        let now = Instant::now();
        assert!(matches!(
            painted.judge(before, target(800.0), false, true, now).0,
            eframe::Paint::Keep
        ));
        match painted.judge(after, target(800.0), false, true, now).0 {
            eframe::Paint::Part(part) => {
                let rect = part[0].clip_rect;
                assert!(part.iter().all(|p| rect.contains_rect(p.clip_rect)));
                check_part(before, after, rect, 1.0).unwrap();
                Some(rect)
            }
            eframe::Paint::Keep => None,
            _ => panic!("drawn"),
        }
    }

    /// A row lit under the pointer, one added in the middle of a panel, one taken away, a
    /// tooltip over it all: each is drawn in part, just about where it changed, and the
    /// rest of the window comes out as a whole frame would (`check_part`).
    #[test]
    fn a_frame_that_changed_a_little_is_drawn_where_it_changed() {
        let all = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let (idle, lit) = (egui::Color32::DARK_GRAY, egui::Color32::LIGHT_BLUE);
        let rows = |n: usize| (0..n).map(|i| (i as f32 * 20.0, idle)).collect::<Vec<_>>();
        let side = panel(&rows(20), all);
        let main = panel(
            &[(0.0, egui::Color32::BLACK)],
            egui::Rect::from_min_size(egui::pos2(300.0, 0.0), egui::vec2(500.0, 600.0)),
        );
        let before = vec![side.clone(), main.clone()];

        let mut hovered = rows(20);
        hovered[5].1 = lit;
        let rect = part_after(&before, &[panel(&hovered, all), main.clone()]).unwrap();
        assert_eq!(
            rect,
            egui::Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(300.0, 20.0))
        );

        // A row inserted after the fifth shifts the vertices of all after it.
        let mut added = rows(20);
        added.insert(5, (100.0, lit));
        let rect = part_after(&before, &[panel(&added, all), main.clone()]).unwrap();
        assert!(rect.height() <= 20.0, "{rect:?}");

        let mut fewer = rows(20);
        fewer.remove(5);
        assert!(part_after(&before, &[panel(&fewer, all), main.clone()]).is_some());

        let tip = panel(&[(200.0, egui::Color32::WHITE)], all);
        let rect = part_after(&before, &[side.clone(), main.clone(), tip]).unwrap();
        assert_eq!(rect.height(), 20.0);

        // Most of the window changed (the side panel scrolled, a 300 × 600 of 800 × 600):
        // drawn whole, which costs no more.
        let moved: Vec<_> = (0..30).map(|i| (i as f32 * 20.0 + 3.0, idle)).collect();
        let mut wide = main.clone();
        wide.clip_rect = all;
        if let egui::epaint::Primitive::Mesh(m) = &mut wide.primitive {
            m.vertices.iter_mut().for_each(|v| v.pos.x += 400.0);
        }
        assert_eq!(part_after(&before, &[panel(&moved, all), wide]), None);
    }

    /// Only what reaches into the rect is drawn, cut to it, on ground cleared first.
    #[test]
    fn a_part_is_its_ground_and_what_reaches_into_it() {
        let all = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let far = egui::Rect::from_min_size(egui::pos2(500.0, 0.0), egui::vec2(100.0, 100.0));
        let frame = [panel(&[(0.0, egui::Color32::RED)], all), panel(&[], far)];
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(50.0, 5.0));
        let part = only(&frame, rect, egui::Color32::BLACK);
        assert_eq!(part.len(), 2, "the far clip left out");
        assert!(part.iter().all(|p| p.clip_rect == rect));
        let egui::epaint::Primitive::Mesh(ground) = &part[0].primitive else {
            panic!()
        };
        assert!(
            ground
                .vertices
                .iter()
                .all(|v| v.color == egui::Color32::BLACK)
        );
    }
}
