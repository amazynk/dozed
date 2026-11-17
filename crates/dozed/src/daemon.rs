//! The main loop: sources in, intents out, journal everywhere.
//!
//! ```text
//!   sources ─► Event ─► WorldState ─► Planner ─► Vec<Intent> ─► Actuator
//!                                                     └────────► Journal ─► IPC
//! ```
//!
//! The loop does not know which actuator it holds. That is the dry-run seam
//! (`DESIGN.md` §3): both modes consume the identical intent stream, so the
//! journal a user reads in dry-run is literally what live mode would have done.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, watch};

use dozed_core::energy::{Meter as EnergyMeter, Rates};
use dozed_core::trace::TraceRecord;
use dozed_core::trial::{Conditions, Trial, TrialPlan};
use dozed_core::{
    AppClass, AppKey, AppSeen, Config, Event, Exemption, Intent, JournalEntry, Mode, MonoTime,
    Outcome, Planner, ReportAccumulator, Visibility, WorldState,
};
use dozed_linux::energy::PowerMeter;
use dozed_linux::thermal::Monitor as ThermalMonitor;
use dozed_linux::{Actuator, CgroupActuator, DryRunActuator, Guard, Target, Unit};

use crate::exemptions::ExemptionTracker;
use crate::identity::{self, Resolved, WindowHint};
use crate::ipcserver::Command;
use crate::journalfile::JournalFile;
use crate::sampler::{self, Sampler};
use crate::sources::{HelperWatch, Incoming};
use crate::tracefile::TraceFile;

/// Floor on the sampling interval. Never spin, however close a deadline is.
const MIN_TICK: Duration = Duration::from_secs(2);

/// How often to re-read the samplers while a client has the app table on screen.
///
/// §13's back-off is an argument about what a tool costs while nobody is looking
/// at it. With nothing held and nothing pending the tick is 60s, so the dashboard
/// — which polls every 2s — redrew the same `wakes/s` and `CPU%` thirty times
/// running. The figures were accurate as of the last tick, which is not the same
/// thing as fresh, and the first question anyone asked of the window was why the
/// numbers did not move.
///
/// Costed rather than guessed. One pass is 33 ms over ~2,725 threads
/// (`sampler::ctxt_switches_of`), so 5s is 0.66% of a core against 0.055% at the
/// idle interval — and it is paid only while a window is open, ending within
/// [`WATCH_TTL`] of it closing. The 2s floor was rejected: it triples that for a
/// figure averaged over `cpu_sample_window`, which therefore cannot move three
/// times faster anyway.
const WATCHED_SAMPLE: Duration = Duration::from_secs(5);

/// How long one `ListApps { watching: true }` keeps [`WATCHED_SAMPLE`] in force.
///
/// Three of the dashboard's 2s poll, so two lost polls do not make the table
/// stutter. Expiring rather than being switched off is the point: a window that
/// is minimised, blocked or killed outright stops costing anything within six
/// seconds without having to say goodbye first, and nothing has to be trusted to
/// send it.
const WATCH_TTL: Duration = Duration::from_secs(6);

/// When the next sampling tick is due.
///
/// Exists to be a value rather than a `sleep` inside `select!`. The deadline is
/// absolute, so re-creating the timer on every loop iteration — which `select!`
/// forces — cannot postpone it. Getting this wrong starved the tick completely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tick {
    due: tokio::time::Instant,
}

impl Tick {
    const fn new(due: tokio::time::Instant) -> Self {
        Self { due }
    }

    /// The instant to sleep until. Constant across iterations by construction.
    const fn deadline(&self) -> tokio::time::Instant {
        self.due
    }

    /// Schedule the next one, `interval` from `now`.
    fn arm(&mut self, now: tokio::time::Instant, interval: Duration) {
        self.due = now + interval;
    }

    /// Bring the next tick forward, if what just happened made it due sooner.
    ///
    /// The interval is chosen from state — apps known, deadlines pending,
    /// whether anything has been measured — and all of that changes between
    /// ticks, while `arm` is only reached *after* one. A daemon that had just
    /// started therefore armed 60s from an empty world and then slept through
    /// the arrival of every app on the desktop: nothing was sampled for a
    /// minute, and a grace deadline created in that window was noticed up to a
    /// minute late.
    ///
    /// Only ever earlier, which is the whole point of the type. Re-arming on
    /// each event would restart the interval from zero every time the helper
    /// heartbeats, and that starvation is exactly what [`Tick`] exists to
    /// prevent — so this takes the minimum rather than the newest.
    fn pull_in(&mut self, now: tokio::time::Instant, interval: Duration) {
        self.due = self.due.min(now + interval);
    }
}

/// How often to ask whether the focus source is still there.
///
/// Matches the helper's own heartbeat period, so the check is never the reason a
/// loss is noticed late. `HELPER_TIMEOUT` is three of these.
const HELPER_CHECK: Duration = Duration::from_secs(5);

/// How long a trial may fail to start a block before the daemon says so.
///
/// Longer than any ordinary gap. A laptop on a desk all afternoon, a machine in
/// use all morning, an evening plugged in — all of those are a trial correctly
/// waiting, and warning about them would teach people to ignore the line. Six
/// hours of *continuous* inability to begin is the point at which "waiting" and
/// "never going to happen" stop being distinguishable from the outside.
const TRIAL_STALL_AFTER: Duration = Duration::from_secs(6 * 3600);

pub struct Daemon {
    world: WorldState,
    planner: Planner,
    sampler: Sampler,
    accumulator: ReportAccumulator,
    helper: HelperWatch,

    live: Option<CgroupActuator>,
    dry: DryRunActuator,

    /// AppKey → cgroup, which is how an intent finds something to write to.
    cgroups: BTreeMap<AppKey, Vec<PathBuf>>,
    /// AppKey → the PIDs we last saw for it. The whole tree: this is what
    /// actuation acts on.
    pids: BTreeMap<AppKey, Vec<u32>>,
    /// AppKey → the PIDs it exclusively owns, which is what it is *measured*
    /// on. Narrower than `pids` for anything that launched another app.
    sample_pids: BTreeMap<AppKey, Vec<u32>>,
    /// Why the frozen-set file could not be written, if it could not.
    ///
    /// `None` while the crash-recovery net is intact. See `persist_frozen`.
    frozen_unwritable: Option<String>,
    /// The last sample actually written to the trace, per app, so an unchanged
    /// one is not written again. See `sample_is_a_repeat`.
    last_traced_sample: BTreeMap<AppKey, dozed_core::ProcSample>,
    /// PID-level exemption snapshots, resolved to apps.
    exemptions: ExemptionTracker,
    /// PIDs of apps that were on screen when a screen share began, and which
    /// might therefore be *in* it (`Exemption::MaybeShared`).
    ///
    /// Held here rather than recomputed, because the fact being remembered is
    /// about a moment that has passed: by the time it matters the shared window
    /// is hidden, which is the whole hazard. Cleared when the last session
    /// closes.
    shared_candidates: BTreeSet<u32>,
    /// Apps we have already pointed out as having no declared identity, so the
    /// advice is given once per run rather than once per snapshot.
    noted_identities: BTreeSet<AppKey>,

    journal: JournalFile,
    /// Entries this run produced. Kept apart from `backlog` because "what has
    /// dozed done" almost always means "since it started" — a journal that
    /// opens on yesterday's decisions, with the clock running backwards at the
    /// boundary, buries the ones you asked for.
    entries: Vec<JournalEntry>,
    /// Entries from previous runs, read from disk at startup and never appended
    /// to. Shown only on request (`dozectl journal --all`).
    backlog: Vec<JournalEntry>,
    broadcast: broadcast::Sender<JournalEntry>,
    /// The full session recording: what we saw, what we concluded, what we did.
    trace: TraceFile,

    started: Instant,
    own_pids: BTreeSet<u32>,
    self_counter: Option<(u64, Instant)>,
    self_wakeups: f32,
    config_path: Option<PathBuf>,
    battery_wh: Option<f64>,
    /// Which display server and focus source we resolved at startup.
    session: Option<crate::session::Session>,

    /// The battery and `cpuidle` counters, with their paths resolved once.
    meter: PowerMeter,
    /// The fitted relationship between load and package watts on this machine
    /// (`dozed_core::power_model`).
    ///
    /// Fed from the same `Rates` the status line renders, so the model and the
    /// figures it explains cannot disagree — and so it costs no reads of its own.
    power_model: dozed_core::power_model::PowerModel,
    /// Turns those counters into rates (`dozed_core::energy`).
    energy: EnergyMeter,
    /// The most recent rates, for `dozectl status` and for the trial.
    rates: Rates,
    /// The self-experiment, when one is running (`dozed_core::trial`).
    trial: Option<Trial>,
    /// Since when the trial has been unable to start a block, and whether that
    /// has already been reported. See [`Daemon::check_trial_stall`].
    trial_waiting_since: Option<Instant>,
    trial_stall_reported: bool,
    /// Property reads the system-bus watcher has made, and the rate derived from
    /// it. On an idle desktop that loop is most of what the daemon does, so it is
    /// the one cost §13 most needs shown.
    bus_calls: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    bus_counter: Option<(u64, Instant)>,
    bus_calls_per_sec: f32,
    /// Temperatures and fan speeds, for the quiet objective (§7.3).
    thermal: ThermalMonitor,
    /// Whether the current stuck-`hot` stretch has already been reported, so the
    /// warning is one line per stretch rather than one per tick.
    thermal_saturation_reported: bool,
    /// The thermal state the *planner* has been told, which is not always the
    /// one the monitor is in — see [`Daemon::check_thermal`]. Kept here rather
    /// than read back off the world so the daemon owns the one decision it is
    /// making about this signal.
    thermal_told: dozed_core::ThermalState,
    /// And the state the *monitor* is in, as last logged. Tracked separately
    /// because the log went silent exactly when the two diverged: see
    /// [`Daemon::check_thermal`].
    thermal_seen: dozed_core::ThermalState,
    /// "Leave me alone until this passes", if a user has asked for it.
    snooze: Option<Snooze>,
    /// What the tray indicator draws, when one is running.
    tray: Option<watch::Sender<crate::tray::TrayState>>,
    /// `[audio]` as the session-bus audio watcher should see it, so a reload
    /// that changes what counts as playing does not wait for a restart.
    audio_config: Option<watch::Sender<dozed_core::config::AudioConfig>>,
    /// The once-a-week summary notification (`crate::notify`).
    digest: Option<crate::notify::Digest>,

    /// When the last client that said it was watching the app table stops
    /// counting as one (`WATCH_TTL`).
    watching_until: Option<Instant>,
    /// Asks the session-bus supervisor for a round trip (`sources::supervise`).
    /// `None` on a daemon with no session bus at all, where there is nothing to ask.
    bus_probe: Option<mpsc::Sender<()>>,
    /// Whether our own session bus is believed to be delivering. Starts true: a
    /// daemon that has not yet had reason to doubt itself should not say it has.
    session_bus_healthy: bool,
    /// When `sample_all` last ran, by whichever path called it.
    ///
    /// The resampling deadline is measured from this rather than from the last
    /// tick, which is what stops the two paths sampling twice in the same
    /// millisecond when they happen to come due together.
    last_sample_at: Instant,
}

/// A temporary, self-expiring return to dry-run.
///
/// The gap it fills: the only controls were `live` and `dry-run`, both of them
/// permanent, so "stop touching my apps for the next hour" meant switching the
/// daemon off and remembering to switch it back on. Nobody remembers. A tool
/// that is switched off and forgotten saves nothing, and it does not even leave
/// a recording behind — so the honest way to be interrupted is a timer, not a
/// toggle.
///
/// Implemented as a mode change rather than as a new state the planner has to
/// know about, which is what keeps the dry-run seam intact: snoozing releases
/// every hold and stops acting through exactly the path `dozectl mode dry-run`
/// already takes, and un-snoozing goes back through `Planner::reassert` the same
/// way going live does. The planner never learns that snooze exists.
#[derive(Debug, Clone, Copy)]
struct Snooze {
    /// When to put the mode back.
    until: Instant,
    /// What to put it back *to*, captured at the moment of snoozing. Not
    /// re-derived on expiry: a config reload in between must not silently turn a
    /// snooze into a mode change the user never asked for.
    restore: Mode,
}

impl Daemon {
    pub fn new(
        config: Config,
        live: bool,
        journal: JournalFile,
        trace: TraceFile,
        config_path: Option<PathBuf>,
    ) -> Self {
        let guard = Guard::for_current_process();
        let own_pids = guard.protected_pids().clone();
        let (broadcast, _) = broadcast::channel(1024);
        // Read before `config` is moved into the world.
        let thermal = ThermalMonitor::with_thresholds(config.thermal.into());
        Self {
            world: WorldState::new(config),
            planner: Planner::new(),
            sampler: Sampler::new(),
            accumulator: ReportAccumulator::new(),
            helper: HelperWatch::default(),
            live: live.then(|| CgroupActuator::new(guard)),
            dry: DryRunActuator::default(),
            cgroups: BTreeMap::new(),
            pids: BTreeMap::new(),
            sample_pids: BTreeMap::new(),
            shared_candidates: BTreeSet::new(),
            last_traced_sample: BTreeMap::new(),
            frozen_unwritable: None,
            exemptions: ExemptionTracker::new(),
            noted_identities: BTreeSet::new(),
            journal,
            entries: Vec::new(),
            backlog: Vec::new(),
            broadcast,
            trace,
            started: Instant::now(),
            own_pids,
            self_counter: None,
            self_wakeups: 0.0,
            config_path,
            battery_wh: None,
            session: None,
            // Paths resolved once, here, not on every tick: globbing
            // `cpu*/cpuidle/state*` per sample would cost more than reading the
            // counters it finds — the lesson `sources::SystemProxies` already
            // paid for.
            meter: PowerMeter::new(),
            power_model: dozed_core::power_model::PowerModel::new(),
            energy: EnergyMeter::new(),
            rates: Rates::default(),
            trial: None,
            trial_waiting_since: None,
            trial_stall_reported: false,
            bus_calls: None,
            bus_counter: None,
            bus_calls_per_sec: 0.0,
            thermal,
            thermal_saturation_reported: false,
            thermal_told: dozed_core::ThermalState::default(),
            thermal_seen: dozed_core::ThermalState::default(),
            snooze: None,
            tray: None,
            audio_config: None,
            digest: None,
            watching_until: None,
            bus_probe: None,
            session_bus_healthy: true,
            last_sample_at: Instant::now(),
        }
    }

    /// Hand over the handle that asks the session bus whether it can still hear.
    pub fn set_bus_probe(&mut self, probe: mpsc::Sender<()>) {
        self.bus_probe = Some(probe);
    }

    /// Whether our own session bus is delivering, for `dozectl status`.
    #[must_use]
    pub fn session_bus_healthy(&self) -> bool {
        self.session_bus_healthy
    }

    /// Note that a client has the app table on screen and will ask again shortly.
    ///
    /// Called for every `ListApps { watching: true }`, so the state it sets is
    /// continuously renewed while a dashboard is up and lapses on its own
    /// afterwards (`WATCH_TTL`).
    pub fn note_watcher(&mut self) {
        self.watching_until = Some(Instant::now() + WATCH_TTL);
    }

    /// Whether somebody is reading the app table right now.
    fn watched(&self) -> bool {
        self.watching_until.is_some_and(|until| Instant::now() < until)
    }

    /// When the samples behind a watched app table next want refreshing.
    ///
    /// Absolute, and measured from the last sample rather than from now, for the
    /// same reason [`Tick`] is a value: a deadline recomputed inside `select!` is
    /// restarted by every branch that fires and never arrives at all.
    /// `last_sample_at` moves only when a sample is actually taken, so this is
    /// stable across iterations and cannot be starved by the 5s heartbeat.
    ///
    /// It is deliberately allowed to be in the past. A dashboard that has just
    /// opened is usually most of a tick away from the next sample, and answering
    /// its first poll with a figure up to a minute old is the very thing this is
    /// here to fix — so the first resample happens at once.
    fn resample_due(&self) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.last_sample_at + WATCHED_SAMPLE)
    }

    /// Publish state to a tray indicator, if one could be started.
    pub fn set_tray(&mut self, tray: watch::Sender<crate::tray::TrayState>) {
        self.tray = Some(tray);
    }

    /// Keep the audio watcher's copy of `[audio]` current across reloads.
    pub fn set_audio_config(&mut self, audio: watch::Sender<dozed_core::config::AudioConfig>) {
        self.audio_config = Some(audio);
    }

    /// Enable the weekly digest notification.
    pub fn set_digest(&mut self, digest: crate::notify::Digest) {
        self.digest = Some(digest);
    }

    /// Handed to the IPC server so each client can subscribe for itself.
    pub fn journal_sender(&self) -> broadcast::Sender<JournalEntry> {
        self.broadcast.clone()
    }

    pub fn set_battery_wh(&mut self, wh: Option<f64>) {
        self.battery_wh = wh;
    }

    /// Share the system-bus watcher's call counter, so its cost is reportable.
    pub fn set_bus_counter(&mut self, counter: std::sync::Arc<std::sync::atomic::AtomicU64>) {
        self.bus_calls = Some(counter);
    }

    pub fn set_session(&mut self, session: crate::session::Session) {
        self.session = Some(session);
    }

    pub fn session(&self) -> Option<&crate::session::Session> {
        self.session.as_ref()
    }

    fn now(&self) -> MonoTime {
        MonoTime::from_millis(self.started.elapsed().as_millis() as u64)
    }

    /// Run until shutdown. Always thaws on the way out, however we get there.
    pub async fn run(
        mut self,
        mut incoming: mpsc::Receiver<Incoming>,
        mut commands: mpsc::Receiver<Command>,
    ) -> Result<()> {
        // Anything a previous run left frozen must go first: a freeze outlives
        // the process that applied it (`DESIGN.md` §10.3).
        self.sweep_orphans();
        let journal_path = self.journal.path().to_path_buf();
        self.load_backlog(&journal_path);
        self.load_trial();
        self.load_holds();

        let mut shutdown = std::pin::pin!(wait_for_shutdown());

        // §10.2 has to be punctual, so it gets its own timer rather than riding
        // the sampling tick. The sampler backs off to 60s when nothing is
        // pending, which meant "three missed heartbeats and we thaw everything"
        // could actually take a minute and a half — caught by testing the
        // invariant instead of trusting it.
        //
        // This costs nothing in practice: a healthy helper already wakes us
        // every 5s with its heartbeat, and when there is no helper to lose the
        // check returns immediately.
        let mut watchdog = tokio::time::interval(HELPER_CHECK);
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // An *absolute* deadline, not a fresh `sleep(delay)` per iteration.
        //
        // `select!` drops the losing futures, so a relative sleep is restarted
        // from zero every time any other branch fires. The helper heartbeats
        // every 5s and the watchdog ticks every 5s, so a 60s idle interval never
        // completed and `on_tick` was starved outright: no sampling, no
        // self-cost measurement, and a counterfactual report stuck at zero.
        //
        // It only bit when idle, which is why no test caught it — with a grace
        // deadline pending the interval drops to 2s and wins against the 5s
        // heartbeat, and every test had a pending deadline. Five hours of real
        // idle desktop found it in one reading of the trace.
        let mut tick = Tick::new(tokio::time::Instant::now());

        loop {
            tokio::select! {
                // Both of these can change how long we may sleep — a window that
                // has just been hidden starts a grace clock, and an app that has
                // just appeared is one nobody has measured — so the deadline is
                // recomputed here rather than only after the next tick.
                Some(msg) = incoming.recv() => {
                    self.on_incoming(msg);
                    tick.pull_in(tokio::time::Instant::now(), self.next_wakeup());
                }
                Some(cmd) = commands.recv() => {
                    self.on_command(cmd);
                    tick.pull_in(tokio::time::Instant::now(), self.next_wakeup());
                }
                _ = watchdog.tick() => self.check_helper(),
                // Sampling only, and deliberately not `on_tick`.
                //
                // Sampling is the one thing a tick does whose staleness a human
                // can see, and also the one thing whose *result* does not depend
                // on how often it runs: the sampler averages over
                // `cpu_sample_window` (§13, "the reported rate must not depend on
                // the sample interval") and `ReportAccumulator::on_sample`
                // integrates rate x dt, so more samples divide the same total more
                // finely rather than inflating it.
                //
                // Neither holds for the rest of `on_tick`. The energy meter
                // differences counters since its previous reading and the trial
                // takes an unweighted mean over the samples in a block, so
                // speeding those up would have made opening the dashboard change
                // the experiment's own measurements. They stay above, on the §13
                // cadence, whether anybody is watching or not.
                () = tokio::time::sleep_until(self.resample_due()), if self.watched() => {
                    self.sample_all(Instant::now());
                }
                () = tokio::time::sleep_until(tick.deadline()) => {
                    self.on_tick();
                    // Back off hard when nothing is frozen and nothing is
                    // pending; only tighten when a grace deadline is close
                    // (`DESIGN.md` §13).
                    tick.arm(tokio::time::Instant::now(), self.next_wakeup());
                }
                () = &mut shutdown => {
                    tracing::info!("shutting down, restoring everything");
                    break;
                }
            }
        }

        self.dispatch(Event::Shutdown);
        // Belt and braces: the planner only restores what it believes it holds,
        // and the actuator is the one that actually knows.
        if let Some(live) = &mut self.live {
            for (key, outcome) in live.restore_all() {
                tracing::info!(app = %key, ?outcome, "restored on exit");
            }
        }
        self.persist_frozen();
        self.trace.flush();
        Ok(())
    }

    fn on_incoming(&mut self, msg: Incoming) {
        match msg {
            Incoming::Event(e) => self.dispatch(e),
            Incoming::WindowFacts(windows) => self.on_windows(&windows),
            Incoming::HelperAlive { compositor } => {
                if let Some(true) = self.helper.alive(Instant::now(), compositor) {
                    // Logged, because the loss is: a `going passive` warning with
                    // no matching recovery reads as a daemon that never came back.
                    // Seen in a real journal — the helper found a restarted daemon
                    // by heartbeat rather than by `Hello` four seconds in, and
                    // nothing in the log said so at all.
                    // The name is absent when the first contact was a heartbeat
                    // rather than a `Hello`, which is the ordinary case after a
                    // daemon restart — so it is omitted rather than printed as
                    // `compositor="unknown"`, which reads like a fault.
                    match self.helper.compositor.as_deref() {
                        Some(c) => tracing::info!(
                            compositor = c,
                            "focus source reporting; decisions resume"
                        ),
                        None => tracing::info!("focus source reporting; decisions resume"),
                    }
                    self.dispatch(Event::FocusSourceHealth { healthy: true });
                }
            }
            Incoming::Exemptions { exemption, pids } => {
                // A share opening is also the only moment at which the *shared*
                // app can be guessed at, so it is derived here rather than in
                // the watcher: the portal knows who asked, and only the daemon
                // knows what was on screen.
                if exemption == Exemption::ScreenCast {
                    for event in self.resolve_shared_windows(&pids) {
                        self.dispatch(event);
                    }
                }
                for event in self.exemptions.snapshot(exemption, pids, &self.pids) {
                    self.dispatch(event);
                }
            }
            // Not a domain event: it changes nothing the planner decides and is
            // never replayed. It exists so the dashboard can name the tab behind
            // a silent stream, so it stays out of the world state and out of the
            // trace, exactly like `SessionBusHealth`.
            Incoming::AudioStreams { streams } => self.exemptions.set_stream_titles(streams),
            // The answer to the question `check_helper` asked. It arrives after the
            // warning that prompted it, which is why that warning does not name a
            // culprit — this is where the culprit is named.
            Incoming::SessionBusHealth { healthy } => {
                if healthy == self.session_bus_healthy {
                    return;
                }
                self.session_bus_healthy = healthy;
                if healthy {
                    tracing::info!("our session bus is delivering again");
                } else {
                    tracing::warn!(
                        "our own session bus stopped delivering — the compositor helper is \
                         probably fine and we could not hear it. Rebuilding the connection."
                    );
                }
            }
            Incoming::SourceHealth { exemption, unavailable, degraded } => {
                self.exemptions.set_health(exemption, unavailable, degraded);
            }
            Incoming::Activation { pid } => {
                // Any process in the app's tree counts: a launcher addresses the
                // name owner, which need not be the process the compositor
                // reported a window for.
                let app = self
                    .pids
                    .iter()
                    .find(|(_, tree)| tree.contains(&pid))
                    .map(|(key, _)| key.clone());
                if let Some(app) = app {
                    self.dispatch(Event::ActivationRequested { app });
                }
            }
        }
    }

    /// How long we may sleep before the world could change on its own.
    fn next_wakeup(&self) -> Duration {
        let now = self.now();
        let until = self
            .planner
            .next_deadline()
            .map(|d| Duration::from_millis(d.ms.saturating_sub(now.ms)));
        let interval = sampler::adaptive_interval(
            self.held_count() > 0,
            until,
            // Nothing measured yet, and something to measure: the first two
            // readings should not wait out the idle interval.
            !self.pids.is_empty() && !self.sampler.any_sample(),
        )
        .max(MIN_TICK);
        // A snooze ends on a tick, so the tick has to arrive when it ends.
        // Without this the daemon backs off to 60s while snoozed — nothing is
        // held, nothing is pending, which is exactly the state that earns the
        // longest interval — and an hour's snooze would run a minute over.
        match self.snooze_remaining() {
            Some(left) => interval.min(left.max(MIN_TICK)),
            None => interval,
        }
    }

    /// Seed the backlog from disk so `dozectl journal --all` still answers
    /// after a restart.
    ///
    /// Into `backlog`, never into `entries`: they are different questions, and
    /// merging them is what let a `dozectl watch` open on entries from previous
    /// runs whose `ELAPSED` column counts from a different zero.
    fn load_backlog(&mut self, path: &std::path::Path) {
        let past = JournalFile::read_all(path);
        if !past.is_empty() {
            tracing::info!(entries = past.len(), "loaded journal backlog");
            self.backlog = past;
        }
    }

    /// The §10.2 watchdog. Losing sight of focus means we must not be holding
    /// anything, so this is checked on its own schedule and not with sampling.
    ///
    /// Going passive is unconditional and immediate: whatever the cause, we cannot
    /// see focus and must not hold anything. What is *not* immediate is the
    /// attribution. This used to log "compositor helper stopped reporting", which
    /// is one of two possibilities and was the wrong one for 2h14m of real
    /// downtime — the helper was calling every five seconds throughout and our own
    /// session bus was swallowing it. So the warning states the observation, and
    /// asks the bus whether the fault is ours; the answer comes back through
    /// `Incoming::SessionBusHealth` and names the culprit then.
    fn check_helper(&mut self) {
        if let Some(false) = self.helper.check(Instant::now()) {
            tracing::warn!(
                after = ?crate::sources::HELPER_TIMEOUT,
                "no window reports from the compositor helper; going passive and \
                 checking whether our own session bus is still delivering"
            );
            self.dispatch(Event::FocusSourceHealth { healthy: false });
            self.ask_about_the_bus();
        }
    }

    /// Ask the session-bus supervisor whether it can still hear anything.
    ///
    /// Only from the path above, so this costs nothing until something is already
    /// wrong: no timer, no polling, and §13's argument about what the daemon does
    /// while it runs is untouched.
    fn ask_about_the_bus(&self) {
        if let Some(probe) = &self.bus_probe {
            // One slot, and a full one means the same question is already in
            // flight. Never blocks: this runs on the main loop.
            let _ = probe.try_send(());
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        self.self_wakeups = sampler::self_wakeups(&mut self.self_counter, now);
        self.measure_bus_cost(now);
        self.check_snooze();
        self.check_helper();
        self.sample_all(now);
        self.measure_energy();
        self.check_thermal();
        self.dispatch(Event::Tick);
        self.publish_tray();
        self.publish_audio_config();
        self.check_digest();
        // Once per tick rather than per record: a trace that fsyncs its way
        // through a session is exactly the background work we exist to remove.
        self.trace.flush();
        // And if it is down, this is where it goes back and tries. Riding the
        // tick rather than owning a timer is what keeps a broken recorder from
        // becoming a wakeup source — the retry has its own interval and simply
        // declines until it has passed, so the cost on a healthy daemon is two
        // comparisons and on a broken one is one `open` a minute.
        self.trace.retry(self.now());
    }

    /// Tell the tray what to draw, and wake it only if that changed.
    ///
    /// `send_if_modified` is the whole cost argument: an idle desktop ticks, the
    /// state compares equal, and no task is woken at all. A status icon that
    /// repainted on a timer would be exactly the background work §13 exists to
    /// keep out of this daemon.
    fn publish_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let next = crate::tray::TrayState {
            mode: self.effective_mode(),
            held: self.planner.held().count(),
            known: self.world.apps.len(),
            snoozed: self.snooze_remaining(),
            can_act: self.is_live(),
            enabled: self.world.config.ui.tray,
        };
        tray.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
    }

    /// Hand the audio watcher the current `[audio]`, and wake it only if that
    /// changed — same argument as `publish_tray`, and the same
    /// `send_if_modified`: a reload that touched a rule must not restart a
    /// measurement that is halfway through establishing silence.
    fn publish_audio_config(&mut self) {
        let Some(audio) = &self.audio_config else { return };
        let next = self.world.config.audio;
        audio.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
    }

    /// Send the weekly digest when it comes due.
    fn check_digest(&mut self) {
        let Some(digest) = &mut self.digest else { return };
        let now = std::time::SystemTime::now();
        let uptime = self.started.elapsed();
        // First sight starts the clock rather than firing: a fresh install would
        // otherwise report on the minutes since it was installed.
        if !digest.seeded() {
            digest.mark(now);
            return;
        }
        if !digest.due(now, uptime) {
            return;
        }
        digest.mark(now);
        let (summary, body) = crate::notify::digest(&self.report(), self.battery_wh);
        tokio::spawn(async move { crate::notify::send(&summary, &body).await });
    }

    /// Turn the shared bus-call counter into a rate, the same shape as
    /// `self_wakeups`.
    ///
    /// A cumulative count is not a cost; a rate is, and it is the only way to see
    /// whether moving the system watcher onto signals actually helped
    /// (`sources::SYSTEM_POLL`).
    fn measure_bus_cost(&mut self, now: Instant) {
        let Some(counter) = &self.bus_calls else { return };
        let current = counter.load(std::sync::atomic::Ordering::Relaxed);
        self.bus_calls_per_sec = match self.bus_counter {
            Some((previous, at)) => {
                let dt = now.duration_since(at).as_secs_f32();
                if dt > 0.0 { current.saturating_sub(previous) as f32 / dt } else { 0.0 }
            }
            None => 0.0,
        };
        self.bus_counter = Some((current, now));
    }

    /// Read the power counters and let the trial, if any, act on them.
    ///
    /// On the sampling tick rather than a timer of its own, so it adds no
    /// wakeups: §13's constraint is not "be cheap" but "do not wake the CPU", and
    /// a measurement with its own interval would break it however cheap each read
    /// was. Measured at 0.32 ms per sweep (`dozed_linux::energy`).
    fn measure_energy(&mut self) {
        let reading = self.meter.read();
        let at = self.now();
        if let Some(rates) = self.energy.observe(reading, at) {
            self.rates = rates;
            // Three numbers already in hand, so the fit is arithmetic on this
            // tick's work rather than a fourth thing to measure.
            if let (Some(package_w), Some(cores_busy), Some(wakeups_per_sec)) =
                (rates.package_w, rates.machine_cores_busy, rates.system_wakeups_per_sec)
            {
                self.power_model.observe(dozed_core::power_model::Observation {
                    cores_busy,
                    wakeups_per_sec,
                    package_w,
                });
            }
            self.drive_trial(at, &rates);
        }
    }

    /// Read the thermal sensors, and say so only when the answer changed.
    ///
    /// On the sampling tick, like the power counters and for the same reason: a
    /// signal with its own timer would add wakeups whatever each read cost (§13).
    /// The hysteresis lives in `Monitor::poll`, so an idle machine still puts
    /// nothing in the trace — what is compared here is not the raw transition
    /// but what the planner was last *told*, which on a saturated machine
    /// deliberately differs from the state the monitor is in.
    fn check_thermal(&mut self) {
        // Polled for its side effects — the hysteresis, the fan calibration and
        // the saturation clock all advance here — rather than for its return.
        // What the planner is told is decided below, and on a saturated machine
        // the two are not the same.
        self.thermal.poll();
        let saturated = self.thermal.saturated();

        // A saturated signal is one this monitor has already concluded carries
        // no information: `hot` for four hours that its own quietest moment
        // would not have cleared. Its own doc names the consequence — "every
        // rule keyed on `thermal` has been firing unconditionally, and a
        // constant and a signal look identical from inside a rule" — and until
        // now that consequence was reported to the user and then handed to the
        // planner anyway, which is the one reader that cannot notice it.
        //
        // So stop passing it on. Not by retuning anything: `Monitor` keeps its
        // state, its thresholds and its readings exactly as they are, `dozectl
        // status` still shows `hot (saturated)`, and the thresholds stay the
        // user's to set — that decision is argued at `Monitor::saturated` and is
        // not being reversed here. This is the narrower claim that a signal
        // known to be uninformative should not be *acted* on, and §3 settles
        // which way it resolves: ambiguity resolves towards doing less, so the
        // planner is told `normal` and `thermal` rules stop firing until the
        // machine demonstrates the state can still clear.
        let told = self.thermal.state_for_rules();
        let seen = self.thermal.state();
        if self.thermal_told != told {
            self.thermal_told = told;
            self.thermal_seen = seen;
            tracing::info!(
                state = told.as_str(),
                monitor = seen.as_str(),
                saturated,
                "thermal state changed"
            );
            self.dispatch(Event::ThermalChanged { thermal: told });
        } else if self.thermal_seen != seen {
            // The monitor moved and the planner was told nothing, which is
            // right — and used to be logged as nothing, which is not. While
            // `told` is pinned at `normal` by saturation it cannot change, so
            // comparing against it alone took the thermal log dark for the whole
            // stretch: on one machine the only thing left in the journal was the
            // saturation warning re-arming at exactly four-hour intervals, four
            // times, with the hundred-odd transitions underneath it invisible.
            //
            // Those transitions are the evidence for the question the warning
            // raises — whether the machine is really that hot or the thresholds
            // are out of its range — so they are worth a line each. The signal
            // is still not acted on: nothing is dispatched here.
            self.thermal_seen = seen;
            tracing::info!(
                monitor = seen.as_str(),
                state = told.as_str(),
                saturated,
                "the thermal monitor changed state; the planner was not told, because this \
                 signal is suspended"
            );
        }

        // Said once per stretch, not once per run: the state clearing resets it,
        // so a machine that is saturated, gets fixed, and saturates again on a
        // different threshold says so both times. Silence is the failure mode
        // being closed here — the signal was pinned at `hot` for 95% of a
        // fortnight's decisions and nothing anywhere mentioned it.
        if saturated {
            if !self.thermal_saturation_reported {
                self.thermal_saturation_reported = true;
                let quietest = self.thermal.quietest_while_hot();
                // `effective()`, not `thresholds()`. The raw config has no fan
                // thresholds on this machine — they are *learned* from the
                // observed RPM span — so `cool_rpm` was `None`, and `tracing`
                // omits a `None` field rather than printing it. The warning
                // therefore said "the readings beside this line say which" and
                // then printed no fan threshold at all, sixteen times, while
                // `saturated_after` reached its verdict using the learned value.
                // `effective()` is the same accessor that decision uses.
                let t = self.thermal.effective();
                tracing::warn!(
                    quietest_rpm = quietest.fastest_rpm,
                    coolest_celsius = quietest.hottest_celsius,
                    cool_rpm = t.cool_rpm,
                    cool_celsius = t.cool_celsius,
                    "thermal has read `hot` for four hours without this machine's quietest \
                     moment coming near clearing it. `thermal` rules are now suspended — a \
                     signal that cannot clear is a constant, and a constant and a signal look \
                     identical from inside a rule. Either the machine really is that hot or \
                     these thresholds are out of its range — the readings beside this line say \
                     which."
                );
            }
        } else if std::mem::replace(&mut self.thermal_saturation_reported, false) {
            // Say that it cleared, for the reason `exemptions::SoundServer`
            // already gives for its own recovery line: "a tool that reports a
            // fault it has recovered from teaches people to ignore the field it
            // reports it in". This stretch ended twelve times on one machine and
            // said nothing any of them, so the only evidence `thermal` rules had
            // started firing again was the absence of further warnings.
            let t = self.thermal.effective();
            tracing::info!(
                state = self.thermal.state().as_str(),
                cool_rpm = t.cool_rpm,
                cool_celsius = t.cool_celsius,
                "the thermal signal cleared; `thermal` rules are live again"
            );
        }
    }

    /// Advance the self-experiment, and put the mode where its schedule says.
    ///
    /// The trial only ever *asks* for a mode. Everything else — re-asserting the
    /// holds, journalling the switch, recording it in the trace — happens because
    /// [`Daemon::set_mode`] is the same path `dozectl mode` takes. A trial that
    /// reached past that would be measuring something other than the mode a user
    /// can set.
    /// Say so when a trial cannot start a block and has not been able to for a
    /// long time.
    ///
    /// A trial that is waiting is not broken — the conditions are gated on
    /// purpose, and "there is no honest way to subtract a person from a
    /// measurement" (§11.2). What is broken is waiting *silently*: measured on a
    /// real machine, `on_battery_only` on a laptop that is on mains 91% of the
    /// time sat at `0/12 blocks` for three days, and the status line said
    /// `waiting for idle on battery` throughout without ever saying that this
    /// had been true since the trial began. Both readings are individually
    /// correct and together they let a trial quietly never happen.
    ///
    /// Reported once per stall, and cleared when a block finally starts, so a
    /// laptop that is unplugged every evening says nothing at all.
    fn check_trial_stall(&mut self, trial: &Trial, cond: &Conditions) {
        let Some(why) = trial.blocked_by(cond) else {
            // A block is running, or nothing is in the way. Either way the next
            // stall is a new one and deserves its own line.
            self.trial_waiting_since = None;
            self.trial_stall_reported = false;
            return;
        };
        let since = *self.trial_waiting_since.get_or_insert_with(Instant::now);
        if self.trial_stall_reported || since.elapsed() < TRIAL_STALL_AFTER {
            return;
        }
        self.trial_stall_reported = true;
        tracing::warn!(
            waiting = ?TRIAL_STALL_AFTER,
            blocked_by = why.as_str(),
            done = trial.blocks().len(),
            total = trial.plan.pairs * 2,
            on_battery_only = trial.plan.on_battery_only,
            "the trial has not been able to start a block for hours and has measured nothing.              It needs an idle desktop, and by default one on battery — `dozectl trial start              --on-mains` measures a plugged-in machine instead, or `dozectl trial stop` ends it."
        );
    }

    fn drive_trial(&mut self, at: MonoTime, rates: &Rates) {
        let Some(mut trial) = self.trial.take() else { return };
        // A finished trial is kept so `dozectl report` can still show what it
        // measured (see `load_trial`), and for a while keeping it also meant
        // obeying it. `desired_mode` falls back to `baseline` when there is no
        // current arm, and a finished trial has no current arm — so one that had
        // ended went on demanding its baseline on every sample. `dozectl mode
        // live` and the dashboard's button both worked and then undid themselves
        // a few seconds later, with the log saying `switching to dry-run mode`
        // for a block that had already been measured, and it survived restarts
        // because persisting the trial across them is the entire point of the
        // file. Stopping the trial did not help: `stop` finishes it, and
        // finished was the broken state.
        //
        // Returning here also stops the same `trial.json` being rewritten every
        // sample for the life of the daemon, which is precisely the sort of
        // background work this thing exists to remove (§13).
        if trial.finished() {
            self.trial = Some(trial);
            return;
        }
        let cond = self.conditions();
        self.check_trial_stall(&trial, &cond);
        // The persisted form is `plan`, `schedule`, `at`, `baseline` and
        // `blocks`; the block in flight is deliberately not serialised. Only
        // closing a block moves any of that, and closing a block is exactly what
        // appends to `blocks` — so this count is a complete test for "is the
        // file about to say something different".
        let blocks_before = trial.blocks().len();
        trial.on_sample(at, &cond, rates);

        // A block that has just ended gives the mode back before anything else
        // looks at it. Between blocks the trial is not measuring, so the mode is
        // the user's again — and leaving it on the last arm is how a 20-minute
        // dry-run block came to hold a live daemon in dry-run for 40 hours.
        if let Some(mode) = trial.handing_back(&cond) {
            tracing::info!(
                blocks = trial.blocks().len(),
                total = trial.blocks_total(),
                "trial: block over, putting the mode back to {}",
                mode.as_str()
            );
            self.trial = Some(trial);
            self.set_mode(mode);
            trial = self.trial.take().expect("set_mode does not clear the trial");
        }

        if let Some(mode) = trial.desired_mode(&cond) {
            tracing::info!(
                arm = trial.current_arm().map_or("baseline", |a| a.as_str()),
                block = trial.blocks().len(),
                "trial: switching to {} mode",
                mode.as_str()
            );
            // Put the trial back before switching: `set_mode` dispatches, which
            // can re-enter this type, and a trial missing from `self` for the
            // duration would silently skip a block boundary.
            self.trial = Some(trial);
            self.set_mode(mode);
            trial = self.trial.take().expect("set_mode does not clear the trial");
        }

        // Reached only on the sample that finished it, since an already-finished
        // one returned above — so this still logs the verdict exactly once, and
        // hands the mode back exactly once.
        if trial.finished() {
            let verdict = trial.verdict();
            tracing::info!("trial complete\n{}", verdict.render(self.battery_wh));
            // Giving the mode back is an event at the end, not something
            // `desired_mode` asserts on every sample — see `Trial::desired_mode`
            // for what asserting it cost.
            if let Some(mode) = trial.baseline_to_restore(&cond) {
                tracing::info!(mode = mode.as_str(), "trial over; restoring the mode it found");
                self.trial = Some(trial);
                self.set_mode(mode);
                trial = self.trial.take().expect("set_mode does not clear the trial");
            }
        }

        // Only when it would differ. A trial waiting for conditions it may never
        // meet changes nothing from one sample to the next, and rewriting the
        // same bytes every 60s for the life of the daemon is the background work
        // this tool exists to remove (§13) — measured on a real machine as an
        // identical `trial.json` rewritten once a minute for three days.
        if trial.blocks().len() != blocks_before || trial.finished() {
            self.persist_trial(&trial);
        }
        self.trial = Some(trial);
    }

    /// The world, as the trial needs to see it.
    fn conditions(&self) -> Conditions {
        Conditions {
            on_ac: self.world.power.on_ac,
            desktop: self.world.desktop,
            // The same predicate the planner gates on, so a block is only ever
            // "live" when live mode could actually have been holding something.
            observing: self.world.is_observing(),
            mode: self.world.config.mode,
            // Not a reason to discard a block — a discharge reading taken while
            // the package is capped is still a good discharge reading — but it
            // is the reason a *package* verdict from this machine cannot be
            // believed. The fit is the only thing that can tell, and it can only
            // tell by watching watts refuse to move while load does.
            package_power_limited: matches!(
                self.power_model.verdict(),
                dozed_core::power_model::Verdict::PowerLimited { .. }
            ),
        }
    }

    /// Fold a window snapshot into per-app state.
    ///
    /// Visibility is decided once, in `dozed_core::visibility`, from the raw
    /// facts every source reports — so the KWin script, the GNOME extension and
    /// the X11 source cannot disagree about §5. An app's visibility is then the
    /// *maximum* over its windows, because any focused window means the user is
    /// looking at the app.
    fn on_windows(&mut self, windows: &[dozed_core::WindowFacts]) {
        let resolved_visibility = dozed_core::visibility::resolve(windows);
        // Fullscreen goes through the same tracker as the D-Bus exemptions so it
        // is deduped too. Dispatching it per app per snapshot put 2015
        // exemption events into five hours of trace, almost all of them
        // repeating "fullscreen off" — noise in the log a person has to read.
        let mut fullscreen_pids: BTreeSet<u32> = BTreeSet::new();
        let mut best: BTreeMap<AppKey, (Visibility, Vec<u32>, bool, bool)> = BTreeMap::new();
        let mut seen_apps: BTreeMap<AppKey, AppSeen> = BTreeMap::new();
        let mut candidate_cgroups: BTreeMap<AppKey, PathBuf> = BTreeMap::new();

        // Resolved once up front, because canonicalisation is a decision about
        // the snapshot as a whole: whether two windows' identities are really one
        // app cannot be answered while looking at either of them alone.
        let mut window_apps: Vec<(Resolved, Vec<u32>, Visibility, bool, bool)> = Vec::new();
        for (w, vis) in windows.iter().zip(resolved_visibility) {
            let hint = WindowHint {
                app_id: Some(w.app_id.clone()),
                desktop_file: Some(w.desktop_file.clone()),
            };
            // Walked once here: identity needs it to decide the class and to
            // check scope exclusivity, and the aggregation below needs it too.
            let pids = collect_tree_pids(w.pid);
            let Some(resolved) = identity::resolve(w.pid, &hint, &self.own_pids, &pids) else {
                continue;
            };
            window_apps.push((resolved, pids, vis, w.fullscreen, w.attention));
        }

        let rewrites = identity::canonical_keys(
            &window_apps
                .iter()
                .map(|(r, pids, ..)| identity::Claim {
                    key: r.key.clone(),
                    exe: r.evidence.exe_path.clone(),
                    tree: pids.iter().copied().collect(),
                })
                .collect::<Vec<_>>(),
        );

        for (resolved, pids, vis, fullscreen, attention) in window_apps {
            // A folded-away identity keeps none of its own state: it is the same
            // app, so it shares the grace clock and the decision.
            let key = rewrites.get(&resolved.key).unwrap_or(&resolved.key).clone();
            let is_canonical = key == resolved.key;

            let entry =
                best.entry(key.clone()).or_insert((Visibility::Hidden, Vec::new(), false, false));
            // Ord on Visibility runs Focused < VisibleUnfocused < Hidden, so the
            // most-visible window is the minimum.
            if vis < entry.0 {
                entry.0 = vis;
            }
            entry.1.extend(pids);
            entry.2 |= fullscreen;
            entry.3 |= attention;

            // Only the canonical window's own scope may name the app; a folded
            // identity's scope is by definition not exclusive to it.
            if let Some(cg) = &resolved.cgroup
                && is_canonical
            {
                candidate_cgroups.insert(key.clone(), cg.clone());
            }

            match seen_apps.get_mut(&key) {
                // Evidence has to describe the app under the key it is filed
                // under, so the canonical window's account wins even if a folded
                // one got here first. Class and protection only ever escalate:
                // §3 says fail towards "never touch this".
                Some(seen) => {
                    if is_canonical {
                        seen.evidence = resolved.evidence;
                        seen.cgroup = resolved.cgroup.map(|p| p.to_string_lossy().into_owned());
                    }
                    seen.protected |= resolved.protected;
                    seen.class = more_protective(seen.class, resolved.class);
                }
                None => {
                    seen_apps.insert(
                        key.clone(),
                        AppSeen {
                            key,
                            evidence: resolved.evidence,
                            class: resolved.class,
                            pids: Vec::new(),
                            cgroup: resolved.cgroup.map(|p| p.to_string_lossy().into_owned()),
                            protected: resolved.protected,
                        },
                    );
                }
            }
        }

        // Apps that no longer have any window have gone.
        let vanished: Vec<AppKey> =
            self.world.apps.keys().filter(|k| !best.contains_key(*k)).cloned().collect();
        for key in vanished {
            self.cgroups.remove(&key);
            self.pids.remove(&key);
            self.sample_pids.remove(&key);
            self.sampler.forget(&key);
            self.dispatch(Event::AppGone { app: key });
        }

        // Which PIDs belong to some *other* app, so a cgroup holding one of them
        // can be recognised as shared. Computed once over the whole snapshot,
        // because the question cannot be answered from one app's row alone —
        // the same reason canonicalisation runs up front.
        let claimed: BTreeMap<AppKey, BTreeSet<u32>> =
            best.iter().map(|(k, (_, p, ..))| (k.clone(), p.iter().copied().collect())).collect();

        for (key, (vis, mut pids, fullscreen, attention)) in best {
            pids.sort_unstable();
            pids.dedup();

            // What this app is *measured* on, as distinct from what it is acted
            // on. A process tree is not exclusive: an app launched from a
            // terminal is inside the terminal's tree, so summing the tree
            // credited the terminal with everything started from it —
            // `org.wezfurlong.wezterm` reported 3137 wakeups/s while
            // `wezterm-gui` itself was doing 70, the rest being 68 unrelated
            // processes. An app that has a key of its own was also counted
            // twice, once under itself and once under whatever launched it.
            //
            // Each PID is measured against the app with the smallest tree
            // containing it — the most specific claim available, and the same
            // rule `crate::exemptions` already applies to attribution.
            //
            // Deliberately *not* used for actuation: freezing has to reach the
            // whole tree or it leaves processes running (§4), so `self.pids`
            // stays the full set and only measurement narrows.
            let mine: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| {
                    claimed
                        .iter()
                        .filter(|(_, tree)| tree.contains(pid))
                        .min_by_key(|(k, tree)| (tree.len(), (*k).clone()))
                        .is_none_or(|(owner, _)| *owner == key)
                })
                .collect();
            self.sample_pids.insert(key.clone(), mine);

            // The cgroups usable as this app's unit are the ones its own
            // processes live in — plural, because an app is not reliably one
            // cgroup: Brave keeps 32 processes in `app-brave@….service` and its
            // main process in a sibling `.scope`, Firefox takes one unit per
            // instance (`DESIGN.md` §4). Taking only the window PID's cgroup
            // covered 2 of Brave's 34 processes, so rung 1 was refused and the
            // app dropped to the signal path, which cannot throttle at all.
            let owned: BTreeSet<u32> = pids.iter().copied().collect();
            let foreign: BTreeSet<u32> =
                claimed.iter().filter(|(k, _)| **k != key).flat_map(|(_, p)| p).copied().collect();
            let candidates = Self::cgroups_of(&pids);
            if dozed_linux::is_the_unit_for_app(&candidates, &owned, &foreign) {
                self.cgroups.insert(key.clone(), candidates);
            } else if self.cgroups.remove(&key).is_some() {
                tracing::debug!(app = %key, cgroups = candidates.len(),
                    "cgroups do not hold exactly this app; falling back to the process tree");
            }

            // Only dispatch what actually changed.
            //
            // The compositor sends complete snapshots, so most of every one of
            // them repeats what we already know — 85% of the events in a
            // five-hour trace changed nothing, which is most of a log a person
            // is supposed to read critically. `WorldState::apply` already
            // ignores a repeated visibility (it does not restart the grace
            // clock), so suppressing these costs no behaviour; grace still
            // expires on the tick, which tightens to 2s as a deadline nears.
            //
            // The bookkeeping below is *not* conditional: `self.pids` feeds
            // sampling and exemption resolution and must track every snapshot.
            let pids_changed = self.pids.get(&key) != Some(&pids);
            if let Some(mut seen) = seen_apps.remove(&key) {
                self.note_weak_identity(&key, &seen);
                seen.pids.clone_from(&pids);
                self.pids.insert(key.clone(), pids);
                if pids_changed {
                    self.dispatch(Event::AppSeen(Box::new(seen)));
                }
            }
            if self.world.app(&key).is_none_or(|a| a.visibility != vis) {
                self.dispatch(Event::VisibilityChanged { app: key.clone(), visibility: vis });
            }
            if fullscreen {
                fullscreen_pids.extend(self.pids.get(&key).into_iter().flatten().copied());
            }
            if attention {
                // The nearest thing KWin gives us to "the user asked for this
                // window back"; see helpers/kwin/README.md.
                self.dispatch(Event::ActivationRequested { app: key });
            }
        }

        for event in self.exemptions.snapshot(Exemption::Fullscreen, fullscreen_pids, &self.pids) {
            self.dispatch(event);
        }

        // The PID map just changed, so re-resolve every exemption against it. A
        // media player is normally already playing by the time the compositor
        // reports its window, which means the audio snapshot naming its PID
        // arrived before we knew which app that PID belonged to.
        for event in self.exemptions.reresolve(&self.pids) {
            self.dispatch(event);
        }
    }

    /// Work out which apps might be *inside* a screen share, given who is doing
    /// the sharing.
    ///
    /// `DESIGN.md` §10.8 records the gap this closes and why it cannot be closed
    /// properly: the portal names the client that asked for a session, and a
    /// `Start` reply names PipeWire nodes, so there is no route from what is
    /// being captured back to a window or a PID. Sharing one window and then
    /// working in another therefore left the shared window hidden, idle-looking
    /// and freezable — and freezing it does not stop the share, it freezes the
    /// picture, which only the people on the other end can see.
    ///
    /// The one fact available is **what was on screen when the session opened**.
    /// Whatever is being shared was, in the ordinary case, among it. So that set
    /// is captured at the transition and held for as long as any session is
    /// open.
    ///
    /// Three properties, each of which is a way this could have gone wrong:
    ///
    /// * **Captured at the transition, not recomputed.** By the time it matters
    ///   the shared window is hidden — that *is* the hazard — so a set derived
    ///   from what is visible now would be empty exactly when it is needed.
    /// * **It grows, never shrinks, while sharing continues.** A second session
    ///   opening is a second thing being shared, and the apps on screen at that
    ///   moment join the set. Recomputing from scratch would drop the first
    ///   share's window, which is still being watched.
    /// * **It ends with the last session.** Bounded by something observable
    ///   rather than by a timer, so a long call keeps its protection and a
    ///   cancelled picker costs nothing beyond the moment it was open.
    ///
    /// Returns the events its own snapshot produces, exactly like every other
    /// source, so this cannot become a second way for an exemption to reach the
    /// world.
    fn resolve_shared_windows(&mut self, sharing: &BTreeSet<u32>) -> Vec<Event> {
        if sharing.is_empty() {
            // Nothing is being shared, so nothing is in a share. Publishing the
            // empty set rather than returning early: this is a complete snapshot
            // like every other exemption source, and a source that goes quiet
            // instead of saying "none" leaves its last word standing forever.
            self.shared_candidates.clear();
        } else {
            // Every app on screen right now. `is_on_screen` rather than
            // `Focused`, because a share is routinely picked from a window
            // sitting beside the one being typed in — on a second monitor, or
            // tiled next to the call.
            let on_screen: Vec<AppKey> = self
                .world
                .apps
                .iter()
                .filter(|(_, a)| a.visibility.is_on_screen())
                .map(|(k, _)| k.clone())
                .collect();
            for key in on_screen {
                if let Some(pids) = self.pids.get(&key) {
                    self.shared_candidates.extend(pids.iter().copied());
                }
            }
        }
        let pids = self.shared_candidates.clone();
        self.exemptions.snapshot(Exemption::MaybeShared, pids, &self.pids)
    }

    fn sample_all(&mut self, now: Instant) {
        // Recorded whichever path got here, because `resample_due` measures from
        // the last sample and not from the last tick.
        self.last_sample_at = now;
        let apps: Vec<(AppKey, Vec<u32>)> =
            self.pids.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        // Read per call rather than cached, so a `dozectl reload` that widens the
        // window takes effect on the next tick.
        let window = self.world.config.defaults.cpu_sample_window.get();
        // Every PID any app is measured on, so departed processes can be
        // dropped once at the end rather than once per app — see
        // `Sampler::retain_pids`.
        let mut live: BTreeSet<u32> = BTreeSet::new();
        for (key, pids) in apps {
            let cgroups = self.cgroups.get(&key).cloned().unwrap_or_default();
            // Measured on what this app exclusively owns; see `sample_pids`.
            let pids = self.sample_pids.get(&key).cloned().unwrap_or(pids);
            live.extend(pids.iter().copied());
            if let Some(sample) = self.sampler.sample(&key, &pids, &cgroups, now, window) {
                // Whether this sample can serve as the "what it costs to leave
                // this app alone" baseline. It can only do so once the app has
                // been hidden for a whole sample window: any sooner and the
                // window still contains the user working in it, which is a
                // foreground rate and would inflate every saving claimed against
                // it (measured: a Signal sustaining 3.5% hidden, credited against
                // 5.4% captured a minute after it lost focus).
                let now = self.now();
                let settled = self.world.app(&key).is_some_and(|a| {
                    a.visibility == Visibility::Hidden && now.since(a.visibility_since) >= window
                });
                self.accumulator.on_sample(
                    &key,
                    sample.cpu_pct,
                    sample.wakeups_per_sec,
                    now,
                    settled,
                    window,
                );
                self.dispatch(Event::Sampled { app: key, sample });
            }
        }
        self.sampler.retain_pids(&live);

        // Close any savings episode whose hold has already ended.
        //
        // `ReportAccumulator` learns about holds from the intent stream, and that
        // stream does not reliably close what it opens: an app that exits while
        // frozen is never restored, and a restore that comes back `Skipped` is
        // dropped before the accumulator sees it. The episode then stays open for
        // the rest of the run, accruing hold time the app never spent held and
        // charging everything it does afterwards as leakage — which is how one
        // app on this machine came to show a single nine-day episode leaking
        // 2,352 wakeups/s while it was plainly running and exempt.
        //
        // The actuator knows what it is holding, so ask it rather than infer.
        // Once a tick, over a handful of apps, inside a loop that has just read
        // `/proc` for every one of them — §13's budget does not notice this.
        let held: BTreeSet<AppKey> = self
            .live
            .as_ref()
            .map(|a| a.held().map(|(k, ..)| k.clone()).collect())
            .unwrap_or_default();
        for key in self.accumulator.reconcile_held(&held, self.now()) {
            // Loud, because it means the report has been wrong until now and
            // because every path that leaves an episode open is a path that
            // forgot to say it had let go.
            tracing::warn!(
                app = %key.label(),
                "closed a savings episode whose hold had already ended; until now the report \
                 counted this app as still held"
            );
        }
    }

    /// Fold one event into the world, then act on whatever the planner says.
    ///
    /// Also the one place a session is recorded, in causal order: the event, the
    /// derivations it changed, then the intents those produced. Recording here
    /// rather than at each source is what guarantees the trace contains exactly
    /// what the planner was fed, which is the property replay depends on.
    fn dispatch(&mut self, event: Event) {
        let now = self.now();
        // The counters keep running across a suspend and the wall clock does not,
        // so a rate taken over the gap is fiction — a machine asleep for eight
        // hours would report eight hours of perfect deep idle it never earned.
        if matches!(event, Event::PrepareForSleep { .. }) {
            self.energy.reset();
            // The machine on the other side of a suspend may not be the one those
            // coefficients describe — a screen that went off, a dock that came
            // away, a thermal limit that disengaged.
            self.power_model.reset();
        }
        // A sample that says the same thing as the last one recorded for that app
        // is not recorded again.
        //
        // §14 already applies this to derivations — "edge-triggered on the
        // derivation, never on the tick", because "recording those would make the
        // log a firehose" — and then recorded the raw samples on every tick for
        // every app anyway. Measured on a real 67 MB recording, that is what a
        // trace mostly is: **93% of its records and 78% of its bytes** were
        // `sampled` events, and it filled a 64 MB cap in 3.6 days against a
        // section claiming a working day costs a few megabytes.
        //
        // Suppressing the unchanged ones is lossless rather than a trade, which
        // is the only reason it is allowed:
        //
        // * The world keeps the last sample it was given, so a replay that never
        //   sees the repeat decides against the identical state.
        // * `ReportAccumulator::on_sample` weights by `dt` since the previous
        //   sample, not by how many arrived, so a longer gap at the same rate
        //   accumulates the same total. `dozectl suggest` scores from that, and
        //   its numbers do not move.
        //
        // It only pays because rates are quantised at source (`sampler`): at full
        // `f32` precision 4.8% of consecutive samples repeat, and at 0.1 it is
        // **40%**. The seq is not consumed for a record that is not written, so
        // the numbering still counts what is actually in the file.
        if !self.sample_is_a_repeat(&event) {
            let seq = self.trace.next_seq();
            self.trace.write(&TraceRecord::Event { seq, at: now, event: Box::new(event.clone()) });
        }

        self.world.apply(&event, now);
        let intents = self.planner.plan(&self.world, &event, now);

        // Most derivations produce no intent — an app waiting out grace, a rule
        // that quietly stopped matching — and those are the ones the journal
        // cannot show and the user most needs to see.
        for (app, decision) in self.planner.take_decisions() {
            let seq = self.trace.next_seq();
            self.trace.write(&TraceRecord::Decision {
                seq,
                at: now,
                app,
                decision: Box::new(decision),
            });
        }

        for intent in intents {
            self.execute(&intent);
        }
    }

    fn execute(&mut self, intent: &Intent) {
        let mode = self.world.config.mode_for(&intent.app);
        // Per-app `monitor` keeps one app in dry-run while the rest run live.
        let outcome = if self.live.is_some() && mode.acts() {
            self.apply_live(intent)
        } else {
            self.dry.apply(intent)
        };

        // Before anything else records this: the planner has to hear whether
        // its intent landed, or it goes on believing a hold that never took
        // (`Planner::note_outcome`).
        self.planner.note_outcome(&intent.app, intent.action, &outcome);

        self.accumulator.on_intent(&intent.app, intent.action, &outcome, intent.at);

        let seq = self.trace.next_seq();
        self.trace.write(&TraceRecord::Intent {
            seq,
            at: intent.at,
            intent: Box::new(intent.clone()),
            outcome: outcome.clone(),
        });

        let wall =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let entry = JournalEntry::new(intent, outcome, wall);
        tracing::info!("{}", entry.line());
        self.journal.append(&entry);
        let _ = self.broadcast.send(entry.clone());
        self.entries.push(entry);
        // Keep the in-memory backlog bounded; the file on disk is the archive.
        if self.entries.len() > 10_000 {
            self.entries.drain(..5_000);
        }
    }

    /// Whether this event is a sample identical to the last one recorded.
    ///
    /// Keyed on what was last *written to the trace* rather than last sampled, so
    /// a run of identical readings collapses to the first of them rather than to
    /// every other one.
    fn sample_is_a_repeat(&mut self, event: &Event) -> bool {
        let Event::Sampled { app, sample } = event else { return false };
        if self.last_traced_sample.get(app) == Some(sample) {
            return true;
        }
        self.last_traced_sample.insert(app.clone(), *sample);
        false
    }

    /// Say so, once, when an app is known only by its executable or its comm.
    ///
    /// Those are the last two rungs of §4: the compositor reported no `app_id`
    /// and no desktop file, nothing was packaged, and no exclusive scope named
    /// it. The app still gets managed correctly — but the name it is filed under
    /// is one dozed derived, so a rule written against the name the user expects
    /// will silently not match, which is the failure mode a dead `signal-desktop`
    /// pattern already demonstrated once. Seen in a real session: LibreOffice ran
    /// for three and a half minutes known only as
    /// `exe:/usr/lib/libreoffice/program/oosplash`.
    ///
    /// Once per key per run: this is advice, and advice repeated every snapshot is
    /// noise. `info` rather than `warn` because nothing is wrong — a transient
    /// splash screen would otherwise cry wolf on every launch.
    fn note_weak_identity(&mut self, key: &AppKey, seen: &AppSeen) {
        if key.kind.is_declared() || key.kind == dozed_core::IdentityKind::AppId {
            return;
        }
        if !self.noted_identities.insert(key.clone()) {
            return;
        }
        tracing::info!(
            app = %key,
            saw = %seen.evidence.describe(),
            "no declared identity: the compositor reported no app_id and no desktop file, \
             so this app is known by its {}. A rule can match it as `app = \"{}\"`.",
            key.kind.as_str(),
            key.label(),
        );
    }

    /// How this app can be acted on, in the order §4 prefers.
    ///
    /// A cgroup is only usable when it holds this app and nothing else, which
    /// `on_windows` has already checked. Without one — an app launched from a
    /// terminal shares the terminal's scope — the fallback is the process trees,
    /// which is racy but is the only unit available (`dozed_linux::tree`).
    ///
    /// *Trees*, plural, and derived from parentage rather than from position.
    /// This used to take `pids.first()`, and since `on_windows` sorts the PIDs
    /// numerically that meant the lowest number, which is the tree root only by
    /// coincidence. Firefox on this machine was 44 processes in three independent
    /// trees; after a PID wraparound the lowest number belonged to a stray
    /// `Web Content` leaf with no children, so the freeze reached exactly one
    /// process and the other 43 ran for seven and a half hours while `dozectl`
    /// reported the app as dozing.
    /// The distinct cgroups an app's processes are spread across, in a stable
    /// order so the set does not churn between snapshots.
    fn cgroups_of(pids: &[u32]) -> Vec<PathBuf> {
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        for pid in pids {
            if let Some(rel) = dozed_linux::cgroup_path_of_pid(*pid)
                // Only real app units. Without this a stray process in
                // `user.slice` itself would drag the whole session in as a
                // "cgroup of the app".
                && dozed_linux::app_unit_of(&rel).is_some()
            {
                seen.insert(
                    PathBuf::from(dozed_linux::cgroup::DEFAULT_ROOT)
                        .join(rel.trim_start_matches('/')),
                );
            }
        }
        seen.into_iter().collect()
    }

    fn unit_for(&self, key: &AppKey) -> Option<Unit> {
        if let Some(cgroups) = self.cgroups.get(key).filter(|c| !c.is_empty()) {
            return Some(Unit::Cgroups(cgroups.clone()));
        }
        let pids = self.pids.get(key)?;
        let roots = dozed_linux::tree::roots_of(pids);
        // Every PID we know is either a root or reachable from one, so an empty
        // result means the parentage was unreadable. Fall back to treating each
        // PID as its own root: over-broad in what it walks, never under-broad.
        let roots = if roots.is_empty() { pids.clone() } else { roots };
        if roots.is_empty() {
            return None;
        }
        Some(Unit::Trees { roots })
    }

    fn apply_live(&mut self, intent: &Intent) -> Outcome {
        let Some(unit) = self.unit_for(&intent.app) else {
            return Outcome::Skipped { why: format!("no processes known for {}", intent.app) };
        };
        let target = Target { key: intent.app.clone(), unit };
        // Record the intent to freeze *before* performing it: if we are killed
        // between the write and the record, the app is stranded frozen.
        if !intent.action.is_restore() {
            self.persist_frozen_with(&target.key, &target.unit);
        }
        let outcome = self.live.as_mut().map_or(Outcome::Skipped { why: "not live".into() }, |a| {
            a.apply_to(&target, intent.action)
        });
        if intent.action.is_restore() {
            self.persist_frozen();
        }
        outcome
    }

    fn on_command(&mut self, cmd: Command) {
        let Command { requests, reply } = cmd;
        // Answered as one batch, so a client that asked three questions in one
        // packet is woken once (`ipcserver::Command`).
        let mut out = Vec::new();
        for request in requests {
            out.extend(crate::ipcserver::handle(self, request));
        }
        // Here as well as on the tick, because a command is exactly the moment
        // the tray is wrong: `dozectl snooze` and a reload that turns `ui.tray`
        // off would otherwise wait out the idle interval, which is up to a
        // minute of an icon showing the state before the thing the user just
        // did. Unchanged state still costs nothing — `publish_tray` compares
        // before it sends.
        self.publish_tray();
        let _ = reply.send(out);
    }

    // ---- state the IPC layer needs -------------------------------------

    pub fn world(&self) -> &WorldState {
        &self.world
    }
    pub fn planner(&self) -> &Planner {
        &self.planner
    }
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    /// Entries from previous runs. Empty unless a journal file was found.
    pub fn backlog(&self) -> &[JournalEntry] {
        &self.backlog
    }
    /// The processes this app is measured on, busiest first.
    pub fn top_processes(&self, key: &AppKey, limit: usize) -> Vec<dozed_core::ProcContrib> {
        self.sample_pids
            .get(key)
            .map(|pids| self.sampler.top_processes(pids, limit))
            .unwrap_or_default()
    }

    /// Whether this app's sample is a measurement rather than a placeholder.
    /// See `ipc::AppSummary::sampled`.
    pub fn has_sample(&self, key: &AppKey) -> bool {
        self.sampler.has_sample(key)
    }

    pub fn cgroup_of(&self, key: &AppKey) -> Option<&[PathBuf]> {
        self.cgroups.get(key).map(Vec::as_slice)
    }
    pub fn pids_of(&self, key: &AppKey) -> Option<&Vec<u32>> {
        self.pids.get(key)
    }
    /// What this app calls the audio streams it holds open — for a browser, the
    /// tab titles behind an `audio` or `audio-silent` report.
    pub fn audio_streams(&self, key: &AppKey) -> Vec<dozed_core::ipc::AudioStream> {
        self.exemptions.titles_for(key, &self.pids)
    }
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
    pub fn mono_now(&self) -> MonoTime {
        self.now()
    }
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }
    /// The mode actually in force.
    ///
    /// Without an actuator there is no live mode to be in, whatever the config
    /// says — `--dry-run` on the command line wins, and the shipped unit passes
    /// it. Reporting the configured mode instead put `mode live` on the status of
    /// a daemon that could not touch anything.
    pub fn effective_mode(&self) -> Mode {
        if self.is_live() { self.world.config.mode } else { Mode::DryRun }
    }
    /// The configured mode, when it is not the one in force.
    /// Whether a trial block is running and dictating the mode.
    ///
    /// Deliberately "a block is running" rather than "a trial exists": since
    /// `Trial::desired_mode` was narrowed to running blocks, a trial waiting for
    /// its conditions has no claim on the mode and saying it did would be the
    /// same false attribution in the other direction.
    /// Why the frozen-set file could not be written, if it could not.
    pub fn frozen_file_error(&self) -> Option<&str> {
        self.frozen_unwritable.as_deref()
    }

    /// The mode an unfinished trial will restore between its blocks.
    ///
    /// `None` when no trial is running, so the status line says nothing in the
    /// ordinary case.
    pub fn trial_restores(&self) -> Option<Mode> {
        self.trial.as_ref().filter(|t| !t.finished()).map(dozed_core::trial::Trial::baseline)
    }

    pub fn mode_driven_by_trial(&self) -> bool {
        self.trial.as_ref().is_some_and(|t| !t.finished() && t.current_arm().is_some())
    }
    pub fn overridden_mode(&self) -> Option<Mode> {
        let configured = self.world.config.mode;
        (configured != self.effective_mode()).then_some(configured)
    }
    pub fn helper_healthy(&self) -> bool {
        self.helper.healthy
    }
    pub fn compositor(&self) -> Option<&str> {
        self.helper.compositor.as_deref()
    }
    pub fn self_wakeups_per_sec(&self) -> f32 {
        self.self_wakeups
    }
    /// D-Bus property reads a second, or `None` when there is no system bus.
    pub fn bus_calls_per_sec(&self) -> Option<f32> {
        self.bus_calls.as_ref().map(|_| self.bus_calls_per_sec)
    }
    pub fn config_path(&self) -> Option<&PathBuf> {
        self.config_path.as_ref()
    }
    pub fn held_count(&self) -> usize {
        self.planner.held().count()
    }
    /// Whether each exemption source is actually connected (`DESIGN.md` §10).
    pub fn exemption_sources(&self) -> Vec<dozed_core::ipc::ExemptionSource> {
        // Keyed on the source we actually run, not on the environment: a daemon
        // started with --focus-source=x11 from a Wayland login shell still has
        // WAYLAND_DISPLAY set, and would otherwise disclaim an exemption it is
        // providing.
        let x11 =
            self.session.as_ref().is_some_and(|s| s.kind == crate::session::FocusSourceKind::X11);
        self.exemptions.status(x11)
    }
    /// Where the session is being recorded, and what it has cost so far. A
    /// recorder whose own cost is invisible is asking to be taken on faith.
    pub fn trace_status(&self) -> Option<dozed_core::ipc::TraceStatus> {
        // `was_started`, not `is_recording`: a run whose recorder broke has
        // something to say, and saying nothing would render as `--no-trace`.
        self.trace.was_started().then(|| dozed_core::ipc::TraceStatus {
            path: self.trace.path().to_string_lossy().into_owned(),
            bytes: self.trace.bytes(),
            records: self.trace.records(),
            part: self.trace.part(),
            stopped: self.trace.stopped().map(str::to_owned),
            gaps: self.trace.gaps(),
            lost: self.trace.lost(),
            retrying: self.trace.retrying(),
        })
    }
    pub fn report(&self) -> dozed_core::Report {
        // Counterfactual whenever nothing could actually have been done, which is
        // *either* no live actuator or a mode that does not act. Keyed on the
        // startup flag alone, a daemon started `--live` but switched to dry-run
        // reported hours of "has dozed" for holds it had released.
        let counterfactual = !self.is_live() || !self.world.config.mode.acts();
        let mut r = self.accumulator.report(self.now(), counterfactual);
        r.battery_wh = self.battery_wh;
        r.on_ac = self.world.power.on_ac;
        // Only once there is something to say: an empty verdict on every report
        // would read as "measured, and it found nothing".
        r.measured = self
            .trial
            .as_ref()
            .map(Trial::verdict)
            .filter(|v| v.discharge_pairs > 0 || v.idle_pairs > 0);
        // The coefficients this machine gave up, where it gave up any. Without
        // them §11's saving falls back to 2-4 J a CPU-second and cannot price a
        // wakeup at all — which is the objection §11.1 opens with, and the whole
        // reason the model exists.
        r.power_fit = self.power_model.verdict().fit();
        r
    }

    pub fn battery_wh(&self) -> Option<f64> {
        self.battery_wh
    }

    /// What the thermal sensors say, for `dozectl status`. `None` when the
    /// machine exposes no hwmon at all.
    pub fn thermal_status(&self) -> Option<dozed_core::ipc::ThermalStatus> {
        self.thermal.available().then(|| {
            let reading = self.thermal.last_reading();
            dozed_core::ipc::ThermalStatus {
                state: self.thermal.state(),
                hottest_celsius: reading.hottest_celsius,
                fastest_rpm: reading.fastest_rpm,
                saturated: self.thermal.saturated(),
                fan: Some(dozed_core::ipc::FanStatus {
                    observed: self.thermal.fan_range().observed(),
                    thresholds: self
                        .thermal
                        .effective()
                        .hot_rpm
                        .zip(self.thermal.effective().cool_rpm),
                    pinned: self.world.config.thermal.hot_rpm.is_some(),
                }),
                ignored: self.thermal.ignored_sensors().to_vec(),
            }
        })
    }

    /// Resolve a user-typed name to a known app.
    ///
    /// Exact key, then the config-file pattern rules, then — only here — a
    /// substring. The substring rung is deliberately *not* part of
    /// [`AppKey::matches_pattern`]: a rule matching too widely wedges a desktop,
    /// whereas a person typing `dozectl why plasmashell` at an app keyed
    /// `desktop:org.kde.plasmashell` just wants an answer. Ambiguity resolves to
    /// nothing rather than to an arbitrary app, so a vague query is a question
    /// rather than a wrong answer.
    pub fn find_app(&self, pattern: &str) -> Option<AppKey> {
        if let Some(k) = self.world.apps.keys().find(|k| k.to_string() == pattern) {
            return Some(k.clone());
        }
        if let Some(k) = self.world.apps.keys().find(|k| k.matches_pattern(pattern)) {
            return Some(k.clone());
        }
        let needle = pattern.to_ascii_lowercase();
        let mut hits =
            self.world.apps.keys().filter(|k| k.value.to_ascii_lowercase().contains(&needle));
        match (hits.next(), hits.next()) {
            (Some(only), None) => Some(only.clone()),
            _ => None,
        }
    }

    pub fn user_event(&mut self, event: Event) {
        self.dispatch(event);
    }

    /// Switch the global mode, and make reality match the new one.
    ///
    /// This used to be one assignment, and it was wrong in both directions.
    ///
    /// **Going live froze nothing.** A dry-run outcome advances the planner's
    /// state machine exactly as a live one does — deliberately, so the
    /// counterfactual is faithful — so after an hour of dry-run the planner
    /// believes the hidden apps are frozen and suppresses the intents that would
    /// freeze them. The actuator changed underneath and nothing was asked of it.
    /// [`Planner::reassert`](dozed_core::Planner) is the other half of this fix.
    ///
    /// **Going back to dry-run left everything frozen.** Intents are routed to the
    /// dry-run recorder from that moment on, so the real holds the live actuator
    /// was carrying had nothing left that could ever release them. `dozectl mode
    /// dry-run` read as "stop touching my apps" and did the opposite.
    ///
    /// Both are now handled here, and the trial drives the mode through this same
    /// path precisely so it cannot diverge from what a user can do by hand.
    pub fn set_mode(&mut self, mode: Mode) {
        let before = self.world.config.mode;
        if before == mode {
            return;
        }
        if before.acts() && !mode.acts() {
            // Release before switching: after the dispatch below, every intent
            // goes to the dry-run recorder and this actuator would hold its
            // freezes for the life of the daemon.
            if let Some(live) = &mut self.live {
                for (key, outcome) in live.restore_all() {
                    tracing::info!(app = %key, ?outcome, "restored on leaving live mode");
                }
            }
            self.persist_frozen();
        }
        self.dispatch(Event::ModeChanged { mode });
    }

    /// Limit how far up the ladder one app may be taken, or clear the limit.
    ///
    /// `ceiling: None` clears. A `duration` of `None` alongside a ceiling means
    /// "until cleared", which is offered because "leave it alone while I am on
    /// this call" has no honest number attached — see `ceiling::Hold::until`.
    pub fn set_hold(
        &mut self,
        key: &AppKey,
        ceiling: Option<dozed_core::Ceiling>,
        duration: Option<Duration>,
    ) {
        let now = self.mono_now();
        match ceiling {
            Some(ceiling) => {
                let until = duration.map(|d| now.plus(d));
                tracing::info!(
                    app = %key,
                    ceiling = ceiling.as_str(),
                    ?duration,
                    "limiting how far this app may be taken"
                );
                self.planner.set_hold(key, ceiling, until, now);
            }
            None => {
                if self.planner.clear_hold(key) {
                    tracing::info!(app = %key, "limit cleared; the rules have it back");
                }
            }
        }
        self.persist_holds();
        // Re-plan now rather than at the next tick: a ceiling that releases a
        // frozen app should not wait out a sample interval to do it, and one
        // that lapses should not leave the app un-acted-on for a minute.
        self.dispatch(Event::Tick);
    }

    /// Write the ceilings out, so a restart does not quietly revoke them.
    fn persist_holds(&self) {
        self.persist_holds_at(&crate::paths::holds_file());
    }

    /// [`Daemon::persist_holds`], against a stated path.
    ///
    /// Split out for the tests, which cannot move `XDG_STATE_HOME`: this crate
    /// forbids `unsafe`, and `set_var` is unsafe in this edition for the good
    /// reason that the environment is process-wide while tests are not.
    fn persist_holds_at(&self, path: &std::path::Path) {
        let now = self.mono_now();
        let records: Vec<HoldRecord> = self
            .planner
            .holds(now)
            .map(|(key, hold)| HoldRecord {
                app: key.clone(),
                ceiling: hold.ceiling,
                until_wall: hold.remaining(now).map(|left| wall_now() + left.as_secs()),
            })
            .collect();
        if records.is_empty() {
            // Rather than an empty array, so the ordinary case leaves no file at
            // all and a stale one cannot outlive the last ceiling.
            let _ = std::fs::remove_file(path);
            return;
        }
        let Ok(json) = serde_json::to_string(&records) else { return };
        if let Err(e) = crate::paths::write_state_file(path, json.as_bytes()) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not record the per-app limits; a restart will forget them"
            );
        }
    }

    /// Put back the ceilings a previous run was holding.
    fn load_holds(&mut self) {
        self.load_holds_at(&crate::paths::holds_file());
    }

    fn load_holds_at(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(records) = serde_json::from_str::<Vec<HoldRecord>>(&text) else {
            tracing::warn!(path = %path.display(), "unreadable limits file; ignoring it");
            let _ = std::fs::remove_file(path);
            return;
        };
        let now = self.mono_now();
        let wall = wall_now();
        let mut restored = 0usize;
        for r in records {
            // A deadline that has already passed is not a ceiling, it is
            // history. Dropped rather than restored-then-expired, so nothing
            // acts on it for the one tick in between.
            let until = match r.until_wall {
                Some(at) if at <= wall => continue,
                Some(at) => Some(now.plus(Duration::from_secs(at - wall))),
                None => None,
            };
            self.planner.set_hold(&r.app, r.ceiling, until, now);
            restored += 1;
        }
        if restored > 0 {
            tracing::info!(count = restored, "restored per-app limits from a previous run");
        }
        // Rewrite immediately: anything that lapsed while we were not running
        // has just been dropped, and leaving it on disk would have it read again
        // at every start.
        self.persist_holds_at(path);
    }

    /// A mode change a person asked for, as opposed to one the trial's schedule
    /// is driving.
    ///
    /// Refused while a trial is running, for exactly the reason
    /// [`snooze`](Self::snooze) is refused, and it is the same sentence: a trial
    /// *is* a schedule of mode changes. `snooze` got that guard and this path
    /// never did, so the two disagreed about who owns the mode.
    ///
    /// What that cost is worse than losing the change, because the change did
    /// not visibly lose. [`drive_trial`](Self::drive_trial) re-asserts
    /// `desired_mode` on the next sample, so a mode set by hand held for a few
    /// seconds and then reverted with nothing anywhere saying why — a button
    /// that appeared to work, then appeared to be broken. Worse, the sample
    /// taken in between was attributed to a block the schedule had not put in
    /// that mode, so a hand-set live minute landed inside a dry-run block and
    /// was measured as one.
    ///
    /// The refusal names `dozectl trial stop`, because "you cannot do this" is
    /// only half an answer when the other half is one command away.
    ///
    /// Narrowed to a trial that is actually *measuring*, which is the other half
    /// of the fix in [`Trial::desired_mode`](dozed_core::trial::Trial::desired_mode).
    /// Refusing whenever a trial merely existed was defensible while a waiting
    /// trial still asserted its baseline — the two were at least consistent. Now
    /// that a waiting trial has no claim on the mode, refusing on its behalf
    /// would leave the mode owned by nobody and changeable by no one: a machine
    /// stuck in whatever mode it happened to be in, told to stop a trial that
    /// was not doing anything. A block that is running still owns the mode, and
    /// still says so.
    pub fn set_mode_by_hand(&mut self, mode: Mode) -> Result<(), String> {
        if self.mode_driven_by_trial() {
            // Short on purpose: this lands in the dashboard's footer strip,
            // which is one row shared with the connection state and the wakeup
            // cost. The reasoning lives in this function's doc comment and in
            // the dashboard's hover text; what an error line owes the reader is
            // the way out, and "finished blocks are kept" is the half of that
            // which decides whether they dare take it.
            return Err(
                "a trial is driving the mode; `dozectl trial stop` first — finished blocks \
                 are kept"
                    .into(),
            );
        }
        // An explicit mode change wins over a snooze, and drops it rather than
        // ending it: `wake` would put back the mode the snooze interrupted,
        // undoing the choice being made right here.
        if self.cancel_snooze() {
            tracing::info!("snooze cancelled by an explicit mode change");
        }
        self.set_mode(mode);
        Ok(())
    }

    // ---- snooze ---------------------------------------------------------

    /// Stop acting for `how_long`, then go back to what we were doing.
    ///
    /// Refused while a trial is running, and the refusal is the point rather
    /// than an inconvenience: a trial *is* a schedule of mode changes, and a
    /// snooze silently overwriting live blocks with dry-run ones would not
    /// produce a wrong measurement, it would produce a confident one — dry-run
    /// compared against dry-run, reported as proof that dozing does nothing.
    ///
    /// Snoozing while already snoozed extends rather than stacks, and keeps the
    /// *original* restore mode. Otherwise a second snooze taken during the first
    /// would record dry-run as the thing to go back to, and the daemon would
    /// come out of it switched off.
    pub fn snooze(&mut self, how_long: Duration) -> Result<(), String> {
        if self.trial.as_ref().is_some_and(|t| !t.finished()) {
            return Err("a trial is running, and it is a schedule of mode changes; \
                        `dozectl trial stop` first"
                .into());
        }
        let restore = self.snooze.map_or(self.world.config.mode, |s| s.restore);
        self.snooze = Some(Snooze { until: Instant::now() + how_long, restore });
        tracing::info!(?how_long, restore = restore.as_str(), "snoozing");
        // Releases every hold on the way in, by the same path `dozectl mode
        // dry-run` takes.
        self.set_mode(Mode::DryRun);
        Ok(())
    }

    /// End a snooze now, putting the mode back.
    ///
    /// Returns whether there was one. Called both by the timer and by a user
    /// who has changed their mind — and by [`set_mode`](Self::set_mode)'s
    /// callers, because an explicit mode change has to win: a snooze that later
    /// restored over the top of it would undo a deliberate choice minutes after
    /// it was made, which is the failure the installer had and the one hardest
    /// to attribute to the right cause.
    pub fn wake(&mut self) -> bool {
        let Some(snooze) = self.snooze.take() else { return false };
        tracing::info!(mode = snooze.restore.as_str(), "snooze over");
        self.set_mode(snooze.restore);
        true
    }

    /// Drop a snooze without restoring the mode, for when the user has just set
    /// the mode themselves.
    pub fn cancel_snooze(&mut self) -> bool {
        self.snooze.take().is_some()
    }

    /// How much of the snooze is left, for `dozectl status` and the dashboard.
    pub fn snooze_remaining(&self) -> Option<Duration> {
        self.snooze.map(|s| s.until.saturating_duration_since(Instant::now()))
    }

    /// Put the mode back once the timer has run out.
    fn check_snooze(&mut self) {
        if self.snooze.is_some_and(|s| Instant::now() >= s.until) {
            self.wake();
        }
    }

    // ---- the self-experiment (`dozed_core::trial`) ----------------------

    /// Begin a trial, or say why not.
    ///
    /// Refused outright without a live actuator, and that refusal is the whole
    /// point of the check: a trial whose "live" blocks cannot act would compare
    /// dry-run against dry-run, measure the noise floor, and report it as proof
    /// that dozing does nothing.
    pub fn start_trial(&mut self, plan: TrialPlan) -> Result<(), String> {
        if !self.is_live() {
            return Err("daemon was started without --live, so a live block would not be live; \
                        restart it with --live to run a trial"
                .into());
        }
        if self.trial.as_ref().is_some_and(|t| !t.finished()) {
            return Err("a trial is already running; `dozectl trial stop` first".into());
        }
        // Before `Trial::new`, which lays the schedule out eagerly. `pairs` and
        // `block` arrive over IPC from whatever the client sent, and the CLI's
        // "six is the floor" is documentation rather than a bound
        // (`TrialPlan::MAX_PAIRS`).
        plan.validate()?;
        let trial = Trial::new(plan, self.world.config.mode);
        tracing::info!(
            pairs = plan.pairs,
            block = ?plan.block,
            "trial starting: up to {:?} of suitable conditions",
            plan.duration()
        );
        self.persist_trial(&trial);
        self.trial = Some(trial);
        Ok(())
    }

    /// End the trial and put the mode back where the user left it.
    pub fn stop_trial(&mut self) -> Option<dozed_core::trial::Verdict> {
        let mut trial = self.trial.take()?;
        trial.stop();
        let verdict = trial.verdict();
        let cond = self.conditions();
        // `handing_back`, not `desired_mode`. That one is `current_arm()?`, and
        // `stop` has just set `at` past the end of the schedule — so it returned
        // `None` every time and stopping a trial put nothing back, leaving the
        // daemon in whatever mode the last block had set.
        let restore = trial.handing_back(&cond);
        self.persist_trial(&trial);
        self.trial = Some(trial);
        if let Some(mode) = restore {
            self.set_mode(mode);
        }
        Some(verdict)
    }

    pub fn trial(&self) -> Option<&Trial> {
        self.trial.as_ref()
    }

    /// The live reading from the power counters, for `dozectl status`.
    ///
    /// Reported whenever *anything* was readable, not only when there is a
    /// battery: a container or a VM with no `cpuidle` still has `/proc/stat`, and
    /// the machine-wide wakeup rate is worth seeing on its own — it is the figure
    /// the per-app numbers ought to add up to.
    pub fn energy_status(&self) -> Option<dozed_core::ipc::EnergyStatus> {
        // A closed counter counts as something to say, even on a machine where
        // nothing else reported: the whole point of the flag is that the silence
        // was the problem.
        let anything = self.meter.available()
            || self.rates.system_wakeups_per_sec.is_some()
            || self.meter.package_denied();
        anything.then(|| dozed_core::ipc::EnergyStatus {
            discharge_w: self.rates.discharge_w,
            package_w: self.rates.package_w,
            package_denied: self.meter.package_denied(),
            deepest_idle: self.rates.deepest_idle,
            system_wakeups_per_sec: self.rates.system_wakeups_per_sec,
            cpus: self.meter.cpus(),
            package_parts: self.rates.package_parts,
            gpu_w: self.rates.gpu_w,
            // Sent only alongside a figure. A label with no watts beside it is a
            // driver name on a status line, which is not a reading.
            gpu_label: self.rates.gpu_w.and(self.meter.gpu_label()).map(str::to_owned),
            machine_cores_busy: self.rates.machine_cores_busy,
            gpu_busy: self.rates.gpu_busy,
            power_model: Some(self.power_model.verdict()),
        })
    }

    /// How busy the GPU chip is, 0.0 to 1.0 — the denominator the per-app engine
    /// figures are read against.
    #[must_use]
    pub fn gpu_busy(&self) -> Option<f64> {
        self.rates.gpu_busy
    }

    /// What the fitted power model can currently say, refusals included.
    pub fn power_model(&self) -> dozed_core::power_model::Verdict {
        self.power_model.verdict()
    }

    fn persist_trial(&self, trial: &Trial) {
        let path = crate::paths::trial_file();
        if let Ok(json) = serde_json::to_string(trial) {
            // Through a rename like the frozen set, and for a milder version of
            // the same reason: this is rewritten on every reading, so a kill
            // during one is likely rather than unlucky, and a torn file loses a
            // trial that took an evening to collect.
            let _ = crate::paths::write_state_file(&path, json.as_bytes());
        }
    }

    /// Pick up a trial a previous run was in the middle of.
    ///
    /// A trial is hours long, so a restart during one is ordinary — an upgrade, a
    /// `systemctl --user restart`. The block that was in flight is lost by
    /// construction (`MonoTime` counts from this process's start, so a persisted
    /// deadline would be meaningless) and the finished ones are kept.
    ///
    /// Not resumed without a live actuator: the rest of the trial could not be
    /// live, and half a trial silently continued as dry-run against dry-run is
    /// worse than none.
    fn load_trial(&mut self) {
        let path = crate::paths::trial_file();
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let Ok(trial) = serde_json::from_str::<Trial>(&text) else {
            tracing::warn!(path = %path.display(), "unreadable trial file; ignoring it");
            return;
        };
        // Checked on the way in as well as on the way out, because the file is
        // the one thing that carries a plan across a restart: a run that predates
        // the bound, or a hand-edited file, would otherwise reinstate a schedule
        // this daemon would now refuse to create — and go on rewriting it on every
        // reading (`TrialPlan::validate`).
        if let Err(why) = trial.plan.validate() {
            tracing::warn!(
                path = %path.display(),
                "discarding a persisted trial whose plan is not one we would start: {why}"
            );
            let _ = std::fs::remove_file(&path);
            return;
        }
        if trial.finished() {
            // Kept, so `dozectl report` can still show what it measured.
            self.trial = Some(trial);
            return;
        }
        if !self.is_live() {
            tracing::warn!("a trial was in progress but this run is not live; abandoning it");
            return;
        }
        // The baseline is named because it is the mode this trial will put back
        // when it ends, and it was captured whenever the trial was started —
        // possibly while the daemon was already somewhere it should not have
        // been. Saying so on resume makes that visible before it happens rather
        // than after, which is the only defence against a stale one.
        tracing::info!(
            done = trial.blocks().len(),
            total = trial.blocks_total(),
            restores = trial.baseline().as_str(),
            "resuming the trial from a previous run"
        );
        self.trial = Some(trial);
    }

    pub fn reload_config(&mut self, config: Config) {
        // Before the dispatch, which moves it. The thermal thresholds live
        // outside the planner — they are how a *reading* becomes the
        // `ThermalState` the planner is given — so nothing downstream of
        // `ConfigChanged` would otherwise pick them up, and the one setting
        // somebody edits because `dozectl status` told them the state was stuck
        // would go on doing nothing until the next restart.
        self.thermal.set_thresholds(config.thermal.into());
        self.dispatch(Event::ConfigChanged { config: Box::new(config) });
    }

    // ---- crash safety --------------------------------------------------

    fn held_records(&self) -> Vec<FrozenRecord> {
        self.live
            .as_ref()
            .map(|a| {
                a.held()
                    .map(|(k, u, _, stopped)| FrozenRecord::held(k, u, stopped, &self.pids))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Write the frozen set, and remember it if that failed.
    ///
    /// §10.3 promotes this file from prudent to mandatory: a freeze outlives the
    /// process that applied it, so without a record of what is held a crash
    /// leaves applications stopped until something sweeps them up. A write that
    /// fails therefore breaks a safety net, and it did — a full disk on a real
    /// machine produced *"a crash now would leave it frozen until the next
    /// sweep"* in the journal and **nothing anywhere else**. §10's rule is that
    /// anything dozed cannot do, it says so; a warning that scrolls past at
    /// three in the morning is not saying so.
    ///
    /// Kept as the reason rather than a flag, and cleared on the next successful
    /// write, for the same reasons the recorder's own failure is
    /// (`tracefile::TraceFile::stopped`): a net that recovered should stop
    /// complaining, and one that has not should say what is wrong.
    fn persist_frozen(&mut self) {
        self.frozen_unwritable = write_frozen(&self.held_records()).err();
    }

    fn persist_frozen_with(&mut self, key: &AppKey, unit: &Unit) {
        let mut held = self.held_records();
        held.push(FrozenRecord::new(key, unit, &self.pids));
        self.frozen_unwritable = write_frozen(&held).err();
    }

    /// Thaw anything a previous run left behind.
    fn sweep_orphans(&self) {
        let records = read_frozen();
        if records.is_empty() {
            return;
        }
        tracing::warn!(count = records.len(), "restoring orphans from a previous run");
        for r in &records {
            // `cgroup` is the pre-`Unit::Cgroups` spelling; chaining it means an
            // upgrade across a crash still thaws what the old binary froze.
            for path in r.cgroups.iter().chain(r.cgroup.iter()) {
                let cg = dozed_linux::Cgroup::new(path);
                match cg.thaw() {
                    Ok(()) => tracing::info!(app = %r.app, cgroup = %path, "thawed orphan"),
                    Err(e) => tracing::warn!(app = %r.app, error = %e, "could not thaw orphan"),
                }
            }
            // The signal path: SIGCONT only processes actually sitting in `T`.
            // A cgroup-frozen task reports `S` and a running one does too, so
            // `T` is specifically "something stopped it" — and checking rather
            // than blanket-CONTing means a recycled PID cannot be resumed by
            // mistake.
            let stopped: Vec<u32> = r.stopped.iter().copied().filter(|p| is_stopped(*p)).collect();
            if !stopped.is_empty() {
                match dozed_linux::tree::resume(&stopped) {
                    Ok(()) => {
                        tracing::info!(app = %r.app, count = stopped.len(), "resumed orphan");
                    }
                    Err(e) => tracing::warn!(app = %r.app, error = %e, "could not resume orphan"),
                }
            }
        }
        // Startup sweep: nothing is held yet, and a failure here is not the
        // daemon's crash-recovery net breaking so much as the disk being full
        // before it has anything to protect. Logged by `write_frozen` either way.
        let _ = write_frozen(&[]);
    }
}

/// Whether a process is stopped by a signal, i.e. `/proc/<pid>/stat` state `T`.
fn is_stopped(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    // The comm field can contain spaces and parens, so the state is the first
    // token after the last ')'.
    stat.rfind(')')
        .and_then(|i| stat[i + 1..].split_whitespace().next())
        .is_some_and(|state| state == "T")
}

/// What a previous run was holding, written *before* each freeze.
///
/// A freeze outlives the process that applied it, so without this a `SIGKILL`
/// would strand apps frozen forever (`DESIGN.md` §10.3). Both units are covered:
/// a cgroup to thaw, or the processes to `SIGCONT`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FrozenRecord {
    app: String,
    /// Every cgroup the app was held in. An app is not reliably one cgroup
    /// (`Unit::Cgroups`), and a record that named only the first would strand
    /// the rest frozen across a crash — the exact failure §10.3 exists to stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cgroups: Vec<String>,
    /// The single-cgroup field this file used to carry. Read, never written, so
    /// that a daemon upgraded while apps are frozen still thaws them: the file
    /// on disk is from the *old* binary precisely when it matters most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cgroup: Option<String>,
    /// Written before the stop, so this is the whole tree rather than exactly
    /// what got stopped — a superset, which is the safe direction: `SIGCONT` to
    /// a process that was never stopped does nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stopped: Vec<u32>,
}

impl FrozenRecord {
    /// The record for a hold already applied, which knows exactly what it
    /// stopped.
    ///
    /// [`Self::new`] is written *before* the stop and can only guess from the
    /// pid map; this is written after and does not have to. The difference is
    /// not cosmetic: the pid map is rebuilt from the app's current windows, so
    /// an instance that exits while stopped leaves the map — and with it the
    /// on-disk record — while its processes stay stopped. `sweep_orphans` then
    /// has nothing to resume them by.
    fn held(key: &AppKey, unit: &Unit, stopped: &[u32], pids: &BTreeMap<AppKey, Vec<u32>>) -> Self {
        let mut record = Self::new(key, unit, pids);
        if !stopped.is_empty() {
            record.stopped = stopped.to_vec();
        }
        record
    }

    fn new(key: &AppKey, unit: &Unit, pids: &BTreeMap<AppKey, Vec<u32>>) -> Self {
        match unit {
            Unit::Cgroups(paths) => Self {
                app: key.to_string(),
                cgroups: paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                cgroup: None,
                stopped: Vec::new(),
            },
            Unit::Trees { roots } => Self {
                app: key.to_string(),
                cgroups: Vec::new(),
                cgroup: None,
                stopped: pids.get(key).cloned().unwrap_or_else(|| roots.clone()),
            },
        }
    }
}

/// Unix seconds. The clock a ceiling's deadline is measured against, since it
/// has to survive the process that set it.
fn wall_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// One app's ceiling, as it goes to disk.
///
/// Wall-clock rather than `MonoTime`, because the deadline has to mean the same
/// thing to the next process and a monotonic clock restarts with it. The cost is
/// that a suspend counts against the ceiling, which is the right answer anyway:
/// "leave it alone for an hour" is an hour of the user's time, not an hour of
/// the machine's.
#[derive(serde::Serialize, serde::Deserialize)]
struct HoldRecord {
    app: AppKey,
    ceiling: dozed_core::Ceiling,
    /// Unix seconds, or absent for "until cleared".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    until_wall: Option<u64>,
}

/// Record what is held. `Err` carries why it could not be, for the status line.
fn write_frozen(records: &[FrozenRecord]) -> Result<(), String> {
    let path = crate::paths::frozen_file();
    let json = serde_json::to_string(records).map_err(|e| e.to_string())?;
    // Written synchronously and before the freeze, so a SIGKILL in between still
    // leaves a record to sweep — and written through a rename, so the SIGKILL
    // cannot catch it half-written either (`paths::write_state_file`).
    //
    // Logged on failure, unlike most of the housekeeping here. This file is what
    // stands between a crash and an application frozen for good, so "we could not
    // record what we are about to freeze" is worth saying out loud even though
    // there is nothing to be done about it from here.
    if let Err(e) = crate::paths::write_state_file(&path, json.as_bytes()) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            held = records.len(),
            "could not record what is held; a crash now would leave it frozen until the next sweep"
        );
        return Err(e.to_string());
    }
    Ok(())
}

fn read_frozen() -> Vec<FrozenRecord> {
    let path = crate::paths::frozen_file();
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    match serde_json::from_str(&text) {
        Ok(records) => records,
        // Distinguished from "no orphans", which is what this used to be
        // indistinguishable from: a file we cannot parse may name applications
        // that are frozen right now, and the sweep is about to conclude there is
        // nothing to do. It cannot recover them — that is what the parse failure
        // means — but it must not do it silently.
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "the record of what a previous run held is unreadable, so anything it froze \
                 cannot be swept. `dozectl thaw --all` releases everything unconditionally."
            );
            Vec::new()
        }
    }
}

/// Every process in the tree rooted at `pid`, so a browser's 20 helpers are
/// sampled as one app (`DESIGN.md` §4).
///
/// Delegates to `dozed_linux::tree`, which the signal actuator already uses.
/// There were two walkers here, and this one was the weaker: it read only
/// `/proc/<pid>/task/<pid>/children`, the thread-group leader's list, so a
/// process forked by any other thread was invisible to sampling and to identity.
fn collect_tree_pids(root: u32) -> Vec<u32> {
    dozed_linux::tree::collect(root)
}

/// The more cautious of two classes, for an app whose windows disagree.
///
/// `System` outranks `LongRunningJob` because it is the one no rule can override
/// (§10.1), and both outrank `Normal`. Not `Ord` on [`AppClass`], whose
/// derivation follows declaration order and would rank `LongRunningJob` highest.
const fn more_protective(a: AppClass, b: AppClass) -> AppClass {
    match (a, b) {
        (AppClass::System, _) | (_, AppClass::System) => AppClass::System,
        (AppClass::LongRunningJob, _) | (_, AppClass::LongRunningJob) => AppClass::LongRunningJob,
        _ => AppClass::Normal,
    }
}

async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
