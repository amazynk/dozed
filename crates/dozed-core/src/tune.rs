//! Turning replay from a diff into a search.
//!
//! §14's tuning story is "tune against your own recorded day instead of
//! guessing", and `dozectl replay --config new.toml` delivers half of it: you
//! write a candidate, it tells you exactly how the day would have gone
//! differently. The missing half is that **you still had to invent the
//! candidate**, and the interesting knobs are multiplicative factors on a grace
//! product (§8) whose effect on a real session nobody can predict by reading it.
//!
//! The planner is pure and its clock is injected, so a recorded day can be
//! re-simulated as often as you like. This module does that a few dozen times and
//! reports what it found.
//!
//! ## The part that needs care is the objective, not the search
//!
//! Wakeups avoided rises monotonically as the config gets more aggressive. Freeze
//! everything the instant it loses focus and you will avoid the most wakeups any
//! config can — and the desktop will be unusable. So a search over savings alone
//! does not need to be clever to be wrong; it needs only to be run.
//!
//! The cost side is [`ScoredReplay::interruptions`]: a time the user switched to
//! an app that was being held and had to wait for it. That is the thing a person
//! actually complains about — "I switched to Slack and waited for it to wake up"
//! — and it is measurable from a recording, because the trace contains the moment
//! they came back.
//!
//! The recommendation is therefore constrained rather than weighted: **no more
//! interruptions than the config you already run**, and among those, the most
//! wakeups avoided. No arbitrary exchange rate between a wakeup and an
//! interruption, which is the number nobody could defend.
//!
//! ## The savings half needs a recording in which nothing was held
//!
//! It is projected from what the trace observed each app doing while a candidate
//! would have held it. If nothing was ever actually held, every app was observed
//! running throughout and the projection has something to project from. If the
//! daemon really did freeze things, an app reads as silent for exactly the
//! stretches it was frozen — and a candidate is credited only for the sliver
//! where it would have acted sooner than the recording did. That is a marginal
//! quantity wearing the units of a total: it can only ever reward aggression, and
//! it cannot score a candidate more patient than the recording at all.
//!
//! So [`search`] refuses to recommend from such a recording rather than answering
//! confidently from arithmetic that does not apply — see [`Suggestions::advice`].
//! The interruption count is unaffected and still reported, because it comes from
//! the replayed intent stream and the recorded events rather than from the
//! samples.
//!
//! Thaws are reported and deliberately *not* constrained. A config that dozes
//! more will thaw more, so bounding thaws at the baseline would reject every
//! improvement; the count is shown because a chat app pays a reconnect for each
//! one (§7) and that is a judgement for the person reading, not for the search.
//!
//! ## What it will not do
//!
//! It will not invent rules, reorder them, or touch the exemptions. Every
//! candidate is the config you already have with one or more scalar factors
//! moved, so the output is a handful of numbers to change and the safety net is
//! untouched by construction.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{Config, Threshold};
use crate::intent::Outcome;
use crate::journal::Report;
use crate::time::{HumanDuration, MonoTime};
use crate::trace::{TraceError, TraceRecord, replay_scored};

/// One scalar knob, moved by a factor.
///
/// Factors rather than absolute values, because the interesting settings are
/// already relative to each other — a rule's `for` against the default base
/// grace — and scaling preserves the relationships the user set up on purpose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tweak {
    /// Every grace period: `defaults.base_grace` and each rule's `for`.
    Grace(f64),
    /// `activity.active_factor`, the anti-thrash multiplier that applies while
    /// the user is at the keyboard (§8).
    ActiveFactor(f64),
    /// Every `wakeups_per_sec` bound a rule demands before it will act.
    ///
    /// Lowering it makes rules reach quieter apps; raising it confines them to
    /// the genuinely noisy ones.
    WakeupThreshold(f64),
    /// `defaults.resume_cooldown`, the minimum wait after the user came back.
    ResumeCooldown(f64),
}

impl Tweak {
    /// The knob, without its value, for grouping a coordinate search.
    const fn knob(self) -> &'static str {
        match self {
            Self::Grace(_) => "grace",
            Self::ActiveFactor(_) => "active-factor",
            Self::WakeupThreshold(_) => "wakeup-threshold",
            Self::ResumeCooldown(_) => "resume-cooldown",
        }
    }

    const fn factor(self) -> f64 {
        match self {
            Self::Grace(f)
            | Self::ActiveFactor(f)
            | Self::WakeupThreshold(f)
            | Self::ResumeCooldown(f) => f,
        }
    }

    /// Whether this changes nothing, so the baseline is not listed twice.
    fn is_identity(self) -> bool {
        (self.factor() - 1.0).abs() < f64::EPSILON
    }

    fn apply(self, config: &mut Config) {
        match self {
            Self::Grace(f) => {
                config.defaults.base_grace = scale_duration(config.defaults.base_grace, f);
                for rule in &mut config.rules {
                    // Both clauses: `Rule::grace` reads `when` then `match`, and
                    // a config that puts `for` in the identity clause would
                    // otherwise be silently untunable.
                    for slot in [&mut rule.when.for_, &mut rule.match_.for_] {
                        if let Some(d) = slot {
                            *slot = Some(scale_duration(*d, f));
                        }
                    }
                }
            }
            Self::ActiveFactor(f) => config.activity.active_factor *= f,
            Self::WakeupThreshold(f) => {
                for rule in &mut config.rules {
                    for slot in [&mut rule.when.wakeups_per_sec, &mut rule.match_.wakeups_per_sec] {
                        if let Some(t) = slot {
                            *slot = Some(scale_threshold(*t, f));
                        }
                    }
                }
            }
            Self::ResumeCooldown(f) => {
                config.defaults.resume_cooldown =
                    scale_duration(config.defaults.resume_cooldown, f);
            }
        }
    }

    /// How this reads in the recommendation, e.g. `grace x0.50`.
    #[must_use]
    pub fn describe(self) -> String {
        format!("{} x{:.2}", self.knob(), self.factor())
    }
}

/// Seconds, and never below one.
///
/// A zero grace is a legitimate policy — act the moment an app is hidden — but it
/// is not one a *scaling* factor should be able to arrive at by rounding, and a
/// config full of `for = "0s"` is indistinguishable from a config with the field
/// deleted. Whole seconds because the result is written back into a TOML file
/// somebody has to read.
fn scale_duration(d: HumanDuration, factor: f64) -> HumanDuration {
    let secs = (d.get().as_secs_f64() * factor).round().max(1.0);
    HumanDuration(Duration::from_secs(secs as u64))
}

fn scale_threshold(t: Threshold, factor: f64) -> Threshold {
    let f = factor as f32;
    match t {
        Threshold::Above(v) => Threshold::Above(v * f),
        Threshold::Bounds { gt, lt } => {
            Threshold::Bounds { gt: gt.map(|v| v * f), lt: lt.map(|v| v * f) }
        }
    }
}

/// What one candidate config would have done to the recorded day.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub wakeups_avoided: f64,
    pub cpu_seconds_avoided: f64,
    pub dozed: Duration,
    pub holds: u32,
    pub thaws: u32,
    pub interruptions: u32,
    /// How many apps were dozed at all. A config that saves a great deal from one
    /// app is a different proposition from one that saves it from six.
    pub apps: usize,
}

impl Score {
    fn from_report(report: &Report, holds: u32, thaws: u32, interruptions: u32) -> Self {
        Self {
            wakeups_avoided: report.wakeups,
            cpu_seconds_avoided: report.cpu_seconds,
            dozed: report.total_dozed,
            holds,
            thaws,
            interruptions,
            apps: report.apps.len(),
        }
    }
}

/// One point in the search: a config, how it got there, and what it did.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Empty for the baseline.
    pub tweaks: Vec<Tweak>,
    pub score: Score,
    pub config: Config,
}

impl Candidate {
    #[must_use]
    pub fn label(&self) -> String {
        if self.tweaks.is_empty() {
            return "as configured".to_owned();
        }
        self.tweaks.iter().map(|t| t.describe()).collect::<Vec<_>>().join(", ")
    }
}

/// The result of a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestions {
    /// How long the recording covers, for the header.
    pub window: Duration,
    pub events: usize,
    pub baseline: Candidate,
    /// Every candidate evaluated, best saving first, baseline included.
    pub candidates: Vec<Candidate>,
    /// Index into `candidates` of the one being recommended, if any beat the
    /// baseline within its interruption budget.
    ///
    /// Always `None` when [`Self::holds_applied`] is set and the caller did not
    /// ask for an answer anyway: the savings the ranking rests on are not
    /// measurable from a live recording (see the module docs).
    pub recommended: Option<usize>,
    /// Candidates on the saving/interruption frontier: no other candidate both
    /// saves more *and* interrupts less. Offered so someone who does not share
    /// the recommendation's constraint can pick their own trade.
    pub frontier: Vec<usize>,
    /// Whether the recording contains a hold that was really applied — an app
    /// that was actually silenced, and so cannot be observed running.
    ///
    /// Taken from the recorded outcomes rather than the header's mode, which
    /// only says how the daemon *started*.
    pub holds_applied: bool,
    /// Whether the caller asked for a recommendation from a live recording
    /// regardless. Recorded so the rendering can keep saying what the number is
    /// worth rather than quietly dropping the caveat.
    pub forced: bool,
}

/// The ladders. Coarse on purpose: a trace is one day of one person's habits, and
/// resolving a factor to three decimal places against it is fitting noise.
fn ladders() -> Vec<Vec<Tweak>> {
    vec![
        vec![0.25, 0.5, 0.75, 1.5, 2.0, 3.0].into_iter().map(Tweak::Grace).collect(),
        vec![0.25, 0.5, 2.0].into_iter().map(Tweak::ActiveFactor).collect(),
        vec![0.25, 0.5, 2.0, 4.0].into_iter().map(Tweak::WakeupThreshold).collect(),
        vec![0.5, 2.0].into_iter().map(Tweak::ResumeCooldown).collect(),
    ]
}

/// Search a recorded day for a better config than the one it ran under.
///
/// `base` is the config to tune, defaulting to the one recorded in the trace —
/// which is the right default, since the question is "could my own config be
/// better" rather than "is some other config better than a config I do not run".
///
/// Coordinate descent, one knob at a time, keeping each improvement before moving
/// on — followed by a sweep of everything evaluated, because the descent walks a
/// path and the attribution pass has already paid for points beside it.
///
/// `force` asks for a recommendation even from a live recording, where the
/// savings arithmetic does not apply. It is spelled out at every call site rather
/// than defaulted, and the caveat survives into the rendering either way.
pub fn search(
    records: &[TraceRecord],
    base: Option<Config>,
    force: bool,
) -> Result<Suggestions, TraceError> {
    if !matches!(records.first(), Some(TraceRecord::Header(_))) {
        return Err(TraceError::MissingHeader);
    }
    // Evidence, not the header's declaration. The header records the mode the
    // daemon *started* in, and `dozectl mode` changes the running one without
    // rotating the trace — so a session told to stop acting halfway through
    // still calls itself live, and one started live that never found anything to
    // hold calls itself live while being perfectly observable throughout.
    //
    // What actually breaks the arithmetic is an app having been silenced, so
    // that is what to look for: a hold that was really applied. A restore does
    // not count even though it implies one, because a freeze that failed left
    // the app running and therefore measurable.
    let holds_applied = records.iter().any(|r| {
        matches!(r, TraceRecord::Intent { intent, outcome, .. }
            if matches!(outcome, Outcome::Applied) && !intent.action.is_restore())
    });
    let evaluate = |config: &Config| -> Result<(Score, usize, Duration), TraceError> {
        let scored = replay_scored(records, Some(config.clone()))?;
        Ok((
            Score::from_report(&scored.report, scored.holds, scored.thaws, scored.interruptions),
            scored.result.events_replayed,
            scored.report.window,
        ))
    };

    // The baseline is replayed through the same path as every candidate, not read
    // out of the recording's own intents. Otherwise the comparison would be
    // against what the daemon *did*, which includes actuator refusals and a
    // config reload halfway through, and a candidate could look better for
    // reasons that have nothing to do with its rules.
    let base_config = match base {
        Some(c) => c,
        None => match records.first() {
            Some(TraceRecord::Header(h)) => (*h.config).clone(),
            _ => return Err(TraceError::MissingHeader),
        },
    };
    let (base_score, events, window) = evaluate(&base_config)?;
    let baseline =
        Candidate { tweaks: Vec::new(), score: base_score.clone(), config: base_config.clone() };

    let mut candidates = vec![baseline.clone()];
    // Keyed on the path of tweaks, purely to avoid replaying a day twice: the
    // attribution pass below and the first step of the descent ask the same
    // question about the first ladder.
    let mut seen: BTreeMap<Vec<String>, Score> = BTreeMap::new();
    let mut evaluate_from = |from: &Candidate,
                             tweak: Tweak,
                             candidates: &mut Vec<Candidate>|
     -> Result<Option<Candidate>, TraceError> {
        let mut tweaks = from.tweaks.clone();
        tweaks.push(tweak);
        let mut config = from.config.clone();
        tweak.apply(&mut config);
        // A factor can produce a config the validator would reject — a wakeup
        // bound scaled past a `lt` it has to stay under, say. Skipping it is
        // right: a suggestion nobody could load is not a suggestion.
        if config.clone().validate().is_err() {
            return Ok(None);
        }
        let key: Vec<String> = tweaks.iter().map(|t| t.describe()).collect();
        let score = match seen.get(&key) {
            Some(s) => s.clone(),
            None => {
                let (s, ..) = evaluate(&config)?;
                seen.insert(key, s.clone());
                let candidate = Candidate { tweaks: tweaks.clone(), score: s.clone(), config };
                candidates.push(candidate.clone());
                return Ok(Some(candidate));
            }
        };
        Ok(Some(Candidate { tweaks, score, config }))
    };

    // Attribution first: every value of every knob against the baseline *alone*.
    //
    // Not needed to find the recommendation, and worth the extra dozen replays
    // anyway. Coordinate descent makes each ladder after the first carry the
    // winners of the ones before, so without this pass there is no way to say
    // which knob did the work — and "shorten the grace" is a change a person can
    // reason about, while "these four factors together" is one they can only
    // accept or reject.
    for ladder in ladders() {
        for tweak in ladder {
            if !tweak.is_identity() {
                evaluate_from(&baseline, tweak, &mut candidates)?;
            }
        }
    }

    let mut best = baseline.clone();
    for ladder in ladders() {
        let mut improved: Option<Candidate> = None;
        for tweak in ladder {
            if tweak.is_identity() {
                continue;
            }
            let Some(candidate) = evaluate_from(&best, tweak, &mut candidates)? else { continue };
            if beats(&candidate.score, &improved.as_ref().unwrap_or(&best).score, &base_score) {
                improved = Some(candidate);
            }
        }
        if let Some(better) = improved {
            best = better;
        }
    }

    // The descent is a path through the space, and every candidate beside it has
    // already been replayed by the attribution pass — so not sweeping them was
    // leaving the answer on the table rather than saving any work.
    //
    // It could also be *printed* and not recommended, which is the part that made
    // it a bug rather than a limitation. Measured on a 2h53m recording: the
    // descent took `grace x0.75` from the first ladder, then found every
    // active-factor rung *combined with it* over the interruption budget and
    // stopped. `active-factor x0.50` alone was inside the budget and saved 6%
    // more, sat two rows above the recommendation in the same table, and could
    // never win. §14.1 promises "among those, the most wakeups avoided"; the
    // descent alone promises only "wherever the first ladder led".
    for candidate in &candidates {
        if beats(&candidate.score, &best.score, &base_score) {
            best = candidate.clone();
        }
    }

    candidates.sort_by(|a, b| {
        b.score.wakeups_avoided.total_cmp(&a.score.wakeups_avoided).then_with(|| {
            a.score
                .interruptions
                .cmp(&b.score.interruptions)
                .then_with(|| a.tweaks.len().cmp(&b.tweaks.len()))
        })
    });
    let recommended = candidates
        .iter()
        .position(|c| c.tweaks == best.tweaks)
        .filter(|_| !best.tweaks.is_empty())
        // A live recording cannot say which of these saves more, so the honest
        // output is the table and no arrow. `force` is the caller insisting.
        .filter(|_| !holds_applied || force);
    let frontier = frontier_of(&candidates);

    Ok(Suggestions {
        window,
        events,
        baseline,
        candidates,
        recommended,
        frontier,
        holds_applied,
        forced: force,
    })
}

/// Whether `candidate` is preferable to `incumbent`, given what the user already
/// puts up with.
///
/// The constraint is a hard gate rather than a term in a weighted sum, because
/// weighting would need an exchange rate between "a million wakeups" and "Slack
/// was asleep when I alt-tabbed to it", and no such number can be defended.
#[must_use]
pub fn beats(candidate: &Score, incumbent: &Score, baseline: &Score) -> bool {
    if candidate.interruptions > baseline.interruptions {
        return false;
    }
    if candidate.wakeups_avoided > incumbent.wakeups_avoided {
        return true;
    }
    // Equal savings: prefer the one that thaws less, since each thaw costs a
    // redraw and often a reconnect.
    candidate.wakeups_avoided == incumbent.wakeups_avoided && candidate.thaws < incumbent.thaws
}

/// Indices of the candidates no other candidate dominates on both axes.
fn frontier_of(candidates: &[Candidate]) -> Vec<usize> {
    (0..candidates.len())
        .filter(|&i| {
            let me = &candidates[i].score;
            !candidates.iter().enumerate().any(|(j, other)| {
                j != i
                    && other.score.wakeups_avoided >= me.wakeups_avoided
                    && other.score.interruptions <= me.interruptions
                    // Strictly better on at least one axis, or two identical
                    // candidates would eliminate each other and the frontier
                    // would come back empty.
                    && (other.score.wakeups_avoided > me.wakeups_avoided
                        || other.score.interruptions < me.interruptions)
            })
        })
        .collect()
}

impl Suggestions {
    /// The recommended candidate, if one beat the baseline.
    #[must_use]
    pub fn best(&self) -> Option<&Candidate> {
        self.recommended.map(|i| &self.candidates[i])
    }

    /// Which settings the recommendation actually moves, old value to new.
    ///
    /// Derived by comparing the two configs rather than by describing the tweaks,
    /// so what is printed is what would be written — a tweak that turned out to
    /// change nothing (a rule with no `for` of its own) cannot be reported as
    /// though it had.
    #[must_use]
    pub fn changes(&self) -> Vec<String> {
        let Some(best) = self.best() else { return Vec::new() };
        let (a, b) = (&self.baseline.config, &best.config);
        let mut out = Vec::new();
        if a.defaults.base_grace != b.defaults.base_grace {
            out.push(format!(
                "defaults.base_grace        {} -> {}",
                a.defaults.base_grace, b.defaults.base_grace
            ));
        }
        if a.defaults.resume_cooldown != b.defaults.resume_cooldown {
            out.push(format!(
                "defaults.resume_cooldown   {} -> {}",
                a.defaults.resume_cooldown, b.defaults.resume_cooldown
            ));
        }
        if (a.activity.active_factor - b.activity.active_factor).abs() > f64::EPSILON {
            out.push(format!(
                "activity.active_factor     {} -> {}",
                a.activity.active_factor, b.activity.active_factor
            ));
        }
        for (old, new) in a.rules.iter().zip(&b.rules) {
            if old.when.for_ != new.when.for_ || old.match_.for_ != new.match_.for_ {
                let show = |r: &crate::config::Rule| {
                    r.when.for_.or(r.match_.for_).map_or_else(|| "-".to_owned(), |d| d.to_string())
                };
                out.push(format!("rule {:<22} for {} -> {}", new.name, show(old), show(new)));
            }
            if old.when.wakeups_per_sec != new.when.wakeups_per_sec {
                out.push(format!(
                    "rule {:<22} wakeups_per_sec {} -> {}",
                    new.name,
                    old.when.wakeups_per_sec.map_or_else(|| "-".to_owned(), |t| t.to_string()),
                    new.when.wakeups_per_sec.map_or_else(|| "-".to_owned(), |t| t.to_string()),
                ));
            }
        }
        out
    }

    /// For each knob, the single-tweak candidate that did best with it.
    ///
    /// The attribution the descent cannot give: every candidate after the first
    /// ladder carries the winners of the ladders before it, so only the
    /// one-tweak candidates say what a knob is worth by itself.
    #[must_use]
    pub fn best_per_knob(&self) -> Vec<usize> {
        let mut best: BTreeMap<&'static str, usize> = BTreeMap::new();
        for (i, c) in self.candidates.iter().enumerate() {
            let [only] = c.tweaks[..] else { continue };
            let entry = best.entry(only.knob()).or_insert(i);
            if beats(&c.score, &self.candidates[*entry].score, &self.baseline.score) {
                *entry = i;
            }
        }
        // Only knobs that actually got somewhere: a row saying a knob achieved
        // nothing is noise, and the recommendation already implies it.
        best.into_values()
            .filter(|&i| {
                self.candidates[i].score.wakeups_avoided > self.baseline.score.wakeups_avoided
            })
            .collect()
    }

    /// What to say about the recording itself, before any numbers are read.
    ///
    /// Empty for a dry-run trace, which is the case the arithmetic was built for.
    #[must_use]
    pub fn advice(&self) -> String {
        if !self.holds_applied {
            return String::new();
        }
        let head = if self.forced {
            "This session really froze things, and --anyway was given.\n\
             The WAKEUPS column below is not what these configs would save.\n"
        } else {
            "This session really froze things, so there is no recommendation.\n"
        };
        format!(
            "{head}\n\
             An app that was held reads as silent for exactly the stretches it was held, \
             so a\ncandidate can only be credited for the sliver where it would have acted \
             sooner than\nthis session did. That flatters aggression, and scores a more \
             patient config at nothing\nat all rather than at what it would really cost.\n\n\
             The same search answers properly against a session that held nothing, because \
             every\napp is observed running throughout. A recording's mode is fixed when \
             the daemon starts\n— `dozectl mode` does not rotate the trace — so this needs \
             a fresh run:\n\n  \
             systemctl --user stop dozed.service\n  \
             dozed --dry-run          # or drop --live from the unit and restart\n\n\
             HOLDS, THAWS and WAITS below are read from the replayed decisions and the \
             recorded events,\nnot from the samples, so they are sound either way.\n\n"
        )
    }

    /// The whole answer, for `dozectl suggest`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = self.advice();
        out.push_str(&format!(
            "tuned against {} events over {}\n\n",
            self.events,
            crate::time::HumanDuration(self.window)
        ));
        out.push_str(&format!(
            "{:<34} {:>10} {:>10} {:>7} {:>7} {:>7}\n",
            // APP-TIME and not DOZED: it is summed across apps, so six apps
            // asleep for an hour is six hours of it, and against a one-hour
            // recording that reads as a bug unless the column says so.
            "CONFIG",
            "WAKEUPS",
            "APP-TIME",
            "HOLDS",
            "THAWS",
            "WAITS"
        ));
        // Every candidate would be dozens of near-identical rows. Three things
        // are worth seeing: what you run now, the frontier you could choose
        // from, and the best each knob manages *on its own* — because "shorten
        // the grace" is a change somebody can reason about, while "these four
        // factors together" is one they can only accept or reject.
        let mut shown: Vec<usize> = self.frontier.clone();
        if let Some(i) = self.candidates.iter().position(|c| c.tweaks.is_empty())
            && !shown.contains(&i)
        {
            shown.push(i);
        }
        for best_solo in self.best_per_knob() {
            if !shown.contains(&best_solo) {
                shown.push(best_solo);
            }
        }
        // Ties are on the frontier by construction — nothing dominates them — but
        // ten rows carrying identical numbers are one row of information. Seen on
        // a real 5h52m recording, where knobs that happened to change nothing
        // produced eight indistinguishable candidates.
        //
        // Deduplicated on the numbers **as rendered**, via the same formatter that
        // prints them. The obvious version keyed on the `f64` and did nothing at
        // all: rows differing in the fourth decimal of a figure displayed as
        // "1.3M" are distinct numbers and the same row. The complaint is about
        // rows a reader cannot tell apart, so the reader's view has to be the key.
        //
        // The survivor is the one with the fewest tweaks, because among changes
        // that achieve the same thing the smallest is the one to recommend.
        shown.sort_by_key(|&i| self.candidates[i].tweaks.len());
        let mut seen: Vec<String> = Vec::new();
        shown.retain(|&i| {
            let key = numbers_of(&self.candidates[i].score);
            if seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        });
        shown.sort_by(|&a, &b| {
            self.candidates[b]
                .score
                .wakeups_avoided
                .total_cmp(&self.candidates[a].score.wakeups_avoided)
        });
        for i in shown {
            let c = &self.candidates[i];
            out.push_str(&format!(
                "{:<34} {}{}\n",
                truncate(&c.label(), 34),
                numbers_of(&c.score),
                if Some(i) == self.recommended { "  <- recommended" } else { "" }
            ));
        }

        let legend = format!(
            "\nWAITS is a time you switched to an app that was held and had to wait for it \
             ({} here).\n",
            self.baseline.score.interruptions
        );
        match self.best() {
            None if self.holds_applied => {
                out.push_str(&legend);
                out.push_str(
                    "Re-run this against a session that held nothing for a recommendation.\n",
                );
            }
            None => {
                out.push_str(&legend);
                out.push_str(
                    "Nothing beat your own config within that budget. That is a result: either \
                     it is already\ntuned for this day, or this day did not exercise it.\n",
                );
            }
            Some(best) => {
                out.push_str(&legend);
                out.push_str("Nothing is recommended that has more of those.\n\nChanges:\n");
                for line in self.changes() {
                    out.push_str(&format!("  {line}\n"));
                }
                out.push_str(&format!(
                    "\nThat is {} instead of {} wakeups avoided, for {} dozing instead of {}.\n",
                    compact(best.score.wakeups_avoided),
                    compact(self.baseline.score.wakeups_avoided),
                    crate::time::HumanDuration(best.score.dozed),
                    crate::time::HumanDuration(self.baseline.score.dozed),
                ));
            }
        }
        out
    }
}

/// The numeric half of a table row, exactly as it will be printed.
///
/// Shared with `render` so the deduplication key and the row itself cannot
/// disagree — which is precisely how the first attempt at deduplicating failed.
fn numbers_of(score: &Score) -> String {
    format!(
        "{:>10} {:>10} {:>7} {:>7} {:>7}",
        compact(score.wakeups_avoided),
        crate::time::HumanDuration(score.dozed).to_string(),
        score.holds,
        score.thaws,
        score.interruptions,
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

fn compact(n: f64) -> String {
    match n.max(0.0) {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e3 => format!("{:.0}k", n / 1e3),
        n => format!("{n:.0}"),
    }
}

/// A whole trace's worth of `MonoTime`, for callers building synthetic sessions.
#[must_use]
pub fn at(secs: u64) -> MonoTime {
    MonoTime::from_millis(secs * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppKey;
    use crate::config::{Predicates, Rule, RuleAction};
    use crate::event::{DesktopState, Event, ProcSample, Visibility};
    use crate::testing;
    use crate::trace::{TRACE_VERSION, TraceHeader};

    /// A config that freezes anything hidden for `grace`, with nothing else in
    /// the way — the fewest moving parts a tuning test can have.
    fn freeze_after(grace: &str) -> Config {
        let mut cfg = Config::empty();
        cfg.defaults.base_grace = grace.parse().unwrap();
        cfg.rules = vec![Rule::new("bg-idle", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            for_: Some(grace.parse().unwrap()),
            ..Default::default()
        })];
        cfg
    }

    /// Build a trace by hand: events only, since replay ignores the rest.
    ///
    /// Deliberately *not* produced by running a planner, so a bug that made the
    /// planner and the scorer agree with each other but not with reality would
    /// still show up here.
    struct Session {
        records: Vec<TraceRecord>,
        seq: u64,
    }

    impl Session {
        fn new(config: Config) -> Self {
            let header = TraceHeader {
                version: TRACE_VERSION,
                dozed_version: "test".into(),
                instance: "tune".into(),
                wall_start: 1_700_000_000,
                live: false,
                config: Box::new(config),
            };
            let mut s = Self { records: vec![TraceRecord::Header(Box::new(header))], seq: 0 };
            s.push(0, Event::FocusSourceHealth { healthy: true });
            s.push(0, Event::PowerChanged { power: testing::ON_BATTERY });
            s
        }

        /// Record a hold that the actuator really carried out.
        ///
        /// What makes a recording untunable is not the mode it was started in but
        /// an app having actually been silenced, so this — not the header — is
        /// what the search looks for.
        fn applied_hold(&mut self, secs: u64, app: &str) {
            self.records.push(TraceRecord::Intent {
                seq: self.seq,
                at: at(secs),
                intent: Box::new(crate::intent::Intent {
                    app: AppKey::comm(app),
                    action: crate::intent::Action::Freeze { reclaim: false },
                    reason: crate::intent::Reason::rule("bg-idle", "test"),
                    at: at(secs),
                }),
                outcome: Outcome::Applied,
            });
            self.seq += 1;
        }

        fn push(&mut self, secs: u64, event: Event) {
            self.records.push(TraceRecord::Event {
                seq: self.seq,
                at: at(secs),
                event: Box::new(event),
            });
            self.seq += 1;
        }

        /// An app that goes hidden at `hidden_at`, wakes the CPU while hidden,
        /// and is returned to at `back_at`.
        fn app(&mut self, name: &str, hidden_at: u64, back_at: u64, wakeups: f32) {
            let key = AppKey::comm(name);
            self.push(hidden_at.saturating_sub(1), testing::seen(name));
            self.push(
                hidden_at,
                Event::VisibilityChanged { app: key.clone(), visibility: Visibility::Hidden },
            );
            // A sample and a tick every minute while it is away, so grace can
            // expire and the accumulator has something to count.
            for t in (hidden_at..back_at).step_by(60) {
                self.push(
                    t,
                    Event::Sampled {
                        app: key.clone(),
                        sample: ProcSample {
                            cpu_pct: 2.0,
                            wakeups_per_sec: wakeups,
                            gpu_pct: None,
                        },
                    },
                );
                self.push(t, Event::Tick);
            }
            self.push(
                back_at,
                Event::VisibilityChanged { app: key.clone(), visibility: Visibility::Focused },
            );
        }
    }

    /// A day with one app hidden for a long stretch — plenty of room for a
    /// shorter grace to pay off, and no reason for it to cost anything.
    fn a_long_absence() -> Session {
        let mut s = Session::new(freeze_after("10m"));
        s.app("slack", 60, 3600, 400.0);
        s.push(3700, Event::Tick);
        s
    }

    #[test]
    fn a_shorter_grace_is_found_when_the_day_rewards_it() {
        // The app is away for an hour, so a ten-minute grace wastes nine minutes
        // of it and the search should say so.
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();

        let best = s.best().expect("something should beat a 10m grace over an hour of absence");
        assert!(
            best.score.wakeups_avoided > s.baseline.score.wakeups_avoided,
            "{:?} vs baseline {:?}",
            best.score,
            s.baseline.score
        );
        assert!(best.score.dozed > s.baseline.score.dozed);
        assert!(
            best.tweaks.iter().any(|t| matches!(t, Tweak::Grace(f) if *f < 1.0)),
            "expected a shorter grace, got {:?}",
            best.tweaks
        );
        // And the recommendation is expressed as settings, not as factors.
        let changes = s.changes();
        assert!(changes.iter().any(|c| c.contains("base_grace")), "{changes:?}");
        assert!(changes.iter().any(|c| c.contains("bg-idle")), "{changes:?}");
    }

    /// A day the two ladders disagree about, which is what it takes to catch the
    /// descent settling for the wrong knob.
    ///
    /// Grace scales every period; the active factor scales only the stretches the
    /// user was at the keyboard. So a session that is *both* — busy, then idle —
    /// is one where "shorten every grace by a quarter" and "halve the anti-thrash
    /// multiplier" are genuinely different moves, and the search has to be able
    /// to reach either. Every session above sits at the `IdlePresent` default,
    /// where the active factor is 1.0 and every rung of that ladder is a no-op.
    ///
    /// Sized so that, against a 10m base grace on battery (x1.0) and the default
    /// active factor of 4.0 — 40m of grace while busy, 10m while idle:
    ///
    /// * `deep` is away long enough that any shortening pays,
    /// * `blink` (16m away, busy) survives grace x0.75 and active-factor x0.50
    ///   on their own, and is caught by the two together,
    /// * `peek` (6m40s away, idle) is caught by grace x0.50, which is what stops
    ///   the grace ladder one rung short of where the other ladder can reach.
    fn a_day_the_ladders_disagree_about() -> Session {
        let mut s = Session::new(freeze_after("10m"));
        s.push(0, Event::ActivityChanged { desktop: DesktopState::Active });
        s.app("blink", 500, 1500, 100.0);
        s.app("deep", 1600, 6500, 4000.0);
        s.push(7000, Event::ActivityChanged { desktop: DesktopState::IdlePresent });
        s.app("peek", 7100, 7500, 100.0);
        s.push(8000, Event::Tick);
        s
    }

    /// §14.1 promises "among those, the most wakeups avoided", and for a while
    /// the code delivered "wherever the first ladder happened to lead".
    ///
    /// Coordinate descent walks a path: once it takes a rung from the grace
    /// ladder, every later ladder is scored *combined with* that rung, and a
    /// combination can break the interruption budget where the knob would not
    /// have broken it alone. The attribution pass had already replayed those solo
    /// candidates and put them in the table — so the better answer was printed,
    /// two rows above the recommendation, and could not win.
    ///
    /// Checked as a property rather than against one known-better candidate,
    /// because the failure is structural: any session where the ladders interact
    /// can produce it, and the fix is a sweep that has to hold for all of them.
    #[test]
    fn nothing_evaluated_beats_the_recommendation() {
        for (name, session) in [
            ("long absence", a_long_absence()),
            ("glances", a_day_of_glances()),
            ("disagreeing ladders", a_day_the_ladders_disagree_about()),
        ] {
            let s = search(&session.records, None, false).unwrap();
            let best = s.best().unwrap_or(&s.baseline);
            for c in &s.candidates {
                assert!(
                    !beats(&c.score, &best.score, &s.baseline.score),
                    "{name}: {} was evaluated, is inside the budget of {}, and beats the \
                     recommended {} — {:?} vs {:?}",
                    c.label(),
                    s.baseline.score.interruptions,
                    best.label(),
                    c.score,
                    best.score,
                );
            }
        }
    }

    /// The property that makes the cost side usable as a constraint, and the one
    /// the previous definition did not have.
    ///
    /// "A hold younger than a minute when the user came back" is a statement
    /// about what the hold *bought*, so a config could improve its score by
    /// holding earlier — starting the clock sooner without letting the user off
    /// any waiting. Measured on a real recording it also ran backwards outright:
    /// `grace x1.50` made fewer holds than the config it was compared against and
    /// scored more contradicted ones.
    ///
    /// Counting the wait itself cannot do either. More aggression means more
    /// holds means at least as many walls to walk into.
    #[test]
    fn interruptions_never_fall_as_the_config_gets_more_aggressive() {
        // Ten-minute absences rather than the two-minute ones above, because the
        // session ticks once a minute and a hold can only land on a tick. Against
        // a two-minute glance every grace below about x0.1 fires at the same tick,
        // which hides the effect being tested: it is *where the hold lands*
        // relative to the user's return that the old definition was sensitive to.
        let mut session = Session::new(freeze_after("10m"));
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            let start = 60 + i as u64 * 700;
            session.app(name, start, start + 600, 300.0);
        }
        session.push(3200, Event::Tick);
        let base = header_config(&session.records);

        // Straddling the return deliberately. At x0.90 the hold lands 60s before
        // the user comes back and at x0.80 it lands 120s before — the same user,
        // the same wait, and under a rule that asked "was the hold younger than a
        // minute" the *more patient* config scored worse.
        let mut scored = Vec::new();
        for factor in [1.5, 1.0, 0.9, 0.8, 0.5, 0.25, 0.1] {
            let mut cfg = (*base).clone();
            Tweak::Grace(factor).apply(&mut cfg);
            let s = replay_scored(&session.records, Some(cfg)).unwrap();
            scored.push((factor, s.holds, s.interruptions));
        }

        for pair in scored.windows(2) {
            let ((pf, ph, pi), (nf, nh, ni)) = (pair[0], pair[1]);
            assert!(nh >= ph, "grace x{nf} should hold at least as often as x{pf}: {nh} vs {ph}");
            assert!(
                ni >= pi,
                "grace x{nf} holds more than x{pf} ({nh} vs {ph}) yet interrupts less \
                 ({ni} vs {pi}) — the cost side is running backwards: {scored:?}"
            );
        }
        assert!(
            scored.last().unwrap().2 > scored[0].2,
            "and across the whole ladder it has to actually move: {scored:?}"
        );
    }

    /// A session that really froze something cannot say what these candidates
    /// would have saved, so the search says so instead of ranking them anyway.
    #[test]
    fn a_session_that_really_froze_things_gets_no_recommendation_unless_forced() {
        // The same events twice. The only difference is whether the recording
        // contains a hold the actuator carried out — which is the whole question,
        // and is why this is read from the outcomes rather than from the header.
        let dry = a_long_absence();
        let s = search(&dry.records, None, false).unwrap();
        assert!(!s.holds_applied, "nothing was ever held");
        assert!(s.best().is_some(), "so this is the case the arithmetic works for");
        assert!(s.advice().is_empty(), "and it needs no caveat: {}", s.advice());

        let mut real = a_long_absence();
        real.applied_hold(700, "slack");

        let s = search(&real.records, None, false).unwrap();
        assert!(s.holds_applied, "an app was actually silenced");
        assert!(s.best().is_none(), "so nothing may be recommended: {:?}", s.best());
        assert!(!s.candidates.is_empty(), "but the candidates are still worth showing");
        assert!(s.advice().contains("--dry-run"), "and it must say what to do: {}", s.advice());
        assert!(!s.render().contains("<- recommended"), "{}", s.render());

        // Forced, the answer comes back — with the caveat still attached, because
        // the arithmetic did not become correct by being insisted upon.
        let forced = search(&real.records, None, true).unwrap();
        assert!(forced.best().is_some(), "--anyway must produce an answer");
        assert!(
            forced.advice().contains("not what these configs would save"),
            "{}",
            forced.advice()
        );
    }

    /// The header says how the daemon *started*, and `dozectl mode` changes the
    /// running mode without rotating the trace. Reading the declaration rather
    /// than the outcomes would refuse a perfectly measurable recording — one
    /// started live on a day that gave it nothing to hold.
    #[test]
    fn a_live_header_over_a_session_that_held_nothing_is_still_tunable() {
        let mut session = a_long_absence();
        let TraceRecord::Header(h) = &mut session.records[0] else { panic!("header first") };
        h.live = true;

        let s = search(&session.records, None, false).unwrap();
        assert!(!s.holds_applied, "the declaration is not the evidence");
        assert!(s.best().is_some(), "and nothing was silenced, so it can be scored");
        assert!(s.advice().is_empty(), "{}", s.advice());
    }

    #[test]
    fn the_interruption_budget_is_never_exceeded() {
        // The property the whole objective rests on. Savings rise monotonically
        // with aggression, so without this the answer is always "freeze
        // everything at once".
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();
        if let Some(best) = s.best() {
            assert!(
                best.score.interruptions <= s.baseline.score.interruptions,
                "recommended {} interruptions against a budget of {}",
                best.score.interruptions,
                s.baseline.score.interruptions
            );
        }
    }

    /// Six apps, each hidden for two minutes and then returned to. A grace short
    /// enough to act inside that window freezes something the user is about to
    /// want, every time.
    fn a_day_of_glances() -> Session {
        let mut session = Session::new(freeze_after("10m"));
        for (i, name) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
            let start = 60 + i as u64 * 200;
            session.app(name, start, start + 120, 300.0);
        }
        session.push(2000, Event::Tick);
        session
    }

    #[test]
    fn a_day_of_glances_makes_aggression_look_bad() {
        // The score has to see it.
        let session = a_day_of_glances();

        let eager = {
            let mut c = freeze_after("10m");
            Tweak::Grace(0.05).apply(&mut c); // 30s
            c
        };
        let scored = replay_scored(&session.records, Some(eager)).unwrap();
        assert!(
            scored.interruptions > 0,
            "freezing apps 30s into a 2m absence must register as interrupting: {scored:?}"
        );

        // And the search declines to recommend it, because the baseline never
        // acted at all and therefore has a budget of zero.
        let s = search(&session.records, None, false).unwrap();
        assert_eq!(s.baseline.score.interruptions, 0, "a 10m grace never fires in a 2m absence");
        if let Some(best) = s.best() {
            assert_eq!(best.score.interruptions, 0, "so nothing that interrupts may win: {best:?}");
        }
    }

    #[test]
    fn a_hold_that_paid_for_itself_still_costs_one_wait() {
        // An app frozen at minute 2 and returned to at minute 50 is a freeze that
        // worked perfectly — and the user still waited for it when they came
        // back. The first version of this metric called that zero cost, on the
        // theory that a long hold had earned its thaw. Netting the benefit off
        // inside the cost term is what let a search shorten graces to move holds
        // across the threshold and bank an improvement nobody felt.
        //
        // So it counts, and the 48 minutes it bought are counted too — as
        // dozing, on the other axis, where they can be weighed against it.
        let session = a_long_absence();
        let mut eager = freeze_after("10m");
        Tweak::Grace(0.2).apply(&mut eager); // 2m
        let scored = replay_scored(&session.records, Some(eager)).unwrap();
        assert!(scored.holds > 0, "it must actually have frozen something");
        assert_eq!(scored.interruptions, 1, "the user came back to a held app once");
        assert!(
            scored.report.total_dozed > Duration::from_secs(40 * 60),
            "and the hold it cost bought most of an hour: {:?}",
            scored.report.total_dozed
        );
    }

    #[test]
    fn the_baseline_replays_through_the_same_path_as_every_candidate() {
        // Otherwise the comparison is against what the daemon *did* — including
        // actuator refusals and any mid-session reload — and a candidate could
        // look better for reasons that have nothing to do with its rules.
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();
        let direct =
            replay_scored(&session.records, Some((*header_config(&session.records)).clone()))
                .unwrap();
        assert_eq!(
            s.baseline.score,
            Score::from_report(&direct.report, direct.holds, direct.thaws, direct.interruptions)
        );
    }

    fn header_config(records: &[TraceRecord]) -> Box<Config> {
        match records.first() {
            Some(TraceRecord::Header(h)) => h.config.clone(),
            _ => panic!("no header"),
        }
    }

    #[test]
    fn every_suggested_config_would_load() {
        // A suggestion the validator rejects is worse than no suggestion: it
        // reads as advice and cannot be taken.
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();
        assert!(s.candidates.len() > 5, "the search should have tried a few things");
        for c in &s.candidates {
            assert!(c.config.clone().validate().is_ok(), "{}: {:?}", c.label(), c.config);
        }
    }

    #[test]
    fn the_table_does_not_repeat_a_row_that_says_the_same_thing() {
        // Seen on a real 5h52m trace: three knobs that happened to change nothing
        // produced four candidates with identical scores, whose labels truncated
        // to the same 34 characters — four rows carrying one row of information.
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();
        let text = s.render();

        let rows: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("CONFIG"))
            .skip(1)
            .take_while(|l| !l.trim().is_empty())
            .collect();
        assert!(!rows.is_empty(), "{text}");

        // The numbers are everything after the label column, so two rows with the
        // same numbers are two rows a reader cannot choose between.
        let mut numbers: Vec<String> = rows
            .iter()
            .map(|r| r.chars().skip(34).collect::<String>().replace("  <- recommended", ""))
            .collect();
        let before = numbers.len();
        numbers.sort();
        numbers.dedup();
        assert_eq!(numbers.len(), before, "duplicate rows in:\n{text}");
    }

    #[test]
    fn the_simplest_of_equally_good_candidates_is_the_one_shown() {
        // Among changes that achieve the same thing, the smallest is the one to
        // recommend — so the row that survives deduplication is the one with the
        // fewest tweaks rather than whichever the sort happened to reach first.
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();
        let text = s.render();
        for line in text.lines().skip_while(|l| !l.starts_with("CONFIG")).skip(1) {
            if line.trim().is_empty() {
                break;
            }
            // A shown row must not be a longer-tweak twin of another shown row.
            let label: String = line.chars().take(34).collect::<String>().trim().to_owned();
            let shown = s
                .candidates
                .iter()
                .filter(|c| truncate(&c.label(), 34) == label)
                .min_by_key(|c| c.tweaks.len());
            assert!(shown.is_some(), "row {label:?} matches no candidate\n{text}");
        }
    }

    #[test]
    fn the_frontier_holds_only_undominated_points() {
        let session = a_long_absence();
        let s = search(&session.records, None, false).unwrap();
        assert!(!s.frontier.is_empty(), "a non-empty search has a frontier");
        for &i in &s.frontier {
            let me = &s.candidates[i].score;
            for (j, other) in s.candidates.iter().enumerate() {
                if i == j {
                    continue;
                }
                let dominated = other.score.wakeups_avoided > me.wakeups_avoided
                    && other.score.interruptions < me.interruptions;
                assert!(
                    !dominated,
                    "{} is dominated by {}",
                    s.candidates[i].label(),
                    other.label()
                );
            }
        }
    }

    #[test]
    fn a_day_with_nothing_to_gain_recommends_nothing() {
        // A session where no app is ever hidden. Every candidate scores zero, so
        // none beats the baseline and the output has to say so rather than
        // inventing a change.
        let mut session = Session::new(freeze_after("10m"));
        session.push(1, testing::seen("editor"));
        for t in 1..20 {
            session.push(t * 60, Event::Tick);
        }
        let s = search(&session.records, None, false).unwrap();
        assert!(s.best().is_none(), "{:?}", s.best().map(Candidate::label));
        assert!(s.render().contains("Nothing beat your own config"), "{}", s.render());
        assert!(s.changes().is_empty());
    }

    #[test]
    fn scaling_is_readable_and_never_degenerate() {
        // The results are written into a TOML file a person has to read, and a
        // grace of "0s" is indistinguishable from a deleted field.
        assert_eq!(scale_duration(HumanDuration::secs(300), 0.5), HumanDuration::secs(150));
        assert_eq!(scale_duration(HumanDuration::secs(300), 0.001), HumanDuration::secs(1));
        assert_eq!(scale_duration(HumanDuration::secs(1), 0.0), HumanDuration::secs(1));
        assert_eq!(scale_duration(HumanDuration::secs(90), 1.0 / 3.0), HumanDuration::secs(30));
    }

    #[test]
    fn a_threshold_scales_on_both_bounds() {
        assert_eq!(scale_threshold(Threshold::Above(50.0), 2.0), Threshold::Above(100.0));
        assert_eq!(
            scale_threshold(Threshold::Bounds { gt: Some(20.0), lt: Some(100.0) }, 0.5),
            Threshold::Bounds { gt: Some(10.0), lt: Some(50.0) }
        );
    }

    #[test]
    fn a_tweak_that_moves_nothing_is_reported_as_moving_nothing() {
        // `changes` is derived by diffing the configs, not by describing the
        // tweaks, so a `WakeupThreshold` factor applied to a rule table with no
        // wakeup bounds in it cannot be printed as though it had done something.
        let mut config = freeze_after("10m");
        let before = config.clone();
        Tweak::WakeupThreshold(4.0).apply(&mut config);
        assert_eq!(config, before, "there is no wakeup bound in this table to scale");
    }

    #[test]
    fn a_trace_with_no_header_is_refused() {
        assert!(matches!(search(&[], None, false), Err(TraceError::MissingHeader)));
    }
}
