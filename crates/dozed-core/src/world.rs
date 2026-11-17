//! What we currently believe about the machine.
//!
//! [`WorldState`] holds observations and nothing else — no decisions, no memory
//! of what we did. Folding an event stream into it is a pure function, so a
//! recorded trace replays to an identical world, which is what makes the
//! planner replayable in turn.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app::{AppClass, AppKey, IdentityEvidence};
use crate::config::Config;
use crate::event::{
    AppSeen, DesktopState, Event, Exemption, PowerState, ProcSample, ThermalState, Visibility,
};
use crate::time::MonoTime;

/// One app, as last observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    pub key: AppKey,
    pub evidence: IdentityEvidence,
    pub class: AppClass,
    pub pids: Vec<u32>,
    pub cgroup: Option<String>,
    /// Session infrastructure, or an ancestor of the daemon. Never actionable
    /// and not overridable — `DESIGN.md` §10.1.
    pub protected: bool,
    pub visibility: Visibility,
    /// When the app last entered its current visibility. The grace clock.
    pub visibility_since: MonoTime,
    /// How long the most recently *completed* focus period lasted, if there has
    /// been one.
    ///
    /// An observation, not a decision, which is why it lives here: it is the
    /// length of time a window held focus, and the planner's memory has no part
    /// in it. Rules read it through the `focused_for` predicate to tell a glance
    /// apart from a visit — the user alt-tabbed to an app, looked, and left,
    /// against the user actually working in it.
    pub focused_for: Option<Duration>,
    /// When the compositor last *told us* this app took focus.
    ///
    /// Not the same as `visibility == Focused`, and the difference matters. A
    /// new app is created `Focused` on purpose — assume the worst until the
    /// compositor says otherwise — so every app passes through a Focused state
    /// it was never observed in. Deriving `focused_for` from that would hand
    /// every app a near-zero "visit" on its first snapshot, and a `focused_for`
    /// rule would then fire on the whole session moments after the daemon
    /// started. `None` here means the focus we are in was assumed rather than
    /// seen, and an assumption is not evidence the user looked at anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_focus: Option<MonoTime>,
    pub exemptions: BTreeSet<Exemption>,
    pub sample: ProcSample,
    pub first_seen: MonoTime,
}

impl AppState {
    #[must_use]
    pub fn new(seen: &AppSeen, now: MonoTime) -> Self {
        Self {
            key: seen.key.clone(),
            evidence: seen.evidence.clone(),
            class: seen.class,
            pids: seen.pids.clone(),
            cgroup: seen.cgroup.clone(),
            protected: seen.protected,
            // Assume the worst until the compositor tells us otherwise: a new
            // app we cannot see must not be freezable.
            visibility: Visibility::Focused,
            visibility_since: now,
            focused_for: None,
            entered_focus: None,
            exemptions: BTreeSet::new(),
            sample: ProcSample::default(),
            first_seen: now,
        }
    }

    /// How long the app has held its current visibility.
    #[must_use]
    pub fn visible_for(&self, now: MonoTime) -> Duration {
        now.since(self.visibility_since)
    }

    /// An exemption that is not negotiable through config.
    ///
    /// Almost everything else is expressed as a `never` rule so the user can
    /// see and change it; these two would wedge the desktop, so they are not
    /// up for discussion (`DESIGN.md` §10.1, §10.7).
    #[must_use]
    pub fn hard_exemption(&self) -> Option<Exemption> {
        if self.exemptions.contains(&Exemption::ClipboardOwner) {
            return Some(Exemption::ClipboardOwner);
        }
        None
    }

    #[must_use]
    pub fn is_exempt(&self, e: Exemption) -> bool {
        self.exemptions.contains(&e)
    }

    /// Never actionable, whatever the rules say.
    #[must_use]
    pub fn is_protected(&self) -> bool {
        self.protected || self.class == AppClass::System
    }
}

/// The whole observed world.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldState {
    pub apps: BTreeMap<AppKey, AppState>,
    pub desktop: DesktopState,
    pub power: PowerState,
    /// Whether the compositor helper is still reporting. Starts `false`: until
    /// we have heard a heartbeat we cannot see focus, and an app we cannot see
    /// must not be frozen (`DESIGN.md` §10.2).
    pub focus_source_healthy: bool,
    /// Set between `PrepareForSleep(true)` and the following wake.
    pub sleeping: bool,
    /// How hard the machine is working thermally, for the quiet objective
    /// (`ThermalState`).
    pub thermal: ThermalState,
    pub config: Config,
}

impl WorldState {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            apps: BTreeMap::new(),
            desktop: DesktopState::default(),
            power: PowerState::default(),
            focus_source_healthy: false,
            sleeping: false,
            thermal: ThermalState::default(),
            config,
        }
    }

    /// Whether the planner should be making new decisions at all.
    #[must_use]
    pub const fn is_observing(&self) -> bool {
        self.focus_source_healthy && !self.sleeping
    }

    #[must_use]
    pub fn app(&self, key: &AppKey) -> Option<&AppState> {
        self.apps.get(key)
    }

    /// Whether some app *other than* `key` is fullscreen.
    ///
    /// The latency objective in one predicate: a fullscreen window is a game, a
    /// video or a call, and it is the one situation where the cost of a background
    /// app is not battery but a dropped frame. "Other than" is the whole content
    /// of it — the fullscreen app itself must never match, or the rule would
    /// freeze the very thing it exists to protect. It is already exempt by
    /// `fullscreen-exempt`, and depending on one rule to save another from
    /// itself is not a safety argument.
    #[must_use]
    pub fn fullscreen_elsewhere(&self, key: &AppKey) -> bool {
        self.apps.iter().any(|(other, app)| other != key && app.is_exempt(Exemption::Fullscreen))
    }

    /// Fold one event in. Pure: same world plus same event gives same world.
    pub fn apply(&mut self, event: &Event, now: MonoTime) {
        match event {
            Event::AppSeen(seen) => {
                self.apps
                    .entry(seen.key.clone())
                    .and_modify(|a| {
                        // The process set changed; identity and visibility are
                        // unaffected, so do not restart the grace clock.
                        a.pids.clone_from(&seen.pids);
                        a.cgroup.clone_from(&seen.cgroup);
                        a.evidence = seen.evidence.clone();
                        a.class = seen.class;
                        a.protected = seen.protected;
                    })
                    .or_insert_with(|| AppState::new(seen, now));
                if let Some(class) = self.config.class_for(&seen.key)
                    && let Some(app) = self.apps.get_mut(&seen.key)
                {
                    app.class = class;
                }
            }
            Event::AppGone { app } => {
                self.apps.remove(app);
            }
            Event::VisibilityChanged { app, visibility } => {
                if let Some(a) = self.apps.get_mut(app)
                    && a.visibility != *visibility
                {
                    // Closing out a focus period is the only moment its length
                    // is known, and only a focus we were *told* about counts —
                    // see `entered_focus`.
                    if a.visibility == Visibility::Focused {
                        a.focused_for = a.entered_focus.map(|t| now.since(t));
                        a.entered_focus = None;
                    }
                    if *visibility == Visibility::Focused {
                        a.entered_focus = Some(now);
                    }
                    a.visibility = *visibility;
                    a.visibility_since = now;
                }
            }
            Event::Sampled { app, sample } => {
                if let Some(a) = self.apps.get_mut(app) {
                    a.sample = *sample;
                }
            }
            Event::ExemptionChanged { app, exemption, active } => {
                if let Some(a) = self.apps.get_mut(app) {
                    if *active {
                        a.exemptions.insert(*exemption);
                    } else {
                        a.exemptions.remove(exemption);
                    }
                }
            }
            Event::ActivityChanged { desktop } => self.desktop = *desktop,
            Event::ThermalChanged { thermal } => self.thermal = *thermal,
            Event::PowerChanged { power } => self.power = *power,
            Event::FocusSourceHealth { healthy } => {
                let recovered = *healthy && !self.focus_source_healthy;
                self.focus_source_healthy = *healthy;
                if recovered {
                    // We were blind for that whole stretch, so "hidden for ten
                    // minutes" is a guess, not an observation. Restart the
                    // clocks and earn the grace again, exactly as after a
                    // suspend.
                    for app in self.apps.values_mut() {
                        app.visibility_since = now;
                    }
                }
            }
            Event::PrepareForSleep { sleeping } => {
                self.sleeping = *sleeping;
                if !*sleeping {
                    // Eight hours asleep is not eight hours of being ignored.
                    // Restart every grace clock so waking does not immediately
                    // trigger a mass freeze (`DESIGN.md` §10.4).
                    for app in self.apps.values_mut() {
                        app.visibility_since = now;
                    }
                }
            }
            Event::ConfigChanged { config } => {
                self.config = (**config).clone();
                let overrides: Vec<_> = self
                    .apps
                    .keys()
                    .filter_map(|k| self.config.class_for(k).map(|c| (k.clone(), c)))
                    .collect();
                for (key, class) in overrides {
                    if let Some(app) = self.apps.get_mut(&key) {
                        app.class = class;
                    }
                }
            }
            Event::ModeChanged { mode } => self.config.mode = *mode,
            Event::ActivationRequested { .. }
            | Event::UserAction { .. }
            | Event::ThawAll
            | Event::Shutdown
            | Event::Tick => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app_seen, seen};

    fn world() -> WorldState {
        WorldState::new(Config::empty())
    }

    #[test]
    fn new_apps_start_focused() {
        // We must not treat an app we have never had a visibility report for as
        // hidden, or we would freeze it on the strength of a guess.
        let mut w = world();
        w.apply(&seen("slack"), MonoTime::from_millis(10));
        assert_eq!(w.apps[&AppKey::comm("slack")].visibility, Visibility::Focused);
    }

    #[test]
    fn visibility_change_restarts_the_clock() {
        let mut w = world();
        let key = AppKey::comm("slack");
        w.apply(&seen("slack"), MonoTime::ZERO);
        w.apply(
            &Event::VisibilityChanged { app: key.clone(), visibility: Visibility::Hidden },
            MonoTime::from_millis(5_000),
        );
        assert_eq!(w.apps[&key].visibility_since, MonoTime::from_millis(5_000));

        // A repeat of the same state is not a change and must not reset it.
        w.apply(
            &Event::VisibilityChanged { app: key.clone(), visibility: Visibility::Hidden },
            MonoTime::from_millis(9_000),
        );
        assert_eq!(w.apps[&key].visibility_since, MonoTime::from_millis(5_000));
        assert_eq!(w.apps[&key].visible_for(MonoTime::from_millis(9_000)), Duration::from_secs(4));
    }

    #[test]
    fn waking_restarts_every_grace_clock() {
        let mut w = world();
        let key = AppKey::comm("slack");
        w.apply(&seen("slack"), MonoTime::ZERO);
        w.apply(
            &Event::VisibilityChanged { app: key.clone(), visibility: Visibility::Hidden },
            MonoTime::ZERO,
        );
        w.apply(&Event::PrepareForSleep { sleeping: true }, MonoTime::from_millis(1_000));
        assert!(!w.is_observing());

        let wake = MonoTime::from_millis(8 * 3_600_000);
        w.apply(&Event::PrepareForSleep { sleeping: false }, wake);
        assert_eq!(w.apps[&key].visible_for(wake), Duration::ZERO);
    }

    #[test]
    fn respawn_keeps_the_grace_clock() {
        let mut w = world();
        let key = AppKey::comm("slack");
        w.apply(&seen("slack"), MonoTime::ZERO);
        w.apply(
            &Event::VisibilityChanged { app: key.clone(), visibility: Visibility::Hidden },
            MonoTime::ZERO,
        );
        // A browser spawning a tab process must not buy the whole app a fresh
        // grace period every time.
        let mut seen = app_seen("slack");
        seen.pids = vec![1, 2, 3];
        w.apply(&Event::AppSeen(Box::new(seen)), MonoTime::from_millis(60_000));
        assert_eq!(w.apps[&key].visibility_since, MonoTime::ZERO);
        assert_eq!(w.apps[&key].pids, vec![1, 2, 3]);
    }

    #[test]
    fn class_override_from_config_wins() {
        let mut cfg = Config::empty();
        cfg.apps.insert(
            "slack".into(),
            crate::config::AppOverride {
                class: Some(AppClass::LongRunningJob),
                ..Default::default()
            },
        );
        let mut w = WorldState::new(cfg);
        w.apply(&seen("slack"), MonoTime::ZERO);
        assert_eq!(w.apps[&AppKey::comm("slack")].class, AppClass::LongRunningJob);
    }
}
