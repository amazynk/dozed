//! Configuration: tuning knobs and the rule table.
//!
//! Rules are structured TOML with first-match-wins by explicit priority, and
//! deliberately not a scripting language. That constraint is what makes
//! `dozectl why` possible: every decision maps to exactly one named rule, so
//! the answer is a rule name rather than a stack trace.
//!
//! Unknown keys are rejected everywhere. A typo in a predicate name would
//! otherwise silently widen a rule to match everything, and the failure mode of
//! that is a frozen desktop.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app::{AppClass, AppKey};
use crate::event::{DesktopState, Visibility};
use crate::time::HumanDuration;

/// Whether intents are executed or merely recorded.
///
/// The planner never reads this: it is the actuator's business (see
/// `DESIGN.md` §3). Both modes consume an identical intent stream, which is
/// what stops dry-run from drifting away from live behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Record what we would have done, touch nothing. The default; there is no
    /// way to enter live mode by accident.
    #[default]
    DryRun,
    /// Execute intents.
    Live,
    /// Per-app only: force dry-run for one app while the rest run live.
    Monitor,
}

impl Mode {
    /// Whether an actuator in this mode should actually touch the app.
    #[must_use]
    pub const fn acts(self) -> bool {
        matches!(self, Self::Live)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Live => "live",
            Self::Monitor => "monitor",
        }
    }
}

/// What to do while the machine is plugged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcPolicy {
    /// Act on AC too, with `battery.ac_factor` for patience.
    ///
    /// The default, and the reason is that a watt is a watt. The charger does
    /// not *pay* for a background app's wakeups any more than the battery does
    /// — it passes them to the wall, and something upstream burns fuel for
    /// them. Endurance is the most *visible* reason to stop an idle app waking
    /// the CPU 900 times a second; it was never the only one.
    ///
    /// What made this a real change rather than a philosophical one is where
    /// laptops spend their time. Plugged in at a desk is most of the working
    /// day for most machines, so `disabled` meant the tool was switched off for
    /// the majority of the hours it was installed for — and every desktop, which
    /// has no battery at all, got nothing from it ever.
    ///
    /// Patience rather than parity: `ac_factor` doubles the grace on mains, so
    /// a plugged-in machine is acted on later than one at 40% charge and much
    /// later than one at 10%. §8's ladder is unchanged; this only stops it
    /// terminating in "and on AC, nothing".
    #[default]
    Enabled,
    /// Do nothing on AC.
    ///
    /// The opt-out for anyone who wants the tool to be strictly about their own
    /// runtime. Note that rules naming `thermal` or `other_app_fullscreen`
    /// ignore it ([`Rule::acts_on_ac`]): noise and dropped frames are paid in
    /// full while plugged in, so silencing those was never what this setting
    /// was for.
    Disabled,
}

/// How hard dozed tries, as one word.
///
/// The knob most people want and the only one they should have to think about.
/// Everything else in `[defaults]` is a dial on one specific mechanism; this is
/// the question those dials are usually being asked to answer — *how soon should
/// it act* — and asking it directly beats making somebody derive it from four
/// numbers whose interactions they cannot see.
///
/// Deliberately **one term of the grace product** (§8) and nothing else. It
/// would have been easy to make each level also flip `reclaim_on_freeze`, or
/// swap throttles for freezes, or shorten the sample window — and the result
/// would be a word that changes five things at once, which is exactly the kind
/// of setting nobody can predict or explain. `dozectl why` prints the factor in
/// the derivation (`5m x battery 1.0 x activity 4.0 x aggression 0.4`), so the
/// effect of the word is visible in arithmetic rather than described in a manual.
///
/// The multipliers are round numbers chosen for the *outcome* they produce on
/// the shipped `bg-idle` rule, not tuned: what a user picks a level by is "how
/// long until my hidden browser is frozen", so the levels are spaced to give
/// answers that differ obviously — a quarter of an hour, five minutes, two
/// minutes, three quarters of a minute.
/// `Eq` is deliberately absent: [`Aggression::Custom`] carries an `f64` and
/// there is no honest total equality on one. `PartialEq` is what the config
/// round-trip and the GUI's selectable values need, and it is all they need.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Aggression {
    /// Three times the patience. For a machine where being surprised once is
    /// worse than the power saved.
    Gentle,
    /// The shipped behaviour, and the baseline the other three are named
    /// against.
    #[default]
    Balanced,
    /// Acts in about a third of the time. A reasonable place to end up after
    /// reading a week of `dozectl report` and deciding it is being too polite.
    Aggressive,
    /// Acts almost as soon as a window is hidden.
    ///
    /// Honest about the trade: at this level you will sometimes alt-tab back to
    /// an app that is thawing, and chat apps will reconnect more often. It is
    /// the right setting for a machine left running unattended, and the wrong
    /// one for the laptop you are actively working on — `active_factor` still
    /// applies, but a fifteenth of five minutes is twenty seconds even after it.
    Maximum,
    /// A multiplier chosen by hand, for when none of the four words is the
    /// answer — "a bit more patient than aggressive" has no name, and the gap
    /// between `aggressive` (0.4) and `maximum` (0.15) is the widest in the set.
    ///
    /// Written in the file as the number itself, `aggression = 0.75`, because
    /// that is exactly what it is: the same term of the same product the four
    /// words feed. A tagged form would have implied it was a different kind of
    /// setting.
    ///
    /// Bounded by [`Aggression::CUSTOM_MIN`] and [`Aggression::CUSTOM_MAX`]
    /// rather than free: this multiplies a grace, so zero means "act with no
    /// grace at all" and a negative one means a duration that runs backwards.
    /// Neither is a preference, and both are better refused at parse time than
    /// discovered by an application vanishing.
    Custom(f64),
}

impl Aggression {
    /// The most patient custom factor accepted. Beyond `gentle`, but finite:
    /// ten times a five-minute grace is fifty minutes, by which point the app
    /// is not being managed, it is being ignored.
    pub const CUSTOM_MAX: f64 = 10.0;
    /// The most impatient. A twentieth of five minutes is fifteen seconds,
    /// which is already below `maximum` and close enough to "freeze it the
    /// moment I look away" that nothing under it would behave differently.
    pub const CUSTOM_MIN: f64 = 0.05;

    /// The grace multiplier. One term of the product in [`crate::grace`].
    #[must_use]
    pub const fn factor(self) -> f64 {
        match self {
            Self::Gentle => 3.0,
            Self::Balanced => 1.0,
            Self::Aggressive => 0.4,
            Self::Maximum => 0.15,
            Self::Custom(factor) => factor,
        }
    }

    /// The word for this level. `Custom` answers `"custom"`, which is its name
    /// in the UI and never how it is written to a file — see the `Serialize`
    /// impl, which writes the number.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gentle => "gentle",
            Self::Balanced => "balanced",
            Self::Aggressive => "aggressive",
            Self::Maximum => "maximum",
            Self::Custom(_) => "custom",
        }
    }

    /// A hand-chosen factor, or why it is not one.
    ///
    /// The error text is the message a user sees from `dozed --check`, so it
    /// says what to do rather than what went wrong.
    ///
    /// # Errors
    /// If `factor` is not finite, or falls outside
    /// [`CUSTOM_MIN`](Self::CUSTOM_MIN)`..=`[`CUSTOM_MAX`](Self::CUSTOM_MAX).
    pub fn custom(factor: f64) -> Result<Self, String> {
        if !factor.is_finite() {
            return Err(format!(
                "aggression must be a number between {} and {}, and `{factor}` is not a number",
                Self::CUSTOM_MIN,
                Self::CUSTOM_MAX
            ));
        }
        if factor < Self::CUSTOM_MIN || factor > Self::CUSTOM_MAX {
            return Err(format!(
                "aggression {factor} is outside {}..={}; it multiplies every grace in the file, \
                 so below {} nothing would ever get a grace worth the name and above {} nothing \
                 would ever be acted on",
                Self::CUSTOM_MIN,
                Self::CUSTOM_MAX,
                Self::CUSTOM_MIN,
                Self::CUSTOM_MAX
            ));
        }
        Ok(Self::Custom(factor))
    }

    /// One of the four words, or why it is not one.
    ///
    /// # Errors
    /// If `word` is not a level. `"custom"` gets its own message, because
    /// somebody writing it has the right idea and the wrong syntax.
    pub fn from_word(word: &str) -> Result<Self, String> {
        match word {
            "gentle" => Ok(Self::Gentle),
            "balanced" => Ok(Self::Balanced),
            "aggressive" => Ok(Self::Aggressive),
            "maximum" => Ok(Self::Maximum),
            "custom" => Err("`aggression = \"custom\"` has no factor to apply; write the \
                             number itself, as in `aggression = 0.75`"
                .to_owned()),
            other => Err(format!(
                "unknown aggression `{other}`; expected gentle, balanced, aggressive, maximum, \
                 or a number such as 0.75"
            )),
        }
    }
}

/// A word for the four named levels, a bare number for a custom one.
///
/// Hand-written rather than derived because neither serde representation fits:
/// the default one would write `Custom` as `{ custom = 0.75 }`, and `untagged`
/// writes unit variants as `null` — which would have turned every existing
/// `aggression = "balanced"` into a parse error on upgrade.
impl Serialize for Aggression {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Custom(factor) => serializer.serialize_f64(*factor),
            named => serializer.serialize_str(named.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for Aggression {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AnyLevel;

        impl serde::de::Visitor<'_> for AnyLevel {
            type Value = Aggression;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("gentle, balanced, aggressive, maximum, or a number such as 0.75")
            }

            fn visit_str<E: serde::de::Error>(self, word: &str) -> Result<Aggression, E> {
                Aggression::from_word(word).map_err(E::custom)
            }

            fn visit_f64<E: serde::de::Error>(self, factor: f64) -> Result<Aggression, E> {
                Aggression::custom(factor).map_err(E::custom)
            }

            // `aggression = 1` is an integer to TOML, and refusing it because a
            // user did not type `1.0` would be pedantry with an error message.
            fn visit_i64<E: serde::de::Error>(self, factor: i64) -> Result<Aggression, E> {
                Aggression::custom(factor as f64).map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, factor: u64) -> Result<Aggression, E> {
                Aggression::custom(factor as f64).map_err(E::custom)
            }
        }

        // `deserialize_any`, so the same impl reads a string from TOML and a
        // number from JSON without either format needing to be told which.
        deserializer.deserialize_any(AnyLevel)
    }
}

const fn d_base_grace() -> HumanDuration {
    HumanDuration::secs(300)
}
const fn d_sample_window() -> HumanDuration {
    HumanDuration::secs(60)
}
const fn d_resume_cooldown() -> HumanDuration {
    HumanDuration::secs(120)
}
const fn d_resume_cooldown_factor() -> f64 {
    2.0
}
const fn d_active_threshold() -> HumanDuration {
    HumanDuration::secs(30)
}
const fn d_active_factor() -> f64 {
    4.0
}
const fn d_away_factor() -> f64 {
    0.0
}
const fn d_low_pct() -> f32 {
    30.0
}
const fn d_low_factor() -> f64 {
    0.5
}
/// Twice the patience while plugged in.
///
/// Fourfold was tried first and was too much: measured on a live session it put
/// Signal at a 32-minute grace and left nothing at all held after seven
/// minutes, which is not "more patient on AC" but "dormant on AC". Doubling
/// keeps the distinction — the point of the tool is endurance, and a plugged-in
/// machine has none to win — without the tool effectively switching itself off
/// whenever the laptop is on a desk.
const fn d_ac_factor() -> f64 {
    2.0
}
const fn d_priority() -> u32 {
    100
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    /// How soon to act, in one word. See [`Aggression`].
    ///
    /// First field of the struct because it is the first thing to reach for, and
    /// `toml::to_string_pretty` writes fields in declaration order — so it is
    /// also the first line of `[defaults]` in the file somebody opens.
    pub aggression: Aggression,
    /// Grace before acting, before the battery and activity factors apply.
    pub base_grace: HumanDuration,
    /// Averaging window for CPU and wakeup samples.
    pub cpu_sample_window: HumanDuration,
    /// Ask the kernel to page out a frozen app after freezing it.
    pub reclaim_on_freeze: bool,
    /// Allow freezing a window that is on screen but unfocused. Off by default:
    /// a frozen visible window is a stale rectangle, which reads as a crash.
    pub freeze_visible: bool,
    pub on_ac: AcPolicy,
    /// An app the user returned to within this window is clearly in their
    /// active rotation, so it gets extra grace next time it is hidden.
    pub resume_cooldown: HumanDuration,
    pub resume_cooldown_factor: f64,
    /// How the cooldown is applied. See [`CooldownMode`].
    pub resume_cooldown_mode: CooldownMode,
}

/// How `resume_cooldown` protects an app the user has just come back to.
///
/// Both modes read the same two settings; the difference is whether the cooldown
/// can be cancelled out by the other grace factors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CooldownMode {
    /// `resume_cooldown` is a *minimum wait* after a restore: whatever the other
    /// factors say, an app is not acted on again until it has passed.
    /// `resume_cooldown_factor` still stretches the ordinary grace on top.
    ///
    /// The default, because the factor alone could be annihilated. Grace is a
    /// product — `base x battery x activity x cooldown` (§8) — and
    /// `away_factor = 0.0` makes the whole product zero, so while the screen was
    /// locked the cooldown bought exactly nothing. Replaying a real locked
    /// session showed a browser restored on unlock and re-frozen 81 seconds
    /// later, inside a cooldown of two minutes that was nominally in force. A
    /// minimum wait is what "do not re-doze something the user just came back
    /// to" actually means: a condition on time since an event, not a multiplier
    /// on a different clock.
    #[default]
    Floor,
    /// `resume_cooldown_factor` multiplies the grace and nothing else — the
    /// original §8 model.
    ///
    /// Kept because it is a legitimate preference: it acts sooner, and while the
    /// screen is locked nobody is waiting on the app. Opting in accepts that a
    /// lock/unlock/lock cycle re-dozes immediately, paying a thaw and, for a
    /// chat app, a reconnect each time.
    Factor,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            aggression: Aggression::Balanced,
            base_grace: d_base_grace(),
            cpu_sample_window: d_sample_window(),
            reclaim_on_freeze: false,
            freeze_visible: false,
            on_ac: AcPolicy::Enabled,
            resume_cooldown: d_resume_cooldown(),
            resume_cooldown_factor: d_resume_cooldown_factor(),
            resume_cooldown_mode: CooldownMode::Floor,
        }
    }
}

/// The anti-thrash knob. During active work the user alt-tabs constantly, and
/// freezing an app they return to twenty seconds later is a net loss: the thaw
/// costs a redraw and often a reconnect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ActivityConfig {
    /// Input more recent than this means [`DesktopState::Active`].
    pub active_threshold: HumanDuration,
    pub active_factor: f64,
    /// Applied when the user is gone (locked, screen off, VT switched). Zero
    /// means act immediately, which costs nothing because nobody is switching
    /// windows — this is where most of the saving comes from.
    pub away_factor: f64,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            active_threshold: d_active_threshold(),
            active_factor: d_active_factor(),
            away_factor: d_away_factor(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BatteryConfig {
    /// **No longer used.** Grace on battery is proportional to charge
    /// (`crate::grace`), which pins the curve to the origin and to full charge
    /// and leaves these two nothing to describe.
    ///
    /// Kept so that existing config files still parse — `deny_unknown_fields`
    /// would otherwise turn an upgrade into a startup failure — and reported by
    /// [`Config::warnings`] when set to anything other than the old defaults,
    /// because a knob that silently does nothing is worse than one that is
    /// gone.
    pub low_pct: f32,
    /// **No longer used.** See `low_pct`.
    pub low_factor: f64,
    /// Grace multiplier while plugged in, when `on_ac = enabled` — which is the
    /// default, so this is the ordinary path rather than an opt-in one
    /// ([`grace`](crate::grace) explains why acting on mains is worth doing).
    ///
    /// Acting on AC should not mean acting *exactly as hard* as on battery:
    /// plugged in there is no endurance to win, so the only thing left to
    /// optimise is not getting in the way. Before this existed the factor was
    /// 1.0 in both states and the two were indistinguishable, so a machine on
    /// mains was treated as urgently as one at 40% charge, chasing a runtime
    /// saving that does not exist there.
    pub ac_factor: f64,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self { low_pct: d_low_pct(), low_factor: d_low_factor(), ac_factor: d_ac_factor() }
    }
}

/// When an open audio stream stops counting as audio.
///
/// `playing_audio` used to mean "holds an uncorked sink-input", and that is a
/// weaker claim than it looks. Measured on this machine: a Firefox with a call
/// tab open held a stream reporting `Corked: no` at 100% volume whose peak was
/// **exactly zero** across four seconds of monitoring. Nothing was coming out,
/// nothing had come out for hours, and the browser was exempt the whole time —
/// which is the single most expensive false exemption there is, because a
/// browser is the app most worth dozing.
///
/// So the sound server is asked what is actually audible, not merely what is
/// open. The measurement is deliberately conservative in one direction: silence
/// has to be *sustained* and *measured* before it counts, and anything that
/// cannot be measured stays exempt.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AudioConfig {
    /// Whether to measure at all.
    ///
    /// Off means the old behaviour exactly: an uncorked stream is audio, and
    /// `audio-silent` never fires. Worth having as a switch because this is the
    /// one exemption the daemon *narrows*, and narrowing a safety net should be
    /// something you can turn off without editing rules.
    pub detect_silence: bool,
    /// How long a stream must be continuously silent before it stops counting.
    ///
    /// Not zero, and not seconds: the gap between two tracks, a video buffering,
    /// and a call with nobody talking are all real silence in a stream that is
    /// very much in use. A minute of it is not.
    pub silence_after: HumanDuration,
    /// How often to measure, while there is anything to measure.
    ///
    /// Nothing at all happens when no app holds an output stream, which is most
    /// of the day — this is not a timer the daemon runs, it is a timer that runs
    /// while something is playing (`DESIGN.md` §13).
    pub probe_every: HumanDuration,
    /// How long each measurement listens for.
    ///
    /// The window is the honest limit of the whole mechanism: sound that falls
    /// entirely between two windows is not heard. Long enough to catch anything
    /// continuous, short enough that the cost is a rounding error.
    pub probe_window: HumanDuration,
    /// Peak amplitude, in dBFS, at or below which a window counts as silent.
    ///
    /// The default is close enough to digital silence that only a stream writing
    /// zeros — or something 10,000 times quieter than the quietest thing you
    /// could hear with the volume at maximum — is below it. A gentler threshold
    /// would start catching quiet music, and the cost of that mistake is an app
    /// frozen mid-track.
    pub silence_dbfs: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            detect_silence: true,
            silence_after: HumanDuration::secs(60),
            probe_every: HumanDuration::secs(15),
            probe_window: HumanDuration(std::time::Duration::from_millis(750)),
            silence_dbfs: -80.0,
        }
    }
}

/// When this machine counts as thermally busy: the **quiet** objective (§7.3).
///
/// Configurable because the shipped numbers are a *measurement*, and a
/// measurement is a fact about one machine on one day. They were taken from a
/// laptop whose fan sat at 0 RPM idle and reached 4484 under a build. On a
/// machine whose fan idles at 3161 — the same laptop, some months later — a
/// `hot_rpm` of 3000 sits below the entire operating range and the state is
/// pinned at `hot` forever, which makes every rule keyed on `thermal`
/// unconditional. That is not a threshold anyone can be expected to guess, and
/// it is not one the daemon may silently retune, so it is a setting.
///
/// ## Both ends of a signal have to move together
///
/// The trap here is worth stating, because it is the obvious edit and it does
/// nothing. Raising `hot_rpm` alone stops the fan *raising* the alarm and leaves
/// it unable to *clear* one: clearing requires `cool_temp && cool_fan`, and
/// `cool_fan` is `rpm < cool_rpm`, so a fan permanently above `cool_rpm` blocks
/// every recovery the temperature would otherwise allow. To take a signal out of
/// the decision, **both** of its thresholds have to move past the range it
/// actually occupies — that is what reproduces the fanless machine's
/// "contributes nothing in either direction".
///
/// [`Config::warnings`] checks for exactly that shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ThermalConfig {
    /// Hot at or above this temperature, in degrees celsius.
    pub hot_celsius: f64,
    /// Back to normal only below this one. Hysteresis, not a second name for the
    /// same number: a sensor hovering at a single threshold would otherwise
    /// produce a stream of transitions, and every one is a re-plan and possibly
    /// a thaw and a re-freeze.
    pub cool_celsius: f64,
    /// Hot at or above this fan speed, in RPM.
    ///
    /// **Unset by default, and that is the design rather than an omission.** A
    /// temperature is comparable across machines and a fan speed is not: 80 °C
    /// is 80 °C on any silicon, while 3000 RPM is idle on one machine and
    /// redline on another. Any number shipped here would be a rule about one
    /// manufacturer's fan wearing the costume of a general setting — and the
    /// first one tried was exactly that, pinning a machine at `hot` for 2,977 of
    /// 3,125 decisions because its fan idles above the threshold.
    ///
    /// Left unset, the daemon learns the range this fan actually uses and puts
    /// the thresholds at fractions of it (`dozed_linux::thermal::FanRange`),
    /// which is a claim that means the same thing on every machine. Set it only
    /// if you know your hardware and want to override that — and then set
    /// `cool_rpm` as well, because half a pair is worse than neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_rpm: Option<u32>,
    /// Back to normal only below this one. Unset alongside `hot_rpm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cool_rpm: Option<u32>,
}

impl Default for ThermalConfig {
    fn default() -> Self {
        Self {
            // Degrees are absolute and mean the same thing everywhere, so these
            // two can honestly be shipped: 80 °C is a CPU working hard but not
            // in trouble, on anybody's silicon.
            hot_celsius: 80.0,
            cool_celsius: 72.0,
            // Fan speeds are not, so they are learned rather than shipped.
            hot_rpm: None,
            cool_rpm: None,
        }
    }
}

/// What the daemon puts on screen for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiConfig {
    /// Whether to show the tray indicator.
    ///
    /// On by default, and that default is a deliberate exception to this
    /// daemon's usual reticence: something that can stop your applications
    /// should be visible without being asked for, and the panic thaw should be
    /// one click from wherever you are. Off is for people who keep an empty tray
    /// on purpose, and for desktops where the indicator looks wrong.
    ///
    /// Costs nothing either way — the icon redraws only when what it shows
    /// changes — so this is a matter of taste rather than of overhead.
    pub tray: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { tray: true }
    }
}

/// One app pattern or a list of them: `app = "slack"` and
/// `app = ["slack", "discord"]` both work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Patterns {
    One(String),
    Many(Vec<String>),
}

impl Patterns {
    #[must_use]
    pub fn matches(&self, key: &AppKey) -> bool {
        match self {
            Self::One(p) => key.matches_pattern(p),
            Self::Many(ps) => ps.iter().any(|p| key.matches_pattern(p)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        match self {
            Self::One(p) => std::slice::from_ref(p).iter(),
            Self::Many(ps) => ps.iter(),
        }
        .map(String::as_str)
    }
}

/// A numeric bound. `wakeups_per_sec = 50` is shorthand for
/// `wakeups_per_sec = { gt = 50 }`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Threshold {
    Above(f32),
    Bounds {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gt: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lt: Option<f32>,
    },
}

impl Threshold {
    #[must_use]
    pub fn allows(self, v: f32) -> bool {
        match self {
            Self::Above(t) => v > t,
            Self::Bounds { gt, lt } => gt.is_none_or(|t| v > t) && lt.is_none_or(|t| v < t),
        }
    }

    /// Whether this bound can never be satisfied, which is always a config bug.
    #[must_use]
    pub fn is_empty(self) -> bool {
        match self {
            Self::Above(_) => false,
            Self::Bounds { gt, lt } => match (gt, lt) {
                (Some(g), Some(l)) => g >= l,
                (None, None) => true,
                _ => false,
            },
        }
    }
}

/// Renders the way it would be written in the config file, so an explanation
/// can be compared against the rule by eye.
impl std::fmt::Display for Threshold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Above(t) => write!(f, "> {t}"),
            Self::Bounds { gt: Some(g), lt: Some(l) } => write!(f, "> {g} and < {l}"),
            Self::Bounds { gt: Some(g), lt: None } => write!(f, "> {g}"),
            Self::Bounds { gt: None, lt: Some(l) } => write!(f, "< {l}"),
            // Rejected by `Config::validate`, so unreachable from a parsed file.
            Self::Bounds { gt: None, lt: None } => f.write_str("(no bound)"),
        }
    }
}

/// The conditions under which a rule applies.
///
/// The same type backs both `match` (identity) and `when` (dynamic state); the
/// split is documentation, not semantics, and both must hold.
///
/// `for` is the exception: it is not a boolean predicate but the base grace for
/// the rule, fed through the factors in [`crate::grace`] by the planner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "snake_case")]
pub struct Predicates {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<Patterns>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<AppClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_state: Option<DesktopState>,
    /// How long the other predicates must have held. Base grace for this rule.
    #[serde(rename = "for", skip_serializing_if = "Option::is_none")]
    pub for_: Option<HumanDuration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_below: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wakeups_per_sec: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playing_audio: Option<bool>,
    /// Holds an output stream that has been measurably silent for
    /// `audio.silence_after`.
    ///
    /// The complement of `playing_audio` rather than its negation: an app with
    /// no stream at all is neither. This is the browser with a call tab open —
    /// uncorked, at full volume, writing zeros for hours — and the shipped rules
    /// deliberately do **not** mention it, so a silent stream is simply not a
    /// reason to leave anything alone. Match on it to say something narrower,
    /// e.g. a longer leash for an app that plays in bursts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_silent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_inhibitor: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
    /// Owns a well-known D-Bus name that other processes call. Freezing one of
    /// these blocks every caller for the 25s D-Bus timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owns_dbus_name: Option<bool>,
    /// Capturing the microphone or the camera.
    ///
    /// The other half of `playing_audio`, and not implied by it: a dictation
    /// tool, a voice recorder or a muted call participant captures while
    /// producing no sound at all. Freezing one of those is the failure the user
    /// cannot see and the person on the other end can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capturing: Option<bool>,
    /// **Doing** the sharing or recording, through the desktop portal.
    ///
    /// The app that asked the portal for a screencast session: OBS, Spectacle,
    /// a browser sharing a tab in a call. Freezing one of those stops the
    /// recording or freezes the picture everyone else is watching, and a screen
    /// recorder is a strong freeze candidate precisely because most of them hide
    /// their own window while they work.
    ///
    /// **Not** the app being *captured*, which is a different question and one
    /// the portal cannot answer: a `Start` reply names PipeWire nodes, not
    /// windows or processes, so there is nothing to map back to an `AppKey`.
    /// See [`Self::maybe_shared`], which is the best available answer to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_casting: Option<bool>,
    /// Was on screen when a screen share began, and so might be *in* it.
    ///
    /// The other side of [`Self::screen_casting`]: that one protects the app
    /// doing the capturing, this one the app being captured. The hazard is the
    /// common one — you share a window, switch to another, and the shared
    /// window is now hidden and looks like an ideal freeze candidate. Freezing
    /// it freezes the picture the other people on the call are watching.
    ///
    /// **This is the one predicate here that is a guess**, because nothing on
    /// the portal answers "what is in this session". It matches every app that
    /// was visible at the instant a session opened, for as long as any session
    /// stays open, which is usually one to three apps for the length of a call.
    /// It is shipped as an ordinary rule rather than hard-coded for exactly that
    /// reason: an invariant may be a certainty, and this is an inference, so
    /// somebody who would rather have the battery must be able to say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maybe_shared: Option<bool>,
    /// How hard the machine is working thermally: the **quiet** objective.
    ///
    /// `thermal = "hot"` reads as "while the fan is audible or the silicon is at
    /// its limit". Nothing to do with endurance, which is why a rule carrying it
    /// is exempt from the AC policy — see [`Rule::acts_on_ac`]. A plugged-in
    /// laptop has no battery to save and can still be loud.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal: Option<crate::event::ThermalState>,
    /// Whether some *other* app is fullscreen: the **latency** objective.
    ///
    /// `other_app_fullscreen = true` reads as "while a game, a video or a call has
    /// the screen". There the cost of a background app is not battery but a
    /// dropped frame, so this too is exempt from the AC policy: nobody plugs in a
    /// laptop to make a game stutter.
    ///
    /// It can never match the fullscreen app itself (`WorldState::fullscreen_elsewhere`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_app_fullscreen: Option<bool>,
    /// Seconds the app held focus in the visit that just ended.
    ///
    /// `focused_for = { lt = 10 }` reads as "the user glanced at it": alt-tabbed
    /// across, looked, and left. That is a different situation from an app the
    /// user has actually been working in, and it is the one case where re-dozing
    /// quickly is right — the app was already dozing a moment ago and the user
    /// has just demonstrated they do not want it.
    ///
    /// A rule carrying this predicate is **exempt from the resume cooldown**
    /// (§8). The cooldown exists because "an app the user returned to is clearly
    /// in their active rotation"; a three-second visit is direct evidence
    /// against that premise for this visit, so the two would otherwise
    /// contradict each other and the cooldown, being a floor, would always win.
    ///
    /// Unset on an app that has never been focused, which never matches — an app
    /// that was never visited cannot have been briefly visited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_for: Option<Threshold>,
}

impl Predicates {
    /// Whether this clause constrains nothing, i.e. matches every app in every
    /// state. Allowed, but only worth having on a low-priority catch-all.
    #[must_use]
    pub fn is_unconstrained(&self) -> bool {
        *self == Self::default()
    }
}

/// What a matching rule wants done.
///
/// Externally tagged, so TOML reads as `action = "freeze"` and
/// `action = { throttle = 5 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleAction {
    /// Leave this app alone. A rule-level exemption.
    Never,
    /// Cap at `pct` percent of one core. The right first rung for anything
    /// network-facing: it stays connected and still notifies, it just cannot
    /// spin.
    Throttle(u8),
    /// Cap wherever it needs to be so the app costs about this many wakeups a
    /// second, adjusting from measurement instead of guessing a percentage
    /// (`crate::throttle`).
    ThrottleTo(u32),
    /// Stop the app completely.
    Freeze,
    /// Freeze it, but let it up for air on a schedule shared with every other
    /// app doing the same.
    ///
    /// The rung this design was missing. The name is borrowed from Android's
    /// *Doze*, and Doze's saving comes as much from **batching** as from
    /// suppression: hold everything, then release all the deferred work at once,
    /// in a maintenance window. §7's ladder had only the suppression half.
    ///
    /// What it buys over [`Self::Freeze`] is a chat app that can be frozen at all.
    /// §7 caps those at `throttle = 5` instead, because a frozen one drops its
    /// connection and reconnects on thaw — so the choice was between paying a
    /// reconnect on every hide, and never really stopping the app. A window makes
    /// it one reconnect per period for an app that is genuinely stopped the rest
    /// of the time.
    ///
    /// What it buys over waking each app on its own timer is the whole point:
    /// **the windows are aligned**, so N apps produce one burst of wakeups instead
    /// of N independent ones. That is timer coalescing at the desktop level, and it
    /// is why the schedule in `crate::planner` is global rather than per-app.
    ///
    /// Whether it actually beats `throttle = 5` on a given machine is an empirical
    /// question, and `dozectl trial` is how to answer it (§11.2).
    DozeCycle {
        /// How often the app comes up for air. The *global* schedule ticks at the
        /// shortest `every` in force, and each app joins the tick after its own
        /// period has elapsed — which is what aligns a 60m app's window with a
        /// 15m one's instead of scattering them.
        every: HumanDuration,
        /// How long it stays up. Long enough to reconnect and drain a queue;
        /// short enough that the app is stopped for almost all of the period.
        wake: HumanDuration,
    },
}

impl RuleAction {
    /// What this rule does, in a phrase.
    ///
    /// Here rather than in each client because there are now three places that
    /// need it — the config editor's rule table, `dozectl`, and the dashboard's
    /// countdown — and a rung described three ways is a rung described wrongly
    /// somewhere.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Never => "never touch".to_owned(),
            Self::Throttle(pct) => format!("throttle to {pct}%"),
            Self::ThrottleTo(w) => format!("throttle to ~{w} wakeups/s"),
            Self::Freeze => "freeze".to_owned(),
            Self::DozeCycle { every, wake } => format!("doze, waking {wake} every {every}"),
        }
    }

    /// The verb for something that has not happened yet: "dozes in 2m".
    ///
    /// Deliberately not derived from [`crate::Action::verb`], which names what
    /// the actuator was told to do. This names what a *rule* is about to ask
    /// for, and the two differ where it matters most: `throttle-to` picks its
    /// cap from the sample it sees when it fires, so the only honest thing to
    /// say in advance is that a cap is coming.
    #[must_use]
    pub const fn coming(self) -> &'static str {
        match self {
            Self::Freeze | Self::DozeCycle { .. } => "dozes",
            Self::Throttle(_) | Self::ThrottleTo(_) => "throttles",
            // A `never` rule decides immediately and never counts anything
            // down, so this is unreachable in practice — and says nothing that
            // would be a lie if some future path reached it.
            Self::Never => "changes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    /// Lower fires first. Rules are held sorted by [`Config::validate`].
    #[serde(default = "d_priority")]
    pub priority: u32,
    /// Identity predicates.
    #[serde(rename = "match", default, skip_serializing_if = "Predicates::is_unconstrained")]
    pub match_: Predicates,
    /// Dynamic-state predicates.
    #[serde(default, skip_serializing_if = "Predicates::is_unconstrained")]
    pub when: Predicates,
    pub action: RuleAction,
}

impl Rule {
    pub fn new(name: impl Into<String>, priority: u32, action: RuleAction) -> Self {
        Self {
            name: name.into(),
            priority,
            match_: Predicates::default(),
            when: Predicates::default(),
            action,
        }
    }

    #[must_use]
    pub fn matching(mut self, p: Predicates) -> Self {
        self.match_ = p;
        self
    }

    #[must_use]
    pub fn when(mut self, p: Predicates) -> Self {
        self.when = p;
        self
    }

    /// The rule's own grace, if it specified one in either clause.
    #[must_use]
    pub fn grace(&self) -> Option<Duration> {
        self.when.for_.or(self.match_.for_).map(HumanDuration::get)
    }

    /// Whether this rule applies while plugged in, whatever `on_ac` says.
    ///
    /// Now only reachable for someone who has set `on_ac = "disabled"`, since
    /// the default acts on mains anyway — but this is where that default came
    /// from. Two of the predicates were never about endurance at all: `thermal`
    /// is about noise and `other_app_fullscreen` is about dropped frames, and
    /// both of those costs are paid in full while the charger is in. Once two
    /// exemptions from "do nothing on AC" existed on the grounds that the cost
    /// is real regardless of who pays it, the third — the power itself — was
    /// hard to argue against, and §1 stopped trying.
    ///
    /// So naming either one opts the rule out of the AC policy. Following the
    /// precedent `focused_for` already set, where carrying a predicate is what
    /// exempts a rule from the resume cooldown: the opt-out is a consequence of
    /// what the rule *says*, not a separate switch that could disagree with it.
    #[must_use]
    pub fn acts_on_ac(&self) -> bool {
        [&self.match_, &self.when]
            .iter()
            .any(|p| p.thermal.is_some() || p.other_app_fullscreen.is_some())
    }
}

/// Per-app escape hatches, keyed by the same patterns rules use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AppOverride {
    /// `monitor` keeps this one app in dry-run while the rest run live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_grace: Option<HumanDuration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freeze_visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim_on_freeze: Option<bool>,
    /// Reclassify an app the built-in heuristics got wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<AppClass>,
}

/// Note the field-level defaults: an absent `[[rule]]` table means *no rules*,
/// not the shipped ones. A user who deletes every rule has disabled `dozed`,
/// and having the built-ins quietly reappear would be the worst kind of
/// surprise. [`Config::default`] is what writes the initial file, so the
/// built-ins arrive spelled out where they can be read and edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub activity: ActivityConfig,
    #[serde(default)]
    pub battery: BatteryConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub thermal: ThermalConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub apps: BTreeMap<String, AppOverride>,
}

impl Default for Config {
    /// The shipped configuration: dry-run, with the safety exemptions and the
    /// single v1 rule from `DESIGN.md` §16.
    fn default() -> Self {
        Self {
            mode: Mode::DryRun,
            defaults: Defaults::default(),
            activity: ActivityConfig::default(),
            battery: BatteryConfig::default(),
            audio: AudioConfig::default(),
            thermal: ThermalConfig::default(),
            ui: UiConfig::default(),
            rules: builtin_rules(),
            apps: BTreeMap::new(),
        }
    }
}

/// The apps `chatty-apps` and its glance counterpart both match.
///
/// One list, because two copies would drift and the drift would be invisible:
/// patterns match exactly, and a spelling present in one rule and missing from
/// the other means an app is throttled on one path and not the other with
/// nothing to say so.
///
/// Every spelling is here for the same reason. `signal-desktop` alone let a real
/// Signal run unthrottled for a whole session, because this machine reports it
/// as plain `signal`.
fn chat_apps() -> Patterns {
    Patterns::Many(vec![
        "com.slack.Slack".into(),
        "slack".into(),
        "com.discordapp.Discord".into(),
        "discord".into(),
        "org.telegram.desktop".into(),
        "signal".into(),
        "signal-desktop".into(),
        "org.signal.Signal".into(),
        "com.microsoft.Teams".into(),
        "element-desktop".into(),
        "im.riot.Riot".into(),
        "thunderbird".into(),
        "org.mozilla.Thunderbird".into(),
    ])
}

/// How short a focus period counts as a glance rather than a visit.
const GLANCE: f32 = 10.0;
/// How long to wait after a glance before acting again. Not zero — see §8.
const AFTER_A_GLANCE: HumanDuration = HumanDuration::secs(15);

/// The exemptions we ship as rules rather than hard-code.
///
/// Only the invariants in `DESIGN.md` §10 are hard-coded; these are policy, so
/// they live where the user can read and edit them.
#[must_use]
pub fn builtin_rules() -> Vec<Rule> {
    vec![
        Rule::new("never-system", 0, RuleAction::Never)
            .matching(Predicates { class: Some(AppClass::System), ..Default::default() }),
        Rule::new("audio-exempt", 10, RuleAction::Never)
            .when(Predicates { playing_audio: Some(true), ..Default::default() }),
        Rule::new("inhibitor-exempt", 11, RuleAction::Never)
            .when(Predicates { has_inhibitor: Some(true), ..Default::default() }),
        Rule::new("fullscreen-exempt", 12, RuleAction::Never)
            .when(Predicates { fullscreen: Some(true), ..Default::default() }),
        // Above `dbus-owner-exempt` and below `fullscreen-exempt` for no reason
        // beyond reading order: these four are all `never`, so their relative
        // priority only decides which name `dozectl why` prints.
        Rule::new("capture-exempt", 13, RuleAction::Never)
            .when(Predicates { capturing: Some(true), ..Default::default() }),
        Rule::new("screencast-exempt", 14, RuleAction::Never)
            .when(Predicates { screen_casting: Some(true), ..Default::default() }),
        Rule::new("dbus-owner-exempt", 15, RuleAction::Never)
            .when(Predicates { owns_dbus_name: Some(true), ..Default::default() }),
        // The other side of `screencast-exempt`, and the only exemption here
        // that rests on an inference rather than an observation: what was on
        // screen when a share began is the closest thing to "what is being
        // shared" that any interface on this desktop will answer. Shipped as a
        // rule, and named so it can be found and deleted, because a guess that
        // errs wide has to be somebody's to decline.
        Rule::new("shared-window-exempt", 16, RuleAction::Never)
            .when(Predicates { maybe_shared: Some(true), ..Default::default() }),
        Rule::new("long-job-exempt", 20, RuleAction::Never)
            .matching(Predicates { class: Some(AppClass::LongRunningJob), ..Default::default() }),
        // The curated presets §15 asks for. These are policy, not invariants, so
        // they ship as ordinary rules the user can read, edit or delete —
        // unlike §10.1's never-freeze list, which is hard-coded because a rule
        // engine that can be edited into freezing dbus-broker is a rule engine
        // that can wedge the desktop.
        //
        // A hard "never" class for the things whose background work *is* the
        // point. `long-job-exempt` above already catches these by comm when they
        // are the process we sampled, but a VM manager or a sync client sitting
        // idle in the tray is matched by identity instead.
        Rule::new("never-vm-container-sync", 25, RuleAction::Never).matching(Predicates {
            app: Some(Patterns::Many(vec![
                // Virtual machines and containers: freezing one stops a guest.
                "org.virt_manager.virt-manager".into(),
                "virt-manager".into(),
                "org.gnome.Boxes".into(),
                "VirtualBox".into(),
                "vmware".into(),
                "com.github.Podman-Desktop".into(),
                "docker-desktop".into(),
                // Backup and sync: a half-written backup is worse than none.
                "syncthing".into(),
                "com.github.zocker_160.SyncThingy".into(),
                "org.gnome.DejaDup".into(),
                "insync".into(),
                "dropbox".into(),
                "nextcloud".into(),
                "com.nextcloud.desktopclient.nextcloud".into(),
                "megasync".into(),
                "com.owncloud.desktopclient".into(),
                "owncloud".into(),
                "seafile-applet".into(),
                "resilio-sync".into(),
                "keybase".into(),
                "pcloud".into(),
                // Backup front ends. `long-job-exempt` catches the engine —
                // `restic`, `borg` — only while it is the process being sampled;
                // the GUI that schedules it is idle in the tray until it fires,
                // which is exactly the profile this tool selects for.
                "org.gnome.World.PikaBackup".into(),
                "vorta".into(),
                "timeshift-gtk".into(),
                "kbackup".into(),
                // Downloads keep running while you are not looking at them.
                "org.qbittorrent.qBittorrent".into(),
                "qbittorrent".into(),
                "transmission-gtk".into(),
                "transmission-qt".into(),
                "deluge".into(),
                "org.kde.ktorrent".into(),
                "de.haeckerfelix.Fragments".into(),
                "uget-gtk".into(),
                "motrix".into(),
            ])),
            ..Default::default()
        }),
        // Interactive security prompts. These are idle by construction — they
        // exist to sit still until something asks — so they present exactly the
        // profile `bg-idle` selects for, and freezing one is not a delay: the
        // daemon behind it applies its own default to every new connection for
        // as long as the UI cannot answer, so a frozen prompt is either a broken
        // network or a silently permitted connection. Found as a live freeze
        // candidate on a real machine, where `opensnitch_ui` sat hidden at 0.1%
        // CPU all session.
        Rule::new("never-security-prompts", 26, RuleAction::Never).matching(Predicates {
            app: Some(Patterns::Many(vec![
                // Both spellings really occur: `opensnitch_ui` is the desktop
                // id, `opensnitch-ui` the comm.
                "opensnitch_ui".into(),
                "opensnitch-ui".into(),
                // Authentication agents are the same shape: idle until something
                // needs authorising, and a frozen one means the prompt never
                // appears rather than appearing late. §10.1's hard list names
                // `polkit`, which is the daemon; these are the session UIs that
                // talk to it.
                "polkit-kde-authentication-agent-1".into(),
                "polkit-gnome-authentication-agent-1".into(),
                "ssh-askpass".into(),
                "ksshaskpass".into(),
            ])),
            ..Default::default()
        }),
        // Remote desktop, in two rules because the family splits in two.
        //
        // What both halves have in common is the thing that must not happen: a
        // frozen one stops draining its socket, the receive window fills, and
        // the peer that sees a stalled client drops the session — so the cost is
        // not a slow redraw on thaw but a connection to re-establish and
        // whatever was on screen in it lost. Neither rule may freeze. Found with
        // a real VNC session hidden for 1h58m sitting in the would-doze list.
        //
        // These were one `never` until it was pointed out that `never` is the
        // wrong shape for the always-on half: a RustDesk sitting in the tray
        // with nobody connected is an ordinary background app that had bought
        // itself a permanent exemption by being *capable* of holding a session.
        // A cap is the right answer to that, and it is also the strongest thing
        // that leaves the app reachable — which a freeze does not, and
        // reachability with nobody at the keyboard is the entire point of
        // running one.
        //
        // What dozed cannot see is whether a session is running right now. That
        // needs a signal it has no source for — no exemption source watches
        // sockets — so both caps have to be right for the connected case and the
        // idle case at once, and that is what picks the two numbers below.
        //
        // **A cap needs an exclusive cgroup, and for the app this was written
        // for it does not have one.** Measured on the machine that prompted the
        // change: `rustdesk` was launched from a terminal and sits in
        // `app-net.local.wezterm@….service`, sharing the terminal's scope, so
        // `Actuator::apply_tree` answers `Skipped` for any `Throttle` — capping
        // the scope would cap wezterm and everything else started from it. The
        // freeze that has been happening used the SIGSTOP tree fallback, which
        // has no such requirement, which is exactly why the old `never` was the
        // only thing that could reach it.
        //
        // So on that machine these rules do not cap, they exempt, and the
        // honest accounting is the one from its own journal: 4d12h held over a
        // 4d14h window, **~111.6M wakeups and ~2708 CPU-seconds avoided — 30%
        // of everything dozing saved there**, second only to the browser. That
        // is the price, and it is not a rounding error. It is paid because the
        // alternative is freezing an app whose job is to be reachable, and
        // because the same journal caught a freeze landing on a live session
        // (102% CPU, 1182 wakeups/s, 4.5s after the window was hidden).
        //
        // The right answer is the one this cannot have: a signal for whether a
        // session is live, which would keep the 277 idle holds and drop only the
        // one that mattered. Nothing here watches sockets. Until something does,
        // the cap number below is the knob, and where an app *does* have its own
        // scope the cap is real.
        //
        // The hosts: always running, waiting for somebody to connect, and when
        // somebody has, the picture being throttled is on a screen the user
        // cannot see and cannot fix. So the cap is loose — half a core, which
        // bounds a runaway but clears what a session needs wherever the encoder
        // is in hardware, as it is by default on all three of these. A software
        // encoder at a high resolution will be clipped by it: the remote end
        // sees a lower frame rate, never a dropped session, which is the trade
        // this rule is choosing. Where the app has no scope of its own the cap
        // is `Skipped` and the rule is an exemption — see above, and do not read
        // the 50 as a saving.
        Rule::new("throttle-remote-hosts", 27, RuleAction::Throttle(50))
            .matching(Predicates {
                app: Some(Patterns::Many(vec![
                    "rustdesk".into(),
                    "anydesk".into(),
                    "teamviewer".into(),
                    "teamviewerd".into(),
                    "nomachine".into(),
                    "x11vnc".into(),
                    "wayvnc".into(),
                    "krfb".into(),
                    "org.kde.krfb".into(),
                    "vino-server".into(),
                    // `comm` is truncated to 15 bytes by the kernel, so the
                    // process reports the short form and the desktop entry the
                    // long one. Both spellings really occur, as they do for
                    // `opensnitch_ui` above.
                    "gnome-remote-desktop".into(),
                    "gnome-remote-de".into(),
                ])),
                ..Default::default()
            })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                for_: Some(HumanDuration::secs(600)),
                ..Default::default()
            }),
        // The clients, including the guest consoles: launched to hold one
        // session and closed when it ends, so unlike a host they are hidden only
        // when the user has switched away from a session they still want.
        //
        // `gvncviewer` *is* gtk-vnc, the thing `virt-viewer` draws a guest
        // console with, so protecting the VM manager under
        // `never-vm-container-sync` while leaving the window that displays the
        // guest freezable was half a rule.
        //
        // The cap is `chatty-apps`' 5, for `chatty-apps`' reason: a capped
        // client stays connected and keeps draining, it just cannot spin. It is
        // tighter than the hosts' because nobody is watching this one — the
        // rule only reaches a client the user has hidden, and these protocols
        // are change-driven, so a hidden session with nothing moving on it
        // receives almost nothing. The case it is wrong about is a hidden
        // session with video playing on the far side, which will decode slowly
        // until the user comes back.
        Rule::new("throttle-remote-clients", 28, RuleAction::Throttle(5))
            .matching(Predicates {
                app: Some(Patterns::Many(vec![
                    "gvncviewer".into(),
                    "vncviewer".into(),
                    "xtightvncviewer".into(),
                    "vinagre".into(),
                    "org.gnome.Vinagre".into(),
                    "virt-viewer".into(),
                    "remmina".into(),
                    "org.remmina.Remmina".into(),
                    "org.kde.krdc".into(),
                    "krdc".into(),
                    "org.gnome.Connections".into(),
                    "xfreerdp".into(),
                    "wlfreerdp".into(),
                    "rdesktop".into(),
                    "x2goclient".into(),
                    // Game streaming is the same shape with a tighter deadline:
                    // the client is decoding a live video stream, so a stall is
                    // one the person at the other end sees too.
                    "moonlight".into(),
                    "parsec".into(),
                ])),
                ..Default::default()
            })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                for_: Some(HumanDuration::secs(300)),
                ..Default::default()
            }),
        // Password managers, and the reason they are not covered by
        // `dbus-owner-exempt` already.
        //
        // §10's own example of the footgun is "freeze your password manager and
        // browser autofill hangs", and the D-Bus rule was measured to protect
        // exactly the processes that own an *activatable* name — which
        // `gnome-keyring-daemon` does and KeePassXC does not. KeePassXC's
        // browser integration runs over a native-messaging proxy rather than the
        // bus, so nothing in the owner rule sees it, and a frozen one hangs
        // auto-type in the middle of a login rather than failing it.
        //
        // Stated as reasoning rather than measurement: unlike the D-Bus rule
        // above, this one has not been checked against a live desktop, and the
        // 34-of-93 count that justified that rule says nothing about this one.
        // It is a `never` on a small, named set, so the cost of being wrong is a
        // few idle megabytes.
        Rule::new("never-password-managers", 29, RuleAction::Never).matching(Predicates {
            app: Some(Patterns::Many(vec![
                "org.keepassxc.KeePassXC".into(),
                "keepassxc".into(),
                "keepassxc-proxy".into(),
                "com.bitwarden.desktop".into(),
                "bitwarden".into(),
                "1password".into(),
                "1Password".into(),
                "io.enpass.Enpass".into(),
                "enpass".into(),
                "org.gnome.World.Secrets".into(),
            ])),
            ..Default::default()
        }),
        // Low battery reaches the foreground, and nothing else does.
        //
        // Every other shipped rule matches `hidden`, because the window you are
        // looking at is where §1's promise — that you cannot tell dozed is
        // running — is easiest to break. But an app you are *not interacting
        // with* is not doing anything for you just because it happens to hold
        // focus: a laptop left with Slack in front of it at 15% charge is
        // spending its remaining runtime on a window nobody is typing into.
        //
        // "Not interacting with" is read from the desktop state rather than any
        // per-app input tracking, and that is exact rather than a proxy: input
        // goes to the focused window, so a focused app under a desktop that is
        // no longer `Active` is precisely an app receiving no input.
        //
        // A throttle, never a freeze — the planner refuses to freeze the
        // foreground whatever a rule asks for — and `throttle-to` rather than a
        // fixed percentage because the right cap for the window in front of you
        // is the one that buys quiet without being felt, which is a measurement
        // and not a guess.
        Rule::new("low-battery-foreground", 35, RuleAction::ThrottleTo(50)).when(Predicates {
            visibility: Some(Visibility::Focused),
            desktop_state: Some(DesktopState::IdlePresent),
            battery_below: Some(20.0),
            for_: Some(HumanDuration::secs(300)),
            ..Default::default()
        }),
        // A glance is not a visit (§8). You alt-tab to a chat window, read one
        // line and leave — and under the resume cooldown alone that three-second
        // look buys the app its full grace *and* a two-minute floor on top, so
        // something that was dozing happily runs for minutes because you looked
        // at it.
        //
        // Two rules rather than one, and the split is the same one §7 already
        // makes. A single glance rule would have to pick an action, and at any
        // priority high enough to beat `bg-idle` it also beats `chatty-apps` —
        // so glancing at Slack would *freeze* it, dropping the connection and
        // costing a reconnect, which is the exact harm `chatty-apps` exists to
        // avoid. Chat apps are the most-glanced-at apps on the machine, so that
        // is not an edge case.
        //
        // The priorities interlock: 45 beats `chatty-apps` at 50 for chat apps,
        // and 55 loses to it, so a chat app never reaches the freezing variant.
        Rule::new("redoze-chat-after-a-glance", 45, RuleAction::Throttle(5))
            .matching(Predicates { app: Some(chat_apps()), ..Default::default() })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                focused_for: Some(Threshold::Bounds { gt: None, lt: Some(GLANCE) }),
                for_: Some(AFTER_A_GLANCE),
                ..Default::default()
            }),
        // Chat apps are §7's headline case for Throttle rather than Freeze: a
        // capped Slack stays connected and still notifies, it just cannot spin.
        // Freezing it would drop the connection and cost a reconnect on thaw.
        Rule::new("chatty-apps", 50, RuleAction::Throttle(5))
            .matching(Predicates { app: Some(chat_apps()), ..Default::default() })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                for_: Some(HumanDuration::secs(120)),
                ..Default::default()
            }),
        Rule::new("redoze-after-a-glance", 55, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            focused_for: Some(Threshold::Bounds { gt: None, lt: Some(GLANCE) }),
            for_: Some(AFTER_A_GLANCE),
            ..Default::default()
        }),
        // Media players and stores. Audio already exempts a *playing* player
        // (§10), so this is about the paused case: Steam and Spotify hidden and
        // silent are perfectly good freeze candidates, but they are slow to come
        // back, so they earn a longer wait rather than an exemption.
        Rule::new("slow-to-wake", 60, RuleAction::Freeze)
            .matching(Predicates {
                app: Some(Patterns::Many(vec![
                    "com.spotify.Client".into(),
                    "spotify".into(),
                    "com.valvesoftware.Steam".into(),
                    "steam".into(),
                    "com.obsproject.Studio".into(),
                ])),
                ..Default::default()
            })
            .when(Predicates {
                visibility: Some(Visibility::Hidden),
                for_: Some(HumanDuration::secs(900)),
                ..Default::default()
            }),
        // Deliberately *not* `on_battery = true`.
        //
        // Whether to act on AC is already decided in one place — `on_ac` in
        // `[defaults]`, which `Grace` turns into "disabled" (§8). Gating the rule
        // on it as well made two knobs that have to agree, and the failure was
        // silent in the worst direction: setting `on_ac = "enabled"` changed
        // nothing, because the rule still refused. Found by replaying a real
        // night against `on_ac = "enabled"` and getting zero intents.
        Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some(d_base_grace()),
            ..Default::default()
        }),
    ]
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("two rules are named `{0}`; rule names must be unique so `dozectl why` is unambiguous")]
    DuplicateRule(String),
    #[error("rule `{rule}` throttles to {pct}%; use 1-100")]
    BadThrottle { rule: String, pct: u8 },
    #[error("rule `{rule}` asks for a wakeup budget of zero, which no cap can reach")]
    BadThrottleTarget { rule: String },
    #[error("rule `{0}` has a bound that nothing can satisfy")]
    ImpossibleBound(String),
    #[error("rule `{0}` has an empty name")]
    UnnamedRule(String),
    #[error("`mode = \"monitor\"` is per-app only; the global mode must be dry-run or live")]
    GlobalMonitor,
    #[error("app override `{0}` has an empty pattern")]
    EmptyAppPattern(String),
    #[error(
        "rule `{rule}` comes up for air every {every} for {wake}, which is never less than the \
         period — the app would never actually doze"
    )]
    CycleNeverDozes { rule: String, every: HumanDuration, wake: HumanDuration },
    #[error(
        "`thermal.cool_{signal}` ({cool}) is not below `thermal.hot_{signal}` ({hot}); the pair is \
         hysteresis, so the level it clears at has to be under the level it fires at"
    )]
    ThermalHysteresis { signal: &'static str, hot: String, cool: String },
}

impl Config {
    /// A config with no rules at all, which does nothing. Mostly for tests.
    #[must_use]
    pub fn empty() -> Self {
        Self { rules: Vec::new(), ..Default::default() }
    }

    /// Parse and validate a config file.
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        let mut cfg: Self = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Check the invariants the planner relies on, and sort the rule table into
    /// priority order so first-match-wins is a simple scan.
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        if self.mode == Mode::Monitor {
            return Err(ConfigError::GlobalMonitor);
        }
        // Inverted hysteresis is not a preference, it is a state machine that
        // cannot leave `hot`: it would fire at the lower number and demand the
        // higher one to clear.
        if self.thermal.cool_celsius >= self.thermal.hot_celsius {
            return Err(ConfigError::ThermalHysteresis {
                signal: "celsius",
                hot: self.thermal.hot_celsius.to_string(),
                cool: self.thermal.cool_celsius.to_string(),
            });
        }
        if let (Some(hot), Some(cool)) = (self.thermal.hot_rpm, self.thermal.cool_rpm)
            && cool >= hot
        {
            return Err(ConfigError::ThermalHysteresis {
                signal: "rpm",
                hot: hot.to_string(),
                cool: cool.to_string(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for rule in &self.rules {
            if rule.name.trim().is_empty() {
                return Err(ConfigError::UnnamedRule(rule.name.clone()));
            }
            if !seen.insert(rule.name.as_str()) {
                return Err(ConfigError::DuplicateRule(rule.name.clone()));
            }
            if let RuleAction::Throttle(pct) = rule.action
                && !(1..=100).contains(&pct)
            {
                return Err(ConfigError::BadThrottle { rule: rule.name.clone(), pct });
            }
            // A budget of zero is unreachable — the cap floor exists precisely
            // so an app is never squeezed to silence — so the loop would chase
            // it to the floor and stall there. Rejected rather than clamped,
            // because "0 wakeups" is a wish, not a setting.
            if rule.action == RuleAction::ThrottleTo(0) {
                return Err(ConfigError::BadThrottleTarget { rule: rule.name.clone() });
            }
            // A window at least as long as its own period is an app that is never
            // actually dozed — it comes up for air before it has been down for the
            // period, forever. Rejected rather than clamped, because there is no
            // sensible thing to clamp it to and the config reads as though it does
            // something.
            if let RuleAction::DozeCycle { every, wake } = rule.action
                && wake >= every
            {
                return Err(ConfigError::CycleNeverDozes { rule: rule.name.clone(), every, wake });
            }
            for p in [&rule.match_, &rule.when] {
                for t in [p.cpu_pct, p.wakeups_per_sec].into_iter().flatten() {
                    if t.is_empty() {
                        return Err(ConfigError::ImpossibleBound(rule.name.clone()));
                    }
                }
            }
        }
        for pattern in self.apps.keys() {
            if pattern.trim().is_empty() {
                return Err(ConfigError::EmptyAppPattern(pattern.clone()));
            }
        }
        // Stable, so equal priorities keep file order and the winner is the one
        // the user wrote first.
        self.rules.sort_by_key(|r| r.priority);
        Ok(())
    }

    /// Rules that cycle, for `dozectl` and the docs to describe.
    #[must_use]
    pub fn cycling_rules(&self) -> Vec<&Rule> {
        self.rules.iter().filter(|r| matches!(r.action, RuleAction::DozeCycle { .. })).collect()
    }

    /// Configurations that parse but will not do what they look like they do.
    ///
    /// Not errors, because each is a legal thing to have written and only the
    /// author knows whether they meant it. They are reported because each fails
    /// *silently and in the safe direction*: the symptom is `dozed` doing
    /// nothing all day without ever saying why, which is indistinguishable from
    /// working correctly on a quiet machine.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        // Grace on battery became proportional to charge, which has no free
        // parameters, so these two stopped meaning anything. Silence would let
        // someone tune a number that cannot move.
        if (self.battery.low_pct - d_low_pct()).abs() > f32::EPSILON
            || (self.battery.low_factor - d_low_factor()).abs() > f64::EPSILON
        {
            out.push(
                "`battery.low_pct` and `battery.low_factor` no longer do anything. Grace on \
                 battery is now proportional to charge — 50% charge, half the patience — so \
                 there is no threshold to set and no multiplier to pick. Remove them."
                    .to_owned(),
            );
        }
        if self.defaults.on_ac == AcPolicy::Disabled {
            for rule in &self.rules {
                // A `never` rule that only applies on battery is fine: it
                // exempts less than it could, and exempting less on AC costs
                // nothing when nothing acts on AC anyway.
                if rule.action == RuleAction::Never {
                    continue;
                }
                if rule.match_.on_battery != Some(true) && rule.when.on_battery != Some(true) {
                    continue;
                }
                out.push(format!(
                    "rule `{}` requires `on_battery = true`, and `defaults.on_ac = \"disabled\"` \
                     already stops every rule acting on AC. That is two gates that have to \
                     agree, so setting `on_ac = \"enabled\"` will not be enough to make this \
                     rule fire — remove the rule's `on_battery` as well.",
                    rule.name
                ));
            }
        }
        // A custom aggression past the most impatient named level. Legal, and
        // the reason `Custom` exists at all — but it is the one term of the
        // grace product that scales every other one, so a value chosen to mean
        // "act sooner" quietly rescales decisions taken elsewhere for reasons
        // that had nothing to do with it.
        //
        // The AC case is not hypothetical. Measured on a real 13-day journal at
        // `aggression = 0.05`: `ac_factor = 2.0` doubled the grace on mains and
        // the aggression then divided it by twenty, so the deliberate "be more
        // patient while plugged in" policy came out ten times *shorter* than the
        // base grace — for 3,259 of 3,947 decisions, because a laptop spends
        // most of its working day plugged in.
        if let Aggression::Custom(factor) = self.defaults.aggression
            && factor < Aggression::Maximum.factor()
        {
            let mut msg = format!(
                "`aggression = {factor}` is more impatient than `\"maximum\"` ({}), which is the \
                 most impatient level with a name. Every grace in this file is multiplied by it, \
                 including the ones other settings just finished lengthening.",
                Aggression::Maximum.factor(),
            );
            if self.defaults.on_ac == AcPolicy::Enabled && self.battery.ac_factor > 1.0 {
                let net = self.battery.ac_factor * factor;
                msg.push_str(&format!(
                    " With `battery.ac_factor = {}` that nets out at x{net:.2} on mains — so \
                     \"more patient while plugged in\" currently acts {:.0} times sooner than the \
                     base grace, not later.",
                    self.battery.ac_factor,
                    1.0 / net,
                ));
            }
            out.push(msg);
        }
        // Half a pair. The fan's two thresholds are one signal and setting one
        // of them is never what somebody means: the unset half falls back to a
        // *learned* number, so the pinned half is being compared against a
        // moving target and the hysteresis gap is whatever the two happen to
        // land on.
        //
        // Worth saying rather than silently repairing, because the sensible
        // repair depends on the intent. `hot_rpm` alone reads as "stop the fan
        // reporting hot", which it does — while leaving a learned `cool_rpm`
        // that can still refuse to clear one, because clearing needs `cool_temp
        // && cool_fan` and `cool_fan` is `rpm < cool_rpm`.
        let pinned = (self.thermal.hot_rpm, self.thermal.cool_rpm);
        if let (Some(hot), None) | (None, Some(hot)) = pinned {
            let which = if self.thermal.hot_rpm.is_some() {
                ("hot_rpm", "cool_rpm")
            } else {
                ("cool_rpm", "hot_rpm")
            };
            out.push(format!(
                "`thermal.{} = {hot}` is set but `thermal.{}` is not, and the two are one signal. \
                 The unset half is learned from the fan speeds this machine is seen to use, so \
                 the pair you get is half a number you chose and half a number that moves. Set \
                 both, or neither — unset, the thresholds are placed inside the range this fan \
                 actually reaches, which is what an absolute RPM cannot do portably.",
                which.0, which.1,
            ));
        }
        // The annihilation that `CooldownMode::Floor` exists to prevent. Legal,
        // and a defensible choice, but worth saying out loud: in this
        // combination the cooldown is not merely shorter, it is inert.
        if self.defaults.resume_cooldown_mode == CooldownMode::Factor
            && self.activity.away_factor == 0.0
        {
            out.push(format!(
                "`resume_cooldown_mode = \"factor\"` with `away_factor = 0.0` means the \
                 {} resume cooldown has no effect while the screen is locked: grace is a \
                 product, so the zero cancels the factor and an app the user just came back \
                 to is re-dozed the instant they lock. Use the default \
                 `resume_cooldown_mode = \"floor\"` to make the cooldown a minimum wait.",
                self.defaults.resume_cooldown,
            ));
        }
        out
    }

    /// The first override whose pattern matches. `BTreeMap` order makes this
    /// deterministic rather than merely usually-stable.
    #[must_use]
    pub fn app_override(&self, key: &AppKey) -> Option<&AppOverride> {
        self.apps.iter().find(|(pattern, _)| key.matches_pattern(pattern)).map(|(_, o)| o)
    }

    /// `effective_mode = app_override.unwrap_or(global_mode)`.
    #[must_use]
    pub fn mode_for(&self, key: &AppKey) -> Mode {
        self.app_override(key).and_then(|o| o.mode).unwrap_or(self.mode)
    }

    #[must_use]
    pub fn base_grace_for(&self, key: &AppKey) -> Duration {
        self.app_override(key).and_then(|o| o.base_grace).unwrap_or(self.defaults.base_grace).get()
    }

    #[must_use]
    pub fn freeze_visible_for(&self, key: &AppKey) -> bool {
        self.app_override(key)
            .and_then(|o| o.freeze_visible)
            .unwrap_or(self.defaults.freeze_visible)
    }

    #[must_use]
    pub fn reclaim_for(&self, key: &AppKey) -> bool {
        self.app_override(key)
            .and_then(|o| o.reclaim_on_freeze)
            .unwrap_or(self.defaults.reclaim_on_freeze)
    }

    /// A class override, for apps the built-in heuristics misclassify.
    #[must_use]
    pub fn class_for(&self, key: &AppKey) -> Option<AppClass> {
        self.app_override(key).and_then(|o| o.class)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
