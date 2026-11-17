# Using dozed

The commands are listed in the [README](../README.md#using-it), and `man dozectl` has every
option. This page covers the parts that need more than a line.

## The tray and the weekly notification

There is a tray icon, showing what is held with the panic thaw and the snooze one click
away — click it with any button — and one notification a week saying what dozing has been
worth. That notification is the only one dozed sends: a tool whose premise is that you
cannot tell it is running has no business interrupting you to say so.

```toml
[ui]
tray = true      # on by default; `dozectl reload` applies a change without a restart
```

## Snooze

**Snooze, rather than switching it off.** For the hour you are presenting, building or on a
call, `dozectl snooze 1h` releases everything and stops acting — and then puts the mode back
on its own. `dozectl mode dry-run` is the same thing with the second half left to you, and
nobody remembers the second half.

## Measuring what it saves

`dozectl trial start` runs a randomised experiment on your own machine: live and dry-run
blocks alternating in pairs, counted only while the desktop is idle, compared on the actual
battery discharge. It reports a saving with a confidence interval, or says "no measurable
difference".

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

## After an upgrade

`dozectl upgrade-presets` is worth knowing about after an upgrade. The
config is written once, on install, and never touched again — so a rule added later, even a
safety one, reaches new installs only. That command lists the rules your file predates and
appends them with `--write`, keeping your comments and never touching a rule you have
edited. It changes nothing without being asked.

## Tuning from a recorded day

Tuning needs no experiment, because the day is
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
