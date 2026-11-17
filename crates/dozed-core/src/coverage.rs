//! What the app list adds up to, and what the machine is doing behind it.
//!
//! A per-app row is unreadable without a scale: `900/s` means one thing on a
//! machine doing 1,200 and another on a machine doing 40,000. §11.1 already
//! names the denominator — `ctxt` in `/proc/stat`, "what the per-app figures
//! should sum to" — and this is that comparison, rendered.
//!
//! It is deliberately *not* called a total. The app list holds what the
//! compositor gave windows for, so its sum excludes every daemon, every kernel
//! thread, and dozed itself. Labelled `total` a partial sum is simply wrong;
//! labelled "these apps, out of the machine" the same number is both right and
//! the only thing here that can be caught being wrong — a tracked figure above
//! the machine's means the sampler is double-counting, and one near zero on a
//! visibly busy desktop means apps are not being attributed at all.
//!
//! Nothing to do with `Report::covered`, which is coverage in *time*: how much
//! of the reporting window had something held. This is coverage of the
//! *machine*: how much of what it is doing right now the app list accounts for.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ipc::AppSummary;
use crate::time::HumanDuration;

/// What the whole machine is doing, carried alongside the app list.
///
/// Sent with [`crate::ipc::Response::Apps`] rather than left to the client to
/// fetch from `status`, so that the rows and their denominator come from one
/// reading of one daemon and cannot disagree by a round trip.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct MachineTotals {
    /// Machine-wide context switches a second, from `/proc/stat`. `None` until
    /// the meter has two readings, and on a kernel that will not show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wakeups_per_sec: Option<f64>,
    /// Cores, for turning a sum of `cpu_pct` into something with a ceiling.
    /// Zero where `cpuidle` is absent — a container, most often — in which case
    /// the cores figure is reported without one.
    #[serde(default)]
    pub cpus: usize,
    /// The window the per-app rates are averaged over
    /// (`defaults.cpu_sample_window`). Carried because it is the explanation
    /// for the one case where the sum can exceed the machine.
    #[serde(default)]
    pub sample_window: Duration,
    /// The measured wattage the app rows are apportioned out of, and what it was
    /// divided by (`crate::attribution`).
    ///
    /// Travels with the app list for the same reason every other field here does:
    /// a row and its denominator must come from one reading of one daemon, or the
    /// column and its footer can disagree by a round trip and there is no way to
    /// tell that from a real discrepancy.
    ///
    /// `None` on the overwhelming majority of machines, where RAPL is `0400 root`
    /// and there is no measured total to divide — and `None` renders as no column
    /// at all rather than as a column of zero watts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<crate::attribution::Basis>,
    /// How much of the GPU's time was spent working, from `gpu_busy_percent`.
    ///
    /// The denominator for the per-app engine figures, and it covers work no
    /// client owns — the compositor's own drawing, the display pipeline, kernel
    /// submissions. Measured on this machine, clients accounted for roughly two
    /// thirds of it, which is the same shape of gap this module already names
    /// for wakeups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_busy: Option<f64>,
}

/// The app rows summed, against the machine they were sampled on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coverage {
    /// Wakeups a second, summed over the apps shown.
    pub tracked_wakeups_per_sec: f64,
    /// Cores busy, summed over the apps shown. `ProcSample::cpu_pct` is percent
    /// of a *single* core, so this is that sum over 100 — a 16-core machine
    /// legitimately reaches 16.0, and printing the raw sum as `1600%` would
    /// read as an arithmetic bug rather than as a busy laptop.
    pub tracked_cores: f64,
    /// Apps with no measurement yet, and so absent from the sums above. Said
    /// out loud, because a coverage figure that is low only because the daemon
    /// started a minute ago should not read as one that is low because
    /// attribution is broken — those two look identical and mean opposite
    /// things.
    pub unsampled: usize,
    /// Percent of one GPU engine, summed over the apps that hold a DRM client.
    ///
    /// `None` where no app in the list has one, which is the ordinary case on a
    /// machine whose GPU nobody is using — and different from `Some(0.0)`, which
    /// means clients exist and did nothing this window.
    pub tracked_gpu_pct: Option<f64>,
    pub machine: MachineTotals,
}

impl Coverage {
    #[must_use]
    pub fn of(apps: &[AppSummary], machine: MachineTotals) -> Self {
        // Unsampled apps are excluded rather than summed as zero. Their
        // `ProcSample` is a placeholder, and folding placeholders into a total
        // that is then compared against `/proc/stat` produces the tool's own
        // "attribution is failing" signal out of nothing but a young daemon.
        let measured = || apps.iter().filter(|a| a.sampled);
        Self {
            tracked_wakeups_per_sec: measured()
                .map(|a| f64::from(a.sample.wakeups_per_sec))
                .sum::<f64>()
                .max(0.0),
            tracked_cores: measured().map(|a| f64::from(a.sample.cpu_pct)).sum::<f64>().max(0.0)
                / 100.0,
            unsampled: apps.iter().filter(|a| !a.sampled).count(),
            // Summed only over apps that actually have a client, so that a
            // desktop where nothing touches the GPU says nothing rather than
            // claiming a measured zero.
            tracked_gpu_pct: measured()
                .filter_map(|a| a.sample.gpu_pct)
                .map(f64::from)
                .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v)),
            machine,
        }
    }

    /// The tracked share of machine-wide wakeups, 0.0 to 1.0 in the ordinary
    /// case. `None` when there is no denominator to divide by.
    ///
    /// Not clamped: a value above 1.0 is a real disagreement between two
    /// counters and hiding it would defeat the point of showing both. See
    /// [`Coverage::leads`] for the benign explanation, which is the usual one.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        match self.machine.wakeups_per_sec {
            Some(system) if system > 0.0 => Some(self.tracked_wakeups_per_sec / system),
            _ => None,
        }
    }

    /// Whether the rows sum to more than the machine reports.
    ///
    /// Ordinarily impossible — every app's context switches are the machine's
    /// too — and ordinarily harmless when it happens anyway: the per-app rates
    /// are averaged over `sample_window` (60s by default) while the machine
    /// figure covers only the last sampling tick, so a burst that has aged out
    /// of one is still in the other. Worth saying rather than smoothing over,
    /// because the *sustained* version of this is a double-counting sampler.
    #[must_use]
    pub fn leads(&self) -> bool {
        self.fraction().is_some_and(|f| f > 1.0)
    }

    /// One line: the two sums, each against what they are a fraction of.
    #[must_use]
    pub fn summary(&self) -> String {
        let cores = if self.machine.cpus > 0 {
            format!("{:.1} of {} cores busy", self.tracked_cores, self.machine.cpus)
        } else {
            format!("{:.1} cores busy", self.tracked_cores)
        };
        let wakeups = match (self.machine.wakeups_per_sec, self.fraction()) {
            // The percentage is dropped rather than printed above 100: it is
            // the one number here that would be actively wrong, and `note`
            // explains the discrepancy in its place.
            (Some(system), Some(f)) if f <= 1.0 => format!(
                "{:.0}/s of {system:.0}/s system-wide wakeups{}",
                self.tracked_wakeups_per_sec,
                // With apps still unmeasured the sum is a floor, not a
                // coverage figure — and with *none* measured, `0%` of a busy
                // machine is precisely the "attribution is broken" alarm this
                // line exists to raise, raised by nothing but a young daemon.
                match (self.unsampled, self.tracked_wakeups_per_sec) {
                    (0, _) => format!(" ({:.0}%)", f * 100.0),
                    (_, t) if t > 0.0 => format!(" (at least {:.0}%)", f * 100.0),
                    _ => String::new(),
                }
            ),
            (Some(system), _) => format!(
                "{:.0}/s of {system:.0}/s system-wide wakeups",
                self.tracked_wakeups_per_sec
            ),
            (None, _) => format!("{:.0}/s wakeups", self.tracked_wakeups_per_sec),
        };
        // Appended rather than woven in, because it is a different chip and a
        // different denominator: the CPU figures are shares of `/proc/stat` and
        // this is a share of `gpu_busy_percent`, and running them together in one
        // clause invites the reader to add them.
        let gpu = match (self.tracked_gpu_pct, self.machine.gpu_busy) {
            (Some(t), Some(m)) => format!(", {t:.0}% of gpu engine vs {:.0}% chip busy", m * 100.0),
            (Some(t), None) => format!(", {t:.0}% of gpu engine"),
            (None, _) => String::new(),
        };
        let pending = match self.unsampled {
            0 => String::new(),
            n => format!(" ({n} not measured yet)"),
        };
        format!("these apps: {cores}, {wakeups}{gpu}{pending}")
    }

    /// What the gap between the two figures is, in one sentence.
    ///
    /// `None` without a machine figure, because then no gap is on display and
    /// there is nothing to account for.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        self.machine.wakeups_per_sec?;
        Some(if self.leads() {
            format!(
                "the rows average over {}, /proc/stat covers the last tick, so the sum can lead it",
                HumanDuration(self.machine.sample_window)
            )
        } else {
            "the rest has no window: daemons, kernel threads, and dozed itself".to_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppKey;
    use crate::event::{ProcSample, Visibility};
    use crate::intent::Action;
    use crate::{Mode, ipc::AppSummary};

    fn app(cpu_pct: f32, wakeups_per_sec: f32) -> AppSummary {
        AppSummary {
            key: AppKey::comm("some-app"),
            label: "some-app".into(),
            visibility: Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: ProcSample { cpu_pct, wakeups_per_sec, gpu_pct: None },
            sampled: true,
            pids: 1,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        }
    }

    /// An app that has been measured, which is what these tests are about
    /// unless they say otherwise.
    fn unmeasured(mut a: AppSummary) -> AppSummary {
        a.sampled = false;
        a
    }

    fn machine(wakeups_per_sec: Option<f64>, cpus: usize) -> MachineTotals {
        MachineTotals {
            wakeups_per_sec,
            cpus,
            sample_window: Duration::from_secs(60),
            basis: None,
            gpu_busy: None,
        }
    }

    #[test]
    fn sums_the_rows_and_divides_by_the_machine() {
        let c = Coverage::of(&[app(150.0, 900.0), app(50.0, 100.0)], machine(Some(4000.0), 16));

        assert!((c.tracked_cores - 2.0).abs() < 1e-9, "200% of a core is two cores");
        assert!((c.tracked_wakeups_per_sec - 1000.0).abs() < 1e-9);
        assert!((c.fraction().unwrap() - 0.25).abs() < 1e-9);
        assert_eq!(
            c.summary(),
            "these apps: 2.0 of 16 cores busy, 1000/s of 4000/s system-wide wakeups (25%)"
        );
        assert_eq!(
            c.note().unwrap(),
            "the rest has no window: daemons, kernel threads, and dozed itself"
        );
    }

    #[test]
    fn an_empty_list_is_zero_rather_than_a_division() {
        let c = Coverage::of(&[], machine(Some(4000.0), 8));
        assert_eq!(c.fraction(), Some(0.0));
        assert_eq!(
            c.summary(),
            "these apps: 0.0 of 8 cores busy, 0/s of 4000/s system-wide wakeups (0%)"
        );
    }

    /// A container has `/proc/stat` and no `cpuidle`, so it reports a wakeup
    /// rate and no core count. Printing "of 0 cores" would be worse than
    /// printing nothing.
    #[test]
    fn no_core_count_drops_the_core_denominator_only() {
        let c = Coverage::of(&[app(100.0, 300.0)], machine(Some(1200.0), 0));
        assert_eq!(
            c.summary(),
            "these apps: 1.0 cores busy, 300/s of 1200/s system-wide wakeups (25%)"
        );
    }

    /// The first tick after startup: one reading is not a rate. The rows are
    /// still worth summing, they simply have nothing to be a fraction of.
    #[test]
    fn no_machine_figure_drops_the_fraction_and_the_note() {
        let c = Coverage::of(&[app(100.0, 300.0)], machine(None, 16));
        assert_eq!(c.fraction(), None);
        assert_eq!(c.summary(), "these apps: 1.0 of 16 cores busy, 300/s wakeups");
        assert_eq!(c.note(), None, "no denominator on show means no gap to explain");
    }

    #[test]
    fn a_sum_above_the_machine_says_so_instead_of_printing_over_100_percent() {
        let c = Coverage::of(&[app(10.0, 4200.0)], machine(Some(4000.0), 16));

        assert!(c.leads());
        assert_eq!(
            c.summary(),
            "these apps: 0.1 of 16 cores busy, 4200/s of 4000/s system-wide wakeups",
            "the percentage is dropped, not clamped"
        );
        assert_eq!(
            c.note().unwrap(),
            "the rows average over 1m, /proc/stat covers the last tick, so the sum can lead it"
        );
    }

    /// `Sum` for f64 has -0.0 as its identity, and `-0.0/s` reads as a bug.
    /// The same reason `Report::render` clamps its own totals.
    #[test]
    fn negative_zero_never_reaches_the_page() {
        let c = Coverage::of(&[], machine(None, 4));
        assert_eq!(c.summary(), "these apps: 0.0 of 4 cores busy, 0/s wakeups");
    }

    /// A young daemon has looked at apps it has not measured. Their placeholder
    /// zeros must not be summed, and must not produce a coverage figure — "0%
    /// of a busy machine" is this line's own alarm for broken attribution.
    #[test]
    fn unmeasured_apps_are_excluded_rather_than_counted_as_idle() {
        let none_yet = Coverage::of(
            &[unmeasured(app(0.0, 0.0)), unmeasured(app(0.0, 0.0))],
            machine(Some(40_000.0), 16),
        );
        assert_eq!(none_yet.unsampled, 2);
        assert_eq!(
            none_yet.summary(),
            "these apps: 0.0 of 16 cores busy, 0/s of 40000/s system-wide wakeups \
             (2 not measured yet)",
            "no percentage at all until something has been measured"
        );

        // Partly measured: the sum is a floor, and says so rather than
        // understating coverage as though it were complete.
        let partial = Coverage::of(
            &[app(100.0, 10_000.0), unmeasured(app(0.0, 0.0))],
            machine(Some(40_000.0), 16),
        );
        assert_eq!(
            partial.summary(),
            "these apps: 1.0 of 16 cores busy, 10000/s of 40000/s system-wide wakeups \
             (at least 25%) (1 not measured yet)"
        );

        // Everything measured: an exact figure, and no parenthetical.
        let full = Coverage::of(&[app(100.0, 10_000.0)], machine(Some(40_000.0), 16));
        assert_eq!(
            full.summary(),
            "these apps: 1.0 of 16 cores busy, 10000/s of 40000/s system-wide wakeups (25%)"
        );
    }

    /// A desktop where nothing touches the GPU must say nothing, not zero: the
    /// per-app figure is absent for an app with no DRM client, and summing
    /// absences into `0.0%` would claim a measurement nobody took.
    #[test]
    fn gpu_time_is_summed_only_over_the_apps_that_have_a_client() {
        let none = Coverage::of(&[app(50.0, 100.0)], machine(Some(4000.0), 16));
        assert_eq!(none.tracked_gpu_pct, None);
        assert!(!none.summary().contains("gpu"), "{}", none.summary());

        let mut a = app(50.0, 100.0);
        a.sample.gpu_pct = Some(4.0);
        let mut b = app(10.0, 20.0);
        b.sample.gpu_pct = Some(2.5);
        // A third app with no client at all contributes nothing rather than zero.
        let c = Coverage::of(&[a, b, app(1.0, 1.0)], machine(Some(4000.0), 16));
        assert_eq!(c.tracked_gpu_pct, Some(6.5));
    }

    /// The GPU figures get their own clause, against their own denominator.
    ///
    /// The CPU numbers are shares of `/proc/stat` and this is a share of
    /// `gpu_busy_percent` — two chips, two counters — and running them together
    /// in one phrase invites the reader to add them.
    #[test]
    fn the_gpu_clause_carries_its_own_denominator() {
        let mut a = app(50.0, 100.0);
        a.sample.gpu_pct = Some(7.5);
        let mut m = machine(Some(4000.0), 16);
        m.gpu_busy = Some(0.21);
        let text = Coverage::of(&[a], m).summary();
        assert!(text.contains("8% of gpu engine"), "{text}");
        assert!(text.contains("21% chip busy"), "{text}");

        // Without the chip figure the app total still stands on its own: it is a
        // measurement, not a share, so it does not need a denominator to exist.
        let mut a = app(50.0, 100.0);
        a.sample.gpu_pct = Some(7.5);
        let text = Coverage::of(&[a], machine(Some(4000.0), 16)).summary();
        assert!(text.contains("8% of gpu engine"), "{text}");
        assert!(!text.contains("chip busy"), "{text}");
    }
}
