//! The unix-socket server that `dozectl` and the GUI talk to.
//!
//! Newline-delimited JSON, because the GUI wants a firehose of journal events
//! and a socket is far simpler to stream and to test than D-Bus (`DESIGN.md` §12).

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use dozed_core::coverage::MachineTotals;
use dozed_core::ipc::{
    self, AppSummary, PendingChange, Request, Response, StatusReport, TrialReport, TrialSummary,
    WhyReport,
};
use dozed_core::trial::TrialPlan;
use dozed_core::{Event, JournalEntry, MonoTime};

use crate::daemon::Daemon;

/// A batch of requests from one client, with somewhere to put the answers.
///
/// A batch rather than a single request because clients send several at once
/// and the daemon was taking them apart again. The GUI's poll writes `Status`,
/// `ListApps` and `Report` back to back, so all three arrive in one packet and
/// one `epoll_wait` — but dispatching them line by line paid a separate mpsc
/// hop to the main loop and oneshot hop back for each, and on a multi-threaded
/// runtime every hop can land on a different worker and cost a futex wake.
///
/// Measured: the daemon's own cost went from 4.4 wakeups/s with no client to
/// 11.4 with the dashboard attached, for 1.5 requests a second. Answering a
/// packet in one round trip instead of three is most of that back.
pub struct Command {
    pub requests: Vec<Request>,
    pub reply: oneshot::Sender<Vec<Response>>,
}

/// The socket's permissions: this user, and nobody else.
///
/// `connect(2)` requires **write** permission on the socket file, so `0600` is
/// exactly "the owner may talk to their own daemon". The execute bit means nothing
/// on a socket and is not set.
///
/// It matters because of what this socket carries. `SetMode`, `Act` and `ThawAll`
/// let a caller freeze and thaw this user's applications at will, and `ListApps`
/// and `Journal` alone are a record of which applications somebody runs and when.
/// The default was whatever the umask allowed — `0775` on this machine, since the
/// shipped umask is `0002` — which is group-writable, and only `$XDG_RUNTIME_DIR`
/// being `0700` was keeping anyone out. That is the directory's guarantee, not
/// ours, and `ipc::socket_path` falls back to `/tmp` when `XDG_RUNTIME_DIR` is
/// unset, where there is no such guarantee at all.
const SOCKET_MODE: u32 = 0o600;

/// The umask held across [`bind`], so the socket is never even briefly reachable
/// by another account.
///
/// `0o077` rather than the `0o177` that would produce [`SOCKET_MODE`] outright.
/// A umask is **process-wide**, and this runs with tasks already spawned, so for
/// the few microseconds it is held it applies to whatever else is being created.
/// Clamping a file to `0600` is harmless. Clamping a *directory* to `0600` strips
/// its execute bit and makes it unusable — a fault, not a tightening — and that is
/// not a thing to do to a journal or a trace directory on the way past.
///
/// Found by two of the tests below binding at the same time: one of them created
/// its parent directory inside the other's umask window and then failed to bind
/// inside it with `EACCES`. The socket is narrowed the rest of the way afterwards,
/// and `0700` is no less private than `0600` in the meantime — the execute bit
/// means nothing on a socket, and group and other have nothing either way.
const BIND_UMASK: u32 = 0o077;

/// Most a client may have in flight without completing a line.
///
/// The reader accumulates until it sees a `\n`, so without a ceiling a client
/// that never sends one grows this buffer until the daemon is killed for it. The
/// socket is same-UID, so that is somebody breaking their own daemon rather than
/// an attack — but this is the process that must not die, and the whole point of
/// §10.3 is that a dead daemon leaves applications frozen.
///
/// Generous by three orders of magnitude: the largest request is a `Why` carrying
/// an app name, and the dashboard's whole three-request batch is a few hundred
/// bytes. Anything approaching this is a fault, not a big query.
const MAX_PENDING_REQUEST: usize = 64 * 1024;

/// Bind the socket, removing any stale one left by a crashed daemon.
pub fn bind(path: &std::path::Path) -> Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if path.exists() {
        // A leftover socket file is not a running daemon; only a successful
        // connect would prove that, and we are the daemon.
        let _ = std::fs::remove_file(path);
    }
    if let Some(parent) = path.parent() {
        // `0700` where we are the one creating it. Normally `$XDG_RUNTIME_DIR`
        // exists already and is already `0700`, so this changes nothing; it is
        // the `/tmp` fallback that would otherwise leave the socket in a
        // world-traversable directory.
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).ok();
    }

    // Created private rather than relaxed-then-tightened. Between `bind` and a
    // later `chmod` the socket is already listening at whatever the umask allowed,
    // and a window of a few microseconds is still a window on a machine where
    // another local account is the threat being considered (`BIND_UMASK`).
    let previous = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(BIND_UMASK));
    let bound = UnixListener::bind(path);
    rustix::process::umask(previous);
    let listener = bound.with_context(|| format!("binding {}", path.display()))?;

    // Then narrowed the rest of the way, and then *checked*. umask is honoured for
    // sockets on Linux, but the consequence of it silently not being honoured is a
    // control socket open to other accounts, and that is not a thing to assume our
    // way past. What is asserted is the property rather than the exact bits: no
    // access for group or other, whatever `SOCKET_MODE` is set to later.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE)).ok();
    let mode = std::fs::metadata(path)
        .with_context(|| format!("reading back {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "refusing to serve {}: it came out mode {mode:o}, which lets another account reach it. \
         This socket can freeze and thaw your applications and lists everything you run, so it \
         is not offered on terms we could not set.",
        path.display(),
    );
    Ok(listener)
}

/// Whether the far end of a connection is the user we are running as.
///
/// Belt and braces again, and this is the brace: [`SOCKET_MODE`] depends on the
/// filesystem honouring it and on the socket having landed where we think, while
/// `SO_PEERCRED` is filled in by the kernel at `connect(2)` time from the peer's
/// real credentials and cannot be shaped by anything the peer sends. It is the
/// mechanism that actually enforces the promise on the tin — *unprivileged, and
/// same-UID only.*
///
/// Root is refused along with everyone else. A root caller does not need this
/// socket to do anything it cannot already do, so admitting it would widen the
/// rule without widening what is possible.
fn same_user(stream: &UnixStream) -> bool {
    let ours = rustix::process::getuid().as_raw();
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == ours => true,
        Ok(cred) => {
            tracing::warn!(
                uid = cred.uid(),
                pid = ?cred.pid(),
                "refused an IPC connection from another user"
            );
            false
        }
        // No credentials, no way to establish that this is us. The requests this
        // socket carries are not ones to serve on a guess.
        Err(e) => {
            tracing::warn!(error = %e, "refused an IPC connection whose peer could not be identified");
            false
        }
    }
}

pub async fn serve(
    listener: UnixListener,
    commands: mpsc::Sender<Command>,
    journal: broadcast::Sender<JournalEntry>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Before a task is spawned for it, and before a byte is read: a
                // stranger's connection is not something to hand a reader.
                if !same_user(&stream) {
                    continue;
                }
                let commands = commands.clone();
                let journal = journal.subscribe();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, commands, journal).await {
                        tracing::debug!(error = %e, "client disconnected");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    commands: mpsc::Sender<Command>,
    mut journal: broadcast::Receiver<JournalEntry>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    // Read raw rather than line by line, so everything that arrived together
    // can be answered together. `lines()` hands back one at a time and hides
    // whether more are already sitting in the buffer.
    let mut buf = Vec::new();

    loop {
        let mut chunk = [0u8; 4096];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        // Checked after appending and before parsing, so the bound is on what is
        // held rather than on what one read delivered. Told, then dropped: a
        // client stuck mid-line cannot be talked round, and leaving it attached
        // is what turns one confused client into an unbounded allocation.
        if buf.len() > MAX_PENDING_REQUEST {
            let msg = Response::Error {
                message: format!(
                    "request exceeded {MAX_PENDING_REQUEST} bytes without a newline; \
                     closing the connection"
                ),
            };
            let _ = write.write_all(ipc::encode(&msg)?.as_bytes()).await;
            anyhow::bail!("client sent {} bytes with no complete line", buf.len());
        }

        // Complete lines only; a partial one stays for the next read.
        let mut requests = Vec::new();
        let mut follow = false;
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Request>(line.trim()) {
                Ok(r) => {
                    follow |= matches!(r, Request::Journal { follow: true, .. });
                    requests.push(r);
                }
                Err(e) => {
                    let msg = Response::Error { message: format!("bad request: {e}") };
                    write.write_all(ipc::encode(&msg)?.as_bytes()).await?;
                }
            }
        }
        if requests.is_empty() {
            continue;
        }

        let (tx, rx) = oneshot::channel();
        commands.send(Command { requests, reply: tx }).await.ok();
        let responses = rx.await.unwrap_or_else(|_| {
            vec![Response::Error { message: "daemon is shutting down".into() }]
        });
        // One write for the whole batch: three responses were three syscalls
        // and, to the client, three chances to be woken.
        let mut out = String::new();
        for r in responses {
            out.push_str(&ipc::encode(&r)?);
        }
        write.write_all(out.as_bytes()).await?;

        if follow {
            // Stream new entries until the client goes away.
            loop {
                match journal.recv().await {
                    Ok(entry) => {
                        let msg = Response::Entry(Box::new(entry));
                        if write.write_all(ipc::encode(&msg)?.as_bytes()).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let msg = Response::Error { message: format!("dropped {n} entries") };
                        write.write_all(ipc::encode(&msg)?.as_bytes()).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

/// Answer one request against daemon state. Runs on the main loop, so it must
/// not block.
pub fn handle(daemon: &mut Daemon, request: Request) -> Vec<Response> {
    match request {
        Request::Status => vec![Response::Status(Box::new(status(daemon)))],
        Request::ListApps { watching } => {
            // A client that keeps asking is asking for fresh samples and not only
            // for this reply, which the daemon can act on where it cannot act on
            // one question (`ipc::Request::ListApps`).
            if watching {
                daemon.note_watcher();
            }
            vec![Response::Apps { apps: list_apps(daemon), machine: machine_totals(daemon) }]
        }
        Request::Why { app, rules } => match why(daemon, &app, rules) {
            Some(w) => vec![Response::Why(Box::new(w))],
            None => vec![Response::Error { message: format!("no app matching `{app}`") }],
        },
        Request::Journal { since, follow: _, all } => {
            // `--since` is measured on the wall clock, not on `at`.
            //
            // `at` counts from the start of the run that wrote the entry, and
            // the journal outlives runs, so comparing a previous run's `at`
            // against this run's clock compares two different zeros. It also
            // floored to nothing whenever the daemon had been up for less than
            // the requested window: `--since 1m` on a 34-minute-old daemon
            // returned 115 entries reaching back to the previous day.
            let cutoff = since.and_then(|s| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                Some(now.saturating_sub(s.as_secs()))
            });
            // Oldest first, so `--all` reads as one chronological stream with
            // the run boundary in the middle rather than history appended.
            let scope = if all { daemon.backlog() } else { &[] }.iter().chain(daemon.entries());
            let mut out: Vec<Response> = scope
                .filter(|e| cutoff.is_none_or(|c| e.wall >= c))
                .map(|e| Response::Entry(Box::new(e.clone())))
                .collect();
            out.push(Response::EndOfStream);
            out
        }
        Request::SetMode { mode } => {
            if mode.acts() && !daemon.is_live() {
                return vec![Response::Error {
                    message: "daemon was started without --live; restart it to act".into(),
                }];
            }
            match daemon.set_mode_by_hand(mode) {
                Ok(()) => vec![Response::Ok],
                Err(message) => vec![Response::Error { message }],
            }
        }
        Request::Snooze { duration } => match duration {
            Some(d) => match daemon.snooze(d) {
                Ok(()) => vec![Response::Ok],
                Err(message) => vec![Response::Error { message }],
            },
            None if daemon.wake() => vec![Response::Ok],
            None => vec![Response::Error { message: "not snoozing".into() }],
        },
        Request::Act { app, action } => match daemon.find_app(&app) {
            Some(key) => {
                daemon.user_event(Event::UserAction { app: key.clone(), action });
                let outcome = daemon
                    .entries()
                    .last()
                    .filter(|e| e.app == key)
                    .map_or(dozed_core::Outcome::Applied, |e| e.outcome.clone());
                vec![Response::Acted { app: key, outcome }]
            }
            None => vec![Response::Error { message: format!("no app matching `{app}`") }],
        },
        Request::SetHold { app, ceiling, duration } => match daemon.find_app(app.as_str()) {
            Some(key) => {
                daemon.set_hold(&key, ceiling.to_owned(), duration.to_owned());
                vec![Response::Ok]
            }
            None => vec![Response::Error { message: format!("no app matching `{app}`") }],
        },
        Request::ThawAll => {
            daemon.user_event(Event::ThawAll);
            vec![Response::Ok]
        }
        Request::Report { since: _ } => vec![Response::Report(Box::new(daemon.report()))],
        Request::ReloadConfig => match crate::config::load(daemon.config_path()) {
            Ok((config, _)) => {
                daemon.reload_config(config);
                vec![Response::Ok]
            }
            Err(e) => vec![Response::Error { message: format!("{e:#}") }],
        },
        Request::StartTrial { pairs, block, seed, on_battery_only } => {
            let default = TrialPlan::default();
            let plan = TrialPlan {
                pairs: pairs.unwrap_or(default.pairs),
                block: block.unwrap_or(default.block),
                // A seed nobody chose comes from the wall clock, so two trials on
                // the same machine are not handed the same block order. Recorded
                // with the verdict, so it stays reproducible after the fact.
                seed: seed.unwrap_or_else(wall_seed),
                on_battery_only: on_battery_only.unwrap_or(default.on_battery_only),
                ..default
            };
            match daemon.start_trial(plan) {
                Ok(()) => vec![trial_response(daemon)],
                Err(message) => vec![Response::Error { message }],
            }
        }
        Request::StopTrial => match daemon.stop_trial() {
            Some(_) => vec![trial_response(daemon)],
            None => vec![Response::Error { message: "no trial is running".into() }],
        },
        Request::TrialStatus => vec![trial_response(daemon)],
    }
}

/// A seed for a trial the caller did not seed.
fn wall_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64)
}

fn trial_response(daemon: &Daemon) -> Response {
    Response::Trial {
        report: daemon.trial().map(|t| {
            Box::new(TrialReport {
                verdict: t.verdict(),
                blocks: t.blocks().to_vec(),
                waiting: t.waiting(),
                arm: t.current_arm().map(|a| a.as_str().to_owned()),
                battery_wh: daemon.battery_wh(),
            })
        }),
    }
}

fn status(daemon: &Daemon) -> StatusReport {
    let world = daemon.world();
    StatusReport {
        mode: daemon.effective_mode(),
        configured_mode: daemon.overridden_mode(),
        mode_driven_by_trial: daemon.mode_driven_by_trial(),
        trial_restores: daemon.trial_restores(),
        uptime: daemon.uptime(),
        now: daemon.mono_now(),
        power: world.power,
        desktop: world.desktop,
        focus_source_healthy: daemon.helper_healthy(),
        session_bus_healthy: daemon.session_bus_healthy(),
        compositor: daemon.compositor().map(str::to_owned),
        focus_source: daemon.session().map(|s| format!("{} ({}): {}", s.kind, s.desktop, s.why)),
        apps_known: world.apps.len(),
        apps_held: daemon.held_count(),
        snooze_remaining: daemon.snooze_remaining(),
        self_wakeups_per_sec: daemon.self_wakeups_per_sec(),
        bus_calls_per_sec: daemon.bus_calls_per_sec(),
        wakeup_counter: Some(crate::sampler::WakeupCounter::in_force().as_str().to_owned()),
        frozen_file_error: daemon.frozen_file_error().map(str::to_owned),
        config_path: daemon.config_path().map(|p| p.to_string_lossy().into_owned()),
        trace: daemon.trace_status(),
        exemption_sources: daemon.exemption_sources(),
        energy: daemon.energy_status(),
        thermal: daemon.thermal_status(),
        trial: daemon.trial().map(|t| TrialSummary {
            blocks_done: t.blocks().len(),
            blocks_total: t.plan.pairs as usize * 2,
            arm: t.current_arm().map(|a| a.as_str().to_owned()),
            waiting: t.waiting(),
        }),
    }
}

fn list_apps(daemon: &Daemon) -> Vec<AppSummary> {
    let world = daemon.world();
    let now = daemon.mono_now();
    world
        .apps
        .values()
        .map(|app| AppSummary {
            key: app.key.clone(),
            label: app.key.label().to_owned(),
            visibility: app.visibility,
            applied: daemon.planner().applied(&app.key),
            mode: world.config.mode_for(&app.key),
            sample: app.sample,
            sampled: daemon.has_sample(&app.key),
            pids: daemon.pids_of(&app.key).map_or(0, Vec::len),
            exempt: app.exemptions.iter().map(|e| e.as_str().to_owned()).collect(),
            // The same call `planner::decide` opens with, so the row and the
            // decision cannot disagree.
            never_actionable: app.is_protected(),
            held_off: daemon.planner().hold(&app.key, now).map(|h| h.summary(now)),
            audio_streams: daemon.audio_streams(&app.key),
            // Five is what fits a detail panel and is nearly always enough:
            // one process is usually responsible.
            top_processes: daemon.top_processes(&app.key, 5),
            pending: pending(daemon, &app.key, now),
        })
        .collect()
}

/// What the planner is about to do to one app, as a countdown.
///
/// The deadline is the planner's own — the one the daemon sleeps on — turned
/// into a remaining duration here, where the monotonic clock it is measured
/// against actually exists. A client cannot do that subtraction and must not
/// try (`ipc::PendingChange`).
fn pending(daemon: &Daemon, key: &dozed_core::AppKey, now: MonoTime) -> Option<PendingChange> {
    let waiting = daemon.planner().pending(key, daemon.mono_now())?;
    // The rule is looked up rather than sent along with its action, because the
    // table is the config the client may already be looking at, and a name it
    // cannot find is more honest than an action it cannot check.
    let rule = daemon.world().config.rules.iter().find(|r| r.name == waiting.rule)?;
    Some(PendingChange {
        // Saturating: a deadline that has just passed is a tick the daemon has
        // not got to yet, which is "any moment now" and not a negative wait.
        within: waiting.at.since(now),
        rule: rule.name.clone(),
        action: rule.action,
    })
}

/// The denominator for the app list: what the whole machine is doing.
///
/// Read from the same counters `status` prints, but reported field by field
/// rather than as an `Option<EnergyStatus>`: a machine with no battery and no
/// `cpuidle` still has `/proc/stat`, and the wakeup rate is the half of this
/// that matters most (`DESIGN.md` §11.1).
fn machine_totals(daemon: &Daemon) -> MachineTotals {
    let energy = daemon.energy_status();
    MachineTotals {
        wakeups_per_sec: energy.as_ref().and_then(|e| e.system_wakeups_per_sec),
        cpus: energy.as_ref().map_or(0, |e| e.cpus),
        sample_window: daemon.world().config.defaults.cpu_sample_window.get(),
        gpu_busy: daemon.gpu_busy(),
        // Built from the same `energy_status()` the wakeup denominator came
        // from, so a row, its estimate and its footer are all one reading of one
        // machine (`dozed_core::coverage::MachineTotals`).
        //
        // The fitted model first, where the machine has varied enough to give one
        // up: it needs no denominator, takes no share, and charges for wakeups at
        // a rate measured here rather than at one nobody chose. Falling back to
        // CPU-time share is a worse answer, not a wrong one, and it is labelled
        // differently wherever it is rendered.
        basis: daemon.power_model().fit().map(dozed_core::attribution::Basis::fitted).or_else(
            || {
                energy.as_ref().and_then(|e| {
                    dozed_core::attribution::Basis::of(
                        e.package_parts.core_w,
                        e.package_w,
                        e.machine_cores_busy,
                    )
                })
            },
        ),
    }
}

fn why(daemon: &Daemon, pattern: &str, rules: bool) -> Option<WhyReport> {
    let key = daemon.find_app(pattern)?;
    let explanation = daemon.planner().explain(daemon.world(), &key, daemon.mono_now())?;
    let app = daemon.world().app(&key)?;
    Some(WhyReport {
        label: key.label().to_owned(),
        key: key.clone(),
        visibility: app.visibility,
        applied: explanation.applied,
        mode: daemon.world().config.mode_for(&key),
        reason: explanation.decision.reason.clone(),
        grace: explanation.decision.grace.map(|g| g.to_string()),
        elapsed: explanation.decision.elapsed,
        evidence: explanation.evidence,
        // An app can own several sibling cgroups; `why` shows them all, because
        // "which cgroup is this app in" is exactly the question whose one-value
        // answer was wrong (`dozed_linux::is_the_unit_for_app`).
        cgroup: daemon.cgroup_of(&key).filter(|c| !c.is_empty()).map(|c| {
            c.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ")
        }),
        rules: if rules { explanation.rules } else { Vec::new() },
        top_processes: daemon.top_processes(&key, 6),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Serialises the tests that call [`bind`], because the umask it holds is
    /// process-wide and `cargo test` runs these on threads of one process. Two
    /// concurrent binds is not a case the daemon has — it binds one socket, once —
    /// so this is the test harness paying for its own parallelism rather than a
    /// constraint being hidden.
    ///
    /// Held across the `bind` call only, never across an `await`.
    static BINDING: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn socket_in(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dozed-sock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("dozed.sock")
    }

    /// `bind`, with the umask to itself.
    fn bind_serialised(path: &std::path::Path) -> Result<UnixListener> {
        let _guard = BINDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        bind(path)
    }

    /// The socket carries `SetMode`, `Act` and `ThawAll`, and its journal is a
    /// record of which applications somebody runs and when. It was created at
    /// whatever the umask allowed — `0775` under the shipped `0002` — and only the
    /// happenstance of `$XDG_RUNTIME_DIR` being `0700` kept other accounts out.
    #[tokio::test]
    async fn the_socket_is_owner_only_and_so_is_a_directory_we_created_for_it() {
        let path = socket_in("mode");
        let listener = bind_serialised(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        // Group and other must have no write bit in particular: `connect(2)` is a
        // write, so that bit alone is the whole difference between private and not.
        assert_eq!(mode & 0o022, 0, "nobody else may connect: {mode:o}");

        let dir = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir, 0o700, "the directory we made for it, too: {dir:o}");

        drop(listener);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The umask is process-wide, so leaving it clamped would quietly change the
    /// mode of every file the daemon wrote afterwards — the journal and the trace
    /// among them. Restrictive is the safe direction, but doing it by accident and
    /// out of sight is how a surprise gets shipped.
    #[tokio::test]
    async fn binding_puts_the_umask_back() {
        let path = socket_in("umask");
        let (listener, after) = {
            let _guard = BINDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o022));
            let listener = bind(&path).unwrap();
            (listener, rustix::process::umask(before))
        };
        assert_eq!(after.bits(), 0o022, "bind left the umask where it found it");

        drop(listener);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The reader accumulates until it sees a newline, so a client that never
    /// sends one is an unbounded allocation inside the one process that must not
    /// die — §10.3's whole premise is that a dead daemon leaves applications
    /// frozen. Same-UID, so this is somebody breaking their own daemon rather
    /// than an attack, which is exactly why a bound is cheaper than a policy.
    #[tokio::test]
    async fn a_client_that_never_completes_a_line_is_dropped() {
        // A socket pair rather than a real bind: `handle_client` wants a stream
        // and nothing else, and `bind` holds a *process-wide* umask that the rest
        // of the suite creates directories inside — the collision the `BINDING`
        // mutex above exists for. Not taking the umask at all is better than
        // taking it correctly.
        let (mut client, server_end) = UnixStream::pair().unwrap();
        let (commands, _held_open) = mpsc::channel(4);
        let (journal, _) = broadcast::channel(4);
        let server = tokio::spawn(handle_client(server_end, commands, journal.subscribe()));

        // No newline, ever. Stops early on error, because the point is that the
        // far end hangs up rather than reading this forever.
        let blob = vec![b'x'; 8 * 1024];
        let mut sent = 0usize;
        while sent < MAX_PENDING_REQUEST * 4 {
            if client.write_all(&blob).await.is_err() {
                break;
            }
            sent += blob.len();
        }

        let outcome = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the server must not sit reading forever")
            .expect("the task itself must not panic");
        assert!(outcome.is_err(), "a line that never ends must close the connection");
    }

    /// `SO_PEERCRED` is what actually enforces "same UID only": the mode depends on
    /// the filesystem honouring it and on the socket landing where we expect, and
    /// `ipc::socket_path` falls back to `/tmp` where nothing protects it.
    ///
    /// Only the accepting half is testable here — refusing another user's
    /// connection needs a second uid, which a unit test cannot conjure without
    /// privilege. What this pins is that the check does not reject *us*, which is
    /// the failure that would take the dashboard and `dozectl` down with it.
    #[tokio::test]
    async fn our_own_connection_is_the_one_that_is_let_in() {
        let path = socket_in("peercred");
        let listener = bind_serialised(&path).unwrap();

        let client = UnixStream::connect(&path).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        assert!(same_user(&accepted), "the daemon must talk to its own user");
        assert_eq!(
            accepted.peer_cred().unwrap().uid(),
            rustix::process::getuid().as_raw(),
            "and the kernel agrees about who that is"
        );

        drop(client);
        drop(listener);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
