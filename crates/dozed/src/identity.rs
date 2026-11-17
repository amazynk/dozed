//! Resolving a PID to a stable [`AppKey`] (`DESIGN.md` §4).
//!
//! No single source is sufficient, so this is a chain, and it records every
//! candidate it saw rather than only the winner. That is what lets `dozectl why`
//! say "matched com.slack.Slack by flatpak id; also saw exe=/app/bin/slack"
//! instead of leaving the user to guess why their rule did not fire.

use std::fs;
use std::path::PathBuf;

use dozed_core::{AppClass, AppKey, IdentityEvidence};
use dozed_linux::{app_unit_of, cgroup_path_of_pid, is_never_freeze, parse_app_scope};

/// What the compositor told us about a window, which is where the two best
/// identity rungs come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowHint {
    pub app_id: Option<String>,
    pub desktop_file: Option<String>,
}

/// Everything we could work out about one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub key: AppKey,
    pub evidence: IdentityEvidence,
    pub class: AppClass,
    /// Absolute path to the app's cgroup, when it lives in a scope of its own.
    pub cgroup: Option<PathBuf>,
    /// On the hard never-freeze list, or an ancestor of ours.
    pub protected: bool,
}

/// Executables whose background CPU is the point rather than a symptom
/// (`DESIGN.md` §9). Matched against `comm`, which the kernel truncates to 15
/// bytes — so every entry here is either short enough or spelled as its
/// truncation (`qemu-system-x86`, `gnome-terminal-`).
///
/// This is one of the three places in dozed where a **name** is the signal, and
/// it is a list rather than an observation for a reason worth stating: audio,
/// capture, screen sharing, inhibitors and bus ownership are all *watched*, so
/// no list of app names could improve them. Long-running work cannot be watched
/// the same way. "High background CPU" is precisely what this daemon is built to
/// suppress, so the only thing separating a compiler from a leaking Electron app
/// is knowing what a compiler is called.
///
/// Erring long is therefore correct here. A name missing from this list risks
/// §9's worst failure — "suspending a 20-minute build is the worst failure this
/// tool can have" — and a name wrongly on it costs some battery. Names generic
/// enough to catch an unrelated GUI (`python`, `java`, `convert`) are still left
/// out: those would exempt whole classes of ordinary app.
const LONG_RUNNING: &[&str] = &[
    // Compilers and build drivers.
    "cargo",
    "cargo-nextest",
    "rustc",
    "make",
    "ninja",
    "cmake",
    "meson",
    "scons",
    "bazel",
    "cc1",
    "cc1plus",
    "cc1obj",
    "gcc",
    "clang",
    "ld",
    "lld",
    "mold",
    "gold",
    "go",
    "zig",
    "javac",
    "gradle",
    "mvn",
    "sbt",
    "dotnet",
    "msbuild",
    "node",
    "webpack",
    "esbuild",
    "tsc",
    "vite",
    // Package managers, which hold locks and leave a half-configured system if
    // they are stopped partway.
    "npm",
    "pnpm",
    "yarn",
    "pip",
    "pip3",
    "nix",
    "nix-build",
    "nix-daemon",
    "apt",
    "apt-get",
    "dpkg",
    "rpm",
    "dnf",
    "yum",
    "zypper",
    "pacman",
    "emerge",
    "flatpak",
    "snapd",
    // Containers and virtual machines: freezing one stops a guest, which has no
    // idea it is being suspended and every reason to notice.
    "docker",
    "dockerd",
    "podman",
    "containerd",
    "crun",
    "runc",
    "libvirtd",
    "virtiofsd",
    "waydroid",
    "qemu",
    "qemu-system-x86",
    "qemu-system-aar",
    "VBoxHeadless",
    "VBoxSVC",
    "virt-manager",
    // Backup, sync and transfer. A half-written backup is worse than none.
    "rsync",
    "restic",
    "borg",
    "kopia",
    "duplicity",
    "duplicati",
    "unison",
    "syncthing",
    "rclone",
    "timeshift",
    "vorta",
    // Encode and compress, which are CPU-bound by definition.
    "ffmpeg",
    "avconv",
    "handbrake",
    "HandBrakeCLI",
    "x264",
    "x265",
    "mkvmerge",
    "blender",
    "bup",
    "tar",
    "zstd",
    "xz",
    "gzip",
    "bzip2",
    "pigz",
    "7z",
    "7za",
    "unzip",
    // Filesystem and disk work, where an interrupted write is the whole risk.
    "dd",
    "btrfs",
    "zfs",
    "mkfs",
    "fsck",
    "badblocks",
];

fn is_long_running(comm: &str) -> bool {
    LONG_RUNNING.contains(&comm)
}

/// Terminal emulators. A terminal is a window; the work is whatever is inside
/// it, and freezing the terminal freezes that too.
const TERMINALS: &[&str] = &[
    "wezterm-gui",
    "wezterm",
    "wezterm-mux-ser",
    "alacritty",
    "kitty",
    "konsole",
    "gnome-terminal-",
    "gnome-console",
    "kgx",
    "xterm",
    "foot",
    "footclient",
    "st",
    "urxvt",
    "rxvt",
    "Eterm",
    "roxterm",
    "termite",
    "terminator",
    "tilix",
    "ghostty",
    "contour",
    "rio",
    "xfce4-terminal",
    "mate-terminal",
    "lxterminal",
    "deepin-terminal",
    "qterminal",
    "sakura",
    "terminology",
    "blackbox",
    "cool-retro-term",
    "yakuake",
    "guake",
    "tilda",
    "hyper",
    // Multiplexers, for the same reason: the work is what is inside them, and
    // it outlives the window entirely.
    "tmux",
    "tmux: server",
    "screen",
    "zellij",
    "byobu",
    "dtach",
    "abduco",
];

/// Shells, which are transparent: a shell sitting at a prompt is not work.
const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "ash", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh",
    "pwsh", "osh", "busybox", "login",
];

fn is_terminal(comm: &str) -> bool {
    TERMINALS.contains(&comm)
}

fn is_shell(comm: &str) -> bool {
    SHELLS.contains(&comm)
}

/// `DESIGN.md` §9's long-job criteria, applied to the whole process tree.
///
/// An app is a process tree, never a PID (§4) — and the class was being decided
/// from the *window's* comm alone, which for a terminal is `wezterm-gui` and
/// never `cargo`. So a terminal running a compile came out `Normal`, i.e.
/// freezable, which is the failure §9 calls the worst this tool can have:
/// "Suspending a 20-minute build is the worst failure this tool can have."
///
/// Demonstrated rather than theorised: replaying a real night showed a hidden
/// wezterm — 4327 CPU-seconds of build inside it — being frozen at the moment
/// the screen locked.
///
/// Two of the three criteria are implemented here:
///
/// * **anything on the built-in long-job list**, now matched anywhere in the
///   tree rather than only at its root;
/// * **a terminal or shell with a live non-shell child.** Broad on purpose. It
///   means a terminal with anything running in it is left alone, including an
///   idle `vim`, and that is the right direction to be wrong in: the cost of
///   sparing it is a little battery, and the cost of freezing it is someone's
///   build. Non-negotiable §3 — fail towards "never touch this".
///
/// The third — "an app whose CPU ramp started while it was focused" — is not
/// implemented, because it needs per-app CPU history the sampler does not keep,
/// and the two above already cover the cases that break something.
fn tree_is_long_running(tree_comms: &[String]) -> bool {
    if tree_comms.iter().any(|c| is_long_running(c)) {
        return true;
    }
    let Some(root) = tree_comms.first() else { return false };
    if !is_terminal(root) && !is_shell(root) {
        return false;
    }
    tree_comms.iter().skip(1).any(|c| !is_shell(c) && !is_terminal(c))
}

/// Whether this scope holds this app and nothing else, and therefore names it.
///
/// Unreadable or absent resolves to `false`: an identity we cannot justify is
/// worse than falling through to `app_id`, `exe` or `comm`, all of which at
/// least describe the process in front of us.
fn scope_describes_this_app(tree: &[u32], cgroup: Option<&std::path::Path>) -> bool {
    let Some(path) = cgroup else { return false };
    let owned: std::collections::BTreeSet<u32> = tree.iter().copied().collect();
    dozed_linux::Cgroup::new(path).is_exclusive_to(&owned)
}

pub fn read_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_owned())
}

pub fn read_exe(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        // A deleted binary shows up as "/usr/bin/foo (deleted)"; the suffix
        // would otherwise fork one AppKey into two across an upgrade.
        .map(|s| s.trim_end_matches(" (deleted)").to_owned())
}

/// Flatpak sandboxes expose their id in `/proc/<pid>/root/.flatpak-info`, which
/// is readable from outside the sandbox for our own processes.
fn read_flatpak_info(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/root/.flatpak-info")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("name="))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// One window's claim on an identity, as far as canonicalisation cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub key: AppKey,
    /// `/proc/<pid>/exe` for the window's own process.
    pub exe: Option<String>,
    /// Every PID in that window's process tree.
    pub tree: std::collections::BTreeSet<u32>,
}

/// Fold identities that are really one application onto a single key.
///
/// The bug this exists for, seen in a real 3h38m session: one Firefox appeared
/// twice, as `desktop:firefox` (44 PIDs) and `app-id:firefox-bin` (14 PIDs),
/// because some of its windows set `app_id` to `firefox` — which resolves
/// against `firefox.desktop` — and others to `firefox-bin`, which matches no
/// desktop file at all. The two PID sets overlapped by 9 and neither contained
/// the other, so one process tree carried two independent grace clocks and two
/// independent decisions. On battery in live mode that is one half `SIGSTOP`ing
/// a subtree the other half has just decided should be running.
///
/// Two conditions, and both are load-bearing:
///
/// * **the same executable.** Without it, an app launched from a terminal folds
///   into the terminal — its PID is in the terminal's tree, so the trees
///   overlap. Observed in the same session: `org.gnome.Evince` shares a PID with
///   `org.wezfurlong.wezterm`. That is the §4 collapse, and it is the reason
///   this is not simply "overlapping trees are one app".
/// * **overlapping process trees.** Without it, everything sharing an
///   interpreter merges: `opensnitch_ui`'s exe is `/usr/bin/python3.14`, so exe
///   alone would collapse every Python app on the machine into one. It also
///   keeps two Firefox profiles apart, since separate instances share an
///   executable but never a tree.
///
/// Only an *inferred* identity is ever folded away ([`IdentityKind::is_declared`]):
/// two declared identities over one tree — Chrome PWAs, Firefox profiles — are
/// two apps on purpose, and merging them would undo that deliberately.
///
/// Returns only the keys that need rewriting, mapping each to the key that
/// should represent it. Ties are broken by `Ord` on [`AppKey`], which is
/// strongest-rung-then-lexicographic, so the result is reproducible — a replay
/// that picked a different winner would not be a replay.
#[must_use]
pub fn canonical_keys(claims: &[Claim]) -> std::collections::BTreeMap<AppKey, AppKey> {
    use std::collections::{BTreeMap, BTreeSet};

    // One entry per key: the union of its windows' trees, and the executable
    // they agree on. Windows of one key that disagree about the exe leave it
    // `None`, which simply means this key never merges.
    let mut apps: BTreeMap<AppKey, (Option<String>, BTreeSet<u32>, bool)> = BTreeMap::new();
    for claim in claims {
        let entry = apps
            .entry(claim.key.clone())
            .or_insert_with(|| (claim.exe.clone(), BTreeSet::new(), true));
        if entry.0 != claim.exe {
            entry.2 = false;
        }
        entry.1.extend(claim.tree.iter().copied());
    }

    let mut rewrites: BTreeMap<AppKey, AppKey> = BTreeMap::new();
    for (key, (exe, tree, exe_agreed)) in &apps {
        if key.kind.is_declared() {
            continue;
        }
        let Some(exe) = exe.as_deref().filter(|_| *exe_agreed) else { continue };

        let winner = apps
            .iter()
            .filter(|(other, _)| other.kind < key.kind)
            .filter(|(_, (other_exe, _, agreed))| *agreed && other_exe.as_deref() == Some(exe))
            .filter(|(_, (_, other_tree, _))| !tree.is_disjoint(other_tree))
            .map(|(other, _)| other)
            .min();
        if let Some(winner) = winner {
            rewrites.insert(key.clone(), winner.clone());
        }
    }

    // A chain (comm -> app-id -> desktop) has to land on the final key, or the
    // caller is left holding an identity that is itself about to be rewritten.
    // Every hop strictly strengthens the rung, so this cannot cycle.
    let resolved: BTreeMap<AppKey, AppKey> = rewrites
        .keys()
        .map(|key| {
            let mut target = &rewrites[key];
            while let Some(next) = rewrites.get(target) {
                target = next;
            }
            (key.clone(), target.clone())
        })
        .collect();
    resolved
}

/// Resolve one process, folding in whatever the compositor knew about it.
#[must_use]
pub fn resolve(
    pid: u32,
    hint: &WindowHint,
    own_pids: &std::collections::BTreeSet<u32>,
    tree: &[u32],
) -> Option<Resolved> {
    let comm = read_comm(pid)?;
    let exe = read_exe(pid);
    let cgroup_rel = cgroup_path_of_pid(pid);
    let unit = cgroup_rel.as_deref().and_then(app_unit_of);
    let scope = unit.and_then(parse_app_scope);

    let mut evidence = IdentityEvidence {
        comm: Some(comm.clone()),
        exe_path: exe.clone(),
        app_id: hint.app_id.clone().filter(|s| !s.is_empty()),
        desktop_id: hint.desktop_file.clone().filter(|s| !s.is_empty()),
        cgroup_scope: unit.map(str::to_owned),
        ..Default::default()
    };

    let cgroup = cgroup_rel.as_deref().filter(|_| unit.is_some()).map(|rel| {
        PathBuf::from(dozed_linux::cgroup::DEFAULT_ROOT).join(rel.trim_start_matches('/'))
    });

    // The scope name is the most trustworthy source we have *when the scope
    // belongs to this app*, because systemd recorded it at launch rather than
    // inferring it later. When it does not, it is actively wrong.
    //
    // §4 established that a scope is not automatically a per-app boundary: an
    // app launched from a terminal inherits the terminal's. That was applied to
    // actuation and missed here, and it showed up within half an hour of real
    // use — a program called `rex`, started from a wezterm window, was given the
    // AppKey `desktop:net.local.wezterm@5a146fdb…`. Three things wrong with
    // that: it is the terminal's name and not the app's; it carries a
    // per-instance hash, so a rule written against it breaks on the next
    // terminal launch, which defeats the whole point of a stable key; and every
    // *other* program launched from that terminal would collapse into the same
    // identity and share one freeze decision.
    //
    // So the scope only contributes a name when it holds this app's tree and
    // nothing else — the same test actuation already applies.
    if let Some(scope) = &scope
        && scope_describes_this_app(tree, cgroup.as_deref())
    {
        match scope.launcher.as_deref() {
            Some("flatpak") => evidence.flatpak_id = Some(scope.app.clone()),
            Some("snap") => evidence.snap_name = Some(scope.app.clone()),
            // app-gnome-… / app-kde-… carry a desktop file id, and a bare
            // app-<name>-<pid>.scope is one too.
            Some(_) | None => {
                evidence.desktop_id.get_or_insert_with(|| scope.app.clone());
            }
        }
    }
    if evidence.flatpak_id.is_none() {
        evidence.flatpak_id = read_flatpak_info(pid);
    }

    let key = evidence.resolve().unwrap_or_else(|| AppKey::comm(&comm));

    let protected = own_pids.contains(&pid) || is_never_freeze(&comm);
    // Read once, used for both the class and the scope check.
    let tree_comms: Vec<String> = tree.iter().filter_map(|p| read_comm(*p)).collect();
    let class = if protected {
        AppClass::System
    } else if tree_is_long_running(&tree_comms) {
        AppClass::LongRunningJob
    } else {
        AppClass::Normal
    };

    Some(Resolved { key, evidence, class, cgroup, protected })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn resolves_our_own_process() {
        let me = std::process::id();
        let r = resolve(me, &WindowHint::default(), &BTreeSet::new(), &[me]).unwrap();
        assert!(r.evidence.comm.is_some());
        assert!(r.evidence.exe_path.is_some(), "we should be able to read our own exe");
        assert!(r.cgroup.is_some(), "we run inside a systemd scope on this box");
        assert!(r.cgroup.unwrap().starts_with("/sys/fs/cgroup"));
    }

    #[test]
    fn our_own_pid_is_protected() {
        let me = std::process::id();
        let own: BTreeSet<u32> = [me].into_iter().collect();
        let r = resolve(me, &WindowHint::default(), &own, &[me]).unwrap();
        assert!(r.protected);
        assert_eq!(r.class, AppClass::System, "protected implies never actionable");
    }

    #[test]
    fn compositor_hints_outrank_comm() {
        let me = std::process::id();
        let hint = WindowHint {
            app_id: Some("org.wezfurlong.wezterm".into()),
            desktop_file: Some("org.wezfurlong.wezterm".into()),
        };
        let r = resolve(me, &hint, &BTreeSet::new(), &[me]).unwrap();
        // desktop id beats app_id beats exe beats comm.
        assert_eq!(r.evidence.desktop_id.as_deref(), Some("org.wezfurlong.wezterm"));
        assert!(
            matches!(
                r.key.kind,
                dozed_core::IdentityKind::DesktopId | dozed_core::IdentityKind::Flatpak
            ),
            "got {:?}",
            r.key
        );
    }

    #[test]
    fn a_dead_pid_resolves_to_nothing() {
        assert!(resolve(u32::MAX, &WindowHint::default(), &BTreeSet::new(), &[u32::MAX]).is_none());
    }

    #[test]
    fn build_tools_are_classified_as_long_jobs() {
        assert!(is_long_running("cargo"));
        assert!(is_long_running("docker"));
        assert!(!is_long_running("slack"));
        assert!(!is_long_running("firefox"));
    }

    fn comms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_terminal_running_a_build_is_not_freezable() {
        // §9: "Suspending a 20-minute build is the worst failure this tool can
        // have." The class was decided from the window's own comm, which for a
        // terminal is `wezterm-gui` and never `cargo`, so a terminal with a
        // compile inside it came out Normal. Replaying a real night showed
        // exactly that: a hidden wezterm with 4327 CPU-seconds of build in it,
        // frozen the moment the screen locked.
        assert!(tree_is_long_running(&comms(&["wezterm-gui", "bash", "cargo", "rustc"])));
        assert!(tree_is_long_running(&comms(&["konsole", "zsh", "make", "cc1plus"])));
        // And the list applies anywhere in the tree, not only at its root.
        assert!(tree_is_long_running(&comms(&["some-gui-app", "ffmpeg"])));
    }

    #[test]
    fn a_terminal_with_anything_running_in_it_is_left_alone() {
        // §9's first criterion, and deliberately broad: a terminal is a window,
        // and the work is whatever is inside it. Sparing an idle vim costs a
        // little battery; freezing someone's ssh session or training run does
        // not cost a little anything.
        assert!(tree_is_long_running(&comms(&["wezterm-gui", "bash", "vim"])));
        assert!(tree_is_long_running(&comms(&["foot", "fish", "ssh"])));
    }

    #[test]
    fn an_empty_terminal_is_still_freezable() {
        // A shell sitting at a prompt is not work, so this must not exempt
        // every terminal unconditionally — that would give away a real target.
        assert!(!tree_is_long_running(&comms(&["wezterm-gui", "bash"])));
        assert!(!tree_is_long_running(&comms(&["wezterm-gui", "bash", "zsh", "sh"])));
        assert!(!tree_is_long_running(&comms(&["wezterm-gui"])));
    }

    #[test]
    fn ordinary_apps_are_unaffected() {
        // The fix must not exempt the things dozed exists to freeze.
        assert!(!tree_is_long_running(&comms(&["firefox-bin", "firefox-bin", "Web Content"])));
        assert!(!tree_is_long_running(&comms(&["signal-desktop", "signal-desktop"])));
        assert!(!tree_is_long_running(&comms(&["brave", "brave", "brave"])));
        assert!(!tree_is_long_running(&[]));
    }

    #[test]
    fn a_shared_scope_does_not_name_the_app() {
        // The bug this guards, seen within half an hour of real use: a program
        // launched from a terminal inherits the terminal's systemd scope, and
        // was taking the terminal's scope name as its own identity.
        //
        // Our own test process runs inside a scope shared with cargo and the
        // shell that launched it, so it stands in for exactly that case.
        let me = std::process::id();
        let cgroup = cgroup_path_of_pid(me).map(|rel| {
            PathBuf::from(dozed_linux::cgroup::DEFAULT_ROOT).join(rel.trim_start_matches('/'))
        });
        if cgroup.as_deref().is_none_or(|p| !p.exists()) {
            return; // no cgroup v2 here; the live tests cover this properly
        }
        // Rooted at a leaf of the scope rather than its owner, the scope holds
        // processes outside our tree, so it must not be used as a name.
        assert!(
            !scope_describes_this_app(&[me], cgroup.as_deref()),
            "a scope shared with the launching shell must not name this process"
        );
    }

    #[test]
    fn an_app_without_an_exclusive_scope_falls_back_to_its_own_identity() {
        // What `rex` should have resolved to: its own app_id, not the
        // terminal's scope. A hint always outranks the scope anyway, but this
        // pins the case where the scope is the *only* other candidate.
        let me = std::process::id();
        let hint = WindowHint { app_id: Some("rex".into()), desktop_file: None };
        let r = resolve(me, &hint, &BTreeSet::new(), &[me]).unwrap();
        assert!(
            !r.key.value.contains("wezterm") && !r.key.value.contains('@'),
            "identity must not be a shared scope's instance name, got {}",
            r.key
        );
        assert!(
            r.evidence.desktop_id.is_none() || r.evidence.app_id.is_some(),
            "evidence: {:?}",
            r.evidence
        );
    }

    mod canonicalisation {
        use dozed_core::IdentityKind;

        use super::*;

        fn claim(key: AppKey, exe: &str, tree: &[u32]) -> Claim {
            Claim { key, exe: Some(exe.to_owned()), tree: tree.iter().copied().collect() }
        }

        fn app_id(v: &str) -> AppKey {
            AppKey::new(IdentityKind::AppId, v)
        }

        const FF: &str = "/home/user/firefox/firefox-bin";

        #[test]
        fn two_identities_for_one_firefox_become_one() {
            // The observed bug: some windows set app_id `firefox`, which resolves
            // against firefox.desktop, and others `firefox-bin`, which matches no
            // desktop file — so one process tree carried two grace clocks and two
            // decisions, overlapping by 9 PIDs with neither containing the other.
            let claims = vec![
                claim(AppKey::desktop("firefox"), FF, &[100, 101, 102, 103]),
                claim(app_id("firefox-bin"), FF, &[102, 103, 104]),
            ];
            let rewrites = canonical_keys(&claims);
            assert_eq!(rewrites.get(&app_id("firefox-bin")), Some(&AppKey::desktop("firefox")));
            assert_eq!(rewrites.len(), 1, "the declared identity is the survivor: {rewrites:?}");
        }

        #[test]
        fn an_app_launched_from_a_terminal_does_not_fold_into_it() {
            // The §4 collapse, and the reason this is not simply "overlapping
            // trees are one app". Observed in the same session: Evince shares a
            // PID with wezterm because it was started from it. Folding them would
            // give one freeze decision to a terminal and everything inside it.
            let claims = vec![
                claim(
                    AppKey::desktop("org.wezfurlong.wezterm"),
                    "/usr/bin/wezterm-gui",
                    &[200, 201, 202],
                ),
                claim(app_id("evince"), "/usr/bin/evince", &[202, 203]),
            ];
            assert!(
                canonical_keys(&claims).is_empty(),
                "different executables must never fold, however much their trees overlap"
            );
        }

        #[test]
        fn apps_sharing_an_interpreter_stay_apart() {
            // opensnitch_ui's exe really is /usr/bin/python3.14, so matching on
            // the executable alone would collapse every Python app into one.
            // Disjoint trees are what stop it.
            let py = "/usr/bin/python3.14";
            let claims = vec![
                claim(AppKey::desktop("opensnitch_ui"), py, &[300]),
                claim(app_id("some-other-python-app"), py, &[400, 401]),
            ];
            assert!(canonical_keys(&claims).is_empty());
        }

        #[test]
        fn two_declared_identities_over_one_tree_stay_apart() {
            // Chrome PWAs and separate Firefox profiles share an executable and a
            // process tree and are still two apps — that is what the desktop-id
            // rung is for. Only an inferred identity may be folded away.
            let claims = vec![
                claim(AppKey::desktop("chrome-gmail"), "/usr/bin/chrome", &[500, 501]),
                claim(AppKey::desktop("chrome-calendar"), "/usr/bin/chrome", &[500, 502]),
            ];
            assert!(canonical_keys(&claims).is_empty());
        }

        #[test]
        fn a_second_instance_with_its_own_tree_stays_apart() {
            // Same executable, same weak spelling, but a tree of its own: a
            // second profile or instance, not the same app.
            let claims = vec![
                claim(AppKey::desktop("firefox"), FF, &[100, 101]),
                claim(app_id("firefox-bin"), FF, &[900, 901]),
            ];
            assert!(canonical_keys(&claims).is_empty());
        }

        #[test]
        fn a_chain_of_inferred_identities_lands_on_the_strongest() {
            // comm -> app-id -> desktop must resolve all the way, or the caller
            // is handed a key that is itself about to be rewritten.
            let claims = vec![
                claim(AppKey::desktop("firefox"), FF, &[100, 101]),
                claim(app_id("firefox-bin"), FF, &[101, 102]),
                claim(AppKey::comm("firefox-bin"), FF, &[102, 103]),
            ];
            let rewrites = canonical_keys(&claims);
            assert_eq!(rewrites.get(&app_id("firefox-bin")), Some(&AppKey::desktop("firefox")));
            assert_eq!(
                rewrites.get(&AppKey::comm("firefox-bin")),
                Some(&AppKey::desktop("firefox")),
                "a two-hop chain must not stop at app-id: {rewrites:?}"
            );
        }

        #[test]
        fn the_winner_is_deterministic_when_two_could_claim_it() {
            // A replay that picked a different survivor would not be a replay, so
            // the tie-break is Ord on AppKey: strongest rung, then lexicographic.
            let claims = vec![
                claim(AppKey::desktop("b-app"), "/usr/bin/thing", &[600, 601]),
                claim(AppKey::desktop("a-app"), "/usr/bin/thing", &[600, 602]),
                claim(app_id("thing"), "/usr/bin/thing", &[600, 603]),
            ];
            assert_eq!(
                canonical_keys(&claims).get(&app_id("thing")),
                Some(&AppKey::desktop("a-app"))
            );
        }

        #[test]
        fn ambiguous_or_unreadable_executables_never_fold() {
            // Two windows filed under one key that disagree about the exe, and a
            // window whose exe could not be read at all. Both resolve to "do not
            // merge", which is the safe direction: a split app costs battery, a
            // wrongly merged one freezes something that should be running.
            let disagreeing = vec![
                claim(AppKey::desktop("firefox"), FF, &[100, 101]),
                claim(app_id("firefox-bin"), FF, &[101, 102]),
                claim(app_id("firefox-bin"), "/something/else", &[101, 102]),
            ];
            assert!(canonical_keys(&disagreeing).is_empty());

            let unreadable = vec![
                claim(AppKey::desktop("firefox"), FF, &[100, 101]),
                Claim { key: app_id("firefox-bin"), exe: None, tree: [101, 102].into() },
            ];
            assert!(canonical_keys(&unreadable).is_empty());
        }

        #[test]
        fn an_empty_snapshot_is_not_a_special_case() {
            assert!(canonical_keys(&[]).is_empty());
        }
    }

    #[test]
    fn deleted_suffix_is_stripped() {
        // An upgraded binary must not become a second app.
        let s = "/usr/bin/firefox (deleted)".trim_end_matches(" (deleted)");
        assert_eq!(s, "/usr/bin/firefox");
    }
}
