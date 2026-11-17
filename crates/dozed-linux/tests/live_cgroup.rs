//! End-to-end actuation against the real kernel.
//!
//! Ignored by default: these tests create a transient systemd unit and freeze
//! it, so they need a live cgroup v2 session with a user manager. `DESIGN.md`
//! §14 says integration testing that actually freezes things belongs in a
//! nested compositor or a VM — that holds for freezing *desktop apps*. These
//! freeze a `sleep` loop this test started itself, which is safe anywhere.
//!
//! ```sh
//! cargo test -p dozed-linux --test live_cgroup -- --ignored --test-threads=1
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use dozed_core::{Action, AppKey, Outcome};
use dozed_linux::{Cgroup, CgroupActuator, Guard, Target};

/// The app slice of whoever runs the test, rather than of UID 1000.
fn slice() -> PathBuf {
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
    ))
}

/// A transient unit that removes itself when the test ends.
struct Unit {
    name: String,
}

impl Unit {
    fn spawn(name: &str, shell: &str) -> Option<Self> {
        // Clear any leftover from a previous run. Noisy when there is nothing to
        // clear, hence the swallowed output.
        let _ = Command::new("systemctl")
            .args(["--user", "reset-failed", &format!("{name}.service")])
            .stderr(std::process::Stdio::null())
            .status();
        let ok = Command::new("systemd-run")
            .args(["--user", &format!("--unit={name}"), "--collect", "sh", "-c", shell])
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        let unit = Self { name: name.to_owned() };
        // systemd-run returns before the cgroup is populated.
        for _ in 0..50 {
            if unit.cgroup().exists() && !unit.cgroup().procs().unwrap_or_default().is_empty() {
                return Some(unit);
            }
            sleep(Duration::from_millis(100));
        }
        Some(unit)
    }

    fn path(&self) -> PathBuf {
        slice().join(format!("{}.service", self.name))
    }

    fn cgroup(&self) -> Cgroup {
        Cgroup::new(self.path())
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        // Thaw before stopping: systemd cannot reap a frozen unit.
        let _ = self.cgroup().thaw();
        for verb in ["stop", "reset-failed"] {
            let _ = Command::new("systemctl")
                .args(["--user", verb, &format!("{}.service", self.name)])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

fn wakeups(cg: &Cgroup) -> u64 {
    cg.procs()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| std::fs::read_to_string(format!("/proc/{p}/status")).ok())
        .flat_map(|s| {
            s.lines()
                .filter(|l| l.contains("ctxt_switches"))
                .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .collect::<Vec<_>>()
        })
        .sum()
}

#[test]
#[ignore = "needs a live cgroup v2 user session"]
fn freeze_stops_wakeups_and_thaw_restores_them() {
    let Some(unit) = Unit::spawn("dozed-it-freeze", "while :; do sleep 0.01; done") else {
        eprintln!("skipping: systemd-run unavailable");
        return;
    };
    let cg = unit.cgroup();
    assert!(cg.exists(), "unit cgroup should exist at {}", cg.path().display());
    assert!(cg.supports_freeze());

    let before = wakeups(&cg);
    sleep(Duration::from_secs(2));
    let running = wakeups(&cg) - before;
    assert!(running > 50, "expected a busy workload, saw {running} wakeups in 2s");

    let mut act = CgroupActuator::new(Guard::for_current_process());
    let target = Target::cgroup(AppKey::comm("dozed-it-freeze"), unit.path());

    assert_eq!(act.apply_to(&target, Action::Freeze { reclaim: true }), Outcome::Applied);
    sleep(Duration::from_millis(300));
    assert!(cg.is_frozen().unwrap(), "cgroup.events should report frozen");

    let frozen_start = wakeups(&cg);
    sleep(Duration::from_secs(2));
    assert_eq!(wakeups(&cg) - frozen_start, 0, "a frozen cgroup must not wake at all");

    assert_eq!(act.apply_to(&target, Action::Restore), Outcome::Applied);
    sleep(Duration::from_millis(300));
    assert!(!cg.is_frozen().unwrap());

    let after = wakeups(&cg);
    sleep(Duration::from_secs(2));
    assert!(wakeups(&cg) - after > 50, "thawing must put the workload back to work");
}

#[test]
#[ignore = "needs a live cgroup v2 user session"]
fn frozen_state_is_invisible_in_proc() {
    // Regression guard for the finding that shaped the whole module: a frozen
    // task still reports S (sleeping), so nothing may infer frozen-ness from
    // /proc.
    let Some(unit) = Unit::spawn("dozed-it-state", "while :; do sleep 0.01; done") else {
        return;
    };
    let cg = unit.cgroup();
    cg.freeze().unwrap();
    sleep(Duration::from_millis(300));

    assert!(cg.is_frozen().unwrap());
    for pid in cg.procs().unwrap() {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let state = status.lines().find(|l| l.starts_with("State:")).unwrap();
        assert!(
            state.contains("S (sleeping)"),
            "expected a frozen task to look like an idle one, got {state:?}"
        );
    }
    cg.thaw().unwrap();
}

#[test]
#[ignore = "needs a live cgroup v2 user session"]
fn a_freeze_outlives_the_process_that_applied_it() {
    // Why DESIGN.md §10.3 must persist the frozen set before freezing: nothing
    // in the kernel ties a freeze to its author.
    let Some(unit) = Unit::spawn("dozed-it-orphan", "while :; do sleep 0.01; done") else {
        return;
    };
    let path = unit.path();

    {
        let actuator_that_dies = CgroupActuator::new(Guard::for_current_process());
        let mut actuator_that_dies = actuator_that_dies;
        let target = Target::cgroup(AppKey::comm("dozed-it-orphan"), path.clone());
        assert_eq!(
            actuator_that_dies.apply_to(&target, Action::Freeze { reclaim: false }),
            Outcome::Applied
        );
    } // dropped, as if the daemon were SIGKILLed

    sleep(Duration::from_millis(300));
    let cg = Cgroup::new(&path);
    assert!(cg.is_frozen().unwrap(), "the freeze must survive its author, and it does");

    // Which is exactly what the startup sweep has to undo.
    cg.thaw().unwrap();
    sleep(Duration::from_millis(200));
    assert!(!cg.is_frozen().unwrap());
}

#[test]
#[ignore = "needs a live cgroup v2 user session"]
fn the_guard_refuses_to_freeze_our_own_cgroup() {
    // The invariant that matters most: dozed must never stop itself, whatever
    // the rules say.
    let own = dozed_linux::cgroup_path_of_pid(std::process::id()).expect("own cgroup");
    let cg = Cgroup::at(std::path::Path::new("/sys/fs/cgroup"), &own);
    let guard = Guard::for_current_process();
    let veto = guard.veto(&cg);
    assert!(veto.is_some(), "guard must veto our own cgroup, got {veto:?}");

    let mut act = CgroupActuator::new(guard);
    let target = Target::cgroup(AppKey::comm("self"), cg.path().to_path_buf());
    let outcome = act.apply_to(&target, Action::Freeze { reclaim: false });
    assert!(matches!(outcome, Outcome::Skipped { .. }), "{outcome:?}");
    assert!(!cg.is_frozen().unwrap(), "and we are still running, obviously");
}

#[test]
#[ignore = "needs a live cgroup v2 user session"]
fn throttle_goes_through_systemd_and_actually_caps() {
    // `app.slice` does not enable the cpu controller, so `cpu.max` does not
    // exist until systemd is asked to create it. This proves the whole route:
    // no cpu.max, SetUnitProperties, cpu.max appears, and the workload obeys it.
    let Some(unit) = Unit::spawn("dozed-it-throttle", "while :; do :; done") else {
        eprintln!("skipping: systemd-run unavailable");
        return;
    };
    let cg = unit.cgroup();
    assert!(cg.exists());

    let mut act = CgroupActuator::new(Guard::for_current_process());
    let target = Target::cgroup(AppKey::comm("dozed-it-throttle"), unit.path());

    // A busy loop with no cap should be using most of a core.
    let before = cpu_usec(&cg);
    sleep(Duration::from_secs(2));
    let uncapped = (cpu_usec(&cg) - before) / 2;
    assert!(uncapped > 500_000, "expected a busy loop, saw {uncapped} usec/s");

    assert_eq!(act.apply_to(&target, Action::Throttle { pct: 5 }), Outcome::Applied);
    sleep(Duration::from_millis(500));
    assert!(
        cg.path().join("cpu.max").is_file(),
        "systemd should have enabled the cpu controller and created cpu.max"
    );

    let start = cpu_usec(&cg);
    sleep(Duration::from_secs(3));
    let capped = (cpu_usec(&cg) - start) / 3;
    // The spike measured 49 ms/s against a 5% cap; allow generous slack for
    // scheduling noise while still failing if the cap did nothing.
    assert!(capped < 150_000, "expected ~50000 usec/s under a 5% cap, saw {capped}");
    assert!(capped < uncapped / 3, "capped {capped} should be far below uncapped {uncapped}");

    assert_eq!(act.apply_to(&target, Action::Restore), Outcome::Applied);
    sleep(Duration::from_secs(1));
    let after = cpu_usec(&cg);
    sleep(Duration::from_secs(2));
    let released = (cpu_usec(&cg) - after) / 2;
    assert!(released > uncapped / 2, "clearing the quota must give the CPU back, saw {released}");
}

fn cpu_usec(cg: &Cgroup) -> u64 {
    std::fs::read_to_string(cg.path().join("cpu.stat"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("usage_usec "))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0)
}

#[test]
#[ignore = "needs a live cgroup v2 user session"]
fn sigstop_falls_back_when_there_is_no_exclusive_scope() {
    // The case that makes this path necessary: an app launched from a terminal
    // shares the terminal's scope, so there is no cgroup to freeze and the
    // process tree is the only unit left.
    let Some(unit) = Unit::spawn("dozed-it-tree", "while :; do sleep 0.01; done & wait") else {
        return;
    };
    let cg = unit.cgroup();
    let pids = cg.procs().unwrap();
    let root = *pids.first().expect("the shell we just started");

    let mut act = CgroupActuator::new(Guard::for_current_process());
    let target = Target::trees(AppKey::comm("dozed-it-tree"), vec![root]);

    let before = wakeups(&cg);
    sleep(Duration::from_secs(2));
    assert!(wakeups(&cg) - before > 20, "expected a busy tree");

    assert_eq!(act.apply_to(&target, Action::Freeze { reclaim: false }), Outcome::Applied);
    sleep(Duration::from_millis(500));

    // A SIGSTOPped task reports T, unlike a cgroup-frozen one which reports S.
    let states: Vec<String> = cg
        .procs()
        .unwrap()
        .iter()
        .filter_map(|p| std::fs::read_to_string(format!("/proc/{p}/stat")).ok())
        .filter_map(|s| {
            let i = s.rfind(')')?;
            Some(s[i + 1..].split_whitespace().next()?.to_owned())
        })
        .collect();
    assert!(states.iter().any(|s| s == "T"), "expected stopped tasks, got {states:?}");

    let stopped_at = wakeups(&cg);
    sleep(Duration::from_secs(2));
    let while_stopped = wakeups(&cg) - stopped_at;
    assert!(while_stopped < 5, "a stopped tree should be quiet, saw {while_stopped}");

    assert_eq!(act.apply_to(&target, Action::Restore), Outcome::Applied);
    sleep(Duration::from_millis(500));
    let resumed = wakeups(&cg);
    sleep(Duration::from_secs(2));
    assert!(wakeups(&cg) - resumed > 20, "SIGCONT must put the tree back to work");
}
