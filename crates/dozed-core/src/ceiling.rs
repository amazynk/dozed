//! A temporary limit on how far up the ladder dozed may go for one app.
//!
//! The rules decide *whether* and *when* to act; §7's ladder decides *how far*.
//! This is a per-app ceiling on that last part, set by hand and expiring on its
//! own — "cap this instead of freezing it, for the next hour", or "leave it
//! alone entirely while I am on this call".
//!
//! ## Why a ceiling rather than a fourth mode
//!
//! There were already three ways to say "treat this app differently": a `never`
//! rule, an `[apps]` override, and the exemptions the daemon derives for itself.
//! A fourth *mode* would be a second knob answering the same question as the
//! `[apps]` override, and §8 carries the scar of exactly that — `on_battery` and
//! `on_ac` disagreeing silently, discoverable only from `dozectl why --rules`.
//!
//! A ceiling is a different question, so it composes instead of competing:
//!
//! * `never` rules still win, because they are decided before this is consulted.
//! * The rule table still picks the rung and still explains itself.
//! * Grace still runs. A ceiling does not make an app act sooner or later, only
//!   less far.
//!
//! And it is *temporary* by construction, which is what keeps it out of the
//! config file's territory. Permanent belongs in `[apps]`, where it is written
//! down, survives everything, and can be read back later. Anything set from a
//! button should wear off, or it becomes a setting nobody remembers making.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::intent::Action;
use crate::time::{HumanDuration, MonoTime};

/// The most dozed may do to an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "ceiling")]
pub enum Ceiling {
    /// Nothing at all. The app is left running whatever the rules conclude.
    LeaveAlone,
    /// A cap, never a freeze.
    ///
    /// `pct` is a percentage of one core, as `RuleAction::Throttle` means it.
    /// `None` keeps whatever the rule asked for and only refuses to escalate
    /// past it — which is the useful default, because the rule's own number came
    /// from a measurement and a hand-picked one usually did not.
    CapOnly { pct: Option<u8> },
}

impl Ceiling {
    /// `action`, brought down to what this ceiling permits.
    ///
    /// `None` means "do nothing at all", which is not the same as `Restore`: an
    /// app already frozen when the ceiling is set has to be let go, and the
    /// caller distinguishes those. See [`Ceiling::clamp_or_release`].
    #[must_use]
    pub fn clamp(self, action: Action) -> Option<Action> {
        match (self, action) {
            // Restoring is below every ceiling: letting go is always permitted.
            (_, Action::Restore) => Some(Action::Restore),
            (Self::LeaveAlone, _) => None,
            // A cap the rule chose passes through; a cap the user chose replaces
            // it. Freezing becomes the cap, which is the whole point of the
            // "throttle it instead" half of this.
            (Self::CapOnly { pct }, Action::Throttle { pct: rule_pct }) => {
                Some(Action::Throttle { pct: pct.unwrap_or(rule_pct) })
            }
            (Self::CapOnly { pct }, Action::Freeze { .. }) => {
                Some(Action::Throttle { pct: pct.unwrap_or(DEFAULT_CAP_PCT) })
            }
        }
    }

    /// What this ceiling permits, given what is in force now.
    ///
    /// The difference from [`Ceiling::clamp`] is the app that is *already* held
    /// above the ceiling when it is set. Doing nothing would leave a frozen app
    /// frozen for as long as the ceiling lasts, which is the opposite of what
    /// somebody clicking "leave this alone" is asking for.
    #[must_use]
    pub fn clamp_or_release(self, wanted: Action, applied: Action) -> Option<Action> {
        match self.clamp(wanted) {
            Some(permitted) => Some(permitted),
            // Nothing is permitted. If we are holding something, let it go.
            None if !applied.is_restore() => Some(Action::Restore),
            None => None,
        }
    }

    /// How this reads in a decision log and in the app table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeaveAlone => "left alone",
            Self::CapOnly { .. } => "capped only",
        }
    }
}

/// The cap a "throttle it instead of freezing it" ceiling uses when no
/// percentage was named.
///
/// The same number the shipped `chatty-apps` rule uses, and for the same reason
/// it does: it is low enough that a chat app stops being a wakeup source and
/// high enough that it keeps its connections. A ceiling that has to invent a
/// number should invent the one the config already argues for.
pub const DEFAULT_CAP_PCT: u8 = 5;

/// A ceiling, and when it lapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub ceiling: Ceiling,
    /// `None` means "until something clears it" — a daemon restart, or the user.
    ///
    /// Offered because "leave it alone while I am in this call" has no honest
    /// duration attached to it, and making somebody pick one produces a number
    /// they will get wrong in one direction or the other. It is deliberately not
    /// the default: a limit with no end is a setting, and settings belong in the
    /// config file where they can be found again.
    pub until: Option<MonoTime>,
}

impl Hold {
    #[must_use]
    pub fn expired(&self, now: MonoTime) -> bool {
        self.until.is_some_and(|until| now >= until)
    }

    /// How much longer this has to run, for the app table and the dashboard.
    #[must_use]
    pub fn remaining(&self, now: MonoTime) -> Option<Duration> {
        self.until.map(|until| until.since(now))
    }

    /// "capped only, 42m left" — the whole state in one phrase.
    #[must_use]
    pub fn summary(&self, now: MonoTime) -> String {
        // Parenthesised rather than comma-separated, because this phrase is
        // joined into a comma-separated notes column and a comma inside one of
        // the items made `capped only, 42m left,audio` out of two facts.
        match self.remaining(now) {
            Some(left) => format!("{} ({} left)", self.ceiling.as_str(), HumanDuration(left)),
            None => format!("{} (until cleared)", self.ceiling.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREEZE: Action = Action::Freeze { reclaim: false };

    #[test]
    fn leaving_an_app_alone_permits_nothing_and_releases_what_is_held() {
        let c = Ceiling::LeaveAlone;
        assert_eq!(c.clamp(FREEZE), None);
        assert_eq!(c.clamp(Action::Throttle { pct: 5 }), None);
        // Letting go is below every ceiling.
        assert_eq!(c.clamp(Action::Restore), Some(Action::Restore));

        // Set while the app is already frozen, it thaws rather than leaving it
        // stuck for the life of the ceiling.
        assert_eq!(c.clamp_or_release(FREEZE, FREEZE), Some(Action::Restore));
        // And with nothing held there is simply nothing to do.
        assert_eq!(c.clamp_or_release(FREEZE, Action::Restore), None);
    }

    #[test]
    fn a_cap_only_ceiling_turns_a_freeze_into_a_throttle() {
        let c = Ceiling::CapOnly { pct: None };
        assert_eq!(c.clamp(FREEZE), Some(Action::Throttle { pct: DEFAULT_CAP_PCT }));
        // A rule that already wanted a cap keeps its own number: it came from a
        // measurement and this ceiling did not.
        assert_eq!(c.clamp(Action::Throttle { pct: 30 }), Some(Action::Throttle { pct: 30 }));
    }

    #[test]
    fn a_hand_picked_cap_replaces_the_rules_number() {
        let c = Ceiling::CapOnly { pct: Some(20) };
        assert_eq!(c.clamp(FREEZE), Some(Action::Throttle { pct: 20 }));
        assert_eq!(c.clamp(Action::Throttle { pct: 30 }), Some(Action::Throttle { pct: 20 }));
    }

    #[test]
    fn a_hold_with_no_deadline_never_expires_and_says_so() {
        let open = Hold { ceiling: Ceiling::LeaveAlone, until: None };
        assert!(!open.expired(MonoTime::from_millis(u64::MAX / 2)));
        assert_eq!(open.remaining(MonoTime::ZERO), None);
        assert!(open.summary(MonoTime::ZERO).contains("until cleared"));

        let timed = Hold {
            ceiling: Ceiling::CapOnly { pct: None },
            until: Some(MonoTime::from_millis(60_000)),
        };
        assert!(!timed.expired(MonoTime::ZERO));
        assert!(timed.expired(MonoTime::from_millis(60_000)));
        assert_eq!(timed.remaining(MonoTime::ZERO), Some(Duration::from_secs(60)));
        assert!(
            timed.summary(MonoTime::ZERO).contains("1m left"),
            "{}",
            timed.summary(MonoTime::ZERO)
        );
    }
}
