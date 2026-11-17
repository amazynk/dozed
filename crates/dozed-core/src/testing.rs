//! Fixtures and a scenario harness for driving the planner.
//!
//! The planner is a pure function, so testing it needs no fakes for I/O — only
//! a convenient way to write down a sequence of events and read back the
//! intents. [`Scenario`] is that, and it is the same shape as the replay
//! machinery in `DESIGN.md` §14: feed a trace, collect intents, diff.

use std::time::Duration;

use crate::app::{AppClass, AppKey, IdentityEvidence};
use crate::config::Config;
use crate::event::{AppSeen, DesktopState, Event, Exemption, PowerState, ProcSample, Visibility};
use crate::intent::Intent;
use crate::planner::Planner;
use crate::time::MonoTime;
use crate::world::WorldState;

/// A plain app keyed by `comm`, which is the least interesting resolver rung
/// and therefore the right default for tests that do not care about identity.
#[must_use]
pub fn app_seen(name: &str) -> AppSeen {
    AppSeen {
        key: AppKey::comm(name),
        evidence: IdentityEvidence { comm: Some(name.to_owned()), ..Default::default() },
        class: AppClass::Normal,
        pids: vec![1234],
        cgroup: Some(format!("/user.slice/app-{name}.scope")),
        protected: false,
    }
}

/// The discovery event for a plain app, ready to feed.
#[must_use]
pub fn seen(name: &str) -> Event {
    Event::AppSeen(Box::new(app_seen(name)))
}

#[must_use]
pub fn protected_app(name: &str) -> AppSeen {
    AppSeen { protected: true, ..app_seen(name) }
}

/// A *full* battery, so the drain ramp contributes a factor of 1.0 and a test
/// about activity or cooldown is not silently also a test about charge. Battery
/// aggression is now a line rather than a step (`crate::grace`), so any charge
/// below 100% scales the grace.
pub const ON_BATTERY: PowerState = PowerState { on_ac: false, percentage: Some(100.0) };
pub const LOW_BATTERY: PowerState = PowerState { on_ac: false, percentage: Some(10.0) };
pub const ON_AC: PowerState = PowerState { on_ac: true, percentage: Some(80.0) };

/// A world plus a planner, driven by events on a virtual clock.
///
/// Time only moves when a test says so, so a five-minute grace period costs
/// nothing to test.
pub struct Scenario {
    pub world: WorldState,
    pub planner: Planner,
    pub now: MonoTime,
}

impl Scenario {
    /// A scenario that is already observing focus and running on battery, since
    /// that is the state every interesting test needs.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let mut s =
            Self { world: WorldState::new(config), planner: Planner::new(), now: MonoTime::ZERO };
        s.feed(Event::FocusSourceHealth { healthy: true });
        s.feed(Event::PowerChanged { power: ON_BATTERY });
        s
    }

    /// Push an event through world then planner, exactly as the daemon does.
    pub fn feed(&mut self, event: Event) -> Vec<Intent> {
        self.world.apply(&event, self.now);
        self.planner.plan(&self.world, &event, self.now)
    }

    /// Move the clock forward and tick, which is how grace periods expire.
    pub fn advance(&mut self, d: Duration) -> Vec<Intent> {
        self.now = self.now.plus(d);
        self.feed(Event::Tick)
    }

    pub fn spawn(&mut self, name: &str) -> AppKey {
        let seen = app_seen(name);
        let key = seen.key.clone();
        self.feed(Event::AppSeen(Box::new(seen)));
        key
    }

    pub fn hide(&mut self, app: &AppKey) -> Vec<Intent> {
        self.feed(Event::VisibilityChanged { app: app.clone(), visibility: Visibility::Hidden })
    }

    pub fn focus(&mut self, app: &AppKey) -> Vec<Intent> {
        self.feed(Event::VisibilityChanged { app: app.clone(), visibility: Visibility::Focused })
    }

    pub fn set_visibility(&mut self, app: &AppKey, visibility: Visibility) -> Vec<Intent> {
        self.feed(Event::VisibilityChanged { app: app.clone(), visibility })
    }

    pub fn set_desktop(&mut self, desktop: DesktopState) -> Vec<Intent> {
        self.feed(Event::ActivityChanged { desktop })
    }

    pub fn set_power(&mut self, power: PowerState) -> Vec<Intent> {
        self.feed(Event::PowerChanged { power })
    }

    pub fn set_exemption(
        &mut self,
        app: &AppKey,
        exemption: Exemption,
        active: bool,
    ) -> Vec<Intent> {
        self.feed(Event::ExemptionChanged { app: app.clone(), exemption, active })
    }

    pub fn sample(&mut self, app: &AppKey, cpu_pct: f32, wakeups_per_sec: f32) -> Vec<Intent> {
        self.feed(Event::Sampled {
            app: app.clone(),
            sample: ProcSample { cpu_pct, wakeups_per_sec, gpu_pct: None },
        })
    }

    /// Drain the decision log, exactly as the daemon's recorder does.
    pub fn decisions(&mut self) -> Vec<(AppKey, crate::planner::Decision)> {
        self.planner.take_decisions()
    }
}
