//! The outside world, turned into [`Event`]s.
//!
//! Sources produce events and nothing else. That keeps the planner honest — it
//! cannot reach out and read a file mid-decision — and makes a recorded event
//! stream a complete description of a session (`DESIGN.md` §14).

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::mpsc;
use zbus::Connection;

use dozed_core::{DesktopState, Event, PowerState};

/// Everything the daemon's main loop consumes.
#[derive(Debug)]
pub enum Incoming {
    /// A domain event, ready for the planner.
    Event(Event),
    /// A window snapshot, still to be resolved into app identities and
    /// visibilities.
    ///
    /// Every focus source produces this same shape — the KWin script, the GNOME
    /// extension and the in-process X11 source alike — so §5's visibility rule
    /// has exactly one implementation, in `dozed_core::visibility`.
    WindowFacts(Vec<dozed_core::WindowFacts>),
    /// The helper said hello, or its heartbeat arrived.
    HelperAlive { compositor: Option<String> },
    /// A complete snapshot of the PIDs currently holding one exemption.
    ///
    /// PIDs rather than apps, because a source watching D-Bus has no idea how
    /// processes map to an `AppKey` — and a snapshot rather than a delta, so a
    /// source that restarts or misses a signal resynchronises on the next
    /// publish (`crate::exemptions`).
    Exemptions { exemption: dozed_core::Exemption, pids: std::collections::BTreeSet<u32> },
    /// What each process calls the audio streams it currently holds open.
    ///
    /// PIDs again rather than apps, and a complete snapshot again, for the same
    /// reasons as `Exemptions` — it rides beside that message and is attributed
    /// by the same tracker.
    ///
    /// It answers the question `audio-silent` provokes and cannot answer on its
    /// own: "firefox is holding a silent stream" is a fact about a process, and
    /// what the reader wants is *which tab*.
    AudioStreams { streams: std::collections::BTreeMap<u32, Vec<dozed_core::ipc::AudioStream>> },
    /// What one exemption source cannot see, and why.
    ///
    /// The rule this exists to enforce: **anything the daemon cannot do, it says
    /// so where a user looks** — `dozectl status` and the dashboard — rather than
    /// in a log line nobody reads. It applies to every external utility the
    /// daemon runs (today `pactl` and `parec`, and that list should stay short)
    /// and to every interface it depends on. A source that is quietly blind is
    /// indistinguishable from a source that is quietly working, and §10's
    /// original sin was exactly that: three exemption rules that could never
    /// fire, with nothing anywhere saying so.
    ///
    /// `unavailable` means the source cannot report at all; `degraded` means it
    /// reports less than it should. Both are `None` to clear a previous report,
    /// because a utility that starts working again should stop being complained
    /// about.
    SourceHealth {
        exemption: dozed_core::Exemption,
        unavailable: Option<String>,
        degraded: Option<String>,
    },
    /// Whether our own session-bus connection is still carrying traffic.
    ///
    /// Answers a question the daemon cannot answer for itself and used to get
    /// wrong: a silent focus source means either the compositor helper went away
    /// *or* we stopped being able to hear it, and those want different words and
    /// different remedies. Not a domain [`Event`], deliberately — it says something
    /// about dozed's own plumbing rather than about the machine, so it stays out of
    /// the world state and out of the trace the planner is replayed against.
    SessionBusHealth { healthy: bool },
    /// A launcher is activating the app that owns some bus name, by PID.
    ///
    /// We must thaw on this and not merely on focus: the call goes into the
    /// frozen process, so without it the launcher blocks for the 25s D-Bus
    /// timeout instead of raising a window (`crate::activation`).
    Activation { pid: u32 },
}

/// One window, exactly as a compositor helper reports it on the wire.
///
/// Deliberately close to [`dozed_core::WindowFacts`] but not the same type: the
/// wire form has to stay tolerant of both protocol versions, and `visibility`
/// arrives as a free-form string that only this layer should have to parse.
#[derive(Debug, Clone, Deserialize)]
pub struct WindowReport {
    /// Signed on the wire on purpose. KWin reports `pid: -1` for a window whose
    /// process it cannot name — some XWayland and internal surfaces — and with
    /// this typed as `u32` that one value failed the *whole* array: 241 snapshots
    /// were discarded in a single day, each one leaving the daemon's view of
    /// every window stale until the next. Worse, the helper sorts by PID, so a
    /// `-1` sorts first and takes the rest of the desktop down with it.
    pub pid: i64,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub desktop_file: String,
    /// Protocol 1: the helper's own verdict, computed in the compositor.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Protocol 2: the raw facts, so §5's rule runs once in Rust instead of
    /// once per platform in three languages.
    #[serde(default)]
    pub focused: Option<bool>,
    #[serde(default)]
    pub minimized: bool,
    #[serde(default = "yes")]
    pub on_current_desktop: bool,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub w: Option<i32>,
    #[serde(default)]
    pub h: Option<i32>,
    #[serde(default)]
    pub stack: u32,
    /// Which monitor the window is on. Windows on different outputs never
    /// occlude each other, whatever their coordinates say.
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub attention: bool,
    #[serde(default)]
    pub translucent: bool,
}

const fn yes() -> bool {
    true
}

impl WindowReport {
    /// Convert to the platform-neutral form the planner path consumes.
    ///
    /// `None` when the window has no usable PID. Nothing can be done with such a
    /// window: identity, class and sampling all start from `/proc/<pid>`, so it
    /// cannot be attributed to an app at all. Dropping the window rather than the
    /// snapshot also errs in the right direction — an unattributable window is
    /// then absent from the occlusion check, which can only make an app behind it
    /// look *more* visible, and §3 says fail towards "never touch this".
    pub fn into_facts(self) -> Option<dozed_core::WindowFacts> {
        let pid = u32::try_from(self.pid).ok().filter(|p| *p != 0)?;
        let parsed = self.visibility.as_deref().map(parse_visibility);
        let geometry = match (self.x, self.y, self.w, self.h) {
            (Some(x), Some(y), Some(w), Some(h)) => Some(dozed_core::Rect::new(x, y, w, h)),
            _ => None,
        };
        Some(dozed_core::WindowFacts {
            pid,
            app_id: self.app_id,
            desktop_file: self.desktop_file,
            // A protocol-1 helper reports focus only inside its visibility
            // string; a protocol-2 one says so directly.
            focused: self.focused.unwrap_or(parsed == Some(dozed_core::Visibility::Focused)),
            minimized: self.minimized,
            on_current_desktop: self.on_current_desktop,
            geometry,
            stack: self.stack,
            output: self.output,
            fullscreen: self.fullscreen,
            attention: self.attention,
            translucent: self.translucent,
            visibility: parsed,
        })
    }
}

/// Parse the helper's visibility vocabulary.
///
/// A word we do not recognise means a helper newer than us, or a corrupt
/// payload. Treating it as `Hidden` would make the app freezable on the strength
/// of a string we could not read, so it becomes the one state that is never
/// actionable.
fn parse_visibility(s: &str) -> dozed_core::Visibility {
    use dozed_core::Visibility;
    match s {
        "focused" => Visibility::Focused,
        "visible-unfocused" => Visibility::VisibleUnfocused,
        "hidden" => Visibility::Hidden,
        other => {
            tracing::warn!(visibility = %other, "unknown visibility; treating as focused");
            Visibility::Focused
        }
    }
}

#[derive(Debug, Deserialize)]
struct WindowsPayload {
    #[allow(dead_code)]
    seq: u64,
    windows: Vec<WindowReport>,
}

/// The D-Bus interface the compositor helper calls into.
///
/// Note the direction: **we** own the name and serve this, because a KWin script
/// can only call D-Bus, never register a service. See `helpers/kwin/README.md`.
pub struct FocusSink {
    tx: mpsc::Sender<Incoming>,
    /// Identifies this run of the daemon. Returned from every heartbeat so the
    /// helper can notice a restart it would otherwise sleep through: a daemon
    /// that restarts quickly reclaims the bus name before three heartbeats go
    /// unanswered, so the helper keeps believing it is talking to the same
    /// instance and its payload dedupe suppresses the republish forever.
    instance: String,
}

/// Handshake payload. A JSON string rather than typed D-Bus arguments — see
/// the note on numeric marshalling below.
#[derive(Debug, Deserialize)]
struct HelloPayload {
    #[allow(dead_code)]
    protocol: u32,
    compositor: String,
}

/// Every method here takes either a single string or nothing at all, and that
/// is deliberate.
///
/// KWin's `callDBus` infers D-Bus types from JS values, and its inference is
/// not stable: a small integer marshals as `i` (int32) and a large one as `d`
/// (double). A `Hello(u, s)` signature was rejected as `(is)`, and a
/// `Heartbeat(t)` carrying `Date.now()` arrived as `d`. Both failed silently
/// from the helper's side — it has no error callback — so the daemon simply
/// never learned the helper existed while window snapshots kept arriving.
#[zbus::interface(name = "org.dozed.FocusSink1")]
impl FocusSink {
    /// Sent whenever the helper (re)discovers us.
    async fn hello(&self, json: String) {
        let compositor = serde_json::from_str::<HelloPayload>(&json)
            .map(|p| p.compositor)
            .unwrap_or_else(|_| "unknown".to_owned());
        tracing::info!(%compositor, "compositor helper connected");
        self.offer(Incoming::HelperAlive { compositor: Some(compositor) }, "hello");
    }

    /// A complete snapshot of every application window.
    async fn windows(&self, json: String) {
        match serde_json::from_str::<WindowsPayload>(&json) {
            Ok(payload) => {
                let total = payload.windows.len();
                let facts: Vec<dozed_core::WindowFacts> =
                    payload.windows.into_iter().filter_map(WindowReport::into_facts).collect();
                if facts.len() != total {
                    // Routine, and the point is that the rest of the snapshot
                    // still gets through.
                    tracing::debug!(
                        dropped = total - facts.len(),
                        kept = facts.len(),
                        "windows with no usable PID skipped"
                    );
                }
                self.offer(Incoming::WindowFacts(facts), "window snapshot");
            }
            Err(e) => tracing::warn!(error = %e, "malformed window snapshot"),
        }
    }

    /// Returns this daemon run's instance token. No arguments: the timestamp
    /// was never used, and carrying one cost us a silent signature mismatch.
    ///
    /// The reply is produced **before** anything is enqueued, and the enqueue
    /// cannot block. This method exists to prove the helper is alive, so it must
    /// not be defeatable by the daemon's own backpressure: awaiting a full channel
    /// here would time out the helper's call, and the daemon would then conclude
    /// from the missing heartbeats that the helper had gone away — a liveness
    /// check that reports death because the reporter was busy.
    async fn heartbeat(&self) -> String {
        let token = self.instance.clone();
        self.offer(Incoming::HelperAlive { compositor: None }, "heartbeat");
        token
    }
}

impl FocusSink {
    /// Hand a message to the daemon if there is room, and drop it if there is not.
    ///
    /// **Never blocks**, which is the property that matters: these methods run on
    /// the connection's dispatcher, so an await here stalls every later call on the
    /// same connection — including the heartbeat that decides whether the helper is
    /// believed to exist.
    ///
    /// Dropping is safe for everything this interface carries because none of it is
    /// a delta. A window snapshot is *complete* and another arrives in five
    /// seconds, so the newer one would have superseded the one we waited for
    /// anyway; a heartbeat repeats on the same schedule; and `hello` is re-sent
    /// whenever the instance token changes. Losing one of any of them costs a
    /// single interval, where blocking costs the connection.
    fn offer(&self, message: Incoming, what: &str) {
        if let Err(e) = self.tx.try_send(message) {
            tracing::debug!(error = %e, "dropped a {what} from the compositor helper");
        }
    }
}

/// The name the compositor helper calls, and the object it calls it on.
const BUS_NAME: &str = "org.dozed.Daemon";
const FOCUS_SINK_PATH: &str = "/org/dozed/FocusSink1";

/// How long a self-ping may take before the connection counts as deaf.
///
/// Generous: the point is to tell "nobody is dispatching" from "the broker is
/// busy", and the failure it is aimed at produced 25-second timeouts for 2h14m,
/// not a slow round trip.
const BUS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Everything that rides the session bus, kept alive as a group.
///
/// A group because the connection is shared, and the trace of the failure this
/// exists for shows them failing as one: the focus sink we serve, the audio,
/// bus-name and screencast exemption watchers, and the activation watcher all ride
/// a single `Connection`, and **every** session-bus-derived event stopped at the
/// same instant while system-bus events carried on for another 2h14m. Rebuilding
/// the connection without respawning its consumers would bring back window reports
/// and leave the exemptions dead — a daemon that looks healthy and would freeze a
/// music player.
struct SessionBus {
    /// The well-known name to serve under. A field rather than [`BUS_NAME`]
    /// directly so the tests can claim something else: this name is a singleton on
    /// the user's session bus, and a test that took it would take it *from the
    /// running daemon* — which is not a hypothetical, it is how this field came to
    /// exist.
    name: String,
    conn: Connection,
    /// The consumer tasks. Stopped before the connection is dropped, because they
    /// hold clones of it and the broker keeps our name until the last one is gone.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    tx: mpsc::Sender<Incoming>,
    /// The token this run started with. A rebuild appends a generation to it.
    instance: String,
    generation: u32,
    /// Live `[audio]` settings, handed to each respawned audio watcher.
    audio: tokio::sync::watch::Receiver<dozed_core::config::AudioConfig>,
}

impl SessionBus {
    /// Whether this connection can still carry a round trip.
    ///
    /// Pings **ourselves**, deliberately. That exercises the whole path that
    /// failed — outbound write, the broker, inbound dispatch to our own object
    /// server, and the reply — rather than merely proving a socket is open. The
    /// connection that wedged kept both its socket and its name, which is exactly
    /// why the helper saw reply timeouts rather than "name has no owner", and why
    /// nothing short of a round trip can tell the difference.
    ///
    /// An *error* reply counts as responsive: it proves the round trip happened.
    /// Only a timeout or a transport failure means nobody is listening.
    async fn responsive(&self) -> bool {
        let ping = self.conn.call_method(
            Some(self.name.as_str()),
            FOCUS_SINK_PATH,
            Some("org.freedesktop.DBus.Peer"),
            "Ping",
            &(),
        );
        match tokio::time::timeout(BUS_PROBE_TIMEOUT, ping).await {
            Ok(Ok(_)) => true,
            // An error reply means the round trip happened, which is what is being
            // measured — *unless* the error is that our name has no owner. That one
            // says the broker is fine and we are no longer the daemon: something
            // replaced us, or our request was never granted. Reachable, and worth
            // rebuilding for, so it must not be filed under "responsive".
            Ok(Err(zbus::Error::MethodError(name, ..)))
                if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner"
                    || name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
            {
                tracing::warn!(name = %self.name, "we no longer own our own bus name");
                false
            }
            Ok(Err(zbus::Error::MethodError(..))) => true,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "the session bus refused our own ping");
                false
            }
            Err(_) => {
                tracing::warn!(
                    timeout = ?BUS_PROBE_TIMEOUT,
                    "the session bus did not answer our own ping"
                );
                false
            }
        }
    }

    /// Throw the connection away and stand a new one up in its place.
    async fn rebuild(&mut self) -> zbus::Result<()> {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.generation += 1;
        // A *fresh* token, deliberately. The helper dedupes its payloads and
        // republishes only when the instance changes, so a rebuild that kept the
        // old token would leave us with a working connection and no window reports
        // — a daemon that looks recovered and still holds nothing. This is the case
        // `FocusSink::instance` was added for; a bus rebuild is a restart as far as
        // anything on the other end can tell.
        let token = format!("{}-r{}", self.instance, self.generation);
        self.conn = connect(&self.name, self.tx.clone(), token).await?;
        self.tasks = spawn_consumers(&self.conn, &self.tx, &self.audio);
        Ok(())
    }
}

/// Build the served connection and claim `BUS_NAME`.
async fn connect(
    name: &str,
    tx: mpsc::Sender<Incoming>,
    instance: String,
) -> zbus::Result<Connection> {
    zbus::connection::Builder::session()?
        .name(name.to_owned())?
        // Both halves are needed, and only for the sake of a rebuild: the
        // replacement has to be *allowed* to take the name (a flag only the
        // current owner can grant, so it must be set here, before anything goes
        // wrong) and has to *ask* to. Without them a deaf connection keeps
        // `org.dozed.Daemon` until its socket closes, which is precisely the thing
        // it has stopped doing.
        .allow_name_replacements(true)
        .replace_existing_names(true)
        .serve_at(FOCUS_SINK_PATH, FocusSink { tx, instance })?
        .build()
        .await
}

/// The watchers that read the session bus, spawned against one connection.
///
/// `audio` carries `[audio]` from the config rather than a copy of it, so a
/// `dozectl reload` that turns silence detection off reaches the watcher instead
/// of waiting for a restart — and survives a bus rebuild, which respawns these.
fn spawn_consumers(
    conn: &Connection,
    tx: &mpsc::Sender<Incoming>,
    audio: &tokio::sync::watch::Receiver<dozed_core::config::AudioConfig>,
) -> Vec<tokio::task::JoinHandle<()>> {
    vec![
        // The exemptions that keep live mode from freezing a hidden music player
        // or a process every other app is waiting on (`DESIGN.md` §10). Without
        // these the shipped `audio-exempt` and `dbus-owner-exempt` rules match on
        // state nobody sets and can never fire.
        tokio::spawn(crate::exemptions::watch_audio(conn.clone(), tx.clone(), audio.clone())),
        tokio::spawn(crate::exemptions::watch_bus_names(conn.clone(), tx.clone())),
        // Screen sharing, the same hazard pointed the other way: the app being
        // shared is by definition the one you are not working in.
        tokio::spawn(crate::exemptions::watch_screencast(conn.clone(), tx.clone())),
        // Thaw before a launcher click reaches a frozen app (`DESIGN.md` §10).
        tokio::spawn(crate::activation::watch_activation(conn.clone(), tx.clone())),
    ]
}

/// Serve the focus sink, claim `org.dozed.Daemon`, and keep both alive.
///
/// Returns the handle the daemon uses to ask "can you still hear anything?". It
/// asks only when it is about to declare the compositor helper lost, so this costs
/// nothing while things work — no timer, no polling, and §13 stays intact.
///
/// The failure this exists for: a session-bus connection went deaf and kept its
/// name for 2h14m. The daemon's loop, its sampler and its system-bus sources all
/// carried on normally, the helper went on calling every five seconds — 1,291 calls,
/// every one of them timing out — and nothing anywhere noticed that the fault was
/// ours. It ended because the daemon was restarted for an unrelated reason.
pub async fn serve_focus_sink(
    tx: mpsc::Sender<Incoming>,
    instance: String,
    audio: tokio::sync::watch::Receiver<dozed_core::config::AudioConfig>,
) -> zbus::Result<mpsc::Sender<()>> {
    let conn = connect(BUS_NAME, tx.clone(), instance.clone()).await?;
    let tasks = spawn_consumers(&conn, &tx, &audio);
    let bus =
        SessionBus { name: BUS_NAME.to_owned(), conn, tasks, tx, instance, generation: 0, audio };
    // One slot: a second request while one is in flight is the same question, and
    // the answer to the first will do.
    let (probe_tx, probe_rx) = mpsc::channel(1);
    tokio::spawn(supervise(bus, probe_rx));
    Ok(probe_tx)
}

/// Answer the daemon's "can you still hear anything?", and act on the answer.
async fn supervise(mut bus: SessionBus, mut probe: mpsc::Receiver<()>) {
    while probe.recv().await.is_some() {
        if bus.responsive().await {
            // The bus is fine, so the helper really has stopped talking to us.
            let _ = bus.tx.send(Incoming::SessionBusHealth { healthy: true }).await;
            continue;
        }
        let _ = bus.tx.send(Incoming::SessionBusHealth { healthy: false }).await;
        match bus.rebuild().await {
            Ok(()) => {
                tracing::info!(
                    generation = bus.generation,
                    "rebuilt the session bus; the helper should call in shortly"
                );
                let _ = bus.tx.send(Incoming::SessionBusHealth { healthy: true }).await;
            }
            // Nothing else to try, and nothing is lost by waiting: the daemon is
            // passive and will ask again the next time it looks.
            Err(e) => tracing::error!(error = %e, "could not rebuild the session bus"),
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
pub trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
    /// Whether the lid is shut. Present on laptops, absent on desktops, and
    /// `LidIsPresent` is what distinguishes "open" from "there is no lid".
    #[zbus(property)]
    fn lid_is_closed(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn lid_is_present(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
pub trait UPowerDevice {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn energy_full(&self) -> zbus::Result<f64>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Login1Manager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
pub trait Login1Session {
    #[zbus(property)]
    fn idle_hint(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;
    /// Emitted when something asks the session to lock — a hotkey, an idle
    /// timeout, `loginctl lock-session`.
    ///
    /// A real signal, unlike `LockedHint`, which is why it is worth having on top
    /// of the poll below: the whole saving from `Away` comes from noticing the
    /// user left, and up to a poll interval of not noticing is the one latency
    /// here that costs anything. It does not *replace* the poll, because a screen
    /// locker that locks itself and only sets the hint emits nothing.
    #[zbus(signal)]
    fn lock(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn unlock(&self) -> zbus::Result<()>;
}

/// Connect to the **system** bus, where UPower and logind live.
///
/// The focus sink is a session-bus service (the compositor helper is a session
/// process), but power and login state are system services. Using one
/// connection for both silently degrades every reading to its fallback, and the
/// fallback for power is "on AC" — which looks right on a plugged-in laptop and
/// is therefore invisible until someone unplugs it.
pub async fn system_bus() -> zbus::Result<Connection> {
    Connection::system().await
}

/// The proxies `watch_system` reads through, built once and kept for the life
/// of the daemon.
///
/// Constructing a `zbus` proxy is not free. For an interface with cached
/// properties it costs a `GetNameOwner`, a `GetAll` and an `AddMatch`, plus a
/// `RemoveMatch` when it drops — and this module used to build all three
/// proxies inside the poll loop. Measured on the system bus while idle, that
/// turned "three cheap property reads" into **15 D-Bus method calls every five
/// seconds**, of which not one was a property `Get`: every value was already
/// being served from a cache that was then thrown away and rebuilt from
/// scratch on the next tick. The setup and teardown *was* the entire cost.
///
/// Held instead, that cost is paid once at startup.
///
/// Caching is switched **off**, which sounds backwards and is the whole point.
/// Leaving it on makes each read local and the bus go silent, which measures
/// beautifully and is wrong: logind does not feed the cache for the session
/// properties read here, so the values freeze at their startup reading and the
/// daemon goes permanently blind to idle and lock. Measured — with the cache
/// on, `SetIdleHint b true` left `dozectl status` reporting `desktop active`
/// for a full minute while logind reported `IdleHint=true` throughout. A
/// silently stale reading is the worst failure this file can produce: §8's
/// whole saving comes from noticing the user left, and the fallbacks here are
/// the plausible-looking ones ("on AC", "active"), so nothing looks broken.
///
/// So: one `Get` per property per tick, six per 5s, on proxies that are built
/// once. Against the 15 setup-and-teardown calls per tick that this replaced,
/// still a clear win, and the semantics are exactly what they were.
struct SystemProxies {
    upower: Option<UPowerProxy<'static>>,
    battery: Option<UPowerDeviceProxy<'static>>,
    session: Option<Login1SessionProxy<'static>>,
    /// Every property read, counted.
    ///
    /// §13 says a battery tool that cannot show its own cost is asking to be
    /// taken on faith, and this loop *is* the daemon's idle cost — the sampler
    /// backs off to 60s when nothing is pending, so on a quiet desktop these
    /// reads are most of what dozed does. Reported in `dozectl status`, which is
    /// also the only way to verify the interval change below actually helped.
    bus_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl SystemProxies {
    async fn connect(
        conn: &Connection,
        bus_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        use zbus::proxy::CacheProperties::No;
        Self {
            upower: UPowerProxy::builder(conn).cache_properties(No).build().await.ok(),
            battery: UPowerDeviceProxy::builder(conn).cache_properties(No).build().await.ok(),
            session: Login1SessionProxy::builder(conn).cache_properties(No).build().await.ok(),
            bus_calls,
        }
    }

    async fn power(&self) -> PowerState {
        let on_ac = match &self.upower {
            Some(p) => {
                self.count(1);
                !p.on_battery().await.unwrap_or(false)
            }
            // No UPower means we cannot tell, and a desktop with no battery is
            // the likeliest reason. Reporting on-AC keeps the default policy
            // inert.
            None => true,
        };
        let percentage = match &self.battery {
            Some(d) => {
                self.count(2);
                if d.is_present().await.unwrap_or(false) {
                    d.percentage().await.ok().map(|p| p as f32)
                } else {
                    None
                }
            }
            _ => None,
        };
        PowerState { on_ac, percentage }
    }

    /// Whether the lid is shut, or `false` when there is no lid to shut.
    ///
    /// Absent hardware answers "open" rather than "unknown": a machine with no
    /// lid must not be treated as one with a closed one.
    async fn lid_closed(&self) -> bool {
        let Some(p) = &self.upower else { return false };
        self.count(2);
        p.lid_is_present().await.unwrap_or(false) && p.lid_is_closed().await.unwrap_or(false)
    }

    /// Read logind's hints and map them onto the three desktop states.
    async fn desktop(&self, lid_closed: bool) -> DesktopState {
        let Some(session) = &self.session else {
            return DesktopState::IdlePresent;
        };
        self.count(3);
        let locked = session.locked_hint().await.unwrap_or(false);
        let active = session.active().await.unwrap_or(true);
        let idle = session.idle_hint().await.unwrap_or(false);
        desktop_state(locked, active, idle, lid_closed)
    }

    fn count(&self, n: u64) {
        self.bus_calls.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Map logind's hints and the lid onto the three desktop states.
///
/// Pure, because this is a policy decision with four inputs and the async
/// property reads around it made it untestable — which is how the lid rule below
/// would otherwise have shipped on nothing but reasoning.
///
/// `Away` is what earns most of the saving and costs nothing, because nobody is
/// switching windows. But "idle" is not "away": watching a two-hour video
/// produces zero keypresses, so `Away` from idleness alone is suppressed by the
/// audio and inhibitor exemptions (`DESIGN.md` §8).
///
/// ## The lid is corroboration, never an override
///
/// A shut lid on a machine that is still running means one of two very different
/// things, and UPower cannot tell them apart: an unattended laptop, or a docked
/// one whose user is working on an external display. The default
/// `HandleLidSwitch=suspend` makes the first case moot — the machine suspends and
/// `PrepareForSleep` handles it — so in practice a lid that is closed *while dozed
/// is still running* is disproportionately the docked case.
///
/// So the lid may only upgrade `IdlePresent` to `Away`. It can never contradict
/// input: someone typing on an external keyboard is present whatever the lid is
/// doing, and treating that as `Away` would collapse grace to zero and freeze the
/// apps they are alt-tabbing between — the exact harm `active_factor` exists to
/// prevent. Combined with no input it is real evidence, and it is the one signal
/// that distinguishes "no keypresses for ten minutes" from "nobody is there".
#[must_use]
pub fn desktop_state(locked: bool, active: bool, idle: bool, lid_closed: bool) -> DesktopState {
    if locked || !active {
        return DesktopState::Away;
    }
    match (idle, lid_closed) {
        (true, true) => DesktopState::Away,
        (true, false) => DesktopState::IdlePresent,
        // Input is input, lid or no lid.
        (false, _) => DesktopState::Active,
    }
}

/// Battery design capacity in watt-hours, for the report's charge estimate.
pub async fn battery_wh(conn: &Connection) -> Option<f64> {
    let d = UPowerDeviceProxy::new(conn).await.ok()?;
    if !d.is_present().await.unwrap_or(false) {
        return None;
    }
    d.energy_full().await.ok().filter(|v| *v > 0.0)
}

/// How often to re-read what cannot be subscribed to.
///
/// Was 5s, which cost six property reads every five seconds — 1.2 bus calls a
/// second, sustained, for the life of the daemon, and on an idle desktop that is
/// most of what dozed does. DESIGN.md listed replacing it with subscriptions as
/// almost the whole of the remaining idle cost.
///
/// It cannot be replaced outright. logind does not emit `PropertiesChanged` for
/// the session hints read here — that is exactly what was measured when property
/// caching was tried and left `desktop active` frozen for a full minute while
/// logind reported otherwise — so a poll is the only way to see `IdleHint` move.
///
/// What it can be is *rarer*, once the two things that actually matter arrive as
/// signals: UPower publishes `PropertiesChanged` for real, and logind emits
/// `Lock`/`Unlock`. Both trigger an immediate re-read, so the interval below is a
/// backstop rather than the mechanism, and the worst case it governs is noticing
/// that a session went idle up to 15s late. That is 15s of extra patience on a
/// grace of minutes.
///
/// Three reads per 15s: 0.2 calls a second, against 1.2 before. Verifiable at
/// runtime rather than on this reasoning — `dozectl status` reports the rate.
const SYSTEM_POLL: Duration = Duration::from_secs(15);

/// The generic properties interface on UPower's manager object.
///
/// Used instead of the typed proxy's per-property streams because those are tied
/// to the property cache, and the cache is deliberately off here — see
/// [`SystemProxies`], where leaving it on froze the daemon's view of idle and lock
/// state for a whole minute. This subscribes to the raw signal and is therefore
/// independent of that decision.
async fn upower_properties(conn: &Connection) -> zbus::Result<zbus::fdo::PropertiesProxy<'static>> {
    zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.UPower")?
        .path("/org/freedesktop/UPower")?
        .build()
        .await
}

/// Watch power, activity and sleep, emitting events on change.
///
/// Signals where they exist, a slow poll for the properties logind refuses to
/// announce. See [`SYSTEM_POLL`] for why it is both.
pub async fn watch_system(
    conn: Connection,
    tx: mpsc::Sender<Incoming>,
    bus_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use futures_util::StreamExt as _;

    let mut last_power: Option<PowerState> = None;
    let mut last_desktop: Option<DesktopState> = None;
    let proxies = SystemProxies::connect(&conn, bus_calls).await;
    let mut ticker = tokio::time::interval(SYSTEM_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // PrepareForSleep is a signal, not a poll: we must thaw *before* the system
    // suspends, and logind waits for inhibitors while the signal is in flight.
    if let Ok(mgr) = Login1ManagerProxy::new(&conn).await
        && let Ok(mut sleeps) = mgr.receive_prepare_for_sleep().await
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(sig) = sleeps.next().await {
                if let Ok(args) = sig.args() {
                    let _ = tx
                        .send(Incoming::Event(Event::PrepareForSleep { sleeping: args.start }))
                        .await;
                }
            }
        });
    }

    // The two things worth hearing about immediately, merged into one stream of
    // "go and look now" nudges. A nudge rather than a payload, deliberately:
    // whatever woke us, the answer still comes from reading the properties, so
    // there is exactly one place that decides what the desktop state is.
    let (nudge_tx, mut nudges) = mpsc::channel::<&'static str>(8);

    // `fdo::PropertiesProxy` rather than the typed proxy's per-property streams,
    // because the typed streams are tied to the property cache and the cache is
    // deliberately off here (see `SystemProxies`). This subscribes to the raw
    // signal and stays independent of that decision.
    if let Ok(props) = upower_properties(&conn).await
        && let Ok(mut changes) = props.receive_properties_changed().await
    {
        let nudge = nudge_tx.clone();
        tokio::spawn(async move {
            while changes.next().await.is_some() {
                // Unplugging the charger and shutting the lid are both here.
                let _ = nudge.send("upower").await;
            }
        });
    }

    // Two streams of different types, so two tasks: `Lock` and `Unlock` are
    // distinct signals and zbus gives each its own stream type.
    if let Some(session) = &proxies.session {
        if let Ok(mut locks) = session.receive_lock().await {
            let nudge = nudge_tx.clone();
            tokio::spawn(async move {
                while locks.next().await.is_some() {
                    let _ = nudge.send("lock").await;
                }
            });
        }
        if let Ok(mut unlocks) = session.receive_unlock().await {
            let nudge = nudge_tx.clone();
            tokio::spawn(async move {
                while unlocks.next().await.is_some() {
                    let _ = nudge.send("unlock").await;
                }
            });
        }
    }

    loop {
        // Whichever comes first: something told us to look, or the backstop
        // expired. `tick()` fires immediately the first time, so the initial
        // state is published without waiting out an interval.
        tokio::select! {
            _ = ticker.tick() => {}
            Some(why) = nudges.recv() => {
                tracing::debug!(signal = why, "re-reading power and session state");
            }
        }

        let power = proxies.power().await;
        if last_power != Some(power) {
            last_power = Some(power);
            let _ = tx.send(Incoming::Event(Event::PowerChanged { power })).await;
        }
        let desktop = proxies.desktop(proxies.lid_closed().await).await;
        if last_desktop != Some(desktop) {
            last_desktop = Some(desktop);
            let _ = tx.send(Incoming::Event(Event::ActivityChanged { desktop })).await;
        }
    }
}

/// The helper is required to beat every 5s; three missed in a row means we can
/// no longer see focus and must not be holding anything (`DESIGN.md` §10.2).
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(16);

/// Tracks helper liveness.
///
/// `healthy` starts false: until the helper says hello we cannot see focus, and
/// an app we cannot see must never be frozen.
#[derive(Debug, Default)]
pub struct HelperWatch {
    pub last_seen: Option<Instant>,
    pub healthy: bool,
    pub compositor: Option<String>,
}

impl HelperWatch {
    /// Note a heartbeat. Returns `Some(true)` when the helper just came back.
    pub fn alive(&mut self, now: Instant, compositor: Option<String>) -> Option<bool> {
        self.last_seen = Some(now);
        if compositor.is_some() {
            self.compositor = compositor;
        }
        if self.healthy {
            return None;
        }
        self.healthy = true;
        Some(true)
    }

    /// Returns `Some(false)` when the helper has just been declared dead.
    pub fn check(&mut self, now: Instant) -> Option<bool> {
        if !self.healthy {
            return None;
        }
        let gone = self.last_seen.is_none_or(|t| now.duration_since(t) > HELPER_TIMEOUT);
        if gone {
            self.healthy = false;
            return Some(false);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_health_is_edge_triggered() {
        let mut w = HelperWatch::default();
        let t0 = Instant::now();
        assert_eq!(w.alive(t0, Some("kwin".into())), Some(true), "first beat is a transition");
        assert_eq!(w.alive(t0, None), None, "subsequent beats are not");
        assert_eq!(w.compositor.as_deref(), Some("kwin"));

        assert_eq!(w.check(t0 + Duration::from_secs(5)), None, "still fresh");
        assert_eq!(w.check(t0 + HELPER_TIMEOUT + Duration::from_secs(1)), Some(false));
        assert_eq!(w.check(t0 + HELPER_TIMEOUT + Duration::from_secs(2)), None, "reported once");

        assert_eq!(w.alive(t0 + Duration::from_secs(60), None), Some(true), "and it can recover");
    }

    #[test]
    fn we_start_out_blind() {
        // Until the helper says hello we cannot see focus, so nothing may be
        // frozen on the strength of a guess.
        let w = HelperWatch::default();
        assert!(!w.healthy);
    }

    #[test]
    fn a_protocol_1_helper_still_works() {
        // An older KWin script computed visibility itself and sent no geometry.
        // Upgrading the daemon must not silently stop understanding it.
        let json = r#"{"seq":7,"windows":[
            {"pid":283751,"app_id":"org.wezfurlong.wezterm",
             "desktop_file":"org.wezfurlong.wezterm","visibility":"hidden",
             "output":"DP-1","fullscreen":false,"attention":false}]}"#;
        let p: WindowsPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.seq, 7);
        let facts = p.windows.into_iter().next().unwrap().into_facts().unwrap();
        assert_eq!(facts.pid, 283751);
        assert_eq!(facts.visibility, Some(dozed_core::Visibility::Hidden));
        assert_eq!(facts.geometry, None, "protocol 1 sends no geometry");
        assert_eq!(
            dozed_core::visibility::resolve(&[facts])[0],
            dozed_core::Visibility::Hidden,
            "and the daemon takes its word rather than inventing an answer"
        );
    }

    #[test]
    fn a_protocol_2_helper_reports_facts_and_the_daemon_decides() {
        let json = r#"{"seq":2,"windows":[
            {"pid":1,"app_id":"a","focused":false,"minimized":false,
             "on_current_desktop":true,"x":0,"y":0,"w":1920,"h":1080,
             "stack":0,"output":"DP-1"},
            {"pid":2,"app_id":"b","focused":true,"minimized":false,
             "on_current_desktop":true,"x":0,"y":0,"w":1920,"h":1080,
             "stack":1,"output":"DP-1"}]}"#;
        let p: WindowsPayload = serde_json::from_str(json).unwrap();
        let facts: Vec<_> = p.windows.into_iter().filter_map(WindowReport::into_facts).collect();
        assert_eq!(facts[0].geometry, Some(dozed_core::Rect::new(0, 0, 1920, 1080)));
        assert_eq!(
            dozed_core::visibility::resolve(&facts),
            vec![dozed_core::Visibility::Hidden, dozed_core::Visibility::Focused],
            "the daemon computes the occlusion the helper no longer does"
        );
    }

    #[test]
    fn one_window_with_no_pid_does_not_discard_the_snapshot() {
        // The bug: KWin reports `pid: -1` for a window whose process it cannot
        // name, `pid` was typed `u32`, and serde failed the whole array — 241
        // snapshots dropped in one day, each leaving every window's state stale
        // until the next arrived. The helper sorts by PID, so the `-1` came
        // first and took the entire desktop with it.
        let json = r#"{"seq":9,"windows":[
            {"pid":-1,"app_id":"","focused":false,"stack":0},
            {"pid":283751,"app_id":"org.wezfurlong.wezterm","focused":true,
             "on_current_desktop":true,"x":0,"y":0,"w":1920,"h":1080,"stack":1}]}"#;

        let p: WindowsPayload = serde_json::from_str(json)
            .expect("a window with no PID must not fail the whole payload");
        let facts: Vec<_> = p.windows.into_iter().filter_map(WindowReport::into_facts).collect();

        assert_eq!(facts.len(), 1, "the unattributable window is dropped, not the snapshot");
        assert_eq!(facts[0].pid, 283751, "and the real window still gets through");
    }

    #[test]
    fn a_zero_pid_is_no_more_usable_than_a_negative_one() {
        // Nothing can be resolved from /proc/0, and it is what a helper that
        // defaults the field rather than omitting it would send.
        for pid in ["0", "-1", "-2147483648", "4294967296"] {
            let json = format!(r#"{{"seq":1,"windows":[{{"pid":{pid}}}]}}"#);
            let p: WindowsPayload = serde_json::from_str(&json).unwrap();
            assert!(
                p.windows.into_iter().next().unwrap().into_facts().is_none(),
                "pid {pid} must not become a window"
            );
        }
    }

    #[test]
    fn missing_optional_window_fields_are_tolerated() {
        let json = r#"{"seq":1,"windows":[{"pid":1}]}"#;
        let p: WindowsPayload = serde_json::from_str(json).unwrap();
        let facts = p.windows.into_iter().next().unwrap().into_facts().unwrap();
        assert_eq!(facts.app_id, "");
        assert!(!facts.fullscreen);
        assert!(facts.on_current_desktop, "an unstated workspace is never `elsewhere`");
        assert_eq!(
            dozed_core::visibility::resolve(&[facts])[0],
            dozed_core::Visibility::VisibleUnfocused,
            "a payload that says almost nothing must not get an app frozen"
        );
    }

    #[test]
    fn a_locked_or_inactive_session_is_away_whatever_else_is_true() {
        // `Away` is where most of the saving comes from and it costs nothing,
        // because nobody is switching windows.
        for lid in [false, true] {
            for idle in [false, true] {
                assert_eq!(
                    desktop_state(true, true, idle, lid),
                    DesktopState::Away,
                    "locked: idle={idle} lid={lid}"
                );
                assert_eq!(
                    desktop_state(false, false, idle, lid),
                    DesktopState::Away,
                    "another VT is in front: idle={idle} lid={lid}"
                );
            }
        }
    }

    #[test]
    fn a_shut_lid_turns_idleness_into_absence() {
        // What the lid actually adds: without it, "no keypresses for ten minutes"
        // and "nobody is there" are the same reading, and the second deserves a
        // grace of zero while the first does not.
        assert_eq!(desktop_state(false, true, true, false), DesktopState::IdlePresent);
        assert_eq!(desktop_state(false, true, true, true), DesktopState::Away);
    }

    #[test]
    fn a_shut_lid_never_contradicts_someone_typing() {
        // The docked case, and the reason the lid is corroboration rather than an
        // override. A closed lid with an external keyboard and monitor is somebody
        // working; calling that `Away` collapses grace to zero and freezes the
        // apps they are alt-tabbing between, which is the exact harm
        // `active_factor` exists to prevent.
        assert_eq!(
            desktop_state(false, true, false, true),
            DesktopState::Active,
            "input is input, lid or no lid"
        );
    }

    #[test]
    fn a_machine_with_no_lid_is_not_a_machine_with_a_closed_one() {
        // `lid_closed` is `LidIsPresent && LidIsClosed`, so a desktop reports
        // false. Reading absent hardware as "closed" would put every desktop into
        // `Away` the moment it went idle — plausible-looking, and a policy change
        // nobody asked for.
        assert_eq!(desktop_state(false, true, true, false), DesktopState::IdlePresent);
    }

    #[test]
    fn the_poll_is_a_backstop_rather_than_the_mechanism() {
        // Pinned as a number because it is a cost: the sampler backs off to 60s
        // when nothing is pending, so these reads are most of what an idle daemon
        // does. Three properties per interval, and the signals are what make the
        // interval affordable.
        assert_eq!(SYSTEM_POLL, Duration::from_secs(15));
        assert!(
            SYSTEM_POLL > Duration::from_secs(5),
            "the whole point was to stop reading six properties every five seconds"
        );
    }

    #[test]
    fn an_unparseable_visibility_fails_towards_never_touch_this() {
        assert_eq!(parse_visibility("focused"), dozed_core::Visibility::Focused);
        assert_eq!(parse_visibility("hidden"), dozed_core::Visibility::Hidden);
        assert_eq!(
            parse_visibility("nonsense"),
            dozed_core::Visibility::Focused,
            "a word we cannot read must never make an app freezable"
        );
    }
}

#[cfg(test)]
mod bus_tests {
    use super::*;

    /// A name of our own, per test process.
    ///
    /// Never `BUS_NAME`. That one is a singleton on the user's session bus, and a
    /// test claiming it takes it **from the running daemon** — which is exactly what
    /// happened while these tests were being written: the live daemon lost the name,
    /// went passive, restored every held app, and reported that the compositor
    /// helper had stopped talking to it. A test suite that can do that to the
    /// machine it runs on is a worse bug than the one it is testing for.
    fn private_name(what: &str) -> String {
        format!("org.dozed.Test{}{}", std::process::id(), what)
    }

    /// Whether a session bus is reachable at all. These test the *served* side of
    /// one, so without a bus there is nothing to say.
    async fn have_session_bus() -> bool {
        zbus::Connection::session().await.is_ok()
    }

    fn bus_of(name: String, conn: Connection, tx: mpsc::Sender<Incoming>) -> SessionBus {
        SessionBus {
            name,
            conn,
            tasks: Vec::new(),
            tx,
            instance: "test".to_owned(),
            generation: 0,
            audio: tokio::sync::watch::channel(dozed_core::config::AudioConfig::default()).1,
        }
    }

    /// The probe has to answer "can we still hear anything", not "is a socket
    /// open". The connection that failed kept both its socket and its name, which
    /// is why the helper saw reply timeouts rather than a name error, and why
    /// nothing short of a round trip can tell the difference.
    #[tokio::test]
    async fn a_healthy_connection_answers_its_own_ping() {
        if !have_session_bus().await {
            return;
        }
        let name = private_name("Ping");
        let (tx, _rx) = mpsc::channel(8);
        let conn = connect(&name, tx.clone(), "probe".to_owned()).await.unwrap();
        assert!(
            bus_of(name, conn, tx).responsive().await,
            "a live connection must answer its own Ping"
        );
    }

    /// Losing the name is a different failure from going deaf, and it needs the
    /// same remedy — so it must not be filed under "responsive" just because the
    /// broker sent us a tidy error back.
    #[tokio::test]
    async fn a_connection_that_no_longer_owns_its_name_is_not_responsive() {
        if !have_session_bus().await {
            return;
        }
        let (tx, _rx) = mpsc::channel(8);
        // A connection that never claimed the name it is asked to ping.
        let conn = zbus::Connection::session().await.unwrap();
        let bus = bus_of(private_name("Nobody"), conn, tx);
        assert!(!bus.responsive().await);
    }

    /// The rebuild has to take the name back from a connection that still holds it,
    /// because the connection it replaces is one that has stopped closing its
    /// socket. That needs `AllowReplacement` on the *original* — a flag only the
    /// current owner can grant, so getting it wrong is undetectable until the day it
    /// matters — and `ReplaceExisting` on the replacement.
    #[tokio::test]
    async fn a_rebuild_takes_the_name_from_the_connection_it_replaces() {
        if !have_session_bus().await {
            return;
        }
        let name = private_name("Rebuild");
        // The daemon's own capacity. A heartbeat is dropped when the channel is
        // full, and on a machine with no `pactl` and no camera the respawned
        // watchers report enough to fill a small one before the heartbeat arrives.
        let (tx, mut rx) = mpsc::channel(256);
        let old = connect(&name, tx.clone(), "gen-0".to_owned()).await.unwrap();
        let mut bus = bus_of(name.clone(), old, tx);
        bus.instance = "gen".to_owned();

        bus.rebuild().await.expect("the replacement must be able to claim the name");
        assert_eq!(bus.generation, 1);
        assert!(bus.responsive().await, "and the new connection must be the one serving");

        // The token has to change, or the helper's payload dedupe suppresses the
        // republish and we recover a bus with no window reports on it — a daemon
        // that looks fixed and still holds nothing.
        let reply = bus
            .conn
            .call_method(
                Some(name.as_str()),
                FOCUS_SINK_PATH,
                Some("org.dozed.FocusSink1"),
                "Heartbeat",
                &(),
            )
            .await
            .expect("the rebuilt sink must answer");
        let token: String = reply.body().deserialize().expect("a heartbeat returns the token");
        assert_eq!(token, "gen-r1", "a rebuild looks like a restart from the outside");

        // And that heartbeat reached the daemon rather than being swallowed.
        //
        // Searched for rather than expected first, because a rebuild respawns the
        // session-bus consumers and they report their own health as they start —
        // the first message here was the capture watcher saying this machine has no
        // camera, which is the group-respawn working rather than a fault.
        let mut seen = Vec::new();
        let found = loop {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Some(Incoming::HelperAlive { .. })) => break true,
                Ok(Some(other)) => seen.push(other),
                Ok(None) | Err(_) => break false,
            }
        };
        assert!(found, "the heartbeat must be reported inward; saw only {seen:?}");
    }

    /// `heartbeat` exists to prove the helper is alive, so a full channel must not
    /// be able to turn it into evidence of death. It replies either way.
    #[tokio::test]
    async fn a_full_channel_cannot_stop_the_heartbeat_replying() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Incoming::HelperAlive { compositor: None }).unwrap();
        let sink = FocusSink { tx, instance: "wedged".to_owned() };

        // Would hang forever on `send().await`; must return the token instead.
        let token = tokio::time::timeout(Duration::from_secs(1), sink.heartbeat())
            .await
            .expect("heartbeat must not block on a full channel");
        assert_eq!(token, "wedged");
    }
}
