//! Everything the outside world can tell the planner.
//!
//! Sources produce [`Event`]s and nothing else. That keeps the planner honest
//! (it cannot reach out and read a file mid-decision) and makes a recorded
//! event stream a complete, replayable description of a session.

use serde::{Deserialize, Serialize};

use crate::app::{AppClass, AppKey, IdentityEvidence};
use crate::config::Config;
use crate::intent::Action;

/// How much of an app the user can currently see.
///
/// Focus alone is the wrong signal: on a second monitor a window is unfocused
/// but fully visible, and freezing it leaves a stale rectangle on screen that
/// reads as a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Visibility {
    /// Holds input focus.
    Focused,
    /// On screen, but not focused.
    VisibleUnfocused,
    /// Another workspace, minimised, or fully occluded.
    Hidden,
}

impl Visibility {
    #[must_use]
    pub const fn is_on_screen(self) -> bool {
        matches!(self, Self::Focused | Self::VisibleUnfocused)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Focused => "focused",
            Self::VisibleUnfocused => "visible-unfocused",
            Self::Hidden => "hidden",
        }
    }
}

/// What the human at the keyboard is doing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum DesktopState {
    /// Input within the activity threshold. Grace is stretched, because during
    /// heavy task switching a freeze that gets undone 20s later is a net loss.
    Active,
    /// No input, but the user is still here (unlocked, screen on).
    #[default]
    IdlePresent,
    /// Locked, screen off, or the session is not the active VT. Nobody is
    /// switching windows, so acting immediately costs nothing.
    Away,
}

impl DesktopState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::IdlePresent => "idle-present",
            Self::Away => "away",
        }
    }
}

/// How hard the machine is working, thermally.
///
/// A second objective, and one that applies where the battery one deliberately
/// does not: §7's policy backs off on AC because a plugged-in machine has no
/// endurance to win — but it can still be *loud*, and a fan at 4500 RPM is a cost
/// somebody is paying whether or not the wall is supplying the power.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ThermalState {
    /// Quiet, and not thermally limited.
    #[default]
    Normal,
    /// The fan is audible or the silicon is near its limit.
    Hot,
}

impl ThermalState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Hot => "hot",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PowerState {
    pub on_ac: bool,
    /// Battery charge, if there is a battery at all.
    pub percentage: Option<f32>,
}

impl Default for PowerState {
    fn default() -> Self {
        // A desktop with no battery reports as on-AC, and under the default
        // `on_ac = "enabled"` policy that is a machine dozed works on like any
        // other — at `ac_factor` patience, since there is no charge to race.
        // It used to mean the opposite: with the old `disabled` default a
        // desktop got nothing from the tool, ever, which is the wrong answer
        // for the machine most likely to leave a browser open for a week.
        Self { on_ac: true, percentage: None }
    }
}

/// A resource sample for one app, already aggregated over its process tree.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ProcSample {
    /// Percent of a single core, summed across the tree.
    pub cpu_pct: f32,
    /// Context switches per second: our proxy for how often this app wakes the
    /// CPU, which is what actually costs battery.
    pub wakeups_per_sec: f32,
    /// Percent of one GPU engine, summed across this app's DRM clients.
    ///
    /// The one per-app figure in this struct that is neither a proxy nor an
    /// apportionment: the kernel counts engine nanoseconds per client and
    /// `dozed_linux::gpu` reads them. It can exceed 100 for the reason
    /// [`Self::cpu_pct`] can — a chip has more than one engine, and `gfx`,
    /// `compute`, `enc` and `dec` are all counted.
    ///
    /// `None` for an app that holds no DRM client at all, which is most of them,
    /// and `None` on a machine with no readable GPU. Not zero: "did no GPU work"
    /// and "cannot do GPU work" are the distinction this codebase keeps drawing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_pct: Option<f32>,
}

/// One process's share of an app's wakeups.
///
/// An app is a tree, and "which app is noisy" is rarely the question anyone
/// actually has — a terminal reported 3137 wakeups/s while `wezterm-gui` itself
/// was doing 70, because 68 processes had been launched from it. The number is
/// only actionable once you can see which process it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcContrib {
    pub pid: u32,
    /// `/proc/<pid>/comm`, read only for the few processes actually reported.
    pub comm: String,
    pub wakeups_per_sec: f32,
    /// What this process *is* within its app, when the app is a Chromium or
    /// Electron one: `renderer`, `gpu-process`, `utility`, `zygote`, or `main`.
    ///
    /// Added because `comm` does not distinguish them and the breakdown was
    /// therefore useless for exactly the apps that need it most. Measured on a
    /// real Signal: **all seventeen processes report `comm = signal-desktop`**, so
    /// the answer to "which of these is it" was seventeen identical rows. With
    /// the role, the same reading says renderer 42%, utility 39%, gpu 14%,
    /// main 6%.
    ///
    /// Empty for an app that is not built this way, and for older daemons.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
}

/// A reason an app must be left alone right now, independent of any rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exemption {
    /// Producing audio (MPRIS `Playing`, or an active PipeWire output stream)
    /// that something could actually be heard in.
    Audio,
    /// Holding an output stream that has been **measurably silent** long enough
    /// to stop counting as audio.
    ///
    /// The one variant here that exempts nothing, and it is in this enum anyway
    /// because it is the reason [`Audio`](Self::Audio) is *absent*. "Has an
    /// uncorked stream" was never the same question as "is making a sound":
    /// measured on this machine, a Firefox holding a call tab reported
    /// `Corked: no` with a peak of exactly zero over four seconds, and would
    /// have been exempt for as long as the tab stayed open.
    ///
    /// Nothing shipped matches on it, and `audio_silent = true` is available as
    /// a predicate for anyone who wants to. It exists mainly so that an app
    /// dozed *despite* holding an audio stream can say why, which is the
    /// difference between this behaviour being debuggable and being spooky.
    AudioSilent,
    /// Holds an idle inhibitor: the app is explicitly asking to stay awake.
    Inhibitor,
    /// Fullscreen, which usually means a game or a video.
    Fullscreen,
    /// Owns a well-known D-Bus name. Freezing it makes every caller block for
    /// the 25s D-Bus timeout.
    DbusNameOwner,
    /// Owns the current clipboard selection; freezing it hangs the next paste.
    ClipboardOwner,
    /// Classified as a build, backup, sync or VM: background CPU is the job.
    LongRunningJob,
    /// Capturing the microphone or the camera.
    ///
    /// Distinct from [`Audio`](Self::Audio), which is about *output*, and the
    /// distinction is not academic: the two are detected from opposite ends of
    /// the stack and one can hold without the other. A dictation tool, a voice
    /// recorder and OBS all capture while playing nothing at all.
    ///
    /// It is also the exemption with an audience. Freezing an app that is
    /// producing sound is heard by the user, who can fix it; freezing one that
    /// is capturing is heard by whoever is on the other end of the call, and
    /// they cannot.
    Capture,
    /// Sharing or recording the screen through the desktop portal.
    ///
    /// Separate from [`Capture`](Self::Capture) because the source is different
    /// — the portal, not the sound server — and so is its availability, which
    /// `dozectl status` has to be able to report honestly per source.
    ScreenCast,
    /// Was on screen when a screen share began, and so might be *in* it.
    ///
    /// [`ScreenCast`](Self::ScreenCast) protects the app doing the capturing.
    /// This protects the app being captured, which is a different app and the
    /// one the user actually cares about: you share a window, switch to another,
    /// and the shared one is now hidden, idle-looking, and to every rule in the
    /// config an excellent freeze candidate. Freezing it does not stop the
    /// share — it freezes the *picture*, and the people watching see a still
    /// frame with no way to tell you why.
    ///
    /// **This one is a guess, and it is the only exemption here that is.** The
    /// portal says who asked for a session; what is inside that session is not
    /// answerable from it, because a `Start` reply names PipeWire nodes and
    /// there is no route from a node back to a window or a PID. So the one fact
    /// available is used instead: what was on screen at the moment the session
    /// opened. Whatever is being shared was almost certainly among it.
    ///
    /// It errs wide, deliberately — every visible app at that instant, usually
    /// one to three, held only for as long as a session is open. The cost of
    /// being wrong in that direction is some battery during a call; the cost of
    /// being wrong in the other is a frozen window in somebody else's meeting,
    /// which they can see and the user cannot.
    ///
    /// What it does not catch is stated rather than left to be found: a window
    /// that was *not* on screen when the share began and is shared anyway. Some
    /// portal backends list occluded and minimised windows in the picker, so
    /// this is reachable, and no signal available here would notice it.
    MaybeShared,
}

impl Exemption {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::AudioSilent => "audio-silent",
            Self::Inhibitor => "inhibitor",
            Self::Fullscreen => "fullscreen",
            Self::DbusNameOwner => "dbus-name-owner",
            Self::ClipboardOwner => "clipboard-owner",
            Self::LongRunningJob => "long-running-job",
            Self::Capture => "capture",
            Self::ScreenCast => "screen-cast",
            Self::MaybeShared => "maybe-shared",
        }
    }

    /// Whether holding this is a reason to leave an app alone.
    ///
    /// True for every variant but [`AudioSilent`](Self::AudioSilent), which is
    /// an observation about why `Audio` is not held. The distinction matters
    /// wherever a list of these is shown to somebody: "exempt: audio-silent"
    /// says the opposite of what it means.
    #[must_use]
    pub const fn exempting(self) -> bool {
        !matches!(self, Self::AudioSilent)
    }

    pub const ALL: [Self; 10] = [
        Self::Audio,
        Self::AudioSilent,
        Self::Inhibitor,
        Self::Fullscreen,
        Self::DbusNameOwner,
        Self::ClipboardOwner,
        Self::LongRunningJob,
        Self::Capture,
        Self::ScreenCast,
        Self::MaybeShared,
    ];
}

/// A newly observed application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSeen {
    pub key: AppKey,
    pub evidence: IdentityEvidence,
    pub class: AppClass,
    pub pids: Vec<u32>,
    /// cgroup v2 path relative to the mount point, when the app lives in a
    /// tidy scope of its own.
    pub cgroup: Option<String>,
    /// Session infrastructure or an ancestor of the daemon: never actionable.
    pub protected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "event")]
pub enum Event {
    /// An app was discovered, or its process set changed. Boxed because it
    /// dwarfs every other variant, and `Tick` is the one we send most often.
    AppSeen(Box<AppSeen>),
    /// An app's last process exited.
    AppGone { app: AppKey },
    /// Visibility changed for one app. The compositor helper is the source.
    VisibilityChanged { app: AppKey, visibility: Visibility },
    /// The user asked the compositor to raise an app that may be frozen. We
    /// must thaw on this, not merely on focus, or single-instance D-Bus
    /// activation will hang against a frozen process.
    ActivationRequested { app: AppKey },
    /// A fresh resource sample.
    Sampled { app: AppKey, sample: ProcSample },
    /// An exemption appeared or disappeared.
    ExemptionChanged { app: AppKey, exemption: Exemption, active: bool },
    /// Input-idle tier changed.
    ActivityChanged { desktop: DesktopState },
    /// The machine got hot or cooled down (`ThermalState`).
    ThermalChanged { thermal: ThermalState },
    /// AC/battery state changed.
    PowerChanged { power: PowerState },
    /// The compositor helper's heartbeat stopped, or came back. Losing it means
    /// we can no longer see focus, so we must not hold anything frozen.
    FocusSourceHealth { healthy: bool },
    /// logind `PrepareForSleep`. `true` = about to sleep, `false` = just woke.
    PrepareForSleep { sleeping: bool },
    /// Config was reloaded from disk.
    ConfigChanged { config: Box<Config> },
    /// The global mode changed — `dozectl mode live`, or a self-trial block
    /// boundary (`crate::trial`).
    ///
    /// An event rather than a field the daemon pokes, for two reasons. It is on
    /// the record, so a trace spanning a switch replays as the session it was.
    /// And the planner has to *hear* it: a dry-run "freeze" advances the same
    /// state machine a live one does (`Planner::note_outcome`), so on going live
    /// the planner already believes those apps are held and suppresses the intent
    /// that would actually hold them. Switching to live mode froze nothing at all
    /// until this existed.
    ModeChanged { mode: crate::config::Mode },
    /// An explicit request from the CLI or GUI, which overrides rules.
    UserAction { app: AppKey, action: Action },
    /// Panic thaw: restore everything, ignore all rules.
    ThawAll,
    /// Daemon is shutting down: restore everything.
    Shutdown,
    /// Periodic nudge so grace deadlines can expire.
    Tick,
}

impl Event {
    /// The app this event concerns, if it concerns exactly one.
    #[must_use]
    pub fn app(&self) -> Option<&AppKey> {
        match self {
            Self::AppSeen(seen) => Some(&seen.key),
            Self::AppGone { app }
            | Self::VisibilityChanged { app, .. }
            | Self::ActivationRequested { app }
            | Self::Sampled { app, .. }
            | Self::ExemptionChanged { app, .. }
            | Self::UserAction { app, .. } => Some(app),
            _ => None,
        }
    }
}
