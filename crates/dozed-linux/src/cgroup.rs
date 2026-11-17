//! cgroup v2: finding an app's scope and acting on it.
//!
//! Everything here was verified against a live Plasma 6.6.6 session on kernel
//! 7.1 (`DESIGN.md` §16 step 2). Three results shaped the code:
//!
//! * A frozen task reports `State: S (sleeping)` in `/proc`, exactly like any
//!   idle process. Frozen-ness is only observable through `cgroup.events`, so
//!   nothing here ever infers it from `/proc`.
//! * A freeze outlives the process that applied it. Killing the daemon does not
//!   thaw anything, which is why the frozen set must be persisted *before* the
//!   write and swept at startup (`DESIGN.md` §10.3).
//! * Scopes disappear under you constantly — an app exiting between the decision
//!   and the write is routine, not exceptional. That is a `Skipped`, never a
//!   `Failed`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the unified hierarchy is mounted. Configurable only so tests can point
/// at a fixture directory.
pub const DEFAULT_ROOT: &str = "/sys/fs/cgroup";

#[derive(Debug, thiserror::Error)]
pub enum CgroupError {
    #[error("cgroup {path} no longer exists")]
    Vanished { path: PathBuf },
    #[error("{what} is not available for {path}")]
    Unsupported { what: &'static str, path: PathBuf },
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl CgroupError {
    /// Whether this is the ordinary "the app exited" case rather than a fault.
    ///
    /// Callers turn this into [`dozed_core::Outcome::Skipped`]; everything else
    /// is a `Failed` worth showing the user.
    #[must_use]
    pub fn is_vanished(&self) -> bool {
        match self {
            Self::Vanished { .. } => true,
            // A scope can be torn down between the check and the write, so a
            // NotFound from the write itself means the same thing.
            Self::Read { source, .. } | Self::Write { source, .. } => {
                source.kind() == io::ErrorKind::NotFound
            }
            Self::Unsupported { .. } => false,
        }
    }
}

type Result<T> = std::result::Result<T, CgroupError>;

/// A handle to one cgroup directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolve a path relative to the hierarchy root, as it appears in
    /// `/proc/<pid>/cgroup`.
    #[must_use]
    pub fn at(root: &Path, relative: &str) -> Self {
        Self::new(root.join(relative.trim_start_matches('/')))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    fn read(&self, file: &str) -> Result<String> {
        let p = self.path.join(file);
        fs::read_to_string(&p).map_err(|source| CgroupError::Read { path: p, source })
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let p = self.path.join(file);
        fs::write(&p, value).map_err(|source| CgroupError::Write { path: p, source })
    }

    /// PIDs currently in this cgroup, not including descendant cgroups.
    pub fn procs(&self) -> Result<Vec<u32>> {
        Ok(self.read("cgroup.procs")?.lines().filter_map(|l| l.trim().parse().ok()).collect())
    }

    /// Whether the kernel reports this cgroup as frozen.
    ///
    /// Read from `cgroup.events`, not `cgroup.freeze`: the latter is what we
    /// *asked* for, the former is what actually happened. They differ while a
    /// freeze is still settling.
    pub fn is_frozen(&self) -> Result<bool> {
        for line in self.read("cgroup.events")?.lines() {
            if let Some(v) = line.strip_prefix("frozen ") {
                return Ok(v.trim() == "1");
            }
        }
        Ok(false)
    }

    pub fn supports_freeze(&self) -> bool {
        self.path.join("cgroup.freeze").is_file()
    }

    /// The `cpu` controller is only present in a cgroup when its parent enables
    /// it in `cgroup.subtree_control`. On a stock Plasma session `app.slice`
    /// does *not*, so app scopes have no `cpu.max` at all and the Throttle rung
    /// is simply unavailable.
    ///
    /// The fix is not to write `+cpu` to `app.slice` behind systemd's back but
    /// to ask systemd for a CPU property on the unit — `CPUQuota=5%` via
    /// `SetUnitProperties` — which makes it enable the controller itself.
    /// Verified: a busy loop then measured 49 ms CPU/s against a 5% cap.
    pub fn supports_throttle(&self) -> bool {
        self.path.join("cpu.max").is_file()
    }

    pub fn freeze(&self) -> Result<()> {
        self.ensure_alive()?;
        self.write("cgroup.freeze", "1")
    }

    pub fn thaw(&self) -> Result<()> {
        // Deliberately no `ensure_alive`: thawing something already gone is a
        // success, and the panic-thaw path must never refuse to try.
        match self.write("cgroup.freeze", "0") {
            Err(e) if e.is_vanished() => Ok(()),
            other => other,
        }
    }

    /// Cap the cgroup at `pct` percent of one core via `cpu.max`.
    pub fn throttle(&self, pct: u8) -> Result<()> {
        self.ensure_alive()?;
        if !self.supports_throttle() {
            return Err(CgroupError::Unsupported {
                what: "cpu.max (the cpu controller is not enabled on the parent slice)",
                path: self.path.clone(),
            });
        }
        const PERIOD_US: u32 = 100_000;
        let quota = u32::from(pct.clamp(1, 100)) * PERIOD_US / 100;
        self.write("cpu.max", &format!("{quota} {PERIOD_US}"))
    }

    /// Remove any CPU cap.
    pub fn clear_throttle(&self) -> Result<()> {
        if !self.supports_throttle() {
            return Ok(());
        }
        match self.write("cpu.max", "max 100000") {
            Err(e) if e.is_vanished() => Ok(()),
            other => other,
        }
    }

    /// Ask the kernel to push this cgroup's pages out (`DESIGN.md` §7).
    ///
    /// Works on a frozen cgroup — measured 3468K down to 2272K on a frozen
    /// shell — so freezing then reclaiming is a valid order.
    pub fn reclaim(&self, bytes: u64) -> Result<()> {
        self.ensure_alive()?;
        if !self.path.join("memory.reclaim").is_file() {
            return Err(CgroupError::Unsupported {
                what: "memory.reclaim",
                path: self.path.clone(),
            });
        }
        self.write("memory.reclaim", &bytes.to_string())
    }

    pub fn memory_current(&self) -> Result<u64> {
        Ok(self.read("memory.current")?.trim().parse().unwrap_or(0))
    }

    /// Cumulative CPU time in microseconds, from `cpu.stat`.
    ///
    /// Preferred over summing `/proc/<pid>/stat` across a process tree: one read
    /// instead of dozens, and it does not miss short-lived children
    /// (`DESIGN.md` §13).
    pub fn cpu_usage_usec(&self) -> Result<u64> {
        for line in self.read("cpu.stat")?.lines() {
            if let Some(v) = line.strip_prefix("usage_usec ") {
                return Ok(v.trim().parse().unwrap_or(0));
            }
        }
        Ok(0)
    }

    /// Whether every process in this cgroup belongs to `pids`.
    ///
    /// A cgroup is only a valid unit of action when it holds one app and
    /// nothing else. That is **not** guaranteed: an app launched from a
    /// terminal inherits the terminal's scope, so `app-…wezterm….service` can
    /// contain the terminal, the shell, and every GUI app started from it.
    /// Freezing that cgroup because one of those apps went idle would take the
    /// terminal down with it.
    ///
    /// Observed directly on this machine: two unrelated apps reported
    /// bit-identical CPU because both were reading their shared scope's
    /// `cpu.stat`.
    pub fn is_exclusive_to(&self, pids: &std::collections::BTreeSet<u32>) -> bool {
        match self.procs() {
            // An empty or unreadable cgroup is not something to act on either.
            Ok(procs) if !procs.is_empty() => procs.iter().all(|p| pids.contains(p)),
            _ => false,
        }
    }

    /// Whether this cgroup is the app's **whole** suspension unit: it holds
    /// nothing but `pids`, *and* it holds all of them.
    ///
    /// `is_exclusive_to` answers only the first half — "is there a stranger in
    /// here I would freeze by accident?" — and that is the right question for
    /// naming an app, where a scope that holds part of it still names it
    /// correctly. For actuation it is not enough, and the gap is not
    /// hypothetical. Observed on this machine: Brave launched from a wezterm
    /// window put its browser and audio processes in
    /// `app-org.chromium.Chromium-1450786.scope` and left the other eleven —
    /// GPU, network, storage, three zygotes, four renderers, the launcher shell
    /// — in the terminal's cgroup. The scope held no strangers, so it passed;
    /// freezing it stopped 2 of 13 processes and the app was reported as
    /// dozing while 246 wakeups/s carried on for seven hours. Same failure as
    /// the single-root bug in `tree.rs`, arriving down the cgroup path.
    ///
    /// A partial freeze is worse than none: the live renderers go on doing IPC
    /// against a frozen browser process, which is how Chromium hangs. When this
    /// returns false the caller drops to the process tree, which reaches every
    /// process wherever it lives.
    ///
    /// A PID that exited between the tree walk and this read has not escaped
    /// anywhere, so only the still-live ones are held against the cgroup;
    /// otherwise routine app churn would flap the unit back and forth.
    pub fn is_the_unit_for(&self, pids: &std::collections::BTreeSet<u32>) -> bool {
        let Ok(procs) = self.procs() else { return false };
        if procs.is_empty() {
            return false;
        }
        let here: std::collections::BTreeSet<u32> = procs.into_iter().collect();
        here.iter().all(|p| pids.contains(p))
            && pids.iter().all(|p| here.contains(p) || !is_alive(*p))
    }
}

/// Whether this *set* of cgroups is the app's suspension unit.
///
/// One cgroup is not enough to ask about, because an app is not reliably in
/// one. Restarted from its launcher, Brave put 32 processes in
/// `app-brave@….service` and its main process in a sibling
/// `app-org.chromium.Chromium-<pid>.scope`; Signal split 5 and 3; Firefox took
/// three `app-firefox@….service` units. Asked about the window PID's cgroup
/// alone, [`Cgroup::is_the_unit_for`] correctly answered "no" for every one of
/// them — 32 of Brave's processes were outside it — and all four fell to the
/// signal path, which cannot throttle at all.
///
/// Two conditions, and the second is not the one you would first write:
///
/// * **Nothing of ours escapes.** Every live PID of the app must be inside the
///   union, or a freeze would leave part of it running — the failure this rung
///   exists to prevent, and the one that let Brave report as dozing at 246
///   wakeups/s.
/// * **Nothing of anyone else's is inside.** Not "nothing outside the app's
///   process tree", which is what the single-cgroup test asked. That test
///   rejected Firefox and Thunderbird over one `bwrap` apiece — a sandbox
///   helper systemd had placed in the app's *own* unit, invisible to a tree
///   walk because its parent link does not appear in
///   `/proc/<pid>/task/<tid>/children`. A process sitting in the app's own unit
///   is the app, whatever parentage says; what must not be there is a process
///   belonging to a *different app we manage*, which is precisely the shared
///   terminal scope §4 is about. An editor launched from wezterm still fails
///   this, because wezterm's own PIDs are in that scope and wezterm is an app
///   we track.
///
/// The residue — a process in neither set — is tolerated. It is in a unit
/// systemd created for this app, and the never-freeze guard still runs over
/// every cgroup before anything is written.
#[must_use]
pub fn is_the_unit_for_app(
    cgroups: &[PathBuf],
    owned: &std::collections::BTreeSet<u32>,
    foreign: &std::collections::BTreeSet<u32>,
) -> bool {
    if cgroups.is_empty() {
        return false;
    }
    let mut union = std::collections::BTreeSet::new();
    for path in cgroups {
        let Ok(procs) = Cgroup::new(path).procs() else { return false };
        union.extend(procs);
    }
    if union.is_empty() {
        return false;
    }
    // Somebody else's process in here means acting on this app would act on
    // theirs: the shared-scope case, and non-negotiable.
    if union.iter().any(|p| foreign.contains(p)) {
        return false;
    }
    // A PID that exited between the walk and the read has not escaped anywhere.
    owned.iter().all(|p| union.contains(p) || !is_alive(*p))
}

impl Cgroup {
    /// Last-resort kill. Works even while frozen, which makes it a usable escape
    /// hatch if a thaw somehow cannot be issued.
    pub fn kill_all(&self) -> Result<()> {
        self.write("cgroup.kill", "1")
    }

    fn ensure_alive(&self) -> Result<()> {
        if self.exists() { Ok(()) } else { Err(CgroupError::Vanished { path: self.path.clone() }) }
    }
}

/// Whether this PID is a process that could still run.
///
/// **Zombies do not count**, and getting that wrong cost Firefox the cgroup
/// rung entirely. A zombie keeps its `/proc/<pid>` entry until the parent reaps
/// it but is removed from `cgroup.procs` immediately, so an existence check
/// reports a process that is in the app's PID set and in none of the app's
/// cgroups — indistinguishable, to the escape test above, from a renderer that
/// got away. Firefox holds three unreaped `bwrap` sandbox helpers permanently,
/// one per instance, so the union could never cover its tree and it fell to the
/// signal path for good, which is why it alone could not be throttled.
///
/// A zombie has no address space, cannot wake the CPU, and cannot be frozen or
/// signalled. It is exactly as absent as a reaped process for every purpose
/// this file has, so the question here is "could this still run", not "is there
/// a directory".
fn is_alive(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    // The comm field can contain spaces and parens, so the state is the first
    // token after the last ')'.
    stat.rfind(')')
        .and_then(|i| stat[i + 1..].split_whitespace().next())
        .is_some_and(|state| state != "Z")
}

/// The cgroup path recorded for a PID, e.g.
/// `/user.slice/user-1000.slice/user@1000.service/app.slice/app-signal-284229.scope`.
///
/// cgroup v2 puts a process in exactly one cgroup, on the `0::` line.
pub fn cgroup_path_of_pid(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_proc_cgroup(&text)
}

fn parse_proc_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
}

/// What a systemd scope or service name says about the app inside it.
///
/// This is `DESIGN.md` §4 rungs 1 and 2 arriving for free: systemd encodes the
/// launcher and the application id straight into the unit name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeIdentity {
    /// `flatpak`, `snap`, `gnome`, `kde`, … when the launcher is named.
    pub launcher: Option<String>,
    /// The application id: `com.slack.Slack`, `signal`, `firefox`.
    pub app: String,
    /// The launching PID systemd appended, which is *not* necessarily the PID
    /// still running.
    pub launch_pid: Option<u32>,
}

/// Parse an `app-*.scope` unit name.
///
/// Handles the two shapes systemd emits — `app-<launcher>-<app>-<pid>.scope`
/// and `app-<app>-<pid>.scope` — and rejects everything else, notably
/// `run-p<pid>-i<n>.scope`, which is a transient `systemd-run` unit and carries
/// no application identity at all.
#[must_use]
pub fn parse_app_scope(unit: &str) -> Option<ScopeIdentity> {
    let stem = unit.strip_suffix(".scope").or_else(|| unit.strip_suffix(".service"))?;
    let rest = stem.strip_prefix("app-")?;
    if rest.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = rest.split('-').map(unescape_unit).collect();

    // A trailing all-digits component is the launcher PID, not part of the name.
    let launch_pid = match parts.last().and_then(|p| p.parse::<u32>().ok()) {
        Some(pid) => {
            parts.pop();
            Some(pid)
        }
        None => None,
    };
    if parts.is_empty() {
        return None;
    }

    // Only treat the first component as a launcher when it is one we recognise.
    // Otherwise `app-signal-284229.scope` would yield launcher=signal and an
    // empty app.
    const LAUNCHERS: [&str; 5] = ["flatpak", "snap", "gnome", "kde", "plasma"];
    let launcher = if parts.len() > 1 && LAUNCHERS.contains(&parts[0].as_str()) {
        Some(parts.remove(0))
    } else {
        None
    };

    let app = parts.join("-");
    if app.is_empty() {
        return None;
    }
    Some(ScopeIdentity { launcher, app, launch_pid })
}

/// systemd escapes characters it cannot put in a unit name as `\xNN`.
///
/// Decoded **byte by byte** into a buffer and converted at the end, rather than
/// pushing each escape as a `char`. Both halves of that matter, and each was
/// wrong before:
///
/// * systemd escapes a non-ASCII name one `\xNN` per **UTF-8 byte**, so
///   `Café` arrives as `Caf\xc3\xa9`. Pushing `0xc3` and `0xa9` as separate
///   `char`s reinterprets them as Latin-1 and yields `CafÃ©` — an `AppKey` no
///   rule the user writes will ever match.
/// * The escape was recognised by slicing `&s[i + 2..i + 4]`, guarded only on
///   length. A literal `\x` followed by a multi-byte character puts that end
///   index inside the character, and slicing a `str` off a boundary **panics**:
///   `app-\x€foo-1.scope` took the daemon down from `on_windows`, which is on
///   the main loop. Indexing bytes cannot land off a boundary, so the shape of
///   the fix is what removes the panic rather than a length check bolted on.
///
/// A cgroup directory whose name systemd did not choose is not hypothetical: any
/// process in the user's own delegated subtree can create one.
fn unescape_unit(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1), hex_byte(bytes.get(i + 2), bytes.get(i + 3))) {
            (b'\\', Some(b'x'), Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    // Lossy rather than a failure: a name we cannot decode still has to resolve
    // to *some* stable key, and refusing the unit would drop the app back to the
    // signal path (§4) over a character in its name.
    String::from_utf8_lossy(&out).into_owned()
}

/// Two ASCII hex digits as the byte they spell, or `None` if they are not two
/// ASCII hex digits.
fn hex_byte(hi: Option<&u8>, lo: Option<&u8>) -> Option<u8> {
    let digit = |b: Option<&u8>| char::from(*b?).to_digit(16);
    Some((digit(hi)? * 16 + digit(lo)?) as u8)
}

/// Walk up from a process's cgroup to the unit that represents the whole app.
///
/// A browser's tab processes sit in the same scope as the browser, but a helper
/// launched with `systemd-run` gets its own; we want the nearest `.scope` or
/// `.service`, which is the boundary systemd itself considers one application.
#[must_use]
pub fn app_unit_of(cgroup_path: &str) -> Option<&str> {
    cgroup_path
        .rsplit('/')
        .find(|seg| seg.ends_with(".scope") || seg.ends_with(".service"))
        .filter(|seg| !seg.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_unified_line() {
        // v2-only, which is what any current desktop looks like.
        let text =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-signal-1.scope\n";
        assert_eq!(
            parse_proc_cgroup(text).unwrap(),
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-signal-1.scope"
        );
    }

    #[test]
    fn ignores_v1_controller_lines() {
        let text = "12:pids:/user.slice\n1:name=systemd:/user.slice\n0::/user.slice/app.scope\n";
        assert_eq!(parse_proc_cgroup(text).unwrap(), "/user.slice/app.scope");
    }

    #[test]
    fn empty_or_v1_only_gives_nothing() {
        assert_eq!(parse_proc_cgroup(""), None);
        assert_eq!(parse_proc_cgroup("1:name=systemd:/user.slice\n"), None);
        assert_eq!(parse_proc_cgroup("0::\n"), None);
    }

    #[test]
    fn parses_a_plain_app_scope() {
        // Observed on this machine.
        let id = parse_app_scope("app-signal-284229.scope").unwrap();
        assert_eq!(id.launcher, None);
        assert_eq!(id.app, "signal");
        assert_eq!(id.launch_pid, Some(284229));
    }

    #[test]
    fn parses_a_flatpak_scope() {
        let id = parse_app_scope("app-flatpak-com.slack.Slack-1234.scope").unwrap();
        assert_eq!(id.launcher.as_deref(), Some("flatpak"));
        assert_eq!(id.app, "com.slack.Slack");
        assert_eq!(id.launch_pid, Some(1234));
    }

    #[test]
    fn parses_a_desktop_launcher_scope() {
        let id = parse_app_scope("app-gnome-org.mozilla.firefox-4321.scope").unwrap();
        assert_eq!(id.launcher.as_deref(), Some("gnome"));
        assert_eq!(id.app, "org.mozilla.firefox");
    }

    #[test]
    fn keeps_dashes_inside_an_app_id() {
        // `signal-desktop` must not lose its dash, and must not be read as
        // launcher=signal.
        let id = parse_app_scope("app-signal-desktop-99.scope").unwrap();
        assert_eq!(id.launcher, None);
        assert_eq!(id.app, "signal-desktop");
        assert_eq!(id.launch_pid, Some(99));
    }

    #[test]
    fn unescapes_systemd_hex() {
        let id = parse_app_scope(r"app-my\x2dapp-7.scope").unwrap();
        assert_eq!(id.app, "my-app");
    }

    /// systemd escapes one `\xNN` per UTF-8 **byte**, so decoding each escape as
    /// a `char` reinterprets it as Latin-1: `Caf\xc3\xa9` came out `CafÃ©`, which
    /// is a key no rule the user writes can match.
    #[test]
    fn a_multibyte_name_reassembles_rather_than_becoming_latin_1() {
        let id = parse_app_scope(r"app-Caf\xc3\xa9-7.scope").unwrap();
        assert_eq!(id.app, "Café");
    }

    /// The escape used to be recognised by slicing `&s[i + 2..i + 4]`, guarded
    /// only on length — so a literal `\x` before a multi-byte character put that
    /// end index inside the character and slicing a `str` off a boundary panicked.
    /// This runs from `on_windows`, on the daemon's main loop, and a cgroup
    /// directory named by something other than systemd is not hypothetical: any
    /// process in the user's own delegated subtree can create one.
    #[test]
    fn an_escape_prefix_before_a_multibyte_char_does_not_panic() {
        for unit in [
            "app-\\x\u{20AC}foo-1.scope", // 3-byte char straddling the slice end
            "app-\\x\u{e9}-1.scope",      // 2-byte char
            r"app-\x-1.scope",            // nothing at all after the marker
            r"app-\xzz-1.scope",          // not hex
            r"app-trailing\x",            // truncated at the very end
        ] {
            // The assertion is that we return rather than unwind; what any of
            // these resolves *to* is not a promise worth making.
            let _ = parse_app_scope(unit);
        }
    }

    #[test]
    fn rejects_transient_run_scopes() {
        // `systemd-run` units carry no app identity; treating `p286389` as an
        // app name would create a junk AppKey for every helper we spawn.
        assert_eq!(parse_app_scope("run-p286389-i296613.scope"), None);
        assert_eq!(parse_app_scope("dozed-freeze-test.service"), None);
        assert_eq!(parse_app_scope("app-.scope"), None);
        assert_eq!(parse_app_scope("something-else"), None);
    }

    #[test]
    fn finds_the_nearest_unit_in_a_path() {
        assert_eq!(
            app_unit_of(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/app-signal-1.scope"
            ),
            Some("app-signal-1.scope")
        );
        // user@1000.service must not win over the app's own scope.
        assert_eq!(
            app_unit_of("/user.slice/user@1000.service/app.slice/foo.scope/child"),
            Some("foo.scope")
        );
        assert_eq!(app_unit_of("/user.slice"), None);
    }

    #[test]
    fn vanished_is_distinguished_from_broken() {
        let gone = CgroupError::Vanished { path: PathBuf::from("/x") };
        assert!(gone.is_vanished());

        let notfound = CgroupError::Write {
            path: PathBuf::from("/x"),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(notfound.is_vanished(), "a race with app exit is not a failure");

        let denied = CgroupError::Write {
            path: PathBuf::from("/x"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(!denied.is_vanished(), "a real error must not be hidden as Skipped");
    }

    /// Assert a "this cgroup *is* the unit" case, tolerating the suite racing
    /// us.
    ///
    /// These tests read the test process's own cgroup, and `cargo test` runs
    /// other tests in parallel in that same cgroup — several of which spawn
    /// child processes. A child appearing between capturing `cgroup.procs` and
    /// the check is a stranger by construction, so the positive direction is
    /// inherently racy against the suite. It flaked exactly once in a full run
    /// and passed five times in isolation, which is the signature.
    ///
    /// The negative cases need no such care: adding a foreign pid or an escaped
    /// one makes them false whatever else appears.
    fn eventually_the_unit(check: impl Fn() -> bool) -> bool {
        (0..20).any(|_| {
            if check() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        })
    }

    #[test]
    fn exclusivity_needs_every_pid_accounted_for() {
        // Uses our own cgroup, which certainly contains our own pid.
        let own = crate::cgroup_path_of_pid(std::process::id()).unwrap();
        let cg = Cgroup::at(Path::new(DEFAULT_ROOT), &own);
        let procs: std::collections::BTreeSet<u32> = cg.procs().unwrap().into_iter().collect();
        assert!(cg.is_exclusive_to(&procs), "the full set of its own pids is exclusive");

        let missing_one: std::collections::BTreeSet<u32> = procs.iter().skip(1).copied().collect();
        if !procs.is_empty() && procs.len() > 1 {
            assert!(!cg.is_exclusive_to(&missing_one), "a stranger in the cgroup disqualifies it");
        }
        assert!(
            !Cgroup::new("/sys/fs/cgroup/nope").is_exclusive_to(&procs),
            "an unreadable cgroup is never a valid unit"
        );
    }

    #[test]
    fn the_unit_must_hold_all_of_the_app_not_just_some_of_it() {
        // The Brave case: a scope holding part of an app passes the
        // no-strangers test and is still the wrong thing to freeze.
        let own = crate::cgroup_path_of_pid(std::process::id()).unwrap();
        let cg = Cgroup::at(Path::new(DEFAULT_ROOT), &own);
        let procs: std::collections::BTreeSet<u32> = cg.procs().unwrap().into_iter().collect();
        if procs.is_empty() {
            return; // no cgroup v2 here; the live tests cover this properly
        }

        assert!(
            eventually_the_unit(|| cg.is_the_unit_for(&cg.procs().unwrap().into_iter().collect())),
            "a cgroup holding exactly the app is its unit"
        );

        // pid 1 is alive and certainly not in our user session's cgroup, so as
        // part of the app it is a process the freeze would not reach.
        let mut escaped = procs.clone();
        escaped.insert(1);
        assert!(cg.is_exclusive_to(&escaped), "still no strangers inside — the old test passes");
        assert!(!cg.is_the_unit_for(&escaped), "but a live process of ours is outside it");

        // A pid that exited between the tree walk and the read has not escaped.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let mut with_dead = procs.clone();
        with_dead.insert(dead);
        assert!(
            eventually_the_unit(|| {
                let mut live: std::collections::BTreeSet<u32> =
                    cg.procs().unwrap().into_iter().collect();
                live.insert(dead);
                cg.is_the_unit_for(&live)
            }),
            "a reaped pid must not disqualify the cgroup"
        );

        // Nor must an *un*reaped one. A zombie keeps its /proc entry but leaves
        // `cgroup.procs` at once, so an existence check reads it as a process
        // that escaped the freeze — which cost Firefox the cgroup rung outright.
        let mut zombie = std::process::Command::new("true").spawn().unwrap();
        let z = zombie.id();
        // Deliberately not reaped until after the assertion.
        while std::fs::read_to_string(format!("/proc/{z}/stat"))
            .ok()
            .and_then(|s| {
                s.rfind(')').and_then(|i| s[i + 1..].split_whitespace().next().map(str::to_owned))
            })
            .is_none_or(|state| state != "Z")
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut with_zombie = procs.clone();
        with_zombie.insert(z);
        assert!(
            eventually_the_unit(|| {
                let mut live: std::collections::BTreeSet<u32> =
                    cg.procs().unwrap().into_iter().collect();
                live.insert(z);
                cg.is_the_unit_for(&live)
            }),
            "an unreaped zombie is not a process that escaped the freeze"
        );
        zombie.wait().unwrap();
    }

    #[test]
    fn a_set_of_cgroups_can_be_the_unit_when_no_single_one_is() {
        // Brave's shape: the app split across two sibling cgroups, so neither
        // covers it and the union does.
        let own_rel = crate::cgroup_path_of_pid(std::process::id()).unwrap();
        let own = PathBuf::from(DEFAULT_ROOT).join(own_rel.trim_start_matches('/'));
        let here: std::collections::BTreeSet<u32> =
            Cgroup::new(&own).procs().unwrap_or_default().into_iter().collect();
        if here.is_empty() {
            return; // no cgroup v2 here; the live tests cover this properly
        }
        let none = std::collections::BTreeSet::new();

        assert!(eventually_the_unit(|| {
            let live: std::collections::BTreeSet<u32> =
                Cgroup::new(&own).procs().unwrap_or_default().into_iter().collect();
            is_the_unit_for_app(std::slice::from_ref(&own), &live, &none)
        }));
        assert!(!is_the_unit_for_app(&[], &here, &none), "no cgroups is never a unit");

        // A live process of ours outside every listed cgroup: something would
        // keep running through the freeze, which is the whole failure mode.
        let mut escaped = here.clone();
        escaped.insert(1);
        assert!(!is_the_unit_for_app(std::slice::from_ref(&own), &escaped, &none));

        // Someone else's process inside it is the shared-terminal case, and is
        // refused even though nothing of ours escapes.
        let foreign: std::collections::BTreeSet<u32> = here.iter().copied().take(1).collect();
        assert!(
            !is_the_unit_for_app(std::slice::from_ref(&own), &here, &foreign),
            "a cgroup holding another app's process must never be actionable"
        );

        // A process in the cgroup that is in neither set — a sandbox helper the
        // tree walk cannot see — is tolerated. Systemd put it in this app's
        // unit, so it is this app.
        assert!(
            eventually_the_unit(|| {
                let live: std::collections::BTreeSet<u32> =
                    Cgroup::new(&own).procs().unwrap_or_default().into_iter().skip(1).collect();
                is_the_unit_for_app(std::slice::from_ref(&own), &live, &none)
            }),
            "an unaccounted process in the app's own unit must not disqualify it"
        );
    }

    #[test]
    fn at_joins_a_proc_relative_path() {
        let c = Cgroup::at(Path::new("/sys/fs/cgroup"), "/user.slice/app.scope");
        assert_eq!(c.path(), Path::new("/sys/fs/cgroup/user.slice/app.scope"));
    }
}
