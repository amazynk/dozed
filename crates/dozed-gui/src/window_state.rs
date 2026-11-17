//! Where the dashboard window was last time.
//!
//! A status window is opened, glanced at and closed dozens of times, and one
//! that comes back at 880x640 in the middle of the screen every time is one you
//! resize dozens of times. Nothing here is clever; it exists because the
//! alternative is a small irritation repeated forever.
//!
//! Rolled by hand rather than by enabling eframe's `persistence` feature, which
//! would pull in `ron` and `directories` to write a file this crate can write
//! with the `serde_json` it already has, into the state directory `dozed-core`
//! already defines. The daemon keeps its own state the same way (`frozen.json`,
//! `trial.json`), so this is the house style rather than a new one.

use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

/// How long the window has to hold still before its geometry is written.
///
/// A drag-resize repaints continuously, and writing on every frame would turn a
/// two-second resize into a hundred file writes. It also has to be short enough
/// that a session ending abruptly does not lose the change.
const SETTLE: Duration = Duration::from_millis(750);

/// The size, place and zoom of the window, as last seen.
///
/// `width` and `height` are **logical points**, the unit
/// [`egui::ViewportBuilder::with_inner_size`] takes — deliberately not the ui
/// points `ViewportInfo::inner_rect` reports, which are divided by the zoom
/// factor. Storing what egui reports and handing it straight back would halve
/// the window on every restart at zoom 2, and double it at zoom 0.5. See
/// [`read`], which does the conversion in one place.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
    /// Outer position. `None` on Wayland, where a client cannot ask to be put
    /// anywhere and cannot find out where it is — see [`Self::restore`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    /// egui's zoom factor: how large the text and every control is drawn.
    ///
    /// Set with ctrl+plus and ctrl+minus, and reset with ctrl+0. This is the
    /// setting most worth remembering of the three, because it is not a
    /// preference about *this window* but about the person reading it — a
    /// dashboard that forgets it is one somebody has to make legible again every
    /// single time they open it.
    ///
    /// Defaulted on read so a `window.json` written before this existed still
    /// loads, rather than being discarded as unparseable and taking the size
    /// with it.
    #[serde(default = "one")]
    pub zoom: f32,
}

fn one() -> f32 {
    1.0
}

impl Default for WindowState {
    fn default() -> Self {
        Self { width: 880.0, height: 640.0, x: None, y: None, zoom: 1.0 }
    }
}

#[must_use]
pub fn path() -> std::path::PathBuf {
    dozed_core::paths::state_dir().join("window.json")
}

impl WindowState {
    /// What to open with. Falls back to the shipped size on anything unreadable.
    #[must_use]
    pub fn load() -> Self {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .map_or_else(Self::default, Self::sane)
    }

    /// Clamp to something a person can actually use.
    ///
    /// A geometry file is user-writable and survives hardware changes, so it can
    /// legitimately hold a size from a monitor that is no longer attached. The
    /// failure it prevents is the unrecoverable one: a window restored at 12x8
    /// pixels, or at x = 6000 on a screen that is now 1920 wide, cannot be
    /// dragged back by the person it happened to.
    #[must_use]
    fn sane(mut self) -> Self {
        if !self.width.is_finite() || !self.height.is_finite() {
            return Self::default();
        }
        self.width = self.width.clamp(560.0, 8000.0);
        self.height = self.height.clamp(360.0, 8000.0);
        self.x = self.x.filter(|v| v.is_finite() && v.abs() < 20_000.0);
        self.y = self.y.filter(|v| v.is_finite() && v.abs() < 20_000.0);
        // Wider than anyone would choose deliberately, narrow enough that a
        // corrupt value cannot open a window whose text is one pixel tall. The
        // bound is generous because ctrl+0 resets zoom, so unlike the size this
        // one is recoverable from inside the window.
        self.zoom = if self.zoom.is_finite() { self.zoom.clamp(0.4, 5.0) } else { 1.0 };
        self
    }

    /// Apply to the window being built.
    ///
    /// Position is requested and may simply not happen: under Wayland a client
    /// has no way to place its own surface, by design, and the compositor
    /// decides. That is not worth working around and not worth hiding either —
    /// the size, which is the part people actually notice, is restored on both.
    #[must_use]
    pub fn restore(self, mut builder: egui::ViewportBuilder) -> egui::ViewportBuilder {
        builder = builder.with_inner_size([self.width, self.height]);
        if let (Some(x), Some(y)) = (self.x, self.y) {
            builder = builder.with_position([x, y]);
        }
        builder
    }

    fn write(self) {
        let p = path();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string(&self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&p, json) {
                    // Never fatal, and never a dialog: a dashboard that cannot
                    // remember its size is a dashboard, and one that interrupts
                    // you about it is a nuisance.
                    eprintln!("dozed-gui: could not save the window geometry: {e}");
                }
            }
            Err(e) => eprintln!("dozed-gui: could not serialise the window geometry: {e}"),
        }
    }
}

/// Watches the live window and writes it out once it has stopped moving.
#[derive(Debug, Default)]
pub struct Tracker {
    /// The geometry last written, so an unchanged window is never rewritten.
    saved: Option<WindowState>,
    /// A change seen but not yet written, and when it was seen.
    pending: Option<(WindowState, Instant)>,
}

impl Tracker {
    #[must_use]
    pub fn new(initial: WindowState) -> Self {
        Self { saved: Some(initial), pending: None }
    }

    /// Call once a frame. Cheap, and silent when nothing has moved.
    pub fn observe(&mut self, ctx: &egui::Context, now: Instant) {
        let Some(current) = read(ctx) else { return };

        // Minimised windows report a size nobody chose, and restoring it would
        // mean the dashboard reopens as a sliver after being minimised once.
        if ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            return;
        }

        if self.saved == Some(current) {
            self.pending = None;
            return;
        }
        match self.pending {
            Some((pending, _)) if pending == current => {}
            _ => self.pending = Some((current, now)),
        }
        if let Some((geom, seen)) = self.pending
            && now.duration_since(seen) >= SETTLE
        {
            geom.write();
            self.saved = Some(geom);
            self.pending = None;
        }
    }

    /// Write whatever is outstanding, on the way out.
    pub fn flush(&mut self, ctx: &egui::Context) {
        if let Some(current) = read(ctx)
            && self.saved != Some(current)
        {
            current.write();
            self.saved = Some(current);
        }
    }
}

/// The window as egui currently sees it, or `None` before the first frame has
/// been laid out.
///
/// **The unit conversion is the whole reason this is a function.** egui reports
/// `inner_rect` in *ui points*, which are physical pixels divided by
/// `pixels_per_point` — and `pixels_per_point` folds in the zoom factor, while
/// the logical size `with_inner_size` takes does not. Saving what egui reports
/// and handing it straight back therefore shrinks the window by the zoom factor
/// on every restart: at zoom 2 an 880-point window is stored as 440 and reopens
/// half the size, then a quarter, then an eighth. Multiplying by the zoom here
/// converts ui points back into the logical points the builder wants, in the one
/// place both numbers are in scope.
fn read(ctx: &egui::Context) -> Option<WindowState> {
    let zoom = ctx.zoom_factor();
    // `viewport_rect`, **not** `ViewportInfo::inner_rect`, and this is the whole
    // bug the first version had. `inner_rect` is built by `egui_winit` from
    // `Window::inner_position()`, which returns `NotSupportedError` on Wayland
    // by design — a client there is not told where it is. So `inner_rect` is
    // permanently `None` on the display server most people run this on, the
    // read bailed out, and nothing was ever saved: no size, and no zoom either,
    // because both travelled through the same early return.
    //
    // `viewport_rect` is the egui canvas and always exists. Not `content_rect`,
    // which egui's own deprecation notice suggests and which is right for laying
    // out widgets and wrong here: it subtracts insets for status bars and display
    // notches, so on a platform with either, the window would shrink by the size
    // of the notch on every restart.
    let size = ctx.viewport_rect().size();
    if !size.is_finite() || size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    ctx.input(|i| {
        // Position stays best-effort: it is genuinely unavailable on Wayland,
        // and unlike the size there is no substitute for it.
        let pos = i.viewport().outer_rect.map(|r| r.min);
        Some(
            WindowState {
                width: size.x * zoom,
                height: size.y * zoom,
                x: pos.map(|p| p.x * zoom),
                y: pos.map(|p| p.y * zoom),
                zoom,
            }
            .sane(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_from_a_monitor_you_no_longer_have_is_clamped() {
        // The unrecoverable failure this prevents: a window restored at 12x8
        // cannot be dragged back to a usable size by the person it happened to,
        // and the file it came from is not one they know exists.
        let tiny = WindowState { width: 12.0, height: 8.0, ..Default::default() }.sane();
        assert_eq!((tiny.width, tiny.height), (560.0, 360.0));

        let offscreen =
            WindowState { width: 900.0, height: 700.0, x: Some(90_000.0), y: Some(4.0), zoom: 1.0 }
                .sane();
        assert_eq!(offscreen.x, None, "an impossible position is dropped, not honoured");
        assert_eq!(offscreen.y, Some(4.0));
    }

    #[test]
    fn a_corrupt_geometry_is_the_shipped_default_rather_than_an_error() {
        // It is a convenience file. Nothing about it is worth failing to start
        // a window over.
        let nonsense = WindowState { width: f32::NAN, height: 600.0, ..Default::default() }.sane();
        assert_eq!(nonsense, WindowState::default());
    }

    #[test]
    fn a_zoomed_window_does_not_shrink_a_little_every_time_it_reopens() {
        // The trap `read` exists to close, in the arithmetic it does. egui
        // reports `inner_rect` in ui points — physical pixels over
        // `pixels_per_point`, which folds in the zoom — while `with_inner_size`
        // takes logical points, which does not. Storing what egui reports and
        // handing it back would halve an 880-point window at zoom 2, then halve
        // it again on the next launch, until it hits the minimum.
        let zoom = 2.0_f32;
        let reported_ui_points = 880.0_f32 / zoom; // what egui would say
        let stored = reported_ui_points * zoom; // what `read` writes
        assert!((stored - 880.0).abs() < f32::EPSILON, "stored {stored}");

        // And the same size survives an arbitrary number of round trips.
        let mut w = 880.0_f32;
        for _ in 0..5 {
            w = (w / zoom) * zoom;
        }
        assert!((w - 880.0).abs() < 0.01, "after five restarts: {w}");
    }

    #[test]
    fn a_geometry_file_written_before_zoom_existed_still_loads() {
        // Upgrading must not throw away somebody's window size just because the
        // file has one fewer field than this build writes.
        let old = r#"{"width":1200.0,"height":800.0}"#;
        let parsed: WindowState = serde_json::from_str(old).expect("the old shape still parses");
        assert_eq!((parsed.width, parsed.height), (1200.0, 800.0));
        assert_eq!(parsed.zoom, 1.0, "and comes back at the ordinary zoom");
    }

    #[test]
    fn an_unusable_zoom_is_clamped_rather_than_honoured() {
        let huge = WindowState { zoom: 400.0, ..Default::default() }.sane();
        assert_eq!(huge.zoom, 5.0);
        let broken = WindowState { zoom: f32::NAN, ..Default::default() }.sane();
        assert_eq!(broken.zoom, 1.0);
    }

    #[test]
    fn the_default_is_the_size_the_window_shipped_with() {
        // So that deleting the file, or never having written one, is exactly the
        // behaviour before any of this existed.
        let d = WindowState::default();
        assert_eq!((d.width, d.height), (880.0, 640.0));
        assert_eq!((d.x, d.y), (None, None));
    }
}
