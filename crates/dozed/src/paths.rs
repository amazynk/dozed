//! XDG locations (`DESIGN.md` §2).
//!
//! The definitions live in `dozed-core` so that `dozectl`, which reads the
//! traces and the journal this daemon writes, cannot derive a different path.
//! Core is not allowed to touch the filesystem, so the one thing here that does —
//! [`tighten_state`] — lives on this side of the line rather than beside the
//! definitions it uses.

use std::path::{Path, PathBuf};

pub use dozed_core::paths::config_file;

/// A state directory for the tests, so a test cannot write to the one a real
/// daemon is using.
///
/// This is not hygiene, it is a defect that was found the expensive way. The
/// daemon's own tests drive `drive_trial` and `stop_trial`, both of which
/// persist through these paths — and the paths resolve to `$XDG_STATE_HOME`,
/// which no test could redirect. Running `cargo test` on a developer's own
/// machine therefore **overwrote the live daemon's `trial.json` and
/// `frozen.json`**, observed doing exactly that: a trial four blocks in was
/// reset to zero on disk while the running daemon still held four in memory.
///
/// `frozen.json` is the one that matters. §10.3 promotes it from prudent to
/// mandatory — a freeze outlives the process that applied it, so that file is
/// what stands between a crash and an application frozen for good. A test
/// suite that can truncate it to `[]` while apps are held is a test suite that
/// can strand them.
///
/// It is the same shape as `packaging/test-install.sh` reaching past its XDG
/// sandbox into the live `systemd --user` (§15): the sandbox covered the paths
/// the test knew about and not the ones underneath it.
///
/// A `OnceLock` rather than `XDG_STATE_HOME`, because `std::env::set_var` is
/// `unsafe` under edition 2024 and this workspace forbids `unsafe_code` — and
/// because an environment variable is process-global anyway, which is the same
/// trap one level down.
#[cfg(test)]
static TEST_STATE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Point every state path at `dir` for the rest of this test binary.
///
/// First caller wins, so tests running in parallel agree rather than race.
#[cfg(test)]
pub fn redirect_state_for_tests(dir: &Path) {
    let _ = TEST_STATE_DIR.set(dir.to_path_buf());
}

/// Where state lives: the real XDG location, or a test's own tree.
fn state_root() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_STATE_DIR.get() {
        return dir.clone();
    }
    dozed_core::paths::state_dir()
}

/// The frozen set (`DESIGN.md` §10.3).
#[must_use]
pub fn frozen_file() -> PathBuf {
    state_root().join("frozen.json")
}

/// A self-trial in progress, so it survives a daemon restart.
#[must_use]
pub fn trial_file() -> PathBuf {
    state_root().join("trial.json")
}

/// Hand-set per-app ceilings.
#[must_use]
pub fn holds_file() -> PathBuf {
    state_root().join("holds.json")
}

/// When the weekly digest notification last went out.
#[must_use]
pub fn digest_file() -> PathBuf {
    state_root().join("digest")
}

/// Where the journal is appended.
#[must_use]
pub fn journal_file() -> PathBuf {
    state_root().join("journal.jsonl")
}

/// Where recorded sessions go, one file per daemon run.
#[must_use]
pub fn trace_dir() -> PathBuf {
    state_root().join("traces")
}

/// The state directory in force — the real XDG one, or a test's own tree.
///
/// Shadows the re-export the rest of the daemon used to get from core, so that
/// *everything* on this side of the line moves together under the redirect. A
/// sandbox that covers five of six paths is the shape of bug this exists to
/// stop.
#[must_use]
pub fn state_dir() -> PathBuf {
    state_root()
}

/// Replace a state file with new contents, or leave the old ones alone.
///
/// `std::fs::write` truncates and then writes, so it has a window in which the
/// file on disk is neither the old contents nor the new ones. For most files that
/// costs a rewrite; for `frozen.json` it costs an application staying frozen
/// forever, because that file exists **precisely** to be read after an
/// unscheduled death (`DESIGN.md` §10.3). A `SIGKILL` between the truncate and
/// the write left a torn file, `read_frozen` parsed it as nothing, `sweep_orphans`
/// found "no orphans", and nothing anywhere said otherwise. The mechanism for
/// surviving a kill was itself not safe against one.
///
/// So: write a sibling and rename over the target. `rename(2)` within a directory
/// is atomic, so any reader sees the whole of one version or the whole of the
/// other.
///
/// Deliberately **no `fsync`**, and that is not an oversight. Renaming is enough
/// for the failure this guards — a process dying, where the page cache outlives
/// it — and the harder case does not exist: a freeze does not survive a reboot,
/// so a record lost to a power cut describes nothing that is still frozen. Paying
/// two `fsync`s per freeze to protect a state that cannot occur would be exactly
/// the background work this daemon exists to remove (§13).
///
/// `0600` explicitly, like every other file under the state directory: between
/// them these are a record of which applications this user runs and when.
pub fn write_state_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    // A sibling, so the rename stays inside one filesystem. Named after the
    // target rather than randomly: only the daemon writes these, one at a time,
    // and a predictable leftover is one somebody can find and delete.
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = file.write_all(bytes).and_then(|()| file.flush());
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Take group and other off anything a previous run left readable.
///
/// The umask set in `main` keeps *new* files private, but a file created before
/// that was true stays exactly as it was created — and these files outlive the
/// process that wrote them by design. The journal spans installs, ten traces are
/// retained, and between them they are a record of which applications this user
/// runs and when. Measured on the machine this was written on, before the change:
/// `journal.jsonl` at `0664` and a megabyte long, `traces/` at `0775` holding 30 MB
/// of the same at full resolution, all of it readable by every account on the box.
///
/// Deliberately narrow, because a startup path that rewrites file modes is a
/// startup path that can do damage:
///
/// * **Only under `state_dir`.** The config file is left alone: it is the user's,
///   it holds rules rather than history, and somebody may have chosen its mode.
/// * **Never through a symlink.** Every stat is a `symlink_metadata`, so a link
///   planted here is skipped rather than followed — otherwise this would be a
///   chmod of whatever it pointed at, which is a gift to anyone who can write the
///   directory.
/// * **Owner bits preserved.** `mode & 0o700` removes other people's access
///   without deciding what the owner's should be.
/// * **One level down**, which is all the layout has (`traces/`).
/// * **Every failure ignored.** This is a tightening, not a precondition: a daemon
///   that refused to start because it could not chmod one file would have turned a
///   privacy fix into an outage.
pub fn tighten_state() {
    let dir = state_dir();
    let narrowed = tighten_at(&dir);
    if narrowed > 0 {
        tracing::info!(
            dir = %dir.display(),
            files = narrowed,
            "made recorded state readable only by you"
        );
    }
}

/// [`tighten_state`] against an explicit root, so it can be tested against a
/// fixture tree instead of the real `$XDG_STATE_HOME`.
///
/// Returns how many paths were changed, which is what makes "and it does nothing
/// the second time" a property a test can state.
fn tighten_at(dir: &Path) -> usize {
    let mut narrowed = 0;
    if let Some(meta) = private_dir(dir) {
        narrowed += usize::from(narrow(dir, &meta));
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                if meta.is_dir() {
                    narrowed += usize::from(narrow(&path, &meta));
                    narrowed += shallow(&path);
                } else if meta.is_file() {
                    narrowed += usize::from(narrow(&path, &meta));
                }
                // A symlink, a socket, a stray fifo: not ours to re-permission.
            }
        }
    }
    narrowed
}

/// The regular files one level inside `dir`, narrowed.
fn shallow(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            meta.is_file().then(|| usize::from(narrow(&path, &meta)))
        })
        .sum()
}

/// `Some` only for a real directory, never for a symlink to one.
fn private_dir(dir: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(dir).ok().filter(std::fs::Metadata::is_dir)
}

/// Clear group and other. Returns whether anything needed clearing.
fn narrow(path: &Path, meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return false; // already ours alone
    }
    let wanted = mode & 0o700;
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted)) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "could not narrow permissions");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn write(path: &Path, mode: u32) {
        std::fs::write(path, "x").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The state directory as this machine actually had it: everything `0664` and
    /// `0775`, world-readable in a world-traversable `$HOME`, and persistent.
    #[test]
    fn recorded_state_stops_being_readable_by_other_accounts() {
        let root = std::env::temp_dir().join(format!("dozed-tighten-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let traces = root.join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(&traces, std::fs::Permissions::from_mode(0o775)).unwrap();
        write(&root.join("journal.jsonl"), 0o664);
        write(&root.join("trial.json"), 0o644);
        write(&traces.join("1786265365.jsonl"), 0o664);
        // Already private, so it should not be counted as changed.
        write(&root.join("frozen.json"), 0o600);

        let narrowed = tighten_at(&root);

        assert_eq!(mode_of(&root), 0o700, "the directory itself");
        assert_eq!(mode_of(&traces), 0o700, "and the one below it");
        assert_eq!(mode_of(&root.join("journal.jsonl")), 0o600);
        assert_eq!(mode_of(&root.join("trial.json")), 0o600);
        assert_eq!(
            mode_of(&traces.join("1786265365.jsonl")),
            0o600,
            "traces too, not just the top"
        );
        assert_eq!(mode_of(&root.join("frozen.json")), 0o600, "left as it was");
        assert_eq!(narrowed, 5, "the five that needed it, not the one that did not");

        // Idempotent, which is what makes it safe on every start rather than once.
        assert_eq!(tighten_at(&root), 0, "nothing left to do the second time");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink in the state directory must not become a chmod of its target.
    /// Anyone who can write this directory could otherwise point it at a file of
    /// ours elsewhere and have the daemon re-permission it on the next start.
    #[test]
    fn a_symlink_is_skipped_rather_than_followed() {
        let root = std::env::temp_dir().join(format!("dozed-tighten-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let target = elsewhere.join("not-ours");
        write(&target, 0o666);
        std::os::unix::fs::symlink(&target, state.join("journal.jsonl")).unwrap();
        let dir_target = elsewhere.join("dir");
        std::fs::create_dir(&dir_target).unwrap();
        std::fs::set_permissions(&dir_target, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(&dir_target, state.join("traces")).unwrap();

        assert_eq!(tighten_at(&state), 1, "only the state directory itself");
        assert_eq!(mode_of(&target), 0o666, "the linked-to file is untouched");
        assert_eq!(mode_of(&dir_target), 0o777, "and so is the linked-to directory");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_state_directory_that_does_not_exist_yet_is_not_an_error() {
        let missing = std::env::temp_dir().join(format!("dozed-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(tighten_at(&missing), 0);
    }

    /// `frozen.json` exists to be read after an unscheduled death, so the write
    /// that produces it must survive one too. `fs::write` truncates first, and a
    /// `SIGKILL` in that window left a torn file that `read_frozen` parsed as
    /// "nothing was held" — the mechanism for surviving a kill was not itself
    /// safe against one.
    #[test]
    fn a_state_file_is_replaced_whole_or_not_at_all() {
        let root = std::env::temp_dir().join(format!("dozed-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("frozen.json");

        // The parent does not exist yet: first write of a fresh install.
        write_state_file(&path, br#"[{"app":"slack"}]"#).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), br#"[{"app":"slack"}]"#);
        assert_eq!(mode_of(&path), 0o600, "state files are owner-only");

        // Replacing it leaves no window and no litter.
        write_state_file(&path, b"[]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[]");
        assert!(!path.with_extension("tmp").exists(), "the sibling must not be left behind");

        // A path that cannot be written must leave the previous contents intact,
        // because a failed record is better than a destroyed one.
        write_state_file(&path, b"[1]").unwrap();
        let unwritable = root.join("nope").join("frozen.json");
        std::fs::write(root.join("nope"), "not a directory").unwrap();
        assert!(write_state_file(&unwritable, b"[2]").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"[1]", "the good file is untouched");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn everything_lives_under_the_state_dir() {
        // `state_dir()` rather than core's, so this keeps holding under the test
        // redirect — and so a path that failed to move with it is caught here.
        let state = state_dir();
        for p in
            [journal_file(), trace_dir(), frozen_file(), trial_file(), digest_file(), holds_file()]
        {
            assert!(p.starts_with(&state), "{} escaped {}", p.display(), state.display());
        }
    }
}
