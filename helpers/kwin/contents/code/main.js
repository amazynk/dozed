// dozed focus source — KWin script.
//
// Tells the dozed daemon which applications the user can actually see, so it can
// distinguish "backgrounded" from "sitting on the second monitor". Focus alone is
// not enough: freezing a window that is still on screen leaves a stale rectangle,
// which reads as a crash.
//
// Direction of travel: the DAEMON owns the D-Bus name and this script calls into
// it. That is not a preference — the KWin script sandbox exposes `callDBus` and
// no way to register a service, so a script cannot publish an interface for
// anyone to consume.
//
// Everything here is shaped by one constraint: dozed exists to remove background
// wakeups, so the helper must not add any. See the comment on the connection
// registry below for what that costs when you get it wrong.

// Every D-Bus argument in this protocol is a string, and that is not a style
// choice. KWin's callDBus infers D-Bus types from JS values and does so
// inconsistently: a small integer marshals as int32, a large one (Date.now())
// as double. Typed numeric signatures on the daemon were rejected with
// "Signature mismatch: got (is), expected (us)" and "got d, expected t" — and
// because callDBus has no error callback, the helper could not tell.
const SERVICE = "org.dozed.Daemon";
const PATH = "/org/dozed/FocusSink1";
const IFACE = "org.dozed.FocusSink1";
const PROTOCOL_VERSION = 2;

// Window changes arrive in bursts — switching desktop restacks everything — so
// coalesce them into one publish rather than one per window.
const DEBOUNCE_MS = 200;
// A heartbeat is what lets the daemon notice this script has died and thaw
// everything (DESIGN.md §10.2). At 5s this is 0.2 wakeups/s, which is noise.
const HEARTBEAT_MS = 5000;
// Heartbeats sent with no reply before we conclude the daemon is gone.
const MISSED_ACKS_BEFORE_DOWN = 3;

// ---------------------------------------------------------------- visibility

// Window kinds that are furniture rather than an application on screen. A
// notification popping up over your terminal does not make the terminal hidden.
function isFurniture(w) {
    return w.desktopWindow || w.dock || w.notification || w.criticalNotification ||
           w.onScreenDisplay || w.dndIcon || w.tooltip || w.popupMenu ||
           w.dropdownMenu || w.comboBox || w.outline || w.deleted;
}

function onCurrentDesktop(w) {
    if (w.onAllDesktops) return true;
    const ds = w.desktops;
    // An empty list means "all desktops" in KWin's model, not "no desktops".
    if (!ds || ds.length === 0) return true;
    const cur = workspace.currentDesktop;
    if (!cur) return true;
    for (let i = 0; i < ds.length; i++) {
        if (ds[i] === cur || (ds[i] && ds[i].id === cur.id)) return true;
    }
    return false;
}

function onCurrentActivity(w) {
    const as = w.activities;
    if (!as || as.length === 0) return true;
    return as.indexOf(workspace.currentActivity) !== -1;
}

// Occlusion used to be computed here. It now happens once, in Rust, in the
// daemon (`dozed_core::visibility`), because a KWin script, a GNOME extension
// and a native X11 source would otherwise be three implementations of one rule
// in two languages with no way to notice them drifting apart.
//
// That move cost nothing. The old code read `frameGeometry` and `stackingOrder`
// at snapshot time — a pull, not a subscription — so reporting them instead of
// interpreting them needs no new signal connection, keeps the same publish
// triggers, and leaves the staleness between publishes exactly as it was. Note
// especially that `frameGeometryChanged` is still not connected; see the
// connection registry below for why that matters.

function stackIndexOf(w, order) {
    for (let i = 0; i < order.length; i++) {
        if (order[i] === w) return i;
    }
    return 0;
}

// ------------------------------------------------------------------ payload

let seq = 0;

function snapshot() {
    const out = [];
    const all = workspace.windowList();
    const order = workspace.stackingOrder;
    for (let i = 0; i < all.length; i++) {
        const w = all[i];
        if (!w || isFurniture(w)) continue;
        // KWin reports pid -1 for a window whose process it cannot name (some
        // XWayland and internal surfaces). The daemon can do nothing with it —
        // identity, class and sampling all start from /proc/<pid> — and sending
        // it did active harm: the sort below puts -1 first, and a daemon that
        // typed pid as unsigned rejected the entire snapshot over it, going
        // blind to every other window until the next one arrived.
        if (!(w.pid > 0)) continue;
        const g = w.frameGeometry || { x: 0, y: 0, width: 0, height: 0 };
        out.push({
            pid: w.pid,
            // Wayland app_id / X11 resource class: DESIGN.md §4 rung 3.
            app_id: (w.resourceClass || "").toString(),
            // Rung 2, handed to us directly. Distinguishes Chrome PWAs and
            // separate Firefox profiles, which the exe path cannot.
            desktop_file: (w.desktopFileName || "").toString(),
            focused: !!w.active,
            minimized: !!(w.minimized || w.hidden),
            on_current_desktop: onCurrentDesktop(w) && onCurrentActivity(w),
            x: Math.round(g.x),
            y: Math.round(g.y),
            w: Math.round(g.width),
            h: Math.round(g.height),
            stack: stackIndexOf(w, order),
            output: w.output ? w.output.name : "",
            fullscreen: !!w.fullScreen,
            // A window we can see through cannot hide what is behind it.
            translucent: w.opacity !== undefined && w.opacity < 1.0,
            // The closest thing KWin offers to "the user is asking for this
            // window back". Not the same as D-Bus activation — see README.
            attention: !!w.demandsAttention
        });
    }
    // Sorted so that an unchanged desktop always serialises to an identical
    // string, which is what makes the equality check below able to suppress it.
    out.sort(function (a, b) {
        if (a.pid !== b.pid) return a.pid - b.pid;
        if (a.stack !== b.stack) return a.stack - b.stack;
        if (a.app_id !== b.app_id) return a.app_id < b.app_id ? -1 : 1;
        return a.output < b.output ? -1 : (a.output > b.output ? 1 : 0);
    });
    return out;
}

let lastPublished = null;
let daemonUp = false;
let missedAcks = 0;

function publishNow() {
    if (!daemonUp) return;
    const windows = snapshot();
    const body = JSON.stringify(windows);
    // Most events do not change what dozed cares about — a resize that does not
    // uncover anything, a caption change we are not even watching. Sending those
    // would be pure wakeup cost on both ends.
    if (body === lastPublished) return;
    lastPublished = body;
    seq += 1;
    callDBus(SERVICE, PATH, IFACE, "Windows",
             JSON.stringify({ seq: seq, windows: windows }));
}

// ------------------------------------------------------------------- timers

const debounce = new QTimer();
debounce.interval = DEBOUNCE_MS;
debounce.singleShot = true;
debounce.timeout.connect(publishNow);

function schedulePublish() {
    // start() on an already-running timer stops and restarts it, which is how a
    // burst collapses into a single publish. Note: KWin hands scripts a
    // KWin::ScriptTimer, and neither it nor QTimer has restart() — calling one
    // throws a TypeError inside the signal handler, where it is invisible except
    // in KWin's journal.
    debounce.start();
}

// The daemon's instance token, as of the last heartbeat we got an answer to.
let daemonInstance = null;

function onDaemonAck(instance) {
    missedAcks = 0;
    // A restart the heartbeat never noticed: the new daemon reclaimed the bus
    // name before three beats went unanswered, so from here it looks like the
    // same conversation. Without this the snapshot dedupe would suppress every
    // publish and the new daemon would never learn a single window exists.
    if (instance !== undefined && instance !== daemonInstance) {
        daemonInstance = instance;
        lastPublished = null;
        publishNow();
    }
}

function onNameHasOwner(has) {
    if (!has) return;
    // The daemon (re)appeared and knows nothing about the current desktop, so
    // force a full publish rather than letting the equality check suppress it.
    daemonUp = true;
    missedAcks = 0;
    lastPublished = null;
    daemonInstance = null;
    callDBus(SERVICE, PATH, IFACE, "Hello",
             JSON.stringify({ protocol: PROTOCOL_VERSION, compositor: "kwin" }), onDaemonAck);
    publishNow();
}

function probeDaemon() {
    callDBus("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
             "NameHasOwner", SERVICE, onNameHasOwner);
}

function onHeartbeat() {
    if (!daemonUp) {
        // Probing the bus daemon always succeeds, so this cannot spam the
        // journal with failures the way calling a dead service would.
        probeDaemon();
        return;
    }
    // KWin's callDBus only invokes the callback on a successful reply, so a
    // silent callback is the only failure signal available to us.
    missedAcks += 1;
    if (missedAcks > MISSED_ACKS_BEFORE_DOWN) {
        daemonUp = false;
        lastPublished = null;
        return;
    }
    callDBus(SERVICE, PATH, IFACE, "Heartbeat", onDaemonAck);
}

const heartbeat = new QTimer();
heartbeat.interval = HEARTBEAT_MS;
heartbeat.singleShot = false;
heartbeat.timeout.connect(onHeartbeat);

// -------------------------------------------------------- connection registry
//
// Read this before adding a signal.
//
// KWin's connect() returns undefined rather than a connection handle, so the
// only way to disconnect is by (signal, handler) identity — which means one
// shared handler for every window, and a record of which windows we have
// already attached to. Getting this wrong is not a small leak: a previous tool
// on this machine accumulated one handler per focus change and ended up making
// ~824 D-Bus calls/s, costing kwin_wayland ~697 wakeups/s and dbus-broker a
// further ~338/s. For a daemon whose whole purpose is deleting wakeups, that
// would be worse than not running at all.
//
// Note what is absent: captionChanged (dozed does not care about window titles)
// and frameGeometryChanged (fires at frame rate throughout a drag). Occlusion
// changes are picked up from maximise, tile and move-finished instead.

const WINDOW_SIGNALS = [
    "minimizedChanged",
    "desktopsChanged",
    "activitiesChanged",
    "outputChanged",
    "fullScreenChanged",
    "hiddenChanged",
    "maximizedChanged",
    "tileChanged",
    "interactiveMoveResizeFinished",
    "demandsAttentionChanged"
];

const attached = [];

function attach(w) {
    if (!w || isFurniture(w)) return;
    if (attached.indexOf(w) !== -1) return;
    for (let i = 0; i < WINDOW_SIGNALS.length; i++) {
        const sig = w[WINDOW_SIGNALS[i]];
        if (sig && sig.connect) sig.connect(schedulePublish);
    }
    attached.push(w);
}

function detach(w) {
    const at = attached.indexOf(w);
    if (at === -1) return;
    attached.splice(at, 1);
    for (let i = 0; i < WINDOW_SIGNALS.length; i++) {
        const sig = w[WINDOW_SIGNALS[i]];
        // A window that has already been destroyed throws here, which is fine:
        // its connections died with it.
        try {
            if (sig && sig.disconnect) sig.disconnect(schedulePublish);
        } catch (e) {
            /* window already gone */
        }
    }
}

function onWindowAdded(w) {
    attach(w);
    schedulePublish();
}

function onWindowRemoved(w) {
    detach(w);
    schedulePublish();
}

// ---------------------------------------------------------------- wire it up

workspace.windowAdded.connect(onWindowAdded);
workspace.windowRemoved.connect(onWindowRemoved);
workspace.windowActivated.connect(schedulePublish);
workspace.currentDesktopChanged.connect(schedulePublish);
workspace.currentActivityChanged.connect(schedulePublish);

const existing = workspace.windowList();
for (let i = 0; i < existing.length; i++) attach(existing[i]);

heartbeat.start();
probeDaemon();
