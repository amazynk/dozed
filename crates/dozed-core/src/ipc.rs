//! The daemon ↔ client protocol.
//!
//! Newline-delimited JSON over a unix socket at `$XDG_RUNTIME_DIR/dozed.sock`,
//! chosen over D-Bus for this path because the GUI wants a firehose of journal
//! events and a socket is simpler to stream and to test (`DESIGN.md` §12).
//!
//! Lives in core so that `dozed` and `dozectl` cannot disagree about it.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app::AppKey;
use crate::config::Mode;
use crate::event::{DesktopState, PowerState, ProcContrib, ProcSample, Visibility};
use crate::explain::RuleOutcome;
use crate::intent::{Action, Outcome, Reason};
use crate::journal::{JournalEntry, Report};
use crate::time::MonoTime;

/// Default socket path, honouring `XDG_RUNTIME_DIR`.
#[must_use]
pub fn socket_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    dir.join("dozed.sock")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "request")]
pub enum Request {
    /// Daemon health, mode, and its own resource cost (`DESIGN.md` §13).
    Status,
    /// Every app dozed knows about, with the last sample taken of each.
    ///
    /// `watching` means the caller has the table on screen and will ask again in
    /// a moment: a standing interest in fresh samples rather than one question.
    /// The daemon answers it by holding the sampling tick down for as long as the
    /// asking continues (`dozed::daemon::WATCHED_SAMPLE`), because §13's back-off
    /// is an argument about a tool nobody is looking at — with nothing held and
    /// nothing pending the tick is 60s, and a dashboard polling every 2s drew the
    /// same wakeup rate thirty times.
    ///
    /// Defaulted to false, which is both the honest answer for a one-shot
    /// `dozectl apps` — it cannot observe a faster tick and should not pay for
    /// one — and what keeps an older client's bare `{"request":"list-apps"}`
    /// decoding.
    ListApps {
        #[serde(default)]
        watching: bool,
    },
    /// Why is this app in the state it is in?
    Why {
        app: String,
        /// Also report every rule that did *not* match, and the predicate that
        /// rejected it. "Why did my rule not fire?" is unanswerable without it.
        #[serde(default)]
        rules: bool,
    },
    /// Past entries, then optionally stream new ones.
    Journal {
        #[serde(default)]
        since: Option<Duration>,
        #[serde(default)]
        follow: bool,
        /// Include entries from previous runs of the daemon. Off by default:
        /// the journal survives restarts, so without this a `watch` opens on
        /// history whose `ELAPSED` column counts from a different zero and
        /// therefore runs backwards partway down the screen.
        #[serde(default)]
        all: bool,
    },
    SetMode {
        mode: Mode,
    },
    /// Stop acting for a while, then go back to whatever mode was in force.
    ///
    /// `None` cancels a snooze in progress, which is a different request from
    /// `SetMode`: cancelling puts back the mode the snooze interrupted, whereas
    /// setting a mode says what it should be from now on.
    Snooze {
        #[serde(default)]
        duration: Option<Duration>,
    },
    /// Act on one app now, overriding the rules.
    Act {
        app: String,
        action: Action,
    },
    /// Limit how far up the ladder one app may be taken, for a while
    /// (`crate::ceiling`). `ceiling: None` clears whatever is in force.
    SetHold {
        app: String,
        #[serde(default)]
        ceiling: Option<crate::ceiling::Ceiling>,
        /// `None` with a ceiling set means "until cleared".
        #[serde(default)]
        duration: Option<Duration>,
    },
    /// Panic thaw. Ignores rules, ignores mode.
    ThawAll,
    Report {
        #[serde(default)]
        since: Option<Duration>,
    },
    ReloadConfig,
    /// Begin a self-trial: alternate live and dry-run blocks and measure the
    /// battery (`crate::trial`). Every field defaults, so `dozectl trial start`
    /// need not have an opinion about any of them.
    StartTrial {
        #[serde(default)]
        pairs: Option<u32>,
        #[serde(default)]
        block: Option<Duration>,
        #[serde(default)]
        seed: Option<u64>,
        #[serde(default)]
        on_battery_only: Option<bool>,
    },
    /// End the trial early, keeping the blocks it has finished.
    StopTrial,
    TrialStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "response")]
pub enum Response {
    Status(Box<StatusReport>),
    /// A struct variant, not `Apps(Vec<_>)`: serde cannot serialise an
    /// internally-tagged newtype variant that wraps a sequence, and it fails at
    /// runtime rather than at compile time.
    Apps {
        apps: Vec<AppSummary>,
        /// What the machine as a whole is doing, so the rows have a
        /// denominator (`crate::coverage`). Defaulted rather than required: a
        /// `dozectl` newer than the daemon it is talking to should print the
        /// table it can build rather than fail to decode the response.
        #[serde(default)]
        machine: crate::coverage::MachineTotals,
    },
    Why(Box<WhyReport>),
    /// One journal entry. A `Journal` request yields a stream of these.
    Entry(Box<JournalEntry>),
    /// Marks the end of the backlog, so a client that did not ask to follow
    /// knows to stop reading.
    EndOfStream,
    Report(Box<Report>),
    /// A struct variant carrying an `Option`, for the same reason `Apps` is a
    /// struct variant: an internally-tagged enum cannot serialise a newtype
    /// variant wrapping certain shapes, and it fails at runtime rather than at
    /// compile time. `None` means no trial has ever been run.
    Trial {
        report: Option<Box<TrialReport>>,
    },
    Acted {
        app: AppKey,
        outcome: Outcome,
    },
    Ok,
    Error {
        message: String,
    },
}

/// The default for a boolean field whose absence must not read as a fault.
const fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    /// The mode actually **in force**, which is not always the configured one.
    ///
    /// `dozed --dry-run` beats `mode = "live"` in the config file, and the shipped
    /// unit passes `--dry-run` — so a config saying `live` produces a daemon with
    /// no actuator at all. Reporting the *configured* mode there printed `mode
    /// live` above `0 held` on a daemon that could not touch anything, which is
    /// the same "one field, two meanings" failure `Session::why` had.
    pub mode: Mode,
    /// The configured mode, present only when it differs from the effective one.
    ///
    /// Carried so the discrepancy can be *explained* rather than merely resolved:
    /// somebody who wrote `mode = "live"` and got dry-run is owed the reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_mode: Option<Mode>,
    /// A trial block is running and the mode is its, not the user's.
    ///
    /// Needed because `configured_mode` cannot see this one. That field compares
    /// the daemon's in-memory `config.mode` against the effective mode — and a
    /// trial changes the mode through `set_mode`, which *rewrites* that very
    /// field. So the two agree, the discrepancy check reports nothing, and the
    /// status line shows a bare `dry-run` on a machine whose config file says
    /// `live`. The mechanism built to explain "you asked for live and got
    /// dry-run" is blind to the one thing in the daemon that most often causes
    /// it.
    ///
    /// A flag rather than a second `Option<Mode>`: the interesting fact is not
    /// which mode the trial wants — that is already on the `mode` line — but
    /// that the answer to "why is it this" is the experiment rather than the
    /// config.
    #[serde(default)]
    pub mode_driven_by_trial: bool,
    /// The mode a trial in progress will put back **between** its blocks, when
    /// one is running. `None` when no trial is under way.
    ///
    /// [`Self::mode_driven_by_trial`] answers "a block is running and owns the
    /// mode right now", which is false for most of a trial's life — blocks are
    /// twenty minutes and the waits between them are hours. That left the other
    /// case unexplained: a daemon resting in its trial's baseline shows a bare
    /// `mode dry-run` with nothing to connect it to the `trial 4/12 blocks` line
    /// further down. The mode being *correct* did not make it legible, and it is
    /// the same invisibility that let a leftover arm go unnoticed for 40 hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_restores: Option<Mode>,
    pub uptime: Duration,
    pub now: MonoTime,
    pub power: PowerState,
    pub desktop: DesktopState,
    /// Whether the compositor helper is still reporting. When false the daemon
    /// is deliberately passive and holds nothing (`DESIGN.md` §10.2).
    pub focus_source_healthy: bool,
    /// Whether the daemon's *own* session-bus connection is delivering.
    ///
    /// The other half of `focus_source_healthy`, and the half that was missing: a
    /// silent focus source is either a helper that went away or a daemon that
    /// stopped being able to hear one, and for 2h14m of real downtime it was the
    /// second while everything on screen said the first.
    ///
    /// Defaults to **true**, which is the only safe default here: `false` would
    /// make a new client report a fault against every older daemon, which is the
    /// same crying-wolf failure this field exists to prevent.
    #[serde(default = "yes")]
    pub session_bus_healthy: bool,
    pub compositor: Option<String>,
    /// Which source we resolved at startup and why. A wrong guess is the most
    /// expensive misconfiguration available — the X11 source under Wayland sees
    /// only XWayland clients, and "no windows" reads as "everything is hidden".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_source: Option<String>,
    pub apps_known: usize,
    pub apps_held: usize,
    /// The daemon's own wakeups per second. A battery tool that cannot show its
    /// own cost is asking to be taken on faith.
    pub self_wakeups_per_sec: f32,
    /// D-Bus property reads a second by the system-bus watcher.
    ///
    /// The other half of the daemon's own cost, and on an idle desktop the larger
    /// half: the sampler backs right off when nothing is pending, so those reads
    /// are most of what dozed does. Shown so that the poll interval is an
    /// engineering decision somebody can check rather than one they have to trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus_calls_per_sec: Option<f32>,
    /// Which `/proc` file the per-thread wakeup counter is read out of —
    /// `"schedstat"` or `"status"` (`dozed::sampler::WakeupCounter`).
    ///
    /// The per-thread sweep is the most expensive thing dozed does, and these
    /// two differ by a factor of two in cost while agreeing to 0.004% in value.
    /// Which one a kernel allows is not something the user chose or can change,
    /// so it is here to explain a cost rather than to offer a knob. `None` from
    /// a daemon predating the choice, which always read `status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wakeup_counter: Option<String>,
    /// Why the record of what is held could not be written, if it could not.
    ///
    /// `DESIGN.md` §10.3 makes that file mandatory rather than prudent: a freeze
    /// outlives the process that applied it, so without it a crash leaves
    /// applications stopped until something sweeps them up. A full disk on a
    /// real machine broke it and said so only in the journal, where it scrolled
    /// past — a safety net drawn and not connected, which is §10's original sin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_file_error: Option<String>,
    pub config_path: Option<String>,
    /// The session recording, if one is running. `None` means `--no-trace`, or
    /// a state directory we could not write to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceStatus>,
    /// One entry per exemption source (`DESIGN.md` §10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exemption_sources: Vec<ExemptionSource>,
    /// How long is left of a snooze, when one is running.
    ///
    /// Reported separately from `mode` rather than folded into it, because they
    /// answer different questions: `mode` says what the daemon is doing, and this
    /// says why it is doing that and when it will stop. A snoozed daemon that
    /// only reported `dry-run` would be indistinguishable from one somebody had
    /// switched off and forgotten — which is the state a snooze exists to avoid
    /// leaving anybody in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_remaining: Option<Duration>,
    /// What the machine's own power counters say right now.
    ///
    /// `None` on hardware that exposes neither a battery nor `cpuidle`. Reported
    /// alongside the daemon's own wakeup rate and for the same reason: a tool
    /// asking to be trusted about power should show the instrument it is reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<EnergyStatus>,
    /// The self-trial, if one is running or has finished (`crate::trial`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<TrialSummary>,
    /// Temperatures and fan speeds, for the quiet objective. `None` on hardware
    /// that exposes no sensors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalStatus>,
}

/// What the thermal sensors say right now.
///
/// Not `Copy`, for the same reason [`EnergyStatus`] is not: [`Self::ignored`]
/// carries the names of the devices whose temperature was deliberately left out
/// of the maximum, and a count would not answer the question those names are on
/// screen to answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalStatus {
    pub state: crate::event::ThermalState,
    /// The hottest sensor, in degrees.
    pub hottest_celsius: Option<f64>,
    /// The fastest fan, in RPM. `None` on a fanless machine, where silence is not
    /// something dozed can improve.
    pub fastest_rpm: Option<u32>,
    /// The state has been `hot` long enough, and quietly enough, that it cannot
    /// be carrying information on this machine — see
    /// `dozed_linux::thermal::Monitor::saturated`.
    ///
    /// Worth a field of its own rather than being left for the user to infer
    /// from the two numbers above: a threshold that can never be reached looks
    /// exactly like a machine that is genuinely always hot, and the difference
    /// decides whether every thermal rule in the file is doing anything.
    /// Defaulted for an older daemon, which could not answer the question.
    #[serde(default)]
    pub saturated: bool,
    /// The fan thresholds in force and where they came from.
    ///
    /// Unless the config pins them these are learned from the speeds this fan
    /// has been seen to use, and a threshold nobody can see is as unexplainable
    /// as a hardcoded one — the whole failure being corrected here was a number
    /// deciding things with nothing on screen to say what it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan: Option<FanStatus>,
    /// hwmon devices whose temperature was excluded from the maximum, because
    /// it describes hardware with its own thermal envelope rather than this
    /// machine — an NVMe drive in its ordinary sixties, a warm radio.
    ///
    /// On screen because the alternative is indistinguishable from the sensor
    /// not existing, and one of those two possibilities is
    /// `dozed_linux::thermal::NOT_THE_MACHINE` being wrong about somebody's
    /// hardware. Empty on an older daemon, which read every sensor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
}

/// What the fan signal is doing, and on what evidence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FanStatus {
    /// The slowest and fastest this fan has been seen at, this run.
    pub observed: Option<(u32, u32)>,
    /// `(hot, cool)` in force. `None` means the fan is out of the decision —
    /// not enough range seen yet, or a fan that does not vary enough to say
    /// anything.
    pub thresholds: Option<(u32, u32)>,
    /// Whether those thresholds came from the config rather than from the fan.
    pub pinned: bool,
}

/// The live reading from the power counters, for `dozectl status`.
///
/// Not `Copy`, unlike everything around it, because [`Self::gpu_label`] is the
/// sensor's own word for what it is measuring and abbreviating it to a `bool`
/// would defeat the point of carrying it (`dozed_linux::energy::Gpu`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergyStatus {
    /// Watts. `None` on mains, where the battery is not paying for anything.
    pub discharge_w: Option<f64>,
    /// CPU package watts from RAPL, on machines where the counter is readable.
    ///
    /// `None` on a stock kernel, where it is `0400 root` — see
    /// `dozed_linux::energy` and the README. Not interchangeable with
    /// [`Self::discharge_w`]: the package is part of the machine, and this is the
    /// half of the answer that survives being plugged in.
    ///
    /// Defaulted, so a client older than this field decodes a daemon that has it.
    #[serde(default)]
    pub package_w: Option<f64>,
    /// The package counter exists and this daemon may not read it.
    ///
    /// Worth a line of a client's output where a missing counter is not: absent
    /// hardware is nothing anybody can act on, and a permission is. It is also the
    /// difference between "your machine cannot do this" and "your machine can, and
    /// one decision away" — and without it the two are the same silence, which is
    /// a silence this project has already spent an afternoon inside.
    #[serde(default)]
    pub package_denied: bool,
    /// Fraction of all CPU-time spent in the deepest idle state, 0.0 to 1.0.
    /// This is the direct physical consequence of the wakeups §13 counts, and
    /// unlike discharge it is measurable while plugged in.
    pub deepest_idle: Option<f64>,
    /// Machine-wide context switches per second, from `/proc/stat`.
    ///
    /// Worth having next to the per-app figures: those are a sum over the apps
    /// dozed knows about, so a large gap between them means dozed is watching the
    /// wrong processes.
    pub system_wakeups_per_sec: Option<f64>,
    pub cpus: usize,
    /// The package's own sub-domains, in watts.
    ///
    /// **Inside [`Self::package_w`], never beside it.** `intel-rapl:0:0` is
    /// `core`, a subset of its parent, and a client that summed a breakdown
    /// alongside its total would report a CPU drawing half again what it draws —
    /// the trap §11.1 names for the package total, which gets worse rather than
    /// better once the parts are on screen and *look* like they should add up.
    /// `dozed_core::energy::PackageBreakdown::rest_w` is the only supported way
    /// to turn these into a remainder.
    ///
    /// Empty on a machine that exposes no sub-domains, and empty from a daemon
    /// too old to have looked — which is the same thing to a renderer, and
    /// correctly so: neither has anything to break down.
    #[serde(default)]
    pub package_parts: crate::energy::PackageBreakdown,
    /// GPU watts, where a driver reports them.
    ///
    /// **Not addable to [`Self::package_w`]**, and this is not the same caution
    /// as the one above — it is a stronger one. On an integrated GPU this is the
    /// *same silicon* as the package, measured twice by two drivers: 24.07 W of
    /// RAPL package against 24.00 W of `amdgpu` PPT on the machine this was
    /// written against. On a discrete card it is a genuinely separate chip. There
    /// is no attribute in `/sys` that tells the two cases apart, so nothing here
    /// tries; the figure is reported under its own label and summed with nothing.
    #[serde(default)]
    pub gpu_w: Option<f64>,
    /// `power1_label` — `PPT` on AMD parts. The reason [`Self`] is not `Copy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_label: Option<String>,
    /// Cores busy machine-wide, from `/proc/stat`'s aggregate `cpu` line.
    ///
    /// The denominator for per-app watts (`crate::attribution`), and worth seeing
    /// on its own beside `deepest_idle`: one is what the cores were doing and the
    /// other is how deeply they slept when they were not.
    #[serde(default)]
    pub machine_cores_busy: Option<f64>,
    /// How much of the GPU's time was spent working, 0.0 to 1.0.
    ///
    /// Beside [`Self::gpu_w`] rather than derived from it: one is what the chip
    /// drew and the other is how much of its time it was doing anything, and on
    /// an integrated GPU the first is mostly a floor while the second is not.
    #[serde(default)]
    pub gpu_busy: Option<f64>,
    /// What the machine's own power response looks like, fitted
    /// (`crate::power_model`).
    ///
    /// Carries the refusals as well as the coefficients, because on the machine
    /// this was written for the refusal *was* the finding: the package sat at a
    /// firmware limit and did not move with load at all, and "no model" would
    /// have said nothing about why.
    ///
    /// `None` from a daemon too old to have fitted one, which is a different
    /// thing again from a daemon that fitted one and refused it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_model: Option<crate::power_model::Verdict>,
}

/// Where a self-trial has got to, in one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialSummary {
    pub blocks_done: usize,
    pub blocks_total: usize,
    /// Which arm is being measured, when a block is running.
    pub arm: Option<String>,
    /// Holding off because the machine is on mains or in use.
    pub waiting: bool,
}

/// The full trial, for `dozectl trial`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialReport {
    pub verdict: crate::trial::Verdict,
    pub blocks: Vec<crate::trial::Block>,
    pub waiting: bool,
    pub arm: Option<String>,
    /// Carried so the client can render the measured saving as a fraction of a
    /// charge without talking to UPower itself — the same reason [`Report`]
    /// carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_wh: Option<f64>,
}

/// One audio stream an app is holding open, and what has been heard on it.
///
/// The pair matters more than either half. A name alone answers "which tab" and
/// leaves "so why is this still counting as audio?" — which is the question
/// somebody asks while looking at an `audio` badge on an app they cannot hear.
/// `silent_for` is the answer: the measurement is running, it agrees with them,
/// and it is partway to the threshold rather than disagreeing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStream {
    /// `media.name`: what the app calls it. A tab title, for a browser.
    pub name: String,
    /// How long it has been continuously measured silent, when it has been.
    ///
    /// `None` covers both "something is coming out of it" and "not measured
    /// yet", which are the same thing to a reader deciding whether to trust the
    /// badge — and which the source health line separates when it matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silent_for: Option<Duration>,
}

/// Whether one exemption source is actually connected, and what it is holding.
///
/// This exists because the failure it detects was real and silent: the shipped
/// `audio-exempt` and `dbus-owner-exempt` rules matched on state nothing ever
/// set, so they could never fire, and nothing anywhere said so. A source that
/// has never reported is a safety net that is drawn but not connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptionSource {
    pub name: String,
    /// Whether a snapshot has ever arrived from this source.
    pub reported: bool,
    /// How many processes currently hold it.
    pub pids: usize,
    /// Why this source cannot work here, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// Why this source sees less than it should, when it still sees something.
    ///
    /// Distinct from `unavailable` because the two call for different actions. A
    /// source that cannot work here is the platform's answer and there is
    /// nothing to do about it; a source running blind because a utility is
    /// missing is a package away from working, and saying so is the difference
    /// between a user installing it and a user never learning that half an
    /// exemption was switched off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

/// Where this run is being recorded and what the recording has cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStatus {
    pub path: String,
    pub bytes: u64,
    pub records: u64,
    /// Why recording stopped, when it did. `None` while healthy.
    ///
    /// Carried because `bytes` above is the evidence that recording is cheap,
    /// and a recorder that broke reports a *smaller* number than one that is
    /// working — so without this the status line reads best exactly when things
    /// have gone worst. Defaulted for an older daemon talking to a newer
    /// `dozectl`, which could only ever have been healthy or absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// How many stretches of this run's trace are missing, because recording
    /// failed and later recovered (`dozed::tracefile::TraceFile::gaps`).
    ///
    /// Separate from [`Self::stopped`] because they answer different questions
    /// and only one of them clears. `stopped` is "is the recorder working right
    /// now", and a disk that frees up makes it `None`; this is "is the file
    /// complete", and nothing makes that true again. A reader who is told only
    /// the first would take a healthy recorder for a whole recording.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gaps: u32,
    /// Records the daemon offered while recording was stopped, and which are
    /// therefore in no file anywhere.
    ///
    /// Counted for as long as a stop lasts, and reset by whatever ends it — the
    /// retry that resumes, or the rotation that gets past a stall at the size
    /// cap — because each of those folds the number into a gap marker in the
    /// file. So this is what is missing from the recording *right now*, whether
    /// or not it is coming back. Reported because "STOPPED: no space left on
    /// device" says a recording ended and not that the run went on for three
    /// more days without it.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub lost: u64,
    /// Which file of this run `path` is, counting from one.
    ///
    /// Carried for the same reason [`Self::stopped`] is. `bytes` above is the
    /// evidence that recording is cheap, and after a rotation it is small for a
    /// reason that has nothing to do with cost — the run has a 64 MiB file
    /// behind it. Absent from every status that has not rotated, and from every older
    /// daemon, which could not — so absent **defaults to one**, not to zero.
    /// Defaulting to zero would have made the skip lossy, which is a thing a
    /// wire format does not get to be: the field has to come back the way it
    /// went out, and an omitted `part` has a known value rather than no value.
    #[serde(default = "first_part", skip_serializing_if = "is_first_part")]
    pub part: u32,
    /// Whether [`Self::stopped`] is a condition the daemon is trying to recover
    /// from on its own.
    ///
    /// On the wire rather than inferred from the reason text, because a client
    /// guessing at that got it wrong and printed a promised retry for a day on a
    /// real machine. Every stop the current daemon can report is in fact
    /// retried — the size cap rotates, and the one thing that still stops a run
    /// is a file the filesystem refused to open, which is the world refusing and
    /// so is tried again like any other. The field stays because an older daemon
    /// can still answer `false`, and because "is it coming back" is not a
    /// question a client should be answering for itself.
    #[serde(default)]
    pub retrying: bool,
}

/// For `skip_serializing_if`: the ordinary case is no gaps at all, and a field
/// that is absent from every healthy status is one an older `dozectl` never has
/// to think about.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// For `skip_serializing_if`: an unrotated run is every run until a 64 MB file
/// happens, and `1` says nothing a reader needs.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_first_part(n: &u32) -> bool {
    *n == 1
}

/// And its inverse for `default`, so the omission means what it meant.
const fn first_part() -> u32 {
    1
}

const fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// See [`AppSummary::sampled`]: absent means an older daemon, whose samples
/// were always real ones.
const fn sampled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSummary {
    pub key: AppKey,
    pub label: String,
    pub visibility: Visibility,
    /// What the daemon believes is in force.
    pub applied: Action,
    /// Per-app mode, which may differ from the global one.
    pub mode: Mode,
    pub sample: ProcSample,
    /// Whether `sample` is a measurement at all.
    ///
    /// A rate needs two readings, and with nothing held or pending the sampler
    /// ticks once a minute (`sampler::adaptive_interval`), so for the first
    /// minute or two of a run an app has no sample and `ProcSample::default()`
    /// is what is there — a pair of zeros indistinguishable from an app that
    /// really is idle. Reported live, that reads as "dozed is broken": every
    /// row zero on a machine doing 40,000 wakeups a second.
    ///
    /// The same distinction `journal_row` already draws with `-` versus `0`,
    /// for the same reason: collapsing "not measured" into "measured zero" is
    /// worse than showing nothing.
    ///
    /// Defaults to **true** so that a `dozectl` newer than its daemon keeps
    /// rendering that daemon's samples as the measurements they are.
    #[serde(default = "sampled_by_default")]
    pub sampled: bool,
    pub pids: usize,
    pub exempt: Vec<String>,
    /// A hand-set limit on how far this app may be taken, and how long is left
    /// of it (`crate::ceiling`).
    ///
    /// Sent as the rendered phrase rather than the structure, because every
    /// consumer wants the same sentence — "capped only, 42m left" — and a
    /// countdown computed client-side would drift against the daemon's own
    /// clock, which is the mistake `PendingChange` already documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_off: Option<String>,
    /// This app can never be acted on, whatever the rules say.
    ///
    /// Session infrastructure (`AppClass::System`, from
    /// `dozed_linux::guard::NEVER_FREEZE`) or an ancestor of the daemon itself —
    /// `planner::decide` refuses it on its first check, before the rule table is
    /// consulted at all.
    ///
    /// Sent because it was the one thing a client could not work out and the one
    /// thing somebody seeing `plasmashell` in the list needs to know. Before
    /// this, a protected app arrived with `applied = Restore` and rendered as
    /// **running**, indistinguishable from an ordinary app that simply had not
    /// been dozed yet: measured on a live session, 84 consecutive `safety`
    /// refusals for `plasmashell` and not one of them visible anywhere a user
    /// looks. §10's rule is that anything the daemon cannot do, it says so, and
    /// a permanent refusal is a stranger thing to keep quiet about than a
    /// temporary one.
    ///
    /// Carries the *predicate the planner uses*, not a class the client has to
    /// re-interpret, so the display cannot come to a different conclusion than
    /// the decision. Defaults to false, which is the honest answer from a daemon
    /// too old to send it — and the safe direction, since a client that wrongly
    /// showed an ordinary app as untouchable would be the more misleading error.
    #[serde(default)]
    pub never_actionable: bool,
    /// The audio streams this app currently holds open, and what has been heard
    /// on them.
    ///
    /// For a browser the names are tab titles, and that is the point:
    /// `audio-silent` says a process is holding a stream and making no sound
    /// with it, and the next question is always *which tab*. Empty for the
    /// overwhelming majority of apps, and empty against a daemon too old to
    /// send it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_streams: Vec<AudioStream>,
    /// The processes contributing most of this app's wakeups, busiest first.
    ///
    /// Sent with the summary rather than fetched on demand because it is
    /// computed from reads the sampler already performs, and because the
    /// question it answers — "which of these 68 processes is it?" — is the one
    /// a reader has the moment they see a number they did not expect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_processes: Vec<ProcContrib>,
    /// What is going to happen to this app on its own, and how long from now.
    ///
    /// `None` for an app with nothing pending, which is most of them: no rule
    /// matches it, or it is already in the state its rule wants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingChange>,
}

/// A change the planner intends to make to an app that has not happened yet.
///
/// Sent as a **remaining duration** rather than as the planner's `MonoTime`
/// deadline, and that is the whole design of this field: `MonoTime` is the
/// daemon's own monotonic clock, which a client has no way to convert and no
/// business guessing at. Subtracted at the instant the reply is built, so
/// whatever a client does with it, it is counting down from a number the daemon
/// vouched for.
///
/// What it is not is a promise. Grace is a product of the power state, what the
/// user is doing and the resume cooldown (`DESIGN.md` §8), so a keystroke moves
/// this out and unplugging moves it in — it is what happens if nothing else
/// changes. Clients say so where they show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingChange {
    /// How long from now, as of this reply.
    pub within: Duration,
    /// The rule that is counting down.
    pub rule: String,
    /// What that rule will do. The rule's own action rather than an
    /// [`Action`], because `throttle-to` picks its cap from the sample it sees
    /// when it fires: "throttle towards 20 wakeups/s" is a thing that can be
    /// said in advance and "throttle to 7%" is not.
    pub action: crate::config::RuleAction,
}

impl PendingChange {
    /// The countdown in a phrase: `dozes in 2m14s`.
    ///
    /// Rendered here rather than in each client for the same reason
    /// [`crate::config::RuleAction::describe`] is: the dashboard's column and
    /// `dozectl apps`' notes are the same sentence about the same field, and
    /// two copies of it drift.
    #[must_use]
    pub fn summary(&self) -> String {
        let verb = self.action.coming();
        // Under a second the deadline is a tick the daemon has not reached yet,
        // and `HumanDuration` would print it as `640ms` — a precision this
        // number does not have, on a figure the client re-reads every two
        // seconds anyway.
        if self.within < Duration::from_secs(1) {
            return format!("{verb} any moment");
        }
        format!("{verb} in {}", crate::time::HumanDuration(self.within))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhyReport {
    pub key: AppKey,
    pub label: String,
    pub visibility: Visibility,
    pub applied: Action,
    pub mode: Mode,
    /// What the planner concluded just now, in its own words.
    pub reason: Reason,
    /// The grace derivation, e.g. "20m (base 5m x battery 1.0 x activity 4.0)".
    pub grace: Option<String>,
    pub elapsed: Duration,
    /// Every identifier we saw, so a narrower rule can be written.
    pub evidence: String,
    pub cgroup: Option<String>,
    /// Every rule and why it did or did not match. Sent only when the client
    /// asks, because on a large rule table it dwarfs the rest of the report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleOutcome>,
    /// The processes contributing most of this app's wakeups, busiest first.
    ///
    /// `dozectl why` is where somebody has already decided a number is
    /// surprising, so it is the place the follow-up question — "which of these
    /// seventeen processes is it" — is actually asked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_processes: Vec<ProcContrib>,
}

/// Encode one message as a protocol line.
pub fn encode<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Reason;

    #[test]
    fn requests_roundtrip() {
        for req in [
            Request::Status,
            Request::ThawAll,
            Request::Why { app: "slack".into(), rules: false },
            Request::Why { app: "slack".into(), rules: true },
            Request::SetMode { mode: Mode::Live },
            Request::Journal { since: Some(Duration::from_secs(3600)), follow: true, all: false },
            Request::Act { app: "slack".into(), action: Action::Freeze { reclaim: false } },
            Request::ListApps { watching: false },
            Request::ListApps { watching: true },
        ] {
            let line = encode(&req).unwrap();
            assert!(line.ends_with('\n'), "protocol is newline-delimited");
            assert!(!line[..line.len() - 1].contains('\n'), "no embedded newlines");
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        }
    }

    /// `ListApps` grew a field, and a client is not upgraded in lockstep with the
    /// daemon it talks to — a `dozectl` from the distribution package against a
    /// `dozed` built from source, most obviously. The field defaults, so the older
    /// spelling has to keep decoding, and to the reading it always had.
    #[test]
    fn a_client_that_predates_the_watching_flag_still_decodes() {
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"request":"list-apps"}"#).unwrap(),
            Request::ListApps { watching: false },
            "and the default is the one that costs the daemon nothing"
        );
    }

    /// The countdown a client prints, so the dashboard column and `dozectl
    /// apps`' notes cannot say different things about the same field.
    #[test]
    fn a_pending_change_reads_as_a_sentence() {
        let pending = |within, action| PendingChange { within, rule: "bg-idle".into(), action };

        assert_eq!(
            pending(Duration::from_secs(134), crate::config::RuleAction::Freeze).summary(),
            "dozes in 2m14s"
        );
        assert_eq!(
            pending(Duration::from_secs(45), crate::config::RuleAction::Throttle(5)).summary(),
            "throttles in 45s",
            "the cap belongs to the rule, not to the countdown"
        );
        assert_eq!(
            pending(Duration::from_secs(45), crate::config::RuleAction::ThrottleTo(20)).summary(),
            "throttles in 45s",
            "and `throttle-to` cannot name its cap in advance at all"
        );
        // A deadline the daemon has not got round to yet. `HumanDuration` would
        // print `640ms`, which is a precision this number does not have.
        assert_eq!(
            pending(Duration::from_millis(640), crate::config::RuleAction::Freeze).summary(),
            "dozes any moment"
        );
    }

    /// A dashboard from before the field existed must not be handed one, and a
    /// daemon with nothing pending must not put an empty one on the wire.
    #[test]
    fn a_summary_without_a_countdown_says_nothing_about_one() {
        let mut app = AppSummary {
            key: AppKey::comm("slack"),
            label: "slack".into(),
            visibility: Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::DryRun,
            sample: ProcSample::default(),
            sampled: true,
            pids: 1,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let quiet = serde_json::to_string(&app).unwrap();
        assert!(!quiet.contains("pending"), "nothing pending is nothing sent: {quiet}");
        assert_eq!(serde_json::from_str::<AppSummary>(&quiet).unwrap().pending, None);

        // And the same reply from a daemon that does have one, so the field is
        // known to survive the round trip rather than only to be omitted.
        app.pending = Some(PendingChange {
            within: Duration::from_secs(90),
            rule: "bg-idle".into(),
            action: crate::config::RuleAction::Freeze,
        });
        let counting = serde_json::to_string(&app).unwrap();
        assert_eq!(serde_json::from_str::<AppSummary>(&counting).unwrap(), app);
    }

    #[test]
    fn optional_fields_may_be_omitted() {
        // Hand-written client requests should not need every field.
        let r: Request = serde_json::from_str(r#"{"request":"journal"}"#).unwrap();
        assert_eq!(r, Request::Journal { since: None, follow: false, all: false });
    }

    #[test]
    fn a_client_that_predates_a_field_gets_the_narrow_default() {
        // The two binaries are versioned together but not restarted together: a
        // `dozectl` from before `all` existed keeps working against a newer
        // daemon, and must get the *smaller* answer rather than the whole
        // backlog. Defaults on this protocol have to fail towards less.
        let old = r#"{"request":"journal","since":null,"follow":true}"#;
        assert_eq!(
            serde_json::from_str::<Request>(old).unwrap(),
            Request::Journal { since: None, follow: true, all: false }
        );
    }

    /// Every variant, because an internally-tagged enum can fail to serialise
    /// at *runtime* depending on the shape of what a variant wraps. A partial
    /// test here let `Apps(Vec<_>)` ship broken: encoding returned an error, the
    /// server dropped the connection, and the client printed nothing at all.
    #[test]
    fn every_response_variant_roundtrips() {
        let app = AppSummary {
            key: AppKey::comm("slack"),
            label: "slack".into(),
            visibility: Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::DryRun,
            sample: ProcSample::default(),
            sampled: true,
            pids: 3,
            exempt: vec!["audio".into()],
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let status = StatusReport {
            mode: Mode::DryRun,
            configured_mode: Some(Mode::Live),
            mode_driven_by_trial: false,
            trial_restores: None,
            uptime: Duration::from_secs(5),
            now: MonoTime::ZERO,
            power: PowerState::default(),
            desktop: DesktopState::IdlePresent,
            focus_source_healthy: true,
            session_bus_healthy: true,
            compositor: Some("kwin".into()),
            focus_source: Some("helper (kwin): Wayland session".into()),
            apps_known: 1,
            apps_held: 0,
            snooze_remaining: None,
            self_wakeups_per_sec: 0.4,
            bus_calls_per_sec: Some(0.2),
            wakeup_counter: Some("schedstat".into()),
            frozen_file_error: None,
            config_path: None,
            trace: Some(TraceStatus {
                path: "/run/user/1000/traces/1700000000.jsonl".into(),
                bytes: 4096,
                records: 91,
                part: 1,
                stopped: None,
                gaps: 0,
                lost: 0,
                retrying: false,
            }),
            exemption_sources: vec![
                ExemptionSource {
                    name: "audio".into(),
                    reported: true,
                    pids: 1,
                    unavailable: None,
                    degraded: None,
                },
                ExemptionSource {
                    name: "clipboard-owner".into(),
                    reported: false,
                    pids: 0,
                    unavailable: Some("not observable on Wayland".into()),
                    degraded: None,
                },
            ],
            energy: Some(EnergyStatus {
                discharge_w: Some(6.4),
                package_w: Some(3.1),
                package_denied: false,
                deepest_idle: Some(0.82),
                system_wakeups_per_sec: Some(1_400.0),
                cpus: 16,
                package_parts: crate::energy::PackageBreakdown {
                    core_w: Some(2.2),
                    ..Default::default()
                },
                gpu_w: Some(13.0),
                gpu_label: Some("PPT".into()),
                machine_cores_busy: Some(1.2),
                gpu_busy: Some(0.21),
                power_model: Some(crate::power_model::Verdict::PowerLimited {
                    watts: 24.2,
                    cores_range: 9.5,
                }),
            }),
            trial: Some(TrialSummary {
                blocks_done: 3,
                blocks_total: 12,
                arm: Some("live".into()),
                waiting: false,
            }),
            thermal: Some(ThermalStatus {
                state: crate::event::ThermalState::Hot,
                hottest_celsius: Some(99.4),
                fastest_rpm: Some(4473),
                saturated: false,
                fan: None,
                ignored: vec!["nvme".into()],
            }),
        };
        let why = WhyReport {
            key: AppKey::comm("slack"),
            label: "slack".into(),
            visibility: Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::DryRun,
            reason: Reason::new(crate::intent::ReasonKind::NoRule, "no rule matches"),
            grace: Some("5m".into()),
            elapsed: Duration::from_secs(1),
            evidence: "comm=slack".into(),
            cgroup: None,
            rules: vec![RuleOutcome {
                name: "audio-exempt".into(),
                priority: 10,
                clause: Some(crate::explain::Clause::When),
                mismatch: Some(crate::explain::Mismatch::Exemption {
                    which: crate::event::Exemption::Audio,
                    want: true,
                }),
                winner: false,
            }],
            top_processes: vec![ProcContrib {
                pid: 4242,
                comm: "signal-desktop".into(),
                wakeups_per_sec: 2.5,
                role: "renderer".into(),
            }],
        };
        let entry = JournalEntry {
            at: MonoTime::ZERO,
            wall: 0,
            app: AppKey::comm("slack"),
            action: Action::Restore,
            reason: Reason::new(crate::intent::ReasonKind::NoRule, "x"),
            outcome: Outcome::WouldHave,
        };

        let trial = TrialReport {
            verdict: crate::trial::Trial::new(crate::trial::TrialPlan::default(), Mode::DryRun)
                .verdict(),
            blocks: vec![crate::trial::Block {
                pair: 0,
                arm: crate::trial::Arm::Live,
                samples: 18,
                discharge_w: Some(5.4),
                package_w: Some(11.8),
                deepest_idle: Some(0.88),
                system_wakeups_per_sec: Some(900.0),
                discarded: None,
            }],
            waiting: false,
            arm: Some("live".into()),
            battery_wh: Some(60.0),
        };

        for r in [
            Response::Status(Box::new(status)),
            // A populated `machine`, not the default: `Option` fields skipped
            // when `None` are exactly where an internally-tagged enum breaks,
            // and a variant that only ever round-trips its empty case is not
            // tested.
            Response::Apps {
                apps: vec![app],
                machine: crate::coverage::MachineTotals {
                    wakeups_per_sec: Some(3_900.0),
                    cpus: 16,
                    sample_window: Duration::from_secs(60),
                    basis: crate::attribution::Basis::of(Some(2.2), Some(24.1), Some(1.2)),
                    gpu_busy: None,
                },
            },
            Response::Why(Box::new(why)),
            Response::Entry(Box::new(entry)),
            Response::EndOfStream,
            Response::Report(Box::default()),
            Response::Trial { report: Some(Box::new(trial)) },
            // Both inhabitants of the `Option`: `None` is what a daemon that has
            // never run a trial answers, and it has to survive the wire too.
            Response::Trial { report: None },
            Response::Acted { app: AppKey::comm("slack"), outcome: Outcome::Applied },
            Response::Ok,
            Response::Error { message: "nope".into() },
        ] {
            let line = encode(&r).unwrap_or_else(|e| panic!("{r:?} failed to encode: {e}"));
            assert_eq!(serde_json::from_str::<Response>(&line).unwrap(), r);
        }
    }
}
