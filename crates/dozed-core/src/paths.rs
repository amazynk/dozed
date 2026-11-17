//! XDG locations (`DESIGN.md` §2).
//!
//! In core rather than in the daemon because `dozectl` reads traces the daemon
//! writes, and two crates deriving the same directory separately is how they end
//! up disagreeing. Reading the environment is not I/O; nothing here touches the
//! filesystem, which keeps the crate's no-I/O rule intact — the same reason
//! [`crate::ipc::socket_path`] already lives here.

use std::path::PathBuf;

fn xdg(var: &str, fallback: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    home.join(fallback)
}

#[must_use]
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("dozed")
}

#[must_use]
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

#[must_use]
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("dozed")
}

/// Where the journal is appended, one JSON object per line.
#[must_use]
pub fn journal_file() -> PathBuf {
    state_dir().join("journal.jsonl")
}

/// Where recorded sessions go, one file per daemon run (`DESIGN.md` §14).
#[must_use]
pub fn trace_dir() -> PathBuf {
    state_dir().join("traces")
}

/// When the weekly digest notification last went out.
///
/// On disk rather than in memory because a laptop restarts more often than once
/// a week, and a timer that resets on every login is a timer that never fires.
#[must_use]
pub fn digest_file() -> PathBuf {
    state_dir().join("digest")
}

/// A self-trial in progress, so it survives a daemon restart (`crate::trial`).
///
/// A trial is hours long and a restart in the middle of one is ordinary — a
/// package upgrade, a `systemctl --user restart`. Losing four hours of blocks to
/// that would mean nobody ever finishes one.
#[must_use]
pub fn trial_file() -> PathBuf {
    state_dir().join("trial.json")
}

/// The frozen set, written *before* each freeze.
///
/// A freeze outlives the process that applied it (measured; see
/// `dozed-linux::cgroup`), so without this file a `SIGKILL` would strand apps
/// frozen forever. `DESIGN.md` §10.3.
#[must_use]
pub fn frozen_file() -> PathBuf {
    state_dir().join("frozen.json")
}

/// Per-app ceilings set by hand (`crate::ceiling`).
///
/// Persisted, unlike the global snooze, and the difference is deliberate: a
/// snooze is measured in minutes and a restart during one is a rare accident,
/// while "cap this app until I say otherwise" is a standing instruction that a
/// restart has no business quietly revoking. Deadlines are stored as wall-clock
/// because a monotonic one means nothing to the next process.
pub fn holds_file() -> PathBuf {
    state_dir().join("holds.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_lives_under_the_state_dir() {
        let state = state_dir();
        for p in [journal_file(), trace_dir(), frozen_file(), trial_file()] {
            assert!(p.starts_with(&state), "{} escaped {}", p.display(), state.display());
        }
    }
}
