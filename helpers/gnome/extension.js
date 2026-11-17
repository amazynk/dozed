// dozed focus source — GNOME Shell extension.
//
// The GNOME half of `DESIGN.md` §5: tells the dozed daemon which applications
// the user can actually see, so it can distinguish "backgrounded" from "sitting
// on the second monitor". Freezing a window that is still on screen leaves a
// stale rectangle, which reads as a crash.
//
// This mirrors helpers/kwin deliberately. Same protocol, same direction, same
// anti-wakeup discipline — because the daemon must not have to care which
// compositor it is talking to, and because every difference between the two is
// somewhere a bug can hide in one and not the other.
//
// ## Direction of the protocol
//
// The DAEMON owns the bus name and this extension calls into it, exactly as the
// KWin script does. Unlike a KWin script, a Shell extension *can* export an
// interface with Gio.DBusExportedObject — and deliberately does not. One
// protocol and one sink serve both compositors; giving GNOME its own shape would
// double the daemon's surface for no gain.
//
// ## Why the daemon computes visibility and this does not
//
// Occlusion is decided once, in Rust, in the daemon. This extension reports
// geometry and stacking as facts. See `dozed_core::visibility` — with three
// platforms, one rule and two languages, a shared implementation is the only way
// to know they agree.
//
// ## §10.2 matters more here than anywhere else
//
// This extension *will* break on a major GNOME upgrade. When it does, the daemon
// must notice and thaw everything rather than hold apps frozen against a focus
// source that is no longer reporting. That is what the heartbeat is for, and it
// is why this file never lets an exception escape a signal handler silently.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';

const SERVICE = 'org.dozed.Daemon';
const PATH = '/org/dozed/FocusSink1';
const IFACE = 'org.dozed.FocusSink1';
const PROTOCOL_VERSION = 2;

// Window changes arrive in bursts — switching workspace restacks everything —
// so coalesce them into one publish rather than one per window.
const DEBOUNCE_MS = 200;
// What lets the daemon notice this extension has died and thaw everything
// (§10.2). At 5s this is 0.2 wakeups/s, which is noise.
const HEARTBEAT_MS = 5000;
const MISSED_ACKS_BEFORE_DOWN = 3;

/** Window types that are furniture rather than an application on screen. */
const APP_WINDOW_TYPES = new Set([
    Meta.WindowType.NORMAL,
    Meta.WindowType.DIALOG,
    Meta.WindowType.MODAL_DIALOG,
    Meta.WindowType.UTILITY,
]);

export default class DozedFocusSource {
    enable() {
        this._seq = 0;
        this._lastPublished = null;
        this._daemonUp = false;
        this._daemonInstance = null;
        this._missedAcks = 0;
        this._debounceId = 0;
        this._windowSignals = new Map();
        this._globalSignals = [];

        this._bus = Gio.DBus.session;
        this._connectGlobals();
        for (const actor of global.get_window_actors())
            this._attach(actor.meta_window);

        this._heartbeatId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT, HEARTBEAT_MS, () => {
                this._onHeartbeat();
                return GLib.SOURCE_CONTINUE;
            });
        this._probeDaemon();
    }

    // Disabling the extension is one of the ways §10.2 gets exercised: every
    // connection goes, no more heartbeats, and the daemon thaws within three.
    // Test it by disabling this while apps are held.
    disable() {
        if (this._heartbeatId) {
            GLib.source_remove(this._heartbeatId);
            this._heartbeatId = 0;
        }
        if (this._debounceId) {
            GLib.source_remove(this._debounceId);
            this._debounceId = 0;
        }
        for (const [obj, ids] of this._windowSignals)
            for (const id of ids) obj.disconnect(id);
        this._windowSignals.clear();
        for (const [obj, id] of this._globalSignals) obj.disconnect(id);
        this._globalSignals = [];
        this._bus = null;
        this._lastPublished = null;
        this._daemonUp = false;
    }

    // ------------------------------------------------------------- signals
    //
    // Read this before adding one.
    //
    // dozed exists to remove background wakeups, so the helper must not add any.
    // A previous tool on this machine leaked one handler per focus change and
    // ended up costing ~1035 wakeups/s across the compositor and the bus — worse
    // than not running at all. Every connection here is recorded and
    // disconnected by id in `disable`.
    //
    // Note what is absent: `notify::title` (dozed does not care about window
    // titles) and `size-changed`/`position-changed`, which fire at frame rate
    // throughout a drag. Geometry is *read* at publish time instead, which is
    // all the occlusion rule needs and costs nothing between publishes.

    _connectGlobals() {
        const track = (obj, name, fn) => {
            this._globalSignals.push([obj, obj.connect(name, fn)]);
        };
        const republish = () => this._schedulePublish();

        track(global.display, 'notify::focus-window', republish);
        track(global.display, 'window-created', (_d, win) => {
            this._attach(win);
            this._schedulePublish();
        });
        track(global.workspace_manager, 'active-workspace-changed', republish);
        track(global.window_manager, 'minimize', republish);
        track(global.window_manager, 'unminimize', republish);
        track(global.window_manager, 'map', republish);
        track(global.window_manager, 'destroy', republish);
        // A finished move or resize can uncover something; the frames in
        // between cannot, as far as anything dozed decides is concerned.
        track(global.window_manager, 'size-change', republish);
    }

    _attach(win) {
        if (!win || this._windowSignals.has(win)) return;
        if (!APP_WINDOW_TYPES.has(win.get_window_type())) return;

        const ids = [
            win.connect('workspace-changed', () => this._schedulePublish()),
            win.connect('notify::minimized', () => this._schedulePublish()),
            win.connect('notify::fullscreen', () => this._schedulePublish()),
            win.connect('notify::demands-attention', () => this._schedulePublish()),
            win.connect('unmanaged', () => this._detach(win)),
        ];
        this._windowSignals.set(win, ids);
    }

    _detach(win) {
        const ids = this._windowSignals.get(win);
        if (!ids) return;
        this._windowSignals.delete(win);
        for (const id of ids) {
            // A window already destroyed throws here, which is fine: its
            // connections died with it.
            try { win.disconnect(id); } catch { /* already gone */ }
        }
        this._schedulePublish();
    }

    _schedulePublish() {
        if (this._debounceId) GLib.source_remove(this._debounceId);
        this._debounceId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT, DEBOUNCE_MS, () => {
                this._debounceId = 0;
                this._publishNow();
                return GLib.SOURCE_REMOVE;
            });
    }

    // ------------------------------------------------------------- payload

    _snapshot() {
        const active = global.display.focus_window;
        const current = global.workspace_manager.get_active_workspace();
        const actors = global.get_window_actors();
        const out = [];

        for (let i = 0; i < actors.length; i++) {
            const win = actors[i].meta_window;
            if (!win || !APP_WINDOW_TYPES.has(win.get_window_type())) continue;
            if (win.is_skip_taskbar()) continue;

            const rect = win.get_frame_rect();
            const ws = win.get_workspace();
            out.push({
                pid: win.get_pid(),
                // §4 rung 3.
                app_id: win.get_wm_class() || '',
                // §4 rung 2, handed over directly — no cgroup parsing needed.
                desktop_file: (win.get_gtk_application_id() || '').replace(/\.desktop$/, ''),
                focused: win === active,
                minimized: !!win.minimized || win.is_hidden(),
                // `get_workspace()` is null for a window on all workspaces.
                on_current_desktop: !ws || !current || ws === current,
                x: rect.x,
                y: rect.y,
                w: rect.width,
                h: rect.height,
                // get_window_actors() is bottom-to-top, which is exactly the
                // ordering the daemon's occlusion rule expects.
                stack: i,
                output: String(win.get_monitor()),
                fullscreen: !!win.is_fullscreen(),
                translucent: false,
                attention: !!win.demands_attention,
            });
        }

        // Sorted so an unchanged desktop always serialises identically, which is
        // what lets the equality check below suppress it.
        out.sort((a, b) => (a.pid - b.pid) || (a.stack - b.stack) ||
                 (a.app_id < b.app_id ? -1 : a.app_id > b.app_id ? 1 : 0));
        return out;
    }

    _publishNow() {
        if (!this._daemonUp) return;
        const windows = this._snapshot();
        const body = JSON.stringify(windows);
        // Most events do not change anything dozed cares about. Sending those
        // would be pure wakeup cost on both ends.
        if (body === this._lastPublished) return;
        this._lastPublished = body;
        this._seq += 1;
        this._call('Windows', JSON.stringify({seq: this._seq, windows}));
    }

    // --------------------------------------------------------------- D-Bus
    //
    // Every argument is a string and every reply is one too, matching the KWin
    // helper. That was forced there by callDBus's unstable numeric marshalling;
    // it is kept here so one sink signature serves both compositors.

    _call(method, arg, onReply) {
        if (!this._bus) return;
        const params = arg === undefined
            ? new GLib.Variant('()', [])
            : new GLib.Variant('(s)', [arg]);
        this._bus.call(SERVICE, PATH, IFACE, method, params, null,
                       Gio.DBusCallFlags.NONE, 2000, null, (bus, res) => {
            try {
                const reply = bus.call_finish(res);
                if (onReply) onReply(reply);
            } catch {
                // Unlike KWin's callDBus we do get told about failures, but the
                // handling is the same: silence counts as a missed ack and the
                // heartbeat decides when that means the daemon is gone.
            }
        });
    }

    _onDaemonAck(reply) {
        this._missedAcks = 0;
        const instance = reply ? reply.deep_unpack()[0] : null;
        // A restart the heartbeat never noticed: a daemon that comes back
        // quickly reclaims the bus name before three beats go unanswered, so
        // from here it looks like the same conversation. Without this the
        // snapshot dedupe would suppress every publish and the new daemon would
        // never learn a single window exists.
        if (instance && instance !== this._daemonInstance) {
            this._daemonInstance = instance;
            this._lastPublished = null;
            this._publishNow();
        }
    }

    _probeDaemon() {
        if (!this._bus) return;
        this._bus.call('org.freedesktop.DBus', '/org/freedesktop/DBus',
                       'org.freedesktop.DBus', 'NameHasOwner',
                       new GLib.Variant('(s)', [SERVICE]), null,
                       Gio.DBusCallFlags.NONE, 2000, null, (bus, res) => {
            let has = false;
            try { has = bus.call_finish(res).deep_unpack()[0]; } catch { return; }
            if (!has) return;
            // The daemon (re)appeared and knows nothing about the current
            // desktop, so force a full publish rather than letting the equality
            // check suppress it.
            this._daemonUp = true;
            this._missedAcks = 0;
            this._lastPublished = null;
            this._daemonInstance = null;
            this._call('Hello', JSON.stringify(
                {protocol: PROTOCOL_VERSION, compositor: 'gnome-shell'}),
                (r) => this._onDaemonAck(r));
            this._publishNow();
        });
    }

    _onHeartbeat() {
        if (!this._daemonUp) {
            // Probing the bus daemon always answers, so this cannot spam the
            // journal the way calling a dead service would.
            this._probeDaemon();
            return;
        }
        this._missedAcks += 1;
        if (this._missedAcks > MISSED_ACKS_BEFORE_DOWN) {
            this._daemonUp = false;
            this._lastPublished = null;
            return;
        }
        this._call('Heartbeat', undefined, (r) => this._onDaemonAck(r));
    }
}
