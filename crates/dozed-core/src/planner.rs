//! The planner: the only thing in `dozed` that decides anything.
//!
//! `plan` is a pure function of (world, event, time) plus the planner's own
//! memory of what it has already asked for. It performs no I/O and never reads
//! a clock, so a recorded trace replays to a byte-identical intent stream —
//! which is both the regression suite and the rule-tuning workflow.
//!
//! It also has no idea whether it is in dry-run or live mode. That is the whole
//! point of the seam: both modes consume the identical stream, so the dry-run
//! journal cannot drift from what live mode would have done.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app::AppKey;
use crate::ceiling::{Ceiling, Hold};
use crate::config::{CooldownMode, RuleAction};
use crate::event::{Event, Visibility};
use crate::explain::{DecisionInputs, RuleOutcome, rule_matches, trace_rules};
use crate::grace::Grace;
use crate::intent::{Action, Intent, Outcome, Reason, ReasonKind};
use crate::throttle::ThrottleState;
use crate::time::{HumanDuration, MonoTime};
use crate::world::{AppState, WorldState};

/// The shortest hold that can pay for itself.
///
/// A freeze is not free, and its cost is *per cycle* rather than per second: the
/// thaw costs a redraw, a chat app costs a reconnect, and both are paid in full
/// whether the hold lasted two seconds or two hours. There is some hold length
/// below which the wakeups saved cannot cover that, and holds shorter than it
/// lose money however good the reasoning that produced them was.
///
/// How often that happens is the part worth measuring, and on a real 13-day
/// journal it was most of the time: 316 of 1,951 freeze/restore pairs lasted ten
/// seconds or less and 148 lasted three or less, with the two glance rules
/// re-freezing a median of 13 seconds after the thaw and a minimum of one.
///
/// Ten seconds is deliberately generous towards acting: it is not a claim that a
/// ten-second hold is worthwhile, it is the point below which a hold is
/// certainly not. Anything longer is left to the grace model to argue about.
pub const MIN_PAYBACK: Duration = Duration::from_secs(10);

/// A hold long enough to be evidence the app has stopped flapping.
///
/// Deliberately far above [`MIN_PAYBACK`] rather than equal to it, and that gap
/// is the whole fix. Counting *any* hold past `MIN_PAYBACK` as recovery is
/// circular once the backoff is working: the backoff exists precisely to push
/// holds just past that line, so it would immediately read its own success as
/// proof it was no longer needed, stand down, and let the thrash resume.
///
/// Measured doing exactly that. Per-app traces of the flap factor over three
/// days read `.....248888........` — climbing to the cap, collapsing straight
/// back to 1.0, and climbing again. A limit cycle, not the convergence this was
/// documented to have. Over the same window holds of three seconds or less went
/// from 7.6% of all holds to 17.2%.
///
/// Four times `MIN_PAYBACK` is "the user genuinely went away", as distinct from
/// "the wait we imposed elapsed".
const SETTLED_HOLD: Duration = Duration::from_secs(40);

/// The most the *doubling* will stretch a grace on its own.
///
/// Capped because this multiplies a grace other terms have already had their
/// say on, and an uncapped doubling would eventually mean "never act on this app
/// again" — a decision the rules should make, not an arithmetic accident.
///
/// It is a cap on the doubling and **not** on the term: see
/// [`Grace::raise_to_at_least`], which may take `flap` higher still to reach an
/// absolute floor. The two are different claims — "back off further each time"
/// is relative, "never take a hold that cannot pay for itself" is not — and
/// this file has the scar to prove a relative claim alone is not enough: an
/// `aggression` of 0.05 turns a 15s rule into a 1.5s wait, and eight times
/// almost nothing is still almost nothing.
const MAX_FLAP: f64 = 8.0;

/// Backoff for an app that keeps being let go before its hold could pay off.
///
/// Doubling per short hold, so it converges rather than oscillates: the wait
/// grows until holds start outlasting [`MIN_PAYBACK`], at which point
/// [`AppMemo::note_return`] resets the count and the app returns to whatever its
/// rules asked for. An app the user is genuinely alt-tabbing through settles at
/// the shortest hold worth taking instead of thrashing at the shortest hold the
/// config permits.
/// How many short holds in a row before an app counts as flapping.
///
/// Two, for the reason [`flap_factor`] gives: one is an event, a run is a
/// pattern.
///
/// Both the backoff and [`Grace::raise_to_at_least`]'s gate key on this, and
/// they must not drift apart: the floor is what makes the backoff mean something
/// at a small `aggression`, so a floor that engaged on a different count than
/// the doubling would leave a band where one applied and the other did not.
const FLAPPING_AT: u32 = 2;

fn flap_factor(short_holds: u32) -> f64 {
    // One short hold is an event, not a pattern: a notification arrived and the
    // user clicked it, which says nothing about whether holding that app works.
    // Two in a row is the thing this term exists for, and the same instinct as
    // the thermal hysteresis — a single sample either side of a line should not
    // move a state.
    let repeats = short_holds.saturating_sub(FLAPPING_AT - 1);
    if repeats == 0 {
        return 1.0;
    }
    f64::from(2u32.saturating_pow(repeats.min(8))).min(MAX_FLAP)
}

/// What the planner has asked for, per app. Decisions live here rather than in
/// [`WorldState`], which holds only observations.
#[derive(Debug, Clone, PartialEq)]
struct AppMemo {
    /// The action we believe is in force. [`Action::Restore`] means untouched.
    ///
    /// Written when the actuator reports back, not when the intent is issued —
    /// see [`Planner::note_outcome`].
    applied: Action,
    applied_at: MonoTime,
    /// What `applied` was before the intent now in flight, so an actuation that
    /// does not land can be rolled back to the truth.
    applied_before: Action,
    /// An action this app was asked to take that the actuator could not
    /// perform. Suppresses re-issuing the same impossible intent every tick;
    /// cleared as soon as the planner wants something else, so the app gets a
    /// fresh attempt on its next hide.
    refused: Option<Action>,
    /// Grace is measured from at least this instant. Bumped whenever we restore
    /// an app for a reason other than a rule, so that thawing an app on
    /// activation does not let it re-freeze on the very next tick.
    baseline: MonoTime,
    /// Last time the user came back to this app, for the resume cooldown.
    last_return: Option<MonoTime>,
    /// A hand-set limit on how far up the ladder this app may be taken, and
    /// when it lapses (`crate::ceiling`). `None` is the ordinary case.
    hold: Option<Hold>,
    /// How many holds in a row the user has cut short before [`MIN_PAYBACK`],
    /// counted by [`AppMemo::note_return`] and spent by [`flap_factor`].
    ///
    /// Survives a restore on purpose — it is the restore that reveals the hold
    /// was too short, so a counter reset by one would never reach two.
    short_holds: u32,
    /// The last derivation written to the decision log, so we can record
    /// changes rather than ticks.
    logged: Option<Decision>,
    /// The dynamic throttle's loop state, when a `throttle-to` rule owns this
    /// app. Planner memory rather than world state: it is what we decided, not
    /// what we observed.
    throttle: ThrottleState,
    /// Set while this app is held by a [`RuleAction::DozeCycle`] rule, so the
    /// maintenance schedule knows it participates and on what period.
    ///
    /// Cleared by any restore that is not a maintenance thaw — the user coming
    /// back ends the cycle, and the next hide starts a new one.
    cycle: Option<Cycle>,
}

/// One app's place in the maintenance schedule.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cycle {
    every: Duration,
    wake: Duration,
    /// When this app last came up for air, or `None` if it has not yet.
    last_woken: Option<MonoTime>,
}

impl AppMemo {
    fn new(now: MonoTime) -> Self {
        Self {
            applied: Action::Restore,
            applied_at: now,
            applied_before: Action::Restore,
            refused: None,
            baseline: now,
            last_return: None,
            hold: None,
            short_holds: 0,
            logged: None,
            throttle: ThrottleState::default(),
            cycle: None,
        }
    }

    /// Record what we have just asked for, keeping the previous value so that a
    /// failed actuation can be rolled back to it. Every write to `applied` goes
    /// through here, so the rollback in [`Planner::note_outcome`] has something
    /// truthful to roll back to.
    fn ask_for(&mut self, action: Action, now: MonoTime) {
        self.applied_before = self.applied;
        self.applied = action;
        self.applied_at = now;
    }

    /// The user came back to this app.
    ///
    /// Records it for the resume cooldown, and judges the hold they interrupted:
    /// a hold cut short by the user returning is the one case where we can tell,
    /// after the fact, that taking it was not worth the thaw it cost.
    ///
    /// Deliberately *not* every restore. The daemon lets go for plenty of
    /// reasons that say nothing about whether the hold was working — the machine
    /// suspending, a mode change, a shutdown, a maintenance thaw — and counting
    /// those would back the grace off for events the app had no part in. The
    /// suspend case is not hypothetical: it made an eight-hour sleep look like a
    /// hold that failed after zero seconds.
    ///
    /// Must be called *before* [`AppMemo::ask_for`], which is what still knows
    /// how long the hold ran.
    fn note_return(&mut self, now: MonoTime) {
        self.last_return = Some(now);
        // Nothing was being held, so there is no hold to judge. An app that was
        // merely running while focused has not failed at anything.
        if self.applied.is_restore() {
            return;
        }
        // Three zones, not two, and the middle one is the point.
        //
        // Under `MIN_PAYBACK` the hold lost money: count up. Past
        // `SETTLED_HOLD` the user genuinely left the app alone: count *down* by
        // one, so the backoff decays rather than vanishing. In between —
        // exactly where a working backoff parks its holds — nothing changes,
        // because a hold that lasted as long as we made it last is evidence
        // about us, not about the app.
        //
        // Decay rather than reset for the same reason the thermal state is
        // hysteretic: this climbs on one bad sample and must not fall on one
        // good one, or it spends its life crossing the line. Reset was the
        // shipped behaviour and it oscillated, measured, for three days.
        let held = now.since(self.applied_at);
        if held < MIN_PAYBACK {
            self.short_holds = self.short_holds.saturating_add(1);
        } else if held >= SETTLED_HOLD {
            self.short_holds = self.short_holds.saturating_sub(1);
        }
    }
}

/// A decision about one app at one instant, with its full derivation.
///
/// Shared by [`Planner::plan`] and [`Planner::explain`] so that what `dozectl
/// why` prints is literally what the planner concluded, not a reconstruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// The state the app should be in, or `None` to leave it as it is because
    /// a grace period is still running.
    pub desired: Option<Action>,
    pub reason: Reason,
    pub grace: Option<Grace>,
    /// How long the current situation has held.
    pub elapsed: Duration,
    /// When this decision could change on its own, so the daemon knows how long
    /// it may sleep (`DESIGN.md` §13).
    pub wake_at: Option<MonoTime>,
    /// The dynamic throttle's state after this step, when one applies. Recorded
    /// so a cap that moved can be explained from the trace rather than
    /// reconstructed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle: Option<ThrottleState>,
    /// The maintenance cycle this decision signs the app up to, when a
    /// `doze-cycle` rule owns it. Carried on the decision for the same reason
    /// `throttle` is: it is part of what was decided, so it belongs in the trace
    /// rather than being re-derived by whoever reads one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<CycleSpec>,
}

/// An app the planner is counting down, and the rule doing the counting.
///
/// See [`Planner::pending`]. Borrowed rather than owned because the callers are
/// building a reply out of it and none of them keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending<'a> {
    /// When the planner next intends to look at this app and act.
    pub at: MonoTime,
    /// The rule that will do it, whose `action` says what will happen.
    pub rule: &'a str,
}

/// What a [`RuleAction::DozeCycle`] rule asks for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CycleSpec {
    pub every: HumanDuration,
    pub wake: HumanDuration,
}

impl Decision {
    /// The rule that owns this app right now, whether it fired, is still
    /// counting down, or exempted the app.
    #[must_use]
    pub fn rule_name(&self) -> Option<&str> {
        self.reason.rule.as_deref()
    }

    /// Whether this is materially the same conclusion, reached the same way.
    ///
    /// What the decision log records is *changes* in the derivation rather than
    /// ticks, so this deliberately ignores everything that moves on its own:
    /// `elapsed` and `wake_at` advance every tick, and the sample and battery
    /// percentage drift continuously (see
    /// [`DecisionInputs::same_situation_as`]). Recording those would make the
    /// log a firehose and `dozed` a wakeup source, which §13 forbids.
    #[must_use]
    pub fn same_derivation_as(&self, other: &Self) -> bool {
        self.desired == other.desired
            && self.reason.kind == other.reason.kind
            && self.reason.rule == other.reason.rule
            && self.grace == other.grace
            && match (&self.reason.inputs, &other.reason.inputs) {
                (Some(a), Some(b)) => a.same_situation_as(b),
                (None, None) => true,
                _ => false,
            }
    }

    /// One line for the decision log, phrased for the commonest question:
    /// "why has nothing happened?".
    #[must_use]
    pub fn line(&self) -> String {
        let verb = match self.desired {
            Some(a) => format!("wants {}", a.verb()),
            None => "waiting".to_owned(),
        };
        let inputs = self.reason.inputs.as_ref().map(DecisionInputs::summary).unwrap_or_default();
        format!("{verb} — {} [{inputs}]", self.reason.detail)
    }
}

/// Everything `dozectl why <app>` needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    pub app: AppKey,
    /// What we believe is in force right now.
    pub applied: Action,
    pub since: MonoTime,
    pub visibility: Visibility,
    pub decision: Decision,
    /// All identifiers we saw for this app, so the user can write a narrower
    /// rule than the one that matched.
    pub evidence: String,
    /// Every rule and why it did or did not match. The winning rule alone
    /// answers "why did this happen"; the losers are what answer "why did my
    /// rule not fire", which is the question people actually have.
    pub rules: Vec<RuleOutcome>,
}

impl Explanation {
    /// One line, in the phrasing `dozectl` prints.
    #[must_use]
    pub fn summary(&self) -> String {
        let verb = match self.decision.desired {
            Some(a) if a == self.applied => format!("staying {}d", a.verb()),
            Some(a) => format!("about to {}", a.verb()),
            None => "waiting".to_owned(),
        };
        format!("{}: {verb} — {}", self.app.label(), self.decision.reason.detail)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Planner {
    memo: BTreeMap<AppKey, AppMemo>,
    /// Apps with a grace period running, and when it expires.
    deadlines: BTreeMap<AppKey, MonoTime>,
    /// Derivations that changed and have not been drained yet.
    pending: Vec<(AppKey, Decision)>,
    /// The maintenance window: when the next one opens, and which apps are up
    /// for air inside the current one (`RuleAction::DozeCycle`).
    ///
    /// One schedule for the whole session rather than a timer per app, because
    /// aligning the windows is the entire point — N apps waking together is one
    /// burst of wakeups, N apps waking whenever their own period happens to
    /// expire is N bursts, and the second is what the ladder already had.
    window: Option<Window>,
    next_window: Option<MonoTime>,
}

/// An open maintenance window.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    closes: MonoTime,
    /// The apps thawed for this window, which are the ones to put back.
    apps: BTreeSet<AppKey>,
}

impl Planner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The action we believe is in force for an app.
    #[must_use]
    pub fn applied(&self, key: &AppKey) -> Action {
        self.memo.get(key).map_or(Action::Restore, |m| m.applied)
    }

    /// Reconcile what we asked for with what actually happened.
    ///
    /// The planner used to commit `applied` when it *issued* an intent and
    /// never hear whether the intent landed, so a `Skipped` left it believing a
    /// hold that was not in force. Observed live: `dozectl apps` reported
    /// thunderbird `throttled` and counted it among the held apps, while the
    /// journal recorded that same throttle skipped on all three attempts and
    /// its processes ran untouched in a shared scope that has no `cpu.max` file
    /// at all.
    ///
    /// Three things were wrong with that, in increasing order of seriousness.
    /// The reporting lied. [`Planner::held`] feeds the crash-safety file
    /// (`DESIGN.md` §10.3), so a hold that never happened was persisted for the
    /// next daemon to sweep. And because [`Planner::reevaluate`] suppresses an
    /// intent whose action already matches `applied`, the false belief was
    /// self-sealing: the throttle was never retried and the app never
    /// escalated, so one sitting at 300+ wakeups/s was left alone for the life
    /// of the daemon under the impression it had been dealt with. §3 requires
    /// ambiguity to resolve towards "do nothing"; this resolved it towards
    /// "assume it worked".
    ///
    /// [`Outcome::WouldHave`] counts as success deliberately. Dry-run exists to
    /// simulate the live state machine exactly, and a dry run whose state
    /// refused to advance would model something else.
    pub fn note_outcome(&mut self, key: &AppKey, action: Action, outcome: &Outcome) {
        let Some(memo) = self.memo.get_mut(key) else { return };
        // Only reconcile the intent we actually have in flight. Anything else
        // is a stale report and must not disturb newer state.
        if memo.applied != action {
            return;
        }
        match outcome {
            Outcome::Applied | Outcome::WouldHave => memo.refused = None,
            Outcome::Skipped { .. } | Outcome::Failed { .. } => {
                memo.applied = memo.applied_before;
                memo.refused = Some(action);
            }
        }
    }

    /// The flap backoff in force for one app, for tests that are about the
    /// mechanism settling rather than about any one decision.
    #[cfg(test)]
    fn flap_of(&self, key: &AppKey) -> f64 {
        self.memo.get(key).map_or(1.0, |m| flap_factor(m.short_holds))
    }

    /// Limit how far up the ladder one app may be taken, until `until`.
    ///
    /// Takes effect on the next evaluation rather than acting here, so a ceiling
    /// goes through the same decision path everything else does and shows up in
    /// the journal with a reason. Setting one on an app that is currently held
    /// above it releases it on that pass ([`Ceiling::clamp_or_release`]).
    pub fn set_hold(
        &mut self,
        key: &AppKey,
        ceiling: Ceiling,
        until: Option<MonoTime>,
        now: MonoTime,
    ) {
        let memo = self.memo.entry(key.clone()).or_insert_with(|| AppMemo::new(now));
        memo.hold = Some(Hold { ceiling, until });
        // The derivation on file no longer describes why this app is in the
        // state it is in, exactly as after a mode change.
        memo.logged = None;
        // Re-examine it now rather than at whatever the last deadline was: a
        // ceiling that releases a frozen app should not wait out a grace period
        // to do it.
        self.deadlines.remove(key);
    }

    /// Drop a hold, whether it has expired or the user has cleared it.
    pub fn clear_hold(&mut self, key: &AppKey) -> bool {
        let Some(memo) = self.memo.get_mut(key) else { return false };
        let had = memo.hold.take().is_some();
        if had {
            memo.logged = None;
            self.deadlines.remove(key);
        }
        had
    }

    /// The hold in force for one app, if any is and it has not lapsed.
    #[must_use]
    pub fn hold(&self, key: &AppKey, now: MonoTime) -> Option<Hold> {
        self.memo.get(key)?.hold.filter(|h| !h.expired(now))
    }

    /// Every hold in force, for the daemon to persist across a restart.
    pub fn holds(&self, now: MonoTime) -> impl Iterator<Item = (&AppKey, Hold)> {
        self.memo
            .iter()
            .filter_map(move |(k, m)| m.hold.filter(|h| !h.expired(now)).map(|h| (k, h)))
    }

    /// Everything we are currently holding down, for the crash-safety file that
    /// lets a fresh daemon thaw orphans from a previous run (`DESIGN.md` §10.3).
    pub fn held(&self) -> impl Iterator<Item = (&AppKey, Action)> {
        self.memo.iter().filter(|(_, m)| !m.applied.is_restore()).map(|(k, m)| (k, m.applied))
    }

    /// What this app is waiting for, when it is waiting for something.
    ///
    /// The same `deadlines` entry the daemon sleeps on, and the rule from the
    /// derivation that produced it — so a client counting down to a freeze is
    /// reading the planner's own intention rather than recomputing the grace
    /// product beside it and drifting. `None` for an app with nothing pending:
    /// no rule matches it, or it is already in the state its rule wants.
    ///
    /// Not a promise. Grace is a product of the power state, what the user is
    /// doing and the resume cooldown (§8), so touching the keyboard moves this
    /// deadline out and unplugging moves it in. It is what would happen if
    /// nothing else changed, which is exactly what the deadline is for.
    #[must_use]
    pub fn pending(&self, key: &AppKey, now: MonoTime) -> Option<Pending<'_>> {
        // An app nothing may be done to has nothing pending, whatever the rule
        // counting it down still believes. Seen on screen as `left alone (29m
        // left), dozes in 1m33s` — a countdown to something we already knew we
        // would refuse, which is precisely what `PendingChange` promises not to
        // be. A cap still keeps its countdown: it *will* be acted on, just not
        // as far.
        if self
            .hold(key, now)
            .is_some_and(|h| h.ceiling.clamp(Action::Freeze { reclaim: false }).is_none())
        {
            return None;
        }
        let at = *self.deadlines.get(key)?;
        // The rule comes from the last derivation rather than being looked up
        // again: it is the one that is actually counting this app down, and a
        // second scan of the table could name a different one.
        let rule = self.memo.get(key)?.logged.as_ref()?.rule_name()?;
        Some(Pending { at, rule })
    }

    /// The earliest moment something could change on its own, so the daemon can
    /// back its sample interval right off when nothing is pending.
    ///
    /// Includes the maintenance boundary as well as the grace deadlines: a window
    /// that opens on a schedule needs the daemon awake to open it, and without
    /// this a window due in 30s would be opened whenever the 60s idle tick next
    /// came round — which is a *late* window, and one whose length is then wrong
    /// by however late it was.
    #[must_use]
    pub fn next_deadline(&self) -> Option<MonoTime> {
        let maintenance = match &self.window {
            Some(open) => Some(open.closes),
            None => self.next_window,
        };
        self.deadlines.values().min().copied().into_iter().chain(maintenance).min()
    }

    /// Decide what should happen, given an event that has already been folded
    /// into `world`.
    pub fn plan(&mut self, world: &WorldState, event: &Event, now: MonoTime) -> Vec<Intent> {
        // Global overrides first. Each of these is a safety invariant, so it
        // runs regardless of rules, mode, or grace.
        match event {
            Event::ThawAll => {
                return self.restore_all(
                    now,
                    Reason::new(ReasonKind::UserRequested, "panic thaw: restoring everything"),
                );
            }
            Event::Shutdown => {
                return self.restore_all(
                    now,
                    Reason::new(ReasonKind::Shutdown, "daemon exiting: restoring everything"),
                );
            }
            Event::PrepareForSleep { sleeping: true } => {
                return self.restore_all(
                    now,
                    Reason::new(ReasonKind::SystemSleep, "system suspending: restoring everything"),
                );
            }
            Event::FocusSourceHealth { healthy: false } => {
                return self.restore_all(
                    now,
                    Reason::new(
                        ReasonKind::FocusSourceLost,
                        "compositor helper stopped reporting: cannot see focus, restoring \
                         everything",
                    ),
                );
            }
            Event::AppGone { app } => {
                // Nothing to restore: the processes are gone. Drop the memo so a
                // future app reusing the key starts clean.
                self.memo.remove(app);
                self.deadlines.remove(app);
                return Vec::new();
            }
            Event::UserAction { app, action } => {
                return self.user_action(app, *action, now);
            }
            _ => {}
        }

        // Blind or asleep: hold everything as it is and make no new decisions.
        if !world.is_observing() {
            return Vec::new();
        }

        if let Event::ModeChanged { mode } = event {
            return self.reassert(*mode, now);
        }

        if let Event::ActivationRequested { app } = event {
            return self.activation(app, now);
        }

        // The shared maintenance schedule, before the per-app pass: a window that
        // has just opened has to be visible to `reevaluate`, which must not
        // re-freeze an app the window has deliberately let up for air.
        let mut out = self.maintenance(world, now);

        // Per-app events touch one app; everything else can change the answer
        // for all of them.
        match event.app() {
            Some(key) => out.extend(self.reevaluate(world, key, now)),
            None => {
                let keys: Vec<AppKey> = world.apps.keys().cloned().collect();
                out.extend(keys.iter().filter_map(|k| self.reevaluate(world, k, now)));
            }
        }
        out
    }

    /// The full derivation for one app, without changing anything.
    #[must_use]
    pub fn explain(&self, world: &WorldState, key: &AppKey, now: MonoTime) -> Option<Explanation> {
        let app = world.app(key)?;
        let memo = self.memo.get(key).cloned().unwrap_or_else(|| AppMemo::new(now));
        Some(Explanation {
            app: key.clone(),
            applied: memo.applied,
            since: memo.applied_at,
            visibility: app.visibility,
            decision: decide(world, app, &memo, now),
            evidence: app.evidence.describe(),
            rules: trace_rules(&world.config, app, world),
        })
    }

    /// Derivations that have changed since they were last drained.
    ///
    /// This is the answer to "why did nothing happen?". `plan` only emits an
    /// [`Intent`] when the applied action *changes*, so an app sitting in grace
    /// for five minutes — or an app a rule silently declines to match — leaves
    /// no trace at all in the intent stream. These records fill that gap, and
    /// they are edge-triggered on the derivation (see
    /// [`Decision::same_derivation_as`]) rather than emitted per tick.
    pub fn take_decisions(&mut self) -> Vec<(AppKey, Decision)> {
        std::mem::take(&mut self.pending)
    }

    fn restore_all(&mut self, now: MonoTime, reason: Reason) -> Vec<Intent> {
        self.deadlines.clear();
        let keys: Vec<AppKey> = self
            .memo
            .iter()
            .filter(|(_, m)| !m.applied.is_restore())
            .map(|(k, _)| k.clone())
            .collect();
        keys.into_iter()
            .map(|key| {
                let memo = self.memo.entry(key.clone()).or_insert_with(|| AppMemo::new(now));
                // Keyed on the reason, the same way the rules-driven restore
                // keys on `BecameVisible`. Of the four events that land here
                // only the panic thaw is the user asking for their app back,
                // and it is the most emphatic form of that request there is —
                // so it has to feed the backoff exactly as `dozectl thaw`
                // does. It did not, and that is the whole of this bug: a
                // thrashing app is *precisely* the app somebody panic-thaws,
                // and every one of those thaws left `short_holds` at zero.
                // Both safeguards key on that counter, so neither the doubling
                // nor the `MIN_PAYBACK` floor ever armed, and the app came
                // back in `grace` seconds — 1.5 of them at `aggression = 0.05`
                // — however many times the user asked. Three thaws in
                // eighteen seconds, measured, and all three read `flap 1.0`.
                //
                // The other three reasons stay uncounted, for the reason
                // `note_return` gives: a suspend is not the app's fault, and
                // counting one made an eight-hour sleep look like a hold that
                // failed instantly.
                //
                // Before `ask_for`, which is what still knows how long the
                // hold ran.
                if reason.kind == ReasonKind::UserRequested {
                    // The app is uncapped again, so the next hide converges
                    // from scratch rather than resuming against a cap that is
                    // no longer in force — same as every other restore the
                    // user asked for.
                    memo.throttle = ThrottleState::default();
                    memo.note_return(now);
                }
                memo.ask_for(Action::Restore, now);
                memo.baseline = now;
                // We just overrode the rules, so the derivation on file no
                // longer describes why this app is in the state it is in.
                memo.logged = None;
                Intent::new(key, Action::Restore, reason.clone(), now)
            })
            .collect()
    }

    fn user_action(&mut self, key: &AppKey, action: Action, now: MonoTime) -> Vec<Intent> {
        let memo = self.memo.entry(key.clone()).or_insert_with(|| AppMemo::new(now));
        if action.is_restore() {
            memo.throttle = ThrottleState::default();
            // Before `ask_for`, which is what still knows how long the hold ran.
            // Somebody typing `dozectl thaw` is the least ambiguous form of "the
            // user wanted this app back" there is, so it counts as a return.
            memo.note_return(now);
        }
        memo.ask_for(action, now);
        // Push the grace clock forward either way: an explicit request should
        // not be undone by a rule a fraction of a second later.
        memo.baseline = now;
        memo.logged = None;
        self.deadlines.remove(key);
        vec![Intent::new(
            key.clone(),
            action,
            Reason::new(ReasonKind::UserRequested, format!("asked to {}", action.verb())),
            now,
        )]
    }

    /// The user asked the compositor to raise this app. We must thaw on this and
    /// not merely on focus: single-instance activation goes through D-Bus into
    /// the frozen process, so the launcher would hang instead of raising a
    /// window (`DESIGN.md` §10).
    fn activation(&mut self, key: &AppKey, now: MonoTime) -> Vec<Intent> {
        let memo = self.memo.entry(key.clone()).or_insert_with(|| AppMemo::new(now));
        memo.baseline = now;
        self.deadlines.remove(key);
        if memo.applied.is_restore() {
            return Vec::new();
        }
        memo.note_return(now);
        memo.ask_for(Action::Restore, now);
        memo.logged = None;
        vec![Intent::new(
            key.clone(),
            Action::Restore,
            Reason::new(ReasonKind::Activated, "activation requested; thawing before it is raised"),
            now,
        )]
    }

    /// Open or close a maintenance window (`RuleAction::DozeCycle`).
    ///
    /// Runs before the per-app pass on every event, so a window opens and closes
    /// on the tick nearest its boundary without needing an event of its own.
    ///
    /// ## Why the schedule is global
    ///
    /// A frozen app that wakes on its own timer is a wakeup. Six of them on six
    /// timers is six bursts an hour apart from each other, which is most of what
    /// dozing removed. The windows are therefore *aligned*: one schedule ticking
    /// at the shortest `every` in force, with each app joining a tick once its own
    /// period has elapsed. A 60m app's window lands on top of a 15m app's rather
    /// than somewhere else, which is the desktop-level equivalent of timer
    /// coalescing — and the reason this is worth more than a per-app `wake` timer
    /// that would have been much easier to write.
    fn maintenance(&mut self, world: &WorldState, now: MonoTime) -> Vec<Intent> {
        // Closing first: an open window whose time is up must be closed on this
        // pass, even if the same pass would also open the next one.
        if let Some(open) = &self.window
            && now >= open.closes
        {
            let apps = open.apps.clone();
            self.window = None;
            let mut out = Vec::new();
            for key in apps {
                let Some(memo) = self.memo.get_mut(&key) else { continue };
                // Only put back what is still ours to put back. An app the user
                // returned to during the window has left the cycle, and one whose
                // window we already superseded is not frozen to begin with.
                let Some(cycle) = memo.cycle else { continue };
                if !world.app(&key).is_some_and(|a| a.visibility == Visibility::Hidden) {
                    memo.cycle = None;
                    continue;
                }
                let action = Action::Freeze { reclaim: world.config.reclaim_for(&key) };
                memo.ask_for(action, now);
                memo.logged = None;
                out.push(Intent::new(
                    key,
                    action,
                    Reason::new(
                        ReasonKind::Maintenance,
                        format!(
                            "maintenance window closed after {}; dozing again for {}",
                            HumanDuration(cycle.wake),
                            HumanDuration(cycle.every),
                        ),
                    ),
                    now,
                ));
            }
            return out;
        }

        // A window stays open until it closes, whatever else happens.
        if self.window.is_some() {
            return Vec::new();
        }
        let Some(due) = self.next_window else { return Vec::new() };
        if now < due {
            return Vec::new();
        }

        // Whose turn it is: held by a cycle rule, still hidden, and its own period
        // elapsed since it last came up.
        let mut waking: BTreeSet<AppKey> = BTreeSet::new();
        let mut longest = Duration::ZERO;
        let mut out = Vec::new();
        for (key, memo) in &mut self.memo {
            let Some(cycle) = &mut memo.cycle else { continue };
            if memo.applied.is_restore() {
                continue;
            }
            let since = cycle.last_woken.unwrap_or(memo.applied_at);
            if now.since(since) < cycle.every {
                continue;
            }
            if !world.app(key).is_some_and(|a| a.visibility == Visibility::Hidden) {
                continue;
            }
            cycle.last_woken = Some(now);
            longest = longest.max(cycle.wake);
            waking.insert(key.clone());
            let detail = format!(
                "maintenance window: up for air for {} after {} dozing",
                HumanDuration(cycle.wake),
                HumanDuration(now.since(since)),
            );
            memo.ask_for(Action::Restore, now);
            memo.logged = None;
            out.push(Intent::new(
                key.clone(),
                Action::Restore,
                Reason::new(ReasonKind::Maintenance, detail),
                now,
            ));
        }

        // The schedule advances whether or not anybody was eligible, or an empty
        // tick would be retried on every event for as long as the config lasts.
        self.next_window = self.shortest_period().map(|p| now.plus(p));
        if !waking.is_empty() {
            self.window = Some(Window { closes: now.plus(longest), apps: waking });
        }
        out
    }

    /// The tick interval for the shared schedule: the shortest period any held
    /// app is on. `None` when nothing is cycling, which stops the schedule.
    fn shortest_period(&self) -> Option<Duration> {
        self.memo.values().filter_map(|m| m.cycle.map(|c| c.every)).min()
    }

    /// Re-issue every action we believe is in force, because the thing that
    /// carries it out has changed underneath us.
    ///
    /// A dry-run outcome advances this state machine exactly as a live one does,
    /// which is the point of the seam and is also a trap: after an hour of
    /// dry-run the planner believes six apps are frozen, so
    /// [`Planner::reevaluate`] suppresses their freeze intents as already
    /// satisfied. `dozectl mode live` therefore switched the actuator and froze
    /// **nothing** — the apps stayed live and believed-held until something moved
    /// their visibility, which for a hidden Slack is hours. The counterfactual
    /// report went on accruing a saving that live mode was no longer producing.
    ///
    /// Re-asserting is deliberately unconditional rather than a diff against the
    /// actuator: the planner does not know what the actuator holds, and must not
    /// learn (§3). Every rung is idempotent — freezing a frozen cgroup, capping a
    /// capped one — so asserting something already true costs a write and a
    /// journal line.
    ///
    /// A refusal recorded before the switch is cleared. The actuator said no in
    /// circumstances that have now changed by definition, and the whole reason
    /// this function exists is that a stale belief is self-sealing.
    fn reassert(&mut self, mode: crate::config::Mode, now: MonoTime) -> Vec<Intent> {
        let detail = format!(
            "mode is now {}: re-asserting what we believe is held, because the actuator changed",
            mode.as_str()
        );
        let mut out = Vec::new();
        for (key, memo) in &mut self.memo {
            memo.refused = None;
            if memo.applied.is_restore() {
                continue;
            }
            out.push(Intent::new(
                key.clone(),
                memo.applied,
                Reason::new(ReasonKind::ModeChanged, detail.clone()),
                now,
            ));
        }
        out
    }

    fn reevaluate(&mut self, world: &WorldState, key: &AppKey, now: MonoTime) -> Option<Intent> {
        let app = world.app(key)?;
        // Checked before the memo is borrowed, and acted on after the decision:
        // an app up for air inside a maintenance window is *deliberately* thawed
        // while its rule still wants it frozen, so the ordinary path would put it
        // straight back on the next tick and the window would last one interval.
        let in_window = self.window.as_ref().is_some_and(|w| w.apps.contains(key));
        let mut joined: Option<Duration> = None;
        let memo = self.memo.entry(key.clone()).or_insert_with(|| AppMemo::new(now));
        let decision = decide(world, app, memo, now);

        // Record the derivation whenever it changes, including — especially —
        // when it produces no intent at all. An app that sits in grace for five
        // minutes and then has its rule stop matching leaves nothing whatsoever
        // in the intent stream, and that is precisely the case that needs
        // debugging.
        let changed = !memo.logged.as_ref().is_some_and(|prev| prev.same_derivation_as(&decision));
        if changed {
            memo.logged = Some(decision.clone());
        }

        let intent = match decision.desired {
            None => {
                if let Some(at) = decision.wake_at {
                    self.deadlines.insert(key.clone(), at);
                }
                None
            }
            Some(action) => {
                self.deadlines.remove(key);
                // Wanting something different clears a refusal: the actuator
                // said no to *that* action in *those* circumstances, and an app
                // that has since been focused and hidden again deserves a fresh
                // attempt. Without this the refusal would be permanent, which
                // is the bug it was introduced to fix wearing different clothes.
                if memo.refused.is_some_and(|refused| refused != action) {
                    memo.refused = None;
                }
                // Re-issuing an intent the actuator has already refused would
                // put a line in the journal every tick and change nothing. It
                // is suppressed rather than retried, but `applied` still says
                // the app is untouched, which is the honest answer.
                if action == memo.applied || memo.refused == Some(action) {
                    None
                } else if in_window && !action.is_restore() {
                    // The window has the floor until it closes. Not an error and
                    // not a refusal: the rule is right, it is simply not its turn.
                    None
                } else {
                    if action.is_restore() {
                        // Whatever brought the app back also restarts its grace,
                        // and ends the throttle loop: the app is uncapped again,
                        // so the next hide has to converge from scratch rather
                        // than resume against a cap that is no longer in force.
                        memo.throttle = ThrottleState::default();
                        memo.baseline = now;
                        // A restore the *rules* wanted — the user came back, an
                        // exemption appeared — ends the cycle. The next hide
                        // starts a fresh one, so an app cannot accumulate a claim
                        // to a window while nobody is dozing it.
                        memo.cycle = None;
                        // The resume cooldown means "we thawed this because the
                        // user came back to it", so it only counts when there was
                        // something to thaw. An app that was merely running while
                        // focused has not earned extra patience.
                        if decision.reason.kind == ReasonKind::BecameVisible {
                            memo.note_return(now);
                        }
                    }
                    memo.ask_for(action, now);
                    // The loop only advances when its intent is actually
                    // issued. Advancing it on every evaluation would let the
                    // cap drift while the app sat in grace, and the trace would
                    // then show a cap nobody ever applied.
                    if let Some(t) = decision.throttle {
                        memo.throttle = t;
                    }
                    // Signing up to the shared maintenance schedule, for the same
                    // reason and at the same moment: only a hold that was actually
                    // asked for gets a window.
                    if let Some(spec) = decision.cycle {
                        let last_woken = memo.cycle.and_then(|c| c.last_woken);
                        memo.cycle = Some(Cycle {
                            every: spec.every.get(),
                            wake: spec.wake.get(),
                            // Kept across a re-freeze, so an app cannot earn a
                            // window every tick by being frozen repeatedly.
                            last_woken,
                        });
                        joined = Some(spec.every.get());
                    }
                    Some(Intent::new(key.clone(), action, decision.reason.clone(), now))
                }
            }
        };

        // After the memo borrow: the schedule is planner-wide state.
        if let Some(every) = joined {
            let next = now.plus(every);
            self.next_window = Some(self.next_window.map_or(next, |t| t.min(next)));
        }

        if changed {
            self.pending.push((key.clone(), decision));
        }
        intent
    }
}

/// Whether this rule may act on the window that currently has focus.
///
/// Deliberately narrow. `dozed`'s first promise is that a user cannot tell it
/// is running, and the foreground window is where that promise is easiest to
/// break — so opting in has to be explicit in the rule text and cannot be
/// arrived at by a predicate that happens to match.
fn foreground_ok(rule: &crate::config::Rule) -> bool {
    let asks_for_focus = rule.when.visibility == Some(Visibility::Focused)
        || rule.match_.visibility == Some(Visibility::Focused);
    let caps_rather_than_stops =
        matches!(rule.action, RuleAction::Throttle(_) | RuleAction::ThrottleTo(_));
    asks_for_focus && caps_rather_than_stops
}

/// The decision procedure, in priority order. Every early return is a reason
/// the user can be shown.
fn decide(world: &WorldState, app: &AppState, memo: &AppMemo, now: MonoTime) -> Decision {
    let cfg = &world.config;
    // Captured once, up front, and attached to every conclusion below —
    // including the early safety returns. A decision recorded without the state
    // it was made from cannot be checked afterwards, only believed.
    let in_cooldown =
        memo.last_return.is_some_and(|t| now.since(t) < cfg.defaults.resume_cooldown.get());
    let inputs = DecisionInputs::capture(app, world, in_cooldown);

    let restore = |kind: ReasonKind, detail: String| Decision {
        desired: Some(Action::Restore),
        reason: Reason::new(kind, detail).with(inputs.clone(), None),
        grace: None,
        elapsed: Duration::ZERO,
        wake_at: None,
        throttle: None,
        cycle: None,
    };

    if app.is_protected() {
        return restore(ReasonKind::Safety, "session infrastructure: never actionable".to_owned());
    }
    if let Some(ex) = app.hard_exemption() {
        return restore(ReasonKind::Safety, format!("{}: never actionable", ex.as_str()));
    }
    if app.visibility == Visibility::VisibleUnfocused && !cfg.freeze_visible_for(&app.key) {
        // A frozen window that is still on screen is a stale rectangle, which
        // users read as a crash.
        return restore(ReasonKind::BecameVisible, "on screen but not focused".to_owned());
    }

    // The focused window used to return here unconditionally, before the rule
    // table was consulted at all. It now gets as far as picking a rule, so that
    // a `never` can still exempt it and so that a rule written explicitly for
    // the foreground can act — but nothing else changed: see `foreground_ok`.
    let Some(rule) = cfg.rules.iter().find(|r| rule_matches(r, app, world)) else {
        // A focused window with no rule is not "unruled", it is simply the
        // window you are looking at — and it thaws for that reason, which is
        // what `dozectl why` and the journal have always said. Every shipped
        // rule matches `hidden`, so this is the ordinary path for the
        // foreground rather than an edge case.
        return if app.visibility == Visibility::Focused {
            restore(ReasonKind::BecameVisible, "focused".to_owned())
        } else {
            restore(ReasonKind::NoRule, "no rule matches".to_owned())
        };
    };

    if rule.action == RuleAction::Never {
        return Decision {
            desired: Some(Action::Restore),
            reason: Reason {
                kind: ReasonKind::Exempt,
                rule: Some(rule.name.clone()),
                detail: format!("exempt by rule `{}`", rule.name),
                inputs: Some(inputs),
                grace: None,
            },
            grace: None,
            elapsed: Duration::ZERO,
            wake_at: None,
            throttle: None,
            cycle: None,
        };
    }

    // A rule predicated on `focused_for` ignores the resume cooldown entirely —
    // both the multiplier here and the floor below. The cooldown's premise is
    // "an app the user returned to is clearly in their active rotation"; a rule
    // that only matches a visit of a few seconds has established the opposite
    // for this visit, so applying it anyway would have the two contradict each
    // other. Exempting only the floor is not enough and was the first thing
    // tried: the factor alone silently doubled a 5s wait to 10s, which looks
    // like the rule misfiring rather than the cooldown still being in force.
    let glance = rule.when.focused_for.is_some() || rule.match_.focused_for.is_some();
    // The foreground is protected by what a rule *is*, not by where this check
    // sits. Two conditions, both required, and neither inferrable:
    //
    // * the rule says `visibility = "focused"` in so many words, so no existing
    //   rule can start acting on the window you are looking at by accident —
    //   every shipped one says `hidden`;
    // * the action is a throttle. A frozen focused window is a stale rectangle
    //   that reads as a crash, which §1 rules out more firmly than it wants any
    //   saving. A cap only makes the app slower.
    //
    // Anything else focused restores, exactly as it did when this was an
    // unconditional return above the rule table.
    if app.visibility == Visibility::Focused && !foreground_ok(rule) {
        return restore(ReasonKind::BecameVisible, "focused".to_owned());
    }

    // Now that the rule is known, so is whether the cooldown was in force but
    // exempted. Recorded on the inputs rather than inferred by a reader from
    // the rule name, because "which rules are glance rules" is a property of
    // the config file at the time, not of the journal.
    let inputs = DecisionInputs { cooldown_bypassed: in_cooldown && glance, ..inputs };

    let base = rule.grace().unwrap_or_else(|| cfg.base_grace_for(&app.key));
    let mut grace = Grace::compute_for_rule(
        cfg,
        base,
        &world.power,
        world.desktop,
        in_cooldown && !glance,
        // A rule about noise or about dropped frames is not about endurance, so
        // `on_ac = "disabled"` must not veto it (`Rule::acts_on_ac`).
        rule.acts_on_ac(),
    );
    // Not exempted for a glance rule, and that is the whole point of it. Every
    // other term in the product is an argument about how long it is worth
    // waiting; this one is the observation that waiting that long has not been
    // working. A glance rule is exactly the rule that produces the pattern —
    // measured on a real session, the two shipped `redoze-after-a-glance` rules
    // accounted for 1,459 of 1,955 rule-fired decisions, with a median of 13
    // seconds between the thaw and the next freeze and a minimum of one.
    grace.flap = flap_factor(memo.short_holds);

    if glance {
        // Exempt from the activity factor as well as the cooldown, because all
        // three were guarding the same thing and multiplying them made the rule
        // nominal. `active_factor` exists because "during heavy task switching a
        // freeze that gets undone 20s later is a net loss" — exactly the alt-tab
        // case this rule's own short `for` was chosen to cover.
        //
        // Measured live: a 15s wait came out as 4m, sixteenfold, so the rule
        // meant to re-doze a glanced-at app promptly did nothing of the sort.
        // The wait a glance rule states is the wait it gets. The power term
        // still applies — that is about what the wait is worth, not about
        // thrash.
        grace.activity = 1.0;
    }

    // Last, and that ordering is load-bearing: the floor is a claim about the
    // number this function finally returns, so every term it is meant to
    // survive must already be in the product.
    //
    // It ran *before* the glance flattening above, which meant solving against
    // an `activity` that was about to be divided back out. With the shipped
    // `active_factor = 4.0` and `aggression = 0.05` the floor read a 12s
    // product, declared itself satisfied, and then watched the flattening turn
    // it into 3s — so the one guarantee written to bound this rule was, on an
    // active desktop, arithmetically incapable of binding. Measured live on
    // exactly that config: graces of 1s, 3s, 6s and 12s at flap factors of 1,
    // 2, 4 and 8, against a floor promising ten seconds at worst, and 24% of
    // holds in a 24-hour window ending inside `MIN_PAYBACK`.
    //
    // Every test in `planner_tests` missed it because `Scenario` starts on
    // `IdlePresent`, where `activity` is 1.0 and the flattening is a no-op. The
    // journal records `desktop: "active"` for essentially all of the thrash.
    //
    // Still gated on the app having a history of short holds, and that gate is
    // not an oversight: `MIN_PAYBACK` bounds a *hold*, and a short wait is only
    // evidence of a short hold once the holds have actually been cut short.
    // Before that, a glance rule's stated `for` is the user asking for a short
    // wait and is entitled to get one.
    if memo.short_holds >= FLAPPING_AT {
        grace.raise_to_at_least(MIN_PAYBACK);
    }

    if grace.is_disabled() {
        // On AC with `on_ac = disabled` there is nothing to save, so also let go
        // of anything we are already holding.
        return Decision {
            desired: Some(Action::Restore),
            reason: Reason {
                kind: ReasonKind::Exempt,
                rule: Some(rule.name.clone()),
                detail: "on AC and `on_ac = disabled`".to_owned(),
                inputs: Some(inputs),
                grace: Some(grace),
            },
            grace: Some(grace),
            elapsed: Duration::ZERO,
            wake_at: None,
            throttle: None,
            cycle: None,
        };
    }

    // Grace runs from the later of "when the app went quiet" and "when we last
    // deliberately left it alone".
    let since = app.visibility_since.max(memo.baseline);
    let elapsed = now.since(since);

    // The resume cooldown as a *minimum wait* rather than only a multiplier
    // (`CooldownMode::Floor`, the default).
    //
    // As a multiplier alone it could be cancelled out entirely: grace is a
    // product, and `away_factor = 0.0` zeroes it, so while the screen was locked
    // the cooldown protected nothing. Replaying a real locked session showed a
    // browser restored on unlock and re-frozen 81 seconds later, inside a
    // nominal two-minute cooldown.
    //
    // Checked before the grace test and reported separately, because "the user
    // just came back to this" is a different reason to wait than "it has not been
    // quiet for long enough", and a decision log that conflates them cannot be
    // used to explain either.
    // The floor, too: it is a minimum wait measured from the very return that
    // makes the visit brief, so it is always in force at exactly the moment a
    // glance rule wants to act. Left subject to it the rule could never fire.
    if !glance
        && cfg.defaults.resume_cooldown_mode == CooldownMode::Floor
        && let Some(returned_at) = memo.last_return
    {
        let until = returned_at.plus(cfg.defaults.resume_cooldown.get());
        if now < until {
            return Decision {
                desired: None,
                reason: Reason {
                    kind: ReasonKind::RuleFired,
                    rule: Some(rule.name.clone()),
                    detail: format!(
                        "returned to {} ago, holding off for the resume cooldown {}",
                        HumanDuration(now.since(returned_at)),
                        HumanDuration(cfg.defaults.resume_cooldown.get()),
                    ),
                    inputs: Some(inputs),
                    grace: Some(grace),
                },
                grace: Some(grace),
                elapsed,
                wake_at: Some(until),
                throttle: None,
                cycle: None,
            };
        }
    }

    if !grace.elapsed_by(elapsed) {
        let effective = grace.effective().unwrap_or_default();
        return Decision {
            desired: None,
            reason: Reason {
                kind: ReasonKind::RuleFired,
                rule: Some(rule.name.clone()),
                detail: format!(
                    "{} for {}, waiting out grace {grace}",
                    app.visibility.as_str(),
                    HumanDuration(elapsed),
                ),
                inputs: Some(inputs),
                grace: Some(grace),
            },
            grace: Some(grace),
            elapsed,
            wake_at: Some(since.plus(effective)),
            throttle: None,
            cycle: None,
        };
    }

    // The dynamic throttle is the one action that is not a constant: the cap is
    // computed from the newest sample and the cap that preceded it
    // (`crate::throttle`). Done here rather than in the actuator so it stays
    // inside the pure function — a recorded trace has to replay to the identical
    // cap sequence or §14's replay stops being a regression suite.
    let (action, detail, throttle, cycle) = match rule.action {
        RuleAction::Freeze => (
            Action::Freeze { reclaim: cfg.reclaim_for(&app.key) },
            format!("{} for {}, grace {grace}", app.visibility.as_str(), HumanDuration(elapsed)),
            None,
            None,
        ),
        RuleAction::DozeCycle { every, wake } => (
            // The same action as a plain freeze: what differs is that the planner
            // will let it up for air, and the actuator has no business knowing
            // about a schedule (§3).
            Action::Freeze { reclaim: cfg.reclaim_for(&app.key) },
            format!(
                "{} for {}, grace {grace}; up for air {wake} every {every}",
                app.visibility.as_str(),
                HumanDuration(elapsed),
            ),
            None,
            Some(CycleSpec { every, wake }),
        ),
        RuleAction::Throttle(pct) => (
            Action::Throttle { pct },
            format!("{} for {}, grace {grace}", app.visibility.as_str(), HumanDuration(elapsed)),
            None,
            None,
        ),
        RuleAction::ThrottleTo(target) => {
            let step = memo.throttle.step(app.sample.wakeups_per_sec, target);
            (Action::Throttle { pct: step.cap }, step.detail, Some(step.state), None)
        }
        RuleAction::Never => unreachable!("handled above"),
    };

    // The last word on *how far*, after the rule has had the last word on
    // *whether* (`crate::ceiling`). Deliberately here rather than earlier: a
    // ceiling is not a reason to skip the rule table, and a decision log that
    // said "held off by a ceiling" without naming the rule that wanted to act
    // would answer a question nobody asked.
    if let Some(hold) = memo.hold.filter(|h| !h.expired(now)) {
        let permitted = hold.ceiling.clamp_or_release(action, memo.applied);
        if permitted != Some(action) {
            let detail = match permitted {
                // Capped instead of frozen, or capped lower than the rule asked.
                Some(Action::Throttle { pct }) => format!(
                    "`{}` wanted {}; {} by hand, so capped at {pct}% instead ({})",
                    rule.name,
                    action.verb(),
                    hold.ceiling.as_str(),
                    hold.summary(now),
                ),
                // Nothing permitted, and something was held: let it go.
                Some(Action::Restore) => format!(
                    "{} by hand, so releasing it ({})",
                    hold.ceiling.as_str(),
                    hold.summary(now)
                ),
                // Unreachable: no ceiling permits a freeze that the rule did not
                // already want, and an unchanged action does not get here at all.
                // Spelled out rather than `_`, so adding a rung to the ladder
                // makes this arm a compile error instead of a wrong sentence.
                Some(Action::Freeze { .. }) => {
                    format!("`{}` wanted {}, and it was permitted", rule.name, action.verb())
                }
                // Nothing permitted and nothing held: the rule is simply not
                // getting its way, and the log says so once rather than per tick.
                None => format!(
                    "`{}` wanted {}; {} by hand ({})",
                    rule.name,
                    action.verb(),
                    hold.ceiling.as_str(),
                    hold.summary(now),
                ),
            };
            return Decision {
                desired: permitted,
                reason: Reason {
                    kind: ReasonKind::UserRequested,
                    rule: Some(rule.name.clone()),
                    detail,
                    inputs: Some(inputs),
                    grace: Some(grace),
                },
                grace: Some(grace),
                elapsed,
                // So the daemon wakes to lift the ceiling rather than sleeping
                // through its expiry.
                wake_at: hold.until,
                throttle: None,
                cycle: None,
            };
        }
    }

    Decision {
        desired: Some(action),
        reason: Reason::rule(rule.name.clone(), detail).with(inputs, Some(grace)),
        grace: Some(grace),
        elapsed,
        wake_at: None,
        throttle,
        cycle,
    }
}

#[cfg(test)]
#[path = "planner_tests.rs"]
mod tests;
