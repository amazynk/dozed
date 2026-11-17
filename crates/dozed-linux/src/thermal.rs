//! Reading how hard the machine is working, thermally.
//!
//! The quiet objective (`DESIGN.md` §7.3). §7's policy backs off on AC because a
//! plugged-in machine has no endurance to win — but it can still be *loud*, and a
//! fan at 4500 RPM is a cost somebody is paying whether or not the wall is
//! supplying the power.
//!
//! ## Fan speed, not just temperature
//!
//! A temperature threshold alone calls a machine *normal* while its fan is audible
//! across the room: 70 °C is unremarkable for a laptop under load, and 70 °C with
//! the fan at full tilt is exactly the case this objective exists for. The
//! complaint is noise, and the fan is the thing making it, so either signal can
//! raise the alarm.
//!
//! Every sensor is read and the **maximum** taken rather than the first. Observed:
//! two hwmon devices describing the same physical fan, one of them reporting 0, so
//! "the first sensor" is a coin toss decided by directory order.
//!
//! ## The two signals are not the same kind of number
//!
//! **A temperature is comparable across machines and a fan speed is not.** 80 °C is
//! 80 °C on any silicon; 3000 RPM is idle on one machine and redline on another. So
//! the temperature thresholds ship as absolute degrees, and the fan thresholds ship
//! as *nothing at all* — they are learned from the range this machine actually
//! exhibits ([`FanRange`]), because any absolute RPM default is a rule about one
//! manufacturer's fan wearing the costume of a general setting.
//!
//! That is not a hypothetical tidiness argument. Shipping `hot_rpm = 3000`, measured
//! from one laptop whose fan idled near zero, pinned the state at `hot` for 2,977 of
//! 3,125 decisions on a machine whose fan idles at 3161 — every rule keyed on
//! `thermal` silently unconditional, on hardware the number was never checked
//! against.
//!
//! ## Two things a sensor can do that look like data
//!
//! * A sensor can be present and unreadable — observed: a `temp*_label` reading
//!   `GPU` beside a `temp*_input` that fails with `ENXIO`. A failed read is skipped
//!   rather than counted as a zero, because a zero drags a maximum down and reads
//!   as a cool machine.
//! * `thermal_throttle` counters are not present on all platforms, so throttle
//!   detection is not part of the signal at all rather than part of it on some
//!   machines.
//!
//! ## And one thing a sensor can do that is data about something else
//!
//! The maximum is taken across every temperature sensor, and not every sensor is
//! describing the machine this objective is about. An NVMe drive idles in the
//! sixties and passes seventy under sustained writes; that is its ordinary
//! operating range and its own controller's problem, not a CPU working hard and
//! not a fan anybody can hear. Left in the maximum it would pin `thermal = hot`
//! on a cool machine for as long as the drive was busy — which is precisely the
//! failure the shipped `hot_rpm = 3000` produced, arriving down a different path:
//! a signal that reads `hot` unconditionally, and every rule keyed on it silently
//! made into no rule at all.
//!
//! So [`NOT_THE_MACHINE`] names the device classes whose temperature may not
//! stand in for the machine's, and they are dropped at discovery.
//!
//! **A denylist rather than an allowlist, deliberately.** An allowlist of CPU
//! sensor names (`k10temp`, `coretemp`, `zenpower`, …) fails in the expensive
//! direction the moment it meets hardware nobody tested: no sensor matches, the
//! temperature half of the signal disappears, and `dozectl status` shows a
//! machine with no temperature rather than a wrong one. A denylist that misses a
//! hot drive leaves this exactly where it was before the list existed. The two
//! errors are not the same size, so the list only ever removes what is certainly
//! not the CPU.

use std::fs;
use std::path::{Path, PathBuf};

use dozed_core::ThermalState;

/// Where the hot/cool decision's thresholds live.
///
/// Hysteresis, not a single threshold, because a fan hovering at the line would
/// otherwise flip the state — and every flip is an event, a re-plan, and possibly
/// a thaw and a re-freeze. The gaps are wide on purpose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Becomes hot at or above this temperature, in degrees.
    pub hot_celsius: f64,
    /// Returns to normal only below this one.
    pub cool_celsius: f64,
    /// Becomes hot at or above this fan speed, in RPM.
    ///
    /// `None` means the fan contributes nothing in either direction — the same
    /// branch a machine that reports no fan takes. That is the state before
    /// [`FanRange`] has seen enough of this machine to say anything, and the
    /// permanent state on a fan whose speed never varies enough to be
    /// informative.
    pub hot_rpm: Option<u32>,
    /// Returns to normal only below this one. `None` alongside `hot_rpm`.
    pub cool_rpm: Option<u32>,
}

impl Default for Thresholds {
    /// The shipped numbers, which live in
    /// [`ThermalConfig`](dozed_core::config::ThermalConfig) so a user can
    /// disagree with them — see that type for why they have to be settable.
    fn default() -> Self {
        dozed_core::config::ThermalConfig::default().into()
    }
}

impl From<dozed_core::config::ThermalConfig> for Thresholds {
    fn from(c: dozed_core::config::ThermalConfig) -> Self {
        Self {
            hot_celsius: c.hot_celsius,
            cool_celsius: c.cool_celsius,
            hot_rpm: c.hot_rpm,
            cool_rpm: c.cool_rpm,
        }
    }
}

/// Fastest a real fan turns. Anything at or above this is the sensor talking
/// about itself rather than about the machine.
///
/// `0xFFFF` is what `thinkpad_acpi` and friends report when a reading is
/// unavailable, and it arrives looking exactly like data: a `u32` that passed
/// the `>= 0` check, higher than every honest reading, and therefore the
/// `max()` winner. Seen live — one sample of **65535** taught [`FanRange`] a
/// span of 0..65535 and put this machine's thresholds at "hot above 43253 rpm,
/// cool below 26214", which no fan can reach in either direction. The signal
/// failed safe, into temperature-only, and printed nonsense on the way.
///
/// 30,000 is far above any fan in a computer — laptops top out near 8,000 and
/// even server blowers stay well under 20,000 — and far below the sentinel. A
/// ceiling rather than an equality test with `0xFFFF`, because the class of bug
/// is "the sensor emitted a marker" and there is more than one marker.
const IMPLAUSIBLE_RPM: u32 = 30_000;

/// hwmon device names whose temperature is not this machine's temperature.
///
/// Matched as a **prefix** of the device's `name` file, because these arrive
/// enumerated (`nvme`, `nvme1`) or suffixed with a phy index (`mt7925_phy0`).
///
/// Every entry has to satisfy two things, and the second is what keeps the list
/// short. It must be hardware with its own thermal envelope, so a reading in its
/// normal range is not evidence about the CPU or the fan. And its name must be
/// unambiguous enough that no CPU sensor can collide with it — which rules out
/// anything as generic as `temp`, `thermal` or `acpitz`, the last of those being
/// a real chassis sensor on plenty of laptops and the CPU's only sensor on some.
///
/// * Storage — `nvme` runs in the sixties as a matter of course and the
///   kernel's `drivetemp` reports SATA drives the same way.
/// * Radios — a wifi chip measured 70 °C on the machine §7.3 was written
///   against, while the CPU it shares a chassis with was at 99 °C. It was not
///   the hottest thing there, and on a machine idling with a busy radio it
///   would be.
///
/// Not configurable, and that is a decision to revisit rather than a principle:
/// the thresholds are the user's to set because they are a policy, while this is
/// a claim about what a sensor is measuring, and it is checkable — `dozectl
/// status` prints what was dropped, so a machine where this list is wrong says
/// so rather than quietly reading one sensor short.
const NOT_THE_MACHINE: &[&str] = &[
    "nvme",
    "drivetemp",
    "iwlwifi",
    "mt7921",
    "mt7925",
    "ath10k",
    "ath11k",
    "ath12k",
    "rtw88",
    "rtw89",
];

/// Whether this hwmon device's temperature describes the machine.
fn speaks_for_the_machine(device_name: &str) -> bool {
    !NOT_THE_MACHINE.iter().any(|excluded| device_name.starts_with(excluded))
}

/// How far apart the extremes have to be before a fan is worth listening to.
///
/// Below this the machine has one fan speed as far as we can tell, and "is the
/// fan loud" has no answer that varies — so the fan stays out of the decision
/// rather than being forced into it by picking a point inside the noise. A
/// machine whose fan idles at 3161 and peaks at 4484 has 1323 RPM of range and
/// qualifies; one pinned at 4470 ± 10 does not, and should be judged on
/// temperature alone.
const INFORMATIVE_SPAN_RPM: u32 = 600;

/// Where in the observed range the fan counts as loud, and where it stops.
///
/// Fractions of *this machine's* span rather than absolute speeds, which is the
/// whole point: the top third of a fan's range is a meaningful claim about any
/// fan, and 3000 RPM is a claim about one.
///
/// The gap between them is the hysteresis, and it is wide for the usual reason —
/// every transition is a re-plan and possibly a thaw and a re-freeze.
const HOT_AT: f32 = 0.66;
const COOL_AT: f32 = 0.40;

/// The fan speeds this machine has actually been seen to use.
///
/// Learned per run rather than persisted. Re-learning costs a few minutes at
/// startup during which the fan says nothing and the temperature decides alone,
/// which is the safe direction; persisting it would mean a stale calibration
/// from a session on a desk outliving the one on a lap, and the failure that
/// prompted all of this was a number that outlived its measurement.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FanRange {
    lowest: Option<u32>,
    highest: Option<u32>,
}

impl FanRange {
    /// Fold in one reading.
    ///
    /// **A stopped fan is not a data point on the loudness scale**, and letting
    /// it be one is what made this mechanism reproduce the failure it was built
    /// to replace. Measured on a real machine over three days:
    ///
    /// | | |
    /// |---|---|
    /// | Observed range | 0 – 4518 rpm |
    /// | Learned `cool_rpm` (40% of it) | **1807** |
    /// | Quietest the fan actually managed, over three 4-hour stretches | 2114, 2188, 2360 |
    /// | Temperature during those stretches | 43.25, 44.63, 47.75 °C |
    /// | `cool_celsius` | 72.0 |
    ///
    /// The machine was cold and it read `hot` for hours at a time, because
    /// clearing needs *both* signals and the fan never returns to 1807 once it
    /// is spinning. That is `hot_rpm = 3000` all over again, and §7.3 said it
    /// first: *"a fan permanently above `cool_rpm` overrides a temperature that
    /// has genuinely come down. That machine's temperature fell to 48.85 °C
    /// during the stuck stretch and it changed nothing."* It is now 43.25 °C and
    /// it changes nothing.
    ///
    /// The zero came from a legitimate reading — this laptop's fan is genuinely
    /// off at times, and `acpi_fan` reports a permanent 0 beside the `thinkpad`
    /// sensor that reports the real speed. One such moment anchors the bottom of
    /// the range for the rest of the run.
    ///
    /// The model is fractions of an observed span, and a fan that is either off
    /// or turning at 2000+ has no meaningful span between those: it is bimodal,
    /// and "40% of the way from stopped to maximum" is a speed it never uses. So
    /// the range is learned from the speeds this fan *runs* at, and a stopped fan
    /// is left out of it — it is not a quiet fan, it is not a fan at all, and
    /// "how loud" has no answer there.
    ///
    /// It still contributes to the *reading*: a fan reported at 0 is below every
    /// threshold and clears the state, which is the correct and unchanged
    /// behaviour. This is only about what the scale is drawn against.
    fn observe(&mut self, rpm: Option<u32>) {
        let Some(rpm) = rpm.filter(|r| *r > 0) else { return };
        self.lowest = Some(self.lowest.map_or(rpm, |v| v.min(rpm)));
        self.highest = Some(self.highest.map_or(rpm, |v| v.max(rpm)));
    }

    /// `(hot, cool)` for this machine, or `None` while the fan has nothing to
    /// say — not enough observed yet, or a fan that does not vary.
    #[must_use]
    pub fn thresholds(self) -> Option<(u32, u32)> {
        let (lowest, highest) = (self.lowest?, self.highest?);
        let span = highest.checked_sub(lowest)?;
        if span < INFORMATIVE_SPAN_RPM {
            return None;
        }
        let at = |f: f32| lowest + (span as f32 * f) as u32;
        Some((at(HOT_AT), at(COOL_AT)))
    }

    #[must_use]
    pub fn observed(self) -> Option<(u32, u32)> {
        Some((self.lowest?, self.highest?))
    }
}

/// One reading of the machine's thermals.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Reading {
    /// The hottest sensor, in degrees. `None` when nothing was readable.
    pub hottest_celsius: Option<f64>,
    /// The fastest fan, in RPM. `None` when the machine reports no fan — a
    /// fanless laptop or a VM, where silence is not a thing dozed can improve.
    pub fastest_rpm: Option<u32>,
}

impl Reading {
    /// Whether the machine is hot, given where it was before.
    ///
    /// Takes the previous state because the thresholds are hysteretic: the answer
    /// to "is it hot" genuinely depends on whether it was hot a moment ago, and
    /// pretending otherwise produces a state that flaps.
    #[must_use]
    pub fn state(&self, was: ThermalState, t: &Thresholds) -> ThermalState {
        // Either signal can raise the alarm, and *both* must fall for it to clear.
        // Deliberately asymmetric: the cost of staying hot a little too long is
        // that some background app is frozen while the machine is quiet, and the
        // cost of clearing too early is a fan that never gets the chance to slow
        // down.
        let (hot_temp, cool_temp) = match self.hottest_celsius {
            Some(c) => (c >= t.hot_celsius, c < t.cool_celsius),
            // Unreadable is not cool. It contributes nothing in either direction,
            // so the other signal decides alone.
            None => (false, true),
        };
        // Needs a reading *and* a calibrated pair. Any of the three missing and
        // the fan is out of the decision entirely, which is the same answer a
        // fanless machine gets and the same reason: nothing to say.
        let (hot_fan, cool_fan) = match (self.fastest_rpm, t.hot_rpm, t.cool_rpm) {
            (Some(rpm), Some(hot), Some(cool)) => (rpm >= hot, rpm < cool),
            _ => (false, true),
        };
        match was {
            ThermalState::Normal if hot_temp || hot_fan => ThermalState::Hot,
            ThermalState::Hot if cool_temp && cool_fan => ThermalState::Normal,
            unchanged => unchanged,
        }
    }
}

/// The sensors, discovered once.
///
/// Same reasoning as `crate::energy`: globbing `hwmon*` on every tick would cost
/// more than reading the files it finds.
#[derive(Debug, Clone, Default)]
pub struct Sensors {
    temps: Vec<PathBuf>,
    fans: Vec<PathBuf>,
    /// Device names dropped by [`NOT_THE_MACHINE`], for `dozectl status`.
    ///
    /// Kept rather than counted: "one sensor ignored" sends somebody to read
    /// this file, and "ignored nvme" answers the question on the line.
    ignored: Vec<String>,
}

impl Sensors {
    #[must_use]
    pub fn new() -> Self {
        Self::at(Path::new("/"))
    }

    /// Discover under `root`, which tests point at a fixture tree.
    #[must_use]
    pub fn at(root: &Path) -> Self {
        let mut temps = Vec::new();
        let mut fans = Vec::new();
        let mut ignored = Vec::new();
        let Ok(entries) = fs::read_dir(root.join("sys/class/hwmon")) else {
            return Self::default();
        };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        // Sorted so the discovered set does not depend on directory order, which
        // keeps a reading reproducible across restarts.
        dirs.sort();
        for dir in dirs {
            // A device with no readable `name` is kept. The list below only ever
            // removes sensors, so failing to read the name has to fail towards
            // keeping one — an unnamed CPU sensor dropped for being unnamed is
            // the temperature signal disappearing on hardware nobody tested.
            let device = fs::read_to_string(dir.join("name"))
                .map(|s| s.trim().to_owned())
                .unwrap_or_default();
            let describes_us = speaks_for_the_machine(&device);
            let mut dropped_a_temp = false;

            let Ok(files) = fs::read_dir(&dir) else { continue };
            let mut names: Vec<String> =
                files.flatten().filter_map(|f| f.file_name().into_string().ok()).collect();
            names.sort();
            for name in names {
                if let Some(rest) = name.strip_prefix("temp")
                    && rest.ends_with("_input")
                {
                    // Only the temperature is dropped. A fan is a fan whatever
                    // is beside it on the bus — though in practice none of these
                    // devices has one.
                    if describes_us {
                        temps.push(dir.join(&name));
                    } else {
                        dropped_a_temp = true;
                    }
                } else if let Some(rest) = name.strip_prefix("fan")
                    && rest.ends_with("_input")
                {
                    fans.push(dir.join(&name));
                }
            }
            // Recorded only when it actually cost us a sensor, so a device that
            // is on the list and exposes no temperature anyway does not show up
            // in `dozectl status` as something that was ignored.
            if dropped_a_temp {
                ignored.push(device);
            }
        }
        ignored.dedup();
        Self { temps, fans, ignored }
    }

    /// Whether there is anything to read, so `dozectl status` can disclaim it.
    #[must_use]
    pub fn available(&self) -> bool {
        !self.temps.is_empty() || !self.fans.is_empty()
    }

    #[must_use]
    pub fn temp_count(&self) -> usize {
        self.temps.len()
    }

    #[must_use]
    pub fn fan_count(&self) -> usize {
        self.fans.len()
    }

    /// Devices whose temperature was dropped by [`NOT_THE_MACHINE`].
    ///
    /// On screen rather than only in this file, for the same reason the learned
    /// fan thresholds are: a sensor silently missing from a maximum is not
    /// distinguishable from a sensor that is not there, and one of those two is
    /// a list in this crate being wrong about somebody's hardware.
    #[must_use]
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    /// Read every sensor and keep the extremes.
    #[must_use]
    pub fn read(&self) -> Reading {
        // The maximum, not the first and not the mean. Two hwmon devices can
        // describe the same physical fan with one of them reporting 0, so "the
        // first" is a coin toss and a mean is an understatement of the thing
        // making the noise.
        let hottest = self
            .temps
            .iter()
            .filter_map(|p| read_i64(p))
            // hwmon reports millidegrees. A negative reading is a sensor that has
            // not been initialised, not a cold machine.
            .filter(|v| *v > 0)
            .map(|v| v as f64 / 1000.0)
            .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a: f64| a.max(v))));
        let fastest = self
            .fans
            .iter()
            .filter_map(|p| read_i64(p))
            // Negative is an uninitialised sensor; at or above the ceiling is a
            // sentinel ([`IMPLAUSIBLE_RPM`]). Both are the sensor failing to
            // answer, and neither may reach the maximum below — this is a
            // `max()`, so one bad reading is not diluted by good ones, it wins.
            .filter(|v| *v >= 0 && *v < i64::from(IMPLAUSIBLE_RPM))
            .map(|v| v as u32)
            .max();
        Reading { hottest_celsius: hottest, fastest_rpm: fastest }
    }
}

/// A sensor that is present but unreadable contributes nothing.
///
/// Observed: a `temp*_label` reading `GPU` beside a `temp*_input` that fails with
/// `ENXIO`. Treating that as a zero would drag the maximum down and report a hot
/// machine as cool, which is the wrong direction for a signal whose job is to
/// notice the fan.
fn read_i64(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// How long the state must be pinned at `hot` before saturation is claimed.
///
/// Four hours, and the length is doing real work. The obvious shorter windows
/// all catch things that are not faults: a kernel build, a video export, a
/// laptop on a duvet for the afternoon. Half an hour was tried first and flagged
/// a machine sitting at a genuine 85 °C, which is not a broken threshold, it is
/// a hot machine.
///
/// Four hours of *continuous* `hot` is longer than any desktop workload sustains
/// without the user knowing about it, and on the machine that prompted this the
/// state was pinned for thirteen days.
const SATURATED_AFTER: std::time::Duration = std::time::Duration::from_secs(4 * 3600);

/// Tracks the state across readings, applying the hysteresis.
#[derive(Debug, Clone, Default)]
pub struct Monitor {
    sensors: Sensors,
    /// What the config asked for. The fan half may be `None`, meaning "learn it"
    /// — see [`Monitor::effective`].
    thresholds: Thresholds,
    /// The fan speeds seen this run, which is where the fan thresholds come from
    /// unless the config pins them.
    fan_range: FanRange,
    state: ThermalState,
    /// When the state last became `hot`, and the quietest the machine has been
    /// since. See [`Monitor::saturated`].
    hot_since: Option<std::time::Instant>,
    quietest: Reading,
}

impl Reading {
    /// The quieter of two readings, taken per signal.
    ///
    /// Per signal rather than per reading because they are independent: the
    /// quietest the fan has been and the coolest the package has been need not
    /// have happened at the same moment, and the question this feeds is whether
    /// *either* has ever been low enough to clear the state.
    ///
    /// A `None` on either side stays `None` — an absent sensor contributes
    /// nothing here exactly as it contributes nothing to [`Reading::state`].
    fn quieter_of(self, other: Self) -> Self {
        fn min_opt<T: PartialOrd>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(if a < b { a } else { b }),
                (Some(v), None) | (None, Some(v)) => Some(v),
                (None, None) => None,
            }
        }
        Self {
            hottest_celsius: min_opt(self.hottest_celsius, other.hottest_celsius),
            fastest_rpm: min_opt(self.fastest_rpm, other.fastest_rpm),
        }
    }
}

impl Monitor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sensors: Sensors::new(),
            thresholds: Thresholds::default(),
            fan_range: FanRange::default(),
            state: ThermalState::Normal,
            hot_since: None,
            quietest: Reading::default(),
        }
    }

    /// Discover this machine's sensors, and judge them against `thresholds`.
    #[must_use]
    pub fn with_thresholds(thresholds: Thresholds) -> Self {
        Self::with_sensors(Sensors::new(), thresholds)
    }

    /// Adopt new thresholds without losing the sensors or the state.
    ///
    /// A config reload has to reach this, or the one setting somebody edits in
    /// response to `dozectl status` telling them the state is stuck would not
    /// take effect until the next restart — and the message that prompted the
    /// edit would go on being printed.
    ///
    /// The `hot`/`normal` state deliberately survives: it is an observation
    /// about the machine, and the next `poll` re-judges it against the new
    /// numbers through the same hysteresis as any other reading. The saturation
    /// clock does restart, because it is a claim about a particular pair of
    /// thresholds and these are not those.
    pub fn set_thresholds(&mut self, thresholds: Thresholds) {
        if self.thresholds == thresholds {
            return;
        }
        self.thresholds = thresholds;
        self.hot_since = None;
        self.quietest = Reading::default();
    }

    #[must_use]
    pub fn with_sensors(sensors: Sensors, thresholds: Thresholds) -> Self {
        Self {
            sensors,
            thresholds,
            fan_range: FanRange::default(),
            state: ThermalState::Normal,
            hot_since: None,
            quietest: Reading::default(),
        }
    }

    #[must_use]
    pub fn available(&self) -> bool {
        self.sensors.available()
    }

    #[must_use]
    pub fn state(&self) -> ThermalState {
        self.state
    }

    #[must_use]
    pub fn last_reading(&self) -> Reading {
        self.sensors.read()
    }

    /// Take a reading. `Some` only when the state actually changed, so the caller
    /// emits an event per transition rather than per tick.
    pub fn poll(&mut self) -> Option<ThermalState> {
        if !self.sensors.available() {
            return None;
        }
        let reading = self.sensors.read();
        // Before the judgement, so the reading that widens the range is judged
        // against the range it widened. A fan first seen at its peak would
        // otherwise be measured against a span it had not yet demonstrated.
        self.fan_range.observe(reading.fastest_rpm);
        let next = reading.state(self.state, &self.effective());

        // Tracked on every poll, including the ones that change nothing —
        // saturation is a statement about the readings, and the readings that
        // change nothing are most of them.
        if next == ThermalState::Hot {
            self.hot_since.get_or_insert_with(std::time::Instant::now);
            self.quietest = self.quietest.quieter_of(reading);
        } else {
            self.hot_since = None;
            self.quietest = Reading::default();
        }

        if next == self.state {
            return None;
        }
        self.state = next;
        Some(next)
    }

    /// Devices whose temperature this monitor is deliberately not reading.
    ///
    /// See [`Sensors::ignored`].
    #[must_use]
    pub fn ignored_sensors(&self) -> &[String] {
        self.sensors.ignored()
    }

    /// The thresholds actually in force: the config's, with the fan half filled
    /// in from what this machine has been observed to do.
    ///
    /// The config wins where it has an opinion. Somebody who knows their
    /// hardware — or who wants the fan out of the decision permanently — can say
    /// so, and calibration must not quietly overrule them.
    #[must_use]
    pub fn effective(&self) -> Thresholds {
        let mut t = self.thresholds;
        if t.hot_rpm.is_none()
            && t.cool_rpm.is_none()
            && let Some((hot, cool)) = self.fan_range.thresholds()
        {
            t.hot_rpm = Some(hot);
            t.cool_rpm = Some(cool);
        }
        t
    }

    /// The fan speeds seen this run, for `dozectl status` and the saturation
    /// message — a calibrated threshold nobody can see is as unexplained as a
    /// hardcoded one.
    #[must_use]
    pub fn fan_range(&self) -> FanRange {
        self.fan_range
    }

    /// Whether this machine's thermal signal has stopped carrying information.
    ///
    /// The quiet objective (§7.3) rests on `hot` meaning something, and on a
    /// machine whose idle floor sits above the clearing threshold it means
    /// nothing at all. Observed in a real journal: a fan idling at **4465 RPM**
    /// against a `hot_rpm` of 3000, so the state was `hot` for 2,977 of 3,125
    /// recorded decisions and `normal` for 148. Every rule keyed on it was
    /// effectively unconditional, and nothing said so — a constant and a signal
    /// look identical from inside a rule.
    ///
    /// Two conditions, both required. The state must have been `hot` without
    /// interruption for [`SATURATED_AFTER`]; and the best each signal managed
    /// *separately* in that stretch must still not have been enough to clear it.
    ///
    /// The second is not implied by the first, and that is the point of it.
    /// Clearing requires both signals to be cool in the *same* reading, so a
    /// machine whose fan drops to a whisper while the package is hot, and cools
    /// off while the fan is still spinning down, never clears a single reading —
    /// yet both of its thresholds are demonstrably reachable. Taking the minimum
    /// of each signal independently and asking whether *that* would have cleared
    /// it is what tells the two apart.
    ///
    /// What this deliberately does **not** claim is the cause. A machine pinned
    /// above `hot_celsius` for four hours may have an unreachable threshold or
    /// may simply be hot; both were seen while tuning this, and no reading can
    /// separate them. What it does claim is the consequence, which is the same
    /// either way and is the thing worth saying: every rule keyed on `thermal`
    /// has been firing unconditionally for four hours, and a constant and a
    /// signal look identical from inside a rule.
    ///
    /// Reported rather than corrected. The thresholds are the user's to set, and
    /// a daemon that silently retunes the number it is judged by would be
    /// removing the evidence — `dozectl status` says so, and the fix is a
    /// `hot_rpm` that means something on this machine.
    ///
    /// Not corrected, but not ignored either, and the difference matters. This
    /// monitor keeps its state, its thresholds and its readings; what
    /// `Daemon::check_thermal` does with the answer is stop forwarding `hot` to
    /// the planner while it holds, so `thermal` rules are suspended rather than
    /// fired unconditionally. Saying "this signal is a constant" and then
    /// handing it to the one reader that cannot tell the difference was the gap:
    /// the warning below went out six times over a fortnight and every rule
    /// keyed on `thermal` went on matching throughout.
    #[must_use]
    pub fn saturated(&self) -> bool {
        self.hot_since.is_some_and(|since| self.saturated_after(since.elapsed()))
    }

    /// [`Monitor::saturated`] against a stated stretch rather than the clock, so
    /// the rule can be tested without waiting half an hour for it.
    #[must_use]
    fn saturated_after(&self, hot_for: std::time::Duration) -> bool {
        // "Even at its quietest, this would not have cleared." Asked of the
        // hysteresis itself rather than reimplemented beside it, so the two
        // cannot drift apart.
        hot_for >= SATURATED_AFTER
            && self.quietest.state(ThermalState::Hot, &self.effective()) == ThermalState::Hot
    }

    /// The state the **rules** should be judged against, which is not always
    /// the state this monitor is in.
    ///
    /// [`Monitor::state`] is the observation and stays exactly what the sensors
    /// and the hysteresis say — `dozectl status` reports it, and the thresholds
    /// behind it remain the user's. This is the narrower question of what a
    /// `thermal` predicate should be allowed to match on, and the answer differs
    /// in one case: a saturated signal is one this type has already concluded
    /// carries no information, and handing a known constant to a rule engine is
    /// handing it a predicate that is always true without saying so.
    ///
    /// `Normal`, because `DESIGN.md` §3 settles which way an ambiguity resolves:
    /// towards doing less. A machine that really is hot loses some throttling it
    /// would have got; a machine whose thresholds are out of range stops
    /// freezing everything it owns on the strength of a number that has not
    /// moved in four hours. The warning in `Daemon::check_thermal` says which
    /// one this is, and the reading beside it says how to tell.
    #[must_use]
    pub fn state_for_rules(&self) -> ThermalState {
        if self.saturated() { ThermalState::Normal } else { self.state }
    }

    /// [`Monitor::state_for_rules`] against a stated stretch rather than the
    /// clock, for the same reason [`Monitor::saturated_after`] exists.
    #[cfg(test)]
    fn state_for_rules_after(&self, hot_for: std::time::Duration) -> ThermalState {
        if self.saturated_after(hot_for) { ThermalState::Normal } else { self.state }
    }

    /// The quietest reading seen in the current `hot` stretch, for the message
    /// that explains [`Monitor::saturated`].
    #[must_use]
    pub fn quietest_while_hot(&self) -> Reading {
        self.quietest
    }

    #[must_use]
    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    /// Unique per fixture, not per name.
    ///
    /// Three tests call `under_load()`, cargo runs them in parallel, and a
    /// directory named after the fixture *and the pid* is the same directory for
    /// all three — so each one's setup deleted the others' sensors mid-read. The
    /// counter is the whole fix and it is worth a comment, because the symptom was
    /// three unrelated failures that all looked like reader bugs.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    impl Fixture {
        fn new(name: &str) -> Self {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("dozed-thermal-{name}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn hwmon(&self, n: u32, name: &str, files: &[(&str, &str)]) {
            let dir = self.0.join(format!("sys/class/hwmon/hwmon{n}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{name}\n")).unwrap();
            for (file, contents) in files {
                fs::write(dir.join(file), contents).unwrap();
            }
        }

        fn sensors(&self) -> Sensors {
            Sensors::at(&self.0)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Thresholds with the fan half filled in, standing in for a machine whose
    /// range [`FanRange`] has already learned.
    ///
    /// Nothing ships absolute fan speeds any more — an RPM is a claim about one
    /// fan, not about fans — so a test that is *about the fan signal* has to say
    /// which fan it means. These are the numbers a machine idling near zero and
    /// peaking around 4500 calibrates to.
    fn calibrated() -> Thresholds {
        Thresholds { hot_rpm: Some(3000), cool_rpm: Some(2200), ..Thresholds::default() }
    }

    /// A laptop during a build, as measured.
    fn under_load() -> Fixture {
        let f = Fixture::new("load");
        f.hwmon(8, "k10temp", &[("temp1_label", "Tctl\n"), ("temp1_input", "70750\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4484\n"), ("fan2_input", "4484\n")]);
        f.hwmon(1, "acpi_fan", &[("fan1_input", "0\n")]);
        f
    }

    #[test]
    fn the_fan_is_what_notices_a_machine_working_hard() {
        // The measurement that shaped this file: 70.8 °C is unremarkable for an
        // laptop under load, and the fan was at 4484 RPM and audible. A temperature
        // threshold alone calls that normal.
        let f = under_load();
        let reading = f.sensors().read();
        assert_eq!(reading.fastest_rpm, Some(4484));
        assert!((reading.hottest_celsius.unwrap() - 70.75).abs() < 1e-9);

        let t = calibrated();
        assert!(reading.hottest_celsius.unwrap() < t.hot_celsius, "not hot by temperature");
        assert_eq!(
            reading.state(ThermalState::Normal, &t),
            ThermalState::Hot,
            "but hot by fan, which is the objective"
        );
    }

    #[test]
    fn the_maximum_is_taken_because_two_sensors_describe_one_fan() {
        // One hwmon device reports 0 for the same fan another reports 4484 for.
        // Reading "the first" would be a coin toss decided by directory order.
        let f = under_load();
        assert_eq!(f.sensors().read().fastest_rpm, Some(4484));
        assert_eq!(f.sensors().fan_count(), 3, "all three fan inputs are found");
    }

    #[test]
    fn a_sensor_that_is_present_but_unreadable_is_skipped() {
        // Observed: a `temp*_label` reads GPU while its `temp*_input` fails
        // with ENXIO. A zero there would drag the maximum down and call a hot
        // machine cool.
        let f = Fixture::new("enxio");
        f.hwmon(7, "thinkpad", &[("temp1_input", "91000\n"), ("temp2_label", "GPU\n")]);
        // An empty file stands in for the failing read: both parse to nothing.
        let dir = f.0.join("sys/class/hwmon/hwmon7");
        fs::write(dir.join("temp2_input"), "").unwrap();

        let reading = f.sensors().read();
        assert_eq!(reading.hottest_celsius, Some(91.0), "the readable sensor still decides");
        assert_eq!(reading.state(ThermalState::Normal, &Thresholds::default()), ThermalState::Hot);
    }

    #[test]
    fn the_state_does_not_flap_around_a_single_threshold() {
        // Every transition is an event, a re-plan, and possibly a thaw and a
        // re-freeze. A fan hovering at the line must not produce a stream of them.
        let t = calibrated();
        let at = |rpm: u32| Reading { hottest_celsius: Some(60.0), fastest_rpm: Some(rpm) };

        // Up through the hot threshold, then back to just below it.
        assert_eq!(at(2900).state(ThermalState::Normal, &t), ThermalState::Normal);
        assert_eq!(at(3000).state(ThermalState::Normal, &t), ThermalState::Hot);
        assert_eq!(
            at(2900).state(ThermalState::Hot, &t),
            ThermalState::Hot,
            "still hot: it has not fallen past the cool threshold"
        );
        assert_eq!(at(2199).state(ThermalState::Hot, &t), ThermalState::Normal);
    }

    #[test]
    fn both_signals_must_fall_before_it_is_quiet_again() {
        // Asymmetric on purpose: staying hot too long freezes a background app on
        // a quiet machine, while clearing too early never lets the fan slow down.
        let t = calibrated();
        let hot_cpu_slow_fan = Reading { hottest_celsius: Some(95.0), fastest_rpm: Some(500) };
        assert_eq!(hot_cpu_slow_fan.state(ThermalState::Hot, &t), ThermalState::Hot);

        let cool_cpu_loud_fan = Reading { hottest_celsius: Some(50.0), fastest_rpm: Some(4000) };
        assert_eq!(cool_cpu_loud_fan.state(ThermalState::Hot, &t), ThermalState::Hot);

        let both_down = Reading { hottest_celsius: Some(50.0), fastest_rpm: Some(500) };
        assert_eq!(both_down.state(ThermalState::Hot, &t), ThermalState::Normal);
    }

    #[test]
    fn a_fanless_machine_is_judged_on_temperature_alone() {
        // A tablet, a passively cooled mini-PC, or a VM. Silence is not something
        // dozed can improve there, but thermal limits still exist.
        let f = Fixture::new("fanless");
        f.hwmon(0, "acpitz", &[("temp1_input", "85000\n")]);
        let reading = f.sensors().read();
        assert_eq!(reading.fastest_rpm, None);
        assert_eq!(reading.state(ThermalState::Normal, &Thresholds::default()), ThermalState::Hot);
    }

    #[test]
    fn a_machine_with_no_sensors_never_reports_hot() {
        // A VM with no hwmon at all. Unreadable must not mean hot, or every rule
        // gated on `thermal = "hot"` fires forever on hardware that cannot say.
        let f = Fixture::new("bare");
        let sensors = f.sensors();
        assert!(!sensors.available());
        let mut m = Monitor::with_sensors(sensors, Thresholds::default());
        assert_eq!(m.poll(), None, "nothing to report");
        assert_eq!(m.state(), ThermalState::Normal);
    }

    #[test]
    fn a_negative_reading_is_an_uninitialised_sensor_not_a_cold_one() {
        let f = Fixture::new("negative");
        f.hwmon(0, "odd", &[("temp1_input", "-274000\n"), ("temp2_input", "84000\n")]);
        assert_eq!(f.sensors().read().hottest_celsius, Some(84.0));
    }

    #[test]
    fn a_hot_drive_does_not_speak_for_the_machine() {
        // The gap §7.3 named and did not close. An NVMe at 68C is in its
        // ordinary operating range; the CPU beside it is at 45C and the machine
        // is idle. Counted in the maximum, the drive pins `thermal = hot` on a
        // cool machine — and every rule keyed on `thermal` becomes
        // unconditional, which is the exact failure `hot_rpm = 3000` produced
        // down the other path.
        let f = Fixture::new("drive");
        f.hwmon(0, "k10temp", &[("temp1_label", "Tctl\n"), ("temp1_input", "45000\n")]);
        f.hwmon(1, "nvme", &[("temp1_label", "Composite\n"), ("temp1_input", "68000\n")]);

        let sensors = f.sensors();
        assert_eq!(sensors.read().hottest_celsius, Some(45.0), "the cpu, not the drive");
        assert_eq!(sensors.temp_count(), 1, "the drive's sensor is not discovered at all");
        assert_eq!(sensors.ignored(), ["nvme"], "and it says which one it dropped");

        // The whole point: with the drive counted this machine reads hot at the
        // default threshold of 65, and it is not hot.
        let t = Thresholds { hot_celsius: 65.0, cool_celsius: 60.0, ..Thresholds::default() };
        assert_eq!(sensors.read().state(ThermalState::Normal, &t), ThermalState::Normal);
    }

    #[test]
    fn an_enumerated_or_suffixed_device_is_still_matched() {
        // These arrive numbered (`nvme1`) or carrying a phy index
        // (`mt7925_phy0`), so the list is matched as a prefix.
        let f = Fixture::new("suffixed");
        f.hwmon(0, "coretemp", &[("temp1_input", "50000\n")]);
        f.hwmon(1, "nvme1", &[("temp1_input", "70000\n")]);
        f.hwmon(2, "mt7925_phy0", &[("temp1_input", "72000\n")]);

        let sensors = f.sensors();
        assert_eq!(sensors.read().hottest_celsius, Some(50.0));
        assert_eq!(sensors.ignored(), ["nvme1", "mt7925_phy0"]);
    }

    #[test]
    fn an_unnamed_device_is_kept() {
        // The list only ever removes sensors, so an unreadable `name` has to
        // fail towards keeping one. A CPU sensor dropped for being unnamed is
        // the temperature signal disappearing on hardware nobody tested — the
        // failure direction an allowlist would have had everywhere.
        let f = Fixture::new("unnamed");
        let dir = f.0.join("sys/class/hwmon/hwmon0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("temp1_input"), "91000\n").unwrap();

        let sensors = f.sensors();
        assert_eq!(sensors.read().hottest_celsius, Some(91.0));
        assert!(sensors.ignored().is_empty());
    }

    #[test]
    fn a_dropped_device_with_no_temperature_is_not_reported_as_dropped() {
        // Nothing was lost, so there is nothing to explain — and a status line
        // naming a sensor that never had a reading is noise that teaches people
        // to skip the line.
        let f = Fixture::new("nvme-no-temp");
        f.hwmon(0, "k10temp", &[("temp1_input", "50000\n")]);
        f.hwmon(1, "nvme", &[("in0_input", "3300\n")]);

        assert!(f.sensors().ignored().is_empty());
    }

    #[test]
    fn a_drive_keeps_its_fan_even_though_its_temperature_is_dropped() {
        // Only the temperature is a claim about the wrong hardware. A fan is a
        // fan whatever is beside it on the bus.
        let f = Fixture::new("drive-fan");
        f.hwmon(0, "k10temp", &[("temp1_input", "50000\n")]);
        f.hwmon(1, "nvme", &[("temp1_input", "70000\n"), ("fan1_input", "3200\n")]);

        let sensors = f.sensors();
        assert_eq!(sensors.read().hottest_celsius, Some(50.0));
        assert_eq!(sensors.read().fastest_rpm, Some(3200));
    }

    #[test]
    fn nothing_on_the_list_can_collide_with_a_cpu_sensor() {
        // The list is a denylist, so an entry generic enough to match a CPU
        // sensor would take the temperature signal away entirely — the failure
        // this shape was chosen to avoid. These are the names real CPU, chassis
        // and GPU sensors use.
        for cpu in [
            "k10temp",
            "coretemp",
            "zenpower",
            "acpitz",
            "cpu_thermal",
            "thinkpad",
            "amdgpu",
            "nouveau",
            "soc_thermal",
            "it8792",
            "nct6798",
        ] {
            assert!(speaks_for_the_machine(cpu), "{cpu} must not be excluded");
        }
        for other in ["nvme", "nvme0", "drivetemp", "iwlwifi_1", "ath11k_hwmon", "rtw89_phy0"] {
            assert!(!speaks_for_the_machine(other), "{other} must be excluded");
        }
    }

    #[test]
    fn the_monitor_reports_transitions_and_not_ticks() {
        // The daemon turns each `Some` into an event, so a per-tick answer would
        // put a thermal event in the trace every two seconds.
        let f = under_load();
        let mut m = Monitor::with_sensors(f.sensors(), calibrated());
        assert_eq!(m.poll(), Some(ThermalState::Hot), "the first reading is a transition");
        assert_eq!(m.poll(), None, "and the next twenty are not");
        assert_eq!(m.poll(), None);
        assert_eq!(m.state(), ThermalState::Hot);
    }

    /// A machine whose idle floor is above the clearing threshold.
    ///
    /// Read off a real machine: the fan sits at 4465 RPM with nothing running,
    /// against a `cool_rpm` of 2200. The state was
    /// `hot` for 2,977 of 3,125 recorded decisions and `normal` for 148, so
    /// every rule keyed on it was firing unconditionally — and a constant and a
    /// signal look identical from inside a rule.
    #[test]
    fn a_signal_that_can_never_clear_is_reported_as_saturated() {
        let f = Fixture::new("saturated");
        f.hwmon(8, "k10temp", &[("temp1_input", "57000\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4465\n")]);
        let mut m = Monitor::with_sensors(f.sensors(), calibrated());

        assert_eq!(m.poll(), Some(ThermalState::Hot));
        assert!(
            !m.saturated_after(std::time::Duration::from_secs(60)),
            "not on the strength of a minute"
        );
        for _ in 0..20 {
            assert_eq!(m.poll(), None, "and it never comes back");
        }
        assert!(m.saturated_after(SATURATED_AFTER), "half an hour of it, and it cannot clear");
        assert_eq!(m.quietest_while_hot().fastest_rpm, Some(4465));
    }

    /// Detecting a constant and then handing it to the rules anyway was the gap.
    ///
    /// `saturated` reached `dozectl status` and the log and stopped there, so
    /// the one reader that cannot tell a constant from a signal — the rule
    /// engine — went on matching `thermal = "hot"` unconditionally. Observed
    /// over a fortnight on a machine whose coolest moment was 43.25 °C against a
    /// `cool_celsius` of 72: the warning went out six times and nothing changed.
    #[test]
    fn a_saturated_signal_is_not_offered_to_the_rules() {
        let f = Fixture::new("saturated-rules");
        f.hwmon(8, "k10temp", &[("temp1_input", "57000\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4465\n")]);
        let mut m = Monitor::with_sensors(f.sensors(), calibrated());

        assert_eq!(m.poll(), Some(ThermalState::Hot));
        for _ in 0..20 {
            assert_eq!(m.poll(), None);
        }

        // Before the stretch is long enough, `hot` is still a claim worth acting
        // on and the rules get it.
        let minute = std::time::Duration::from_secs(60);
        assert!(!m.saturated_after(minute));
        assert_eq!(m.state_for_rules_after(minute), ThermalState::Hot);

        // Past it, the signal is a constant and the rules stop seeing it —
        // while the observation itself is untouched, because the thresholds are
        // the user's and `dozectl status` still has to show what the sensors say.
        assert!(m.saturated_after(SATURATED_AFTER));
        assert_eq!(m.state_for_rules_after(SATURATED_AFTER), ThermalState::Normal);
        assert_eq!(m.state(), ThermalState::Hot, "the observation is not rewritten");
        assert_eq!(m.thresholds().cool_celsius, calibrated().cool_celsius, "nor are the numbers");
    }

    /// Both thresholds reachable, never at the same moment — which is a working
    /// signal on a busy machine, not a stuck one.
    #[test]
    fn a_machine_whose_signals_take_turns_is_not_saturated() {
        let f = Fixture::new("busy");
        f.hwmon(8, "k10temp", &[("temp1_input", "85000\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4400\n")]);
        let mut m = Monitor::with_sensors(f.sensors(), calibrated());
        assert_eq!(m.poll(), Some(ThermalState::Hot));

        // The package cools while the fan is still spinning hard...
        fs::write(f.0.join("sys/class/hwmon/hwmon8/temp1_input"), "60000\n").unwrap();
        assert_eq!(m.poll(), None, "still hot: the fan is at 4400");
        // ...and later the fan settles while the package is hot again.
        fs::write(f.0.join("sys/class/hwmon/hwmon8/temp1_input"), "85000\n").unwrap();
        fs::write(f.0.join("sys/class/hwmon/hwmon7/fan1_input"), "900\n").unwrap();
        assert_eq!(m.poll(), None, "still hot: the package is at 85C");
        assert_eq!(m.state(), ThermalState::Hot, "no single reading ever cleared it");

        assert!(
            !m.saturated_after(SATURATED_AFTER),
            "but each threshold was reached, so neither is out of this machine's range"
        );
    }

    /// The false positive that set [`SATURATED_AFTER`] where it is: a machine
    /// genuinely pinned at 85 °C is hot, and half an hour of it is an afternoon
    /// rather than a fault. It is still reported eventually — four hours of
    /// unconditional `thermal` rules is worth saying either way — but the
    /// message claims the consequence and not the cause.
    #[test]
    fn a_genuinely_hot_machine_is_not_flagged_on_the_strength_of_an_afternoon() {
        let f = Fixture::new("genuinely-hot");
        f.hwmon(8, "k10temp", &[("temp1_input", "85000\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4400\n")]);
        let mut m = Monitor::with_sensors(f.sensors(), Thresholds::default());
        assert_eq!(m.poll(), Some(ThermalState::Hot));

        assert!(!m.saturated_after(std::time::Duration::from_secs(30 * 60)));
        assert!(!m.saturated_after(std::time::Duration::from_secs(2 * 3600)));
        assert!(m.saturated_after(SATURATED_AFTER), "four hours of it is worth saying");
    }

    #[test]
    fn clearing_the_state_restarts_the_saturation_clock() {
        let f = under_load();
        let mut m = Monitor::with_sensors(f.sensors(), calibrated());
        assert_eq!(m.poll(), Some(ThermalState::Hot));
        assert!(m.saturated_after(SATURATED_AFTER));

        // Everything quietens down, and the state clears.
        fs::write(f.0.join("sys/class/hwmon/hwmon7/fan1_input"), "0\n").unwrap();
        fs::write(f.0.join("sys/class/hwmon/hwmon7/fan2_input"), "0\n").unwrap();
        fs::write(f.0.join("sys/class/hwmon/hwmon8/temp1_input"), "45000\n").unwrap();
        assert_eq!(m.poll(), Some(ThermalState::Normal));
        assert!(!m.saturated(), "a signal that just cleared is not a stuck one");
        assert_eq!(
            m.quietest_while_hot(),
            Reading::default(),
            "and the next hot stretch is judged on its own readings, not the last one's"
        );
    }

    /// Taking the fan out of the decision needs *both* of its thresholds moved,
    /// and this is the pair of asserts that says why.
    #[test]
    fn a_fan_signal_is_only_inert_when_both_thresholds_clear_its_range() {
        let f = Fixture::new("inert");
        f.hwmon(8, "k10temp", &[("temp1_input", "48850\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4470\n")]);
        let sensors = f.sensors();

        // Half the move: the fan can no longer raise the alarm, and can still
        // block every clear, because clearing wants `rpm < cool_rpm`.
        let half =
            Thresholds { hot_rpm: Some(6000), cool_rpm: Some(2200), ..Thresholds::default() };
        assert_eq!(
            sensors.read().state(ThermalState::Hot, &half),
            ThermalState::Hot,
            "48.85C is well under cool_celsius, so the fan is the only thing holding this"
        );

        // Both moved past what the fan reaches: it now contributes nothing in
        // either direction, which is exactly the fanless machine's branch.
        let both =
            Thresholds { hot_rpm: Some(6000), cool_rpm: Some(5500), ..Thresholds::default() };
        assert_eq!(sensors.read().state(ThermalState::Hot, &both), ThermalState::Normal);
        assert_eq!(sensors.read().state(ThermalState::Normal, &both), ThermalState::Normal);

        // And the temperature still decides, which is the point of doing it.
        fs::write(f.0.join("sys/class/hwmon/hwmon8/temp1_input"), "85000\n").unwrap();
        assert_eq!(sensors.read().state(ThermalState::Normal, &both), ThermalState::Hot);
    }

    #[test]
    fn new_thresholds_restart_the_saturation_clock_but_keep_the_state() {
        let f = under_load();
        let mut m = Monitor::with_sensors(f.sensors(), calibrated());
        assert_eq!(m.poll(), Some(ThermalState::Hot));
        assert!(m.saturated_after(SATURATED_AFTER));

        // The user reads that warning and edits the thresholds. The claim it
        // made was about the old pair, so the clock starts again — but what the
        // machine is doing has not changed, and the next poll re-judges it.
        m.set_thresholds(Thresholds { hot_rpm: Some(6000), cool_rpm: Some(5500), ..calibrated() });
        assert_eq!(m.state(), ThermalState::Hot, "the observation survives the edit");
        assert!(!m.saturated(), "but the claim about the old thresholds does not");
        assert_eq!(m.poll(), Some(ThermalState::Normal), "and the next reading clears it");

        // Setting the same values again is not an edit and must not reset it.
        m.set_thresholds(Thresholds { hot_rpm: Some(6000), cool_rpm: Some(5500), ..calibrated() });
        assert_eq!(m.state(), ThermalState::Normal);
    }

    /// The fan says nothing until this machine has shown it a range, and then
    /// says something scaled to that range rather than to somebody else's.
    #[test]
    fn the_fan_thresholds_are_learned_from_the_machine_not_shipped() {
        let mut range = FanRange::default();
        assert_eq!(range.thresholds(), None, "nothing seen, nothing to say");

        range.observe(Some(4470));
        assert_eq!(range.thresholds(), None, "one speed is not a range");

        // A fan that barely moves is not a signal, whatever its absolute speed.
        range.observe(Some(4300));
        assert_eq!(
            range.thresholds(),
            None,
            "170 RPM of span is inside the noise; temperature decides alone"
        );

        // The machine from the journal that prompted all of this: idles at 3161,
        // peaks at 4484. A shipped `hot_rpm = 3000` sits below the whole range
        // and pins the state at `hot`; thresholds cut from the span do not.
        let mut range = FanRange::default();
        range.observe(Some(3161));
        range.observe(Some(4484));
        let (hot, cool) = range.thresholds().expect("1323 RPM of span is a signal");
        assert!(hot > 3161 && hot < 4484, "hot must sit inside the range, got {hot}");
        assert!(cool > 3161 && cool < hot, "and cool below it with a gap, got {cool}");

        // Which is the whole claim: this machine's idle no longer reads as hot.
        let idle = Reading { hottest_celsius: Some(48.8), fastest_rpm: Some(3161) };
        let t = Thresholds { hot_rpm: Some(hot), cool_rpm: Some(cool), ..Thresholds::default() };
        assert_eq!(idle.state(ThermalState::Hot, &t), ThermalState::Normal);
        let busy = Reading { hottest_celsius: Some(48.8), fastest_rpm: Some(4484) };
        assert_eq!(busy.state(ThermalState::Normal, &t), ThermalState::Hot);
    }

    #[test]
    fn a_pinned_pair_is_not_overruled_by_what_the_machine_does() {
        // Calibration fills a gap; it does not argue with somebody who knows
        // their hardware.
        let f = under_load();
        let pinned =
            Thresholds { hot_rpm: Some(9000), cool_rpm: Some(8000), ..Thresholds::default() };
        let mut m = Monitor::with_sensors(f.sensors(), pinned);
        assert_eq!(m.poll(), None, "4484 is nowhere near 9000, so nothing is hot");
        assert_eq!(m.effective().hot_rpm, Some(9000), "and the config still says 9000");
    }

    #[test]
    fn an_uncalibrated_fan_leaves_the_decision_to_temperature() {
        let f = Fixture::new("uncalibrated");
        // Hot by any fan threshold that could be learned, but nothing has been
        // learned yet — and cool by temperature.
        f.hwmon(8, "k10temp", &[("temp1_input", "50000\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "4470\n")]);
        let mut m = Monitor::with_sensors(f.sensors(), Thresholds::default());

        assert_eq!(m.poll(), None, "a single fan speed calibrates nothing");
        assert_eq!(m.state(), ThermalState::Normal);
        assert_eq!(m.effective().hot_rpm, None, "so the fan is out of the decision");

        // The temperature is still very much in it.
        fs::write(f.0.join("sys/class/hwmon/hwmon8/temp1_input"), "85000\n").unwrap();
        assert_eq!(m.poll(), Some(ThermalState::Hot));
    }

    /// Read off a live machine three days after calibration shipped: one
    /// sentinel sample taught the range 0..65535 and put the thresholds at
    /// "hot >= 43253 rpm, cool < 26214" — unreachable in both directions.
    #[test]
    fn a_sentinel_reading_is_not_a_fan_speed() {
        let f = Fixture::new("sentinel");
        f.hwmon(8, "k10temp", &[("temp1_input", "56000\n")]);
        // 0xFFFF, which is what a driver reports when it cannot answer. It
        // passes every check a plain `>= 0` makes, and being the largest value
        // present it wins the `max()` outright.
        f.hwmon(7, "thinkpad", &[("fan1_input", "65535\n"), ("fan2_input", "2801\n")]);

        let reading = f.sensors().read();
        assert_eq!(
            reading.fastest_rpm,
            Some(2801),
            "the real fan beside it is the answer, not the marker"
        );

        // And it cannot reach the calibration, which is where the damage was.
        // The second speed is a real idle rather than a zero: a stopped fan is
        // deliberately not a point on this scale — see `FanRange::observe`.
        let mut range = FanRange::default();
        range.observe(reading.fastest_rpm);
        range.observe(Some(1900));
        assert_eq!(range.observed(), Some((1900, 2801)));
        let (hot, _cool) = range.thresholds().expect("901 rpm of span is a signal");
        assert!(hot < IMPLAUSIBLE_RPM, "a threshold inside the range this fan reaches: {hot}");
    }

    #[test]
    fn a_fan_that_stops_does_not_drag_the_cool_threshold_below_its_idle() {
        // The measured failure, with this machine's own numbers. Observed range
        // 0-4518 put `cool_rpm` at 1807; the fan's quietest over three separate
        // four-hour stretches was 2114, 2188 and 2360, so it never cleared —
        // while the temperature sat at 43-48 °C against a 72 °C threshold.
        // Clearing needs both, so the state was pinned `hot` on a cold machine,
        // which is `hot_rpm = 3000` reproduced by the mechanism built to replace
        // it.
        let mut range = FanRange::default();
        range.observe(Some(0)); // genuinely off, at boot
        for rpm in [2114, 2188, 2360, 3673, 4076, 4518] {
            range.observe(Some(rpm));
        }

        assert_eq!(
            range.observed(),
            Some((2114, 4518)),
            "the range is the speeds this fan runs at, not the fact it can stop"
        );
        let (hot, cool) = range.thresholds().expect("2404 rpm of span is a signal");
        assert!(cool > 2360, "the quietest this fan managed while stuck must clear: cool={cool}");
        assert!(hot < 4076, "and a fan at 4076 must still read hot: hot={hot}");

        // The old behaviour, stated so the regression is legible: with the zero
        // in the range, `cool` lands below anything the fan does in operation.
        let span_with_zero = 4518;
        let old_cool = (span_with_zero as f32 * COOL_AT) as u32;
        assert!(old_cool < 2114, "this is what it used to compute: {old_cool}");
    }

    #[test]
    fn a_stopped_fan_still_clears_the_state() {
        // Excluding zero from the *calibration* must not exclude it from the
        // *reading*. A fan that is off is below every threshold and says so.
        let mut range = FanRange::default();
        for rpm in [2114, 4518] {
            range.observe(Some(rpm));
        }
        let (hot, cool) = range.thresholds().expect("a span this wide is a signal");
        let t = Thresholds { hot_rpm: Some(hot), cool_rpm: Some(cool), ..calibrated() };

        let off = Reading { hottest_celsius: Some(45.0), fastest_rpm: Some(0) };
        assert_eq!(
            off.state(ThermalState::Hot, &t),
            ThermalState::Normal,
            "a fan that has stopped is the quietest a machine gets"
        );
    }

    #[test]
    fn a_machine_whose_only_fan_reading_is_a_sentinel_reports_no_fan() {
        let f = Fixture::new("all-sentinel");
        f.hwmon(8, "k10temp", &[("temp1_input", "56000\n")]);
        f.hwmon(7, "thinkpad", &[("fan1_input", "65535\n")]);
        // `None`, the same as a fanless machine — which is the honest answer,
        // and puts the fan out of the decision rather than into it at a made-up
        // speed.
        assert_eq!(f.sensors().read().fastest_rpm, None);
    }

    #[test]
    #[ignore = "reads this machine's own sensors"]
    fn the_real_machine_answers() {
        let sensors = Sensors::new();
        assert!(sensors.available(), "no hwmon at all on a real Linux box?");
        let reading = sensors.read();
        println!(
            "{} temps, {} fans: {reading:?} -> {:?}",
            sensors.temp_count(),
            sensors.fan_count(),
            reading.state(ThermalState::Normal, &Thresholds::default())
        );
        if let Some(c) = reading.hottest_celsius {
            assert!((10.0..130.0).contains(&c), "implausible temperature {c}");
        }
        if let Some(rpm) = reading.fastest_rpm {
            assert!(rpm < 20_000, "implausible fan speed {rpm}");
        }
    }
}
