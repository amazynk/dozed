//! The journal, and the counterfactual report built from it.
//!
//! A battery tool that cannot show a benefit is indistinguishable from one that
//! does not work. In dry-run we keep sampling an app while it *would have been*
//! dozing, so the saving we report is measured from the real process rather than
//! modelled — which is what makes it credible enough to switch live mode on
//! (`DESIGN.md` §11).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app::AppKey;
use crate::intent::{Action, Intent, Outcome, Reason};
use crate::time::{MonoTime, Rounded};

/// One line of the journal. Written in both modes, identical apart from
/// `outcome` — that identity is the whole point of the dry-run seam.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub at: MonoTime,
    /// Wall-clock seconds since the unix epoch, so entries survive a restart
    /// with a meaning a human can read.
    pub wall: u64,
    pub app: AppKey,
    pub action: Action,
    pub reason: Reason,
    pub outcome: Outcome,
}

impl JournalEntry {
    #[must_use]
    pub fn new(intent: &Intent, outcome: Outcome, wall: u64) -> Self {
        Self {
            at: intent.at,
            wall,
            app: intent.app.clone(),
            action: intent.action,
            reason: intent.reason.clone(),
            outcome,
        }
    }

    /// What was done, phrased so dry-run reads as the counterfactual it is.
    #[must_use]
    pub fn verb(&self) -> String {
        match self.outcome {
            Outcome::WouldHave => format!("would {}", self.action.verb()),
            _ => format!("{}d", self.action.verb()),
        }
    }

    /// The outcome qualifier. Empty unless the action did not land, in which
    /// case it must travel with the reason — a `dozed` line and a `dozed
    /// [skipped: …]` line mean opposite things.
    #[must_use]
    pub fn qualifier(&self) -> String {
        match &self.outcome {
            Outcome::Skipped { why } => format!(" [skipped: {why}]"),
            Outcome::Failed { error } => format!(" [FAILED: {error}]"),
            _ => String::new(),
        }
    }

    /// The whole entry on one line, for contexts that supply their own clock:
    /// the daemon's `tracing` output and the trace renderer both prefix a time
    /// of their own, so this one carries `at` and nothing else. `dozectl
    /// journal` builds its own columns from the parts above.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {} — {}{}", self.app.label(), self.verb(), self.reason.detail, self.qualifier())
    }

    /// The same line with its timestamp, for a terminal where there is no hover
    /// to put it in.
    ///
    /// The dashboard drops the leading offset from every row and shows it on
    /// hover instead: in a panel a few hundred pixels wide it cost nine or ten
    /// characters of *every* line to answer a question asked of about one line
    /// in fifty, and the app name and the reason are what the row is for. In a
    /// terminal the row is as wide as the terminal and the column is free, so
    /// `dozectl journal` keeps it.
    #[must_use]
    pub fn stamped_line(&self) -> String {
        format!("{} {}", self.at, self.line())
    }
}

/// What one app cost us, or would have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSaving {
    pub app: AppKey,
    /// Total time spent dozing, or that would have been spent dozing.
    pub dozed: Duration,
    /// How much of `dozed` was a throttle rather than a freeze.
    ///
    /// Carried separately because the two rungs are not the same claim and the
    /// headline was quietly adding them up: measured live, a session reporting
    /// 23h "dozed" had 8h41m of it in a 5% cap on Signal and Thunderbird, which
    /// are *meant* to keep running (§7). A capped app is held, not dozing, and a
    /// summary that cannot tell them apart overstates the stronger of the two.
    #[serde(default)]
    pub throttled: Duration,
    /// CPU-seconds the app burned while it would have been frozen. In live mode
    /// this is what it did *not* burn; in dry-run it is what it actually burned
    /// and need not have.
    pub cpu_seconds: f64,
    pub wakeups: f64,
    pub episodes: u32,
    /// CPU-seconds the app was observed still burning *while held frozen*.
    ///
    /// Zero is the expectation and anything else is a freeze that is not holding.
    /// Always zero in dry-run, where nothing is held. This is the number that
    /// would have caught a `SIGSTOP` reaching 1 of an app's 44 processes on the
    /// first night instead of after seven hours.
    #[serde(default)]
    pub leaked_cpu_seconds: f64,
    #[serde(default)]
    pub leaked_wakeups: f64,
}

impl AppSaving {
    /// Whether this app was measurably still running while supposedly frozen.
    ///
    /// No threshold and no ratio: `leaked_cpu_seconds` now counts only samples
    /// whose whole averaging window fell inside the frozen period, so every
    /// CPU-second in it is activity that happened while the app was supposed to
    /// be stopped. A small floor remains for float dust and for a scheduler tick
    /// landing on a process as it stops.
    ///
    /// Three thresholds were tried before this and each produced a false alarm on
    /// a verifiably silent freeze, because the thing they were trying to filter —
    /// the pre-freeze work inside a straddling sample — is real measurement, not
    /// noise. It had to be excluded at the source rather than argued away here.
    /// Wakeups count as well as CPU, and leaving them out let the check miss
    /// the exact bug it was written for — twice.
    ///
    /// Brave, frozen through a scope that held 2 of its 13 processes, leaked
    /// roughly six million wakeups over seven hours at **0.0 CPU-seconds**: the
    /// frozen cgroup's `cpu.stat` was honestly zero while every context switch
    /// came from the eleven processes outside it. A CPU-only verdict passed it
    /// cleanly, and the same shape passed again when a throttle was skipped and
    /// reported as applied. §13 opens by saying wakeups are the primary metric
    /// and CPU% is the ambiguous one; this was the place that forgot it.
    ///
    /// Wakeups are judged as a **rate**, unlike the CPU total, and the
    /// difference is the settling tail. A freeze does not silence an app the
    /// instant it lands: the sample whose averaging window opens at the freeze
    /// still carries the last of its activity, and one such reading on a real
    /// Firefox contributed 810 wakeups. Against an absolute floor that is a
    /// leak; against 147 seconds of dozing it is 5.5/s, which is a tail. Brave
    /// escaping its cgroup ran at 246/s for seven hours.
    ///
    /// This is the same lesson the CPU side already learned from the opposite
    /// direction — there, a total measured against a saving that had only had
    /// 147 seconds to accumulate produced the false alarm, and the fix was to
    /// stop counting the straddle at all. Here the straddle is excluded too,
    /// but the residue is larger relative to the metric, so the comparison has
    /// to be against time rather than against zero.
    ///
    /// Either metric alone is enough. They answer different questions — "it is
    /// still computing" and "it is still waking the CPU" — and either one means
    /// the hold is not holding.
    ///
    /// ## Both are rates now
    ///
    /// CPU was an absolute total — `leaked_cpu_seconds > 1.0` — while wakeups
    /// were a rate, and the argument above for making wakeups a rate applies
    /// word for word to CPU: *"against an absolute floor that is a leak; against
    /// 147 seconds of dozing it is a tail"*. A total has no denominator, so
    /// whatever residue survives per episode simply accumulates until it crosses
    /// the floor, and the floor is then reached by duration rather than by
    /// severity. Measured on a real report: Firefox over **594 episodes** and
    /// 6d21h of holding had leaked 2 CPU-seconds — 3ms an episode, 0.0018% of
    /// the 113,867 CPU-seconds the same row claimed to have saved — and was
    /// flagged, while the same report's genuinely escaped app was burning 27% of
    /// a core. One of those is a tail and the other is a freeze that never took,
    /// and the total ranked them the wrong way round.
    ///
    /// The threshold is 1% of one core, which keeps the old sensitivity where the
    /// old rule was right: at the 100-second episode the `1.0` floor was chosen
    /// for, 1% of a core is one CPU-second. It stops being reached by patience.
    #[must_use]
    pub fn leaking(&self) -> bool {
        self.leak_cpu_fraction() > 0.01 || self.leak_wakeup_rate() > 20.0
    }

    /// Leaked CPU as a fraction of one core over the time held.
    ///
    /// The denominator the absolute total never had. See [`Self::leaking`].
    #[must_use]
    pub fn leak_cpu_fraction(&self) -> f64 {
        let secs = self.dozed.as_secs_f64();
        if secs > 0.0 { self.leaked_cpu_seconds / secs } else { 0.0 }
    }

    /// Leaked wakeups per second of hold — the form [`Self::leaking`] judges.
    ///
    /// Reported as well as judged, because the two failures it separates look
    /// identical as a total and nothing like each other as a rate: a real
    /// Firefox settling after a freeze contributed 810 wakeups, and Brave
    /// escaping its cgroup ran at 246/s for seven hours. A reader shown only
    /// "~6M wakeups" cannot tell a tail from an escape, which is the whole
    /// question once this flag is rare rather than routine.
    #[must_use]
    pub fn leak_wakeup_rate(&self) -> f64 {
        let secs = self.dozed.as_secs_f64();
        if secs > 0.0 { self.leaked_wakeups / secs } else { 0.0 }
    }
}

/// The whole-session answer to "was this worth running?".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub window: Duration,
    pub apps: Vec<AppSaving>,
    /// The sum of every app's hold time. Apps doze concurrently, so this is
    /// **app-time** and its ceiling is `apps × window`, not `window` — six apps
    /// left alone for a five-hour session legitimately total twenty-five hours.
    ///
    /// Kept as a sum because that is what the tuner scores (§14) and what the
    /// per-app rows add up to. What it is *not* is elapsed time, and printing it
    /// under a wall-clock window without saying so read as a plain arithmetic
    /// error the first time a real session put six apps in it — hence
    /// [`Report::covered`] beside it.
    pub total_dozed: Duration,
    /// Wall-clock time within the window during which *at least one* app was
    /// held. The union of the episodes rather than their sum, so it can never
    /// exceed `window` and is the number that answers "how much of my session
    /// was dozed doing something".
    #[serde(default)]
    pub covered: Duration,
    pub cpu_seconds: f64,
    pub wakeups: f64,
    /// Whether these are counterfactual ("would have") or realised numbers.
    pub dry_run: bool,
    /// Battery capacity in watt-hours, when there is a battery. Carried with
    /// the report so a client can render the charge estimate without having to
    /// talk to UPower itself.
    #[serde(default)]
    pub battery_wh: Option<f64>,
    /// Whether the machine is on mains right now.
    ///
    /// Carried for one reason: the charge estimate is expressed as a fraction
    /// of a battery, and while the wall is supplying the power there is no
    /// charge being spent for it to be a fraction of. Reporting "3-6% of a
    /// charge" to someone who is plugged in states a saving that did not
    /// happen.
    #[serde(default)]
    pub on_ac: bool,
    /// What a self-trial measured, when one has been run (`crate::trial`).
    ///
    /// Carried on the report rather than kept apart because they answer the same
    /// question with different authority, and a reader deciding whether to switch
    /// live mode on deserves to see them side by side. The modelled figure above
    /// is available immediately and cannot be wrong in a way anyone notices; this
    /// one takes an evening to earn and can come back saying "no difference".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<crate::trial::Verdict>,
    /// This machine's own power coefficients, where it has given up enough to
    /// fit them (`crate::power_model`).
    ///
    /// The whole reason §11.1 calls the estimate below the weakest claim in the
    /// design is that its basis — 2-4 J a CPU-second — was asserted rather than
    /// measured, and was blind to wakeups, which is the metric the tool exists
    /// for. This is that basis, measured, and it counts wakeups.
    ///
    /// `None` on most machines and most of the time: RAPL is `0400 root`, and
    /// even with it open the fit is refused whenever the machine has not varied
    /// enough to identify the coefficients. The 2-4 J model is still there
    /// underneath, and the report says which one produced the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_fit: Option<crate::power_model::Fit>,
}

impl Report {
    /// An estimate of the fraction of a charge saved, as a range, from whichever
    /// of the two bases is available ([`Self::saving_basis`]).
    ///
    /// **Fitted**, where this machine has given up its own coefficients: the
    /// joules are `watts_per_core × cpu_seconds + joules_per_wakeup × wakeups`,
    /// and the range is those coefficients' own standard errors propagated
    /// through. This is the answer §11.1 asks for. It counts wakeups, which the
    /// model below cannot — and the app that wakes the CPU 900 times a second
    /// while using almost no CPU is the case this whole tool is built around.
    ///
    /// **Modelled**, otherwise: a busy core on a modern laptop costs roughly
    /// 2-4 W, so a CPU-second saved is roughly 2-4 J. Wide on purpose, because
    /// nothing narrower would be honest, and blind to wakeups because there is
    /// nothing here to price them with.
    ///
    /// `None` when nothing was saved to estimate from, or when there is no
    /// battery for it to be a fraction of.
    #[must_use]
    pub fn charge_fraction(&self, battery_wh: f64) -> Option<(f64, f64)> {
        if battery_wh <= 0.0 {
            return None;
        }
        let capacity_j = battery_wh * 3600.0;
        let (low_j, high_j) = match self.power_fit {
            // Either counter having moved is something to price. Under the model
            // below only CPU time counts, so a purely wakeup-bound saving — the
            // one that matters most — has to be nothing at all.
            Some(fit) if self.cpu_seconds > 0.0 || self.wakeups > 0.0 => {
                fit.joules_interval(self.cpu_seconds, self.wakeups)
            }
            _ if self.cpu_seconds > 0.0 => (self.cpu_seconds * 2.0, self.cpu_seconds * 4.0),
            _ => return None,
        };
        Some((low_j / capacity_j * 100.0, high_j / capacity_j * 100.0))
    }

    /// Where the number in [`Self::charge_fraction`] came from, in a few words.
    ///
    /// Printed beside it. §11.1's objection to the old estimate was never only
    /// that it was wide — it was that nothing on screen said what it rested on,
    /// so it could not be argued with. A reader who can see "2-4 J a CPU-second,
    /// wakeups not counted" can at least tell how much to trust it, and can tell
    /// that opening one counter would change the answer.
    #[must_use]
    pub fn saving_basis(&self) -> &'static str {
        match self.power_fit {
            Some(_) => "measured on this machine, wakeups included",
            None => "2-4 J a CPU-second, wakeups not counted",
        }
    }

    /// The §11 summary, in the phrasing the design specifies.
    #[must_use]
    pub fn render(&self, battery_wh: Option<f64>) -> String {
        let mut out = String::new();
        let verb = if self.dry_run { "would have" } else { "has" };
        // `Rounded` throughout this summary, not `HumanDuration`: every duration
        // here is a total somebody is reading, and a live report printed
        // `7d17h19m8s39ms` in the column below. The daemon's journal keeps the
        // exact form for the reason `Coarse` gives — a grace is a product
        // printed beside its factors — and nothing on this page is a grace, a
        // countdown, or anything else whose seconds somebody needs.
        out.push_str(&format!("Over the last {}, dozed {verb}:\n", Rounded(self.window)));
        // "held", not "dozed", because a throttled app is held and still
        // running, and the split below only makes sense under a verb that
        // covers both rungs.
        let throttled: Duration = self.apps.iter().map(|a| a.throttled).sum();
        let frozen = self.total_dozed.saturating_sub(throttled);
        out.push_str(&format!(
            "  held {} app{} for a total of {} of app-time{}\n",
            self.apps.len(),
            if self.apps.len() == 1 { "" } else { "s" },
            Rounded(self.total_dozed),
            if throttled > Duration::ZERO {
                format!(" — {} frozen, {} throttled", Rounded(frozen), Rounded(throttled))
            } else {
                String::new()
            }
        ));
        // The line above sums apps that were dozing at the same time, so under a
        // wall-clock window it reads as impossible — a real session printed
        // "a total of 23h25m" beneath "over the last 4h58m" and the arithmetic
        // was correct. Say what the elapsed number actually is rather than
        // leaving the reader to work out which of the two they are looking at.
        //
        // Only when they differ: with one app, or with episodes that never
        // overlap, the sum *is* the elapsed time and repeating it explains
        // nothing.
        if self.covered > Duration::ZERO && self.covered < self.total_dozed {
            out.push_str(&format!(
                "  something was held for {} of that window; apps overlap, so the total \
                 above counts each one separately\n",
                Rounded(self.covered)
            ));
        }
        out.push_str(&format!(
            "  avoided ~{:.0} CPU-seconds and ~{} wakeups\n",
            // `max(0.0)`, because `Sum` for f64 has -0.0 as its identity and an
            // empty app list otherwise renders as "~-0". These are non-negative by
            // construction, so a minus sign on them is never information.
            self.cpu_seconds.max(0.0),
            compact(self.wakeups)
        ));
        // Suppressed on mains. The CPU-seconds and wakeups above are real
        // either way — they were or were not spent — but a fraction *of a
        // charge* is not a meaningful unit while the charge is not what is
        // paying for them.
        if !self.on_ac
            && let Some((lo, hi)) = battery_wh.and_then(|wh| self.charge_fraction(wh))
        {
            // A decimal below 1%, because whole percent renders a real saving
            // as "0-0% of a charge" — 226 CPU-seconds against a 60Wh battery is
            // 0.2-0.4%, and printing that as zero says the tool did nothing.
            let basis = self.saving_basis();
            let out_line = if hi < 1.0 {
                format!("  estimated saving: {lo:.1}-{hi:.1}% of a charge ({basis})\n")
            } else {
                format!("  estimated saving: {lo:.0}-{hi:.0}% of a charge ({basis})\n")
            };
            out.push_str(&out_line);
        }
        if !self.apps.is_empty() {
            out.push('\n');
            for a in &self.apps {
                out.push_str(&format!(
                    "  {:<24} {:>8}   ~{:>5.0} CPU-s   ~{} wakeups{}\n",
                    a.app.label(),
                    Rounded(a.dozed).to_string(),
                    a.cpu_seconds.max(0.0),
                    compact(a.wakeups),
                    // "LEAKING" beside an app's name reads as that app leaking
                    // something — memory, for most people. The condition is
                    // dozed's: this app went on running while dozed had it held,
                    // so the hold did not take. The block below has always said
                    // it that way round; the row said it the other.
                    if a.leaking() { "   NOT HELD" } else { "" },
                ));
            }
        }
        // Said once, in full, at the bottom: a freeze that is not holding makes
        // every other number on the page meaningless, so it cannot be a footnote
        // on one row.
        let leaking: Vec<&AppSaving> = self.apps.iter().filter(|a| a.leaking()).collect();
        if !leaking.is_empty() {
            out.push_str(&format!(
                "\n{} app(s) kept running while held frozen — the freeze is not taking, so \
                 the savings above are overstated by whatever these cost:\n",
                leaking.len()
            ));
            for a in leaking {
                // The rate, beside the totals. A settling tail and a process
                // that escaped its cgroup are the same number of wakeups and
                // two different problems; only the rate tells them apart, and
                // only one of them is worth reporting as a bug.
                out.push_str(&format!(
                    "  {:<24} ~{:.0} CPU-s and ~{} wakeups observed *while dozing* \
                     ({:.1}% of a core, {:.1} wakeups/s, over {} episode(s))\n",
                    a.app.label(),
                    a.leaked_cpu_seconds,
                    compact(a.leaked_wakeups),
                    a.leak_cpu_fraction() * 100.0,
                    a.leak_wakeup_rate(),
                    a.episodes,
                ));
            }
        }
        // Last, and labelled as the stronger claim, because it is: everything
        // above is a model of what a saving would be worth, and this is what a
        // battery did. If they disagree, this one is right.
        if let Some(measured) = &self.measured {
            out.push_str("\nMeasured directly, by running the experiment (`dozectl trial`):\n");
            out.push_str(&measured.render(battery_wh));
        }
        out
    }
}

fn compact(n: f64) -> String {
    match n.max(0.0) {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e3 => format!("{:.0}k", n / 1e3),
        n => format!("{n:.0}"),
    }
}

/// Accumulates savings from the intent stream plus samples.
///
/// Drives both `dozectl report` and rule tuning via replay, so it must depend
/// only on things a recorded trace contains.
#[derive(Debug, Default)]
pub struct ReportAccumulator {
    apps: BTreeMap<AppKey, AppAccum>,
    started: Option<MonoTime>,
    last: MonoTime,
    /// How many apps have an episode open right now. The union below is a
    /// reference count rather than a set of intervals kept for later: episodes
    /// arrive in time order, so the only thing needed to merge them is knowing
    /// when the count leaves and returns to zero.
    held: usize,
    /// When the count last went from zero to one, if it is still above zero.
    any_since: Option<MonoTime>,
    /// Closed union of every episode — see [`Report::covered`].
    covered: Duration,
}

#[derive(Debug, Default, Clone)]
struct AppAccum {
    /// When the current dozing episode began, if one is in progress.
    dozing_since: Option<MonoTime>,
    /// When we last folded a sample in, so intervals do not double count.
    last_sample: Option<MonoTime>,
    total: Duration,
    /// The part of `total` spent throttled rather than frozen.
    throttled: Duration,
    cpu_seconds: f64,
    wakeups: f64,
    episodes: u32,
    /// Whether the current episode was really applied, or merely journalled.
    ///
    /// The two need different arithmetic, and conflating them made the headline
    /// number mean the opposite of what it said. In dry-run the app is still
    /// running, so what it is observed doing *is* the waste a freeze would
    /// remove. Once actually frozen it should be doing nothing, so the same sum
    /// measures **leakage** — and a freeze that failed outright then reported the
    /// largest saving. Measured overnight: a Firefox whose freeze reached 1 of 44
    /// processes was credited 1604 CPU-seconds "avoided", while the one app that
    /// really was frozen was correctly credited nothing.
    applied: bool,
    /// Whether the current episode stops the app outright.
    ///
    /// A throttled app is *meant* to keep running — §7 caps a chat app precisely
    /// so it stays connected — so its continued activity is neither leakage nor a
    /// full saving. Conflating the two rungs flagged a correctly capped Signal
    /// with "the freeze is not taking", which is the sort of false alarm that
    /// teaches people to ignore the real one.
    stops_the_app: bool,
    /// The cap, when this episode is a throttle. A cap cannot withhold more than
    /// the amount by which the app's demand exceeds it — which is the whole of
    /// what a throttle can honestly claim.
    cap_pct: Option<f32>,
    /// Whether a baseline was taken while the app was hidden and settled.
    ///
    /// Without one there is nothing to project from, and the episode claims
    /// nothing. A baseline sampled within one `cpu_sample_window` of the app being
    /// on screen is contaminated by its *foreground* rate: measured live, a Signal
    /// that sustains 3.5% while hidden was credited against a 5.4% baseline
    /// captured a minute after it lost focus.
    baseline_settled: bool,
    /// The last rate seen while the app was *not* dozing, which is the only
    /// honest basis for what a freeze avoided once the app is frozen and quiet.
    baseline_cpu_pct: f32,
    baseline_wakeups: f32,
    /// What the app was observed still doing while held. Zero is the expectation;
    /// anything else is a freeze that is not holding.
    leaked_cpu_seconds: f64,
    leaked_wakeups: f64,
}

impl ReportAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in a decision. Both `Applied` and `WouldHave` count: the whole point
    /// is that dry-run produces the same accounting live mode would.
    pub fn on_intent(&mut self, app: &AppKey, action: Action, outcome: &Outcome, at: MonoTime) {
        if matches!(outcome, Outcome::Skipped { .. } | Outcome::Failed { .. }) {
            return;
        }
        self.mark(at);
        if action.is_restore() {
            self.close_episode(app, at);
            return;
        }
        let entry = self.apps.entry(app.clone()).or_default();
        if entry.dozing_since.is_some() {
            return;
        }
        entry.dozing_since = Some(at);
        entry.last_sample = Some(at);
        entry.episodes += 1;
        entry.applied = matches!(outcome, Outcome::Applied);
        entry.stops_the_app = !matches!(action, Action::Throttle { .. });
        entry.cap_pct = match action {
            Action::Throttle { pct } => Some(f32::from(pct)),
            _ => None,
        };
        if self.held == 0 {
            self.any_since = Some(at);
        }
        self.held += 1;
    }

    /// Close the open episode for `app`, if it has one. Whether it did.
    ///
    /// The only place an episode ends, so the per-app arithmetic and the
    /// whole-session union cannot drift apart — they were inline in
    /// [`Self::on_intent`] when a restore intent was the only way to get here.
    fn close_episode(&mut self, app: &AppKey, at: MonoTime) -> bool {
        let Some(entry) = self.apps.get_mut(app) else { return false };
        let Some(since) = entry.dozing_since.take() else { return false };
        let held = at.since(since);
        entry.total += held;
        if !entry.stops_the_app {
            entry.throttled += held;
        }
        entry.last_sample = None;
        // `saturating_sub` rather than `-= 1`: the count is only ever decremented
        // on an episode this accumulator opened, but a panic here would take the
        // daemon down over a report line.
        self.held = self.held.saturating_sub(1);
        if self.held == 0
            && let Some(since) = self.any_since.take()
        {
            self.covered += at.since(since);
        }
        true
    }

    /// Close every episode whose hold has actually ended, and name them.
    ///
    /// `held` is what the actuator is holding *now* — the same source §10.3's
    /// frozen set is written from, and the only thing entitled to answer the
    /// question. This accumulator otherwise learns about holds from the intent
    /// stream, and that stream is not guaranteed to close what it opens:
    ///
    /// * An app that **exits while frozen** is never restored, because there is
    ///   nothing left to restore. No intent is emitted, ever.
    /// * A restore that comes back `Skipped` or `Failed` is dropped at the top of
    ///   [`Self::on_intent`], so an app released by a path that declined to act
    ///   leaves its episode open.
    ///
    /// The episode then stays open for the rest of the daemon's life, and
    /// [`Self::report`] counts an open episode's time right up to `now` — so the
    /// row keeps accruing hold time the app never spent held, and every sample
    /// it produces is charged as leakage against a hold that ended. Measured on a
    /// live machine: one app showed a **single nine-day episode** leaking 2,352
    /// wakeups/s and 27% of a core while it was plainly running and exempt, and
    /// the hold time and projected saving printed beside that were fiction from
    /// the same cause. It read as the one failure this whole area exists to
    /// detect, and it was an accounting error.
    ///
    /// Only episodes that claim to be **applied** are reconciled. A dry-run
    /// episode is `WouldHave`, nothing is being held, and the actuator could
    /// never list it — closing those would delete dry-run reporting entirely,
    /// which is the mode that has to work before any of this is trusted.
    pub fn reconcile_held(&mut self, held: &BTreeSet<AppKey>, at: MonoTime) -> Vec<AppKey> {
        let orphaned: Vec<AppKey> = self
            .apps
            .iter()
            .filter(|(key, a)| a.dozing_since.is_some() && a.applied && !held.contains(*key))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &orphaned {
            self.close_episode(key, at);
        }
        orphaned
    }

    /// Fold in a resource sample.
    ///
    /// While the app is *not* dozing this only records a baseline. While it is,
    /// what the sample means depends on whether the freeze was real:
    ///
    /// * **dry-run** — the app is still running, so what it is observed doing is
    ///   exactly the waste a freeze would have removed. Count it.
    /// * **live** — the app should be doing nothing, so the observed figure is
    ///   leakage, and what was *avoided* has to be projected from the last rate
    ///   seen before the freeze. Counting the observed figure as a saving made a
    ///   failing freeze look like the best-performing one.
    ///
    /// With no baseline — an app frozen before it was ever seen running, which is
    /// what a daemon started while the screen is locked does — the projection is
    /// zero. That under-claims, and under-claiming is the only honest option: we
    /// never measured the thing we would be estimating.
    pub fn on_sample(
        &mut self,
        app: &AppKey,
        cpu_pct: f32,
        wakeups_per_sec: f32,
        at: MonoTime,
        settled: bool,
        window: Duration,
    ) {
        self.mark(at);
        // `or_default` rather than `get_mut`: the baseline is by definition
        // recorded before the app has ever dozed, so before any intent has
        // created its entry. Apps that never doze end up with an empty accumulator
        // and are filtered out of the report by their zero episode count.
        let entry = self.apps.entry(app.clone()).or_default();
        if entry.dozing_since.is_none() {
            // Only a settled sample becomes the baseline: one taken while the app
            // was recently on screen measures the user working in it, not the cost
            // of leaving it alone.
            if settled {
                entry.baseline_cpu_pct = cpu_pct;
                entry.baseline_wakeups = wakeups_per_sec;
                entry.baseline_settled = true;
            }
            return;
        }
        let prev = entry.last_sample.unwrap_or(at);
        let dt = at.since(prev).as_secs_f64();
        entry.last_sample = Some(at);
        if dt <= 0.0 {
            return;
        }
        let (cpu, wakeups) = match (entry.applied, entry.stops_the_app) {
            // A real freeze: the app should be silent, so what it is still doing
            // is leakage, and what was avoided is the settled baseline. Without a
            // settled baseline there is nothing to project from and we say so by
            // claiming nothing.
            (true, true) => {
                // Only a sample whose *whole window* lies inside the frozen
                // period says anything about leakage. Anything sooner still
                // contains pre-freeze work, and counting it produced three
                // separate false alarms: a short episode measured against a small
                // saving, and then — once browsers were tuned to doze every four
                // minutes — fourteen straddles in one session summing to 24 CPU-s
                // on an app whose freezes were verifiably silent (0.00% on every
                // sample that had actually drained).
                //
                // A threshold cannot fix that, because the straddle is real
                // measurement, not noise. Excluding it by construction can, and it
                // also makes an episode shorter than one window contribute
                // nothing — which is right, since such an episode cannot be
                // judged either way.
                if entry.dozing_since.is_some_and(|since| at.since(since) >= window) {
                    entry.leaked_cpu_seconds += f64::from(cpu_pct) / 100.0 * dt;
                    entry.leaked_wakeups += f64::from(wakeups_per_sec) * dt;
                }
                if entry.baseline_settled {
                    (entry.baseline_cpu_pct, entry.baseline_wakeups)
                } else {
                    (0.0, 0.0)
                }
            }
            // A throttle, in either mode. A cap can only remove the demand *above*
            // it, so that is the ceiling on the claim — `baseline - observed`
            // credited ordinary variation to the cap and produced 173 CPU-seconds
            // for a 5% cap on an app demanding 3.5%, which it never touched.
            //
            // And no wakeup claim at all: a CPU quota preempts rather than quiets,
            // so it does not reduce context switches. Measured on the same app,
            // wakeups went *up* under the cap, 492/s to 526/s.
            (_, false) => {
                let cap = entry.cap_pct.unwrap_or(0.0);
                let cpu = if !entry.applied {
                    // Dry-run: nothing is capped, so the claim is the demand
                    // standing above where the cap would have been.
                    (cpu_pct - cap).max(0.0)
                } else if entry.baseline_settled {
                    // Live: bounded by the mechanism *and* by what happened. A cap
                    // cannot withhold more than demand-above-cap, and must not be
                    // credited with a reduction that did not occur — so a cap that
                    // silently fails to cap claims nothing, which is the signal.
                    (entry.baseline_cpu_pct - cpu_pct.max(cap)).max(0.0)
                } else {
                    0.0
                };
                (cpu, 0.0)
            }
            // Dry-run freeze: nothing is held, so what the app is observed doing is
            // exactly the waste the freeze would have removed.
            (false, true) => (cpu_pct, wakeups_per_sec),
        };
        entry.cpu_seconds += f64::from(cpu) / 100.0 * dt;
        entry.wakeups += f64::from(wakeups) * dt;
    }

    fn mark(&mut self, at: MonoTime) {
        self.started.get_or_insert(at);
        self.last = self.last.max(at);
    }

    /// Snapshot the report as of `now`, closing any open episodes.
    #[must_use]
    pub fn report(&self, now: MonoTime, dry_run: bool) -> Report {
        let mut apps: Vec<AppSaving> = self
            .apps
            .iter()
            .map(|(key, a)| {
                let open = a.dozing_since.map_or(Duration::ZERO, |s| now.since(s));
                AppSaving {
                    app: key.clone(),
                    dozed: a.total + open,
                    // An open episode counts towards whichever rung it is on, the
                    // same way `dozed` counts it — otherwise a session snapshotted
                    // mid-throttle reports the time as frozen.
                    throttled: a.throttled + if a.stops_the_app { Duration::ZERO } else { open },
                    cpu_seconds: a.cpu_seconds,
                    wakeups: a.wakeups,
                    episodes: a.episodes,
                    leaked_cpu_seconds: a.leaked_cpu_seconds,
                    leaked_wakeups: a.leaked_wakeups,
                }
            })
            .filter(|a| a.dozed > Duration::ZERO || a.episodes > 0)
            .collect();
        // Biggest contributor first: the user wants to know which app to tune.
        apps.sort_by(|a, b| {
            b.cpu_seconds.total_cmp(&a.cpu_seconds).then_with(|| b.dozed.cmp(&a.dozed))
        });

        Report {
            window: now.since(self.started.unwrap_or(now)),
            total_dozed: apps.iter().map(|a| a.dozed).sum(),
            // Close the open run of the union at `now`, the same way the per-app
            // totals close their open episodes.
            covered: self.covered + self.any_since.map_or(Duration::ZERO, |since| now.since(since)),
            cpu_seconds: apps.iter().map(|a| a.cpu_seconds).sum(),
            wakeups: apps.iter().map(|a| a.wakeups).sum(),
            apps,
            dry_run,
            // All filled in by the daemon, which is where the power state, the
            // battery's capacity and the trial live.
            battery_wh: None,
            on_ac: false,
            measured: None,
            power_fit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sampler's averaging window in these tests.
    const MINUTE: Duration = Duration::from_secs(60);

    fn t(secs: u64) -> MonoTime {
        MonoTime::from_millis(secs * 1000)
    }

    fn slack() -> AppKey {
        AppKey::flatpak("com.slack.Slack")
    }

    #[test]
    fn accumulates_a_dry_run_episode() {
        let mut acc = ReportAccumulator::new();
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::WouldHave, t(0));
        // Ten minutes of sampling a process that would have been frozen.
        for m in 1..=10 {
            acc.on_sample(&slack(), 8.0, 500.0, t(m * 60), true, MINUTE);
        }
        acc.on_intent(&slack(), Action::Restore, &Outcome::WouldHave, t(600));

        let r = acc.report(t(600), true);
        assert_eq!(r.apps.len(), 1);
        assert_eq!(r.total_dozed, Duration::from_secs(600));
        // 8% of a core for 600s.
        assert!((r.cpu_seconds - 48.0).abs() < 0.01, "{}", r.cpu_seconds);
        assert!((r.wakeups - 300_000.0).abs() < 1.0, "{}", r.wakeups);
        assert_eq!(r.apps[0].episodes, 1);
    }

    #[test]
    fn an_episode_the_intent_stream_never_closed_is_closed_by_the_held_set() {
        // The nine-day episode, in miniature. A freeze lands; the app is then let
        // go by a path that emits no restore — it exited, or the restore came
        // back `Skipped` — so nothing ever closes the episode. Before this,
        // `dozing_since` stayed set for the rest of the run: the row went on
        // accruing hold time the app never spent held, and everything it did
        // afterwards was charged as leakage against a hold that had ended.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 8.0, 500.0, t(10), true, MINUTE); // the baseline
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for m in 2..=6 {
            acc.on_sample(&slack(), 0.0, 0.0, t(m * 60), true, MINUTE); // frozen: silent
        }

        // The actuator is holding nothing, and no restore intent is coming.
        let closed = acc.reconcile_held(&BTreeSet::new(), t(360));
        assert_eq!(closed, vec![slack()], "it names the app it corrected");

        let at_close = acc.report(t(360), false).total_dozed;
        assert_eq!(at_close, Duration::from_secs(300), "held from t60 to t360, and no longer");

        // The app is running normally again. None of this is leakage and none of
        // it is hold time, which is exactly what used to go wrong.
        for m in 7..=20 {
            acc.on_sample(&slack(), 90.0, 9000.0, t(m * 60), true, MINUTE);
        }
        let r = acc.report(t(1200), false);
        assert_eq!(r.total_dozed, at_close, "the hold stopped growing when it ended");
        assert!(r.apps[0].leaked_cpu_seconds.abs() < f64::EPSILON, "{:?}", r.apps[0]);
        assert!(!r.apps[0].leaking(), "a closed episode cannot leak: {:?}", r.apps[0]);
    }

    /// A dry-run episode must survive reconciliation, or the mode that has to
    /// work before any of this is trusted stops reporting anything at all.
    #[test]
    fn reconciling_leaves_a_dry_run_episode_alone() {
        let mut acc = ReportAccumulator::new();
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::WouldHave, t(0));
        for m in 1..=10 {
            acc.on_sample(&slack(), 8.0, 500.0, t(m * 60), true, MINUTE);
        }
        // Nothing is held, because nothing is ever held in dry-run. The actuator
        // could not list this episode even in principle.
        assert!(acc.reconcile_held(&BTreeSet::new(), t(600)).is_empty());

        let r = acc.report(t(600), true);
        assert_eq!(r.total_dozed, Duration::from_secs(600), "still counted");
        assert!((r.cpu_seconds - 48.0).abs() < 0.01, "and still a saving: {}", r.cpu_seconds);
    }

    #[test]
    fn a_long_hold_is_not_flagged_by_residue_that_merely_accumulated() {
        // Firefox's shape, from a real report: 594 episodes and 6d21h of holding
        // had left 2 leaked CPU-seconds behind — 3ms an episode, 0.0018% of what
        // the same row claimed to have saved — and the absolute `> 1.0` floor
        // flagged it, because a total has no denominator and so is crossed by
        // patience rather than by severity.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 8.0, 500.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for n in 1..=10 {
            acc.on_sample(&slack(), 0.0015, 0.0, t(60 + n * 10_000), true, MINUTE);
        }

        let r = acc.report(t(60 + 100_000), false);
        let a = &r.apps[0];
        assert!(a.leaked_cpu_seconds > 1.0, "over the floor the old rule used: {a:?}");
        assert!(a.leak_cpu_fraction() < 0.000_1, "and nothing at all as a rate: {a:?}");
        assert!(!a.leaking(), "so it is a tail, not a leak: {a:?}");
    }

    #[test]
    fn a_hold_that_is_not_holding_is_still_flagged_on_cpu_alone() {
        // A GPU app's shape: 27% of a core burned straight through the hold, with
        // no wakeup signal needed to convict it. Making CPU a rate must not cost
        // the detection the total was there for.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 8.0, 500.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for n in 1..=10 {
            acc.on_sample(&slack(), 27.0, 0.0, t(60 + n * 600), true, MINUTE);
        }

        let r = acc.report(t(60 + 6_000), false);
        let a = &r.apps[0];
        assert!(a.leak_wakeup_rate() < 20.0, "it is not the wakeup axis convicting it: {a:?}");
        assert!(a.leaking(), "27% of a core while held is not a tail: {a:?}");
    }

    #[test]
    fn samples_while_awake_do_not_count() {
        // Only what the app burns *while it would have been frozen* is a saving.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 90.0, 9000.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::WouldHave, t(20));
        acc.on_sample(&slack(), 10.0, 100.0, t(30), true, MINUTE);
        acc.on_intent(&slack(), Action::Restore, &Outcome::WouldHave, t(30));
        acc.on_sample(&slack(), 90.0, 9000.0, t(40), true, MINUTE);

        let r = acc.report(t(40), true);
        assert!(
            (r.cpu_seconds - 1.0).abs() < 0.01,
            "only the 10s at 10% counts: {}",
            r.cpu_seconds
        );
    }

    #[test]
    fn a_live_freeze_that_holds_reports_the_baseline_it_avoided() {
        // In live mode the frozen app is quiet, so the observed figure is not the
        // saving — the saving is the rate it was running at before, projected
        // across the time it was held.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 8.0, 500.0, t(10), true, MINUTE); // awake: the baseline
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for m in 2..=11 {
            acc.on_sample(&slack(), 0.0, 0.0, t(m * 60), true, MINUTE); // frozen: silent
        }

        let r = acc.report(t(660), false);
        assert!((r.cpu_seconds - 48.0).abs() < 0.01, "8% of a core for 600s: {}", r.cpu_seconds);
        assert!((r.wakeups - 300_000.0).abs() < 1.0, "{}", r.wakeups);
        assert!(!r.apps[0].leaking(), "a freeze that holds must not be flagged");
        assert_eq!(r.apps[0].leaked_cpu_seconds, 0.0);
    }

    #[test]
    fn a_live_freeze_that_leaks_is_reported_as_leakage_not_as_a_saving() {
        // The bug, found after a 7h34m live night: `on_sample` credited samples
        // taken *while dozing* as avoided, so a freeze reaching 1 of 44 processes
        // was credited 1604 CPU-seconds while the app that really was frozen was
        // credited nothing. The number was inverted — the worse the freeze, the
        // better the report.
        let mut acc = ReportAccumulator::new();
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(0));
        for m in 1..=10 {
            acc.on_sample(&slack(), 6.0, 220.0, t(m * 60), true, MINUTE); // still running!
        }

        let r = acc.report(t(600), false);
        let app = &r.apps[0];
        assert_eq!(
            r.cpu_seconds, 0.0,
            "nothing was avoided: it was never seen running before the freeze, \
             and it did not stop after it"
        );
        assert!((app.leaked_cpu_seconds - 36.0).abs() < 0.01, "{}", app.leaked_cpu_seconds);
        assert!(app.leaking(), "and it must be called out: {app:?}");
        assert!(acc.report(t(600), false).render(None).contains("NOT HELD"));
        assert!(
            acc.report(t(600), false).render(None).contains("the freeze is not taking"),
            "the summary has to say it in words, not only as a column flag"
        );
    }

    #[test]
    fn dry_run_still_measures_what_the_app_actually_burned() {
        // The dry-run arithmetic must be untouched: nothing is held, so what the
        // app is observed doing *is* the waste a freeze would remove. Getting
        // this wrong would break the number the whole tool exists to produce.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 1.0, 10.0, t(10), true, MINUTE); // a baseline that must be ignored
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::WouldHave, t(60));
        for m in 2..=11 {
            acc.on_sample(&slack(), 8.0, 500.0, t(m * 60), true, MINUTE);
        }

        let r = acc.report(t(660), true);
        assert!(
            (r.cpu_seconds - 48.0).abs() < 0.01,
            "the observed 8%, not the 1% baseline: {}",
            r.cpu_seconds
        );
        assert_eq!(r.apps[0].leaked_cpu_seconds, 0.0, "nothing is held in dry-run");
        assert!(!r.apps[0].leaking());
    }

    #[test]
    fn a_throttled_app_that_keeps_running_is_not_leaking() {
        // Regression, and one this fix introduced: a throttled app is *meant* to
        // keep running — §7 caps a chat app precisely so it stays connected — but
        // the leak check treated every non-restore action as a freeze and told the
        // user "the freeze is not taking" about a correctly capped Signal. False
        // alarms are how a real alarm gets ignored.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 4.0, 100.0, t(10), true, MINUTE); // baseline before the cap
        acc.on_intent(&slack(), Action::Throttle { pct: 5 }, &Outcome::Applied, t(60));
        for m in 2..=11 {
            acc.on_sample(&slack(), 1.0, 40.0, t(m * 60), true, MINUTE); // capped, still alive
        }

        let r = acc.report(t(660), false);
        let app = &r.apps[0];
        assert_eq!(app.leaked_cpu_seconds, 0.0, "a cap is not a freeze: {app:?}");
        assert!(!app.leaking());
        assert!(!r.render(None).contains("NOT HELD"), "{}", r.render(None));

        // And it claims *nothing*, because a 5% cap cannot be what took a 4% app
        // down to 1%: it was never binding. Crediting the drop to the cap is the
        // over-claim measured live — 173 CPU-seconds for a 5% cap on a Signal
        // demanding 3.5%, which the cap never touched.
        assert_eq!(app.cpu_seconds, 0.0, "the cap was never binding: {app:?}");
        assert_eq!(app.wakeups, 0.0, "and a CPU quota does not reduce wakeups");
    }

    #[test]
    fn a_cap_that_binds_claims_only_the_demand_above_it() {
        // The other side: an app that genuinely wants more than the cap allows.
        // 30% demand against a 5% cap, observed down at the cap, is a real 25%
        // withheld — and no wakeup claim, because a quota preempts rather than
        // quiets and was measured *raising* context switches.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 30.0, 900.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Throttle { pct: 5 }, &Outcome::Applied, t(60));
        for m in 2..=11 {
            acc.on_sample(&slack(), 5.0, 950.0, t(m * 60), true, MINUTE);
        }

        let r = acc.report(t(660), false);
        assert!((r.cpu_seconds - 150.0).abs() < 0.01, "25% of 600s: {}", r.cpu_seconds);
        assert_eq!(r.wakeups, 0.0);
        assert!(!r.apps[0].leaking());
    }

    #[test]
    fn a_freeze_with_no_settled_baseline_claims_nothing() {
        // A lock-driven freeze acts with `grace 0s`, so the only sample before it
        // may still contain the user working in the app. Measured: a Signal that
        // sustains 3.5% hidden was credited against 5.4% captured a minute after
        // it lost focus. With no settled baseline there is nothing to project, and
        // under-claiming is the only honest answer.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 40.0, 2000.0, t(10), false, MINUTE); // foreground, unsettled
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for m in 2..=11 {
            acc.on_sample(&slack(), 0.0, 0.0, t(m * 60), true, MINUTE);
        }

        let r = acc.report(t(660), false);
        assert_eq!(r.cpu_seconds, 0.0, "an unsettled baseline is not a baseline");
        assert_eq!(r.wakeups, 0.0);
    }

    #[test]
    fn a_cap_that_does_not_cap_shows_up_as_a_saving_of_nothing() {
        // The throttle equivalent of leakage needs no separate concept: if the app
        // runs exactly as hard as before, the reduction is zero and says so.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 30.0, 900.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Throttle { pct: 5 }, &Outcome::Applied, t(60));
        for m in 2..=11 {
            acc.on_sample(&slack(), 30.0, 900.0, t(m * 60), true, MINUTE);
        }

        let r = acc.report(t(660), false);
        assert_eq!(r.cpu_seconds, 0.0, "no reduction, no saving");
        assert_eq!(r.wakeups, 0.0);
    }

    #[test]
    fn an_empty_report_does_not_print_negative_zero() {
        // `Sum` for f64 uses -0.0 as its identity, so an empty app list rendered
        // as "avoided ~-0 CPU-seconds". These quantities are non-negative by
        // construction; a minus sign on them is never information.
        let r = ReportAccumulator::new().report(t(0), false);
        let text = r.render(None);
        assert!(!text.contains("-0"), "{text}");
    }

    #[test]
    fn a_small_charge_saving_is_not_rounded_away_to_zero() {
        // 226 CPU-seconds against a 60Wh battery is 0.2-0.4%. At whole percent
        // that renders as "0-0% of a charge", which reads as "this did nothing"
        // for a saving that is real.
        let mut r = Report { cpu_seconds: 226.0, ..Report::default() };
        r.battery_wh = Some(60.0);
        let text = r.render(r.battery_wh);
        assert!(text.contains("estimated saving"), "{text}");
        assert!(!text.contains("0-0%"), "{text}");
    }

    #[test]
    fn the_charge_estimate_is_suppressed_on_mains() {
        // The CPU-seconds and wakeups are real either way — they were or were
        // not spent — but a fraction *of a charge* is not a meaningful unit
        // while the wall is paying for them.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 20.0, 500.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for at in [t(130), t(190), t(250)] {
            acc.on_sample(&slack(), 0.0, 0.0, at, true, MINUTE);
        }
        let mut r = acc.report(t(250), false);
        r.battery_wh = Some(60.0);

        r.on_ac = false;
        let on_battery = r.render(r.battery_wh);
        assert!(on_battery.contains("estimated saving"), "{on_battery}");

        r.on_ac = true;
        let plugged_in = r.render(r.battery_wh);
        assert!(!plugged_in.contains("estimated saving"), "{plugged_in}");
        // The measured numbers stay, because they happened.
        assert!(plugged_in.contains("CPU-seconds"), "{plugged_in}");
    }

    #[test]
    fn a_freeze_that_leaks_only_wakeups_is_still_caught() {
        // The bug this check exists for, in the shape that got past it twice.
        // Brave was frozen through a scope holding 2 of its 13 processes: the
        // frozen cgroup's `cpu.stat` read an honest zero while every context
        // switch came from the eleven processes outside it. A CPU-only verdict
        // passed it cleanly for seven hours.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 0.0, 250.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for at in [t(130), t(190), t(250), t(310), t(370)] {
            // Zero CPU throughout — the cgroup really is frozen — and a wakeup
            // rate that never budges, because most of the app is not in it.
            acc.on_sample(&slack(), 0.0, 250.0, at, true, MINUTE);
        }

        let r = acc.report(t(370), false);
        let app = &r.apps[0];
        assert_eq!(app.leaked_cpu_seconds, 0.0, "the frozen cgroup burned nothing: {app:?}");
        assert!(app.leaked_wakeups > 0.0, "but the escaped processes kept waking: {app:?}");
        assert!(app.leaking(), "and that has to be caught: {app:?}");
        assert!(r.render(None).contains("NOT HELD"), "{}", r.render(None));
    }

    #[test]
    fn a_short_episode_is_not_flagged_for_its_straddling_sample() {
        // Observed live: a Firefox frozen for 2m27s, verifiably silent from the
        // third sample onward, flagged NOT HELD because the one straddling sample
        // (2 CPU-s of genuine pre-freeze work) was measured against a saving that
        // had only had 147 seconds to accumulate. The straddle is bounded by one
        // sample window; the test has to be about *sustained* activity.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 11.4, 1050.0, t(10), true, MINUTE); // baseline
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        acc.on_sample(&slack(), 5.7, 614.0, t(90), true, MINUTE); // straddles the freeze
        acc.on_sample(&slack(), 0.0, 27.0, t(120), true, MINUTE);
        acc.on_sample(&slack(), 0.0, 0.0, t(150), true, MINUTE);
        acc.on_sample(&slack(), 0.0, 0.0, t(180), true, MINUTE);
        acc.on_sample(&slack(), 0.0, 0.0, t(207), true, MINUTE);

        let r = acc.report(t(207), false);
        let app = &r.apps[0];
        assert_eq!(
            app.leaked_cpu_seconds, 0.0,
            "a straddling sample is pre-freeze work and must not be counted at all: {app:?}"
        );
        assert!(!app.leaking());
        assert!(!r.render(None).contains("NOT HELD"), "{}", r.render(None));
    }

    #[test]
    fn a_working_freeze_is_never_flagged_however_long_it_holds() {
        // Scale invariance: the straddle is a one-off, the saving grows with the
        // episode, so a correct freeze must not drift into being flagged.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 11.4, 1050.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        acc.on_sample(&slack(), 5.7, 614.0, t(90), true, MINUTE); // the straddle
        for m in 2..=120 {
            acc.on_sample(&slack(), 0.0, 0.0, t(m * 60), true, MINUTE);
        }
        let r = acc.report(t(7200), false);
        assert!(!r.apps[0].leaking(), "two hours of a holding freeze: {:?}", r.apps[0]);
    }

    #[test]
    fn a_sample_straddling_the_freeze_is_not_called_a_leak() {
        // The sampler averages over a window, so the first sample after a freeze
        // legitimately carries pre-freeze activity. A warning that fires on every
        // successful freeze is a warning nobody reads.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 40.0, 2000.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        acc.on_sample(&slack(), 20.0, 1000.0, t(90), true, MINUTE); // straddles the freeze
        for m in 2..=20 {
            acc.on_sample(&slack(), 0.0, 0.0, t(m * 60), true, MINUTE);
        }

        let r = acc.report(t(1200), false);
        assert_eq!(r.apps[0].leaked_cpu_seconds, 0.0, "{:?}", r.apps[0]);
        assert!(!r.apps[0].leaking(), "a silent freeze is never a leak: {:?}", r.apps[0]);
    }

    #[test]
    fn many_short_episodes_do_not_accumulate_into_a_false_alarm() {
        // The third false alarm, and the one that showed a threshold could never
        // work: tuning browsers to doze every four minutes made Firefox freeze
        // fourteen times in ten hours, and fourteen straddles summed to 24 CPU-s
        // on an app whose freezes were verifiably silent — 0.00% on every sample
        // that had actually drained.
        let mut acc = ReportAccumulator::new();
        let mut clock = 0;
        for _ in 0..14 {
            acc.on_sample(&slack(), 11.0, 1000.0, t(clock + 10), true, MINUTE);
            acc.on_intent(
                &slack(),
                Action::Freeze { reclaim: false },
                &Outcome::Applied,
                t(clock + 60),
            );
            // A short episode: every sample in it straddles the freeze.
            acc.on_sample(&slack(), 5.5, 500.0, t(clock + 90), true, MINUTE);
            acc.on_intent(&slack(), Action::Restore, &Outcome::Applied, t(clock + 120));
            clock += 300;
        }

        let r = acc.report(t(clock), false);
        assert_eq!(r.apps[0].episodes, 14);
        assert_eq!(r.apps[0].leaked_cpu_seconds, 0.0, "{:?}", r.apps[0]);
        assert!(!r.render(None).contains("NOT HELD"), "{}", r.render(None));
    }

    #[test]
    fn a_leak_that_outlasts_the_window_is_still_caught() {
        // The detector still has to work: an app that keeps running well past one
        // averaging window is leaking, and that is what the whole mechanism is
        // for — it is what a `SIGSTOP` reaching 1 of 44 processes looks like.
        let mut acc = ReportAccumulator::new();
        acc.on_sample(&slack(), 11.0, 1000.0, t(10), true, MINUTE);
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(60));
        for m in 2..=20 {
            acc.on_sample(&slack(), 11.0, 1000.0, t(m * 60), true, MINUTE);
        }

        let r = acc.report(t(1200), false);
        assert!(r.apps[0].leaked_cpu_seconds > 100.0, "{:?}", r.apps[0]);
        assert!(r.apps[0].leaking());
        assert!(r.render(None).contains("the freeze is not taking"));
    }

    #[test]
    fn skipped_and_failed_intents_are_not_savings() {
        let mut acc = ReportAccumulator::new();
        acc.on_intent(
            &slack(),
            Action::Freeze { reclaim: false },
            &Outcome::Skipped { why: "vanished".into() },
            t(0),
        );
        acc.on_sample(&slack(), 50.0, 5000.0, t(60), true, MINUTE);
        let r = acc.report(t(60), true);
        assert!(r.apps.is_empty(), "nothing happened, so nothing was saved");
    }

    #[test]
    fn an_open_episode_counts_up_to_now() {
        let mut acc = ReportAccumulator::new();
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(0));
        let r = acc.report(t(300), false);
        assert_eq!(r.total_dozed, Duration::from_secs(300));
        assert!(!r.dry_run);
    }

    #[test]
    fn repeated_freezes_do_not_restart_the_clock() {
        let mut acc = ReportAccumulator::new();
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::Applied, t(0));
        acc.on_intent(&slack(), Action::Freeze { reclaim: true }, &Outcome::Applied, t(100));
        let r = acc.report(t(200), false);
        assert_eq!(r.total_dozed, Duration::from_secs(200));
        assert_eq!(r.apps[0].episodes, 1);
    }

    #[test]
    fn apps_are_ranked_by_what_they_cost() {
        let mut acc = ReportAccumulator::new();
        let discord = AppKey::flatpak("com.discordapp.Discord");
        for app in [&slack(), &discord] {
            acc.on_intent(app, Action::Freeze { reclaim: false }, &Outcome::WouldHave, t(0));
        }
        acc.on_sample(&slack(), 4.0, 100.0, t(100), true, MINUTE);
        acc.on_sample(&discord, 20.0, 900.0, t(100), true, MINUTE);

        let r = acc.report(t(100), true);
        assert_eq!(r.apps[0].app, discord, "the worst offender leads");
    }

    #[test]
    fn renders_the_design_summary() {
        let mut acc = ReportAccumulator::new();
        acc.on_intent(&slack(), Action::Freeze { reclaim: false }, &Outcome::WouldHave, t(0));
        acc.on_sample(&slack(), 10.0, 2000.0, t(3600), true, MINUTE);
        let text = acc.report(t(3600), true).render(Some(60.0));

        assert!(text.contains("would have"), "{text}");
        assert!(text.contains("held 1 app for a total of 1h"), "{text}");
        assert!(text.contains("CPU-seconds"), "{text}");
        assert!(text.contains("% of a charge"), "{text}");
        assert!(text.contains("com.slack.Slack"), "{text}");
        // One app cannot overlap itself, so the sum *is* the elapsed time and
        // the second line would only repeat it.
        assert!(!text.contains("apps overlap"), "{text}");
    }

    /// The bug this pair of fields exists for: six apps dozing at once through a
    /// five-hour session sum to twenty-three hours, and the summary printed that
    /// under "over the last 4h58m" with nothing to say it was app-time.
    #[test]
    fn concurrent_apps_sum_past_the_window_and_the_summary_says_so() {
        let mut acc = ReportAccumulator::new();
        let apps: Vec<AppKey> = (0..6).map(|n| AppKey::comm(format!("app{n}"))).collect();
        for app in &apps {
            acc.on_intent(app, Action::Freeze { reclaim: false }, &Outcome::Applied, t(0));
        }
        // One of them woken an hour before the end, so the total lands on the
        // 23h the live session actually printed rather than a round 24h.
        acc.on_intent(&apps[5], Action::Restore, &Outcome::Applied, t(3 * 3600));
        let r = acc.report(t(4 * 3600), false);

        assert_eq!(r.window, Duration::from_secs(4 * 3600));
        assert_eq!(r.total_dozed, Duration::from_secs(23 * 3600), "five apps at 4h, one at 3h");
        assert!(r.total_dozed > r.window, "the sum is allowed past the window; that is the point");
        assert_eq!(r.covered, r.window, "the other five kept something held throughout");

        let text = r.render(None);
        assert!(text.contains("a total of 23h of app-time"), "{text}");
        assert!(text.contains("held for 4h of that window"), "{text}");
    }

    /// The union is a union, not a sum: a gap with nothing held is not covered,
    /// and two apps overlapping across it are not counted twice.
    #[test]
    fn coverage_merges_overlapping_episodes_and_skips_the_gaps() {
        let mut acc = ReportAccumulator::new();
        let (a, b) = (AppKey::comm("a"), AppKey::comm("b"));
        let doze = Action::Freeze { reclaim: false };

        // a: [0,300)  b: [100,400)  — overlapping, so 400s covered, 600s summed.
        acc.on_intent(&a, doze, &Outcome::Applied, t(0));
        acc.on_intent(&b, doze, &Outcome::Applied, t(100));
        acc.on_intent(&a, Action::Restore, &Outcome::Applied, t(300));
        acc.on_intent(&b, Action::Restore, &Outcome::Applied, t(400));
        // A 200s gap with nothing held, then a: [600,700).
        acc.on_intent(&a, doze, &Outcome::Applied, t(600));
        acc.on_intent(&a, Action::Restore, &Outcome::Applied, t(700));

        let r = acc.report(t(700), false);
        assert_eq!(r.total_dozed, Duration::from_secs(700), "a's 300 + 100, plus b's 300");
        assert_eq!(r.covered, Duration::from_secs(500), "400 overlapping + 100, not the gap");
    }

    /// §7 caps a chat app so it *stays connected*, which is not dozing. Adding
    /// the two rungs into one headline overstated the stronger of them.
    #[test]
    fn a_throttle_is_held_but_reported_apart_from_a_freeze() {
        let mut acc = ReportAccumulator::new();
        let (chat, browser) = (AppKey::comm("signal"), AppKey::comm("firefox"));
        acc.on_intent(&chat, Action::Throttle { pct: 5 }, &Outcome::Applied, t(0));
        acc.on_intent(&browser, Action::Freeze { reclaim: false }, &Outcome::Applied, t(0));
        let r = acc.report(t(3600), false);

        let of = |k: &AppKey| r.apps.iter().find(|a| &a.app == k).expect("present").clone();
        assert_eq!(of(&chat).throttled, Duration::from_secs(3600), "open episode keeps its rung");
        assert_eq!(of(&browser).throttled, Duration::ZERO);

        let text = r.render(None);
        assert!(text.contains("1h frozen, 1h throttled"), "{text}");
    }

    #[test]
    fn journal_lines_read_as_counterfactual_in_dry_run() {
        let intent = Intent::new(
            slack(),
            Action::Freeze { reclaim: false },
            Reason::rule("bg-idle", "hidden for 5m"),
            t(42),
        );
        let would = JournalEntry::new(&intent, Outcome::WouldHave, 0);
        assert!(would.line().contains("would doze"), "{}", would.line());

        let did = JournalEntry::new(&intent, Outcome::Applied, 0);
        assert!(did.line().contains("dozed"), "{}", did.line());

        let skipped = JournalEntry::new(&intent, Outcome::Skipped { why: "gone".into() }, 0);
        assert!(skipped.line().contains("[skipped: gone]"), "{}", skipped.line());
    }

    #[test]
    fn charge_estimate_is_a_range_and_declines_to_guess() {
        let r = Report { cpu_seconds: 3600.0, ..Default::default() };
        let (lo, hi) = r.charge_fraction(60.0).unwrap();
        assert!(lo < hi, "must be a range, not a point");
        assert!(r.charge_fraction(0.0).is_none(), "no battery, no estimate");
        assert!(Report::default().charge_fraction(60.0).is_none(), "no saving, no estimate");
    }

    fn fit() -> crate::power_model::Fit {
        // 2 W a busy core, 20 µJ a wakeup, known to about 5% either way.
        crate::power_model::Fit {
            floor_w: 6.0,
            watts_per_core: 2.0,
            joules_per_wakeup: 20e-6,
            se_watts_per_core: 0.1,
            se_joules_per_wakeup: 1e-6,
            cov_core_wakeup: 0.0,
            r2: 0.86,
            samples: 90,
        }
    }

    /// §11.1's actual complaint, made into a test.
    ///
    /// "Wakeups enter it only through the CPU time they happen to cause, so the
    /// case the whole design is built around — a hidden app waking the CPU 900
    /// times a second while using almost no CPU — is the case the model
    /// undercounts most." Under the 2-4 J model an app that saved no CPU time at
    /// all saves *nothing*, whatever it did to the wakeup count. The fitted basis
    /// prices it, because it has a joules-per-wakeup to price it with.
    #[test]
    fn a_wakeup_bound_saving_is_priced_where_the_old_model_reported_nothing() {
        let wakeup_bound = Report { cpu_seconds: 0.0, wakeups: 900_000.0, ..Report::default() };
        assert!(
            wakeup_bound.charge_fraction(60.0).is_none(),
            "the 2-4 J model has nothing to price a wakeup with, and says so by declining"
        );

        let measured = Report { power_fit: Some(fit()), ..wakeup_bound };
        let (lo, hi) = measured.charge_fraction(60.0).expect("wakeups alone are a saving");
        // 900,000 × 20 µJ = 18 J, against a 60 Wh battery of 216,000 J.
        assert!((lo..=hi).contains(&(18.0 / 216_000.0 * 100.0)), "{lo}..{hi}");
        assert!(lo < hi, "still a range");
    }

    /// The range is the coefficients' own standard errors propagated through, not
    /// a margin chosen to look humble — which is what the 2-4 W basis was.
    #[test]
    fn the_fitted_range_comes_from_the_coefficients_own_uncertainty() {
        let r = Report {
            cpu_seconds: 100.0,
            wakeups: 0.0,
            power_fit: Some(fit()),
            ..Report::default()
        };
        let (lo, hi) = r.charge_fraction(60.0).unwrap();
        // 100 CPU-seconds × 2 W = 200 J, ±2 standard errors of 0.1 × 100 = ±20 J.
        let j = |pct: f64| pct / 100.0 * 216_000.0;
        assert!((j(lo) - 180.0).abs() < 1e-6, "{lo} -> {} J", j(lo));
        assert!((j(hi) - 220.0).abs() < 1e-6, "{hi} -> {} J", j(hi));

        // A fit that knows its coefficients exactly offers a point, not a
        // manufactured spread.
        let certain =
            crate::power_model::Fit { se_watts_per_core: 0.0, se_joules_per_wakeup: 0.0, ..fit() };
        let (lo, hi) = Report { power_fit: Some(certain), ..r }.charge_fraction(60.0).unwrap();
        assert!((lo - hi).abs() < 1e-12, "{lo} vs {hi}");
    }

    /// A number whose basis is not on screen cannot be argued with, and §11.1's
    /// objection to the old estimate was never only that it was wide.
    #[test]
    fn the_report_says_which_basis_produced_the_saving() {
        let modelled = Report { cpu_seconds: 226.0, battery_wh: Some(60.0), ..Report::default() };
        let text = modelled.render(Some(60.0));
        assert!(text.contains("2-4 J a CPU-second, wakeups not counted"), "{text}");

        let measured = Report { power_fit: Some(fit()), ..modelled };
        let text = measured.render(Some(60.0));
        assert!(text.contains("measured on this machine, wakeups included"), "{text}");
        assert!(!text.contains("2-4 J"), "one basis at a time: {text}");
    }

    /// Nothing held and nothing woken is nothing to estimate from, with or
    /// without coefficients — and a `0-0%` line reads as a tool that did nothing
    /// rather than as one that was not asked to.
    #[test]
    fn a_report_with_no_saving_offers_no_estimate_even_with_a_fit() {
        let empty = Report { power_fit: Some(fit()), ..Report::default() };
        assert!(empty.charge_fraction(60.0).is_none());
        assert!(
            Report { cpu_seconds: 226.0, power_fit: Some(fit()), ..Report::default() }
                .charge_fraction(0.0)
                .is_none(),
            "no battery, no fraction of one"
        );
    }
}
