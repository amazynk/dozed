# dozed focus source — KWin script

Tells the `dozed` daemon which applications the user can actually see. Focus
alone is not enough: a window on the second monitor is unfocused but fully
visible, and freezing it leaves a stale rectangle that reads as a crash
(`DESIGN.md` §5).

## Direction of the protocol

**The daemon owns the D-Bus name; this script calls into it.** That is forced,
not chosen. The KWin 6 script sandbox exposes exactly these globals:

```
QTimer, KWin, options, workspace, readConfig, callDBus, registerShortcut,
registerScreenEdge, unregisterScreenEdge, registerTouchScreenEdge,
unregisterTouchScreenEdge, registerUserActionsMenu, assert*
```

`callDBus` is a client. There is no way for a KWin script to register a service,
so the original `DESIGN.md` §12 shape — a script publishing
`org.dozed.FocusSource1` for the daemon to consume — cannot be built.

The daemon therefore owns `org.dozed.Daemon` and implements
`org.dozed.FocusSink1` at `/org/dozed/FocusSink1`:

| Method | Signature | Meaning |
|---|---|---|
| `Hello` | `(s json)` | Sent once whenever the script finds the daemon. The JSON carries `protocol`, the protocol version (currently 2), and `compositor`. |
| `Windows` | `(s json)` | A full snapshot of every application window. |
| `Heartbeat` | `() → s` | Liveness. Returns the daemon's instance token, so the script can tell a restarted daemon from the one it greeted. |

`Windows` carries JSON rather than a D-Bus struct array because marshalling
nested JS arrays into `a(usssbbu)` through `callDBus` is fragile, and a
versioned JSON blob is far easier to extend and to test:

```json
{"seq": 7, "windows": [
  {"pid": 283751, "app_id": "org.wezfurlong.wezterm",
   "desktop_file": "org.wezfurlong.wezterm", "visibility": "focused",
   "output": "DP-1", "fullscreen": false, "attention": false}
]}
```

Snapshots are always complete, never deltas, so a daemon that restarts or drops
a message resynchronises on the next publish. One window per entry: an app with
several windows appears several times, and aggregating to a per-app visibility
(the maximum, per §5) is the daemon's job because only the daemon knows how PIDs
map to an `AppKey`.

### Liveness in both directions

- The daemon watches `Heartbeat`. Three missed in a row means it can no longer
  see focus, so it must thaw everything (`DESIGN.md` §10.2).
- The script watches for replies. `callDBus` only invokes its callback on a
  successful reply, so a silent callback is the only available failure signal.
  After three unanswered heartbeats the script marks the daemon down and
  switches to polling `NameHasOwner` on the bus daemon, which always answers and
  so cannot spam the journal. When the daemon reappears the script sends `Hello`
  and force-publishes a full snapshot, because a restarted daemon knows nothing
  about the current desktop.

A daemon that dies costs exactly three `"The name is not activatable"` lines in
KWin's journal before the script goes quiet. That is bounded, not a loop.

## Install

```bash
kpackagetool6 -t KWin/Script -i helpers/kwin        # or -u to update
kwriteconfig6 --file kwinrc --group Plugins --key dozed-focus-sourceEnabled true
qdbus6 org.kde.KWin /KWin org.kde.KWin.reconfigure
```

Or enable it from System Settings → Window Management → KWin Scripts.

## Uninstall

```bash
kpackagetool6 -t KWin/Script -r dozed-focus-source
qdbus6 org.kde.KWin /KWin org.kde.KWin.reconfigure
```

## Develop without installing

`loadScript` runs a script straight from a path and does not touch `kwinrc`, so
nothing persists across a KWin restart:

```bash
gdbus call --session --dest org.kde.KWin --object-path /Scripting \
    --method org.kde.kwin.Scripting.loadScript "$PWD/helpers/kwin/contents/code/main.js" dozed-dev
gdbus call --session --dest org.kde.KWin --object-path /Scripting \
    --method org.kde.kwin.Scripting.start
# ... and to remove it again:
gdbus call --session --dest org.kde.KWin --object-path /Scripting \
    --method org.kde.kwin.Scripting.unloadScript dozed-dev
```

**A script's exceptions are invisible unless you look.** They are swallowed
inside the signal handler and only appear in KWin's log:

```bash
journalctl --user -f | grep -E 'js:|kwin_scripting'
```

## Not becoming the problem

`DESIGN.md` §13 applies to the helper too. A previous tool on this machine leaked
one signal connection per focus change and ended up making ~824 D-Bus calls/s,
costing `kwin_wayland` ~697 wakeups/s and `dbus-broker` ~338/s. A focus reporter
that burns 1000 wakeups/s to save a few hundred is worse than not running.

So:

- Per-window signals are connected once on `windowAdded` and disconnected on
  `windowRemoved`, guarded by a registry of attached windows. KWin's `connect()`
  returns `undefined` rather than a handle, so disconnecting works by
  `(signal, handler)` identity — which is why every window shares one handler
  function.
- `captionChanged` is never connected. dozed does not care about window titles,
  and it is the highest-frequency signal there is.
- `frameGeometryChanged` is never connected either: it fires at frame rate for
  the whole of a drag. Occlusion changes are picked up from `maximizedChanged`,
  `tileChanged` and `interactiveMoveResizeFinished` instead.
- Publishes are debounced by 200 ms and suppressed entirely when the serialised
  snapshot is unchanged, so a resize that uncovers nothing costs no IPC.

Measured on a live Plasma 6.6.6 session, idle with 10 windows: **1 publish and 8
heartbeats over 45 s**. Against `kwin_wayland`'s ~200 wakeups/s baseline the
script's own cost was not distinguishable from sampling noise, which is the
expected result for a single 5 s timer.

## Known gaps

- **Activation is approximated.** `DESIGN.md` §10 wants to thaw on single-instance
  D-Bus activation, so that clicking a launcher for a frozen app does not hang
  against the frozen process. KWin exposes no such signal; the best available
  proxy is `demandsAttention`, reported here as the `attention` flag. Genuine
  activation interception has to happen daemon-side, and eavesdropping on the
  bus is not possible under `dbus-broker` without special configuration. This
  gap is unresolved.
- **Occlusion is single-window containment**, not union coverage: a window
  counts as hidden when one window above it covers it completely. Two windows
  jointly covering a third will not be detected. Union coverage costs much more
  to compute for a case that is rare, and a false positive here means freezing
  something the user can see.
- **Restacking without activation** is not observed. Occlusion is recomputed on
  activation, maximise, tile, move-finished, desktop and output changes, which
  covers the realistic paths.
