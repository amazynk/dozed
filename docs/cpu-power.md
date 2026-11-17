# CPU power, if you decide to open the counter

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
