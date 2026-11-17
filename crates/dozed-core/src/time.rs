//! Time primitives.
//!
//! The planner never reads the clock. It is handed a [`MonoTime`], which is a
//! plain count of milliseconds since the daemon started. That makes every
//! decision reproducible: a recorded trace replays to byte-identical intents.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Milliseconds since the daemon's monotonic epoch.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MonoTime {
    pub ms: u64,
}

impl MonoTime {
    pub const ZERO: Self = Self { ms: 0 };

    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self { ms }
    }

    /// How long ago `earlier` was. Saturates at zero rather than panicking, so
    /// a clock hiccup or an out-of-order replay can never blow up the planner.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.ms.saturating_sub(earlier.ms))
    }

    #[must_use]
    pub const fn plus(self, d: Duration) -> Self {
        Self { ms: self.ms.saturating_add(d.as_millis() as u64) }
    }
}

impl fmt::Display for MonoTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "+{:.3}s", self.ms as f64 / 1000.0)
    }
}

/// A `Duration` that reads and writes as `"90s"`, `"5m"`, `"1h30m"`.
///
/// Config files are meant to be edited by hand, so durations are written the
/// way a person would say them. A bare number is rejected on purpose: `for = 5`
/// is ambiguous and the resulting mistake (5 ms instead of 5 minutes) would be
/// invisible until something froze far too eagerly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    #[must_use]
    pub const fn secs(s: u64) -> Self {
        Self(Duration::from_secs(s))
    }

    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}

impl From<HumanDuration> for Duration {
    fn from(h: HumanDuration) -> Self {
        h.0
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DurationParseError {
    #[error("empty duration")]
    Empty,
    #[error("duration `{0}` has no unit; write e.g. `30s`, `5m`, `1h30m`")]
    MissingUnit(String),
    #[error("unknown unit `{unit}` in duration `{input}`; use ms, s, m, h or d")]
    UnknownUnit { input: String, unit: String },
    #[error("malformed duration `{0}`")]
    Malformed(String),
}

impl FromStr for HumanDuration {
    type Err = DurationParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let input = s.trim();
        if input.is_empty() {
            return Err(DurationParseError::Empty);
        }

        let mut total = Duration::ZERO;
        let mut digits = String::new();
        let mut unit = String::new();
        let mut saw_any = false;

        // Walk the string as alternating runs of digits and letters so that
        // compound forms like "1h30m" fall out without a regex.
        for ch in input.chars() {
            if ch.is_ascii_digit() {
                if !unit.is_empty() {
                    total += apply_unit(input, &digits, &unit)?;
                    digits.clear();
                    unit.clear();
                }
                digits.push(ch);
            } else if ch.is_ascii_alphabetic() {
                if digits.is_empty() {
                    return Err(DurationParseError::Malformed(input.to_owned()));
                }
                unit.push(ch);
                saw_any = true;
            } else if !ch.is_whitespace() {
                return Err(DurationParseError::Malformed(input.to_owned()));
            }
        }

        if !digits.is_empty() && unit.is_empty() {
            return Err(DurationParseError::MissingUnit(input.to_owned()));
        }
        if !digits.is_empty() {
            total += apply_unit(input, &digits, &unit)?;
        }
        if !saw_any {
            return Err(DurationParseError::MissingUnit(input.to_owned()));
        }
        Ok(Self(total))
    }
}

fn apply_unit(input: &str, digits: &str, unit: &str) -> Result<Duration, DurationParseError> {
    let n: u64 = digits.parse().map_err(|_| DurationParseError::Malformed(input.to_owned()))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "s" | "sec" | "secs" => Duration::from_secs(n),
        "m" | "min" | "mins" => Duration::from_secs(n.saturating_mul(60)),
        "h" | "hr" | "hrs" => Duration::from_secs(n.saturating_mul(3600)),
        "d" | "day" | "days" => Duration::from_secs(n.saturating_mul(86_400)),
        other => {
            return Err(DurationParseError::UnknownUnit {
                input: input.to_owned(),
                unit: other.to_owned(),
            });
        }
    };
    Ok(d)
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rest = self.0.as_secs();
        let ms = self.0.subsec_millis();
        if rest == 0 && ms > 0 {
            return write!(f, "{ms}ms");
        }
        if rest == 0 {
            return write!(f, "0s");
        }
        let mut out = String::new();
        for (unit, size) in [("d", 86_400_u64), ("h", 3600), ("m", 60)] {
            let n = rest / size;
            if n > 0 {
                out.push_str(&format!("{n}{unit}"));
                rest -= n * size;
            }
        }
        if rest > 0 {
            out.push_str(&format!("{rest}s"));
        }
        // The sub-second remainder, and not as a rounding: this type is what
        // prints a grace, and a grace is a *product* of factors printed beside
        // it. Truncating here made the derivation stop equalling its own answer
        // — `base 15s x battery 2.0 x activity 1.0 x aggression 0.05` is 1.5
        // seconds and was printed as `grace 1s`, a third short, while anything
        // under a second printed as `0s` and so read as the deliberate "no grace
        // at all" that `away_factor = 0.0` means. Both were observed in the live
        // journal.
        //
        // Compound rather than decimal because the parser above is the other
        // half of this impl: `1s500ms` round-trips through `FromStr`, `1.5s`
        // does not. A millisecond is that parser's finest unit, so it is also
        // the limit of what this can promise to preserve.
        if ms > 0 {
            out.push_str(&format!("{ms}ms"));
        }
        f.write_str(&out)
    }
}

/// [`HumanDuration`] for a person reading it, rather than for the parser.
///
/// The compound form is exact to the millisecond because [`HumanDuration`] is
/// its own serialiser and has to round-trip through `FromStr` — see its
/// `Display`, which is pinned by test for that reason. Nothing reading a status
/// line needs it: `uptime 18m19s679ms` spends its most prominent digits on its
/// least useful ones, and `hidden for 2s2ms` reads as a glitch rather than as
/// two seconds. Both were taken from a live machine.
///
/// So the sub-second part is dropped — but only where something larger is there
/// to carry the meaning. Below a second the milliseconds *are* the answer and
/// `112ms` stays `112ms`, because truncating those would print `0s`, and `0s`
/// is not merely imprecise here: `activity.away_factor = 0.0` makes it the
/// deliberate "no grace at all", so a rounded-down 500 ms would read as a
/// setting nobody chose.
///
/// The seconds stay, and that is a decision rather than an oversight: see
/// [`Rounded`], which drops them, and the list of things that must not have them
/// dropped.
///
/// Display only, and deliberately not applied to the daemon's journal: a grace
/// there is printed beside the factors that produced it, and has to keep
/// equalling their product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Coarse(pub Duration);

impl fmt::Display for Coarse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        // Under a second there is nothing else to show, so hand the whole value
        // over: `HumanDuration` already prints `112ms`, and `0s` for zero.
        let shown = if secs == 0 { self.0 } else { Duration::from_secs(secs) };
        write!(f, "{}", HumanDuration(shown))
    }
}

/// [`Coarse`] for a *total*, which is one unit coarser again.
///
/// `dozectl report` printed `firefox 7d17h19m8s39ms` for an app it had held for
/// a week. Two of those five fields are below the resolution of anything the
/// number is for, and they are the two the eye reaches last. So above a minute
/// the seconds go the way [`Coarse`] sends the milliseconds, and for the same
/// reason: drop the smallest unit only where something larger carries the
/// reading. Under a minute they are the reading, so `59s` keeps them.
///
/// Separate from [`Coarse`] rather than a widening of it, because the two are
/// read for different things and the seconds are load-bearing in half of them:
///
/// * a snooze and a grace are *counted down*, and `Coarse` is already the
///   display-only view of a grace whose exact form has to stay a product of its
///   factors;
/// * `silent for 1m30s` is measured against `audio.silence_after`, which
///   defaults to a minute — rounding it to `1m` would freeze the one number a
///   user is watching cross a threshold they set.
///
/// Nothing above is a total. Everything this is used on is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Rounded(pub Duration);

impl fmt::Display for Rounded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        if secs < 60 {
            // Including the sub-second case, which `Coarse` already answers and
            // which this must not answer differently: truncating towards `0s`
            // is the one thing neither of them may do.
            return write!(f, "{}", Coarse(self.0));
        }
        write!(f, "{}", HumanDuration(Duration::from_secs(secs - secs % 60)))
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grace is a product printed beside its factors, so the answer has to be
    /// the answer.
    ///
    /// Truncating the remainder made `15s x 2.0 x 1.0 x 0.05` — 1.5 seconds —
    /// print as `grace 1s`, and anything under a second print as `0s`, which is
    /// the deliberate "no grace at all" that `away_factor = 0.0` means. Both
    /// were read off a live journal.
    #[test]
    fn a_sub_second_remainder_is_printed_rather_than_dropped() {
        let show = |d: Duration| HumanDuration(d).to_string();
        assert_eq!(show(Duration::from_millis(1500)), "1s500ms", "the measured case");
        assert_eq!(show(Duration::from_millis(500)), "500ms", "and never `0s`");
        assert_eq!(show(Duration::from_millis(90_250)), "1m30s250ms");
        // Whole values are untouched, which is nearly everything this prints.
        assert_eq!(show(Duration::from_secs(90)), "1m30s");
        assert_eq!(show(Duration::ZERO), "0s");
    }

    /// And the halves of this impl stay each other's inverse.
    ///
    /// Compound rather than decimal for exactly this reason: `1s500ms` parses,
    /// `1.5s` does not, and this type is serialised by `to_string`.
    #[test]
    fn a_fractional_duration_round_trips_through_its_own_parser() {
        for ms in [1_500u64, 500, 90_250, 1, 999, 3_600_001] {
            let d = HumanDuration(Duration::from_millis(ms));
            assert_eq!(d.to_string().parse::<HumanDuration>().unwrap(), d, "{ms}ms");
        }
    }

    /// The sub-second part is noise once a larger unit carries the meaning, and
    /// the whole answer once nothing does.
    ///
    /// The `0s` case is the one worth pinning: `activity.away_factor = 0.0`
    /// gives `0s` a meaning of its own, so a sub-second duration must never
    /// arrive at it by truncation.
    #[test]
    fn coarse_drops_milliseconds_only_where_something_larger_carries_the_reading() {
        let show = |ms: u64| Coarse(Duration::from_millis(ms)).to_string();

        // Taken from a live status line and a live journal.
        assert_eq!(show(1_099_679), "18m19s");
        assert_eq!(show(2_002), "2s");
        assert_eq!(show(1_500), "1s");

        // All there is, so it stays — and specifically does not become `0s`.
        assert_eq!(show(112), "112ms");
        assert_eq!(show(999), "999ms");
        assert_eq!(show(1), "1ms");

        // Zero has no milliseconds to keep, and a whole unit grows no empty
        // seconds field.
        assert_eq!(show(0), "0s");
        assert_eq!(show(3_600_001), "1h");
        assert_eq!(show(90_250), "1m30s");
    }

    /// And a total goes one unit further, by the same rule.
    #[test]
    fn a_total_drops_its_seconds_only_where_something_larger_carries_the_reading() {
        let show = |ms: u64| Rounded(Duration::from_millis(ms)).to_string();

        // From a live `dozectl report`, which printed `7d17h19m8s39ms`.
        assert_eq!(show(667_148_039), "7d17h19m");
        // From a live status line.
        assert_eq!(show(1_099_679), "18m");
        assert_eq!(show(90_250), "1m");
        assert_eq!(show(60_000), "1m");

        // Under a minute the seconds are the reading, so `59s` must not round
        // down to an empty `0m` — the same thing `Coarse` refuses to do to
        // milliseconds, one unit up.
        assert_eq!(show(59_999), "59s");
        assert_eq!(show(2_002), "2s");

        // And below a second this *is* `Coarse`, so `0s` keeps its one meaning.
        assert_eq!(show(112), "112ms");
        assert_eq!(show(0), "0s");
        assert_eq!(show(3_600_001), "1h");
    }

    /// `Coarse` is a reader's view, not a second encoding: the type that has to
    /// survive a round trip is untouched by it.
    #[test]
    fn coarsening_leaves_the_serialised_form_alone() {
        let d = HumanDuration(Duration::from_millis(1_500));
        assert_eq!(d.to_string(), "1s500ms");
        assert_eq!(Coarse(d.get()).to_string(), "1s");
        assert_eq!(d.to_string().parse::<HumanDuration>().unwrap(), d);
    }

    #[test]
    fn parses_simple_units() {
        assert_eq!("30s".parse::<HumanDuration>().unwrap().get(), Duration::from_secs(30));
        assert_eq!("5m".parse::<HumanDuration>().unwrap().get(), Duration::from_secs(300));
        assert_eq!("2h".parse::<HumanDuration>().unwrap().get(), Duration::from_secs(7200));
        assert_eq!("250ms".parse::<HumanDuration>().unwrap().get(), Duration::from_millis(250));
    }

    #[test]
    fn parses_compound() {
        assert_eq!("1h30m".parse::<HumanDuration>().unwrap().get(), Duration::from_secs(5400));
        assert_eq!("1d2h3m4s".parse::<HumanDuration>().unwrap().get(), Duration::from_secs(93_784));
    }

    #[test]
    fn rejects_bare_number() {
        // The whole point: `for = 5` must not silently mean 5 milliseconds.
        assert!(matches!("5".parse::<HumanDuration>(), Err(DurationParseError::MissingUnit(_))));
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(matches!(
            "5y".parse::<HumanDuration>(),
            Err(DurationParseError::UnknownUnit { .. })
        ));
    }

    #[test]
    fn display_roundtrips() {
        for s in ["30s", "5m", "1h30m", "2d", "250ms"] {
            let d: HumanDuration = s.parse().unwrap();
            assert_eq!(d.to_string().parse::<HumanDuration>().unwrap(), d, "{s}");
        }
    }

    #[test]
    fn monotime_saturates() {
        let a = MonoTime::from_millis(100);
        let b = MonoTime::from_millis(50);
        assert_eq!(b.since(a), Duration::ZERO);
        assert_eq!(a.since(b), Duration::from_millis(50));
    }
}
