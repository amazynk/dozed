# Configuration

Configuration lives in `~/.config/dozed/config.toml`, written on first install from the
shipped presets. It is a **starter kit**: once the file exists the installer will not touch
it again, so new presets in later versions do not appear there by themselves.

The file is also the reference. Every section and field is explained where it appears —
what it does, what it defaults to and why — along with the predicates and actions the
shipped rules happen not to use, so there is nothing to discover from a manual that could
disagree with the code. `dozed --write-default-config` writes a fresh one at any time — it
**overwrites** the file at that path, so read one into somewhere else
(`--config /tmp/reference.toml`) rather than over a config you have edited.

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

## Rules

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

## Heat and latency

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

## Checking a config

Check a config before trusting it — this also prints the effective rule table in priority
order:

```sh
dozed --check
```
