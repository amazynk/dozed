//! The far side of the dry-run seam.
//!
//! The planner produces intents without knowing what will receive them; this is
//! the trait that receives them. It lives in core rather than beside the cgroup
//! implementation because the seam is a domain concept, not a Linux one — the
//! daemon is wired identically in both modes and would be wired identically
//! against a different platform's actuator (`DESIGN.md` §3).
//!
//! Keeping it here is also the cheap half of ever supporting a second platform.
//! The expensive half is the sources; the trait costs nothing to place
//! correctly now and is awkward to move once anything else depends on where it
//! lives.

use crate::app::AppKey;
use crate::intent::{Intent, Outcome};

/// Applies intents. Implemented once for real and once for dry-run so that the
/// daemon can be wired identically in both modes.
pub trait Actuator {
    fn apply(&mut self, intent: &Intent) -> Outcome;

    /// Restore everything currently held. Used for panic thaw, shutdown, and
    /// the startup sweep, so it must try every target even if one fails.
    fn restore_all(&mut self) -> Vec<(AppKey, Outcome)>;
}

/// Records intents and touches nothing. The default mode.
///
/// Not a mock and not test scaffolding: this is what runs in dry-run, so the
/// journal a user reads before switching live mode on comes from the identical
/// intent stream with only this at the end of it.
#[derive(Debug, Default)]
pub struct DryRunActuator {
    pub recorded: Vec<Intent>,
}

impl Actuator for DryRunActuator {
    fn apply(&mut self, intent: &Intent) -> Outcome {
        self.recorded.push(intent.clone());
        Outcome::WouldHave
    }

    fn restore_all(&mut self) -> Vec<(AppKey, Outcome)> {
        Vec::new()
    }
}
