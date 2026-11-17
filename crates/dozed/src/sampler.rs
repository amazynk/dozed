//! Resource sampling (`DESIGN.md` §9, §13).
//!
//! The primary metric is **wakeups per second**, not CPU%. High background CPU
//! is ambiguous — it is either a misbehaving app or exactly the work you asked
//! for — but a process that wakes 900 times a second while hidden is doing
//! nothing useful, and wakeups are what actually cost battery.
//!
//! A battery tool that polls `/proc` for 400 processes at 4 Hz is the problem it
//! claims to solve, so this reads `cpu.stat` once per cgroup where it can, and
//! only touches per-PID files for the context-switch counters the cgroup does
//! not expose.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use std::time::Duration;

use dozed_core::{AppKey, ProcContrib, ProcSample};

/// Accumulated counters for one app: increments that have actually been
/// observed, and therefore monotonic by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Counters {
    ctxt_switches: u64,
    cpu_usec: u64,
    /// GPU engine nanoseconds, summed over this app's DRM clients.
    gpu_nsec: u64,
}

/// Where one CPU figure came from.
///
/// A reading never mixes the two — `read_counters` is all or nothing about
/// cgroups — but consecutive readings can differ, because a cgroup that stops
/// being readable drops the whole app back to the per-PID walk. Keying the two
/// apart means such a swap reads as "every member is new", which costs one
/// interval, rather than as a cgroup's lifetime total arriving as work.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CpuSource {
    Cgroup(PathBuf),
    Pid(u32),
}

/// One app's raw counters at one instant, kept **per member** rather than
/// summed.
///
/// Summing first and differencing after cannot tell "the app got busier" from
/// "the app got bigger", and an app's membership changes constantly: a browser
/// spawns a content process per tab, and `read_counters` sums over whatever
/// PIDs and cgroups the app owns at that moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reading {
    /// Context switches per PID, summed over that PID's threads.
    ctxt: BTreeMap<u32, u64>,
    /// CPU microseconds per cgroup, or per PID where there is no usable cgroup.
    cpu: BTreeMap<CpuSource, u64>,
    /// GPU engine nanoseconds per PID, for the processes that have a DRM client.
    ///
    /// Per member like the others, and for the same reason: a browser opening a
    /// tab adds a client whose counter starts at whatever the driver has already
    /// given it, and summing before differencing cannot tell that from work.
    gpu: BTreeMap<u32, u64>,
}

/// What the members present in *both* readings did between them.
///
/// A member absent from `previous` has just joined the app and contributes
/// nothing: the counter it arrives holding is its lifetime total, not this
/// interval's work. A member absent from `current` has gone and taken its
/// counter with it, which is not negative work. Both are membership changes,
/// and per-member differencing is the only place that can tell them from
/// activity — by the time the counters are summed the distinction is gone.
///
/// The clamp still matters for the member that stays: a process restarting
/// under a recycled PID resets its counters, and under-reporting is the safe
/// direction (`ctxt_switches_of`).
fn increment<K: Ord>(previous: &BTreeMap<K, u64>, current: &BTreeMap<K, u64>) -> u64 {
    current
        .iter()
        .filter_map(|(member, now)| previous.get(member).map(|before| now.saturating_sub(*before)))
        .sum()
}

/// Every core, fully busy: the most CPU any app can possibly be using.
///
/// An arithmetic ceiling rather than a tuning threshold. `cpu_pct` is a
/// percentage of *one* core (`starter.rs` documents it that way to users), so a
/// sixteen-core machine tops out at 1600 and anything above that is a bug in the
/// measurement, not a busy application.
///
/// Read once. `available_parallelism` consults the cgroup's CPU allocation as
/// well as the machine's, which is the right answer for a daemon that may itself
/// be confined — and it errs high on failure, because the cost of a ceiling that
/// is too high is one bad sample surviving and the cost of one too low is real
/// measurements being thrown away.
fn cpu_ceiling_pct() -> f32 {
    static CEILING: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *CEILING.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1024, std::num::NonZero::get);
        // Multiplied by more than 100 on purpose. Cores can be hot-plugged, a
        // container's quota can be raised under us, and the reading is taken
        // over a window during which either could have happened — so the
        // ceiling is deliberately loose. It is here to catch a number three
        // orders of magnitude out, not to police the last few percent.
        cores as f32 * 200.0
    })
}

/// How many readings one app may keep, whatever `cpu_sample_window` says.
///
/// At the 2s floor a 60s window is 30 readings; this bounds a pathological
/// window (`"1h"`) to something that cannot grow without limit. Hitting it
/// shortens the effective window rather than failing.
const MAX_READINGS: usize = 256;

/// One app's measurement history.
#[derive(Debug, Default)]
struct History {
    /// The last raw read, turned into an increment on the next one.
    last: Option<Reading>,
    /// Increments accumulated so far, and therefore monotonic by construction.
    total: Counters,
    /// `(total, at)` spanning `cpu_sample_window`, oldest first.
    window: std::collections::VecDeque<(Counters, std::time::Instant)>,
    /// Whether this app has ever been seen holding a DRM client.
    ///
    /// Sticky, so that an app which does GPU work in bursts reports `0.0%`
    /// between them rather than flickering to "no GPU at all" — the column
    /// would otherwise be unreadable on exactly the apps that use the chip.
    saw_gpu: bool,
}

/// Turns cumulative counters into rates.
#[derive(Debug, Default)]
pub struct Sampler {
    /// The DRM descriptor cache, which is what makes per-app GPU time cost
    /// 4.5 ms a tick instead of 194 (`dozed_linux::gpu`).
    gpu: dozed_linux::gpu::GpuClients,
    /// Per app: a window of readings rather than only the previous one.
    ///
    /// The sampling interval is not a constant — `adaptive_interval` moves it
    /// between 60s and 2s according to how close the next grace deadline is — so
    /// a rate taken over "since last time" measured a 30× narrower window
    /// precisely when a decision was imminent, and `cpu_sample_window`, which
    /// documents itself as the averaging window, was read by no code at all.
    ///
    /// Seen live: at the 2s floor every app on the machine reported 3-80× its
    /// usual rate for exactly one sample (`opensnitch_ui` 8/s → 635/s) and was
    /// back to normal on the next. Over a real 60s window that burst is ~115/s
    /// rather than 3466/s. §9 rests on wakeups being comparable against a
    /// threshold, so the number must not depend on an unrelated deadline.
    ///
    /// The window is kept over *accumulated increments*, not over raw counters,
    /// which matters because the raw figure is a sum over a membership that
    /// changes: `ctxt_switches_of` walks `/proc/<pid>/task` at read time, so
    /// threads that exit take their counters out of the sum and the raw total can
    /// fall. Differencing raw counters across a 60s window therefore clamped a
    /// whole window to zero whenever a threaded app churned — measured
    /// immediately after this change went live, with a wezterm running builds
    /// reporting `0/s` on 2 of 8 samples while holding 25% CPU. Accumulating
    /// per-interval increments confines that loss to the one interval it happened
    /// in, which is where it was before the window existed.
    ///
    /// Each increment is in turn taken per member rather than off the sum
    /// ([`increment`]), because membership changes in both directions and only
    /// one of them is a fall. A member *joining* raises the sum by its lifetime
    /// counter, which differencing the sum credits as this interval's work.
    history: BTreeMap<AppKey, History>,
    /// Per-process counters and the instant they were read, for the breakdown.
    per_pid: BTreeMap<u32, (u64, std::time::Instant)>,
    /// The rate derived from the last two reads of each process.
    rates: BTreeMap<u32, f32>,
    /// Set once an impossible reading has been reported, so the warning is one
    /// line rather than one per tick. See [`Sampler::record`].
    implausible_reported: bool,
}

impl Sampler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample one app. Returns `None` on the first call for an app, because a
    /// rate needs two points.
    pub fn sample(
        &mut self,
        key: &AppKey,
        pids: &[u32],
        cgroups: &[std::path::PathBuf],
        now: std::time::Instant,
        window: Duration,
    ) -> Option<ProcSample> {
        let mut reading = read_counters(pids, cgroups);
        // Kept out of `read_counters` because it needs the descriptor cache, and
        // the cache is what makes it affordable at all (`dozed_linux::gpu`).
        reading.gpu = self.gpu.engine_nsec(pids);
        self.attribute(&reading.ctxt, now);
        self.record(key, reading, now, window)
    }

    /// Keep each PID's own increment, so the app's total can be broken down.
    ///
    /// Free: `read_counters` has already walked every thread of every process
    /// to produce the sum, and this only declines to throw the parts away.
    /// Storage is one `u64` and an `Instant` per process — 173 of them on the
    /// machine this was written against.
    fn attribute(&mut self, per_pid: &BTreeMap<u32, u64>, now: std::time::Instant) {
        for (&pid, &current) in per_pid {
            let rate = match self.per_pid.get(&pid) {
                Some(&(previous, at)) => {
                    let dt = now.duration_since(at).as_secs_f32();
                    // A fall means threads exited and took their counters with
                    // them; that is not a negative rate (`ctxt_switches_of`).
                    if dt > 0.0 { current.saturating_sub(previous) as f32 / dt } else { 0.0 }
                }
                None => 0.0,
            };
            self.per_pid.insert(pid, (current, now));
            self.rates.insert(pid, rate);
        }
    }

    /// Drop processes that are no longer any app's, once per tick.
    ///
    /// Deliberately not done inside `attribute`, which sees one app at a time:
    /// pruning to that app's PIDs there wiped every other app's previous
    /// reading, so each app in turn found no prior value and reported a rate of
    /// zero. The breakdown was empty for the entire session before this was
    /// separated out.
    pub fn retain_pids(&mut self, live: &std::collections::BTreeSet<u32>) {
        self.per_pid.retain(|pid, _| live.contains(pid));
        self.rates.retain(|pid, _| live.contains(pid));
        // The descriptor cache is keyed by pid too, and a desktop session churns
        // through thousands of them.
        self.gpu.retain(live);
    }

    /// The busiest processes of `pids`, with their names, busiest first.
    ///
    /// `comm` is read here rather than during sampling because it is only
    /// needed for the handful actually shown, and reading it for all 173 every
    /// tick would be exactly the kind of cost §13 objects to.
    pub fn top_processes(&self, pids: &[u32], limit: usize) -> Vec<ProcContrib> {
        let mut rows: Vec<(f32, u32)> = pids
            .iter()
            .filter_map(|p| self.rates.get(p).map(|r| (*r, *p)))
            .filter(|(rate, _)| *rate > 0.5)
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        rows.truncate(limit);
        rows.into_iter()
            .map(|(wakeups_per_sec, pid)| ProcContrib {
                pid,
                comm: std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default(),
                wakeups_per_sec,
                role: role_of(pid),
            })
            .collect()
    }

    /// The rate maths, with the `/proc` reads lifted out so it can be tested
    /// against a synthetic clock and synthetic counters.
    fn record(
        &mut self,
        key: &AppKey,
        current: Reading,
        now: std::time::Instant,
        window: Duration,
    ) -> Option<ProcSample> {
        let history = self.history.entry(key.clone()).or_default();

        // The increment since the previous read, taken per member and clamped at
        // zero. Only the processes and cgroups the app owned at *both* instants
        // can say anything about the interval between them; see [`increment`].
        // Accumulating per interval rather than differencing across the window is
        // what stops one moment of thread churn from erasing a whole window.
        if let Some(previous) = &history.last {
            history.total.ctxt_switches += increment(&previous.ctxt, &current.ctxt);
            history.total.cpu_usec += increment(&previous.cpu, &current.cpu);
            history.total.gpu_nsec += increment(&previous.gpu, &current.gpu);
        }
        history.saw_gpu |= !current.gpu.is_empty();
        history.last = Some(current);
        history.window.push_back((history.total, now));

        // Keep the oldest reading that is still at least `window` old, and
        // everything after it: that reading is the far end of the window. Drop a
        // front entry only once its successor is old enough to take over, or the
        // window would collapse to the newest pair again.
        while history.window.len() > 2
            && history.window.get(1).is_some_and(|(_, at)| now.duration_since(*at) >= window)
        {
            history.window.pop_front();
        }
        while history.window.len() > MAX_READINGS {
            history.window.pop_front();
        }

        let (oldest, oldest_at) = *history.window.front()?;
        if oldest_at == now {
            return None; // the first reading for this app: a rate needs two
        }

        let dt = now.duration_since(oldest_at).as_secs_f32();
        if dt <= 0.0 {
            return None;
        }
        // Both totals are monotonic, so these cannot underflow.
        let switches = history.total.ctxt_switches - oldest.ctxt_switches;
        let cpu = history.total.cpu_usec - oldest.cpu_usec;
        let gpu = history.total.gpu_nsec - oldest.gpu_nsec;

        let sample = ProcSample {
            wakeups_per_sec: quantise(switches as f32 / dt, RATE_STEP),
            cpu_pct: quantise((cpu as f32 / 1_000_000.0) / dt * 100.0, RATE_STEP),
            // `None` for an app that has never held a DRM client, so that a
            // window with no GPU work reads as "no GPU work" and an app that
            // cannot do any reads as nothing at all.
            gpu_pct: history
                .saw_gpu
                .then(|| quantise((gpu as f32 / 1_000_000_000.0) / dt * 100.0, GPU_STEP)),
        };

        // A reading that cannot have happened is discarded rather than reported.
        //
        // Seen once in a 13-day journal: Firefox at **4246% CPU on a 16-core
        // machine**, beside 198,649 wakeups/s where the 99.9th percentile of
        // every other sample was 7,340. The arithmetic above has no way to
        // produce that from counters it read twice, so something upstream —
        // `/proc` membership, a recycled pid, a counter that is not what it
        // claims — put a lifetime total where an interval belonged.
        //
        // Discarded, not clamped, and that is the same judgement `has_sample`
        // already makes: a clamp would hand the rule table a number nobody
        // measured, and rules compare `cpu_pct` and `wakeups_per_sec` against
        // thresholds, so a fabricated ceiling decides just as firmly as a real
        // reading would. `None` means "not yet", which is true, and leaves the
        // app in whatever state it was already in.
        if sample.cpu_pct > cpu_ceiling_pct() {
            if !self.implausible_reported {
                self.implausible_reported = true;
                tracing::warn!(
                    app = %key,
                    cpu_pct = sample.cpu_pct,
                    wakeups_per_sec = sample.wakeups_per_sec,
                    ceiling_pct = cpu_ceiling_pct(),
                    window_secs = dt,
                    "impossible sample discarded; this app's window restarts. \
                     Reported once per run."
                );
            }
            // The bad increment is already in `total` and in every window entry
            // built from it, so leaving the history in place would inflate every
            // sample until it aged out. One window of measurement for this app
            // is the right price for not carrying the error forward.
            self.history.remove(key);
            return None;
        }
        Some(sample)
    }

    pub fn forget(&mut self, key: &AppKey) {
        self.history.remove(key);
    }

    /// Whether this app has ever produced a rate.
    ///
    /// A rate needs two readings and the tick backs off to 60s when nothing is
    /// held or pending, so for the first minute or two of a run there is
    /// genuinely nothing to report — and `ProcSample::default()` sitting in the
    /// world state reads as a measured zero. The clients ask this so they can
    /// print "not yet" instead of a number nobody measured
    /// (`ipc::AppSummary::sampled`).
    ///
    /// Two entries, not one: `record` pushes the first reading and returns
    /// `None`, so a window of length one is an app that has been *looked at*
    /// and not yet measured.
    #[must_use]
    pub fn has_sample(&self, key: &AppKey) -> bool {
        self.history.get(key).is_some_and(|h| h.window.len() >= 2)
    }

    /// Whether *anything* has been measured yet.
    ///
    /// One flag for the whole sampler rather than per app, because
    /// [`Daemon::sample_all`](crate::daemon) reads every app on the same tick:
    /// they all reach their second reading together, so "has any app got a
    /// rate" and "have two ticks happened" are the same question. Used to sample
    /// quickly until the answer is yes — see [`adaptive_interval`].
    ///
    /// Monotone once true, which is what makes it safe to key a faster tick on:
    /// readings are retained, so this cannot flap back to false and leave the
    /// daemon busy-polling forever.
    #[must_use]
    pub fn any_sample(&self) -> bool {
        self.history.values().any(|h| h.window.len() >= 2)
    }
}

/// CPU comes from the cgroups when the app has them and from `/proc` when it
/// does not — and *all* of them, summed. An app spread across sibling cgroups
/// (`dozed_linux::is_the_unit_for_app`) would otherwise report only the slice
/// its first cgroup happened to hold.
fn read_counters(pids: &[u32], cgroups: &[PathBuf]) -> Reading {
    let from_cgroups: Option<BTreeMap<CpuSource, u64>> = if cgroups.is_empty() {
        None
    } else {
        // All or nothing: a cgroup we cannot read is a hole of unknown size, and
        // quietly leaving it out under-reports for as long as it stays unreadable
        // — worse than falling back to the per-PID walk that always works.
        cgroups.iter().map(|c| Some((CpuSource::Cgroup(c.clone()), cgroup_cpu_usec(c)?))).collect()
    };
    Reading {
        // Kept per process rather than summed and discarded: the walk is the
        // expensive part and it has already happened, and the parts are what the
        // rate maths and the breakdown both need.
        ctxt: pids.iter().filter_map(|p| ctxt_switches_of(*p).map(|v| (*p, v))).collect(),
        cpu: from_cgroups.unwrap_or_else(|| {
            // No tidy scope: fall back to per-PID CPU time.
            pids.iter().filter_map(|p| Some((CpuSource::Pid(*p), proc_cpu_usec(*p)?))).collect()
        }),
        // Filled by the caller, which owns the descriptor cache.
        gpu: BTreeMap::new(),
    }
}

/// What a process is within its app, for the Chromium and Electron family.
///
/// Read from the command line, and only for the handful of processes actually
/// reported — the same reasoning as `comm`, and the same reason it is not read
/// during sampling for all 173.
///
/// This exists because `comm` cannot answer the question. Measured on a real
/// Signal: all seventeen of its processes report `comm = signal-desktop`, so the
/// breakdown that was added to say *which* process is responsible showed
/// seventeen identical rows. Chromium passes `--type=` to every child, and that
/// is the distinction a person is actually looking for.
///
/// The main process passes no `--type` at all, so its absence is the answer
/// rather than a failure — hence a plain string and no `Option`.
fn role_of(pid: u32) -> String {
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else { return String::new() };
    // Arguments are NUL-separated, so this needs no shell-style parsing and
    // cannot be confused by a filename containing a space.
    for arg in raw.split(|b| *b == 0) {
        if let Some(kind) = arg.strip_prefix(b"--type=") {
            return String::from_utf8_lossy(kind).into_owned();
        }
    }
    // Only claim "main" for something that looks like this family at all; an
    // ordinary single-process app has no roles to distinguish and a column
    // reading "main" for every row of it is noise.
    if raw.split(|b| *b == 0).any(|arg| arg.starts_with(b"--")) {
        return "main".to_owned();
    }
    String::new()
}

/// Voluntary plus involuntary context switches, summed over **every thread** of
/// the process: our proxy for how often it wakes the CPU.
///
/// `/proc/<pid>/status` reports the thread group leader alone, not the process,
/// and reading it was undercounting the tool's own primary metric. Measured on
/// this machine: `kwin_wayland` 1.60M by the leader against 2.50M across its 32
/// threads, `plasmashell` 281k against 585k across 63. The daemon's own figure
/// was out by 21x, because a tokio runtime parks most of its work on threads the
/// leader knows nothing about. A 2x error on the number every `wakeups_per_sec`
/// rule is written against makes those rules mean something other than what they
/// say.
///
/// Cost of getting it right, measured over 104 threads of three real desktop
/// processes: 1.25 ms per sample against 0.036 ms for the leader-only read —
/// about **12 µs per thread**, and that part has held up.
///
/// What did not hold up is the sentence that used to follow it: "at the 2s
/// sampling floor that is 0.06% of a core". Three processes is not a session.
/// Measured live across nine apps, `dozed` sweeps **173 processes and ~2,725
/// threads**, one full pass every 3.7 s, sustaining **740 `status` opens per
/// second** for **0.58% of a core** — ten times the figure this comment used to
/// give. Browsers are dozens of processes each, and a terminal that launched
/// GUI apps carries them all in its own tree, so those threads are swept twice
/// (`DESIGN.md` §4 and §13).
///
/// It is still a good trade against the hundreds of wakeups one frozen browser
/// returns, and it is still the most expensive thing the daemon does — which is
/// the reason to state it accurately rather than reassuringly.
/// `/proc/<tid>/schedstat` was rejected as "not worth 0.6 ms" against the
/// 1.25 ms baseline; against 33 ms per sweep that halving is worth revisiting.
///
/// Threads exiting between samples take their counters with them, so a delta can
/// come out negative. [`increment`] clamps that to zero, which *under*-reports
/// during thread churn — the safe direction, because a low wakeup rate means we
/// leave the app alone (non-negotiable: ambiguity resolves to "do nothing",
/// never to "freezable").
fn ctxt_switches_of(pid: u32) -> Option<u64> {
    let tasks = fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let counter = WakeupCounter::in_force();
    let mut total = 0;
    let mut any = false;
    for entry in tasks.flatten() {
        // A thread can exit while we are walking the directory; that is normal
        // and not a reason to discard the whole process's sample.
        let Some(n) = counter.read(&entry.path()) else { continue };
        any = true;
        total += n;
    }
    any.then_some(total)
}

/// Which `/proc` file the per-thread wakeup count is read out of.
///
/// The sweep is the most expensive thing dozed does — measured at 33 ms over
/// ~2,725 threads, every 3.7 s on a busy session, which is 0.58% of a core and
/// 740 file opens a second (`DESIGN.md` §13). Almost all of that is
/// `/proc/<tid>/status`, which is 61 lines of which exactly two are wanted.
///
/// `/proc/<tid>/schedstat` is three numbers on one line, and its third field —
/// `sched_info.pcount`, the number of times the task has been put on a CPU — is
/// the same quantity counted from the scheduler's side.
///
/// §13 dismissed this once, against a 1.25 ms baseline that later turned out to
/// be 33 ms, and said the trade should be re-run rather than inherited. Re-run,
/// on this machine, 5,190 threads, best of five alternating sweeps:
///
/// | | |
/// |---|---|
/// | `status` sweep | **31.24 ms** |
/// | `schedstat` sweep | **14.11 ms** |
/// | agreement over 5 s | 26,852 against 26,853 increments — **0.004%** |
///
/// Twice as cheap and the same number. The agreement is what licenses the swap:
/// the absolute values differ by a small constant (a task is put on a CPU once
/// before it has switched away from it), and dozed only ever differences them.
///
/// ## It has to be able to refuse, because the failure would be silent
///
/// `pcount` is maintained under `CONFIG_SCHED_INFO`, which is on for the
/// overwhelming majority of distribution kernels but is not architecturally
/// guaranteed — and note that it does **not** require the `sched_schedstats`
/// sysctl, which was `0` on the machine measured above. Where it is not
/// maintained the field reads a permanent `0`, which would arrive here as *every
/// app wakes the CPU zero times a second*: no rule keyed on `wakeups_per_sec`
/// could ever fire, `dozectl report` would claim nothing was ever worth
/// freezing, and nothing anywhere would say why. That is §10's original sin
/// exactly — a signal nothing sets, and no line of output admitting it.
///
/// So it is probed once at startup, by [`WakeupCounter::probe`], against the one
/// task certain to have been scheduled: our own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeupCounter {
    /// `/proc/<tid>/schedstat`, field 3. Half the cost, same number.
    Schedstat,
    /// `/proc/<tid>/status`, `voluntary_ctxt_switches + nonvoluntary_ctxt_switches`.
    ///
    /// The fallback, and what this always did.
    Status,
}

/// Resolved once, at startup, by [`WakeupCounter::probe`].
///
/// A `OnceLock` rather than a field threaded through `Sampler`, because
/// `ctxt_switches_of` is called from a free function three layers down and the
/// answer cannot change while the process runs — the kernel does not gain
/// `CONFIG_SCHED_INFO` at teatime.
static COUNTER: std::sync::OnceLock<WakeupCounter> = std::sync::OnceLock::new();

impl WakeupCounter {
    /// Decide which counter this kernel can be asked for, and remember it.
    ///
    /// Called once from `main`. Idempotent, and the first caller wins, so a test
    /// that pins one is not overruled by a later probe.
    pub fn probe() -> Self {
        let probed = if Self::schedstat_is_maintained() { Self::Schedstat } else { Self::Status };
        *COUNTER.get_or_init(|| probed)
    }

    /// What is in force, defaulting to the safe answer.
    ///
    /// `Status` rather than `Schedstat` when nothing has probed, because a unit
    /// test or a caller that skipped the probe must get the counter that works
    /// everywhere rather than the one that is merely usually right.
    #[must_use]
    pub fn in_force() -> Self {
        *COUNTER.get().unwrap_or(&Self::Status)
    }

    /// Pin the counter. Tests only — the first setter wins, as for [`Self::probe`].
    #[cfg(test)]
    fn pin(which: Self) {
        let _ = COUNTER.set(which);
    }

    /// Whether this kernel actually maintains `pcount`.
    ///
    /// Asked of our own thread, which has by definition been scheduled — this
    /// runs after `main` has done real work — so a zero here is the counter not
    /// being kept rather than a task that has not run yet. A missing or
    /// unparseable file answers the same way.
    fn schedstat_is_maintained() -> bool {
        Self::pcount("/proc/self/schedstat").is_some_and(|n| n > 0)
    }

    /// The name of this counter, for `dozectl status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schedstat => "schedstat",
            Self::Status => "status",
        }
    }

    /// Read one thread's count, given its `/proc/<pid>/task/<tid>` directory.
    fn read(self, task_dir: &Path) -> Option<u64> {
        match self {
            Self::Schedstat => Self::pcount(task_dir.join("schedstat")),
            Self::Status => {
                fs::read_to_string(task_dir.join("status")).ok().map(|s| sum_ctxt_switches(&s))
            }
        }
    }

    /// Field 3 of a `schedstat` file: times this task has been put on a CPU.
    fn pcount(path: impl AsRef<Path>) -> Option<u64> {
        parse_pcount(&fs::read_to_string(path).ok()?)
    }
}

/// `sum_exec_runtime run_delay pcount` — the third field is the one wanted.
///
/// Split out from the read so the parse can be tested without a `/proc`.
fn parse_pcount(schedstat: &str) -> Option<u64> {
    schedstat.split_whitespace().nth(2)?.parse().ok()
}

/// The two counters out of one `status` file.
fn sum_ctxt_switches(status: &str) -> u64 {
    status
        .lines()
        .filter(|l| {
            l.starts_with("voluntary_ctxt_switches:")
                || l.starts_with("nonvoluntary_ctxt_switches:")
        })
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum()
}

fn cgroup_cpu_usec(cgroup: &Path) -> Option<u64> {
    let text = fs::read_to_string(cgroup.join("cpu.stat")).ok()?;
    text.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())
}

/// utime + stime from `/proc/<pid>/stat`, in microseconds.
fn proc_cpu_usec(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field can contain spaces and parentheses, so fields are counted
    // from the last ')' rather than from the start.
    let after = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After comm and state, utime is field 11 and stime field 12 (1-based in
    // proc(5)); here index 11 and 12 counting state as 0.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let hz = 100; // USER_HZ is 100 on every Linux target we support.
    Some((utime + stime) * 1_000_000 / hz)
}

/// The resolution a sampled rate is reported at.
///
/// **A rate averaged over a 60-second window does not have seven significant
/// figures**, and `wakeups_per_sec: 2290.4712` claims a precision the
/// measurement has not got — it is a counter difference divided by a wall-clock
/// interval, both of which move. Quantising says what is actually known.
///
/// Done here rather than at the point of display, so the planner and a replay of
/// the same session see the identical number. Rounding only on the way to the
/// trace would mean the daemon decided on one value and `dozectl replay` decided
/// on another, and a rule with a threshold between the two would break §14's
/// promise that a trace replays to its own intent stream.
///
/// It is also most of what makes a trace affordable. Measured on a real 67 MB
/// recording: at full `f32` precision **4.8%** of consecutive per-app samples
/// were identical, so almost nothing could be suppressed as unchanged; at this
/// resolution **40%** are — 122,259 of 305,459 records, about 21 MB. A frozen
/// app reporting nothing is the common case, and it should cost nothing to say
/// so twice.
///
/// 0.1 against thresholds written in whole wakeups a second and whole percents
/// is far below anything a rule can distinguish, and far below the noise §13
/// documents in the sampler itself.
const RATE_STEP: f32 = 0.1;

/// The same, for a GPU percentage — one decimal finer, because engine shares of
/// interesting apps sit in the low single digits where 0.1 would be most of the
/// signal.
const GPU_STEP: f32 = 0.01;

/// Round to a multiple of `step`, keeping the sign and never producing a NaN.
fn quantise(v: f32, step: f32) -> f32 {
    if !v.is_finite() {
        return v;
    }
    (v / step).round() * step
}

/// How fast to sample while nothing has been measured yet. Short enough that a
/// table of dashes fills in before anybody has finished reading it, long enough
/// that the two readings are a rate rather than noise.
const WARMUP: Duration = Duration::from_secs(3);

/// How often to sample, backed off hard when nothing is pending (`DESIGN.md` §13).
///
/// The daemon measures its own cost and reports it in `dozectl status`, so this
/// being wrong is at least visible.
///
/// `warming_up` is the one case that overrides the back-off: a rate needs two
/// readings, so at the idle interval a fresh daemon knows nothing about anything
/// for two whole minutes. That is not only a blank table — every rule with a
/// `wakeups_per_sec` or `cpu_pct` threshold is deciding against
/// `ProcSample::default()` for that long, which is to say deciding on a
/// measurement nobody took. Two quick readings cost a handful of wakeups, once,
/// and §13's argument is about what a tool costs while it runs rather than about
/// its first ten seconds.
#[must_use]
pub fn adaptive_interval(
    anything_held: bool,
    next_deadline: Option<Duration>,
    warming_up: bool,
) -> Duration {
    if warming_up {
        return WARMUP.min(match next_deadline {
            Some(d) => d.max(Duration::from_millis(500)),
            None => WARMUP,
        });
    }
    match next_deadline {
        // A deadline is close: tighten up so we act on time, but never faster
        // than we could usefully react.
        Some(d) if d < Duration::from_secs(30) => {
            Duration::from_secs(2).min(d.max(Duration::from_millis(500)))
        }
        Some(d) if d < Duration::from_secs(120) => Duration::from_secs(10),
        _ if anything_held => Duration::from_secs(30),
        // Nothing frozen, nothing pending: there is nothing worth waking for.
        _ => Duration::from_secs(60),
    }
}

/// Our own wakeup rate, for `dozectl status`.
#[must_use]
pub fn self_wakeups(
    previous: &mut Option<(u64, std::time::Instant)>,
    now: std::time::Instant,
) -> f32 {
    let current = ctxt_switches_of(std::process::id()).unwrap_or(0);
    let rate = match previous {
        Some((prev, at)) => {
            let dt = now.duration_since(*at).as_secs_f32();
            if dt > 0.0 { current.saturating_sub(*prev) as f32 / dt } else { 0.0 }
        }
        None => 0.0,
    };
    *previous = Some((current, now));
    rate
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);

    /// A reading from a single process, which is what the rate-maths tests want:
    /// with membership fixed, differencing per member and differencing the sum
    /// agree, so these tests keep saying what they were written to say.
    fn one_process(ctxt_switches: u64, cpu_usec: u64) -> Reading {
        Reading {
            ctxt: BTreeMap::from([(1, ctxt_switches)]),
            cpu: BTreeMap::from([(CpuSource::Pid(1), cpu_usec)]),
            gpu: BTreeMap::new(),
        }
    }

    #[test]
    fn the_breakdown_names_the_busiest_processes_and_forgets_the_dead() {
        // What the total on an app's row cannot say: which of its processes is
        // responsible. A terminal reported 3137 wakeups/s while its own process
        // was doing 70, the rest being 68 things launched from it.
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let me = std::process::id();
        let t0 = std::time::Instant::now();

        // Two reads are needed for a rate, as everywhere else here.
        s.sample(&key, &[me], &[], t0, MINUTE);
        assert!(s.top_processes(&[me], 5).is_empty(), "one reading is not a rate");

        s.sample(&key, &[me], &[], t0 + Duration::from_secs(1), MINUTE);
        let top = s.top_processes(&[me], 5);
        // Our own process may legitimately be idle, so assert the shape rather
        // than a rate: it is listed, named, and attributed to the right pid.
        for row in &top {
            assert_eq!(row.pid, me);
            assert!(!row.comm.is_empty(), "the point is naming it: {row:?}");
        }

        // A process that has gone must not linger — the PID will be reused.
        // Pruning is per tick, not per app: doing it inside the per-app path
        // wiped every other app's previous reading and left every rate at zero.
        s.retain_pids(&std::collections::BTreeSet::new());
        assert!(s.top_processes(&[me], 5).is_empty(), "a departed pid must be forgotten");
    }

    #[test]
    fn our_own_role_is_read_from_our_own_command_line() {
        // The test binary is invoked with flags, so it looks like the family and
        // has no `--type`: the answer is "main". Read from the real `/proc`,
        // because the parsing is only useful if it matches what the kernel writes
        // — NUL-separated arguments, not a shell string.
        let me = std::process::id();
        let role = role_of(me);
        assert!(role == "main" || role.is_empty(), "unexpected role {role:?}");
        assert_eq!(role_of(u32::MAX), "", "a dead process has no role, and does not panic");
    }

    #[test]
    fn the_chromium_process_types_are_the_ones_that_matter() {
        // The measurement that motivated this: a real Signal runs seventeen
        // processes and every one of them reports `comm = signal-desktop`, so the
        // breakdown said nothing. `--type=` is what tells them apart, and the
        // shares it revealed were renderer 42%, utility 39%, gpu 14%, main 6%.
        //
        // Driven through the same NUL-separated shape `/proc/<pid>/cmdline` has,
        // since that is the only input this ever sees.
        let role = |args: &[&str]| {
            let raw: Vec<u8> = args.join("\0").into_bytes();
            let mut found = String::new();
            for arg in raw.split(|b| *b == 0) {
                if let Some(kind) = arg.strip_prefix(b"--type=") {
                    found = String::from_utf8_lossy(kind).into_owned();
                    break;
                }
            }
            if found.is_empty() && raw.split(|b| *b == 0).any(|a| a.starts_with(b"--")) {
                found = "main".to_owned();
            }
            found
        };

        assert_eq!(role(&["signal-desktop", "--type=renderer", "--enable-crashpad"]), "renderer");
        assert_eq!(role(&["signal-desktop", "--type=gpu-process"]), "gpu-process");
        assert_eq!(
            role(&["signal-desktop", "--type=utility", "--utility-sub-type=network.mojom.Net"]),
            "utility",
            "the network service is a utility process, which is why 'keep the main \
             process alive' was the wrong model for a partial freeze"
        );
        assert_eq!(role(&["signal-desktop", "--no-sandbox"]), "main");

        // A plain single-process app has no roles to distinguish, and a column
        // reading `main` on every row of it would be noise.
        assert_eq!(role(&["/usr/bin/alacritty"]), "");
        assert_eq!(role(&[]), "");
    }

    #[test]
    fn an_argument_containing_a_space_does_not_confuse_the_role() {
        // The classic bug in anything that reads `/proc/<pid>/cmdline` as a
        // string. Arguments are NUL-separated, so there is nothing to be confused
        // by — but only if the separator is honoured.
        let raw: Vec<u8> = b"app\0--user-data-dir=/home/a b/c\0--type=renderer\0".to_vec();
        let found = raw
            .split(|b| *b == 0)
            .find_map(|a| a.strip_prefix(b"--type="))
            .map(|k| String::from_utf8_lossy(k).into_owned());
        assert_eq!(found.as_deref(), Some("renderer"));
    }

    #[test]
    fn first_sample_has_no_rate() {
        let mut s = Sampler::new();
        let key = AppKey::comm("self");
        let me = std::process::id();
        assert_eq!(s.sample(&key, &[me], &[], Instant::now(), MINUTE), None);
    }

    #[test]
    fn second_sample_produces_a_rate() {
        let mut s = Sampler::new();
        let key = AppKey::comm("self");
        let me = std::process::id();
        let t0 = Instant::now();
        s.sample(&key, &[me], &[], t0, MINUTE);
        // Do something that costs a few context switches.
        for _ in 0..200 {
            let _ = fs::read_to_string("/proc/self/status");
        }
        let got = s.sample(&key, &[me], &[], t0 + Duration::from_secs(1), MINUTE).unwrap();
        assert!(got.wakeups_per_sec >= 0.0);
        assert!(got.cpu_pct >= 0.0);
    }

    /// Drive the rate maths directly: `n` readings at a fixed interval, each
    /// adding `per_tick` switches and `cpu_per_tick` microseconds.
    fn steady(interval: Duration, n: u32, per_tick: u64, cpu_per_tick: u64) -> ProcSample {
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        let mut last = None;
        for i in 0..n {
            let c = one_process(u64::from(i) * per_tick, u64::from(i) * cpu_per_tick);
            last = s.record(&key, c, t0 + interval * i, MINUTE);
        }
        last.expect("a rate after several readings")
    }

    #[test]
    fn the_reported_rate_does_not_depend_on_how_often_we_sample() {
        // The bug: the interval moves between 60s and 2s according to how close
        // the next grace deadline is (`adaptive_interval`), and the rate was taken
        // over "since last time" — so the same behaviour measured 30x noisier
        // exactly when a decision was imminent. §9's whole premise is that a
        // wakeup rate can be compared against a threshold.
        //
        // 100 switches and 10ms of CPU per second, sampled three different ways.
        let two_sec = steady(Duration::from_secs(2), 60, 200, 20_000);
        let ten_sec = steady(Duration::from_secs(10), 12, 1_000, 100_000);
        let sixty_sec = steady(Duration::from_secs(60), 3, 6_000, 600_000);

        for got in [&two_sec, &ten_sec, &sixty_sec] {
            assert!(
                (got.wakeups_per_sec - 100.0).abs() < 1.0,
                "expected ~100/s, got {}",
                got.wakeups_per_sec
            );
            assert!((got.cpu_pct - 1.0).abs() < 0.1, "expected ~1%, got {}", got.cpu_pct);
        }
    }

    #[test]
    fn a_single_tick_burst_is_diluted_across_the_window() {
        // The observed artifact: one 2s sample caught a burst and reported 3466/s
        // where the app's steady rate was single digits, then went straight back.
        // Over a 60s window the same burst is the burst divided by the window.
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        let mut total = 0u64;

        // Half a minute of near-silence, at the 2s floor.
        for i in 0..15 {
            total += 4;
            s.record(&key, one_process(total, 0), t0 + Duration::from_secs(2 * i), MINUTE);
        }
        // Then 7000 switches in one 2s interval.
        total += 7_000;
        let got =
            s.record(&key, one_process(total, 0), t0 + Duration::from_secs(32), MINUTE).unwrap();

        assert!(
            got.wakeups_per_sec < 300.0,
            "a 2s burst must not read as a sustained rate: {}",
            got.wakeups_per_sec
        );
        // ~7060 switches over the 32s of history available so far.
        assert!(
            got.wakeups_per_sec > 150.0,
            "but it must still be visible: {}",
            got.wakeups_per_sec
        );
    }

    #[test]
    fn an_impossible_reading_is_discarded_and_does_not_poison_the_window() {
        // Taken from a real journal: 4246% CPU on a 16-core machine, beside
        // 198,649 wakeups/s where the 99.9th percentile of every other sample in
        // the file was 7,340. Whatever produced it, the app was not doing it.
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();

        s.record(&key, one_process(0, 0), t0, MINUTE);
        // One interval that claims more CPU-microseconds than there are
        // core-microseconds in it, by three orders of magnitude.
        let absurd = (cpu_ceiling_pct() as u64) * 1_000_000 * 10;
        assert_eq!(
            s.record(&key, one_process(200_000, absurd), t0 + Duration::from_secs(2), MINUTE),
            None,
            "an impossible reading must not be reported as a measurement"
        );

        // And the app starts clean rather than carrying that increment through
        // the rest of the window: a fresh pair of readings reads as itself.
        s.record(&key, one_process(0, 0), t0 + Duration::from_secs(4), MINUTE);
        let got = s
            .record(&key, one_process(200, 2_000_000), t0 + Duration::from_secs(6), MINUTE)
            .expect("two clean readings are a rate again");
        assert!((got.wakeups_per_sec - 100.0).abs() < 1.0, "{}", got.wakeups_per_sec);
        assert!((got.cpu_pct - 100.0).abs() < 1.0, "{}", got.cpu_pct);
    }

    #[test]
    fn readings_older_than_the_window_stop_counting() {
        // Otherwise the window only ever grows and an app is judged on what it
        // did ten minutes ago.
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        let window = Duration::from_secs(10);

        // A big burst, then a long quiet stretch well past the window.
        s.record(&key, one_process(0, 0), t0, window);
        s.record(&key, one_process(100_000, 0), t0 + Duration::from_secs(2), window);
        let mut got = None;
        for i in 1..=10 {
            got = s.record(
                &key,
                one_process(100_000, 0),
                t0 + Duration::from_secs(2 + 5 * i),
                window,
            );
        }
        assert_eq!(got.unwrap().wakeups_per_sec, 0.0, "the burst has aged out of the window");
    }

    #[test]
    fn history_is_bounded_however_wide_the_window() {
        // A pathological `cpu_sample_window` must not turn into unbounded memory.
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        let hour = Duration::from_secs(3600);
        for i in 0..(MAX_READINGS as u32 + 50) {
            s.record(&key, one_process(u64::from(i), 0), t0 + Duration::from_secs(2) * i, hour);
        }
        assert_eq!(s.history[&key].window.len(), MAX_READINGS);
    }

    #[test]
    fn thread_churn_costs_one_interval_not_the_whole_window() {
        // The regression this guards, measured minutes after the window landed:
        // `ctxt_switches_of` sums over `/proc/<pid>/task`, so threads that exit
        // take their counters out of the sum and the *raw* total falls. Taking the
        // difference across a 60s window then clamped the entire window to zero —
        // a wezterm running builds reported `0/s` on 2 of 8 samples while holding
        // 25% CPU.
        //
        // Accumulating per-interval increments keeps the window's total monotonic,
        // so a churn event costs only the interval it happened in.
        let mut s = Sampler::new();
        let key = AppKey::comm("builder");
        let t0 = Instant::now();
        let step = Duration::from_secs(10);

        // 1000 switches per interval, then a fleet of threads exits and the raw
        // sum collapses, then it carries on climbing from the lower base.
        let raw = [0u64, 1_000, 2_000, 3_000, 200, 1_200, 2_200];
        let mut got = None;
        for (i, r) in raw.iter().enumerate() {
            got = s.record(&key, one_process(*r, 0), t0 + step * u32::try_from(i).unwrap(), MINUTE);
        }
        let rate = got.unwrap().wakeups_per_sec;

        // Increments: 1000, 1000, 1000, clamped 0, 1000, 1000 = 5000 over 60s.
        assert!(
            (rate - 5_000.0 / 60.0).abs() < 1.0,
            "expected ~83/s from the increments that were observed, got {rate}"
        );
        assert!(rate > 0.0, "a churn event must not erase the window");
    }

    #[test]
    fn a_counter_reset_reads_as_zero_not_as_a_huge_negative() {
        // An app restarting under the same key resets its counters. Clamping
        // under-reports, which is the safe direction: a low rate means we leave
        // the app alone.
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        s.record(&key, one_process(500_000, 900_000), t0, MINUTE);
        let got = s.record(&key, one_process(10, 5), t0 + Duration::from_secs(2), MINUTE).unwrap();
        assert_eq!(got.wakeups_per_sec, 0.0);
        assert_eq!(got.cpu_pct, 0.0);
    }

    #[test]
    fn forgetting_an_app_drops_its_history() {
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        s.record(&key, one_process(0, 0), t0, MINUTE);
        assert!(s.history.contains_key(&key));
        s.forget(&key);
        assert!(!s.history.contains_key(&key));
        assert_eq!(
            s.record(&key, one_process(0, 0), t0 + Duration::from_secs(2), MINUTE),
            None,
            "and a re-appearing app starts from one point again"
        );
    }

    #[test]
    fn we_can_read_our_own_counters() {
        let me = std::process::id();
        assert!(ctxt_switches_of(me).unwrap() > 0);
        assert!(proc_cpu_usec(me).is_some());
    }

    #[test]
    fn counting_covers_every_thread_not_just_the_leader() {
        // /proc/<pid>/status reports the thread group leader alone. Reading it
        // undercounted every threaded app — measured at 1.5-2x for real desktop
        // processes and 21x for the daemon's own tokio pool.
        let me = std::process::id();
        let leader_only = fs::read_to_string(format!("/proc/{me}/status"))
            .map(|s| sum_ctxt_switches(&s))
            .unwrap();
        // Make sure there is a second thread doing something to be counted.
        std::thread::spawn(|| {
            for _ in 0..500 {
                let _ = fs::read_to_string("/proc/self/status");
            }
        })
        .join()
        .unwrap();

        let all_threads = ctxt_switches_of(me).unwrap();
        assert!(
            all_threads > leader_only,
            "whole process {all_threads} should exceed the leader's {leader_only}"
        );
    }

    #[test]
    fn a_dead_process_has_no_counters_rather_than_zero() {
        // Zero would read as "perfectly idle", which is the one answer that
        // could talk us into freezing something.
        assert_eq!(ctxt_switches_of(u32::MAX), None);
    }

    #[test]
    fn only_the_two_switch_counters_are_summed() {
        let status = "Name:\tslack\n\
                      Threads:\t9\n\
                      voluntary_ctxt_switches:\t120\n\
                      nonvoluntary_ctxt_switches:\t7\n";
        assert_eq!(sum_ctxt_switches(status), 127);
        assert_eq!(sum_ctxt_switches("Threads:\t9\n"), 0);
    }

    #[test]
    fn a_process_name_with_spaces_does_not_break_stat_parsing() {
        // /proc/<pid>/stat puts comm in parens and comm may contain spaces and
        // parens; parsing from the left is the classic bug here.
        let stat =
            "1234 (weird name (x)) S 1 1234 1234 0 -1 4194304 100 0 0 0 42 58 0 0 20 0 1 0 100 0 0";
        let after = &stat[stat.rfind(')').unwrap() + 1..];
        let fields: Vec<&str> = after.split_whitespace().collect();
        assert_eq!(fields[11], "42");
        assert_eq!(fields[12], "58");
    }

    #[test]
    fn membership_changes_are_not_activity() {
        let previous = BTreeMap::from([(1u32, 100u64), (2, 500)]);

        // Both members present and both climbed: ordinary work.
        assert_eq!(increment(&previous, &BTreeMap::from([(1, 150), (2, 550)])), 100);

        // A newcomer arrives holding hours of counters. That is not this
        // interval's work, and it is the whole reason this is done per member.
        assert_eq!(
            increment(&previous, &BTreeMap::from([(1, 150), (2, 500), (3, 9_000_000)])),
            50,
            "a process joining the app must contribute nothing until it has been seen twice"
        );

        // A departure takes its counter with it; that is not negative work.
        assert_eq!(increment(&previous, &BTreeMap::from([(1, 150)])), 50);

        // A restart under a recycled PID resets the counter: clamped, and the
        // under-report is the safe direction.
        assert_eq!(increment(&previous, &BTreeMap::from([(1, 5), (2, 500)])), 0);

        // Nothing in common — every member is new — so there is nothing to say
        // about the interval, rather than everything to say about it.
        assert_eq!(increment(&previous, &BTreeMap::from([(7, 9_000_000)])), 0);
    }

    #[test]
    fn a_process_joining_the_app_is_not_a_burst() {
        // Measured live, and it produced the worst number this tool has printed.
        // A Firefox window opened, six content processes joined the app's PID
        // set, and their *lifetime* counters entered the sum as one interval's
        // work: 253,107 wakeups/s and 5417% CPU on a 16-core machine, sustained
        // across the 60s window rather than corrected on the next sample.
        //
        // The damage was not the sample. `journal::ReportAccumulator` projects
        // the last rate seen before a freeze across the whole hold — it has to,
        // since a frozen app cannot be measured — so that sample became the
        // baseline for a 7h35m hold and `dozectl report` claimed 1,095,308
        // CPU-seconds avoided. The machine only has 445,456 of them in that
        // window. The one number the README asks people to trust was 2.5x the
        // capacity of the hardware.
        let mut s = Sampler::new();
        let key = AppKey::comm("firefox");
        let t0 = Instant::now();
        let step = Duration::from_secs(10);

        // One process ticking over quietly: 100 switches per interval, no CPU.
        for i in 0..6 {
            let quiet = Reading {
                ctxt: BTreeMap::from([(1, 1_000 + i * 100)]),
                cpu: BTreeMap::from([(CpuSource::Pid(1), 0)]),
                gpu: BTreeMap::new(),
            };
            s.record(&key, quiet, t0 + step * u32::try_from(i).unwrap(), MINUTE);
        }

        // A second process joins, carrying hours of accumulated counters: 15M
        // switches and 9000 CPU-seconds of lifetime.
        let joined = Reading {
            ctxt: BTreeMap::from([(1, 1_600), (2, 15_000_000)]),
            cpu: BTreeMap::from([(CpuSource::Pid(1), 0), (CpuSource::Pid(2), 9_000_000_000)]),
            gpu: BTreeMap::new(),
        };
        let got = s.record(&key, joined, t0 + step * 6, MINUTE).unwrap();
        assert!(
            got.wakeups_per_sec < 20.0,
            "a process joining is not 15M wakeups: {}",
            got.wakeups_per_sec
        );
        assert!(got.cpu_pct < 1.0, "nor is it 9000 CPU-seconds: {}", got.cpu_pct);

        // But it is measured from the next reading on — the newcomer costs one
        // interval of its own, not the app's whole number.
        let settled = Reading {
            ctxt: BTreeMap::from([(1, 1_700), (2, 15_006_000)]),
            cpu: BTreeMap::from([(CpuSource::Pid(1), 0), (CpuSource::Pid(2), 9_000_600_000)]),
            gpu: BTreeMap::new(),
        };
        let got = s.record(&key, settled, t0 + step * 7, MINUTE).unwrap();
        assert!(
            got.wakeups_per_sec > 50.0,
            "the newcomer's real work must still be counted: {}",
            got.wakeups_per_sec
        );
        assert!(
            (got.cpu_pct - 1.0).abs() < 0.1,
            "0.6 CPU-seconds over a 60s window is 1%, got {}",
            got.cpu_pct
        );
    }

    #[test]
    fn a_cgroup_joining_the_app_is_not_a_burst() {
        // The same incident on the CPU side: the six processes arrived under a
        // sibling systemd unit, so the app's *cgroup* set grew too and that
        // unit's lifetime `usage_usec` entered the sum. An app spread across
        // sibling cgroups is normal and expected (`read_counters`), which means
        // the set growing is normal — and it is still not work.
        let mut s = Sampler::new();
        let key = AppKey::comm("firefox");
        let t0 = Instant::now();
        let step = Duration::from_secs(10);
        let scope = |n: u32| CpuSource::Cgroup(PathBuf::from(format!("/sys/fs/cgroup/app-{n}")));

        for i in 0..6 {
            let quiet = Reading {
                ctxt: BTreeMap::new(),
                cpu: BTreeMap::from([(scope(1), u64::from(i) * 100_000)]),
                gpu: BTreeMap::new(),
            };
            s.record(&key, quiet, t0 + step * i, MINUTE);
        }
        let joined = Reading {
            ctxt: BTreeMap::new(),
            cpu: BTreeMap::from([(scope(1), 600_000), (scope(2), 9_000_000_000)]),
            gpu: BTreeMap::new(),
        };
        let got = s.record(&key, joined, t0 + step * 6, MINUTE).unwrap();
        assert!(
            got.cpu_pct < 2.0,
            "a sibling unit's lifetime is not one interval: {}",
            got.cpu_pct
        );
    }

    #[test]
    fn losing_the_cgroups_costs_one_interval_rather_than_inventing_one() {
        // `read_counters` is all or nothing about cgroups, so one unreadable
        // cgroup drops the whole app back to the per-PID walk. The two sources
        // count different things from different epochs, so the first reading
        // after a swap has nothing to difference against — which is why they are
        // keyed apart rather than both being "the CPU number".
        let mut s = Sampler::new();
        let key = AppKey::comm("app");
        let t0 = Instant::now();
        let cgroup = CpuSource::Cgroup(PathBuf::from("/sys/fs/cgroup/app"));

        s.record(
            &key,
            Reading {
                ctxt: BTreeMap::new(),
                cpu: BTreeMap::from([(cgroup.clone(), 5_000_000)]),
                gpu: BTreeMap::new(),
            },
            t0,
            MINUTE,
        );
        let swapped = Reading {
            ctxt: BTreeMap::new(),
            cpu: BTreeMap::from([(CpuSource::Pid(1), 4_000_000_000)]),
            gpu: BTreeMap::new(),
        };
        let got = s.record(&key, swapped, t0 + Duration::from_secs(10), MINUTE).unwrap();
        assert_eq!(got.cpu_pct, 0.0, "a change of source is not 4000 CPU-seconds of work");
    }

    #[test]
    fn interval_backs_off_when_there_is_nothing_to_do() {
        assert_eq!(adaptive_interval(false, None, false), Duration::from_secs(60));
        assert_eq!(adaptive_interval(true, None, false), Duration::from_secs(30));
        assert_eq!(
            adaptive_interval(false, Some(Duration::from_secs(90)), false),
            Duration::from_secs(10)
        );
        assert!(
            adaptive_interval(false, Some(Duration::from_secs(5)), false) <= Duration::from_secs(5)
        );
        // Never spin: even an expired deadline has a floor.
        assert!(
            adaptive_interval(false, Some(Duration::ZERO), false) >= Duration::from_millis(500)
        );
    }

    /// The two readings a rate is made of should not take two minutes to
    /// collect. At the idle interval they did, and for that whole time every
    /// threshold rule was deciding against a measurement nobody had taken.
    #[test]
    fn a_daemon_that_has_measured_nothing_takes_its_first_readings_promptly() {
        assert_eq!(adaptive_interval(false, None, true), WARMUP);
        assert_eq!(adaptive_interval(false, Some(Duration::from_secs(300)), true), WARMUP);
        // And it is a ceiling, not a floor: a deadline sooner than the warm-up
        // still wins, because acting on time outranks measuring on time.
        assert_eq!(
            adaptive_interval(false, Some(Duration::from_secs(1)), true),
            Duration::from_secs(1)
        );
        assert!(adaptive_interval(false, Some(Duration::ZERO), true) >= Duration::from_millis(500));

        // It is a warm-up and not a mode: one measured app ends it, because
        // every app is sampled on the same tick and reaches its second reading
        // with the rest.
        assert_eq!(adaptive_interval(false, None, false), Duration::from_secs(60));
    }

    #[test]
    fn any_sample_is_false_until_something_has_two_readings() {
        let mut s = Sampler::new();
        let key = AppKey::comm("slack");
        let t0 = std::time::Instant::now();
        assert!(!s.any_sample(), "a sampler that has read nothing has measured nothing");

        s.record(&key, one_process(100, 0), t0, MINUTE);
        assert!(!s.any_sample(), "one reading is a look, not a measurement");

        s.record(&key, one_process(200, 0), t0 + Duration::from_secs(3), MINUTE);
        assert!(s.any_sample());

        // Monotone: the flag drives a faster tick, so it must not be able to
        // flap back and leave the daemon polling hard forever.
        s.forget(&key);
        s.record(&key, one_process(300, 0), t0 + Duration::from_secs(6), MINUTE);
        assert!(!s.any_sample(), "an app whose history was dropped starts again");
    }

    #[test]
    fn the_third_field_of_schedstat_is_the_one_wanted() {
        // `sum_exec_runtime run_delay pcount`. Read live from this machine:
        // `966911 0 2` — note the middle field is zero, because the
        // `sched_schedstats` sysctl is off, which is the default. The third
        // field is maintained regardless and is the one this reads.
        assert_eq!(parse_pcount("966911 0 2\n"), Some(2));
        assert_eq!(parse_pcount("1101117 0 4823"), Some(4823));
        // Anything that is not three numbers is a kernel that does not answer
        // this question, and answering `0` for it would read as an app that
        // never wakes the CPU.
        assert_eq!(parse_pcount(""), None);
        assert_eq!(parse_pcount("966911 0"), None);
        assert_eq!(parse_pcount("966911 0 not-a-number"), None);
    }

    #[test]
    fn the_two_counters_are_read_from_the_files_they_name() {
        // Both shapes against one fixture directory, so a swap of the two
        // readers would be caught rather than being invisible behind agreeing
        // numbers on a live machine.
        let dir = std::env::temp_dir().join(format!("dozed-counter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("schedstat"), "966911 0 4823\n").unwrap();
        fs::write(
            dir.join("status"),
            "Name:\tdozed\nThreads:\t17\nvoluntary_ctxt_switches:\t4000\n\
             nonvoluntary_ctxt_switches:\t823\n",
        )
        .unwrap();

        assert_eq!(WakeupCounter::Schedstat.read(&dir), Some(4823));
        assert_eq!(WakeupCounter::Status.read(&dir), Some(4823));

        // A thread that exits between the walk and the read is normal and is
        // not a reason to report a zero.
        let gone = dir.join("gone");
        assert_eq!(WakeupCounter::Schedstat.read(&gone), None);
        assert_eq!(WakeupCounter::Status.read(&gone), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unprobed_counter_is_the_one_that_works_everywhere() {
        // `status` is twice the cost and always available; `schedstat` is half
        // the cost and needs a kernel that keeps `pcount`. A caller that never
        // probed — a unit test, a tool linking this module — must get the first,
        // because the failure mode of guessing the second is every app reading
        // zero wakeups a second with nothing anywhere saying so.
        //
        // Pinned here so the assertion holds however the suite is ordered; the
        // first setter wins, and `probe` uses the same `OnceLock`.
        WakeupCounter::pin(WakeupCounter::Status);
        assert_eq!(WakeupCounter::in_force(), WakeupCounter::Status);
        assert_eq!(WakeupCounter::in_force().as_str(), "status");
    }
}
