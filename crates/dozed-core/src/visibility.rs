//! Deciding what the user can actually see, once, for every platform.
//!
//! ## Why this is here and not in the helpers
//!
//! `DESIGN.md` §5 defines three visibility states, and getting them wrong in
//! either direction is expensive: call a visible window hidden and we freeze
//! something the user is looking at; call a hidden one visible and the tool does
//! nothing. With a KWin script, a GNOME extension and a native X11 source there
//! would be three implementations of that rule, in two languages, and no way to
//! notice when they drifted apart.
//!
//! The objection to centralising it was that helpers would have to *stream*
//! geometry, and `frameGeometryChanged` fires at frame rate throughout a drag —
//! precisely the signal the KWin helper refuses to connect, because a previous
//! tool on this machine leaked handlers into ~1035 wakeups/s.
//!
//! That objection turns out not to apply. The KWin helper never subscribed to
//! geometry in the first place: it *reads* `frameGeometry` and `stackingOrder`
//! inside its occlusion check, at snapshot time, and publishes on the discrete
//! events that change occlusion (maximise, tile, move-finished, restack). Moving
//! the computation here means putting the geometry it already read into the
//! payload it already sends. No new signal connections, the same publish
//! triggers, and exactly the same staleness between publishes as before.
//!
//! So the helpers report facts and this decides. One implementation, one set of
//! tests, and a new platform gets the rule for free by filling in
//! [`WindowFacts`].

use serde::{Deserialize, Serialize};

use crate::event::Visibility;

/// A window rectangle in layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Whether `self` completely contains `other`.
    #[must_use]
    pub const fn covers(&self, other: &Self) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && self.x + self.w >= other.x + other.w
            && self.y + self.h >= other.y + other.h
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// One window exactly as a compositor reports it, before any interpretation.
///
/// Every field is a fact the platform can answer directly. Anything requiring
/// judgement — above all occlusion — is [`resolve`]'s job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowFacts {
    pub pid: u32,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub desktop_file: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub minimized: bool,
    /// On the workspace/desktop/activity the user is currently looking at.
    /// Defaults to `true` so a platform that cannot tell never reports a window
    /// as hidden on the strength of a field it did not fill in.
    #[serde(default = "yes")]
    pub on_current_desktop: bool,
    /// Position and size. `None` when the platform will not say, which disables
    /// the occlusion check for this window rather than guessing.
    #[serde(default)]
    pub geometry: Option<Rect>,
    /// Index in the bottom-to-top stacking order: higher is nearer the user.
    #[serde(default)]
    pub stack: u32,
    /// Which monitor. Windows on different outputs cannot occlude each other.
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub attention: bool,
    /// Not opaque, so it cannot hide anything behind it.
    #[serde(default)]
    pub translucent: bool,
    /// A protocol-1 helper's own verdict.
    ///
    /// Kept so an older KWin script still works against a newer daemon: if it
    /// computed visibility itself and sent no geometry, we take its word rather
    /// than inventing an answer.
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

const fn yes() -> bool {
    true
}

impl Default for WindowFacts {
    fn default() -> Self {
        Self {
            pid: 0,
            app_id: String::new(),
            desktop_file: String::new(),
            focused: false,
            minimized: false,
            on_current_desktop: true,
            geometry: None,
            stack: 0,
            output: String::new(),
            fullscreen: false,
            attention: false,
            translucent: false,
            visibility: None,
        }
    }
}

/// Decide the visibility of every window in one snapshot.
///
/// The order of the tests is the order of certainty: focus and minimisation are
/// facts, occlusion is a computation, and anything we cannot compute resolves to
/// the state that is *not* freezable.
#[must_use]
pub fn resolve(windows: &[WindowFacts]) -> Vec<Visibility> {
    windows.iter().map(|w| resolve_one(w, windows)).collect()
}

fn resolve_one(w: &WindowFacts, all: &[WindowFacts]) -> Visibility {
    if w.focused {
        return Visibility::Focused;
    }
    // Definite, and cheap. A minimised window is hidden whatever the geometry
    // says, and a window on another workspace likewise.
    if w.minimized || !w.on_current_desktop {
        return Visibility::Hidden;
    }
    let Some(mine) = w.geometry.filter(|g| !g.is_empty()) else {
        // No geometry: we cannot judge occlusion. Take an older helper's own
        // verdict if it gave one, and otherwise resolve to on-screen — never to
        // hidden, which is the only answer that could get something frozen.
        return w.visibility.unwrap_or(Visibility::VisibleUnfocused);
    };
    if is_occluded(w, &mine, all) {
        return Visibility::Hidden;
    }
    Visibility::VisibleUnfocused
}

/// Whether some single window above `w` completely covers it.
///
/// Deliberately not union coverage of several overlapping windows: that is a far
/// more expensive computation for a case that barely occurs, and getting it
/// wrong means freezing something the user can see. Single-window containment
/// catches what actually matters — a maximised browser on top of everything else
/// on one monitor.
fn is_occluded(w: &WindowFacts, mine: &Rect, all: &[WindowFacts]) -> bool {
    all.iter().any(|above| {
        if std::ptr::eq(above, w) || above.stack <= w.stack {
            return false;
        }
        if above.minimized || !above.on_current_desktop || above.translucent {
            return false;
        }
        // Two windows on different monitors never occlude, whatever their
        // coordinates say — outputs can share a coordinate space.
        if above.output != w.output {
            return false;
        }
        above.geometry.is_some_and(|g| !g.is_empty() && g.covers(mine))
    })
}

/// Fold per-window visibilities into one per app.
///
/// The maximum over an app's windows (§5): any focused window means the user is
/// looking at the app. `Ord` on [`Visibility`] runs
/// `Focused < VisibleUnfocused < Hidden`, so the most-visible window is the
/// minimum.
#[must_use]
pub fn most_visible(states: impl IntoIterator<Item = Visibility>) -> Option<Visibility> {
    states.into_iter().min()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared fixture: every platform's source is held to
    /// these, so KWin, GNOME and X11 cannot silently disagree about §5.
    fn win(pid: u32, stack: u32, geom: Rect) -> WindowFacts {
        WindowFacts {
            pid,
            stack,
            geometry: Some(geom),
            output: "DP-1".into(),
            ..Default::default()
        }
    }

    const SCREEN: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };

    #[test]
    fn a_focused_window_is_focused_whatever_else_is_true() {
        // Even minimised-and-focused, which should not happen but must not
        // resolve to freezable if it does.
        let mut w = win(1, 0, SCREEN);
        w.focused = true;
        w.minimized = true;
        assert_eq!(resolve(&[w])[0], Visibility::Focused);
    }

    #[test]
    fn a_lone_window_is_visible_not_hidden() {
        assert_eq!(resolve(&[win(1, 0, SCREEN)])[0], Visibility::VisibleUnfocused);
    }

    #[test]
    fn minimised_and_other_workspace_are_hidden() {
        let mut m = win(1, 0, SCREEN);
        m.minimized = true;
        let mut o = win(2, 0, SCREEN);
        o.on_current_desktop = false;
        assert_eq!(resolve(&[m, o]), vec![Visibility::Hidden, Visibility::Hidden]);
    }

    #[test]
    fn a_maximised_window_on_top_hides_the_one_behind() {
        // The case the whole occlusion check exists for.
        let behind = win(1, 0, SCREEN);
        let front = win(2, 1, SCREEN);
        assert_eq!(
            resolve(&[behind, front]),
            vec![Visibility::Hidden, Visibility::VisibleUnfocused]
        );
    }

    #[test]
    fn a_window_below_does_not_hide_the_one_above() {
        let front = win(1, 5, SCREEN);
        let behind = win(2, 1, SCREEN);
        assert_eq!(resolve(&[front, behind])[0], Visibility::VisibleUnfocused);
    }

    #[test]
    fn partial_overlap_is_not_occlusion() {
        // Two half-covered windows are both still visible; only full containment
        // counts, because a partly visible window frozen mid-redraw reads as a
        // crash just as loudly.
        let behind = win(1, 0, SCREEN);
        let front = win(2, 1, Rect::new(0, 0, 960, 1080));
        assert_eq!(resolve(&[behind, front])[0], Visibility::VisibleUnfocused);
    }

    #[test]
    fn the_second_monitor_is_the_case_that_started_all_this() {
        // §5's opening example: unfocused but fully visible on another screen.
        // Freezing it leaves a stale rectangle, which reads as a crash.
        let mut other_screen = win(1, 0, SCREEN);
        other_screen.output = "HDMI-1".into();
        let front = win(2, 9, SCREEN); // covers the same coordinates on DP-1
        assert_eq!(
            resolve(&[other_screen, front])[0],
            Visibility::VisibleUnfocused,
            "windows on different outputs must never occlude each other"
        );
    }

    #[test]
    fn a_translucent_window_hides_nothing() {
        let behind = win(1, 0, SCREEN);
        let mut front = win(2, 1, SCREEN);
        front.translucent = true;
        assert_eq!(resolve(&[behind, front])[0], Visibility::VisibleUnfocused);
    }

    #[test]
    fn a_minimised_window_on_top_hides_nothing() {
        let behind = win(1, 0, SCREEN);
        let mut front = win(2, 1, SCREEN);
        front.minimized = true;
        assert_eq!(resolve(&[behind, front])[0], Visibility::VisibleUnfocused);
    }

    #[test]
    fn missing_geometry_never_resolves_to_freezable() {
        // Fail towards "never touch this": a platform that will not tell us
        // where a window is must not thereby get it frozen.
        let mut w = WindowFacts { pid: 1, ..Default::default() };
        assert_eq!(resolve(&[w.clone()])[0], Visibility::VisibleUnfocused);

        // Except where it is definitely hidden for a reason geometry cannot
        // change.
        w.minimized = true;
        assert_eq!(resolve(&[w])[0], Visibility::Hidden);
    }

    #[test]
    fn an_old_helper_that_computed_visibility_itself_is_still_believed() {
        // Protocol 1: a KWin script that predates this module sends no geometry
        // but does send its own verdict.
        let w = WindowFacts { pid: 1, visibility: Some(Visibility::Hidden), ..Default::default() };
        assert_eq!(resolve(&[w])[0], Visibility::Hidden);
    }

    #[test]
    fn a_zero_sized_window_neither_hides_nor_is_hidden() {
        let snapshot = [win(1, 0, SCREEN), win(2, 1, Rect::new(0, 0, 0, 0))];
        let got = resolve(&snapshot);
        assert_eq!(got[0], Visibility::VisibleUnfocused, "an empty window hides nothing");
        assert_eq!(
            got[1],
            Visibility::VisibleUnfocused,
            "and it is not itself hidden by the window below it"
        );
    }

    #[test]
    fn an_app_is_as_visible_as_its_most_visible_window() {
        assert_eq!(
            most_visible([Visibility::Hidden, Visibility::Focused, Visibility::Hidden]),
            Some(Visibility::Focused)
        );
        assert_eq!(
            most_visible([Visibility::Hidden, Visibility::VisibleUnfocused]),
            Some(Visibility::VisibleUnfocused)
        );
        assert_eq!(most_visible([]), None);
    }
}
