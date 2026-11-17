//! Why a rule did or did not match, and what the decision was computed from.
//!
//! `DESIGN.md` §1 promises that every action maps to exactly one named rule and
//! that `dozectl why` gives a straight answer. Naming the *winning* rule is only
//! half of that. The question a user actually asks is "why did my rule not
//! fire?", and answering it needs the predicate that rejected it — otherwise the
//! only way to find out is to read the source.
//!
//! The rejection check is the same code path as the match check:
//! [`predicates_match`] is defined as "there is no mismatch", so an explanation
//! cannot drift away from the decision it explains. [`Mismatch`] is `Copy` and
//! allocates nothing, which is what makes that affordable on the hot path —
//! `decide` runs this for every rule of every app on every re-evaluation, and a
//! battery tool that allocates its way through a tick is exactly what §13 warns
//! against.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::app::AppClass;
use crate::config::{Config, Predicates, Rule, Threshold};
use crate::event::{DesktopState, Exemption, PowerState, ProcSample, Visibility};
use crate::world::{AppState, WorldState};

/// The predicate that rejected a rule, carrying the two values that disagreed.
///
/// One variant per predicate in [`Predicates`], minus `for`, which is a timing
/// gate handled as grace rather than as a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "predicate")]
pub enum Mismatch {
    /// No pattern in the rule matched this app's identity.
    App,
    Class {
        want: AppClass,
        got: AppClass,
    },
    Visibility {
        want: Visibility,
        got: Visibility,
    },
    DesktopState {
        want: DesktopState,
        got: DesktopState,
    },
    OnBattery {
        want: bool,
        on_ac: bool,
    },
    BatteryBelow {
        want: f32,
        got: Option<f32>,
    },
    CpuPct {
        want: Threshold,
        got: f32,
    },
    WakeupsPerSec {
        want: Threshold,
        got: f32,
    },
    /// The visit that just ended was not the length the rule wanted. `got` is
    /// `None` when the app has never been focused at all, which no bound
    /// satisfies: an app never visited cannot have been briefly visited.
    FocusedFor {
        want: Threshold,
        got: Option<f32>,
    },
    /// An exemption predicate (`playing_audio`, `has_inhibitor`, `fullscreen`,
    /// `owns_dbus_name`, `capturing`, `screen_casting`) wanted a state the app
    /// is not in.
    Exemption {
        which: Exemption,
        want: bool,
    },
    /// The machine is not as hot as the rule wanted (the quiet objective).
    Thermal {
        want: crate::event::ThermalState,
        got: crate::event::ThermalState,
    },
    /// The rule wanted another app to be fullscreen, or wanted none to be (the
    /// latency objective).
    OtherAppFullscreen {
        want: bool,
    },
}

impl Mismatch {
    /// The config key that rejected the rule, so the message points at the line
    /// the user would edit.
    #[must_use]
    pub const fn predicate(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Class { .. } => "class",
            Self::Visibility { .. } => "visibility",
            Self::DesktopState { .. } => "desktop_state",
            Self::OnBattery { .. } => "on_battery",
            Self::BatteryBelow { .. } => "battery_below",
            Self::CpuPct { .. } => "cpu_pct",
            Self::WakeupsPerSec { .. } => "wakeups_per_sec",
            Self::FocusedFor { .. } => "focused_for",
            Self::Exemption { which, .. } => match which {
                Exemption::Audio => "playing_audio",
                Exemption::AudioSilent => "audio_silent",
                Exemption::Inhibitor => "has_inhibitor",
                Exemption::Fullscreen => "fullscreen",
                Exemption::DbusNameOwner => "owns_dbus_name",
                Exemption::Capture => "capturing",
                Exemption::ScreenCast => "screen_casting",
                Exemption::MaybeShared => "maybe_shared",
                // Not expressible as a predicate: these are hard invariants
                // (§10.1, §10.7) rather than anything a rule can ask about.
                Exemption::ClipboardOwner => "clipboard_owner",
                Exemption::LongRunningJob => "long_running_job",
            },
            Self::Thermal { .. } => "thermal",
            Self::OtherAppFullscreen { .. } => "other_app_fullscreen",
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = self.predicate();
        match *self {
            Self::App => write!(f, "{key}: no pattern matches this app"),
            Self::Class { want, got } => {
                write!(f, "{key}: wants {}, app is {}", want.as_str(), got.as_str())
            }
            Self::Visibility { want, got } => {
                write!(f, "{key}: wants {}, app is {}", want.as_str(), got.as_str())
            }
            Self::DesktopState { want, got } => {
                write!(f, "{key}: wants {}, desktop is {}", want.as_str(), got.as_str())
            }
            Self::OnBattery { want, on_ac } => write!(
                f,
                "{key}: wants {want}, machine is on {}",
                if on_ac { "AC" } else { "battery" }
            ),
            Self::BatteryBelow { want, got } => match got {
                Some(pct) => write!(f, "{key}: wants < {want:.0}%, charge is {pct:.0}%"),
                None => write!(f, "{key}: wants < {want:.0}%, but there is no battery"),
            },
            Self::CpuPct { want, got } => write!(f, "{key}: wants {want}, sampled {got:.1}%"),
            Self::WakeupsPerSec { want, got } => {
                write!(f, "{key}: wants {want}, sampled {got:.0}/s")
            }
            Self::FocusedFor { want, got } => match got {
                Some(secs) => write!(f, "{key}: wants {want}, last visit was {secs:.0}s"),
                None => write!(f, "{key}: wants {want}, but it has never been focused"),
            },
            Self::Exemption { want, .. } => {
                write!(f, "{key}: wants {want}, app is {}", !want)
            }
            Self::Thermal { want, got } => {
                write!(f, "{key}: wants {}, machine is {}", want.as_str(), got.as_str())
            }
            Self::OtherAppFullscreen { want } => write!(
                f,
                "{key}: wants {want}, and {}",
                if want { "nothing else is fullscreen" } else { "something else is" }
            ),
        }
    }
}

/// Which half of a rule rejected it. Cosmetic — both clauses must hold — but
/// it tells the user which table to go and edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Clause {
    Match,
    When,
}

impl Clause {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::When => "when",
        }
    }
}

/// One rule's verdict for one app at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleOutcome {
    pub name: String,
    pub priority: u32,
    /// Which clause rejected the rule, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clause: Option<Clause>,
    /// `None` when every predicate held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<Mismatch>,
    /// The first rule that matched, which is the one that actually decides.
    ///
    /// Later rules are still reported even though they can never fire: "my rule
    /// matches, but a higher-priority one won" is its own confusing failure, and
    /// it is invisible if we stop scanning at the winner.
    pub winner: bool,
}

impl RuleOutcome {
    #[must_use]
    pub fn matched(&self) -> bool {
        self.mismatch.is_none()
    }

    /// One aligned line for `dozectl why --rules`.
    #[must_use]
    pub fn line(&self) -> String {
        let mark = match (self.winner, self.matched()) {
            (true, _) => "->",
            (false, true) => " *",
            (false, false) => "  ",
        };
        match &self.mismatch {
            Some(m) => format!(
                "{mark} {:>4}  {:<22} {} {m}",
                self.priority,
                self.name,
                self.clause.map_or("", Clause::as_str)
            ),
            None if self.winner => {
                format!("{mark} {:>4}  {:<22} matched", self.priority, self.name)
            }
            None => format!(
                "{mark} {:>4}  {:<22} matched, but a higher-priority rule won",
                self.priority, self.name
            ),
        }
    }
}

/// Everything [`crate::planner::decide`] read, recorded next to what it
/// concluded.
///
/// A journal line saying "hidden for 5m12s" cannot be checked after the fact.
/// The same line with the inputs attached — on battery at 62%, desktop
/// idle-present, no exemptions, sampled at 340 wakeups/s — can be, and that is
/// the difference between a log you read and a log you trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionInputs {
    pub visibility: Visibility,
    pub class: AppClass,
    /// Session infrastructure or an ancestor of ours (§10.1).
    pub protected: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exemptions: Vec<Exemption>,
    pub sample: ProcSample,
    pub desktop: DesktopState,
    pub power: PowerState,
    /// The user came back to this app recently, so it earns extra patience.
    pub in_cooldown: bool,
    /// The rule that produced this decision is exempt from the resume cooldown
    /// (`planner::decide`), so `in_cooldown` above was true and bought nothing.
    ///
    /// Recorded because without it the journal cannot explain itself. Under the
    /// default [`CooldownMode::Floor`](crate::config::CooldownMode::Floor) the
    /// cooldown is a minimum wait rather than a term of the grace product, so
    /// `Grace::cooldown` is structurally 1.0 on every line — and a reader
    /// looking at `in_cooldown: true` beside a freeze taken four seconds after a
    /// thaw had nothing on the line that could account for it. The bypass is
    /// deliberate and argued for; it was simply invisible.
    ///
    /// Defaulted so a trace recorded before this existed still replays.
    #[serde(default)]
    pub cooldown_bypassed: bool,
    /// How hard the machine is working thermally (the quiet objective).
    ///
    /// Defaulted so a trace recorded before this existed still replays: an older
    /// session simply says `normal`, which is what it effectively assumed.
    #[serde(default)]
    pub thermal: crate::event::ThermalState,
    /// Whether some *other* app was fullscreen (the latency objective).
    #[serde(default)]
    pub other_app_fullscreen: bool,
}

impl DecisionInputs {
    #[must_use]
    pub fn capture(app: &AppState, world: &WorldState, in_cooldown: bool) -> Self {
        Self {
            visibility: app.visibility,
            class: app.class,
            protected: app.protected,
            exemptions: app.exemptions.iter().copied().collect(),
            sample: app.sample,
            desktop: world.desktop,
            power: world.power,
            in_cooldown,
            // Not knowable here: which rule will be picked is decided after
            // this capture. `planner::decide` sets it once it knows.
            cooldown_bypassed: false,
            thermal: world.thermal,
            other_app_fullscreen: world.fullscreen_elsewhere(&app.key),
        }
    }

    /// Whether two captures describe the same *situation*.
    ///
    /// Deliberately ignores the raw sample and the battery percentage: both move
    /// continuously, and a decision log that records every twitch of a
    /// float is a firehose nobody reads — and a wakeup source in its own right
    /// (`DESIGN.md` §13). Nothing is lost, because anything a sample can change
    /// about the *decision* surfaces as a different rule or a different desired
    /// action, and the caller compares those separately.
    #[must_use]
    pub fn same_situation_as(&self, other: &Self) -> bool {
        self.visibility == other.visibility
            && self.class == other.class
            && self.protected == other.protected
            && self.exemptions == other.exemptions
            && self.desktop == other.desktop
            && self.power.on_ac == other.power.on_ac
            && self.in_cooldown == other.in_cooldown
    }

    /// Compact one-line rendering for the decision log.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} {} {}",
            self.visibility.as_str(),
            self.desktop.as_str(),
            if self.power.on_ac { "on-ac" } else { "on-battery" },
        );
        if let Some(pct) = self.power.percentage {
            s.push_str(&format!(" {pct:.0}%"));
        }
        s.push_str(&format!(
            " cpu {:.1}% wakeups {:.0}/s",
            self.sample.cpu_pct, self.sample.wakeups_per_sec
        ));
        if !self.exemptions.is_empty() {
            let names: Vec<&str> = self.exemptions.iter().map(|e| e.as_str()).collect();
            s.push_str(&format!(" exempt[{}]", names.join(",")));
        }
        if self.in_cooldown {
            s.push_str(if self.cooldown_bypassed { " cooldown-bypassed" } else { " cooldown" });
        }
        // Only when they say something. A line reading `thermal-normal` on every
        // app of every desktop is noise, and `why` is read by somebody looking
        // for the one fact that explains a decision.
        if self.thermal != crate::event::ThermalState::Normal {
            s.push_str(&format!(" thermal-{}", self.thermal.as_str()));
        }
        if self.other_app_fullscreen {
            s.push_str(" other-app-fullscreen");
        }
        s
    }
}

/// The first predicate in `p` that this app fails, or `None` if it passes.
///
/// Every predicate except `for`, which is a timing gate handled as grace.
#[must_use]
pub fn first_mismatch(p: &Predicates, app: &AppState, world: &WorldState) -> Option<Mismatch> {
    if let Some(pats) = &p.app
        && !pats.matches(&app.key)
    {
        return Some(Mismatch::App);
    }
    if let Some(want) = p.class
        && app.class != want
    {
        return Some(Mismatch::Class { want, got: app.class });
    }
    if let Some(want) = p.visibility
        && app.visibility != want
    {
        return Some(Mismatch::Visibility { want, got: app.visibility });
    }
    if let Some(want) = p.desktop_state
        && world.desktop != want
    {
        return Some(Mismatch::DesktopState { want, got: world.desktop });
    }
    if let Some(want) = p.on_battery
        && world.power.on_ac == want
    {
        return Some(Mismatch::OnBattery { want, on_ac: world.power.on_ac });
    }
    if let Some(want) = p.battery_below
        && !world.power.percentage.is_some_and(|pct| pct < want)
    {
        return Some(Mismatch::BatteryBelow { want, got: world.power.percentage });
    }
    if let Some(want) = p.cpu_pct
        && !want.allows(app.sample.cpu_pct)
    {
        return Some(Mismatch::CpuPct { want, got: app.sample.cpu_pct });
    }
    if let Some(want) = p.wakeups_per_sec
        && !want.allows(app.sample.wakeups_per_sec)
    {
        return Some(Mismatch::WakeupsPerSec { want, got: app.sample.wakeups_per_sec });
    }
    if let Some(want) = p.focused_for {
        let got = app.focused_for.map(|d| d.as_secs_f32());
        if !got.is_some_and(|secs| want.allows(secs)) {
            return Some(Mismatch::FocusedFor { want, got });
        }
    }
    if let Some(want) = p.thermal
        && world.thermal != want
    {
        return Some(Mismatch::Thermal { want, got: world.thermal });
    }
    if let Some(want) = p.other_app_fullscreen
        && world.fullscreen_elsewhere(&app.key) != want
    {
        return Some(Mismatch::OtherAppFullscreen { want });
    }
    for (want, which) in [
        (p.playing_audio, Exemption::Audio),
        (p.audio_silent, Exemption::AudioSilent),
        (p.has_inhibitor, Exemption::Inhibitor),
        (p.fullscreen, Exemption::Fullscreen),
        (p.owns_dbus_name, Exemption::DbusNameOwner),
        (p.capturing, Exemption::Capture),
        (p.screen_casting, Exemption::ScreenCast),
        (p.maybe_shared, Exemption::MaybeShared),
    ] {
        if let Some(want) = want
            && app.is_exempt(which) != want
        {
            return Some(Mismatch::Exemption { which, want });
        }
    }
    None
}

/// Whether every predicate in `p` holds. Defined as "no mismatch" so the
/// explanation and the decision can never disagree.
#[must_use]
pub fn predicates_match(p: &Predicates, app: &AppState, world: &WorldState) -> bool {
    first_mismatch(p, app, world).is_none()
}

/// The first clause of `rule` that rejects this app, if any.
#[must_use]
pub fn rule_mismatch(
    rule: &Rule,
    app: &AppState,
    world: &WorldState,
) -> Option<(Clause, Mismatch)> {
    first_mismatch(&rule.match_, app, world)
        .map(|m| (Clause::Match, m))
        .or_else(|| first_mismatch(&rule.when, app, world).map(|m| (Clause::When, m)))
}

#[must_use]
pub fn rule_matches(rule: &Rule, app: &AppState, world: &WorldState) -> bool {
    rule_mismatch(rule, app, world).is_none()
}

/// Every rule's verdict for one app, in priority order.
///
/// Only used by the explain path, never by `plan`: scanning past the winner is
/// wasted work in the hot path and the whole point of it in the cold one.
#[must_use]
pub fn trace_rules(cfg: &Config, app: &AppState, world: &WorldState) -> Vec<RuleOutcome> {
    let mut winner_found = false;
    cfg.rules
        .iter()
        .map(|rule| {
            let verdict = rule_mismatch(rule, app, world);
            let winner = verdict.is_none() && !winner_found;
            winner_found |= winner;
            RuleOutcome {
                name: rule.name.clone(),
                priority: rule.priority,
                clause: verdict.map(|(c, _)| c),
                mismatch: verdict.map(|(_, m)| m),
                winner,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppKey;
    use crate::config::{Predicates, RuleAction};
    use crate::event::AppSeen;
    use crate::time::MonoTime;

    fn app(key: &str) -> AppState {
        AppState::new(
            &AppSeen {
                key: AppKey::comm(key),
                evidence: crate::app::IdentityEvidence::default(),
                class: AppClass::Normal,
                pids: vec![1],
                cgroup: None,
                protected: false,
            },
            MonoTime::ZERO,
        )
    }

    fn world() -> WorldState {
        WorldState::new(Config::empty())
    }

    #[test]
    fn a_match_reports_no_mismatch() {
        let a = app("slack");
        let w = world();
        assert_eq!(first_mismatch(&Predicates::default(), &a, &w), None);
        assert!(predicates_match(&Predicates::default(), &a, &w));
    }

    #[test]
    fn the_reported_predicate_is_the_one_that_failed() {
        let a = app("slack");
        let w = world();
        let p = Predicates {
            visibility: Some(Visibility::Hidden),
            class: Some(AppClass::LongRunningJob),
            ..Default::default()
        };
        // `class` is checked before `visibility`, and both fail; the user should
        // be pointed at the first one so fixing it makes progress.
        assert_eq!(
            first_mismatch(&p, &a, &w),
            Some(Mismatch::Class { want: AppClass::LongRunningJob, got: AppClass::Normal })
        );
    }

    #[test]
    fn matching_and_explaining_can_never_disagree() {
        // The property that justifies having two entry points at all.
        let mut a = app("slack");
        let mut w = world();
        let predicates = [
            Predicates { visibility: Some(Visibility::Hidden), ..Default::default() },
            Predicates { on_battery: Some(true), ..Default::default() },
            Predicates { battery_below: Some(30.0), ..Default::default() },
            Predicates { playing_audio: Some(true), ..Default::default() },
            Predicates { class: Some(AppClass::System), ..Default::default() },
            Predicates {
                app: Some(crate::config::Patterns::One("slack".into())),
                ..Default::default()
            },
            Predicates {
                wakeups_per_sec: Some(Threshold::Above(50.0)),
                cpu_pct: Some(Threshold::Above(5.0)),
                ..Default::default()
            },
        ];
        for visibility in [Visibility::Focused, Visibility::Hidden] {
            for on_ac in [true, false] {
                a.visibility = visibility;
                w.power = PowerState { on_ac, percentage: Some(50.0) };
                for p in &predicates {
                    assert_eq!(
                        first_mismatch(p, &a, &w).is_none(),
                        predicates_match(p, &a, &w),
                        "{p:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_rule_gets_a_verdict_and_exactly_one_wins() {
        let mut cfg = Config::empty();
        cfg.rules = vec![
            Rule::new("needs-audio", 10, RuleAction::Never)
                .when(Predicates { playing_audio: Some(true), ..Default::default() }),
            Rule::new("catch-all", 50, RuleAction::Freeze),
            Rule::new("also-matches", 60, RuleAction::Freeze),
        ];
        let w = WorldState::new(cfg.clone());
        let a = app("slack");
        let trace = trace_rules(&cfg, &a, &w);

        assert_eq!(trace.len(), 3, "every rule is reported, not just the winner");
        assert_eq!(
            trace[0].mismatch,
            Some(Mismatch::Exemption { which: Exemption::Audio, want: true })
        );
        assert!(trace[1].winner);
        assert!(trace[2].matched(), "it does match");
        assert!(!trace[2].winner, "but a higher-priority rule already won");
        assert!(trace[2].line().contains("higher-priority"), "{}", trace[2].line());
    }

    #[test]
    fn mismatches_name_the_config_key_to_edit() {
        // The message is only useful if it points at the line in the file.
        let m = Mismatch::Exemption { which: Exemption::Audio, want: true };
        assert_eq!(m.predicate(), "playing_audio");
        assert!(m.to_string().starts_with("playing_audio:"), "{m}");

        let m = Mismatch::OnBattery { want: true, on_ac: true };
        assert_eq!(m.to_string(), "on_battery: wants true, machine is on AC");

        let m = Mismatch::BatteryBelow { want: 30.0, got: None };
        assert!(m.to_string().contains("no battery"), "{m}");
    }

    #[test]
    fn inputs_ignore_what_moves_on_its_own() {
        let a = app("slack");
        let w = world();
        let base = DecisionInputs::capture(&a, &w, false);

        let mut jittered = base.clone();
        jittered.sample = ProcSample { cpu_pct: 12.5, wakeups_per_sec: 900.0, gpu_pct: None };
        jittered.power.percentage = Some(41.0);
        assert!(
            base.same_situation_as(&jittered),
            "a drifting battery and a noisy sample are not a new decision"
        );

        let mut moved = base.clone();
        moved.visibility = Visibility::Hidden;
        assert!(!base.same_situation_as(&moved), "but going hidden is");

        let mut exempted = base.clone();
        exempted.exemptions = vec![Exemption::Audio];
        assert!(!base.same_situation_as(&exempted), "and so is starting to play audio");
    }

    #[test]
    fn the_summary_names_every_input() {
        let mut a = app("slack");
        a.visibility = Visibility::Hidden;
        a.exemptions.insert(Exemption::Audio);
        a.sample = ProcSample { cpu_pct: 8.0, wakeups_per_sec: 512.0, gpu_pct: None };
        let mut w = world();
        w.power = PowerState { on_ac: false, percentage: Some(62.0) };
        w.desktop = DesktopState::IdlePresent;

        let s = DecisionInputs::capture(&a, &w, true).summary();
        for want in ["hidden", "idle-present", "on-battery", "62%", "8.0%", "512/s", "audio"] {
            assert!(s.contains(want), "{want} missing from {s}");
        }
        assert!(s.contains("cooldown"));
    }
}
