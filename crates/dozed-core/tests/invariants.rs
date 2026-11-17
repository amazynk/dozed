//! The safety invariants, checked against randomly generated sessions.
//!
//! `DESIGN.md` §10 states a handful of things that must be true *whatever* the
//! config says and whatever order events arrive in. Every one of them has a unit
//! test built from the situation that motivated it, which is the right way to
//! catch the bug you already know about. This file is for the ones nobody thought
//! of: it generates random rule tables and random event streams and asserts the
//! invariants hold across all of them.
//!
//! ## Why hand-rolled and not `proptest`
//!
//! Two reasons, and neither is aversion to dependencies as such. The generator
//! needs to produce *plausible* sessions — an app is spawned before it is hidden,
//! visibility changes are interleaved with ticks — and expressing that as a
//! strategy is more work than writing the walk directly. And `dozed_core::stats`
//! already contains a seeded PRNG, added for the trial's resampling, so the cost
//! here is a `for` loop over seeds.
//!
//! Every case is therefore reproducible from its seed and printed on failure,
//! which is the property that actually matters. What is given up is shrinking: a
//! failure arrives as a 200-event session rather than the three events that would
//! have sufficed. Worth revisiting if one ever fails.

use std::time::Duration;

use dozed_core::config::{Patterns, Predicates, Rule, RuleAction, Threshold};
use dozed_core::stats::Rng;
use dozed_core::testing::{ON_AC, ON_BATTERY, Scenario, app_seen, protected_app};
use dozed_core::{
    Action, AppClass, AppKey, Config, DesktopState, Event, Exemption, Intent, Outcome, PowerState,
    Visibility,
};

/// How many random sessions to run. Each is a few hundred events over a virtual
/// clock, so the whole file is milliseconds.
const SESSIONS: u64 = 200;

/// The apps every generated session draws from. A protected one and a
/// long-running job are always present, because they are what two of the
/// invariants are about.
const APPS: [&str; 5] = ["slack", "firefox", "editor", "kwin_wayland", "backup"];

fn is_protected(name: &str) -> bool {
    name == "kwin_wayland"
}

/// A random rule table.
///
/// Deliberately allowed to be silly — rules that contradict each other, an
/// exemption at a priority where it can never win, a wakeup bound no app will
/// reach. The invariants are supposed to hold for configs nobody would write, and
/// a generator that only produced sensible ones would be testing the author's
/// taste.
fn random_config(rng: &mut Rng) -> Config {
    let mut cfg = Config::empty();
    let count = 1 + rng.below(5);
    for i in 0..count {
        let action = match rng.below(5) {
            0 => RuleAction::Never,
            1 => RuleAction::Throttle(1 + rng.below(100) as u8),
            2 => RuleAction::ThrottleTo(1 + rng.below(200) as u32),
            3 => RuleAction::DozeCycle {
                every: dozed_core::HumanDuration::secs(60 + 60 * rng.below(20) as u64),
                wake: dozed_core::HumanDuration::secs(5 + rng.below(30) as u64),
            },
            _ => RuleAction::Freeze,
        };
        let visibility = match rng.below(4) {
            0 => Some(Visibility::Hidden),
            1 => Some(Visibility::VisibleUnfocused),
            // A rule that asks for the foreground explicitly, which §10 allows
            // and which the "never freeze what is focused" invariant has to
            // account for rather than assume away.
            2 => Some(Visibility::Focused),
            _ => None,
        };
        let mut when = Predicates {
            visibility,
            for_: Some(dozed_core::HumanDuration::secs(15 + 30 * rng.below(10) as u64)),
            ..Default::default()
        };
        if rng.flip() {
            when.wakeups_per_sec = Some(Threshold::Above(rng.below(500) as f32));
        }
        if rng.flip() {
            when.on_battery = Some(rng.flip());
        }
        let mut match_ = Predicates::default();
        if rng.flip() {
            match_.app = Some(Patterns::One(APPS[rng.below(APPS.len())].to_owned()));
        }
        if rng.flip() {
            when.playing_audio = Some(rng.flip());
        }
        let rule =
            Rule::new(format!("r{i}"), rng.below(200) as u32, action).matching(match_).when(when);
        cfg.rules.push(rule);
    }
    // Sorted into priority order, exactly as a parsed config would be.
    cfg.validate().expect("a generated config must be loadable");
    cfg
}

/// Everything the generator can do to a session, and what it did, so a failure
/// can be read.
fn random_session(seed: u64, steps: usize) -> (Scenario, Vec<String>, Vec<Intent>) {
    let mut rng = Rng::seeded(seed);
    let config = random_config(&mut rng);
    let mut s = Scenario::new(config);
    let mut log = vec![format!("seed {seed}")];
    let mut all = Vec::new();

    // Spawn them all up front so later events have something to act on.
    for name in APPS {
        let seen = if is_protected(name) { protected_app(name) } else { app_seen(name) };
        all.extend(s.feed(Event::AppSeen(Box::new(seen))));
    }

    for _ in 0..steps {
        let app = AppKey::comm(APPS[rng.below(APPS.len())]);
        let (what, intents) = match rng.below(10) {
            0 => ("focus".to_owned(), s.focus(&app)),
            1 => ("hide".to_owned(), s.hide(&app)),
            2 => (
                "visible-unfocused".to_owned(),
                s.set_visibility(&app, Visibility::VisibleUnfocused),
            ),
            3 => {
                let power = match rng.below(3) {
                    0 => ON_BATTERY,
                    1 => ON_AC,
                    _ => PowerState { on_ac: false, percentage: Some(rng.below(100) as f32) },
                };
                (format!("power {power:?}"), s.set_power(power))
            }
            4 => {
                let desktop = match rng.below(3) {
                    0 => DesktopState::Active,
                    1 => DesktopState::IdlePresent,
                    _ => DesktopState::Away,
                };
                (format!("desktop {}", desktop.as_str()), s.set_desktop(desktop))
            }
            5 => {
                let which = Exemption::ALL[rng.below(Exemption::ALL.len())];
                let on = rng.flip();
                (format!("exempt {} {on}", which.as_str()), s.set_exemption(&app, which, on))
            }
            6 => {
                let wakeups = rng.below(2000) as f32;
                (format!("sample {wakeups}"), s.sample(&app, rng.below(100) as f32, wakeups))
            }
            7 => {
                let healthy = rng.flip();
                (format!("focus-source {healthy}"), s.feed(Event::FocusSourceHealth { healthy }))
            }
            8 => {
                let sleeping = rng.flip();
                (format!("sleep {sleeping}"), s.feed(Event::PrepareForSleep { sleeping }))
            }
            _ => {
                let d = Duration::from_secs(15 + 60 * rng.below(10) as u64);
                (format!("advance {d:?}"), s.advance(d))
            }
        };
        // Feed every outcome back, as the daemon does: without this the planner
        // never advances its belief and half the state machine is untested.
        for i in &intents {
            s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
        }
        log.push(format!("{what} -> {} intent(s)", intents.len()));
        all.extend(intents);
    }
    (s, log, all)
}

/// Render the session for a failure message.
fn story(log: &[String]) -> String {
    log.join("\n  ")
}

#[test]
fn session_infrastructure_is_never_touched() {
    // §10.1, the invariant no config may override: freezing the compositor or an
    // ancestor of the daemon wedges the desktop, and nothing else in this file
    // matters if that can happen.
    for seed in 0..SESSIONS {
        let (_, log, intents) = random_session(seed, 60);
        for i in &intents {
            if is_protected(i.app.value.as_str()) {
                assert!(
                    i.action.is_restore(),
                    "protected app {} was asked to {}\n  {}",
                    i.app,
                    i.action.verb(),
                    story(&log)
                );
            }
        }
    }
}

#[test]
fn nothing_is_held_while_we_cannot_see_focus() {
    // §10.2. Losing the focus source means every visibility we hold is a guess,
    // and a guess must never keep an app frozen.
    for seed in 0..SESSIONS {
        let (mut s, log, _) = random_session(seed, 60);
        let restores = s.feed(Event::FocusSourceHealth { healthy: false });
        for i in &restores {
            s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
        }
        let held: Vec<String> = s.planner.held().map(|(k, a)| format!("{k} {a:?}")).collect();
        assert!(held.is_empty(), "still holding {held:?} while blind\n  {}", story(&log));

        // And it stays that way: further events must not conjure a new hold out
        // of a world nobody can see.
        for _ in 0..10 {
            let more = s.advance(Duration::from_secs(300));
            assert!(more.iter().all(|i| i.action.is_restore()), "{more:?}\n  {}", story(&log));
        }
    }
}

#[test]
fn a_panic_thaw_leaves_nothing_held() {
    // The one command whose contract is absolute: it ignores rules, ignores mode,
    // and is what a user types when something has gone wrong.
    for seed in 0..SESSIONS {
        let (mut s, log, _) = random_session(seed, 60);
        let thawed = s.feed(Event::ThawAll);
        for i in &thawed {
            s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
        }
        assert_eq!(s.planner.held().count(), 0, "held after a panic thaw\n  {}", story(&log));
        assert!(thawed.iter().all(|i| i.action.is_restore()), "{thawed:?}");
    }
}

#[test]
fn shutting_down_restores_everything() {
    // A freeze outlives the process that applied it (§10.3), so exiting without
    // restoring strands apps frozen with nothing left to thaw them.
    for seed in 0..SESSIONS {
        let (mut s, log, _) = random_session(seed, 60);
        let restores = s.feed(Event::Shutdown);
        for i in &restores {
            s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
        }
        assert_eq!(s.planner.held().count(), 0, "held after shutdown\n  {}", story(&log));
    }
}

#[test]
fn suspending_restores_everything_first() {
    // §10.4. A frozen app across a suspend wakes into a world that moved on
    // without it, and the thaw has to happen while the system is still awake to
    // perform it.
    for seed in 0..SESSIONS {
        let (mut s, log, _) = random_session(seed, 60);
        let restores = s.feed(Event::PrepareForSleep { sleeping: true });
        for i in &restores {
            s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
        }
        assert_eq!(s.planner.held().count(), 0, "held into a suspend\n  {}", story(&log));
    }
}

#[test]
fn the_focused_app_is_only_ever_touched_by_a_rule_that_asks_for_it() {
    // dozed's first promise is that a user cannot tell it is running, and the
    // foreground window is where that promise is easiest to break. A rule may opt
    // in — `visibility = "focused"` — but it cannot be arrived at by a predicate
    // that happens to match.
    for seed in 0..SESSIONS {
        let mut rng = Rng::seeded(seed);
        let mut cfg = random_config(&mut rng);
        // Strip the opt-in, so any hold on a focused app is a bug rather than a
        // request.
        cfg.rules.retain(|r| {
            r.when.visibility != Some(Visibility::Focused)
                && r.match_.visibility != Some(Visibility::Focused)
        });
        if cfg.rules.is_empty() {
            continue;
        }
        let mut s = Scenario::new(cfg);
        for name in APPS {
            s.feed(Event::AppSeen(Box::new(app_seen(name))));
        }
        let focused = AppKey::comm("editor");
        s.focus(&focused);

        for _ in 0..40 {
            let intents = match rng.below(4) {
                0 => s.sample(&focused, 90.0, rng.below(3000) as f32),
                1 => s.set_power(ON_BATTERY),
                2 => s.set_desktop(DesktopState::Away),
                _ => s.advance(Duration::from_secs(600)),
            };
            for i in &intents {
                s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
                assert!(
                    i.app != focused || i.action.is_restore(),
                    "seed {seed}: focused app was asked to {}",
                    i.action.verb()
                );
            }
        }
    }
}

#[test]
fn every_app_can_always_be_explained() {
    // §1's promise: `dozectl why <app>` answers, always. An app the planner has
    // no opinion about must still produce a derivation rather than nothing, or the
    // command has a hole in it that only shows up on somebody else's desktop.
    for seed in 0..SESSIONS {
        let (s, log, _) = random_session(seed, 40);
        for name in APPS {
            let key = AppKey::comm(name);
            let explanation = s.planner.explain(&s.world, &key, s.now);
            assert!(explanation.is_some(), "no answer for {name}\n  {}", story(&log));
            let e = explanation.unwrap();
            assert!(!e.summary().is_empty());
            assert!(!e.decision.reason.detail.is_empty(), "an empty reason explains nothing");
        }
    }
}

#[test]
fn an_intent_is_only_ever_issued_when_it_changes_something() {
    // The property that keeps the journal readable and the actuator idle: the
    // planner emits an intent when the *applied* action changes, never as a
    // heartbeat. A regression here shows up as a journal line every tick.
    for seed in 0..SESSIONS {
        let mut rng = Rng::seeded(seed);
        let config = random_config(&mut rng);
        let mut s = Scenario::new(config);
        for name in APPS {
            s.feed(Event::AppSeen(Box::new(app_seen(name))));
        }
        let mut applied: std::collections::BTreeMap<AppKey, Action> =
            APPS.iter().map(|n| (AppKey::comm(*n), Action::Restore)).collect();

        for _ in 0..50 {
            let intents = match rng.below(3) {
                0 => s.hide(&AppKey::comm(APPS[rng.below(APPS.len())])),
                1 => s.focus(&AppKey::comm(APPS[rng.below(APPS.len())])),
                _ => s.advance(Duration::from_secs(120)),
            };
            for i in &intents {
                let before = applied.get(&i.app).copied().unwrap_or(Action::Restore);
                // A maintenance window deliberately re-asserts a freeze it just
                // lifted, and that is a change in the applied action both ways —
                // so this holds for it too.
                assert_ne!(
                    before, i.action,
                    "seed {seed}: {} was asked for {:?} while already in it",
                    i.app, i.action
                );
                applied.insert(i.app.clone(), i.action);
                s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
            }
        }
    }
}

#[test]
fn grace_never_grows_as_the_battery_empties() {
    // §8's model: patience is proportional to charge. Not a claim about any one
    // number, but about the shape — a fuller battery is never *less* patient, or
    // the tool becomes more aggressive exactly when it has least to gain.
    for seed in 0..SESSIONS {
        let mut rng = Rng::seeded(seed);
        let base = Duration::from_secs(30 + 30 * rng.below(20) as u64);
        let desktop = match rng.below(3) {
            0 => DesktopState::Active,
            1 => DesktopState::IdlePresent,
            _ => DesktopState::Away,
        };
        let cfg = Config::empty();
        let mut previous: Option<Duration> = None;
        // Walk the charge downwards, so each step must be no more patient than
        // the last.
        for pct in (0..=100).rev().step_by(5) {
            let power = PowerState { on_ac: false, percentage: Some(pct as f32) };
            let grace = dozed_core::Grace::compute(&cfg, base, &power, desktop, false);
            let effective = grace.effective().unwrap_or_default();
            if let Some(p) = previous {
                assert!(
                    effective <= p,
                    "seed {seed}: grace grew from {p:?} to {effective:?} as charge fell to {pct}%"
                );
            }
            previous = Some(effective);
        }
    }
}

#[test]
fn a_long_running_job_is_never_frozen_by_a_generic_rule() {
    // §9: background CPU is the *job* for a build, a backup or a sync, so the
    // class exists to keep a catch-all rule from stopping one. A rule naming the
    // app is a different matter and is allowed.
    for seed in 0..SESSIONS {
        let mut rng = Rng::seeded(seed);
        let mut cfg = random_config(&mut rng);
        // Only generic rules: anything naming an app is the user being explicit.
        cfg.rules.retain(|r| r.match_.app.is_none());
        // The class-based exemption the shipped config ships, which is the thing
        // under test.
        cfg.rules.insert(
            0,
            Rule::new("long-job-exempt", 0, RuleAction::Never).matching(Predicates {
                class: Some(AppClass::LongRunningJob),
                ..Default::default()
            }),
        );
        cfg.validate().unwrap();

        let mut s = Scenario::new(cfg);
        let job = AppKey::comm("backup");
        s.feed(Event::AppSeen(Box::new(dozed_core::AppSeen {
            class: AppClass::LongRunningJob,
            ..app_seen("backup")
        })));
        s.hide(&job);
        for _ in 0..20 {
            let intents = s.advance(Duration::from_secs(300));
            for i in &intents {
                s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
                assert!(
                    i.app != job || i.action.is_restore(),
                    "seed {seed}: a long-running job was asked to {}",
                    i.action.verb()
                );
            }
        }
    }
}
