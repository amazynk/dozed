//! The hard never-freeze list (`DESIGN.md` §10.1).
//!
//! These are not rules and not configuration. A rule engine that can be edited
//! into freezing `dbus-broker` is a rule engine that can wedge the desktop, so
//! this check sits below the rules and vetoes whatever they concluded.
//!
//! The guard is deliberately the *last* thing consulted, immediately before the
//! write, rather than a filter applied when apps are discovered: process sets
//! change, and a scope that held nothing dangerous a minute ago may hold
//! something now.

use std::collections::BTreeSet;
use std::fs;

use crate::cgroup::Cgroup;

/// Processes that must never be stopped, matched on `/proc/<pid>/comm`.
///
/// `comm` is truncated to 15 bytes by the kernel, so entries here are matched by
/// prefix — `xdg-desktop-por` is what a `xdg-desktop-portal` process actually
/// reports.
pub const NEVER_FREEZE: &[&str] = &[
    // Compositors and display servers. Freezing one of these ends the session.
    "kwin_wayland",
    "kwin_x11",
    "gnome-shell",
    "Xorg",
    "Xwayland",
    "plasmashell",
    "sway",
    "hyprland",
    // Init and IPC. A frozen bus blocks every caller for the 25s D-Bus timeout.
    "systemd",
    "dbus-daemon",
    "dbus-broker",
    "dbus-broker-lau",
    // Audio. Also the thing our own audio exemption depends on.
    "pipewire",
    "pipewire-pulse",
    "wireplumber",
    "pulseaudio",
    // Portals, authorisation, session management.
    "xdg-desktop-por",
    "xdg-document-po",
    "xdg-permission-",
    "polkitd",
    "polkit-kde-auth",
    "ksmserver",
    "gnome-session-b",
    "logind",
    "systemd-logind",
    // Ourselves. Freezing the daemon means nothing ever thaws.
    "dozed",
    "dozectl",
    "dozed-gui",
];

/// Vetoes actuation on anything that must not be touched.
#[derive(Debug, Clone, Default)]
pub struct Guard {
    /// Our own PID and every ancestor, resolved once at startup.
    protected_pids: BTreeSet<u32>,
    disabled: bool,
}

impl Guard {
    /// Build the guard for the current process, protecting it and its ancestors.
    #[must_use]
    pub fn for_current_process() -> Self {
        Self { protected_pids: ancestors_of(std::process::id()), disabled: false }
    }

    /// A guard that vetoes nothing. Tests only — never construct this in the
    /// daemon.
    #[must_use]
    pub fn allow_all() -> Self {
        Self { protected_pids: BTreeSet::new(), disabled: true }
    }

    #[must_use]
    pub fn protected_pids(&self) -> &BTreeSet<u32> {
        &self.protected_pids
    }

    /// Why this cgroup must not be touched, if it must not be.
    ///
    /// Returns `None` when actuation is permitted — which is why an unreadable
    /// `cgroup.procs` must not take the `?` path out of here. This is the last
    /// check between the rule engine and freezing `dbus-broker`, and it decides
    /// by *reading the process list*: with no list there is no check, so
    /// answering "permitted" is answering a question that was never asked. The
    /// safe direction for a veto is to veto.
    ///
    /// The one read failure that is not a fault is the cgroup having gone away,
    /// and that one still returns `None` deliberately — not to permit anything,
    /// but so the message the user sees comes from the actuator's own existence
    /// check, which says "no longer exists" rather than "could not read".
    /// `Cgroup::freeze` calls `ensure_alive` before it writes, so both paths end
    /// in `Skipped` either way.
    ///
    /// In practice a live cgroup we can write is one we can read — the two files
    /// sit in the same directory with the same owner — so this is a fail-safe for
    /// a case that should not arise, not a hot path.
    #[must_use]
    pub fn veto(&self, cg: &Cgroup) -> Option<String> {
        if self.disabled {
            return None;
        }
        let pids = match cg.procs() {
            Ok(pids) => pids,
            Err(e) if e.is_vanished() => return None,
            Err(e) => {
                return Some(format!(
                    "cannot read the process list of {}, so the never-freeze list cannot be \
                     checked against it: {e}",
                    cg.path().display()
                ));
            }
        };
        for pid in pids {
            if self.protected_pids.contains(&pid) {
                return Some(format!("cgroup holds pid {pid}, which is dozed or an ancestor"));
            }
            if let Some(comm) = comm_of(pid)
                && is_never_freeze(&comm)
            {
                return Some(format!("cgroup holds {comm} (pid {pid}), which is never freezable"));
            }
        }
        None
    }

    /// Whether one process, named by its `comm`, must be left alone.
    ///
    /// The per-PID counterpart to [`Guard::veto`], for the signal fallback,
    /// where there is no cgroup to check and the tree walk can reach processes
    /// the caller never named (`crate::tree`).
    #[must_use]
    pub fn forbids_comm(&self, comm: &str) -> bool {
        !self.disabled && is_never_freeze(comm)
    }

    /// Whether this PID is the daemon or one of its ancestors.
    #[must_use]
    pub fn forbids_pid(&self, pid: u32) -> bool {
        !self.disabled && self.protected_pids.contains(&pid)
    }
}

/// Whether a `/proc/<pid>/comm` value is on the never-freeze list.
#[must_use]
pub fn is_never_freeze(comm: &str) -> bool {
    NEVER_FREEZE.iter().any(|n| comm == *n || comm.starts_with(n))
}

/// `/proc/<pid>/comm`, trimmed. `None` if the process is gone.
#[must_use]
pub fn comm_of(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_owned())
}

fn ppid_of(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
        .filter(|p| *p != 0)
}

/// A PID and every ancestor up to init.
fn ancestors_of(pid: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    let mut cur = Some(pid);
    // Bounded so a malformed /proc cannot spin forever.
    for _ in 0..64 {
        let Some(p) = cur else { break };
        if !out.insert(p) {
            break;
        }
        cur = ppid_of(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_truncated_comm() {
        // The kernel truncates comm to 15 bytes, so the full name never appears.
        assert!(is_never_freeze("xdg-desktop-por"));
        assert!(is_never_freeze("kwin_wayland"));
        assert!(is_never_freeze("dbus-broker"));
        assert!(is_never_freeze("systemd"));
    }

    /// Every entry, by name, and a floor on how many there are.
    ///
    /// This list is the most load-bearing artefact in the crate: freezing
    /// anything on it ends a session, hangs the bus for the full 25s D-Bus
    /// timeout, or stops the daemon that would otherwise thaw everything. Four
    /// of its entries were pinned and twenty were not, so a typo or a bad merge
    /// in the other twenty was a session-ending bug no test would have caught —
    /// in a crate that is otherwise fastidious about pinning exactly this kind
    /// of invariant.
    ///
    /// Spelled out rather than iterated over `NEVER_FREEZE`, which would pass
    /// against an empty list. The count is a floor and not an equality so that
    /// adding a name is an ordinary change, while losing one is not.
    #[test]
    fn every_name_on_the_list_is_actually_protected() {
        // Compositors and display servers: freezing one ends the session.
        for comm in [
            "kwin_wayland",
            "kwin_x11",
            "gnome-shell",
            "Xorg",
            "Xwayland",
            "plasmashell",
            "sway",
            "hyprland",
        ] {
            assert!(is_never_freeze(comm), "{comm} must never be freezable");
        }
        // Init and IPC: a frozen bus blocks every caller for 25 seconds.
        for comm in ["systemd", "dbus-daemon", "dbus-broker", "dbus-broker-lau"] {
            assert!(is_never_freeze(comm), "{comm} must never be freezable");
        }
        // Audio, including the server our own audio exemption reads.
        for comm in ["pipewire", "pipewire-pulse", "wireplumber", "pulseaudio"] {
            assert!(is_never_freeze(comm), "{comm} must never be freezable");
        }
        // Portals, authorisation, session management.
        for comm in [
            "xdg-desktop-por",
            "xdg-document-po",
            "xdg-permission-",
            "polkitd",
            "polkit-kde-auth",
            "ksmserver",
            "gnome-session-b",
            "logind",
            "systemd-logind",
        ] {
            assert!(is_never_freeze(comm), "{comm} must never be freezable");
        }
        // Ourselves. Freezing the daemon means nothing ever thaws.
        for comm in ["dozed", "dozectl", "dozed-gui"] {
            assert!(is_never_freeze(comm), "{comm} must never be freezable");
        }

        assert!(
            NEVER_FREEZE.len() >= 24,
            "the list has lost entries: {} left. Adding is ordinary; removing one is a \
             session-ending change and should be deliberate enough to edit this number.",
            NEVER_FREEZE.len()
        );
    }

    /// The daemon's own processes, from the other direction.
    ///
    /// `dozed` is a prefix of `dozed-gui` and `dozectl` is not, so the two are
    /// covered by different halves of the match and both have to be checked.
    #[test]
    fn we_are_on_our_own_list_under_every_name_we_ship() {
        for comm in ["dozed", "dozed-gui", "dozectl"] {
            assert!(is_never_freeze(comm), "{comm} is one of ours");
        }
    }

    #[test]
    fn does_not_match_ordinary_apps() {
        for ok in ["slack", "firefox", "code", "signal-desktop", "wezterm-gui"] {
            assert!(!is_never_freeze(ok), "{ok} must be freezable");
        }
    }

    #[test]
    fn prefix_matching_is_deliberate_but_anchored() {
        // Prefix matching exists for truncation, so an unrelated name that
        // merely contains a protected word must still be freezable.
        assert!(!is_never_freeze("my-systemd-helper"));
        assert!(!is_never_freeze("not-pipewire"));
    }

    #[test]
    fn we_protect_ourselves_and_our_ancestors() {
        let g = Guard::for_current_process();
        let me = std::process::id();
        assert!(g.protected_pids().contains(&me), "the daemon must never freeze itself");
        assert!(g.protected_pids().len() > 1, "expected at least one ancestor");
    }

    #[test]
    fn allow_all_is_inert() {
        let g = Guard::allow_all();
        assert_eq!(g.veto(&Cgroup::new("/sys/fs/cgroup")), None);
    }

    #[test]
    fn a_cgroup_that_has_gone_away_is_not_vetoed() {
        // The app exited, which is routine (`DESIGN.md` §7). `None` here is not
        // permission to act: `Cgroup::freeze` calls `ensure_alive` before it
        // writes, so this still ends in Skipped — but with "no longer exists"
        // rather than "could not read", which is the true reason.
        let g = Guard::for_current_process();
        assert_eq!(g.veto(&Cgroup::new("/sys/fs/cgroup/definitely-not-here")), None);
    }

    /// A veto decides by reading the process list. With no list there is no
    /// check, and this is the last thing standing between the rule engine and
    /// freezing `dbus-broker` — so a read failure that is *not* the app exiting
    /// has to refuse rather than fall through to "permitted".
    #[test]
    fn a_cgroup_whose_process_list_cannot_be_read_is_refused() {
        let root = std::env::temp_dir().join(format!("dozed-veto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // A directory where the file should be: present, and unreadable for a
        // reason that is not "gone".
        std::fs::create_dir_all(root.join("cgroup.procs")).unwrap();

        let why = Guard::for_current_process()
            .veto(&Cgroup::new(&root))
            .expect("an unreadable process list must be refused, not permitted");
        assert!(why.contains("never-freeze list cannot be checked"), "{why}");

        // And a guard that vetoes nothing still vetoes nothing: this must not
        // become a way to break the test actuators.
        assert_eq!(Guard::allow_all().veto(&Cgroup::new(&root)), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
