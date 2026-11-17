//! Apportioning a **measured** wattage between apps, and saying so.
//!
//! There is no per-process energy counter in Linux. There is no way to make one:
//! the hardware counts joules per power domain, and a domain is a piece of
//! silicon shared by everything that runs on it. So a per-app watt figure is
//! necessarily a model, and §11.1 is a section about what happens when a model is
//! mistaken for a measurement — PowerTOP's estimates are read as readings
//! constantly, and the whole point of that section is not to repeat it.
//!
//! What is different here, and it is the only thing that makes this shippable:
//!
//! * **The total is measured.** RAPL says what the silicon spent. The old model
//!   multiplied CPU-seconds by a guessed 2-4 J each and could not be caught being
//!   wrong; this one starts from a counter and can only be wrong about the
//!   *split*.
//! * **The denominator is measured too, and it is the whole machine.** The share
//!   is an app's CPU time over `/proc/stat`'s, not over the app list's. Dividing
//!   by the app list would hand 100% of the package to the windows dozed happens
//!   to know about, on a machine where they account for 23% of it
//!   (`crate::coverage`). The leftover here is real and is shown.
//! * **The proxy is stated, and it is one proxy.** CPU time, nothing else. A
//!   blend of CPU time and wakeups would fit this machine better and would need a
//!   weight, and a weight is a guess wearing a measurement's clothes — which is
//!   the exact failure mode §11.1 exists to prevent. One stated proxy with its
//!   bias named beats a tuned one whose bias is buried.
//!
//! ## The bias, named
//!
//! CPU time under-attributes an app that wakes the CPU often and uses little of
//! it — which is *the* case this whole tool is built around (§13). A timer firing
//! 900 times a second drags a core out of C6 900 times and shows up as almost no
//! CPU time. So this figure understates precisely the app dozed most wants to
//! catch, and it is not the metric to act on: `wakeups_per_sec` is, and it is in
//! the column next to it.
//!
//! This is a smaller error than the one it replaces. The old model's error was
//! unbounded and in an unknown direction; this one is bounded by a measured total
//! and known to lean one way. But it is an error, the UI says "est." in front of
//! every figure it produces, and this file refuses to produce one at all in the
//! four cases where the arithmetic would be a fiction.
//!
//! ## What is apportioned
//!
//! The `core` RAPL domain where the kernel exposes it, and the whole package
//! otherwise. `core` is the better numerator by some distance: it is the energy
//! the executing cores spent, which is the thing CPU time is a proxy for. The
//! package also contains a floor that no app causes — the interconnect, the
//! memory controller, the integrated GPU — and on the machine this was written
//! against that floor is most of it: **2.2 W of core inside a 24.1 W package**.
//! Apportioning all 24.1 W by CPU share would have charged an app that ran for a
//! second with watts that were being spent whether it ran or not.
//!
//! Which of the two was used is carried in [`Basis::source`] and printed, because
//! the two differ by an order of magnitude and a reader comparing two machines
//! needs to know which number they are looking at.

use serde::{Deserialize, Serialize};

/// How one app's share of a measured wattage is arrived at.
///
/// Two shapes, and the first is strictly better where the machine has given
/// enough of itself away to fit one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "basis")]
pub enum Basis {
    /// Coefficients fitted to this machine's own power response
    /// (`crate::power_model`).
    ///
    /// The better answer by some distance, and not because it is more precise.
    /// It needs no denominator and takes no share: an app's watts are its own two
    /// measured rates multiplied by two coefficients the machine gave up itself,
    /// so one app's figure does not move when an unrelated app gets busy. It also
    /// charges for wakeups, which is the whole bias the CPU-time proxy below is
    /// documented as having — and charges at a rate nobody chose.
    Fitted(crate::power_model::Fit),
    /// A measured total, apportioned by CPU-time share.
    ///
    /// The fallback, used until the model has seen enough of the machine to fit —
    /// or where it has seen enough and refused, which on a power-limited package
    /// is permanent. Its bias is named in [`Self::caveat`] and it is labelled an
    /// estimate everywhere it is rendered.
    Share {
        /// Watts to apportion: the `core` domain, or the package where `core` is
        /// not exposed.
        attributable_w: f64,
        /// What that figure is, for the label.
        source: Attributable,
        /// Cores busy machine-wide, from `/proc/stat`. The denominator.
        machine_cores_busy: f64,
    },
}

/// Below this the machine is doing nothing anyone can be blamed for.
///
/// Not a tuning threshold — a division guard, and it applies only to
/// [`Basis::Share`], which is the only shape that divides. At 0.01 cores busy the
/// denominator is a rounding error in the tick counter, and any app that ran at
/// all takes a share near 1.0 of it: an app using 0.4% of one core would be
/// handed the whole package's idle floor and rendered as the reason the laptop is
/// warm. The numerator is small there too, so the product is not large, but it is
/// arbitrary, and an arbitrary number that looks like a measurement is the thing
/// this module exists not to print.
///
/// 0.05 of a core is 5% of one core across the whole machine, which on 16 cores
/// is 0.3% of it. Below that there is nothing to divide up and the honest answer
/// is that nobody is using any power worth naming.
///
/// **Chosen, not measured.** No experiment established this boundary; it is a
/// round fraction picked to sit clearly above the noise floor of a `/proc/stat`
/// tick and clearly below any load a person would call activity. What makes it
/// tolerable is that it fails safe in one direction only: too high and the watts
/// column is absent on a very idle machine, which is a column nobody misses; too
/// low and the arithmetic starts dividing by a rounding error.
const MIN_BUSY_CORES: f64 = 0.05;

impl Basis {
    /// The fitted basis, which is preferred wherever it exists.
    #[must_use]
    pub fn fitted(fit: crate::power_model::Fit) -> Self {
        Self::Fitted(fit)
    }

    /// The share basis, or `None` if there is nothing to divide or divide by.
    ///
    /// `None` is the common case and is not a failure: RAPL is `0400 root` on a
    /// stock kernel, so most machines have no measured total to apportion, and
    /// **an absent basis renders as absent rather than as zero watts per app**.
    /// The distinction `EnergyStatus::package_denied` and `AppSummary::sampled`
    /// both already draw, drawn once more.
    #[must_use]
    pub fn of(
        core_w: Option<f64>,
        package_w: Option<f64>,
        machine_cores_busy: Option<f64>,
    ) -> Option<Self> {
        let busy = machine_cores_busy.filter(|c| c.is_finite() && *c >= MIN_BUSY_CORES)?;
        // `core` first: see the module docs. Falling through to the package is a
        // worse answer, not a wrong one, and it is labelled differently.
        let (attributable_w, source) = match (core_w, package_w) {
            (Some(w), _) => (w, Attributable::Core),
            (None, Some(w)) => (w, Attributable::Package),
            (None, None) => return None,
        };
        (attributable_w.is_finite() && attributable_w >= 0.0).then_some(Self::Share {
            attributable_w,
            source,
            machine_cores_busy: busy,
        })
    }

    /// One app's watts. An **estimate** in both shapes, for different reasons.
    ///
    /// `None` from [`Self::Share`] when the app's own CPU exceeds the machine's,
    /// which is not the contradiction it looks like: the per-app rates are
    /// averaged over `cpu_sample_window` (60 s by default) while `/proc/stat`
    /// covers the last tick, so a burst that has aged out of one is still in the
    /// other. That is the same benign race `coverage::Coverage::leads` documents,
    /// and the same answer is given to it — say nothing rather than clamp. A clamp
    /// would hand one app the entire measured package and render it as fact.
    ///
    /// [`Self::Fitted`] has no such failure mode, because it takes no share:
    /// there is no denominator for the app to exceed.
    #[must_use]
    pub fn watts(&self, cpu_pct: f32, wakeups_per_sec: f32) -> Option<f64> {
        let cores = f64::from(cpu_pct) / 100.0;
        let wakeups = f64::from(wakeups_per_sec);
        if !cores.is_finite() || cores < 0.0 || !wakeups.is_finite() || wakeups < 0.0 {
            return None;
        }
        match self {
            Self::Fitted(fit) => Some(fit.watts(cores, wakeups)),
            Self::Share { attributable_w, machine_cores_busy, .. } => {
                let share = cores / machine_cores_busy;
                (share <= 1.0).then_some(attributable_w * share)
            }
        }
    }

    /// What is being spent that no app in the list accounts for.
    ///
    /// For [`Self::Fitted`] this is the fitted floor: what the package draws with
    /// nothing running, which belongs to no app by construction rather than by
    /// subtraction. For [`Self::Share`] it is the leftover of the denominator —
    /// daemons, kernel threads, and dozed itself.
    ///
    /// Shown either way, because a per-app breakdown that does not add up to its
    /// own header is read as a bug in the tool.
    #[must_use]
    pub fn unattributed_w(&self, tracked_cores: f64) -> Option<f64> {
        match self {
            Self::Fitted(fit) => Some(fit.floor_w),
            Self::Share { attributable_w, machine_cores_busy, .. } => {
                let rest = machine_cores_busy - tracked_cores;
                (rest >= 0.0).then_some(attributable_w * rest / machine_cores_busy)
            }
        }
    }

    /// The arithmetic, in words, for the footer under the app list.
    ///
    /// §11.1's rule: a modelled figure travels with the thing it was derived from,
    /// or it is a number nobody can check. Everything needed to reproduce the
    /// column by hand is in this one line.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Fitted(fit) => format!(
                "est. watts: {:.2} W per busy core + {:.1} µJ per wakeup, both fitted to this \
                 machine's own package counter (R²={:.2} over {} readings), plus a {:.2} W floor \
                 no app causes",
                fit.watts_per_core,
                fit.joules_per_wakeup * 1e6,
                fit.r2,
                fit.samples,
                fit.floor_w
            ),
            Self::Share { attributable_w, source, machine_cores_busy } => format!(
                "est. watts: each app's CPU share of {attributable_w:.2} W measured {} \
                 ({machine_cores_busy:.2} of the machine's cores busy) — an estimate, not a \
                 measurement",
                source.as_str()
            ),
        }
    }

    /// Which way the estimate leans, in one sentence.
    ///
    /// Separate from [`Self::summary`] because it is the caveat rather than the
    /// arithmetic, and because the two shapes lean differently — which is the
    /// entire reason the fitted one is preferred.
    #[must_use]
    pub fn caveat(&self) -> &'static str {
        match self {
            Self::Fitted(_) => {
                "Wakeups are charged at a rate measured on this machine rather than assumed, so \
                 an app that wakes the CPU often and uses little of it is no longer understated. \
                 Still an estimate: no per-process energy counter exists, and the coefficients \
                 describe the machine as a whole."
            }
            Self::Share { .. } => {
                "CPU time is the proxy, so an app that wakes the CPU often and uses little of it \
                 is understated — which is the app dozed cares most about. Judge on wakeups/s; \
                 this column is for scale."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power_model::Fit;

    /// A share basis with `core` measured, as on any machine exposing the domain.
    fn share(attributable_w: f64, cores_busy: f64) -> Basis {
        Basis::of(Some(attributable_w), Some(24.07), Some(cores_busy)).unwrap()
    }

    fn fit(watts_per_core: f64, joules_per_wakeup: f64) -> Basis {
        Basis::fitted(Fit {
            floor_w: 6.0,
            watts_per_core,
            joules_per_wakeup,
            r2: 0.86,
            samples: 90,
            se_watts_per_core: 0.05,
            se_joules_per_wakeup: 1e-6,
            cov_core_wakeup: 0.0,
        })
    }

    #[test]
    fn an_app_gets_its_cpu_share_of_a_measured_total() {
        // Two cores busy machine-wide, one of them this app's: half the measured
        // watts. The total is a reading, the split is the model, and only the
        // second one can be wrong.
        let b = share(4.0, 2.0);
        assert_eq!(b.watts(100.0, 0.0), Some(2.0));
        assert_eq!(b.watts(50.0, 0.0), Some(1.0));
        assert_eq!(b.watts(0.0, 0.0), Some(0.0));
    }

    /// The whole reason the fitted basis is preferred.
    ///
    /// Two apps, same CPU, wildly different wakeup rates — the case §13 is built
    /// around. Under CPU-time share they are indistinguishable; under the fitted
    /// coefficients the chatty one is charged for what it actually costs.
    #[test]
    fn the_fitted_basis_charges_for_wakeups_and_the_share_basis_cannot() {
        let quiet = (1.0f32, 10.0f32);
        let chatty = (1.0f32, 3_000.0f32);

        let b = share(4.0, 2.0);
        assert_eq!(
            b.watts(quiet.0, quiet.1),
            b.watts(chatty.0, chatty.1),
            "CPU-time share is blind to the metric the tool exists for"
        );

        // 2 W per core, 100 µJ per wakeup.
        let b = fit(2.0, 100e-6);
        let quiet_w = b.watts(quiet.0, quiet.1).unwrap();
        let chatty_w = b.watts(chatty.0, chatty.1).unwrap();
        assert!((quiet_w - (0.02 + 0.001)).abs() < 1e-9, "{quiet_w}");
        assert!((chatty_w - (0.02 + 0.3)).abs() < 1e-9, "{chatty_w}");
        assert!(chatty_w > quiet_w * 10.0, "the chatty app must dominate: {chatty_w} vs {quiet_w}");
    }

    /// A fitted figure is the app's own two rates times two constants, so no
    /// other app's behaviour can move it. The share basis divides by a machine
    /// total and therefore cannot say that.
    #[test]
    fn a_fitted_figure_does_not_move_when_an_unrelated_app_gets_busy() {
        let b = fit(2.0, 100e-6);
        assert_eq!(b.watts(50.0, 100.0), b.watts(50.0, 100.0));

        let idle_machine = share(4.0, 1.0);
        let busy_machine = share(4.0, 8.0);
        assert_ne!(
            idle_machine.watts(50.0, 0.0),
            busy_machine.watts(50.0, 0.0),
            "the share basis is a share, and shares move when the denominator does"
        );
    }

    #[test]
    fn the_core_domain_is_preferred_over_the_whole_package() {
        // The package carries a floor no app causes — interconnect, memory
        // controller, integrated graphics — and on the machine this was written
        // against that floor is most of it: 2.21 W of core inside 24.07 W of
        // package. Apportioning all 24.07 W by CPU share would charge an app that
        // ran for a second with watts that were being spent regardless.
        let b = Basis::of(Some(2.21), Some(24.07), Some(1.0)).unwrap();
        assert!(matches!(b, Basis::Share { source: Attributable::Core, .. }));

        // And the fallback is labelled differently, because the two figures
        // differ by an order of magnitude and a reader comparing machines needs
        // to know which one they have.
        let b = Basis::of(None, Some(24.07), Some(1.0)).unwrap();
        assert!(matches!(b, Basis::Share { source: Attributable::Package, .. }));
        assert!(b.summary().contains("cpu package"), "{}", b.summary());
    }

    #[test]
    fn an_app_cannot_be_attributed_more_than_the_machine_was_measured_using() {
        // Not the contradiction it looks like: the per-app rates average over
        // `cpu_sample_window` (60s) while `/proc/stat` covers the last tick, so a
        // burst that has aged out of one is still in the other — the same benign
        // race `coverage::Coverage::leads` documents. Clamping would hand this
        // app the entire measured package and render the guess as a fact.
        let b = share(4.0, 1.0);
        assert_eq!(b.watts(100.0, 0.0), Some(4.0), "exactly the machine is still honest");
        assert_eq!(b.watts(150.0, 0.0), None, "more than the machine is not");

        // The fitted basis has no denominator, so there is nothing to exceed.
        assert!(fit(2.0, 1e-4).watts(150.0, 0.0).is_some());
    }

    #[test]
    fn an_idle_machine_has_no_denominator_worth_dividing_by() {
        // At a hundredth of a core busy the denominator is a rounding error in
        // the tick counter, and any app that ran at all takes a share near 1.0 of
        // it — so an app using 0.4% of one core would be handed the package's
        // whole idle floor and rendered as the reason the laptop is warm.
        assert!(Basis::of(Some(2.2), Some(24.07), Some(0.01)).is_none());
        assert!(Basis::of(Some(2.2), Some(24.07), Some(0.0)).is_none());
        assert!(
            Basis::of(Some(2.2), Some(24.07), Some(0.05)).is_some(),
            "the floor itself is fine"
        );
    }

    #[test]
    fn a_counter_nobody_may_read_yields_no_basis_rather_than_zero_watts() {
        // RAPL is `0400 root` on a stock kernel, so this is the common case and
        // not a failure. An absent basis renders as no column at all — the
        // distinction `EnergyStatus::package_denied` and `AppSummary::sampled`
        // both already draw, drawn once more.
        assert!(Basis::of(None, None, Some(2.0)).is_none());
        assert!(Basis::of(Some(2.2), Some(24.07), None).is_none(), "and no denominator either");
    }

    #[test]
    fn a_reading_that_is_not_a_number_is_not_a_basis() {
        assert!(Basis::of(Some(f64::NAN), None, Some(2.0)).is_none());
        assert!(Basis::of(Some(-1.0), None, Some(2.0)).is_none());
        assert!(Basis::of(Some(2.2), None, Some(f64::INFINITY)).is_none());
        assert!(fit(2.0, 1e-4).watts(f32::NAN, 0.0).is_none());
        assert!(fit(2.0, 1e-4).watts(1.0, f32::NAN).is_none());
    }

    #[test]
    fn what_no_app_accounts_for_is_named_rather_than_left_as_a_gap() {
        // The app list holds what the compositor gave windows for, so it excludes
        // every daemon and every kernel thread — measured at 23% of machine-wide
        // wakeups on a real desktop. A per-app breakdown that does not add up to
        // its own header is read as a bug in the tool, so the remainder is shown.
        let b = share(4.0, 2.0);
        assert_eq!(b.unattributed_w(0.5), Some(3.0), "1.5 of 2.0 cores is nobody in the list");
        assert_eq!(b.unattributed_w(2.0), Some(0.0), "the list accounted for all of it");
        assert_eq!(b.unattributed_w(3.0), None, "the rows leading the machine says nothing");

        // For a fitted basis the remainder is the fitted floor, which belongs to
        // no app by construction rather than by subtraction — so it does not
        // depend on what the app list happened to sum to.
        let b = fit(2.0, 1e-4);
        assert_eq!(b.unattributed_w(0.5), Some(6.0));
        assert_eq!(b.unattributed_w(99.0), Some(6.0));
    }

    #[test]
    fn the_footer_carries_the_arithmetic_and_the_word_estimate() {
        // §11.1's rule: a modelled figure travels with what it was derived from,
        // or it is a number nobody can check.
        let text = share(2.21, 1.20).summary();
        assert!(text.contains("2.21 W"), "{text}");
        assert!(text.contains("core"), "{text}");
        assert!(text.contains("1.20"), "the denominator: {text}");
        assert!(text.contains("estimate"), "{text}");

        let text = fit(2.5, 30e-6).summary();
        assert!(text.contains("2.50 W per busy core"), "{text}");
        assert!(text.contains("30.0 µJ per wakeup"), "{text}");
        assert!(text.contains("R²=0.86"), "the fit has to carry its own quality: {text}");
        assert!(text.contains("6.00 W floor"), "{text}");
    }

    /// The two shapes lean differently, and the difference is the entire reason
    /// one is preferred — so they must not share a caveat.
    #[test]
    fn each_basis_names_its_own_bias() {
        let shared = share(2.21, 1.2).caveat();
        assert!(shared.contains("understated"), "{shared}");
        assert!(shared.contains("wakes"), "{shared}");

        let fitted = fit(2.0, 1e-4).caveat();
        assert!(fitted.contains("no longer understated"), "{fitted}");
        assert!(fitted.contains("Still an estimate"), "no per-process counter exists: {fitted}");
    }

    #[test]
    fn a_basis_survives_a_round_trip() {
        // It crosses the IPC boundary, and the two shapes are apportioned
        // completely differently — so a client that decoded one as the other
        // would render numbers that are not wrong by a little.
        for b in [share(2.21, 1.2), fit(2.0, 1e-4)] {
            let json = serde_json::to_string(&b).unwrap();
            assert_eq!(serde_json::from_str::<Basis>(&json).unwrap(), b, "{json}");
        }
    }
}

/// Which measured domain a [`Basis::Share`] estimate was apportioned out of.
///
/// An enum rather than a string because it crosses the IPC boundary and the two
/// variants differ by an order of magnitude — 2.2 W against 24.1 W on this
/// machine — so a client that silently accepted an unknown third value would
/// render a number ten times too large with no way to tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Attributable {
    /// The RAPL `core` domain: the energy the executing cores spent, and the
    /// right numerator for a CPU-time proxy.
    Core,
    /// The whole package, on a machine that exposes no `core` sub-domain. Worse,
    /// because it includes a floor no app causes — but measured, and labelled.
    Package,
}

impl Attributable {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Package => "cpu package",
        }
    }
}
