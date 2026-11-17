//! Throttling through systemd, because the cgroup route is not available.
//!
//! `app.slice` does not enable the `cpu` controller, so app scopes have no
//! `cpu.max` at all and writing to it fails — measured during the §16 spike, and
//! the reason the `Throttle` rung shipped returning `Skipped`. Writing `+cpu` to
//! `app.slice`'s `cgroup.subtree_control` behind systemd's back would work, but
//! it fights the unit manager: systemd owns that hierarchy and rewrites it. The
//! supported route is `SetUnitProperties(CPUQuota=)`, after which systemd
//! enables the controller itself. Measured: a busy loop against a 5% cap used
//! 49 ms CPU/s.
//!
//! ## Why this owns a thread
//!
//! [`crate::Actuator`] is synchronous, and deliberately so: it sits at the far
//! side of the dry-run seam and returns an [`Outcome`](dozed_core::Outcome) the
//! journal records immediately. A D-Bus round trip cannot happen on the daemon's
//! reactor without either making the whole actuation path async — which would
//! ripple through every call site for the benefit of one rung — or blocking a
//! tokio worker, which is worse.
//!
//! So the D-Bus client lives on one dedicated thread with its own
//! single-threaded runtime, and callers hand it a request and wait for a reply
//! with a timeout. Throttles are rare (once per app state change), so a thread
//! that is asleep almost always costs nothing, and the timeout means a wedged
//! systemd degrades to `Skipped` rather than hanging the daemon.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

/// How long to wait for systemd. Generous for a local call, and bounded so a
/// stuck unit manager cannot take the daemon with it.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// `CPUQuotaPerSecUSec` is microseconds of CPU per second of wall clock, so one
/// whole core is this.
const USEC_PER_SEC: u64 = 1_000_000;

/// systemd spells "no quota" as `USEC_INFINITY`.
const USEC_INFINITY: u64 = u64::MAX;

#[derive(Debug, thiserror::Error)]
pub enum SystemdError {
    #[error("cannot reach the systemd user manager: {0}")]
    Unavailable(String),
    #[error("systemd refused to set the quota on `{unit}`: {detail}")]
    Refused { unit: String, detail: String },
    #[error("systemd did not answer within {}s", CALL_TIMEOUT.as_secs())]
    Timeout,
    #[error("the systemd worker has stopped")]
    Gone,
}

impl SystemdError {
    /// Whether this is the unit having gone away, which is routine: an app
    /// exiting between the decision and the write happened twice in ten minutes
    /// of spiking (`DESIGN.md` §7).
    #[must_use]
    pub fn is_vanished(&self) -> bool {
        match self {
            Self::Refused { detail, .. } => {
                detail.contains("NoSuchUnit") || detail.contains("not loaded")
            }
            _ => false,
        }
    }
}

#[derive(Debug)]
struct Request {
    unit: String,
    /// `None` clears the quota.
    quota_usec: Option<u64>,
    reply: Sender<Result<(), SystemdError>>,
}

/// A handle to the systemd user manager.
pub struct SystemdUnits {
    tx: Sender<Request>,
}

impl std::fmt::Debug for SystemdUnits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SystemdUnits")
    }
}

impl SystemdUnits {
    /// Start the worker thread. Cheap: the connection is made lazily on the
    /// first call, so a session with no throttling rule never talks to systemd.
    #[must_use]
    pub fn spawn() -> Self {
        let (tx, rx) = channel::<Request>();
        std::thread::Builder::new()
            .name("dozed-systemd".to_owned())
            .spawn(move || worker(&rx))
            .expect("spawning the systemd worker thread");
        Self { tx }
    }

    /// Cap `unit` at `pct` percent of one core.
    pub fn set_quota(&self, unit: &str, pct: u8) -> Result<(), SystemdError> {
        let pct = u64::from(pct.clamp(1, 100));
        self.call(unit, Some(USEC_PER_SEC * pct / 100))
    }

    /// Remove any cap. Idempotent, because restore runs on paths that must never
    /// decline to try.
    pub fn clear_quota(&self, unit: &str) -> Result<(), SystemdError> {
        self.call(unit, None)
    }

    fn call(&self, unit: &str, quota_usec: Option<u64>) -> Result<(), SystemdError> {
        let (reply_tx, reply_rx) = channel();
        self.tx
            .send(Request { unit: unit.to_owned(), quota_usec, reply: reply_tx })
            .map_err(|_| SystemdError::Gone)?;
        match reply_rx.recv_timeout(CALL_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(SystemdError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(SystemdError::Gone),
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    /// `runtime = true` keeps the change out of `/etc`: a CPU cap we applied
    /// must not outlive the session, let alone a reboot.
    fn set_unit_properties(
        &self,
        name: &str,
        runtime: bool,
        properties: Vec<(String, zbus::zvariant::Value<'_>)>,
    ) -> zbus::Result<()>;
}

fn worker(rx: &Receiver<Request>) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "systemd worker could not start; Throttle unavailable");
            // Drain and refuse, so callers get an error rather than a timeout.
            while let Ok(req) = rx.recv() {
                let _ = req.reply.send(Err(SystemdError::Unavailable(e.to_string())));
            }
            return;
        }
    };

    // Connected on first use, then reused. A session that never throttles never
    // opens a second bus connection.
    let mut conn: Option<zbus::Connection> = None;

    while let Ok(req) = rx.recv() {
        let result = runtime.block_on(async {
            if conn.is_none() {
                conn = Some(
                    zbus::Connection::session()
                        .await
                        .map_err(|e| SystemdError::Unavailable(e.to_string()))?,
                );
            }
            let c = conn.as_ref().expect("just connected");
            let mgr = SystemdManagerProxy::new(c)
                .await
                .map_err(|e| SystemdError::Unavailable(e.to_string()))?;
            let usec = req.quota_usec.unwrap_or(USEC_INFINITY);
            mgr.set_unit_properties(
                &req.unit,
                true,
                vec![("CPUQuotaPerSecUSec".to_owned(), zbus::zvariant::Value::U64(usec))],
            )
            .await
            .map_err(|e| SystemdError::Refused { unit: req.unit.clone(), detail: e.to_string() })
        });

        if let Err(e) = &result {
            // Drop the connection on failure so the next call reconnects rather
            // than reusing a bus we may have lost.
            if matches!(e, SystemdError::Unavailable(_)) {
                conn = None;
            }
        }
        let _ = req.reply.send(result);
    }
}

/// The systemd unit name for a cgroup path.
///
/// The last path component of an app's cgroup *is* its unit name — systemd names
/// the directory after the unit — so this is a trim rather than a lookup. Only
/// `.scope` and `.service` are accepted: a slice is a container for other units
/// and capping one would throttle everything inside it.
#[must_use]
pub fn unit_of_cgroup(path: &std::path::Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    (name.ends_with(".scope") || name.ends_with(".service")).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_quota_is_microseconds_of_cpu_per_second() {
        // systemd's CPUQuota=5% is 50ms of CPU per wall second. The spike
        // measured 49 ms/s against exactly this setting.
        assert_eq!(USEC_PER_SEC * 5 / 100, 50_000);
        assert_eq!(USEC_PER_SEC * 100 / 100, USEC_PER_SEC, "100% is one whole core");
    }

    #[test]
    fn the_unit_name_is_the_last_path_component() {
        assert_eq!(
            unit_of_cgroup(Path::new(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/\
                 app-flatpak-com.slack.Slack-1234.scope"
            )),
            Some("app-flatpak-com.slack.Slack-1234.scope".to_owned())
        );
        assert_eq!(
            unit_of_cgroup(Path::new("/sys/fs/cgroup/user.slice/session-2.scope")),
            Some("session-2.scope".to_owned())
        );
    }

    #[test]
    fn a_slice_is_never_a_throttle_target() {
        // Capping app.slice would throttle every application at once.
        assert_eq!(unit_of_cgroup(Path::new("/sys/fs/cgroup/user.slice/app.slice")), None);
        assert_eq!(unit_of_cgroup(Path::new("/sys/fs/cgroup")), None);
    }

    #[test]
    fn a_vanished_unit_is_not_a_failure() {
        let gone = SystemdError::Refused {
            unit: "app-slack-1.scope".into(),
            detail: "org.freedesktop.systemd1.NoSuchUnit: Unit not loaded.".into(),
        };
        assert!(gone.is_vanished(), "an app exiting mid-write is routine");

        let real = SystemdError::Refused { unit: "x".into(), detail: "AccessDenied".into() };
        assert!(!real.is_vanished(), "a real error must not be hidden as Skipped");
        assert!(!SystemdError::Timeout.is_vanished());
    }
}
