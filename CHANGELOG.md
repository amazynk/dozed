# Changelog

Notable changes per release. The reasoning behind each of them is in `DESIGN.md`,
which is the long form and the place to look before changing behaviour.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project uses [semantic versioning](https://semver.org/spec/v2.0.0.html) —
with the caveat that below 1.0 the public surface is the config file, the IPC
protocol and the CLI, and those are the three things a minor bump may move.

## [Unreleased]

### Added

* **A Discord button in the dashboard.** The footer of `dozed-gui` has a button
  that opens the community's Discord in your browser. The dashboard hands the
  address to the desktop's own opener and fetches nothing itself.
* **The other Amazynk apps, from the dashboard.** A **More apps** menu beside
  the Discord button names Specular (a viewer for photos, RAW, video and
  audio) and Pagebara (a PDF viewer and editor), and opens each one's page in
  your browser, where its downloads are. Nothing is downloaded by the
  dashboard, and it does not look for what is installed.

## [0.1.0] - 2026-11-17

The first public release, and the first version that does the whole job.

### Added

* **The daemon, dry-run by default.** `dozed` watches which apps are hidden,
  waits out a grace period, and holds the ones a rule names. In `dry-run` it
  records what it would have done and touches nothing; `dozectl mode live` is
  a deliberate act. (`DESIGN.md` §3)
* **An action ladder.** Freeze through the cgroup freezer, or the process tree
  where an app has no cgroup of its own; a CPU cap (`throttle`) for apps that
  must stay connected; and maintenance windows (`doze-cycle`) that let a frozen
  app up for air on a shared schedule. (`DESIGN.md` §7)
* **A grace model with one setting.** `aggression` is `gentle`, `balanced`,
  `aggressive` or `maximum`; grace is doubled on mains and shortens with the
  charge left on battery. (`DESIGN.md` §8)
* **Rules, first match wins.** A starter config of named rules written on first
  install, with every field explained in the file. `dozed --check` prints the
  effective table, and `dozectl upgrade-presets` offers the rules a later
  version ships that an older config predates. (`DESIGN.md` §9, §15)
* **Exemptions for anything somebody else can hear or see.** Audio, the
  microphone and the camera, screen sharing and the windows being shared,
  inhibitors, long-running jobs, and remote-desktop hosts, which are capped
  rather than frozen. Each is a watched signal, and `dozectl status` says per
  source whether it is reporting. (`DESIGN.md` §10)
* **Four focus sources.** A KWin script for KDE Plasma on Wayland, a Shell
  extension for GNOME 45 to 49 on Wayland, the compositor's own IPC for Sway
  and i3, and EWMH on X11. (`DESIGN.md` §12)
* **`dozectl`.** `status`, `watch`, `why`, `report`, `apps`, `journal`,
  `snooze`, `mode`, `thaw --all`, `reload`, `export` for Prometheus, `trace` and
  `replay`. Manuals and shell completions are generated from the same
  definitions. (`DESIGN.md` §11, §14)
* **A self-trial.** `dozectl trial` alternates live and dry-run blocks on an
  idle desktop and reports a saving with a confidence interval, or "no
  measurable difference". On battery it judges on discharge; with `--on-mains`,
  where RAPL is readable, on CPU package energy. (`DESIGN.md` §11.2)
* **Rule tuning by search.** `dozectl suggest` replays a recorded dry-run day
  against variations of the running config and recommends one only if it
  interrupts no more than the current one does. (`DESIGN.md` §14.1)
* **A dashboard and a tray icon.** `dozed-gui` shows what is held and why, with
  per-app thaw, freeze and throttle, a config editor, and in-app help. The tray
  shows the state, with the panic thaw and the snooze one click away.
  (`DESIGN.md` §12)
* **Thermal and fullscreen predicates.** `thermal = "hot"` and
  `other_app_fullscreen = true` act for quiet and for latency, on mains too.
  (`DESIGN.md` §7.3)
* **An installer that wants no root.** `packaging/install.sh` puts everything
  under `$HOME`, leaves an edited unit and an existing config alone, and
  `--uninstall` takes every piece back out. (`DESIGN.md` §15)
* **Checks against real compositors.** `packaging/verify-matrix.sh` runs the
  daemon against headless Sway and nested X11 and reports which were actually
  exercised. GNOME is not among them.

`DESIGN.md` §16 records what was verified against a live session or a real
kernel, and — as importantly — what was not.

[Unreleased]: https://github.com/amazynk/dozed/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/amazynk/dozed/releases/tag/v0.1.0
