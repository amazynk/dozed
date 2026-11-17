//! Recording a session, and replaying it against a different ruleset.
//!
//! `DESIGN.md` §14: the planner is pure and its clock is injected, so a recorded
//! event stream is a complete and replayable description of a session. That buys
//! two things at once.
//!
//! The first is debugging. A journal entry says what `dozed` decided; a trace
//! says what it *saw*, which is the only way to tell a wrong decision apart from
//! a correct decision made on bad input. Traces carry the derivations too —
//! including the ones that produced no intent, which is most of them.
//!
//! The second is tuning. `dozectl replay <trace> --config new.toml` re-runs a
//! recorded day through a modified rule table and diffs the result, so a rule
//! change is evaluated against what actually happened rather than against a
//! guess. Replaying a trace against its own config must reproduce its intent
//! stream exactly; [`replay`] is the function that has to hold that line, and
//! `identical_config_reproduces_the_stream` is the test that keeps it honest.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::app::AppKey;
use crate::config::Config;
use crate::event::Event;
use crate::intent::{Intent, Outcome};
use crate::planner::{Decision, Planner};
use crate::time::MonoTime;
use crate::world::WorldState;

/// Bumped when a change would stop an older reader from making sense of a
/// trace. A reader that sees a version it does not know refuses rather than
/// guessing, because a misread trace is worse than no trace.
pub const TRACE_VERSION: u32 = 1;

/// Traces are named for their start in unix seconds, so a lexicographic sort of
/// the directory is a chronological one. Defined here so the daemon that writes
/// them and the tools that read them cannot disagree.
#[must_use]
pub fn trace_file_name(wall_start: u64) -> String {
    format!("{wall_start:010}.jsonl")
}

#[must_use]
pub fn is_trace_name(name: &str) -> bool {
    name.ends_with(".jsonl")
}

/// Parse JSON lines into records, skipping any that will not read.
///
/// Returns the records and the number of lines dropped. A truncated final line
/// is the normal result of a `SIGKILL`, and refusing the whole file over it
/// would lose exactly the session most worth reading — so the count is reported
/// rather than the parse failing.
pub fn parse_lines<I: IntoIterator<Item = S>, S: AsRef<str>>(
    lines: I,
) -> (Vec<TraceRecord>, usize) {
    let mut records = Vec::new();
    let mut skipped = 0;
    for line in lines {
        let line = line.as_ref().trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(r) => records.push(r),
            Err(_) => skipped += 1,
        }
    }
    (records, skipped)
}

/// The first line of every trace: everything a replay needs before the events
/// mean anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceHeader {
    pub version: u32,
    /// Which build produced this, so a trace that replays differently against a
    /// newer binary is diagnosable rather than mysterious.
    pub dozed_version: String,
    /// Identifies the daemon run. The same token the compositor helper sees, so
    /// a trace can be lined up against the helper's own view.
    pub instance: String,
    /// Unix seconds at `MonoTime::ZERO`, which is what lets a monotonic offset
    /// be turned back into a time of day.
    pub wall_start: u64,
    /// Whether this run could actually touch anything.
    pub live: bool,
    pub config: Box<Config>,
}

impl TraceHeader {
    #[must_use]
    pub fn wall_of(&self, at: MonoTime) -> u64 {
        self.wall_start + at.ms / 1000
    }
}

/// What a [`TraceRecord::Gap`] marks, as a value rather than as prose.
///
/// The three cases used to be told apart only by their `reason` text, and two
/// of them differed by a suffix — `"hit the 64 MB size cap"` for a run that
/// ended there and `"hit the 64 MB size cap; continues in ..."` for one that
/// did not. Opposite outcomes, and the bad one a strict prefix of the good one,
/// so every reader wanting the distinction was matching on prose. It is a fact
/// about the record; it belongs in the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapKind {
    /// Recording failed, and later recovered. Records are missing across this
    /// point and the stream goes on below it.
    ///
    /// The default, and deliberately so: it is what every gap written before
    /// this field existed was, bar the capped ones, and it is the reading that
    /// errs towards *less* trust in the recording rather than more.
    #[default]
    Hole,
    /// The file reached its size cap and the run continues in the next part.
    /// Nothing is missing — this is a seam, not a hole.
    Rotated,
    /// Recording stopped at this point and did not come back in this file.
    Truncated,
}

impl GapKind {
    /// Whether records are actually missing across this point.
    ///
    /// The question every consumer is really asking. A rotation is the one kind
    /// that answers `false`, and treating it as a hole would report an
    /// incomplete recording for a recorder that never faltered.
    #[must_use]
    pub const fn loses_records(self) -> bool {
        matches!(self, Self::Hole | Self::Truncated)
    }
}

/// One line of a trace.
///
/// Deliberately one stream rather than three files: the whole value is being
/// able to read an event, the derivation it caused and the intent that followed
/// in the order they happened, and a shared `seq` is what makes that ordering
/// checkable rather than assumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rec", rename_all = "kebab-case")]
pub enum TraceRecord {
    Header(Box<TraceHeader>),
    /// An event, exactly as the daemon fed it to the planner.
    Event {
        seq: u64,
        at: MonoTime,
        event: Box<Event>,
    },
    /// A derivation that changed. Most produce no intent — an app waiting out
    /// grace, a rule that quietly stopped matching — and those are precisely the
    /// ones the intent stream cannot show you.
    Decision {
        seq: u64,
        at: MonoTime,
        app: AppKey,
        decision: Box<Decision>,
    },
    /// An intent and what became of it.
    Intent {
        seq: u64,
        at: MonoTime,
        intent: Box<Intent>,
        outcome: Outcome,
    },
    /// Recording stopped for a while and started again. **This is the hole.**
    ///
    /// The recorder used to stop for the rest of the run when a write or a flush
    /// failed, and `DESIGN.md` §13 gives the reason: *"a trace with unmarked
    /// gaps is worse than no trace, because §14 replays it as though it were
    /// complete"*. That is right about the gap and wrong about the remedy — a
    /// disk that filled up at nine in the morning should not cost the whole day's
    /// recording, and `ENOSPC` is a condition that ends.
    ///
    /// So the recorder retries, and this record is what makes resuming
    /// defensible: the gap is *marked*, in the stream, at the point it happened,
    /// with its length and the reason. Everything downstream can then act on it
    /// rather than replaying across it in silence — which is the whole of the
    /// objection §13 raised.
    Gap {
        seq: u64,
        at: MonoTime,
        /// Which of the three things this gap is.
        ///
        /// Defaulted, so a trace written before the field existed still parses
        /// — as a [`GapKind::Hole`], which is what all but the capped ones
        /// were. Those older files carry the distinction in `reason` only, and
        /// nothing can recover it for them.
        #[serde(default)]
        kind: GapKind,
        /// How long the recorder was down.
        down_ms: u64,
        /// Records dropped in that time. A floor rather than a count: it is what
        /// the daemon *offered* the recorder while it was refusing, and anything
        /// the buffer was holding when the write failed is lost above and beyond
        /// it.
        lost: u64,
        /// What stopped it — the `ENOSPC` or whatever else it was.
        reason: String,
    },
}

impl TraceRecord {
    #[must_use]
    pub const fn seq(&self) -> Option<u64> {
        match self {
            Self::Header(_) => None,
            Self::Event { seq, .. }
            | Self::Decision { seq, .. }
            | Self::Intent { seq, .. }
            | Self::Gap { seq, .. } => Some(*seq),
        }
    }

    #[must_use]
    pub const fn at(&self) -> Option<MonoTime> {
        match self {
            Self::Header(_) => None,
            Self::Event { at, .. }
            | Self::Decision { at, .. }
            | Self::Intent { at, .. }
            | Self::Gap { at, .. } => Some(*at),
        }
    }

    /// One line for `dozectl trace`, in the order things happened.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::Header(h) => {
                format!("trace v{} from dozed {} ({})", h.version, h.dozed_version, h.instance)
            }
            Self::Event { seq, at, event } => {
                format!("{seq:>6} {at} event    {}", describe_event(event))
            }
            Self::Decision { seq, at, app, decision } => {
                format!("{seq:>6} {at} decide   {} {}", app.label(), decision.line())
            }
            Self::Intent { seq, at, intent, outcome } => {
                let entry = crate::journal::JournalEntry::new(intent, outcome.clone(), 0);
                format!("{seq:>6} {at} intent   {}", entry.line())
            }
            // Deliberately loud in a column of otherwise uniform rows. Somebody
            // scrolling a trace to work out why an app was not held needs to see
            // that the answer might be "we stopped writing for eleven minutes"
            // before they conclude anything from the silence.
            // Named for what happened, because "GAP" over a rotation reads as
            // damage to a file that has none.
            Self::Gap { seq, at, kind: GapKind::Rotated, reason, .. } => {
                format!("{seq:>6} {at} ROLL     {reason}")
            }
            Self::Gap { seq, at, kind: GapKind::Truncated, lost, reason, .. } => {
                format!("{seq:>6} {at} END      recording stopped ({lost} records lost): {reason}")
            }
            Self::Gap { seq, at, kind: GapKind::Hole, down_ms, lost, reason } => format!(
                "{seq:>6} {at} GAP      recording was down {} ({lost} records lost): {reason}",
                crate::time::HumanDuration(std::time::Duration::from_millis(*down_ms))
            ),
        }
    }
}

/// A short, greppable rendering of an event. Deliberately not `Debug`: this is
/// read by humans scanning for the moment something changed.
#[must_use]
pub fn describe_event(event: &Event) -> String {
    match event {
        Event::AppSeen(s) => format!("app-seen {} ({} pids)", s.key.label(), s.pids.len()),
        Event::AppGone { app } => format!("app-gone {}", app.label()),
        Event::VisibilityChanged { app, visibility } => {
            format!("visibility {} -> {}", app.label(), visibility.as_str())
        }
        Event::ActivationRequested { app } => format!("activation {}", app.label()),
        Event::Sampled { app, sample } => format!(
            "sampled {} cpu {:.1}% wakeups {:.0}/s",
            app.label(),
            sample.cpu_pct,
            sample.wakeups_per_sec
        ),
        Event::ExemptionChanged { app, exemption, active } => format!(
            "exemption {} {} {}",
            app.label(),
            exemption.as_str(),
            if *active { "on" } else { "off" }
        ),
        Event::ActivityChanged { desktop } => format!("desktop {}", desktop.as_str()),
        Event::ThermalChanged { thermal } => format!("thermal {}", thermal.as_str()),
        Event::PowerChanged { power } => match power.percentage {
            Some(pct) => {
                format!("power {} {pct:.0}%", if power.on_ac { "on-ac" } else { "on-battery" })
            }
            None => format!("power {}", if power.on_ac { "on-ac" } else { "on-battery" }),
        },
        Event::FocusSourceHealth { healthy } => {
            format!("focus-source {}", if *healthy { "healthy" } else { "LOST" })
        }
        Event::PrepareForSleep { sleeping } => {
            format!("sleep {}", if *sleeping { "suspending" } else { "woke" })
        }
        Event::ConfigChanged { config } => {
            format!("config reloaded ({} rules)", config.rules.len())
        }
        Event::ModeChanged { mode } => format!("mode {}", mode.as_str()),
        Event::UserAction { app, action } => format!("user {} {}", action.verb(), app.label()),
        Event::ThawAll => "thaw-all".to_owned(),
        Event::Shutdown => "shutdown".to_owned(),
        Event::Tick => "tick".to_owned(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("not a dozed trace: the first line is not a header")]
    MissingHeader,
    #[error("trace is version {found}, this build understands {TRACE_VERSION}")]
    Version { found: u32 },
}

/// What a replay produced, next to what was recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayResult {
    pub header: TraceHeader,
    /// Whether the recorded config was replaced with a different one.
    pub config_replaced: bool,
    pub events_replayed: usize,
    pub recorded: Vec<Intent>,
    pub produced: Vec<Intent>,
    /// Per app, and only for apps whose stream changed.
    pub diffs: Vec<AppDiff>,
    /// How many stretches of this recording are missing
    /// ([`TraceRecord::Gap`]).
    ///
    /// Load-bearing rather than informational. `dozectl replay` with no
    /// `--config` asserts that a trace replays to its own recorded intent stream
    /// and **exits non-zero when it does not**, because a drift there is a bug in
    /// the dry-run seam rather than a tuning result. A trace with a hole in it
    /// diverges for a completely different reason — events that were never
    /// written cannot be replayed — and reporting that as drift would send
    /// somebody hunting a bug that is not there.
    pub gaps: usize,
}

impl ReplayResult {
    #[must_use]
    pub fn identical(&self) -> bool {
        self.diffs.is_empty()
    }

    /// Whether this replay is entitled to be believed as a reproduction.
    ///
    /// Distinct from [`Self::identical`], and the distinction is the point: a
    /// gapped trace that replays identically is a coincidence rather than a
    /// proof, and a gapped trace that does not is not evidence of anything. The
    /// self-check that guards the seam asks this first.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.gaps == 0
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "replayed {} events from a trace of {} intents\n",
            self.events_replayed,
            self.recorded.len()
        );
        if self.config_replaced {
            out.push_str("against a replacement config\n");
        }
        if self.gaps > 0 {
            // Before the verdict, not after it. Whatever the next line says, it
            // is a statement about an incomplete recording, and a reader who
            // learns that second has already believed the first.
            out.push_str(&format!(
                "\nthis recording has {} gap(s): the daemon could not write for a while,\n                 so events are missing and anything below is about what survived\n",
                self.gaps
            ));
        }
        if self.identical() {
            out.push_str(&format!(
                "\nno change: {} intents, identical to the recording\n",
                self.produced.len()
            ));
            return out;
        }
        out.push_str(&format!("\n{} app(s) would behave differently:\n", self.diffs.len()));
        for d in &self.diffs {
            out.push_str(&format!("\n  {}\n", d.app.label()));
            for line in &d.lines {
                out.push_str(&format!("    {line}\n"));
            }
        }
        out
    }
}

/// How one app's intent stream changed.
#[derive(Debug, Clone, PartialEq)]
pub struct AppDiff {
    pub app: AppKey,
    pub was: Vec<Intent>,
    pub now: Vec<Intent>,
    /// Rendered `-`/`+` lines, aligned by position within this app.
    pub lines: Vec<String>,
}

/// A replay, plus what the resulting session would have been worth.
///
/// The accounting rides the same loop as the diff rather than getting one of its
/// own, because two simulations of one recording is two things to keep in step —
/// and `identical_config_reproduces_the_stream` only guards one of them. Tuning
/// (`crate::tune`) scores candidates from this, so if it drifted, every
/// suggestion would be about a session that never happened.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredReplay {
    pub result: ReplayResult,
    /// What the replayed intent stream would have saved, from the samples the
    /// trace already contains.
    pub report: crate::journal::Report,
    /// Holds — freeze or throttle — the replayed config would have applied.
    pub holds: u32,
    /// Restores. Each costs a redraw, and for a chat app a reconnect (§7).
    pub thaws: u32,
    /// Times the user switched to an app that was being held, and had to wait
    /// for it.
    ///
    /// The cost side of tuning, and the reason a search over savings can be
    /// trusted: wakeups avoided always rises as the config gets more aggressive,
    /// so an objective made only of savings recommends freezing everything
    /// instantly. This is the number that says what that would feel like.
    ///
    /// Counted at the moment of *return*, with no regard for how long the hold
    /// had been running. The first version asked instead whether the hold was
    /// younger than a minute — "did it buy enough quiet to be worth the thaw" —
    /// and that turned out to be the wrong shape for an objective, in two ways
    /// measured on a real 2h53m recording:
    ///
    /// * **It ran backwards.** `grace x1.50` holds *less* than the config it was
    ///   compared against — 26 holds where the baseline made 29 — and scored 8
    ///   contradicted holds against the baseline's 5. A cost that rises as the
    ///   tool gets *more* patient cannot constrain a search for aggression.
    /// * **The search optimised its edge.** Signal was held at 8965.5s and the
    ///   user came back at 9024.9s, 59.4s later: inside the window, counted.
    ///   Shorten every grace by a quarter and the same hold starts at 8957.2s,
    ///   the same user returns at the same instant to the same held app — 67.7s
    ///   later, outside the window, not counted. The candidate held the app
    ///   eight seconds *longer* and scored one interruption better. With only
    ///   five in the whole session, that step across a threshold was a fifth of
    ///   the entire cost side.
    ///
    /// Both faults come from mixing benefit into a cost term: hold age is a
    /// statement about what the hold bought, and what it bought is already the
    /// other axis. Counting the wait alone leaves the two independent, and makes
    /// the number monotone in aggression — 16 waits at `grace x3.00` rising to
    /// 30 at `grace x0.25` on the same recording, which is the property that
    /// makes it usable as a constraint.
    ///
    /// Not the same as [`Self::thaws`], which §14.1 declines to constrain for
    /// good reason: a config that dozes more does thaw more, so bounding thaws
    /// at the baseline rejects every improvement. A config can hold *more often*
    /// without making the user wait more often, if the extra holds land in
    /// stretches they never came back to — and on that recording two candidates
    /// did exactly that, adding dozing at 23 waits against the baseline's 23.
    pub interruptions: u32,
}

/// Re-run a recorded trace through a fresh planner.
///
/// With `config` set, the replacement is used for the whole session — including
/// in place of any recorded `ConfigChanged`. That is the question being asked
/// ("what if I had run all day with this config?"), but it does mean a trace
/// spanning a config reload replays as though the reload never happened, and the
/// caller should say so.
pub fn replay(records: &[TraceRecord], config: Option<Config>) -> Result<ReplayResult, TraceError> {
    replay_scored(records, config).map(|s| s.result)
}

/// [`replay`], keeping the accounting as well as the diff.
pub fn replay_scored(
    records: &[TraceRecord],
    config: Option<Config>,
) -> Result<ScoredReplay, TraceError> {
    let Some(TraceRecord::Header(header)) = records.first() else {
        return Err(TraceError::MissingHeader);
    };
    if header.version != TRACE_VERSION {
        return Err(TraceError::Version { found: header.version });
    }

    let config_replaced = config.is_some();
    let effective = config.unwrap_or_else(|| (*header.config).clone());
    let sample_window = effective.defaults.cpu_sample_window.get();
    let mut world = WorldState::new(effective.clone());
    let mut planner = Planner::new();
    let mut accumulator = crate::journal::ReportAccumulator::new();

    let mut produced = Vec::new();
    let mut recorded = Vec::new();
    let mut events_replayed = 0;
    let mut last = MonoTime::ZERO;
    // Per app, whether we have it held and the user has not yet come back to it.
    let mut held: BTreeSet<AppKey> = BTreeSet::new();
    let (mut holds, mut thaws, mut interruptions) = (0, 0, 0);
    let mut gaps = 0;

    for record in records {
        match record {
            TraceRecord::Event { at, event, .. } => {
                events_replayed += 1;
                last = *at;
                // A recorded reload would otherwise undo the replacement halfway
                // through and answer a question nobody asked.
                let event = match (config_replaced, event.as_ref()) {
                    (true, Event::ConfigChanged { .. }) => {
                        Event::ConfigChanged { config: Box::new(effective.clone()) }
                    }
                    (_, e) => e.clone(),
                };
                world.apply(&event, *at);

                // The user coming back to an app we had just put down. Counted
                // from the *events*, because the intent stream cannot say
                // whether a restore was the user's idea or the planner's.
                let returned_to = match &event {
                    Event::VisibilityChanged { app, visibility } if visibility.is_on_screen() => {
                        Some(app)
                    }
                    Event::ActivationRequested { app } => Some(app),
                    _ => None,
                };
                // One hold is one wait however many times the window is raised:
                // the return is what causes the thaw, so a second return is to
                // an app that is already coming back.
                if let Some(app) = returned_to
                    && held.remove(app)
                {
                    interruptions += 1;
                }

                if let Event::Sampled { app, sample } = &event {
                    // The same gate the daemon applies: a sample is only a
                    // baseline for "what leaving this app alone costs" once the
                    // app has been hidden for a whole averaging window, or the
                    // window still contains the user working in it.
                    let settled = world.app(app).is_some_and(|a| {
                        a.visibility == crate::event::Visibility::Hidden
                            && at.since(a.visibility_since) >= sample_window
                    });
                    accumulator.on_sample(
                        app,
                        sample.cpu_pct,
                        sample.wakeups_per_sec,
                        *at,
                        settled,
                        sample_window,
                    );
                }

                for intent in planner.plan(&world, &event, *at) {
                    if intent.action.is_restore() {
                        thaws += 1;
                        held.remove(&intent.app);
                    } else {
                        holds += 1;
                        held.insert(intent.app.clone());
                    }
                    // `WouldHave`, because a replay is a counterfactual by
                    // definition however the recording was made — and the
                    // accumulator's arithmetic differs between the two (a live
                    // hold's observed activity is leakage, a counterfactual
                    // one's is the saving).
                    accumulator.on_intent(
                        &intent.app,
                        intent.action,
                        &Outcome::WouldHave,
                        intent.at,
                    );
                    produced.push(intent);
                }
            }
            TraceRecord::Intent { intent, .. } => recorded.push((**intent).clone()),
            // Counted, not replayed. There is nothing in a gap to feed the
            // planner — that is what makes it a gap — but a replay that crossed
            // one without saying so would report the resulting divergence as
            // drift in the seam, which is a bug claim, when the true answer is
            // that the recording is missing a stretch. See `ReplayResult::gaps`.
            // Only the kinds that actually lose records. A rotation is a seam
            // between two files with nothing missing between them, and counting
            // it here would have `complete()` report an incomplete recording
            // for a recorder that worked perfectly — the same false alarm, in
            // the opposite direction, as replaying across a real hole.
            TraceRecord::Gap { kind, .. } if kind.loses_records() => gaps += 1,
            TraceRecord::Gap { .. } => {}
            TraceRecord::Header(_) | TraceRecord::Decision { .. } => {}
        }
    }

    let diffs = diff_by_app(&recorded, &produced);
    let mut report = accumulator.report(last, true);
    report.window = last.since(MonoTime::ZERO);
    Ok(ScoredReplay {
        result: ReplayResult {
            header: (**header).clone(),
            config_replaced,
            events_replayed,
            recorded,
            produced,
            diffs,
            gaps,
        },
        report,
        holds,
        thaws,
        interruptions,
    })
}

/// Diff two intent streams, grouped by app.
///
/// Per app rather than globally on purpose: a single extra intent early in the
/// day would shift every later one and turn a global positional diff into noise,
/// while an app's own stream stays short and aligned. It is also how a person
/// reads the result — "Slack changed, Discord did not".
#[must_use]
pub fn diff_by_app(recorded: &[Intent], produced: &[Intent]) -> Vec<AppDiff> {
    let mut by_app: BTreeMap<AppKey, (Vec<Intent>, Vec<Intent>)> = BTreeMap::new();
    for i in recorded {
        by_app.entry(i.app.clone()).or_default().0.push(i.clone());
    }
    for i in produced {
        by_app.entry(i.app.clone()).or_default().1.push(i.clone());
    }

    by_app
        .into_iter()
        .filter_map(|(app, (was, now))| {
            let lines = diff_lines(&was, &now);
            if lines.is_empty() {
                return None;
            }
            Some(AppDiff { app, was, now, lines })
        })
        .collect()
}

fn diff_lines(was: &[Intent], now: &[Intent]) -> Vec<String> {
    let mut lines = Vec::new();
    for i in 0..was.len().max(now.len()) {
        match (was.get(i), now.get(i)) {
            (Some(a), Some(b)) if same_intent(a, b) => {}
            (Some(a), Some(b)) => {
                lines.push(format!("- {}", summarise(a)));
                lines.push(format!("+ {}", summarise(b)));
            }
            (Some(a), None) => lines.push(format!("- {}", summarise(a))),
            (None, Some(b)) => lines.push(format!("+ {}", summarise(b))),
            (None, None) => unreachable!("index is bounded by the longer stream"),
        }
    }
    lines
}

/// Equality for diffing: what was done, when, and which rule decided it.
///
/// Not `PartialEq` on the whole `Intent`, which would also compare the captured
/// inputs and the prose. Those can differ for reasons that are not a behaviour
/// change, and a tuning diff that flags them is a diff nobody trusts.
fn same_intent(a: &Intent, b: &Intent) -> bool {
    a.at == b.at
        && a.action == b.action
        && a.reason.kind == b.reason.kind
        && a.reason.rule == b.reason.rule
}

fn summarise(i: &Intent) -> String {
    format!("{} {} — {}", i.at, i.action.verb(), i.reason.summary())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::{Predicates, Rule, RuleAction};
    use crate::event::Visibility;
    use crate::testing::Scenario;

    const MIN: Duration = Duration::from_secs(60);

    fn freeze_after(grace: &str) -> Config {
        let mut cfg = Config::empty();
        cfg.rules = vec![Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some(grace.parse().unwrap()),
            ..Default::default()
        })];
        cfg
    }

    /// The events `Scenario::new` feeds before a test says anything.
    fn prelude() -> Vec<Event> {
        vec![
            Event::FocusSourceHealth { healthy: true },
            Event::PowerChanged { power: crate::testing::ON_BATTERY },
        ]
    }

    /// A session where Slack goes hidden and stays hidden for ten minutes.
    fn a_hidden_slack() -> Vec<Event> {
        let slack = AppKey::comm("slack");
        let mut events = vec![crate::testing::seen("slack")];
        events
            .push(Event::VisibilityChanged { app: slack.clone(), visibility: Visibility::Hidden });
        events.extend((0..10).map(|_| Event::Tick));
        events
    }

    /// Capture that session as a trace, along exactly the path the daemon takes:
    /// the event, then any derivation it changed, then any intent it produced.
    fn record_hidden_slack(config: Config) -> Vec<TraceRecord> {
        let mut s2 = Scenario::new(config.clone());
        let mut out = vec![TraceRecord::Header(Box::new(TraceHeader {
            version: TRACE_VERSION,
            dozed_version: "test".into(),
            instance: "test-1".into(),
            wall_start: 1_700_000_000,
            live: false,
            config: Box::new(config),
        }))];
        let mut seq = 0;
        let mut emit = |out: &mut Vec<TraceRecord>, s2: &mut Scenario, event: Event| {
            let at = s2.now;
            out.push(TraceRecord::Event { seq, at, event: Box::new(event.clone()) });
            seq += 1;
            s2.world.apply(&event, at);
            let intents = s2.planner.plan(&s2.world, &event, at);
            for (app, decision) in s2.planner.take_decisions() {
                out.push(TraceRecord::Decision { seq, at, app, decision: Box::new(decision) });
                seq += 1;
            }
            for intent in intents {
                out.push(TraceRecord::Intent {
                    seq,
                    at,
                    intent: Box::new(intent),
                    outcome: Outcome::WouldHave,
                });
                seq += 1;
            }
        };
        for event in prelude() {
            emit(&mut out, &mut s2, event);
        }
        for event in a_hidden_slack() {
            if event == Event::Tick {
                s2.now = s2.now.plus(MIN);
            }
            emit(&mut out, &mut s2, event);
        }
        out
    }

    #[test]
    fn identical_config_reproduces_the_stream() {
        // The acceptance criterion for §14: a replay against the recorded config
        // must not drift by so much as one intent, or nothing built on top of it
        // means anything.
        let trace = record_hidden_slack(freeze_after("5m"));
        let result = replay(&trace, None).unwrap();

        assert!(!result.recorded.is_empty(), "the recording must contain something to check");
        assert!(result.identical(), "{}", result.render());
        assert_eq!(result.produced, result.recorded);
        assert!(result.render().contains("identical to the recording"));
    }

    #[test]
    fn a_gap_is_counted_and_a_complete_recording_is_not() {
        // The distinction `dozectl replay`'s self-check turns on. A trace that
        // reproduces itself is evidence about the seam; a trace with a hole
        // cannot reproduce itself and is evidence about the disk, and reporting
        // the second as the first sends somebody hunting a bug that is not there.
        let trace = record_hidden_slack(freeze_after("5m"));
        let clean = replay(&trace, None).unwrap();
        assert!(clean.complete(), "an ordinary recording has no gaps");
        assert_eq!(clean.gaps, 0);
        assert!(!clean.render().contains("gap"));

        // The same session, with the recorder having been down in the middle.
        let mut gapped = trace.clone();
        let at = gapped.get(2).and_then(TraceRecord::at).unwrap_or(MonoTime::ZERO);
        gapped.insert(
            2,
            TraceRecord::Gap {
                seq: 999,
                at,
                kind: GapKind::Hole,
                down_ms: 11 * 60 * 1000,
                lost: 42,
                reason: "flush failed: No space left on device (os error 28)".into(),
            },
        );
        let holed = replay(&gapped, None).unwrap();
        assert_eq!(holed.gaps, 1);
        assert!(!holed.complete());
        // Said before the verdict, because a reader who learns it afterwards has
        // already believed the verdict.
        let text = holed.render();
        assert!(text.contains("gap(s)"), "{text}");
        let gap_line = text.find("gap(s)").unwrap();
        let verdict = text.find("no change").or_else(|| text.find("behave differently")).unwrap();
        assert!(gap_line < verdict, "the caveat has to come first:\n{text}");

        // A gap record is not an event and must not be replayed as one: the
        // intent stream either side of it is unchanged.
        assert_eq!(holed.produced, clean.produced, "a marker is not a thing that happened");
    }

    #[test]
    fn a_looser_rule_shows_up_as_a_removed_freeze() {
        let trace = record_hidden_slack(freeze_after("5m"));
        // The same day, but with a grace period the session never reaches.
        let result = replay(&trace, Some(freeze_after("30m"))).unwrap();

        assert!(!result.identical(), "a rule that can no longer fire must show as a difference");
        assert!(result.produced.is_empty(), "nothing fires within the recorded ten minutes");
        let diff = &result.diffs[0];
        assert_eq!(diff.app, AppKey::comm("slack"));
        assert!(diff.lines.iter().any(|l| l.starts_with("- ") && l.contains("doze")), "{diff:?}");
        assert!(result.render().contains("would behave differently"));
    }

    #[test]
    fn a_tighter_rule_shows_up_as_an_added_freeze() {
        let trace = record_hidden_slack(freeze_after("30m"));
        let result = replay(&trace, Some(freeze_after("2m"))).unwrap();

        assert!(!result.identical());
        assert!(result.recorded.is_empty(), "the recorded day froze nothing");
        assert!(result.diffs[0].lines.iter().any(|l| l.starts_with("+ ")), "{:?}", result.diffs);
    }

    #[test]
    fn a_replacement_config_survives_a_recorded_reload() {
        // Otherwise the reload halfway through the day silently undoes the
        // substitution and the answer is about the old rules again.
        let mut trace = record_hidden_slack(freeze_after("30m"));
        trace.push(TraceRecord::Event {
            seq: 9999,
            at: MonoTime::from_millis(11 * 60_000),
            event: Box::new(Event::ConfigChanged { config: Box::new(freeze_after("30m")) }),
        });
        trace.push(TraceRecord::Event {
            seq: 10_000,
            at: MonoTime::from_millis(12 * 60_000),
            event: Box::new(Event::Tick),
        });

        let result = replay(&trace, Some(freeze_after("2m"))).unwrap();
        assert!(result.config_replaced);
        assert!(
            result.produced.iter().any(|i| i.reason.rule.as_deref() == Some("bg-idle")),
            "the replacement must still be in force after the reload: {:?}",
            result.produced
        );
    }

    #[test]
    fn a_trace_without_a_header_is_refused() {
        let err = replay(&[], None).unwrap_err();
        assert!(matches!(err, TraceError::MissingHeader));
    }

    #[test]
    fn a_future_trace_version_is_refused_rather_than_guessed_at() {
        let trace = vec![TraceRecord::Header(Box::new(TraceHeader {
            version: TRACE_VERSION + 1,
            dozed_version: "test".into(),
            instance: "x".into(),
            wall_start: 0,
            live: false,
            config: Box::new(Config::empty()),
        }))];
        assert!(matches!(replay(&trace, None), Err(TraceError::Version { .. })));
    }

    #[test]
    fn records_roundtrip_through_json_lines() {
        // The on-disk format is the API for every tool that reads a trace.
        for record in record_hidden_slack(freeze_after("5m")) {
            let line = serde_json::to_string(&record).unwrap();
            assert!(!line.contains('\n'), "one record per line");
            let back: TraceRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(back, record);
        }
    }

    #[test]
    fn sequence_numbers_order_the_whole_stream() {
        // Reading a trace is only useful if event, derivation and consequence
        // can be put back in the order they happened.
        let trace = record_hidden_slack(freeze_after("5m"));
        let seqs: Vec<u64> = trace.iter().filter_map(TraceRecord::seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        assert!(matches!(trace[0], TraceRecord::Header(_)));
    }

    #[test]
    fn the_trace_records_derivations_that_produced_no_intent() {
        // The whole point: an app waiting out grace leaves nothing in the intent
        // stream, and that is the case people need to debug.
        let trace = record_hidden_slack(freeze_after("30m"));
        assert!(
            trace.iter().all(|r| !matches!(r, TraceRecord::Intent { .. })),
            "nothing fires in this session"
        );
        let waits: Vec<&TraceRecord> =
            trace.iter().filter(|r| matches!(r, TraceRecord::Decision { .. })).collect();
        assert!(!waits.is_empty(), "but the waiting must still be on the record");
        assert!(waits.iter().any(|r| r.line().contains("waiting")), "{waits:?}");
    }

    #[test]
    fn every_event_renders_a_greppable_line() {
        for record in record_hidden_slack(freeze_after("5m")) {
            let line = record.line();
            assert!(!line.trim().is_empty());
            assert!(!line.contains('\n'));
        }
        assert_eq!(describe_event(&Event::Tick), "tick");
        assert!(describe_event(&Event::ThawAll).contains("thaw"));
    }
}
