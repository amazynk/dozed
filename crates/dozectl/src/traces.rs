//! Reading recorded sessions, and replaying them against a different ruleset.
//!
//! Both work on files and need no running daemon, which matters: the session you
//! most want to inspect is usually the one that has already ended.
//!
//! The reading half is the debugging story. A journal entry says what `dozed`
//! decided; a trace says what it *saw* and what it concluded — including the
//! conclusions that produced no action, which are the majority and the ones the
//! journal structurally cannot show. The replaying half is the tuning story
//! (`DESIGN.md` §14): diff a candidate ruleset against a day that really
//! happened rather than against a guess.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use dozed_core::trace::{TraceRecord, is_trace_name, parse_lines};
use dozed_core::{Coarse, Config, paths};

/// Recorded sessions, newest first.
fn list(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_trace_name))
        .collect();
    // Names are the start time in unix seconds, so sorting is chronological.
    files.sort();
    files.reverse();
    files
}

/// Resolve an explicit path, or fall back to the most recent recording.
fn pick(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let dir = paths::trace_dir();
    list(&dir).into_iter().next().with_context(|| {
        format!(
            "no traces in {} — is dozed running, and was it started without --no-trace?",
            dir.display()
        )
    })
}

fn load(path: &Path) -> Result<Vec<TraceRecord>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let (records, skipped) = parse_lines(text.lines());
    if skipped > 0 {
        // Normal after a SIGKILL: the last line is half-written. Worth saying,
        // not worth refusing over.
        eprintln!("note: skipped {skipped} unreadable line(s) at the end of the trace");
    }
    if records.is_empty() {
        bail!("{} contains no records", path.display());
    }
    Ok(records)
}

/// Print a recorded session in the order it happened.
pub fn show(
    trace: Option<&Path>,
    app: Option<&str>,
    decisions_only: bool,
    list_only: bool,
    json: bool,
) -> Result<()> {
    if list_only {
        let dir = paths::trace_dir();
        let files = list(&dir);
        if files.is_empty() {
            println!("no traces in {}", dir.display());
            return Ok(());
        }
        for f in files {
            let size = std::fs::metadata(&f).map(|m| m.len()).unwrap_or_default();
            crate::emit(&format!("{:>9}  {}\n", human_bytes(size), f.display()));
        }
        return Ok(());
    }

    let path = pick(trace)?;
    let records = load(&path)?;
    if !json {
        eprintln!("# {}", path.display());
    }

    for record in &records {
        if decisions_only && matches!(record, TraceRecord::Event { .. }) {
            continue;
        }
        if let Some(pattern) = app
            && !mentions(record, pattern)
        {
            continue;
        }
        if json {
            crate::emit(&format!("{}\n", serde_json::to_string(record)?));
        } else {
            crate::emit(&format!("{}\n", record.line()));
        }
    }
    Ok(())
}

/// Whether a record concerns the app the user asked about.
///
/// Matched with the same pattern rules config files use, so `dozectl trace --app
/// slack` and a rule written `app = "slack"` select the same thing.
fn mentions(record: &TraceRecord, pattern: &str) -> bool {
    match record {
        // The header describes the whole session, so it is always relevant. So
        // does a gap: filtering it out of a per-app view would hide the reason
        // that app looks quiet for eleven minutes, which is the one question a
        // filtered trace is being read to answer.
        TraceRecord::Header(_) | TraceRecord::Gap { .. } => true,
        TraceRecord::Decision { app, .. } => app.matches_pattern(pattern),
        TraceRecord::Intent { intent, .. } => intent.app.matches_pattern(pattern),
        TraceRecord::Event { event, .. } => {
            // Events that concern no app in particular — power, activity, sleep,
            // ticks — are kept: they are often the reason the app's own
            // behaviour changed.
            event.app().is_none_or(|a| a.matches_pattern(pattern))
        }
    }
}

/// How much session a trace covers.
///
/// Taken from the records, not from the last intent: a session that decided to
/// do nothing has no intents at all — the normal dry-run outcome — and reporting
/// `0s` for a recorded half hour hides the only difference that matters at that
/// point, between a quiet session and a daemon that died at startup.
fn session_span(records: &[TraceRecord]) -> Duration {
    let ms = records.iter().filter_map(TraceRecord::at).map(|at| at.ms).max().unwrap_or(0);
    Duration::from_millis(ms)
}

/// Re-run a recorded session and diff the result against what was recorded.
pub fn replay(
    trace: Option<&Path>,
    config: Option<&Path>,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let path = pick(trace)?;
    let records = load(&path)?;

    let replacement = match config {
        None => None,
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("could not read {}", p.display()))?;
            let cfg = Config::parse(&text).with_context(|| format!("in {}", p.display()))?;
            // A candidate ruleset that cannot fire replays as "no change", which
            // reads exactly like "this config changes nothing" — the wrong
            // conclusion, and the one this path exists to prevent.
            for w in cfg.warnings() {
                eprintln!("warning: {w}");
            }
            Some(cfg)
        }
    };

    let result = dozed_core::replay(&records, replacement)?;

    if json {
        // The diff is for humans; the intent stream is what a tool would want.
        for intent in &result.produced {
            crate::emit(&format!("{}\n", serde_json::to_string(intent)?));
        }
        return Ok(());
    }

    eprintln!(
        "# {} — recorded {} by dozed {}",
        path.display(),
        Coarse(session_span(&records)),
        result.header.dozed_version,
    );
    crate::emit(&result.render());

    if verbose {
        crate::emit("\nresulting intent stream:\n");
        for intent in &result.produced {
            crate::emit(&format!(
                "  {} {} {}\n",
                intent.at,
                intent.app.label(),
                intent.reason.summary()
            ));
        }
    }

    // A replay against the recording's own config that does not reproduce it is
    // a bug in the seam, not a tuning result, so it must not exit zero.
    //
    // Unless the recording has a hole in it, and that distinction is why gaps
    // are marked in the stream rather than merely reported in `dozectl status`.
    // Events that were never written cannot be replayed, so a gapped trace
    // diverges for a reason that has nothing to do with determinism — and this
    // check used to hedge ("or the trace is incomplete") while still failing and
    // still leading with the bug claim. It can now tell the two apart, so it
    // says which, and only fails for the one that is actually a fault.
    if config.is_none() && !result.identical() {
        if result.complete() {
            bail!(
                "replaying a trace against its own config did not reproduce it — \
                 the planner is not deterministic"
            );
        }
        crate::emit(&format!(
            "\nnot reproduced, and not a fault: this recording has {} gap(s), so the \n\
             events that would have produced the missing intents were never written.\n",
            result.gaps
        ));
    }
    Ok(())
}

/// Search a recorded session for a better config than the one it ran under.
///
/// The other half of §14's tuning story. `replay` answers "what would this config
/// have done"; this answers "what config should I be running", by trying a few
/// dozen and scoring each against the day (`dozed_core::tune`).
pub fn suggest(
    trace: Option<&Path>,
    config: Option<&Path>,
    write: Option<&Path>,
    json: bool,
    anyway: bool,
) -> Result<()> {
    let path = pick(trace)?;
    let records = load(&path)?;

    // A base config to tune, defaulting to the one the session ran under —
    // "could my own config be better" rather than "is some config better than one
    // I do not run".
    let base = match config {
        None => None,
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("could not read {}", p.display()))?;
            Some(Config::parse(&text).with_context(|| format!("in {}", p.display()))?)
        }
    };

    let suggestions = dozed_core::tune::search(&records, base, anyway)?;

    if json {
        // One object per candidate, so a script can pick its own trade-off
        // instead of accepting this one's constraint.
        for c in &suggestions.candidates {
            crate::emit(&format!(
                "{}\n",
                serde_json::json!({
                    "label": c.label(),
                    "wakeups_avoided": c.score.wakeups_avoided,
                    "cpu_seconds_avoided": c.score.cpu_seconds_avoided,
                    "dozed_secs": c.score.dozed.as_secs(),
                    "holds": c.score.holds,
                    "thaws": c.score.thaws,
                    "interruptions": c.score.interruptions,
                    "apps": c.score.apps,
                    "recommended": suggestions.best().is_some_and(|b| b.tweaks == c.tweaks),
                    // So a script reading this cannot miss what a live recording
                    // does to the savings column, the way the formatted output
                    // cannot.
                    "holds_applied": suggestions.holds_applied,
                    "savings_measurable": !suggestions.holds_applied,
                })
            ));
        }
        return Ok(());
    }

    eprintln!("# {} — {}", path.display(), Coarse(session_span(&records)));
    crate::emit(&suggestions.render());

    if let Some(out) = write {
        // Ahead of the generic message, because "no candidate beat your config"
        // is not what happened and would send someone looking at their rules.
        if suggestions.holds_applied && !anyway {
            bail!(
                "refusing to write a config ranked against a session that really froze \
                 things: the savings it was ranked on are not measurable from one.\n\
                 Record a day with a daemon started `--dry-run` and re-run, or pass \
                 --anyway if you know what the number is worth."
            );
        }
        let Some(best) = suggestions.best() else {
            bail!("nothing to write: no candidate beat the config this session ran under");
        };
        let toml = toml::to_string_pretty(&best.config).context("serialising the tuned config")?;
        std::fs::write(out, &toml).with_context(|| format!("writing {}", out.display()))?;
        eprintln!("\nwrote {}", out.display());
        eprintln!(
            "check it with: dozed --check --config {}\nthen see the intent-level diff with: \
             dozectl replay {} --config {}",
            out.display(),
            path.display(),
            out.display()
        );
    } else if suggestions.best().is_some() {
        crate::emit(&format!(
            "\nWrite it out with: dozectl suggest {} --write tuned.toml\n",
            path.display()
        ));
    }
    Ok(())
}

pub fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KiB", n as f64 / (1u64 << 10) as f64),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use dozed_core::trace::{TRACE_VERSION, TraceHeader, trace_file_name};
    use dozed_core::{AppKey, Event, MonoTime};

    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dozectl-tr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_newest_trace_is_the_default() {
        let dir = tmpdir("pick");
        for wall in [1_700_000_000u64, 1_700_000_500, 1_700_000_100] {
            std::fs::write(dir.join(trace_file_name(wall)), "{}\n").unwrap();
        }
        let newest = list(&dir).into_iter().next().unwrap();
        assert!(newest.to_string_lossy().contains("1700000500"), "{newest:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_traces_in_the_directory_are_ignored() {
        let dir = tmpdir("filter");
        std::fs::write(dir.join("README"), "not a trace").unwrap();
        std::fs::write(dir.join(trace_file_name(1)), "{}\n").unwrap();
        assert_eq!(list(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_filtering_keeps_the_context_that_explains_the_app() {
        // Filtering to one app must not hide the power or activity change that
        // caused its behaviour to change, or the filtered view misleads.
        let slack = AppKey::comm("slack");
        let header = TraceRecord::Header(Box::new(TraceHeader {
            version: TRACE_VERSION,
            dozed_version: "test".into(),
            instance: "x".into(),
            wall_start: 0,
            live: false,
            config: Box::new(Config::empty()),
        }));
        let power = TraceRecord::Event {
            seq: 1,
            at: MonoTime::ZERO,
            event: Box::new(Event::PowerChanged { power: dozed_core::PowerState::default() }),
        };
        let other = TraceRecord::Event {
            seq: 2,
            at: MonoTime::ZERO,
            event: Box::new(Event::AppGone { app: AppKey::comm("firefox") }),
        };
        let mine = TraceRecord::Event {
            seq: 3,
            at: MonoTime::ZERO,
            event: Box::new(Event::AppGone { app: slack }),
        };

        assert!(mentions(&header, "slack"), "the header describes the whole session");
        assert!(mentions(&power, "slack"), "an unattributed event is context, not noise");
        assert!(!mentions(&other, "slack"), "another app's event is noise");
        assert!(mentions(&mine, "slack"));
    }

    #[test]
    fn the_reported_span_is_the_session_not_the_last_intent() {
        // Regression: the span used to come from the last recorded *intent*, so
        // every session that decided to do nothing — the normal dry-run result,
        // and the one you are most likely to be inspecting — reported `0s` for a
        // recording that covered half an hour. That is the number you read to
        // tell a quiet session apart from a daemon that died at startup.
        let header = TraceRecord::Header(Box::new(TraceHeader {
            version: TRACE_VERSION,
            dozed_version: "test".into(),
            instance: "x".into(),
            wall_start: 0,
            live: false,
            config: Box::new(Config::empty()),
        }));
        let tick_at = |ms: u64| TraceRecord::Event {
            seq: ms,
            at: MonoTime::from_millis(ms),
            event: Box::new(Event::Tick),
        };

        let quiet_session = vec![header.clone(), tick_at(5), tick_at(1_680_786)];
        assert_eq!(
            session_span(&quiet_session),
            Duration::from_millis(1_680_786),
            "a session with no intents still covers the time it recorded"
        );

        // A header-only file really is zero, and must not panic.
        assert_eq!(session_span(&[header]), Duration::ZERO);
        assert_eq!(session_span(&[]), Duration::ZERO);
    }

    /// A trace file on disk: one app hidden for a long stretch under a grace far
    /// longer than it needed to be.
    fn a_tunable_trace(dir: &std::path::Path) -> PathBuf {
        let mut cfg = Config::empty();
        cfg.defaults.base_grace = "10m".parse().unwrap();
        cfg.rules =
            vec![dozed_core::Rule::new("bg-idle", 100, dozed_core::RuleAction::Freeze).when(
                dozed_core::config::Predicates {
                    visibility: Some(dozed_core::Visibility::Hidden),
                    for_: Some("10m".parse().unwrap()),
                    ..Default::default()
                },
            )];
        let mut lines = vec![
            serde_json::to_string(&TraceRecord::Header(Box::new(TraceHeader {
                version: TRACE_VERSION,
                dozed_version: "test".into(),
                instance: "suggest".into(),
                wall_start: 1_700_000_000,
                live: false,
                config: Box::new(cfg),
            })))
            .unwrap(),
        ];
        let slack = AppKey::comm("slack");
        let mut seq = 0;
        let mut push = |lines: &mut Vec<String>, secs: u64, event: Event| {
            lines.push(
                serde_json::to_string(&TraceRecord::Event {
                    seq,
                    at: MonoTime::from_millis(secs * 1000),
                    event: Box::new(event),
                })
                .unwrap(),
            );
            seq += 1;
        };
        push(&mut lines, 0, Event::FocusSourceHealth { healthy: true });
        push(&mut lines, 0, Event::PowerChanged { power: dozed_core::testing::ON_BATTERY });
        push(&mut lines, 1, dozed_core::testing::seen("slack"));
        push(
            &mut lines,
            60,
            Event::VisibilityChanged {
                app: slack.clone(),
                visibility: dozed_core::Visibility::Hidden,
            },
        );
        for t in (60..3600).step_by(60) {
            push(
                &mut lines,
                t,
                Event::Sampled {
                    app: slack.clone(),
                    sample: dozed_core::ProcSample {
                        cpu_pct: 2.0,
                        wakeups_per_sec: 400.0,
                        gpu_pct: None,
                    },
                },
            );
            push(&mut lines, t, Event::Tick);
        }

        let path = dir.join(trace_file_name(1_700_000_000));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn suggest_reads_a_trace_and_writes_a_config_the_validator_accepts() {
        // The end of the loop is the point: the recommendation has to be a file
        // `dozed --check` will load and `dozectl replay` will re-run. A suggestion
        // that has to be hand-edited first is advice, not a tool.
        let dir = tmpdir("suggest");
        let trace = a_tunable_trace(&dir);
        let out = dir.join("tuned.toml");

        suggest(Some(&trace), None, Some(&out), false, false).expect("suggest should answer");

        let text = std::fs::read_to_string(&out).expect("a config should have been written");
        let tuned = Config::parse(&text).expect("and it should parse");
        tuned.clone().validate().expect("and validate");
        assert!(
            tuned.defaults.base_grace.get() < Duration::from_secs(600),
            "an hour of absence under a 10m grace should have been shortened: {}",
            tuned.defaults.base_grace
        );

        // And the written config must actually be the one that was scored, which
        // is checkable by replaying it: it has to freeze sooner than the recording.
        let scored = dozed_core::trace::replay_scored(&load(&trace).unwrap(), Some(tuned)).unwrap();
        assert!(scored.holds > 0, "the tuned config must actually fire");
        assert_eq!(scored.interruptions, 0, "and not by interrupting anyone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same trace, recorded live: the savings it would be ranked on are not
    /// measurable from it, so `--write` has to refuse rather than emit a config
    /// somebody would then run.
    #[test]
    fn suggest_will_not_write_a_config_ranked_against_a_session_that_froze_things() {
        let dir = tmpdir("suggest-live");
        let trace = a_tunable_trace(&dir);
        let mut text = std::fs::read_to_string(&trace).unwrap();
        // One hold the actuator really carried out is what makes the savings
        // unmeasurable — not the mode written in the header.
        text.push_str(
            &serde_json::to_string(&TraceRecord::Intent {
                seq: 9999,
                at: MonoTime::from_millis(700_000),
                intent: Box::new(dozed_core::Intent {
                    app: AppKey::comm("slack"),
                    action: dozed_core::Action::Freeze { reclaim: false },
                    reason: dozed_core::Reason::rule("bg-idle", "test"),
                    at: MonoTime::from_millis(700_000),
                }),
                outcome: dozed_core::Outcome::Applied,
            })
            .unwrap(),
        );
        text.push('\n');
        let live = trace.with_file_name("live.jsonl");
        std::fs::write(&live, text).unwrap();
        let out = dir.join("tuned.toml");

        let err = suggest(Some(&live), None, Some(&out), false, false).unwrap_err();
        assert!(err.to_string().contains("really froze things"), "{err}");
        assert!(!out.exists(), "and no file may be left behind");

        // --anyway is the escape hatch, and it has to actually work: refusing
        // twice would just be a command nobody can use on the trace they have.
        suggest(Some(&live), None, Some(&out), false, true).expect("--anyway should answer");
        assert!(out.exists(), "--anyway must write the file it promised");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggest_declines_to_write_a_config_it_did_not_recommend() {
        // A session with nothing hidden has nothing to tune, and `--write` must
        // fail rather than emit a copy of the input under a name that implies it
        // was improved.
        let dir = tmpdir("suggest-empty");
        let header = serde_json::to_string(&TraceRecord::Header(Box::new(TraceHeader {
            version: TRACE_VERSION,
            dozed_version: "test".into(),
            instance: "x".into(),
            wall_start: 1,
            live: false,
            config: Box::new(Config::default()),
        })))
        .unwrap();
        let path = dir.join(trace_file_name(1));
        std::fs::write(&path, header + "\n").unwrap();
        let out = dir.join("tuned.toml");

        let err = suggest(Some(&path), None, Some(&out), false, false).unwrap_err();
        assert!(err.to_string().contains("nothing to write"), "{err}");
        assert!(!out.exists(), "and no file may be left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes_render_readably() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KiB");
        // Divided by 1024 and labelled accordingly. It used to print `3.0M`
        // beside a daemon message saying `MB` for the same division, so one
        // status line offered two names for one unit and neither was MB.
        assert_eq!(human_bytes(3 << 20), "3.0 MiB");
    }
}
