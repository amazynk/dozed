//! Tests for [`super`], in a file of their own, so that the file a reader opens to
//! understand what the code does is not mostly assertions about it. Still a child
//! module rather than an integration test: these reach private items, and the
//! `#[path]` attribute on the declaration keeps `super::*` meaning what it would at
//! the bottom of that file.

use super::*;
use crate::app::AppClass;
use crate::config::{Config, Mode, Patterns, Predicates, Rule, RuleAction};
use crate::event::{DesktopState, Exemption};
use crate::testing::{ON_AC, ON_BATTERY, Scenario, protected_app};

const MIN: Duration = Duration::from_secs(60);

fn freeze_after(grace: &str) -> Config {
    let mut cfg = Config::empty();
    cfg.rules = vec![Rule::new("test-freeze", 100, RuleAction::Freeze).when(Predicates {
        visibility: Some(Visibility::Hidden),
        for_: Some(grace.parse().unwrap()),
        ..Default::default()
    })];
    cfg
}

fn only(intents: &[Intent]) -> &Intent {
    assert_eq!(intents.len(), 1, "expected exactly one intent, got {intents:?}");
    &intents[0]
}

#[test]
fn hidden_app_freezes_once_grace_expires() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    assert!(s.hide(&slack).is_empty(), "hiding alone must not act");

    assert!(s.advance(4 * MIN).is_empty(), "still inside grace");

    let intents = s.advance(MIN);
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Freeze { reclaim: false });
    assert_eq!(intent.reason.rule.as_deref(), Some("test-freeze"));
    assert_eq!(s.planner.applied(&slack), Action::Freeze { reclaim: false });
}

/// The opt-in rule: a glance does not earn the full grace back.
fn redoze_after_a_glance(visit_under: f32, wait: &str) -> Config {
    let mut cfg = freeze_after("5m");
    cfg.rules.push(Rule::new("redoze-after-a-glance", 40, RuleAction::Freeze).when(Predicates {
        visibility: Some(Visibility::Hidden),
        focused_for: Some(crate::config::Threshold::Bounds { gt: None, lt: Some(visit_under) }),
        for_: Some(wait.parse().unwrap()),
        ..Default::default()
    }));
    // Ordered the way `Config::validate` orders it, so the test cannot pass
    // by sitting first in the vec while the real precedence would put it
    // last. A lower number wins, and the documented example was written at
    // 200 — below every ordinary doze rule, where it could never have fired.
    cfg.rules.sort_by_key(|r| r.priority);
    cfg
}

#[test]
fn a_glance_re_dozes_on_the_short_wait_not_the_full_grace() {
    let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert_eq!(only(&s.advance(6 * MIN)).action, Action::Freeze { reclaim: false });

    // The user glances at it for three seconds and leaves.
    s.focus(&slack);
    s.advance(Duration::from_secs(3));
    s.hide(&slack);

    assert!(s.advance(Duration::from_secs(3)).is_empty(), "the short wait has not passed");
    let intents = s.advance(Duration::from_secs(3));
    assert_eq!(
        only(&intents).action,
        Action::Freeze { reclaim: false },
        "a three-second visit must not buy the full five minutes back"
    );
    assert_eq!(only(&intents).reason.rule.as_deref(), Some("redoze-after-a-glance"));
}

#[test]
fn a_glance_rules_wait_is_not_stretched_by_a_busy_desktop() {
    // The regression this guards is that the rule became nominal. Measured
    // live before the fix: a stated 15s wait came out as 4m, because the
    // activity factor multiplied it exactly when the rule most needed to
    // act — a glance only happens while the user is switching windows, so
    // `Active` is the normal case for it, not an edge one.
    let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
    let app = s.spawn("slack");
    s.set_desktop(DesktopState::Active);
    s.hide(&app);
    s.advance(30 * MIN);
    s.focus(&app);
    s.advance(Duration::from_secs(2));
    s.hide(&app);

    assert!(s.advance(Duration::from_secs(3)).is_empty(), "the stated wait has not passed");
    let intents = s.advance(Duration::from_secs(3));
    let intent = only(&intents);
    assert_eq!(intent.reason.rule.as_deref(), Some("redoze-after-a-glance"));
    assert_eq!(
        intent.reason.grace.map(|g| g.activity),
        Some(1.0),
        "the activity factor must not apply to a glance rule: {:?}",
        intent.reason.grace
    );
}

#[test]
fn a_real_visit_keeps_the_ordinary_grace() {
    // The predicate has to tell a glance from work, or it is just "re-doze
    // everything immediately" wearing a threshold.
    let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(6 * MIN);

    s.focus(&slack);
    s.advance(Duration::from_secs(30)); // actually used it
    s.hide(&slack);

    assert!(s.advance(Duration::from_secs(20)).is_empty(), "30s is not a glance");
    assert_eq!(
        only(&s.advance(6 * MIN)).action,
        Action::Freeze { reclaim: false },
        "it still re-dozes, just on the ordinary schedule"
    );
}

#[test]
fn the_glance_rule_is_not_blocked_by_the_resume_cooldown() {
    // Without the exemption the rule could never fire: the cooldown floor is
    // measured from the very return that made the visit brief, so it is
    // always in force exactly when this rule wants to act.
    let mut cfg = redoze_after_a_glance(10.0, "5s");
    cfg.defaults.resume_cooldown = "10m".parse().unwrap();
    cfg.defaults.resume_cooldown_mode = CooldownMode::Floor;

    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(6 * MIN);
    s.focus(&slack);
    s.advance(Duration::from_secs(2));
    s.hide(&slack);

    let intents = s.advance(Duration::from_secs(6));
    assert_eq!(
        only(&intents).action,
        Action::Freeze { reclaim: false },
        "a ten-minute cooldown floor must not swallow the glance rule"
    );
}

/// One glance is an event; a run of them is the pattern the term exists for.
///
/// The numbers this guards came from a real 13-day journal: the two shipped
/// glance rules produced 1,459 of 1,955 rule-fired decisions, with a median
/// of 13 seconds between a thaw and the next freeze and a minimum of one —
/// 148 holds lasted three seconds or less. Every one of those was correct by
/// the rules and a net loss in practice.
#[test]
fn a_run_of_short_holds_backs_the_grace_off_and_one_does_not() {
    let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert_eq!(only(&s.advance(6 * MIN)).action, Action::Freeze { reclaim: false });

    // Glance once: an event, not a pattern. The rule's stated wait stands.
    s.focus(&slack);
    s.advance(Duration::from_secs(2));
    s.hide(&slack);
    let intents = s.advance(Duration::from_secs(6));
    assert_eq!(only(&intents).action, Action::Freeze { reclaim: false });
    assert_eq!(
        only(&intents).reason.grace.map(|g| g.flap),
        Some(1.0),
        "one short hold must not move anything"
    );

    // Glance again. That hold lasted six seconds, under `MIN_PAYBACK`, and
    // it is the second in a row — so the wait doubles.
    s.focus(&slack);
    s.advance(Duration::from_secs(2));
    s.hide(&slack);
    assert!(
        s.advance(Duration::from_secs(6)).is_empty(),
        "the stated 5s wait must no longer be enough"
    );
    let intents = s.advance(Duration::from_secs(6));
    assert_eq!(
        only(&intents).reason.grace.map(|g| g.flap),
        Some(2.0),
        "the second short hold in a row doubles the wait"
    );
}

#[test]
fn a_cap_only_ceiling_throttles_what_a_rule_wanted_to_freeze() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.planner.set_hold(&slack, Ceiling::CapOnly { pct: Some(20) }, None, s.now);
    s.hide(&slack);

    let intents = s.advance(6 * MIN);
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Throttle { pct: 20 }, "capped, not frozen");
    // The rule that wanted to act is still named: a ceiling changes how far,
    // not whether, and a log that hid the rule would answer the wrong
    // question.
    assert_eq!(intent.reason.rule.as_deref(), Some("test-freeze"));
    assert!(intent.reason.detail.contains("wanted doze"), "{}", intent.reason.detail);
    assert!(intent.reason.detail.contains("20%"), "{}", intent.reason.detail);
}

#[test]
fn leaving_an_app_alone_stops_the_rule_and_releases_what_is_held() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert_eq!(only(&s.advance(6 * MIN)).action, Action::Freeze { reclaim: false });

    // Set while it is frozen: it has to come back, not sit there for the
    // life of the ceiling.
    s.planner.set_hold(&slack, Ceiling::LeaveAlone, None, s.now);
    let intents = s.advance(Duration::from_secs(5));
    assert_eq!(only(&intents).action, Action::Restore);
    assert!(only(&intents).reason.detail.contains("releasing"), "{}", only(&intents).reason.detail);

    // And it stays released: the rule keeps wanting to act and keeps being
    // refused, without a fresh intent every tick.
    assert!(s.advance(30 * MIN).is_empty(), "no churn while the ceiling stands");
    assert_eq!(s.planner.applied(&slack), Action::Restore);
}

#[test]
fn a_ceiling_lapses_and_the_rules_have_their_way_again() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    let until = s.now.plus(10 * MIN);
    s.planner.set_hold(&slack, Ceiling::LeaveAlone, Some(until), s.now);
    s.hide(&slack);

    assert!(s.advance(6 * MIN).is_empty(), "grace expired, but the ceiling holds");
    assert!(s.planner.hold(&slack, s.now).is_some());

    // Past the deadline the ceiling is simply not there any more.
    let intents = s.advance(6 * MIN);
    assert_eq!(only(&intents).action, Action::Freeze { reclaim: false });
    assert!(s.planner.hold(&slack, s.now).is_none(), "and it reports as lapsed");
}

#[test]
fn a_never_rule_still_outranks_a_ceiling_that_would_permit_a_cap() {
    // A ceiling limits how far up the ladder we go. It is not a licence to
    // act on something the rules exempt, and the order of the two checks is
    // what guarantees that.
    let mut cfg = freeze_after("5m");
    cfg.rules.insert(
        0,
        Rule::new("hands-off", 0, RuleAction::Never).matching(Predicates {
            app: Some(Patterns::One("slack".to_owned())),
            ..Default::default()
        }),
    );
    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.planner.set_hold(&slack, Ceiling::CapOnly { pct: Some(20) }, None, s.now);
    s.hide(&slack);

    assert!(s.advance(30 * MIN).is_empty(), "an exempt app is exempt, ceiling or not");
    assert_eq!(s.planner.applied(&slack), Action::Restore);
}

#[test]
fn an_app_that_may_not_be_touched_has_nothing_pending() {
    // A countdown to something we already knew we would refuse is exactly
    // what `PendingChange` promises not to be. Seen on screen as
    // `left alone (29m left), dozes in 1m33s`.
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(MIN);
    assert!(s.planner.pending(&slack, s.now).is_some(), "counting down to a freeze");

    s.planner.set_hold(&slack, Ceiling::LeaveAlone, Some(s.now.plus(30 * MIN)), s.now);
    s.advance(Duration::from_secs(1));
    assert_eq!(s.planner.pending(&slack, s.now), None, "nothing is going to happen to it");

    // A cap keeps its countdown: it *will* be acted on, just not as far.
    s.planner.set_hold(&slack, Ceiling::CapOnly { pct: None }, Some(s.now.plus(30 * MIN)), s.now);
    s.advance(Duration::from_secs(1));
    assert!(s.planner.pending(&slack, s.now).is_some(), "still going to be capped");
}

#[test]
fn holds_are_reported_for_persistence_and_drop_out_when_they_lapse() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    let until = s.now.plus(10 * MIN);
    s.planner.set_hold(&slack, Ceiling::CapOnly { pct: None }, Some(until), s.now);

    let held: Vec<_> = s.planner.holds(s.now).map(|(k, h)| (k.clone(), h)).collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].1.ceiling, Ceiling::CapOnly { pct: None });

    assert_eq!(s.planner.holds(until).count(), 0, "a lapsed hold is not worth persisting");
    assert!(s.planner.clear_hold(&slack), "and it can be cleared by hand");
    assert!(!s.planner.clear_hold(&slack), "clearing twice says nothing happened");
}

/// Let grace run out, then glance at the app the instant it is taken.
///
/// Durations are not hard-coded because the whole point of the backoff is
/// that the wait *changes*: a test that advanced a fixed 30s would stop
/// finding the freeze as soon as the mechanism it is testing started working.
/// Ticking until the intent appears and glancing on the same tick is what
/// makes the hold reliably shorter than `MIN_PAYBACK`.
fn glance_after_a_freeze(s: &mut Scenario, app: &AppKey) {
    for _ in 0..200 {
        if !s.advance(Duration::from_secs(5)).is_empty() {
            break;
        }
    }
    s.focus(app);
    s.advance(Duration::from_secs(2));
    s.hide(app);
}

#[test]
fn the_backoff_decays_one_step_at_a_time_rather_than_collapsing() {
    // Reset-on-recovery is what made this oscillate in production. The
    // backoff pushes holds just past `MIN_PAYBACK` by design, so treating
    // "past MIN_PAYBACK" as recovery let it read its own success as proof it
    // was unnecessary — climb to the cap, collapse, climb again.
    let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
    let slack = s.spawn("slack");
    s.hide(&slack);

    // Three glances in a row, each cutting the hold short the moment it is
    // taken: the backoff climbs.
    for _ in 0..3 {
        glance_after_a_freeze(&mut s, &slack);
    }
    let climbed = s.planner.flap_of(&slack);
    assert!(climbed > 1.0, "three short holds must back it off, got {climbed}");

    // Now leave it alone for a good while past `SETTLED_HOLD`, then glance
    // once. That steps it down by one — it does not clear it.
    for _ in 0..200 {
        if !s.advance(Duration::from_secs(5)).is_empty() {
            break;
        }
    }
    s.advance(5 * MIN);
    s.focus(&slack);
    s.advance(Duration::from_secs(2));
    s.hide(&slack);

    let after = s.planner.flap_of(&slack);
    assert!(after < climbed, "a long hold steps it down: {after} vs {climbed}");
    assert!(after > 1.0, "but one long hold does not clear it outright: {after}");
}

/// A hold that only lasted as long as the backoff made it last says nothing
/// about the app, and must not be read as recovery.
#[test]
fn a_hold_the_backoff_itself_produced_is_not_evidence_of_recovery() {
    let mut memo = AppMemo::new(MonoTime::ZERO);
    memo.ask_for(Action::Freeze { reclaim: false }, MonoTime::ZERO);

    // Two short holds to establish the pattern.
    for _ in 0..2 {
        memo.note_return(memo.applied_at.plus(Duration::from_secs(2)));
        memo.ask_for(Action::Freeze { reclaim: false }, memo.applied_at);
    }
    assert_eq!(memo.short_holds, 2, "flapping");

    // A hold in the dead band: longer than MIN_PAYBACK, shorter than
    // SETTLED_HOLD. Exactly where a working floor parks them.
    memo.note_return(memo.applied_at.plus(MIN_PAYBACK + Duration::from_secs(2)));
    assert_eq!(memo.short_holds, 2, "the dead band changes nothing in either direction");

    // Past SETTLED_HOLD, and it steps down by one.
    memo.ask_for(Action::Freeze { reclaim: false }, memo.applied_at);
    memo.note_return(memo.applied_at.plus(SETTLED_HOLD + Duration::from_secs(1)));
    assert_eq!(memo.short_holds, 1);
}

/// The floor, which is what makes the backoff mean anything at a small
/// `aggression`.
#[test]
fn a_flapping_app_never_waits_less_than_a_hold_can_pay_for() {
    // The config this was found on: three times more impatient than the most
    // impatient named level, turning a 15s rule into a 1.5s wait.
    let mut cfg = redoze_after_a_glance(10.0, "15s");
    cfg.defaults.aggression = crate::config::Aggression::custom(0.05).unwrap();

    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);

    for _ in 0..3 {
        glance_after_a_freeze(&mut s, &slack);
    }
    let mut intents = Vec::new();
    for _ in 0..200 {
        intents = s.advance(Duration::from_secs(5));
        if !intents.is_empty() {
            break;
        }
    }
    let grace = only(&intents).reason.grace.expect("a rule decision carries its grace");
    let effective = grace.effective().expect("acting");
    assert!(
        effective >= MIN_PAYBACK,
        "a flapping app must not be re-taken on a wait no hold could pay for: {effective:?}              from {grace:?}"
    );
    // And the derivation still multiplies out, because the floor is applied
    // *in* the term rather than beside it.
    assert!(grace.flap > 8.0, "the doubling alone could not have got there: {}", grace.flap);
}

/// The floor is a claim about the number the planner returns, so it has to
/// survive every term applied after it.
///
/// The regression this pins was invisible to every other test in this file for
/// one reason: `Scenario` starts on `DesktopState::IdlePresent`, where
/// `activity` is 1.0 and the glance rule's flattening of it is a no-op. On an
/// **active** desktop — `active_factor = 4.0`, the shipped default, and the
/// state the live journal records for essentially every thrash cycle — the
/// floor was raised against a product four times larger than the one that
/// would be used, declared itself satisfied, and was then divided back down
/// below itself.
///
/// Measured on the machine that found it, with `aggression = 0.05`: graces of
/// 1s, 3s, 6s and 12s at flap factors of 1, 2, 4 and 8, against a floor that
/// promised ten seconds at worst. 24% of all holds in a 24-hour window ended
/// inside `MIN_PAYBACK`.
#[test]
fn the_payback_floor_survives_the_glance_flattening() {
    let mut cfg = redoze_after_a_glance(10.0, "15s");
    cfg.defaults.aggression = crate::config::Aggression::custom(0.05).unwrap();
    assert_eq!(cfg.activity.active_factor, 4.0, "the shipped default is what made this bite");

    let mut s = Scenario::new(cfg);
    // The state the journal shows for the thrash, and the one no other test
    // in this file uses.
    s.set_desktop(DesktopState::Active);
    let slack = s.spawn("slack");
    s.hide(&slack);

    for _ in 0..3 {
        glance_after_a_freeze(&mut s, &slack);
    }
    let mut intents = Vec::new();
    for _ in 0..200 {
        intents = s.advance(Duration::from_secs(5));
        if !intents.is_empty() {
            break;
        }
    }
    let grace = only(&intents).reason.grace.expect("a rule decision carries its grace");
    assert_eq!(grace.activity, 1.0, "the glance rule still flattens the activity term");
    let effective = grace.effective().expect("acting");
    assert!(
        effective >= MIN_PAYBACK,
        "the floor must hold against the term applied after it: {effective:?} from {grace:?}"
    );
}

/// The backoff converges rather than running away.
#[test]
fn the_backoff_stops_at_its_cap() {
    assert_eq!(flap_factor(0), 1.0);
    assert_eq!(flap_factor(1), 1.0, "one short hold is an event, not a pattern");
    assert_eq!(flap_factor(2), 2.0);
    assert_eq!(flap_factor(3), 4.0);
    assert_eq!(flap_factor(4), MAX_FLAP);
    assert_eq!(flap_factor(u32::MAX), MAX_FLAP, "and it cannot overflow past it");
}

/// The regression the suspend case exposed: letting go because the *machine*
/// stopped says nothing about whether holding the app was working.
#[test]
fn a_thaw_the_user_did_not_ask_for_is_not_a_failed_hold() {
    // Two machine thaws, each of a hold that had lasted no time at all.
    // Neither is evidence about the app, so neither may be counted — and if
    // either were, the single glance at the end would be the *third* short
    // hold and would come out at 4.0 rather than 1.0.
    //
    // `ThawAll` used to be in this list and no longer is, because it is the
    // one event reaching `restore_all` that this test's own title excludes: a
    // panic thaw is the user asking, as `dozectl thaw <app>` already counted
    // as. Holding it here cost a measured eighteen seconds of a user
    // hammering the hotkey against an app that came back in 1.5 every time.
    // See `a_panic_thaw_is_a_return_and_arms_the_backoff`.
    let sleep_then_wake: &[Event] =
        &[Event::PrepareForSleep { sleeping: true }, Event::PrepareForSleep { sleeping: false }];
    let lost_then_found: &[Event] =
        &[Event::FocusSourceHealth { healthy: false }, Event::FocusSourceHealth { healthy: true }];
    for machine_thaw in [sleep_then_wake, lost_then_found] {
        let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
        let slack = s.spawn("slack");
        s.hide(&slack);
        s.advance(6 * MIN);

        for _ in 0..2 {
            for event in machine_thaw {
                s.feed(event.clone());
            }
            // Both reset the grace baseline, so this re-freezes on the
            // ordinary five-minute rule rather than the glance one.
            s.advance(6 * MIN);
        }

        s.focus(&slack);
        s.advance(Duration::from_secs(2));
        s.hide(&slack);
        let intents = s.advance(Duration::from_secs(6));
        assert_eq!(
            only(&intents).reason.grace.map(|g| g.flap),
            Some(1.0),
            "{machine_thaw:?} must not count against the app"
        );
    }
}

/// Under the default `CooldownMode::Floor` the cooldown is a minimum wait,
/// so `Grace::cooldown` is structurally 1.0 and cannot record the bypass.
/// Without this flag a journal line showing `in_cooldown: true` beside a
/// freeze taken seconds after a thaw had nothing on it that explained why.
#[test]
fn the_journal_records_that_a_glance_rule_bypassed_the_cooldown() {
    let mut cfg = redoze_after_a_glance(10.0, "5s");
    cfg.defaults.resume_cooldown = "10m".parse().unwrap();
    cfg.defaults.resume_cooldown_mode = CooldownMode::Floor;

    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(6 * MIN);
    s.focus(&slack);
    s.advance(Duration::from_secs(2));
    s.hide(&slack);

    let intents = s.advance(Duration::from_secs(6));
    let inputs = only(&intents).reason.inputs.as_ref().expect("a rule decision carries inputs");
    assert!(inputs.in_cooldown, "the ten-minute floor is very much in force");
    assert!(inputs.cooldown_bypassed, "and the glance rule went ahead anyway");
    assert!(
        inputs.summary().contains("cooldown-bypassed"),
        "`dozectl why` has to say so too: {}",
        inputs.summary()
    );
}

#[test]
fn an_assumed_focus_is_not_a_visit() {
    // Every app is created `Focused` — assume the worst until the
    // compositor says otherwise — so every app passes through a Focused
    // state nobody observed. Counting that as a visit gave the whole
    // session a near-zero `focused_for` on its first snapshot, and this rule
    // then re-dozed everything on the 5s wait moments after startup instead
    // of on its own grace. `focused_for` unset must not read as zero.
    let mut s = Scenario::new(redoze_after_a_glance(10.0, "5s"));
    let slack = s.spawn("slack");
    s.hide(&slack); // the assumed focus ends here, and does not count
    assert_eq!(s.world.app(&slack).unwrap().focused_for, None);

    assert!(s.advance(Duration::from_secs(10)).is_empty(), "an assumption is not a glance");
    assert_eq!(
        only(&s.advance(6 * MIN)).reason.rule.as_deref(),
        Some("test-freeze"),
        "it falls through to the ordinary rule"
    );
}

#[test]
fn the_dynamic_throttle_converges_and_is_reported_each_step() {
    // End to end through the planner, against a plant where capping does
    // reduce wakeups. What matters here is not the control law — that is
    // `throttle`'s own tests — but that the loop is driven by real samples,
    // that each move is journalled with its reason, and that the caps land
    // somewhere sane rather than at the floor.
    let mut cfg = Config::empty();
    cfg.rules = vec![Rule::new("budget", 100, RuleAction::ThrottleTo(50)).when(Predicates {
        visibility: Some(Visibility::Hidden),
        for_: Some("1m".parse().unwrap()),
        ..Default::default()
    })];
    let mut s = Scenario::new(cfg);
    let app = s.spawn("chatty");
    s.hide(&app);

    let free_rate = 1000.0;
    let mut cap = 100u8;
    let mut seen = Vec::new();
    for _ in 0..10 {
        // The app's rate follows the cap it is under.
        s.sample(&app, 1.0, free_rate * f32::from(cap) / 100.0);
        for intent in s.advance(2 * MIN) {
            if let Action::Throttle { pct } = intent.action {
                cap = pct;
                seen.push((pct, intent.reason.detail.clone()));
            }
        }
    }

    assert!(!seen.is_empty(), "the controller never acted");
    let (settled, detail) = seen.last().unwrap();
    assert!(
        (5..=15).contains(settled),
        "50 of 1000/s is 5% of its free rate; settled at {settled}% via {seen:?}"
    );
    // Every move has to carry its own justification, or a cap that moves is
    // a number nobody can account for.
    assert!(detail.contains("/s"), "the reason must show the measurement: {detail}");
    assert!(
        seen.iter().any(|(_, d)| d.contains("tightening")),
        "the steps should say what they did: {seen:?}"
    );
}

/// The shipped low-battery rule, plus a catch-all so the ordinary paths are
/// still exercised.
fn foreground_on_low_battery() -> Config {
    let mut cfg = freeze_after("5m");
    cfg.rules.push(Rule::new("low-battery-foreground", 35, RuleAction::ThrottleTo(50)).when(
        Predicates {
            visibility: Some(Visibility::Focused),
            desktop_state: Some(DesktopState::IdlePresent),
            battery_below: Some(20.0),
            for_: Some("5m".parse().unwrap()),
            ..Default::default()
        },
    ));
    cfg.rules.sort_by_key(|r| r.priority);
    cfg
}

#[test]
fn a_focused_app_nobody_is_typing_into_is_throttled_on_low_battery() {
    let mut s = Scenario::new(foreground_on_low_battery());
    let app = s.spawn("slack");
    s.focus(&app);
    s.set_power(crate::event::PowerState { on_ac: false, percentage: Some(15.0) });
    // Input goes to the focused window, so a desktop that is no longer
    // Active is exactly an app receiving no input.
    s.set_desktop(DesktopState::IdlePresent);

    let intents = s.advance(20 * MIN);
    let intent = only(&intents);
    assert!(matches!(intent.action, Action::Throttle { .. }), "{:?}", intent.action);
    assert_eq!(intent.reason.rule.as_deref(), Some("low-battery-foreground"));
}

#[test]
fn a_focused_app_being_typed_into_is_left_alone() {
    // The whole risk of reaching the foreground at all. `Active` means input
    // within the threshold, and input goes to this window.
    let mut s = Scenario::new(foreground_on_low_battery());
    let app = s.spawn("slack");
    s.focus(&app);
    s.set_power(crate::event::PowerState { on_ac: false, percentage: Some(5.0) });
    s.set_desktop(DesktopState::Active);
    assert!(s.advance(60 * MIN).is_empty(), "never touch the window in use");
}

#[test]
fn a_healthy_battery_keeps_the_foreground_untouchable() {
    let mut s = Scenario::new(foreground_on_low_battery());
    let app = s.spawn("slack");
    s.focus(&app);
    s.set_power(crate::event::PowerState { on_ac: false, percentage: Some(90.0) });
    s.set_desktop(DesktopState::IdlePresent);
    assert!(s.advance(60 * MIN).is_empty(), "this is a low-battery measure only");
}

#[test]
fn no_rule_may_freeze_the_foreground_however_it_is_written() {
    // The invariant that makes reaching the foreground safe at all: a frozen
    // focused window is a stale rectangle that reads as a crash, so the
    // planner refuses regardless of what the rule asks for.
    let mut cfg = Config::empty();
    cfg.rules = vec![
        Rule::new("freeze-the-world", 1, RuleAction::Freeze)
            .when(Predicates { visibility: Some(Visibility::Focused), ..Default::default() }),
    ];
    let mut s = Scenario::new(cfg);
    let app = s.spawn("slack");
    s.focus(&app);
    assert!(s.advance(60 * MIN).is_empty(), "a freeze rule must not reach the foreground");
}

#[test]
fn a_rule_that_did_not_ask_for_the_foreground_cannot_reach_it() {
    // Opting in is explicit in the rule text, so no existing rule — they all
    // say `hidden` — starts acting on the window you are looking at.
    let mut cfg = Config::empty();
    cfg.rules = vec![Rule::new("catch-all-throttle", 1, RuleAction::Throttle(5))];
    let mut s = Scenario::new(cfg);
    let app = s.spawn("slack");
    s.focus(&app);
    assert!(s.advance(60 * MIN).is_empty());
}

#[test]
fn an_exemption_still_wins_over_a_foreground_rule() {
    let mut cfg = foreground_on_low_battery();
    cfg.rules.insert(
        0,
        Rule::new("never-slack", 1, RuleAction::Never).matching(Predicates {
            app: Some(Patterns::One("slack".to_owned())),
            ..Default::default()
        }),
    );
    let mut s = Scenario::new(cfg);
    let app = s.spawn("slack");
    s.focus(&app);
    s.set_power(crate::event::PowerState { on_ac: false, percentage: Some(5.0) });
    s.set_desktop(DesktopState::IdlePresent);
    assert!(s.advance(60 * MIN).is_empty(), "a never rule outranks the foreground rule");
}

#[test]
fn an_action_the_actuator_refused_is_not_believed() {
    // The live bug: `dozectl apps` reported thunderbird `throttled` and
    // counted it as held, while every throttle had been skipped for want of
    // a cgroup scope and nothing whatsoever was holding it.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intent = only(&s.advance(2 * MIN)).clone();
    assert_eq!(intent.action, Action::Freeze { reclaim: false });

    s.planner.note_outcome(&slack, intent.action, &Outcome::Skipped { why: "no scope".into() });

    assert_eq!(
        s.planner.applied(&slack),
        Action::Restore,
        "a skipped freeze leaves the app untouched, and we must say so"
    );
    assert_eq!(
        s.planner.held().count(),
        0,
        "held() feeds the crash-safety file; it must not list a hold that never happened"
    );
}

#[test]
fn a_refused_action_is_not_retried_every_tick() {
    // Rolling `applied` back without remembering the refusal would put an
    // identical skipped line in the journal on every tick forever.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intent = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, intent.action, &Outcome::Failed { error: "nope".into() });

    assert!(s.advance(MIN).is_empty(), "must not re-issue what was just refused");
    assert!(s.advance(10 * MIN).is_empty(), "still silent much later");
    assert_eq!(s.planner.applied(&slack), Action::Restore);
}

#[test]
fn a_refusal_does_not_outlive_the_circumstances_it_happened_in() {
    // Otherwise the suppression becomes its own permanent lie: an app that
    // could not be frozen once would never be tried again, even after the
    // user has been back to it and left it a second time.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let first = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, first.action, &Outcome::Skipped { why: "no scope".into() });
    assert!(s.advance(MIN).is_empty(), "suppressed while nothing has changed");

    s.focus(&slack); // wanting Restore instead clears the refusal
    s.hide(&slack);
    let retried = s.advance(2 * MIN);
    assert_eq!(
        only(&retried).action,
        Action::Freeze { reclaim: false },
        "a fresh hide earns a fresh attempt"
    );
}

#[test]
fn dry_run_still_advances_its_own_state_machine() {
    // `WouldHave` is not a failure. Dry-run exists to model live mode
    // exactly, so its state must move the way live mode's would — otherwise
    // the counterfactual journal stops matching what live would have done.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intent = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, intent.action, &Outcome::WouldHave);

    assert_eq!(s.planner.applied(&slack), Action::Freeze { reclaim: false });
    assert!(s.advance(MIN).is_empty(), "and it is not asked for twice");
}

#[test]
fn going_live_re_asserts_what_dry_run_only_pretended_to_hold() {
    // The bug, and it was silent and total: dry-run advances the same state
    // machine live mode does, so after an hour of dry-run the planner believes
    // six apps are frozen. `reevaluate` suppresses an intent whose action
    // already matches that belief — so `dozectl mode live` switched the
    // actuator over and froze *nothing*. The apps stayed running and
    // believed-held until something moved their visibility, which for a hidden
    // Slack is hours, while the report went on accruing a saving that was no
    // longer being produced.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intent = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, intent.action, &Outcome::WouldHave);
    assert!(s.advance(MIN).is_empty(), "nothing more to do while the mode holds still");

    let reasserted = s.feed(Event::ModeChanged { mode: Mode::Live });
    assert_eq!(
        only(&reasserted).action,
        Action::Freeze { reclaim: false },
        "the freeze has to be issued again, against an actuator that will perform it"
    );
    assert_eq!(only(&reasserted).reason.kind, ReasonKind::ModeChanged);
}

#[test]
fn the_mode_change_re_assertion_covers_every_held_app_and_only_those() {
    // An app that is merely running must not be dragged into a freeze by a
    // mode change: re-assertion restates beliefs, it does not form new ones.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    let editor = s.spawn("editor");
    s.hide(&slack);
    for intent in s.advance(2 * MIN) {
        s.planner.note_outcome(&intent.app, intent.action, &Outcome::WouldHave);
    }

    let reasserted = s.feed(Event::ModeChanged { mode: Mode::Live });
    assert_eq!(reasserted.len(), 1, "{reasserted:?}");
    assert_eq!(reasserted[0].app, slack);
    assert_eq!(s.planner.applied(&editor), Action::Restore, "the visible app is untouched");
}

#[test]
fn a_refusal_does_not_survive_the_actuator_that_made_it() {
    // A throttle refused for want of a `cpu.max` file is evidence about one
    // actuator in one moment. Carrying it across a mode change would make the
    // stale-belief bug immortal in a second form: the app would never be
    // retried, having been refused by an actuator no longer in use.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intent = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, intent.action, &Outcome::Skipped { why: "no scope".into() });
    assert_eq!(s.planner.applied(&slack), Action::Restore, "the hold never took");
    assert!(s.advance(MIN).is_empty(), "and is not retried on its own");

    // Nothing to re-assert — the belief is "not held" — but the refusal is
    // cleared, so the next evaluation is free to try again.
    assert!(s.feed(Event::ModeChanged { mode: Mode::Live }).is_empty());
    assert_eq!(
        only(&s.advance(MIN)).action,
        Action::Freeze { reclaim: false },
        "a new actuator deserves a fresh attempt"
    );
}

#[test]
fn a_mode_change_while_blind_asserts_nothing() {
    // §10.2: with no sight of focus the planner holds nothing, so there is
    // nothing to re-assert and a mode change must not conjure one.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intent = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, intent.action, &Outcome::WouldHave);

    let restores = s.feed(Event::FocusSourceHealth { healthy: false });
    for intent in &restores {
        s.planner.note_outcome(&intent.app, intent.action, &Outcome::WouldHave);
    }
    assert!(
        s.feed(Event::ModeChanged { mode: Mode::Live }).is_empty(),
        "everything was restored when we went blind, so there is nothing held"
    );
}

/// A config whose one rule dozes hidden apps on a cycle.
fn doze_cycle(grace: &str, every: &str, wake: &str) -> Config {
    let mut cfg = Config::empty();
    cfg.rules = vec![
        Rule::new(
            "cycled",
            100,
            RuleAction::DozeCycle { every: every.parse().unwrap(), wake: wake.parse().unwrap() },
        )
        .when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some(grace.parse().unwrap()),
            ..Default::default()
        }),
    ];
    cfg.validate().unwrap();
    cfg
}

/// Feed the outcome back, as the daemon does, so `applied` advances.
fn confirm(s: &mut Scenario, intents: &[Intent]) {
    for i in intents {
        s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
    }
}

/// Tick a minute at a time for `minutes`, confirming outcomes as the daemon
/// does, and return everything that happened.
///
/// Minute by minute rather than one long jump because a maintenance window is
/// opened and closed *by* the ticks: a single `advance(30m)` would step over a
/// window entirely, which is a property of the test harness and not of the
/// planner.
fn run_for(s: &mut Scenario, minutes: u32) -> Vec<Intent> {
    let mut out = Vec::new();
    for _ in 0..minutes {
        let step = s.advance(MIN);
        confirm(s, &step);
        out.extend(step);
    }
    out
}

#[test]
fn a_cycled_app_comes_up_for_air_and_goes_back_down() {
    // The rung §7 was missing. A chat app can now be genuinely frozen and
    // still catch up, instead of being capped at 5% forever because a freeze
    // would cost it its connection.
    let mut s = Scenario::new(doze_cycle("1m", "15m", "30s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let frozen = run_for(&mut s, 2);
    assert_eq!(only(&frozen).action, Action::Freeze { reclaim: false });
    // The grace is a minute, so it went down on the first tick and the window
    // is due fifteen minutes after *that*, not after the test stopped ticking.
    assert_eq!(only(&frozen).at, MonoTime::from_millis(60_000));

    // Nothing for the next quarter of an hour.
    assert!(run_for(&mut s, 13).is_empty(), "no window before it is due");

    let woken = run_for(&mut s, 1);
    assert_eq!(only(&woken).action, Action::Restore);
    assert_eq!(only(&woken).reason.kind, ReasonKind::Maintenance);

    // It must not be re-frozen by the ordinary path while the window is open,
    // or the window would last exactly one tick.
    let during = s.advance(Duration::from_secs(10));
    assert!(during.is_empty(), "the window has the floor: {during:?}");

    let closed = s.advance(Duration::from_secs(30));
    confirm(&mut s, &closed);
    assert_eq!(only(&closed).action, Action::Freeze { reclaim: false });
    assert_eq!(only(&closed).reason.kind, ReasonKind::Maintenance);

    // And the cycle repeats rather than being a one-off.
    assert!(run_for(&mut s, 14).is_empty());
    let again = run_for(&mut s, 2);
    assert!(
        again.iter().any(|i| i.action.is_restore() && i.reason.kind == ReasonKind::Maintenance),
        "a second window must follow: {again:?}"
    );
}

#[test]
fn the_windows_of_different_apps_are_aligned() {
    // The entire justification for the rung. Six apps waking on six timers is
    // six bursts of wakeups; six apps waking together is one. So the schedule
    // is global, and an app on a longer period joins a tick of the shared
    // schedule rather than getting a tick of its own.
    let mut cfg = doze_cycle("1m", "15m", "30s");
    cfg.rules.push(
        Rule::new(
            "cycled-slowly",
            50,
            RuleAction::DozeCycle { every: "30m".parse().unwrap(), wake: "30s".parse().unwrap() },
        )
        .matching(Predicates {
            app: Some(crate::config::Patterns::One("hourly".into())),
            ..Default::default()
        })
        .when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some("1m".parse().unwrap()),
            ..Default::default()
        }),
    );
    // Lower priority fires first, and first-match-wins is a scan of the table
    // in order — so the table has to be sorted or `hourly` gets the 15m rule.
    cfg.validate().unwrap();

    let mut s = Scenario::new(cfg);
    let fast = s.spawn("slack");
    let slow = s.spawn("hourly");
    s.hide(&fast);
    s.hide(&slow);
    let frozen = run_for(&mut s, 2);
    assert_eq!(frozen.len(), 2, "both dozed: {frozen:?}");

    // Fifteen minutes in: the fast app's turn, and only its turn.
    let first = run_for(&mut s, 15);
    let restores: Vec<&Intent> = first.iter().filter(|i| i.action.is_restore()).collect();
    assert_eq!(restores.len(), 1, "only the 15m app is due: {first:?}");
    assert_eq!(restores[0].app, fast);

    // Thirty minutes in, the slow app is due — and it must wake on a tick of
    // the shared schedule, alongside the fast app's second window, rather
    // than thirty minutes after it happened to be frozen.
    let second = run_for(&mut s, 15);
    let restores: Vec<&Intent> = second.iter().filter(|i| i.action.is_restore()).collect();
    assert_eq!(restores.len(), 2, "both should wake in one window: {second:?}");
    let at = restores[0].at;
    assert!(
        restores.iter().all(|i| i.at == at),
        "the whole point is that they wake at the same instant: {restores:?}"
    );
}

#[test]
fn the_user_coming_back_mid_window_ends_the_cycle_rather_than_being_re_frozen() {
    // The window thaws an app whose rule still wants it frozen, so the
    // suppression that protects the window must not also swallow the user.
    let mut s = Scenario::new(doze_cycle("1m", "15m", "60s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    run_for(&mut s, 2);

    let woken = run_for(&mut s, 15);
    assert!(
        woken.iter().any(|i| i.reason.kind == ReasonKind::Maintenance),
        "a window should have opened: {woken:?}"
    );

    // The user focuses it inside the window, then leaves again.
    s.focus(&slack);
    let closing = s.advance(Duration::from_secs(70));
    confirm(&mut s, &closing);
    assert!(
        closing.iter().all(|i| i.action.is_restore()),
        "a visible app must not be re-frozen when the window closes: {closing:?}"
    );

    // And once hidden again it earns its grace from scratch.
    s.hide(&slack);
    assert!(s.advance(Duration::from_secs(30)).is_empty(), "grace restarts");
    let refrozen = run_for(&mut s, 2);
    let freeze = refrozen.iter().find(|i| !i.action.is_restore()).expect("re-dozed");
    assert_eq!(freeze.reason.kind, ReasonKind::RuleFired, "a rule, not a window");
}

#[test]
fn the_daemon_is_told_when_the_next_window_is_due() {
    // Otherwise a window due in 30s is opened whenever the 60s idle tick next
    // comes round — which is a late window, and one whose length is then wrong
    // by however late it was (`Planner::next_deadline`).
    // A wake long enough to still be open on a minute boundary, so the test
    // can observe an open window at all: a 30s window opens and closes inside
    // one of `run_for`'s steps.
    let mut s = Scenario::new(doze_cycle("1m", "15m", "2m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let frozen = run_for(&mut s, 2);
    // Measured from when the app actually went down — the first tick after a
    // one-minute grace — and not from wherever the test stopped ticking.
    let went_down = only(&frozen).at;
    assert_eq!(went_down, MonoTime::from_millis(60_000));

    let due = s.planner.next_deadline().expect("a window is scheduled");
    assert_eq!(due, went_down.plus(Duration::from_secs(15 * 60)));

    // Once one is open, the deadline is when it closes.
    let woken = run_for(&mut s, 15);
    let opened = woken.iter().find(|i| i.action.is_restore()).expect("a window opened").at;
    let closes = s.planner.next_deadline().expect("an open window closes");
    assert_eq!(closes, opened.plus(Duration::from_secs(120)));
}

#[test]
fn a_plain_freeze_never_gets_a_window() {
    // The cycle is per rule, so an app dozed by an ordinary rule stays dozed.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    run_for(&mut s, 2);
    assert_eq!(s.planner.next_deadline(), None, "nothing is scheduled");
    assert!(run_for(&mut s, 40).is_empty(), "and it is left alone indefinitely");
}

#[test]
fn a_window_never_reaches_an_app_the_planner_is_not_holding() {
    // A cycle rule that has not fired yet must be invisible to the schedule:
    // the grace is longer than the period, so the schedule would otherwise
    // tick repeatedly against an app nobody is holding.
    let mut s = Scenario::new(doze_cycle("30m", "1m", "30s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert!(run_for(&mut s, 10).is_empty(), "nothing is held, so nothing wakes");
}

#[test]
fn losing_sight_of_focus_cancels_the_cycle() {
    // §10.2: with no focus source the planner holds nothing, so there is
    // nothing to wake — and a window that opened anyway would thaw an app
    // that is already running and then freeze it while blind.
    let mut s = Scenario::new(doze_cycle("1m", "15m", "30s"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    run_for(&mut s, 2);

    let restores = s.feed(Event::FocusSourceHealth { healthy: false });
    confirm(&mut s, &restores);
    assert_eq!(only(&restores).action, Action::Restore);

    assert!(run_for(&mut s, 20).is_empty(), "blind means passive, schedule or no schedule");
}

#[test]
fn a_quiet_rule_acts_on_mains_where_a_battery_rule_does_not() {
    // The point of the objective, tested against the opt-out rather than
    // the default: somebody who has asked dozed to leave their plugged-in
    // machine alone still has a fan, and silencing the rule that exists to
    // address it would make the whole predicate decorative.
    let mut cfg = Config::empty();
    cfg.defaults.on_ac = crate::config::AcPolicy::Disabled;
    cfg.rules = vec![
        Rule::new("quiet-down", 50, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            thermal: Some(crate::event::ThermalState::Hot),
            for_: Some("1m".parse().unwrap()),
            ..Default::default()
        }),
        Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some("1m".parse().unwrap()),
            ..Default::default()
        }),
    ];
    cfg.validate().unwrap();

    let mut s = Scenario::new(cfg);
    s.set_power(ON_AC);
    let app = s.spawn("editor");
    s.hide(&app);

    // On AC and cool: the ordinary rule is vetoed by the AC policy, and the
    // quiet rule does not match because the machine is not hot.
    assert!(s.advance(10 * MIN).is_empty(), "nothing to do on a cool, plugged-in machine");

    // The fan spins up.
    // The fan spinning up is itself the trigger: the app has been hidden for
    // ten minutes already, so there is no further grace to wait out and the
    // event that changes the answer is the one that acts on it.
    let acted = s.feed(Event::ThermalChanged { thermal: crate::event::ThermalState::Hot });
    assert_eq!(only(&acted).action, Action::Freeze { reclaim: false });
    assert_eq!(only(&acted).reason.rule.as_deref(), Some("quiet-down"));
}

#[test]
fn the_ac_veto_still_applies_to_a_rule_that_is_about_the_battery() {
    // The opt-out is a consequence of what a rule *says*, so a rule that says
    // nothing about noise or frames keeps the ordinary policy. Otherwise this
    // would be a general escape hatch, which is not what was intended.
    let mut cfg = Config::empty();
    cfg.defaults.on_ac = crate::config::AcPolicy::Disabled;
    cfg.rules = vec![Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
        visibility: Some(Visibility::Hidden),
        for_: Some("1m".parse().unwrap()),
        ..Default::default()
    })];
    cfg.validate().unwrap();
    let mut s = Scenario::new(cfg);
    s.set_power(ON_AC);
    s.feed(Event::ThermalChanged { thermal: crate::event::ThermalState::Hot });
    let app = s.spawn("editor");
    s.hide(&app);
    assert!(s.advance(30 * MIN).is_empty(), "a battery rule stays vetoed on mains");
}

#[test]
fn a_fullscreen_app_makes_the_others_freezable_and_is_never_itself() {
    // The latency objective: while a game, a video or a call has the screen,
    // the cost of a background app is a dropped frame rather than battery.
    //
    // The second half is the safety property. The fullscreen app must never
    // match its own rule — it is already exempt by `fullscreen-exempt`, and
    // relying on one rule to save another from itself is not an argument.
    let mut cfg = Config::empty();
    cfg.rules = vec![Rule::new("clear-the-decks", 50, RuleAction::Freeze).when(Predicates {
        other_app_fullscreen: Some(true),
        for_: Some("30s".parse().unwrap()),
        ..Default::default()
    })];
    cfg.validate().unwrap();

    let mut s = Scenario::new(cfg);
    s.set_power(ON_AC);
    let game = s.spawn("game");
    let chat = s.spawn("slack");
    s.hide(&chat);
    assert!(s.advance(5 * MIN).is_empty(), "nothing is fullscreen yet");

    s.set_exemption(&game, Exemption::Fullscreen, true);
    let acted = s.advance(5 * MIN);
    assert_eq!(acted.len(), 1, "only the background app: {acted:?}");
    assert_eq!(acted[0].app, chat);
    assert_eq!(acted[0].action, Action::Freeze { reclaim: false });

    // And when the game exits fullscreen, everything comes back.
    for i in &acted {
        s.planner.note_outcome(&i.app, i.action, &Outcome::WouldHave);
    }
    s.set_exemption(&game, Exemption::Fullscreen, false);
    let back = s.advance(MIN);
    assert_eq!(only(&back).action, Action::Restore);
}

#[test]
fn the_fullscreen_app_itself_is_not_freezable_even_alone() {
    // With one app on the machine and it fullscreen, `other_app_fullscreen`
    // must be false for it — the predicate is about *others* and the word is
    // the whole content of it.
    let mut cfg = Config::empty();
    cfg.rules = vec![Rule::new("clear-the-decks", 50, RuleAction::Freeze).when(Predicates {
        other_app_fullscreen: Some(true),
        for_: Some("30s".parse().unwrap()),
        ..Default::default()
    })];
    cfg.validate().unwrap();
    let mut s = Scenario::new(cfg);
    s.set_power(ON_AC);
    let game = s.spawn("game");
    s.set_exemption(&game, Exemption::Fullscreen, true);
    s.hide(&game);
    assert!(s.advance(10 * MIN).is_empty(), "it is the fullscreen app, not another one");
}

#[test]
fn why_explains_a_quiet_rule_that_did_not_fire() {
    // §1: `dozectl why --rules` has to name the predicate that rejected a
    // rule, or the only way to find out is to read the source.
    let mut cfg = Config::empty();
    cfg.rules =
        vec![Rule::new("quiet-down", 50, RuleAction::Freeze).when(Predicates {
            thermal: Some(crate::event::ThermalState::Hot),
            ..Default::default()
        })];
    cfg.validate().unwrap();
    let mut s = Scenario::new(cfg);
    let app = s.spawn("editor");
    s.hide(&app);

    let e = s.planner.explain(&s.world, &app, s.now).expect("an explanation");
    let outcome = e.rules.iter().find(|r| r.name == "quiet-down").expect("the rule");
    let mismatch = outcome.mismatch.expect("it did not match");
    assert_eq!(mismatch.predicate(), "thermal");
    assert!(mismatch.to_string().contains("machine is normal"), "{mismatch}");
}

#[test]
fn a_stale_outcome_cannot_disturb_newer_state() {
    // Reports are keyed to the intent in flight. A late `Skipped` for an
    // action already superseded by a restore must not roll the restore back
    // into a freeze.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    let freeze = only(&s.advance(2 * MIN)).clone();
    s.planner.note_outcome(&slack, freeze.action, &Outcome::Applied);
    s.focus(&slack);
    assert_eq!(s.planner.applied(&slack), Action::Restore);

    s.planner.note_outcome(&slack, freeze.action, &Outcome::Skipped { why: "late".into() });
    assert_eq!(s.planner.applied(&slack), Action::Restore, "the stale report was ignored");
}

#[test]
fn a_frozen_app_is_not_frozen_again() {
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert_eq!(s.advance(2 * MIN).len(), 1);
    // Every subsequent tick must be silent, or the journal becomes noise and
    // the actuator does pointless work.
    assert!(s.advance(MIN).is_empty());
    assert!(s.advance(MIN).is_empty());
}

#[test]
fn focus_thaws_immediately() {
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let intents = s.focus(&slack);
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Restore);
    assert_eq!(intent.reason.kind, ReasonKind::BecameVisible);
}

#[test]
fn returning_to_an_app_buys_it_extra_grace() {
    // The resume cooldown: an app in the active rotation should not be
    // frozen on the same schedule as one the user has abandoned.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);
    s.focus(&slack);
    s.hide(&slack);

    assert!(s.advance(MIN).is_empty(), "cooldown should still be holding it back");
    assert_eq!(s.advance(MIN).len(), 1, "but only for the doubled grace");
}

/// Hide an app, let it freeze, return to it, hide it again — then lock the
/// screen. The state the bug needed. Returns whatever locking itself
/// produced, because with `away_factor = 0` that is the instant it fires.
fn returned_to_then_locked(cfg: Config) -> (Scenario, AppKey, Vec<Intent>) {
    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);
    s.focus(&slack);
    s.hide(&slack);
    let on_lock = s.set_desktop(DesktopState::Away);
    (s, slack, on_lock)
}

#[test]
fn the_cooldown_survives_the_screen_locking() {
    // The bug, found by replaying a real 5h25m session that was locked for
    // most of it: grace is a product, `away_factor = 0.0` zeroes it, and a
    // browser restored on unlock was re-frozen 81 seconds later inside a
    // nominal two-minute cooldown. As a floor the cooldown cannot be
    // cancelled out by another factor.
    let (mut s, _slack, on_lock) = returned_to_then_locked(freeze_after("1m"));
    assert_eq!(s.world.config.activity.away_factor, 0.0, "the annihilating factor");

    assert!(
        on_lock.is_empty(),
        "the user was in this app a moment ago; locking must not re-doze it: {on_lock:?}"
    );
    assert!(s.advance(MIN).is_empty(), "nor one minute into a two-minute cooldown");
    assert_eq!(
        s.advance(MIN).len(),
        1,
        "and once the cooldown is spent, away means act immediately"
    );
}

#[test]
fn the_factor_mode_keeps_the_original_behaviour_for_anyone_who_wants_it() {
    // Opting back in accepts the trade it always made: it acts sooner, and
    // while the screen is locked nobody is waiting on the app. This is also
    // the bug preserved as a choice — the freeze lands on the lock itself,
    // zero seconds after the user was using the app.
    let mut cfg = freeze_after("1m");
    cfg.defaults.resume_cooldown_mode = CooldownMode::Factor;
    let (_s, _slack, on_lock) = returned_to_then_locked(cfg);

    assert_eq!(
        on_lock.len(),
        1,
        "away_factor = 0 zeroes the product, so `factor` mode acts at once"
    );
    assert_eq!(on_lock[0].action, Action::Freeze { reclaim: false });
}

#[test]
fn the_floor_is_a_minimum_and_never_a_replacement_for_grace() {
    // A long grace with a short cooldown must still wait out the grace.
    // Otherwise "minimum wait" would have become "wait exactly this long",
    // and the floor would be licence to act sooner than the rules say.
    let mut cfg = freeze_after("30m");
    cfg.defaults.resume_cooldown = "1m".parse().unwrap();
    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(31 * MIN);
    s.focus(&slack);
    s.hide(&slack);

    assert!(s.advance(2 * MIN).is_empty(), "cooldown spent, but grace has 28m to run");
    assert!(s.advance(20 * MIN).is_empty(), "still inside grace");
    assert_eq!(s.advance(40 * MIN).len(), 1, "and it fires once grace is actually up");
}

#[test]
fn an_app_never_returned_to_gets_no_floor() {
    // The floor is bought by the user coming back. An app hidden since it
    // started has nothing to be patient about, and must still be actionable
    // the moment the screen locks — that is where most of the saving is.
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert_eq!(
        s.set_desktop(DesktopState::Away).len(),
        1,
        "away and never returned to: act on the lock itself"
    );
}

#[test]
fn holding_off_for_the_cooldown_is_logged_as_its_own_reason() {
    // "You just came back to this" and "it has not been quiet long enough"
    // are different answers to "why has nothing happened?", and a decision
    // log that conflates them cannot explain either.
    let (mut s, slack, _) = returned_to_then_locked(freeze_after("1m"));
    s.advance(MIN);

    let decisions = s.decisions();
    let (_, d) = decisions
        .iter()
        .rev()
        .find(|(k, _)| *k == slack)
        .expect("the app must have a recorded derivation");
    assert!(d.desired.is_none(), "it is waiting, not acting: {d:?}");
    assert!(d.reason.detail.contains("resume cooldown"), "{}", d.reason.detail);
    assert_eq!(
        d.wake_at,
        // Returned to at +2m, cooldown 2m, so look again at +4m.
        Some(MonoTime::from_millis(4 * 60_000)),
        "and it must say when to look again, or the tick sleeps through it"
    );
}

#[test]
fn visible_but_unfocused_is_left_alone() {
    // A frozen window on a second monitor is a stale rectangle, which reads
    // as a crash. DESIGN.md §10.6.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.set_visibility(&slack, Visibility::VisibleUnfocused);
    assert!(s.advance(10 * MIN).is_empty());
}

#[test]
fn freeze_visible_is_opt_in_per_app() {
    let mut cfg = freeze_after("1m");
    cfg.rules[0].when.visibility = None;
    cfg.apps.insert(
        "slack".into(),
        crate::config::AppOverride { freeze_visible: Some(true), ..Default::default() },
    );
    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.set_visibility(&slack, Visibility::VisibleUnfocused);
    assert_eq!(s.advance(2 * MIN).len(), 1);
}

#[test]
fn protected_processes_are_never_touched() {
    let mut s = Scenario::new(freeze_after("1m"));
    s.feed(Event::AppSeen(Box::new(protected_app("kwin_wayland"))));
    let kwin = AppKey::comm("kwin_wayland");
    s.hide(&kwin);
    assert!(s.advance(10 * MIN).is_empty());
}

#[test]
fn the_clipboard_owner_is_never_touched() {
    // Freezing the selection owner hangs the next paste. DESIGN.md §10.7.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.set_exemption(&slack, Exemption::ClipboardOwner, true);
    assert!(s.advance(10 * MIN).is_empty());
}

#[test]
fn audio_exemption_is_a_rule_and_thaws_a_frozen_app() {
    let mut cfg = freeze_after("1m");
    cfg.rules.push(
        Rule::new("audio-exempt", 10, RuleAction::Never)
            .when(Predicates { playing_audio: Some(true), ..Default::default() }),
    );
    cfg.validate().unwrap();
    let mut s = Scenario::new(cfg);
    let spotify = s.spawn("spotify");
    s.hide(&spotify);
    s.advance(2 * MIN);
    assert_eq!(s.planner.applied(&spotify), Action::Freeze { reclaim: false });

    let intents = s.set_exemption(&spotify, Exemption::Audio, true);
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Restore);
    assert_eq!(intent.reason.kind, ReasonKind::Exempt);
    assert_eq!(intent.reason.rule.as_deref(), Some("audio-exempt"));
}

#[test]
fn away_acts_at_once_and_active_stretches_grace() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.set_desktop(DesktopState::Active);
    s.hide(&slack);
    assert!(s.advance(10 * MIN).is_empty(), "4x factor means 20m of grace");

    // Locking the screen means nobody is switching windows, so waiting
    // costs nothing and saves nothing.
    let intents = s.set_desktop(DesktopState::Away);
    assert_eq!(only(&intents).action, Action::Freeze { reclaim: false });
}

#[test]
fn plugging_in_thaws_everything_only_where_ac_is_disabled() {
    // A disabled tier thaws what it holds, because there is nothing left to
    // save by holding it. Under the default that tier is no longer reached:
    // the charger is not a reason to stop, so plugging in leaves a held app
    // held. Both halves are asserted here — the second is the behaviour
    // change, and somebody who plugs in and watches nothing happen deserves
    // a test that says it was meant.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);
    assert!(s.set_power(ON_AC).is_empty(), "the default holds through a plug-in");

    let mut cfg = freeze_after("1m");
    cfg.defaults.on_ac = crate::config::AcPolicy::Disabled;
    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let intents = s.set_power(ON_AC);
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Restore);
    assert_eq!(intent.reason.kind, ReasonKind::Exempt);

    assert!(s.advance(10 * MIN).is_empty(), "and stays thawed while on AC");
}

#[test]
fn a_flatter_battery_acts_sooner_at_every_step() {
    // Not a threshold any more but a line, so this asserts the ordering
    // across the whole range rather than one magic number at one charge.
    // The old test pinned "half of 5m below 30%", which was true of a cliff
    // and is now true of exactly one point on it.
    let fires_after = |pct: f32| {
        let mut s = Scenario::new(freeze_after("5m"));
        let app = s.spawn("slack");
        s.hide(&app);
        s.set_power(crate::event::PowerState { on_ac: false, percentage: Some(pct) });
        // Step in tens of seconds until it acts; the first step that
        // produces an intent is the grace, near enough to compare.
        for step in 1..=120 {
            if !s.advance(Duration::from_secs(10)).is_empty() {
                return step * 10;
            }
        }
        u64::MAX
    };

    let full = fires_after(100.0);
    let half = fires_after(50.0);
    let low = fires_after(10.0);
    assert!(full > half, "50% must act sooner than 100% ({half}s vs {full}s)");
    assert!(half > low, "10% must act sooner than 50% ({low}s vs {half}s)");
    assert_eq!(full, 300, "a full battery is still the unscaled 5m");
}

#[test]
fn losing_the_focus_source_thaws_everything() {
    // DESIGN.md §10.2: if we cannot see focus we must not hold anything.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let intents = s.feed(Event::FocusSourceHealth { healthy: false });
    assert_eq!(only(&intents).reason.kind, ReasonKind::FocusSourceLost);
    assert!(s.advance(10 * MIN).is_empty(), "and we stay passive while blind");

    // Once the helper comes back, normal service resumes.
    s.feed(Event::FocusSourceHealth { healthy: true });
    assert_eq!(s.advance(2 * MIN).len(), 1);
}

#[test]
fn suspend_thaws_and_waking_restarts_every_timer() {
    // DESIGN.md §10.4: eight hours asleep must not read as eight hours idle.
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(6 * MIN);

    let intents = s.feed(Event::PrepareForSleep { sleeping: true });
    assert_eq!(only(&intents).reason.kind, ReasonKind::SystemSleep);

    s.now = s.now.plus(Duration::from_secs(8 * 3600));
    let intents = s.feed(Event::PrepareForSleep { sleeping: false });
    assert!(intents.is_empty(), "waking must not trigger a mass freeze");
    assert!(s.advance(4 * MIN).is_empty(), "the grace clock restarted at wake");
    assert_eq!(s.advance(2 * MIN).len(), 1);
}

#[test]
fn panic_thaw_ignores_everything() {
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let intents = s.feed(Event::ThawAll);
    assert_eq!(only(&intents).action, Action::Restore);
    assert!(s.planner.held().next().is_none());
}

/// A panic thaw is a return, and the backoff has to hear it.
///
/// Both tests above stop at the instant of the thaw, and that gap is where
/// this lived. From a live journal: three panic thaws in eighteen seconds,
/// each undone by a re-freeze two or three seconds later, every one of them
/// deriving `flap 1.0`. `restore_all` never called `note_return`, so
/// `short_holds` stayed at zero however many times the user asked — and both
/// safeguards key on that counter, so neither the doubling nor the
/// [`MIN_PAYBACK`] floor ever armed. The user's one recourse against a
/// thrashing app was the only signal that could not teach the backoff
/// anything, which is the worst possible place for a blind spot.
///
/// Note what is *not* asserted: that the thaw sticks forever. The app is still
/// hidden and the rules still own it. The claim is only that a hold taken
/// straight back is a hold that could not have paid for itself.
#[test]
fn a_panic_thaw_is_a_return_and_arms_the_backoff() {
    // The config it was found on: a 15s rule cut to a 1.5s wait.
    let mut cfg = redoze_after_a_glance(10.0, "15s");
    cfg.defaults.aggression = crate::config::Aggression::custom(0.05).unwrap();

    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);
    // One glance, so it is the glance rule counting this app down — the rule
    // every cycle in the journal names.
    s.focus(&slack);
    s.advance(Duration::from_secs(2));
    s.hide(&slack);

    // Hammering the hotkey. The app is never focused again, so the panic thaw
    // is the *only* evidence the planner gets that the user wants it back.
    for _ in 0..3 {
        for _ in 0..200 {
            if !s.advance(Duration::from_secs(5)).is_empty() {
                break;
            }
        }
        assert!(s.planner.held().next().is_some(), "a hold for the thaw to interrupt");
        s.feed(Event::ThawAll);
        assert!(s.planner.held().next().is_none(), "§10.5: unconditional, and still is");
    }

    let flap = s.planner.flap_of(&slack);
    assert!(flap > 1.0, "three panic thaws are three holds cut short, got flap {flap}");

    // And the floor, which is the part the user actually feels: whatever the
    // rules want, the app is not taken back before a hold could have paid for
    // itself.
    let mut waited = Duration::ZERO;
    let retaken = loop {
        let fired = !s.advance(Duration::from_secs(1)).is_empty();
        waited += Duration::from_secs(1);
        assert!(waited < 5 * MIN, "never re-taken at all, which is not the claim either");
        if fired {
            break waited;
        }
    };
    assert!(retaken >= MIN_PAYBACK, "re-taken {retaken:?} after a panic thaw");
}

/// The other three reasons that reach `restore_all` are not returns.
///
/// The asymmetry is the point, and [`AppMemo::note_return`] has the scar:
/// counting a suspend made an eight-hour sleep read as a hold that failed
/// after zero seconds. Only the user asking counts.
#[test]
fn a_suspend_is_not_the_user_coming_back() {
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    for _ in 0..3 {
        s.feed(Event::PrepareForSleep { sleeping: true });
        assert_eq!(s.planner.flap_of(&slack), 1.0, "a suspend says nothing about the hold");
        s.advance(2 * MIN);
    }
}

#[test]
fn shutdown_restores_everything_it_holds() {
    let mut s = Scenario::new(freeze_after("1m"));
    for name in ["slack", "discord"] {
        let key = s.spawn(name);
        s.hide(&key);
    }
    assert_eq!(s.advance(2 * MIN).len(), 2);
    assert_eq!(s.planner.held().count(), 2);

    let intents = s.feed(Event::Shutdown);
    assert_eq!(intents.len(), 2);
    assert!(intents.iter().all(|i| i.reason.kind == ReasonKind::Shutdown));
}

#[test]
fn activation_thaws_without_waiting_for_focus() {
    // Single-instance activation goes through D-Bus into the frozen process,
    // so the launcher hangs unless we thaw on the request itself.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let intents = s.feed(Event::ActivationRequested { app: slack.clone() });
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Restore);
    assert_eq!(intent.reason.kind, ReasonKind::Activated);

    // The app is still nominally hidden, so without a grace baseline the
    // very next tick would re-freeze it under the launcher.
    assert!(s.advance(Duration::from_secs(10)).is_empty());
}

#[test]
fn an_app_that_exits_leaves_nothing_behind() {
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let intents = s.feed(Event::AppGone { app: slack.clone() });
    assert!(intents.is_empty(), "there is no process left to restore");
    assert!(s.planner.held().next().is_none());
    assert_eq!(s.planner.applied(&slack), Action::Restore);
}

#[test]
fn a_user_request_overrides_the_rules() {
    let mut s = Scenario::new(Config::empty());
    let slack = s.spawn("slack");
    let intents =
        s.feed(Event::UserAction { app: slack.clone(), action: Action::Freeze { reclaim: true } });
    let intent = only(&intents);
    assert_eq!(intent.reason.kind, ReasonKind::UserRequested);
    assert_eq!(s.planner.applied(&slack), Action::Freeze { reclaim: true });
}

#[test]
fn wakeup_hogs_are_caught_but_busy_apps_are_not() {
    // The distinction the whole metric choice rests on: a compile is high
    // CPU and low wakeups, a misbehaving chat app is the reverse.
    let mut cfg = Config::empty();
    cfg.rules = vec![Rule::new("wakeup-hog", 60, RuleAction::Freeze).when(Predicates {
        visibility: Some(Visibility::Hidden),
        wakeups_per_sec: Some(crate::config::Threshold::Above(50.0)),
        for_: Some("60s".parse().unwrap()),
        ..Default::default()
    })];
    let mut s = Scenario::new(cfg);

    let builder = s.spawn("cargo");
    s.hide(&builder);
    s.sample(&builder, 400.0, 5.0);
    assert!(s.advance(10 * MIN).is_empty(), "a build is doing exactly what was asked");

    let slack = s.spawn("slack");
    s.hide(&slack);
    s.sample(&slack, 2.0, 900.0);
    assert_eq!(s.advance(2 * MIN).len(), 1);
}

#[test]
fn long_running_jobs_are_exempt_by_class() {
    let mut cfg = freeze_after("1m");
    cfg.rules.push(
        Rule::new("long-job-exempt", 20, RuleAction::Never)
            .matching(Predicates { class: Some(AppClass::LongRunningJob), ..Default::default() }),
    );
    cfg.validate().unwrap();
    let mut s = Scenario::new(cfg);

    let mut seen = crate::testing::app_seen("docker");
    seen.class = AppClass::LongRunningJob;
    s.feed(Event::AppSeen(Box::new(seen)));
    let docker = AppKey::comm("docker");
    s.hide(&docker);
    assert!(s.advance(10 * MIN).is_empty());
}

#[test]
fn first_matching_rule_wins_by_priority() {
    let mut cfg = Config::empty();
    cfg.rules = vec![
        Rule::new("catch-all-freeze", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some("1m".parse().unwrap()),
            ..Default::default()
        }),
        Rule::new("chatty", 50, RuleAction::Throttle(5))
            .matching(Predicates {
                app: Some(crate::config::Patterns::One("slack".into())),
                ..Default::default()
            })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                for_: Some("1m".parse().unwrap()),
                ..Default::default()
            }),
    ];
    cfg.validate().unwrap();
    let mut s = Scenario::new(cfg);

    let slack = s.spawn("slack");
    let other = s.spawn("gedit");
    s.hide(&slack);
    s.hide(&other);

    let intents = s.advance(2 * MIN);
    assert_eq!(intents.len(), 2);
    let by_app = |k: &AppKey| intents.iter().find(|i| &i.app == k).unwrap().clone();
    assert_eq!(by_app(&slack).action, Action::Throttle { pct: 5 });
    assert_eq!(by_app(&slack).reason.rule.as_deref(), Some("chatty"));
    assert_eq!(by_app(&other).action, Action::Freeze { reclaim: false });
}

#[test]
fn a_rule_that_stops_matching_restores_the_app() {
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);

    let mut cfg = Config::empty();
    cfg.validate().unwrap();
    let intents = s.feed(Event::ConfigChanged { config: Box::new(cfg) });
    let intent = only(&intents);
    assert_eq!(intent.action, Action::Restore);
    assert_eq!(intent.reason.kind, ReasonKind::NoRule);
}

/// The same deadline, per app and with the rule that owns it, so a client
/// can say *what* is about to happen rather than only that something is.
#[test]
fn a_waiting_app_says_what_is_coming_and_when() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    assert_eq!(s.planner.pending(&slack, s.now), None, "nothing matches a visible app");

    s.hide(&slack);
    s.advance(MIN);
    let pending = s.planner.pending(&slack, s.now).expect("counting down");
    assert_eq!(pending.rule, "test-freeze", "the rule doing the counting, by name");
    assert_eq!(
        pending.at,
        MonoTime::from_millis(5 * 60_000),
        "and the moment it will act, which is the one the daemon sleeps on"
    );
    assert_eq!(Some(pending.at), s.planner.next_deadline());

    // Once it has fired there is nothing to count down to: the app is in
    // the state its rule wanted, and a countdown still on screen would be
    // to an event that has already happened.
    s.advance(5 * MIN);
    assert_eq!(s.planner.pending(&slack, s.now), None);

    // And coming back cancels it rather than leaving a stale deadline —
    // the case a client would render as "dozes in 4m" under a window the
    // user is looking at.
    s.focus(&slack);
    assert_eq!(s.planner.pending(&slack, s.now), None);
}

#[test]
fn deadlines_let_the_daemon_sleep() {
    // DESIGN.md §13: a battery tool that polls hard is the problem it claims
    // to solve, so the planner must say when it next needs waking.
    let mut s = Scenario::new(freeze_after("5m"));
    assert_eq!(s.planner.next_deadline(), None, "nothing pending, sleep as long as you like");

    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(MIN);
    assert_eq!(s.planner.next_deadline(), Some(MonoTime::from_millis(5 * 60_000)));

    s.advance(5 * MIN);
    assert_eq!(s.planner.next_deadline(), None, "acted; nothing left to wait for");
}

#[test]
fn explain_reports_the_derivation() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(MIN);

    let e = s.planner.explain(&s.world, &slack, s.now).unwrap();
    assert_eq!(e.applied, Action::Restore);
    assert_eq!(e.decision.desired, None);
    assert_eq!(e.decision.rule_name(), Some("test-freeze"));
    assert_eq!(e.decision.grace.unwrap().effective(), Some(5 * MIN));
    assert_eq!(e.decision.elapsed, MIN);
    assert!(e.evidence.contains("comm=slack"), "{}", e.evidence);
    assert!(e.summary().contains("waiting"), "{}", e.summary());
}

#[test]
fn nothing_happens_before_the_focus_source_reports() {
    // A fresh daemon has not heard from the compositor yet, so it is blind.
    let mut s = Scenario {
        world: WorldState::new(freeze_after("1m")),
        planner: Planner::new(),
        now: MonoTime::ZERO,
    };
    s.set_power(ON_BATTERY);
    let slack = s.spawn("slack");
    s.hide(&slack);
    assert!(s.advance(10 * MIN).is_empty());
}

// ---- the decision log ------------------------------------------------
//
// The intent stream only records changes of state. These cover the gap it
// leaves: an app that is deliberately *not* acted on produces no intent, and
// "why did nothing happen?" is the question a user is most likely to ask.

#[test]
fn waiting_out_grace_is_recorded_even_though_nothing_happens() {
    let mut s = Scenario::new(freeze_after("5m"));
    let slack = s.spawn("slack");
    s.decisions();

    assert!(s.hide(&slack).is_empty(), "grace has not expired, so no intent");
    let decisions = s.decisions();
    let (key, d) = decisions.first().expect("but entering the wait must be on the record");
    assert_eq!(key, &slack);
    assert_eq!(d.desired, None, "nothing to do yet");
    assert_eq!(d.rule_name(), Some("test-freeze"), "and this is the rule we are waiting on");
    assert!(d.line().contains("waiting"), "{}", d.line());
    assert!(d.line().contains("hidden"), "{}", d.line());
    assert!(d.line().contains("on-battery"), "{}", d.line());
}

#[test]
fn a_ticking_clock_alone_is_not_a_new_decision() {
    // Edge-triggered, not level-triggered. Recording every tick would make
    // the log a firehose and dozed a wakeup source, which §13 forbids.
    let mut s = Scenario::new(freeze_after("30m"));
    let slack = s.spawn("slack");
    s.decisions();
    s.hide(&slack);
    assert_eq!(s.decisions().len(), 1, "going hidden is news");

    for _ in 0..5 {
        s.advance(MIN);
        // Samples jitter constantly; they must not each count as a decision.
        s.sample(&slack, 4.0, 300.0);
        s.sample(&slack, 9.0, 850.0);
    }
    assert!(s.decisions().is_empty(), "but the same wait, five minutes later, is not");
}

#[test]
fn a_changed_situation_is_a_new_decision() {
    let mut s = Scenario::new(freeze_after("30m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(MIN);
    s.decisions();

    s.set_exemption(&slack, Exemption::Audio, true);
    let decisions = s.decisions();
    assert_eq!(decisions.len(), 1, "starting to play audio changes the derivation");
    assert!(decisions[0].1.reason.inputs.as_ref().unwrap().exemptions.contains(&Exemption::Audio));
}

#[test]
fn every_decision_carries_the_state_it_was_made_from() {
    // A journal line that cannot be checked afterwards can only be believed.
    let mut s = Scenario::new(freeze_after("1m"));
    s.set_desktop(DesktopState::IdlePresent);
    let slack = s.spawn("slack");
    s.hide(&slack);
    let intents = s.advance(2 * MIN);

    let inputs = only(&intents).reason.inputs.as_ref().expect("inputs recorded");
    assert_eq!(inputs.visibility, Visibility::Hidden);
    assert_eq!(inputs.desktop, DesktopState::IdlePresent);
    assert!(!inputs.power.on_ac);
    assert!(inputs.exemptions.is_empty());
    // And the grace derivation, factorised rather than collapsed.
    let grace = only(&intents).reason.grace.expect("grace recorded");
    assert_eq!(grace.base, MIN);
    assert_eq!(grace.activity, 1.0);
}

#[test]
fn safety_decisions_carry_inputs_too() {
    // The early returns in `decide` are the ones a user is most likely to
    // find surprising, so they must not be the ones that explain least.
    let mut s = Scenario::new(freeze_after("1m"));
    s.feed(Event::AppSeen(Box::new(protected_app("kwin_wayland"))));
    let kwin = AppKey::comm("kwin_wayland");
    s.hide(&kwin);
    let decisions = s.decisions();
    let (_, d) = decisions.iter().find(|(k, _)| k == &kwin).expect("recorded");
    assert_eq!(d.reason.kind, ReasonKind::Safety);
    assert!(d.reason.inputs.as_ref().unwrap().protected);
}

#[test]
fn a_panic_thaw_invalidates_the_recorded_derivation() {
    // Otherwise the next tick compares against a derivation that no longer
    // describes why the app is in the state it is in, and stays silent.
    let mut s = Scenario::new(freeze_after("1m"));
    let slack = s.spawn("slack");
    s.hide(&slack);
    s.advance(2 * MIN);
    s.decisions();

    s.feed(Event::ThawAll);
    s.advance(Duration::from_secs(1));
    assert!(!s.decisions().is_empty(), "the post-thaw derivation must be re-recorded");
}

#[test]
fn explain_reports_every_rule_not_only_the_winner() {
    let mut cfg = Config::empty();
    cfg.rules = vec![
        Rule::new("audio-exempt", 10, RuleAction::Never)
            .when(Predicates { playing_audio: Some(true), ..Default::default() }),
        Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some("1m".parse().unwrap()),
            ..Default::default()
        }),
    ];
    let mut s = Scenario::new(cfg);
    let slack = s.spawn("slack");
    s.hide(&slack);

    let e = s.planner.explain(&s.world, &slack, s.now).unwrap();
    assert_eq!(e.rules.len(), 2);
    // The one that did not fire says which predicate stopped it.
    assert_eq!(e.rules[0].name, "audio-exempt");
    assert_eq!(e.rules[0].mismatch.unwrap().predicate(), "playing_audio");
    assert!(e.rules[1].winner);
}
