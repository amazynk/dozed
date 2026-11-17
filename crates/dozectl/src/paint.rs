//! Colour, and the several good reasons not to use it.
//!
//! `dozectl` prints tables people read at a glance and pipe into other things,
//! and those two audiences want opposite output. The rule here is that colour is
//! **carried by meaning, never by decoration**: the same red that marks a
//! degraded exemption source marks a lost focus source, because they are the
//! same kind of news. Nothing is coloured merely because a column exists.
//!
//! ## When it is off
//!
//! Off is the safe answer, so every one of these wins:
//!
//! * `--no-color`, which is the flag people already type.
//! * `NO_COLOR` set to anything non-empty — the cross-tool convention
//!   (<https://no-color.org>), and worth honouring precisely because somebody
//!   who set it once should not have to discover this program's own spelling.
//! * `TERM=dumb`, which is a terminal saying it cannot.
//! * `--json`, because that output has a consumer that is not a person.
//! * stdout not being a terminal. This is the one that matters most in practice:
//!   `dozectl apps > apps.txt` and `dozectl status | grep audio` have to keep
//!   producing exactly what they produced before this module existed, and a
//!   file full of `\x1b[32m` would be a regression dressed as a feature.
//!
//! ## Why a global rather than a parameter
//!
//! The decision is made once, from the command line and the environment, and
//! nothing after that can change it — so threading it through every renderer
//! would be ceremony around a constant. Tests set it explicitly through
//! [`force`], which is also how the colouring gets tested at all: the suite runs
//! without a terminal, so the default there is off and every existing assertion
//! about plain text stays true.
//!
//! ## Padding
//!
//! Colour goes on **after** a cell is padded, never before. `{:<12}` counts
//! bytes, and an escape sequence is bytes — so colouring first and padding
//! second shifts every column right by the width of the escape and produces a
//! table that looks fine until somebody turns colour off.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Decide once, from the flags and the environment.
pub fn configure(no_color: bool, json: bool) {
    use std::io::IsTerminal as _;

    let on = !no_color
        && !json
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").is_ok_and(|t| t != "dumb")
        && std::io::stdout().is_terminal();
    ENABLED.store(on, Ordering::Relaxed);
}

/// Serialises the tests that move the global. Rust runs them on threads of one
/// process, so without this a test asserting "colour is off" fails whenever it
/// happens to overlap the one that turns it on — which it did, first run.
#[cfg(test)]
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Colour forced on or off for as long as the returned guard lives, and off
/// again after it. Tests only — see the module note.
#[cfg(test)]
#[must_use]
pub fn force(on: bool) -> Forced {
    // A poisoned lock means some other test panicked while holding it. That is
    // that test's failure to report, not a reason to fail this one too.
    let guard = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    ENABLED.store(on, Ordering::Relaxed);
    Forced(guard)
}

#[cfg(test)]
pub struct Forced(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl Drop for Forced {
    fn drop(&mut self) {
        // Off is the default everywhere else, including in every other test in
        // this binary that renders a table.
        ENABLED.store(false, Ordering::Relaxed);
    }
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn wrap(code: &str, s: &str) -> String {
    if enabled() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_owned() }
}

/// Something is wrong and somebody has to do something: a source that cannot
/// fire, a focus source that has gone, a daemon that is not there.
pub fn bad(s: &str) -> String {
    wrap("31", s)
}

/// Working, and worth being able to find in a column of twenty lines.
pub fn good(s: &str) -> String {
    wrap("32", s)
}

/// Half working, or acting: degraded sources, and live mode.
///
/// Live mode shares the colour deliberately. It is not a fault, but it is the
/// state where this program can stop your applications, and that deserves the
/// same "look at me" as a fault rather than the calm of a normal reading.
pub fn warn(s: &str) -> String {
    wrap("33", s)
}

/// Held: dozing, throttled. The tool doing its job.
pub fn held(s: &str) -> String {
    wrap("36", s)
}

/// A reason rather than a state — the exemptions and notes columns.
pub fn note(s: &str) -> String {
    wrap("34", s)
}

/// Structure: headers, and the labels down the left of `status`.
pub fn head(s: &str) -> String {
    wrap("1", s)
}

/// Present but not the point: dry-run, a figure nobody measured yet.
pub fn faint(s: &str) -> String {
    wrap("2", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property every one of these has to have: with colour off, the output
    /// is byte for byte what it was before this module existed.
    #[test]
    fn colour_off_changes_nothing_at_all() {
        let _off = force(false);
        for f in [bad, good, warn, held, note, head, faint] {
            assert_eq!(f("dozing"), "dozing");
        }
    }

    #[test]
    fn colour_on_wraps_and_always_resets() {
        let _on = force(true);
        let s = bad("LOST");
        assert!(s.starts_with("\x1b["), "{s:?}");
        assert!(s.ends_with("\x1b[0m"), "a sequence left open bleeds into the shell: {s:?}");
        assert!(s.contains("LOST"));
    }

    /// Padding first, colour second. The other order counts escape bytes as
    /// column width and shifts everything after it.
    #[test]
    fn a_coloured_cell_is_padded_to_its_visible_width() {
        let _on = force(true);
        let cell = held(&format!("{:<12}", "dozing"));
        // Six visible characters plus six spaces, whatever the escapes weigh.
        let visible: String = cell.replace("\x1b[36m", "").replace("\x1b[0m", "");
        assert_eq!(visible.len(), 12, "{cell:?}");
    }

    /// The guard puts it back, so no other test in this binary ever renders a
    /// table with escapes in it by accident.
    #[test]
    fn the_global_goes_back_to_off_afterwards() {
        {
            let _on = force(true);
            assert!(enabled());
        }
        assert!(!enabled());
    }
}
