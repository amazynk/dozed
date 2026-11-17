//! Talking to the daemon from a thread, so the UI never blocks on a socket.
//!
//! The protocol is newline-delimited JSON over a unix socket (`DESIGN.md` §12),
//! chosen partly because the GUI wants a firehose of journal events and a socket
//! is far simpler to stream than D-Bus. This is the client half.
//!
//! Blocking std sockets rather than tokio: there is nothing else on these
//! threads to yield to, and a runtime here would be machinery for its own sake.
//!
//! ## Two connections, deliberately
//!
//! A client that asks to follow the journal gets a stream and nothing else — the
//! daemon's handler enters a `recv` loop and stops reading requests from that
//! socket for as long as the follow lasts. Sharing one connection therefore
//! means either a live timeline or a refreshing status, never both. Found by
//! running the window: it said "connected" and showed no apps. Sockets are
//! cheap, so the timeline gets its own and everything else keeps a
//! request/response one.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dozed_core::coverage::MachineTotals;
use dozed_core::ipc::{self, AppSummary, Request, Response, StatusReport};
use dozed_core::{JournalEntry, Report};

/// How often to ask for status, the app list and the report.
///
/// The journal arrives pushed, so this only refreshes what has no event behind
/// it. Two seconds is responsive for a dashboard and is nothing next to what the
/// daemon saves.
const POLL: Duration = Duration::from_secs(2);

/// How long a failure stays in the footer before a working reply may clear it.
///
/// Both ends of this are a bug that happened. Clearing on the next reply alone
/// gives the message `POLL` — two seconds — which is not long enough to read one
/// sentence, let alone act on it. Never clearing is worse and is what shipped: a
/// refusal to go live sat in the footer insisting a trial was running for as
/// long as the connection lasted, while the user stopped the trial and watched
/// nothing change. An error is a report about a moment, and the moment ends.
const ERROR_DWELL: Duration = Duration::from_secs(12);

/// Anything the UI can be told to redraw with.
type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Everything the UI draws, refreshed by the client threads.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub status: Option<StatusReport>,
    pub apps: Vec<AppSummary>,
    /// What the machine as a whole is doing, so the app rows have a
    /// denominator. Arrives with the app list rather than being read off
    /// `status`, so the two halves of the comparison are from one reading.
    pub machine: MachineTotals,
    pub report: Option<Report>,
    /// Newest last, bounded — this is a window, not an archive.
    pub journal: Vec<JournalEntry>,
    pub connected: bool,
    pub error: Option<String>,
    /// When [`Snapshot::error`] was set, so a later success can retire it
    /// without retiring it before it has been read. See [`ERROR_DWELL`].
    error_at: Option<Instant>,
}

impl Snapshot {
    /// Keep the timeline bounded. `dozectl journal` is the archive, and the
    /// panel says so on hover — hence `pub(crate)`, so the tooltip quotes the
    /// bound rather than a number somebody typed twice.
    pub(crate) const MAX_JOURNAL: usize = 500;

    fn push_entry(&mut self, entry: JournalEntry) {
        self.journal.push(entry);
        if self.journal.len() > Self::MAX_JOURNAL {
            self.journal.drain(..self.journal.len() - Self::MAX_JOURNAL);
        }
    }
}

/// What the UI can ask the daemon to do.
#[derive(Debug, Clone)]
pub enum Command {
    Thaw(String),
    Freeze(String),
    Throttle(String, u8),
    /// Panic thaw: restore everything, ignore rules, ignore mode.
    ThawAll,
    SetMode(dozed_core::Mode),
    Reload,
    /// Stop acting for a while; `None` ends a snooze early.
    Snooze(Option<std::time::Duration>),
    /// Limit how far up the ladder one app may be taken; `None` clears it.
    SetHold(String, Option<dozed_core::Ceiling>, Option<std::time::Duration>),
}

impl Command {
    fn into_request(self) -> Request {
        match self {
            Self::Thaw(app) => Request::Act { app, action: dozed_core::Action::Restore },
            Self::Freeze(app) => {
                Request::Act { app, action: dozed_core::Action::Freeze { reclaim: false } }
            }
            Self::SetHold(app, ceiling, duration) => Request::SetHold { app, ceiling, duration },
            Self::Throttle(app, pct) => {
                Request::Act { app, action: dozed_core::Action::Throttle { pct } }
            }
            Self::ThawAll => Request::ThawAll,
            Self::SetMode(mode) => Request::SetMode { mode },
            Self::Reload => Request::ReloadConfig,
            Self::Snooze(duration) => Request::Snooze { duration },
        }
    }
}

pub struct Client {
    pub shared: Arc<Mutex<Snapshot>>,
    tx: Sender<Command>,
    /// Whether the window is on screen. Set by the UI thread each frame; the
    /// poll thread stops asking the daemon for data while it is false.
    visible: Arc<AtomicBool>,
}

impl Client {
    /// Tell the poll thread whether the dashboard is on screen.
    pub fn set_visible(&self, on_screen: bool) {
        self.visible.store(on_screen, Ordering::Relaxed);
    }

    /// Start the connection threads. Returns immediately; the UI reads `shared`
    /// and shows whatever is there, including "no daemon".
    pub fn spawn(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let (tx, rx) = channel();
        let repaint: Repaint = Arc::new(repaint);
        // Starts true so the first screenful arrives without waiting to be told
        // the window exists.
        let visible = Arc::new(AtomicBool::new(true));
        let poll_visible = Arc::clone(&visible);

        // Polling and commands.
        let poll_shared = Arc::clone(&shared);
        let poll_repaint = Arc::clone(&repaint);
        std::thread::Builder::new()
            .name("dozed-gui-poll".to_owned())
            .spawn(move || {
                reconnect_forever(&poll_shared, &poll_repaint, |s, r| {
                    poll(s, &rx, r, &poll_visible)
                })
            })
            .expect("spawning the poll thread");

        // The timeline, on its own connection.
        let follow_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("dozed-gui-follow".to_owned())
            .spawn(move || reconnect_forever(&follow_shared, &repaint, follow))
            .expect("spawning the follow thread");

        Self { shared, tx, visible }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

/// A daemon that is not running yet, or has just restarted, is an ordinary
/// state for a dashboard rather than a fatal one.
fn reconnect_forever(
    shared: &Arc<Mutex<Snapshot>>,
    repaint: &Repaint,
    mut session: impl FnMut(&Arc<Mutex<Snapshot>>, &Repaint) -> anyhow::Result<()>,
) {
    loop {
        if let Err(e) = session(shared, repaint) {
            if let Ok(mut s) = shared.lock() {
                s.connected = false;
                s.error = Some(e.to_string());
                s.error_at = Some(Instant::now());
            }
            repaint();
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Connect to the daemon, and establish that the daemon is *ours*.
///
/// The counterpart of `ipcserver::same_user`, which refuses a connection from
/// another UID before reading a byte. That check was only ever made in one
/// direction, and the reason it needs both is in `ipc::socket_path`: with
/// `$XDG_RUNTIME_DIR` unset the socket falls back to `/tmp`, where any account can
/// create `dozed.sock` before we do. A dashboard that does not look would draw
/// somebody else's numbers — held counts, app names, the whole timeline — under
/// this user's title bar.
///
/// `SO_PEERCRED` rather than a `stat` of the path, because the kernel fills it in
/// at `connect(2)` from the peer's real credentials and there is therefore no
/// window between checking and trusting. Via `rustix`, because `std`'s
/// `UnixStream::peer_cred` is still unstable.
///
/// A failure here surfaces the same way a missing daemon does — `reconnect_forever`
/// puts it in `Snapshot::error` and the window says so — which is the right
/// outcome: "something else is answering" belongs on screen, not in a log.
fn connect() -> anyhow::Result<UnixStream> {
    let path = ipc::socket_path();
    let stream = UnixStream::connect(&path)?;
    let ours = rustix::process::getuid().as_raw();
    match rustix::net::sockopt::socket_peercred(&stream) {
        Ok(cred) if cred.uid.as_raw() == ours => Ok(stream),
        Ok(cred) => anyhow::bail!(
            "{} is served by uid {}, not by you — this is not your dozed",
            path.display(),
            cred.uid.as_raw(),
        ),
        Err(e) => anyhow::bail!("cannot tell who is serving {}: {e}", path.display()),
    }
}

/// Journal entries, pushed.
fn follow(shared: &Arc<Mutex<Snapshot>>, repaint: &Repaint) -> anyhow::Result<()> {
    let stream = connect()?;
    let mut write = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    write.write_all(
        ipc::encode(&Request::Journal { since: None, follow: true, all: false })?.as_bytes(),
    )?;

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            anyhow::bail!("daemon closed the journal stream");
        }
        if let Ok(response) = serde_json::from_str::<Response>(line.trim()) {
            apply(shared, response);
            repaint();
        }
    }
}

/// Status, apps, report, and whatever the user asks for.
fn poll(
    shared: &Arc<Mutex<Snapshot>>,
    rx: &Receiver<Command>,
    repaint: &Repaint,
    visible: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let stream = connect()?;
    // Short enough that queued commands go out promptly, long enough that an
    // idle dashboard is not a busy loop.
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut write = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    if let Ok(mut s) = shared.lock() {
        s.connected = true;
        s.error = None;
        s.error_at = None;
    }
    repaint();

    // Ask once immediately: a dashboard that is blank for two seconds after
    // opening looks broken.
    let mut last_poll = Instant::now() - POLL;
    let mut line = String::new();
    loop {
        for cmd in rx.try_iter() {
            write.write_all(ipc::encode(&cmd.into_request())?.as_bytes())?;
        }
        // A dashboard nobody is looking at needs no data. Polling it anyway
        // costs the *daemon* — measured at ~7 wakeups/s on top of its own 4.4,
        // because every request is a hop to the main loop and back — which is
        // an awkward bill for a tool that exists to stop things waking the CPU.
        //
        // Commands are still sent above whatever the window is doing: a click
        // can only come from a visible window, and a queued one must not be
        // stranded by a race with minimising.
        if visible.load(Ordering::Relaxed) && last_poll.elapsed() >= POLL {
            last_poll = Instant::now();
            // Written in one go: three separate writes arrive as three packets
            // if the scheduler intervenes, and the daemon then wakes three
            // times to answer what is logically one question
            // (`dozed::ipcserver::Command`).
            let mut out = String::new();
            for r in [
                Request::Status,
                // `watching`, unconditionally, because reaching here means the
                // window is on screen — that is what the `visible` check above
                // is. It asks the daemon to keep the samples behind the table
                // fresh for as long as that stays true, and lapses by itself when
                // this loop stops asking.
                Request::ListApps { watching: true },
                Request::Report { since: None },
            ] {
                out.push_str(&ipc::encode(&r)?);
            }
            write.write_all(out.as_bytes())?;
        }

        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => anyhow::bail!("daemon closed the connection"),
            Ok(_) => {
                if let Ok(response) = serde_json::from_str::<Response>(line.trim()) {
                    apply(shared, response);
                    repaint();
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
    }
}

fn apply(shared: &Arc<Mutex<Snapshot>>, response: Response) {
    let Ok(mut s) = shared.lock() else { return };
    // A reply that is not an error means the daemon is answering normally, so a
    // failure the user has since dealt with stops being reported as if it were
    // still true — once it has had [`ERROR_DWELL`] on screen to be read.
    if !matches!(response, Response::Error { .. })
        && s.error_at.is_some_and(|at| at.elapsed() >= ERROR_DWELL)
    {
        s.error = None;
        s.error_at = None;
    }
    match response {
        Response::Status(status) => s.status = Some(*status),
        Response::Apps { apps, machine } => {
            s.apps = apps;
            s.machine = machine;
        }
        Response::Report(report) => s.report = Some(*report),
        Response::Entry(entry) => s.push_entry(*entry),
        Response::Error { message } => {
            s.error = Some(message);
            s.error_at = Some(Instant::now());
        }
        // The dashboard shows the trial through `Status` and `Report`, both of
        // which already carry it, so it never asks for the full block table.
        Response::Why(_)
        | Response::Ok
        | Response::EndOfStream
        | Response::Acted { .. }
        | Response::Trial { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dozed_core::{Action, AppKey, MonoTime, Outcome, Reason, ReasonKind};

    /// An error has to outlive the next poll, and must not outlive the problem.
    ///
    /// The shipped behaviour was the second half only: a refusal to go live sat
    /// in the footer saying a trial was running long after `dozectl trial stop`,
    /// because nothing but a reconnect ever cleared it.
    #[test]
    fn a_failure_is_readable_for_a_while_and_then_stops_being_reported() {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        apply(&shared, Response::Error { message: "a trial is driving the mode".into() });
        assert_eq!(shared.lock().unwrap().error.as_deref(), Some("a trial is driving the mode"));

        // The reply to the next two-second tick must not wipe it: that is not
        // reading time, and the user has to be able to see what they hit.
        apply(&shared, Response::Ok);
        assert!(shared.lock().unwrap().error.is_some(), "cleared before it could be read");

        // Once it has been up long enough, the next working reply retires it.
        shared.lock().unwrap().error_at = Some(Instant::now() - ERROR_DWELL);
        apply(&shared, Response::Ok);
        assert_eq!(shared.lock().unwrap().error, None, "a fixed problem is still being reported");
        assert_eq!(shared.lock().unwrap().error_at, None, "and the clock went with it");

        // A second failure while the first is showing replaces it and restarts
        // the clock, rather than inheriting an expiry it did not earn.
        apply(&shared, Response::Error { message: "first".into() });
        shared.lock().unwrap().error_at = Some(Instant::now() - ERROR_DWELL);
        apply(&shared, Response::Error { message: "second".into() });
        apply(&shared, Response::Ok);
        assert_eq!(shared.lock().unwrap().error.as_deref(), Some("second"));
    }

    fn entry(n: u64) -> JournalEntry {
        JournalEntry {
            at: MonoTime::from_millis(n),
            wall: n,
            app: AppKey::comm("slack"),
            action: Action::Restore,
            reason: Reason::new(ReasonKind::NoRule, "x"),
            outcome: Outcome::WouldHave,
        }
    }

    #[test]
    fn the_timeline_stays_bounded() {
        // A dashboard left open for a week must not become the archive.
        let mut s = Snapshot::default();
        for i in 0..(Snapshot::MAX_JOURNAL as u64 + 200) {
            s.push_entry(entry(i));
        }
        assert_eq!(s.journal.len(), Snapshot::MAX_JOURNAL);
        assert_eq!(
            s.journal.last().unwrap().at,
            MonoTime::from_millis(Snapshot::MAX_JOURNAL as u64 + 199),
            "and it keeps the newest, not the oldest"
        );
    }

    #[test]
    fn every_command_maps_to_a_request() {
        // The panic thaw especially: §10.5 requires it to work unconditionally.
        assert_eq!(Command::ThawAll.into_request(), Request::ThawAll);
        assert_eq!(
            Command::Throttle("slack".into(), 5).into_request(),
            Request::Act { app: "slack".into(), action: Action::Throttle { pct: 5 } }
        );
        assert_eq!(
            Command::Thaw("slack".into()).into_request(),
            Request::Act { app: "slack".into(), action: Action::Restore }
        );
    }
}
