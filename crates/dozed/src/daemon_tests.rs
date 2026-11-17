//! Tests for [`super`], in a file of their own, so that the file a reader opens to
//! understand what the code does is not mostly assertions about it. Still a child
//! module rather than an integration test: these reach private items, and the
//! `#[path]` attribute on the declaration keeps `super::*` meaning what it would at
//! the bottom of that file.

use super::*;

#[test]
fn most_visible_window_wins() {
    // Ord is Focused < VisibleUnfocused < Hidden, which is what lets the
    // aggregation in on_windows take a minimum.
    assert!(Visibility::Focused < Visibility::VisibleUnfocused);
    assert!(Visibility::VisibleUnfocused < Visibility::Hidden);
}

/// A daemon writing to a throwaway trace, so events can be counted.
/// A daemon whose state files land in this test binary's own directory.
///
/// The redirect is the load-bearing part. `drive_trial` and `stop_trial` persist
/// through `crate::paths`, which resolved to `$XDG_STATE_HOME` with nothing a
/// test could do about it — so running `cargo test` on a machine with a live
/// daemon **overwrote that daemon's `trial.json` and `frozen.json`**, observed
/// resetting a four-block trial to zero on disk. `frozen.json` is the one that
/// matters: §10.3 makes it the thing standing between a crash and an
/// application frozen for good.
///
/// Set here rather than per test, because the first caller wins and every
/// daemon test wants the same answer.
fn daemon_with_trace(dir: &std::path::Path) -> Daemon {
    crate::paths::redirect_state_for_tests(
        &std::env::temp_dir().join(format!("dozed-test-state-{}", std::process::id())),
    );
    let header = dozed_core::TraceHeader {
        version: dozed_core::TRACE_VERSION,
        dozed_version: "test".into(),
        instance: "test".into(),
        wall_start: 0,
        live: false,
        config: Box::new(Config::default()),
    };
    Daemon::new(
        Config::default(),
        false,
        JournalFile::open(dir.join("journal.jsonl")),
        TraceFile::create(dir, &header),
        None,
    )
}

fn window_of(pid: u32, minimized: bool) -> dozed_core::WindowFacts {
    dozed_core::WindowFacts {
        pid,
        app_id: "test-app".into(),
        desktop_file: "test-app".into(),
        minimized,
        ..Default::default()
    }
}

/// Open (or close) a screen share, through the same path
/// `Incoming::Exemptions` takes — the derivation only reaches the world
/// through dispatched events, and a test that skipped that would be
/// asserting about a set nothing reads.
fn share(d: &mut Daemon, sharing: &[u32]) {
    let pids: BTreeSet<u32> = sharing.iter().copied().collect();
    for event in d.resolve_shared_windows(&pids) {
        d.dispatch(event);
    }
}

/// The tick is armed from state, and state changes between ticks. A daemon
/// that armed 60s from an empty world then slept through the arrival of
/// every app on the desktop — which is why nothing was measured for the
/// first minute of a run however fast the warm-up asked to sample.
#[test]
fn an_event_may_bring_the_next_tick_forward_but_never_push_it_out() {
    let t0 = tokio::time::Instant::now();
    let mut tick = Tick::new(t0 + Duration::from_secs(60));

    tick.pull_in(t0, Duration::from_secs(3));
    assert_eq!(tick.deadline(), t0 + Duration::from_secs(3), "sooner wins");

    // The helper heartbeats every 5s. If each one re-armed the interval the
    // tick would never come due at all, which is the starvation `Tick`
    // exists to prevent.
    tick.pull_in(t0, Duration::from_secs(60));
    assert_eq!(tick.deadline(), t0 + Duration::from_secs(3), "later is ignored");

    // And time passing does not push it out either.
    tick.pull_in(t0 + Duration::from_secs(2), Duration::from_secs(60));
    assert_eq!(tick.deadline(), t0 + Duration::from_secs(3));
}

/// A rate needs two readings, and at the idle interval collecting them took
/// two minutes. For that whole time every client drew a table of dashes and
/// — worse — every rule with a `wakeups_per_sec` threshold was deciding
/// against a measurement nobody had taken.
#[test]
fn a_fresh_daemon_takes_its_first_readings_before_it_backs_off() {
    let dir = std::env::temp_dir().join(format!("dozed-warmup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });

    // Nothing to measure yet: an empty desktop is not a reason to poll.
    assert!(d.next_wakeup() >= Duration::from_secs(30), "{:?}", d.next_wakeup());

    d.on_windows(&[window_of(std::process::id(), true)]);
    assert!(
        d.next_wakeup() <= Duration::from_secs(3),
        "an app nobody has measured is worth a prompt look: {:?}",
        d.next_wakeup()
    );

    let t = std::time::Instant::now();
    d.sample_all(t);
    assert!(
        d.next_wakeup() <= Duration::from_secs(3),
        "one reading is a look, not a rate: {:?}",
        d.next_wakeup()
    );

    // Measured. The warm-up is over and §13's back-off takes over again,
    // for the rest of the run.
    d.sample_all(t + Duration::from_secs(3));
    assert!(
        d.next_wakeup() >= Duration::from_secs(10),
        "a measured daemon must go back to sleep: {:?}",
        d.next_wakeup()
    );
}

/// §13's back-off is an argument about a daemon nobody is looking at. With a
/// dashboard open it produced a table whose `wakes/s` column was redrawn
/// thirty times a minute and changed once — accurate as of the last tick,
/// which is not the same as fresh.
#[test]
fn a_watched_app_table_is_resampled_without_speeding_up_the_tick() {
    let dir = std::env::temp_dir().join(format!("dozed-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });
    d.on_windows(&[window_of(std::process::id(), true)]);

    // Two readings, so the warm-up is over and the back-off is in charge.
    let t = std::time::Instant::now();
    d.sample_all(t);
    d.sample_all(t + Duration::from_secs(3));
    assert!(d.next_wakeup() >= Duration::from_secs(10), "{:?}", d.next_wakeup());
    assert!(!d.watched(), "nobody has asked");

    let unwatched_tick = d.next_wakeup();
    d.note_watcher();
    assert!(d.watched());

    // The tick itself must not move. The energy meter differences counters
    // since its previous reading and the trial takes a mean over the samples
    // in a block, so a window being opened must not change what the
    // experiment measures.
    assert_eq!(d.next_wakeup(), unwatched_tick, "the tick is not what speeds up");
    // The samples are, and by one interval from the last one taken rather
    // than from the last tick.
    assert_eq!(d.resample_due(), tokio::time::Instant::from_std(d.last_sample_at + WATCHED_SAMPLE));

    // And the interest lapses on its own: a window that is killed outright
    // never gets to say goodbye, so nothing may depend on it doing so.
    d.watching_until = Some(std::time::Instant::now() - Duration::from_millis(1));
    assert!(!d.watched());
    assert_eq!(d.next_wakeup(), unwatched_tick, "back to §13 with nobody watching");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The first poll from a window that has just opened is the one most likely to
/// be read, and at the idle interval it was answered with a figure up to a
/// minute old.
#[test]
fn a_dashboard_that_has_just_opened_is_resampled_at_once() {
    let dir = std::env::temp_dir().join(format!("dozed-watch-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });
    d.on_windows(&[window_of(std::process::id(), true)]);

    // Most of a minute-long tick has gone by since the last sample.
    d.sample_all(std::time::Instant::now() - Duration::from_secs(45));
    d.note_watcher();
    assert!(
        d.resample_due() <= tokio::time::Instant::from_std(std::time::Instant::now()),
        "a resample already overdue must happen on the next pass, not one interval later"
    );

    // Taking it puts the deadline back out to a full interval, so a watched
    // daemon samples every `WATCHED_SAMPLE` and not continuously.
    d.sample_all(std::time::Instant::now());
    assert!(d.resample_due() > tokio::time::Instant::from_std(std::time::Instant::now()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_repeated_snapshot_says_nothing() {
    // The compositor sends complete snapshots, so most of each one repeats
    // what we already know: 85% of the events in a five-hour trace changed
    // nothing. That is most of a log a person has to read to debug this.
    let dir = std::env::temp_dir().join(format!("dozed-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });

    let snapshot = vec![window_of(std::process::id(), true)];
    d.on_windows(&snapshot);
    let after_first = d.trace.records();
    assert!(after_first > 0, "the first snapshot must be recorded");

    for _ in 0..5 {
        d.on_windows(&snapshot);
    }
    assert_eq!(
        d.trace.records(),
        after_first,
        "five identical snapshots must add nothing to the log"
    );

    // But a real change still gets through.
    d.on_windows(&[window_of(std::process::id(), false)]);
    assert!(d.trace.records() > after_first, "an actual change must still be recorded");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(start_paused = true)]
async fn other_activity_cannot_postpone_the_tick() {
    // The bug this guards: `select!` drops losing futures, so a fresh
    // `sleep(60s)` per iteration was restarted every time a 5s heartbeat
    // arrived and `on_tick` never ran at all — no sampling, no self-cost, a
    // counterfactual report stuck at zero for five hours.
    //
    // An absolute deadline is immune, and that is the property under test:
    // re-reading it as often as you like must not move it.
    let start = tokio::time::Instant::now();
    let mut tick = Tick::new(start);
    tick.arm(start, Duration::from_secs(60));
    let due = tick.deadline();

    // Stand in for the heartbeats and watchdog firing every 5s for a minute.
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(tick.deadline(), due, "an unrelated wakeup moved the tick");
    }
    assert!(
        tokio::time::Instant::now() >= due,
        "60s of 5s interruptions must still leave the tick due"
    );

    // And arming from the new `now` is what advances it.
    tick.arm(tokio::time::Instant::now(), Duration::from_secs(60));
    assert!(tick.deadline() > due);
}

#[test]
fn a_mode_change_goes_through_the_event_stream() {
    // Not a field the daemon pokes. It has to be an event so the planner can
    // re-assert what it believes is held — see `Planner::reassert`, whose
    // absence meant `dozectl mode live` froze nothing at all — and so a trace
    // spanning the switch replays as the session it actually was.
    let dir = std::env::temp_dir().join(format!("dozed-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    let before = d.trace.records();

    d.set_mode(Mode::Live);
    assert!(d.trace.records() > before, "the switch must be on the record");
    assert_eq!(d.world().config.mode, Mode::Live);

    let after = d.trace.records();
    d.set_mode(Mode::Live);
    assert_eq!(d.trace.records(), after, "setting the mode it is already in says nothing");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_snooze_returns_to_the_mode_it_interrupted() {
    // The whole difference between this and `dozectl mode dry-run`: the way
    // back is automatic. A snooze that had to be undone by hand would be the
    // same trap as switching the daemon off, which is the thing people
    // actually do and then forget.
    let dir = std::env::temp_dir().join(format!("dozed-snooze-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.set_mode(Mode::Live);

    d.snooze(Duration::from_secs(3600)).expect("no trial is running");
    assert_eq!(d.world().config.mode, Mode::DryRun, "a snooze stops acting");
    assert!(d.snooze_remaining().is_some());

    assert!(d.wake());
    assert_eq!(d.world().config.mode, Mode::Live, "and puts back what it interrupted");
    assert!(d.snooze_remaining().is_none());
    assert!(!d.wake(), "waking twice is not an error, it is a no-op");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snoozing_twice_does_not_leave_the_daemon_switched_off() {
    // The bug the `restore` field is captured once to avoid: a second snooze
    // taken during the first would record the *snoozed* mode as the thing to
    // go back to, so the daemon would come out of an hour's snooze in
    // dry-run and stay there. Extending is the only sensible reading of
    // "snooze again", and it must not touch what we return to.
    let dir = std::env::temp_dir().join(format!("dozed-snooze2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.set_mode(Mode::Live);

    d.snooze(Duration::from_secs(60)).unwrap();
    d.snooze(Duration::from_secs(3600)).unwrap();
    d.wake();
    assert_eq!(d.world().config.mode, Mode::Live);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_explicit_mode_change_beats_a_snooze() {
    // `cancel_snooze` rather than `wake`: ending the snooze properly would
    // restore the old mode straight over the top of the one the user just
    // chose. Undoing a deliberate choice minutes later is the hardest kind
    // of bug to attribute to the right cause.
    let dir = std::env::temp_dir().join(format!("dozed-snooze3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.set_mode(Mode::Live);
    d.snooze(Duration::from_secs(3600)).unwrap();

    assert!(d.cancel_snooze(), "there was one to cancel");
    assert!(d.snooze_remaining().is_none());
    assert_eq!(d.world().config.mode, Mode::DryRun, "cancelling does not restore anything");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stalled_trial_leaves_the_users_mode_alone() {
    // End to end, through the daemon rather than the trial type: a trial
    // that cannot start a block held a real machine in dry-run across two
    // restarts and 28 hours, and refused `dozectl mode live` the whole time
    // because "a trial is driving the mode". The user's mode has to survive
    // a trial that is measuring nothing.
    let dir = std::env::temp_dir().join(format!("dozed-stall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.set_mode(Mode::Live);
    // A baseline of dry-run, as a real trial.json carried: this is the value
    // that used to be reasserted forever.
    d.trial = Some(Trial::new(TrialPlan::default(), Mode::DryRun));

    // On mains, which the default plan disqualifies, so no block can begin.
    d.world.power = dozed_core::PowerState { on_ac: true, percentage: Some(80.0) };
    for i in 0..20 {
        d.drive_trial(MonoTime::from_millis(i * 60_000), &Rates::default());
    }

    assert_eq!(d.world().config.mode, Mode::Live, "the user asked for live and stays live");
    let trial = d.trial.as_ref().expect("still there, still waiting");
    assert!(trial.waiting());
    assert_eq!(trial.blocks().len(), 0, "and it has measured nothing, which is the honest part");
    assert!(!d.mode_driven_by_trial(), "so nothing should claim the mode is the trial's");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half: the fix must not have switched the mechanism off.
/// Drive the trial until a block is actually running.
///
/// A block now begins only once conditions have *held* for the settle period,
/// rather than on the first agreeable sample — see `Trial::on_sample`. Starting
/// eagerly is what let four consecutive blocks be discarded on a real machine,
/// each holding the mode for twenty minutes to measure nothing.
fn drive_until_a_block_runs(d: &mut Daemon) -> MonoTime {
    let mut now = MonoTime::ZERO;
    for _ in 0..64 {
        d.drive_trial(now, &Rates::default());
        if d.trial.as_ref().is_some_and(|t| t.current_arm().is_some()) {
            return now;
        }
        now = now.plus(Duration::from_secs(30));
    }
    panic!("no trial block started");
}

#[test]
fn a_running_block_still_drives_the_mode() {
    let dir = std::env::temp_dir().join(format!("dozed-drive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.set_mode(Mode::Live);
    d.trial = Some(Trial::new(TrialPlan::default(), Mode::DryRun));

    // Idle, on battery, observing: a block may start.
    d.world.power = dozed_core::PowerState { on_ac: false, percentage: Some(80.0) };
    d.world.desktop = dozed_core::DesktopState::IdlePresent;
    d.world.focus_source_healthy = true;
    drive_until_a_block_runs(&mut d);

    let trial = d.trial.as_ref().expect("still there");
    assert!(trial.current_arm().is_some(), "a block is under way");
    assert!(d.mode_driven_by_trial(), "and the status line should say the mode is its");
    assert_eq!(
        d.world().config.mode,
        trial.current_arm().expect("just asserted").mode(),
        "the mode is the arm's"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_ceiling_survives_a_restart_and_a_lapsed_one_does_not() {
    // The difference from the global snooze, and it is deliberate: a snooze
    // is minutes and a restart during one is a rare accident, while "cap
    // this until I say otherwise" is a standing instruction a restart has no
    // business quietly revoking.
    let dir = std::env::temp_dir().join(format!("dozed-holds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("holds.json");

    let mut d = daemon_with_trace(&dir);
    let live = dozed_core::AppKey::desktop("firefox");
    let lapsed = dozed_core::AppKey::desktop("slack");
    let now = d.mono_now();
    d.planner.set_hold(
        &live,
        dozed_core::Ceiling::CapOnly { pct: Some(20) },
        Some(now.plus(Duration::from_secs(3600))),
        now,
    );
    d.planner.set_hold(
        &lapsed,
        dozed_core::Ceiling::LeaveAlone,
        Some(now.plus(Duration::from_millis(1))),
        now,
    );
    d.persist_holds_at(&path);

    // A fresh daemon, reading what the last one wrote.
    let mut next = daemon_with_trace(&dir);
    next.load_holds_at(&path);
    let now = next.mono_now();
    assert_eq!(
        next.planner().hold(&live, now).map(|h| h.ceiling),
        Some(dozed_core::Ceiling::CapOnly { pct: Some(20) }),
        "the standing instruction survived"
    );
    assert!(
        next.planner().hold(&lapsed, now).is_none(),
        "and one whose time was up is history, not a ceiling"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clearing_the_last_ceiling_leaves_no_file_behind() {
    let dir = std::env::temp_dir().join(format!("dozed-holds2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("holds.json");

    let mut d = daemon_with_trace(&dir);
    let app = dozed_core::AppKey::desktop("firefox");
    let now = d.mono_now();
    d.planner.set_hold(&app, dozed_core::Ceiling::LeaveAlone, None, now);
    d.persist_holds_at(&path);
    assert!(path.exists(), "something to remember");

    d.planner.clear_hold(&app);
    d.persist_holds_at(&path);
    assert!(!path.exists(), "nothing to remember, and no stale file outliving the last ceiling");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_snooze_is_refused_while_a_trial_is_running() {
    // A trial is a schedule of mode changes. A snooze overwriting its live
    // blocks with dry-run ones would not produce a wrong measurement, it
    // would produce a confident one: dry-run against dry-run, reported as
    // proof that dozing saves nothing.
    let dir = std::env::temp_dir().join(format!("dozed-snooze4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.trial = Some(Trial::new(TrialPlan::default(), Mode::DryRun));

    let refused = d.snooze(Duration::from_secs(60)).unwrap_err();
    assert!(refused.contains("trial"), "{refused}");
    assert!(d.snooze_remaining().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(start_paused = true)]
async fn a_snoozed_daemon_still_wakes_up_to_end_its_snooze() {
    // Nothing held and nothing pending is exactly the state that earns the
    // 60s idle interval, and a snooze is that state by construction — so
    // without the deadline being folded into `next_wakeup` an hour's snooze
    // would quietly run up to a minute long every time.
    let dir = std::env::temp_dir().join(format!("dozed-snooze5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    let idle = d.next_wakeup();

    d.snooze(Duration::from_secs(5)).unwrap();
    assert!(
        d.next_wakeup() <= Duration::from_secs(5),
        "the tick must arrive when the snooze ends, not {:?} later",
        d.next_wakeup()
    );
    assert!(idle > Duration::from_secs(5), "otherwise this test proves nothing: {idle:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_report_stays_counterfactual_when_nothing_could_have_been_done() {
    // Keyed on the startup flag alone, a daemon started `--live` and then
    // switched to dry-run reported hours of "has dozed" for holds it had
    // already released. The inverse is this case: a mode saying `live` on a
    // daemon with no actuator is still a counterfactual, and must read as one.
    let dir = std::env::temp_dir().join(format!("dozed-cf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    assert!(d.report().dry_run);

    d.set_mode(Mode::Live);
    assert!(!d.is_live(), "this daemon has no live actuator");
    assert!(d.report().dry_run, "so the report is still about what would have happened");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_status_mode_is_the_one_in_force_not_the_one_configured() {
    // Seen on a real restart: the shipped unit passes `--dry-run`, the config
    // said `mode = "live"`, and `dozectl status` printed `mode live` above
    // `0 held` on a daemon holding an actuator it did not have. Same shape as
    // the `Session::why` bug — one field answering two questions.
    let dir = std::env::temp_dir().join(format!("dozed-mode-eff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = Config { mode: Mode::Live, ..Config::default() };
    let header = dozed_core::TraceHeader {
        version: dozed_core::TRACE_VERSION,
        dozed_version: "test".into(),
        instance: "test".into(),
        wall_start: 0,
        live: false,
        config: Box::new(config.clone()),
    };
    // `live: false` is what `--dry-run` produces, whatever the config says.
    let d = Daemon::new(
        config,
        false,
        JournalFile::open(dir.join("journal.jsonl")),
        TraceFile::create(&dir, &header),
        None,
    );

    assert_eq!(d.effective_mode(), Mode::DryRun, "no actuator, so no live mode");
    assert_eq!(
        d.overridden_mode(),
        Some(Mode::Live),
        "and the discrepancy is reported rather than silently resolved"
    );
    assert!(d.report().dry_run, "the report already agreed; only status did not");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mode_in_force_is_not_reported_as_overridden() {
    // The note must appear only when the two genuinely differ, or every status
    // carries a parenthetical nobody needs.
    let dir = std::env::temp_dir().join(format!("dozed-mode-agree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = daemon_with_trace(&dir);
    assert_eq!(d.effective_mode(), Mode::DryRun);
    assert_eq!(d.overridden_mode(), None, "config and reality agree");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_trial_is_refused_without_a_live_actuator() {
    // The check that matters most: a trial whose "live" blocks cannot act
    // would compare dry-run against dry-run, measure the noise floor, and
    // report it as proof that dozing does nothing on this machine.
    let dir = std::env::temp_dir().join(format!("dozed-trial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);

    let err = d.start_trial(TrialPlan::default()).unwrap_err();
    assert!(err.contains("--live"), "{err}");
    assert!(d.trial().is_none());
    assert!(d.report().measured.is_none(), "and nothing is claimed to have been measured");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A trial that has ended is kept for the report, and must stop steering.
///
/// It had no current arm, so `desired_mode` fell back to `baseline`, and
/// `drive_trial` asserted that on every sample for the life of the daemon —
/// and past it, because the trial is reloaded from disk at startup. Going
/// live reverted a few seconds later and no amount of `dozectl trial stop`
/// helped, `stop` being the thing that finishes a trial.
#[test]
fn a_finished_trial_is_kept_for_the_report_and_stops_driving_the_mode() {
    let dir = std::env::temp_dir().join(format!("dozed-finished-trial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.live = Some(dozed_linux::CgroupActuator::without_systemd(Guard::allow_all()));

    d.start_trial(TrialPlan::default()).expect("the default plan starts");
    let _ = d.stop_trial();
    assert!(d.trial().is_some_and(Trial::finished), "kept, so the report can still show it");

    // The user goes live after the trial is over, which is the whole point
    // of it being over.
    d.set_mode_by_hand(Mode::Live).expect("a finished trial does not own the mode");
    assert_eq!(d.world.config.mode, Mode::Live);

    // Sampling is what used to undo it — every few seconds, indefinitely.
    for _ in 0..5 {
        let at = d.now();
        d.drive_trial(at, &Rates::default());
    }
    assert_eq!(
        d.world.config.mode,
        Mode::Live,
        "a finished trial reverted the mode the user had just chosen"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bug this guard exists for: "go live" worked, and then undid itself.
///
/// `drive_trial` re-asserts the schedule's mode on every sample, so a mode
/// set by hand during a trial held for one tick and reverted with nothing
/// saying why — and the sample taken in between was attributed to a block
/// the schedule had not put in that mode.
#[test]
fn a_mode_set_by_hand_is_refused_while_a_trial_is_driving_the_mode() {
    let dir = std::env::temp_dir().join(format!("dozed-mode-trial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.live = Some(dozed_linux::CgroupActuator::without_systemd(Guard::allow_all()));

    // Without a trial, the same call is exactly what it always was.
    d.set_mode_by_hand(Mode::Live).expect("a mode change with no trial running must work");
    assert_eq!(d.world.config.mode, Mode::Live);
    d.set_mode_by_hand(Mode::DryRun).expect("and back again");

    d.start_trial(TrialPlan::default()).expect("the default plan starts");

    // A trial that has not started a block owns nothing. Refusing here was
    // half of a real trap: on a machine the plan disqualified, the trial
    // could never begin, so this refusal was permanent and pointed at a
    // measurement that was never going to be taken.
    d.set_mode_by_hand(Mode::Live).expect("a waiting trial has no claim on the mode");
    d.set_mode_by_hand(Mode::DryRun).expect("and back again");

    // Once a block is actually running, the mode is its.
    d.world.power = dozed_core::PowerState { on_ac: false, percentage: Some(80.0) };
    d.world.desktop = dozed_core::DesktopState::IdlePresent;
    d.world.focus_source_healthy = true;
    drive_until_a_block_runs(&mut d);
    assert!(d.mode_driven_by_trial(), "a block is under way");

    let err = d
        .set_mode_by_hand(Mode::Monitor)
        .expect_err("a running block owns the mode; setting it by hand would spoil it");
    assert!(err.contains("trial stop"), "the refusal must name the way out: {err}");

    // Stopping the trial hands the mode back, rather than leaving it stuck.
    let _ = d.stop_trial();
    d.set_mode_by_hand(Mode::Live).expect("with the trial stopped the mode is ours again");
    assert_eq!(d.world.config.mode, Mode::Live);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `pairs` comes over IPC from whatever the client sent, and `Trial::new`
/// lays the schedule out eagerly before anything else looks at it. The
/// refusal has to happen on this side: the CLI's "six is the floor" is help
/// text, and a client is not obliged to be `dozectl` at all.
#[test]
fn a_trial_plan_from_a_client_is_bounded_before_it_is_laid_out() {
    let dir = std::env::temp_dir().join(format!("dozed-trial-bound-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    // Past the `--live` gate, so what is under test is the plan and not that.
    d.live = Some(dozed_linux::CgroupActuator::without_systemd(Guard::allow_all()));

    let err = d
        .start_trial(TrialPlan { pairs: 50_000_000, ..TrialPlan::default() })
        .expect_err("50 million pairs is an 850 MB trial.json rewritten every reading");
    assert!(err.contains("pairs"), "{err}");
    assert!(d.trial().is_none(), "and nothing was laid out");

    // The shipped plan still starts, which is the half worth guarding.
    d.start_trial(TrialPlan::default()).expect("the default plan must still be startable");
    assert!(d.trial().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_power_meter_reaches_the_status_report() {
    // End to end over the real machine: `PowerMeter` reads `/sys` and
    // `/proc`, `energy::Meter` turns two reads into rates, and
    // `energy_status` is what `dozectl status` prints. Every layer is unit
    // tested against fixtures; this is the one check that they are wired to
    // each other and to the tick.
    //
    // Only `/proc/stat` is asserted on, because it is the one counter every
    // Linux kernel has — a container without `cpuidle` or a battery must not
    // fail this.
    let dir = std::env::temp_dir().join(format!("dozed-energy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);

    d.measure_energy();
    assert!(d.rates.system_wakeups_per_sec.is_none(), "one reading is not a rate");

    // Long enough for the monotonic clock to move a whole millisecond, since
    // a zero-length interval is correctly refused.
    std::thread::sleep(Duration::from_millis(120));
    d.measure_energy();

    let status = d.energy_status().expect("/proc/stat exists on every Linux");
    assert!(
        status.system_wakeups_per_sec.unwrap() >= 0.0,
        "the machine-wide wakeup rate should be a number: {status:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_bus_watchers_cost_is_reported_as_a_rate() {
    // A cumulative count is not a cost. This is the only way to see whether
    // moving the system watcher onto signals actually helped
    // (`sources::SYSTEM_POLL`), so it has to be a number on `dozectl status`
    // and not a claim in a comment.
    let dir = std::env::temp_dir().join(format!("dozed-bus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    assert_eq!(d.bus_calls_per_sec(), None, "no system bus, nothing to report");

    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    d.set_bus_counter(counter.clone());
    let t0 = Instant::now();
    d.measure_bus_cost(t0);
    assert_eq!(d.bus_calls_per_sec(), Some(0.0), "one reading is not a rate");

    // Three property reads, ten seconds apart: 0.3 a second.
    counter.store(3, std::sync::atomic::Ordering::Relaxed);
    d.measure_bus_cost(t0 + Duration::from_secs(10));
    assert!((d.bus_calls_per_sec().unwrap() - 0.3).abs() < 1e-6, "{:?}", d.bus_calls_per_sec());

    // And a counter that somehow went backwards is zero, not a huge negative.
    counter.store(0, std::sync::atomic::Ordering::Relaxed);
    d.measure_bus_cost(t0 + Duration::from_secs(20));
    assert_eq!(d.bus_calls_per_sec(), Some(0.0));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_broken_recorder_is_retried_from_the_tick_and_recovers_there() {
    // The wiring, as distinct from the mechanism. `tracefile`'s own tests prove
    // an outage ends when the disk does; this proves the daemon is the thing
    // that asks. A `retry` nobody calls is the original bug with more code.
    let dir = std::env::temp_dir().join(format!("dozed-tick-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });
    d.on_windows(&[window_of(std::process::id(), true)]);
    assert!(d.trace.is_recording(), "the fixture has to be recording for this to be about it");

    // A full disk, from the kernel rather than from a mock: `/dev/full` accepts
    // the open and refuses every write with ENOSPC.
    d.trace.fill_the_disk();
    // Something has to be *in* the buffer for the flush to have anything to
    // fail on — an empty flush succeeds on a full disk, which is itself worth
    // knowing: the failure arrives on the first tick that has records to push.
    d.dispatch(Event::FocusSourceHealth { healthy: false });
    d.trace.flush();
    assert!(!d.trace.is_recording(), "the flush should have taken it down");
    assert!(d.trace.stopped().is_some());

    // Nothing changed about the real path, so it is writable again — which is
    // what a disk being emptied looks like from here. The tick is the only thing
    // that touches it.
    d.on_tick();
    assert!(d.trace.is_recording(), "the tick has to be what retries");
    assert!(d.trace.stopped().is_none(), "and the message has to clear itself");
    assert_eq!(d.trace.gaps(), 1, "with the hole still counted");

    // And the status the user reads agrees with all three.
    let status = d.trace_status().expect("a started recorder reports");
    assert!(status.stopped.is_none());
    assert_eq!(status.gaps, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unchanged_sample_is_not_written_to_the_trace_twice() {
    // 93% of a real 67 MB recording was `sampled` events, and 40% of those said
    // exactly what the one before them said. A frozen app reporting nothing
    // should not cost 172 bytes a tick to keep saying so.
    let dir = std::env::temp_dir().join(format!("dozed-dupsample-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });

    let key = AppKey::comm("slack");
    let quiet = dozed_core::ProcSample { cpu_pct: 0.0, wakeups_per_sec: 0.0, gpu_pct: None };
    let busy = dozed_core::ProcSample { wakeups_per_sec: 900.0, ..quiet };

    let before = d.trace.records();
    for _ in 0..10 {
        d.dispatch(Event::Sampled { app: key.clone(), sample: quiet });
    }
    let after_quiet = d.trace.records();
    assert_eq!(after_quiet - before, 1, "ten identical samples are one record");

    // A change is recorded, and then repeats of *it* are suppressed in turn.
    d.dispatch(Event::Sampled { app: key.clone(), sample: busy });
    for _ in 0..5 {
        d.dispatch(Event::Sampled { app: key.clone(), sample: busy });
    }
    assert_eq!(d.trace.records() - after_quiet, 1, "the change, and nothing after it");

    // Going back to the previous value is a change too — suppression is against
    // the last thing written, not against everything ever seen.
    d.dispatch(Event::Sampled { app: key.clone(), sample: quiet });
    assert_eq!(d.trace.records() - after_quiet, 2);

    // And it is per app: one app repeating itself must not silence another.
    let other = AppKey::comm("firefox");
    d.dispatch(Event::Sampled { app: other, sample: quiet });
    assert_eq!(d.trace.records() - after_quiet, 3, "a different app is a different stream");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_tests_cannot_write_to_a_real_daemons_state() {
    // The check that keeps the redirect honest. Without it these tests reach
    // `$XDG_STATE_HOME` directly, and `cargo test` on a developer's own machine
    // truncates the frozen set of whatever daemon is running there — which, if
    // anything were held, would leave those applications frozen with no record
    // to sweep them from.
    let dir = std::env::temp_dir().join(format!("dozed-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);

    for p in [crate::paths::frozen_file(), crate::paths::trial_file()] {
        assert!(
            p.starts_with(std::env::temp_dir()),
            "a test would have written to {}, which is a real daemon's",
            p.display()
        );
    }

    // And writing really does land there rather than in the live tree.
    d.persist_frozen();
    assert!(crate::paths::frozen_file().exists(), "the redirected file is the one written");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn we_can_walk_our_own_process_tree() {
    let pids = collect_tree_pids(std::process::id());
    assert!(pids.contains(&std::process::id()));
}

#[test]
fn a_window_on_screen_when_a_share_began_is_still_protected_once_it_is_hidden() {
    // The gap §10.8 named. You share a window, switch to another, and the
    // shared one is hidden — idle-looking and, to every rule in the config,
    // an excellent freeze candidate. Freezing it does not stop the share, it
    // freezes the picture only the other people on the call can see.
    let dir = std::env::temp_dir().join(format!("dozed-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });

    let me = std::process::id();
    d.on_windows(&[window_of(me, false)]);
    let key = d.pids.keys().next().expect("the window produced an app").clone();
    assert!(
        d.world.apps[&key].visibility.is_on_screen(),
        "the app has to be on screen for this test to be about anything"
    );

    // A share opens. Nothing says what is in it, so what was on screen is
    // taken as the answer.
    share(&mut d, &[4242]);
    assert!(d.shared_candidates.contains(&me));
    assert!(
        d.world.apps[&key].is_exempt(Exemption::MaybeShared),
        "an app on screen when a share opened is a candidate for being in it"
    );

    // Now the user switches away. This is the moment the whole exemption
    // exists for: the app is hidden, and it is still on somebody's screen.
    d.on_windows(&[window_of(me, true)]);
    assert_eq!(d.world.apps[&key].visibility, Visibility::Hidden);
    assert!(
        d.world.apps[&key].is_exempt(Exemption::MaybeShared),
        "the protection has to outlive the visibility that created it, \
         or it protects nothing at all"
    );

    // And it ends with the session rather than on a timer, so a long call
    // keeps it and a cancelled picker costs nothing.
    share(&mut d, &[]);
    assert!(d.shared_candidates.is_empty());
    assert!(!d.world.apps[&key].is_exempt(Exemption::MaybeShared));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_share_adds_to_the_candidates_rather_than_replacing_them() {
    // Two sessions open at once is two things being shared. Recomputing the
    // set from what is visible *now* would drop the first share's window,
    // which is hidden by then precisely because it is being shared.
    let dir = std::env::temp_dir().join(format!("dozed-shared2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut d = daemon_with_trace(&dir);
    d.dispatch(Event::FocusSourceHealth { healthy: true });

    let me = std::process::id();
    d.on_windows(&[window_of(me, false)]);
    share(&mut d, &[4242]);
    assert!(d.shared_candidates.contains(&me));

    // The first shared window goes away entirely, and a second share opens
    // while the first is still running.
    d.on_windows(&[]);
    share(&mut d, &[4242, 4243]);
    assert!(
        d.shared_candidates.contains(&me),
        "the first share's window is still being watched by somebody"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
