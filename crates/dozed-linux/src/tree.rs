//! Freezing an app that has no cgroup scope of its own.
//!
//! The cgroup path is always preferred: a cgroup freeze is atomic, so it cannot
//! lose a race against `fork()`. But a scope is not automatically a per-app
//! boundary. An app launched from a terminal inherits the terminal's scope
//! (`DESIGN.md` §4), and freezing that would take the terminal down with it — so
//! for those apps there is no scope to use and the only remaining unit is the
//! process tree.
//!
//! This path is **racy by nature**, which is exactly why cgroups are preferred:
//!
//! * `SIGSTOP` on a parent leaves its children running. Verified directly during
//!   the §16 spike, and the reason the walk has to visit every process rather
//!   than just the root.
//! * The transient children of a shell loop come and go mid-operation, so a
//!   process can be forked after we enumerate and before we signal. We walk
//!   children first so that a parent cannot spawn new children behind us, but
//!   this narrows the window rather than closing it.
//!
//! Two invariants are enforced here rather than trusted from the caller, because
//! this is the last point before a signal goes out:
//!
//! * **Same UID only** (`DESIGN.md` §1). A PID we do not own is never signalled.
//! * **The never-freeze list** (§10.1). The tree walk can reach processes the
//!   caller never named — a shell's children include whatever it launched — so
//!   every PID is checked individually, not just the root.

use rustix::process::{Pid, Signal};

use crate::guard::Guard;

/// Bound on how many processes one app's tree may contain.
///
/// A browser is 20-odd processes and a compile can be hundreds; past this we are
/// almost certainly walking something that is not one app, and signalling it
/// would be worse than doing nothing.
const MAX_TREE: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("process {pid} vanished before it could be signalled")]
    Vanished { pid: u32 },
    #[error("not permitted to signal process {pid}")]
    Denied { pid: u32 },
    #[error("nothing left of the process tree rooted at {root}")]
    Empty { root: u32 },
    #[error("process tree rooted at {root} exceeds {MAX_TREE} processes")]
    TooLarge { root: u32 },
}

impl TreeError {
    /// Whether this is the app having exited, which is routine rather than a
    /// fault (`DESIGN.md` §7).
    #[must_use]
    pub const fn is_vanished(&self) -> bool {
        matches!(self, Self::Vanished { .. } | Self::Empty { .. })
    }
}

/// Every process in the tree rooted at `root`, parents before children.
///
/// Reads `/proc/<pid>/task/<tid>/children`, which is cheaper and far more direct
/// than scanning every `/proc/*/stat` for a matching ppid.
#[must_use]
pub fn collect(root: u32) -> Vec<u32> {
    let mut out = vec![root];
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        // A process has one `children` file per thread, and a thread other than
        // the leader can be the one that forked.
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for task in tasks.flatten() {
            let Ok(text) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in text.split_whitespace().filter_map(|c| c.parse::<u32>().ok()) {
                if !out.contains(&child) {
                    out.push(child);
                    queue.push(child);
                }
            }
        }
        if out.len() > MAX_TREE {
            break;
        }
    }
    out
}

/// Which processes in the tree we are actually allowed to touch.
///
/// Anything owned by another user, or on the never-freeze list, is dropped —
/// and dropping it is safe in a way that keeping it is not, because the result
/// is that we leave a process running rather than stopping one we should not.
fn signallable(tree: &[u32], guard: &Guard) -> Vec<u32> {
    let our_uid = rustix::process::getuid().as_raw();
    tree.iter()
        .copied()
        .filter(|pid| match uid_of(*pid) {
            Some(uid) if uid == our_uid => true,
            // Unreadable or someone else's: not ours to touch.
            _ => false,
        })
        .filter(|pid| match crate::guard::comm_of(*pid) {
            Some(comm) => !guard.forbids_comm(&comm),
            // A process whose comm we cannot read might be anything, including
            // the compositor. Fail towards "never touch this".
            None => false,
        })
        .collect()
}

/// The parent of `pid`, or `None` if it cannot be read.
///
/// `comm` can contain spaces *and* parentheses — `Isolated Web Co` and
/// `(sd-pam)` both occur — so the fields cannot be counted from the left. They
/// are counted from the last `)`, after which come state, ppid, and the rest.
fn ppid_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// The roots of `pids`: those whose parent is not itself in the set.
///
/// An app is not one tree. Firefox on this machine was 44 processes in **three**
/// independent trees, one per `firefox-bin`, and the unit was being taken as
/// `pids.first()` — the numerically smallest PID, which after a PID wraparound
/// was a stray `Web Content` leaf with no children at all. `SIGSTOP` reached that
/// one process; the other 43, including three live browser mains, ran untouched
/// for seven and a half hours while `dozed` reported the app as dozing.
///
/// A PID whose parent cannot be read counts as a root: walking it as its own tree
/// is how it still gets stopped, where dropping it would silently leave it out.
#[must_use]
pub fn roots_of(pids: &[u32]) -> Vec<u32> {
    let set: std::collections::BTreeSet<u32> = pids.iter().copied().collect();
    pids.iter().copied().filter(|p| ppid_of(*p).is_none_or(|pp| !set.contains(&pp))).collect()
}

fn uid_of(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    // "Uid:\treal\teffective\tsaved\tfs" — the real uid is what we compare.
    line.split_whitespace().nth(1)?.parse().ok()
}

/// `SIGSTOP` every process in the tree, children first.
///
/// Children first so that stopping a parent cannot be followed by that parent
/// spawning a child we never see. The window is narrowed, not closed; a cgroup
/// freeze is the only way to close it, which is why this is the fallback.
pub fn stop(roots: &[u32], guard: &Guard) -> Result<Vec<u32>, TreeError> {
    // Errors name a root for diagnostics; with a forest, the first one.
    let root = roots.first().copied().unwrap_or_default();
    // The union of every root's tree, deduplicated: two roots can reach the same
    // process once one has been reparented.
    let mut tree: Vec<u32> = Vec::new();
    for r in roots {
        for pid in collect(*r) {
            if !tree.contains(&pid) {
                tree.push(pid);
            }
        }
    }
    if tree.len() > MAX_TREE {
        return Err(TreeError::TooLarge { root });
    }
    let targets = signallable(&tree, guard);
    if targets.is_empty() {
        return Err(TreeError::Empty { root });
    }

    let mut stopped = Vec::new();
    // `collect` yields parents before children, so reversing gives children
    // before parents.
    for pid in targets.iter().rev().copied() {
        match signal(pid, Signal::STOP) {
            Ok(()) => stopped.push(pid),
            // It exited while we were walking. Routine; keep going, because the
            // rest of the tree is still worth stopping.
            Err(TreeError::Vanished { .. }) => {}
            Err(e) => {
                // Undo what we already did rather than leaving the app in
                // pieces: half a stopped browser is worse than a running one.
                let _ = resume(&stopped);
                return Err(e);
            }
        }
    }
    if stopped.is_empty() {
        return Err(TreeError::Empty { root });
    }
    Ok(stopped)
}

/// `SIGCONT` a set of previously stopped processes, parents first.
///
/// Parents first is the mirror of `stop`, and matters less: a continued child
/// whose parent is still stopped simply runs. Errors are collected rather than
/// returned early, because a resume that gives up halfway strands the rest of
/// the app stopped — the one outcome this must never produce.
pub fn resume(pids: &[u32]) -> Result<(), TreeError> {
    let mut first_error = None;
    for pid in pids {
        match signal(*pid, Signal::CONT) {
            Ok(()) | Err(TreeError::Vanished { .. }) => {}
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn signal(pid: u32, sig: Signal) -> Result<(), TreeError> {
    let Some(p) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return Err(TreeError::Vanished { pid });
    };
    match rustix::process::kill_process(p, sig) {
        Ok(()) => Ok(()),
        Err(e) if e == rustix::io::Errno::SRCH => Err(TreeError::Vanished { pid }),
        Err(e) if e == rustix::io::Errno::PERM => Err(TreeError::Denied { pid }),
        Err(_) => Err(TreeError::Denied { pid }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn we_can_walk_our_own_tree() {
        let me = std::process::id();
        let tree = collect(me);
        assert_eq!(tree[0], me, "the root comes first");
    }

    #[test]
    fn our_own_parent_is_readable() {
        let me = std::process::id();
        let parent = ppid_of(me).expect("we can read our own ppid");
        assert!(parent > 0, "the test harness has a parent");
        assert_ne!(parent, me);
    }

    #[test]
    fn every_pid_whose_parent_is_outside_the_set_is_a_root() {
        // The bug: the unit was `pids.first()`, and since the daemon sorts PIDs
        // numerically that is the lowest number, which is the tree root only by
        // coincidence. Firefox was 44 processes in three trees and the lowest
        // number was a childless `Web Content` leaf, so 43 processes kept running
        // while the app was reported as dozing.
        let me = std::process::id();
        let parent = ppid_of(me).unwrap();

        // Ourselves alone: our parent is outside the set, so we are the root.
        assert_eq!(roots_of(&[me]), vec![me]);

        // With our parent included, only the parent is a root — we hang off it.
        let roots = roots_of(&[me, parent]);
        assert_eq!(roots, vec![parent], "a child must not be reported as a root");

        // Order must not decide it. This is the actual regression: the lowest
        // number is not the root.
        assert_eq!(roots_of(&[parent, me]), vec![parent]);
    }

    #[test]
    fn a_forest_keeps_every_root() {
        // What a browser actually presents: several independent trees under one
        // AppKey. Every one of them has to be walked, or the half that is not
        // walked keeps running while the app is reported as dozing.
        //
        // Deliberately no spawned child: these tests share a process, so a child
        // here would appear in another test's view of "our own tree".
        let me = std::process::id();
        let unreadable = [u32::MAX - 1, u32::MAX - 2];
        let roots = roots_of(&[me, unreadable[0], unreadable[1]]);

        assert_eq!(roots.len(), 3, "three independent roots: {roots:?}");
        for pid in [me, unreadable[0], unreadable[1]] {
            assert!(roots.contains(&pid), "{pid} missing from {roots:?}");
        }
    }

    #[test]
    fn our_own_uid_is_readable() {
        let me = std::process::id();
        assert_eq!(uid_of(me), Some(rustix::process::getuid().as_raw()));
        assert_eq!(uid_of(u32::MAX), None, "a dead pid has no uid");
    }

    #[test]
    fn a_dead_process_is_vanished_not_denied() {
        // The commonest real case, and it must not read as a failure.
        let err = signal(u32::MAX - 1, Signal::CONT).unwrap_err();
        assert!(err.is_vanished(), "{err}");
    }

    #[test]
    fn pid_one_is_not_ours_to_signal() {
        // init runs as root; the same-UID constraint must drop it before any
        // signal is sent.
        let allowed = signallable(&[1], &Guard::allow_all());
        assert!(allowed.is_empty(), "pid 1 must never be signallable by us");
    }

    #[test]
    fn the_never_freeze_list_is_applied_per_process_not_per_root() {
        // The tree walk reaches processes the caller never named — a shell's
        // children are whatever it launched — so filtering only the root would
        // let a protected process through.
        let me = std::process::id();
        let guard = Guard::for_current_process();
        let allowed = signallable(&[me], &guard);
        assert!(
            allowed.is_empty(),
            "our own process is on the never-freeze list and must be filtered"
        );
    }

    #[test]
    fn resuming_dead_processes_is_not_an_error() {
        // Panic thaw and shutdown both run this against a set that may have
        // exited; giving up halfway would strand the rest stopped.
        assert!(resume(&[u32::MAX - 1, u32::MAX - 2]).is_ok());
    }

    #[test]
    fn an_empty_tree_reads_as_vanished() {
        let err = stop(&[u32::MAX - 1], &Guard::allow_all()).unwrap_err();
        assert!(err.is_vanished(), "{err}");
    }
}
