//! A recorded session, committed, with the decisions it must produce.
//!
//! §14 says replaying a trace against its own config must reproduce its intent
//! stream exactly, and `dozectl replay` exits non-zero when it does not. That
//! guards determinism. It does not guard *behaviour*: a change to the grace model
//! or the ladder alters what the planner concludes, both the recording and the
//! replay move together, and nothing anywhere says so.
//!
//! So the expectation lives in a file instead. `fixtures/session.jsonl` is a
//! trace and `fixtures/session.intents` is what it decides; a change to either
//! shows up as a diff in the review rather than as a number in a summary. It is
//! the same idea as the shared visibility fixtures, applied to the planner as a
//! whole.
//!
//! Both files are generated rather than typed:
//!
//! ```sh
//! cargo test -p dozed-core --test golden_replay -- --ignored regenerate
//! ```
//!
//! Regenerating is deliberately a separate, ignored test. A harness that rewrote
//! its own expectation whenever it failed would assert nothing at all, so
//! updating the file has to be something a person chose to do — and the diff is
//! then the description of the behaviour change.

use std::path::{Path, PathBuf};

use dozed_core::config::{Predicates, Rule, RuleAction};
use dozed_core::testing;
use dozed_core::trace::{TRACE_VERSION, TraceHeader, TraceRecord, parse_lines, replay_scored};
use dozed_core::{AppKey, Config, Event, HumanDuration, MonoTime, ProcSample, Visibility};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The config the recorded session ran under: the shipped shape in miniature —
/// an exemption, a throttle for chat, a cycle, and a catch-all freeze.
fn config() -> Config {
    let mut cfg = Config::empty();
    cfg.rules = vec![
        Rule::new("audio-exempt", 10, RuleAction::Never)
            .when(Predicates { playing_audio: Some(true), ..Default::default() }),
        Rule::new("chatty-apps", 50, RuleAction::Throttle(5))
            .matching(Predicates {
                app: Some(dozed_core::config::Patterns::One("slack".into())),
                ..Default::default()
            })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                for_: Some(HumanDuration::secs(120)),
                ..Default::default()
            }),
        Rule::new(
            "cycled-mail",
            60,
            RuleAction::DozeCycle {
                every: HumanDuration::secs(900),
                wake: HumanDuration::secs(30),
            },
        )
        .matching(Predicates {
            app: Some(dozed_core::config::Patterns::One("thunderbird".into())),
            ..Default::default()
        })
        .when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some(HumanDuration::secs(60)),
            ..Default::default()
        }),
        Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some(HumanDuration::secs(300)),
            ..Default::default()
        }),
    ];
    cfg.validate().expect("the golden config must be loadable");
    cfg
}

/// The session: three apps, a music player that starts and stops playing, a
/// suspend, a lock, and enough ticks for every rule to get a turn.
///
/// Built here rather than captured from a real desktop on purpose. A real
/// recording is thousands of events of which four matter, and a diff over it is
/// unreadable; this is the smallest session that exercises each rung.
fn records() -> Vec<TraceRecord> {
    let mut out = vec![TraceRecord::Header(Box::new(TraceHeader {
        version: TRACE_VERSION,
        dozed_version: "golden".into(),
        instance: "golden".into(),
        wall_start: 1_700_000_000,
        live: false,
        config: Box::new(config()),
    }))];
    let mut seq = 0;
    let mut push = |out: &mut Vec<TraceRecord>, secs: u64, event: Event| {
        out.push(TraceRecord::Event {
            seq,
            at: MonoTime::from_millis(secs * 1000),
            event: Box::new(event),
        });
        seq += 1;
    };

    push(&mut out, 0, Event::FocusSourceHealth { healthy: true });
    push(&mut out, 0, Event::PowerChanged { power: testing::ON_BATTERY });
    for app in ["slack", "thunderbird", "editor", "player"] {
        push(&mut out, 1, testing::seen(app));
    }

    // Everything goes away, and the player starts playing.
    for app in ["slack", "thunderbird", "editor", "player"] {
        push(
            &mut out,
            10,
            Event::VisibilityChanged { app: AppKey::comm(app), visibility: Visibility::Hidden },
        );
    }
    push(
        &mut out,
        10,
        Event::ExemptionChanged {
            app: AppKey::comm("player"),
            exemption: dozed_core::Exemption::Audio,
            active: true,
        },
    );

    // Forty minutes of an idle desktop, sampled every minute.
    let mut at = 60u64;
    while at < 40 * 60 {
        for app in ["slack", "thunderbird", "editor", "player"] {
            push(
                &mut out,
                at,
                Event::Sampled {
                    app: AppKey::comm(app),
                    sample: ProcSample { cpu_pct: 3.0, wakeups_per_sec: 250.0, gpu_pct: None },
                },
            );
        }
        push(&mut out, at, Event::Tick);
        at += 60;
    }

    // The user comes back to Slack, then leaves again.
    push(
        &mut out,
        at,
        Event::VisibilityChanged { app: AppKey::comm("slack"), visibility: Visibility::Focused },
    );
    at += 60;
    push(&mut out, at, Event::Tick);
    push(
        &mut out,
        at,
        Event::VisibilityChanged { app: AppKey::comm("slack"), visibility: Visibility::Hidden },
    );

    // The music stops, so its exemption lifts.
    at += 120;
    push(
        &mut out,
        at,
        Event::ExemptionChanged {
            app: AppKey::comm("player"),
            exemption: dozed_core::Exemption::Audio,
            active: false,
        },
    );

    // Ten more minutes, a suspend and a resume, then ten more.
    for _ in 0..10 {
        at += 60;
        push(&mut out, at, Event::Tick);
    }
    at += 60;
    push(&mut out, at, Event::PrepareForSleep { sleeping: true });
    at += 60;
    push(&mut out, at, Event::PrepareForSleep { sleeping: false });
    for _ in 0..12 {
        at += 60;
        push(&mut out, at, Event::Tick);
    }
    // And the screen locks, which is where most of the saving comes from.
    push(&mut out, at, Event::ActivityChanged { desktop: dozed_core::DesktopState::Away });
    for _ in 0..3 {
        at += 60;
        push(&mut out, at, Event::Tick);
    }
    out
}

/// One line per intent: when, which app, what, and which rule decided.
///
/// Deliberately not `Debug`: the point is a diff a person can read, so it carries
/// what a behaviour change would move and nothing that merely moves with it.
fn render(scored: &dozed_core::trace::ScoredReplay) -> String {
    let mut out = String::new();
    for i in &scored.result.produced {
        out.push_str(&format!(
            "{} {:<14} {:<9} {}\n",
            i.at,
            i.app.label(),
            i.action.verb(),
            i.reason.summary()
        ));
    }
    out.push_str(&format!(
        "\n{} holds, {} thaws, {} interruptions\n",
        scored.holds, scored.thaws, scored.interruptions
    ));
    out
}

#[test]
fn the_recorded_session_still_decides_what_it_decided() {
    let trace = fixtures().join("session.jsonl");
    let expected_path = fixtures().join("session.intents");
    let text = std::fs::read_to_string(&trace).unwrap_or_else(|e| {
        panic!("{}: {e} — regenerate with `-- --ignored regenerate`", trace.display())
    });
    let (records, skipped) = parse_lines(text.lines());
    assert_eq!(skipped, 0, "the committed trace must be clean");

    let scored = replay_scored(&records, None).expect("the golden trace must replay");
    let got = render(&scored);
    let expected = std::fs::read_to_string(&expected_path).unwrap_or_default();

    assert_eq!(
        got, expected,
        "the planner's conclusions about the golden session changed.\n\n\
         If that was the point of your change, regenerate the fixture and let the diff \
         describe it:\n    cargo test -p dozed-core --test golden_replay -- --ignored regenerate\n\n\
         got:\n{got}"
    );
}

#[test]
fn the_golden_trace_is_a_session_worth_guarding() {
    // A fixture that exercises one rung would pass while three others rotted, so
    // the fixture itself has expectations.
    let text = std::fs::read_to_string(fixtures().join("session.jsonl")).expect("the fixture");
    let (records, _) = parse_lines(text.lines());
    let scored = replay_scored(&records, None).unwrap();

    let rules: std::collections::BTreeSet<&str> =
        scored.result.produced.iter().filter_map(|i| i.reason.rule.as_deref()).collect();
    assert!(rules.contains("bg-idle"), "the catch-all must fire: {rules:?}");
    assert!(rules.contains("chatty-apps"), "the throttle rung must fire: {rules:?}");
    assert!(rules.contains("cycled-mail"), "the cycle rung must fire: {rules:?}");

    let kinds: std::collections::BTreeSet<&str> =
        scored.result.produced.iter().map(|i| i.reason.kind.as_str()).collect();
    assert!(kinds.contains("maintenance"), "a maintenance window must open: {kinds:?}");
    assert!(kinds.contains("became-visible"), "the user must come back to something: {kinds:?}");
    assert!(kinds.contains("system-sleep"), "the suspend must restore: {kinds:?}");

    // And the player must never be frozen while it is playing, which is the one
    // line of this session that is a safety property rather than a behaviour.
    let player = AppKey::comm("player");
    let first_freeze = scored
        .result
        .produced
        .iter()
        .find(|i| i.app == player && !i.action.is_restore())
        .map(|i| i.at);
    if let Some(at) = first_freeze {
        assert!(
            at.ms >= 40 * 60 * 1000,
            "the player was frozen at {at} while its audio exemption was active"
        );
    }
}

#[test]
#[ignore = "rewrites the committed fixture; run deliberately"]
fn regenerate() {
    let dir = fixtures();
    std::fs::create_dir_all(&dir).unwrap();
    let records = records();
    let mut lines = String::new();
    for r in &records {
        lines.push_str(&serde_json::to_string(r).unwrap());
        lines.push('\n');
    }
    std::fs::write(dir.join("session.jsonl"), &lines).unwrap();

    let (parsed, skipped) = parse_lines(lines.lines());
    assert_eq!(skipped, 0);
    let scored = replay_scored(&parsed, None).unwrap();
    std::fs::write(dir.join("session.intents"), render(&scored)).unwrap();
    println!("wrote {} ({} records)", dir.display(), records.len());
}

#[test]
fn replaying_the_golden_trace_is_deterministic() {
    // §14's acceptance criterion, on a committed input rather than a generated
    // one: same trace, same config, byte-identical intents, twice.
    let text = std::fs::read_to_string(fixtures().join("session.jsonl")).expect("the fixture");
    let (records, _) = parse_lines(text.lines());
    let a = replay_scored(&records, None).unwrap();
    let b = replay_scored(&records, None).unwrap();
    assert_eq!(a.result.produced, b.result.produced);
    assert_eq!(render(&a), render(&b));
}
