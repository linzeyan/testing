//! Whether a finished frame is worth drawing.
//!
//! eframe draws and presents every frame it runs, and it runs one for every pointer move
//! and another after it (egui asks for a second once the pointer has moved). Most come to
//! the picture the window already shows. On a GPU that costs little; a software renderer
//! (WARP, on a VDI with no vGPU) rasterises the whole window on the CPU for each of them,
//! on every core: moving the pointer over the window was a CPU spike for as long as it
//! moved. `App::wants_paint`, the hook added to the vendored eframe, asks here.

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
            size: window.inner_rect.map(|r| r.size()),
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
    /// The least time between two drawn frames; None draws a changed one at once.
    pub gap: Option<Duration>,
    /// Set by the renderer when a frame couldn't be shown (the surface was outdated, lost
    /// or timed out): what is held here as on screen isn't.
    pub failed: Arc<AtomicBool>,
    shown: Option<(u64, Target)>,
    /// When the last drawn frame went to the renderer, and when the last frame of either
    /// kind left here.
    drawn: Option<Instant>,
    left: Option<Instant>,
    /// What the last frame was told to wait.
    waited: Duration,
}

impl Painted {
    /// Whether to draw the frame that came to `primitives`, and how long to wait first:
    /// `gap` before a drawn one, `gap` or `PACE` after one that isn't. `must`: it is drawn
    /// whatever is said here.
    pub fn judge(
        &mut self,
        primitives: &[egui::ClippedPrimitive],
        target: Target,
        must: bool,
        now: Instant,
    ) -> (bool, Duration) {
        FRAMES.fetch_add(1, Relaxed);
        let frame = picture(primitives).map(|p| (p, target));
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
            return (false, wait);
        }
        let wait = self.gap.map_or(Duration::ZERO, |gap| rest(gap, self.drawn));
        (self.shown, self.drawn, self.left) = (frame, Some(now + wait), Some(now + wait));
        self.waited = wait;
        DRAWN.fetch_add(1, Relaxed);
        (true, wait)
    }

    /// What the last frame waited here: eframe counts it into that frame's time.
    pub fn waited(&self) -> Duration {
        self.waited
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
        assert!(painted.judge(&mesh(0.0), target(800.0), false, now).0);
        assert!(!painted.judge(&mesh(0.0), target(800.0), false, now).0);
        assert!(painted.judge(&mesh(1.0), target(800.0), false, now).0);
        assert!(!painted.judge(&mesh(1.0), target(800.0), false, now).0);
        // Back to an earlier picture is a change too: it isn't what the window shows.
        assert!(painted.judge(&mesh(0.0), target(800.0), false, now).0);
    }

    /// A resized, restored or uncovered window may have lost what was drawn in it, and a
    /// frame that failed to show never got there: the same picture is drawn again.
    #[test]
    fn a_picture_that_may_be_gone_is_drawn_again() {
        let mut painted = Painted::default();
        let now = Instant::now();
        assert!(painted.judge(&mesh(0.0), target(800.0), false, now).0);
        assert!(painted.judge(&mesh(0.0), target(801.0), false, now).0);
        let mut restored = target(801.0);
        restored.minimized = Some(true);
        assert!(painted.judge(&mesh(0.0), restored, false, now).0);
        assert!(painted.judge(&mesh(0.0), target(801.0), false, now).0);
        assert!(!painted.judge(&mesh(0.0), target(801.0), false, now).0);
        painted.failed.store(true, Relaxed);
        assert!(painted.judge(&mesh(0.0), target(801.0), false, now).0);
        assert!(!painted.judge(&mesh(0.0), target(801.0), false, now).0);
        // eframe draws it anyway (a texture changed under the same triangles).
        assert!(painted.judge(&mesh(0.0), target(801.0), true, now).0);
        painted.all = true;
        assert!(painted.judge(&mesh(0.0), target(801.0), false, now).0);
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
            painted.judge(&mesh(0.0), target(800.0), false, start),
            (true, ms(0))
        );
        let at = start + ms(10);
        assert_eq!(
            painted.judge(&mesh(1.0), target(800.0), false, at),
            (true, ms(23))
        );
        // Drawn at 33 ms; the next, 7 ms after that, waits for 66 ms.
        let at = start + ms(40);
        assert_eq!(
            painted.judge(&mesh(2.0), target(800.0), false, at),
            (true, ms(26))
        );
        let at = start + ms(200);
        assert_eq!(
            painted.judge(&mesh(3.0), target(800.0), false, at),
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
                painted.judge(&mesh(0.0), target(800.0), false, start),
                (true, ms(0))
            );
            let at = start + ms(2);
            assert_eq!(
                painted.judge(&mesh(0.0), target(800.0), false, at),
                (false, pace - ms(2))
            );
            // That one left at `pace`; the next comes 1 ms after it did.
            let at = start + pace + ms(1);
            assert_eq!(
                painted.judge(&mesh(0.0), target(800.0), false, at),
                (false, pace - ms(1))
            );
            // After a pause there is nothing to wait for.
            let at = start + ms(500);
            assert_eq!(
                painted.judge(&mesh(0.0), target(800.0), false, at),
                (false, ms(0))
            );
        }
        // Drawing every frame, as eframe by itself: the present does the waiting.
        let mut painted = Painted {
            all: true,
            ..Default::default()
        };
        let start = Instant::now();
        painted.judge(&mesh(0.0), target(800.0), false, start);
        assert_eq!(
            painted.judge(&mesh(0.0), target(800.0), false, start),
            (true, ms(0))
        );
    }
}
