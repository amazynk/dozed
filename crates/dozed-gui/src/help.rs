//! Every explanation the dashboard shows on hover, in one place.
//!
//! The window is full of numbers that are meaningless without their unit and
//! their denominator — `4.4/s`, `hidden`, `dbus,clip`, `0.7 of 16 cores` — and
//! the person reading them is the person who has not read `DESIGN.md`. Each of
//! those has a sentence attached here saying what it is and, where it matters,
//! what it changes.
//!
//! Three rules, learned from the config editor next door:
//!
//! 1. **Nothing is invented.** A tooltip that describes behaviour the daemon
//!    does not have is worse than no tooltip, because it will be believed. Every
//!    claim below is one this crate can point at in `dozed-core` — the freeze
//!    mechanism, the grace factors, the dry-run seam.
//! 2. **Config fields keep their one wording.** Anything that names a setting
//!    the config file also explains is spelled with the same TOML path
//!    (`activity.active_factor`), so the sentence here and the comment in
//!    `config.toml` are recognisably about the same knob. The editor's own
//!    prose still comes from `dozed_core::starter` verbatim.
//! 3. **A tooltip says what changes.** "Hidden" is a state; "hidden is what the
//!    freeze rules match on" is the reason anybody is hovering it.
//!
//! Dry-run runs through the whole file. Half of what the dashboard shows —
//! held counts, the state column, the per-app buttons — is a counterfactual
//! there, and a window that renders "dozing" identically in both modes owes the
//! reader that distinction wherever it appears.

use std::time::Duration;

use dozed_core::config::Aggression;
use dozed_core::event::{DesktopState, ProcContrib, Visibility};
use dozed_core::ipc::{AppSummary, ExemptionSource, PendingChange, StatusReport, TraceStatus};
use dozed_core::{Action, Coarse, JournalEntry, Mode, Outcome, Rounded, Rule};

/// "Leave alone" for `hours`.
pub fn ease(hours: u64) -> String {
    format!(
        "Do nothing to this app for {hours} hour{}, then go back to the rules.\n\nIt is not \
         exempt — the rules still match it and `dozectl why` still explains them — they simply \
         may not act on it while this stands. Anything currently held is released now.\n\nFor a \
         permanent exemption write a `never` rule or an `[apps]` override, where it is written \
         down and can be found again. This wears off on purpose.",
        if hours == 1 { "" } else { "s" }
    )
}

/// "Cap, do not freeze".
pub const CAP_ONLY: &str = "For the next hour, the most dozed may do to this app is cap it — no \
     freezing.\n\nA cap is the gentle rung of the ladder: the app keeps running and keeps its \
     connections, it is just slower. Good for something you want kept quiet without losing what \
     it is in the middle of.\n\nThe percentage is whatever the matching rule asked for, which \
     is usually better than a guess — theirs came from a measurement. `dozectl cap --pct` picks \
     your own.";

/// Clearing a hand-set limit.
pub const UNHOLD: &str = "Drop the limit and let the rules have this app back.\n\nWhatever they \
     would have done to it, they will do on the next evaluation — which may be immediately, if \
     it has been sitting hidden the whole time.";

/// The hand-set limit column, in the summary view.
pub fn held_off(app: &AppSummary) -> String {
    match &app.held_off {
        Some(held) => format!(
            "{held}.\n\nYou asked dozed to go easy on this one. The rules still run and still \
             explain themselves — this only limits how far up the ladder they may take it, and \
             it wears off.\n\nClear it from this app's row, or with `dozectl unhold`."
        ),
        None => "No hand-set limit: this app is whatever the rules make of it.\n\nSelect the row \
                 to leave it alone for a while, or to have it capped instead of frozen."
            .to_owned(),
    }
}

/// The mode badge: what is in force, and why it is not what the file says.
pub fn mode(status: &StatusReport) -> String {
    // The badge is the mode control now, and a control whose hover only describes
    // a state leaves the reader to discover that it is one.
    let mut out = String::from("Click to change it.\n\n");
    out += &match status.mode {
        Mode::Live => "Live: apps are really being frozen and throttled.".to_owned(),
        Mode::DryRun => "Dry-run: every decision is made and recorded, nothing is touched. \
                         The report is what live mode would have saved."
            .to_owned(),
        Mode::Monitor => {
            "Monitor: decisions are recorded for this app and not acted on.".to_owned()
        }
    };
    // The discrepancy is the whole reason `configured_mode` is on the wire:
    // somebody who wrote `mode = "live"` and got dry-run is owed the reason,
    // and the header alone can only show them the answer.
    if let Some(configured) = status.configured_mode {
        // The only way the two can differ: without an actuator there is no live
        // mode to be in, whatever the file says, and the shipped unit passes
        // `--dry-run`. Same wording as `dozectl status`, which answers the same
        // question in a terminal.
        out.push_str(&format!(
            "\n\nThe config file asks for `{}`. The daemon was started without `--live`, so it \
             has no actuator at all and dry-run is the only thing it can do. Only a restart \
             with `--live` changes that — picking `live` here will be refused by this one.",
            configured.as_str()
        ));
    }
    // A trial moves the mode, and for most of its life no block is running — so
    // the window would otherwise show a mode with no reason, exactly as the
    // terminal did. `mode_driven_by_trial` covers only the twenty minutes a
    // block lasts; this covers the hours between them.
    if status.mode_driven_by_trial {
        out.push_str(
            "\n\nA trial block is running and it owns the mode: this is the arm being              measured, not a setting of yours.",
        );
    } else if let Some(restores) = status.trial_restores {
        out.push_str(&format!(
            "\n\nA trial is running. Between blocks it puts the mode back to `{}` — the mode              it found when it started — so that is what you are looking at now.              `dozectl trial stop` ends it, and note that stopping restores that same `{}`              rather than whatever you were in before.",
            restores.as_str(),
            restores.as_str()
        ));
    }
    out
}

/// The masthead, hovered.
///
/// The one place in this window allowed a joke, and it still has to obey rule 1:
/// nothing invented. "Countless watts" is funny precisely because the count is
/// the thing dozed refuses to make up — §11 exists so the saving is measured on
/// *this* machine rather than asserted in a README — so the boast and the
/// correction arrive together, and the real figure follows when there is one.
pub fn slogan(snapshot: &crate::client::Snapshot) -> String {
    let mut out = String::from(
        "dozed — saving countless watts globally.\n\n\
         Countless because nobody counted them. Every other tool in this category \
         would have picked a number by now; this one measures yours instead, which is \
         less impressive and considerably more true.",
    );
    match &snapshot.report {
        Some(r) if r.covered > Duration::ZERO => {
            out.push_str(&format!(
                "\n\nYour count, over the last {}: {} of CPU and {:.0} wakeups not spent, across \
                 {} of your session with something dozing. {}",
                // The same three figures `Report::render` prints, so the same
                // view of them: these are totals, not the countdowns the rest of
                // this file hands to `Coarse`.
                Rounded(r.window),
                Rounded(Duration::from_secs_f64(r.cpu_seconds.max(0.0))),
                r.wakeups.max(0.0),
                Rounded(r.covered),
                if r.dry_run {
                    "All of it counterfactual — dry-run measures the saving without taking it."
                } else {
                    "Actually taken."
                }
            ));
        }
        _ => out.push_str(
            "\n\nYour count so far: nothing yet. Give it a session; the number that matters is \
             the one from your own machine.",
        ),
    }
    out
}

/// The power state, and the patience it buys.
///
/// Takes the whole status rather than just [`dozed_core::event::PowerState`] so it
/// can carry the one
/// thing this window otherwise says by not saying it: that the machine has a CPU
/// power counter dozed is not allowed to read. Somebody wondering where the
/// wattage is hovers *here*, and a header item for a number that does not exist
/// would be clutter on every machine that can never have it.
pub fn power(status: &StatusReport) -> String {
    let power = &status.power;
    let mut out = if power.on_ac {
        "Plugged in. Grace is multiplied by `battery.ac_factor`, so apps are left \
         alone for longer than on battery — on mains the cost being cut is waste \
         rather than endurance. `defaults.on_ac = \"disabled\"` stops dozed acting \
         here at all."
            .to_owned()
    } else {
        let mut out = "On battery. Grace is proportional to the charge left: at half a \
                       battery an app is acted on in about half the time it would be at full."
            .to_owned();
        if let Some(pct) = power.percentage {
            out.push_str(&format!("\n\n{pct:.0}% now, as UPower reports it."));
        }
        out
    };
    if status.energy.as_ref().is_some_and(|e| e.package_denied) {
        out.push_str(
            "\n\nThis machine can report what its CPU package is drawing, and dozed is not \
             allowed to read the counter — so there is no `cpu W` beside this label. The \
             kernel keeps `energy_uj` to root because energy readings are fine-grained \
             enough to be a side channel (CVE-2020-8694). The README says how to open it to \
             one group, and what you are accepting if you do.",
        );
    }
    out
}

/// What the human at the keyboard is doing, and which factor that applies.
pub const fn desktop(state: DesktopState) -> &'static str {
    match state {
        DesktopState::Active => {
            "You have typed or moved the mouse within `activity.active_threshold`. Grace is \
             multiplied by `activity.active_factor`, because a freeze undone twenty seconds \
             later costs a redraw and often a reconnect."
        }
        DesktopState::IdlePresent => {
            "No input, but you are still here: unlocked, screen on. The ordinary state, and \
             the one the grace periods are named for."
        }
        DesktopState::Away => {
            "Locked, screen off, or another VT has the session. Grace is multiplied by \
             `activity.away_factor` — nobody is switching windows, so acting immediately \
             costs nothing."
        }
    }
}

/// The app counter, which counts would-be holds in dry-run too.
pub fn apps(status: &StatusReport) -> String {
    let mut out = format!(
        "{} apps dozed can see — one per application the compositor has given a window for, \
         not per process. {} of them are held right now: frozen, or capped by a throttle.",
        status.apps_known, status.apps_held
    );
    if !status.mode.acts() {
        out.push_str(
            "\n\nIn dry-run nothing is really held. The planner keeps the same books either \
             way, so this is the count live mode would be holding.",
        );
    }
    out
}

/// The machine's current power draw, in watts.
///
/// Shown only when the battery reports one, so this never has to explain a
/// missing number — but it does have to be clear about whose consumption it is.
/// The apps in the table below sum to a fraction of it, and somebody reading the
/// two together will otherwise conclude that dozed has lost the rest.
pub fn draw(watts: f64, status: &StatusReport) -> String {
    let mut out = format!(
        "{watts:.1} W: what the whole machine is drawing from the battery right now — every \
         app, the kernel, the screen, and dozed itself. The apps in the table account for \
         only part of it, and the difference is not something dozed has lost track of.\n\n\
         It comes from the battery's own `power_now` where the firmware reports one, and \
         otherwise from the slope of its energy counter over the last half-minute or so."
    );
    // The reading is only available at all while discharging, which makes "why is
    // it gone" a question the AC state answers — but only if the reader is told
    // that is where the answer lives.
    if status.power.on_ac {
        out.push_str(
            "\n\nYou are on mains now, so this is the last reading from before you plugged \
             in and the next sampling tick will drop it. A battery that is not discharging \
             reports 0 W rather than what the machine is really using, and there is nothing \
             honest to replace this with.",
        );
    } else if status.energy.as_ref().and_then(|e| e.package_w).is_some() {
        out.push_str(
            "\n\nOn mains it is not shown at all, for the same reason. The CPU package figure \
             beside it keeps working there, and so does deep-idle residency in \
             `dozectl status` — both are measurable whether or not the battery is paying.",
        );
    } else {
        out.push_str(
            "\n\nOn mains it is not shown at all, for the same reason. Deep-idle residency in \
             `dozectl status` is the figure that is measurable either way, and it is the one \
             wakeups actually move.",
        );
    }
    out
}

/// The CPU package wattage, where RAPL is readable.
///
/// Only shown on machines that have it, so this explains what it *is* rather than
/// how to obtain it — the README covers the permission, and a tooltip nobody with
/// the closed counter can ever see is not the place to put instructions.
pub fn package_draw(watts: f64, status: &StatusReport) -> String {
    let mut out = format!(
        "{watts:.1} W in the processor package: cores, cache and memory controller, as the \
         CPU's own energy counter (RAPL) reports it. Not the whole machine — the screen, \
         the radios and the disk are all outside it — so it is smaller than the battery \
         figure and does not disagree with it.\n\n\
         This is the number dozing moves most directly. A wakeup is a core coming out of \
         idle, and this is what that costs."
    );
    // The parts, inside the total. Written as "of which" rather than as a list
    // beside it because `intel-rapl:0:0` is `core`, a subset of its parent, and a
    // breakdown printed alongside its own total invites the addition that
    // double-counts it.
    if let Some(e) = &status.energy
        && !e.package_parts.is_empty()
    {
        let parts: Vec<String> =
            e.package_parts.parts().map(|(n, w)| format!("{n} {w:.2} W")).collect();
        out.push_str(&format!("\n\nOf which: {}.", parts.join(", ")));
        if let Some(rest) = e.package_w.and_then(|w| e.package_parts.rest_w(w)) {
            out.push_str(&format!(
                " The remaining {rest:.2} W is package the kernel exposes no sub-domain \
                 for — interconnect, memory controller, integrated graphics — and on this \
                 hardware it is usually the larger half."
            ));
        }
    }
    // The one thing about these numbers a reader could get badly wrong, said
    // where they are looking at them.
    if let Some(gpu) = status.energy.as_ref().and_then(|e| e.gpu_w) {
        let label =
            status.energy.as_ref().and_then(|e| e.gpu_label.as_deref()).unwrap_or("power1_average");
        out.push_str(&format!(
            "\n\nThis machine's GPU also reports a wattage — {gpu:.1} W, from `amdgpu`'s \
             `{label}` sensor — and it is **not** an addition to the figure above. On an \
             integrated GPU it is the same silicon reported by a second driver: measured here \
             at 24.07 W of package against 24.00 W of amdgpu PPT over the same ten seconds. On \
             a discrete card the two are genuinely separate. Nothing the kernel exposes says \
             which you have — `mem_info_vram_vendor` is absent on both and the label reads \
             `PPT` on both — so dozed reports each under its own name and never sums them.\n\n\
             It is not on the header beside the CPU figure because that row has no space left \
             to give it; `dozectl status` prints both, and the GPU's utilisation is in the \
             footer under the application list."
        ));
    }
    // The reason it is worth having at all, and it is the reason a plugged-in
    // machine has any power figure on screen.
    if status.power.on_ac {
        out.push_str(
            "\n\nIt is measurable on mains, which battery discharge is not — so on this \
             machine, right now, it is the only power reading dozed has.",
        );
    }
    out
}

// Opens with the consequence, because the badge beside it no longer does: the
// header is one row shared with the buttons, so it says "focus LOST" and leaves
// the sentence to whoever hovers it.
pub const FOCUS_LOST: &str = "Holding nothing. The compositor helper has stopped reporting, so dozed cannot tell \
     which window has focus, and it will not guess: everything it was holding has been \
     restored and nothing new will be held until the helper comes back.";

pub const CONNECTING: &str = "The socket is open but the first status reply has not arrived yet. Nothing is wrong \
     unless this stays here.";

/// The connection strip, with the socket it is or is not talking to.
pub fn connection(connected: bool) -> String {
    let socket = dozed_core::ipc::socket_path();
    if connected {
        format!(
            "Talking to the daemon over {}. Status, apps and the report are polled every \
             two seconds — and not at all while this window is minimised, because a \
             dashboard nobody is reading still costs the daemon a wakeup per request.",
            socket.display()
        )
    } else {
        format!(
            "Nothing is listening on {}. This window retries every two seconds on its own; \
             `systemctl --user start dozed.service` starts the daemon.",
            socket.display()
        )
    }
}

/// The daemon's own bill.
pub fn self_cost(status: &StatusReport) -> String {
    let mut out = format!(
        "What dozed itself wakes the CPU for: {:.1} times a second. A battery tool that \
         cannot show its own cost is asking to be taken on faith, and this number has to \
         stay far below what it saves.",
        status.self_wakeups_per_sec
    );
    if let Some(bus) = status.bus_calls_per_sec {
        out.push_str(&format!(
            "\n\n{bus:.1} D-Bus property reads a second on top, from the system-bus watcher. \
             On an idle desktop that is the larger half: the sampler backs right off when \
             nothing is pending."
        ));
    }
    out
}

/// The session recording.
pub fn trace(t: &TraceStatus) -> String {
    let mut out = format!(
        "This session is being recorded to\n{}\n\n{} events, {}. Every input the planner saw \
         and every decision it made, so `dozectl replay` can re-run the session and `dozectl \
         suggest` can search for better settings against what actually happened. `dozed \
         --no-trace` turns it off.",
        t.path,
        t.records,
        bytes(t.bytes)
    );
    match (&t.stopped, t.retrying) {
        (Some(why), true) => out.push_str(&format!(
            "\n\nRecording is stopped right now: {why}. It is retried every minute for as long \
             as the daemon runs, and resumes on its own the moment a write succeeds — a full \
             disk that gets emptied does not cost the rest of the session."
        )),
        // Reached when the daemon says it is not retrying. This used to claim
        // the stop was the size cap and therefore harmless — "a limit dozed sets
        // on itself rather than something that went wrong, so it clears on the
        // next restart". Both halves stopped being true when the cap learned to
        // rotate: what ends a run now is the filesystem refusing a file, and a
        // full disk is neither harmless nor emptied by restarting a daemon.
        (Some(why), false) => out.push_str(&format!(
            "\n\nRecording stopped for this run: {why}. The daemon is not trying again on \
             its own. Restarting it starts a new recording — but only once there is \
             somewhere to write one."
        )),
        (None, _) => {}
    }
    if t.gaps > 0 {
        out.push_str(&format!(
            "\n\nThis recording has {} gap{}: stretches where the daemon could not write and \
             later recovered. Each one is marked in the file itself, so `replay` and `suggest` \
             know the session is incomplete rather than reading across the hole in silence.",
            t.gaps,
            if t.gaps == 1 { "" } else { "s" }
        ));
    }
    out
}

/// The timeline panel.
pub fn timeline(kept: usize) -> String {
    format!(
        "Every decision as it is made, oldest first — including the ones that changed \
         nothing, because \"why did my rule not fire\" is the question this answers. \
         The last {kept} are kept here; `dozectl journal` is the archive."
    )
}

/// One timeline line, unpacked: the rule, the derivation, and whether it landed.
pub fn entry(e: &JournalEntry) -> String {
    // The timestamp lives here rather than in the row. In a panel a few hundred
    // pixels wide the leading offset cost nine or ten characters of every line
    // to answer a question asked of about one line in fifty, and squeezed out
    // the app name and the reason — which is what the row is actually for.
    let mut out = format!("{} {} {}\n{}\n\n", e.at, e.app.label(), e.verb(), e.app);
    match &e.reason.rule {
        // The kind is `rule` whenever there is one, so naming both would print
        // "rule `bg-idle` (rule)".
        Some(rule) => out.push_str(&format!("rule `{rule}`\n")),
        None => out.push_str(&format!("{}\n", e.reason.kind.as_str())),
    }
    out.push_str(&e.reason.detail);
    if let Some(grace) = &e.reason.grace {
        // Factorised rather than collapsed, which is the point of keeping it:
        // a surprising wait can be attributed to one term. Only when the line
        // above has not already printed it, which for a rule that fired on time
        // it has.
        let derivation = grace.to_string();
        if !e.reason.detail.contains(derivation.as_str()) {
            out.push_str(&format!("\ngrace {derivation}"));
        }
    }
    out.push_str(
        match &e.outcome {
            Outcome::WouldHave => "\n\nrecorded only — dry-run, so nothing was touched".to_owned(),
            Outcome::Applied => "\n\ncarried out".to_owned(),
            Outcome::Skipped { why } => format!("\n\nskipped: {why}"),
            Outcome::Failed { error } => format!("\n\nFAILED: {error}"),
        }
        .as_str(),
    );
    out
}

pub const REPORT: &str = "What the session has been worth. The CPU-seconds and wakeups are counted from the \
     episodes themselves; the charge estimate is a model, not a measurement — a busy core \
     costs roughly 2-4 W, which is why it is a range and never a single confident number. \
     It is left out entirely while you are on mains, where a fraction of a charge is not a \
     thing being spent.\n\nHold time is app-time: six apps dozing for an hour is six hours. \
     The elapsed figure beside it is the union of those episodes.\n\nA row marked NOT HELD \
     is dozed admitting a failure of its own: that app went on burning CPU or waking the \
     processor while it was supposed to be frozen, so the hold did not take and the savings \
     on that row are overstated by whatever it kept doing. It is usually not something you \
     can configure away — the likeliest cause is that dozed could not see the whole app, \
     which is a bug worth reporting rather than a setting worth changing. The rate beneath \
     tells a transient from a real escape: a few wakeups a second is an app settling after \
     the freeze landed, a few hundred is a process that got out of it.";

pub const APPLICATIONS: &str = "Every application dozed can see, with what it is doing right now and what is being \
     done to it. Click a row for its controls and the processes behind the numbers.";

/// The state column for one app, which answers a different question when that
/// app can never be acted on at all.
///
/// The dry-run caveat below is not appended in that case, and deliberately: "not
/// really, nothing is being touched" implies live mode *would* touch it, and for
/// session infrastructure that is false in every mode. A protected app is the
/// one row where dry-run and live are genuinely identical.
pub fn state_of_app(app: &AppSummary, acting: bool) -> String {
    if app.never_actionable {
        return "Protected: dozed will never freeze or throttle this, whatever the rules say. \
                It is session infrastructure — a compositor, the bus, the sound server, a \
                portal — or one of dozed's own processes. The planner refuses it before it \
                reads the rule table, and the actuator refuses it again by reading the \
                cgroup's process list, so a rule written to act on it cannot.\n\nIt is listed \
                because it is running and you may want its numbers. Nothing here is waiting \
                to happen."
            .to_owned();
    }
    state(app.applied, acting)
}

/// The state column. `acting` is false when nothing here is really being done —
/// dry-run globally, or `monitor` for this one app.
pub fn state(applied: Action, acting: bool) -> String {
    let what = match applied {
        Action::Restore => {
            "Running: dozed is holding nothing here. Either no rule matches it, its grace \
             has not run out, or something exempts it."
                .to_owned()
        }
        Action::Throttle { pct } => format!(
            "Throttled: capped at {pct}% of one core through the cgroup's `cpu.max`. It \
             keeps running and keeps its connections — the gentle rung of the ladder, for \
             apps that are meant to stay alive."
        ),
        Action::Freeze { reclaim } => format!(
            "Dozing: stopped outright through the cgroup freezer, or `SIGSTOP` across the \
             process tree where there is no scope to use. It runs nothing until it is \
             thawed, which happens the moment you focus it.{}",
            if reclaim { " Its pages have been reclaimed back to the system too." } else { "" }
        ),
    };
    if acting || applied.is_restore() {
        what
    } else {
        format!("{what}\n\nNot really: this is what would be in force. Nothing is being touched.")
    }
}

/// The app name cell.
pub fn app_row(app: &AppSummary) -> String {
    format!(
        "{}\n\nThe identity rules match on, worked out from the window and the process \
         rather than trusted from one of them. Click for controls and the busiest \
         processes inside it.",
        app.key
    )
}

pub const fn visibility(v: Visibility) -> &'static str {
    match v {
        Visibility::Focused => {
            "Has input focus. Nothing that matches `visibility = \"hidden\"` can fire on it, \
             and anything held is thawed the moment it gets here."
        }
        Visibility::VisibleUnfocused => {
            "On screen, but not the window you are in. Left alone by default — \
             `defaults.freeze_visible` is what allows acting on a visible window."
        }
        Visibility::Hidden => {
            "On another workspace, minimised, or completely covered. This is the state the \
             freeze rules wait on, and the grace clock counts from the moment it starts."
        }
    }
}

/// The measurement column: two numbers whose window is not obvious.
pub fn measurement(
    app: &AppSummary,
    window: Duration,
    basis: Option<dozed_core::attribution::Basis>,
) -> String {
    if !app.sampled {
        return "Not measured yet. A rate needs two readings, and with nothing held the \
                sampler ticks once a minute — so a young daemon shows a dash here rather \
                than a zero it cannot tell apart from an idle app."
            .to_owned();
    }
    let mut out = format!(
        "{:.0} wakeups a second and {:.1}% of one core, summed over all {} process(es) in \
         this app and averaged over `defaults.cpu_sample_window` ({}). Percent of *one* \
         core, so an app using two of them reads 200%.",
        app.sample.wakeups_per_sec,
        app.sample.cpu_pct,
        app.pids,
        Coarse(window)
    );
    // The watts, and immediately what they are not. Linux has no per-process
    // energy counter and cannot have one — the hardware counts joules per power
    // domain, and a domain is silicon everything shares — so this is a measured
    // total split by a stated proxy, and the split is the modelled half.
    if let Some(b) = basis {
        out.push_str(&match b.watts(app.sample.cpu_pct, app.sample.wakeups_per_sec) {
            Some(w) => format!("\n\nAn estimated {w:.2} W. {}\n\n{}", b.summary(), b.caveat()),
            // Not a failure and not zero watts: the rows average over a minute
            // while `/proc/stat` covers the last tick, so a burst can be in one
            // and out of the other. Clamping would hand this app the whole
            // measured package and render the guess as a fact.
            None => String::from(
                "\n\nNo watts estimated this tick: this app's averaged CPU is more than \
                 `/proc/stat` saw the whole machine use in the last one, so there is no \
                 honest share to take. It resolves itself on the next reading.",
            ),
        });
    }
    out
}

/// The exemption column, on a row that has one and on a row that does not.
///
/// Not everything reported here exempts anything: `audio-silent` is an
/// observation about why `audio` is *absent*, and listing it under "exempt right
/// now" would say the exact opposite of what it means. So the two are separated
/// before a word is written about either.
pub fn exempt(app: &AppSummary) -> String {
    let (holding, observed): (Vec<&String>, Vec<&String>) =
        app.exempt.iter().partition(|n| exempting(n));

    let mut out = if holding.is_empty() {
        String::from("Nothing exempts this app right now, so the rules decide on their own.")
    } else {
        // Exemptions are not a setting but a position in the rule table: the
        // shipped `audio-exempt`, `dbus-owner-exempt` and friends sit at
        // priorities 10-20 with `action = "never"`, so they answer before any
        // freeze rule is reached.
        let mut out = String::from(
            "Exempt right now. The exemption rules at the top of the table match on this and say \
             `action = \"never\"`, so nothing below them gets a chance to act while it holds:\n",
        );
        for name in holding {
            out.push_str(&format!("\n{name} — {}", exemption(name)));
        }
        out
    };
    if !observed.is_empty() {
        out.push_str("\n\nAlso observed, and exempting nothing:\n");
        for name in observed {
            out.push_str(&format!("\n{name} — {}", exemption(name)));
        }
    }
    out.push_str(&audio_streams(app));
    out
}

/// The streams an app is holding open, named.
///
/// The question `audio-silent` provokes and the exemption name cannot answer:
/// "firefox is holding a silent audio stream" is a fact about a process, and
/// what a reader wants is *which tab*. `media.name` is a browser's tab title,
/// and for everything else it is whatever the app chose to call the stream — so
/// the wording promises a stream name and lets a tab title be the pleasant case
/// rather than claiming every app has tabs.
///
/// Silent by design when there are none: an app with no stream has nothing to
/// say here, and a heading over an empty list is worse than no heading.
fn audio_streams(app: &AppSummary) -> String {
    if app.audio_streams.is_empty() {
        return String::new();
    }
    let silent = app.exempt.iter().any(|e| e == "audio-silent");
    let mut out =
        format!("\n\nStreams it has open{}:\n", if silent { ", none of them audible" } else { "" });
    for stream in &app.audio_streams {
        // The measurement per stream, and not only once it has won. Somebody
        // looking at an `audio` badge on an app they cannot hear is asking
        // whether dozed disagrees with them or is simply not finished counting,
        // and those two want very different reactions.
        let heard = match stream.silent_for {
            Some(d) => format!("silent for {}", Coarse(d)),
            None => "nothing silent measured on it yet".to_owned(),
        };
        out.push_str(&format!("\n· {} — {heard}", stream.name));
    }
    if silent {
        out.push_str(
            "\n\nClosing whatever is holding these — the tab, usually — is what stops the app \
             opening an audio stream it never uses.",
        );
    } else if app.audio_streams.iter().any(|s| s.silent_for.is_some()) {
        // The state that reads as a contradiction and is not one.
        out.push_str(
            "\n\nStill counted as audio: silence has to last `audio.silence_after` before it \
             stops counting, and every stream the app holds has to be silent for it. The \
             figures above are where that has got to.",
        );
    }
    out
}

/// Whether a reported name is a reason to leave an app alone.
///
/// Keyed by the wire name rather than by the enum, because this window is meant
/// to keep working against a daemon newer than itself — and an unknown name is
/// assumed to exempt, which is the reading that cannot mislead somebody into
/// thinking dozed is free to act.
pub fn exempting(name: &str) -> bool {
    dozed_core::Exemption::ALL
        .iter()
        .find(|e| e.as_str() == name)
        .is_none_or(|e| dozed_core::Exemption::exempting(*e))
}

/// What one exemption source watches for. Keyed by
/// [`Exemption::as_str`](dozed_core::event::Exemption::as_str), which is what
/// travels over the wire.
pub fn exemption(name: &str) -> &'static str {
    match name {
        "audio" => {
            "producing audio you could hear: MPRIS says playing, or PipeWire has an active \
                     output stream, and the sink it feeds is not silent"
        }
        "audio-silent" => {
            "holding an audio stream and making no sound with it. Not an exemption — the \
                            opposite: a browser with a call tab open keeps a stream uncorked at \
                            full volume for hours while writing nothing but zeros, and this is \
                            what stops that counting as audio"
        }
        "inhibitor" => {
            "holds an idle inhibitor, which is the app asking in so many words to \
                        stay awake"
        }
        "fullscreen" => "a fullscreen window, which is usually a game or a video",
        "dbus-name-owner" => {
            "owns a well-known D-Bus name; freezing it makes every caller \
                              block for the 25-second D-Bus timeout"
        }
        "clipboard-owner" => "owns the clipboard selection, so freezing it hangs the next paste",
        "long-running-job" => {
            "classified as a build, backup, sync or VM, where background CPU \
                               is the entire point of the app"
        }
        "capture" => {
            "holding the microphone or the camera. Not the same as producing \
                     audio: a dictation tool, a recorder, or somebody on a call whose side is \
                     silent captures without playing anything"
        }
        "screen-cast" => {
            "sharing or recording the screen through the desktop portal. The \
                         shared window is usually the one you are not working in, which is \
                         exactly what dozed would otherwise pick"
        }
        "maybe-shared" => {
            "was on screen when a screen share began, so it might be inside it. The \
                          portal says who is sharing but never what is being shared, so this \
                          is a guess from the one fact available — and it errs wide, holding \
                          every app that was visible at that moment until the last session \
                          closes. The `shared-window-exempt` rule is where to turn it off"
        }
        // A source this build does not know about is one only its author can
        // explain, and inventing a description for it would be the one kind of
        // tooltip worse than none.
        _ => "an exemption this window has no description for",
    }
}

/// The countdown column: what is coming, and why it is not a promise.
pub fn pending(next: Option<&PendingChange>, acting: bool) -> String {
    let Some(next) = next else {
        return "Nothing is due to happen to this app on its own. Either no rule is counting \
                it down, or it is already in the state its rule wants — the state column says \
                which."
            .to_owned();
    };
    let mut out = format!(
        "Rule `{}` is counting this app down, and will {} when it runs out.",
        next.rule,
        next.action.describe()
    );
    // The honest half. Grace is a product of the power state, what you are
    // doing and the resume cooldown, so this number is not a schedule — it is
    // where the deadline sits given how things are right now, and it moves
    // whenever they change.
    out.push_str(
        "\n\nIf nothing else changes. Touch the keyboard and the activity factor pushes this \
         out; unplug, or let the battery run down, and it comes in. It is the deadline the \
         daemon itself is waiting on, not a second guess at it — but it is a deadline for \
         these conditions, not an appointment.",
    );
    if !acting {
        out.push_str(
            "\n\nAnd in dry-run it is what would happen: the timer runs and the journal \
             records the decision, but nothing is touched.",
        );
    }
    out
}

/// The per-app buttons. `acting` as in [`state`].
pub fn thaw(acting: bool) -> String {
    dry_run_caveat(
        "Restore it now: unfreeze, clear any cap. The grace clock restarts from this moment, \
         so a rule can act on it again later — and `defaults.resume_cooldown` makes that wait \
         longer than usual, because an app you came back to is one you are using.",
        acting,
    )
}

pub fn freeze(acting: bool) -> String {
    dry_run_caveat(
        "Freeze it now, without waiting for a rule. A one-off, not a pin: the planner \
         re-decides from here as normal.",
        acting,
    )
}

pub fn throttle(pct: u8, acting: bool) -> String {
    dry_run_caveat(
        &format!(
            "Cap it at {pct}% of one core now, without waiting for a rule. It keeps running \
             and keeps its connections."
        ),
        acting,
    )
}

fn dry_run_caveat(text: &str, acting: bool) -> String {
    if acting {
        return text.to_owned();
    }
    format!(
        "{text}\n\nDry-run: this is recorded in the timeline as what would have happened, \
         and nothing is touched."
    )
}

pub fn pids(n: usize) -> String {
    format!(
        "{n} process(es) in this app's tree. They are frozen and thawed together — a freeze \
         that reaches some of them and not others is the failure the report's leak check \
         exists to catch."
    )
}

pub const fn app_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Live => "This app is acted on for real.",
        Mode::DryRun => "This app's decisions are recorded, not carried out.",
        Mode::Monitor => {
            "`monitor` in `[apps]`: this one app is watched and never touched, while the \
             rest of the machine runs live. The way to exempt something and still see what \
             would have happened to it."
        }
    }
}

/// A row in the per-app process list.
pub fn process(proc: &ProcContrib) -> String {
    format!(
        "pid {}: {:.0} wakeups a second of this app's total, over the same window as the row \
         above. An app is a tree, and its total says nothing about which part of the tree is \
         busy — `comm` repeats across an Electron app's helpers, so the role in brackets is \
         what tells them apart.",
        proc.pid, proc.wakeups_per_sec
    )
}

/// The coverage footer under the app table. `note` is
/// [`Coverage::note`](dozed_core::coverage::Coverage::note), which explains the
/// gap between the two figures when there is a gap on show.
pub fn coverage(note: Option<String>, basis: Option<dozed_core::attribution::Basis>) -> String {
    let mut out = String::from(
        "The rows above, summed, against what the whole machine is doing — context switches \
         a second from `/proc/stat`. A scale, not a total: 900/s means one thing on a \
         machine doing 1,200 and another on one doing 40,000.",
    );
    if let Some(note) = note {
        out.push_str(&format!("\n\n{note}."));
    }
    if let Some(b) = basis {
        out.push_str(&format!("\n\n{}.\n\n{}", b.summary(), b.caveat()));
    }
    out
}

pub const SNOOZE: &str = "Stop acting for as long as you pick, then carry on exactly as before. Everything held \
     is released first, and the mode goes back on its own when the timer runs out.\n\nThis \
     is the control for the meeting, the presentation or the build — the times you want \
     dozed out of the way but would rather not switch it off, because switching it off is \
     permanent and nobody remembers to switch it back on.";

/// Ending a snooze early, from inside the snooze menu.
///
/// Its own text rather than a share of [`snoozing`], because that one is the
/// hover on the whole control and has to explain the *state*; this is one entry
/// inside it and only has to say what clicking does.
pub const WAKE_NOW: &str = "End the snooze now and go back to the mode you were in before it — not to whatever \
     the config says, so a config reload while snoozed cannot change what you come back to.";

/// The countdown on the wake button, and why it is not the same as dry-run.
pub fn snoozing(left: std::time::Duration) -> String {
    format!(
        "Snoozed: nothing will be frozen or throttled for another {}, and nothing is being \
         held now.\n\nWhen the timer runs out the mode goes back to what it was before the \
         snooze — not to whatever the config says, so a config reload in the meantime cannot \
         change what you come back to. Click to end it early, or to pick a different length: \
         a new length restarts the clock and keeps the mode you were going to come back to.",
        Coarse(left)
    )
}

/// The fitted power model, hovered — including every way it declines to answer.
///
/// A model that always produces coefficients is exactly as unfalsifiable as the
/// constant it replaced (`DESIGN.md` §11.5), so the refusals are the load-bearing
/// half and each gets its own sentence rather than a shared "unavailable".
pub fn power_model(verdict: &dozed_core::power_model::Verdict) -> String {
    use dozed_core::power_model::Verdict;
    let mut out = String::from(
        "dozed fits its own package counter against two things it already measures — cores \
         busy and wakeups a second — so the cost of a wakeup on this machine is derived \
         rather than assumed. That is what the estimated watts beside each app are built \
         from.\n\n",
    );
    out.push_str(&match verdict {
        Verdict::Warming { .. } => String::from(
            "Not enough readings yet. It needs a few minutes of the machine doing different \
             things; until then the watts column falls back to apportioning by CPU time.",
        ),
        Verdict::Unvarying { .. } => String::from(
            "This machine has held one load throughout, which gives the fit nothing to work \
             with: with no variation there is nothing to explain, and any coefficients would \
             be noise wearing a respectable-looking R².",
        ),
        Verdict::PowerLimited { watts, .. } => format!(
            "**The package is pinned at {watts:.1} W by firmware.** Load moved and the watts \
             did not, which means the chip is holding its power budget and varying its clocks \
             underneath.\n\nThis changes what the watts column means. At the limit, freezing \
             an app does not lower the package draw — it hands that budget to whatever else is \
             runnable. The saving only appears once the machine can drop below the cap. The \
             column is still a share of something measured; it is just a share of a fixed \
             allowance rather than energy any app caused."
        ),
        Verdict::Collinear { .. } => String::from(
            "Cores busy and wakeups have risen and fallen together on this machine, and where \
             they move as one no arithmetic can say which of them the watts belong to. The \
             coefficients would still come out — large, opposite in sign, and different every \
             tick.",
        ),
        Verdict::Poor { .. } => String::from(
            "The fit explains too little of what the watts did to be worth attributing by. \
             Something this model cannot see is moving them around.",
        ),
        Verdict::Unphysical { watts_per_core, .. } => format!(
            "The fit is a good one and it is not physics: it has a busy core costing \
             {watts_per_core:.2} W. A core cannot cost negative watts and a wakeup cannot \
             refund energy, so something outside the model is driving the package — most \
             often clocks being scaled to hold a power budget, where putting more cores to \
             work genuinely does lower the power each one gets.\n\nRefused rather than \
             clamped: a clamped coefficient would go on to be multiplied by a real rate and \
             rendered as a measurement."
        ),
        Verdict::Fitted(f) => format!(
            "Fitted over {} readings, explaining {:.0}% of the variation. A busy core costs \
             {:.2} W here and a wakeup costs {:.1} µJ — that second figure is the one dozed \
             has never been able to state before, and it is why the watts column no longer \
             understates an app that wakes the CPU constantly while using almost none of \
             it.\n\nThe {:.2} W floor is what the package draws with nothing running. It \
             belongs to no app, so it is not charged to any.",
            f.samples,
            f.r2 * 100.0,
            f.watts_per_core,
            f.joules_per_wakeup * 1e6,
            f.floor_w
        ),
    });
    out
}

pub const EXEMPTION_SOURCES: &str = "What can stop a rule acting, and whether each source is actually reporting. A rule \
     matching on state nobody ever sets can never fire, and that failure is silent unless \
     it is on screen.";

/// One exemption source: what it watches, and whether it can work here.
pub fn exemption_source(e: &ExemptionSource) -> String {
    let what = exemption(&e.name);
    match (&e.unavailable, &e.degraded, e.reported) {
        (Some(why), _, _) => format!(
            "{}: {what}.\n\nUnavailable here — {why}. Any rule that matches on it will never \
             fire on this machine.",
            e.name
        ),
        // Fixable, unlike the case above, so the wording points at the fix. This
        // is the difference between a user installing a package and a user never
        // learning that half an exemption was switched off.
        (None, Some(why), _) => format!(
            "{}: {what}.\n\nDegraded — {why}. It still reports what it can, and {} process(es) \
             hold it right now, but part of what it watches is invisible from here.",
            e.name, e.pids
        ),
        (None, None, false) => format!(
            "{}: {what}.\n\nNo snapshot has ever arrived from this source, so as far as the \
             planner is concerned nothing holds it and no rule matching on it can fire. \
             Either nothing has claimed it yet, or the watcher is not running.",
            e.name
        ),
        (None, None, true) => format!(
            "{}: {what}.\n\nReporting normally; {} process(es) hold it right now.",
            e.name, e.pids
        ),
    }
}

pub const FOCUS_SOURCE: &str = "How dozed learns which window has focus, chosen at startup from the session type and \
     overridable with `dozed --focus-source`. The most expensive thing to get wrong here: \
     the X11 source under Wayland sees only XWayland clients, and \"no windows\" reads as \
     \"everything is hidden\".";

pub const UPTIME: &str = "How long this daemon has been running. The report above covers this run, and every \
     rate on screen is measured within it.";

pub const THAW_ALL: &str = "Restore every app now. Ignores rules, ignores mode, and works even when the rest of \
     the window has given up — the one button that has to work when something has gone \
     wrong.";

pub const RELOAD: &str = "Re-read `config.toml` from disk and apply it, without restarting the daemon or \
     dropping what it is holding. The same thing `dozectl reload` does.";

pub const CONFIGURE: &str = "Edit the config, with every setting explained beside it. Opens the file the daemon \
     actually loaded, which is not always the default one.";

pub const GO_LIVE: &str = "Start acting for real, from the moment you pick it. Apps will actually be frozen and \
     throttled: everything the report said would have happened starts happening. Picking \
     dry-run again undoes it and restores anything held, so this is not a door that locks \
     behind you.";

pub const CUSTOM_FACTOR: &str = "The number every grace in the file is multiplied by. Below 1 acts sooner than the \
     shipped behaviour, above 1 waits longer — the four words are just names for 3, 1, 0.4 and \
     0.15. The box above shows what your number does to the catch-all rule; it is written to \
     config.toml as `aggression = 0.75`.";

pub const TRIAL_OWNS_MODE: &str = "A trial is running, and a trial is a schedule of mode changes — it switches \
     between live and dry-run blocks and compares the battery. While it does, it owns the mode, so \
     picking one by hand is refused rather than quietly reverted at the next sample. `dozectl trial stop` \
     ends it and keeps the blocks it has already finished.";

pub const BACK_TO_DRY_RUN: &str = "Keep deciding and recording, but touch nothing. Everything held right now is \
     restored on the way out — leaving apps frozen under a mode that says \"stop touching my \
     apps\" would be the opposite of what it says. The report goes on being written, and it \
     becomes the counterfactual: what live mode would have saved.";

// ---------------------------------------------------------------------------
// The configuration window.
//
// Field prose there comes from `dozed_core::starter` and is not repeated here.
// What is here is the window's own furniture: the tabs, the preview, the things
// the file has no comment for because they do not exist in the file.
// ---------------------------------------------------------------------------

/// One aggression level, in the terms somebody picks a level by. The multiplier
/// comes from [`Aggression::factor`] rather than being written out, so the words
/// and the arithmetic cannot disagree.
pub fn aggression(level: Aggression) -> String {
    let what = match level {
        Aggression::Gentle => {
            "For a machine where being surprised once is worse than the power saved."
        }
        Aggression::Balanced => {
            "The shipped behaviour, and what the other three are named against."
        }
        Aggression::Aggressive => {
            "A reasonable place to end up after reading a week of reports and deciding it is \
             being too polite."
        }
        Aggression::Maximum => {
            "Acts almost as soon as a window is hidden. Honest about the trade: you will \
             sometimes alt-tab back to an app that is thawing, and chat apps will reconnect \
             more often."
        }
        Aggression::Custom(_) => {
            "Your own multiplier, for when none of the four words is the answer. It is the same \
             term of the same product they set — the only difference is that you picked the \
             number."
        }
    };
    format!(
        "Grace x {factor}. {what}\n\nOne term of the grace product and nothing else — it does not \
         quietly change four other settings, and `dozectl why` prints the factor in the \
         derivation.",
        factor = trim(level.factor())
    )
}

pub const TAB_SETTINGS: &str = "The scalar settings, as controls that cannot be typed wrong. Rules are shown here \
     read-only and edited in the other tab.";

pub const TAB_FILE: &str = "The whole `config.toml`, rules included. Nothing is checked while you type; saving is \
     what runs the daemon's parser, and a file that does not parse is refused rather than \
     written.";

pub const CONFIG_PATH: &str = "The file being edited: the one the daemon reported loading, since `dozed --config` \
     can point it somewhere else. Saving writes here and asks the daemon to re-read it.";

pub const PREVIEW: &str = "What the settings above mean in seconds, for the shipped `bg-idle` rule. Computed by \
     calling the daemon's own `Grace::compute`, so this cannot promise something the daemon \
     will not do.";

pub const VALID: &str = "This parses with `Config::parse` — the daemon's own function, not a second opinion — so \
     the daemon will start on it.";

/// A read-only rule row in the settings tab.
pub fn rule_row(rule: &Rule) -> String {
    format!(
        "`{}`, priority {}. Rules are scanned in priority order and the first match wins, so \
         a lower number here beats everything below it.",
        rule.name, rule.priority
    )
}

/// A multiplier, without the trailing zeros. "Grace x 3" is a factor somebody
/// can hold in their head; "Grace x 3.00" reads as a measurement.
fn trim(factor: f64) -> String {
    let text = format!("{factor:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Bytes, for the trace tooltip. Nothing else in the window prints a size, so
/// this stays local rather than becoming a shared helper with one caller.
fn bytes(n: u64) -> String {
    let f = n as f64;
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.0} KiB", f / 1024.0),
        _ => format!("{:.1} MiB", f / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dozed_core::event::{Exemption, PowerState, ProcSample};
    use dozed_core::{AppKey, MonoTime, Reason};

    fn status() -> StatusReport {
        StatusReport {
            mode: Mode::DryRun,
            configured_mode: None,
            mode_driven_by_trial: false,
            trial_restores: None,
            uptime: Duration::from_secs(60),
            now: MonoTime::from_millis(0),
            power: PowerState { on_ac: false, percentage: Some(50.0) },
            desktop: DesktopState::IdlePresent,
            focus_source_healthy: true,
            session_bus_healthy: true,
            compositor: None,
            focus_source: None,
            apps_known: 3,
            apps_held: 1,
            snooze_remaining: None,
            self_wakeups_per_sec: 4.4,
            bus_calls_per_sec: None,
            wakeup_counter: None,
            frozen_file_error: None,
            config_path: None,
            trace: None,
            exemption_sources: Vec::new(),
            energy: None,
            trial: None,
            thermal: None,
        }
    }

    /// The discrepancy `configured_mode` exists for. A header that resolves it
    /// without explaining it leaves the reader with the same question they
    /// opened the window with.
    #[test]
    fn the_mode_tooltip_explains_a_config_that_lost_to_a_flag() {
        let plain = mode(&status());
        assert!(!plain.contains("config file asks"), "{plain}");

        let overridden = mode(&StatusReport { configured_mode: Some(Mode::Live), ..status() });
        assert!(overridden.contains("config file asks for `live`"), "{overridden}");
        assert!(
            overridden.contains("started without `--live`"),
            "and why it lost — the only way the two can differ: {overridden}"
        );
    }

    /// The header wattage is the whole machine and the table under it is a
    /// handful of apps. A reader who takes the first for the sum of the second
    /// concludes dozed has lost track of the difference.
    #[test]
    fn the_power_draw_says_whose_consumption_it_is() {
        let battery = draw(6.4, &status());
        assert!(battery.contains("6.4 W"), "{battery}");
        assert!(battery.contains("whole machine"), "{battery}");
        assert!(
            !battery.contains("last reading from before"),
            "on battery it is live, not a leftover: {battery}"
        );

        // Shown for one tick after plugging in, then gone: the reader is owed the
        // reason it is about to disappear rather than left to wonder.
        let mains = draw(
            6.4,
            &StatusReport { power: PowerState { on_ac: true, percentage: Some(50.0) }, ..status() },
        );
        assert!(mains.contains("last reading from before"), "{mains}");
    }

    /// The window explains every number it shows; this is the one thing it has to
    /// explain by *not* showing one. A header item for a figure most machines can
    /// never have would be clutter, so it hangs off the label somebody wondering
    /// about power would hover.
    #[test]
    fn a_cpu_counter_that_is_shut_says_so_somewhere() {
        let quiet = power(&status());
        assert!(!quiet.contains("not allowed"), "nothing to explain on most machines: {quiet}");

        let denied = power(&StatusReport {
            energy: Some(dozed_core::ipc::EnergyStatus {
                discharge_w: None,
                package_w: None,
                package_denied: true,
                deepest_idle: None,
                system_wakeups_per_sec: None,
                cpus: 16,
                package_parts: dozed_core::energy::PackageBreakdown::default(),
                gpu_w: None,
                gpu_label: None,
                machine_cores_busy: None,
                power_model: None,
                gpu_busy: None,
            }),
            ..status()
        });
        assert!(denied.contains("not allowed to read the counter"), "{denied}");
        assert!(denied.contains("CVE-2020-8694"), "the tradeoff is named, not hidden: {denied}");
        assert!(denied.contains("README"), "and it says where the instructions are: {denied}");
        // Still the tooltip it was: the note is appended, not substituted.
        assert!(denied.contains("On battery"), "{denied}");
    }

    /// Two wattages can be on screen at once and they are different quantities.
    /// A tooltip that let them read as two opinions about one number would make
    /// the smaller of them look like an error.
    #[test]
    fn the_cpu_draw_is_distinguished_from_the_machines() {
        let text = package_draw(3.1, &status());
        assert!(text.contains("3.1 W"), "{text}");
        assert!(text.contains("Not the whole machine"), "{text}");

        // On mains it is the only power figure there is, which is the whole
        // argument for reading RAPL at all.
        let mains = package_draw(
            3.1,
            &StatusReport { power: PowerState { on_ac: true, percentage: Some(50.0) }, ..status() },
        );
        assert!(mains.contains("measurable on mains"), "{mains}");
    }

    /// Half of what this window shows is a counterfactual in dry-run, and the
    /// state column renders identically in both modes.
    #[test]
    fn anything_that_is_only_hypothetical_says_so() {
        let held = state(Action::Freeze { reclaim: false }, false);
        assert!(held.contains("Nothing is being touched"), "{held}");
        assert!(
            !state(Action::Freeze { reclaim: false }, true).contains("Nothing is being touched")
        );
        // A restore is not hypothetical in any mode that matters: nothing is
        // held in dry-run, so "would have restored" is not a caveat worth
        // hanging on a row that is simply running.
        assert!(!state(Action::Restore, false).contains("Nothing is being touched"));

        assert!(freeze(false).contains("Dry-run"), "{}", freeze(false));
        assert!(!freeze(true).contains("Dry-run"));
        assert!(apps(&status()).contains("In dry-run nothing is really held"));
        assert!(!apps(&StatusReport { mode: Mode::Live, ..status() }).contains("dry-run"));
    }

    /// The exemption column has to keep "this is holding the app" apart from
    /// "this is why nothing is", and then name the tab behind the second one.
    #[test]
    fn a_silent_stream_is_explained_as_the_reason_rather_than_as_an_exemption() {
        let row = |exempt: &[&str], streams: &[(&str, Option<Duration>)]| AppSummary {
            key: AppKey::comm("firefox"),
            label: "firefox".into(),
            visibility: Visibility::Hidden,
            applied: Action::Freeze { reclaim: false },
            mode: Mode::Live,
            sample: ProcSample::default(),
            sampled: true,
            pids: 40,
            exempt: exempt.iter().map(|s| (*s).to_owned()).collect(),
            held_off: None,
            never_actionable: false,
            audio_streams: streams
                .iter()
                .map(|(name, silent_for): &(&str, Option<Duration>)| dozed_core::ipc::AudioStream {
                    name: (*name).to_owned(),
                    silent_for: *silent_for,
                })
                .collect(),
            top_processes: Vec::new(),
            pending: None,
        };

        let silent = exempt(&row(
            &["audio-silent"],
            &[("(505) Discord | #general", Some(Duration::from_secs(90)))],
        ));
        assert!(silent.contains("Nothing exempts this app"), "{silent}");
        assert!(silent.contains("exempting nothing"), "{silent}");
        assert!(silent.contains("none of them audible"), "{silent}");
        assert!(silent.contains("(505) Discord | #general"), "{silent}");
        assert!(silent.contains("silent for 1m30s"), "{silent}");

        // The state that reads as a contradiction: the badge still says `audio`
        // on an app the user cannot hear. The tooltip has to show that the
        // measurement agrees with them and is partway through, rather than
        // leaving them to conclude it disagrees.
        let counting = exempt(&row(
            &["audio"],
            &[("(505) Discord | #general", Some(Duration::from_secs(25)))],
        ));
        assert!(counting.contains("silent for 25s"), "{counting}");
        assert!(counting.contains("audio.silence_after"), "{counting}");
        assert!(counting.contains("Still counted as audio"), "{counting}");

        // Actually playing: the same stream is named, without the claim that
        // nothing is coming out of it.
        let playing = exempt(&row(&["audio"], &[("Rachmaninoff", None)]));
        assert!(playing.contains("Exempt right now"), "{playing}");
        assert!(playing.contains("Rachmaninoff"), "{playing}");
        assert!(!playing.contains("none of them audible"), "{playing}");
        assert!(playing.contains("nothing silent measured on it yet"), "{playing}");
        // Nothing to explain away when the measurement has found no silence at
        // all: the badge and the ears agree.
        assert!(!playing.contains("Still counted as audio"), "{playing}");

        // Nothing open: no heading over an empty list.
        assert!(!exempt(&row(&[], &[])).contains("Streams it has open"));
    }

    /// The masthead joke still has to be true.
    ///
    /// "Countless watts" is the boast; the sentence after it is the reason this
    /// window is allowed to make it. A tooltip that claimed a saving dozed had
    /// not measured would break rule 1 in the one place everybody hovers first.
    #[test]
    fn the_slogan_admits_it_has_not_counted_them() {
        let mut snap = crate::client::Snapshot::default();
        let empty = slogan(&snap);
        assert!(empty.contains("countless watts"), "{empty}");
        assert!(empty.contains("nothing yet"), "{empty}");
        assert!(!empty.contains("of CPU"), "no figure until there is one: {empty}");

        snap.report = Some(dozed_core::journal::Report {
            window: Duration::from_secs(3600),
            covered: Duration::from_secs(1800),
            cpu_seconds: 120.0,
            wakeups: 90_000.0,
            dry_run: true,
            ..dozed_core::journal::Report::default()
        });
        let measured = slogan(&snap);
        assert!(measured.contains("Your count, over the last 1h"), "{measured}");
        assert!(measured.contains("2m of CPU"), "{measured}");
        assert!(measured.contains("counterfactual"), "dry-run must not read as taken: {measured}");
    }

    /// Every exemption the daemon can report has a description here. A source
    /// added to `dozed-core` and not to this table shows up in the window as
    /// a name with no meaning, which is what the section is there to prevent.
    #[test]
    fn every_exemption_source_is_described() {
        for e in Exemption::ALL {
            assert_ne!(
                exemption(e.as_str()),
                exemption("something-invented"),
                "`{}` has no description",
                e.as_str()
            );
        }
    }

    /// The three ways an exemption source can be, which mean three different
    /// things and look nearly identical in the table.
    #[test]
    fn a_source_that_cannot_fire_says_so_rather_than_looking_healthy() {
        let source = |unavailable: Option<&str>, reported| ExemptionSource {
            name: "clipboard-owner".into(),
            reported,
            pids: 2,
            unavailable: unavailable.map(str::to_owned),
            degraded: None,
        };

        let ok = exemption_source(&source(None, true));
        assert!(ok.contains("2 process(es) hold it"), "{ok}");

        let silent = exemption_source(&source(None, false));
        assert!(silent.contains("no rule matching on it can fire"), "{silent}");

        let gone = exemption_source(&source(Some("no XFixes"), false));
        assert!(gone.contains("no XFixes"), "{gone}");
        assert!(gone.contains("never fire on this machine"), "{gone}");
    }

    /// The sampler's own distinction, which the number cannot show: a dash is
    /// "not measured", a zero is "measured, and idle".
    #[test]
    fn an_unmeasured_app_is_explained_as_unmeasured_rather_than_as_idle() {
        let app = AppSummary {
            key: AppKey::comm("slack"),
            label: "slack".into(),
            visibility: Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: ProcSample { cpu_pct: 0.0, wakeups_per_sec: 0.0, gpu_pct: None },
            sampled: false,
            pids: 4,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let window = Duration::from_secs(60);
        assert!(measurement(&app, window, None).contains("Not measured yet"));

        let measured = AppSummary {
            sampled: true,
            sample: ProcSample { cpu_pct: 12.5, wakeups_per_sec: 900.0, gpu_pct: None },
            ..app
        };
        let text = measurement(&measured, window, None);
        assert!(text.contains("900 wakeups a second"), "{text}");
        assert!(text.contains("1m"), "the averaging window has to be in it: {text}");
    }

    /// The countdown is the one thing on screen that talks about the future,
    /// so it is the one that most needs to say what it is not.
    #[test]
    fn the_countdown_says_it_is_a_deadline_and_not_an_appointment() {
        let next = PendingChange {
            within: Duration::from_secs(134),
            rule: "bg-idle".into(),
            action: dozed_core::RuleAction::Throttle(5),
        };

        let text = pending(Some(&next), true);
        assert!(text.contains("`bg-idle`"), "the rule doing the counting: {text}");
        assert!(text.contains("throttle to 5%"), "and what it will actually do: {text}");
        assert!(text.contains("If nothing else changes"), "{text}");
        assert!(!text.contains("dry-run"));

        assert!(pending(Some(&next), false).contains("dry-run"), "a timer that acts on nothing");
        assert!(
            pending(None, true).contains("Nothing is due to happen"),
            "an app with no countdown has something to say too"
        );
    }

    /// The timeline is one line per decision, and the line drops the derivation
    /// that makes it checkable.
    #[test]
    fn a_timeline_entry_carries_the_rule_and_the_outcome() {
        let e = JournalEntry {
            at: MonoTime::from_millis(1000),
            wall: 0,
            app: AppKey::comm("slack"),
            action: Action::Freeze { reclaim: false },
            reason: Reason::rule("bg-idle", "hidden for 5m12s"),
            outcome: Outcome::WouldHave,
        };
        let text = entry(&e);
        assert!(text.contains("rule `bg-idle`"), "{text}");
        assert!(text.contains("hidden for 5m12s"), "{text}");
        assert!(text.contains("dry-run, so nothing was touched"), "{text}");
    }

    /// The level names are the whole interface to the grace product, and the
    /// multiplier is what they actually do.
    #[test]
    fn each_aggression_level_prints_its_own_multiplier() {
        assert!(aggression(Aggression::Gentle).contains("Grace x 3"));
        assert!(aggression(Aggression::Balanced).contains("Grace x 1"));
        assert!(aggression(Aggression::Maximum).contains("Grace x 0.15"));
    }

    #[test]
    fn sizes_are_readable_at_every_scale() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    /// The one verdict whose hover has to do more than describe a state.
    ///
    /// A power cap does not degrade the watts column — it inverts what the column
    /// means, because at the cap freeing an app reallocates its budget instead of
    /// lowering the draw. A tooltip that only said "no fit available" would leave
    /// the reader with a number pointing the wrong way.
    #[test]
    fn a_power_capped_package_explains_what_it_does_to_the_watts_column() {
        let text = power_model(&dozed_core::power_model::Verdict::PowerLimited {
            watts: 24.2,
            cores_range: 9.5,
        });
        assert!(text.contains("24.2 W by firmware"), "{text}");
        assert!(text.contains("hands that budget"), "the consequence, not just the state: {text}");
        assert!(text.contains("does not lower the package draw"), "{text}");
    }

    /// Every refusal says which one it is. "Unavailable" for all five would be
    /// the silence §10 exists to stop, and they call for different responses:
    /// one needs time, one needs the machine to do something different, and one
    /// needs nothing because it is not going to change.
    #[test]
    fn each_way_the_power_model_declines_says_which_one_it_is() {
        use dozed_core::power_model::{Fit, Verdict};
        let cases = [
            (Verdict::Warming { samples: 3, needed: 30 }, "Not enough readings"),
            (Verdict::Unvarying { cores_range: 0.2 }, "held one load"),
            (Verdict::Collinear { correlation: 0.99 }, "move as one"),
            (Verdict::Poor { r2: 0.1 }, "explains too little"),
            (
                Verdict::Unphysical { r2: 0.82, watts_per_core: -0.5, joules_per_wakeup: 1e-6 },
                "not physics",
            ),
            (
                Verdict::Fitted(Fit {
                    floor_w: 6.0,
                    watts_per_core: 2.0,
                    joules_per_wakeup: 3e-5,
                    r2: 0.87,
                    samples: 90,
                    se_watts_per_core: 0.05,
                    se_joules_per_wakeup: 1e-6,
                    cov_core_wakeup: 0.0,
                }),
                "30.0 µJ",
            ),
        ];
        for (verdict, want) in cases {
            let text = power_model(&verdict);
            assert!(text.contains(want), "{verdict:?} rendered without {want:?}: {text}");
        }
    }
}
