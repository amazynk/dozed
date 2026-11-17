# dozed — design

`dozed` freezes backgrounded desktop applications so they stop waking the CPU, and
thaws them the instant you switch back. It targets **Linux: Wayland (KDE, GNOME, Sway) and X11**,
is **opt-in by default**, and optimises for **energy that is being wasted**.

The name is borrowed from Android's *Doze* mode, which does the same job for phones.

---

## 1. The premise

**The problem is wasteful software, not a short battery.** A hidden Slack waking the CPU
900 times a second is not doing work anybody asked for. It is doing it whether the machine
runs on a battery, a charger, or a wall socket it has never been unplugged from — and
somebody burns fuel for all three. The battery is simply where the waste is easiest to
*notice*, because there the meter is in the corner of your screen and it moves.

That distinction is not decoration; it decides the defaults. A tool aimed at endurance
switches itself off when the charger goes in, which is most of the working day for most
laptops and the entire life of every desktop. A tool aimed at waste does not, because the
waste did not stop. So `on_ac` defaults to `"enabled"` (§8), and being plugged in buys
*patience* — `ac_factor`, a doubled grace — rather than a veto.

What follows from that, by machine:

| Machine | What is being saved | How dozed behaves |
|---|---|---|
| **Laptop, on battery** | Runtime you can feel, plus the waste | Most aggressive, and it sharpens as the charge falls: the grace is proportional to charge, so 50% halves it and 10% is a tenth. A five-minute rule fires in thirty seconds on a nearly-flat battery. |
| **Laptop, plugged in** | The waste, the fan, and the frames | Acts, at `ac_factor` (2x) patience. There is no runtime to win, so nothing is urgent — but an idle app spinning a core still costs power, still spins the fan, and still steals frames from whatever you are actually doing. |
| **Desktop, no battery** | The waste, the fan, and the frames | Same as a plugged-in laptop, and this is the machine that gains most over a week: it is never unplugged, never suspended, and routinely left with fifty tabs and a chat client open overnight. Under the old default it got nothing at all. |

The three rows differ only in the power term of the grace product (§8). One mechanism, one
config, no special cases — a desktop is a laptop that is always on AC, and dozed treats it
as exactly that.

**Two costs the charger never absorbed**, and which apply to every row above: a fan is
audible whoever is paying for it, and a dropped frame is dropped whoever is paying for it.
Rules naming `thermal` or `other_app_fullscreen` therefore ignore the AC policy entirely
(§7.3) — that was true even when the default was to do nothing on mains, and it is the
seed the wider premise grew from.

### Goals

* Cut background CPU **wakeups**, which is what turns an idle machine into a warm one —
  the direct physical consequence is a CPU that never reaches a deep idle state, and that
  is measurable on mains as readily as on a battery (§11.1).
* Never make the desktop feel broken. A user should not be able to tell `dozed` is
  running except that the machine is cooler, quieter and lasts longer.
* Be **explainable**. Every action maps to exactly one named rule, and the user can ask
  `dozectl why slack` at any moment and get a straight answer.
* Be **provable**. Dry-run mode must produce a credible estimate of what live mode would
  have saved, so the user can decide to switch it on with evidence rather than faith.

### Non-goals

* Freeing RAM. Freezing does not release memory. (We do optionally ask the kernel to
  reclaim a frozen app's pages — see §7 — but that is a bonus, not the point.)
* Cross-platform support. The action layer and the focus layer are both deeply
  OS-specific. macOS and Windows would be separate backends; they are out of scope.
* Managing processes belonging to other users, or anything requiring root.

### Hard constraints

* **Same-UID only.** `dozed` never touches a process it does not own. This keeps the
  whole thing unprivileged and keeps the security story short.
* **No `unsafe`.** Every crate in the workspace is `#![forbid(unsafe_code)]`.
* **MSRV 1.90**, edition 2024, resolver 3.

---

## 2. Components

| Component | Kind | Job |
|---|---|---|
| `dozed-core` | lib | Pure domain logic. No I/O, no syscalls, no clock. |
| `dozed-linux` | lib | Everything OS-specific, behind traits from core. |
| `dozed` | bin | The daemon. Wires sources → planner → actuator. |
| `dozectl` | bin | CLI: status, why, watch, report, thaw. |
| `dozed-gui` | bin | egui dashboard: live timeline, per-app control, config editor. |
| `helpers/kwin` | KWin script | Publishes focus/visibility on D-Bus (KDE). |
| `helpers/gnome` | Shell extension | Same, for GNOME (45+, ESM). |

Paths follow XDG: config `~/.config/dozed/config.toml`, state
`~/.local/state/dozed/`, runtime socket `$XDG_RUNTIME_DIR/dozed.sock`.

---

## 3. Architecture: the dry-run seam

The single most important structural decision is that **deciding and acting are
different objects**, connected by a stream of `Intent` values.

```
  ┌──────────────┐
  │ FocusSource  │─┐                                    ┌─ DryRunActuator ── records only
  │ ActivitySrc  │─┤                                    │
  │ PowerSource  │─┼─► Event ─► Planner ─► Vec<Intent> ─┤
  │ ProcSampler  │─┤            (pure)         │        │
  │ Clock/Ticks  │─┘                           │        └─ LiveActuator ──── cgroup / signals
  └──────────────┘                             │
                                               └──► Journal ──► IPC ──► dozectl / GUI
```

Consequences that make this worth it:

* Dry-run is **not a simulation**. It is the identical intent stream with a no-op
  actuator, so it cannot drift from what live mode would do.
* The journal — the thing the user reads to build trust — is produced in both modes and
  is byte-identical apart from the outcome field.
* The planner is a pure function, so it is unit-testable and replayable (§14).

### Modes

Mode is resolved per app: `effective_mode = app_override.unwrap_or(global_mode)`.

* `dry-run` — **the default.** Intents are journalled with `outcome = WouldHave`. Nothing
  is touched.
* `live` — intents are executed.
* `monitor` (per-app only) — force dry-run for one app while the rest run live. Useful
  for an app you have not built confidence in yet.

`dozed --dry-run` / `dozed --live` on the command line override the config file.
There is no way to enter live mode by accident.

### Snooze: the mode that puts itself back

Both modes above are permanent, so "stop touching my apps for the next hour" — the
presentation, the build, the call — had no answer except switching the daemon off. People
do that, and then they forget, and a tool that is switched off and forgotten saves nothing
and does not even leave a recording behind. `dozectl snooze 1h` is the honest version:
release everything, stop acting, and go back on a timer.

It is implemented as a **mode change with a deadline**, not as a new state the planner
knows about, and that is what keeps the dry-run seam (§3) intact. Snoozing takes exactly
the path `dozectl mode dry-run` takes, releasing every hold on the way in; expiry takes the
path going live takes, through `Planner::reassert`. The planner never learns that snooze
exists.

Three details that are each a bug avoided:

* **The restore mode is captured once**, at the moment of snoozing. Snoozing again extends
  the timer and keeps the original — otherwise the second snooze records *dry-run* as the
  thing to return to, and an hour later the daemon comes out of it switched off.
* **An explicit `mode` command cancels rather than ends it.** Ending it would restore the
  old mode straight over the top of the choice just made, undoing a deliberate action
  minutes after it was taken.
* **The snooze deadline is folded into the tick interval.** Nothing held and nothing
  pending is precisely the state that earns the 60s idle interval, and a snooze is that
  state by construction, so without this an hour's snooze quietly runs a minute long.

It is refused while a trial is running, and the refusal is the point: a trial *is* a
schedule of mode changes, and a snooze overwriting its live blocks with dry-run ones would
not produce a wrong measurement but a confident one — dry-run against dry-run, reported as
proof that dozing does nothing.

---

## 4. App identity: `AppKey`

PIDs are useless across restarts and rules must survive reboots, so every rule is keyed
on a stable `AppKey`. No single source is sufficient, so identity is a **resolver chain**
that records *which* method matched, so the UI can explain itself.

Order, first match wins:

1. **Flatpak / Snap id** — `com.slack.Slack`. Stable, exact, per-app. Read from
   `/proc/<pid>/cgroup` (`app-flatpak-com.slack.Slack-1234.scope`) or `/.flatpak-info`.
2. **Desktop file id** — from the systemd scope name `app-gnome-slack-1234.scope`, or
   from `XDG_ACTIVATION_TOKEN`/`GIO_LAUNCHED_DESKTOP_FILE`. Distinguishes Chrome PWAs and
   separate Firefox profiles, which users expect to be separate apps.
3. **Wayland `app_id`** — reported by the compositor helper alongside the focus event.
4. **Executable path** — `/proc/<pid>/exe` resolved. Last resort; lumps all Electron apps
   that share a wrapper.
5. **Comm** — `/proc/<pid>/comm`. Only when `exe` is unreadable.

```rust
pub struct AppKey {
    pub kind: IdentityKind,  // Flatpak | DesktopId | AppId | ExePath | Comm
    pub value: String,
}
```

`AppKey` is what appears in config files. The daemon also keeps the raw `IdentityEvidence`
(all candidates it found) so `dozectl why` can print "matched `com.slack.Slack` by flatpak
id; also saw exe=/app/bin/slack, app_id=Slack".

### Suspension unit

An app is a **process tree**, never a PID. `SIGSTOP` on a parent does not stop children,
and browsers are 20+ processes. The unit of action is, in order of preference:

1. The app's **cgroup v2 scope** (`/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-*.scope`),
   **but only when that scope holds this app and nothing else.** Freezing a
   cgroup is atomic and survives `fork()` races, which is why it is preferred —
   but a scope is not automatically a per-app boundary. An app launched from a
   terminal inherits the terminal's scope: measured on this machine, an editor
   and the terminal that spawned it shared
   `app-net.local.wezterm@….service`, and both reported bit-identical CPU
   because both were reading that shared scope's `cpu.stat`. Freezing it because
   the editor went idle would have taken the terminal down too. The daemon
   therefore checks `cgroup.procs` against the app's own process tree and falls
   back to rung 2 when the scope is shared.

   The check runs in **both directions**, and for a while it did not. Asking
   only "does this scope hold a stranger?" misses the scope that holds *part*
   of an app: Brave launched from a wezterm window put its browser and audio
   processes in `app-org.chromium.Chromium-1450786.scope` and left the other
   eleven — GPU, network, storage, three zygotes, four renderers, the launcher
   shell — behind in the terminal's cgroup. No strangers, so the scope passed;
   the freeze reached 2 of 13 processes, and `dozed` reported the app as dozing
   while 246 wakeups/s went on for seven hours. This is the same failure as the
   single-root `SIGSTOP` bug in §16 — *"43 processes running while the app was
   reported as dozing"* — reached down the cgroup path instead, and it is worse
   than doing nothing: live renderers doing IPC against a frozen browser
   process is how Chromium hangs. A scope is rung 1 only when `cgroup.procs`
   and the process tree are the same set, ignoring pids that exited between the
   walk and the read. Otherwise rung 2, which reaches every process wherever it
   lives.
2. The **process tree** rooted at the app's main PID, walked from `/proc`, as a fallback
   for apps not in a tidy scope.

### A shared scope must not name an app either

The same caveat applies to *identity*, and applying it only to actuation was a bug. Half
an hour of real use produced this: a program called `rex`, launched from a wezterm window,
had no scope of its own, inherited the terminal's, and was therefore given the `AppKey`
`desktop:net.local.wezterm@5a146fdb7c194b17944068c8f38aa857`.

Three things wrong with that, in increasing order of seriousness. It is the terminal's
name, not the app's. It carries a per-instance hash, so a rule written against it breaks
on the next terminal launch — which defeats the entire purpose of a stable key. And every
*other* program launched from that terminal would collapse into the same identity and
share one freeze decision.

So a scope contributes a name only when `cgroup.procs` is a subset of the app's own
process tree — the same test rung 1 already applies. Otherwise identity falls through to
`app_id`, `exe` or `comm`, all of which at least describe the process in front of us.
`rex` now resolves to `app-id:rex`.

---

## 5. Windows, visibility, and what "backgrounded" means

Focus is not the right signal on its own. On a multi-monitor setup the window on your
second screen is unfocused but **fully visible**; freezing it leaves a stale, half-drawn
window on screen, which reads as a crash. So the compositor helper reports a three-state
visibility per app:

| State | Meaning | Freezable? |
|---|---|---|
| `Focused` | Has input focus | Never |
| `VisibleUnfocused` | On screen but not focused (other monitor, tiled beside) | Opt-in per app |
| `Hidden` | Other workspace, minimised, or fully occluded | Yes (default) |

An app's visibility is the **maximum** over its windows: any focused window ⇒ `Focused`.

This is also why the helper must report workspace and minimised state, not just the
active window — speccing that now costs nothing and retrofitting it costs a rewrite of
both helpers.

### The rule lives in the daemon, not in the helpers

There are three producers of this judgement — a KWin script, a GNOME Shell extension and
the daemon's native X11 source — and two languages. Three implementations of one rule
with no way to notice them drifting apart is how a window ends up frozen on one desktop
and not another.

The objection to centralising it was that helpers would then have to *stream* geometry,
and `frameGeometryChanged` fires at frame rate throughout a drag — precisely the signal
the KWin helper refuses to connect, because a previous tool on this machine leaked
handlers into ~1035 wakeups/s.

**That objection does not apply, and checking the helper is what showed it.** The KWin
script never subscribed to geometry: it *reads* `frameGeometry` and `stackingOrder` inside
its occlusion check, at snapshot time, and publishes on the discrete events that change
occlusion — maximise, tile, move-finished, restack. Moving the computation into the daemon
means putting geometry it already read into a payload it already sends. No new signal
connections, the same publish triggers, and exactly the same staleness between publishes.

So protocol 2 reports facts — focused, minimised, on-current-desktop, geometry, stacking
index, output, translucency — and `dozed_core::visibility` decides. One implementation,
one set of tests, and `crates/dozed-core/tests/visibility_fixtures.rs` is the table of
desktop situations every source must agree on. Protocol 1 helpers still work: a helper
that sends its own verdict and no geometry is believed rather than second-guessed.

Anything the rule cannot compute resolves to *on-screen*, never to hidden. A platform that
will not say where a window is must not thereby get it frozen.

---

## 6. Per-app state machine

```
                  ┌──────────────────────── focus/visible ────────────────────────┐
                  │                                                               │
             ┌────▼────┐   hidden      ┌────────┐  grace expires   ┌───────────┐  │
   spawn ───►│ Running │──────────────►│ Grace  │─────────────────►│  Dozing   │──┘
             └────┬────┘   + eligible  └───┬────┘   + still        └─────┬─────┘
                  │                        │         eligible            │
                  │  cpu rule fires        │ exemption appears           │ exemption
                  │  (throttle)            │ or becomes visible          │ appears
             ┌────▼─────┐                  │                             │
             │Throttled │◄─────────────────┴─────────────────────────────┘
             └──────────┘
```

* `Grace` is a timer, not a state with side effects; entering it emits no action.
* Every transition carries the `RuleId` that caused it, which is what the journal prints.
* `Exempt` is not a state but a predicate evaluated at decision time (§9), so it can
  appear and disappear without transitions.

---

## 7. The action ladder

Freeze/run is too blunt for a conservative tool. cgroup v2 gives us a spectrum, and the
right rung is usually not the last one:

| Rung | Mechanism | Effect | Good for |
|---|---|---|---|
| `Nice` | `setpriority` | Deprioritise | Barely worth it; included for completeness |
| `Weight` | `cpu.weight` | Loses CPU under contention | Responsiveness, not battery |
| `Throttle{pct}` | `cpu.max` | Hard cap, e.g. 5% of one core | **Chat apps.** Stays connected, still notifies, cannot spin |
| `ThrottleTo{n}` | `cpu.max`, moving | Cap converges on a wakeup budget | An app whose right percentage you do not know |
| `Freeze` | `cgroup.freeze` | Fully stopped | Hidden and genuinely idle apps |
| `DozeCycle{every,wake}` | `cgroup.freeze`, on a shared schedule | Stopped, with an aligned maintenance window | **Chat apps**, if a periodic reconnect beats a permanent cap |

`Throttle` is the recommended first rung for anything network-facing: Slack still receives
messages, it just cannot burn 8% CPU doing nothing. `Freeze` is for apps you are not
using at all.

### 7.1 `DozeCycle`: the half of Doze this ladder was missing

The name of this project is borrowed from Android's *Doze*, and Doze's saving comes as much
from **batching** as from suppression: hold everything, then release all the deferred work
at once, in a maintenance window. Only the suppression half was here.

The gap it fills is visible in the table above: chat apps are recommended `Throttle{5}`
rather than `Freeze`, because a frozen one drops its connection and reconnects on thaw. So
the choice was between paying a reconnect on every hide and never really stopping the app —
and a permanently capped Slack is still scheduled, still waking, just less often.

A cycle makes it one reconnect per period for an app that is genuinely stopped the rest of
the time. `every = "15m", wake = "20s"` is stopped for 98% of the period.

**The windows are aligned, and that is the whole point.** A frozen app that wakes on its own
timer *is a wakeup*; six of them on six timers is six bursts scattered through the hour,
which is most of what dozing removed. So the schedule is global: it ticks at the shortest
`every` in force, and each app joins the tick once its own period has elapsed. A 60-minute
app's window lands on top of a 15-minute app's rather than somewhere else. This is timer
coalescing, one level up, and it is the reason the implementation is a planner-wide schedule
rather than the per-app `wake` timer that would have been much easier to write.

Four consequences worth stating, each of which is a test:

* **The window has the floor.** Inside it the app's own rule still wants it frozen, so the
  ordinary path is suppressed for exactly the apps in the window — otherwise the next tick
  puts them straight back and the window lasts one interval.
* **The user outranks the window.** A restore the rules asked for — the user came back, an
  exemption appeared — ends the cycle, and the next hide starts a fresh one with grace
  measured from scratch. An app that is visible when the window closes is not re-frozen.
* **`next_deadline` includes the window boundary**, or a window due in 30s would be opened
  whenever the 60s idle tick next came round: a late window, whose length is then wrong by
  however late it was.
* **`wake >= every` is a config error.** Such a rule never actually dozes anything, and
  there is nothing sensible to clamp it to.

The shipped presets are **unchanged**: `chatty-apps` stays at `throttle = 5`. Whether a
window beats a cap depends on how the app behaves on reconnect, which is a property of the
app and the network rather than of this design — and §11.2 now provides the way to settle
it on a particular machine instead of arguing about it:

```toml
[[rule]]
name     = "chatty-apps"
priority = 50
action   = { doze-cycle = { every = "15m", wake = "20s" } }
```

then `dozectl trial start` under each version, and compare.

### 7.2 A rung below the whole tree, rejected on the measurement

The next rung down suggests itself: an Electron app is a main process plus a dozen
renderers, so freeze the renderers and leave the process holding the socket alive. That
would be a better `Throttle` than `cpu.max` — no cap to choose, no CPU left to spin with —
and it needs no new mechanism, since the freezer already takes a set of PIDs.

Measured before building it, on a real hidden Signal, wakeups attributed by Chromium
process type:

| Role | Processes | Share of wakeups |
|---|---|---|
| `renderer` | 1 | 42% |
| `utility` | 2 | 39% |
| `gpu-process` | 1 | 14% |
| `main` | 10 | **6%** |
| `zygote` | 3 | 0% |

The measurement kills the design, in two ways.

**The socket is not in the main process.** Chromium's network stack runs in a *utility*
process (`--utility-sub-type=network.mojom.NetworkService`), so "freeze everything except
the main process" would freeze the network service and lose the connection anyway — which
was the entire justification. The rule would have to be "freeze everything except the
utility process that happens to be the network service", which is Chromium's internal
architecture encoded in a battery daemon, and which stops working silently the day a
process type is renamed.

**There is no dominant share to win.** Renderers are 42% of the wakeups, not 90%. Freezing
them and nothing else leaves more than half the cost in place, against a rung whose failure
modes include Chromium's own hang detection killing a renderer that stopped answering IPC,
and the partial process graph deadlocking on a synchronous call into a frozen child.

So the rung is **not built**, and §7.1's `DozeCycle` reaches the same goal from the other
direction: a chat app that is genuinely and wholly frozen for 98% of the time, with no
knowledge of anybody's process architecture.

What the investigation did leave behind is the diagnostic that made it possible.
`ProcContrib` already reported which process inside an app was busiest — and for exactly
the apps that question is asked about, it could not answer: **all seventeen of Signal's
processes report `comm = signal-desktop`**, so the breakdown was seventeen identical rows.
The role from `--type=` is now carried alongside, and `dozectl why <app>` prints the
breakdown, because "which of these is it" is asked at the moment somebody has already
decided a number is surprising.

### 7.3 Two objectives that are not endurance

These predicates predate the wider premise in §1 and are what argued for it. When `on_ac`
still defaulted to `"disabled"`, dozed did nothing for most of the day on most desks — and
two of the costs a background app imposes are not paid by the battery at all, so silencing
them along with the endurance rules was never defensible. Both remain exempt from the AC
policy however it is set, which matters now only for someone who has opted out.

**Noise.** A fan is a cost somebody is listening to whether or not the wall is supplying
the power. `thermal = "hot"` is the predicate.

**Dropped frames.** While a game, a video or a call has the screen, the cost of a
background app is not milliwatts but a stutter. `other_app_fullscreen = true` is the
predicate, and the word *other* is its entire content: it can never match the fullscreen app
itself, which would freeze the very thing the rule exists to protect. It is already covered
by `fullscreen-exempt`, and relying on one rule to save another from itself is not a safety
argument.

Naming either predicate opts the rule out of the AC policy. That follows the precedent
`focused_for` already set — carrying a predicate is what exempts a rule from the resume
cooldown — so the opt-out is a consequence of what the rule *says* rather than a separate
switch that could disagree with it. The AC *factor* still applies: a plugged-in machine
deserves more patience, just not a veto.

```toml
[[rule]]
name = "quiet-down"
priority = 40
action = "freeze"
[rule.when]
visibility = "hidden"
thermal    = "hot"
for        = "1m"

[[rule]]
name = "clear-the-decks"
priority = 45
action = { throttle = 5 }
[rule.when]
other_app_fullscreen = true
for                  = "30s"
```

#### The fan is a signal too, not just the temperature

A temperature threshold alone calls a machine *normal* while its fan is audible across the
room: 70 °C is unremarkable for a laptop under load, and 70 °C at full fan is exactly the
case the quiet objective exists for. The complaint is noise, and the fan is the thing making
it, so either signal can raise the alarm.

Every sensor is read and the **maximum** taken, not the first. Observed: two hwmon devices
describing the same physical fan with one of them reporting `0`, so "the first sensor" is a
coin toss decided by directory order.

Two ways a sensor produces something that is not data:

* A sensor can be present and unreadable — a `temp*_label` reading `GPU` beside a
  `temp*_input` that fails with `ENXIO`. A failed read is skipped rather than counted as
  zero, because a zero drags a maximum down and reads as a cool machine.
* `thermal_throttle` counters exist on some platforms and not others, so throttle detection
  is not part of the signal at all, rather than part of it on some hardware.

#### A hot drive does not speak for the machine

Taking the maximum across *all* temperature sensors let a hot NVMe speak for the CPU.
Checked on one machine at its hottest, the top six sensors were all the CPU (99 °C) with
wifi at 70 °C and the GPU at 67 °C — which is why this went unmitigated for a while. But
that is one machine at its *hottest*, and the interesting case is the opposite one: an idle
laptop whose drive is doing a large write. An NVMe passes 70 °C there as a matter of
course, and against the shipped `hot_celsius` that pins `thermal = hot` on a machine whose
CPU is at 45 °C and whose fan is barely turning.

The consequence is the one this section already has a scar from. A `thermal` rule keyed on
a signal that is permanently true is not a rule at all, and from inside a rule a constant
and a signal look identical — the same failure `hot_rpm = 3000` produced, arriving down a
different path. `Monitor::saturated` would eventually notice and say so, four hours later,
which is the right backstop and the wrong place to leave it.

So `dozed_linux::thermal::NOT_THE_MACHINE` drops, at discovery, the hwmon devices whose
temperature describes hardware with its own thermal envelope: storage (`nvme`,
`drivetemp`) and radios (`iwlwifi`, `mt79xx`, `ath1xk`, `rtw8x`). Matched as a prefix,
because these arrive enumerated or carrying a phy index.

**A denylist rather than an allowlist**, and that is the load-bearing choice. An allowlist
of CPU sensor names (`k10temp`, `coretemp`, `zenpower`, …) fails in the expensive direction
the first time it meets hardware nobody tested: nothing matches, the temperature half of
the signal disappears entirely, and the machine reports no temperature rather than a wrong
one. A denylist that misses somebody's hot drive leaves this exactly where it was before the
list existed. The two errors are not the same size. The same reasoning §9 gives for the
long-job list, pointed the other way: there, a missing name risks the worst failure, so err
long; here, a *present* name risks it, so err short.

Only the temperature is dropped — a fan is a fan whatever is beside it on the bus — and a
device with an unreadable `name` is kept, since the list can only ever remove sensors and
an unnamed CPU sensor dropped for being unnamed is the allowlist failure by the back door.
`dozectl status` prints what was dropped, because a sensor silently missing from a maximum
is indistinguishable from a sensor that is not there, and one of those two possibilities is
this list being wrong about somebody's hardware.

#### Degrees are portable; RPM is not

**This is the part that has to be a rule about machines rather than about a machine.** 80 °C
is 80 °C on anybody's silicon, so an absolute temperature threshold is a claim that travels.
3000 RPM is idle on one fan and redline on another, so an absolute *fan* threshold is a claim
about one manufacturer's cooling wearing the costume of a general setting.

That distinction was learned the expensive way. `hot_rpm = 3000` was measured from a laptop
whose fan idled near zero, and shipped. On a machine whose fan idles at **3161** it sits below
the entire operating range: the state read `hot` for 2,977 of 3,125 recorded decisions and
`normal` for 148, making every rule keyed on `thermal` silently unconditional. Worse, the fan
half of the pair also *blocks* recovery — clearing requires `cool_temp && cool_fan`, so a fan
permanently above `cool_rpm` overrides a temperature that has genuinely come down. That
machine's temperature fell to 48.85 °C during the stuck stretch and it changed nothing.

So the fan thresholds are **not shipped at all**. `FanRange` watches the speeds this fan is
actually seen to use and places the pair inside that span — hot in the top third, cool at
40%, the gap being the usual hysteresis. Fractions of an observed range mean the same thing on
every machine in a way an RPM cannot.

Three properties keep that honest:

* **Silence until it has evidence.** Before a range is known the fan contributes nothing in
  either direction — the identical branch a fanless machine takes — and the temperature
  decides alone. Re-learned per run rather than persisted, because a calibration from a
  session on a desk outliving one on a lap is the same failure as a threshold outliving its
  measurement.
* **A fan that does not vary is not a signal.** Under 600 RPM of observed span there is no
  meaningful "loud", so no threshold is invented inside the noise.
* **It is visible.** `dozectl status` prints the pair in force, where it came from, and the
  range seen so far. A learned threshold nobody can see would be exactly as unexplainable as
  a hardcoded one, and being unexplainable is what made the original number expensive.

#### A fan that stops is not a data point on the loudness scale

The calibration above removed the *shipped* wrong number and then reproduced its effect,
which is worth recording in full because the mechanism was built specifically to prevent it.
Measured on a real machine over three days:

| | |
|---|---|
| Observed fan range | 0 – 4518 rpm |
| Learned `cool_rpm` (40% of it) | **1807** |
| Quietest the fan managed, over three separate 4-hour stretches | 2114, 2188, 2360 |
| Temperature during those stretches | 43.25, 44.63, 47.75 °C |
| `cool_celsius` | 72.0 |

The machine was **cold** and read `hot` for hours at a time. Clearing needs both signals, and
the fan never returns to 1807 once it is turning — which is word for word the failure this
section already describes for `hot_rpm = 3000`: *"a fan permanently above `cool_rpm`
overrides a temperature that has genuinely come down. That machine's temperature fell to
48.85 °C during the stuck stretch and it changed nothing."* It is now 43.25 °C and it changes
nothing. `Monitor::saturated` caught it and said so three times, which is the part that
worked.

The zero was a legitimate reading. This laptop's fan genuinely stops, and `acpi_fan` reports
a permanent 0 beside the `thinkpad` sensor that carries the real speed. One moment of the fan
being off anchors the bottom of the range for the rest of the run.

The flaw is in the model rather than in the reading. Fractions of an observed span assume the
span is a *scale*, and a fan that is either off or turning at 2000+ has nothing in between:
it is bimodal, and "40% of the way from stopped to maximum" is a speed it never uses. So
`FanRange` now learns from the speeds the fan is seen to **run** at and leaves the stopped
readings out — a fan that is off is not a quiet fan, it is not a fan at all, and "how loud"
has no answer there.

It still contributes to the *reading*: a fan reported at 0 is below every threshold and
clears the state, unchanged. This is only about what the scale is drawn against. On the
numbers above it puts the range at 2114–4518, `cool` at 3076 and `hot` at 3717 — so the
quietest stretches clear and a fan at 4076 is still loud.

A user who knows their hardware can still pin both — and must pin *both*, since half a pair
leaves the other half moving underneath it. `Config::warnings` says so.

The state is **hysteretic** — hot at 80 °C or 3000 RPM, normal again only below 72 °C and
2200 RPM, and *both* must fall. Every transition is an event, a re-plan and possibly a thaw
and a re-freeze, so a fan hovering at a single threshold must not produce a stream of them.
The asymmetry is deliberate: staying hot slightly too long freezes a background app on a
quiet machine, while clearing too early never lets the fan slow down.

Read on the existing sampling tick, like the power counters, so it adds no wakeups; and
`Monitor::poll` returns a state only on a transition, so an idle machine puts nothing in the
trace.

The presets are unchanged: both of these are opt-in, because they are policies about what a
particular person finds annoying.

#### Saying so when a threshold cannot be reached

Calibration removes the worst version of this, but not all of it: a pinned pair can still be
wrong, a machine can genuinely run hot for a long time, and `hot_celsius` is still an
absolute number that some hardware will sit above all day.

The failure to catch is *silence*. When a threshold cannot be reached, the rule keyed on it
becomes unconditional — and from inside a rule a constant and a signal look identical. There
is no reading, no log line and no status field that distinguishes "this predicate is doing
work" from "this predicate is always true". In the 13-day journal that prompted this, nothing
anywhere would have said so.

So `Monitor::saturated` says so. Two conditions, both required:

* the state has been `hot` without interruption for **four hours**; and
* the best each signal managed *separately* in that stretch would still not have cleared it.

The second is not implied by the first. Clearing needs both signals cool in the *same*
reading, so a machine whose fan drops to a whisper while the package is hot, and cools off
while the fan is still spinning down, never clears a single reading — yet both its
thresholds are demonstrably reachable. Taking the minimum of each signal independently and
asking whether *that* would have cleared it is what tells the two apart.

Four hours, and the length is load-bearing. Half an hour was tried first and flagged a
machine sitting at a genuine 85 °C, which is not a broken threshold, it is a hot machine.
No reading can separate "unreachable threshold" from "genuinely hot for four hours", so the
report deliberately claims only the **consequence**, which is the same either way: every
`thermal` rule has been firing unconditionally for four hours, and here are the readings
that say which cause it was.

Reported, not corrected. The thresholds are the user's to set, and a daemon that quietly
retunes the number it is judged by has removed the evidence rather than the problem.

### `ThrottleTo`: a cap chosen by measurement

`action = { throttle = 5 }` picks a percentage and hopes. `action = { throttle-to = 50 }`
states the thing you actually care about — "this app should cost about 50 wakeups a
second" — and the cap moves until it does: `cap *= target / measured`, evaluated on each
sample. Both quantities are rates, so the correction is a ratio rather than a difference.

Three properties make it safe to point at a live app:

* **Damped.** The sample is an average over `cpu_sample_window` taken on an interval that
  itself moves between 2s and 60s, so the loop runs against a lagged, noisy measurement.
  One step may at most halve or double the cap, so a single spike costs a few samples of
  convergence rather than the app's responsiveness. Caps are quantised to 5% so a cap
  wandering between 23% and 24% does not write `cpu.max` on every sample.
* **Floored.** Never below 5%. A cap low enough to stop an app responding makes it look
  crashed, which §1 forbids more strongly than it wants the battery.
* **Pure.** The cap is computed in the planner, not the actuator, so it stays inside the
  pure function and §14's replay reproduces the identical cap sequence.

**It does not control the metric directly, and says so.** `cpu.max` limits CPU *time*, not
wakeup frequency: a timer firing at 60 Hz still fires at 60 Hz under a 5% cap. What changes
is how much work completes per wake, which lengthens the event loop and indirectly
coalesces the wakeups that follow from that work. So the loop has real gain for an app
whose wakeups are its own churn and close to none for one driven by a fixed-rate timer or
inbound traffic. Rather than squeeze such an app to the floor for nothing, the controller
counts tightenings that fail to move the rate and, after three, holds the cap and reports
*"capping is not moving the rate"* in the journal. Every step carries its own measurement
in the reason, because a number that moves on its own is otherwise unaccountable.

After a successful freeze, if `reclaim_on_freeze` is set, we write to the cgroup's
`memory.reclaim` to push the frozen app's pages to swap/zram. This partially answers
"freezing does not free RAM" without changing the primary goal.

### How `Throttle` actually happens

Not through `cpu.max`. `app.slice` does not enable the `cpu` controller, so an app scope
has no `cpu.max` file at all — confirmed again on this machine, where a fresh transient
unit has none. Writing `+cpu` to `app.slice`'s `cgroup.subtree_control` would work but
fights the unit manager, which owns that hierarchy and rewrites it. The supported route
is `SetUnitProperties(CPUQuotaPerSecUSec=…, runtime=true)`, after which systemd enables
the controller itself and writes the file. Measured end to end on a busy loop:

| | `cpu.max` | CPU used |
|---|---|---|
| Before | *(does not exist)* | 1,002,929 usec/s |
| `CPUQuota=5%` | `5000 100000` | **49,710 usec/s** (4.97%) |
| Cleared | `max 100000` | 1,003,955 usec/s |

`runtime = true` matters: a CPU cap `dozed` applied must not outlive the session, let
alone survive a reboot in `/etc`.

That call is a D-Bus round trip, and the actuator is synchronous by design — it sits at
the far side of the dry-run seam and returns an `Outcome` the journal records
immediately. Rather than make the whole actuation path async for one rung, the systemd
client lives on a dedicated thread with its own single-threaded runtime and a five-second
timeout, so a wedged unit manager degrades to `Skipped` instead of hanging the daemon.

### Fallback when there is no scope to use

`Freeze` becomes `SIGSTOP` walked over the process tree, children first so a parent
cannot spawn a child behind us. This is the fallback and not the default because it is
racy in a way a cgroup freeze is not — the window is narrowed, never closed.

`Throttle` degrades to **nothing**, not to `Nice`. There is no cgroup to cap and systemd
knows nothing about a bare process tree, and `Nice` was measured to be worth so little
that journalling it as a throttle would be a claim that is not true.

Three invariants are enforced at the signal, not inherited from the caller, because the
tree walk reaches processes nobody named — a shell's children are whatever it launched:

* same UID only, so `pid 1` and every root daemon are dropped before any signal;
* the never-freeze list, checked per process rather than per root;
* a process whose `comm` cannot be read is skipped, because it might be the compositor.

Verified against the kernel (`tests/live_cgroup.rs`, `--ignored`): a stopped tree reports
state `T` — unlike a cgroup-frozen one, which reports `S` — its wakeups fall to under
five in two seconds, and `SIGCONT` puts it back to work. That `T`/`S` difference is what
lets the startup sweep resume signal-stopped orphans precisely, rather than blanket-
`SIGCONT`ing PIDs that may have been recycled.

### What the spike established (§16 step 2)

Measured on Plasma 6.6.6, kernel 7.1, against a 165 wakeups/s workload:

* **Freeze does what the whole tool assumes it does.** 165 wakeups/s and 177 ms
  CPU/s went to **0 and 0**; thawing returned it to 163 wakeups/s. Freezing a
  GUI app for six seconds left `kwin` and `plasmashell` fully responsive.
* **`Throttle` needs help from systemd.** `app.slice` does not enable the `cpu`
  controller, so app scopes have no `cpu.max` at all. Writing `+cpu` to
  `app.slice` behind systemd's back would work but fights the unit manager; the
  supported route is `SetUnitProperties(CPUQuota=…)`, now implemented and
  re-measured at 49,710 usec/s against a 5% cap. `Nice` and `Weight` stay
  unimplemented, and `Throttle` degrades to nothing rather than to `Nice` where
  no cgroup exists.
* **`memory.reclaim` works on an already-frozen cgroup** (3468K → 2272K), so
  freeze-then-reclaim is the right order.
* **A frozen task reports `State: S (sleeping)`** in `/proc`, identical to an
  idle one. Frozen-ness is only observable via `cgroup.events`; anything that
  infers it from `/proc` is wrong.
* **A freeze outlives the process that applied it.** Nothing in the kernel ties
  the two together, which promotes §10.3's persistence file from prudent to
  mandatory. `cgroup.kill` still works on a frozen cgroup, so there is a last
  resort if a thaw cannot be issued.
* **Scopes vanish constantly.** An app exiting between the decision and the
  write happened twice in ten minutes of manual spiking, which is why that case
  is `Outcome::Skipped` and never `Failed`.
* **`SIGSTOP` on a parent leaves its children running**, confirmed directly, and
  the transient children of a shell loop came and went mid-operation — the fork
  race a cgroup freeze is atomic against.

---

## 8. The grace model

Grace is how long an app must stay hidden before we act. It is a product of independent
factors so each can be reasoned about — and explained — separately:

```
effective_grace = base_grace × aggression × battery_factor × activity_factor × cooldown × flap
```

**`aggression`** — the one knob a user should have to touch, and the reason it is a term
in this product rather than a mode elsewhere in the code. `gentle` / `balanced` (default)
/ `aggressive` / `maximum` multiply by `3.0 / 1.0 / 0.4 / 0.15`.

A **number** is also accepted where a word is — `aggression = 0.75` — for when none of the
four is the answer; "a bit more patient than aggressive" has no name, and the gap between
`aggressive` (0.4) and `maximum` (0.15) is the widest in the set. It is written as the
number because that is exactly what it is: the same term of the same product the words
feed. Bounded to `0.05..=10.0`, since this multiplies a grace and zero would mean "act with
no grace at all".

Past `maximum`, `Config::warnings` says so, and says what it does to the *other* settings.
This is the failure mode worth naming, because it is silent and it was found in the wild.
A real 13-day journal recorded `aggression = 0.05` — the floor of the range, three times
past the most impatient named level. `battery.ac_factor = 2.0` doubled the grace on mains
and the aggression then divided it by twenty, so the deliberate "be more patient while
plugged in" policy came out ten times *shorter* than the base grace, for 3,259 of 3,947
decisions. Every term in this product scales every other one, and a value chosen to mean
"act sooner" silently rescales decisions taken elsewhere for unrelated reasons.

Everything else in `[defaults]` is a dial on one specific mechanism, and tuning "act
sooner" out of four of them at once requires understanding how they interact. A single
word does not, and being a plain multiplier is what keeps it honest: it cannot override
the model, it cannot make something freezable that was not, and it appears in the
derivation `dozectl why` prints, so its effect is arithmetic a user can check rather than
behaviour they have to trust. The levels are spaced by the *outcome* they produce for the
catch-all rule — 15m / 5m / 2m / 45s on a full battery — because that is what somebody is
actually choosing between, and `grace::tests` pins that table so the config file cannot
promise something the code does not do.

**`battery_factor`** — gates whether we act at all, and is the *only* place that
decision is made. The shipped rule deliberately carries no `on_battery` predicate: two
knobs for one question have to agree, and when they disagree the failure is silent.
Setting `on_ac = "enabled"` used to change nothing at all, because the rule still required
`on_battery = true`. A user could only discover that from `dozectl why --rules`. `on_battery`
remains available as a predicate for rules that genuinely want it — a rule that only applies
when unplugged, alongside another that applies always.

The current values are in the table further down — the cliff at `low_pct` this table used
to describe was replaced by a line proportional to charge, and the AC row now acts rather
than vetoes (§1).

**`activity_factor`** — modulates within an acting tier. This is the anti-thrash knob:
during active work you alt-tab constantly, and freezing an app you return to 20 seconds
later is a net loss, because thawing costs a redraw and often a reconnect.

| Desktop state | Factor | Why |
|---|---|---|
| `Active` (input < 30 s ago) | `4.0` | Rapid task switching — do not thrash |
| `IdlePresent` (no input, not locked) | `1.0` | Normal |
| `Away` (locked, screen off, or VT switched) | `0.0` | User is gone; act immediately |

The `Away` row is where most of the savings come from and it carries no UX cost, because
nobody is switching windows.

**Resume cooldown.** An app thawed by focus within the last `resume_cooldown` gets extra
patience next time it is hidden — it is clearly in your active rotation. How that patience
is applied is `resume_cooldown_mode`:

| Mode | Meaning |
|---|---|
| `floor` (default) | `resume_cooldown` is a **minimum wait** after a restore: whatever the factors say, the app is not acted on again until it has passed. `resume_cooldown_factor` still stretches the ordinary grace on top. |
| `factor` | `resume_cooldown_factor` multiplies the grace and nothing else. |

A rule predicated on `focused_for` — a *glance* rule — is exempt from both, and has to be:
the floor is measured from the very return that made the visit brief, so it is always in
force at exactly the moment such a rule wants to act.

That exemption is deliberate and it was also **invisible**, which is a separate bug. Under
the default `floor` mode the cooldown is not a term of the grace product at all, so
`Grace::cooldown` is structurally `1.0` on every journal line — and a reader looking at
`in_cooldown: true` beside a freeze taken four seconds after a thaw had nothing on the line
that could account for it. `DecisionInputs::cooldown_bypassed` now records it, and
`dozectl why` prints `cooldown-bypassed` rather than `cooldown`. A log that cannot explain
its own most common decision is not a log you can trust.

**`flap`** — the term that is not an argument about how long to wait, but an observation
that waiting that long has not been working.

A hold is not free, and its cost is **per cycle** rather than per second: the thaw costs a
redraw, a chat app costs a reconnect, and both are paid in full whether the hold lasted two
seconds or two hours. So there is some hold length below which the wakeups saved cannot
cover it — and nothing else in this product can see that the user is about to come back
*again*, so a rule can be perfectly correct and still lose money on every cycle.

How often that happens is the measurable part, and on a real 13-day journal it was most of
the time. The two shipped glance rules produced 1,459 of 1,955 rule-fired decisions, at a
median of 13 seconds between the thaw and the next freeze and a minimum of one. 316 of
1,951 freeze/restore pairs lasted ten seconds or less; 148 lasted three or less; 84
re-freezes landed within five seconds of the thaw that preceded them.

So each hold that ends inside `MIN_PAYBACK` (10s) because the user came back doubles the
next grace, capped at ×8. Three properties make it safe to compose with everything above:

* **It converges.** The wait grows until holds start outlasting `MIN_PAYBACK`, at which
  point the count resets outright and the app returns to whatever its rules asked for. An
  app you are genuinely alt-tabbing through settles at the shortest hold worth taking,
  rather than at the shortest hold the config permits.
* **One short hold is an event, not a pattern.** A notification arrived and you clicked it.
  The backoff starts at the *second* consecutive short hold — the same instinct as the
  thermal hysteresis, where a single sample either side of a line must not move a state.
* **Only the user coming back counts.** The daemon lets go for plenty of reasons that say
  nothing about the app — the machine suspending, a mode change, a shutdown, a maintenance
  thaw. Counting those made an eight-hour sleep look like a hold that failed after zero
  seconds, which is how the restriction was found.

Glance rules are **not** exempt from this one, and that is the point of it: they are the
rules that produce the pattern.

**Plugged in is not the same as unplugged.**

| State | Factor |
|---|---|
| On AC, `on_ac = enabled` (the default) | `ac_factor` = 2.0 |
| On AC, `on_ac = disabled` | — (not acting at all, and thawing what is held) |
| On battery | **charge remaining**: `pct / 100`, floored at 0.05 |

This is the whole of the difference between a laptop on battery, a laptop on a charger and
a desktop (§1): one term of one product. A desktop is a machine permanently on the first
row.

`ac_factor` did not exist at first, so acting on AC bought the *battery* schedule exactly —
a plugged-in machine treated as urgently as one at 40% charge, chasing a runtime saving
that does not exist while the wall is supplying the power. What is being cut on mains is
waste, and waste is worth being patient about: the only thing left to optimise there is not
being noticed.
Fourfold was tried first and was too much — measured live it put Signal at a 32-minute
grace and left nothing at all held after seven minutes, which is not "more patient on AC"
but "off on AC".

**Grace on battery is proportional to charge**, and that shape is an argument rather than a
preference. What a saved wakeup is *worth* scales inversely with the runtime left, and
remaining runtime is proportional to charge, so the time we are willing to wait before
saving it should be proportional to charge too. 50% charge, half the patience. 10% charge, a
tenth of it — a five-minute rule fires after thirty seconds.

Two shapes were tried and discarded on the way:

* **A step** — 1.0 above `low_pct`, `low_factor` below. A laptop at 31% was treated exactly
  as patiently as one at 100%, and crossing 30% halved the grace in a single sample.
* **An arbitrary line** through `(100%, 1.0)` and `(30%, 0.5)`, continuing to 0.29 at empty.
  It removed the cliff but the slope came from nowhere.

**Exponential is the wrong shape here**, which is worth recording because it is the obvious
next guess. Constant *relative* decay means the absolute steps shrink exactly where you want
them to bite: a geometric curve through the same anchor is milder than linear below 30% and
harsher above it — the opposite of "increasingly aggressive as it drains". Linear already
accelerates in relative terms; 100→90% costs 7% of the grace and 20→10% costs 50% of it.

The floor of 0.05 is not tuning. Grace is a product, so a battery term of zero means "act the
instant anything is hidden, forever", and a 0% reading is at least as likely to be a
flat-lining sensor as an empty cell. `away_factor = 0.0` remains the deliberate way to ask
for no grace at all.

`battery.low_pct` and `battery.low_factor` no longer do anything: pinning the line to the
origin and to full charge leaves no free parameter. They still parse, so an upgrade is not a
startup failure, and `dozed --check` reports them when they have been set to something that
used to mean something.

`floor` is the default because a multiplier alone can be cancelled out. Grace is a product,
so the `Away` row's `0.0` zeroes it, and the cooldown protected nothing at exactly the moment
the rules turn most aggressive. Replaying a real 5h25m session that was locked for most of it
showed a browser restored on unlock and re-frozen 81 seconds later, inside a two-minute
cooldown that was nominally in force. A minimum wait is what "do not re-doze something the
user just came back to" means: a condition on time since an event, not a multiplier on a
different clock. `factor` remains available — it acts sooner, and while the screen is locked
nobody is waiting on the app — and choosing it alongside `away_factor = 0.0` is reported by
`dozed --check`, because in that combination the cooldown is not shorter but inert.

**A glance is not a visit** (`focused_for`, opt-in). The cooldown above assumes that
coming back to an app means you want it. Often you do not: you alt-tab to Signal, read one
line, and leave. Under the cooldown alone that three-second look buys the app its full
grace *and* a two-minute floor on top, so something that was dozing happily a moment ago
runs for minutes because you glanced at it.

The `focused_for` predicate matches on how long the visit that just ended lasted, so a rule
can tell a glance from work. Two are shipped **on by default**:

| Priority | Rule | Action |
|---|---|---|
| 45 | `redoze-chat-after-a-glance` | `throttle 5%` |
| 55 | `redoze-after-a-glance` | `freeze` |

Both match `focused_for < 10s` and wait `15s`. Written by hand it would be:

```toml
[[rule]]
name     = "redoze-after-a-glance"
priority = 55
action   = "freeze"

[rule.when]
visibility   = "hidden"
focused_for  = { lt = 10 }          # seconds: what counts as a glance
for          = "15s"                # how long to wait before re-dozing
```

**Why two rules and not one.** A single glance rule has to pick an action, and any priority
high enough to beat `bg-idle` also beats `chatty-apps` at 50 — so glancing at Slack would
*freeze* it, dropping the connection and costing a reconnect, which is the exact harm §7's
throttle exists to avoid. Chat apps are the most-glanced-at apps on the machine, so that is
not an edge case; it is the main one. The priorities interlock instead: 45 beats
`chatty-apps` for chat apps, 55 loses to it, so a chat app is throttled quickly after a
glance and never reaches the freezing variant. Both draw from one shared app list, because
two copies would drift and the drift would be silent.

**Why `for` is not zero.** Re-dozing the instant focus leaves is the obvious reading and it
is wrong twice over. Alt-tab cycling focuses each window you pass through, so a zero wait
turns one keystroke into a freeze/thaw for every window in the ring. And §7 already prices
the thaw: for a chat app it costs a reconnect, which is more than the few seconds of
wakeups it saved. This is the same trade `DesktopState::Active` already names — *"during
heavy task switching a freeze that gets undone 20s later is a net loss"*. A wait of
10–20s covers the cycling case and is still an order of magnitude below the ordinary grace.
`for = "0s"` is accepted for anyone who disagrees.

A rule carrying `focused_for` is **exempt from the resume cooldown and from the activity
factor**. All three guard the same thing, and multiplying them made the rule nominal:
measured live, a 15s wait came out as 4m — sixteenfold — so the rule meant to re-doze a
glanced-at app promptly did nothing of the sort. The wait a glance rule states is the wait
it gets. The power term still applies, because that is about what the wait is worth rather
than about thrash. The cooldown's premise is that a returned-to app is in your active rotation; a rule
that only matches a three-second visit has established the opposite for this visit, so the
two would otherwise contradict each other — and the floor, being measured from the very
return that made the visit brief, would win every time and the rule could never fire at
all. Exempting only the floor is not enough: the factor alone silently doubled a 5s wait
to 10s, which reads as the rule misfiring rather than the cooldown still being in force.

One subtlety worth knowing, because it was a bug before it was a note: a new app is created
`Focused` on purpose — assume the worst until the compositor says otherwise — so every app
passes through a focus state nobody observed. Only a focus the compositor actually reported
counts, or every app would be handed a near-zero "visit" on its first snapshot and this rule
would re-doze the whole session moments after the daemon started.

**Low battery reaches the foreground, and nothing else does.** Every other shipped rule
matches `visibility = "hidden"`, because the window you are looking at is where §1's
promise — that you cannot tell `dozed` is running — is easiest to break. But an app you are
not *interacting with* is not doing anything for you just because it holds focus: a laptop
left with Slack in front of it at 15% charge is spending its remaining runtime on a window
nobody is typing into.

| Priority | Rule | Action | Fires when |
|---|---|---|---|
| 35 | `low-battery-foreground` | `throttle-to = 50` | focused, desktop not `Active`, below 20%, for 5m |

"Not interacting with" is read from the desktop state, and that is exact rather than a
proxy: **input goes to the focused window**, so a focused app under a desktop that is no
longer `Active` is precisely an app receiving no input. No per-app input tracking is needed
or would be more accurate.

Reaching the foreground at all is guarded by two conditions, both required and neither
inferrable from a predicate that happens to match:

* The rule says `visibility = "focused"` **in so many words**. Every rule shipped before
  this says `hidden`, so none of them can begin acting on the window in front of you as a
  side effect of anything.
* The action is a **throttle**. The planner refuses to freeze the foreground whatever a
  rule asks for — a frozen focused window is a stale rectangle, which reads as a crash, and
  §1 rules that out more firmly than it wants any saving. A cap only makes the app slower.

`never` rules still outrank it, and a focused app that matches no rule still thaws with
`became-visible` rather than "no rule matches" — the foreground is the ordinary case for
that path, not an edge one.

**Idle is not away.** Watching a two-hour video produces zero keypresses. `Away` is
suppressed while any of these hold: an MPRIS player is `Playing`, PipeWire reports an
active output stream, or an idle-inhibitor is held (`org.freedesktop.ScreenSaver.Inhibit`,
logind inhibitors, Wayland idle-inhibit).

---

## 9. Rules

Structured TOML, **first-match-wins by explicit priority**, no scripting language — so
every decision maps to exactly one named rule and the "why" string is unambiguous.

```toml
mode = "dry-run"            # dry-run | live   (default: dry-run)

[defaults]
base_grace          = "5m"
cpu_sample_window   = "60s"
reclaim_on_freeze   = false
freeze_visible      = false  # never freeze a visible-unfocused window
on_ac               = "enabled"   # enabled | disabled -- a watt is a watt, §1
resume_cooldown      = "2m"
resume_cooldown_mode = "floor"   # floor | factor -- see the grace model, §8

[activity]
active_threshold = "30s"
active_factor    = 4.0
away_factor      = 0.0

[battery]
low_pct    = 30
low_factor = 0.5

# --- rules, evaluated in priority order ---

[[rule]]
name     = "never-system"
priority = 0
match    = { class = "system" }        # built-in denylist, see §10
action   = "never"

[[rule]]
name     = "audio-exempt"
priority = 10
match    = { playing_audio = true }
action   = "never"

[[rule]]
name     = "long-job-exempt"
priority = 20
match    = { class = "long-running-job" }
action   = "never"

[[rule]]
name     = "chatty-apps"
priority = 50
match    = { app = ["com.slack.Slack", "com.discordapp.Discord"] }
when     = { visibility = "hidden", for = "2m" }
action   = { throttle = 5 }

[[rule]]
name     = "idle-wakeup-hog"
priority = 60
when     = { visibility = "hidden", wakeups_per_sec = { gt = 50 }, for = "60s" }
action   = "freeze"

[[rule]]
name     = "bg-idle"
priority = 100
when     = { visibility = "hidden", for = "5m" }
action   = "freeze"

[apps."org.mozilla.firefox"]
mode = "monitor"            # per-app dry-run while the rest run live
```

Match predicates: `app`, `class`, `playing_audio`, `has_inhibitor`, `fullscreen`,
`visibility`, `for`, `on_battery`, `battery_below`, `cpu_pct`, `wakeups_per_sec`,
`desktop_state`.

### Wakeups, not CPU%

The obvious rule is "suspend it if it takes too much CPU", but high background CPU is
ambiguous: it is either a misbehaving app **or exactly the work you asked for** — a
compile, a video export, a backup, `docker build`. Suspending a 20-minute build is the
worst failure this tool can have.

So the primary metric is **wakeups per second** (from `voluntary_ctxt_switches` +
`nonvoluntary_ctxt_switches` in `/proc/<pid>/status`), not CPU%. Wakeups cleanly separate
"idling badly" from "doing real work", and they are what actually costs battery.

CPU% remains available as a predicate, guarded by the `long-running-job` class:

* a terminal or shell with a live non-shell child,
* an app whose CPU ramp **started while it was focused** (user-initiated),
* anything on the built-in long-job list (build tools, VMs, containers, backup, sync).

### The class has to be decided from the tree, not the window

An app is a process tree, never a PID (§4) — and the class was being decided from the
*window's* comm alone. For a terminal that is `wezterm-gui` and never `cargo`, so a
terminal with a compile running in it came out `Normal`, which is to say freezable. That
is precisely the failure named above as the worst this tool can have.

Demonstrated rather than theorised. Replaying a real nine-hour night with `on_ac =
"enabled"` froze a hidden wezterm at the moment the screen locked — with 4327 CPU-seconds
of build inside it.

Two of the three criteria are now implemented, both over the whole tree:

* the built-in list, matched anywhere in the tree rather than only at its root;
* a terminal or shell with a live non-shell child. **Broad on purpose**: a terminal with
  anything at all running in it is left alone, an idle `vim` included. Sparing it costs a
  little battery; freezing someone's `ssh` session or training run does not cost a little
  anything. A terminal sitting at a bare prompt is still freezable, so this does not give
  away the whole category.

The third — "an app whose CPU ramp started while it was focused" — is still not
implemented. It needs per-app CPU history the sampler does not keep, and the two above
already cover the cases that break something.

### Where a name list is the right answer, and where it is not

Three things in dozed are curated lists of names — the long-job executables, the terminals
and shells around them, and the identity-matched `never-*` rules — and the question of
whether to keep growing them has one answer per category, not one answer overall.

**Grow the lists where a name is the only signal.** Long-running work is the case: "high
background CPU" is exactly what this daemon exists to suppress, so nothing distinguishes a
compiler from a leaking Electron app except knowing what a compiler is called. The same
goes for a VM manager, a sync client or a backup front end idling in the tray, which is
behaviourally indistinguishable from an idle chat app and catastrophically different to
freeze. Erring long is correct in this category: a name missing risks §9's worst failure,
and a name wrongly present costs some battery.

**Do not grow them where the thing is watched.** Audio, capture, screen sharing,
inhibitors and bus ownership are observations, and no list of app names improves an
observation — it only adds a second, staler source of truth that can disagree with the
first. A "conferencing apps" list is the tempting one and it is exactly the wrong move:
Zoom, Teams and a browser tab in a call are already covered by `audio` and `capture` on
evidence, and the list would fail silently for whatever the user actually runs.

Two constraints on the ones that do grow. Names are matched against `comm`, which the
kernel truncates to 15 bytes, so entries are spelled as their truncations
(`qemu-system-x86`). And names generic enough to catch an unrelated GUI — `python`,
`java`, `convert` — stay out, because those exempt whole classes of ordinary app to catch
one.

The one addition that closes a stated gap rather than widening an existing one is
`never-password-managers`. §10 names "freeze your password manager and browser autofill
hangs" as a footgun and answers it with the D-Bus owner rule, which was measured to protect
owners of *activatable* names — `gnome-keyring-daemon` has one. KeePassXC does not, and its
browser integration runs over a native-messaging proxy rather than the bus, so nothing in
that rule sees it. Unlike the 34-of-93 measurement behind the owner rule, this is reasoning
and is recorded as such.

The remote-desktop list is the one where the interesting choice is not the membership but
the **action**, and it was wrong for a while. `never-remote-sessions` held RustDesk,
AnyDesk, TeamViewer, the VNC and RDP clients and the guest consoles under a blanket
`never`, on the strength of a real VNC session found hidden for 1h58m in the would-doze
list. The harm it was avoiding is specific: a *frozen* client stops draining its socket,
the receive window fills, and the peer drops the session — the cost is a connection to
re-establish, not a slow redraw. A cap does not have that failure. It is also the
strongest rung that leaves an always-on host reachable, which a freeze is not, and
unattended reachability is the only reason one is running.

So the list is now two rules and the strongest either may do is throttle
(`throttle-remote-hosts`, `throttle-remote-clients`). Splitting it is what makes two
honest numbers possible from one dishonest one. No source here can see whether a session
is live — nothing in §10.8 watches sockets — so each cap has to serve the connected case
and the idle case at once. For a host that means loose (50% of a core: bounds a runaway,
clears a hardware encoder, and a software encoder at high resolution loses frame rate
rather than the session), because when it is wrong the person who notices is at the far
end and cannot fix it — the same asymmetry §10.8 gives for `Capture`. For a client it
means tight (5, `chatty-apps`' number), because that rule only reaches a session the user
has already switched away from and these protocols send nothing while nothing moves.

What this costs is recorded rather than estimated, because the journal on the machine that
prompted the change answers it directly. Over 4d14h live it froze `rustdesk` **278 times,
holding it 4d12h and avoiding ~111.6M wakeups and ~2708 CPU-seconds — 30% of every wakeup
dozing saved there**, second only to the browser. Almost all of that is given back, because
that `rustdesk` was launched from a terminal and shares its cgroup, so a `Throttle` is
`Skipped` and the rule exempts rather than caps. The case for paying it is in the same
journal: **one** of those 278 freezes landed on a live session — 102% CPU, 1182 wakeups/s,
4.5s after the window was hidden — and several of the rest ran 8 to 12 hours overnight, a
host unreachable for exactly as long.

One in 278 is also the argument for the thing this does not have. A live-session signal
would keep the 277 idle holds and drop only the freeze that mattered, which is a better
trade than either rule can make, and it is the version to want: one rule, a tight cap, and
an exemption that fires on evidence. Nothing in §10.8 watches sockets, so it is not
available here.

Note what curation does **not** reach: the shipped rules are a starter kit, written once on
first install and never touched again (§15), so new names help new installs only. Growing
these lists is not a fix that reaches the people already running it.

---

## 10. Safety invariants

These are hard rules in code, not configuration. They are the difference between a bug
and a wedged desktop.

1. **Never freeze**, and not overridable in the UI: the compositor (`kwin_wayland`,
   `gnome-shell`, `Xorg`), `systemd`, `dbus-daemon`/`dbus-broker`, `pipewire`,
   `wireplumber`, `pulseaudio`, `xdg-desktop-portal*`, `polkit`, the session manager,
   `dozed` itself and all of its ancestors.
2. **Loss of the focus source ⇒ thaw everything and go passive.** The GNOME extension
   *will* break on a major GNOME upgrade. If we cannot see focus, we must not be holding
   anything frozen. Enforced by a heartbeat on the helper's D-Bus name.
2a. **A hold is undone the way it was taken.** The unit an app presents is not
   stable — `is_the_unit_for_app` re-answers it on every compositor snapshot —
   so "which mechanism is holding this app" must be read from the hold, never
   re-derived at release time. Getting this wrong does not fail loudly: the
   restore succeeds against the wrong mechanism, reports `Applied`, and discards
   the record of the right one, after which invariants 3 and 5 below are both
   false for that app and nothing says so. Measured cost of the version that got
   it wrong: 64 processes stopped for 15 to 20 days on a live machine, against a
   journal that showed every freeze matched by a restore.
3. **Daemon exit ⇒ everything thaws**, for clean exit, panic, SIGTERM, and SIGKILL. The
   frozen set is persisted to `~/.local/state/dozed/frozen.json` *before* each freeze;
   a startup pass thaws every orphan found there from a previous run. **A write that fails
   is reported in `dozectl status`**, not only in the log: a full disk on a real machine
   produced *"a crash now would leave it frozen until the next sweep"* in the journal and
   nothing anywhere else, which is a safety net drawn and not connected — §10's original
   sin, in the one invariant that promotes this file from prudent to mandatory.
4. **`PrepareForSleep` ⇒ thaw everything before system suspend**, and on wake reset all
   grace timers, so eight hours of "idle" does not trigger a mass-freeze storm.
5. **Panic thaw** — `dozectl thaw --all`, a GUI button, and a global hotkey. Works
   unconditionally, ignores rules, ignores mode.
6. **Never freeze a visible window** unless `freeze_visible` is explicitly set per app.
7. **Never freeze the clipboard owner.** Clipboard contents live in the source app;
   freeze it and the next paste hangs. We track the current selection owner and exempt it.

### Where the exemptions come from

Each is a source publishing a **complete snapshot of PIDs**, never a delta — same
reasoning as the window snapshot in §12, so a source that restarts or misses a signal
resynchronises on the next publish. The daemon resolves PIDs to apps, and an app is
exempt when *any* process in its tree is, because a browser's audio comes out of a child.

`dozectl status` lists every source and whether it has ever reported. That line exists
because the failure it detects was real and silent: the shipped `audio-exempt`,
`inhibitor-exempt` and `dbus-owner-exempt` rules matched on state nothing ever set, so
they could never fire, and nothing anywhere said so.

| Exemption | Source | Status |
|---|---|---|
| `audio` | MPRIS `PlaybackStatus` ∪ `pactl list sink-inputs`, signal-driven, minus what is measurably silent | Working |
| `audio-silent` | `parec` on the sink monitor, while a stream is open | Working — **not an exemption**, see §10.9 |
| `capture` | `pactl list source-outputs` ∪ inotify on `/dev/video*` | Working |
| `screen-cast` | portal `ScreenCast` sessions, via a filtered bus monitor | Unverified against a live portal |
| `inhibitor` | logind `ListInhibitors()`, polled 15s | Working |
| `dbus-name-owner` | `ListNames` ∩ `ListActivatableNames` | Working |
| `fullscreen` | compositor helper | Working |
| `clipboard-owner` | XFixes | **X11 only** — not observable on Wayland |
| `long-running-job` | classified at identity time | Not a watched signal |

### 10.8 The two exemptions with an audience

`audio` protects the app you can hear. The two added beside it protect the app *somebody
else* can hear or see, and that difference is the argument for both: a frozen music player
is a fault its owner notices immediately and fixes in a second, while a frozen microphone
or a frozen shared window is a fault only the people on the other end of the call can see,
and they have no idea what happened or who to tell.

**Capture is not the other half of audio; it is a different signal entirely.** The
playback detector reads `sink-inputs`, and a dictation tool, a voice recorder, OBS, or a
call participant whose side happens to be silent has no sink-input at all. `source-outputs`
is the same file format read with a different header, so it is the same parser with the
record name as a parameter — and giving it the wrong header must find *nothing* rather
than the other list's trailing record, which is why fields only count after a matching
header has been seen. That was a real defect in the original parser, found by the test
that asserts the two directions cannot contaminate each other.

**The camera is watched with inotify, and that is what makes it free.** There is no kernel
interface answering "who has this device open" — `fuser` walks `/proc/*/fd`, and so do we —
so the question is not how to find the holder but how rarely to look. Measured on this
machine: `IN_OPEN` and `IN_CLOSE_NOWRITE` both fire on `/dev/video0` (`0x20` then `0x10`
around an open/close pair), and one full `/proc` scan across 707 processes costs ~24 ms in
a slow implementation. So the scan runs when a camera is opened or closed and at no other
time, which on an ordinary day is a handful of times, and `/dev` is watched for `CREATE`
so a webcam plugged in later is picked up rather than ignored until the next restart.

**Screen sharing protects the app doing the capturing, not the app being captured**, and
the first draft of this section claimed the opposite. What the portal tells us is who asked
for a session — OBS, Spectacle, a browser sharing a tab. What is being captured is not
answerable from it: a `Start` reply names PipeWire nodes, and there is no route from a node
back to a window or a PID. So "share one window and then work in another" left the shared
window freezable, which was a real gap and was stated here rather than left to be
discovered. §10.8.1 below is what closes it, as far as it can be closed.

Protecting the capturer is worth having on its own. A screen recorder is a strong freeze
candidate exactly because most of them hide their own window while they work, and freezing
one stops the recording.

#### 10.8.1 The app being captured, inferred from the one fact available

The gap above is the common case rather than an exotic one: you share a window, switch to
another, and the shared one is hidden, idle-looking and — to every rule in the config — an
excellent freeze candidate. Freezing it does not stop the share. It freezes the *picture*,
and the people watching see a still frame with no way to tell you why. It is the same class
of harm as the frozen microphone in §10.8: a fault only the far end can see, which is
precisely the kind this design says to spend battery on.

Nothing on the portal answers "what is in this session", and no amount of asking it
differently will. So `Exemption::MaybeShared` uses the one fact that *is* available: **what
was on screen at the moment a session opened.** Whatever is being shared was, in the
ordinary case, among it.

This is the only exemption in this document that rests on an inference rather than an
observation, and it is built so that the inference is visible rather than buried:

* **It ships as a rule, not as an invariant.** `shared-window-exempt`, in the config file,
  named and deletable. §10's invariants are hard-coded because they are certainties; a guess
  that errs wide has to be somebody's to decline. Somebody who would rather have the battery
  during a two-hour call can say so, and can find the place to say it.
* **It errs wide, deliberately.** Every app visible at that instant, which is usually one to
  three, held until the last session closes. Being wrong that way costs some battery during
  a call. Being wrong the other way is a frozen window in somebody else's meeting.
* **Captured at the transition, never recomputed.** By the time it matters the shared window
  is hidden — that is the entire hazard — so a set derived from what is visible *now* would
  be empty exactly when it is needed.
* **It grows while sharing continues.** A second session is a second thing being shared, and
  the apps on screen then join the set; recomputing would drop the first share's window,
  which is hidden by then precisely because it is being shared.
* **It ends with the last session**, so it is bounded by something observable rather than by
  a timer. A cancelled picker costs nothing beyond the moment it was open, for the same
  reason `screen-cast` counts sessions rather than latching on `Start`.

**What it does not catch**, stated rather than left to be found: a window that was *not* on
screen when the share began and is shared anyway. Some portal backends list occluded and
minimised windows in the picker, so this is reachable, and no signal available here would
notice it.

It is also the rule that made the gap in §15 worth closing. `shared-window-exempt` exists
because of a fault people were hitting, and shipping it in the starter kit would have
reached new installs only — which is to say, everybody except the people it was written for.
See §15's `dozectl upgrade-presets`.

That hiding has a second consequence worth knowing before reading `dozectl apps`: **dozed
only knows apps the compositor reports a window for.** A recorder with no window is invisible
to it, so the exemption cannot be attributed to any app and nothing appears in the table —
which is safe, because an app dozed cannot see is also one it can never freeze, but it does
mean the table is not where to look. `dozectl status` shows the `screen-cast` source's PID
count, and the daemon logs `screencast session opened` at info; those two answer "was it seen"
when the app list cannot.

**Sessions are tracked by lifetime, not by `Start`.** `Start` is the
obvious signal and it is the wrong one: a user who opens the picker and cancels has already
caused a `Start`, and nothing afterwards says it came to nothing — so an app latched on it
stays exempt for the rest of its life after one cancelled share, and Firefox is the app
most likely to hit that. The portal closes the session whether the flow succeeded or was
cancelled, so counting open sessions per client answers the actual question. The `Closed`
signal comes from the portal rather than from the app, so the client is recovered from the
object path, which encodes the caller's unique name by documented convention
(`/…/session/1_42/token` → `:1.42`).

This one is **not verified against a live portal** — there was none on the machine it was
written on — and it is recorded here as unverified rather than quietly presented alongside
the measurements. The failure direction is the safe one: a session miscounted as open
leaves an app alone that could have been frozen, which costs battery rather than somebody's
meeting.

### 10.9 Audio that cannot be heard

`playing_audio` used to mean "holds an uncorked sink-input", and that is a much weaker
claim than it reads as. Measured on this machine, on an ordinary afternoon:

```text
Sink Input #52394
        Corked: no
        Mute: no
        Volume: front-left: 65536 / 100% / 0.00 dB,   front-right: … 100% …
        application.process.binary = "firefox-bin"
        media.name = "(505) Discord | #general | …"
```

Uncorked, unmuted, full volume — and monitoring that stream for four seconds gave a peak
of **exactly zero across 32,000 samples**. A browser with a call tab open holds a stream
like that for as long as the tab exists. So `audio-exempt` matched, and the app most worth
dozing was exempt all day. That is the most expensive false exemption there is, and it was
invisible: everything downstream was working perfectly on an input that meant nothing.

So the question asked changed from "is a stream open" to "is anything coming out of it".
An app whose streams have been measurably silent for `audio.silence_after` is reported as
`audio-silent`, which **exempts nothing** — it is in the exemption enum because it is the
reason `audio` is *absent*, and `dozectl apps` shows it so that an app dozed despite having
an audio stream can say why.

**Silence outranks MPRIS.** A player reporting `Playing` for a minute while nothing
measurable comes out of the sink it feeds is reporting, not playing — and the case this
exists for is one that will happily claim both.

**Why the sink and not the stream.** PulseAudio can monitor a single sink-input
(`parec --monitor-stream=N`), which would attribute silence per app, and it is not usable
here. Measured against PipeWire's Pulse server: an index that no longer exists does not
fail, it **falls back to the default source** — verified by watching `pactl list
source-outputs` during a probe, where a stale index attached our `parec` to
`alsa_input…Mic1`. Passing `-d` alongside it does not help; the fallback wins there too,
and `pw-record --target=` behaves the same way. A daemon that monitored streams by index
would therefore open the user's microphone every time a stream ended a moment before it was
asked about, and would then read the room's silence as the app's.

Sinks are addressed by name, which has no such failure mode, so this listens to a sink's
whole mixed output. That costs attribution and buys safety, and it loses only in the
harmless direction:

* mix silent ⇒ **every** stream feeding it is silent. Certain, because a mix is a sum:
  nothing audible hides inside a silent one.
* mix audible ⇒ something is playing and we cannot say what, so nothing on that sink is
  called silent. A silent app sharing a sink with a playing one stays exempt, which costs
  battery and nothing else.

One race survives that: a sink disappearing between the listing and the probe puts us back
on the default source. It is ruled out by re-reading the sink list, and only on the answer
that can freeze something — a measurement of silence — because that is the only one where
being wrong is expensive.

**Every MPRIS read is on a one-second leash, and that is load-bearing.** Found by this
feature failing on a live desktop: `audio-silent` sat at zero for minutes while the sink
was measurably silent, and no `parec` ever ran. The cause was not in the measurement at
all. dozed had frozen Brave; Brave owns two `org.mpris.MediaPlayer2.brave.instance*` names;
a frozen process answers no method call, so each `PlaybackStatus` read blocked for the full
**25-second D-Bus timeout**, twice per pass. Everything else in that task — the probe
timer included — lives in the same `select!` loop and never got a turn.

The shape of that bug is worth naming: **dozing a browser broke the detector that decides
whether to doze a browser.** It predates §10.9 and was merely invisible, because before the
silence probe nothing else in the task had anywhere urgent to be. Players are now queried
concurrently and each read is bounded, so one frozen player costs a second and one player,
rather than the task. A player that does not answer in time is not treated as playing —
the same direction this function already failed in, where the cost is battery rather than
somebody's music.

**It names the tab.** `audio-silent` is a fact about a *process*, and the question it
provokes is which tab, so `media.name` from each uncorked stream rides beside the PID
snapshot and is attributed to apps by the same rule the exemptions are — the browser, not
the terminal it was launched from. The dashboard's exemption tooltip lists them, which is
what turns "firefox is dozing and it has audio open" into something actionable. It is a
*stream* name and is described as one: for a browser it is the tab title, and for
everything else it is whatever the app chose to call the stream. Not a domain event, so it
stays out of the world state and out of the trace — it changes nothing the planner decides.

**What it does not hear.** Sound falling entirely between two probe windows. The
measurement is a sample, not a recording — `probe_window` out of every `probe_every` — so
an app that beeps once a minute and is silent in between reads as silent. `silence_after`
is what keeps the ordinary gaps (between two tracks, a video rebuffering, a call with
nobody talking) from ever getting near that, and an app that really does play in bursts
wants a rule of its own.

**What it costs.** Nothing while no app holds an output stream, which is most of the day:
this is not a timer the daemon runs, it is a timer that runs while something is playing.
The deadline is absolute rather than a delay, because the same loop is woken by `pactl
subscribe` and a relative timer would be restarted by every one of those — a stream that
changes twice a second would be measured never, which is exactly the stream worth
measuring.

`--latency-msec=50` is not tuning; it is what makes the measurement possible. Without it
the first sample arrives **more than two seconds** after `parec` starts, in one burst, so
every window shorter than that read nothing and every sink looked unmeasurable. With it,
about 50 ms. The window is counted from the first sample rather than from the spawn, so a
slow connection shortens the measurement rather than emptying it.

**The threshold is digital silence, not quiet.** `silence_dbfs` defaults to −80, which only
a stream writing zeros gets under — two least-significant bits of a 16-bit sample, so
silence that has been through a rate conversion still counts. Music at −60 dBFS is very
quiet and is emphatically not silence; the cost of getting that wrong is an app frozen
mid-track.

Verified end to end on this machine, both directions: the Firefox stream above reads a peak
of 0.0 and is called silent, and a −50 dBFS tone played to the same sink reads 0.0032 and is
not.

### Anything it cannot do, it says so

The rule §10's original sin taught: three exemption rules matched on state nothing ever set
and nothing anywhere said so. `dozectl status` and the dashboard therefore report per source
not only *whether it has ever reported* but **why it sees less than it should**, and the two
are kept apart because they call for different actions:

* **Unavailable** is the platform's answer — clipboard ownership on Wayland — and there is
  nothing to do about it.
* **Degraded** is a package away from working, and the message names the package. Without
  `pactl` the audio source falls back to MPRIS alone and the microphone cannot be seen at
  all, so the two exemptions are told different things rather than one blanket warning.

This extends to every external utility the daemon runs, which today is `pactl` and `parec`
and should stay a short list. `parec` missing is its own report: silence cannot be measured,
so an open stream counts as audio exactly as it did before §10.9 existed. Both of its failure modes are covered, because they are different bugs
with the same symptom — not installed is a missing package, installed but not answering is
a sound server that is not running — and a recovery clears the report, because a tool that
keeps complaining about a fault it has recovered from teaches people to ignore the field it
complains in.

Three measured findings changed the implementation from the obvious one:

**Not every inhibitor means "keep me awake".** The inhibitors held on an ordinary KDE
session are mostly the wrong kind:

```
  what                    who                  mode    uid
  shutdown                Unattended Upgrades  delay      0
  idle:handle-lid-switch  repowerd             block      0
  sleep                   NetworkManager       delay      0
  sleep                   signal-desktop       delay   1000   <-- a real app
```

`signal-desktop` is the case that matters: a `sleep`/`delay` inhibitor means "let me
finish before the machine suspends", not "do not freeze me". Treating any inhibitor as an
exemption would have permanently exempted Signal — exactly the chat app §7 wants
throttled. The filter is therefore `idle` **and** `block` **and** same-UID, which on this
machine passes 1 of 8 live inhibitors.

**"Owns a well-known name" is far too wide; "owns an *activatable* name" is exactly
right.** Of 93 owned names on this desktop, 34 are also in `ListActivatableNames`,
meaning a third party can call them and expect an answer. Every one of those 34 is held
by session infrastructure — `gnome-keyring-daemon` (the password manager named above),
`xdg-desktop-portal`, `gvfs`, `kded6`, `dconf`. The owned-but-not-activatable set, by
contrast, includes **Firefox** (one `org.mozilla.firefox.<profile>` per profile) and
Remmina. Exempting on mere ownership would have given away the browser, the biggest
wakeup source on a desktop, for no safety gain: nobody else calls Firefox's name.

Note this leaves the single-instance launcher hang below exactly where it was — clicking
Firefox's launcher while Firefox is frozen still goes through its own non-activatable
name. That is the activation problem, and folding it into a blanket exemption would have
hidden it rather than solved it.

**`org.freedesktop.ScreenSaver.Inhibit` callers are not enumerable on KDE.** Probed
directly: holding a screensaver inhibit and reading back both
`org.kde.Solid.PowerManagement.PolicyAgent.ActiveInhibitions` (present, `emits-change`)
and logind's inhibitor list shows the inhibit in **neither**. There is no registry to
read, so this is the same shape of dead end as activation detection, and logind's own
inhibitors are the whole of what we can see.

### Footguns handled explicitly

* **D-Bus name owners.** A frozen process owning a well-known bus name blocks every caller
  for the 25 s D-Bus timeout — freeze your password manager and browser autofill hangs.
  We enumerate name owners and exempt those holding an *activatable* name, per the
  measurement above.
* **Single-instance activation.** Clicking a launcher for a frozen app goes through D-Bus
  activation into the frozen process, so the launcher would hang instead of raising the
  window. The daemon monitors the bus for `org.freedesktop.Application` calls and thaws on
  activation, not just on focus — see §12, which also records why the first attempt at
  this was abandoned and why that was a mistake.
* **Notification burst on thaw.** Cosmetic, documented, not prevented.

---

## 11. Metrics and the counterfactual report

A battery tool that cannot show a benefit is indistinguishable from one that does not
work. Per app we accumulate CPU-seconds and wakeups while dozing (or, in dry-run, while
we *would have been* dozing, sampled from the live process).

Dry-run therefore produces the report that sells live mode:

```
$ dozectl report --since 24h
Over the last 24h, dozed would have:
  held 6 apps for a total of 4h 12m of app-time — 3h 30m frozen, 42m throttled
  something was held for 2h 51m of that window; apps overlap, so the total above
  counts each one separately
  avoided ~310 CPU-seconds and ~2.1M wakeups
  estimated saving: 6-9% of a charge

  com.slack.Slack        1h 40m   ~120 CPU-s   ~890k wakeups
  com.discordapp.Discord 1h 02m    ~74 CPU-s   ~610k wakeups
  ...
```

The same accumulator drives rule tuning via replay (§14).

### 11.0 Why the headline says "app-time", and says the elapsed number too

The total is a **sum over apps**, and apps doze concurrently, so its ceiling is `apps ×
window` rather than `window`. The version of this line that said only `dozed 6 apps for a
total of 23h25m` under `Over the last 4h58m37s` was arithmetically correct and read as a
plain bug — six background apps left alone through a five-hour session each contribute
nearly the whole of it. Every reader who noticed went looking for a double-count that was
not there.

So the summary now carries both numbers and labels which is which: the sum, as *app-time*,
and the union of the episodes as the elapsed time something was actually held. The union
can never exceed the window, and the two lines together are the ones that make the sum
checkable. The sum stays the primary figure because it is what the per-app rows add up to
and what the tuner scores (§14); it just no longer claims to be a clock reading.

**A hold that is not holding is judged as a rate, on both axes.** The leak check convicts an
app that was measurably still running while held, and it used to compare wakeups against time
and CPU against the absolute floor `1.0` CPU-seconds. The argument for the first applies word
for word to the second: *against an absolute floor that is a leak; against 147 seconds of
dozing it is a tail*. A total has no denominator, so whatever residue survives per episode
simply accumulates until it crosses the floor — which is then reached by patience rather than
by severity. Measured on one real report: Firefox over **594 episodes** and 6d21h of holding
had left 2 leaked CPU-seconds, 3ms an episode and 0.0018% of the 113,867 CPU-seconds the same
row claimed to save, and was flagged; the same report's genuinely escaped app was burning 27%
of a core. The total ranked those the wrong way round. The threshold is now 1% of one core,
which keeps the old sensitivity exactly where the old rule was right — at the 100-second
episode `1.0` was chosen for, 1% of a core *is* one CPU-second — and stops it being reached
by duration. Applied to that report, four flagged apps became two, and the two were the real
ones.

**And the tag says whose failure it is.** It read `LEAKING` beside the app's name, which is
most readily understood as that app leaking memory; the condition is dozed's, so it now reads
`NOT HELD`, the way the block at the foot of the report always phrased it. The rate is printed
beside the totals, because a settling tail and a process that escaped its cgroup are the same
number of wakeups and two different problems — 0.5/s against 2,350/s on one real report — and
only the rate separates them. `dozectl` carried a legend line explaining the tag and the GUI
deliberately did not, on the grounds that it *"has room for real labels and tooltips"*; it
then never grew one, so the single alarming word on that panel was the only one with no
explanation anywhere near it. It is in the tooltip now.

The same line separates **frozen** from **throttled** app-time. A throttle is a cap, and §7
applies it precisely so a chat app *keeps running* — counting its held time as "dozed"
credits the weaker rung with the stronger one's claim. On the session that prompted this,
8h41m of the 23h was a 5% cap on Signal and Thunderbird, both of which correctly reported
~0 CPU-seconds avoided. A summary that adds those hours to a browser's real freezes is
overstating what dozing did.

### 11.0 An episode has to be closed by whatever ended it

`ReportAccumulator` learns about holds from the intent stream: a freeze opens an episode, a
restore closes it. That is a reconstruction, and the stream does not reliably close what it
opens. **An app that exits while frozen is never restored** — there is nothing left to
restore, so no intent is ever emitted — and a restore that comes back `Skipped` or `Failed`
is dropped before the accumulator sees it.

An open episode's time is counted right up to `now`, so an episode nothing closed does not
stall: it grows. The row goes on accruing hold time the app never spent held, and every
sample the app produces afterwards is charged as **leakage** against a hold that ended.
Measured on a live machine: one app showed a *single nine-day episode* leaking 2,352
wakeups/s and 27% of a core while it was plainly running and exempt, with one line in the
whole journal — a freeze, and no restore ever. The `9d1h` of hold and the ~7,034 CPU-seconds
of saving printed on the same row were fiction from the same cause.

It read as the one failure §4's leak check exists to detect, and it was an accounting error.
That is the worst way for this to fail: the check whose job is to say "do not trust the
numbers above me" was itself the thing producing the untrustworthy numbers.

**So the actuator is asked instead of inferred from.** `reconcile_held` takes the set the
actuator is holding *now* — the same source §10.3's frozen set is written from — and closes
any applied episode not in it. Once a tick, inside a loop that has just read `/proc` for
every app in it, so §13's budget does not notice. Only episodes claiming to be **applied**
are touched: a dry-run episode is `WouldHave`, nothing is held, the actuator could never list
it, and closing those would delete dry-run reporting altogether — the mode that has to work
before any of this is believed.

And when it closes one it says so, at `warn`. Every path that leaves an episode open is a
path that forgot to mention it had let go, and the report was wrong until the sweep caught it.

### 11.1 The modelled saving cannot be caught being wrong

`estimated saving: 6-9% of a charge` is CPU-seconds multiplied by a 2-4 W guess at what a
busy core costs. It is honestly labelled as a range and it is still the weakest claim in
this document, for a reason that is not the width of the range: **the number moves when
dozed does more work, whether or not the battery noticed.** It measures dozed's activity
and reports it as a saving. §1 asks this tool to be *provable*, and a figure that cannot
disagree with reality proves nothing.

Worse, it is blind to the metric §13 says is the primary one. Wakeups enter it only
through the CPU time they happen to cause, so the case the whole design is built
around — a hidden app waking the CPU 900 times a second while using almost no CPU — is
the case the model undercounts most.

So there is now a second, measured answer. Three counters, all readable **without
privilege** — a daemon that needs root to justify itself is a daemon nobody installs:

| Counter | Source | What it is |
|---|---|---|
| Discharge, watts | `power_now`, or the slope of `energy_now` | the quantity the tool exists to reduce |
| Deep-idle residency | `cpuidle/state*/time`, deepest state per CPU | the direct physical consequence of a wakeup |
| System-wide wakeups | `ctxt` in `/proc/stat` | one read; what the per-app figures should sum to |

A fourth is read **where it happens to be readable**, and never asked for. RAPL
(`/sys/class/powercap/intel-rapl:*/energy_uj`) is the better instrument — it is what the
package itself spends, and unlike discharge it survives being plugged in, which is the only
power figure a desktop or a docked laptop can have. It has also been `0400 root` since
CVE-2020-8694, where energy readings fine-grained enough to be a side channel recovered AES
keys. Requiring it is therefore out of the question, and so is nagging about it: the
counter is probed once at startup **by reading it**, and where that fails the figure is
absent exactly as a battery is absent on a desktop. Opening it is a root-level decision the
README documents with the CVE named, and the installer does not touch it.

Absent is not the same as **shut**, and the two are one silence unless the code separates
them. A machine with no RAPL has nothing its owner could grant and is owed no line of
anybody's terminal; a machine whose counter is `0400` has a figure one deliberate decision
away. So discovery distinguishes a permission denial from every other reason a read can fail
— an unparseable counter is a firmware fault and reporting it as "needs a permission" would
send somebody after the wrong thing — and `dozectl status` says `cpu package unreadable` in
exactly the one actionable case. The cost of not doing this was paid before it was written:
a correct udev rule, a correct build, no number, and nothing on screen to say which of the
two was wrong. §1's promise is that this tool says what it cannot see.

Two details are worth stating because getting either wrong would put a fiction on screen.
Only `package-*` domains are summed — `intel-rapl:0:0` is `core`, a subset of its parent,
and adding both reports a CPU drawing half again what it draws. And the counter wraps: 65.5
kJ on this machine, which at 10 W is every 1.8 hours, so a wrap is a routine event over a
session and not an edge case. It is corrected per domain against that domain's own
`max_energy_range_uj`, which is why the platform layer accumulates rather than handing the
raw counter up — a sum taken across domains has already thrown away the information needed
to recognise whose counter turned over.

That single package figure is broken down per domain, and apportioned per app, in §11.4 —
where the line between the measured half and the modelled half is drawn explicitly, because
this is the section that exists to keep the two from being confused.

The package figure was deliberately **not** part of the trial's verdict, on the grounds that
teaching the experiment to judge on RAPL is a change to what the experiment *is* and belongs
with that decision. That decision has now been taken — §11.2's "a trial a desktop can run"
— and it is a second verdict beside the first rather than a term folded into it, which is
what keeps the objection above answered: a package reading still contributes nothing to the
discharge mean.

That last row is now on the screen rather than merely available. `dozectl apps` and the
dashboard both foot the app list with the comparison (`dozed_core::coverage`):

```text
these apps: 1.2 of 16 cores busy, 904/s of 3900/s system-wide wakeups (23%)
the rest has no window: daemons, kernel threads, and dozed itself
```

Three deliberate choices in two lines:

* **Not called a total.** The list holds what the compositor gave windows for, so its sum
  excludes every daemon and every kernel thread. Labelled `total` a partial sum is simply
  wrong; labelled *these apps, out of the machine* the same number is right, and the
  second line accounts for the gap rather than leaving it to be discovered.
* **Cores, not percent.** `cpu_pct` is percent of a *single* core, so the honest sum on a
  16-core laptop reaches 1600 and `TOTAL 412.7%` reads as an arithmetic bug rather than as
  a busy machine.
* **It can be caught being wrong**, which is the whole point of §11.1. Sustained above
  100% is a double-counting sampler; near zero on a visibly busy desktop is attribution
  failing silently — the failure `dozectl apps` could not previously show. *Transiently*
  above 100% is neither: the rows average over `cpu_sample_window` while `/proc/stat`
  covers the last tick, so a burst can be in one and out of the other. The percentage is
  dropped in that case and the reason printed in its place, rather than clamped to 100 —
  clamping would hide the sustained version too.

It also moves while dozing does: a frozen app samples at ~0, so this line and its fraction
fall as holds take. That makes it the one place a hold is visible as it happens rather
than afterwards in §11's report.

Measured on the development machine, and both findings are load-bearing:

* `power_now` reads **0** on mains — not a small number — with `status` reporting
  `Not charging`, which is neither `Charging` nor `Discharging`. Discharge is therefore
  reported as *unavailable* on mains rather than as zero. A zero would have read as "this
  machine consumes no power", and a trial block would have recorded it as the most
  efficient block of the run.
* `energy_now` moves in 10 mWh steps (36 J). Over 5 s at 6 W the true change is 8 mWh, so
  the reading is either 0 or 10 mWh and the derived rate is either 0 W or 20 W. The slope
  fallback therefore refuses intervals under 30 s.

Cost of the new sweep, per §13's obligation to measure any new periodic read: 132 files,
**0.32 ms** per sweep, 0.016% of a core at the 2 s sampling floor, and it rides the
existing sampling tick so it adds **no wakeups at all**.

### 11.2 The self-trial

Measuring the battery is not the same as knowing what dozing is worth, because a
before-and-after on a laptop measures the evening. Two hours of dry-run followed by two
hours of live mode differ in what dozed was doing *and* in state of charge, room
temperature, which tabs were open, and whether anyone was compiling. Each of those moves
discharge by more than dozed does.

`dozectl trial start` therefore runs an actual experiment:

* **Paired blocks, order randomised within each pair.** Anything that drifts slowly lands
  on both arms of a pair and differences away. The randomisation is not decoration: it is
  what licenses the sign-flip test.
* **Conditions gated, disturbed blocks discarded whole.** Idle desktop, and on battery
  unless told otherwise. There is no honest way to subtract a person from a measurement.
* **A settling period after each flip**, because entering live mode has to actually freeze
  what dry-run was pretending to freeze.
* **A non-parametric verdict.** A sign-flip permutation test for the p-value and a
  percentile bootstrap for the interval, both seeded and reproducible. No normal
  approximation to be quietly wrong about at n = 6.

**Six pairs is a floor, not a preference.** The smallest two-sided p-value a sign-flip
test can return from `n` pairs is `2 / 2^n`, because only the observed arrangement and its
mirror image are as extreme as itself. Four pairs of immaculate data give p = 0.125 and
five give 0.0625; neither can clear 0.05. Six reaches 0.03125 and is therefore the
shortest trial *capable* of concluding anything. This was discovered by writing the test
with four pairs and watching the verdict correctly refuse it.

A saving is only claimed when the bootstrap interval excludes zero **and** p < 0.05.
Otherwise the report says "no measurable difference", including the sign. A tool that can
only produce good news is what §11.1 exists to replace.

The trial measures **the idle desktop**, deliberately — that is the case dozing is for and
the only one where the signal is not buried — and the rendered verdict says so rather than
letting the number imply more.

#### A trial a desktop can run

§1 says the target is wasteful software rather than a short battery, and that the machine
gaining most over a week is the desktop: never unplugged, never suspended, routinely left
with fifty tabs and a chat client open overnight. The whole of §8's grace model was
rewritten around that premise.

The proof pillar did not follow it. `dozectl trial` judged on battery discharge, and
`power_now` reads nothing at all while the wall is supplying the power (§11.1: **0**, not a
small number). So the machines the premise is largely about were exactly the machines on
which the experiment could never return a verdict — dozed asking to be believed on a
desktop, having ruled out the only way it had of proving anything there.

RAPL is the instrument that survives being plugged in, and the trial now judges on it as a
**second verdict**, reported beside the discharge one and never folded into it. Blocks
accumulate both counters; a machine on battery with no readable RAPL reports the first, a
desktop with the udev rule reports the second, a plugged-in laptop with both reports both.
`--on-mains` stops meaning "residency only" and starts meaning "judge on the processor's own
energy".

**It is a narrower claim and is worded as one.** The package is not the machine: this is
watts off the CPU, which is where dozing acts, and it says nothing about the display, the
radios or the drives. Two figures that are not the same quantity must not be renderable as
one, which is the same discipline §11.4 applies to the sub-domains.

**And it refuses on a power-limited package**, which is the part that matters most, because
the number it withholds would have looked entirely reasonable. At the firmware limit the
chip holds its watts and moves its clocks underneath (§11.5, measured: R² of 0.005 against a
package that would not move), so freezing an app hands the freed budget to whatever else is
runnable rather than lowering the draw. Package watts are then flat between the arms however
well dozing worked — and reporting that as "no measurable difference" would be a fact about
the silicon wearing the clothes of a fact about dozed. It is the same instinct as §11.5
separating "the fit was poor" from "the package is capped": a wrong reason is worse than no
reason.

The refusal is **sticky across the trial** rather than per block. A run that spent part of
itself capped produces figures that are partly a share of a fixed allowance, and no later
stretch of unconstrained readings makes the earlier ones mean what they appear to.

#### A block nobody can use is not worth twenty minutes of the mode

Conditions are checked when a block *starts* and on every sample after, and
spoiling is sticky — the first thing that went wrong is the reason, and nothing
later makes the block usable again. So a block that spoiled in its first minute
still ran to term, holding a mode it was going to throw away.

Measured on a real machine, four blocks into a twelve-block trial:

| pair | arm | samples | discarded |
|---|---|---|---|
| 0 | dry-run | 42 | `user-active` |
| 0 | live | 49 | `on-ac` |
| 1 | live | 39 | `user-active` |
| 1 | dry-run | 63 | `on-ac` |

Four blocks, four discards, **zero usable pairs**, and 89% of an eight-hour
stretch spent in a trial-driven mode for measurements that were all thrown away.
A spoiled block is now abandoned on the sample that spoils it. It is still
recorded, with its reason, because "the desktop was in use" is what explains a
trial getting nowhere.

**And a block starts only once conditions have held for the settle period**,
which is what makes abandoning safe rather than frantic. Without it one idle
moment starts a block, the next sample ends it, and the one after starts
another — the whole schedule burned in a couple of minutes, each block costing
two mode changes and a re-assert of every held app. That is the thrash §8 spends
itself avoiding, arriving through the experiment meant to measure it. The settle
period is the right length for free: it is already the answer to "how long before
a mode change means anything", and a machine that cannot stay suitable for that
long was never going to produce a usable block.

Note what this does **not** change: a discarded block still consumes its slot in
the schedule, so a trial on a machine that never settles still finishes having
measured nothing. That is the honest outcome and it now arrives quickly instead
of over a month — but it is a separate decision from this one, and it is not
taken here.

#### A trial owns the mode only while it is measuring — including between blocks

The section below fixed a *waiting* trial demanding its baseline, and the layer above it
fixed a *finished* one doing the same. Both were about a trial asking for a mode it had no
claim on. The third case is the opposite: a trial that stops asking and leaves its last
answer standing.

`desired_mode` is `current_arm()?.mode()`, and `current_arm()` is `Some` only while a block
is running. Between blocks it returned `None`, which reads as neutral and is not: it leaves
the mode wherever the last block put it, which is the trial's leftover rather than the
user's choice.

Measured on a real 68-hour run, from the trace's `mode-changed` records and the journal:

| | |
|---|---|
| Block length configured | **1,200 s** |
| Mode held in dry-run | **40.25 hours** of monotonic runtime (47 wall-clock, the rest suspend) |
| Share of the run | **59%** |
| Intents `would-have` | **1,562 of 1,877**, on a daemon started `--live` |

A twenty-minute dry-run block ended, nothing put the mode back, and the daemon sat switched
off until the next block happened to start — which on a machine that is usually on mains
took two days. At two blocks in five days that trial needed a month, and six of its twelve
arms are dry-run.

**It was invisible while it happened, which is what made it expensive.**
`mode_driven_by_trial` is false between blocks because no arm is current, and
`configured_mode` cannot see it for the reason recorded further down — a trial moves the
mode through `set_mode`, which rewrites the very field that check compares against. So
`dozectl status` showed a bare `mode dry-run` with nothing anywhere to say why.

A block ending now hands the mode back to the baseline, and it does so as an **event**
(`Trial::handing_back`) rather than as a standing demand, which is what keeps this from
becoming the 19.5-hour bug below in the other direction. Asked once, cleared when asked, so
a `dozectl mode` afterwards is not undone a few seconds later.

The same flag covers `stop()`, and that closed a second hole in the same family:
`Daemon::stop_trial` asked `desired_mode` what to restore — but `stop` sets `at` past the end
of the schedule, making `current_arm()` `None` **by definition**, so the answer was always
`None`. Stopping a trial restored nothing at all and left the daemon in whatever mode the
last block had set. For a dry-run arm, that is switched off.

**Correct is not the same as legible.** Removing the leftover did not remove the
confusion it caused: blocks are twenty minutes and the waits between them are
hours, so `mode_driven_by_trial` is false for most of a trial's life, and a
daemon resting in its baseline still showed a bare `mode dry-run` with nothing
connecting it to the `trial 4/12 blocks` line further down. On the machine above
that was 89% of the day. `StatusReport::trial_restores` carries the baseline
whenever a trial is unfinished, so the mode line says which of the two situations
it is in — a block owning the mode, or the mode having been given back between
them — and the dashboard's hover says the same.

One thing this deliberately does not do is second-guess the baseline. A trial captures the
mode it found, once, and hands that back; if it was started while the daemon was already in
dry-run then dry-run is the honest answer, however unhelpful. The resume log line now names
the mode a trial will restore, so that is visible before it happens rather than afterwards.

#### A trial owns the mode only while it is measuring

Gating the conditions means a trial can wait a long time, and on some machines forever. The
mistake was letting a waiting trial keep hold of the mode anyway.

`desired_mode` fell back to the trial's `baseline` whenever there was no current arm, and a
trial waiting for its conditions has no current arm — so it demanded that baseline on every
sample. Measured on a real machine: `on_battery_only` on a laptop that is on mains for 91%
of its decisions, so not one of twelve blocks ever began, and the daemon was pinned to a
dry-run baseline for 19.5 hours and then 9.3 more across two restarts. 127 hours of dry-run
against 215 live, every dry-run span beginning at a restart. The unit passed `--live`, the
config file said `live`, and `dozectl mode live` was refused with *"a trial is driving the
mode"*. **The tool switched itself off to run an experiment it was never going to perform.**

This is the same failure that had already been found and fixed for *finished* trials one
layer up, in `daemon::drive_trial` — whose comment says, correctly, that `desired_mode`
falls back to baseline when there is no current arm. `finished` and `waiting` are two ways
to have no current arm; a caller-side guard for one left the other open. The fix belongs in
`desired_mode`, which now answers only for a block that exists:

* **A running block owns the mode.** Unchanged, and still refuses a mode set by hand — a
  hand-set live minute inside a dry-run block is measured as one, which is worse than
  losing the change.
* **A waiting trial owns nothing.** It cannot move the mode and cannot refuse yours. A
  refusal on behalf of a trial that is not measuring leaves the mode owned by nobody and
  changeable by no one.
* **Restoring the baseline is an event, not a standing demand.** `baseline_to_restore` is
  asked once, on the sample that finishes the trial.

Two smaller things fell out of the same investigation:

* `trial.json` was rewritten on **every sample for the life of the daemon** — measured
  byte-identical, once a minute, for three days. Only closing a block changes anything
  persisted, so it is now written when a block closes. The finished-trial comment raises
  exactly this §13 objection; the waiting case had it too.
* A stalled trial now **says so**. After six hours of continuous inability to start a
  block, one warning names the condition in the way (`on mains`, `user active`) and the two
  ways out. Six hours because an afternoon on a desk, a morning in use and an evening
  plugged in are all a trial correctly waiting, and a line that fires on those is a line
  people learn to ignore.

There is a reporting gap worth naming, because it defeated the mechanism built for it.
`configured_mode` explains "you asked for live and got dry-run" by comparing the daemon's
in-memory `config.mode` against the effective mode — but a trial changes the mode through
`set_mode`, which *rewrites that very field*. The two then agree, the check reports nothing,
and the status line shows a bare `dry-run` on a machine whose config says `live`.
`StatusReport::mode_driven_by_trial` is a separate flag for that reason, rendered directly
under the mode line for the same reason the snooze countdown is: it is why the mode reads
the way it does.

### 11.3 Switching mode has to change reality, in both directions

Found while building the trial, and it was silent, total, and pre-existing.

A dry-run outcome advances the planner's state machine exactly as a live one does. That is
deliberate — it is what makes the counterfactual faithful — and it means that after an
hour of dry-run the planner believes the hidden apps are frozen. `Planner::reevaluate`
suppresses an intent whose action already matches that belief. So `dozectl mode live`
switched the actuator over and froze **nothing**: the apps kept running, believed-held,
until something moved their visibility, which for a hidden Slack is hours. The
counterfactual report went on accruing a saving live mode was no longer producing.

The reverse was worse. Going back to dry-run routes every intent to the recorder, so the
real holds the live actuator was carrying had nothing left that could ever release them.
`dozectl mode dry-run` read as "stop touching my apps" and left them frozen for the life
of the daemon.

Both are fixed by making a mode change an **event** rather than an assignment:

* `Event::ModeChanged` is dispatched, so it is in the trace and a session spanning a switch
  replays as what it was.
* The planner answers it by re-asserting every action it believes is in force. Unconditional
  rather than diffed against the actuator, because the planner must not learn which actuator
  it is talking to (§3); every rung is idempotent, so asserting something already true costs
  a write and a journal line.
* A refusal recorded before the switch is cleared. The actuator said no in circumstances
  that have by definition changed, and the stale-belief bug is otherwise immortal in a
  second form.
* Leaving live mode releases the live actuator's holds *before* the switch, while intents
  still reach it.

The trial drives the mode through this same path, so it cannot measure something other
than what a user can set by hand.

### 11.4 Where the watts go: one half measured, one half modelled, and a line between them

§11.1 put a single measured wattage on screen. This splits it two ways — by hardware
domain, and by app — and the two halves are not the same kind of claim. The first is a
counter being read. The second is that counter being divided by a stated proxy. Mixing
them is the failure §11.1 is entirely about, so they are kept apart in the data, in the
rendering, and in the words.

**Measured: per domain.** RAPL exposes sub-domains beneath each package, and they are
subsets of it, never addends. `intel-rapl:0:0` is `core`; adding it to its parent reports
a CPU drawing half again what it draws. That trap gets *worse* once the parts are on
screen, because a breakdown looks like it ought to add up — so the layout is the defence.
Parts are printed under the total as "of which", never beside it, and what no sub-domain
claims is named rather than left as the gap between two numbers. Measured here:

| | |
|---|---|
| `package-0` | **24.07 W** |
| `core` | **2.21 W** |
| Not itemised — interconnect, memory controller, integrated GPU | **21.86 W** |

The unitemised part being the larger one is not an anomaly on this hardware, and it is the
reason the second half below apportions `core` rather than the package.

**The GPU is a second instrument that may be pointed at the same silicon.** `amdgpu`'s
`power1_average` reads 24.00 W on this machine against RAPL's 24.07 W over the same ten
seconds, because the label says `PPT` — Package Power Tracking — and on an APU the package
it tracks is the one RAPL is already reporting. Nothing in `/sys` separates that case from
a discrete card: `mem_info_vram_vendor` is absent on both, `mem_info_vram_total` reads
4 GiB of system RAM carved out for the iGPU and looks exactly like VRAM, and `power1_label`
says `PPT` on discrete Radeons too. There is no fact to read, only a guess to make, so no
guess is made — the figure is printed under the sensor's own name and **summed with
nothing**, and where both are present `dozectl status` says in words that it may be the
same number twice. §10's rule is that anything the tool cannot do, it says so; what it
cannot do here is tell you whether two of its readings are one reading.

**Modelled: per app.** There is no per-process energy counter in Linux and there cannot
be one — the hardware counts joules per power domain, and a domain is silicon that
everything shares. So a per-app watt figure is a model, and §11.1 is a section about what
happens when a model is read as a measurement. Four things make this one shippable where
the old CPU-seconds × 2-4 J estimate was not:

* **The total is measured.** The old model could not be caught being wrong: it went up
  when dozed did more, whether or not the hardware noticed. This one starts from a counter
  and can only be wrong about the split.
* **The numerator is `core`, not the package.** The package carries a floor no app causes,
  and on this machine that floor is 21.86 of 24.07 W. Apportioning all of it by CPU share
  would charge an app that ran for a second with watts that were being spent regardless.
  Where the kernel exposes no `core` domain the package is used instead and the label says
  so, because the two differ by an order of magnitude.
* **The denominator is the whole machine, not the app list.** An app's share is its CPU
  time over `/proc/stat`'s, not over the sum of the rows. Dividing by the rows would hand
  100% of the measured watts to the windows dozed happens to know about, on a machine where
  they account for 23% of it (§11.1's coverage line). The leftover is real, it is the
  daemons and the kernel threads and dozed itself, and it is printed.
* **The proxy is one proxy, stated.** CPU time. A blend of CPU time and wakeups would fit
  this hardware better and would need a weight — and a weight is a guess wearing a
  measurement's clothes, which is precisely the thing being avoided. One stated proxy whose
  bias is named beats a tuned one whose bias is buried in a constant.

**The bias, named, because it runs the wrong way.** CPU time under-attributes an app that
wakes the CPU often and uses little of it — which is *the* case this whole tool is built
around (§13). A timer firing 900 times a second drags a core out of C6 900 times and shows
up as almost no CPU time. So this column understates exactly the app dozed most wants to
catch, it is not the number to act on, and the footer says so under every rendering of it:
judge on wakeups/s, and read this for scale.

**Where it refuses.** A modelled figure that renders in every circumstance is one nobody
can catch being wrong, so this one declines in four:

* No readable counter — the ordinary case, since RAPL is `0400 root` — and then there is no
  column at all rather than a column of zeros. A permanent row of dashes is a promise the
  hardware cannot keep, printed on every invocation.
* Under 0.05 of a core busy machine-wide. The denominator is a rounding error in the tick
  counter there, and any app that ran at all takes a share near 1.0 of it.
* An app whose averaged CPU exceeds what `/proc/stat` saw the whole machine use in the last
  tick. That is the benign window race §11.1 already documents for the coverage line — the
  rows average over `cpu_sample_window`, `/proc/stat` covers one tick — and the answer is
  the same: say nothing rather than clamp, because a clamp hands one app the entire measured
  package and renders the guess as a fact.
* A wattage above 1 kW, from `dozed_core::energy::IMPLAUSIBLE_W`. A RAPL counter that
  *resets* rather than wraps is indistinguishable from a wrap at the moment it happens and
  is corrected as one — up to 65.5 kJ credited to one interval, which over the 2 s sampling
  floor is 32 kW. `0xFFFFFFFF` µW, hwmon's way of saying nothing, is 4,295 W. Same instinct
  as `IMPLAUSIBLE_RPM` and `cpu_ceiling_pct`, and the same honest incompleteness: it catches
  the reading that is three orders of magnitude out, not the last few percent.

**Two other sources were measured and rejected, and the measurements are here so that
nobody repeats them.**

*Whole-system input power from the USB-C charger.* A laptop charged over USB-C exposes
`ucsi_source_psy_*` with `current_now` and `voltage_now`, and on this machine they multiply
out to a confident 65 W — which would be the whole-system figure §11.1 says a docked
machine cannot have. It is not a measurement. Held at **exactly 3250 mA × 20 V through
every load**, including a run that took all sixteen cores to 100% and the package from 9 W
to 33 W, without moving by one microamp. It is the negotiated PD contract — the brick's
nameplate, 20 V/3.25 A being a standard 65 W supply — and it arrives looking exactly like
data, which is the same shape of trap as the fan reporting `0xFFFF`. Nothing reads it.

*Per-app GPU watts.* Rejected, and per-app GPU **time** built instead. The counters exist:
`/proc/<pid>/fdinfo/*` gives `drm-engine-gfx` and `drm-engine-compute` in nanoseconds per DRM
client, and `gpu_busy_percent` gives the whole-chip denominator — the same three-part shape
as the CPU attribution above, with 22 of this desktop's 723 processes exposing it, including
every browser. What is refused is turning it into watts: on an APU the GPU power *is* the
package power (24.07 W against 24.00 W, measured above), so a per-app GPU wattage beside a
per-app CPU wattage would be two shares of one measurement, side by side, inviting addition.
Engine time is a different claim and a safe one — the only per-app figure in this document
that is a measurement rather than an apportionment, because the kernel counts it per client
and nothing needs modelling.

Two things about it would each have produced a wrong number:

* **A client is not a file descriptor.** Measured: **136 DRM descriptors carrying 50
  distinct `drm-client-id`s** — every process holds about three, Firefox and Brave six
  across two clients. Summing per descriptor triples the figure. It happens not to change
  the total much on this driver today, because the duplicates carry an id and no engine
  counters, but that is one driver on one day. Descriptors are folded by client id.
* **Finding the descriptors is the whole cost; reading them is nothing.** A sweep of every
  `fdinfo` is **194 ms**, and 45 ms filtered by `readlink` first, against 33 ms for the
  entire existing pass over 2,725 threads. Narrowing to the app trees does not help —
  browsers *are* the descriptors, 8,268 across 53 processes. But a DRM client opens
  `/dev/dri/renderD128` once and holds it for life, so discovery is cached per process and
  the tick re-reads only the paths already known: **1.4 ms on a quiet machine and 4.1 ms on a
  loaded one** — both measured, and both quoted, because picking the flattering one is how a
  cost estimate stops being checkable.
  The cache is rebuilt every 64 ticks, because "holds it for life" is not "opens it at
  startup" and a tab that first touches WebGL an hour in would otherwise read as using no
  GPU at all.

Clients do not account for the whole chip, and the gap is stated rather than hidden.
Measured over three windows: clients at 7.5%, 10.9% and 10.8% of an engine against 10.8%,
15.1% and 17.2% of chip busy — roughly two thirds. The rest is the compositor's own
drawing, the display pipeline, and kernel submissions belonging to no client. It is the
same shape of gap §11.1's coverage line already names for wakeups, and it is footed the
same way.

*Everything else on this machine has no power sensor at all.* Every `hwmon` was enumerated:
the NVMe, the `mt7925` wifi, the ThinkPad embedded controller, `k10temp` and the ACPI
thermal zones expose temperatures and fan speeds and no current, voltage or power. The
amdgpu's own `in0_input`/`in1_input` (`vddgfx`, `vddnb`) read a flat zero. There is no disk
figure and no radio figure to show, so none is invented.

Where a fit exists it is no longer only a per-app column: §11's headline saving is computed
from it too. `Report::charge_fraction` used the 2-4 J-a-CPU-second guess this section opens
by criticising, and — worse — could not price a wakeup at all, so an app that saved no CPU
time and 900,000 wakeups was reported as having saved *nothing*. With coefficients it is
priced, the range comes from their own standard errors rather than from a chosen margin, and
the line says which of the two bases produced it.

### 11.5 The weight nobody chose, measured

§11.4 apportions by CPU time and names its bias out loud: CPU time understates an app that
wakes the CPU often and uses little of it, which is the app §13 is entirely about. The
obvious repair is to blend CPU time with wakeups. The obvious repair is refused, because a
blend needs a weight and a weight nobody measured is a guess wearing a measurement's
clothes — which is the thing §11.1 exists to prevent.

So the weight is measured. All three quantities are already sampled every tick: cores busy
from `/proc/stat`, wakeups from `ctxt`, package watts from RAPL. Three numbers a tick is a
regression waiting to be run:

```text
  package watts = floor + watts_per_core × cores_busy + joules_per_wakeup × wakeups_per_sec
```

Every term is then something the tool can say with a straight face. `floor` is what the
package draws with nothing running and belongs to no app — which is the honest reason a
per-app column does not add up to its header. `watts_per_core` replaces the 2-4 W §11.1
called the weakest claim in this document. And `joules_per_wakeup` is the number this
project has never had: what one wakeup costs, on this hardware, derived rather than
asserted. Where a fit is available it replaces the share model entirely — it needs no
denominator, takes no share, and one app's figure therefore does not move when an unrelated
app gets busy.

Coefficients arrive with standard errors, from the inverse of the normal-equations matrix,
so the saving they produce is an interval derived from what the fit actually knows rather
than a point widened by a chosen margin — which is what the 2-4 W range always was.

**A fitted model that always produces coefficients is exactly as unfalsifiable as the
constant it replaces**, so this one refuses in five ways and reports which: not enough
readings; a machine that held one load throughout; cores and wakeups that moved as one, so
no arithmetic can separate them; a fit explaining under half the variation; a fit explaining
plenty whose coefficients are still not physics; and a package at a fixed power limit.

Those last two started as one verdict and had to be separated the first time a real machine
reached them. It printed **"power model explains only 82% of the variation; not used"** — 82%
being a *good* fit, offered as the reason the fit was rejected. The actual reason was a
negative watts-per-core, which is what a package holding a power budget looks like from
outside: put more cores to work and each one gets less power. §10's rule is that the tool
says what it cannot do, and a wrong reason is worse than no reason — it is the same mistake
`find_rapl` avoids by refusing to report a malformed counter as a permission problem.

That last refusal was not a corner case — it was the first result. Measured over 90 samples:

| | |
|---|---|
| cores busy | 6.47 → **16.00** |
| wakeups/s | 7,183 → **30,132** |
| package watts | 23.18 → **24.28** |
| R² | **0.005** |

The package did not respond to load at all, and a settled idle reading was 24.2 W — the same
number. That is an APU pinned to its firmware power limit, holding its watts and moving its
clocks underneath. AMD exposes no `constraint_*` file to read the cap from, so it is
inferred from the one signature it cannot hide: watts that will not move while load does.

**The consequence reaches past the arithmetic and into what dozed is for.** At the power
limit, freezing an app saves no package watts — it hands the budget to whatever else is
runnable. The saving materialises only once the machine can drop below the cap. A per-app
watt figure on a capped machine is a share of a fixed allowance rather than energy caused,
and every reader will take it for the second, so the cap is reported in its own words rather
than folded into "the fit was poor".

**What it cost:** nothing measurable. The denominator comes from `/proc/stat`, which was
already being read in full for `ctxt`, so it is a second scan of a string already in memory
and no syscall at all. The sub-domains and the GPU are two more small files on the same
existing tick — three timed runs on a machine that was building at the time gave 1.03,
0.67, 1.11 ms without them and 1.05, 0.64, 0.91 ms with them. §13's constraint is wakeups,
and this adds none.

---

## 12. IPC and D-Bus interfaces

### Daemon ↔ clients

Unix socket at `$XDG_RUNTIME_DIR/dozed.sock`, newline-delimited JSON. Chosen over D-Bus
for the client path because the GUI wants a firehose of journal events and a socket is
simpler to stream and to test.

Requests: `Status`, `ListApps`, `Why{app}`, `Journal{since, follow}`, `SetMode{mode}`,
`Act{app, action}`, `ThawAll`, `Report{since}`, `ReloadConfig`, `StartTrial{…}`,
`StopTrial`, `TrialStatus` (§11.2).

### The session bus can go deaf, and used to do it silently

`FocusSink1` is the only interface dozed **serves**; everything else it consumes. That
asymmetry produced the longest silent outage this tool has had, and the trace of it is worth
keeping.

The connection went deaf. It kept its socket, so it kept `org.dozed.Daemon`, so the KWin
script's calls did not fail — they *timed out*, 1,291 of them, one every five seconds for
**2h14m**. Meanwhile the daemon was fine by every measure it had: the trace shows 135 more
ticks, 13,584 more samples, 72 `power-changed` and 2 `prepare-for-sleep` arriving from the
system bus, and a clean shutdown at the end. Everything that came over the *session* bus —
window reports, and the audio, bus-name and screencast exemptions that share the connection
— stopped at the same instant. It ended when the daemon was restarted for an unrelated
reason.

The §10.2 watchdog reported "compositor helper stopped reporting". The helper was reporting
throughout. That diagnosis sent the first hour of the investigation after a compositor that
had been up for nine days.

Three things follow, and all three are now in the code:

* **A silent focus source has two causes, so the daemon asks which.** Going passive stays
  unconditional and immediate — whatever the cause, we cannot see focus and must not hold
  anything — but the attribution waits for an answer. The watchdog now states the
  observation and asks the bus for a round trip; `Incoming::SessionBusHealth` names the
  culprit when it comes back, and `dozectl status` says `LOST — our own session bus is not
  delivering` where it used to say only `LOST`.
* **The probe is a self-ping, and only ever on that path.** Pinging our own name exercises
  the whole route that failed — write, broker, inbound dispatch to our own object server,
  reply — where checking the socket proves nothing. An *error* reply still counts as
  responsive, because the round trip is what is being measured; `NameHasNoOwner` does not,
  because that says the broker is fine and we are no longer the daemon. It runs only when
  the helper has already gone quiet, so §13's argument about steady-state cost is untouched:
  no timer, no polling.
* **Recovery rebuilds the group, not the sink.** The connection is shared by four consumers,
  and the trace shows them dying together, so a rebuild that restored only the focus sink
  would leave the exemptions dead — a daemon that looks healthy and would freeze a music
  player. The replacement also presents a **fresh instance token**: the helper dedupes its
  payloads and republishes only when the instance changes, so keeping the old token would
  buy a working bus with no window reports on it. A bus rebuild is a restart as far as
  anything on the other end can tell.

Reclaiming the name needs `AllowReplacement` set by the *original* connection — a flag only
the current owner can grant, so it has to be set before anything goes wrong — and
`ReplaceExisting` on the replacement. Both are now set, and there is a test that a rebuild
really does take the name from a connection still holding it, because this is the kind of
flag whose absence is undetectable until the day it matters.

The handlers were also made unable to block. Each now hands its message to the daemon with a
`try_send` and drops it if there is no room: they run on the connection's dispatcher, so an
await there stalls every later call on it, and nothing this interface carries is a delta —
a window snapshot is complete and superseded in five seconds, a heartbeat repeats, and
`hello` is re-sent whenever the token changes. `Heartbeat` in particular now produces its
reply *before* enqueuing anything, because a liveness check that can be defeated by the
daemon's own backpressure will report the helper dead for being busy itself.

### Who may connect

This socket is a control interface, not a read-only status feed: `SetMode`, `Act` and
`ThawAll` freeze and thaw this user's applications on demand, and `ListApps` and `Journal`
on their own are a record of which applications somebody runs and when they use them. §1's
promise is *unprivileged, and same-UID only*, and on this path that has to be enforced
rather than assumed.

It was assumed. The socket was created at whatever the umask allowed — `0775` under the
umask this machine ships, which is **group-writable** — and the only thing keeping another
account out was `$XDG_RUNTIME_DIR` happening to be `0700`. That is the directory's
guarantee and not ours, and it evaporates in the one case where it is most needed:
`ipc::socket_path` falls back to `/tmp` when `XDG_RUNTIME_DIR` is unset, where nothing
protects it at all.

Two mechanisms now, because they fail differently:

* **Mode `0600`, set at creation.** A umask held across `bind` rather than a `chmod`
  afterwards, since between the two the socket is already listening at the old mode. It is
  then read back and the daemon **refuses to serve** a socket it could not make private —
  a control socket on terms we could not set is not one to offer. (The umask is
  process-wide, so it is `0o077` rather than the `0o177` that would give `0600` outright:
  the wider value cannot strip the execute bit off a directory another task is creating in
  that window. Two tests binding concurrently found that, with `EACCES`.)
* **`SO_PEERCRED` on every accept.** The mode depends on the filesystem honouring it and on
  the socket landing where we expect; peer credentials are filled in by the kernel at
  `connect(2)` and cannot be shaped by anything the client sends. This is the one that
  actually enforces the rule, and it is checked before a reader task is spawned. Root is
  refused with everyone else: it needs nothing this socket offers.

Every field added to this protocol carries `#[serde(default)]` and defaults towards the
*narrower* answer, because the two binaries are versioned together and restarted
separately. Verified against a running daemon from before the trial existed: a new
`dozectl status` reads an old daemon's reply without complaint and simply prints no power
meter.

### What is about to happen

`ListApps` carries, per app, what the planner intends to do to it next and how long from
now — the shipped clients render it as `dozes in 2m14s`. It answers the question a status
table otherwise cannot: a row saying `running` says nothing about the freeze thirty
seconds away from it, and "when is firefox going to be frozen" was previously answerable
only by reading `dozectl why` and doing the arithmetic by hand.

Three things make it honest rather than a second opinion:

* It is the planner's **own deadline** — the entry in `Planner::deadlines` that the daemon
  sleeps on — and the rule from the derivation that produced it. Nothing recomputes the
  grace product beside the planner, so a countdown cannot promise something the daemon
  will not do.
* It travels as a **remaining duration, not an instant**. The deadline lives on the
  daemon's monotonic clock, which a client has no way to convert and no business guessing
  at; the subtraction happens where that clock exists.
* It is the **rule's action**, not an `Action`. `throttle-to` picks its cap from the sample
  it sees when it fires, so "throttle towards 20 wakeups/s" can be said in advance and
  "throttle to 7%" cannot.

What it is not is an appointment. Grace is a product of the power state, what the user is
doing and the resume cooldown (§8), so a keystroke pushes the deadline out and unplugging
pulls it in. The dashboard says so on hover rather than leaving a moving number to be read
as a schedule.

### One dashboard, however many times you ask for it

The tray's "Open dashboard" item spawned `dozed-gui` on every click, and every click got
another window — five clicks, five dashboards, each with its own 2s poll against the daemon
and its own repaints. That is §13's argument failing in the one place it is least defensible:
the window that exists to show you what the tool costs.

The fix is not the tray declining to spawn. Somebody who clicks "Open dashboard" wants to be
*looking at* the dashboard, and a window buried behind a browser on another workspace is not
that — a click that correctly does nothing is a worse answer than the bug. So the second
instance starts, finds the first, asks it to come forward, and exits before drawing anything.

A unix socket in `$XDG_RUNTIME_DIR` rather than a lock file, because the interesting half is
the *message*. A lock file answers "is one running" and leaves the raise to be built out of a
pid that can go stale between the check and the signal; a socket carries "come forward" with
no extra machinery, and inherits the stale-cleanup dance the daemon's own socket already
uses — a file left by a crashed instance refuses `connect`, and that refusal is the proof it
is safe to remove.

Raising is three commands, because "raise" means three things depending on where the window
was: un-minimise, focus, and request attention. The third is the honest fallback — **under
Wayland a client cannot simply raise itself**, focus-stealing prevention belongs to the
compositor, and `Focus` is a request that may be declined. Where it is declined the taskbar
entry is highlighted instead, which is the difference between the click doing nothing and
the click doing what the compositor permits. Verified honoured under KWin 6.

One thing fell out of testing it. `Command::spawn` without a `wait` leaves the exit status
for somebody to collect, and nobody did: three `dozed-gui` processes in state `Z`, parented
to the daemon, one per click. It mattered more after this change than before — every click
past the first now starts a process that exits in milliseconds, where previously making a
zombie took closing a window — so the tray reaps its child on a thread parked in `waitpid`.
Not `tokio::process`, which would mean adding a feature and a SIGCHLD reaper to a daemon
that spawns exactly one kind of child.

### A hand-set ceiling on the ladder

The rules decide *whether* and *when*; §7's ladder decides *how far*. `dozectl ease`,
`dozectl cap` and the dashboard's per-app buttons set a temporary ceiling on that last part —
"leave this alone while I am on a call", "cap this instead of freezing it".

A ceiling rather than a fourth mode, and the distinction is load-bearing. There were already
three ways to say "treat this app differently": a `never` rule, an `[apps]` override, and the
exemptions the daemon derives. A fourth *mode* would be a second knob answering the same
question as the override, and §8 carries the scar of exactly that shape — `on_battery` and
`on_ac` disagreeing silently, discoverable only from `dozectl why --rules`. A ceiling asks a
different question, so it composes instead of competing: `never` rules still win because they
are decided first, the rule table still picks the rung and still explains itself, and grace
still runs. It does not make an app act sooner or later, only less far.

Three consequences worth stating:

* **It is temporary by construction.** Permanent belongs in `[apps]`, where it is written
  down and can be found again. Anything set from a button should wear off, or it becomes a
  setting nobody remembers making. A duration of zero is refused rather than accepted: a
  ceiling that lapses on the tick it was set is a command that looked like it worked.
* **Setting one releases what is already held.** `Ceiling::clamp_or_release` distinguishes
  "nothing is permitted" from "nothing to do", because leaving a frozen app frozen for the
  life of a ceiling is the opposite of what somebody clicking *leave this alone* is asking.
* **It survives a restart**, unlike the global snooze. A snooze is minutes and a restart
  during one is a rare accident; "cap this until I say otherwise" is a standing instruction a
  restart has no business quietly revoking. Deadlines are stored as wall-clock, so a suspend
  counts against them — which is the right answer, since an hour is an hour of the user's
  time rather than the machine's.

And an app under a *leave alone* ceiling reports **nothing pending**. The rule underneath is
still counting down, but a countdown to something we already know we will refuse is precisely
what `PendingChange` promises not to be — it read `left alone (29m left), dozes in 1m33s` on
screen before this was fixed. A cap keeps its countdown, because it really will be acted on,
just not as far.

### Two views of the same window

The dashboard answers two different questions and was only ever laid out for the second.
*What is it doing to my machine* is what somebody opens it for; *how does it know* is what
they want afterwards, when tuning a rule or when something looks wrong. Everything was shown
at once, so the first question was answered in the margins of the second.

`Details` splits them, and persists with the geometry for the same reason `zoom` does: it is a
fact about the reader, not about the window. Off by default — a first-time reader can use the
summary and cannot yet use the derivation.

| Always | Behind `Details` |
|---|---|
| mode, and what is driving it | per-app wakeups/s and CPU% |
| the panic thaw, per-app controls | pid counts, top processes |
| app, visibility, state, hand-set limit | the exemption-source table |
| the counterfactual report | the daemon's own wakeups |
| **anything degraded or broken** | the recorder's event count |

The last row of the left column is the rule that decides the whole split. A lost focus
source, a recorder that stopped, a degraded exemption, a thermal signal stuck at `hot` — §10's
"anything it cannot do, it says so" does not get a checkbox. The recorder is the sharpest
case: the number it reports goes *down* when it breaks, so hiding it would hide the one state
worth seeing.

The timeline dropped its leading timestamp for the same reason. In a panel a few hundred
pixels wide the offset cost nine or ten characters of every row to answer a question asked of
about one row in fifty, squeezing out the app name and the reason — which is what the row is
for. It is on hover instead, where the rest of the derivation already lives. `dozectl journal`
keeps its column: a terminal is as wide as the terminal, and the space is free.

### The dashboard opens where you left it

A status window is opened, glanced at and closed dozens of times a week — from the tray, or
to check one number — and one that reopens at 880x640 at the default text size every time is
one you set up dozens of times. Size, position and **zoom** are written to
`~/.local/state/dozed/window.json` and restored on the next launch.

Zoom is the one that matters most, and the one that would have been easiest to leave out. It
is not a preference about this window but about the person reading it: somebody who needs
ctrl+plus twice to read a table of rates needs it every time, and a dashboard that forgets
is one they have to make legible again on every launch.

Two traps, and the first shipped broken.

**Where the size is read from.** The obvious source is `ViewportInfo::inner_rect`, and it is
permanently `None` on Wayland: `egui_winit` builds it from `Window::inner_position()`, which
returns `NotSupportedError` there by design, because a Wayland client is not told where it
is. So the read bailed out on the display server most people run this on and *nothing* was
ever saved — not the size, and not the zoom either, since both travelled through the same
early return. It reads `Context::viewport_rect` instead, which always exists. Not
`content_rect`, which egui's own deprecation notice suggests: that subtracts insets for
status bars and display notches, so on a platform with either the window would shrink by the
size of the notch on every restart. Position stays best-effort, because it is genuinely
unavailable on Wayland and has no substitute.

**Which points.** egui reports rects in **ui points** — physical pixels over
`pixels_per_point`, which folds the zoom factor in — while `with_inner_size` takes **logical
points**, which does not. Saving what egui reports and handing it straight back shrinks the
window by the zoom factor on every restart: at zoom 2 an 880-point window is stored as 440,
reopens half the size, and halves again on each launch until it reaches the minimum. The
multiplication happens in one place, where both numbers are in scope, with a test that walks
five restarts.

Written by hand rather than by enabling eframe's `persistence` feature, which would pull in
`ron` and `directories` to write a file this crate can already write with `serde_json`, into
the state directory `dozed-core` already defines — the daemon keeps `frozen.json` and
`trial.json` the same way, so this is the house style rather than a second one.

Three details:

* **It saves once the window stops moving**, not per frame. A drag-resize repaints
  continuously, and a write per frame would turn two seconds of resizing into a hundred of
  them. A close also flushes, because "resize it, decide that will do, close it" would
  otherwise fall inside the settle window and be lost.
* **What it reads back is clamped.** The file is user-writable and outlives the monitor it
  was written on, and the failure that matters is unrecoverable: a window restored at 12x8
  pixels, or at x = 6000 on a screen now 1920 wide, cannot be dragged back by the person it
  happened to, and they do not know the file exists. Anything unparseable is the shipped
  default, which is exactly the behaviour before any of this existed.
* **Position is a request.** Under Wayland a client cannot place its own surface, by
  design, so only the size is restored there. That is not worth working around and not
  worth hiding either.

### The dashboard explains itself

Everything the window shows carries its explanation on hover, from one module
(`dozed-gui/src/help.rs`): what the number is measured over, what the state word means,
and — where it matters more than the definition — what it changes. `900/s 18.0%` is not
readable without knowing that the rate is averaged over `defaults.cpu_sample_window` and
that the percentage is of *one* core; `hidden` is not interesting until it says that
hidden is the state the freeze rules wait on.

Three rules hold it together. Nothing is invented: every claim is one the crate can point
at in `dozed-core`, because a tooltip describing behaviour the daemon does not have is
worse than no tooltip. Anything that names a setting spells it as the TOML path the config
file uses, so the two are recognisably about the same knob — and the config editor's own
prose still comes from `dozed_core::starter` verbatim rather than being paraphrased here.
And dry-run runs through all of it: the held count, the state column and the per-app
buttons are counterfactual there and render identically to live, so each of them says so
on hover. `Exemption::ALL` is walked by a test, so a source added to core and not to the
table fails the build rather than appearing in the window as a name with no meaning.

### Editing the config from the dashboard

The GUI writes `config.toml` directly and then sends `ReloadConfig` — it does not gain a
protocol command for "set this setting". The daemon and the window run as the same user
and the file is the source of truth; an IPC path that could change settings *without*
going through it would create a second way for the config to be true, and the first
question anybody asks about a surprising decision is what the file says.

Three properties, in the order they matter:

* **A broken config cannot be saved.** Every save is validated by `Config::parse` — the
  daemon's own function, not a reimplementation — and a file it rejects is never written.
  What is validated is byte for byte what will be written, because the form validates the
  *rendered annotated file* rather than an intermediate.
* **Most mistakes cannot be expressed.** Enumerations are radio groups, booleans are
  checkboxes, factors are drag values floored at zero. That leaves durations as the only
  free text, and they report their parse result per field as you type — the save button
  says something is wrong, the field says which one.
* **Nothing has to be looked up.** Each control carries the explanation from
  `dozed_core::starter`, which is the same text that becomes the comments in
  `config.toml`. A second wording maintained beside it would go stale invisibly.

The settings form covers the scalar tables; the rule table is shown read-only there and
edited in a whole-file tab under the same parse-before-save rule. A form that pretended to
express a rule table would either be a worse TOML editor or a lie about what rules can do.
Saving from that tab writes the user's own bytes: a file somebody has hand-commented is
theirs, and reformatting it underneath them is not a convenience.

The two tabs *report* that rule differently, and the difference is the point. In the form,
invalid means one named field is half-typed, so the save button is disabled and the error
is on screen immediately — the field itself already says which one. In the text tab
invalid is the ordinary state of a file being edited: deleting a line to retype it is not
a mistake, and a red strip that appears on the first keystroke and stays until the last is
noise a reader learns to scroll past. So that tab says nothing while the text does not
parse beyond withholding the "valid" tick, keeps the save button live, and produces the
parser's message when it is pressed — which is the moment somebody has actually asked
whether the file is good yet. Nothing is written either way.

An `aggression` preview sits under the four buttons and shows what the current settings
mean in seconds, on battery, nearly flat, and plugged in. It calls `Grace::compute` rather
than reproducing the arithmetic, so it cannot promise something the daemon will not do.

### Choosing a focus source

Five session types, two shapes. Detection reads `XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`,
`DISPLAY` and `XDG_CURRENT_DESKTOP`, and `--focus-source` overrides it.

| Session | Source | Needs installing? |
|---|---|---|
| Wayland / KDE | `helpers/kwin` calls in | yes |
| Wayland / GNOME | `helpers/gnome` calls in | yes |
| X11 / KDE | in-process, EWMH | **no** |
| X11 / GNOME | in-process, EWMH | **no** |
| Sway on Wayland, i3 on X11 | in-process, the compositor's own IPC | **no** |

`WAYLAND_DISPLAY` is trusted over `XDG_SESSION_TYPE`, because a Wayland session that also
exports `DISPLAY` for XWayland is the normal case and some greeters do not set
`XDG_SESSION_TYPE` at all. Getting this backwards is the most expensive misdetection
available: under Wayland the X11 source sees only XWayland clients, so every native app
looks gone — and "no windows" reads as "everything is hidden". The resolved source and the
reasoning behind it appear in `dozectl status`.

On X11 no extension is needed at all. EWMH puts everything on the root window, so one
in-process source covers both desktops: `_NET_CLIENT_LIST_STACKING` (bottom to top, which
is the stacking index the visibility rule wants), `_NET_ACTIVE_WINDOW`, `_NET_WM_STATE`,
`_NET_WM_DESKTOP` against `_NET_CURRENT_DESKTOP`, and `PropertyChangeMask |
SubstructureNotifyMask` on the root plus `PropertyChangeMask` per window, so there is no
polling. `_NET_WM_PID` is advisory and some clients omit it, so the PID falls back to the
X-Resource extension, which asks the server who owns the resource and cannot be lied to.

### The wlroots family, and why the portable protocol was the wrong answer

The obvious way to reach Sway, Hyprland, river and niri at once is
`ext-foreign-toplevel-list-v1` plus `wlr-foreign-toplevel-management-v1`: no helper to
install, no per-compositor code, pure Rust, one source for the whole family. That was the
plan and it does not work.

**Neither protocol carries a PID.** Nor geometry. Checked against the protocol XML rather
than assumed — the wlr management interface's complete event set is `toplevel`, `app_id`,
`title`, `state`, `output_enter`, `output_leave`, `parent`, `closed`, `done`, `finished`,
and the string "pid" does not occur in either file.

For a taskbar that is fine. For `dozed` it is fatal, because everything downstream of a
window begins at `/proc/<pid>`: §4's identity ladder, the class that decides whether an app
is a long-running job, the sampler, and the cgroup an actuation writes to. A window that
cannot be attributed to a process is a window nothing can be done with — which is why the
KWin path already *drops* such windows rather than keeping them.

So the source is per-compositor after all, and the compositors' own IPC is what makes it
worth doing, because all three carry a PID:

| Compositor | IPC | Shape |
|---|---|---|
| Sway, i3 | `$SWAYSOCK` / `$I3SOCK` | 14-byte header, JSON payload, `SUBSCRIBE` for pushes |
| Hyprland | `$XDG_RUNTIME_DIR/hypr/…/.socket.sock` | JSON, plus an event socket |
| niri | `$NIRI_SOCKET` | JSON lines, plus an event stream |

Sway's is implemented, which also covers i3 and anything i3-compatible. It costs no new
dependencies — a unix socket and the `serde_json` already in the tree — and nothing polls:
`SUBSCRIBE` to `window` and `workspace`, one `GET_TREE` per push, unchanged snapshots
suppressed before they reach the daemon.

Three details that are decisions rather than transcription:

* **The tree is the source of truth, not the event payload.** An event says only that
  something moved; a delta would be a second way to be wrong.
* **`visible` is not trusted alone.** Sway sets it on views and i3 does not, so the walk
  tracks which workspace each output is showing and consults that. Without the fallback
  every window under i3 reads as hidden, which is the one error that freezes a desktop.
* **The source reports facts, never a verdict** — geometry, stacking, workspace, output —
  so §5's rule stays the single implementation and Sway cannot disagree with KWin about
  what "hidden" means.

#### The gap this section named, and what was in it

The paragraph that used to sit here said the wire format and the tree walk were unit-tested
against captured JSON but **not** verified against a live Sway, because the machine this
was written on runs KDE — and it ended: *"Every decision the walk makes is tested; that a
real Sway produces the tree shape the fixtures describe is not."*

It does not. The first time this source was pointed at a real Sway it reported **no windows
at all**, and had never been able to report any.

Sway's `GET_TREE` names each node in a field called **`type`**. The parser read
`node_type`, which is what `swaymsg`'s prose calls it and what nothing has ever put on the
socket. Every node therefore deserialised with an empty kind, no node was recognised as an
output, a workspace or a view, and `parse_tree` returned an empty vector against every
tree Sway can produce.

**Eleven tests passed throughout**, because the fixtures were hand-written to the same
wrong name. That is the whole lesson and it is not about Sway: a fixture invented from
documentation tests that the parser agrees with the fixture, and the two were wrong
together. The honest label above was correct about what was untested and, being a label,
did nothing about it.

The failure direction was the safe one, which is why it could survive: no windows means no
apps, so dozed froze nothing rather than freezing the wrong thing. It also means the Sway
backend did nothing whatsoever — and "no windows" is one keystroke of interpretation away
from "everything is hidden", which is the state §12 elsewhere calls the most expensive
misdetection available.

**Verification status now:** `packaging/verify-matrix.sh` runs the daemon against
`WLR_BACKENDS=headless sway` and asserts on *which* window comes back, and
`crates/dozed/tests/fixtures/sway-tree.json` is a tree captured from that run — the whole
reply, unedited, including every field this code does not read. An edited capture is a
hand-written fixture with extra steps. The hand-written `TREE` is kept for the situations a
one-window headless session cannot produce (two outputs, a hidden workspace, a floating
window, an XWayland client), and a test now pins the two against each other so it cannot
drift back into describing a schema nobody emits.

X11 is verified the same way, against nested Xvfb and metacity. **GNOME is not**, and that
row prints as unverifiable-from-a-script rather than being quietly omitted: a Shell
extension needs a real GNOME session. It is the row most likely to break — GNOME extensions
break on every major release by design — so it is the one worth re-running by hand on each
one, and the script says so with the commands to use.

#### A second daemon takes the first one's bus name, and that nearly hid all of the above

Worth recording because it made a broken backend look like a working one for the length of
one script run. The first version of `verify-matrix.sh` shared the login session's bus and
reported a confident `ok` for Sway **on a machine running KDE**.

dozed serves `org.dozed.FocusSink1` under the well-known name `org.dozed.Daemon`, and asks
for that name with `ReplaceExisting` so that a restarted daemon can reclaim it from its own
corpse. A *second* daemon takes it just as effectively from a first that is alive and well.
The host's KWin script then began reporting the host's windows to the test daemon, which
duly listed seven apps — none of them the one window the nested Sway had open, which is
what gave it away.

The daemon that lost the name is not left believing anything false: the §12 watchdog's
self-ping treats `NameHasNoOwner` as *we are no longer the daemon* rather than as a healthy
round trip, warns, and §10.2 thaws everything and goes passive. So the outcome is safe. But
it is a footgun worth knowing about — running a second daemon to test something silently
stands the real one down — and it is a trap for any test harness that shares a bus. Every
daemon the matrix starts now runs under `dbus-run-session`, and every check asserts on
*which* window it saw rather than on how many, because a count is exactly the kind of
evidence that survives being about the wrong compositor.

**X11 also closes §10.7.** XFixes `SelectSelectionInput` on `CLIPBOARD` gives
owner-changed events and the owning window maps to a PID, so the clipboard-owner exemption
works there. It is not solvable on Wayland, and `dozectl status` says so rather than
leaving the gap silent. Verified on a nested Xvfb + metacity desktop: a 244x134 window
fully covered by a maximised one is reported hidden by the shared rule, and a real
`xclip` owner shows up as `clipboard-owner ok, 1 process`.

### Compositor helper → daemon

**The daemon owns the bus name and the helper calls into it**, which is the
opposite of the obvious arrangement. A KWin script cannot publish an interface:
its sandbox exposes `callDBus` and no way to register a service, so a helper that
implements `org.dozed.FocusSource1` for the daemon to consume is not buildable.
Verified against KWin 6.6.6; see `helpers/kwin/README.md` for the full global list.

The daemon implements `org.dozed.FocusSink1` at `/org/dozed/FocusSink1` on
`org.dozed.Daemon`:

```
interface org.dozed.FocusSink1 {
    method Hello(u protocol_version, s compositor)  /* "kwin" | "gnome-shell" */
    method Windows(s json)                          /* full snapshot, see below */
    method Heartbeat(t monotonic_ms)
}
```

`Windows` carries JSON rather than `a(usssbbu)`: marshalling nested arrays out of
a KWin script is fragile, and a versioned blob is easier to extend and to replay
in tests.

```json
{"seq": 7, "windows": [
  {"pid": 283751, "app_id": "org.wezfurlong.wezterm",
   "desktop_file": "org.wezfurlong.wezterm", "visibility": "focused",
   "output": "DP-1", "fullscreen": false, "attention": false}
]}
```

Snapshots are always complete rather than deltas, so a restarted daemon
resynchronises on the next publish. The cost of that is that most of every
snapshot repeats what the daemon already knows, so it dispatches only the parts
that changed: measured on a five-hour trace, **85% of the `app-seen` and
`visibility-changed` events changed nothing**, which was most of a log a person
is meant to read to debug this. Suppressing them costs no behaviour — a repeated
visibility never restarted the grace clock anyway, and expiry runs off the tick,
which tightens to 2s as a deadline nears. One entry per window; folding an app's
windows into a single visibility (the maximum, §5) is the daemon's job, because
only the daemon knows how PIDs map to an `AppKey`. Note that `desktop_file`
arrives free from the compositor — that is §4's rung 2 without parsing a cgroup.

**Every argument in this protocol is a string, and every reply is one too.**
KWin's `callDBus` infers D-Bus types from JS values and does so inconsistently:
a small integer marshals as `i`, a large one as `d`. A `Hello(u,s)` signature was
rejected as `(is)` and a `Heartbeat(t)` carrying `Date.now()` arrived as `d`.
Because `callDBus` has no error callback, the helper could not tell, and the
daemon simply never learned the helper existed while window snapshots kept
arriving.

`Heartbeat` returns the daemon's **instance token**. Liveness alone is not
enough: a daemon that restarts quickly reclaims the bus name before three
heartbeats go unanswered, so the helper keeps believing it is the same instance
and its snapshot dedupe suppresses every publish — leaving the new daemon
convinced there are no windows at all. A changed token forces a full republish.

`Heartbeat` is what invariant §10.2 watches. Missing three in a row ⇒ thaw
everything — and the check has its own 5s timer rather than riding the sampling tick,
because the sampler backs off to 60s when nothing is pending and that turned "within three
heartbeats" into "within a minute and a half". Found by testing the invariant rather than
trusting it; it costs nothing, since a healthy helper already wakes the daemon every 5s. The helper watches liveness in the other direction too, since
`callDBus` only invokes its callback on success: three unanswered heartbeats and
it falls back to polling `NameHasOwner`, then re-`Hello`s and force-publishes
when the daemon returns.

**Activation: solved, and the earlier finding was wrong.** §10 wants to thaw on
single-instance D-Bus activation so a launcher click does not hang against a frozen
process. This section previously recorded that as impossible, on the grounds that
"eavesdropping on the bus is not possible under `dbus-broker` without special
configuration". That is true of the *old* mechanism — `eavesdrop=true` on a match rule
was removed — and not of the current one.
`org.freedesktop.DBus.Monitoring.BecomeMonitor` works for the owner of a session bus with
no configuration at all; it was observing method calls between two other processes within
seconds of being tried.

The reason to think it unaffordable was also wrong. `BecomeMonitor` takes match rules and
the broker applies them **server-side**, so only activations are ever forwarded. Measured
over ten seconds of an ordinary idle session:

| Monitor | Messages in 10s |
|---|---|
| Unfiltered | 46 (4.6/s) |
| `interface='org.freedesktop.Application'` | **0** |

The filtered monitor still caught both test `Activate` calls, with the target's well-known
name in `Destination`. So the daemon watches for `org.freedesktop.Application` method
calls — `Activate`, `ActivateAction` and `Open` all land in a frozen process the same way
— resolves the destination name to a PID, and thaws. Verified end to end: a launcher-style
`Activate` to `org.kde.kdeconnect` produced `activation requested pid=282929` in the
daemon. Note that it fires on the *attempt*, before the callee replies, which is the only
timing that helps — a frozen callee cannot reply at all.

Two things to know. A monitor connection can only receive, never call, so this needs a
connection of its own and the name→PID lookup happens on another. And it sees the bodies
of matching messages, which given the filter means activation payloads (a platform-data
dictionary) and nothing else.

KWin's `demandsAttention` is still reported and still useful — it covers "the app is
asking for attention" rather than "someone is launching it" — but it is no longer the
approximation standing in for this.

### Consumed system interfaces

* `org.freedesktop.UPower` — `OnBattery`, `Percentage`.
* `org.freedesktop.login1` — `IdleHint`, `LockedHint`, `PrepareForSleep`, session `Active`.
* `org.freedesktop.ScreenSaver` / `org.gnome.Mutter.IdleMonitor` — idle time fallback.
* MPRIS `org.mpris.MediaPlayer2.*` — `PlaybackStatus` for the audio exemption.

---

## 13. Not becoming the problem

A battery tool that polls `/proc` for 400 processes at 4 Hz burns real power. So:

* Process lifecycle comes from the **netlink proc connector**, not `/proc` rescans.
* Per-cgroup `cpu.stat` is read in preference to per-PID files where possible.
* The sample interval is **adaptive**: it backs off hard when nothing is frozen and the
  desktop is idle, and only tightens when a grace timer is close to expiring — or when
  somebody is watching the figures, which is the one case where staleness is the cost.
* The reported *rate* is over `cpu_sample_window`, **not** over the adaptive interval.
* The daemon measures **its own** wakeups and reports them in `dozectl status`.

An interval is only as good as when it is applied. The tick deadline is absolute — a
relative `sleep` inside `select!` is restarted by every other branch and starved the tick
outright, which is why `Tick` exists — but it was re-armed *only after a tick had fired*,
and the interval is computed from state that changes between them. A daemon that had just
started therefore armed 60s against an empty world and then slept through the arrival of
every app on the desktop; the same gap could notice a grace deadline created just after a
tick up to a minute late. Events now bring the deadline **forward** where they shorten it
(`Tick::pull_in`), and never push it out: taking the minimum rather than the newest is
what keeps the 5s helper heartbeat from restarting the interval forever.

The one thing that overrides the back-off is the **first two readings**. A rate needs two
of them, so at the 60s idle interval a daemon that had just started knew nothing about
anything for two minutes: `dozectl apps` and the dashboard drew a column of dashes, and —
the part that is not cosmetic — every rule carrying a `wakeups_per_sec` or `cpu_pct`
threshold spent that time deciding against `ProcSample::default()`, which is to say
against a measurement nobody had taken. So while the sampler has measured *nothing* and
there is something to measure, it ticks every 3s instead. It ends after one rate exists,
which is after two ticks: every app is sampled on the same tick, so they all cross
together. The cost is a handful of wakeups once per run, and §13's argument has always
been about what a tool costs while it runs rather than about its first ten seconds.

### A dashboard on screen is somebody watching

The back-off argues about a daemon nobody is looking at, and it is right about that daemon.
It was also applied to the one nobody could stop looking at: with nothing held and nothing
pending the tick is 60s, so `dozed-gui` — which polls every 2s — redrew the same `wakes/s`
and `CPU%` thirty times running. The figures were accurate as of the last tick, which is
not the same as fresh, and "why do the numbers not move" was the first question anyone
asked of the window.

So a client may say it is `watching` when it asks for the app table (`ipc::Request::ListApps`),
and while somebody keeps saying so the samples are re-read every 5s. Three properties make
that safe to bolt onto §13 rather than an argument against it:

* **It expires rather than being switched off.** Each poll renews six seconds of interest,
  so a window that is minimised, blocked or killed outright stops costing anything without
  having to say goodbye — nothing has to be trusted to send an "off".
* **It is costed.** A sweep is 33 ms over ~2,725 threads, so 5s is 0.66% of a core against
  0.055% at the idle interval, and it is paid only while a window is open. The 2s floor was
  rejected: three times the cost for a figure averaged over `cpu_sample_window`, which
  therefore cannot move three times faster anyway.
* **It samples and nothing else.** Sampling is the only thing on the tick whose staleness a
  human can see, and — by the section below — the only one whose *result* does not depend on
  how often it runs. The energy meter differences counters since its previous reading, and
  `trial` takes an unweighted mean over the samples in a block; speeding those up would have
  meant opening a window changed what the experiment measured. They stay on the tick.

### The reported rate must not depend on the sample interval

The adaptive interval moves between 60s and a 2s floor. Rates were computed as a delta
since the previous reading, so the measurement window moved with it — and `cpu_sample_window`,
which documents itself as "averaging window for CPU and wakeup samples", was read by no
code at all.

That made the primary metric 30× noisier exactly when it mattered, because what tightens
the interval is a grace deadline coming up — the moment a freeze decision is about to be
taken on the strength of that number. Seen on the first live run: at the 2s floor every app
on the machine reported 3–80× its usual rate for a single sample (`opensnitch_ui` 8/s →
635/s, a hidden Signal 0/s → 3466/s) and was back to normal on the next. The example
`idle-wakeup-hog` rule fires above 50/s; only its `for = 60s` stood between that artifact
and a freeze.

The sampler therefore keeps a short per-app history and reports the rate across
`cpu_sample_window` (default 60s), bounded to 256 readings so a pathological window cannot
grow without limit. §9's premise is that a wakeup rate is comparable against a threshold;
that requires the number to mean the same thing whichever interval produced it.

**And a rate that cannot have happened is discarded, not reported.** One entry in a 13-day
journal recorded Firefox at **4246% CPU on a 16-core machine**, beside 198,649 wakeups/s
where the 99.9th percentile of every other sample in the file was 7,340. `cpu_pct` is a
percentage of one core, so 1600 is the arithmetic ceiling and nothing above it is a busy
application — it is a lifetime total that arrived where an interval belonged. The sampler
now checks against `cores × 200` (loose on purpose: cores hot-plug, container quotas move,
and this is here to catch a number three orders of magnitude out, not to police the last
few percent), and on a violation returns `None` and drops that app's history so the bad
increment cannot inflate the rest of the window.

`None`, not a clamp, and for the same reason `ProcSample::sampled` exists: rules compare
these numbers against thresholds, so a fabricated ceiling decides just as firmly as a real
reading would. "Not yet" is true. A clamped 1600% is not.

The same lesson had a second instance, unnoticed until the header started showing watts. A
battery that reports no `power_now` is measured by differencing `energy_now`, which needs
tens of seconds to say anything — the counter steps in 10 mWh, so over 2s at 6 W it reads
either 0 W or 20 W. That minimum was enforced against the *previous reading*, which is the
one thing guaranteed to be one tick old: whenever the tick was tight, the interval was
always too short and the wattage was therefore `None` on every single sample. On such
hardware the figure vanished exactly when a grace deadline was close, and would have
vanished for as long as any dashboard was open. The slope now keeps its own anchor and
advances it only when it has been used, so it is over a real half-minute whatever the tick
is doing.

### The adaptive interval starved the sampler

`select!` drops the futures that lose, so building a fresh `sleep(interval)` on every
loop iteration restarts it from zero whenever anything else fires. The compositor helper
heartbeats every 5s and the §10.2 watchdog ticks every 5s, so the 60s idle interval never
completed once: `on_tick` was starved outright.

Consequences, all silent: no sampling at all, so `cpu_pct` and `wakeups_per_sec` read zero
forever and no rule using them could ever fire; no self-cost measurement, so the number
above read `0.0/s`; and `ReportAccumulator` never saw a sample, so §11's counterfactual —
the entire argument for switching live mode on — would have reported zero savings
indefinitely.

**It only bit when idle**, which is why no test caught it. With a grace deadline pending
the interval drops to 2s and wins against the 5s heartbeat, and every test had a pending
deadline. Five hours of a real idle desktop found it, in one reading of the trace: 6033
events, not one of them a `tick`.

The fix is an absolute deadline (`sleep_until`) held in a value, so re-creating the timer
each iteration cannot postpone it. That property is now a test with a paused clock, since
the bug is invisible to any test that does not deliberately stay idle.

### Counting wakeups correctly costs more than counting them wrongly

`/proc/<pid>/status` reports context switches for the **thread group leader**, not for
the process. Reading it — which is what we did — undercounts every threaded app, and
`wakeups_per_sec` is the metric every rule in §9 is written against. Measured on this
machine:

| Process | Threads | Leader only | All threads | Error |
|---|---|---|---|---|
| `kwin_wayland` | 32 | 1.60M | 2.50M | 1.6× |
| `plasmashell` | 63 | 281k | 585k | 2.1× |
| `dozed` itself | 17 | 23 | 485 | 21× |

The daemon's own figure was the worst of the three, because a tokio runtime parks most of
its work on threads the leader knows nothing about — so the one number that exists to
prove `dozed` is cheap was the least trustworthy number in the tool.

The fix sums `/proc/<pid>/task/*/status`. Measured cost over 104 threads of three real
desktop processes: **1.25 ms per sample, against 0.036 ms** for the leader-only read —
about 12 µs per thread.

### That per-thread figure is right; the example around it was not

This section used to conclude "at the 2s sampling floor that is 0.06% of a core", and
that number was quoted whenever the sampler's cost came up. It describes three processes.
A real session is not three processes.

Measured on a live daemon, nine apps, 48 minutes of steady state:

| | Design's example | Measured, real session |
|---|---|---|
| Processes swept | 3 | **173** |
| Threads swept | 104 | **~2,725** |
| Per sweep, at 12 µs/thread | 1.25 ms | **~33 ms** |
| Sustained `status` opens | — | **740/s** |
| Cost | 0.06% of a core | **0.58% of a core** |

Ten times the stated figure, and the per-thread cost was never wrong — the example was
26x too small. Two things make a real session bigger than "processes belonging to apps
we manage". Browsers are dozens of processes each (firefox 45, brave 34). And a terminal
that has launched GUI apps carries all of them in its own tree, so
`org.wezfurlong.wezterm` alone reports **77 pids** and those threads are swept twice —
once under the terminal, once under each app that inherited its scope (§4).

The sweep interval matters as much as its size and is not the 60s idle case either: with
several apps hidden and grace clocks running, `adaptive_interval` sits near its floor.
Measured here, one full sweep every **3.7 s**.

None of this is a regression and none of it is alarming — 0.58% of a core against the
hundreds of wakeups a single frozen browser gives back is still a trade worth making. It
is recorded because the old figure invited the conclusion that sampling was free, and at
740 file opens a second it is the most expensive thing `dozed` does.

#### The `schedstat` trade, re-run rather than inherited

`/proc/<tid>/schedstat` was dismissed here as "not worth 0.6 ms" against a 1.25 ms
baseline. That baseline turned out to be 26× too small, so the same halving is worth ~16 ms
of the most expensive thing dozed does — and this section said the trade should be re-run
rather than inherited. Re-run, on this machine, over **5,190 threads**, best of five
alternating sweeps so both readers meet the same cache:

| | |
|---|---|
| `status` sweep | **31.24 ms** |
| `schedstat` sweep | **14.11 ms** |
| Agreement, summed increments over 5 s | 26,852 against 26,853 — **0.004%** |

Twice as cheap and the same number, so the sampler now reads `schedstat` where it can. The
agreement is what licenses the swap rather than the cost: the two counters are not the same
quantity — `sched_info.pcount` counts times a task was put on a CPU, where `status` sums
voluntary and involuntary switches away from one — and their absolute values differ by a
small constant. dozed only ever differences them, so a constant offset is nothing, and the
*increments* are what had to match.

One correction to the old note, and it matters because it decides how many machines this
reaches: it is **not** `CONFIG_SCHEDSTATS` that this needs. The `sched_schedstats` sysctl
was **`0`** on the machine measured above, which is the shipped default nearly everywhere,
and the third field was maintained regardless — it comes from `CONFIG_SCHED_INFO`, which
delay accounting also turns on. What the sysctl gates is the *second* field, the run-queue
wait time, which nothing here reads.

**It has to be able to refuse, because the failure would be silent.** Where `pcount` is not
maintained the file still exists and reads a permanent `0` — which would arrive as *every
app wakes the CPU zero times a second*. No rule keyed on `wakeups_per_sec` could ever fire,
`dozectl report` would show nothing worth freezing, and nothing anywhere would say why.
That is §10's original sin exactly: a signal nothing sets, and no line of output admitting
it. So the counter is probed once at startup against the one task certain to have been
scheduled — our own — and a zero there means `status` for the life of the run.
`dozectl status` says which is in force **when it is the expensive one**, since `schedstat`
is the ordinary case and owes nobody an explanation.

Threads exiting between two samples take their counters with them, so a delta can come
out negative; it clamps to zero, which *under*-reports during thread churn. That is the
safe direction — a low wakeup rate means we leave the app alone.

### What recording costs

Session recording (§14) adds no polling loop of its own: it writes from the events and
ticks that already happen. Measured idle, dry-run, one hidden app, all threads counted:

| | Wakeups/s | Window |
|---|---|---|
| Recording | 7.51 | 30.1s |
| `--no-trace` | 7.69 | 13.7s |

Indistinguishable — recording is free within the noise, and the trace grew at roughly
3 KB per three minutes of idle, a few megabytes for a working day.

**That row is the cheap regime, and it is the only one measured.** systemd's own accounting
across 135 real runs of the same daemon spans 6.4s CPU over 5h45m (0.03%) to **51.2s over
1h36m (0.89%)**, and peak RSS from 6.0 MB to 58.7 MB. The table above says what *recording*
costs on an idle machine; it says nothing about the regime that produces 0.89%, and this
section has never had a number for it. Some of the RSS spread had a cause and it is fixed
below; the CPU spread does not yet have an explanation, and saying so is better than
leaving the 7.51 figure to stand in for it.

### Two files that grew without a bound, and one that spent its ring badly

A trace is one file per run with ten runs retained, so it could never grow without limit.
The **journal is the opposite shape** — one file, appended to for as long as the tool has
been installed — and nothing bounded it. Measured: 1.9 MB and 3,947 entries after thirteen
days, read into memory *in full* at each of 135 restarts. A daemon whose resident memory
grows with how long ago it was installed is depending on the wrong quantity, and
extrapolated to a year it is a 55 MB file parsed at every login.

Two bounds, because they answer different questions. The file rolls over at 8 MB keeping
one previous generation — roughly two months of real use before a rotation and four before
anything is lost, so `dozectl report --week` and any "versus last month" comparison are
answered from the live file alone. The backlog read is separately capped at 5,000 entries,
and reads the previous generation first so a rotation is invisible until the second one.

The trace had a bound and spent it badly. Three restarts inside two seconds each wrote a
3.8 KB file containing a header and whichever windows happened to be open, and pushed three
real sessions — one of them 398 KB — out of the ten-deep ring. Eleven kilobytes of startup
banner displaced most of a day's evidence. A trace is now judged on **conclusions**:
`dozectl replay`, `dozectl suggest` and every comparison between two runs read derivations
and intents, so a session with none of them replays to an empty intent stream and is
deleted at exit rather than claiming a slot. A genuinely quiet run is deleted too, which is
the intended answer — there is nothing in it to read, and the journal already has whatever
was decided.

### A recorder that breaks must not look cheap

`dozectl status` reports trace size as the evidence that recording is affordable. That makes
the failure mode inverted: a recorder that stopped reports a *smaller* number than a working
one, so the status line reads best exactly when things have gone worst.

Seen live: two `ENOSPC` warnings sixteen minutes apart, and the daemon kept writing for
hours afterwards. A failed *write* already stopped recording; a failed **flush** only warned,
which is the worst of both — the trace has a hole in it and nothing records where, and §14
replays it as though it were complete. A trace with unmarked gaps is worse than no trace.

Both now stop recording and keep the reason, and `TraceStatus::stopped` carries it to
`dozectl status`, which prints it beside the size. The size cap does the same. A number
offered as evidence has to be able to say when it is not evidence.

#### What a trace actually costs, measured on a session rather than on an idle minute

The figure above — 3 KB per three minutes, "a few megabytes for a working day" — was taken
from an idle machine with **one hidden app**, and it is not what a session costs. Measured on
a real 68-hour run with seven apps:

| | |
|---|---|
| Records | 341,955 |
| Size | 64 MiB, **hitting the cap in 3.6 days** |
| Growth | ~18 MiB/day, **12× the figure above** |
| `sampled` events | **93% of records, 78% of bytes** |

This is the same mistake §13 already corrected once for the sampler's own cost — *"the
per-thread cost was never wrong; the example was 26× too small"* — repeated for the recorder.
It also makes `MAX_TRACE_BYTES`'s comment false: "anything approaching this is a bug worth
noticing" describes ordinary use.

Two changes, and both are the discipline this section already claims rather than new policy.

**Rates are quantised at source**, to 0.1 for CPU percent and wakeups a second and 0.01 for
GPU percent. `wakeups_per_sec: 2290.4712` claims seven significant figures for a counter
difference divided by a wall-clock interval, averaged over a minute; it does not have them.
Done in the sampler rather than on the way to disk, so the planner and a replay of the same
session see the identical number — rounding only for the trace would mean the daemon decided
on one value and `dozectl replay` on another, and a rule with a threshold between the two
would break §14's promise.

**A sample identical to the last one recorded for that app is not recorded again.** §14
already applies exactly this to derivations — "edge-triggered on the derivation, never on the
tick", because "recording those would make the log a firehose" — and then wrote the raw
samples on every tick for every app regardless. Suppressing the repeats is lossless rather
than a trade, which is the only reason it is permitted: the world keeps the last sample it
was given, so a replay that never sees the repeat decides against identical state; and
`ReportAccumulator::on_sample` weights by `dt` since the previous sample rather than by how
many arrived, so `dozectl suggest`'s numbers do not move.

It only pays because of the quantisation. Measured on that same recording: at full `f32`
precision **4.8%** of consecutive per-app samples repeat, and at 0.1 it is **40%** — 122,259
records, about 21 MB.

**And the directory has a budget now, not just a count.** `MAX_TRACE_BYTES` bounds one file
and `KEEP_TRACES` bounds the number, and neither bounds the product: ten runs at the file cap
is 640 MiB of somebody's disk held by a diagnostic they did not ask for. `MAX_TRACES_BYTES`
caps the retained runs at 256 MiB, newest first, and a single oversized run still keeps its
own file — deleting the recording currently being written to make room for nothing is not a
saving.

**That exemption is part of the bound, so the bound is not 256 MiB.** The file being appended
to is not counted, which puts the real ceiling at 256 MiB plus one `MAX_TRACE_BYTES`: 320 MiB.
Measured on a live machine at 283.6 MiB — a 252.2 MiB retained set, correctly under budget,
plus a 31.4 MiB file still growing. Worth stating rather than rounding off, because a budget
quoted 25% under what it permits is the same sort of claim as a size that reads as good news:
true of the thing it measures, and not of the thing the reader cares about. **And pruning says
so when it happens** — it logged at `debug`, so a recorder deleting somebody's recordings to
stay inside this did it silently at default levels.

#### Stopping was the right half of the answer, and the wrong whole of it

*"A trace with unmarked gaps is worse than no trace"* is correct about the **gap** and was
wrong about the **remedy**. `ENOSPC` is the case this was written for, and a full disk is a
condition that ends: somebody empties a download folder at lunchtime, and under the rule
above the recorder stays dead until the next restart — so a disk that was full for ten
minutes at nine in the morning cost the whole day's recording, and nothing about that was
ever going to fix itself.

The recorder now **retries every 60 seconds, for as long as the daemon runs.** There is no
attempt limit and there should not be one: no number of failures makes the next attempt less
worth making, and a recorder that gave up after ten tries would be this same bug with a
longer fuse. Nothing else changes about §13's cost argument — the retry rides the existing
tick, so there is no timer and no wakeup, and on a healthy recorder it is two comparisons.
The minute is not the tick: the tick drops to two seconds near a grace deadline, and
hammering a full disk thirty times a minute is the daemon becoming the thing it removes.

**Resuming is only defensible because the gap is marked.** `TraceRecord::Gap` goes into the
stream at the point recording came back, carrying how long it was down, how many records are
missing and why — so the objection above is answered rather than ignored. Everything
downstream can then act on it instead of reading across the hole in silence:

* `dozectl trace` prints the gap as a row of its own, and keeps it even under `--app`,
  because "this app looks quiet for eleven minutes" is exactly the question a filtered trace
  is being read to answer.
* `replay` counts them, and `dozectl replay` with no `--config` **no longer claims the
  planner is non-deterministic** when a trace fails to reproduce itself. That check used to
  hedge — *"the planner is not deterministic, or the trace is incomplete"* — while failing
  anyway and leading with the bug claim. It can now tell the two apart, and only fails for
  the one that is a fault.

Four details, each of which was wrong in a first draft and is a test now:

* **The hole starts at the last record that reached disk**, not at the tick that noticed. A
  `BufWriter` over a full disk accepts bytes happily and fails only when they are pushed, so
  the record that provokes the failure is itself one of the lost ones — anchoring to what was
  merely buffered dated the gap from a record that is not in the file.
* **`records` and `bytes` are corrected downwards on failure.** Those two numbers are what
  `dozectl status` offers as evidence that recording is working, and everything sitting in
  the buffer at the moment of failure was never written. Counting it would have a broken
  recorder overstating its own output, which is the inverted-health-signal problem from the
  top of this section arriving from the other direction.
* **A newline goes down before the gap marker.** The failed write can leave a partial line;
  appending straight onto it would fuse the torn record and the marker into one unparseable
  line, costing the marker — and the marker is the thing that makes the whole resume
  defensible.
* **Opening the file is not the test.** A full disk lets you `open` all day. The probe is the
  gap marker itself followed by a real flush, and only a flush that returns `Ok` counts as
  recovery. Anything less clears the warning while writing nothing.

**The size cap rotates**, and the same distinction decides how. A write error is the *world*
refusing and the world changes, so it is retried; the cap is this program deciding something
about its own files, so the answer is to make a decision rather than to wait.

Stopping the run at the cap was that decision, and it was the wrong one for the same reason
`ENOSPC` was. The bound that actually matters is `MAX_TRACES_BYTES` — the disk a diagnostic
may hold — and one file reaching a limit says nothing about it. Worse, the two defects
compounded: `prune` runs when a trace is **created**, so a recorder that never created
another one never pruned again. Measured on a live machine, the cap was hit on day four of a
nine-day run and the directory sat 2.9 MB over its own budget with no recording going on at
all. The size cap was holding disk, not saving it.

So the cap closes the file, marks it in-band with the name of its successor, opens the next
part and prunes. One change, because the thing that bounds the directory is the thing that
resumes the recording.

* **Every part carries the run's header**, including `wall_start` unchanged. §14 reads a
  trace standalone, and a file beginning mid-session with no version, no config and no
  anchor has no way to turn its monotonic offsets back into times of day. `wall_start` is
  the unix time at `MonoTime::ZERO` and every `at` in every part is an offset from it, so a
  part that "corrected" it to its own start would decode every timestamp in itself wrongly.
  The *file name* is the rotation's wall clock, which is what keeps the directory listing
  chronological.
* **The new file is opened before the old one is marked.** That is the step that can fail,
  and failing after writing "continues in ..." would leave a trace pointing at a file that
  does not exist. A reader that cannot trust a marker is worse off than one that has none.
* **A rotation is not a gap.** Nothing is missing across it, and counting one would have
  `dozectl status` report an incomplete recording for a recorder working perfectly — the
  same false alarm as the small file that read as good news, from the other direction.
* **`dozectl status` says which part it is writing.** The size on that line is the evidence
  that recording is affordable, and at a boundary it drops from 64 MiB to nothing for a reason
  that has nothing to do with cost. Said where it is true, which is after a rotation.

**What still stops is a rotation that cannot open its next file** — and that *is* retried,
like any other refusal from the world. It was not, once, on the reasoning that rotation had
already tried the only thing a retry would try. That reasoning was inherited from the cap and
did not survive the cap learning to rotate: the only way to arrive here now is an `open` the
filesystem turned down, which is the same `ENOSPC` the write path recovers from. Leaving it
alone meant one disk filling and emptying revived a recorder that failed a *write* and left
one that failed a *rotation* dead for the rest of the run — same fault, same cure, opposite
outcomes, decided by nothing but which line of code noticed first.

* **The reason names what refused**, not the limit it was passing at the time. This read
  "hit the 64 MiB size cap", which describes the harmless half of the event, hides the half
  that is a fault and drops the errno that says which fault. A user was told the recorder had
  reached a limit by design while the disk was full.
* **A stall writes no marker.** The recorder is waiting, not finished, so the file is not
  truncated yet — the marker that closes it is written when it is actually closed, by the
  rotation that succeeds or by `Drop` if the run ends first. Writing one at the stall *and*
  another on recovery would leave the file asserting both that it ended there and that it
  goes on.
* **A rotation that recovers from a stall is a hole, not a seam.** Records were offered and
  dropped while it was stuck, and the marker carries how long and how many — the same shape
  the retry path writes, because a reader's question is the same.

**And a gap says which of the three it is**, in a field rather than in its prose.
`GapKind::Rotated`, `Hole` and `Truncated` were distinguishable only by reason text, where
two of them differed by a suffix: `"hit the 64 MiB size cap"` for a run that ended there and
`"hit the 64 MiB size cap; continues in ..."` for one that did not. Opposite outcomes, the
bad one a strict prefix of the good one, and every reader wanting the difference matching on
English. `loses_records()` is the question they were all actually asking, and it is what
decides whether a replay counts the recording incomplete.

**Two states, and only one of them clears.** `stopped` answers "is the recorder working now"
and goes to `None` the moment a write succeeds — the user asked for exactly that, and a
message describing a condition that has passed is the same stale claim as a size that goes
down when things break. `gaps` answers "is the file complete", and nothing makes that true
again, so it is reported for the life of the run. A reader told only the first would take a
healthy recorder for a whole recording.

What the same measurement *did* show is that **the daemon's wakeups are almost entirely
the 5s UPower/logind poll in `watch_system`**, not the planner and not the recorder.
Against the 165 wakeups/s a single frozen app gives back that is a good trade, but it was
the obvious thing to cut next.

### Cutting it, and the trap on the way

The cost was not where the poll's own comment claimed. Traced on the system bus while
idle, each 5s cycle cost **15 D-Bus method calls — and not one of them a property
`Get`**. `watch_system` built its three proxies *inside* the loop, so every cycle paid a
`GetNameOwner`, a `GetAll` and an `AddMatch` per proxy to construct a property cache,
read every value out of that cache for free, then `RemoveMatch`ed it all on drop. The
setup and teardown was the entire cost; the data was already free.

The tempting fix is to hoist the proxies and stop there. It measures beautifully — bus
traffic goes to **zero** and the daemon drops to 2.0 wakeups/s — and it is wrong.
**logind does not feed the property cache for the session properties read here.** With
caching left on, `SetIdleHint b true` left `dozectl status` reporting `desktop active`
for a full minute while logind reported `IdleHint=true` throughout. The daemon would go
permanently blind to idle and lock, and §8's entire saving comes from noticing the user
left. Worse, it fails silently: the frozen readings are the plausible-looking ones, "on
AC" and "active", exactly the trap §7 already records for the wrong-bus bug.

So the proxies are hoisted **and caching is explicitly disabled**: one real `Get` per
property per tick, six per 5s, on proxies built once. Measured on the live daemon,
method calls per 10s went 30 → 12, with all twelve now carrying data, and the reported
wakeup rate 8.2/s → 4.7/s. Half the win the cache appeared to offer was the daemon
quietly going blind.

If someone later reaches for `PropertiesChanged` again, it has to be verified against
`SetIdleHint`, not against a traffic graph — the traffic graph is what makes the bug look
like a success.

Both figures are visible in `dozectl status`, which reports the daemon's own wakeup rate
and the trace's path, record count and size. A recorder whose cost is invisible is asking
to be taken on faith.

### What being visible costs

A daemon that can stop your applications should be visible without being asked, and the two
things that make it visible are held to the same rule as everything else here.

**The tray indicator redraws on change, never on a clock.** The daemon writes its state into
a `watch` channel after each tick and `send_if_modified` compares it first, so an idle
desktop — where the state is the same tick after tick — wakes the tray task not at all. The
state deliberately holds only what a user would notice changing (mode, held count, apps
known, snooze remaining), because every extra field is another way for two equal states to
compare unequal and cost a repaint.

The indicator is also **a client, not a second control path**: each menu item sends an
ordinary `Request` down the same channel the unix socket uses, so it can do exactly what
`dozectl` can do, through the same code, and every click is journalled and traced like any
other. There is no tray-only path into the daemon that could behave differently from the
documented one.

`ui.tray` turns it off, and it defaults to **on** — a deliberate exception to how quiet the
rest of this daemon is, on the grounds that something which can stop your applications
should be visible without being asked and the panic thaw should be one click from wherever
you are. The setting is carried in the published state rather than read once at startup, so
`dozectl reload` takes the icon away and puts it back; a setting that needs a restart to
apply is one people conclude is broken.

Three things about it were wrong on first contact with a real desktop, and each is worth
recording because each is a class of mistake rather than a typo:

* **The menu opened on no button anyone presses.** ksni routes a left click to
  `Tray::activate`, which this item does not implement, so the icon looked dead to
  everything but a middle click and the menu holding the panic thaw could only be reached
  by knowing to right-click. Everything the indicator does is *in* the menu, so
  `MENU_ON_ACTIVATE` is set and every button opens it.
* **"Open dashboard" could not find the dashboard.** `Command::new("dozed-gui")` searches
  `PATH`, and a `systemd --user` service does not inherit a login shell's — it gets
  systemd's own, which has nothing under `$HOME`. `install.sh` puts both binaries in
  `~/.local/bin`, so a correctly installed pair produced `os error 2` on every click. The
  daemon's own directory is stronger evidence about where its dashboard is than a search
  path that was never meant to describe this service, so `current_exe()`'s parent is tried
  first and `PATH` is the fallback for distributions that split the two up. The failure
  notification names the path it tried, because "not found" without the path sends people
  looking in the wrong place.
* **The icon was three media-player glyphs, and then it was nothing at all.**
  Play/stop/pause said "something is running" rather than "this is dozed". Replacing them
  with a snowflake in three weights — thin outline idle, bold with a solid hub while
  holding, grey with a pause badge while snoozed — was right; shipping them as *icon names*
  in `hicolor` was not, and produced an empty tray.

  The names were fine. `Gtk.IconTheme` resolves all three, asked directly. What is not fine
  is that resolving a name depends on the host's icon cache **at the moment it looks up**,
  and a panel already running when the installer wrote the files may never see them — and a
  name that fails to resolve is not a fallback to anything, it is a blank space that reads
  as a broken daemon. The icon is now sent as pixels in the D-Bus message, which has nothing
  to find, nothing to cache and nothing to invalidate. It is the pattern ksni's own
  custom-icon example uses.

  Pixels are usually an embedded PNG, which costs an image-decoding dependency and pins the
  icon to whatever sizes somebody remembered to export. The flake is three crossed lines and
  six chevrons, so it is cheaper to describe than to store: `crates/dozed/src/icon.rs`
  rasterises it at whatever size the panel asks for, anti-aliased by distance — every shape
  is a thick segment or a disc, and for both the distance from a pixel centre is a closed
  form. No blobs in the repository, no new dependency, and a high-DPI panel gets a sharp
  icon rather than a scaled 22-pixel one.

  The three were drawn against a rendered 22-pixel panel on light and dark, and the first
  attempt failed that test: idle and holding differed only in stroke width and were
  indistinguishable at a glance. The gap is now weight *and* a solid hub, and snoozed
  carries a badge rather than only a colour, because at that size a colour shift alone is
  not reliably noticed. An `#[ignore]`d test emits the images so the same check can be
  repeated rather than remembered.

* **The tray lagged the thing you just did.** State was published on the sampling tick,
  which backs off to 60s when nothing is pending — so after `dozectl snooze` the icon spent
  up to a minute showing the state before the command. It is published after every command
  as well, which costs nothing, because an unchanged state is compared and dropped before it
  is sent.

**One notification a week, and it is the only one dozed sends.** A notification per freeze
would be dozens a day from a daemon whose entire premise is that you cannot tell it is
running (§1). Weekly is roughly the interval at which the figure has actually changed. Three
things make it honest rather than an advertisement:

* It quotes `Report::covered`, the wall-clock union, **not** `total_dozed`. App-time is a
  sum across apps and legitimately exceeds the window — six apps held for five hours is
  twenty-five hours of it — which is defensible in a table with a legend beside it (§11.0)
  and indefensible in one line, where "dozed saved you 25 hours this week" reads as a lie
  about a 168-hour week.
* The counterfactual survives into it. In dry-run it says *would have*, and the modelled
  charge fraction stays a range, per §11.1.
* The clock is on disk and gated on an hour of uptime. A laptop restarts more often than
  once a week, so an in-memory timer would never fire at all; and a missing state file
  seeds the clock rather than firing immediately, or the first notification a user ever
  sees reports on the four minutes since login.

### 13.1 Signals where they exist, a slow poll where they do not

Someone did reach for `PropertiesChanged` again. Taking the warning above seriously, the
answer is **both**, and the split follows the evidence rather than the preference:

* **UPower does publish.** Its `PropertiesChanged` on the manager object covers unplugging
  the charger *and* shutting the lid. Subscribed via `fdo::PropertiesProxy` rather than the
  typed proxy's per-property streams, because those are tied to the property cache and the
  cache is off here on purpose.
* **logind's session hints still do not.** That is the measurement above and nothing has
  changed about it, so `IdleHint` can only be seen by asking.
* **logind does emit `Lock` and `Unlock`**, which are real signals on the session object.
  They cover a hotkey, an idle timeout or `loginctl lock-session` — though not a locker that
  locks itself and only sets the hint, which is why they supplement the poll rather than
  replace it.

Both signal sources feed one channel of "go and look now" nudges, and a nudge carries no
payload: whatever woke the loop, the answer still comes from reading the properties, so
there remains exactly one place that decides what the desktop state is.

That makes the poll a *backstop*, and a backstop can be slow. It moved 5s → **15s**, and
the reads per cycle 6 → 3 now that power arrives by signal: **1.2 bus calls a second to
0.2**. The worst case it now governs is noticing a session went idle up to 15s late, which
is 15s of extra patience against a grace measured in minutes.

The claim is checkable rather than assertable: `dozectl status` now reports **`own bus
calls`** alongside `own wakeups`. That loop is most of what an idle daemon does — the
sampler backs off to 60s when nothing is pending — so it is precisely the cost §13 exists
to keep visible, and a future change to the interval can be judged instead of argued.

### 13.2 The lid, and why it is only corroboration

`UPower.LidIsClosed` was available all along and unread. It answers a question no other
signal can: "no keypresses for ten minutes" and "nobody is there" read identically to
logind, and only the second deserves a grace of zero.

It is deliberately **not** allowed to override input. A shut lid on a machine that is still
running means either an unattended laptop or a docked one whose user is working on an
external display, and UPower cannot tell them apart — and since the default
`HandleLidSwitch=suspend` makes the unattended case suspend itself (which
`PrepareForSleep` already handles), a lid closed *while dozed is still running* is
disproportionately the docked case.

So the rule is: the lid may upgrade `IdlePresent` to `Away`, and may never contradict
`Active`. Someone typing on an external keyboard is present whatever the lid is doing, and
calling that `Away` would collapse grace to zero and freeze the apps they are alt-tabbing
between — the exact harm `active_factor` exists to prevent. `LidIsPresent` gates the whole
thing, so a desktop reports "open" rather than "unknown".

The mapping is a pure function of four booleans with a test per row, because the async
property reads around it previously made it untestable — which is how this rule would
otherwise have shipped on reasoning alone.

---

## 14. Testability, and being debuggable

The planner is a pure function:

```rust
fn plan(&mut self, state: &WorldState, event: &Event, now: Instant) -> Vec<Intent>
```

No I/O, no syscalls, no ambient clock. Everything OS-specific sits behind
`FocusSource` / `ActivitySource` / `PowerSource` / `ProcSampler` / `Actuator` traits with
fake implementations in `dozed-core::testing`.

Integration testing that actually freezes things happens in a **nested compositor or a
VM**, never the live session.

### The journal is not enough

An intent is only produced when the applied action **changes**. That makes the journal a
good record of what `dozed` did and a useless one for the two questions people actually
have while debugging it: *why did my rule not fire*, and *why has nothing happened*. An
app sitting in grace for five minutes produces no intent at all, so it leaves no trace in
the journal; a rule that quietly stops matching leaves none either.

Three things close that gap, and all of them are in the explain path rather than the
decision path, so none of them can change a decision:

* **Every decision carries the state it was made from.** `Reason` holds a
  `DecisionInputs` — visibility, class, exemptions, sample, desktop, power, cooldown —
  and the factorised `Grace`. "Hidden for 5m12s" is a claim; the same line with the
  inputs attached is checkable after the fact.
* **Rules report why they lost.** `predicates_match` is defined as "there is no
  `Mismatch`", so one code path answers both "does this match" and "which predicate
  rejected it, and with what values". `dozectl why --rules <app>` prints the whole table,
  including rules that match but were outranked — its own confusing failure, and
  invisible if the scan stops at the winner.
* **Derivations that produce nothing are still recorded.** `Planner::take_decisions`
  surfaces them. It is edge-triggered on the derivation, never on the tick: the sample
  and the battery percentage drift continuously, and recording those would make the log a
  firehose and `dozed` a wakeup source, which §13 forbids. Nothing is lost, because
  anything a sample can change about a *decision* shows up as a different rule or a
  different desired action.

### Record and replay

The daemon records every session to `~/.local/state/dozed/traces/<start>.jsonl` — one
JSON object per line, one file per run, ten runs kept. `--no-trace` turns it off.

One stream rather than three files, carrying the header, the events, the derivations and
the intents under a **shared sequence number**, because the whole value is being able to
read an event, the derivation it caused and the intent that followed in the order they
happened:

```
     3 +2.022s event    app-seen org.kde.plasmashell (1 pids)
     4 +2.022s decide   org.kde.plasmashell wants restore — session infrastructure:
                        never actionable [focused active on-ac 79% cpu 0.0% wakeups 0/s]
     5 +2.022s event    visibility org.kde.plasmashell -> hidden
```

`dozectl trace` reads one back (`--app` to filter, `--decisions` to drop raw events);
`dozectl replay <trace> --config new.toml` re-runs it through the planner and diffs the
resulting intents per app. That is both the regression suite and the rule-tuning
workflow — you tune against your own recorded day instead of guessing. **Replaying a
trace against its own config must reproduce its intent stream exactly**, and `dozectl
replay` with no `--config` exits non-zero if it does not, because a drift there is a bug
in the seam rather than a tuning result.

Two things the implementation had to get right, both found by running it:

* **Conclusions are flushed; events are buffered.** The idle tick backs off to 60s, so
  buffering everything meant a `SIGKILL` losing up to a minute of trace — in the run most
  worth reading. Decision and intent records flush immediately, which drags the events
  that led to them to disk as well; only ticks and samples that changed nothing are ever
  at risk.
* **A replacement config replaces recorded reloads too.** Otherwise a recorded
  `ConfigChanged` halfway through the day silently undoes the substitution and answers a
  question nobody asked.

**Measured cost of recording**, on this machine over 60s idle with one hidden app: see
§13.

### 14.2 What the tests cannot reach on their own

Three additions, each aimed at a class of failure the existing tests structurally cannot
catch.

**Randomised sessions against the invariants.** Every safety property in §10 has a unit
test built from the situation that motivated it, which is the right way to catch a bug you
already know about. `tests/invariants.rs` generates random rule tables — including silly
ones, contradictory ones, exemptions at priorities where they can never win — drives random
event streams through them, and asserts what must hold regardless: session infrastructure is
never touched, nothing is held while focus cannot be seen, panic thaw and shutdown and
suspend leave nothing held, the focused app is only touched by a rule that asks for it,
every app can always be explained, no intent is issued that changes nothing, and grace never
grows as the battery empties.

Hand-rolled rather than `proptest`, because the generator has to produce *plausible*
sessions and `dozed_core::stats` already has a seeded PRNG. Each case is reproducible from
its seed and printed on failure; what is given up is shrinking.

The invariant tests were mutation-checked rather than trusted: disabling the protected-app
guard in the planner made `session_infrastructure_is_never_touched` fail with
`protected app comm:kwin_wayland was asked to throttle`. A property test that has never
been seen to fail is a property test that might assert nothing.

**A golden session.** §14 already guards *determinism* — a trace must replay to the same
intents — but not *behaviour*: change the grace model and both the recording and the replay
move together, with nothing anywhere saying so. So `tests/fixtures/session.jsonl` is a
committed 239-record session and `session.intents` is the 22 lines it must decide, covering
every rung including a maintenance window, a user returning, an audio exemption and a
suspend. A behaviour change now arrives as a diff in the review. Regenerating is a separate
`--ignored` test, because a harness that rewrites its own expectation on failure asserts
nothing.

**`dozectl export`.** The week of dry-run this section asks for produces a report read once,
which is an anecdote; the same numbers plotted across the week are where the pattern lives.
Prometheus exposition format for `node_exporter`'s textfile collector — a command rather
than an HTTP endpoint, because a battery tool that listens on a port has to be woken to
serve it (§13). The series worth alerting on are `dozed_leaking`,
`dozed_exemption_source_reporting` (0 means a safety net drawn but not connected) and
`dozed_self_wakeups_per_second` against the saving.

The exporter's tests validate the output as a parser would — every sample's name declared
with a HELP and a TYPE, every value a number, no series repeated — which immediately caught
one series emitted with neither. Label values are escaped, because an app name is derived
from a desktop file or an executable path and one stray quote does not produce a wrong
number, it produces a file the collector discards whole. Verified against the live daemon:
60 samples, 17 metrics, valid.

### 14.1 Replay as a search, not only a diff

`replay --config` delivers half the tuning story: write a candidate, see exactly how the
day would have gone differently. The missing half is that **you still had to invent the
candidate**, and the knobs that matter are multiplicative factors on a grace product (§8)
whose effect on a real session nobody can predict by reading it.

The planner is pure and its clock is injected, so a day can be re-simulated as often as
you like. `dozectl suggest` does that a few dozen times.

The search is the easy part; the objective is where this could go wrong. **Wakeups avoided
rises monotonically with aggression** — freeze everything the instant it loses focus and
you avoid the most wakeups any config can, on an unusable desktop. So a search over
savings alone does not need to be clever to be wrong, only to be run.

The cost side is a number a trace can actually answer: an **interruption**, meaning a time
the user switched to an app that was being held and had to wait for it. That is what people
complain about — "I switched to Slack and waited for it to wake up" — and the recording
contains the moment they came back.

Counted at the return, with no regard for how long the hold had been running. The first
version instead asked whether the hold was younger than a minute — "did it buy enough quiet
to be worth the thaw" — and that is the wrong shape for an objective, in two ways measured
on a real 2h53m recording from this machine:

* **It ran backwards.** `grace x1.50` holds *less* than the config it was compared against
  — 26 holds where the baseline made 29 — and scored 8 contradicted holds against the
  baseline's 5. A cost that rises as the tool gets more patient cannot constrain a search
  for aggression.
* **The search optimised its edge.** Signal was held at 8965.5s and the user came back at
  9024.9s, 59.4s later: inside the window, counted. Shorten every grace by a quarter and
  the same hold starts at 8957.2s, the same user returns at the same instant to the same
  held app — 67.7s later, outside the window, not counted. The candidate held the app eight
  seconds *longer* and scored one interruption better. With five in the whole session, that
  step across a threshold was a fifth of the entire cost side.

Both faults come from mixing benefit into a cost term: hold age is a statement about what
the hold bought, and what it bought is already the other axis. Counting the wait alone
leaves the two independent and makes the number monotone in aggression — 16 waits at
`grace x3.00` rising to 30 at `grace x0.25` on that recording, which is the property that
makes it usable as a constraint at all.

The recommendation is therefore *constrained*, not weighted: no more interruptions than
the config already in use, and among those, the most wakeups avoided. Weighting would need
an exchange rate between a million wakeups and one interruption, and there is no such
number to defend. Thaws are reported but deliberately not constrained — a config that
dozes more thaws more, so bounding them at the baseline would reject every improvement,
and the reconnect cost (§7) is a judgement for the reader. Interruptions are not the same
number: a config can hold *more often* without making the user wait more often, if the
extra holds land in stretches they never came back to, and on that recording two candidates
did exactly that at 23 waits against the baseline's 23.

Four further deliberate limits:

* **It cannot invent or reorder rules, or touch the exemptions.** Every candidate is the
  config you already have with scalar factors moved, so the output is a handful of numbers
  and the safety net is untouched by construction.
* **The baseline is replayed through the same path as every candidate**, not read from the
  recorded intents. Otherwise the comparison includes actuator refusals and any mid-session
  reload, and a candidate could win for reasons unrelated to its rules.
* **There is an attribution pass as well as a descent.** Coordinate descent makes every
  ladder after the first carry the winners of the ones before, so each knob is also scored
  *alone* against the baseline. "Shorten the grace" is a change somebody can reason about;
  "these four factors together" is one they can only accept or reject.
* **The descent is followed by a sweep of everything evaluated.** Descent walks a path:
  once it takes a rung from the grace ladder, every later ladder is scored *combined with*
  that rung, and a combination can break the interruption budget where the knob alone would
  not. On the same 2h53m recording it took `grace x0.75`, found every active-factor rung
  combined with it over budget, and stopped — while `active-factor x0.50` alone was inside
  the budget and saved 6% more. The attribution pass had already replayed it, so it was
  printed two rows above the recommendation and could not win. The sweep costs nothing,
  because every candidate it considers has already been paid for.

### The recording must be one in which nothing was held

The savings are projected from what the trace observed each app doing while a candidate
would have held it. If nothing was ever actually held, every app was observed running
throughout and the projection has something to project from. If the daemon really did
freeze things, an app reads as silent for exactly the stretches it was frozen — and a
candidate is credited only for the sliver where it would have acted sooner than the
recording did. That is a marginal quantity wearing the units of a total: it can only reward
aggression, and it cannot score a candidate more patient than the recording at all, which
is why every longer-grace candidate comes back at approximately zero rather than at what it
would really cost.

The test is the **recorded outcomes**, not the header. The header says which mode the
daemon *started* in, and `dozectl mode` changes the running mode without rotating the
trace — so a session told to stop acting halfway through still calls itself live, while a
session started live on a quiet day may never have held anything and be perfectly
measurable. What breaks the arithmetic is an app having been silenced, so what the search
looks for is a hold carried out: an `Intent` recorded with `Outcome::Applied` that is not a
restore. A freeze that *failed* left the app running, and therefore observable.

So `suggest` declines to recommend from such a recording rather than answering confidently
from arithmetic that does not apply. `--write` refuses for the same reason, and `--anyway`
overrides both while keeping the caveat attached. The table is still printed: `HOLDS`,
`THAWS` and `WAITS` come from the replayed decisions and the recorded events rather than
from the samples, so they are sound either way.

A recording's mode is fixed when the daemon starts, so getting a tunable one means a fresh
run started without `--live` rather than a call to `dozectl mode`.

Verified on a real 1-hour dry-run recording from this machine: the shipped config avoided
107k wakeups, the search found 806k inside the interruption budget, and attributed 635k of
it to the grace factor by itself. (The interruption counts from that session — 3 against
1 — were measured under the older definition above and are not comparable to the current
one; the savings are, because that arithmetic did not change.) `--write` emits a config
that `dozed --check` loads and `dozectl replay` re-runs, which is what makes the loop
close.

Ladders are coarse (0.25x to 4x, a handful of steps) on purpose. A trace is one day of one
person's habits, and resolving a factor to three decimals against it is fitting noise.

---

## 15. Packaging

* `systemd --user` unit, `WantedBy=graphical-session.target`.
* **Not a Flatpak.** A sandboxed daemon cannot see or freeze host processes.
* Ship curated per-app presets (Slack, Discord, Spotify, Steam, Docker, VMs) and a hard
  "never" class for VMs, containers, backup and sync clients.

### The starter kit, and reaching the installs that already exist

The config is written once, on first install, and never touched again. That is the right
default — a tool that rewrites your config on upgrade is a tool that undoes your edits, and
`packaging/test-install.sh` exists because the installer once did exactly that to the unit
file. §9 states the consequence and then leaves it standing:

> Note what curation does **not** reach: the shipped rules are a starter kit, written once on
> first install and never touched again (§15), so new names help new installs only. Growing
> these lists is not a fix that reaches the people already running it.

For the curated app lists that is a shame. For `shared-window-exempt` (§10.8.1) it is a
defect, because that rule exists to stop a shared window being frozen in somebody's meeting
— and shipping it to new installs only means the people it was written for, the ones already
running dozed, are precisely the ones who never get it.

`dozectl upgrade-presets` is the answer, and what it is *not* is most of the design:

* **It is not a migration and does not run on upgrade.** The installer still never touches
  an existing config. This is a command somebody types.
* **It prints before it writes.** Nothing changes without `--write`, and the reason is a
  thing the tool genuinely cannot know: a rule deleted on purpose is indistinguishable from
  one that predates the version which added it. Nothing on disk records the difference. The
  user has that information and the program does not, so the program asks.
* **It only ever adds, matched by name.** A rule the user edited is a rule they have an
  opinion about, and an "upgrade" that reverts it would be the failure §15 avoids by not
  touching the file — done under a friendlier word. There is an explicit check that every
  pre-existing rule survives byte-for-byte, so this is a property rather than an intention.
* **It appends rather than regenerating.** `dozed --annotate-config` rewrites the file from
  the parsed struct, which loses every comment the user wrote; that is acceptable for a
  command whose entire purpose is the comments and unacceptable for one that wants to add a
  rule at the end. Same instinct as §12's config editor writing the user's own bytes back
  from the text tab.
* **It never reorders or renumbers**, and reports a priority already in use rather than
  silently slotting in. A tie is legal — first-match-wins resolves it by position — but which
  of the two wins is then decided by something not visible in the file.
* **The result is parsed by `Config::parse` before it is kept**, and the original is copied
  aside first, so a file this produces cannot be one that fails at the next restart.

Each added rule arrives with the same sentence the starter file would have given it, because
a rule appearing in a config with no explanation is indistinguishable from one somebody else
put there.

### Manuals and completions are generated, not written

`dozectl completions <shell>`, `dozectl man [--to DIR]` and `dozed --man` emit their own
completion scripts and roff, from the same `clap` definition the binaries parse with. None
of it is checked in, and that is the point: a subcommand cannot exist without its completion
or its manual entry, and a flag cannot be removed while its documentation stays. The usual
alternative — a hand-written `dozectl.1` beside the parser — is a second description of the
same thing, maintained separately, which is how a manual comes to describe a flag that was
deleted two releases ago.

Two details worth keeping:

* **One page per subcommand.** `dozectl(1)` cross-references `dozectl-status(1)` and
  eighteen others, so shipping the top-level page alone leaves every one of those pointing
  at nothing. `--to` writes them all, and the installer uses it.
* **`long_about`, not just `about`.** A manual whose DESCRIPTION is the one-line summary
  already in NAME is a manual nobody reads twice.

Everything lands under `$HOME` in the XDG locations, and `--uninstall` removes each of them
— checked by `packaging/test-install.sh`, because the failure otherwise is a stale
`dozectl-snooze(1)` outliving the subcommand it documents.

### The same sandbox leak, one level down: tests writing a real daemon's state

Found while fixing the trial, by noticing that `trial.json` on disk said zero
blocks while the running daemon reported four. The daemon's own tests drive
`drive_trial` and `stop_trial`, both of which persist through `crate::paths` —
and those resolved to `$XDG_STATE_HOME` with nothing a test could do about it.
So `cargo test` on a machine with a live daemon **overwrote that daemon's
`trial.json` and `frozen.json`**.

`frozen.json` is the one that matters. §10.3 promotes it from prudent to
mandatory: a freeze outlives the process that applied it, so that file is what
stands between a crash and an application frozen for good. A test suite able to
truncate it to `[]` while apps are held is a test suite able to strand them. It
happened to be harmless here — the daemon was in dry-run, so `[]` was already
true — which is luck rather than design.

This is the same shape as the installer leak below, one level down: the sandbox
covered the paths the test knew about and not the ones underneath them. The
remedy is the same in spirit — make the isolation a property of the code rather
than of the caller remembering. `paths::state_root` consults a test-only
`OnceLock`, every state path in the daemon crate goes through it, and
`everything_lives_under_the_state_dir` compares against *that* root so a path
which fails to move with the redirect fails the build.

A `OnceLock` rather than `XDG_STATE_HOME`, for two reasons: `std::env::set_var`
is `unsafe` under edition 2024 and this workspace forbids `unsafe_code`, and an
environment variable is process-global anyway, which is the same trap again.

### The test harness reached past its own sandbox and switched the daemon off

`test-install.sh` redirects `XDG_BIN_HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` into a
throwaway tree and runs the real `install.sh` against it, which is the right shape for the
test: it exercises the shipped code path rather than a copy of it.

**`systemctl --user` is not redirected by any of that.** It talks to the running
`systemd --user` manager over D-Bus, and that manager knows nothing about the environment of
whoever invoked the client. So every `systemctl` call in `install.sh` reached straight out of
the sandbox and acted on the daemon somebody was actually running.

Measured from the journal of the machine it happened on: the test runs `install.sh` five
times and then once more with `--uninstall`, which produced five forced restarts of a **live**
daemon inside two seconds — each one thawing everything and re-reading a 5,000-entry journal
backlog — followed by `disable --now`, which stopped it and removed its enable symlink. The
unit file, the config and the state all survived, because those paths *were* redirected. Only
the switching-off escaped.

The test then printed *"all install.sh checks passed"*, and the daemon stayed off for two
days until somebody asked why it was not running.

Three things about this are worth keeping.

**It is the failure this file exists to prevent, committed by the file that prevents it.**
`test-install.sh` was written because the installer once reverted a deliberate `--live` edit,
and §15's note on that says *"a battery tool quietly switching itself off is the failure that
is hardest to notice, because everything keeps working and simply stops saving anything."*
That is exactly what happened, one level up.

**It also cost three real traces.** The five restarts each wrote a near-empty file into the
ten-deep trace ring — the churn §13 already describes, arriving from a new direction. The
conclusions-based deletion added there did its job and kept the loss to one slot.

**The guard is a question, not a flag.** `install.sh` could have grown a `--no-systemd`
option for the test to pass, and then the property would hold only for callers who remembered.
Instead every call goes through `sctl`, which asks whether the unit file being written is the
unit file the running manager would load — `UnitPath` is the manager's own search list — and
declines, once and out loud, when it is not. A harness cannot forget it, and an ordinary
install is unaffected because there the answer is yes.

`test-install.sh` now records the service's `ActiveState`, `UnitFileState` and `MainPID`
before its first run and asserts they are identical after its last. A restart shows as a new
pid and a `disable --now` as both of the others, so "nothing moved" is checkable rather than
assumed. On a machine with no `dozed.service` — a CI runner — it says it is skipping rather
than passing silently, since a check that quietly does nothing is indistinguishable from one
that succeeded, which is the shape of the bug the whole section is about.

The installer also refuses early rather than late: no `cargo` on `PATH` is reported before
the first step rather than as a `command not found` from inside a subshell three steps in,
and `--no-build` exists for installing what is already built. After a restart it checks that
the service is actually *running*, not merely that `systemctl restart` returned zero — a
cheerful summary about a daemon that failed to start is how somebody spends an afternoon
wondering why nothing is happening.

---

## 16. Build order

1. ~~**Spike the focus source**~~ — done, `helpers/kwin`. Verified against KWin 6.6.6:
   per-window visibility including occlusion, identity via `desktopFileName`,
   heartbeats both ways, and recovery when the daemon restarts. Idle cost measured
   at 1 publish and 8 heartbeats per 45s, indistinguishable from noise against
   `kwin_wayland`'s ~200 wakeups/s baseline. Two corrections came out of it: §12's
   protocol direction had to invert, and activation detection has no compositor
   answer.
2. ~~**Spike the freeze backend**~~ — done, `crates/dozed-linux`. Freeze/thaw verified
   against the real kernel with the desktop staying responsive; findings folded into
   §7 above and into `tests/live_cgroup.rs`, which reproduces each of them on demand
   (`cargo test -p dozed-linux --test live_cgroup -- --ignored`).
3. ~~**Planner + intent stream + dry-run actuator + journal**~~ — done, CLI and egui
   timeline both.
   Verified end to end on a live session: apps discovered and identified through
   the helper, grace explained in the journal, `dozectl why` answering with the
   full derivation. Four integration bugs came out of it that no unit test would
   have found.
4. ~~**Live actuator** behind `--live`, with panic thaw and thaw-on-exit~~ — done,
   including both remaining rungs of §7 and crash safety for each.
5. ~~**Wakeup/CPU discovery, UPower tiers, replay**~~ — done. Recording and replay landed
   *before* the week of dry-run rather than after it, because without a trace a week
   yields a report and nothing to diff a rule change against. A rules editor is still
   only the config file.
6. ~~**The other three desktop combinations**~~ — done, with §5's rule centralised so the
   three sources cannot disagree.
7. ~~**Packaging and the dashboard**~~ — done.

### Still outstanding

~~**A week of living with it.**~~ — done, and most of this document is what it produced. The
13-day journal behind §8's `aggression` floor and the flap term, the 5h25m locked session
that made `resume_cooldown_mode = floor` the default, the 2h53m recording §14.1's objective
was rewritten against, the 135 systemd-accounted runs behind §13's cost figures, and the
2h14m silent bus outage in §12 are all findings from real days rather than from tests. The
step that could not be shortened was not shortened, and it changed more than any other.

What it has **not** produced is a second machine. Every measurement here comes from one or
two, and the mechanisms most likely to be wrong are precisely the ones that calibrate to
hardware: `FanRange`, the power-model refusals, `NOT_THE_MACHINE`, the shared-scope checks.
Those are bets that other hardware is *differently* strange, and only strangers' machines
can settle them. That is now the step that cannot be shortened.

~~**Verifying the backends nobody had run.**~~ — done for two of the three, and it was not a
formality: the Sway source had **never worked**, for the reason §12 now records at length.
`packaging/verify-matrix.sh` covers Sway and X11; GNOME still cannot be arranged from a
script and is named as such rather than omitted.

Still outstanding, in rough order: **Hyprland and niri**, both scoped in §12 and each a
bounded addition now that Sway's shape is known and actually verified; **PipeWire output
streams** as a second audio signal, since MPRIS covers what users notice rather than
everything; and **the 0.89% regime** in §13, where systemd's accounting spans 0.03% to 0.89%
across 135 runs and this document has never had an explanation for the top of that range —
the traces contain enough to correlate it with what was happening, and nobody has.

`Nice`/`Weight` stay unimplemented, measured to be worth too little to justify a rung.

~~Replacing the 5s UPower/logind poll with `PropertiesChanged` subscriptions~~ — done, and
it turned out to be a *partial* replacement, which is the interesting part. See §13.1.

### What v1 was scoped as

Hidden-window detection, one rule (`hidden + on battery + N minutes → freeze`), dry-run
with the counterfactual report, panic thaw, thaw-on-exit. Everything else was to land after
a week of real traces to tune against, and that week is where the rest of this document
came from.
