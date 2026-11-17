//! The self-experiment: a randomised trial of dozed, run by dozed, on you.
//!
//! §1 asks this tool to be **provable** — "dry-run must produce a credible
//! estimate of what live mode would have saved, so the user can decide to switch
//! it on with evidence rather than faith". The counterfactual report does that
//! from a model: CPU-seconds times a guess at watts (`crate::journal`). This
//! module answers the same question from the battery instead, by running live
//! mode and dry-run mode alternately and comparing the discharge.
//!
//! ## Why an experiment and not just a before-and-after
//!
//! Because a before-and-after on a laptop measures the evening, not the tool. Two
//! hours of dry-run followed by two hours of live mode differ in what dozed was
//! doing *and* in the battery's state of charge, the room temperature, which tabs
//! were open, and whether the user was compiling something. Every one of those
//! moves discharge by more than dozed does.
//!
//! Three things make the comparison mean something, and each of them costs
//! nothing but arrangement:
//!
//! 1. **Blocks in pairs, order randomised inside each pair.** Anything drifting
//!    slowly — voltage sag, a background sync winding down — lands on both arms of
//!    a pair almost equally and differences away. Randomising which arm goes first
//!    is what licenses the sign-flip test in [`crate::stats`]; it is not a
//!    flourish.
//! 2. **Conditions gated, and a disturbed block discarded rather than repaired.**
//!    The trial only runs while the desktop is idle and (by default) on battery.
//!    A block during which the user came back is dropped whole. There is no
//!    honest way to subtract a person from a measurement.
//! 3. **A settling period after each mode flip.** Entering live mode has to
//!    actually freeze the apps dry-run was only pretending to freeze, and entering
//!    dry-run has to thaw them; the first minute of a block is therefore a
//!    transient and is not sampled.
//!
//! ## What it can and cannot tell you
//!
//! It measures **the idle desktop**, deliberately. That is the case dozed exists
//! for — a hidden Slack waking the CPU while nobody is looking — and it is the
//! only case where the signal is not buried. It says nothing about whether dozing
//! helps while you work, and [`Verdict::render`] says so rather than letting the
//! number imply otherwise.
//!
//! It is also allowed to come back with "no measurable difference". A tool that
//! can only produce good news is the thing this module was written to replace.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Mode;
use crate::energy::{Accumulated, Rates};
use crate::event::DesktopState;
use crate::stats::{Paired, Rng};
use crate::time::{HumanDuration, MonoTime};

/// Which mode a block runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arm {
    /// dozed acts.
    Live,
    /// dozed watches and touches nothing: the control.
    DryRun,
}

impl Arm {
    #[must_use]
    pub const fn mode(self) -> Mode {
        match self {
            Self::Live => Mode::Live,
            Self::DryRun => Mode::DryRun,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::DryRun => "dry-run",
        }
    }
}

/// Why a block does not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Discard {
    /// The machine was plugged in, so the battery was not paying.
    OnAc,
    /// The user came back. Input changes discharge by far more than dozing does.
    UserActive,
    /// We lost sight of focus, or the machine suspended: dozed goes passive
    /// there, so a "live" block would not have been live.
    NotObserving,
    /// Something else moved the mode mid-block — a `dozectl mode`, most likely.
    /// The arm stopped being the arm.
    ModeOverridden,
    /// Fewer readings than [`TrialPlan::min_samples`]. Usually a block cut short
    /// by the trial being stopped.
    TooFewSamples,
}

impl Discard {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnAc => "on mains",
            Self::UserActive => "user active",
            Self::NotObserving => "not observing",
            Self::ModeOverridden => "mode overridden",
            Self::TooFewSamples => "too few readings",
        }
    }
}

/// How a trial is set up.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrialPlan {
    /// How long each block runs.
    pub block: Duration,
    /// How many live/dry-run pairs to collect.
    ///
    /// **Six is a floor, not a preference.** The smallest two-sided p-value a
    /// sign-flip test can return from `n` pairs is `2 / 2^n` — only the observed
    /// arrangement and its mirror image are as extreme as itself — so the best
    /// possible result from four pairs is p = 0.125 and from five, p = 0.0625.
    /// Neither can clear 0.05 however clean the data is. Six pairs reaches
    /// 0.03125, and is therefore the shortest trial that is *capable* of
    /// concluding anything.
    ///
    /// This was found by writing a test with four pairs of immaculate data and
    /// watching [`Verdict::saving_is_real`] correctly refuse it.
    ///
    /// Six pairs is four hours of *suitable* conditions — idle, on battery —
    /// which is an evening rather than a day.
    pub pairs: u32,
    /// Fixes the randomisation. Recorded with the result so a verdict can be
    /// reproduced exactly.
    pub seed: u64,
    /// Dead time after each mode flip, during which readings are dropped.
    pub settle: Duration,
    /// Fewest readings a block may contribute and still count.
    pub min_samples: u32,
    /// Whether to insist on battery power.
    ///
    /// On by default because discharge is the headline number and mains makes it
    /// unmeasurable. Turning it off leaves deep-idle residency, which is a real
    /// measurement and the only one available on a desktop.
    pub on_battery_only: bool,
}

impl Default for TrialPlan {
    fn default() -> Self {
        Self {
            block: Duration::from_secs(20 * 60),
            pairs: 6,
            seed: 1,
            // A minute. Measured: `apply_live` walks the tree and writes
            // `cgroup.freeze` per app, and the sampler's own averaging window
            // (default 60s) still contains pre-flip activity for that long.
            settle: Duration::from_secs(60),
            min_samples: 3,
            on_battery_only: true,
        }
    }
}

impl TrialPlan {
    /// Most pairs a plan may ask for.
    ///
    /// A ceiling rather than a preference, and it is here because `pairs` arrives
    /// over IPC from whatever the client sent. The schedule is built eagerly —
    /// two [`Arm`]s per pair — and then **serialised to disk on every reading**,
    /// so the number is not merely a length: measured, `pairs = 50_000_000` (a
    /// plausible fat-finger, and well inside `u32`) builds a 100-million-element
    /// schedule and writes an **850 MB** `trial.json` per sample. Worse, the next
    /// start reads it back, so one mistyped command outlives the run that made it.
    ///
    /// 100 pairs at the default block is 33 hours of *suitable* conditions —
    /// idle, on battery — against the six pairs that are the shortest trial
    /// capable of concluding anything. Nobody wanting a real answer is past this,
    /// and anybody past this has typed something they did not mean.
    pub const MAX_PAIRS: u32 = 100;

    /// Shortest block that can contribute a reading.
    ///
    /// Below `settle` a block is entirely dead time and contributes nothing
    /// whatever the hardware does, so a trial of them would run to completion and
    /// report "too few readings" having measured the settling period.
    pub const MIN_BLOCK: Duration = Duration::from_secs(60);

    /// Longest block a plan may ask for. A day per block is already absurd; the
    /// bound exists so `duration()` cannot be asked to describe a geological era.
    pub const MAX_BLOCK: Duration = Duration::from_secs(24 * 60 * 60);

    /// Longest the trial can take, assuming conditions never disqualify a block.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.block * self.pairs * 2
    }

    /// Whether this plan is one worth laying out, and why not if it is not.
    ///
    /// Checked before [`Trial::new`] rather than clamped inside it: a trial is an
    /// experiment somebody will read a p-value off, so silently running a
    /// different one from the one they asked for is worse than refusing. The
    /// message goes back to the caller, so it has to say what to do instead.
    pub fn validate(&self) -> Result<(), String> {
        if self.pairs == 0 {
            // Not merely useless: an empty schedule is `finished()` on the first
            // look, so this would report a completed trial that never ran.
            return Err("a trial needs at least one pair; six is the fewest that can reach \
                        significance at all"
                .to_owned());
        }
        if self.pairs > Self::MAX_PAIRS {
            return Err(format!(
                "{} pairs is more than the {} this will lay out: the schedule is held in memory \
                 and written to disk on every reading, so a large one costs a large file rather \
                 than a better measurement. Six pairs is the floor for significance and {} is \
                 far past anything useful.",
                self.pairs,
                Self::MAX_PAIRS,
                Self::MAX_PAIRS,
            ));
        }
        if self.block < Self::MIN_BLOCK {
            return Err(format!(
                "a block of {:?} is shorter than the {:?} floor; the default 20m is what makes a \
                 block long enough to average over",
                self.block,
                Self::MIN_BLOCK,
            ));
        }
        if self.block > Self::MAX_BLOCK {
            return Err(format!(
                "a block of {:?} is longer than the {:?} ceiling",
                self.block,
                Self::MAX_BLOCK,
            ));
        }
        if self.block <= self.settle {
            return Err(format!(
                "a block of {:?} is not longer than the {:?} settling period, so every reading in \
                 it would be discarded and the trial would measure nothing",
                self.block, self.settle,
            ));
        }
        Ok(())
    }
}

/// The world, as far as the trial is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conditions {
    pub on_ac: bool,
    pub desktop: DesktopState,
    /// Whether dozed can see focus and is not mid-suspend. When false the
    /// planner holds nothing by design, so a live block would be live in name
    /// only.
    pub observing: bool,
    /// The mode the daemon is actually in right now.
    pub mode: Mode,
    /// The CPU package is pinned to a firmware power limit
    /// (`crate::power_model::Verdict::PowerLimited`).
    ///
    /// Not a reason to discard a block — a discharge measurement taken while the
    /// package is capped is still a perfectly good discharge measurement — but
    /// it is a reason the *package* verdict cannot be believed, and that is a
    /// different thing to say. At the limit, freezing an app frees budget for
    /// whatever else is runnable instead of lowering the draw, so package watts
    /// would be flat between the arms whatever dozing did, and a trial would
    /// report "no measurable difference" as a fact about dozed rather than about
    /// the chip.
    ///
    /// False on every machine that has not fitted a power model at all, which is
    /// the honest reading: nothing has been established either way, and the
    /// refusals in `crate::power_model` are what establish it.
    pub package_power_limited: bool,
}

impl Conditions {
    /// Whether a block may run. The first disqualifying condition wins, so the
    /// reason a block was dropped is the reason a reader would name.
    fn unsuitable(&self, plan: &TrialPlan) -> Option<Discard> {
        if plan.on_battery_only && self.on_ac {
            return Some(Discard::OnAc);
        }
        if self.desktop == DesktopState::Active {
            return Some(Discard::UserActive);
        }
        if !self.observing {
            return Some(Discard::NotObserving);
        }
        None
    }
}

/// One finished block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Which pair this belongs to, counting from zero.
    pub pair: u32,
    pub arm: Arm,
    pub samples: u32,
    pub discharge_w: Option<f64>,
    /// Mean CPU package watts, where RAPL was readable.
    ///
    /// Defaulted, so a `trial.json` written before this existed still loads —
    /// a trial in progress across an upgrade loses its package figures rather
    /// than failing to parse, and the discharge half is untouched.
    #[serde(default)]
    pub package_w: Option<f64>,
    pub deepest_idle: Option<f64>,
    pub system_wakeups_per_sec: Option<f64>,
    /// `None` means the block counts.
    pub discarded: Option<Discard>,
}

impl Block {
    #[must_use]
    pub fn counts(&self) -> bool {
        self.discarded.is_none()
    }
}

/// A block in progress. Not persisted: `MonoTime` is measured from the daemon's
/// own start, so `started` means nothing to the next process.
#[derive(Debug, Clone, PartialEq)]
struct Running {
    started: MonoTime,
    acc: Accumulated,
    /// The first thing that went wrong, if anything has.
    spoiled: Option<Discard>,
}

/// A trial in progress, or finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trial {
    pub plan: TrialPlan,
    /// The arm of every block, decided up front so the sequence is fixed by the
    /// seed and cannot drift with the conditions the trial happens to meet.
    schedule: Vec<Arm>,
    /// Index into `schedule` of the block now running or next to run.
    at: usize,
    /// The mode to return to when the trial ends — whatever the user had.
    baseline: Mode,
    blocks: Vec<Block>,
    /// Deliberately not serialised: see [`Running`]. A restart abandons the block
    /// it was in, which loses at most one block and never corrupts one.
    #[serde(skip)]
    running: Option<Running>,
    /// Whether [`Self::stop`] ended this trial rather than its schedule running
    /// out.
    ///
    /// Persisted, because the distinction outlives the process and is not
    /// recoverable from anything else that is. `stop` jumps `at` to the end of
    /// the schedule, so afterwards a stopped trial and a completed one are the
    /// same two numbers — and a `trial.json` reading `at: 12, blocks: []` was
    /// reported by `Verdict::render` as "trial complete: 0 of 12 blocks", which
    /// is the one thing it was not. Seen on a real machine: a trial that waited
    /// six hours for an idle desktop on battery, never got one, was stopped, and
    /// then read as a finished experiment with no result rather than as an
    /// abandoned one.
    ///
    /// Defaulted so a `trial.json` written before this existed still loads. Such
    /// a file replays as "completed", which is the reading it already had.
    #[serde(default)]
    stopped: bool,
    /// Set when a block closes, so the mode goes back to [`Self::baseline`]
    /// **once** rather than being demanded on every sample.
    ///
    /// This is the difference between "the trial owns the mode only while it is
    /// measuring" being a sentence in a design document and being true. See
    /// [`Self::desired_mode`] for the 40 hours it cost.
    ///
    /// Not persisted: a restart takes its mode from the config and the command
    /// line, so there is nothing outstanding for a new process to hand back.
    #[serde(skip)]
    hand_back: bool,
    /// Since when conditions have been continuously suitable, while no block is
    /// running. `None` means something is currently wrong.
    ///
    /// Not persisted: a restart has observed nothing yet, and inheriting a
    /// stretch of suitability it did not see would let the first sample after a
    /// restart start a block.
    #[serde(skip)]
    suitable_since: Option<MonoTime>,
    /// Whether the package was ever seen at its firmware power limit while this
    /// trial was measuring.
    ///
    /// Sticky, and that is the point: a package that spent part of the run
    /// capped produces package figures that are partly a share of a fixed
    /// allowance, and no later stretch of unconstrained readings makes the
    /// earlier ones mean what they appear to. Once it has been seen, the package
    /// verdict is withheld for the whole trial rather than for the blocks that
    /// happened to notice.
    #[serde(default)]
    saw_power_limit: bool,
}

impl Trial {
    /// Lay out a trial: one live and one dry-run block per pair, order inside
    /// each pair decided by the seed.
    ///
    /// Balanced by construction rather than by sampling each block
    /// independently, which is the difference between "six of each" and "eleven
    /// live blocks and one control, if the coin lands that way".
    #[must_use]
    pub fn new(plan: TrialPlan, baseline: Mode) -> Self {
        let mut rng = Rng::seeded(plan.seed);
        let mut schedule = Vec::with_capacity(plan.pairs as usize * 2);
        for _ in 0..plan.pairs {
            if rng.flip() {
                schedule.extend([Arm::Live, Arm::DryRun]);
            } else {
                schedule.extend([Arm::DryRun, Arm::Live]);
            }
        }
        Self {
            plan,
            schedule,
            at: 0,
            baseline,
            blocks: Vec::new(),
            stopped: false,
            running: None,
            hand_back: false,
            suitable_since: None,
            saw_power_limit: false,
        }
    }

    #[must_use]
    pub fn finished(&self) -> bool {
        self.at >= self.schedule.len()
    }

    /// The arm currently being measured, if a block is running.
    #[must_use]
    pub fn current_arm(&self) -> Option<Arm> {
        self.running.as_ref().and(self.schedule.get(self.at).copied())
    }

    /// Whether the trial is holding off because conditions do not qualify.
    #[must_use]
    pub fn waiting(&self) -> bool {
        !self.finished() && self.running.is_none()
    }

    /// What is stopping the next block from starting, right now.
    ///
    /// `None` when nothing is — a block is running, or the trial is over. The
    /// same test `on_sample` applies, exposed so the daemon can say *why* a
    /// trial has sat at 0/12 for a day rather than leaving the user to work it
    /// out from the plan and the power state.
    #[must_use]
    pub fn blocked_by(&self, cond: &Conditions) -> Option<Discard> {
        self.waiting().then(|| cond.unsuitable(&self.plan)).flatten()
    }

    /// The mode this trial will put back when it ends — whatever it found when
    /// it started.
    #[must_use]
    pub fn baseline(&self) -> Mode {
        self.baseline
    }

    /// How many blocks this trial will run in total.
    #[must_use]
    pub fn blocks_total(&self) -> usize {
        self.schedule.len()
    }

    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// What the daemon should do with the mode right now.
    ///
    /// Returns `None` when the mode is already right, so the caller can treat
    /// every `Some` as a change worth journalling.
    #[must_use]
    pub fn desired_mode(&self, cond: &Conditions) -> Option<Mode> {
        // Only a block that is *running* has any claim on the mode.
        //
        // This used to fall back to `baseline` whenever there was no current
        // arm, and a trial waiting for its conditions has no current arm — so a
        // trial that could not start a single block spent its whole life
        // demanding its baseline on every sample. Measured on a real machine:
        // `on_battery_only` with a laptop that is on mains 91% of the time, so
        // no block ever began, and the daemon was pinned to the dry-run baseline
        // for 19.5 hours and then 9.3 more across two restarts — freezing
        // nothing, while `dozectl mode live` was refused because "a trial is
        // driving the mode". The tool switched itself off to run a measurement
        // it was never going to take.
        //
        // Exactly the shape already fixed for *finished* trials one layer up
        // (`daemon::drive_trial`), which is the argument for fixing it here
        // instead: `finished` and `waiting` are two ways to have no current arm,
        // and a caller-side guard for one of them left the other open. Returning
        // `None` for both means the only thing that can move the mode is a block
        // that exists.
        //
        // Restoring the baseline is therefore an event, not a standing demand —
        // see [`Trial::baseline_to_restore`] and [`Trial::handing_back`].
        //
        // ## What "owns nothing" has to mean
        //
        // Returning `None` between blocks was read as "the trial is neutral
        // here". It is not: it leaves the mode wherever the last block put it,
        // which is the trial's leftover rather than the user's choice. Measured
        // on a real machine, from the trace and the journal: a **20-minute**
        // dry-run block ended and nothing put the mode back, so the daemon sat
        // in dry-run for **40 hours of monotonic runtime** — 59% of that run —
        // until the next block happened to start. 1,562 of its 1,877 intents
        // were `would-have` on a daemon started `--live`.
        //
        // It was invisible while it happened, which is what made it expensive.
        // `mode_driven_by_trial` is false between blocks because no arm is
        // current, and `configured_mode` cannot see it because a trial moves the
        // mode through `set_mode`, which rewrites the very field that check
        // compares against. `dozectl status` therefore showed a bare
        // `mode dry-run` with nothing anywhere to say why.
        //
        // So a block ending now hands the mode back through
        // [`Trial::handing_back`], and this function keeps answering only for a
        // block that exists.
        let want = self.current_arm()?.mode();
        (want != cond.mode).then_some(want)
    }

    /// The mode to put back now that a block has ended, asked once.
    ///
    /// Between blocks the trial is not measuring anything, so the mode belongs
    /// to whoever had it when the trial started. A one-shot rather than a
    /// standing demand: taking it once means a `dozectl mode` afterwards is not
    /// undone a few seconds later, which is exactly what the standing version of
    /// this cost before (§11.2).
    ///
    /// Returns `None` when nothing is outstanding, and clears the flag either
    /// way — a request made and found unnecessary is still a request answered.
    pub fn handing_back(&mut self, cond: &Conditions) -> Option<Mode> {
        if !std::mem::take(&mut self.hand_back) {
            return None;
        }
        // A block that ended and was immediately followed by another cannot be
        // handing anything back: the new arm owns the mode.
        if self.current_arm().is_some() {
            return None;
        }
        (self.baseline != cond.mode).then_some(self.baseline)
    }

    /// The mode to hand back now the trial is over, if it is over and the mode
    /// is not already there.
    ///
    /// Separate from [`Trial::desired_mode`] because it is a different kind of
    /// claim: that one is "a block is running and needs this mode", asked on
    /// every sample; this is "the experiment is done, put things back", asked
    /// once. Conflating them is what let a trial with nothing to measure hold
    /// the mode hostage.
    #[must_use]
    pub fn baseline_to_restore(&self, cond: &Conditions) -> Option<Mode> {
        (self.finished() && self.baseline != cond.mode).then_some(self.baseline)
    }

    /// Fold in one energy sample, rolling blocks over as their time expires.
    ///
    /// The caller passes the rates it already computed for `dozectl status`, so
    /// the trial costs no extra reads — which matters, because §13 forbids a
    /// measurement that pays for itself in wakeups.
    pub fn on_sample(&mut self, now: MonoTime, cond: &Conditions, rates: &Rates) {
        if self.finished() {
            return;
        }
        let unsuitable = cond.unsuitable(&self.plan);
        // Noted here rather than at the verdict, because by then the reading is
        // gone — and on every sample rather than only inside a running block,
        // because the sample that *starts* a block observes the condition just
        // as much as the ones after it. A block that will be discarded anyway
        // still counts: the question this answers is whether this machine's
        // package responds to load at all, and a spoiled block answers it as
        // well as a good one.
        self.saw_power_limit |= cond.package_power_limited;

        match &mut self.running {
            Some(run) => {
                let arm = self.schedule[self.at];
                // Spoil first, finish second: a block whose last sample arrived
                // after the user came back is spoiled, and rolling it over as
                // valid because its time happened to be up would be the one
                // failure mode nobody would ever notice.
                if run.spoiled.is_none() {
                    run.spoiled = unsuitable;
                }
                if run.spoiled.is_none() && cond.mode != arm.mode() {
                    run.spoiled = Some(Discard::ModeOverridden);
                }
                // A spoiled block is abandoned now rather than run to term.
                //
                // Spoiling is sticky by design — the first thing that went wrong
                // is the reason, and nothing later can make the block usable
                // again — so the remaining minutes buy exactly nothing and cost
                // the mode. Measured on a real machine: four blocks, four
                // discards, **zero** usable pairs, and 89% of an eight-hour
                // stretch spent in a trial-driven mode for a measurement that was
                // thrown away. Ending here turns each of those from twenty
                // minutes into one sample.
                //
                // It is still recorded, with its reason, because "the desktop was
                // in use" is what the user needs to see to understand why a trial
                // is getting nowhere.
                if run.spoiled.is_some() {
                    self.finish_block();
                    return;
                }
                // Readings from the settling period are transient by
                // construction: the freezes this block depends on have only just
                // been asked for.
                if now.since(run.started) >= self.plan.settle {
                    run.acc.push(rates);
                }
                if now.since(run.started) >= self.plan.block {
                    self.finish_block();
                }
            }
            // A block starts only once conditions have *held* for the settling
            // period, not on the first sample that happens to look right.
            //
            // This is what makes abandoning safe rather than frantic. Without it,
            // one idle moment starts a block, the next sample ends it, and the
            // one after starts another — the whole schedule burned in a couple of
            // minutes, each block costing two mode changes and a re-assert of
            // every held app. That is the thrash this project spends §8 avoiding,
            // arriving through the experiment meant to measure it.
            //
            // The settle period is the right length for free: it is already the
            // answer to "how long before a mode change means anything", and a
            // machine that cannot stay suitable for that long was never going to
            // produce a usable block.
            None => {
                if unsuitable.is_some() {
                    self.suitable_since = None;
                } else {
                    let since = *self.suitable_since.get_or_insert(now);
                    if now.since(since) >= self.plan.settle {
                        self.suitable_since = None;
                        self.running = Some(Running {
                            started: now,
                            acc: Accumulated::default(),
                            spoiled: None,
                        });
                    }
                }
            }
        }
    }

    /// Close the running block and advance.
    fn finish_block(&mut self) {
        let Some(run) = self.running.take() else { return };
        let arm = self.schedule[self.at];
        let samples = run.acc.samples();
        let discarded = run
            .spoiled
            .or_else(|| (samples < self.plan.min_samples).then_some(Discard::TooFewSamples));
        self.blocks.push(Block {
            pair: self.at as u32 / 2,
            arm,
            samples,
            discharge_w: run.acc.mean_discharge_w(),
            package_w: run.acc.mean_package_w(),
            deepest_idle: run.acc.mean_deepest_idle(),
            system_wakeups_per_sec: run.acc.mean_system_wakeups(),
            discarded,
        });
        self.at += 1;
        // The block is over, so the trial has stopped measuring and has no
        // further claim on the mode until the next one starts. Handing it back
        // is an *event* here rather than a standing demand from `desired_mode`,
        // which is what keeps this from becoming the 19.5-hour bug that section
        // describes in the other direction.
        self.hand_back = true;
    }

    /// End the trial early, keeping whatever complete blocks it has.
    ///
    /// The block in flight is closed and recorded rather than dropped silently:
    /// it will almost always fall to `TooFewSamples`, and a reader who stopped a
    /// trial deserves to see why the last block did not count.
    pub fn stop(&mut self) {
        if self.running.is_some() {
            self.finish_block();
        }
        self.at = self.schedule.len();
        self.stopped = true;
        // `finish_block` may not have run, and a stopped trial owes the mode
        // back either way. `Daemon::stop_trial` used to ask `desired_mode` for
        // this, which is `current_arm()?` — and `stop` has just made that `None`
        // by definition, so the answer was always `None` and stopping a trial
        // restored nothing at all. It left the daemon in whatever mode the last
        // block had set, which for a dry-run arm means switched off.
        self.hand_back = true;
    }

    /// The result so far.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        let mut discharge_pairs: Vec<(f64, f64)> = Vec::new();
        let mut package_pairs: Vec<(f64, f64)> = Vec::new();
        let mut idle_pairs: Vec<(f64, f64)> = Vec::new();
        let mut live_w = Vec::new();
        let mut dry_w = Vec::new();
        let mut pairs_discarded = 0;

        for pair in 0..self.plan.pairs {
            let of = |arm: Arm| {
                self.blocks.iter().find(|b| b.pair == pair && b.arm == arm && b.counts())
            };
            let (Some(live), Some(dry)) = (of(Arm::Live), of(Arm::DryRun)) else {
                // Only count a pair as discarded once both its blocks have been
                // attempted; the ones still in the future are not failures.
                if self.blocks.iter().filter(|b| b.pair == pair).count() == 2 {
                    pairs_discarded += 1;
                }
                continue;
            };
            if let (Some(l), Some(d)) = (live.discharge_w, dry.discharge_w) {
                // Dry-run minus live: positive is a saving. The convention is
                // stated in one place and tested, because getting it backwards
                // reports every saving as a cost and looks entirely plausible.
                discharge_pairs.push((d, l));
                live_w.push(l);
                dry_w.push(d);
            }
            // Same sign convention as discharge — dry-run minus live, positive
            // is a saving — and a separate list, because a machine can have
            // either counter, both or neither. Pairing them together would mean
            // a block with no battery reading silently discarding a perfectly
            // good package one.
            if let (Some(l), Some(d)) = (live.package_w, dry.package_w) {
                package_pairs.push((d, l));
            }
            if let (Some(l), Some(d)) = (live.deepest_idle, dry.deepest_idle) {
                // Live minus dry-run here: more deep idle is the *good*
                // direction, so this sign is the opposite of the one above and
                // has to be, or a real gain would render as a loss.
                idle_pairs.push((l, d));
            }
        }

        let discharge = Paired::from_pairs(&discharge_pairs);
        let package = Paired::from_pairs(&package_pairs);
        let idle = Paired::from_pairs(&idle_pairs);
        Verdict {
            plan: self.plan,
            blocks_done: self.blocks.len(),
            blocks_total: self.schedule.len(),
            pairs_used: discharge_pairs.len().max(package_pairs.len()).max(idle_pairs.len()),
            pairs_discarded,
            live_w: crate::stats::mean(&live_w),
            dry_run_w: crate::stats::mean(&dry_w),
            saving_w: discharge.mean(),
            saving_interval_w: discharge.bootstrap_interval(self.plan.seed, 0.95),
            saving_p: discharge.sign_flip_p(self.plan.seed),
            package_saving_w: package.mean(),
            package_interval_w: package.bootstrap_interval(self.plan.seed, 0.95),
            package_p: package.sign_flip_p(self.plan.seed),
            package_power_limited: self.saw_power_limit,
            deep_idle_gain: idle.mean(),
            deep_idle_p: idle.sign_flip_p(self.plan.seed),
            discharge_pairs: discharge_pairs.len(),
            package_pairs: package_pairs.len(),
            idle_pairs: idle_pairs.len(),
            finished: self.finished(),
            stopped: self.stopped,
        }
    }

    /// Whether this trial was ended by [`Self::stop`] rather than by its
    /// schedule running out.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stopped
    }
}

/// What the trial measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub plan: TrialPlan,
    pub blocks_done: usize,
    pub blocks_total: usize,
    pub pairs_used: usize,
    pub pairs_discarded: usize,
    /// Mean discharge in the live blocks, across usable pairs.
    pub live_w: Option<f64>,
    pub dry_run_w: Option<f64>,
    /// Dry-run minus live: watts saved, positive being a saving.
    pub saving_w: Option<f64>,
    pub saving_interval_w: Option<(f64, f64)>,
    pub saving_p: Option<f64>,
    /// The same arithmetic on **CPU package watts** rather than on battery
    /// discharge, where RAPL was readable.
    ///
    /// This is what lets a machine that is never unplugged prove anything at
    /// all. §1's premise is that the waste is the target and the battery is
    /// merely where it is easiest to notice — and until this existed, the
    /// desktops and docked laptops that premise is largely about were exactly
    /// the machines on which `dozectl trial` could never return a verdict,
    /// because `power_now` reads nothing on mains.
    ///
    /// It is a **different claim** from [`Self::saving_w`] and is rendered as
    /// one: the package is not the machine. A saving here is watts off the
    /// processor, which is where dozing acts, and it does not include whatever
    /// the display, the radios and the drives are doing.
    ///
    /// Defaulted, so a `trial.json` from before this existed still loads.
    #[serde(default)]
    pub package_saving_w: Option<f64>,
    #[serde(default)]
    pub package_interval_w: Option<(f64, f64)>,
    #[serde(default)]
    pub package_p: Option<f64>,
    /// The package was seen pinned at its firmware power limit during this
    /// trial, so [`Self::package_saving_w`] cannot be believed whatever it says.
    ///
    /// At the limit the chip holds its watts and moves its clocks underneath, so
    /// freezing an app hands the freed budget to whatever else is runnable
    /// rather than lowering the draw. Package watts are then flat between the
    /// arms no matter how well dozing worked, and reporting that as "no
    /// measurable difference" would be a statement about the silicon presented
    /// as one about the tool.
    #[serde(default)]
    pub package_power_limited: bool,
    /// Live minus dry-run in deep-idle residency, as a fraction. Positive is the
    /// good direction.
    pub deep_idle_gain: Option<f64>,
    pub deep_idle_p: Option<f64>,
    pub discharge_pairs: usize,
    #[serde(default)]
    pub package_pairs: usize,
    pub idle_pairs: usize,
    pub finished: bool,
    /// The trial was stopped rather than allowed to run its schedule out. See
    /// [`Trial::stopped`]. Defaulted so an older recorded verdict still loads.
    #[serde(default)]
    pub stopped: bool,
}

impl Verdict {
    /// Whether the discharge difference is one worth quoting.
    ///
    /// Two independent hurdles, and both have to clear. The interval must not
    /// span zero — if the data is consistent with dozing having made things
    /// *worse*, there is no saving to report — and the permutation test must
    /// agree at the conventional level. Either alone can be cleared by a small
    /// enough sample, which is precisely the situation this guards against.
    #[must_use]
    pub fn saving_is_real(&self) -> bool {
        let Some(saving) = self.saving_w else { return false };
        let Some((lo, hi)) = self.saving_interval_w else { return false };
        let Some(p) = self.saving_p else { return false };
        saving > 0.0 && lo > 0.0 && hi > 0.0 && p < 0.05
    }

    /// Whether the **package** difference is one worth quoting.
    ///
    /// The same two hurdles as [`Self::saving_is_real`], plus a third that has
    /// nothing to do with statistics: a package pinned at its firmware power
    /// limit cannot show a saving even when there is one, so a verdict from such
    /// a machine is withheld rather than reported as a null result. A confident
    /// "no measurable difference" derived from a chip that was physically unable
    /// to differ is the most misleading thing this module could produce.
    #[must_use]
    pub fn package_saving_is_real(&self) -> bool {
        if self.package_power_limited {
            return false;
        }
        let Some(saving) = self.package_saving_w else { return false };
        let Some((lo, hi)) = self.package_interval_w else { return false };
        let Some(p) = self.package_p else { return false };
        saving > 0.0 && lo > 0.0 && hi > 0.0 && p < 0.05
    }

    /// How much of a charge the measured saving is worth, given a battery size.
    ///
    /// The measured counterpart of `Report::charge_fraction`, and a much stronger
    /// claim: this is watts off the actual draw, extrapolated over an hour of the
    /// idle conditions the trial ran under. It is not a whole-day figure and
    /// [`Verdict::render`] does not present it as one.
    #[must_use]
    pub fn charge_fraction_per_hour(&self, battery_wh: f64) -> Option<f64> {
        if battery_wh <= 0.0 || !self.saving_is_real() {
            return None;
        }
        Some(self.saving_w? / battery_wh * 100.0)
    }

    #[must_use]
    pub fn render(&self, battery_wh: Option<f64>) -> String {
        let mut out = String::new();
        // Three states, not two. `stop` jumps the cursor to the end of the
        // schedule, so "finished" alone could not tell an experiment that ran to
        // completion from one that was abandoned — and the abandoned case is the
        // common one, because stopping a trial is what you do when it has been
        // waiting for conditions it is not going to get.
        let state = match (self.finished, self.stopped) {
            (_, true) => "trial stopped",
            (true, false) => "trial complete",
            (false, false) => "trial running",
        };
        out.push_str(&format!(
            "{state}: {} of {} blocks of {} each\n",
            self.blocks_done,
            self.blocks_total,
            HumanDuration(self.plan.block)
        ));

        if self.pairs_discarded > 0 {
            out.push_str(&format!(
                "  {} pair(s) discarded: the desktop was in use, or on mains\n",
                self.pairs_discarded
            ));
        }

        match (self.discharge_pairs, self.live_w, self.dry_run_w) {
            // Not "nothing measured" any more: a machine on mains has no
            // discharge and may still have a package counter, and saying
            // nothing was measured when something was is the failure this
            // section is otherwise entirely about.
            (0, ..) if self.package_pairs == 0 => {
                // "yet" is a promise, and a trial that is over cannot keep it.
                // A stopped trial with no blocks at all is the one case where
                // the reason is worth naming on the spot rather than leaving the
                // reader to infer it from a zero.
                if self.finished && self.blocks_done == 0 {
                    out.push_str(
                        "  nothing measured: no block ever started, so there is no result \
                         to report\n",
                    );
                } else if self.finished {
                    out.push_str("  no usable pairs — nothing measured\n");
                } else {
                    out.push_str("  no usable pairs yet — nothing measured\n");
                }
            }
            (0, ..) => {}
            (n, Some(live), Some(dry)) => {
                out.push_str(&format!(
                    "  discharge over {n} pair(s): {dry:.2} W idle, {live:.2} W dozing\n"
                ));
                if let (Some(saving), Some((lo, hi)), Some(p)) =
                    (self.saving_w, self.saving_interval_w, self.saving_p)
                {
                    if self.saving_is_real() {
                        out.push_str(&format!(
                            "  measured saving: {saving:.2} W (95% CI {lo:.2}-{hi:.2}, p={p:.3})\n"
                        ));
                        if let Some(pct) =
                            battery_wh.and_then(|wh| self.charge_fraction_per_hour(wh))
                        {
                            out.push_str(&format!("  = {pct:.1}% of a charge per idle hour\n"));
                        }
                    } else {
                        // The honest answer, and the reason this module exists:
                        // a difference the data cannot distinguish from zero is
                        // not a saving, however much anyone would like it to be.
                        out.push_str(&format!(
                            "  no measurable difference: {saving:+.2} W, \
                             95% CI {lo:.2}-{hi:.2}, p={p:.3}\n"
                        ));
                    }
                }
            }
            _ => {}
        }

        // The package arm, under its own heading and never folded into the line
        // above. The package is not the machine: this is watts off the
        // processor, which is where dozing acts, and it says nothing about the
        // display, the radios or the drives. Two figures that are not the same
        // quantity must not be able to be read as one.
        if self.package_pairs > 0 {
            if self.package_power_limited {
                // The refusal that matters most, because the number it withholds
                // would have looked entirely reasonable. At the firmware limit
                // the chip holds its watts and moves its clocks underneath, so
                // freezing an app hands the budget to whatever else is runnable
                // — package watts are flat between the arms however well dozing
                // worked, and "no measurable difference" would be a fact about
                // the silicon wearing the clothes of a fact about dozed.
                out.push_str(&format!(
                    "  cpu package over {} pair(s): no verdict — the package was at its \n                         firmware power limit, where freezing an app frees budget for whatever \n                         else is runnable instead of lowering the draw\n",
                    self.package_pairs
                ));
            } else if let (Some(saving), Some((lo, hi)), Some(p)) =
                (self.package_saving_w, self.package_interval_w, self.package_p)
            {
                if self.package_saving_is_real() {
                    out.push_str(&format!(
                        "  cpu package saving over {} pair(s): {saving:.2} W \
                         (95% CI {lo:.2}-{hi:.2}, p={p:.3})\n",
                        self.package_pairs
                    ));
                } else {
                    out.push_str(&format!(
                        "  cpu package over {} pair(s): no measurable difference: \
                         {saving:+.2} W, 95% CI {lo:.2}-{hi:.2}, p={p:.3}\n",
                        self.package_pairs
                    ));
                }
                out.push_str(
                    "    that is the processor's own draw, not the whole machine's — \n                         it is the figure a machine on mains can have, where discharge is \n                         unmeasurable\n",
                );
            }
        }

        if let (Some(gain), Some(p)) = (self.deep_idle_gain, self.deep_idle_p) {
            out.push_str(&format!(
                "  deep idle over {} pair(s): {:+.1} points while dozing (p={p:.3})\n",
                self.idle_pairs,
                gain * 100.0
            ));
        }

        out.push_str("  measured with the desktop idle, which is the case dozing is for\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE_ON_BATTERY: Conditions = Conditions {
        on_ac: false,
        desktop: DesktopState::IdlePresent,
        observing: true,
        mode: Mode::DryRun,
        package_power_limited: false,
    };

    fn plan() -> TrialPlan {
        TrialPlan {
            block: Duration::from_secs(600),
            pairs: 4,
            seed: 42,
            settle: Duration::from_secs(60),
            min_samples: 2,
            on_battery_only: true,
        }
    }

    /// `pairs` and `block` arrive over IPC from whatever the client sent, and the
    /// schedule is laid out eagerly and then serialised on every reading.
    /// Measured before this bound existed: `pairs = 50_000_000` — inside `u32`,
    /// and a plausible mistype — built a 100-million-element schedule and wrote
    /// an 850 MB `trial.json` per sample, which the next start then read back.
    #[test]
    fn a_plan_nobody_could_want_is_refused_rather_than_laid_out() {
        assert!(TrialPlan::default().validate().is_ok(), "the shipped plan must pass");
        assert!(plan().validate().is_ok());

        let huge = TrialPlan { pairs: 50_000_000, ..plan() };
        assert!(huge.validate().is_err(), "the measured case");
        assert!(TrialPlan { pairs: TrialPlan::MAX_PAIRS, ..plan() }.validate().is_ok());
        assert!(TrialPlan { pairs: TrialPlan::MAX_PAIRS + 1, ..plan() }.validate().is_err());

        // Zero is not merely useless: an empty schedule is `finished()` on the
        // first look, so it reports a completed trial that never ran.
        let none = TrialPlan { pairs: 0, ..plan() };
        assert!(none.validate().is_err());
        assert!(Trial::new(none, Mode::DryRun).finished(), "which is why it must not be laid out");

        // A block inside its own settling period discards every reading it takes,
        // so it would run to completion and report having measured nothing.
        assert!(TrialPlan { block: Duration::from_secs(30), ..plan() }.validate().is_err());
        assert!(
            TrialPlan { block: Duration::from_secs(60), settle: Duration::from_secs(60), ..plan() }
                .validate()
                .is_err(),
            "equal is no better than shorter: nothing survives the settle"
        );
        assert!(
            TrialPlan { block: TrialPlan::MAX_BLOCK + Duration::from_secs(1), ..plan() }
                .validate()
                .is_err()
        );
    }

    /// The reason for a ceiling at all, stated as arithmetic rather than as a
    /// comment: what a plan costs is set by the schedule it builds, and the
    /// schedule is written to disk on every reading.
    #[test]
    fn the_worst_permitted_schedule_is_a_size_worth_writing_repeatedly() {
        let worst = TrialPlan {
            pairs: TrialPlan::MAX_PAIRS,
            block: TrialPlan::MAX_BLOCK,
            ..TrialPlan::default()
        };
        worst.validate().expect("the extremes of the permitted range must themselves be permitted");
        let trial = Trial::new(worst, Mode::DryRun);
        let json = serde_json::to_string(&trial).expect("a plain struct serialises");
        assert!(json.len() < 8 * 1024, "the persisted worst case is {} bytes", json.len());
    }

    /// Rates that look like a machine drawing `w` watts and `idle` deep-idle.
    fn rates(w: f64, idle: f64) -> Rates {
        rates_full(Some(w), None, idle)
    }

    /// The same, with either power figure independently present or absent.
    ///
    /// Both, because a machine can have either counter, both or neither: a
    /// laptop on battery with no readable RAPL has discharge alone, a desktop
    /// with the udev rule applied has the package alone, and a plugged-in
    /// laptop with both has both.
    fn rates_full(w: Option<f64>, package_w: Option<f64>, idle: f64) -> Rates {
        Rates {
            discharge_w: w,
            package_w,
            deepest_idle: Some(idle),
            idle: Some(idle),
            system_wakeups_per_sec: Some(1_000.0),
            idle_entries_per_sec: Some(500.0),
            // Carried past the verdict untouched: the sub-domains are inside
            // the package figure, never beside it, so they are not a second
            // measurement to judge on.
            package_parts: crate::energy::PackageBreakdown::default(),
            gpu_w: None,
            gpu_busy: None,
            machine_cores_busy: Some(1.5),
        }
    }

    /// Sample until a block is actually running, and return when it started.
    ///
    /// A block now begins only once conditions have *held* for the settle
    /// period, so a test that wants one running has to let that time pass. That
    /// is the behaviour rather than an inconvenience: a machine that cannot stay
    /// suitable for a minute was never going to produce a usable block, and
    /// starting one on the first agreeable sample is what let four consecutive
    /// blocks be discarded on a real machine.
    fn start_a_block(trial: &mut Trial, cond: &Conditions, from: MonoTime) -> MonoTime {
        let mut now = from;
        for _ in 0..64 {
            trial.on_sample(now, cond, &rates(6.0, 0.8));
            if trial.current_arm().is_some() {
                return now;
            }
            now = now.plus(Duration::from_secs(30));
        }
        panic!("no block started within 32 minutes of suitable conditions");
    }

    /// Drive a whole trial. `draw` decides the watts for an arm, so a test can
    /// describe a machine where dozing helps, or one where it does nothing.
    fn run(plan: TrialPlan, mut cond: Conditions, draw: impl Fn(Arm, u32) -> (f64, f64)) -> Trial {
        let mut trial = Trial::new(plan, Mode::DryRun);
        let step = Duration::from_secs(60);
        let mut now = MonoTime::ZERO;
        // Generously more ticks than the trial needs, so a bug that stalls it
        // shows up as an unfinished trial rather than as a hang.
        for _ in 0..(plan.pairs * 2 * 20) {
            // The daemon applies the requested mode before the next sample, which
            // is what the loop below stands in for.
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            let arm = trial.current_arm().unwrap_or(Arm::DryRun);
            let pair = trial.blocks().len() as u32 / 2;
            let (w, idle) = draw(arm, pair);
            trial.on_sample(now, &cond, &rates(w, idle));
            now = now.plus(step);
        }
        trial
    }

    /// Drive a trial on a machine that is plugged in: no discharge at all, and a
    /// package counter instead. `draw` gives the package watts for an arm.
    fn run_on_mains(
        plan: TrialPlan,
        mut cond: Conditions,
        draw: impl Fn(Arm, u32) -> f64,
    ) -> Trial {
        let mut trial = Trial::new(plan, Mode::DryRun);
        let step = Duration::from_secs(60);
        let mut now = MonoTime::ZERO;
        for _ in 0..(plan.pairs * 2 * 20) {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            let arm = trial.current_arm().unwrap_or(Arm::DryRun);
            let pair = trial.blocks().len() as u32 / 2;
            trial.on_sample(now, &cond, &rates_full(None, Some(draw(arm, pair)), 0.8));
            now = now.plus(step);
        }
        trial
    }

    /// A desktop, or a docked laptop: on mains, and told to measure anyway.
    const IDLE_ON_MAINS: Conditions = Conditions {
        on_ac: true,
        desktop: DesktopState::IdlePresent,
        observing: true,
        mode: Mode::DryRun,
        package_power_limited: false,
    };

    #[test]
    fn a_machine_that_is_never_unplugged_can_still_reach_a_verdict() {
        // The gap this arm closes. §1's premise is that the waste is the target
        // and the battery is only where it is easiest to notice — and the
        // desktops and docked laptops that premise is mostly about were exactly
        // the machines `dozectl trial` could never answer for, because
        // `power_now` reads nothing on mains. The package counter can.
        let plan = TrialPlan { pairs: 6, on_battery_only: false, ..plan() };
        let trial = run_on_mains(plan, IDLE_ON_MAINS, |arm, pair| {
            // A real saving, plus a slow drift that lands on both arms of a pair
            // and differences away — which is what the pairing is for.
            let base = if arm == Arm::Live { 9.0 } else { 12.0 };
            base + f64::from(pair) * 0.05
        });

        let v = trial.verdict();
        assert_eq!(v.discharge_pairs, 0, "there is no battery reading on mains");
        assert_eq!(v.package_pairs, 6);
        assert!(v.package_saving_is_real(), "{v:?}");
        assert!((v.package_saving_w.unwrap() - 3.0).abs() < 0.5, "{:?}", v.package_saving_w);

        // And it is rendered as a different claim from a discharge saving,
        // because the package is not the machine.
        let text = v.render(None);
        assert!(text.contains("cpu package saving"), "{text}");
        assert!(text.contains("not the whole machine"), "{text}");
        assert!(
            !text.contains("no usable pairs yet"),
            "saying nothing was measured when something was is the failure this \
             whole module is about:\n{text}"
        );
    }

    #[test]
    fn a_package_at_its_power_limit_gets_no_verdict_rather_than_a_null_one() {
        // The refusal that matters most, because the number it withholds looks
        // entirely reasonable. At the firmware limit the chip holds its watts and
        // moves its clocks underneath, so freezing an app hands the freed budget
        // to whatever else is runnable rather than lowering the draw. Package
        // watts are then flat between the arms however well dozing worked, and
        // reporting that as "no measurable difference" would be a statement about
        // the silicon presented as one about dozed.
        let plan = TrialPlan { pairs: 6, on_battery_only: false, ..plan() };
        let capped = Conditions { package_power_limited: true, ..IDLE_ON_MAINS };
        // 24.2 W whatever the arm — the measured signature of this machine.
        let trial = run_on_mains(plan, capped, |_, _| 24.2);

        let v = trial.verdict();
        assert!(v.package_power_limited);
        assert!(!v.package_saving_is_real());
        let text = v.render(None);
        assert!(text.contains("no verdict"), "{text}");
        assert!(text.contains("firmware power limit"), "{text}");
        assert!(
            !text.contains("no measurable difference"),
            "a chip that could not differ must not be reported as having been \
             measured and found equal:\n{text}"
        );
    }

    #[test]
    fn seeing_the_limit_once_withholds_the_verdict_for_the_whole_trial() {
        // Sticky on purpose. A package that spent part of the run capped
        // produces figures that are partly a share of a fixed allowance, and no
        // later stretch of unconstrained readings makes the earlier ones mean
        // what they appear to.
        let plan = TrialPlan { pairs: 6, on_battery_only: false, ..plan() };
        let mut trial = Trial::new(plan, Mode::DryRun);
        let mut cond = Conditions { package_power_limited: true, ..IDLE_ON_MAINS };
        let mut now = MonoTime::ZERO;

        // One sample under the cap, then it lifts for the rest of the run.
        if let Some(mode) = trial.desired_mode(&cond) {
            cond.mode = mode;
        }
        trial.on_sample(now, &cond, &rates_full(None, Some(24.2), 0.8));
        cond.package_power_limited = false;

        for _ in 0..(plan.pairs * 2 * 20) {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            let arm = trial.current_arm().unwrap_or(Arm::DryRun);
            now = now.plus(Duration::from_secs(60));
            let w = if arm == Arm::Live { 9.0 } else { 12.0 };
            trial.on_sample(now, &cond, &rates_full(None, Some(w), 0.8));
        }

        let v = trial.verdict();
        assert!(v.package_pairs > 0, "there is data; it is the data that cannot be believed");
        assert!(v.package_power_limited, "one capped reading taints the run");
        assert!(!v.package_saving_is_real());
    }

    #[test]
    fn a_battery_trial_is_unchanged_by_the_package_arm() {
        // The discharge verdict is the primary one and must not have moved. A
        // machine with both counters reports both, and the battery figure is
        // still the one `saving_is_real` speaks for.
        let plan = TrialPlan { pairs: 6, ..plan() };
        let mut trial = Trial::new(plan, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        let mut now = MonoTime::ZERO;
        for _ in 0..(plan.pairs * 2 * 20) {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            let arm = trial.current_arm().unwrap_or(Arm::DryRun);
            let (w, p) = if arm == Arm::Live { (5.0, 9.0) } else { (7.0, 12.0) };
            trial.on_sample(now, &cond, &rates_full(Some(w), Some(p), 0.8));
            now = now.plus(Duration::from_secs(60));
        }

        let v = trial.verdict();
        assert!(v.saving_is_real(), "the discharge verdict still stands on its own");
        assert!((v.saving_w.unwrap() - 2.0).abs() < 0.5);
        assert!(v.package_saving_is_real(), "and the package one is reported beside it");
        let text = v.render(None);
        assert!(text.contains("measured saving"), "{text}");
        assert!(text.contains("cpu package saving"), "{text}");
    }

    #[test]
    fn every_pair_gets_one_block_of_each_arm() {
        // The whole design rests on this: a pair is a matched comparison, and an
        // independent coin per block would sometimes produce a pair of two live
        // blocks, which measures nothing at all.
        for seed in 0..20 {
            let mut p = plan();
            p.seed = seed;
            let trial = Trial::new(p, Mode::DryRun);
            assert_eq!(trial.schedule.len(), p.pairs as usize * 2);
            for pair in trial.schedule.chunks(2) {
                assert_ne!(pair[0], pair[1], "seed {seed} produced an unmatched pair");
            }
        }
    }

    #[test]
    fn the_order_within_a_pair_is_randomised_but_reproducible() {
        // Randomised, because the sign-flip test in `stats` is licensed by it and
        // by nothing else. Reproducible, because a verdict has to be checkable.
        let orders: Vec<Vec<Arm>> = (0..8)
            .map(|seed| {
                let mut p = plan();
                p.seed = seed;
                Trial::new(p, Mode::DryRun).schedule
            })
            .collect();
        assert!(orders.iter().any(|o| o[0] == Arm::Live), "some trial must start live");
        assert!(orders.iter().any(|o| o[0] == Arm::DryRun), "and some must start dry-run");

        let a = Trial::new(plan(), Mode::DryRun);
        let b = Trial::new(plan(), Mode::DryRun);
        assert_eq!(a.schedule, b.schedule, "the same seed must lay out the same trial");
    }

    #[test]
    fn a_real_saving_is_found_and_quantified() {
        // 6 W idle, 5 W while dozing, with a little noise so it is not a
        // degenerate case.
        //
        // Six pairs, not the four the rest of these tests use, and that is the
        // point: the smallest p-value four pairs can produce is 2/2^4 = 0.125, so
        // immaculate data still cannot clear 0.05. This test was written with four
        // and failed, which is how `TrialPlan::pairs` came to document a floor.
        let mut p = plan();
        p.pairs = 6;
        let trial = run(p, IDLE_ON_BATTERY, |arm, pair| {
            let jitter = f64::from(pair) * 0.05;
            match arm {
                Arm::DryRun => (6.0 + jitter, 0.70),
                Arm::Live => (5.0 + jitter, 0.85),
            }
        });
        assert!(trial.finished(), "the trial must actually complete: {:?}", trial.blocks());

        let v = trial.verdict();
        assert_eq!(v.discharge_pairs, 6, "{:?}", trial.blocks());
        assert_eq!(v.pairs_discarded, 0);
        assert!((v.saving_w.unwrap() - 1.0).abs() < 1e-9, "{:?}", v.saving_w);
        assert!(v.saving_is_real(), "{v:?}");

        let text = v.render(Some(60.0));
        assert!(text.contains("measured saving"), "{text}");
        assert!(text.contains("of a charge per idle hour"), "{text}");
        // Deep idle moved the other way, and its sign must read as a gain.
        assert!(v.deep_idle_gain.unwrap() > 0.0, "{:?}", v.deep_idle_gain);
        assert!(text.contains("+15.0 points"), "{text}");
    }

    #[test]
    fn a_tool_that_does_nothing_is_reported_as_doing_nothing() {
        // The result this module exists to be capable of producing. Both arms
        // draw the same, with noise that happens to favour each arm in turn.
        let trial = run(plan(), IDLE_ON_BATTERY, |arm, pair| {
            let bump = if pair % 2 == 0 { 0.4 } else { -0.4 };
            match arm {
                Arm::DryRun => (6.0 + bump, 0.80),
                Arm::Live => (6.0, 0.80),
            }
        });
        let v = trial.verdict();
        assert!(!v.saving_is_real(), "{v:?}");
        let text = v.render(Some(60.0));
        assert!(text.contains("no measurable difference"), "{text}");
        assert!(!text.contains("measured saving"), "{text}");
    }

    #[test]
    fn a_saving_that_points_the_wrong_way_is_never_reported_as_a_saving() {
        // If live mode costs more — a plausible outcome for a machine whose apps
        // reconnect noisily on every thaw — the sign must survive to the reader.
        let trial = run(plan(), IDLE_ON_BATTERY, |arm, _| match arm {
            Arm::DryRun => (5.0, 0.80),
            Arm::Live => (6.0, 0.80),
        });
        let v = trial.verdict();
        assert!(v.saving_w.unwrap() < 0.0);
        assert!(!v.saving_is_real(), "a cost is not a saving: {v:?}");
        assert!(v.render(None).contains("-1.00 W"), "{}", v.render(None));
    }

    #[test]
    fn the_trial_holds_off_while_the_machine_is_plugged_in() {
        let mut cond = IDLE_ON_BATTERY;
        cond.on_ac = true;
        let mut trial = Trial::new(plan(), Mode::DryRun);
        let mut now = MonoTime::ZERO;
        for _ in 0..60 {
            trial.on_sample(now, &cond, &rates(0.0, 0.9));
            now = now.plus(Duration::from_secs(60));
        }
        assert!(trial.waiting(), "no block may start on mains");
        assert!(trial.blocks().is_empty());
        assert_eq!(trial.desired_mode(&cond), None, "and the mode is left alone");
    }

    #[test]
    fn a_block_the_user_walked_into_is_discarded_whole() {
        // There is no honest way to subtract a person from a measurement, so the
        // block goes. Ten minutes lost beats a number nobody should trust.
        let mut trial = Trial::new(plan(), Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        let mut now = MonoTime::ZERO;
        for i in 0..11 {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            // Seven minutes in, the user comes back for one sample.
            cond.desktop = if i == 7 { DesktopState::Active } else { DesktopState::IdlePresent };
            trial.on_sample(now, &cond, &rates(6.0, 0.8));
            now = now.plus(Duration::from_secs(60));
        }
        let first = &trial.blocks()[0];
        assert_eq!(first.discarded, Some(Discard::UserActive), "{first:?}");
        assert!(!first.counts());
    }

    #[test]
    fn a_disturbance_in_the_last_sample_still_spoils_the_block() {
        // The ordering trap: the block's time is up on the same sample that
        // reports the user is back, and finishing before spoiling would file it
        // as a good block. Nobody would ever see it.
        let mut p = plan();
        p.block = Duration::from_secs(120);
        p.settle = Duration::ZERO;
        p.min_samples = 1;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = trial.desired_mode(&cond).unwrap_or(cond.mode);

        trial.on_sample(MonoTime::ZERO, &cond, &rates(6.0, 0.8));
        cond.desktop = DesktopState::Active;
        trial.on_sample(MonoTime::ZERO.plus(Duration::from_secs(120)), &cond, &rates(6.0, 0.8));

        assert_eq!(trial.blocks().len(), 1);
        assert_eq!(trial.blocks()[0].discarded, Some(Discard::UserActive));
    }

    #[test]
    fn readings_from_the_settling_period_do_not_count() {
        // Entering live mode has to actually freeze what dry-run was pretending
        // to freeze; sampling during that transient measures the changeover.
        let mut p = plan();
        p.settle = Duration::from_secs(300);
        p.min_samples = 1;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        let mut now = MonoTime::ZERO;
        // Long enough to cover the wait for conditions to hold *and* the block
        // itself: the settle period is now spent twice, once before the block
        // begins and once inside it.
        for _ in 0..=15 {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            trial.on_sample(now, &cond, &rates(6.0, 0.8));
            now = now.plus(Duration::from_secs(60));
        }
        // Conditions have to hold for five minutes before the block starts, so it
        // runs from 5 minutes to 15. Its own first five minutes are settling, so
        // the readings at 10, 11, 12, 13, 14 and 15 minutes count.
        assert_eq!(trial.blocks()[0].samples, 6, "{:?}", trial.blocks());
    }

    #[test]
    fn a_mode_change_from_outside_invalidates_the_block() {
        // Someone runs `dozectl mode dry-run` during a live block. The arm has
        // stopped being the arm, and pretending otherwise silently averages the
        // two conditions together.
        let mut p = plan();
        p.settle = Duration::ZERO;
        p.min_samples = 1;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        let mut now = MonoTime::ZERO;
        for i in 0..11 {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            if i == 4 {
                // The user's own hand on the wheel.
                cond.mode = match cond.mode {
                    Mode::Live => Mode::DryRun,
                    _ => Mode::Live,
                };
            }
            trial.on_sample(now, &cond, &rates(6.0, 0.8));
            now = now.plus(Duration::from_secs(60));
        }
        assert_eq!(trial.blocks()[0].discarded, Some(Discard::ModeOverridden));
    }

    #[test]
    fn a_block_with_too_few_readings_does_not_count() {
        let mut p = plan();
        p.min_samples = 50;
        p.settle = Duration::ZERO;
        let trial = run(p, IDLE_ON_BATTERY, |_, _| (6.0, 0.8));
        assert!(trial.blocks().iter().all(|b| b.discarded == Some(Discard::TooFewSamples)));
        let v = trial.verdict();
        assert_eq!(v.discharge_pairs, 0);
        assert!(v.pairs_discarded > 0, "and the reader is told the pairs were lost: {v:?}");
        assert!(v.render(None).contains("no usable pairs"), "{}", v.render(None));
    }

    /// A trial that was stopped is not a trial that finished.
    ///
    /// `stop` jumps the cursor to the end of the schedule, so afterwards the two
    /// are the same two numbers and `render` called both of them "complete". The
    /// case that matters is the common one: a `trial.json` reading `at: 12,
    /// blocks: []` — a trial that waited six hours for an idle desktop on
    /// battery, never got one, and was stopped — reported as "trial complete: 0
    /// of 12 blocks", which reads as an experiment that ran and found nothing
    /// rather than one that never ran at all.
    #[test]
    fn a_stopped_trial_does_not_report_itself_complete() {
        let mut trial = Trial::new(plan(), Mode::DryRun);
        assert!(!trial.finished(), "and it has not started");
        trial.stop();

        assert!(trial.finished(), "stopping does end it");
        assert!(trial.stopped(), "but it ended by being stopped");
        let v = trial.verdict();
        assert_eq!(v.blocks_done, 0);
        let text = v.render(None);
        assert!(text.contains("trial stopped"), "{text}");
        assert!(!text.contains("trial complete"), "{text}");
        // "yet" is a promise, and a trial that is over cannot keep it.
        assert!(!text.contains("yet"), "{text}");
        assert!(text.contains("no block ever started"), "{text}");
    }

    /// The distinction survives the state file, because that is where it is read.
    #[test]
    fn stopping_is_remembered_across_a_reload() {
        let mut trial = Trial::new(plan(), Mode::DryRun);
        trial.stop();
        let json = serde_json::to_string(&trial).unwrap();
        let back: Trial = serde_json::from_str(&json).unwrap();
        assert!(back.stopped(), "a stopped trial reloads as stopped: {json}");

        // And a file written before the flag existed still loads, reading as the
        // completed trial it already read as.
        let older = json.replace(",\"stopped\":true", "");
        assert!(!older.contains("stopped"), "the field really was removed: {older}");
        let back: Trial = serde_json::from_str(&older).unwrap();
        assert!(!back.stopped(), "an older file defaults to the reading it had");
    }

    #[test]
    fn the_mode_returns_to_what_the_user_had_when_the_trial_ends() {
        // A trial that leaves the machine in live mode after measuring that live
        // mode does nothing has made a decision that was not its to make.
        let trial = run(plan(), IDLE_ON_BATTERY, |_, _| (6.0, 0.8));
        assert!(trial.finished());
        let cond = Conditions { mode: Mode::Live, ..IDLE_ON_BATTERY };
        assert_eq!(trial.baseline_to_restore(&cond), Some(Mode::DryRun), "back to the baseline");

        // Asked once, at the end, and not standing: a caller that acts on it
        // and then asks again gets nothing the second time.
        let settled = Conditions { mode: Mode::DryRun, ..IDLE_ON_BATTERY };
        assert_eq!(trial.baseline_to_restore(&settled), None);

        let live_baseline = Trial::new(plan(), Mode::Live);
        assert_eq!(
            live_baseline.baseline_to_restore(&IDLE_ON_BATTERY),
            None,
            "and an unfinished trial has nothing to hand back yet"
        );
    }

    /// The bug this closes, from a real machine: `on_battery_only` on a laptop
    /// that is on mains, so no block could ever start — and the trial pinned the
    /// daemon to its dry-run baseline for 19.5 hours and then 9.3 more across
    /// two restarts, freezing nothing, while `dozectl mode live` was refused
    /// because "a trial is driving the mode".
    #[test]
    fn a_block_that_ends_gives_the_mode_back_instead_of_leaving_its_arm_behind() {
        // Reconstructed from a real 68-hour run. A 20-minute dry-run block ended
        // and nothing put the mode back, so the daemon sat in dry-run for 40
        // hours of monotonic runtime — 59% of the run — until the next block
        // happened to start. It was invisible throughout: no arm is current
        // between blocks, so `mode_driven_by_trial` reads false, and
        // `configured_mode` cannot see it because a trial moves the mode through
        // `set_mode`, which rewrites the field that check compares against.
        let plan = TrialPlan { pairs: 6, seed: 3, ..plan() };
        let mut trial = Trial::new(plan, Mode::Live);
        // A machine that meets the conditions for exactly one block and then
        // does not — which is the ordinary state of a laptop on mains.
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = Mode::Live;

        // The first block starts and takes the mode.
        let started = start_a_block(&mut trial, &cond, MonoTime::ZERO);
        let arm = trial.current_arm().expect("a block must be running");
        if let Some(m) = trial.desired_mode(&cond) {
            cond.mode = m;
        }
        assert_eq!(cond.mode, arm.mode());

        // It runs its course and closes.
        let end = started.plus(plan.block).plus(Duration::from_secs(1));
        trial.on_sample(end, &cond, &rates(6.0, 0.8));
        assert_eq!(trial.blocks().len(), 1, "the block should have closed");

        // Conditions now forbid a new one — the machine went back on mains.
        cond.on_ac = true;
        let mut later = end;
        for _ in 0..200 {
            later = later.plus(Duration::from_secs(60));
            trial.on_sample(later, &cond, &rates(6.0, 0.8));
            if let Some(m) = trial.handing_back(&cond) {
                cond.mode = m;
            }
            // And nothing may *demand* a mode while no block is running, which
            // is the other half of the same rule.
            assert_eq!(
                trial.desired_mode(&cond),
                None,
                "a trial with no running block has no claim on the mode"
            );
        }

        assert_eq!(trial.blocks().len(), 1, "no further block could start");
        assert_eq!(
            cond.mode,
            Mode::Live,
            "three hours after a dry-run block ended, the mode must be the user's again"
        );
    }

    #[test]
    fn a_spoiled_block_is_abandoned_rather_than_run_out() {
        // Measured on a real machine: four blocks, four discards, zero usable
        // pairs, and 89% of an eight-hour stretch spent in a trial-driven mode
        // for measurements that were all thrown away. Spoiling is sticky, so
        // every one of those blocks was already worthless the moment it spoiled
        // and then held the mode for twenty more minutes.
        let mut p = plan();
        p.settle = Duration::from_secs(60);
        let mut trial = Trial::new(p, Mode::Live);
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = Mode::Live;

        let started = start_a_block(&mut trial, &cond, MonoTime::ZERO);
        let arm = trial.current_arm().expect("a block must be running");
        // As the daemon does: the running block takes the mode.
        if let Some(m) = trial.desired_mode(&cond) {
            cond.mode = m;
        }
        assert_eq!(cond.mode, arm.mode());

        // The user comes back one sample in — nowhere near the block's end.
        cond.desktop = DesktopState::Active;
        let spoiled_at = started.plus(Duration::from_secs(30));
        trial.on_sample(spoiled_at, &cond, &rates(6.0, 0.8));

        assert!(trial.current_arm().is_none(), "the block must be over, not merely doomed");
        assert_eq!(trial.blocks().len(), 1);
        assert_eq!(
            trial.blocks()[0].discarded,
            Some(Discard::UserActive),
            "and it keeps the reason, because that is what explains a trial getting nowhere"
        );

        // The mode comes back immediately rather than in twenty minutes.
        if let Some(m) = trial.handing_back(&cond) {
            cond.mode = m;
        }
        assert_eq!(cond.mode, Mode::Live, "the user's mode, back at once");

        // Well inside what would have been the block's run.
        assert!(spoiled_at.since(started) < p.block);
    }

    #[test]
    fn a_block_does_not_start_until_conditions_have_held() {
        // What makes abandoning safe rather than frantic. Starting on the first
        // agreeable sample and ending on the next would burn the whole schedule
        // in minutes, each block costing two mode changes and a re-assert of
        // every held app — the thrash §8 exists to avoid, arriving through the
        // experiment meant to measure it.
        let mut p = plan();
        p.settle = Duration::from_secs(60);
        let mut trial = Trial::new(p, Mode::Live);
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = Mode::Live;

        // A machine that is idle for a moment and then not, over and over, never
        // starts a block at all.
        let mut now = MonoTime::ZERO;
        for _ in 0..40 {
            cond.desktop = DesktopState::IdlePresent;
            trial.on_sample(now, &cond, &rates(6.0, 0.8));
            now = now.plus(Duration::from_secs(20));
            cond.desktop = DesktopState::Active;
            trial.on_sample(now, &cond, &rates(6.0, 0.8));
            now = now.plus(Duration::from_secs(20));
        }
        assert!(trial.current_arm().is_none(), "flapping conditions start nothing");
        assert_eq!(trial.blocks().len(), 0, "and cost no blocks from the schedule");

        // Once it settles down, a block does start.
        cond.desktop = DesktopState::IdlePresent;
        start_a_block(&mut trial, &cond, now);
        assert!(trial.current_arm().is_some());
    }

    #[test]
    fn handing_back_is_asked_once_and_does_not_undo_a_later_choice() {
        // The failure mode in the other direction, and the reason this is an
        // event rather than a standing demand: a trial that kept asking for its
        // baseline on every sample pinned a daemon to dry-run for 19.5 hours
        // while `dozectl mode live` was refused (§11.2).
        let mut trial = Trial::new(plan(), Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = Mode::Live;

        let started = start_a_block(&mut trial, &cond, MonoTime::ZERO);
        let end = started.plus(plan().block).plus(Duration::from_secs(1));
        trial.on_sample(end, &cond, &rates(6.0, 0.8));

        assert_eq!(trial.handing_back(&cond), Some(Mode::DryRun), "the block just ended");
        assert_eq!(trial.handing_back(&cond), None, "and it is asked exactly once");

        // The user now says live. Nothing may take it back off them.
        cond.mode = Mode::Live;
        for _ in 0..50 {
            assert_eq!(trial.handing_back(&cond), None, "a spent hand-back must stay spent");
        }
    }

    #[test]
    fn stopping_a_trial_actually_restores_the_mode() {
        // `Daemon::stop_trial` asked `desired_mode` for this, which is
        // `current_arm()?` — and `stop` sets `at` past the end of the schedule,
        // making that `None` by definition. So the answer was always `None`,
        // stopping a trial restored nothing, and the daemon was left in whatever
        // mode the last block had set. For a dry-run arm that means switched off.
        let mut trial = Trial::new(plan(), Mode::Live);
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = Mode::Live;

        start_a_block(&mut trial, &cond, MonoTime::ZERO);
        if let Some(m) = trial.desired_mode(&cond) {
            cond.mode = m;
        }

        trial.stop();
        assert!(trial.finished());
        assert_eq!(
            trial.handing_back(&cond),
            Some(Mode::Live),
            "a stopped trial owes the mode back, whatever arm it died on"
        );
    }

    #[test]
    fn a_trial_that_cannot_start_a_block_does_not_touch_the_mode() {
        let trial = Trial::new(plan(), Mode::DryRun);
        let on_mains = Conditions { on_ac: true, mode: Mode::Live, ..IDLE_ON_BATTERY };

        assert!(trial.waiting(), "nothing can start while the machine is plugged in");
        assert_eq!(
            trial.blocked_by(&on_mains),
            Some(Discard::OnAc),
            "and it can say which condition it is"
        );
        assert_eq!(
            trial.desired_mode(&on_mains),
            None,
            "so it has no claim on the mode: the user stays live"
        );

        // The same trial the moment a block can run does claim the mode, so the
        // fix has not simply switched the mechanism off.
        let mut running = Trial::new(plan(), Mode::DryRun);
        start_a_block(&mut running, &IDLE_ON_BATTERY, MonoTime::ZERO);
        assert!(running.current_arm().is_some(), "a block is under way");
        let arm = running.current_arm().expect("just asserted").mode();
        let other = if arm == Mode::Live { Mode::DryRun } else { Mode::Live };
        let elsewhere = Conditions { mode: other, ..IDLE_ON_BATTERY };
        assert_eq!(running.desired_mode(&elsewhere), Some(arm));
    }

    #[test]
    fn stopping_early_keeps_what_was_measured() {
        let mut p = plan();
        p.settle = Duration::ZERO;
        p.min_samples = 1;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        let mut now = MonoTime::ZERO;
        for _ in 0..25 {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            let w = if trial.current_arm() == Some(Arm::Live) { 5.0 } else { 6.0 };
            trial.on_sample(now, &cond, &rates(w, 0.8));
            now = now.plus(Duration::from_secs(60));
        }
        let before = trial.blocks().len();
        assert!(before >= 2, "there should be finished blocks to keep");
        trial.stop();
        assert!(trial.finished());
        assert_eq!(trial.blocks().len(), before + 1, "the block in flight is recorded, not lost");
        assert!(trial.verdict().discharge_pairs >= 1);
    }

    #[test]
    fn losing_sight_of_focus_spoils_a_live_block() {
        // dozed holds nothing while it cannot see focus (§10.2), so a "live"
        // block spanning that is not a live block.
        let mut p = plan();
        p.settle = Duration::ZERO;
        p.min_samples = 1;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        cond.mode = trial.desired_mode(&cond).unwrap_or(cond.mode);
        trial.on_sample(MonoTime::ZERO, &cond, &rates(6.0, 0.8));
        cond.observing = false;
        trial.on_sample(MonoTime::from_millis(60_000), &cond, &rates(6.0, 0.8));
        cond.observing = true;
        trial.on_sample(MonoTime::from_millis(600_000), &cond, &rates(6.0, 0.8));
        assert_eq!(trial.blocks()[0].discarded, Some(Discard::NotObserving));
    }

    #[test]
    fn a_desktop_trial_can_measure_residency_without_a_battery() {
        // `on_battery_only = false` is for the machine that is never on battery.
        // Discharge is unavailable there; deep idle is not, and it is a real
        // measurement rather than a consolation prize.
        let mut p = plan();
        p.on_battery_only = false;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = Conditions { on_ac: true, ..IDLE_ON_BATTERY };
        let mut now = MonoTime::ZERO;
        for _ in 0..(p.pairs * 2 * 20) {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            let idle = if trial.current_arm() == Some(Arm::Live) { 0.9 } else { 0.7 };
            // No battery figure at all, which is what mains looks like.
            let r = Rates { discharge_w: None, ..rates(0.0, idle) };
            trial.on_sample(now, &cond, &r);
            now = now.plus(Duration::from_secs(60));
        }
        let v = trial.verdict();
        assert_eq!(v.discharge_pairs, 0, "no watts on mains");
        assert_eq!(v.idle_pairs, 4, "but residency is measurable: {:?}", trial.blocks());
        assert!((v.deep_idle_gain.unwrap() - 0.2).abs() < 1e-9);
        let text = v.render(None);
        assert!(text.contains("deep idle"), "{text}");
        assert!(!text.contains("measured saving"), "{text}");
    }

    #[test]
    fn a_trial_survives_a_daemon_restart_by_forgetting_only_the_block_in_flight() {
        // MonoTime is measured from the daemon's own start, so a persisted
        // `started` would be nonsense in the next process. Completed blocks are
        // absolute facts and are kept.
        let mut p = plan();
        p.settle = Duration::ZERO;
        p.min_samples = 1;
        let mut trial = Trial::new(p, Mode::DryRun);
        let mut cond = IDLE_ON_BATTERY;
        let mut now = MonoTime::ZERO;
        for _ in 0..15 {
            if let Some(mode) = trial.desired_mode(&cond) {
                cond.mode = mode;
            }
            trial.on_sample(now, &cond, &rates(6.0, 0.8));
            now = now.plus(Duration::from_secs(60));
        }
        assert!(trial.current_arm().is_some(), "mid-block, so there is something to lose");

        let json = serde_json::to_string(&trial).unwrap();
        let back: Trial = serde_json::from_str(&json).unwrap();
        assert_eq!(back.blocks(), trial.blocks(), "finished blocks survive");
        assert_eq!(back.at, trial.at);
        assert!(back.current_arm().is_none(), "the block in flight does not");
        assert!(back.waiting(), "and it starts a fresh one when conditions allow");
    }

    #[test]
    fn a_finished_trial_ignores_further_samples() {
        let mut trial = Trial::new(plan(), Mode::DryRun);
        trial.stop();
        let before = trial.blocks().len();
        trial.on_sample(MonoTime::ZERO, &IDLE_ON_BATTERY, &rates(6.0, 0.8));
        assert_eq!(trial.blocks().len(), before);
    }

    #[test]
    fn the_plan_states_how_long_it_will_take() {
        // So `dozectl trial` can say "about four hours" rather than leaving
        // someone to wonder whether it is stuck.
        let p = TrialPlan::default();
        assert_eq!(p.duration(), Duration::from_secs(4 * 3600));
    }
}
