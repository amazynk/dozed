//! Turning an [`Intent`] into something that actually happens.
//!
//! This is the far side of the dry-run seam (`DESIGN.md` §3). The planner
//! produced the intent without knowing which actuator would receive it;
//! [`dozed_core::actuator::DryRunActuator`] records and stops, [`CgroupActuator`] writes to the
//! kernel. Both return an [`Outcome`], and the journal is identical apart from
//! that field.

use std::path::PathBuf;

use dozed_core::actuator::Actuator;
use dozed_core::{Action, AppKey, Intent, Outcome};

use crate::cgroup::{Cgroup, CgroupError};
use crate::guard::Guard;
use crate::systemd::SystemdUnits;

/// How an app can be acted on.
///
/// In strict order of preference (`DESIGN.md` §4). A cgroup freeze is atomic and
/// cannot lose a race against `fork()`; the signal path can, and only exists
/// because some apps have no scope to use — one launched from a terminal shares
/// the terminal's, and freezing that would take the terminal with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    /// The app's own cgroup scopes, which between them hold this app and
    /// nothing else.
    ///
    /// A `Vec` for the same reason `Trees` is one, learned one rung later. A
    /// desktop app does not reliably get *one* cgroup: restarted from its
    /// launcher rather than a terminal, Brave put 32 processes in
    /// `app-brave@….service` and its main process in a sibling
    /// `app-org.chromium.Chromium-<pid>.scope`; Signal split 5 and 3 the same
    /// way; Firefox landed in three separate `app-firefox@….service` units, one
    /// per instance. Those are siblings under `app.slice`, so no single cgroup
    /// holds the app and the only common ancestor holds the whole session.
    ///
    /// Taking just the window PID's cgroup meant Brave's scope covered 2 of 34
    /// processes, so rung 1 was correctly refused and every one of those apps
    /// fell to the signal path — which cannot throttle at all, leaving Signal
    /// running at 900 wakeups/s with nothing applied.
    Cgroups(Vec<PathBuf>),
    /// No exclusive scope: signal the process trees instead.
    ///
    /// A `Vec` because an app is not one tree. Firefox was observed as 44
    /// processes in three independent trees, one per `firefox-bin`; a single root
    /// cannot express that, and taking one anyway left 43 processes running while
    /// the app was reported as dozing.
    Trees { roots: Vec<u32> },
}

impl Unit {
    #[must_use]
    pub fn cgroups(&self) -> &[PathBuf] {
        match self {
            Self::Cgroups(p) => p,
            Self::Trees { .. } => &[],
        }
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Cgroups(p) if p.len() == 1 => p[0].display().to_string(),
            Self::Cgroups(p) => {
                format!(
                    "{} cgroups: {}",
                    p.len(),
                    p.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ")
                )
            }
            Self::Trees { roots } if roots.len() == 1 => {
                format!("process tree at {}", roots[0])
            }
            Self::Trees { roots } => format!("{} process trees at {roots:?}", roots.len()),
        }
    }
}

/// Where an app lives, as far as actuation is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub key: AppKey,
    pub unit: Unit,
}

impl Target {
    #[must_use]
    pub fn cgroup(key: AppKey, cgroup: PathBuf) -> Self {
        Self { key, unit: Unit::Cgroups(vec![cgroup]) }
    }

    #[must_use]
    pub fn cgroups(key: AppKey, cgroups: Vec<PathBuf>) -> Self {
        Self { key, unit: Unit::Cgroups(cgroups) }
    }

    #[must_use]
    pub fn trees(key: AppKey, roots: Vec<u32>) -> Self {
        Self { key, unit: Unit::Trees { roots } }
    }
}

/// The live actuator: cgroup v2 writes.
pub struct CgroupActuator {
    guard: Guard,
    /// What we have applied, so `restore_all` knows what to undo. Held in memory
    /// only; crash safety comes from the daemon's on-disk frozen set, which must
    /// be written *before* the freeze (`DESIGN.md` §10.3) because a freeze
    /// outlives the process that applied it.
    held: Vec<Holding>,
    /// The systemd user manager, for the `Throttle` rung. Absent in tests.
    systemd: Option<SystemdUnits>,
    /// How much to ask `memory.reclaim` for. Zero disables reclaim.
    pub reclaim_bytes: u64,
}

/// The unit an action must go through: the one that took the hold, for a
/// restore, and the app's current one for everything else.
///
/// Split out and pure because it is the whole of the bug it fixes, and because
/// reproducing that bug end-to-end means `SIGSTOP`ping a real process inside a
/// parallel test harness — which perturbs every other test that walks this
/// process's own tree.
fn undo_unit<'a>(held: &'a [Holding], target: &'a Target, action: Action) -> &'a Unit {
    if !action.is_restore() {
        return &target.unit;
    }
    held.iter().find(|h| h.key == target.key).map_or(&target.unit, |h| &h.unit)
}

/// One thing we are holding down, and what it will take to let go.
#[derive(Debug, Clone)]
struct Holding {
    key: AppKey,
    unit: Unit,
    action: Action,
    /// For the signal path: exactly the processes we stopped, so `SIGCONT` goes
    /// to those and not to whatever the tree looks like by then.
    stopped: Vec<u32>,
}

impl CgroupActuator {
    #[must_use]
    pub fn new(guard: Guard) -> Self {
        Self {
            guard,
            held: Vec::new(),
            systemd: Some(SystemdUnits::spawn()),
            reclaim_bytes: 512 * 1024 * 1024,
        }
    }

    /// An actuator with no systemd connection, for tests that must not talk to
    /// the session bus.
    #[must_use]
    pub fn without_systemd(guard: Guard) -> Self {
        Self { guard, held: Vec::new(), systemd: None, reclaim_bytes: 0 }
    }

    /// Everything currently held down, for persisting to disk.
    ///
    /// The `stopped` list comes last and is empty for a cgroup hold. The daemon
    /// needs it because the on-disk record is the only thing that survives this
    /// process, and for the signal path "which processes" is not derivable from
    /// anything else afterwards.
    pub fn held(&self) -> impl Iterator<Item = (&AppKey, &Unit, Action, &[u32])> {
        self.held.iter().map(|h| (&h.key, &h.unit, h.action, h.stopped.as_slice()))
    }

    /// Apply `action` to `target`, undoing a hold the way it was applied.
    ///
    /// **A restore dispatches on the unit that took the hold, not on the one the
    /// app presents now**, and those differ more often than the rest of this
    /// file assumes. `Daemon::snapshot_windows` re-decides between cgroups and
    /// the process tree on every compositor snapshot — `is_the_unit_for_app`
    /// answers "no" the moment a foreign process shares a candidate cgroup, and
    /// "yes" again when it leaves — so a browser that was `SIGSTOP`ped as a tree
    /// can be a `Unit::Cgroups` by the time it is let go.
    ///
    /// Dispatching on the current unit then thaws cgroups that were never
    /// frozen, reports `Applied`, and drops the `Holding` — which is where the
    /// stopped PIDs were. The processes stay stopped with nothing anywhere
    /// naming them: not `held`, so `dozectl thaw --all` cannot reach them, and
    /// not the on-disk record, so neither can the startup sweep. Found on a
    /// live machine with **64 Firefox processes stopped for 15 to 20 days**, in
    /// five orphaned trees whose parents had exited, while the journal showed
    /// 353 freezes and 353 restores and `dozectl apps` reported the app running.
    ///
    /// [`Self::restore_all`] already undid holds this way; the per-app path is
    /// the one that did not.
    pub fn apply_to(&mut self, target: &Target, action: Action) -> Outcome {
        match undo_unit(&self.held, target, action).clone() {
            Unit::Cgroups(paths) => self.apply_cgroups(target, &paths, action),
            Unit::Trees { roots } => self.apply_tree(target, &roots, action),
        }
    }

    /// Apply to every cgroup the app owns, as one operation.
    ///
    /// The guard runs over all of them *before* any of them is touched: a veto
    /// on one must stop the whole thing, not leave the app half-held.
    ///
    /// A restore, by contrast, tries every cgroup even after one fails. Leaving
    /// part of an app frozen is the one outcome worse than not freezing it, so
    /// the loop reports the first error but never stops early (§10.3).
    fn apply_cgroups(&mut self, target: &Target, paths: &[PathBuf], action: Action) -> Outcome {
        if paths.is_empty() {
            return Outcome::Skipped { why: format!("no cgroup known for {}", target.key) };
        }
        // §10.1: refuse before touching anything, whatever the rules concluded.
        for path in paths {
            if let Some(why) = self.guard.veto(&Cgroup::new(path)) {
                return Outcome::Skipped { why };
            }
        }

        // Throttle is the one rung that does not go through the cgroup at all:
        // app.slice has no `cpu` controller, so `cpu.max` does not exist and
        // systemd has to be asked to create it (`crate::systemd`).
        if let Action::Throttle { pct } = action {
            return self.throttle_via_systemd(target, paths, pct);
        }

        let mut result = Ok(());
        for path in paths {
            let cg = Cgroup::new(path);
            let one = match action {
                Action::Restore => self.restore_cgroup(&cg, target),
                Action::Freeze { reclaim } => self.freeze_one(&cg, reclaim),
                Action::Throttle { .. } => unreachable!("handled above"),
            };
            // Keep the first real error, but keep going: a partially applied
            // freeze is the state this whole rung exists to avoid, and a
            // partially undone one is worse still.
            if let Err(e) = one
                && result.is_ok()
            {
                result = Err(e);
            }
        }

        match result {
            Ok(()) => {
                self.remember(target, action, Vec::new());
                Outcome::Applied
            }
            // An app exiting between the decision and the write is routine, and
            // was hit twice in ten minutes of spiking. It is not a fault.
            Err(e) if e.is_vanished() => {
                self.forget(&target.key);
                Outcome::Skipped { why: format!("{e}") }
            }
            Err(e @ CgroupError::Unsupported { .. }) => Outcome::Skipped { why: format!("{e}") },
            Err(e) => Outcome::Failed { error: format!("{e}") },
        }
    }

    /// Cap every unit the app owns, each at the full percentage.
    ///
    /// Not the percentage divided between them, which would be the tidier-
    /// looking choice and the wrong one. `cpu.max` is a ceiling, not a
    /// reservation, and the split an app happens to make between its cgroups is
    /// an implementation detail of that app — dividing by it would mean Firefox
    /// with three units got a third of the CPU Signal with one did, from the
    /// same rule. The cost is that an app whose cgroups all saturate at once
    /// could reach N times the nominal cap. In the shape actually observed that
    /// is theoretical: the satellite cgroup holds the main process, which is
    /// close to idle exactly when the app is hidden and throttled.
    ///
    /// A unit that is not a systemd unit is skipped rather than failing the
    /// whole app — the others are still worth capping.
    fn throttle_via_systemd(&mut self, target: &Target, paths: &[PathBuf], pct: u8) -> Outcome {
        let Some(systemd) = &self.systemd else {
            return Outcome::Skipped { why: "no systemd connection".to_owned() };
        };
        let units: Vec<String> =
            paths.iter().filter_map(|p| crate::systemd::unit_of_cgroup(p)).collect();
        if units.is_empty() {
            return Outcome::Skipped {
                why: format!("no systemd unit among {} cgroup(s) of {}", paths.len(), target.key),
            };
        }

        let mut capped = 0usize;
        let mut error = None;
        for unit in &units {
            match systemd.set_quota(unit, pct) {
                Ok(()) => capped += 1,
                Err(e) if e.is_vanished() => {}
                Err(e) => {
                    error.get_or_insert_with(|| format!("{e}"));
                }
            }
        }
        match (capped, error) {
            (0, Some(e)) => Outcome::Failed { error: e },
            (0, None) => {
                self.forget(&target.key);
                Outcome::Skipped { why: format!("every unit of {} vanished", target.key) }
            }
            _ => {
                self.remember(target, Action::Throttle { pct }, Vec::new());
                Outcome::Applied
            }
        }
    }

    /// The fallback for an app with no scope of its own.
    ///
    /// `Throttle` is unavailable here — there is no cgroup to cap and systemd
    /// knows nothing about a bare process tree — so per §7 it degrades rather
    /// than pretending. It degrades to *nothing* rather than to `Nice`, because
    /// `Nice` was measured to be worth so little that reporting it as a throttle
    /// would be a lie in the journal.
    fn apply_tree(&mut self, target: &Target, roots: &[u32], action: Action) -> Outcome {
        match action {
            Action::Throttle { .. } => Outcome::Skipped {
                why: format!(
                    "{} has no cgroup scope of its own, and a process tree cannot be capped",
                    target.key
                ),
            },
            Action::Restore => {
                let stopped = self
                    .held
                    .iter()
                    .find(|h| h.key == target.key)
                    .map(|h| h.stopped.clone())
                    .unwrap_or_default();
                self.forget(&target.key);
                match crate::tree::resume(&stopped) {
                    Ok(()) => Outcome::Applied,
                    Err(e) => Outcome::Failed { error: format!("{e}") },
                }
            }
            Action::Freeze { .. } => match crate::tree::stop(roots, &self.guard) {
                Ok(stopped) => {
                    self.remember(target, action, stopped);
                    Outcome::Applied
                }
                Err(e) if e.is_vanished() => {
                    self.forget(&target.key);
                    Outcome::Skipped { why: format!("{e}") }
                }
                Err(e) => Outcome::Failed { error: format!("{e}") },
            },
        }
    }

    fn freeze_one(&self, cg: &Cgroup, reclaim: bool) -> Result<(), CgroupError> {
        if !cg.supports_freeze() {
            return Err(CgroupError::Unsupported {
                what: "cgroup.freeze",
                path: cg.path().to_path_buf(),
            });
        }
        cg.freeze()?;
        if reclaim && self.reclaim_bytes > 0 {
            // Best effort: the freeze is the point, and reclaim works on an
            // already-frozen cgroup, so failing here must not undo it.
            if let Err(e) = cg.reclaim(self.reclaim_bytes) {
                tracing::debug!(path = %cg.path().display(), error = %e, "reclaim declined");
            }
        }
        Ok(())
    }

    fn restore_cgroup(&self, cg: &Cgroup, target: &Target) -> Result<(), CgroupError> {
        // Thaw first: an app that is both frozen and capped must not be left
        // frozen because clearing the cap failed.
        let thawed = cg.thaw();
        // The cap was set through systemd, so it has to be cleared through
        // systemd; writing `max` to cpu.max directly would be undone the next
        // time the unit manager reloads its view of the unit.
        if let (Some(systemd), Some(unit)) =
            (&self.systemd, crate::systemd::unit_of_cgroup(cg.path()))
            && let Err(e) = systemd.clear_quota(&unit)
            && !e.is_vanished()
        {
            tracing::warn!(app = %target.key, error = %e, "could not clear the CPU quota");
        }
        thawed
    }

    fn remember(&mut self, target: &Target, action: Action, stopped: Vec<u32>) {
        self.forget(&target.key);
        if !action.is_restore() {
            self.held.push(Holding {
                key: target.key.clone(),
                unit: target.unit.clone(),
                action,
                stopped,
            });
        }
    }

    fn forget(&mut self, key: &AppKey) {
        self.held.retain(|h| &h.key != key);
    }
}

impl Actuator for CgroupActuator {
    fn apply(&mut self, intent: &Intent) -> Outcome {
        // The daemon resolves an AppKey to a unit; without that mapping there is
        // nothing to act on.
        let Some(unit) = self.held.iter().find(|h| h.key == intent.app).map(|h| h.unit.clone())
        else {
            return Outcome::Skipped { why: format!("no unit known for {}", intent.app) };
        };
        let target = Target { key: intent.app.clone(), unit };
        self.apply_to(&target, intent.action)
    }

    fn restore_all(&mut self) -> Vec<(AppKey, Outcome)> {
        let held = std::mem::take(&mut self.held);
        held.into_iter()
            .map(|h| {
                // No guard check and no existence check: this path runs on panic
                // thaw and on shutdown, and must never decline to try.
                let outcome = match &h.unit {
                    Unit::Cgroups(paths) => {
                        let mut worst = Ok(());
                        for path in paths {
                            if let (Some(systemd), Some(unit)) =
                                (&self.systemd, crate::systemd::unit_of_cgroup(path))
                            {
                                let _ = systemd.clear_quota(&unit);
                            }
                            // Every cgroup, always: this runs on panic thaw and
                            // shutdown, where stopping at the first failure
                            // would strand the rest of the app frozen.
                            if let Err(e) = Cgroup::new(path).thaw()
                                && worst.is_ok()
                            {
                                worst = Err(e);
                            }
                        }
                        match worst {
                            Ok(()) => Outcome::Applied,
                            Err(e) if e.is_vanished() => Outcome::Skipped { why: format!("{e}") },
                            Err(e) => Outcome::Failed { error: format!("{e}") },
                        }
                    }
                    Unit::Trees { .. } => match crate::tree::resume(&h.stopped) {
                        Ok(()) => Outcome::Applied,
                        Err(e) => Outcome::Failed { error: format!("{e}") },
                    },
                };
                (h.key, outcome)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use dozed_core::{Reason, ReasonKind, testing::app_seen};

    use super::*;

    fn intent(action: Action) -> Intent {
        Intent::new(
            app_seen("slack").key,
            action,
            Reason::new(ReasonKind::RuleFired, "test"),
            dozed_core::MonoTime::ZERO,
        )
    }

    #[test]
    fn dry_run_records_and_touches_nothing() {
        let mut a = dozed_core::actuator::DryRunActuator::default();
        let i = intent(Action::Freeze { reclaim: true });
        assert_eq!(a.apply(&i), Outcome::WouldHave);
        assert_eq!(a.recorded, vec![i]);
        assert!(a.restore_all().is_empty());
    }

    #[test]
    fn a_missing_cgroup_is_skipped_not_failed() {
        // The single most common real-world case: the app exited.
        let mut a = CgroupActuator::without_systemd(Guard::allow_all());
        let target = Target::cgroup(
            app_seen("slack").key,
            PathBuf::from("/sys/fs/cgroup/definitely/not/here.scope"),
        );
        let outcome = a.apply_to(&target, Action::Freeze { reclaim: false });
        assert!(matches!(outcome, Outcome::Skipped { .. }), "{outcome:?}");
        assert!(!outcome.is_real());
    }

    #[test]
    fn restoring_an_unknown_app_is_skipped() {
        let mut a = CgroupActuator::without_systemd(Guard::allow_all());
        assert!(matches!(a.apply(&intent(Action::Restore)), Outcome::Skipped { .. }));
    }

    #[test]
    fn a_process_tree_cannot_be_throttled() {
        // §7: Throttle needs a cgroup, and an app without an exclusive scope has
        // none. It degrades to nothing rather than to Nice — reporting Nice as a
        // throttle would put a claim in the journal that is not true.
        let mut a = CgroupActuator::without_systemd(Guard::allow_all());
        let target = Target::trees(app_seen("slack").key, vec![std::process::id()]);
        let outcome = a.apply_to(&target, Action::Throttle { pct: 5 });
        match outcome {
            Outcome::Skipped { why } => assert!(why.contains("cannot be capped"), "{why}"),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn throttling_without_systemd_is_skipped_not_failed() {
        // A user session with no systemd is unusual but not broken, and losing
        // one rung is not a reason to report a failure.
        let mut a = CgroupActuator::without_systemd(Guard::allow_all());
        let target = Target::cgroup(app_seen("slack").key, PathBuf::from("/sys/fs/cgroup"));
        assert!(matches!(
            a.apply_to(&target, Action::Throttle { pct: 5 }),
            Outcome::Skipped { .. }
        ));
    }

    #[test]
    fn a_hold_is_undone_by_the_unit_that_took_it_not_the_one_in_force_now() {
        // The bug this pins cost 64 Firefox processes 15 to 20 days stopped on a
        // real machine, while the journal showed 353 freezes and 353 restores
        // and `dozectl apps` reported the app running.
        //
        // `is_the_unit_for_app` re-decides cgroups vs. process tree on every
        // compositor snapshot, so an app can be SIGSTOPped as a tree and present
        // as `Unit::Cgroups` by the time it is let go. Dispatching the restore
        // on the *current* unit thawed cgroups that were never frozen, answered
        // `Applied`, and dropped the `Holding` — which held the only list of
        // which processes had been stopped. Nothing named them afterwards:
        // not `held`, so `dozectl thaw --all` could not reach them, and not the
        // on-disk record, so neither could the startup sweep.
        let key = app_seen("slack").key;
        let stopped_as_a_tree = Holding {
            key: key.clone(),
            unit: Unit::Trees { roots: vec![4242] },
            action: Action::Freeze { reclaim: false },
            stopped: vec![4242, 4243],
        };
        // What the app looks like now: the foreign process that was sharing its
        // scope has gone, so the cgroups are exclusive again.
        let now = Target::cgroup(key.clone(), PathBuf::from("/sys/fs/cgroup"));

        assert!(
            matches!(
                undo_unit(std::slice::from_ref(&stopped_as_a_tree), &now, Action::Restore),
                Unit::Trees { .. }
            ),
            "a restore must go back through the signal path that stopped it"
        );
        // A freeze is a fresh decision and takes the unit in force now.
        assert!(
            matches!(
                undo_unit(
                    std::slice::from_ref(&stopped_as_a_tree),
                    &now,
                    Action::Freeze { reclaim: false }
                ),
                Unit::Cgroups(_)
            ),
            "only a restore looks backwards"
        );
        // And an app we hold nothing for has only the current unit to go on.
        assert!(matches!(undo_unit(&[], &now, Action::Restore), Unit::Cgroups(_)));
    }

    #[test]
    fn stopping_a_tree_of_protected_processes_is_skipped() {
        // Our own tree is entirely on the never-freeze list, so there is nothing
        // signallable and the outcome must be Skipped, never a partial stop.
        let mut a = CgroupActuator::without_systemd(Guard::for_current_process());
        let target = Target::trees(app_seen("slack").key, vec![std::process::id()]);
        let outcome = a.apply_to(&target, Action::Freeze { reclaim: false });
        assert!(matches!(outcome, Outcome::Skipped { .. }), "{outcome:?}");
    }

    #[test]
    fn a_unit_describes_itself_for_the_journal() {
        assert_eq!(Unit::Trees { roots: vec![42] }.describe(), "process tree at 42");
        assert!(Unit::Trees { roots: vec![42] }.cgroups().is_empty());
        let cg = Unit::Cgroups(vec![PathBuf::from("/sys/fs/cgroup/app.scope")]);
        assert_eq!(cg.describe(), "/sys/fs/cgroup/app.scope");
        assert_eq!(cg.cgroups().len(), 1);

        // The split case has to name every cgroup: "which cgroups is this app
        // in" is the question whose one-value answer was wrong.
        let split = Unit::Cgroups(vec![
            PathBuf::from("/sys/fs/cgroup/app-brave.service"),
            PathBuf::from("/sys/fs/cgroup/app-chromium.scope"),
        ]);
        assert!(split.describe().starts_with("2 cgroups: "));
        assert!(split.describe().contains("app-brave.service"));
        assert!(split.describe().contains("app-chromium.scope"));
    }
}
