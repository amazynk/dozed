//! Intents: the seam between deciding and doing.
//!
//! The planner only ever produces [`Intent`]s. In dry-run the actuator records
//! them and stops; in live mode it executes them. Because both modes consume
//! the identical stream, the dry-run journal is not a simulation that can drift
//! from reality — it is literally what live mode would have done.

use serde::{Deserialize, Serialize};

use crate::app::AppKey;
use crate::explain::DecisionInputs;
use crate::grace::Grace;
use crate::time::MonoTime;

/// What to do to an app.
///
/// Freeze-or-run is too blunt for a conservative tool, so this is a ladder.
/// [`Action::Throttle`] is the recommended rung for anything network-facing:
/// a chat app keeps its connection and still notifies, it just cannot spin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Undo everything: unfreeze, clear any CPU cap, restore priority.
    Restore,
    /// Cap the app at `pct` percent of one core via cgroup `cpu.max`.
    Throttle { pct: u8 },
    /// Fully stop the app via cgroup `cgroup.freeze`, or `SIGSTOP` over the
    /// process tree where no scope exists.
    Freeze {
        /// Also ask the kernel to reclaim the frozen app's pages. Freezing does
        /// not free memory on its own; this is the optional bonus.
        reclaim: bool,
    },
}

impl Action {
    #[must_use]
    pub const fn is_restore(self) -> bool {
        matches!(self, Self::Restore)
    }

    /// Verb for log lines and the timeline, phrased so that the dry-run prefix
    /// ("would ") reads naturally in front of it.
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::Throttle { .. } => "throttle",
            Self::Freeze { .. } => "doze",
        }
    }
}

/// Why the planner wants something done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasonKind {
    /// A configured rule matched and its grace expired.
    RuleFired,
    /// The app came back on screen.
    BecameVisible,
    /// The compositor was asked to raise this app.
    Activated,
    /// An exemption applies right now.
    Exempt,
    /// No rule matches this app, so it must be left running. The resting state
    /// for anything the user has not written a rule for.
    NoRule,
    /// A hard safety invariant, not overridable by config.
    Safety,
    /// The CLI or GUI asked for this explicitly.
    UserRequested,
    /// We lost sight of focus, so we cannot safely hold anything frozen.
    FocusSourceLost,
    /// The system is about to suspend.
    SystemSleep,
    /// Shutting down.
    Shutdown,
    /// The mode changed, so what we believe is held has to be asserted against
    /// whichever actuator is now in force (`Planner::reassert`).
    ModeChanged,
    /// A maintenance window opened or closed: a `doze-cycle` app coming up for
    /// air on the shared schedule, or going back down (`Planner::maintenance`).
    Maintenance,
}

impl ReasonKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RuleFired => "rule",
            Self::BecameVisible => "became-visible",
            Self::Activated => "activated",
            Self::Exempt => "exempt",
            Self::NoRule => "no-rule",
            Self::Safety => "safety",
            Self::UserRequested => "user",
            Self::FocusSourceLost => "focus-source-lost",
            Self::SystemSleep => "system-sleep",
            Self::Shutdown => "shutdown",
            Self::ModeChanged => "mode-changed",
            Self::Maintenance => "maintenance",
        }
    }
}

/// The full justification for an intent. Every field here exists so that
/// `dozectl why` can answer without the user reading the source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reason {
    pub kind: ReasonKind,
    /// The rule that fired, when `kind` is [`ReasonKind::RuleFired`].
    pub rule: Option<String>,
    /// Prose detail, e.g. "hidden for 5m12s, grace 5m (base 5m x battery 1.0 x activity 1.0)".
    pub detail: String,
    /// What the world looked like when this was decided.
    ///
    /// Prose is for reading; this is for checking. `None` only for the global
    /// restores — shutdown, suspend, panic thaw, focus-source loss — where the
    /// kind is the whole explanation and there is no per-app derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<DecisionInputs>,
    /// The grace derivation, kept factorised (`20m = 5m x 1.0 x 4.0`) rather
    /// than collapsed, so a surprising wait can be attributed to a factor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace: Option<Grace>,
}

impl Reason {
    pub fn new(kind: ReasonKind, detail: impl Into<String>) -> Self {
        Self { kind, rule: None, detail: detail.into(), inputs: None, grace: None }
    }

    pub fn rule(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: ReasonKind::RuleFired,
            rule: Some(name.into()),
            detail: detail.into(),
            inputs: None,
            grace: None,
        }
    }

    /// Attach the derivation. Called by the planner for every decision it makes
    /// about a specific app.
    #[must_use]
    pub fn with(mut self, inputs: DecisionInputs, grace: Option<Grace>) -> Self {
        self.inputs = Some(inputs);
        self.grace = grace;
        self
    }

    #[must_use]
    pub fn summary(&self) -> String {
        match &self.rule {
            Some(r) => format!("rule: {r}"),
            None => self.kind.as_str().to_owned(),
        }
    }
}

/// A decision, not yet an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub app: AppKey,
    pub action: Action,
    pub reason: Reason,
    pub at: MonoTime,
}

impl Intent {
    pub fn new(app: AppKey, action: Action, reason: Reason, at: MonoTime) -> Self {
        Self { app, action, reason, at }
    }
}

/// What actually happened to an intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Dry-run: recorded, deliberately not executed.
    WouldHave,
    Applied,
    /// Live mode declined for a reason that is not an error, e.g. the app's
    /// cgroup vanished between the decision and the write.
    Skipped {
        why: String,
    },
    Failed {
        error: String,
    },
}

impl Outcome {
    #[must_use]
    pub const fn is_real(&self) -> bool {
        matches!(self, Self::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_read_naturally_with_would() {
        assert_eq!(format!("would {}", Action::Freeze { reclaim: false }.verb()), "would doze");
        assert_eq!(format!("would {}", Action::Throttle { pct: 5 }.verb()), "would throttle");
    }

    #[test]
    fn action_json_is_stable() {
        let json = serde_json::to_string(&Action::Throttle { pct: 5 }).unwrap();
        assert_eq!(json, r#"{"throttle":{"pct":5}}"#);
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), Action::Throttle { pct: 5 });
    }
}
