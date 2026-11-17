//! The starter `config.toml`: the shipped defaults, with every section and
//! field explained in place.
//!
//! The *values* are serialised from [`Config::default`] rather than written out
//! by hand. A hand-copied rule table is a second source of truth for fifteen
//! rules and several dozen app patterns, and it would drift from the built-ins
//! the first time one changed — silently, because a config file that parses is
//! a config file that looks fine.
//!
//! The *prose* is a lookup keyed by the TOML path it documents, applied while
//! walking the serialised output. That way a field added to [`Config`] arrives
//! in the file with no comment above it and `every_field_is_explained` fails,
//! rather than the file quietly growing an undocumented knob.
//!
//! Two shapes of documentation, because a rule table cannot use the first:
//!
//! * `[defaults]`, `[activity]`, `[battery]` and `[apps]` occur once, so every
//!   field carries its own comment.
//! * The rule schema is explained once, in a block above the table, and each
//!   rule then carries a comment saying why *that rule* exists. Repeating the
//!   meaning of `visibility` fifteen times would bury the thing worth reading.

use crate::config::Config;

/// The starter config file, ready to write.
///
/// Pure: the caller does the I/O (`dozed::config::write_default`), so this stays
/// inside the crate's no-I/O rule and can be tested as a string.
#[must_use]
pub fn starter_config() -> String {
    annotate(&Config::default())
}

/// The same explanations, over a config that already exists.
///
/// What `dozed --annotate-config` runs on a file somebody has been editing for
/// weeks, so it takes the config rather than assuming the defaults: the comments
/// are the point, and rewriting somebody's `mode = "live"` back to `"dry-run"`
/// to get them would be a spectacular way to miss it.
///
/// Values in, values out — the caller is expected to check that, and
/// `dozed::config::annotate_in_place` refuses to write if the rendered file does
/// not parse back to what it was given.
#[must_use]
pub fn annotate(config: &Config) -> String {
    let body = toml::to_string_pretty(config)
        .expect("a parsed config is a plain struct and always serialises");

    let mut out =
        String::from(if *config == Config::default() { PREAMBLE_SHIPPED } else { PREAMBLE_EDITED });
    let mut table = String::new();
    let mut in_array = false;
    let mut rules_started = false;

    let lines: Vec<&str> = body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Continuation lines of a multi-line array — the app pattern lists —
        // are values, not keys, and must pass through untouched.
        if in_array {
            out.push_str(line);
            out.push('\n');
            in_array = !trimmed.starts_with(']');
            continue;
        }

        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }

        if let Some(header) = trimmed.strip_prefix('[') {
            let header = header.trim_end_matches(']').trim_start_matches('[');
            table = header.to_owned();

            // The rule schema, once, immediately before the first rule.
            if header == "rule" && !rules_started {
                rules_started = true;
                out.push_str(RULE_REFERENCE);
            }
            match header {
                // `[[rule]]` is followed by `name = "..."`, and the name is what
                // selects the comment — so it has to be read ahead of itself.
                "rule" => {
                    if let Some(name) = lines[i..].iter().take(4).find_map(|l| {
                        l.trim().strip_prefix("name = \"")?.strip_suffix('"').map(str::to_owned)
                    }) {
                        // A rule dozed did not ship is one only its author can
                        // explain, so the comment says that rather than
                        // inventing a reason — and says *something*, because an
                        // uncommented rule in a commented file reads as an
                        // oversight.
                        push_comment(&mut out, rule_doc(&name).unwrap_or(&YOUR_OWN_RULE));
                    }
                }
                _ => {
                    if let Some(doc) = section_doc(header) {
                        push_comment(&mut out, doc);
                    }
                }
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // A key. Inside the rule table the reference block above covers it;
        // everywhere else each field gets its own comment.
        if let Some(key) = trimmed.split(" =").next()
            && !table.starts_with("rule")
            && let Some(doc) = field_doc(&table, key)
        {
            push_comment(&mut out, doc);
        }
        out.push_str(line);
        out.push('\n');
        in_array = trimmed.ends_with('[');
    }

    out.push_str(APPS_EXAMPLE);
    out
}

/// One rule as TOML, carrying the same explanation the starter file gives it.
///
/// For `dozectl upgrade-presets`, which **appends** to a config rather than
/// regenerating it. [`annotate`] renders the whole file from the parsed struct,
/// which loses every comment the user wrote themselves — acceptable for
/// `--annotate-config`, whose entire purpose is the comments, and not acceptable
/// for a command that only wants to add a rule to the end of somebody's file.
/// The same instinct as §12's config editor, which writes the user's own bytes
/// back from the text tab rather than reformatting a file they have commented.
#[must_use]
pub fn render_rule(rule: &crate::config::Rule) -> String {
    let mut out = String::new();
    push_comment(&mut out, rule_doc(&rule.name).unwrap_or(&YOUR_OWN_RULE));
    // Serialised as a one-element array so the `[[rule]]` header comes out,
    // which is what the config file's schema actually wants. Serialising the
    // rule alone produces a bare table with no header at all.
    #[derive(serde::Serialize)]
    struct Table<'a> {
        rule: [&'a crate::config::Rule; 1],
    }
    let body = toml::to_string_pretty(&Table { rule: [rule] })
        .expect("a rule is a plain struct and always serialises");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

fn push_comment(out: &mut String, doc: &[&str]) {
    for line in doc {
        if line.is_empty() {
            out.push_str("#\n");
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// Header for a file that is still exactly what dozed ships.
const PREAMBLE_SHIPPED: &str = "\
# dozed configuration.
#
# Every section and field below is explained where it appears. The values are
# the shipped defaults, so deleting this file and running
# `dozed --write-default-config` brings it back exactly as it is now.
#
# Nothing here is applied until it parses: `dozed --check` validates the file
# and lists the rules in the order they will be tried. `dozectl why <app>` then
# names the one rule that decided anything, so a rule that is not doing what you
# expected can be found by name rather than by guesswork.
#
# Two things worth knowing before editing:
#
#   * dry-run is the default, and there is no way into live mode by accident.
#     Read `dozectl report` for a few days first — it says what live mode would
#     have done, on your machine, with your apps.
#   * deleting a rule really does delete it. An absent rule table means no rules
#     at all, not the shipped ones; built-ins that quietly reappeared would be
#     the worst kind of surprise.

";

/// Header for a file somebody has edited. Same advice, minus the two sentences
/// that would be false: these are *your* values, and regenerating the defaults
/// over them would lose them.
const PREAMBLE_EDITED: &str = "\
# dozed configuration.
#
# Every section and field below is explained where it appears. The values are
# yours — this file was annotated in place by `dozed --annotate-config`, which
# rewrites the comments and changes nothing else. `dozed --write-default-config`
# is the other one: it writes the shipped defaults and would overwrite what is
# here, so point it at a scratch path if you only want to read them.
#
# Nothing here is applied until it parses: `dozed --check` validates the file
# and lists the rules in the order they will be tried. `dozectl why <app>` then
# names the one rule that decided anything, so a rule that is not doing what you
# expected can be found by name rather than by guesswork.
#
# One thing worth knowing before editing: deleting a rule really does delete it.
# An absent rule table means no rules at all, not the shipped ones; built-ins
# that quietly reappeared would be the worst kind of surprise.

";

/// Stands in for a rule dozed did not ship and cannot speak for.
const YOUR_OWN_RULE: [&str; 1] = ["Your own rule — dozed ships no explanation for this one."];

/// The whole rule schema, once, above the table.
const RULE_REFERENCE: &str = "\
# ---------------------------------------------------------------------------
# The rule table.
#
# Rules are tried in `priority` order, lowest first, and the first one that
# matches decides — so exemptions sit at the top and the catch-all at the
# bottom. Every decision maps to exactly one named rule, which is what lets
# `dozectl why <app>` answer with a rule name instead of a stack trace.
#
# A rule is four things:
#
#   name      unique, and the name `dozectl why` will print. Required.
#   priority  lower fires first. Defaults to 100 if omitted.
#   [rule.match] / [rule.when]   the conditions. Both must hold.
#   action    what to do. Required.
#
# `match` and `when` take the same predicates; the split is documentation, not
# semantics — `match` is conventionally what an app *is*, `when` what it is
# *doing*. Predicates, all optional, all ANDed together:
#
#   app = \"slack\"            one pattern, or a list: [\"slack\", \"discord\"].
#                            Matched against every identity dozed has for the
#                            app (desktop file, app_id, executable, comm), so
#                            `dozectl why` is the way to find the spelling that
#                            actually matches on your machine.
#   class = \"normal\"         also \"system\" (session infrastructure, never
#                            actionable) or \"long-running-job\" (a build, sync,
#                            download or VM, where background CPU is the point).
#   visibility = \"hidden\"    also \"focused\", or \"visible-unfocused\" for a
#                            window on screen without focus.
#   desktop_state = \"active\" input within `activity.active_threshold`. Also
#                            \"idle-present\" (here, but not typing) and \"away\"
#                            (locked, screen off, or another VT).
#   for = \"5m\"               how long everything else must have held. This is
#                            the rule's base grace, and it is fed through the
#                            factors in [activity] and [battery] rather than
#                            used raw — so the wait you get is longer than the
#                            number here while you are working, and shorter or
#                            immediate once you are away.
#   on_battery = true        on battery, or plugged in.
#   battery_below = 20.0     percent charge.
#   cpu_pct = 5              percent of ONE core, so 100 is a fully busy core
#                            and a 16-core machine can reach 1600. Shorthand
#                            for { gt = 5 }; the long form takes `gt`, `lt` or
#                            both: { gt = 5, lt = 90 }.
#   wakeups_per_sec = 50     context switches a second, dozed's proxy for how
#                            often an app wakes the CPU. Same shorthand.
#   playing_audio = true     holding a PulseAudio/PipeWire sink AND something is
#                            actually audible on it. See [audio] below.
#   audio_silent = true      holding a sink and making no sound with it: the
#                            browser with a call tab open, uncorked and writing
#                            zeros for hours. The complement of playing_audio
#                            rather than its negation — an app with no stream at
#                            all is neither — and nothing shipped matches on it,
#                            because a silent stream is not a reason to leave
#                            anything alone.
#   has_inhibitor = true     the app has taken an idle or sleep inhibitor.
#   fullscreen = true        THIS app is fullscreen.
#   other_app_fullscreen = true   some OTHER app is — a game, a video, a call.
#                            The cost there is a dropped frame rather than
#                            battery, so a rule naming this acts on mains too,
#                            whatever `defaults.on_ac` says.
#   owns_dbus_name = true    owns a well-known D-Bus name other processes call.
#                            Freezing one blocks every caller for the 25s D-Bus
#                            timeout.
#   capturing = true         holding the microphone or the camera. NOT implied
#                            by playing_audio: a dictation tool, a recorder or a
#                            muted call participant captures while making no
#                            sound at all.
#   screen_casting = true    DOING the sharing or recording, through the portal:
#                            OBS, Spectacle, a browser sharing a tab. Not the
#                            window being captured, which the portal does not
#                            name. Recorders hide their own window while they
#                            work, which is what makes this worth a rule.
#   maybe_shared = true      BEING shared, as far as can be told. The portal
#                            never says what is inside a session, so this holds
#                            every app that was on screen when one opened, until
#                            the last one closes. The only predicate here that
#                            is an inference rather than an observation, and it
#                            errs wide on purpose: the cost of being wrong is
#                            some battery during a call, and the cost of being
#                            wrong the other way is a frozen window in somebody
#                            else's meeting.
#   thermal = \"hot\"          the fan is audible or the silicon is at its limit;
#                            \"normal\" otherwise. Noise is not endurance, so a
#                            rule naming this also acts on mains.
#   focused_for = { lt = 10 }   seconds the app held focus in the visit that
#                            just ended — \"you glanced at it and left\". A rule
#                            carrying this is exempt from the resume cooldown,
#                            which exists for apps you actually came back to.
#
# Actions:
#
#   action = \"never\"              leave this app alone. A rule-level exemption.
#   action = \"freeze\"             stop it completely (SIGSTOP via the freezer).
#   action = { throttle = 5 }     cap at 5% of one core. The right first rung
#                                 for anything network-facing: it stays
#                                 connected and still notifies, it just cannot
#                                 spin. Needs the app to have its own cgroup.
#   action = { throttle-to = 50 } cap wherever it needs to be so the app costs
#                                 about 50 wakeups a second — measured, rather
#                                 than a percentage guessed in advance.
#   action = { doze-cycle = { every = \"15m\", wake = \"20s\" } }
#                                 frozen, but let up for air on a schedule
#                                 shared with every other cycling app, so N apps
#                                 make one burst of wakeups instead of N. `wake`
#                                 must be shorter than `every`, or the app never
#                                 actually dozes and dozed refuses the file.
#
# The rules below are policy, not invariants: read them, edit them, delete the
# ones you disagree with. The genuinely dangerous cases — pipewire, dbus-broker,
# the compositor — are refused in code and cannot be re-enabled from here.
# ---------------------------------------------------------------------------

";

/// The `[apps]` table ships empty, so the shape goes in as a worked example.
const APPS_EXAMPLE: &str = "\
# [apps.slack]
# mode = \"monitor\"          keep this one app in dry-run while the rest run
#                           live. The way to try a rule on something you are
#                           not ready to have frozen.
# base_grace = \"20m\"        this app's own patience, instead of
#                           `defaults.base_grace`.
# freeze_visible = false    allow freezing it while on screen but unfocused.
# reclaim_on_freeze = true  page it out when frozen: frees memory, slower thaw.
# class = \"long-running-job\"   reclassify an app the built-in heuristics got
#                           wrong. \"normal\", \"system\" or \"long-running-job\".
";

/// The explanation of one field, for anything that is not the config file.
///
/// The GUI's config editor shows exactly this text beside each control, so the
/// field is explained identically wherever somebody meets it. A second wording
/// maintained separately is a second wording that goes stale, and the one in the
/// editor would go stale invisibly — nobody diffs a tooltip.
///
/// `table` is the TOML table (`"defaults"`, `"activity"`, `"battery"`, or `""`
/// for a top-level key); `key` is the field.
#[must_use]
pub fn explain(table: &str, key: &str) -> Option<&'static [&'static str]> {
    field_doc(table, key)
}

/// The explanation of a whole section, for the same reason as [`explain`].
#[must_use]
pub fn explain_section(table: &str) -> Option<&'static [&'static str]> {
    section_doc(table)
}

fn section_doc(table: &str) -> Option<&'static [&'static str]> {
    Some(match table {
        "defaults" => &[
            "Global settings. Anything here can be overridden per app in [apps]",
            "at the bottom of the file.",
        ],
        "activity" => &[
            "The anti-thrash knob.",
            "",
            "Grace is a product: base x battery x activity x cooldown. These two",
            "factors are the activity term, and they are what stops dozed acting",
            "while you are in the middle of using the machine — during heavy",
            "alt-tabbing a freeze that gets undone twenty seconds later is a net",
            "loss, because the thaw costs a redraw and often a reconnect.",
        ],
        "battery" => &["Power-state factors."],
        "audio" => &[
            "When an open audio stream stops counting as audio.",
            "",
            "\"Has a stream\" is a weaker claim than it sounds. Measured here: a",
            "Firefox with a call tab open held a stream marked uncorked, at 100%",
            "volume, whose peak was exactly zero across four seconds — nothing",
            "was coming out and nothing had for hours, and the browser was exempt",
            "the whole time. That is the most expensive false exemption there is,",
            "because a browser is the app most worth dozing.",
            "",
            "So dozed listens to the sink for a moment and asks whether anything",
            "is actually audible. An app whose stream has been silent for",
            "silence_after is reported as `audio-silent` instead of `audio`, and",
            "audio-silent exempts nothing — see `dozectl status`.",
        ],
        "thermal" => &[
            "When this machine counts as thermally busy.",
            "",
            "Only the temperatures are set here, because only a temperature",
            "means the same thing on every machine: 80C is 80C on anybody's",
            "silicon, while 3000 RPM is idle on one fan and redline on another.",
            "So the fan thresholds are not shipped — dozed watches the speeds",
            "your fan actually uses and puts them inside that range.",
            "",
            "`dozectl status` shows what it learned and what it has seen. It",
            "also says when the state has been stuck at \"hot\" for four hours",
            "without coming near clearing, which is what every rule matching",
            "`thermal = \"hot\"` firing unconditionally looks like from outside.",
            "",
            "You can pin hot_rpm and cool_rpm if you know your hardware. Set",
            "BOTH or neither: they are one signal, and half of a pair leaves the",
            "other half moving underneath it.",
        ],
        "ui" => &["What the daemon shows for itself."],
        "apps" => &[
            "Per-app escape hatches, keyed by the same patterns rules use.",
            "",
            "Empty by default. The worked example below shows every field; each",
            "one is optional and overrides the global setting for that app alone.",
        ],
        _ => return None,
    })
}

fn field_doc(table: &str, key: &str) -> Option<&'static [&'static str]> {
    Some(match (table, key) {
        ("", "mode") => &[
            "\"dry-run\" records what would have happened and touches nothing.",
            "\"live\" actually freezes and throttles. (\"monitor\" is per-app only,",
            "in [apps] below; as a global mode it is refused.)",
            "",
            "Note that the shipped systemd unit passes --dry-run, which wins over",
            "this line — so setting \"live\" here alone changes nothing. `dozectl",
            "status` reports the mode actually in force and says when the two",
            "disagree.",
        ],

        ("defaults", "aggression") => &[
            "HOW SOON DOZED ACTS. If you change one thing, change this.",
            "",
            "  gentle       waits three times as long as balanced",
            "  balanced     the shipped behaviour",
            "  aggressive   acts in about a third of the time",
            "  maximum      acts almost as soon as a window is hidden",
            "",
            "Or a number of your own, if none of the four is the answer: the",
            "words are names for the multipliers 3, 1, 0.4 and 0.15, and you can",
            "write the multiplier instead —",
            "",
            "  aggression = 0.75      # between balanced and aggressive",
            "",
            "Anything from 0.05 to 10. It is refused outside that: this number",
            "multiplies every grace in the file, so 0 would mean acting with no",
            "grace at all and a negative one a duration that runs backwards.",
            "",
            "What that means for the `bg-idle` rule at the bottom of this file —",
            "the one that catches everything — on a full battery:",
            "",
            "               on battery      plugged in",
            "  gentle          15m             30m",
            "  balanced         5m             10m",
            "  aggressive       2m              4m",
            "  maximum         45s           1m30s",
            "",
            "Two things it does NOT do. It does not change *what* happens to an",
            "app — a rule that throttles still throttles, and nothing new becomes",
            "freezable. And it does not override the rest of this file: it is one",
            "multiplier in the grace calculation, so a rule with its own `for`,",
            "the battery level, and whether you are at the keyboard all still",
            "apply. `dozectl why <app>` prints the whole sum, this term included,",
            "so you can always see what produced the number.",
            "",
            "At \"maximum\" you will occasionally alt-tab back to an app that is",
            "still thawing, and chat apps will reconnect more often. That is the",
            "trade, stated plainly rather than discovered.",
        ],
        ("defaults", "base_grace") => &[
            "How long an app must hold its matched state before dozed acts, for",
            "rules that do not set their own `for`. Fed through the factors",
            "below rather than used raw.",
        ],
        ("defaults", "cpu_sample_window") => &[
            "The window CPU and wakeup rates are averaged over. Longer is",
            "steadier and slower to notice a change; the sampler reads the same",
            "counters either way, so the cost is the same.",
        ],
        ("defaults", "reclaim_on_freeze") => &[
            "After freezing an app, ask the kernel to page it out. Frees real",
            "memory; costs a slower thaw when you come back to it.",
        ],
        ("defaults", "freeze_visible") => &[
            "Allow freezing a window that is on screen but not focused. Off,",
            "because a frozen visible window is a stale rectangle that stops",
            "redrawing — which reads as a crash rather than as a saving.",
        ],
        ("defaults", "on_ac") => &[
            "What to do while plugged in. \"enabled\" acts, stretched by",
            "`battery.ac_factor`; \"disabled\" does nothing at all.",
            "",
            "Enabled by default, because a watt is a watt. The charger does not",
            "absorb an idle app's wakeups, it passes them to the wall — and a",
            "laptop spends most of its working day plugged in, so leaving this",
            "off meant the tool was switched off for most of the hours it was",
            "installed for, and did nothing on a desktop ever.",
            "",
            "Set it to \"disabled\" if you want dozed to be strictly about your",
            "own runtime. Rules about noise (`thermal`) or dropped frames",
            "(`other_app_fullscreen`) ignore that setting either way: those costs",
            "are paid in full while plugged in.",
        ],
        ("defaults", "resume_cooldown") => &[
            "An app you came back to within this window is clearly in your active",
            "rotation, so it gets extra patience the next time it is hidden.",
        ],
        ("defaults", "resume_cooldown_factor") => {
            &["How much extra: the multiplier applied to that app's grace."]
        }
        ("defaults", "resume_cooldown_mode") => &[
            "How the cooldown is enforced.",
            "",
            "\"floor\": `resume_cooldown` is a minimum wait after a restore —",
            "whatever the other factors say, the app is not acted on again until",
            "it has passed. \"factor\": the multiplier above and nothing else.",
            "",
            "Floor is the default because grace is a *product*, and",
            "`activity.away_factor = 0.0` makes the whole product zero: while the",
            "screen was locked the multiplier bought exactly nothing. Replaying a",
            "real locked session showed a browser restored on unlock and re-frozen",
            "81 seconds later, inside a cooldown of two minutes that was nominally",
            "in force.",
        ],

        ("activity", "active_threshold") => {
            &["Input more recent than this means the desktop is \"active\"."]
        }
        ("activity", "active_factor") => &[
            "Grace multiplier while active. Four times the patience while you are",
            "working, so nothing is frozen out from under an alt-tab.",
        ],
        ("activity", "away_factor") => &[
            "Grace multiplier while you are away: locked, screen off, or switched",
            "to another VT. Zero means act immediately, which costs nothing",
            "because nobody is switching windows — and this is where most of the",
            "saving actually comes from.",
        ],

        ("battery", "low_pct") => &[
            "No longer used, along with `low_factor` below. Grace on battery is",
            "proportional to charge, which pins the curve to empty and to full and",
            "leaves these two nothing left to describe.",
            "",
            "Still here so that existing config files parse — an upgrade that",
            "turned an obsolete key into a startup failure would be worse — and",
            "dozed warns at startup if either is set to anything but the value",
            "here, because a knob that silently does nothing is worse than one",
            "that is gone.",
        ],
        // Deliberately terse: the paragraph above covers both, and repeating it
        // verbatim two lines later is how a config file teaches people to skim
        // past its comments.
        ("battery", "low_factor") => &["No longer used. See `low_pct` above."],
        ("battery", "ac_factor") => &[
            "Grace multiplier while plugged in — the patience half of",
            "`defaults.on_ac`, and what stops \"acts on mains\" meaning \"acts on",
            "mains exactly as hard as at 40% charge\". On battery this factor is",
            "the charge itself, so a plugged-in machine waits longer than any",
            "machine running on its own power.",
            "",
            "Twice the patience, not four times: fourfold was tried first and put",
            "Signal on a 32-minute grace with nothing at all held after seven",
            "minutes, which is not \"more patient on AC\" but \"dormant on AC\".",
        ],

        ("audio", "detect_silence") => &[
            "Measure at all. False is the old behaviour exactly: an uncorked",
            "stream is audio, whatever is or is not coming out of it.",
            "",
            "Worth having as a switch because this is the one place dozed",
            "*narrows* a safety net, and narrowing one should be something you",
            "can undo without editing rules. `dozectl reload` applies it without",
            "a restart.",
        ],

        ("audio", "silence_after") => &[
            "How long a stream must be continuously silent before it stops",
            "counting as audio.",
            "",
            "Not seconds, deliberately: the gap between two tracks, a video",
            "rebuffering and a call with nobody talking are all real silence in a",
            "stream that is very much in use. A minute of it is not.",
        ],

        ("audio", "probe_every") => &[
            "How often to listen, while there is anything to listen to.",
            "",
            "Nothing happens at all when no app holds an output stream, which is",
            "most of the day — this is not a timer the daemon runs, it is a timer",
            "that runs while something is playing.",
        ],

        ("audio", "probe_window") => &[
            "How long each measurement listens for.",
            "",
            "This is the honest limit of the whole mechanism: a measurement is a",
            "sample, not a recording, so a sound falling entirely between two",
            "windows is not heard. An app that beeps once a minute and is silent",
            "in between therefore reads as silent — give it a rule of its own if",
            "that matters.",
        ],

        ("audio", "silence_dbfs") => &[
            "Peak level, in dBFS, at or below which a moment counts as silent.",
            "",
            "The default is close enough to digital silence that only a stream",
            "writing zeros gets under it. Raising it starts catching quiet music,",
            "and the cost of that mistake is an app frozen mid-track.",
        ],

        ("thermal", "hot_celsius") => &[
            "Hot at or above this temperature. 80C is a laptop CPU working",
            "hard but not in trouble.",
        ],
        ("thermal", "cool_celsius") => &[
            "Back to normal only below this one — and both signals have to fall",
            "before the state clears.",
            "",
            "A gap, not the same number twice: a sensor hovering on a single",
            "threshold would flip the state repeatedly, and every flip is a",
            "re-plan and possibly a thaw and a re-freeze.",
        ],
        ("thermal", "hot_rpm") => &[
            "Hot at or above this fan speed. LEARNED, not shipped — leave it",
            "unset unless you know your hardware.",
            "",
            "Fan speed matters and not just temperature, because the objective",
            "here is NOISE: 70C with the fan at full tilt is a machine somebody",
            "can hear, and a temperature threshold alone calls it normal.",
            "",
            "But a fan speed is not portable the way a temperature is, so there",
            "is nothing sensible to ship. dozed watches the range yours uses and",
            "places the thresholds inside it. If you do set this, set cool_rpm",
            "too — see there.",
        ],
        ("thermal", "cool_rpm") => &[
            "Back to normal only below this one. Learned alongside hot_rpm.",
            "",
            "If you pin these, pin both. They are one signal: clearing needs the",
            "fan under cool_rpm, so a hot_rpm raised past your fan's range with",
            "cool_rpm left behind stops the fan reporting hot while leaving it",
            "able to refuse every clear — worse than either alone.",
        ],

        ("ui", "tray") => &[
            "Show a tray icon: what is held, a snooze, and the panic thaw.",
            "",
            "On by default, which is a deliberate exception to how quiet the rest",
            "of this daemon is. Something that can stop your applications should",
            "be visible without being asked, and \"thaw everything\" should be one",
            "click from wherever you are rather than a command you have to",
            "remember while an app is misbehaving.",
            "",
            "It costs nothing either way — the icon redraws when what it shows",
            "changes and at no other time — so turn it off if you keep an empty",
            "tray on purpose. `dozectl reload` applies this without a restart.",
        ],

        _ => return None,
    })
}

/// Why each shipped rule exists. One or two lines: the reference block above
/// already says what the predicates mean.
fn rule_doc(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "never-system" => &[
            "Session infrastructure — the compositor, the bus, the audio server.",
            "Freezing any of these wedges the desktop, so the class is never",
            "actionable and cannot be overridden per app.",
        ],
        "audio-exempt" => &[
            "Anything holding an audio sink. Freezing a player mid-track is the",
            "most obvious way to be noticed.",
        ],
        "inhibitor-exempt" => &[
            "The app has taken an idle or sleep inhibitor, which is it saying",
            "outright that it is busy. Believe it.",
        ],
        "fullscreen-exempt" => &["A fullscreen app is in use even with no input for minutes."],
        "capture-exempt" => &[
            "Holding the microphone or the camera. The exemption with an",
            "audience: a frozen player is heard by you and can be fixed, a frozen",
            "microphone is heard by whoever is on the call and cannot.",
        ],
        "screencast-exempt" => &[
            "The app doing the recording or sharing — OBS, Spectacle, a browser",
            "sharing a tab. Most of them hide their own window while they work,",
            "which is exactly the profile this tool selects for, and freezing one",
            "stops the recording. It does not cover the window being captured;",
            "the portal names PipeWire nodes, not processes.",
        ],
        "shared-window-exempt" => &[
            "The other side of `screencast-exempt`: not the app doing the",
            "sharing but the app being shared. Nothing on the portal answers",
            "\"what is in this session\", so this uses the one fact available —",
            "what was on screen when the share began — and holds it until the",
            "last session closes. It is the only rule here resting on an",
            "inference rather than an observation, and it errs wide on purpose:",
            "being wrong costs some battery during a call, and being wrong the",
            "other way freezes a window in somebody else's meeting. Delete it if",
            "you would rather have the battery.",
        ],
        "dbus-owner-exempt" => &[
            "Owns a well-known D-Bus name, so other processes call into it.",
            "Freezing one blocks every caller for the 25-second D-Bus timeout.",
        ],
        "long-job-exempt" => &[
            "Builds, backups, downloads, VMs: high background CPU is the point of",
            "these, not a symptom.",
        ],
        "never-vm-container-sync" => &[
            "The same intent as `long-job-exempt`, by identity rather than by",
            "behaviour — a VM manager or a sync client idling in the tray is not",
            "recognisable from its CPU use, and freezing one stops a guest or",
            "leaves a half-written backup.",
        ],
        "never-password-managers" => &[
            "Freezing one hangs autofill or auto-type in the middle of a login.",
            "`dbus-owner-exempt` does not already cover these: it protects owners",
            "of *activatable* bus names, and a password manager's browser",
            "integration usually runs over a native-messaging proxy instead.",
        ],
        "never-security-prompts" => &[
            "Interactive security prompts are idle by construction: they exist to",
            "sit still until something asks. That is exactly the profile `bg-idle`",
            "selects for, and a frozen prompt is not a delayed prompt — the daemon",
            "behind it applies its default to every new connection while the UI",
            "cannot answer.",
        ],
        "throttle-remote-hosts" => &[
            "RustDesk, AnyDesk, TeamViewer and the VNC servers: always running,",
            "waiting for somebody to connect. Capped rather than frozen because a",
            "freeze costs the session — the app stops draining its socket, the",
            "receive window fills and the peer drops it — and because a frozen",
            "host is not reachable at all, which is the whole reason one is",
            "running with nobody at the keyboard.",
            "",
            "The cap is loose on purpose. dozed cannot see whether a session is",
            "live right now, so the same number has to be right for an idle tray",
            "icon and for one encoding somebody's desktop, and the person who",
            "would notice it being too tight is at the other end and cannot fix",
            "it. Half a core clears a hardware encoder; a software one at a high",
            "resolution loses frame rate, never the connection.",
        ],
        "throttle-remote-clients" => &[
            "The other half: viewers and guest consoles, launched to hold one",
            "session and closed when it ends. Same refusal to freeze, for the",
            "same reason — found with a real VNC session hidden for 1h58m sitting",
            "in the would-doze list — but a tighter cap, because this rule only",
            "reaches a session the user has switched away from, and these",
            "protocols send nothing while nothing moves.",
        ],
        "low-battery-foreground" => &[
            "The one rule that reaches the foreground. An app you are not",
            "interacting with is not doing anything for you just because it holds",
            "focus: a laptop left with a chat window in front of it at 15% charge",
            "is spending its remaining runtime on a window nobody is typing into.",
            "",
            "A cap, never a freeze — the planner refuses to freeze the foreground",
            "whatever a rule asks — and `throttle-to` because the right cap for",
            "the window in front of you is a measurement, not a guess.",
        ],
        "redoze-chat-after-a-glance" => &[
            "You alt-tab to a chat window, read one line and leave. Under the",
            "resume cooldown alone that three-second look buys the app its full",
            "grace and a two-minute floor on top.",
            "",
            "Separate from the rule below, and capped rather than frozen, because",
            "any priority high enough to beat `bg-idle` also beats `chatty-apps` —",
            "so a glance at Slack would freeze it and cost the reconnect that",
            "`chatty-apps` exists to avoid. Chat apps are the most-glanced-at apps",
            "on the machine, so this is not an edge case.",
        ],
        "chatty-apps" => &[
            "Chat apps are the headline case for a cap rather than a freeze: a",
            "capped Slack stays connected and still notifies, it just cannot spin.",
            "A frozen one drops its connection and pays a reconnect on every thaw.",
        ],
        "redoze-after-a-glance" => &[
            "The glance rule for everything that is not a chat app, where there is",
            "no connection to lose and a freeze is the right answer.",
        ],
        "slow-to-wake" => &[
            "Media players and stores, in the paused case — a playing one is",
            "already exempt via audio. Perfectly good freeze candidates, but slow",
            "to come back, so they earn a longer wait rather than an exemption.",
        ],
        "bg-idle" => &[
            "The catch-all, and where most of the saving comes from: anything",
            "hidden long enough, with no exemption above it, is frozen.",
            "",
            "Deliberately not gated on `on_battery`. Whether to act on mains is",
            "already decided by `defaults.on_ac`, and gating it here as well made",
            "two knobs that have to agree — with the failure silent in the worst",
            "direction: setting `on_ac = \"enabled\"` changed nothing, because the",
            "rule still refused.",
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The file has to survive the round trip it exists to start: comments and
    /// all, it must parse back to exactly what it claims to be.
    #[test]
    fn the_starter_file_parses_back_to_the_shipped_defaults() {
        let parsed = Config::parse(&starter_config()).expect("the starter config must parse");
        assert_eq!(parsed, Config::default());
    }

    /// Every field the file *shows* is explained where it is shown.
    ///
    /// Scoped to the tables outside the rule list, where each field occurs once
    /// and carries its own comment; the rule schema is documented in one block
    /// instead, which `every_field_is_explained` checks separately.
    #[test]
    fn every_key_outside_the_rule_table_has_a_comment_above_it() {
        let text = starter_config();
        let lines: Vec<&str> = text.lines().collect();
        let mut table = String::new();
        let mut in_array = false;

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if in_array {
                in_array = !trimmed.starts_with(']');
                continue;
            }
            if let Some(header) = trimmed.strip_prefix('[') {
                table = header.trim_end_matches(']').trim_start_matches('[').to_owned();
                continue;
            }
            in_array = trimmed.ends_with('[');
            if trimmed.is_empty() || trimmed.starts_with('#') || table.starts_with("rule") {
                continue;
            }
            let key = trimmed.split(" =").next().unwrap_or(trimmed);
            assert!(
                lines[..i]
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.trim().starts_with('#')),
                "`{key}` in [{table}] has no comment above it"
            );
        }
    }

    /// Every field that *can* appear is explained somewhere, including the ones
    /// the defaults never exercise.
    ///
    /// The key list is taken from a config with every optional field populated,
    /// so adding a predicate, an action or an app override and forgetting to
    /// document it fails here rather than shipping a knob nobody can discover.
    #[test]
    fn every_field_is_explained() {
        let text = starter_config();
        for key in every_possible_key() {
            // Either live in the file, or spelled out in a comment — the rule
            // predicates and the [apps] overrides are documented as examples,
            // and an example is a perfectly good explanation.
            let documented = text.lines().any(|l| {
                let t = l.trim().trim_start_matches('#').trim();
                t.starts_with(&format!("{key} ="))
                    || t.starts_with(&format!("{key} = "))
                    || t.starts_with(&format!("[{key}]"))
                    || t.starts_with(&format!("[[{key}]]"))
                    || t.contains(&format!("`{key}`"))
                    || t.contains(&format!("{key} = "))
            });
            assert!(documented, "`{key}` can appear in a config file and is not explained");
        }
    }

    /// Every rule the file ships says why it is there.
    #[test]
    fn every_shipped_rule_is_justified() {
        for rule in crate::config::builtin_rules() {
            assert!(
                rule_doc(&rule.name).is_some(),
                "rule `{}` ships with no explanation",
                rule.name
            );
        }
    }

    /// The app pattern lists must come through as values, not be mistaken for
    /// keys and sprayed with comments.
    #[test]
    fn multi_line_arrays_survive_intact() {
        let text = starter_config();
        assert!(text.contains("    \"signal-desktop\",\n"), "{text}");
        assert!(!text.contains("# \"signal-desktop\""), "a pattern was treated as a key:\n{text}");
    }

    /// Keys reachable from a fully-populated config: everything a user could
    /// legally write, rather than only what the defaults happen to use.
    fn every_possible_key() -> BTreeSet<String> {
        use crate::app::{AppClass, AppKey};
        use crate::config::{
            AppOverride, Defaults, Patterns, Predicates, Rule, RuleAction, Threshold,
        };
        use crate::event::{DesktopState, ThermalState, Visibility};
        use crate::time::HumanDuration;

        let everything = Predicates {
            app: Some(Patterns::One("slack".into())),
            class: Some(AppClass::Normal),
            visibility: Some(Visibility::Hidden),
            desktop_state: Some(DesktopState::Away),
            for_: Some(HumanDuration::secs(60)),
            on_battery: Some(true),
            battery_below: Some(20.0),
            cpu_pct: Some(Threshold::Bounds { gt: Some(1.0), lt: Some(90.0) }),
            wakeups_per_sec: Some(Threshold::Above(50.0)),
            playing_audio: Some(true),
            audio_silent: Some(true),
            has_inhibitor: Some(true),
            fullscreen: Some(true),
            owns_dbus_name: Some(true),
            capturing: Some(true),
            screen_casting: Some(true),
            maybe_shared: Some(true),
            thermal: Some(ThermalState::Hot),
            other_app_fullscreen: Some(true),
            focused_for: Some(Threshold::Bounds { gt: None, lt: Some(10.0) }),
        };
        let mut cfg = Config {
            rules: vec![
                Rule::new(
                    "everything",
                    1,
                    RuleAction::DozeCycle {
                        every: HumanDuration::secs(900),
                        wake: HumanDuration::secs(20),
                    },
                )
                .matching(everything.clone())
                .when(everything),
                Rule::new("capped", 2, RuleAction::Throttle(5)),
                Rule::new("budgeted", 3, RuleAction::ThrottleTo(50)),
                Rule::new("stopped", 4, RuleAction::Freeze),
                Rule::new("spared", 5, RuleAction::Never),
            ],
            ..Config::default()
        };
        cfg.apps.insert(
            AppKey::comm("slack").to_string(),
            AppOverride {
                mode: Some(crate::Mode::Monitor),
                base_grace: Some(HumanDuration::secs(1200)),
                freeze_visible: Some(true),
                reclaim_on_freeze: Some(true),
                class: Some(AppClass::LongRunningJob),
            },
        );
        // Named explicitly: `Defaults` is `#[serde(default)]` throughout, so
        // every field of it always serialises and is caught by the walk below.
        let _: Defaults = Defaults::default();

        let text = toml::to_string_pretty(&cfg).expect("serialises");
        let mut keys = BTreeSet::new();
        for line in text.lines() {
            let t = line.trim();
            if let Some(header) = t.strip_prefix('[') {
                // The last segment: `[rule.when.focused_for]` documents
                // `focused_for`, and `[apps.comm:slack]` documents nothing new.
                let header = header.trim_end_matches(']').trim_start_matches('[');
                if let Some(last) = header.split('.').next_back()
                    && !last.contains(':')
                {
                    keys.insert(last.to_owned());
                }
            } else if let Some((key, _)) = t.split_once(" = ") {
                keys.insert(key.to_owned());
            }
        }
        keys
    }
}
