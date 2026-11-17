//! Measuring what dozing is actually worth, instead of modelling it.
//!
//! §11's report converts CPU-seconds into a fraction of a charge at 2-4 J each.
//! That model has two problems and only one of them is the width of the range.
//! The other is that it cannot be caught being wrong: it goes up when dozed does
//! more, whether or not the battery noticed, so it measures dozed's activity and
//! calls it a saving. §1 asks the tool to be *provable*, and a number that cannot
//! disagree with reality does not prove anything.
//!
//! Three counters, all readable without privilege, close that gap:
//!
//! * **Discharge, in watts.** `power_now` from the battery, or the slope of
//!   `energy_now` where the firmware does not offer a power figure. This is the
//!   quantity the whole tool exists to reduce, and the one the model can only
//!   guess at.
//! * **Deep-idle residency.** `cpuidle`'s per-state time counters. A wakeup costs
//!   battery precisely because it drags a core out of a deep C-state, so the
//!   fraction of time the cores spend in the deepest state the driver offers is
//!   the direct physical consequence of the metric §13 optimises. It also moves
//!   on a plugged-in machine, where discharge is flat zero and the model refuses
//!   to speak.
//! * **System-wide wakeups.** `ctxt` from `/proc/stat`, one read, no walk. The
//!   per-app figure in `sampler` is a sum over the processes dozed knows about;
//!   this is the whole machine, so the two disagreeing means dozed is watching
//!   the wrong things.
//!
//! Everything in this module is arithmetic on counters somebody else read. The
//! reading lives in `dozed_linux::energy`, because core performs no I/O.
//!
//! ## What the counters actually did, on the machine this was written against
//!
//! Measured, because §1's second non-negotiable says to:
//!
//! | Counter | Reading |
//! |---|---|
//! | `power_now` while on AC | **0** — the firmware reports nothing, not a small number |
//! | `status` while on AC and full-ish | `Not charging` (neither `Charging` nor `Discharging`) |
//! | `energy_now` granularity | 10 mWh, i.e. 36 J — fine over a block, useless over a tick |
//! | `cpuidle` states | 4 (POLL, C1, C2, C3), 16 CPUs, 128 files per sweep |
//!
//! Two consequences are baked into the code below. Discharge is unavailable on
//! mains rather than zero — [`Rates::discharge_w`] is `None` there, and a `Some(0.0)`
//! would have read as "this machine uses no power". And `energy_now` differencing
//! only produces a usable number over tens of seconds, so it is offered as a
//! fallback with a minimum interval rather than as an equal alternative.

use serde::{Deserialize, Serialize};

use crate::stats;
use crate::time::MonoTime;

/// One read of the platform's power counters.
///
/// Every field is optional and independently so: a desktop has no battery, a
/// virtual machine may expose no `cpuidle`, and a kernel could drop `/proc/stat`
/// fields we expect. Nothing here is load-bearing for safety — the worst case is
/// that the measured report has less to say than the modelled one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Reading {
    /// Instantaneous discharge in watts, when the battery reports one *and* is
    /// actually discharging.
    pub discharge_w: Option<f64>,
    /// Remaining energy in µWh, for the fallback slope. Also `None` on mains,
    /// where a rising figure is charge going in rather than out.
    pub energy_uwh: Option<u64>,
    /// Cumulative CPU package energy in µJ, from RAPL, where the counter is
    /// readable at all — `None` on a stock kernel, where it is `0400 root`
    /// (`dozed_linux::energy`).
    ///
    /// Already corrected for the counter's wrap by the platform layer, so this is
    /// monotonic and can be differenced like any other cumulative reading.
    pub package_uj: Option<u64>,
    pub idle: Option<IdleCounters>,
    /// The package's own sub-domains, in µJ, each already wrap-corrected and
    /// accumulated by the platform layer exactly as [`Self::package_uj`] is.
    ///
    /// **Subsets of `package_uj`, never addends to it** — see [`Components`].
    pub components: Components,
    /// Instantaneous discrete-GPU draw in µW, from `amdgpu`'s
    /// `hwmon/power1_average`.
    ///
    /// Not a cumulative counter and not differenced: the driver reports a
    /// running average in watts directly, so there is nothing to accumulate and
    /// no wrap to correct. Kept in µW because that is the unit the file is in,
    /// and converting on the way out keeps the sentinel checks in one place
    /// ([`IMPLAUSIBLE_W`]).
    pub gpu_uw: Option<u64>,
    /// The aggregate `cpu` line of `/proc/stat`, for the denominator every
    /// per-app attribution is taken against.
    pub cpu: Option<CpuTime>,
    /// `gpu_busy_percent`: how much of the GPU's time was spent doing work.
    ///
    /// The denominator for per-app engine time (`dozed_linux::gpu`), and the same
    /// kind of figure `/proc/stat` provides for the CPU — it includes work no
    /// client owns, which is why the two do not sum to each other.
    pub gpu_busy_pct: Option<u32>,
    /// `ctxt` from `/proc/stat`: context switches since boot, whole machine.
    pub ctxt_switches: Option<u64>,
}

/// The RAPL domains *inside* a package, in µJ.
///
/// **These are subsets of the package, not addends to it.** `intel-rapl:0:0` is
/// `core`, part of `intel-rapl:0`, and §11.1 already names the trap once for
/// the package total — summing a child beside its parent reports a CPU drawing
/// half again what it draws. The trap gets worse rather than better once the
/// children are on screen, because a breakdown *looks* like it should add up,
/// so nothing in this type is summable with `package_uj` and the renderers put
/// these inside the package figure rather than beside it.
///
/// Three named fields rather than a map, for two reasons. These are the three
/// domains the `intel-rapl` driver defines beneath a package, so the set is
/// closed by the driver rather than by us; and a fixed set keeps [`Reading`]
/// `Copy` and allocation-free on a path that runs on every sampling tick, which
/// is the same reason `sources::SystemProxies` resolves once.
///
/// `psys` is deliberately absent, and is not an oversight for a second time:
/// it is a *sibling* of the package rather than a child — it measures the whole
/// platform, screen included — so it is neither the package nor a part of one,
/// and putting it in here would make the "inside the package" contract a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Components {
    /// The cores. The one sub-domain that is near-universal: present on this
    /// AMD machine, which exposes `package-0` and `core` and nothing else.
    pub core_uj: Option<u64>,
    /// The rest of the package — on client Intel parts this is the integrated
    /// graphics, which is why it can be the larger half on an idle laptop.
    pub uncore_uj: Option<u64>,
    /// The memory controller. Server parts, mostly.
    pub dram_uj: Option<u64>,
}

/// The aggregate `cpu` line of `/proc/stat`, in the kernel's `USER_HZ` ticks.
///
/// Carried as busy-and-total rather than as a duration because only the *ratio*
/// is ever used, and a ratio needs no clock tick: `USER_HZ` cancels, and so does
/// any error in how long the interval was believed to be. That matters more than
/// it looks — `sysconf(_SC_CLK_TCK)` is a libc call, this workspace is
/// `#![forbid(unsafe_code)]`, and hardcoding 100 would have been a constant
/// nobody could check.
///
/// Reading it costs nothing. `/proc/stat` is already read in full for `ctxt`
/// (§11.1), and this is two more lines of a string we have; §13's rule is about
/// new *wakeups*, and there are none here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTime {
    /// `user + nice + system + irq + softirq + steal`.
    ///
    /// `guest` and `guest_nice` are excluded on purpose: the kernel already
    /// counts them inside `user` and `nice`, so adding them would inflate the
    /// denominator every app's share is divided by and shrink every app's
    /// attributed watts on precisely the machines that run VMs.
    pub busy_ticks: u64,
    /// Everything in [`Self::busy_ticks`] plus `idle` and `iowait`.
    ///
    /// `iowait` is a *kind of idle* — the CPU is executing nothing, it is
    /// waiting for a disk — so it belongs here and not in the busy half.
    /// Counting it as busy would quietly shrink every app's share on a machine
    /// that is merely waiting for storage.
    pub total_ticks: u64,
    /// How many `cpuN` lines followed the aggregate one.
    ///
    /// Counted here rather than taken from [`IdleCounters::cpus`] because a
    /// container or a VM can expose no `cpuidle` at all and still needs a core
    /// count — and because it then comes from the same read as the ticks it
    /// scales, so the two cannot disagree with each other.
    pub cpus: u32,
}

/// Cumulative `cpuidle` counters, summed across CPUs.
///
/// Summed rather than per-CPU because the question is about the machine: one core
/// held out of C6 by a chatty timer costs battery whichever core it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleCounters {
    /// How many CPUs contributed. Carried so a hotplug can be detected and the
    /// interval discarded, rather than reported as a residency above 100%.
    pub cpus: u32,
    /// Time in the deepest state each CPU offers, in microseconds.
    ///
    /// "Deepest" is the highest-numbered state, which the kernel's cpuidle
    /// framework requires to be the one with the longest exit latency — so this
    /// is portable across `intel_idle`, `acpi_idle` and the AMD equivalents
    /// without this file needing to know what a C-state is called.
    pub deepest_usec: u64,
    /// Time in any idle state, in microseconds. Includes `POLL`, which is idle
    /// that saves nothing — reported separately from `deepest_usec` for exactly
    /// that reason.
    pub idle_usec: u64,
    /// Entries into any idle state: the machine-wide count of "went to sleep and
    /// was woken again".
    pub entries: u64,
}

/// Rates and fractions derived from two [`Reading`]s.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rates {
    /// Watts. `None` on mains, where the battery is not paying.
    pub discharge_w: Option<f64>,
    /// CPU package watts, from RAPL where it is readable.
    ///
    /// A different quantity from [`Self::discharge_w`] and not a substitute for
    /// it: this is what the processor package spends, where that is what the whole
    /// machine draws. Its compensation is that it is measurable on mains.
    pub package_w: Option<f64>,
    /// Fraction of wall-clock time, across all CPUs, spent in the deepest idle
    /// state. 0.0 to 1.0.
    pub deepest_idle: Option<f64>,
    /// Fraction spent in any idle state, deep or shallow.
    pub idle: Option<f64>,
    /// Machine-wide context switches per second.
    pub system_wakeups_per_sec: Option<f64>,
    /// Machine-wide idle entries per second: how often *something* woke a core.
    pub idle_entries_per_sec: Option<f64>,
    /// The package's own sub-domains, in watts. **Inside [`Self::package_w`]**,
    /// never beside it — see [`PackageBreakdown`].
    pub package_parts: PackageBreakdown,
    /// Discrete-GPU watts, where a driver reports them.
    ///
    /// A third quantity again, and one that is *not* inside the package: on a
    /// laptop with switchable graphics this is a separate chip on a separate
    /// rail, and on one with only integrated graphics it is absent and the GPU's
    /// draw is inside [`Self::package_w`] as `uncore`. Two machines, two right
    /// answers, and no way to add them into one number honestly — so they stay
    /// separate figures with separate labels.
    pub gpu_w: Option<f64>,
    /// GPU busy, 0.0 to 1.0, from `gpu_busy_percent`.
    ///
    /// Taken from the newest reading rather than differenced: the driver reports
    /// a utilisation, not a counter, exactly as it does for [`Self::gpu_w`].
    pub gpu_busy: Option<f64>,
    /// Cores busy, machine-wide, from the aggregate `cpu` line of `/proc/stat`.
    ///
    /// The denominator per-app attribution divides by (`crate::attribution`),
    /// and the only figure here that is about *all* processes rather than the
    /// ones dozed can see. `1.0` is one core fully busy, so a 16-core machine
    /// tops out at 16.0 — the same unit `coverage::Coverage::tracked_cores`
    /// already uses, and for the same reason: percent of a single core summed
    /// over a machine reads as an arithmetic bug.
    pub machine_cores_busy: Option<f64>,
}

/// A watt figure that no counter in a computer can honestly produce.
///
/// The same instinct as `thermal::IMPLAUSIBLE_RPM` and `sampler::cpu_ceiling_pct`,
/// applied to energy, and for the same reason both of those exist: this project
/// has twice shipped a sentinel that arrived looking exactly like data — a fan
/// reporting `0xFFFF`, a sampler reporting 4246% CPU on 16 cores — and watts have
/// two more ways to do it than either.
///
/// * A counter that **resets** rather than wraps is indistinguishable from a wrap
///   at the moment it happens, and `dozed_linux::energy` corrects it as a wrap:
///   `max_energy_range_uj - previous + current`. On this machine that is up to
///   65.5 kJ credited to one interval, which over the 2 s sampling floor is
///   **32 kW**.
/// * `0xFFFFFFFF` µW, hwmon's way of saying nothing, is 4,295 W.
///
/// 1,000 W is far above any real reading — a four-socket server package tops out
/// in the low hundreds and a laptop's is single digits — and far below both of
/// those. A ceiling rather than an equality test with either marker, because the
/// class of bug is "the counter emitted something that is not a measurement" and
/// there is more than one such thing.
///
/// It is honestly incomplete, and saying so is cheaper than discovering it: the
/// same reset differenced over a 60 s tick is 1.1 kW and *is* caught, but one
/// over a several-minute gap would land inside the ceiling and be believed. This
/// catches the reading that is three orders of magnitude out, which is the one
/// that has actually happened, not the last few percent.
pub const IMPLAUSIBLE_W: f64 = 1_000.0;

/// The sub-domain watts inside a package total.
///
/// Every consumer of this wants the same two things — the parts in a stable
/// order, and the remainder the parts do not account for — and both are easy to
/// get subtly wrong in a way that puts a fiction on screen, so they live here
/// once rather than in each renderer.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct PackageBreakdown {
    /// Each defaulted independently: a machine exposing only `core` — which is
    /// every AMD part seen — must decode as one part known and two absent, not
    /// as a decode failure that costs the package figure too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncore_w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dram_w: Option<f64>,
}

impl PackageBreakdown {
    /// The parts that were readable, labelled, in a fixed order.
    ///
    /// Fixed rather than sorted by size: a breakdown whose rows reorder between
    /// two-second polls is unreadable, and the eye is looking for a value
    /// changing, not a row moving.
    pub fn parts(&self) -> impl Iterator<Item = (&'static str, f64)> {
        [("core", self.core_w), ("uncore", self.uncore_w), ("dram", self.dram_w)]
            .into_iter()
            .filter_map(|(name, w)| Some((name, w?)))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts().next().is_none()
    }

    /// What the package spends that no readable sub-domain accounts for.
    ///
    /// `None` when nothing was readable — there is no remainder to speak of when
    /// there is nothing to subtract — and `None` again when the subtraction comes
    /// out **negative**, which is the case worth handling rather than rendering.
    /// The domains are read one file at a time while the CPU keeps running, so a
    /// sweep can straddle a busy moment and hand a child slightly more than its
    /// parent got. A negative "rest" would be nonsense on screen and, worse, would
    /// be read as the package under-reporting; the honest answer is to say nothing
    /// about the remainder for that interval, exactly as
    /// `Meter::observe` discards an interval a CPU was hotplugged across.
    #[must_use]
    pub fn rest_w(&self, package_w: f64) -> Option<f64> {
        let accounted: f64 = self.parts().map(|(_, w)| w).sum();
        (!self.is_empty()).then_some(package_w - accounted).filter(|rest| *rest >= 0.0)
    }
}

impl Rates {
    /// Whether this carries anything worth recording. A reading with nothing in
    /// it must not be accumulated, or a block's mean is diluted by samples that
    /// measured nothing.
    ///
    /// Everything measured *about the machine* rather than about its energy is
    /// deliberately not counted here: [`Self::package_w`], its sub-domains, the
    /// GPU figures, and [`Self::machine_cores_busy`]. They are all real readings
    /// and none of them is what a trial block is a sample *of*, so admitting one
    /// would let a reading with no discharge and no idle figure count as a
    /// sample — the dilution this method exists to prevent, arriving by a
    /// different door each time a counter is added. The rule is the one below,
    /// stated once for all of them.
    ///
    /// [`Self::package_w`] is the case that makes it concrete. [`crate::trial`] does not
    /// judge on it — its verdict is discharge and deep idle — so a reading that
    /// carried only package watts would be admitted to a block as a sample and
    /// then contribute to none of its means, which is exactly the dilution this
    /// exists to prevent. Teaching the trial to use RAPL is a change to what the
    /// experiment measures, and belongs with that decision rather than here.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.discharge_w.is_none()
            && self.deepest_idle.is_none()
            && self.idle.is_none()
            && self.system_wakeups_per_sec.is_none()
    }
}

/// The shortest interval over which differencing `energy_now` says anything.
///
/// The counter on this machine moves in 10 mWh steps, so over 5 seconds at 6 W
/// the true change is 8 mWh and the reading is therefore either 0 or 10 mWh —
/// a rate of either zero or 20 W. Over a minute the same granularity is under
/// 1% of the answer.
const MIN_SLOPE_INTERVAL_MS: u64 = 30_000;

/// Turns successive [`Reading`]s into [`Rates`].
///
/// Stateful in the same shape as `sampler::Sampler`, and for the same reason: a
/// cumulative counter is not a rate until you have two of them.
#[derive(Debug, Clone, Default)]
pub struct Meter {
    last: Option<(Reading, MonoTime)>,
    /// The far end of the energy-counter slope, in µWh.
    ///
    /// Kept apart from `last` because the slope needs a longer baseline than
    /// anything else here ([`MIN_SLOPE_INTERVAL_MS`]) and therefore cannot be the
    /// previous reading — see [`Meter::slope_w`].
    slope_anchor: Option<(u64, MonoTime)>,
}

impl Meter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in a reading. `None` until there are two, and for any interval too
    /// short or too strange to derive from.
    pub fn observe(&mut self, reading: Reading, at: MonoTime) -> Option<Rates> {
        // Seeded here, before the `previous?` below, because the slope wants the
        // *oldest* reading available: anchoring on the second one instead would
        // put the first wattage another `MIN_SLOPE_INTERVAL_MS` away for no
        // reason.
        if self.slope_anchor.is_none() {
            self.slope_anchor = reading.energy_uwh.map(|uwh| (uwh, at));
        }
        let previous = self.last.replace((reading, at));
        let (before, before_at) = previous?;
        let elapsed_ms = at.ms.saturating_sub(before_at.ms);
        if elapsed_ms == 0 {
            return None;
        }
        let secs = elapsed_ms as f64 / 1000.0;

        // Preferred: the firmware's own figure. Fallback: the slope of the energy
        // counter, over an interval of its own choosing.
        let discharge_w = reading.discharge_w.or_else(|| self.slope_w(&reading, at));

        let (deepest_idle, idle, idle_entries_per_sec) = match (before.idle, reading.idle) {
            // A CPU coming online or going offline changes what the sums are
            // sums *over*, so the difference is not a residency. Discarding one
            // interval is the honest response; there is no way to repair it.
            (Some(a), Some(b)) if a.cpus == b.cpus && b.cpus > 0 => {
                let cpu_usec = f64::from(b.cpus) * elapsed_ms as f64 * 1000.0;
                let deepest = b.deepest_usec.saturating_sub(a.deepest_usec) as f64 / cpu_usec;
                let any = b.idle_usec.saturating_sub(a.idle_usec) as f64 / cpu_usec;
                let entries = b.entries.saturating_sub(a.entries) as f64 / secs;
                // Clamped, because the counters are read one file at a time
                // while the CPUs keep running: a sweep that straddles a long
                // idle period can attribute slightly more than the interval.
                (Some(deepest.clamp(0.0, 1.0)), Some(any.clamp(0.0, 1.0)), Some(entries))
            }
            _ => (None, None, None),
        };

        let system_wakeups_per_sec = match (before.ctxt_switches, reading.ctxt_switches) {
            (Some(a), Some(b)) => Some(b.saturating_sub(a) as f64 / secs),
            _ => None,
        };

        // No minimum interval, unlike the battery slope above: RAPL counts in
        // microjoules, so even a two-second interval is thousands of steps rather
        // than the one-or-none the 10 mWh battery counter offers.
        let package_w = joules_per_sec(before.package_uj, reading.package_uj, secs);
        let package_parts = PackageBreakdown {
            core_w: joules_per_sec(before.components.core_uj, reading.components.core_uj, secs),
            uncore_w: joules_per_sec(
                before.components.uncore_uj,
                reading.components.uncore_uj,
                secs,
            ),
            dram_w: joules_per_sec(before.components.dram_uj, reading.components.dram_uj, secs),
        };

        // Taken from the *new* reading alone. `power1_average` is already a
        // wattage the driver averaged for us, so there is no interval here to
        // difference and none of the wrap machinery above applies — the only
        // thing it shares with RAPL is the need for a ceiling.
        let gpu_w =
            reading.gpu_uw.map(|uw| uw as f64 / 1e6).filter(|w| (0.0..IMPLAUSIBLE_W).contains(w));

        // Cores busy, machine-wide. A ratio of tick deltas times the core count,
        // so `USER_HZ` never appears and neither does `secs` — see [`CpuTime`].
        let machine_cores_busy = match (before.cpu, reading.cpu) {
            (Some(a), Some(b)) if b.cpus > 0 => {
                let total = b.total_ticks.saturating_sub(a.total_ticks);
                let busy = b.busy_ticks.saturating_sub(a.busy_ticks);
                // A zero denominator is a tick that has not advanced — two reads
                // inside one `USER_HZ` period, which at the 2 s sampling floor
                // should not happen and is not worth dividing by if it does. And
                // `busy` cannot honestly exceed `total`, since it is a subset of
                // the same sum; if it does, one of the two reads was torn.
                (total > 0 && busy <= total).then(|| busy as f64 / total as f64 * f64::from(b.cpus))
            }
            _ => None,
        };

        Some(Rates {
            gpu_busy: reading.gpu_busy_pct.map(|p| f64::from(p) / 100.0),
            discharge_w,
            package_w,
            deepest_idle,
            idle,
            system_wakeups_per_sec,
            idle_entries_per_sec,
            package_parts,
            gpu_w,
            machine_cores_busy,
        })
    }

    /// Watts from the slope of the energy counter, for hardware that reports no
    /// wattage of its own.
    ///
    /// Measured against its own anchor rather than against the previous reading,
    /// because the sampling tick is not a constant: it moves between 60s and a 2s
    /// floor (`sampler::adaptive_interval`), and over 2s a counter that steps in
    /// 10 mWh says either 0 W or 20 W. Differencing the previous reading
    /// therefore fell below [`MIN_SLOPE_INTERVAL_MS`] and returned `None` on
    /// *every* sample for as long as the tick stayed tight — so on a battery with
    /// no `power_now`, the wattage vanished from `dozectl status` and from the
    /// dashboard header exactly when an app was about to be acted on, which is
    /// when somebody is most likely to be looking.
    ///
    /// The anchor advances only when it has been used, so successive answers are
    /// each over a fresh interval of at least that length rather than over an
    /// ever-widening one.
    fn slope_w(&mut self, reading: &Reading, at: MonoTime) -> Option<f64> {
        let (from, from_at) = self.slope_anchor?;
        let elapsed_ms = at.ms.saturating_sub(from_at.ms);
        if elapsed_ms < MIN_SLOPE_INTERVAL_MS {
            return None;
        }
        let energy_uwh = reading.energy_uwh?;
        self.slope_anchor = Some((energy_uwh, at));
        // A rise is charge going in, which is not a discharge rate. Nor is a flat
        // counter evidence of zero draw — it is evidence the step has not been
        // crossed yet — but reporting the zero is correct on average over a
        // block, and the caller only accumulates on battery.
        let drop_uwh = from.checked_sub(energy_uwh)?;
        // µWh over seconds into watts: µWh × 3600 / 1e6 = joules.
        Some(drop_uwh as f64 * 3600.0 / 1e6 / (elapsed_ms as f64 / 1000.0))
    }

    /// Forget the previous reading, so the next interval starts fresh.
    ///
    /// Called across a suspend: the counters keep running through a resume but
    /// the wall clock does not, and a rate taken across the gap is meaningless.
    /// The slope anchor goes with it, for the same reason and more so: it spans
    /// the longest interval here and would otherwise be the one reading that
    /// straddles the sleep.
    pub fn reset(&mut self) {
        self.last = None;
        self.slope_anchor = None;
    }
}

/// Two readings of one cumulative µJ counter, as watts — or nothing.
///
/// The ceiling lives here rather than in each caller because every domain has
/// the same two ways to lie and there are now five of them. See
/// [`IMPLAUSIBLE_W`] for what it catches and what it admits it does not.
///
/// A reading that fails it is dropped, not clamped, which is the judgement
/// `sampler::Sampler::record` already makes about an impossible sample: a clamp
/// hands the caller a number nobody measured, and this one would go on to be
/// apportioned between apps as though it were real.
fn joules_per_sec(before: Option<u64>, after: Option<u64>, secs: f64) -> Option<f64> {
    let (a, b) = (before?, after?);
    let w = b.saturating_sub(a) as f64 / 1e6 / secs;
    // A range rather than three comparisons, and it handles the fourth case for
    // free: `contains` is false for `NaN` and for infinity, both of which a
    // division by a zero-length interval can produce.
    (0.0..IMPLAUSIBLE_W).contains(&w).then_some(w)
}

/// Averaged [`Rates`] over some period, with the sample count kept.
///
/// The count is not decoration: a mean of two readings and a mean of two hundred
/// are rendered identically and mean very different things, and [`crate::trial`]
/// discards blocks on it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Accumulated {
    discharge_w: Vec<f64>,
    /// CPU package watts. Accumulated beside discharge rather than instead of
    /// it, because the two are different quantities and a block can have either,
    /// both or neither: discharge is absent on mains and the package counter is
    /// absent on a stock kernel, where it is `0400 root`.
    package_w: Vec<f64>,
    deepest_idle: Vec<f64>,
    system_wakeups: Vec<f64>,
    samples: u32,
}

impl Accumulated {
    pub fn push(&mut self, rates: &Rates) {
        if rates.is_empty() {
            return;
        }
        self.samples += 1;
        if let Some(w) = rates.discharge_w {
            self.discharge_w.push(w);
        }
        if let Some(w) = rates.package_w {
            self.package_w.push(w);
        }
        if let Some(f) = rates.deepest_idle {
            self.deepest_idle.push(f);
        }
        if let Some(w) = rates.system_wakeups_per_sec {
            self.system_wakeups.push(w);
        }
    }

    #[must_use]
    pub fn samples(&self) -> u32 {
        self.samples
    }

    #[must_use]
    pub fn mean_discharge_w(&self) -> Option<f64> {
        stats::mean(&self.discharge_w)
    }

    /// Mean CPU package watts over the block, where the counter was readable.
    ///
    /// The figure that lets a desktop run a trial at all: discharge is `None`
    /// the whole time a machine is plugged in, and a docked laptop is a desktop
    /// for this purpose.
    #[must_use]
    pub fn mean_package_w(&self) -> Option<f64> {
        stats::mean(&self.package_w)
    }

    #[must_use]
    pub fn mean_deepest_idle(&self) -> Option<f64> {
        stats::mean(&self.deepest_idle)
    }

    #[must_use]
    pub fn mean_system_wakeups(&self) -> Option<f64> {
        stats::mean(&self.system_wakeups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MINUTE: Duration = Duration::from_secs(60);

    fn idle(cpus: u32, deepest: u64, any: u64, entries: u64) -> Option<IdleCounters> {
        Some(IdleCounters { cpus, deepest_usec: deepest, idle_usec: any, entries })
    }

    #[test]
    fn a_rate_needs_two_readings() {
        let mut m = Meter::new();
        let r = Reading { discharge_w: Some(6.0), ..Default::default() };
        assert_eq!(m.observe(r, MonoTime::ZERO), None, "one counter is not a rate");
        assert!(m.observe(r, MonoTime::ZERO.plus(MINUTE)).is_some());
    }

    #[test]
    fn residency_is_a_fraction_of_all_the_cpus_time() {
        // 16 CPUs idle for the whole minute is 16 CPU-minutes of residency, and
        // that must read as 100% rather than 1600%. Dividing by wall time alone
        // was the first version of this and it produced exactly that.
        let mut m = Meter::new();
        let cpus = 16;
        let minute_usec = 60 * 1_000_000;
        m.observe(Reading { idle: idle(cpus, 0, 0, 0), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading {
                    idle: idle(
                        cpus,
                        u64::from(cpus) * minute_usec,
                        u64::from(cpus) * minute_usec,
                        0,
                    ),
                    ..Default::default()
                },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.deepest_idle, Some(1.0));
        assert_eq!(rates.idle, Some(1.0));
    }

    #[test]
    fn half_idle_reads_as_half() {
        let mut m = Meter::new();
        let half = 8 * 60 * 1_000_000; // 8 of 16 CPUs, for a minute
        m.observe(Reading { idle: idle(16, 0, 0, 0), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { idle: idle(16, half, half * 2, 6_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert!((rates.deepest_idle.unwrap() - 0.5).abs() < 1e-9);
        assert!((rates.idle.unwrap() - 1.0).abs() < 1e-9, "shallow idle counts towards `idle`");
        assert_eq!(rates.idle_entries_per_sec, Some(100.0));
    }

    #[test]
    fn a_cpu_going_offline_discards_the_interval_rather_than_inventing_a_residency() {
        // The sums are over a membership that changed, so the difference is not a
        // residency at all. §1's third non-negotiable: an ambiguous measurement
        // resolves to "no answer".
        let mut m = Meter::new();
        m.observe(Reading { idle: idle(16, 0, 0, 0), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { idle: idle(8, 400_000_000, 400_000_000, 0), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.deepest_idle, None);
        assert_eq!(rates.idle, None);
    }

    #[test]
    fn residency_cannot_exceed_the_interval_that_produced_it() {
        // The counters are read one file at a time while the CPUs keep running,
        // so a sweep straddling a long idle stretch can over-attribute slightly.
        // 101% residency in a report reads as a bug in the tool, which it is.
        let mut m = Meter::new();
        m.observe(Reading { idle: idle(4, 0, 0, 0), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { idle: idle(4, u64::MAX / 4, u64::MAX / 4, 0), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.deepest_idle, Some(1.0));
    }

    #[test]
    fn the_firmwares_own_watts_are_preferred_to_the_slope() {
        let mut m = Meter::new();
        let a =
            Reading { discharge_w: Some(6.0), energy_uwh: Some(40_000_000), ..Default::default() };
        // A slope that would say something very different, to prove which wins.
        let b =
            Reading { discharge_w: Some(6.0), energy_uwh: Some(30_000_000), ..Default::default() };
        m.observe(a, MonoTime::ZERO);
        assert_eq!(m.observe(b, MonoTime::ZERO.plus(MINUTE)).unwrap().discharge_w, Some(6.0));
    }

    #[test]
    fn the_energy_slope_covers_a_battery_that_reports_no_wattage() {
        // 10 mWh over a minute is 10 mWh x 3600 / 60s = 0.6 W.
        let mut m = Meter::new();
        m.observe(Reading { energy_uwh: Some(43_800_000), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { energy_uwh: Some(43_790_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert!((rates.discharge_w.unwrap() - 0.6).abs() < 1e-9, "{:?}", rates.discharge_w);
    }

    #[test]
    fn the_slope_declines_to_answer_over_too_short_an_interval() {
        // Measured: this battery's `energy_now` moves in 10 mWh steps, so over
        // five seconds the reading is either 0 W or 20 W and never the truth.
        let mut m = Meter::new();
        m.observe(Reading { energy_uwh: Some(43_800_000), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { energy_uwh: Some(43_790_000), ..Default::default() },
                MonoTime::ZERO.plus(Duration::from_secs(5)),
            )
            .unwrap();
        assert_eq!(rates.discharge_w, None);
    }

    /// The bug: the slope was differenced against the *previous* reading, so a
    /// tick faster than `MIN_SLOPE_INTERVAL_MS` never spanned enough of it and
    /// the wattage was `None` on every sample. The tick is 2s whenever a grace
    /// deadline is close and 5s whenever the dashboard is open, so on hardware
    /// with no `power_now` that was most of the time somebody was watching.
    #[test]
    fn the_slope_still_answers_when_the_tick_is_faster_than_its_interval() {
        // Draining 500 µWh every 2s: 0.25 mWh/s x 3600 = 0.9 W.
        let mut m = Meter::new();
        let mut watts = Vec::new();
        // A 2s tick, sixty seconds of it — fifteen ticks fit in one 30s slope
        // interval, and two whole intervals fit in the run.
        for i in 0..=30u64 {
            let at = MonoTime::ZERO.plus(Duration::from_secs(i * 2));
            let energy_uwh = Some(43_800_000 - i * 500);
            if let Some(r) = m.observe(Reading { energy_uwh, ..Default::default() }, at)
                && let Some(w) = r.discharge_w
            {
                watts.push(w);
            }
        }
        assert_eq!(watts.len(), 2, "one answer per elapsed slope interval: {watts:?}");
        for w in watts {
            assert!((w - 0.9).abs() < 1e-9, "{w}");
        }
    }

    #[test]
    fn package_energy_comes_out_as_watts() {
        // 30 J over a minute is 0.5 W. Microjoules, so unlike the battery slope
        // there is no interval too short to say anything over — and no minimum
        // interval gating it.
        let mut m = Meter::new();
        m.observe(Reading { package_uj: Some(1_000_000), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { package_uj: Some(31_000_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.package_w, Some(0.5));

        // 15 J over two seconds is 7.5 W, from the same counter, with no
        // complaint about the interval.
        let short = m
            .observe(
                Reading { package_uj: Some(46_000_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE).plus(Duration::from_secs(2)),
            )
            .unwrap();
        assert_eq!(short.package_w, Some(7.5));
    }

    #[test]
    fn a_machine_whose_rapl_is_closed_reports_no_package_watts() {
        let mut m = Meter::new();
        m.observe(Reading { ctxt_switches: Some(0), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { ctxt_switches: Some(1_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.package_w, None, "absent, not zero");
        // And a reading that carried only package watts must not be admitted to a
        // trial block as a sample: the verdict is discharge and deep idle, so it
        // would count towards neither mean (`Rates::is_empty`).
        assert!(Rates { package_w: Some(3.1), ..Default::default() }.is_empty());
    }

    #[test]
    fn charging_is_not_a_negative_discharge() {
        let mut m = Meter::new();
        m.observe(Reading { energy_uwh: Some(30_000_000), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { energy_uwh: Some(31_000_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.discharge_w, None, "energy going in is not energy coming out");
    }

    #[test]
    fn system_wakeups_come_out_as_a_rate() {
        let mut m = Meter::new();
        m.observe(Reading { ctxt_switches: Some(1_000_000), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { ctxt_switches: Some(1_060_000), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.system_wakeups_per_sec, Some(1_000.0));
    }

    #[test]
    fn a_counter_that_went_backwards_reads_as_zero_not_as_a_huge_negative() {
        // `/proc/stat`'s `ctxt` does not reset, but a reboot between two readings
        // of a persisted figure would, and the sampler learned this lesson the
        // hard way already.
        let mut m = Meter::new();
        m.observe(Reading { ctxt_switches: Some(1_000_000), ..Default::default() }, MonoTime::ZERO);
        let rates = m
            .observe(
                Reading { ctxt_switches: Some(5), ..Default::default() },
                MonoTime::ZERO.plus(MINUTE),
            )
            .unwrap();
        assert_eq!(rates.system_wakeups_per_sec, Some(0.0));
    }

    #[test]
    fn a_reset_starts_the_interval_again() {
        // Across a suspend the counters advance and the wall clock does not, so
        // the rate over the gap is fiction.
        let mut m = Meter::new();
        let r = Reading { ctxt_switches: Some(10), ..Default::default() };
        m.observe(r, MonoTime::ZERO);
        m.reset();
        assert_eq!(m.observe(r, MonoTime::ZERO.plus(MINUTE)), None);
    }

    #[test]
    fn accumulating_ignores_readings_that_measured_nothing() {
        // A machine with no battery and no cpuidle produces empty rates on every
        // tick, and counting those as samples would let a block pass the
        // "enough data" check with no data in it.
        let mut acc = Accumulated::default();
        acc.push(&Rates::default());
        assert_eq!(acc.samples(), 0);
        assert_eq!(acc.mean_discharge_w(), None);

        acc.push(&Rates { discharge_w: Some(6.0), ..Default::default() });
        acc.push(&Rates { discharge_w: Some(8.0), ..Default::default() });
        assert_eq!(acc.samples(), 2);
        assert_eq!(acc.mean_discharge_w(), Some(7.0));
        assert_eq!(acc.mean_deepest_idle(), None, "and each field averages only what it saw");
    }

    // ---- the breakdown, the GPU and the denominator -----------------------

    fn at(uj: u64, secs: u64) -> (Reading, MonoTime) {
        (
            Reading { package_uj: Some(uj), ..Default::default() },
            MonoTime::ZERO.plus(Duration::from_secs(secs)),
        )
    }

    #[test]
    fn a_counter_that_reset_is_not_a_thirty_kilowatt_cpu() {
        // A RAPL counter that *resets* rather than wraps — a driver reload, a
        // resume — is indistinguishable from a wrap at the moment it happens, and
        // the platform layer corrects it as one: up to a whole 65.5 kJ credited
        // to a single interval. Over the 2 s sampling floor that is 32 kW, which
        // is not a reading and must not be handed to `attribution` to divide
        // between apps.
        let mut m = Meter::new();
        let (a, t0) = at(0, 0);
        m.observe(a, t0);
        let (b, t1) = at(65_532_610_987, 2);
        assert_eq!(m.observe(b, t1).unwrap().package_w, None, "32 kW is not a CPU");
    }

    #[test]
    fn an_ordinary_wattage_survives_the_ceiling_untouched() {
        // The ceiling is here to catch a number three orders of magnitude out,
        // not to police the last few percent — the same judgement
        // `sampler::cpu_ceiling_pct` documents.
        let mut m = Meter::new();
        let (a, t0) = at(0, 0);
        m.observe(a, t0);
        let (b, t1) = at(48_000_000, 2);
        assert_eq!(m.observe(b, t1).unwrap().package_w, Some(24.0));
    }

    #[test]
    fn a_gpu_sentinel_is_not_a_four_kilowatt_laptop() {
        // `0xFFFFFFFF` µW is hwmon's way of saying nothing, and it arrives
        // looking exactly like data — the same shape of bug as the fan that
        // reported `0xFFFF` and taught this project's thresholds a span no fan
        // can reach.
        let mut m = Meter::new();
        m.observe(Reading::default(), MonoTime::ZERO);
        let absurd = Reading { gpu_uw: Some(u64::from(u32::MAX)), ..Default::default() };
        let rates = m.observe(absurd, MonoTime::ZERO.plus(MINUTE)).unwrap();
        assert_eq!(rates.gpu_w, None, "4,295 W is a marker, not a GPU");
    }

    #[test]
    fn the_gpu_is_read_straight_because_the_driver_already_averaged_it() {
        // `power1_average` is a wattage, not a counter: there is no interval to
        // difference and none of the wrap machinery applies.
        let mut m = Meter::new();
        m.observe(Reading::default(), MonoTime::ZERO);
        let r = Reading { gpu_uw: Some(13_049_000), ..Default::default() };
        assert_eq!(m.observe(r, MonoTime::ZERO.plus(MINUTE)).unwrap().gpu_w, Some(13.049));
    }

    #[test]
    fn the_parts_go_inside_the_package_and_the_remainder_is_named() {
        // Measured on the machine this was written against: 2.21 W of `core`
        // inside a 24.07 W package, so the part nobody itemises is the larger
        // half by an order of magnitude. Left as the gap between two numbers it
        // reads as the tool losing 22 W; named, it is the interconnect, the
        // memory controller and the integrated GPU.
        let parts = PackageBreakdown { core_w: Some(2.21), ..Default::default() };
        assert_eq!(parts.parts().count(), 1);
        let rest = parts.rest_w(24.07).unwrap();
        assert!((rest - 21.86).abs() < 1e-9, "{rest}");
    }

    #[test]
    fn parts_that_exceed_their_package_produce_no_remainder_rather_than_a_negative_one() {
        // The domains are read one file at a time while the CPU keeps running, so
        // a sweep can straddle a busy moment and hand a child slightly more than
        // its parent got. A negative remainder on screen would be nonsense, and
        // would be read as the package under-reporting rather than as the skew it
        // is. The interval says nothing instead, exactly as `Meter::observe`
        // discards one a CPU was hotplugged across.
        let parts = PackageBreakdown { core_w: Some(3.0), ..Default::default() };
        assert_eq!(parts.rest_w(2.5), None);
    }

    #[test]
    fn a_package_with_no_readable_parts_has_no_remainder_to_speak_of() {
        // Nothing to subtract is not the same as nothing left over: "0.00 W not
        // itemised" beside a 24 W package would claim a breakdown that does not
        // exist.
        assert_eq!(PackageBreakdown::default().rest_w(24.07), None);
        assert!(PackageBreakdown::default().is_empty());
    }

    #[test]
    fn the_parts_come_out_in_a_fixed_order_so_the_rows_do_not_move() {
        // A breakdown whose rows reorder between two-second polls is unreadable:
        // the eye is looking for a value changing, not a row moving.
        let parts = PackageBreakdown { core_w: Some(9.0), uncore_w: Some(1.0), dram_w: Some(5.0) };
        let names: Vec<&str> = parts.parts().map(|(n, _)| n).collect();
        assert_eq!(names, ["core", "uncore", "dram"], "declaration order, not by size");
    }

    #[test]
    fn cores_busy_is_a_ratio_of_ticks_so_the_clock_tick_never_enters_it() {
        // Half the ticks busy on 16 cores is 8 cores busy, whatever `USER_HZ` is
        // and however long the interval was believed to be.
        let mut m = Meter::new();
        let before = Reading {
            cpu: Some(CpuTime { busy_ticks: 0, total_ticks: 0, cpus: 16 }),
            ..Default::default()
        };
        let after = Reading {
            cpu: Some(CpuTime { busy_ticks: 500, total_ticks: 1_000, cpus: 16 }),
            ..Default::default()
        };
        m.observe(before, MonoTime::ZERO);
        let rates = m.observe(after, MonoTime::ZERO.plus(MINUTE)).unwrap();
        assert_eq!(rates.machine_cores_busy, Some(8.0));
    }

    #[test]
    fn a_torn_stat_read_is_discarded_rather_than_reported_above_the_core_count() {
        // `busy` is a subset of the same sum `total` is, so it cannot honestly
        // exceed it. If it does, one of the two reads was torn — and the result
        // would be a machine reported as using more cores than it has, which
        // `attribution` would then divide by.
        let mut m = Meter::new();
        let before = Reading {
            cpu: Some(CpuTime { busy_ticks: 0, total_ticks: 0, cpus: 16 }),
            ..Default::default()
        };
        let after = Reading {
            cpu: Some(CpuTime { busy_ticks: 2_000, total_ticks: 1_000, cpus: 16 }),
            ..Default::default()
        };
        m.observe(before, MonoTime::ZERO);
        assert_eq!(m.observe(after, MonoTime::ZERO.plus(MINUTE)).unwrap().machine_cores_busy, None);
    }

    #[test]
    fn a_kernel_that_will_not_show_its_cpu_line_yields_no_denominator() {
        // Absent, not zero. Zero cores busy on a running machine would hand the
        // first app that sampled the entire measured package.
        let mut m = Meter::new();
        m.observe(Reading::default(), MonoTime::ZERO);
        let rates = m.observe(Reading::default(), MonoTime::ZERO.plus(MINUTE)).unwrap();
        assert_eq!(rates.machine_cores_busy, None);
    }
}
