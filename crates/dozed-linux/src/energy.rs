//! Reading the machine's own power counters, unprivileged.
//!
//! `dozed_core::energy` does the arithmetic; this finds the files and parses
//! them. Everything load-bearing here is world-readable on a stock kernel — no
//! `root`, no `CAP_SYS_ADMIN`, no `perf` — which is the whole reason these
//! counters were chosen over the more obvious ones.
//!
//! ## RAPL, where somebody has opened it
//!
//! RAPL (`/sys/class/powercap`) reports package energy directly and is the better
//! instrument: it is what the CPU itself spends, and unlike the battery it is
//! measurable on mains. It has also been `0400 root` since CVE-2020-8694 — a side
//! channel that recovered AES keys from the energy counter — so a daemon that
//! *needed* it would be a daemon nobody installs.
//!
//! So it is read when it happens to be readable and absent otherwise, which is
//! the same contract every other counter here has. Nothing is required of the
//! user, nothing is asked for, and no diagnostic nags about it; opening the
//! counter is a deliberate root-level decision documented in the README, with the
//! CVE named so it can be made knowingly. Readability is proved by reading rather
//! than by inspecting the mode — see [`find_rapl`].
//!
//! ## Paths are resolved once
//!
//! `sources::SystemProxies` learned this the expensive way: building the D-Bus
//! proxies inside the poll loop turned three property reads into fifteen bus
//! calls every five seconds, and the setup *was* the cost. The same shape applies
//! here — globbing `/sys/devices/system/cpu/cpu*/cpuidle/state*` on every tick
//! would cost more than reading the counters it finds. So discovery happens in
//! [`PowerMeter::at`] and the tick only opens files whose paths are already
//! known.
//!
//! ## What this costs, measured
//!
//! On the machine this was written against — 16 CPUs, 4 `cpuidle` states, so 128
//! counter files plus `/proc/stat` and three battery files:
//!
//! | | |
//! |---|---|
//! | Files per sweep | 132, plus one per readable RAPL domain and two per GPU |
//! | Per sweep | **0.32 ms** (debug build, so the release figure is lower) |
//! | At the 2 s sampling floor | 0.016% of one core |
//!
//! RAPL costs nothing on the machines that cannot read it — discovery finds no
//! domains and the tick opens no files — and one small file per domain on those
//! that can, which is below the resolution of the figure above: 0.32 ms per sweep
//! measured both before and after it was added.
//!
//! The same held when the package sub-domains and the GPU sensor were added, and
//! it was checked rather than assumed. Three runs each, on a machine busy
//! building at the time, so the absolute figures are well above the 0.32 ms
//! measured on a quiet one — the comparison is what the numbers are for:
//!
//! | | |
//! |---|---|
//! | Without the sub-domains and the GPU | 1.03, 0.67, 1.11 ms |
//! | With them | 1.05, 0.64, 0.91 ms |
//!
//! Two more files on this machine — `intel-rapl:0:0/energy_uj` and one hwmon —
//! and the difference is entirely inside the noise the build was making. The
//! machine-wide CPU denominator (`parse_cpu_time`) is genuinely free: `/proc/stat`
//! was already being read in full for `ctxt`, and this is a second scan of a
//! string that is already in memory.
//!
//! Against `sampler`'s 33 ms per pass over 2,725 threads, this is free. It is
//! also read on the same tick as everything else, so it adds no wakeups at all —
//! which is the constraint that actually binds (`DESIGN.md` §13).
//!
//! ## Hardware that does not cooperate
//!
//! Two fallbacks, both needed on real laptops:
//!
//! * Batteries report either energy (`energy_now` in µWh, `power_now` in µW) or
//!   charge (`charge_now` in µAh, `current_now` in µA). Both are handled; the
//!   charge form is converted through `voltage_now`.
//! * `power_now` reads **0** on mains on this hardware, not a small number, and
//!   the status is `Not charging` rather than either `Charging` or `Discharging`.
//!   So discharge is reported only while the status actually says `Discharging` —
//!   a zero passed upwards would read as "this machine uses no power".

use std::fs;
use std::path::{Path, PathBuf};

use dozed_core::energy::{Components, CpuTime, IdleCounters, Reading};

/// Where the counters live, resolved once.
#[derive(Debug, Clone)]
pub struct PowerMeter {
    battery: Option<PathBuf>,
    /// Per CPU: every idle state, and which of them is the deepest.
    cpus: Vec<CpuStates>,
    proc_stat: PathBuf,
    /// The RAPL package domains that turned out to be readable, and whether one
    /// was found that we are not allowed to read.
    rapl: Rapl,
    /// Package energy accumulated across wraps, in µJ.
    ///
    /// Accumulated here rather than differenced by the caller because a wrap can
    /// only be recognised per domain, against that domain's own
    /// `max_energy_range_uj`, and a sum over domains has already thrown that away.
    /// The same shape as `sampler::Sampler`, for the same reason.
    package_uj: u64,
    /// The same accumulators for the sub-domains inside the package. One each,
    /// because a two-socket machine has two `core` domains that wrap
    /// independently and must be corrected before they are summed.
    core_uj: u64,
    uncore_uj: u64,
    dram_uj: u64,
    /// GPU wattage sensors, and what the driver calls them.
    gpu: Gpu,
}

/// What discovery found under `/sys/class/hwmon` for the GPU.
///
/// ## This may be the package again, and there is no way to be sure it is not
///
/// Measured on the machine this was written against — an AMD Krackan APU with a
/// Radeon 860M on the same die — over a 10 s interval:
///
/// | | |
/// |---|---|
/// | RAPL `package-0` | **24.07 W** |
/// | `amdgpu` `power1_average`, labelled `PPT` | **24.00 W** |
///
/// They are the same silicon. `PPT` is Package Power Tracking, and on an APU the
/// package it tracks is the one RAPL is already reporting. Adding the two would
/// have told the owner of a 24 W laptop that it was drawing 48.
///
/// Nothing the kernel exposes settles it. `mem_info_vram_vendor`, which only a
/// card with its own memory has, is absent here — but so it would be on some
/// discrete cards, and `mem_info_vram_total` reads 4 GiB of *system* RAM carved
/// out for the iGPU, which looks exactly like a dGPU's VRAM. `power1_label` says
/// `PPT` on discrete Radeons too. There is no fact to read, only a guess to make.
///
/// So no guess is made. This figure is reported under its own label and is
/// **never added to anything** — not to the package, not to discharge, not into
/// a system total, because there is no system total anywhere in this codebase to
/// add it to. §10's rule is that anything the daemon cannot do it says so, and
/// what it cannot do here is tell you whether these two numbers are one number.
/// `dozectl status` says that in words where both are present.
///
/// Several sensors *are* summed, which is a different question and a safe one:
/// two `amdgpu` devices are two chips, and their draws genuinely add. It is only
/// the package boundary that cannot be crossed honestly.
#[derive(Debug, Clone, Default)]
struct Gpu {
    /// One per `amdgpu` hwmon that proved readable.
    sensors: Vec<PathBuf>,
    /// `power1_label` from the first sensor — `PPT` on every AMD part seen.
    /// Rendered as-is rather than translated, so the reader can go and find the
    /// same word in the same file.
    label: Option<String>,
    /// `device/gpu_busy_percent`: how much of the chip's time was spent doing
    /// work, whoever asked for it.
    ///
    /// The denominator the per-client engine times in [`crate::gpu`] are read
    /// against — and, like `/proc/stat` for the CPU, it covers work no client
    /// owns. Measured over three windows: clients accounted for 7.5%, 10.9% and
    /// 10.8% of an engine against 10.8%, 15.1% and 17.2% of chip busy.
    busy: Option<PathBuf>,
}

/// What discovery found under `/sys/class/powercap`.
#[derive(Debug, Clone, Default)]
struct Rapl {
    /// The package domains that turned out to be readable.
    package: Vec<RaplDomain>,
    /// The readable sub-domains, grouped by kind and flattened across packages.
    ///
    /// Flattened because the question anyone asks is "how much of this package is
    /// the cores", not "how much of socket 1 is". A two-socket machine's two
    /// `core` domains are summed into one figure exactly as its two packages are,
    /// and each is wrap-corrected against its own range first — which is the whole
    /// reason `package_uj` is accumulated down here rather than up in `core`.
    core: Vec<RaplDomain>,
    uncore: Vec<RaplDomain>,
    dram: Vec<RaplDomain>,
    /// A package domain exists whose `energy_uj` we are **not allowed** to read.
    ///
    /// Kept apart from having found nothing at all, because only one of the two is
    /// worth telling anybody about: a machine with no RAPL has nothing its owner
    /// could grant, while a machine with a `0400` counter has a figure one
    /// deliberate decision away. Reported rather than left silent because this
    /// project has already walked into that silence once — a correct udev rule, a
    /// correct build, no number, and no way to tell which of the three was wrong.
    denied: bool,
}

/// One RAPL energy domain, and the last thing read from it.
#[derive(Debug, Clone)]
struct RaplDomain {
    energy_uj: PathBuf,
    /// The value this counter wraps at, from `max_energy_range_uj`. 65.5 kJ on
    /// the machine this was written against, which at 10 W is a wrap every 1.8
    /// hours — routine over a session, not an edge case.
    range_uj: u64,
    last_uj: Option<u64>,
}

#[derive(Debug, Clone)]
struct CpuStates {
    /// All states, so `idle_usec` and `entries` cover shallow idle too.
    all: Vec<PathBuf>,
    /// The highest-numbered state.
    ///
    /// The kernel's cpuidle framework orders states by increasing target
    /// residency and exit latency, so "highest-numbered" is "deepest" without
    /// this file needing to know whether it is looking at C6, C3 or something
    /// AMD calls neither.
    deepest: PathBuf,
}

impl PowerMeter {
    /// Discover the counters under the real filesystem root.
    #[must_use]
    pub fn new() -> Self {
        Self::at(Path::new("/"))
    }

    /// Discover the counters under `root`, which tests point at a fixture tree.
    ///
    /// Taking a root rather than hard-coding `/sys` is what makes this file
    /// testable at all: a battery that reports charge instead of energy, a CPU
    /// hotplug, a kernel with no `cpuidle` — none of those can be arranged on the
    /// machine running the tests, and all of them are a directory of small text
    /// files.
    #[must_use]
    pub fn at(root: &Path) -> Self {
        Self {
            battery: find_battery(root),
            cpus: find_cpuidle(root),
            proc_stat: root.join("proc/stat"),
            rapl: find_rapl(root),
            package_uj: 0,
            core_uj: 0,
            uncore_uj: 0,
            dram_uj: 0,
            gpu: find_gpu(root),
        }
    }

    /// Whether anything at all was found, for `dozectl status` to disclaim.
    ///
    /// A source that has never reported is a measurement that is drawn but not
    /// connected — the exact failure the exemption sources were audited for.
    #[must_use]
    pub fn available(&self) -> bool {
        self.battery.is_some()
            || !self.cpus.is_empty()
            || !self.rapl.package.is_empty()
            || !self.gpu.sensors.is_empty()
    }

    /// Whether the CPU package counter is there and closed to us.
    ///
    /// Distinct from "no package figure": absent hardware is not worth a line of
    /// anybody's terminal, and a permission is (`Rapl::denied`).
    #[must_use]
    pub fn package_denied(&self) -> bool {
        self.rapl.denied
    }

    #[must_use]
    pub fn has_battery(&self) -> bool {
        self.battery.is_some()
    }

    /// What the GPU driver calls the figure it reports — `PPT` on AMD.
    ///
    /// Carried up so the reader sees the sensor's own word rather than ours.
    /// "GPU: 24 W" invites the assumption that it is separate from the CPU
    /// package; "amdgpu PPT: 24 W" is what the file actually says, and on the
    /// machine this was written against it says it about the same silicon (see
    /// [`Gpu`]).
    #[must_use]
    pub fn gpu_label(&self) -> Option<&str> {
        self.gpu.label.as_deref()
    }

    #[must_use]
    pub fn cpus(&self) -> usize {
        self.cpus.len()
    }

    /// Read every counter once.
    ///
    /// Never fails: each field is independently optional, and a missing one means
    /// the report has less to say rather than that anything is wrong.
    ///
    /// `&mut` for the RAPL accumulator alone. Every other counter here is read and
    /// forgotten, but a counter that wraps cannot be differenced by anybody who
    /// has not been watching it.
    #[must_use]
    pub fn read(&mut self) -> Reading {
        let (discharge_w, energy_uwh) = self.battery.as_deref().map_or((None, None), read_battery);
        let (package_uj, components) = self.read_rapl();
        // One read of `/proc/stat` for both figures. Two reads would not only
        // cost twice as much, they could disagree: the wakeup count and the busy
        // fraction are compared against each other in `dozectl apps`, and two
        // reads a microsecond apart would make that comparison unfalsifiable in
        // the small.
        let (ctxt_switches, cpu) = self.proc_stat();
        Reading {
            discharge_w,
            energy_uwh,
            package_uj,
            components,
            gpu_uw: self.read_gpu(),
            // A percentage that reads above 100 is not a busier chip, it is a
            // counter that is not what it claims — the same instinct as
            // `IMPLAUSIBLE_RPM`, applied to the one figure here with an
            // arithmetic ceiling of its own.
            gpu_busy_pct: self
                .gpu
                .busy
                .as_deref()
                .and_then(read_u64)
                .filter(|v| *v <= 100)
                .map(|v| u32::try_from(v).unwrap_or(100)),
            cpu,
            idle: self.read_cpuidle(),
            ctxt_switches,
        }
    }

    /// Package energy since this meter started, in µJ, summed over every readable
    /// package domain.
    ///
    /// Monotonic by construction: each domain's increment is taken against its own
    /// previous reading and corrected for the wrap, so the caller can difference
    /// this like any other cumulative counter. A domain that has become unreadable
    /// since discovery — a permission put back, a CPU package offlined —
    /// contributes nothing rather than a fall.
    fn read_rapl(&mut self) -> (Option<u64>, Components) {
        let package = accumulate(&mut self.rapl.package, &mut self.package_uj);
        // Read whether or not the package was: a machine could in principle open
        // the children and not the parent, and a `core` figure with no package to
        // sit inside is still a true statement about the cores. The renderers
        // handle the missing parent; inventing one here would not.
        let components = Components {
            core_uj: accumulate(&mut self.rapl.core, &mut self.core_uj),
            uncore_uj: accumulate(&mut self.rapl.uncore, &mut self.uncore_uj),
            dram_uj: accumulate(&mut self.rapl.dram, &mut self.dram_uj),
        };
        (package, components)
    }

    /// The GPU sensors, summed, in µW.
    ///
    /// Summed because two `amdgpu` devices are two chips and their draws add —
    /// unlike the package boundary, which cannot be crossed (see [`Gpu`]). An
    /// instantaneous read with nothing to accumulate: `power1_average` is already
    /// an average the driver keeps, so there is no counter here and no wrap.
    fn read_gpu(&self) -> Option<u64> {
        let mut total: u64 = 0;
        let mut read_any = false;
        for sensor in &self.gpu.sensors {
            // A sensor that has become unreadable since discovery contributes
            // nothing rather than a zero, for the reason the whole file repeats:
            // "we cannot see it" and "it is drawing nothing" are different
            // statements and only one of them is ours to make.
            if let Some(uw) = read_u64(sensor) {
                read_any = true;
                total = total.saturating_add(uw);
            }
        }
        read_any.then_some(total)
    }

    fn read_cpuidle(&self) -> Option<IdleCounters> {
        if self.cpus.is_empty() {
            return None;
        }
        let mut counters = IdleCounters { cpus: 0, ..Default::default() };
        for cpu in &self.cpus {
            // A CPU that went offline between discovery and now contributes
            // nothing, and `cpus` therefore falls — which is exactly the signal
            // `dozed_core::energy::Meter` discards the interval on. Counting it
            // as present with zero counters would instead report a residency
            // quietly diluted by however many CPUs are asleep.
            let Some(deepest) = read_u64(&cpu.deepest.join("time")) else { continue };
            counters.cpus += 1;
            counters.deepest_usec += deepest;
            for state in &cpu.all {
                counters.idle_usec += read_u64(&state.join("time")).unwrap_or(0);
                counters.entries += read_u64(&state.join("usage")).unwrap_or(0);
            }
        }
        (counters.cpus > 0).then_some(counters)
    }

    /// `/proc/stat`, read once, parsed twice.
    ///
    /// `ctxt` is the machine-wide wakeup count §11.1 names as the denominator for
    /// the per-app figures; the `cpu` line is the denominator for the per-app
    /// *watts*. Both come out of one `read_to_string`, so the second figure costs
    /// a scan of a string already in memory and no syscall at all — which is what
    /// made per-app attribution affordable in the first place (`DESIGN.md` §13:
    /// the constraint that binds is wakeups, and this adds none).
    fn proc_stat(&self) -> (Option<u64>, Option<CpuTime>) {
        let Ok(text) = fs::read_to_string(&self.proc_stat) else { return (None, None) };
        (parse_ctxt(&text), parse_cpu_time(&text))
    }
}

impl Default for PowerMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// The first power supply of type `Battery`.
///
/// Type-checked rather than name-matched: this machine also exposes two
/// `ucsi-source-psy-USBC000:*` supplies for the USB-C ports, which have a
/// `current_now` and would happily be read as a battery discharging.
fn find_battery(root: &Path) -> Option<PathBuf> {
    let dir = root.join("sys/class/power_supply");
    let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| read_trimmed(&p.join("type")).as_deref() == Some("Battery"))
        .collect();
    // Sorted so a machine with two batteries picks the same one every run; a
    // meter that silently changed which battery it was reading would produce
    // exactly the kind of unexplainable step the trial is built to survive.
    candidates.sort();
    candidates.into_iter().next()
}

/// Every RAPL package domain we can actually read.
///
/// **Readability is proved by reading**, not by inspecting the mode. The mode is
/// not the only thing that can stop us — an LSM, a namespace, a read-only or
/// masked `/sys` — and a counter that cannot be read is not a counter we have.
/// One read per package at startup is also the cheapest possible probe.
///
/// **`package-*` domains only.** `intel-rapl:0:0` is `core`, a *subset* of its
/// parent package, so summing both would count the cores twice. `psys`, where
/// firmware offers it, measures the whole platform including the screen — a
/// better number for a battery tool, but a different one from what the label
/// says, and one this machine cannot exercise. Left out deliberately rather than
/// by oversight.
fn find_rapl(root: &Path) -> Rapl {
    let dir = root.join("sys/class/powercap");
    let Ok(entries) = fs::read_dir(dir) else { return Rapl::default() };
    let mut found: Vec<(PathBuf, RaplDomain)> = Vec::new();
    let mut parts: Vec<(PathBuf, &'static str, RaplDomain)> = Vec::new();
    let mut denied = false;
    for entry in entries.flatten() {
        let path = entry.path();
        // `intel-rapl` itself is the control type and has no `name`; the
        // per-domain directories are `intel-rapl:N` and `intel-rapl:N:M`. The
        // children appear at this level too, and are skipped here and found
        // below via their parent instead — which is what makes the parent/child
        // relationship a fact about the tree rather than a guess from the name.
        if !read_trimmed(&path.join("name")).is_some_and(|n| n.starts_with("package")) {
            continue;
        }
        match probe(&path) {
            Probe::Readable(domain) => found.push((path.clone(), domain)),
            // The ordinary case, and the only one worth telling anybody about:
            // 0400 on a stock kernel, and the permission is something they can
            // change if they decide to.
            Probe::Denied => denied = true,
            // Absent, or present and not a number — a firmware fault, which is
            // nothing to grant and would send somebody after the wrong thing if
            // it were reported as a permission.
            Probe::Absent => {}
        }
        // The sub-domains, found by reading the package's own directory rather
        // than by parsing `intel-rapl:0:0` for a colon. Containment in the tree
        // *is* the subset relation the whole breakdown depends on, so it is read
        // from the tree; a name-shaped guess would put a sibling inside a parent
        // on the first driver that numbers things differently.
        let Ok(children) = fs::read_dir(&path) else { continue };
        for child in children.flatten() {
            let child = child.path();
            let Some(kind) = read_trimmed(&child.join("name")).and_then(|n| match n.as_str() {
                "core" => Some("core"),
                "uncore" => Some("uncore"),
                "dram" => Some("dram"),
                // Anything else is a domain this code has no place to put. Left
                // out rather than lumped in: a breakdown is read as adding up,
                // and a domain filed under the wrong heading is worse than one
                // that is missing — `PackageBreakdown::rest_w` already accounts
                // for whatever is not itemised.
                _ => None,
            }) else {
                continue;
            };
            // A closed sub-domain does not set `denied`. That flag means exactly
            // "the package counter is there and shut", which is the one sentence
            // `dozectl status` acts on and the README's udev rule fixes; widening
            // it to the children would make it fire on machines whose package is
            // already open and where there is nothing left to grant.
            if let Probe::Readable(domain) = probe(&child) {
                parts.push((child, kind, domain));
            }
        }
    }
    // Sorted for the same reason `find_battery` sorts: a meter that changed which
    // domain it read first between runs would produce an unexplainable step.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    parts.sort_by(|a, b| a.0.cmp(&b.0));
    let of = |want: &str| -> Vec<RaplDomain> {
        parts.iter().filter(|(_, kind, _)| *kind == want).map(|(_, _, d)| d.clone()).collect()
    };
    Rapl {
        core: of("core"),
        uncore: of("uncore"),
        dram: of("dram"),
        package: found.into_iter().map(|(_, domain)| domain).collect(),
        denied,
    }
}

/// What happened when we tried to read one domain's counter.
enum Probe {
    Readable(RaplDomain),
    /// `0400 root`, and one deliberate decision away from a figure.
    Denied,
    /// Missing, or present and not a number.
    Absent,
}

/// Try one RAPL domain directory, **by reading it**.
///
/// The mode is not the only thing that can stop us — an LSM, a namespace, a
/// read-only or masked `/sys` — and a counter that cannot be read is not a
/// counter we have. One read per domain at startup is also the cheapest possible
/// probe, and it is the same probe for a package and for a child, which is why it
/// is a function rather than two copies that could drift apart.
fn probe(path: &Path) -> Probe {
    let energy_uj = path.join("energy_uj");
    match fs::read_to_string(&energy_uj) {
        Ok(text) if text.trim().parse::<u64>().is_ok() => {
            let range_uj = read_u64(&path.join("max_energy_range_uj")).unwrap_or(0);
            Probe::Readable(RaplDomain { energy_uj, range_uj, last_uj: None })
        }
        Ok(_) => Probe::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Probe::Denied,
        Err(_) => Probe::Absent,
    }
}

/// One family of RAPL domains, accumulated across wraps, in µJ.
///
/// Monotonic by construction: each domain's increment is taken against its own
/// previous reading and corrected for the wrap, so the caller can difference the
/// result like any other cumulative counter. A domain that has become unreadable
/// since discovery — a permission put back, a CPU package offlined — contributes
/// nothing rather than a fall.
///
/// `None` when the family is empty or nothing in it could be read this tick,
/// which is the distinction the whole module turns on: not measured is not zero.
fn accumulate(domains: &mut [RaplDomain], total: &mut u64) -> Option<u64> {
    if domains.is_empty() {
        return None;
    }
    let mut read_any = false;
    let mut increment: u64 = 0;
    for domain in domains.iter_mut() {
        let Some(current) = read_u64(&domain.energy_uj) else { continue };
        read_any = true;
        let Some(previous) = domain.last_uj.replace(current) else { continue };
        increment = increment.saturating_add(if current >= previous {
            current - previous
        } else if domain.range_uj > previous {
            // One wrap: up to the range, round to zero, then up to `current`.
            //
            // A counter that *reset* — a driver reload, a resume — is
            // indistinguishable from a wrap at this point and is corrected as
            // one, which credits the interval with up to a whole range. That is
            // caught downstream by `dozed_core::energy::IMPLAUSIBLE_W` rather
            // than here, because only the caller knows how long the interval was
            // and 65.5 kJ is absurd over two seconds and unremarkable over a day.
            (domain.range_uj - previous).saturating_add(current)
        } else {
            // A fall we cannot account for, because the range was unreadable
            // or is smaller than a reading taken against it. Discarding the
            // interval is the honest response; inventing its size is not.
            0
        });
    }
    *total = total.saturating_add(increment);
    read_any.then_some(*total)
}

/// Every `amdgpu` wattage sensor we can actually read.
///
/// `power1_average` is the driver's own running mean and the right figure for a
/// two-second poll; `power1_input` is the instantaneous one and is taken only
/// where the average is absent, because a spot reading of a GPU is mostly noise.
/// Measured together on this machine: 13.049 W average against 13.052 W input,
/// so the fallback costs nothing where both exist.
///
/// Readability is proved by reading, as everywhere else here. Unlike RAPL there
/// is no permission to grant — these are world-readable on a stock kernel — so
/// nothing distinguishes "denied" from "absent" and nothing needs to.
fn find_gpu(root: &Path) -> Gpu {
    let Ok(entries) = fs::read_dir(root.join("sys/class/hwmon")) else { return Gpu::default() };
    let mut found: Vec<(PathBuf, PathBuf, Option<String>)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if read_trimmed(&path.join("name")).as_deref() != Some("amdgpu") {
            continue;
        }
        let sensor = ["power1_average", "power1_input"]
            .into_iter()
            .map(|f| path.join(f))
            .find(|f| read_u64(f).is_some());
        if let Some(sensor) = sensor {
            let label = read_trimmed(&path.join("power1_label")).filter(|l| !l.is_empty());
            found.push((path, sensor, label));
        }
    }
    // Sorted, for the reason every other discovery here sorts: `hwmon10` and
    // `hwmon2` arrive from `read_dir` in whatever order the filesystem offers,
    // and a label that came from a different card between two runs would be
    // unexplainable.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Gpu {
        label: found.first().and_then(|(_, _, l)| l.clone()),
        // From the first device only, and not summed across two: it is a
        // percentage, and percentages of different chips do not add.
        busy: found
            .first()
            .map(|(dir, _, _)| dir.join("device/gpu_busy_percent"))
            .filter(|p| read_u64(p).is_some()),
        sensors: found.into_iter().map(|(_, sensor, _)| sensor).collect(),
    }
}

/// Every CPU's idle states, deepest identified.
fn find_cpuidle(root: &Path) -> Vec<CpuStates> {
    let dir = root.join("sys/devices/system/cpu");
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut cpus: Vec<(u32, CpuStates)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(index) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("cpu"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue; // cpufreq, cpuidle, hotplug, vulnerabilities, ...
        };
        let mut states: Vec<(u32, PathBuf)> = match fs::read_dir(path.join("cpuidle")) {
            Ok(dirs) => dirs
                .flatten()
                .filter_map(|s| {
                    let p = s.path();
                    let n = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_prefix("state"))
                        .and_then(|n| n.parse::<u32>().ok())?;
                    Some((n, p))
                })
                .collect(),
            Err(_) => continue,
        };
        // Numerically, not lexicographically: `state10` sorts before `state2` as
        // a string, which would nominate a mid-depth state as the deepest on any
        // machine with ten or more of them.
        states.sort_by_key(|(n, _)| *n);
        if let Some((_, deepest)) = states.last().cloned() {
            let all = states.into_iter().map(|(_, p)| p).collect();
            cpus.push((index, CpuStates { all, deepest }));
        }
    }
    cpus.sort_by_key(|(index, _)| *index);
    cpus.into_iter().map(|(_, states)| states).collect()
}

/// Discharge in watts and remaining energy in µWh, or `None` for each.
///
/// Both are `None` unless the battery says it is discharging. On mains this
/// hardware reports `power_now = 0` with status `Not charging`, and a zero
/// passed upwards would read as a machine that consumes nothing.
fn read_battery(dir: &Path) -> (Option<f64>, Option<u64>) {
    let status = read_trimmed(&dir.join("status")).unwrap_or_default();
    if status != "Discharging" {
        return (None, None);
    }
    let voltage_uv = read_u64(&dir.join("voltage_now"));

    // Energy-reporting batteries first, then charge-reporting ones via voltage.
    let power_uw = read_u64(&dir.join("power_now")).or_else(|| {
        let current_ua = read_u64(&dir.join("current_now"))?;
        Some(current_ua.saturating_mul(voltage_uv?) / 1_000_000)
    });
    let energy_uwh = read_u64(&dir.join("energy_now")).or_else(|| {
        let charge_uah = read_u64(&dir.join("charge_now"))?;
        Some(charge_uah.saturating_mul(voltage_uv?) / 1_000_000)
    });

    // A zero from a battery that claims to be discharging is a firmware that
    // does not implement the counter, not a machine drawing nothing.
    let discharge_w = power_uw.filter(|w| *w > 0).map(|w| w as f64 / 1e6);
    (discharge_w, energy_uwh)
}

/// The `ctxt` line of `/proc/stat`.
fn parse_ctxt(text: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix("ctxt "))?.trim().parse().ok()
}

/// The aggregate `cpu` line of `/proc/stat`, plus a count of the per-CPU ones.
///
/// ```text
/// cpu  173909950 47442864 33129789 2103658563 104351732 0 2368713 0 0 0
///      user      nice     system   idle       iowait    irq softirq steal guest guest_nice
/// ```
///
/// `idle` and `iowait` are excluded from the busy half — `iowait` is the CPU
/// executing nothing while a disk answers, and calling it busy would shrink every
/// app's attributed share on a machine that is merely waiting. `guest` and
/// `guest_nice` are excluded from **both** halves, because the kernel already
/// counts them inside `user` and `nice` and adding them would inflate the
/// denominator on exactly the machines that run virtual machines.
///
/// Unknown trailing fields are ignored rather than summed. A kernel that grows an
/// eleventh column would otherwise silently change what every app's share is a
/// share *of*, and a denominator that moves for a reason nobody can see is the
/// failure §11.1 is about.
fn parse_cpu_time(text: &str) -> Option<CpuTime> {
    // `cpu ` with the trailing space: `cpu0` must not match the aggregate line,
    // and on a single-CPU machine it is the only other candidate.
    let line = text.lines().find_map(|l| l.strip_prefix("cpu "))?;
    let f: Vec<u64> = line.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    // user, nice, system, idle — the four every kernel since 2.6 has had. Fewer
    // than that is not a line we recognise, and guessing at it would be worse
    // than having no denominator.
    if f.len() < 4 {
        return None;
    }
    let at = |i: usize| f.get(i).copied().unwrap_or(0);
    let busy = at(0) + at(1) + at(2) + at(5) + at(6) + at(7);
    Some(CpuTime {
        busy_ticks: busy,
        total_ticks: busy + at(3) + at(4),
        // Counted from the lines that are there rather than from
        // `available_parallelism`, so it matches the ticks it scales even inside
        // a container whose CPU allocation is not the machine's.
        cpus: text.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count()
            as u32,
    })
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory, in the shape the rest of this workspace uses.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("dozed-energy-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        /// A battery in the energy-reporting form, as on the machine this was
        /// written against.
        fn energy_battery(&self, status: &str, power_uw: u64, energy_uwh: u64) {
            let d = "sys/class/power_supply/BAT0";
            self.write(&format!("{d}/type"), "Battery\n");
            self.write(&format!("{d}/status"), &format!("{status}\n"));
            self.write(&format!("{d}/power_now"), &format!("{power_uw}\n"));
            self.write(&format!("{d}/energy_now"), &format!("{energy_uwh}\n"));
            self.write(&format!("{d}/voltage_now"), "17086000\n");
        }

        fn cpuidle(&self, cpus: u32, states: &[(u64, u64)]) {
            for cpu in 0..cpus {
                for (i, (time, usage)) in states.iter().enumerate() {
                    let d = format!("sys/devices/system/cpu/cpu{cpu}/cpuidle/state{i}");
                    self.write(&format!("{d}/time"), &format!("{time}\n"));
                    self.write(&format!("{d}/usage"), &format!("{usage}\n"));
                }
            }
        }

        fn meter(&self) -> PowerMeter {
            PowerMeter::at(&self.0)
        }

        /// One RAPL domain: `intel-rapl:N` for a package, `intel-rapl:N:M` for a
        /// subdomain, in the layout `/sys/class/powercap` actually has.
        fn rapl(&self, dir: &str, name: &str, energy_uj: u64, max: u64) {
            let d = format!("sys/class/powercap/{dir}");
            self.write(&format!("{d}/name"), &format!("{name}\n"));
            self.write(&format!("{d}/energy_uj"), &format!("{energy_uj}\n"));
            self.write(&format!("{d}/max_energy_range_uj"), &format!("{max}\n"));
        }

        /// An `amdgpu` hwmon, in the layout `/sys/class/hwmon` actually has.
        fn amdgpu(&self, hwmon: &str, average_uw: Option<u64>, input_uw: Option<u64>) {
            let d = format!("sys/class/hwmon/{hwmon}");
            self.write(&format!("{d}/name"), "amdgpu\n");
            self.write(&format!("{d}/power1_label"), "PPT\n");
            if let Some(uw) = average_uw {
                self.write(&format!("{d}/power1_average"), &format!("{uw}\n"));
            }
            if let Some(uw) = input_uw {
                self.write(&format!("{d}/power1_input"), &format!("{uw}\n"));
            }
        }

        fn set_energy_uj(&self, dir: &str, energy_uj: u64) {
            self.write(&format!("sys/class/powercap/{dir}/energy_uj"), &format!("{energy_uj}\n"));
        }

        /// Take away our own read permission, the way the kernel ships it.
        fn close_energy_uj(&self, dir: &str) {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(format!("sys/class/powercap/{dir}/energy_uj"));
            fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_discharging_battery_reports_watts_and_energy() {
        let f = Fixture::new("discharging");
        f.energy_battery("Discharging", 6_400_000, 43_790_000);
        let r = f.meter().read();
        assert_eq!(r.discharge_w, Some(6.4));
        assert_eq!(r.energy_uwh, Some(43_790_000));
    }

    #[test]
    fn a_battery_on_mains_reports_nothing_rather_than_zero_watts() {
        // Measured on this hardware: plugged in, `power_now` is 0 and the status
        // is `Not charging` — neither `Charging` nor `Discharging`. Passing the
        // zero up would read as a machine that consumes no power, and a trial
        // block would record it as the most efficient block of the run.
        let f = Fixture::new("mains");
        f.energy_battery("Not charging", 0, 43_790_000);
        let r = f.meter().read();
        assert_eq!(r.discharge_w, None);
        assert_eq!(r.energy_uwh, None, "and its energy is not a discharge either");

        f.energy_battery("Charging", 0, 43_800_000);
        assert_eq!(f.meter().read().discharge_w, None);
    }

    #[test]
    fn a_battery_that_claims_to_discharge_at_zero_watts_is_not_believed() {
        // A firmware that exposes `power_now` without implementing it. The
        // fallback slope can still answer, so refusing the zero is not a loss.
        let f = Fixture::new("zero-watts");
        f.energy_battery("Discharging", 0, 43_790_000);
        let r = f.meter().read();
        assert_eq!(r.discharge_w, None);
        assert_eq!(r.energy_uwh, Some(43_790_000), "the energy counter still stands");
    }

    #[test]
    fn a_charge_reporting_battery_is_converted_through_its_voltage() {
        // Half of laptops report µAh and µA rather than µWh and µW. Without this
        // the whole measured report is silently unavailable on those machines.
        let f = Fixture::new("charge-form");
        let d = "sys/class/power_supply/BAT0";
        f.write(&format!("{d}/type"), "Battery\n");
        f.write(&format!("{d}/status"), "Discharging\n");
        f.write(&format!("{d}/current_now"), "400000\n"); // 0.4 A
        f.write(&format!("{d}/charge_now"), "3000000\n"); // 3 Ah
        f.write(&format!("{d}/voltage_now"), "15000000\n"); // 15 V

        let r = f.meter().read();
        // 0.4 A x 15 V = 6 W.
        assert_eq!(r.discharge_w, Some(6.0));
        // 3 Ah x 15 V = 45 Wh.
        assert_eq!(r.energy_uwh, Some(45_000_000));
    }

    /// 65.5 kJ, the wrap point on the machine this was written against.
    const RANGE: u64 = 65_532_610_987;

    #[test]
    fn rapl_is_read_where_the_permission_has_been_opened() {
        let f = Fixture::new("rapl-open");
        f.rapl("intel-rapl:0", "package-0", 1_000_000, RANGE);
        // The first read has nothing to difference against, so it contributes
        // nothing — the accumulated total starts where the counter is found.
        let mut m = f.meter();
        assert_eq!(m.read().package_uj, Some(0));
        f.set_energy_uj("intel-rapl:0", 3_000_000);
        assert_eq!(m.read().package_uj, Some(2_000_000), "2 J spent between reads");
        f.set_energy_uj("intel-rapl:0", 3_500_000);
        assert_eq!(m.read().package_uj, Some(2_500_000), "and it accumulates");
    }

    #[test]
    fn rapl_the_kernel_keeps_to_itself_is_absent_rather_than_zero() {
        // The ordinary case: `energy_uj` has been 0400 root since CVE-2020-8694,
        // so most machines have no package figure at all. Zero watts would read as
        // a CPU that costs nothing, which is the same mistake refused for a
        // battery on mains.
        let f = Fixture::new("rapl-closed");
        f.rapl("intel-rapl:0", "package-0", 1_000_000, RANGE);
        f.close_energy_uj("intel-rapl:0");
        f.energy_battery("Discharging", 6_400_000, 43_790_000);

        let mut m = f.meter();
        assert_eq!(m.read().package_uj, None, "a counter we cannot read is not one we have");
        assert!(m.available(), "and the counters that are readable still are");
        assert!(m.package_denied(), "but this machine could report one, and is owed the reason");
    }

    /// The three cases have to stay three, because two of them look identical from
    /// the outside and only one is worth a line of anybody's terminal.
    #[test]
    fn a_closed_counter_is_told_apart_from_no_counter_at_all() {
        // Hardware that has none: an AMD box without powercap, a VM. Nothing its
        // owner could grant, so nothing to say.
        let none = Fixture::new("rapl-absent");
        none.cpuidle(2, &[(1_000, 10)]);
        assert!(!none.meter().package_denied(), "absent hardware is not a permission problem");

        // Readable: the figure speaks for itself and needs no explanation.
        let open = Fixture::new("rapl-says-nothing-when-open");
        open.rapl("intel-rapl:0", "package-0", 1_000_000, RANGE);
        assert!(!open.meter().package_denied());

        // Present and shut, which is the one case that is actionable.
        let shut = Fixture::new("rapl-shut");
        shut.rapl("intel-rapl:0", "package-0", 1_000_000, RANGE);
        shut.close_energy_uj("intel-rapl:0");
        assert!(shut.meter().package_denied());
    }

    #[test]
    fn a_counter_that_is_readable_but_not_a_number_is_nobodys_fault() {
        // Present, permitted, and gibberish — a kernel or firmware fault, not
        // something a permission would fix. Reporting it as "needs a permission"
        // would send somebody after the wrong thing.
        let f = Fixture::new("rapl-garbage");
        let d = "sys/class/powercap/intel-rapl:0";
        f.write(&format!("{d}/name"), "package-0\n");
        f.write(&format!("{d}/energy_uj"), "not a number\n");

        let mut m = f.meter();
        assert_eq!(m.read().package_uj, None);
        assert!(!m.package_denied(), "unparseable is not denied");
    }

    #[test]
    fn only_whole_packages_are_summed() {
        // `intel-rapl:0:0` is `core`, a subset of its parent package. Summing both
        // would count the cores twice and report a CPU drawing half again what it
        // draws. Both files exist and both are readable on any machine where RAPL
        // has been opened up, so this is not hypothetical.
        let f = Fixture::new("rapl-subdomain");
        f.rapl("intel-rapl:0", "package-0", 1_000_000, RANGE);
        f.rapl("intel-rapl:0:0", "core", 900_000, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0", 2_000_000);
        f.set_energy_uj("intel-rapl:0:0", 1_800_000);
        assert_eq!(m.read().package_uj, Some(1_000_000), "the package alone, not plus its core");
    }

    #[test]
    fn every_package_of_a_two_socket_machine_is_counted() {
        let f = Fixture::new("rapl-two-packages");
        f.rapl("intel-rapl:0", "package-0", 1_000_000, RANGE);
        f.rapl("intel-rapl:1", "package-1", 5_000_000, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0", 1_500_000);
        f.set_energy_uj("intel-rapl:1", 5_250_000);
        assert_eq!(m.read().package_uj, Some(750_000), "0.5 J from one, 0.25 J from the other");
    }

    #[test]
    fn a_counter_that_wrapped_is_one_wrap_and_not_a_reset() {
        // At 10 W this counter wraps every 1.8 hours, so a daemon that runs for a
        // day meets this repeatedly. Read as a fall it would be clamped to zero and
        // lose the interval; read as a reset it would credit the CPU with 65 kJ it
        // never spent.
        let f = Fixture::new("rapl-wrap");
        f.rapl("intel-rapl:0", "package-0", RANGE - 400_000, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        // 0.4 J to the top of the range, then 0.1 J past zero.
        f.set_energy_uj("intel-rapl:0", 100_000);
        assert_eq!(m.read().package_uj, Some(500_000));
    }

    #[test]
    fn a_fall_with_no_range_to_explain_it_is_discarded() {
        // `max_energy_range_uj` is world-readable wherever `energy_uj` is, so this
        // is the paranoid branch rather than an observed one. Discarding the
        // interval keeps the total monotonic; inventing the wrap's size would put a
        // spike of unknown height into the figure the header shows.
        let f = Fixture::new("rapl-no-range");
        let d = "sys/class/powercap/intel-rapl:0";
        f.write(&format!("{d}/name"), "package-0\n");
        f.write(&format!("{d}/energy_uj"), "1000000\n");

        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0", 400_000);
        assert_eq!(m.read().package_uj, Some(0), "no negative energy, and no invented spike");
        // And it carries on from the new reading rather than staying stuck.
        f.set_energy_uj("intel-rapl:0", 700_000);
        assert_eq!(m.read().package_uj, Some(300_000));
    }

    #[test]
    fn a_usb_c_port_is_not_a_battery() {
        // This machine exposes two `ucsi-source-psy-USBC000:*` supplies with a
        // `current_now` each. Matching on the directory name instead of on `type`
        // would read a charger as a battery discharging.
        let f = Fixture::new("usb-c");
        let d = "sys/class/power_supply/ucsi-source-psy-USBC000:001";
        f.write(&format!("{d}/type"), "USB\n");
        f.write(&format!("{d}/status"), "Discharging\n");
        f.write(&format!("{d}/current_now"), "500000\n");
        f.write(&format!("{d}/voltage_now"), "5000000\n");

        let mut m = f.meter();
        assert!(!m.has_battery());
        assert_eq!(m.read().discharge_w, None);
    }

    #[test]
    fn the_deepest_state_is_the_highest_numbered_one() {
        let f = Fixture::new("deepest");
        // Four states; only the last one's time is the deep-idle figure.
        f.cpuidle(2, &[(100, 10), (200, 20), (300, 30), (4_000, 40)]);
        let idle = f.meter().read().idle.unwrap();
        assert_eq!(idle.cpus, 2);
        assert_eq!(idle.deepest_usec, 2 * 4_000);
        assert_eq!(idle.idle_usec, 2 * (100 + 200 + 300 + 4_000));
        assert_eq!(idle.entries, 2 * (10 + 20 + 30 + 40));
    }

    #[test]
    fn state_directories_are_ordered_numerically_not_alphabetically() {
        // `state10` sorts before `state2` as a string, so a machine with ten or
        // more idle states would have had a mid-depth state nominated as the
        // deepest — and the headline residency would quietly be of the wrong
        // state, on exactly the hardware with the most to gain.
        let f = Fixture::new("many-states");
        let mut states = Vec::new();
        for i in 0..12u64 {
            states.push((i * 1_000, i));
        }
        f.cpuidle(1, &states);
        let idle = f.meter().read().idle.unwrap();
        assert_eq!(idle.deepest_usec, 11_000, "state11 is the deepest, not state9");
    }

    #[test]
    fn cpus_are_ordered_numerically_too() {
        // Not load-bearing for the sums, but `cpu10` sorting before `cpu2` would
        // make the discovered order depend on the CPU count, and a difference
        // that appears only on large machines is the worst kind.
        let f = Fixture::new("many-cpus");
        f.cpuidle(12, &[(1_000, 1)]);
        let mut m = f.meter();
        assert_eq!(m.cpus(), 12);
        assert_eq!(m.read().idle.unwrap().cpus, 12);
    }

    #[test]
    fn a_kernel_with_no_cpuidle_says_so_rather_than_reporting_perfect_idle() {
        // Virtual machines frequently expose none. Zero counters over a real
        // interval would compute as 0% residency, which reads as a machine that
        // never sleeps — a plausible, wrong, and permanent verdict.
        let f = Fixture::new("no-cpuidle");
        f.energy_battery("Discharging", 6_000_000, 40_000_000);
        let mut m = f.meter();
        assert_eq!(m.cpus(), 0);
        assert_eq!(m.read().idle, None);
        assert!(m.available(), "the battery is still worth reading");
    }

    #[test]
    fn a_machine_with_neither_battery_nor_cpuidle_is_unavailable() {
        let f = Fixture::new("bare");
        f.write("proc/stat", "ctxt 12345\n");
        let mut m = f.meter();
        assert!(!m.available(), "so `dozectl status` can disclaim it rather than report zeroes");
        let r = m.read();
        assert_eq!(r.discharge_w, None);
        assert_eq!(r.idle, None);
        assert_eq!(r.ctxt_switches, Some(12_345), "but /proc/stat still answers");
    }

    #[test]
    fn a_cpu_that_went_offline_lowers_the_cpu_count() {
        // Which is what `Meter` discards the interval on. Counting it as present
        // with zero counters would instead report a residency diluted by however
        // many CPUs had gone away — a slow, quiet drift downwards.
        let f = Fixture::new("hotplug");
        f.cpuidle(4, &[(1_000, 1)]);
        assert_eq!(f.meter().read().idle.unwrap().cpus, 4);

        // The meter's paths are already resolved, so this is exactly the mid-run
        // hotplug case rather than a rediscovery.
        let mut m = f.meter();
        fs::remove_dir_all(f.0.join("sys/devices/system/cpu/cpu3")).unwrap();
        assert_eq!(m.read().idle.unwrap().cpus, 3);
    }

    #[test]
    fn proc_stat_is_parsed_from_the_right_line() {
        // `processes` and `ctxt` are adjacent and both are bare counters, so a
        // prefix match on the wrong one is easy and silent.
        let stat = "cpu  1 2 3 4\nintr 4612544396 130 4811\nctxt 3973448613\nbtime 1785694150\n\
                    processes 85126754\n";
        assert_eq!(parse_ctxt(stat), Some(3_973_448_613));
        assert_eq!(parse_ctxt("ctxtsomething 5\n"), None, "the space is part of the key");
        assert_eq!(parse_ctxt("cpu 1 2 3\n"), None);
    }

    #[test]
    fn unreadable_and_malformed_counters_are_absent_rather_than_wrong() {
        let f = Fixture::new("garbage");
        let d = "sys/class/power_supply/BAT0";
        f.write(&format!("{d}/type"), "Battery\n");
        f.write(&format!("{d}/status"), "Discharging\n");
        f.write(&format!("{d}/power_now"), "not a number\n");
        f.write(&format!("{d}/energy_now"), "\n");
        let r = f.meter().read();
        assert_eq!(r.discharge_w, None);
        assert_eq!(r.energy_uwh, None);
    }

    /// Reads the real machine. Ignored, like every other test here that needs
    /// hardware to be what it is.
    #[test]
    #[ignore = "reads this machine's own counters"]
    fn the_real_machine_answers() {
        let mut m = PowerMeter::new();
        assert!(m.available(), "no battery and no cpuidle on a real Linux box?");
        let r = m.read();
        assert!(r.ctxt_switches.unwrap() > 0);
        if let Some(idle) = r.idle {
            assert!(idle.cpus > 0);
            assert!(idle.deepest_usec > 0, "some deep idle since boot is a safe bet");
            assert!(idle.idle_usec >= idle.deepest_usec, "deep idle is a subset of all idle");
        }
        // RAPL either answered or did not, and both are correct — printed rather
        // than asserted because which one it is depends on a permission this test
        // must not change. `None` on a stock kernel is the expected outcome.
        println!(
            "rapl: {}",
            match (r.package_uj.is_some(), m.package_denied()) {
                (true, _) => "readable",
                (false, true) => "present, permission denied (the ordinary case)",
                (false, false) => "no package domains on this machine",
            }
        );
        println!("{r:?}");
    }

    /// The §13 obligation: a new periodic read has to have its cost measured and
    /// stated, not assumed to be small.
    #[test]
    #[ignore = "measures this machine's own sweep cost"]
    fn what_a_sweep_costs() {
        let mut m = PowerMeter::new();
        let n = 200;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let _ = m.read();
        }
        let per = start.elapsed() / n;
        println!("{} cpus, {:?} per sweep", m.cpus(), per);
        // Ten times the measured 0.35 ms, so this fails on a real regression and
        // not on a loaded CI box.
        assert!(per < std::time::Duration::from_millis(4), "a sweep should be cheap: {per:?}");
    }

    // ---- the package breakdown -------------------------------------------

    #[test]
    fn a_subdomain_is_reported_inside_the_package_and_never_added_to_it() {
        // The trap this whole breakdown is built around. `intel-rapl:0:0` is
        // `core`, part of `intel-rapl:0` — so `package` must be the parent's own
        // 1000 µJ and not 1500, which is what summing every readable domain in
        // `/sys/class/powercap` would give and what the first version of
        // `find_rapl` was written to avoid for exactly one level of the tree.
        let f = Fixture::new("subdomain-inside");
        f.rapl("intel-rapl:0", "package-0", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:0", "core", 0, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0", 1_000);
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:0", 500);
        let r = m.read();
        assert_eq!(r.package_uj, Some(1_000), "the package is the parent alone");
        assert_eq!(r.components.core_uj, Some(500));
        assert!(
            r.components.core_uj.unwrap() < r.package_uj.unwrap(),
            "a child that is not smaller than its parent is not a child"
        );
    }

    #[test]
    fn a_subdomain_is_found_by_the_tree_and_not_by_its_name() {
        // `intel-rapl:0:0` also appears as a top-level symlink in the real
        // `/sys/class/powercap`, so a `find_rapl` that parsed names for colons
        // would find `core` twice — once as a child and once as a stray — and
        // double its figure. Containment is read from the directory instead.
        let f = Fixture::new("subdomain-by-tree");
        f.rapl("intel-rapl:0", "package-0", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:0", "core", 0, RANGE);
        // The sibling copy of the same domain, exactly as the real tree has it.
        f.rapl("intel-rapl:0:0", "core", 0, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:0", 500);
        f.set_energy_uj("intel-rapl:0:0", 500);
        assert_eq!(
            m.read().components.core_uj,
            Some(500),
            "the same domain reached by two paths is still one domain"
        );
    }

    #[test]
    fn every_core_domain_of_a_two_socket_machine_is_summed() {
        // The same argument `every_package_of_a_two_socket_machine_is_counted`
        // makes about packages: two sockets have two `core` domains, they wrap
        // independently, and "how much of this machine is the cores" is one
        // number.
        let f = Fixture::new("two-socket-core");
        for socket in 0..2 {
            f.rapl(&format!("intel-rapl:{socket}"), &format!("package-{socket}"), 0, RANGE);
            f.rapl(&format!("intel-rapl:{socket}/intel-rapl:{socket}:0"), "core", 0, RANGE);
        }
        let mut m = f.meter();
        let _ = m.read();
        for socket in 0..2 {
            f.set_energy_uj(&format!("intel-rapl:{socket}/intel-rapl:{socket}:0"), 700);
        }
        assert_eq!(m.read().components.core_uj, Some(1_400));
    }

    #[test]
    fn every_kind_of_subdomain_lands_under_its_own_heading() {
        let f = Fixture::new("all-subdomains");
        f.rapl("intel-rapl:0", "package-0", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:0", "core", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:1", "uncore", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:2", "dram", 0, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:0", 100);
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:1", 200);
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:2", 300);
        let c = m.read().components;
        assert_eq!((c.core_uj, c.uncore_uj, c.dram_uj), (Some(100), Some(200), Some(300)));
    }

    #[test]
    fn a_subdomain_nobody_has_a_heading_for_is_left_out_rather_than_misfiled() {
        // A breakdown is read as adding up, so a domain filed under the wrong
        // name is worse than one that is missing — and `PackageBreakdown::rest_w`
        // already has a place for whatever is not itemised.
        let f = Fixture::new("unknown-subdomain");
        f.rapl("intel-rapl:0", "package-0", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:0", "somethingnew", 0, RANGE);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:0", 500);
        let c = m.read().components;
        assert_eq!((c.core_uj, c.uncore_uj, c.dram_uj), (None, None, None));
    }

    #[test]
    fn a_closed_subdomain_is_absent_rather_than_zero_and_is_not_the_permission_to_grant() {
        // Two claims. A sub-domain we cannot read contributes nothing rather than
        // a zero watt figure; and it must not set `package_denied`, which means
        // exactly "the package counter is there and shut" and is the one sentence
        // `dozectl status` tells a user to act on. A machine whose package is
        // already open has nothing left to grant, and saying otherwise sends
        // somebody back to the README for no reason.
        let f = Fixture::new("closed-subdomain");
        f.rapl("intel-rapl:0", "package-0", 0, RANGE);
        f.write("sys/class/powercap/intel-rapl:0/intel-rapl:0:0/name", "core\n");
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0", 1_000);
        let r = m.read();
        assert_eq!(r.components.core_uj, None, "unreadable is not zero");
        assert_eq!(r.package_uj, Some(1_000), "and it does not cost us the package");
        assert!(!m.package_denied(), "the package is open; there is nothing to grant");
    }

    #[test]
    fn a_subdomain_wraps_against_its_own_range_and_not_its_parents() {
        // Whose counter turned over can only be recognised per domain, which is
        // the whole reason accumulation happens down here. A child with a smaller
        // range than its package is the case that catches a shared constant.
        const SMALL: u64 = 1_000;
        let f = Fixture::new("subdomain-wrap");
        f.rapl("intel-rapl:0", "package-0", 0, RANGE);
        f.rapl("intel-rapl:0/intel-rapl:0:0", "core", 900, SMALL);
        let mut m = f.meter();
        let _ = m.read();
        f.set_energy_uj("intel-rapl:0/intel-rapl:0:0", 50);
        assert_eq!(
            m.read().components.core_uj,
            Some(150),
            "100 up to the wrap, then 50 past it — not a 850 µJ fall"
        );
    }

    // ---- the GPU ---------------------------------------------------------

    #[test]
    fn the_gpu_average_is_preferred_and_the_instantaneous_reading_is_the_fallback() {
        let f = Fixture::new("gpu-average");
        f.amdgpu("hwmon10", Some(13_049_000), Some(13_052_000));
        assert_eq!(f.meter().read().gpu_uw, Some(13_049_000));

        let g = Fixture::new("gpu-input-only");
        g.amdgpu("hwmon10", None, Some(13_052_000));
        assert_eq!(g.meter().read().gpu_uw, Some(13_052_000), "a spot reading beats none");
    }

    #[test]
    fn two_gpus_are_two_chips_and_their_draws_add() {
        // Unlike the package boundary, which cannot be crossed: two `amdgpu`
        // devices are genuinely two pieces of silicon.
        let f = Fixture::new("two-gpus");
        f.amdgpu("hwmon2", Some(5_000_000), None);
        f.amdgpu("hwmon10", Some(13_000_000), None);
        assert_eq!(f.meter().read().gpu_uw, Some(18_000_000));
    }

    #[test]
    fn a_hwmon_that_is_not_a_gpu_is_not_read_as_one() {
        // This machine exposes eleven hwmons, of which one is the GPU and the
        // rest are fans, batteries, an NVMe and two USB-C ports. `power1_input`
        // on `BAT0` is the battery's draw and would have been reported as GPU
        // watts by a search that matched on the filename alone.
        let f = Fixture::new("not-a-gpu");
        f.write("sys/class/hwmon/hwmon3/name", "BAT0\n");
        f.write("sys/class/hwmon/hwmon3/power1_input", "6400000\n");
        assert_eq!(f.meter().read().gpu_uw, None);
    }

    #[test]
    fn the_gpu_label_is_the_sensors_own_word() {
        // "GPU: 24 W" invites the reader to add it to the package. "amdgpu PPT"
        // is what the file says, and PPT is a package figure.
        let f = Fixture::new("gpu-label");
        f.amdgpu("hwmon10", Some(13_049_000), None);
        assert_eq!(f.meter().gpu_label(), Some("PPT"));
    }

    // ---- the /proc/stat denominator --------------------------------------

    #[test]
    fn busy_time_is_a_fraction_so_the_clock_tick_never_enters_it() {
        // `USER_HZ` is a libc constant this workspace cannot ask for — `sysconf`
        // is unsafe and the crate is `#![forbid(unsafe_code)]`. Carrying busy and
        // total separately means it cancels instead of being hardcoded to a 100
        // nobody could check.
        let stat = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\n\
                    cpu1 50 0 50 400 0 0 0 0 0 0\nctxt 5\n";
        let c = parse_cpu_time(stat).unwrap();
        assert_eq!(c.busy_ticks, 200);
        assert_eq!(c.total_ticks, 1_000, "busy plus idle plus iowait");
        assert_eq!(c.cpus, 2, "counted from the per-cpu lines, not from cpuidle");
    }

    #[test]
    fn iowait_is_idle_and_is_not_counted_as_busy() {
        // A machine waiting on a disk is executing nothing. Counting the wait as
        // busy would inflate the denominator and shrink every app's attributed
        // watts on exactly the machines that are slow for storage reasons.
        let c =
            parse_cpu_time("cpu  10 0 0 0 990 0 0 0 0 0\ncpu0 10 0 0 0 990 0 0 0 0 0\n").unwrap();
        assert_eq!(c.busy_ticks, 10);
        assert_eq!(c.total_ticks, 1_000);
    }

    #[test]
    fn guest_time_is_not_counted_twice() {
        // The kernel already counts `guest` inside `user` and `guest_nice` inside
        // `nice`. Summing all ten fields would report a host running VMs as
        // busier than it is, and divide every app's share down accordingly.
        let c = parse_cpu_time("cpu  100 0 0 900 0 0 0 0 80 0\ncpu0 100 0 0 900 0 0 0 0 80 0\n")
            .unwrap();
        assert_eq!(c.busy_ticks, 100, "the 80 guest ticks are already inside the 100 user");
        assert_eq!(c.total_ticks, 1_000);
    }

    #[test]
    fn the_aggregate_line_is_taken_and_not_the_first_cpus() {
        // `cpu0` starts with `cpu`. On a single-CPU machine it is the only other
        // candidate, and taking it would report one core's ticks as the
        // machine's — a denominator too small by however many cores there are.
        let stat = "cpu  400 0 0 600 0 0 0 0 0 0\ncpu0 100 0 0 900 0 0 0 0 0 0\n";
        assert_eq!(parse_cpu_time(stat).unwrap().busy_ticks, 400);
    }

    #[test]
    fn a_stat_line_we_do_not_recognise_yields_no_denominator_rather_than_a_guess() {
        assert!(parse_cpu_time("cpu  1 2\nctxt 5\n").is_none(), "too few fields to be that line");
        assert!(parse_cpu_time("ctxt 5\n").is_none());
    }

    #[test]
    fn one_read_of_proc_stat_answers_both_questions() {
        // Two reads would cost twice as much and could disagree: the wakeup count
        // and the busy fraction are compared against each other in `dozectl
        // apps`, and a discrepancy from two reads a microsecond apart would be
        // indistinguishable from a real one.
        let f = Fixture::new("one-read");
        f.write("proc/stat", "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 0 0 0 0 0 0 0 0\nctxt 999\n");
        let r = f.meter().read();
        assert_eq!(r.ctxt_switches, Some(999));
        assert_eq!(r.cpu.unwrap().busy_ticks, 200);
    }
}
