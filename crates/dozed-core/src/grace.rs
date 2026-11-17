//! The grace model: how long an app must stay hidden before we touch it.
//!
//! Grace is a product of independent factors rather than a single number, so
//! each one can be reasoned about — and explained to the user — separately:
//!
//! ```text
//! effective_grace = base_grace x aggression x battery_factor x activity_factor
//!                              x cooldown_factor x flap_factor
//! ```
//!
//! Keeping the factors around instead of collapsing them immediately is what
//! lets `dozectl why` print "grace 20m (base 5m x battery 1.0 x activity 4.0)"
//! rather than an unexplained "20m".

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{AcPolicy, Config};
use crate::event::{DesktopState, PowerState};

/// A grace period with its derivation intact.
///
/// Serialisable because it travels into the journal and the trace: "grace 20m"
/// is not checkable after the fact, "20m = 5m x 1.0 x 4.0" is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Grace {
    pub base: Duration,
    /// The `aggression` word, as a number (`config::Aggression::factor`).
    ///
    /// Defaulted rather than required, and to **1.0** rather than to `f64`'s own
    /// zero: this type is serialised into the journal and the trace, so a
    /// recording made before the setting existed must replay as the neutral term
    /// it had. A `#[serde(default)]` here would have quietly multiplied every
    /// historical grace by zero and turned the whole archive into "acted
    /// immediately".
    #[serde(default = "neutral")]
    pub aggression: f64,
    /// `None` means acting is disabled outright — currently only "on AC, and
    /// `on_ac = disabled`", which is now an opt-out rather than the default.
    /// Distinct from a very large factor: a disabled tier also thaws anything
    /// already held, because there is nothing to save.
    pub battery: Option<f64>,
    pub activity: f64,
    /// Extra patience for an app the user came back to recently: it is clearly
    /// in their active rotation.
    pub cooldown: f64,
    /// Backoff for an app whose recent holds kept ending before they could pay
    /// for themselves. 1.0 for an app that is not flapping, which is nearly all
    /// of them.
    ///
    /// The other five terms all answer "how long is it worth waiting"; this one
    /// answers "is holding this app working at all". A freeze costs a thaw, a
    /// redraw and often a reconnect, so a hold that lasts two seconds is a net
    /// loss however good the reasoning that produced it was — and the reasoning
    /// can be perfectly good, because nothing in the grace product can see that
    /// the user is about to come back again.
    ///
    /// Deliberately a term of the same product rather than a veto elsewhere: it
    /// composes with the rest and it shows up in the derivation `dozectl why`
    /// prints. Each short hold doubles the wait
    /// ([`MAX_FLAP`](crate::planner::MAX_FLAP)), and
    /// [`Grace::raise_to_at_least`] lifts it further where doubling cannot reach
    /// a wait worth having.
    ///
    /// It settles because the count *decays* rather than resetting: a single
    /// long hold steps it down by one, and only a hold well past the wait we
    /// imposed counts as one at all. An earlier version reset outright on any
    /// hold past [`MIN_PAYBACK`](crate::planner::MIN_PAYBACK) and oscillated
    /// instead — measured over three days as `.....248888........`, climbing to
    /// the cap and collapsing straight back.
    ///
    /// Defaulted to the neutral 1.0 for the same reason as `aggression`: this
    /// travels into the journal and the trace, and a recording made before the
    /// term existed must replay as if it were not there.
    #[serde(default = "neutral")]
    pub flap: f64,
}

/// Longest grace we will represent. Anything beyond this is indistinguishable
/// from "never" and keeping it finite avoids overflow arithmetic.
const MAX_GRACE: Duration = Duration::from_secs(365 * 86_400);

/// The identity for a grace term. See [`Grace::aggression`].
const fn neutral() -> f64 {
    1.0
}

impl Grace {
    /// Derive the grace for one app right now.
    #[must_use]
    pub fn compute(
        cfg: &Config,
        base: Duration,
        power: &PowerState,
        desktop: DesktopState,
        in_cooldown: bool,
    ) -> Self {
        Self::compute_for_rule(cfg, base, power, desktop, in_cooldown, false)
    }

    /// [`Grace::compute`], with the option to ignore the AC policy.
    ///
    /// `acts_on_ac` comes from the rule (`Rule::acts_on_ac`), and is set by a rule
    /// whose predicates are about something other than endurance — noise, or a
    /// dropped frame. Both of those costs are paid in full while the charger is
    /// in, so `on_ac = "disabled"` has no business silencing them.
    ///
    /// A separate constructor rather than a sixth argument on `compute`, because
    /// every existing caller means "the ordinary battery policy" and should keep
    /// saying so without being edited.
    #[must_use]
    pub fn compute_for_rule(
        cfg: &Config,
        base: Duration,
        power: &PowerState,
        desktop: DesktopState,
        in_cooldown: bool,
        acts_on_ac: bool,
    ) -> Self {
        let battery = if acts_on_ac && power.on_ac {
            // The AC factor still applies: a plugged-in machine deserves more
            // patience than one on battery, even when the reason for acting is
            // noise rather than charge. What does not apply is the *veto*.
            Some(cfg.battery.ac_factor)
        } else {
            battery_factor(cfg, power)
        };
        let activity = match desktop {
            DesktopState::Active => cfg.activity.active_factor,
            DesktopState::IdlePresent => 1.0,
            DesktopState::Away => cfg.activity.away_factor,
        };
        let cooldown = if in_cooldown { cfg.defaults.resume_cooldown_factor } else { 1.0 };
        // `flap` is not derivable from the config or the world: it is a fact
        // about this app's own recent history, which only the planner's memo
        // holds. Set by `planner::decide` immediately after this call, in the
        // same place and for the same reason that a glance rule flattens
        // `activity`.
        Self {
            base,
            aggression: cfg.defaults.aggression.factor(),
            battery,
            activity,
            cooldown,
            flap: 1.0,
        }
    }

    /// Whether we should not be acting at all right now.
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.battery.is_none()
    }

    /// The single number, or `None` when acting is disabled.
    #[must_use]
    pub fn effective(&self) -> Option<Duration> {
        let battery = self.battery?;
        let product = self.base.as_secs_f64()
            * self.aggression
            * battery
            * self.activity
            * self.cooldown
            * self.flap;
        // A negative or NaN factor is a config error. Round it towards "wait
        // forever", never towards "act now".
        if !product.is_finite() || product < 0.0 {
            return Some(MAX_GRACE);
        }
        if product == 0.0 {
            return Some(Duration::ZERO);
        }
        Some(Duration::try_from_secs_f64(product).unwrap_or(MAX_GRACE).min(MAX_GRACE))
    }

    /// Stretch [`Grace::flap`] until the product reaches `floor`, if it does not
    /// already.
    ///
    /// Expressed *in* the term rather than beside it, so grace stays one product
    /// and the derivation `dozectl why` prints stays arithmetic a reader can
    /// check — `x flap 12.5` is a number that multiplies out, where a floor
    /// applied afterwards would make the printed factors stop equalling the
    /// printed answer.
    ///
    /// Needed because a multiplier alone can be annihilated, which is not a new
    /// lesson in this file: `CooldownMode::Floor` exists because grace is a
    /// product and `away_factor = 0.0` cancelled the cooldown outright. The flap
    /// term has the same shape of weakness against a small `aggression` — 0.05
    /// turns a 15s rule into a 1.5s wait, and doubling that eight times is 12s,
    /// which only just clears the bar it was trying to clear. Below that
    /// aggression, or on a shorter rule, the doubling cannot get there at all.
    ///
    /// A no-op when acting is disabled: there is no product to raise.
    pub fn raise_to_at_least(&mut self, floor: Duration) {
        let Some(current) = self.effective() else { return };
        if current >= floor {
            return;
        }
        // The other terms, with `flap` divided back out, so the new factor is
        // solved against the same product rather than compounded onto it.
        let without_flap = current.as_secs_f64() / self.flap;
        if without_flap <= 0.0 || !without_flap.is_finite() {
            // A zero product is a deliberate "act immediately" somewhere in the
            // config (`away_factor = 0.0` is the shipped example). No multiplier
            // reaches a floor from zero, and inventing one would override a
            // setting rather than back off from a pattern.
            return;
        }
        let wanted = floor.as_secs_f64() / without_flap;
        if wanted.is_finite() && wanted > self.flap {
            self.flap = wanted;
        }
    }

    /// Whether `elapsed` has reached the grace period.
    #[must_use]
    pub fn elapsed_by(&self, elapsed: Duration) -> bool {
        self.effective().is_some_and(|g| elapsed >= g)
    }
}

/// The power term of the grace product: how much patience the current power
/// state buys.
///
/// `None` is "not acting at all", which on AC now requires asking for it with
/// `on_ac = "disabled"`. The default is to act, because the wall is not a free
/// source of power — a wakeup a charger pays for is burnt somewhere upstream
/// just the same, and a laptop spends most of its working day plugged in.
///
/// Acting is not the same as acting *identically*. Both states once returned
/// 1.0, so a plugged-in machine was treated as urgently as one at 40% charge,
/// chasing a runtime saving that does not exist while the wall is supplying the
/// power. `ac_factor` stretches that case instead: on mains the cost being cut
/// is waste, not endurance, and waste is worth being patient about.
///
/// On battery the factor is the **fraction of charge remaining**: grace scales
/// with the runtime you have left.
///
/// It was a cliff first — 1.0 above `low_pct`, `low_factor` below — so a laptop
/// at 31% was treated exactly as patiently as one at 100% and crossing 30%
/// halved the grace in a single sample. Replacing that with a line removed the
/// cliff but kept an arbitrary slope. This is the shape with an actual argument
/// behind it: what a saved wakeup is *worth* scales inversely with the runtime
/// left, and remaining runtime is proportional to charge, so the time we are
/// willing to wait before saving it should be proportional to charge too.
///
/// 50% charge, half the patience. 10% charge, a tenth of it — a five-minute
/// rule fires after thirty seconds. That is the intent, not an accident of
/// tuning.
fn battery_factor(cfg: &Config, power: &PowerState) -> Option<f64> {
    if power.on_ac {
        if cfg.defaults.on_ac == AcPolicy::Disabled {
            return None;
        }
        return Some(cfg.battery.ac_factor);
    }
    // No battery reading at all — a desktop, or UPower absent. Assume nothing
    // and apply no correction; §3 says ambiguity resolves towards doing less.
    let Some(pct) = power.percentage else { return Some(1.0) };
    Some(drain_factor(f64::from(pct)))
}

/// Patience proportional to charge, clamped at both ends.
///
/// The floor is not tuning. Grace is a product, so a battery term of zero means
/// "act the instant anything is hidden, forever" — and a reading of 0% is at
/// least as likely to be a flat-lining sensor as a truly empty battery. Five
/// percent of the base grace is still an emergency response (fifteen seconds
/// for a five-minute rule) without handing one bad reading a switch that
/// disables grace altogether. `away_factor = 0.0` remains the deliberate way to
/// ask for no grace at all.
///
/// Takes no config, and that is the cost of this shape: `low_pct` and
/// `low_factor` have nothing left to describe once the line is pinned to the
/// origin and to full charge. They are kept so existing files still parse, and
/// [`Config::warnings`](crate::config::Config::warnings) says so when they have
/// been set to something that used to mean something.
pub(crate) fn drain_factor(pct: f64) -> f64 {
    const FLOOR: f64 = 0.05;
    (pct / 100.0).clamp(FLOOR, 1.0)
}

impl fmt::Display for Grace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::time::HumanDuration;
        let Some(battery) = self.battery else {
            return write!(f, "disabled (on AC)");
        };
        write!(
            f,
            "{} (base {} x battery {battery:.1} x activity {:.1}",
            HumanDuration(self.effective().unwrap_or_default()),
            HumanDuration(self.base),
            self.activity,
        )?;
        // Shown only when it is doing something, exactly like `cooldown`: a
        // `x aggression 1.0` on every line of the journal is noise, and the
        // whole value of printing the derivation is that each term present in
        // it changed the answer.
        if self.aggression != 1.0 {
            write!(f, " x aggression {:.2}", self.aggression)?;
        }
        if self.cooldown != 1.0 {
            write!(f, " x cooldown {:.1}", self.cooldown)?;
        }
        if self.flap != 1.0 {
            write!(f, " x flap {:.1}", self.flap)?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::empty()
    }

    const ON_BATTERY: PowerState = PowerState { on_ac: false, percentage: Some(100.0) };
    const LOW_BATTERY: PowerState = PowerState { on_ac: false, percentage: Some(10.0) };
    const ON_AC: PowerState = PowerState { on_ac: true, percentage: Some(80.0) };

    /// The premise: a wakeup on mains is waste somebody still pays for, so the
    /// tool works there too. It used to do nothing at all on AC, which switched
    /// it off for most of the working day and for every desktop ever built.
    #[test]
    fn on_ac_acts_by_default() {
        let g = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &ON_AC,
            DesktopState::IdlePresent,
            false,
        );
        assert!(!g.is_disabled(), "mains is not a reason to do nothing");
        assert_eq!(g.effective(), Some(Duration::from_secs(600)), "at ac_factor patience");
    }

    /// The opt-out still opts out, and still cannot be reached by accident.
    #[test]
    fn on_ac_disabled_is_available_for_anyone_who_wants_it() {
        let mut c = cfg();
        c.defaults.on_ac = AcPolicy::Disabled;
        let g = Grace::compute(&c, Duration::from_secs(300), &ON_AC, DesktopState::Away, false);
        assert!(g.is_disabled());
        assert_eq!(g.effective(), None);
    }

    #[test]
    fn on_ac_acts_but_far_more_patiently_than_on_battery() {
        // Acting on AC used to buy the *battery* schedule exactly, because both
        // states returned a factor of 1.0. The cost being cut on mains is waste
        // rather than runtime, and waste is worth being patient about.
        let mut c = cfg();
        c.defaults.on_ac = AcPolicy::Enabled;
        let on_ac =
            Grace::compute(&c, Duration::from_secs(300), &ON_AC, DesktopState::IdlePresent, false);
        let on_battery = Grace::compute(
            &c,
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::IdlePresent,
            false,
        );
        assert_eq!(on_ac.effective(), Some(Duration::from_secs(600)));
        assert_eq!(on_battery.effective(), Some(Duration::from_secs(300)));
        assert!(on_ac.effective() > on_battery.effective());
    }

    #[test]
    fn the_power_ladder_spans_plugged_in_to_nearly_flat() {
        let mut c = cfg();
        c.defaults.on_ac = AcPolicy::Enabled;
        let at = |p: &PowerState| {
            Grace::compute(&c, Duration::from_secs(300), p, DesktopState::IdlePresent, false)
                .effective()
                .unwrap()
        };
        // A wide span between a charging laptop and one about to die, without AC
        // being so patient that the tool is effectively off while plugged in —
        // fourfold was tried and left Signal at a 32-minute grace.
        assert!(at(&ON_AC).as_secs() / at(&LOW_BATTERY).as_secs() >= 8);
        assert!(at(&ON_AC) > at(&ON_BATTERY) && at(&ON_BATTERY) > at(&LOW_BATTERY));
    }

    #[test]
    fn active_desktop_stretches_grace_fourfold() {
        let g = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::Active,
            false,
        );
        assert_eq!(g.effective(), Some(Duration::from_secs(1200)));
    }

    #[test]
    fn away_acts_immediately() {
        let g = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::Away,
            false,
        );
        assert_eq!(g.effective(), Some(Duration::ZERO));
        assert!(g.elapsed_by(Duration::ZERO));
    }

    #[test]
    // Renamed off "aggression": that word now names a config setting, and two
    // different meanings of it in one module is how a test comes to be read as
    // covering something it does not.
    fn urgency_rises_smoothly_as_the_battery_drains() {
        // It used to be a cliff at `low_pct`: 31% behaved exactly like 100%, and
        // crossing 30% doubled the aggression in one sample. Draining is
        // continuous, so this is a line.
        let f = super::drain_factor;

        assert!((f(100.0) - 1.0).abs() < 1e-9, "full charge is the reference");
        assert!((f(50.0) - 0.5).abs() < 1e-9, "half the charge, half the patience");
        assert!((f(10.0) - 0.1).abs() < 1e-9, "a tenth of the charge, a tenth of it");
        // Strictly decreasing all the way down — the property that makes it a
        // progression rather than a set of tiers.
        let mut prev = f(100.0);
        for pct in (5..100).rev() {
            let here = f(f64::from(pct));
            assert!(here < prev, "not monotonic at {pct}%: {here} vs {prev}");
            prev = here;
        }
        // The floor: a product with a zero term disables grace entirely, and a
        // 0% reading is as likely to be a flat-lining sensor as an empty cell.
        assert!(f(0.0) > 0.0, "an empty battery still has a finite grace, not zero");
        assert_eq!(f(0.0), f(1.0), "and the floor holds across the bottom few percent");
    }

    #[test]
    fn a_half_full_battery_is_already_more_urgent_than_a_full_one() {
        // The case the cliff got wrong: under the old model this was 1.0, the
        // same as fully charged.
        let half = PowerState { on_ac: false, percentage: Some(50.0) };
        let full = PowerState { on_ac: false, percentage: Some(100.0) };
        let at = |p: &PowerState| {
            Grace::compute(&cfg(), Duration::from_secs(300), p, DesktopState::IdlePresent, false)
                .effective()
                .unwrap()
        };
        assert!(at(&half) < at(&full));
    }

    #[test]
    fn a_machine_with_no_battery_reading_gets_no_correction() {
        let unknown = PowerState { on_ac: false, percentage: None };
        let g = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &unknown,
            DesktopState::IdlePresent,
            false,
        );
        assert_eq!(g.effective(), Some(Duration::from_secs(300)));
    }

    #[test]
    fn cooldown_adds_patience() {
        let g = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::IdlePresent,
            true,
        );
        assert_eq!(g.effective(), Some(Duration::from_secs(600)));
    }

    #[test]
    fn explains_itself() {
        let g = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::Active,
            false,
        );
        assert_eq!(g.to_string(), "20m (base 5m x battery 1.0 x activity 4.0)");
        // On AC the battery term is `ac_factor`, and the derivation says so
        // rather than collapsing to a word: 40m is a number somebody can check.
        assert_eq!(
            Grace::compute(&cfg(), Duration::from_secs(300), &ON_AC, DesktopState::Active, false)
                .to_string(),
            "40m (base 5m x battery 2.0 x activity 4.0)"
        );
        // The disabled tier still renders as one, for anyone who opts out.
        let mut off = cfg();
        off.defaults.on_ac = AcPolicy::Disabled;
        assert_eq!(
            Grace::compute(&off, Duration::from_secs(300), &ON_AC, DesktopState::Active, false)
                .to_string(),
            "disabled (on AC)"
        );
    }

    /// The starter config prints a table of what each aggression level means
    /// for the `bg-idle` rule. A user picks a word by reading that table, so it
    /// is a promise, and this is the test that keeps it true.
    #[test]
    fn the_aggression_table_in_the_config_file_is_accurate() {
        use crate::config::Aggression;

        let grace = |level: Aggression, power: &PowerState| {
            let mut c = cfg();
            c.defaults.aggression = level;
            Grace::compute(&c, Duration::from_secs(300), power, DesktopState::IdlePresent, false)
                .effective()
                .unwrap()
        };
        let mins = |m: u64| Duration::from_secs(m * 60);

        for (level, on_battery, on_ac) in [
            (Aggression::Gentle, mins(15), mins(30)),
            (Aggression::Balanced, mins(5), mins(10)),
            (Aggression::Aggressive, mins(2), mins(4)),
            (Aggression::Maximum, Duration::from_secs(45), Duration::from_secs(90)),
        ] {
            assert_eq!(grace(level, &ON_BATTERY), on_battery, "{} on battery", level.as_str());
            assert_eq!(grace(level, &ON_AC), on_ac, "{} on AC", level.as_str());
        }
    }

    /// The word is a multiplier, not an override: the rest of the model still
    /// applies underneath it. Somebody who sets `maximum` and then finds their
    /// focused app untouched should find the reason in the same product.
    #[test]
    fn aggression_composes_with_the_other_factors_rather_than_replacing_them() {
        let mut c = cfg();
        c.defaults.aggression = crate::config::Aggression::Maximum;

        // Still four times as patient while the user is actually working.
        let active =
            Grace::compute(&c, Duration::from_secs(300), &ON_BATTERY, DesktopState::Active, false);
        assert_eq!(active.effective(), Some(Duration::from_secs(180)));

        // Still proportional to charge underneath.
        let flat = Grace::compute(
            &c,
            Duration::from_secs(300),
            &LOW_BATTERY,
            DesktopState::IdlePresent,
            false,
        );
        assert_eq!(flat.effective(), Some(Duration::from_millis(4500)));

        // And `away_factor = 0.0` still wins outright — a product with a zero
        // in it is zero however aggressive the other terms are.
        let away =
            Grace::compute(&c, Duration::from_secs(300), &ON_BATTERY, DesktopState::Away, false);
        assert_eq!(away.effective(), Some(Duration::ZERO));
    }

    /// The derivation is the explanation, so the new term has to appear in it —
    /// and only when it is doing something.
    #[test]
    fn aggression_shows_up_in_the_derivation_when_it_is_not_neutral() {
        let mut c = cfg();
        c.defaults.aggression = crate::config::Aggression::Aggressive;
        let g = Grace::compute(
            &c,
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::IdlePresent,
            false,
        );
        assert_eq!(g.to_string(), "2m (base 5m x battery 1.0 x activity 1.0 x aggression 0.40)");

        let balanced = Grace::compute(
            &cfg(),
            Duration::from_secs(300),
            &ON_BATTERY,
            DesktopState::IdlePresent,
            false,
        );
        assert!(!balanced.to_string().contains("aggression"), "{balanced}");
    }

    /// A trace recorded before the setting existed must replay unchanged. The
    /// failure mode being guarded is severe and silent: `f64::default()` is
    /// zero, and a zero in a product is "acted immediately", so every historical
    /// grace would have replayed as no grace at all.
    #[test]
    fn a_grace_from_an_older_trace_replays_with_a_neutral_aggression() {
        let old = r#"{"base":{"secs":300,"nanos":0},"battery":1.0,"activity":4.0,"cooldown":1.0}"#;
        let g: Grace = serde_json::from_str(old).unwrap();
        assert_eq!(g.aggression, 1.0);
        assert_eq!(g.effective(), Some(Duration::from_secs(1200)));
    }

    #[test]
    fn absurd_factors_do_not_panic() {
        let mut c = cfg();
        c.activity.active_factor = f64::INFINITY;
        let g =
            Grace::compute(&c, Duration::from_secs(300), &ON_BATTERY, DesktopState::Active, false);
        assert_eq!(g.effective(), Some(MAX_GRACE));

        // A nonsensical factor must never round down to "act immediately".
        c.activity.active_factor = -1.0;
        let g =
            Grace::compute(&c, Duration::from_secs(300), &ON_BATTERY, DesktopState::Active, false);
        assert_eq!(g.effective(), Some(MAX_GRACE));
    }
}
