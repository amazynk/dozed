<div align="center">

<a href="https://amazynk.com/dozed/"><img src=".github/assets/icon.png" alt="Dozed" width="128" height="128"></a>

<h1>Dozed</h1>

<p>
  Freezes backgrounded desktop applications so they stop waking the CPU, and thaws them the instant you switch back.
  <br>
  Free and open source, for Linux. Unprivileged, and dry-run by default.
</p>

<p>
  <a href="#why-you-might-trust-it">Why trust it</a>
  ·
  <a href="#install">Install</a>
  ·
  <a href="#using-it">Using it</a>
  ·
  <a href="DESIGN.md">Design</a>
  ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

<p>
  <a href="https://github.com/amazynk/dozed/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/amazynk/dozed/ci.yml?branch=main&style=flat-square&label=CI" alt="CI status"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-7cb8e4?style=flat-square" alt="License: MIT or Apache-2.0"></a>
  <a href="#what-you-need"><img src="https://img.shields.io/badge/platform-Linux-ffb45c?style=flat-square" alt="Platform: Linux"></a>
</p>

</div>

<p align="center">
  <a href="https://amazynk.com/dozed/#film"><img src=".github/assets/film.jpg" alt="A frame from the Dozed film: one hidden app waking the CPU 165 times a second, drawn as a wall of spikes" width="840"></a>
  <br>
  <sub>A frame from <a href="https://amazynk.com/dozed/#film">the 30-second film</a>. The 165 wakeups a second, and the 0 that follows, are measured on KDE Plasma 6.6.6 and Linux 7.1.</sub>
</p>

`dozed` is a daemon for the Linux desktop: Wayland (KDE, GNOME, Sway) and X11.

A hidden Slack that wakes the CPU 900 times a second is burning power on nothing. `dozed`
notices, waits out a grace period, and stops it — then gets out of the way before you can
tell.

**The target is wasteful software, not a short battery.** That app wastes the same power
plugged in as unplugged; the battery is only where you happen to see it. So dozed works on
a charger and on a desktop too, where it just takes its time about it — twice the patience
on mains, and on battery a grace proportional to the charge left, so a nearly-flat laptop
acts in seconds where a plugged-in one waits ten minutes. One mechanism, three machines:

| Your machine | What dozed is for |
|---|---|
| Laptop on battery | Runtime you can feel, and it gets sharper as the battery empties |
| Laptop plugged in | The waste, the fan noise, and frames stolen from what you are doing |
| Desktop, no battery | The same — and it gains most over a week, being never unplugged |

## Why you might trust it

* **It does nothing until you say so.** The default mode is `dry-run`: it records what it
  *would* have done and touches nothing. `dozectl report` then tells you what that would
  have been worth, measured from the real processes rather than modelled.
* **It can prove it, or admit it cannot.** `dozectl trial start` runs a randomised
  experiment on your own machine — alternating live and dry-run blocks in pairs, gated on an
  idle desktop, comparing the actual battery discharge — and reports a saving with a
  confidence interval, or says "no measurable difference". It is allowed to bring back bad
  news.
* **Every action maps to one named rule.** `dozectl why slack` answers, always, including
  when the answer is "nothing happened, and here is which rule was counting down".
* **It shows its own cost.** `dozectl status` reports the daemon's own wakeups per second.
* **It leaves alone anything somebody else can hear or see.** An app playing audio, holding
  the microphone or the camera, or sharing the screen is never touched — that last one
  matters most, because you share a window and then work in another, which is exactly the
  app dozed would otherwise pick. A frozen music player is a fault you notice and fix in a
  second; a frozen microphone is one only the people on the call can hear.
* **It says what it cannot see.** Every exemption is a watched signal, and `dozectl status`
  reports per source whether it is reporting, unavailable here, or running degraded because
  a utility is missing — naming the package. A safety net drawn but not connected is the
  failure this project has already had once, and it was silent.

## What you need

* Linux with a systemd user session and cgroup v2, which every current desktop distribution
  has.
* Rust 1.90 or newer, from [rustup](https://rustup.rs). There is no packaged release yet:
  0.1 installs from source, and builds in a few minutes.
* One of these sessions:

| Session | How dozed sees your windows | Extra piece |
|---|---|---|
| KDE Plasma on Wayland | a KWin script that reports to the daemon | installed for you |
| GNOME 45 to 49 on Wayland | a Shell extension that reports to the daemon | installed for you |
| Sway, i3 | the window manager's own IPC | none |
| X11, any desktop | EWMH, read in-process | none |

Hyprland and niri are not supported yet. Sway and X11 are exercised by
`packaging/verify-matrix.sh` against real compositors; GNOME cannot be arranged from a
script, so it is not in that check.

## Install

```sh
git clone https://github.com/amazynk/dozed.git
cd dozed
./packaging/install.sh          # builds, installs binaries, unit and compositor helper
systemctl --user enable --now dozed.service
```

That also installs `man dozed` and `man dozectl`, shell completions for bash, zsh and fish,
and a desktop entry for the dashboard. All of it goes under `$HOME` — nothing here wants
root — and `./packaging/install.sh --uninstall` takes every piece back out while leaving
your config and recorded sessions alone. `--no-build` installs what is already in
`target/release`; `--no-restart` leaves a running daemon on its old binary; `--no-helper`
leaves your compositor's own configuration untouched.

It starts in dry-run. Leave it for a day, read `dozectl report`, and switch over when the
number convinces you:

```sh
dozectl mode live               # or edit ExecStart in the unit, deliberately
```

## Choosing how hard it tries

One setting, four words:

```toml
[defaults]
aggression = "balanced"     # gentle | balanced | aggressive | maximum
```

What each means for the catch-all rule, which is what most apps hit:

| | on battery | plugged in |
|---|---|---|
| `gentle` | 15m | 30m |
| `balanced` | 5m | 10m |
| `aggressive` | 2m | 4m |
| `maximum` | 45s | 1m30s |

It is a multiplier on the grace calculation and nothing else — it never changes *what*
happens to an app, never overrides a rule's own timing, and shows up in the arithmetic
`dozectl why` prints. At `maximum` you will occasionally alt-tab back to an app that is
still thawing; that is the trade, and it is stated in the config file rather than left to
be discovered.

Prefer a window? `dozed-gui` → **Config** has every setting with its explanation
beside it, a live preview of what your choices mean in seconds, and a save button that
stays disabled while the config would not parse. The whole file, rules included, is
editable in the second tab under the same rule.

The window reopens the size you left it, where you left it, and at the zoom you set with
ctrl+plus and ctrl+minus — the last because somebody who needs larger text needs it every
time, not once per launch.

## Using it

| Command | |
|---|---|
| `dozectl status` | mode, what is held, the daemon's own cost |
| `dozectl watch` | decisions as they happen, this run only (`--all` for history) |
| `dozectl why <app>` | why this app is in the state it is in; `--rules` for the misses too |
| `dozectl report` | what dozing saved, or would have — modelled, and measured if you have run a trial |
| `dozectl trial` | measure whether it actually saves anything, by experiment |
| `dozectl suggest` | search your own recorded day for a better config than the one you run |
| `dozectl export` | the same numbers as Prometheus metrics, for graphing a week of them |
| `dozectl upgrade-presets` | rules a later version ships that your config predates |
| `dozectl snooze 1h` | stop acting for a while, then carry on by itself (`--off` to end early) |
| `dozectl thaw --all` | panic button: restore everything, ignore rules, ignore mode |

There is a tray icon too, showing what is held with the panic thaw and the snooze one click
away — click it with any button — and one notification a week saying what dozing has been
worth. That notification is the only one dozed sends: a tool whose premise is that you
cannot tell it is running has no business interrupting you to say so.

```toml
[ui]
tray = true      # on by default; `dozectl reload` applies a change without a restart
```

**Snooze, rather than switching it off.** For the hour you are presenting, building or on a
call, `dozectl snooze 1h` releases everything and stops acting — and then puts the mode back
on its own. `dozectl mode dry-run` is the same thing with the second half left to you, and
nobody remembers the second half.

The trial needs an evening: it only counts blocks where the desktop is idle and on
battery, and six live/dry-run pairs is the fewest that can reach significance at all
(the smallest possible p-value from *n* pairs is 2/2ⁿ). It needs a daemon started
`--live`, and it puts the mode back where it found it when it finishes.

**On a desktop, or a laptop that lives in a dock**, run `dozectl trial start --on-mains`. There is no discharge
to measure while the wall is supplying the power, so the experiment judges on the
processor's own energy counter instead — which needs the RAPL permission below, and is a
narrower claim: it is watts off the CPU, where dozing acts, rather than off the whole
machine. It refuses outright on a chip pinned to its firmware power limit, where freezing an
app hands the freed budget to whatever else is runnable instead of lowering the draw.

`dozectl upgrade-presets` is the other thing worth knowing about after an upgrade. The
config is written once, on install, and never touched again — so a rule added later, even a
safety one, reaches new installs only. That command lists the rules your file predates and
appends them with `--write`, keeping your comments and never touching a rule you have
edited. It changes nothing without being asked.

Tuning is the other side of the same coin, and needs no experiment because the day is
already recorded — but it does need a day on which nothing was actually held. A recording's
mode is fixed when the daemon starts, so that means a run started in **dry-run**, not a
call to `dozectl mode`:

```sh
dozed --dry-run                          # nothing is held, so everything stays observable
dozectl suggest                          # score a few dozen variations of your own config
dozectl suggest --write tuned.toml       # emit the recommendation
dozed --check --config tuned.toml        # it is a config file like any other
dozectl replay --config tuned.toml       # and this is the intent-by-intent diff
```

It ranks candidates on wakeups avoided *and* on the times you switched to an app that was
held and had to wait for it, and it will not recommend anything that interrupts you more
than your current config already does. If nothing beats what you run, it says so.

Run it against a session that really froze things and it declines to recommend anything,
which is the honest answer rather than a shortcoming: an app that was held reads as silent
for exactly the stretches it was held, so a candidate can only be credited for acting
sooner than that session did. That flatters aggression and scores a more patient config at
nothing at all. It checks the recorded outcomes rather than the mode in the header, so a
day that simply never gave the daemon anything to hold still tunes fine. The interruption
counts always hold — they come from the decisions and the events, not the samples — and
`--anyway` will rank on the savings regardless if you know what the number is worth.

Configuration lives in `~/.config/dozed/config.toml`, written on first install from the
shipped presets. It is a **starter kit**: once the file exists the installer will not touch
it again, so new presets in later versions do not appear there by themselves.

The file is also the reference. Every section and field is explained where it appears —
what it does, what it defaults to and why — along with the predicates and actions the
shipped rules happen not to use, so there is nothing to discover from a manual that could
disagree with the code. `dozed --write-default-config` writes a fresh one at any time — it
**overwrites** the file at that path, so read one into somewhere else
(`--config /tmp/reference.toml`) rather than over a config you have edited.

Rules are first-match-wins by explicit priority, lowest first, and deliberately not a
scripting language — that constraint is what makes `dozectl why` answerable.

```toml
[[rule]]
name     = "chatty-apps"
priority = 50
action   = { throttle = 5 }     # a capped Slack stays connected; a frozen one reconnects

[rule.match]
app = ["slack", "com.slack.Slack"]

[rule.when]
visibility = "hidden"
for        = "2m"
```

The other rung for a chat app is a **maintenance window**, borrowed from the Android mode
this is named after: freeze it properly, and let it up for air on a schedule shared with
every other app doing the same, so they reconnect in one burst instead of six.

```toml
action = { doze-cycle = { every = "15m", wake = "20s" } }   # stopped for 98% of the time
```

Which of the two is better depends on how your apps behave on reconnect. `dozectl trial`
is how to find out rather than guess.

Power is not the only cost a background app imposes, and the other two are paid on mains
too — so a rule naming either of these predicates keeps working even if you have set
`on_ac = "disabled"`:

```toml
[rule.when]
thermal = "hot"                # while the fan is audible: the quiet objective
```

```toml
[rule.when]
other_app_fullscreen = true    # while a game or a call has the screen: the latency one
```

The thermal state watches fan RPM as well as temperature, because a laptop at 70 °C with
its fan at 4500 RPM is not "normal" to anyone in the room.

Check a config before trusting it — this also prints the effective rule table in priority
order:

```sh
dozed --check
```

## CPU power, if you decide to open the counter

The dashboard header and `dozectl status` show `W discharge` while you are on battery. They
also show a `W cpu package` figure — what the processor itself is spending, from the CPU's
own energy counter (RAPL) — **on machines where that counter is readable.** It usually is
not, and dozed asks for nothing: where it cannot be read, the figure is simply absent, and
nothing else changes.

It is worth having because it is measurable **on mains**, which battery discharge is not,
and because it is the closest thing to a direct measure of what a wakeup costs.

The kernel has shipped `/sys/class/powercap/intel-rapl:*/energy_uj` as `0400 root` since
**CVE-2020-8694**: energy readings are fine-grained enough to be a side channel, and the
published attack recovered AES keys with them. That is the tradeoff, and it is yours to
make, not dozed's. Nothing in `packaging/install.sh` touches it — the installer still wants
no root at all.

If you want it, one udev rule, which survives reboots. `MODE`/`GROUP` cannot be used here —
those apply to device nodes, and `energy_uj` is a sysfs attribute — so it is a `RUN`:

```sh
sudo groupadd -f dozed-power && sudo gpasswd -a "$USER" dozed-power
sudo tee /etc/udev/rules.d/99-dozed-rapl.rules >/dev/null <<'EOF'
SUBSYSTEM=="powercap", KERNEL=="intel-rapl:*", RUN+="/bin/chgrp dozed-power /sys/%p/energy_uj", RUN+="/bin/chmod g+r /sys/%p/energy_uj"
EOF
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=powercap
```

What the counter buys, so the tradeoff above can be weighed against something concrete
rather than against "a wattage":

* **A per-app watts column**, in `dozectl apps` and the dashboard. There is no per-process
  energy counter in Linux and there cannot be one, so this is a measured total apportioned by
  a stated proxy — labelled an estimate everywhere it appears, with its denominator printed
  under it.
* **The package broken down**, into the sub-domains the kernel exposes (`core`, and
  `uncore`/`dram` where they exist), rendered *inside* the total rather than beside it, with
  whatever no sub-domain accounts for named rather than left as a gap.
* **A measured cost per wakeup.** dozed fits its own package counter against cores-busy and
  wakeups-a-second, both of which it already samples, which turns "wakeups cost battery" into
  a number in joules for *this* machine. It refuses to offer one in five stated ways rather
  than always producing coefficients.
* **A warning when the package is at a firmware power limit** — the state in which freezing
  an app frees budget for whatever else is runnable instead of lowering the draw. Without the
  counter that state is invisible, and the saving it silently cancels is the whole point of
  the tool.

That grants it to one group rather than to everyone, which keeps the side channel closed to
other accounts on the machine. If your system has no `/bin` symlink, use `/usr/bin/chgrp`
and `/usr/bin/chmod`.

**Then log out and back in, or reboot.** Not optional, and `systemctl --user restart dozed`
on its own will not do it — this is the step that looks skippable and is not.

A process's supplementary groups are set when its session is created and **cannot be added
to a running process**: the kernel has no interface for it, `setgroups(2)` needs `CAP_SETGID`
and applies only to the caller, and groups are otherwise inherited across `fork` and `exec`.
The daemon is forked by your long-lived `systemd --user` manager, which was started at login
and still holds the group list from before `dozed-power` existed, so restarting the daemon
inherits that stale list however many times you do it. `systemd-run --user` has no
`SupplementaryGroups=` for the same reason — the user manager has no privilege to grant one.
(If you have lingering enabled — `loginctl show-user "$USER" --property=Linger` — the manager
survives logout too, so reboot.)

What *can* pick up a new group is a **newly created** process, which is all `sg`, `newgrp`,
`su -l "$USER"` and `setpriv --groups` do. They are useful for checking, and no use for a
service systemd already owns.

Check the file first and the daemon second, because they fail for different reasons:

```sh
ls -l /sys/class/powercap/intel-rapl:0/energy_uj   # want: -r--r----- root dozed-power
id -nG | tr ' ' '\n' | grep dozed-power            # want: a match. no match => log out
sg dozed-power -c 'cat /sys/class/powercap/intel-rapl:0/energy_uj'   # want: a number
dozectl status | grep 'power meter'                # want: a `W cpu package` figure
```

The third line borrows the group for one command. A number there with nothing in
`dozectl status` means the rule is right and only the daemon's credentials are stale — log
out. `dozectl status` says `cpu package unreadable` whenever the counter is there and shut,
so this state is visible rather than a blank space you have to guess about; the dashboard
says the same thing on the hover of its `AC`/`battery` label. Discovery also happens once, at
startup, so a permission granted under a running daemon is not noticed until it restarts.

If logging out is not worth it to you, `sudo chmod o+r
/sys/class/powercap/intel-rapl:*/energy_uj` works the moment you restart the daemon, with no
group and no new session — at the price of opening the counter to every account on the
machine rather than one group, and it reverts on the next boot.

To undo the permanent version: delete the rule file, `sudo gpasswd -d "$USER" dozed-power`,
and reboot or re-trigger udev.

## Building

```sh
cargo test --workspace
```

If doctests fail with no explanation, check whether `/tmp` is mounted `noexec`: rustdoc
builds the doctest binary there and execs it, and reports neither the `EACCES` nor which
test failed. `TMPDIR=~/.cache/rustdoc-tmp cargo test` works around it.

## Layout

| Crate | |
|---|---|
| `dozed-core` | pure domain logic — no I/O, no syscalls, no clock |
| `dozed-linux` | everything OS-specific, behind traits from core |
| `dozed` | the daemon |
| `dozectl` | the CLI |
| `dozed-gui` | egui dashboard |

`DESIGN.md` is the long form: why each decision is what it is, and the measurements behind
it. It is worth reading before changing behaviour — most of the non-obvious choices are
there because the obvious one was tried first and recorded when it failed.

## Status

This is 0.1, the first public release. It comes out of weeks of daily use, and `DESIGN.md`
§16 records what was verified against a live session or a real kernel and what was not. Every
measurement in that document comes from one or two machines, and the parts most likely to
be wrong are the ones that calibrate to hardware. If yours behaves differently,
`packaging/collect-evidence.sh` gathers what an issue needs.

To watch it decide without waiting out the shipped grace periods, `packaging/verify-run.sh`
runs a foreground dry-run with an impatient config. It cannot freeze anything.

## Contributing

Bugs and requests go to [issues](https://github.com/amazynk/dozed/issues/new/choose). Before
sending a change, read [CONTRIBUTING.md](CONTRIBUTING.md), and `DESIGN.md` for the section
your change touches. Please report security problems privately, as described in
[SECURITY.md](SECURITY.md). For anything else, write to
[help@amazynk.com](mailto:help@amazynk.com).

## License

Dozed is licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT License](LICENSE-MIT), at your option. Unless you explicitly state otherwise, any
contribution you intentionally submit for inclusion in Dozed, as defined in the Apache-2.0
license, is dual-licensed as above, without any additional terms or conditions.

<br>

<div align="center">
<sub>Made by the team behind <a href="https://zynk.it">Zynk</a> · <a href="https://amazynk.com">amazynk</a></sub>
</div>
