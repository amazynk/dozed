//! One dashboard, however many times you click "Open dashboard".
//!
//! The tray's menu item spawns `dozed-gui` every time it is clicked, and every
//! click produced another window: five clicks, five dashboards, each with its
//! own poll loop against the daemon. That is a bug twice over — a stack of
//! identical windows is a mess to clean up, and §13's argument about not
//! becoming the problem applies to the dashboard most of all, since each extra
//! instance adds its own 2s poll and its own repaints to a daemon whose whole
//! job is to stop things waking the CPU.
//!
//! ## Why the second instance decides, not the tray
//!
//! The tray could have checked for a running dashboard and declined to spawn
//! one. That fixes the stack and leaves the click doing *nothing*, which is a
//! worse answer than the bug: somebody who clicks "Open dashboard" wants to be
//! looking at the dashboard, and a window buried behind a browser on another
//! workspace is not that. So the second instance starts, finds the first, asks
//! it to come forward, and exits before it has drawn anything.
//!
//! ## Why a socket rather than a lock file
//!
//! A lock file answers "is one running" and nothing else. The interesting half
//! is the message — *raise yourself* — and a socket carries it with no extra
//! machinery, no signal handler, and no pid to go stale between the check and
//! the kill. It also inherits the stale-cleanup dance
//! [`crate::client`] and the daemon's own socket already use: a file left by a
//! crashed instance refuses `connect`, and a refusal is the proof that it is
//! safe to remove.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One byte, so a truncated read cannot be mistaken for a message.
const RAISE: &[u8] = b"r";

/// Same reasoning as the daemon's control socket: `connect(2)` needs write
/// permission, so `0600` is "this user may talk to their own dashboard". The
/// message is only "come forward", but a socket another account can reach is a
/// socket another account can make flash your window at will.
const SOCKET_MODE: u32 = 0o600;

/// Where the running dashboard listens.
///
/// `$XDG_RUNTIME_DIR` for the same reason `ipc::socket_path` uses it: it is
/// per-user, `0700`, and cleared at logout, so a stale socket cannot outlive the
/// session that made it.
fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("dozed-gui.sock")
}

/// Set when another instance has asked us to come forward. Read once per frame.
///
/// A flag rather than a channel because the answer is idempotent: three clicks
/// while the window is already coming forward are one raise, and a queue would
/// make them three.
pub type RaiseFlag = Arc<AtomicBool>;

/// What starting up found.
pub enum Instance {
    /// We are the dashboard. Hold the listener for as long as the window lives.
    First(RaiseFlag),
    /// One was already running and has been asked to come forward. Exit.
    AlreadyRunning,
}

/// Claim the dashboard slot, or hand the job to whoever already has it.
///
/// `wake` is called when another instance asks us to raise, so the window can
/// repaint out of an idle wait — this dashboard deliberately does not run on a
/// frame clock (§13), so without it the raise would sit unread until the daemon
/// happened to say something.
pub fn acquire(wake: impl Fn() + Send + 'static) -> Instance {
    acquire_at(socket_path(), wake)
}

/// [`acquire`], against a stated path.
///
/// Split out for the tests, which cannot move `XDG_RUNTIME_DIR` — this crate
/// forbids `unsafe`, and `set_var` is unsafe in this edition for the good reason
/// that the environment is process-wide while tests are not.
fn acquire_at(path: PathBuf, wake: impl Fn() + Send + 'static) -> Instance {
    // Someone answering is someone already running. Nothing else to do.
    if let Ok(mut stream) = UnixStream::connect(&path) {
        // Best effort: if the write fails the other instance is going away, and
        // the right response is to take over rather than to insist. Falling
        // through to `bind` below would race with its cleanup, so this run is
        // simply lost — the next click gets a dashboard.
        // Quiet on success: the user clicked a menu item and got the window
        // they asked for, which needs no commentary on a stream nobody is
        // reading. A failure is worth a line, because the click will look like
        // it did nothing.
        if let Err(e) = stream.write_all(RAISE) {
            eprintln!("dozed-gui: a dashboard is running but did not answer ({e})");
        }
        return Instance::AlreadyRunning;
    }

    // Nobody answered. Either there is no socket, or one was left by an
    // instance that died — and a refused `connect` is what proves the second,
    // which is why the file is only removed after the attempt above.
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        // Without a listener there is no single-instance guarantee, and the
        // dashboard is still worth showing: this degrades to the old behaviour
        // rather than refusing to start.
        eprintln!("dozed-gui: could not claim {}; a second window is possible", path.display());
        return Instance::First(Arc::new(AtomicBool::new(false)));
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(SOCKET_MODE));

    let flag: RaiseFlag = Arc::new(AtomicBool::new(false));
    let listening = Arc::clone(&flag);
    // Detached, and never joined: the thread's whole life is this process's,
    // and the socket is unlinked by `Guard` on the way out.
    std::thread::Builder::new()
        .name("dozed-gui-raise".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut buf = [0u8; 1];
                let mut stream = stream;
                if stream.read_exact(&mut buf).is_ok() && buf == RAISE[..] {
                    listening.store(true, Ordering::Relaxed);
                    wake();
                }
            }
        })
        .ok();

    Instance::First(flag)
}

/// Unlinks the socket when the dashboard exits.
///
/// Without this the file outlives the process and the next launch pays a failed
/// `connect` to discover it is stale. Harmless, but the stale path is the one
/// that is hard to reason about, so it is worth not taking routinely.
pub struct Guard(PathBuf);

impl Guard {
    #[must_use]
    pub fn new() -> Self {
        Self(socket_path())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole contract, end to end: the first caller owns the slot, the
    /// second is told to go away, and the message reaches the first.
    #[test]
    fn the_second_instance_raises_the_first_and_stands_down() {
        let dir = std::env::temp_dir().join(format!("dozed-gui-si-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dozed-gui.sock");

        let woken = Arc::new(AtomicBool::new(false));
        let w = Arc::clone(&woken);
        let first = acquire_at(path.clone(), move || w.store(true, Ordering::Relaxed));
        let Instance::First(flag) = first else { panic!("the first caller must own the slot") };
        assert!(!flag.load(Ordering::Relaxed), "nothing has asked yet");

        let second = acquire_at(path.clone(), || {});
        assert!(
            matches!(second, Instance::AlreadyRunning),
            "the second must stand down rather than opening another window"
        );

        // The listener thread is doing the read, so give it a moment.
        for _ in 0..200 {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(flag.load(Ordering::Relaxed), "the running dashboard was asked to come forward");
        assert!(woken.load(Ordering::Relaxed), "and woken, since it does not poll on a clock");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A socket left by an instance that died must not lock the dashboard out.
    #[test]
    fn a_stale_socket_from_a_dead_instance_is_taken_over() {
        let dir = std::env::temp_dir().join(format!("dozed-gui-stale-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dozed-gui.sock");

        // A file where the socket goes, answering nothing — which is what a
        // crashed instance leaves behind.
        std::fs::write(&path, b"not a socket").unwrap();

        let got = acquire_at(path, || {});
        assert!(
            matches!(got, Instance::First(_)),
            "a refused connect proves it is stale, so we take the slot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
