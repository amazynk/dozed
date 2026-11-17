//! Per-process GPU engine time, from the kernel's DRM fdinfo interface.
//!
//! The one per-app figure here that is a **measurement rather than an
//! apportionment**. `crate::energy` can only measure whole power domains, so
//! `dozed_core::attribution` has to model each app's share of one; the GPU
//! exposes an actual per-client counter, so nothing is modelled — the kernel
//! says how many nanoseconds of each engine a client was given.
//!
//! ```text
//! /proc/<pid>/fdinfo/<fd>
//!   drm-driver:        amdgpu
//!   drm-client-id:     223
//!   drm-engine-gfx:    145328304278 ns
//!   drm-engine-compute:  9901187193 ns
//! ```
//!
//! ## Two things that will produce a wrong number
//!
//! **A client is not a file descriptor, and processes dup theirs.** Measured on
//! this desktop: **136 DRM file descriptors carrying only 50 distinct
//! `drm-client-id`s** — every process holds about three, and Firefox and Brave
//! hold six each across two clients. Summing per descriptor triple-counts. It
//! happens not to change the total much here, because the duplicates carry a
//! client id and no engine counters, but that is a property of one driver on one
//! day and not something to build on. Descriptors are folded by client id.
//!
//! **Finding the descriptors is expensive; reading them is not.** A sweep of
//! every `/proc/<pid>/fdinfo` costs **194 ms**, and 45 ms even when filtered by
//! `readlink` first — against 33 ms for the whole of `sampler`'s existing pass
//! over 2,725 threads. Narrowing to the app trees does not help, because browsers
//! *are* the file descriptors: 8,268 of them across 53 processes. But a DRM
//! client opens `/dev/dri/renderD128` once at startup and holds it for its life,
//! so the paths are stable. Discovery is therefore per process and cached, and
//! the tick re-reads only the ~130 paths already known: **4.5 ms**, a seventh of
//! the existing sweep. §13's constraint is wakeups and this adds none, but the
//! difference between those two implementations is the difference between
//! shipping this and not.
//!
//! ## What it does not tell you
//!
//! Engine time is not watts, and this module does not pretend otherwise. On an
//! integrated GPU the chip's power is the CPU package's power
//! (`crate::energy::Gpu`), so a per-app GPU *wattage* would be a second share of
//! joules the CPU column has already apportioned. A percentage is a different
//! claim and a safe one.
//!
//! Nor do clients account for the whole chip. Measured against `gpu_busy_percent`
//! over the same windows: clients 7.5%, 10.9%, 10.8% of one engine against 10.8%,
//! 15.1%, 17.2% of chip busy — roughly two thirds. The rest is the compositor's
//! own work, the display pipeline and kernel submissions that belong to no
//! client, and it is the same gap `coverage` already names for wakeups.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

/// How often the descriptor cache is rebuilt from scratch, in ticks.
///
/// Discovery is cached because a DRM client holds its descriptor for life — but
/// "holds it for life" is not "opens it at startup". A tab that first touches
/// WebGL an hour in opens one then, and a cache that never looked again would
/// report it as a process using no GPU at all, which is the "measured zero"
/// this codebase keeps refusing to print.
///
/// **Measured**, which is the reason this one has a number rather than a
/// judgement: a cold discovery over 755 processes costs **60 ms**, and a cached
/// tick costs **4.1 ms** on the same busy machine (1.4 ms measured on a quieter
/// one — the figure moves with load, and quoting only the flattering one would be
/// the kind of single confident number this file is otherwise careful about).
/// Rebuilding every 64 ticks therefore adds 60/64 ≈ **0.9 ms** a tick, about a
/// quarter again on top of the sweep it rides. Every 8 ticks would have added
/// 7.5 ms and doubled it twice over; every 512 would be free and would leave a
/// process misreported for the better part of an hour.
const REDISCOVER_EVERY: u32 = 64;

/// The DRM clients of the processes dozed is watching.
#[derive(Debug, Clone)]
pub struct GpuClients {
    proc: PathBuf,
    /// Per process: the `fdinfo` paths that proved to be DRM clients.
    ///
    /// Cached including the **empty** answer, which is the common one and the
    /// expensive one to compute — a process with no GPU work still has its
    /// descriptors walked to find that out.
    known: HashMap<u32, Vec<PathBuf>>,
    tick: u32,
}

impl GpuClients {
    #[must_use]
    pub fn new() -> Self {
        Self::at(Path::new("/proc"))
    }

    /// Discover under `proc`, which tests point at a fixture tree.
    #[must_use]
    pub fn at(proc: &Path) -> Self {
        Self { proc: proc.to_owned(), known: HashMap::new(), tick: 0 }
    }

    /// Engine nanoseconds per process, summed over that process's DRM clients.
    ///
    /// Cumulative since each client was created, so the caller differences it
    /// like every other counter here. A process with no DRM client is absent
    /// rather than zero, for the reason the whole crate repeats: "it did no GPU
    /// work" and "it cannot do GPU work" are different statements.
    pub fn engine_nsec(&mut self, pids: &[u32]) -> BTreeMap<u32, u64> {
        self.tick = self.tick.wrapping_add(1);
        if self.tick.is_multiple_of(REDISCOVER_EVERY) {
            self.known.clear();
        }
        let mut out = BTreeMap::new();
        for &pid in pids {
            let paths = match self.known.get(&pid) {
                Some(paths) => paths,
                None => self.known.entry(pid).or_insert_with(|| discover(&self.proc, pid)),
            };
            if paths.is_empty() {
                continue;
            }
            // Folded by `drm-client-id`, because a descriptor is not a client:
            // this desktop holds 136 descriptors across 50 clients, so summing
            // per descriptor would report about three times the work done.
            let mut seen = BTreeSet::new();
            let mut total = 0u64;
            let mut any = false;
            for path in paths {
                let Ok(text) = fs::read_to_string(path) else { continue };
                let Some((client, nsec)) = parse_client(&text) else { continue };
                if !seen.insert(client) {
                    continue;
                }
                any = true;
                total = total.saturating_add(nsec);
            }
            if any {
                out.insert(pid, total);
            }
        }
        out
    }

    /// Forget processes that have gone, so the cache cannot grow without bound.
    pub fn retain(&mut self, live: &BTreeSet<u32>) {
        self.known.retain(|pid, _| live.contains(pid));
    }
}

impl Default for GpuClients {
    fn default() -> Self {
        Self::new()
    }
}

/// One process's DRM `fdinfo` paths, found by following its descriptors.
///
/// `readlink` rather than reading each `fdinfo`, because that is the whole cost:
/// 45 ms against 194 ms over this machine's 14,700 descriptors. The link target
/// is enough — every DRM node lives under `/dev/dri`.
fn discover(proc: &Path, pid: u32) -> Vec<PathBuf> {
    let dir = proc.join(pid.to_string()).join("fd");
    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else { continue };
        if !target.starts_with("/dev/dri") {
            continue;
        }
        let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        found.push((fd, proc.join(pid.to_string()).join("fdinfo").join(fd.to_string())));
    }
    // Numerically, so the client that gets folded in first is the same one
    // between runs. `read_dir` offers descriptors in whatever order it likes and
    // `10` sorts before `2` as a string.
    found.sort_by_key(|(fd, _)| *fd);
    found.into_iter().map(|(_, path)| path).collect()
}

/// `(client id, engine nanoseconds)` from one `fdinfo`, or `None` if it is not a
/// DRM client.
///
/// Every `drm-engine-*` is summed rather than only `gfx`: `compute`, `enc` and
/// `dec` are work the chip did for this client too, and an app that decodes video
/// on the GPU would otherwise read as using none of it. The sum can exceed one
/// engine's worth of time for the same reason `cpu_pct` can exceed 100 — there is
/// more than one engine.
fn parse_client(text: &str) -> Option<(u64, u64)> {
    let mut client = None;
    let mut nsec = 0u64;
    let mut any_engine = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("drm-client-id:") {
            client = v.trim().parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("drm-engine-") {
            // `drm-engine-gfx:\t145328304278 ns`
            let Some((_, value)) = rest.split_once(':') else { continue };
            if let Some(v) = value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok()) {
                any_engine = true;
                nsec = nsec.saturating_add(v);
            }
        }
    }
    any_engine.then_some(())?;
    Some((client?, nsec))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway `/proc`, in the shape the rest of this workspace uses.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dozed-gpu-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// One descriptor of one process: a symlink to `target`, and the
        /// `fdinfo` beside it. The symlink target need not exist — `read_link`
        /// returns what was written, which is exactly what discovery reads.
        fn fd(&self, pid: u32, fd: u32, target: &str, fdinfo: &str) {
            let base = self.0.join(pid.to_string());
            fs::create_dir_all(base.join("fd")).unwrap();
            fs::create_dir_all(base.join("fdinfo")).unwrap();
            let link = base.join("fd").join(fd.to_string());
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(target, link).unwrap();
            fs::write(base.join("fdinfo").join(fd.to_string()), fdinfo).unwrap();
        }

        fn clients(&self) -> GpuClients {
            GpuClients::at(&self.0)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn drm(client: u64, gfx: u64, compute: u64) -> String {
        format!(
            "pos:\t0\nflags:\t02\ndrm-driver:\tamdgpu\ndrm-client-id:\t{client}\n\
             drm-pdev:\t0000:c4:00.0\ndrm-engine-gfx:\t{gfx} ns\ndrm-engine-compute:\t{compute} ns\n"
        )
    }

    /// Every engine, not only `gfx`. An app decoding video on the GPU would
    /// otherwise read as using none of it.
    #[test]
    fn every_engine_the_client_used_is_counted() {
        let f = Fixture::new("engines");
        f.fd(42, 3, "/dev/dri/renderD128", &drm(7, 1_000, 250));
        assert_eq!(f.clients().engine_nsec(&[42]).get(&42), Some(&1_250));
    }

    /// The trap that would have tripled every figure in the column.
    ///
    /// Measured on the desktop this was written against: **136 DRM descriptors
    /// carrying 50 distinct `drm-client-id`s**. Every process holds about three,
    /// and Firefox and Brave hold six across two clients. A sum per descriptor is
    /// a sum over duplicates.
    #[test]
    fn a_client_reached_through_three_descriptors_is_counted_once() {
        let f = Fixture::new("dup-client");
        for fd in [3, 7, 11] {
            f.fd(42, fd, "/dev/dri/renderD128", &drm(7, 1_000, 0));
        }
        assert_eq!(
            f.clients().engine_nsec(&[42]).get(&42),
            Some(&1_000),
            "three descriptors, one client, one thousand nanoseconds"
        );
    }

    /// Two genuinely different clients in one process — Firefox holds exactly
    /// this shape — do add.
    #[test]
    fn two_distinct_clients_in_one_process_are_summed() {
        let f = Fixture::new("two-clients");
        f.fd(42, 3, "/dev/dri/renderD128", &drm(7, 1_000, 0));
        f.fd(42, 4, "/dev/dri/renderD128", &drm(9, 500, 0));
        assert_eq!(f.clients().engine_nsec(&[42]).get(&42), Some(&1_500));
    }

    /// Absent, not zero. "Did no GPU work" and "cannot do GPU work" are the
    /// distinction this whole crate keeps drawing, and the column renders them
    /// differently.
    #[test]
    fn a_process_with_no_drm_client_is_absent_rather_than_zero() {
        let f = Fixture::new("no-client");
        f.fd(42, 3, "/home/user/notes.txt", "pos:\t0\n");
        let got = f.clients().engine_nsec(&[42]);
        assert!(!got.contains_key(&42), "{got:?}");
    }

    /// Discovery follows the link target, so a socket or a regular file costs one
    /// `readlink` and nothing more — which is the whole reason it is affordable.
    #[test]
    fn a_descriptor_that_is_not_a_gpu_is_never_opened() {
        let f = Fixture::new("not-a-gpu");
        // An `fdinfo` that *would* parse as a DRM client, behind a link that says
        // it is a pipe. If discovery read the file instead of the link, this
        // would be counted.
        f.fd(42, 3, "pipe:[12345]", &drm(7, 9_999, 0));
        assert!(f.clients().engine_nsec(&[42]).is_empty());
    }

    /// A DRM client that reports no engine at all is not a client we can measure,
    /// and must not become a zero in the column.
    #[test]
    fn a_drm_descriptor_with_no_engine_counters_reports_nothing() {
        let f = Fixture::new("no-engine");
        f.fd(42, 3, "/dev/dri/renderD128", "drm-driver:\tamdgpu\ndrm-client-id:\t7\n");
        assert!(f.clients().engine_nsec(&[42]).is_empty());
    }

    /// The cache is keyed by pid and a desktop session churns through thousands
    /// of them, so it has to be prunable or it is a leak with a long fuse.
    #[test]
    fn a_process_that_has_gone_is_dropped_from_the_cache() {
        let f = Fixture::new("prune");
        f.fd(42, 3, "/dev/dri/renderD128", &drm(7, 1_000, 0));
        let mut c = f.clients();
        c.engine_nsec(&[42]);
        assert_eq!(c.known.len(), 1);
        c.retain(&BTreeSet::new());
        assert!(c.known.is_empty());
    }

    /// Discovery is cached because a DRM client holds its descriptor for life —
    /// but that is not the same as opening it at startup. A tab that first
    /// touches WebGL an hour in opens one then, and a cache that never looked
    /// again would report it as a process using no GPU at all.
    #[test]
    fn a_client_opened_later_is_found_on_the_next_rediscovery() {
        let f = Fixture::new("late-client");
        fs::create_dir_all(f.0.join("42/fd")).unwrap();
        fs::create_dir_all(f.0.join("42/fdinfo")).unwrap();
        let mut c = f.clients();
        assert!(c.engine_nsec(&[42]).is_empty(), "nothing yet");

        f.fd(42, 3, "/dev/dri/renderD128", &drm(7, 1_000, 0));
        assert!(c.engine_nsec(&[42]).is_empty(), "still cached as having none");

        // Tick round to the rediscovery boundary.
        for _ in 0..REDISCOVER_EVERY {
            c.engine_nsec(&[42]);
        }
        assert_eq!(c.engine_nsec(&[42]).get(&42), Some(&1_000), "found on the sweep");
    }

    /// `read_dir` offers descriptors in whatever order it likes, and `10` sorts
    /// before `2` as a string — so which client gets folded in first would vary
    /// between runs without this.
    #[test]
    fn descriptors_are_ordered_numerically_not_alphabetically() {
        let f = Fixture::new("fd-order");
        f.fd(42, 2, "/dev/dri/renderD128", &drm(1, 100, 0));
        f.fd(42, 10, "/dev/dri/renderD128", &drm(2, 200, 0));
        let mut c = f.clients();
        c.engine_nsec(&[42]);
        let paths = &c.known[&42];
        assert!(paths[0].ends_with("2"), "{paths:?}");
        assert!(paths[1].ends_with("10"), "{paths:?}");
    }

    #[test]
    fn a_fdinfo_that_is_not_a_drm_client_parses_as_nothing() {
        assert!(parse_client("pos:\t0\nflags:\t02\n").is_none());
        assert!(parse_client("drm-driver:\tamdgpu\ndrm-client-id:\t3\n").is_none());
        // Engines but no client id: nothing to fold duplicates by, so it cannot
        // be counted safely.
        assert!(parse_client("drm-engine-gfx:\t500 ns\n").is_none());
        assert_eq!(parse_client(&drm(3, 10, 20)), Some((3, 30)));
    }

    #[test]
    #[ignore = "reads the real /proc; run with --ignored"]
    fn the_real_machine_answers() {
        let mut c = GpuClients::new();
        let pids: Vec<u32> = fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        let got = c.engine_nsec(&pids);
        println!("{} of {} processes hold a DRM client", got.len(), pids.len());
        // Cold discovery: the cost the rediscovery sweep pays, amortised over
        // REDISCOVER_EVERY ticks. This is the number that constant is chosen
        // against, so it is printed rather than assumed.
        let cold = std::time::Instant::now();
        for _ in 0..3 {
            let mut fresh = GpuClients::new();
            let _ = fresh.engine_nsec(&pids);
        }
        println!("cold discovery: {:?} per sweep", cold.elapsed() / 3);
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let _ = c.engine_nsec(&pids);
        }
        // The whole reason this is shippable: discovery is cached, so the tick
        // re-reads only the paths it already knows.
        println!("cached re-read: {:?} per sweep", start.elapsed() / 10);
    }
}
