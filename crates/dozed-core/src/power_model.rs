//! What a watt actually buys on *this* machine, fitted to its own counters.
//!
//! `crate::attribution` apportions a measured wattage by CPU time and names its
//! bias out loud: CPU time understates an app that wakes the CPU often and uses
//! little of it, which is the app §13 is entirely about. The obvious repair is to
//! blend CPU time with wakeups — and the obvious repair is refused there, because
//! a blend needs a weight, and a weight nobody measured is a guess wearing a
//! measurement's clothes. §11.1 is a section about exactly that.
//!
//! This is the weight, measured. Both quantities are already sampled machine-wide
//! every tick — cores busy from `/proc/stat`, wakeups from `ctxt` — and the
//! package wattage is sampled beside them. Three numbers a tick is a regression
//! waiting to be run:
//!
//! ```text
//!   package watts  =  floor  +  watts_per_core × cores_busy
//!                             +  joules_per_wakeup × wakeups_per_sec
//! ```
//!
//! Every term is then something the tool can say with a straight face. `floor` is
//! what the package draws with nothing running — the interconnect, the memory
//! controller, the display pipe — and belongs to no app. `watts_per_core` is what
//! a busy core costs here, rather than the 2-4 W §11.1 called the weakest claim
//! in the document. And `joules_per_wakeup` is the number this project has never
//! had: what one wakeup costs, on this hardware, derived rather than asserted.
//!
//! ## It has to be able to fail, and on the machine it was written on, it does
//!
//! A fitted model that always produces coefficients is exactly as unfalsifiable
//! as the constant it replaces. So this refuses in five distinct ways, and the
//! refusal is reported rather than silently swallowed — see [`Verdict`].
//!
//! One of those refusals is not a corner case; it was the first result. Measured
//! here over 90 samples, on a machine under sustained load:
//!
//! | | |
//! |---|---|
//! | cores busy | 6.47 → **16.00** |
//! | wakeups/s | 7,183 → **30,132** |
//! | package watts | 23.18 → **24.28** |
//! | R² | **0.005** |
//!
//! The package did not respond to load at all, and a settled idle reading was
//! 24.2 W — the same number. That is an APU pinned to its firmware power limit:
//! the chip holds its watts and moves its clocks underneath. AMD exposes no
//! `constraint_*` file to read the cap from, so it is inferred from the one
//! signature it cannot hide — watts that will not move while load does.
//!
//! **This matters far beyond the arithmetic.** At the power limit, freezing an
//! app saves no package watts; it hands the budget to whatever else is runnable.
//! A per-app watt figure on a capped machine is a share of a fixed allowance, not
//! energy caused, and every reader will take it for the second. So the cap is
//! reported as its own verdict rather than folded into "the fit was poor".

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// One tick's worth of the three quantities, all measured.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Observation {
    pub cores_busy: f64,
    pub wakeups_per_sec: f64,
    pub package_w: f64,
}

/// How many readings before a fit is offered at all.
///
/// **Chosen, not measured**, and worth saying so: thirty is the conventional
/// "large sample" line in statistics, not something this machine taught anybody.
/// Three coefficients from thirty points is an undemanding ratio and the sampling
/// tick is 2-60s (`sampler::adaptive_interval`), so it costs minutes rather than
/// hours to clear.
///
/// The consequences of it being wrong are asymmetric, which is why it errs low.
/// Too high and the model stays silent on a machine that could have been fitted,
/// and the estimate falls back to the 2-4 J guess it exists to replace. Too low
/// and a fit arrives early and unstable — but it arrives carrying its own R² and
/// standard errors, so it can be seen to be shaky rather than believed.
const MIN_SAMPLES: usize = 30;

/// How many readings are kept.
///
/// A rolling window rather than everything since boot, because the thing being
/// fitted moves: plugging in, a thermal limit engaging, a display going to sleep.
/// A fit averaged over a state change describes neither state.
///
/// The size is **chosen**: 256 readings is between eight minutes and four hours
/// depending on where the adaptive tick sits, which is long enough to hold a
/// varied afternoon and short enough that yesterday's thermal state is gone. Its
/// other job is a hard ceiling on memory, which is the same reason
/// `sampler::MAX_READINGS` exists.
const WINDOW: usize = 256;

/// How much the machine's load must have varied for the fit to mean anything.
///
/// In cores. A regression run over a machine that sat at one load has no leverage
/// on any coefficient — it would return the mean wattage as `floor` and noise for
/// everything else, with an R² that looks respectable because there is nothing to
/// explain.
///
/// **Chosen.** One whole core of movement is a lot to ask of an idle laptop and
/// nothing at all to a busy one; there is no measurement here that settles it,
/// only the observation that a range much smaller than one core is within the
/// noise of `/proc/stat` over a short tick.
const MIN_CORES_RANGE: f64 = 1.0;

/// Above this, the two regressors are one regressor.
///
/// Cores busy and wakeups rise together on most real workloads, and where they
/// rise *exactly* together no arithmetic can say which of them the watts belong
/// to. The coefficients would still come out — enormous, opposite in sign, and
/// unstable between ticks. This is the guard that stops the model inventing a
/// wakeup cost from a machine that never varied one without the other.
///
/// **A convention, named rather than derived.** At r = 0.95 the variance
/// inflation factor — how much collinearity widens a coefficient's standard error
/// — is 1/(1 − r²) ≈ 10, and 10 is the usual textbook line for "these two
/// regressors are one regressor". Nothing measured on this hardware chose it. It
/// is at least checkable: the inflation it permits shows up directly in the
/// standard errors [`Fit`] now carries, so a fit admitted just under this bar
/// reports its own wide error bars rather than hiding behind a tidy R².
const MAX_CORRELATION: f64 = 0.95;

/// How much of the variation the fit must explain to be attributed by.
///
/// **Chosen, not measured, and the weakest number in this file.** Half is a round
/// fraction, not a finding; no experiment here established that a model
/// explaining 49% of the variation is useless and one explaining 51% is sound.
///
/// It is defensible only in what it protects against. Below roughly half, most of
/// what moves the package is something this model cannot see, and coefficients
/// fitted through that much unexplained movement are being asked to carry an
/// explanation they do not have. The cost of it being too strict is falling back
/// to the 2-4 J guess, which is worse but honest; the cost of it being too loose
/// is a confident per-app wattage built on noise. It errs strict for that reason.
///
/// Whatever it is set to, the R² travels with the fit to every renderer, so a
/// reader can disagree with this line without reading this file.
const MIN_R2: f64 = 0.5;

/// Watts of spread below which the package is not responding to load.
///
/// Reached only when the load *did* vary ([`MIN_CORES_RANGE`]), which is what
/// makes flat watts diagnostic rather than merely quiet. Measured on this
/// machine: 1.1 W of spread across a 9.5-core swing, which is a power cap and not
/// a fit.
const POWER_LIMITED_SPREAD_W: f64 = 2.0;

/// The fitted coefficients, each in units somebody can argue with.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fit {
    /// Watts the package draws with no cores busy and nothing waking it.
    ///
    /// Belongs to no app, and is the honest reason a per-app column does not add
    /// up to the header above it.
    pub floor_w: f64,
    /// Watts per fully-busy core.
    pub watts_per_core: f64,
    /// Joules per wakeup.
    ///
    /// Watts per wakeup-per-second *is* joules per wakeup, which is why this is
    /// the one coefficient worth the whole exercise: §13 counts wakeups because
    /// they cost battery, and this is the first time the project can say how
    /// much.
    pub joules_per_wakeup: f64,
    /// Standard error of [`Self::watts_per_core`], in the same units.
    ///
    /// Defaulted so a client older than this field still decodes a daemon that
    /// has it — and a zero there reads as "no error bar offered", which
    /// [`Self::joules_interval`] turns into a point rather than a false range.
    #[serde(default)]
    pub se_watts_per_core: f64,
    /// Standard error of [`Self::joules_per_wakeup`].
    #[serde(default)]
    pub se_joules_per_wakeup: f64,
    /// Covariance of the two slopes.
    ///
    /// Not decoration. Cores busy and wakeups rise together on any real
    /// workload — the fit is refused outright past a correlation of
    /// [`MAX_CORRELATION`] — so an interval built from the two variances alone
    /// would be wrong by however much they move together, and wrong in whichever
    /// direction this machine happens to lean.
    #[serde(default)]
    pub cov_core_wakeup: f64,
    /// Fraction of the variation in watts the two regressors explain.
    pub r2: f64,
    pub samples: usize,
}

impl Fit {
    /// What this app costs, in watts, from its own two measured rates.
    ///
    /// The floor is deliberately absent: it is drawn whether or not this app
    /// exists, and charging a share of it to whoever happened to run is the
    /// error `attribution::Basis` documents for the package as a whole.
    #[must_use]
    pub fn watts(&self, cores: f64, wakeups_per_sec: f64) -> f64 {
        (self.watts_per_core * cores + self.joules_per_wakeup * wakeups_per_sec).max(0.0)
    }

    /// The joules behind `cpu_seconds` of CPU time and `wakeups` wakeups, as a
    /// range.
    ///
    /// The range is the coefficients' own uncertainty propagated through, not a
    /// margin picked to look humble: for `J = a·C + b·W` with `C` and `W` known,
    /// `Var(J) = C²·Var(a) + W²·Var(b) + 2·C·W·Cov(a,b)`. §11's saving has always
    /// been reported as a range because the old 2-4 W basis was a guess with no
    /// error bar to offer; this one has a real one, and it is narrow exactly when
    /// the machine has given the fit enough to be sure.
    ///
    /// Two standard errors rather than 1.96. The difference is under 3% at the
    /// 27 degrees of freedom [`MIN_SAMPLES`] guarantees, and it errs *wide* —
    /// which is the right direction for a number that claims a saving.
    ///
    /// Clamped at zero on the low side. A negative saving is not a thing this can
    /// mean: the counters it is built from only ever counted work that happened.
    #[must_use]
    pub fn joules_interval(&self, cpu_seconds: f64, wakeups: f64) -> (f64, f64) {
        let point = self.watts_per_core * cpu_seconds + self.joules_per_wakeup * wakeups;
        let variance = cpu_seconds.powi(2) * self.se_watts_per_core.powi(2)
            + wakeups.powi(2) * self.se_joules_per_wakeup.powi(2)
            + 2.0 * cpu_seconds * wakeups * self.cov_core_wakeup;
        // A negative variance is arithmetic that has gone wrong somewhere
        // upstream, not a narrow interval.
        let half = 2.0 * variance.max(0.0).sqrt();
        ((point - half).max(0.0), (point + half).max(0.0))
    }
}

/// What the model can currently say, including all the ways it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "verdict")]
pub enum Verdict {
    /// Not enough readings yet. Ordinary, and passes on its own.
    Warming { samples: usize, needed: usize },
    /// The machine held one load throughout, so nothing can be told apart.
    Unvarying { cores_range: f64 },
    /// Load moved and the watts did not: a firmware power cap.
    ///
    /// Not a failed fit — a different fact about the machine, and the one with
    /// consequences: at the cap, holding an app frees budget for another rather
    /// than saving power.
    PowerLimited { watts: f64, cores_range: f64 },
    /// Cores and wakeups moved as one, so their coefficients cannot be separated.
    Collinear { correlation: f64 },
    /// A fit that explains too little of what the watts did.
    Poor { r2: f64 },
    /// A fit that explains plenty and is still not physics.
    ///
    /// Kept apart from [`Self::Poor`] because the two are told apart by nothing
    /// the reader can see, and merging them printed a lie: the first machine to
    /// hit this reported "explains only 82% of the variation; not used", which is
    /// a *good* R² and left the reason for the refusal looking like the reason it
    /// was refused. §10's rule is that the tool says what it cannot do, and a
    /// wrong reason is worse than no reason — it sends somebody after the wrong
    /// thing, which is the same mistake `find_rapl` avoids by not reporting a
    /// malformed counter as a permission problem.
    ///
    /// A core cannot cost negative watts and a wakeup cannot refund energy, so a
    /// tight fit that says otherwise has latched onto something this model cannot
    /// see — most often a package whose clocks are being moved to hold a power
    /// budget, where more cores busy really does mean less power per core.
    Unphysical { r2: f64, watts_per_core: f64, joules_per_wakeup: f64 },
    /// Usable.
    Fitted(Fit),
}

impl Verdict {
    #[must_use]
    pub fn fit(&self) -> Option<Fit> {
        match self {
            Self::Fitted(f) => Some(*f),
            _ => None,
        }
    }

    /// One line for `dozectl status` and the dashboard.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Warming { samples, needed } => {
                format!("power model warming up ({samples}/{needed} readings)")
            }
            Self::Unvarying { cores_range } => format!(
                "power model idle: this machine's load has only moved {cores_range:.1} cores, \
                 too little to fit against"
            ),
            Self::PowerLimited { watts, cores_range } => format!(
                "cpu package is at a fixed {watts:.1} W limit: load moved {cores_range:.1} cores \
                 and the watts did not, so holding an app frees budget for another rather than \
                 saving power"
            ),
            Self::Collinear { correlation } => format!(
                "power model cannot separate cpu from wakeups here: they moved together \
                 (r={correlation:.2})"
            ),
            Self::Poor { r2 } => {
                format!("power model explains only {:.0}% of the variation; not used", r2 * 100.0)
            }
            Self::Unphysical { r2, watts_per_core, joules_per_wakeup } => format!(
                "power model fits well ({:.0}%) and impossibly: {watts_per_core:.2} W per core \
                 and {:.1} µJ per wakeup. Something it cannot see is moving the watts — most \
                 likely clocks being scaled to hold a power budget",
                r2 * 100.0,
                joules_per_wakeup * 1e6
            ),
            Self::Fitted(f) => format!(
                "power model: {:.2} W floor + {:.2} W per busy core + {:.1} µJ per wakeup \
                 (R²={:.2} over {} readings)",
                f.floor_w,
                f.watts_per_core,
                f.joules_per_wakeup * 1e6,
                f.r2,
                f.samples
            ),
        }
    }
}

/// A rolling least-squares fit of package watts against load and wakeups.
#[derive(Debug, Clone, Default)]
pub struct PowerModel {
    samples: VecDeque<Observation>,
}

impl PowerModel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in one tick. Ignores anything that is not a finite, plausible triple —
    /// a reading the meter already refused has nothing to contribute here.
    pub fn observe(&mut self, o: Observation) {
        if !(o.cores_busy.is_finite() && o.wakeups_per_sec.is_finite() && o.package_w.is_finite()) {
            return;
        }
        if o.cores_busy < 0.0 || o.wakeups_per_sec < 0.0 || o.package_w <= 0.0 {
            return;
        }
        self.samples.push_back(o);
        while self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
    }

    /// Forget everything. Called across a suspend, for the same reason
    /// `energy::Meter::reset` is: the machine on the other side may not be the
    /// one these coefficients describe.
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    #[must_use]
    pub fn verdict(&self) -> Verdict {
        let n = self.samples.len();
        if n < MIN_SAMPLES {
            return Verdict::Warming { samples: n, needed: MIN_SAMPLES };
        }
        let cores: Vec<f64> = self.samples.iter().map(|o| o.cores_busy).collect();
        let wakes: Vec<f64> = self.samples.iter().map(|o| o.wakeups_per_sec).collect();
        let watts: Vec<f64> = self.samples.iter().map(|o| o.package_w).collect();

        let cores_range = spread(&cores);
        if cores_range < MIN_CORES_RANGE {
            return Verdict::Unvarying { cores_range };
        }
        // Checked *before* the fit and reported as its own answer. On a capped
        // package the regression still returns coefficients — slightly negative
        // ones, with an R² near zero — and calling that "a poor fit" would hide
        // the actual finding behind a statistic.
        if spread(&watts) < POWER_LIMITED_SPREAD_W {
            return Verdict::PowerLimited { watts: mean(&watts), cores_range };
        }
        if let Some(r) = correlation(&cores, &wakes)
            && r.abs() > MAX_CORRELATION
        {
            return Verdict::Collinear { correlation: r };
        }
        let Some(fit) = solve(&cores, &wakes, &watts) else {
            return Verdict::Poor { r2: 0.0 };
        };
        let [floor_w, watts_per_core, joules_per_wakeup] = fit.b;
        let r2 = fit.r2;
        // A core that costs negative watts, or a wakeup that refunds energy, is
        // not a finding — it is a fit that has latched onto something else.
        // Refused rather than clamped, because a clamped coefficient would go on
        // to be multiplied by a real per-app rate and rendered as a measurement.
        if watts_per_core < 0.0 || joules_per_wakeup < 0.0 {
            return Verdict::Unphysical { r2, watts_per_core, joules_per_wakeup };
        }
        if r2 < MIN_R2 {
            return Verdict::Poor { r2 };
        }
        Verdict::Fitted(Fit {
            floor_w,
            watts_per_core,
            joules_per_wakeup,
            // Square roots of the diagonal, and the off-diagonal as it stands:
            // cores and wakeups are correlated on any real workload, so an
            // interval that ignored their covariance would be wrong in a
            // direction that depends on the machine.
            se_watts_per_core: fit.cov[1][1].max(0.0).sqrt(),
            se_joules_per_wakeup: fit.cov[2][2].max(0.0).sqrt(),
            cov_core_wakeup: fit.cov[1][2],
            r2,
            samples: self.samples.len(),
        })
    }
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

fn spread(v: &[f64]) -> f64 {
    let (lo, hi) = v.iter().fold((f64::MAX, f64::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
    hi - lo
}

/// Pearson's r, or `None` where either side never moved.
fn correlation(a: &[f64], b: &[f64]) -> Option<f64> {
    let (ma, mb) = (mean(a), mean(b));
    let mut sab = 0.0;
    let mut saa = 0.0;
    let mut sbb = 0.0;
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma).powi(2);
        sbb += (y - mb).powi(2);
    }
    (saa > 0.0 && sbb > 0.0).then(|| sab / (saa * sbb).sqrt())
}

/// What a least-squares fit gives back, including how well it knows it.
struct Solution {
    /// Intercept, then the two slopes.
    b: [f64; 3],
    r2: f64,
    /// σ²·(X'X)⁻¹ — the coefficients' variance-covariance matrix.
    ///
    /// Carried because the caller has to report a *range* and the only honest one
    /// comes from here. The alternative was a point estimate widened by some
    /// invented percentage, which is the guess this whole module replaces.
    cov: [[f64; 3]; 3],
}

/// Ordinary least squares for `y = b0 + b1·x1 + b2·x2`.
///
/// Three unknowns, so the normal equations are a 3×3. It is solved by inverting
/// `X'X` outright rather than by elimination alone, because the inverse is what
/// the standard errors need and computing it twice would be the more complicated
/// choice. Small enough to write out and to test against a case whose answer is
/// known exactly — `an_exact_relationship_is_recovered_exactly` does that.
///
/// `None` when the system is singular, which the guards in [`PowerModel::verdict`]
/// should already have caught; returning it rather than dividing by zero is the
/// belt to their braces.
fn solve(x1: &[f64], x2: &[f64], y: &[f64]) -> Option<Solution> {
    let n = y.len();
    // Three coefficients, so three degrees of freedom are spent estimating them
    // and σ² needs what is left. Fewer than four points leaves nothing.
    if n <= 3 {
        return None;
    }
    let rows: Vec<[f64; 3]> = (0..n).map(|i| [1.0, x1[i], x2[i]]).collect();
    let mut xtx = [[0.0f64; 3]; 3];
    let mut xty = [0.0f64; 3];
    for i in 0..3 {
        for j in 0..3 {
            xtx[i][j] = (0..n).map(|m| rows[m][i] * rows[m][j]).sum();
        }
        xty[i] = (0..n).map(|m| rows[m][i] * y[m]).sum();
    }
    let inv = invert(xtx)?;
    let b: [f64; 3] = std::array::from_fn(|i| (0..3).map(|j| inv[i][j] * xty[j]).sum::<f64>());

    let ybar = mean(y);
    let ss_res: f64 = (0..n).map(|m| (y[m] - (b[0] + b[1] * x1[m] + b[2] * x2[m])).powi(2)).sum();
    let ss_tot: f64 = y.iter().map(|v| (v - ybar).powi(2)).sum();
    let r2 = if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { 0.0 };
    // Residual variance per degree of freedom. This is what turns a fit into a
    // fit *with an error bar*, and without it the model could only ever offer a
    // confident-looking point.
    let sigma2 = ss_res / (n - 3) as f64;
    let cov = std::array::from_fn(|i| std::array::from_fn(|j| sigma2 * inv[i][j]));
    Some(Solution { b, r2, cov })
}

/// A 3×3 inverse by Gauss-Jordan with partial pivoting.
fn invert(m: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    // `[m | I]`; the identity comes out as the inverse.
    let mut a: [[f64; 6]; 3] = std::array::from_fn(|i| {
        std::array::from_fn(|j| if j < 3 { m[i][j] } else { f64::from(u8::from(j - 3 == i)) })
    });
    for i in 0..3 {
        let p = (i..3).max_by(|&r, &s| a[r][i].abs().total_cmp(&a[s][i].abs()))?;
        a.swap(i, p);
        let pivot = a[i][i];
        if pivot.abs() < 1e-12 {
            return None;
        }
        for cell in &mut a[i] {
            *cell /= pivot;
        }
        for r in 0..3 {
            if r != i {
                let f = a[r][i];
                let row = a[i];
                for (c, cell) in a[r].iter_mut().enumerate() {
                    *cell -= f * row[c];
                }
            }
        }
    }
    Some(std::array::from_fn(|i| std::array::from_fn(|j| a[i][j + 3])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(obs: impl IntoIterator<Item = (f64, f64, f64)>) -> PowerModel {
        let mut m = PowerModel::new();
        for (cores_busy, wakeups_per_sec, package_w) in obs {
            m.observe(Observation { cores_busy, wakeups_per_sec, package_w });
        }
        m
    }

    /// The arithmetic, against a case whose answer is known exactly.
    ///
    /// Everything else here is about refusing to answer; this is the one test
    /// that checks the answer, and it is worth having because a 3×3 solved by
    /// hand is exactly the kind of code that is subtly transposed and still
    /// returns plausible numbers.
    #[test]
    fn an_exact_relationship_is_recovered_exactly() {
        // 4 W of floor, 1.5 W per busy core, 20 µJ per wakeup.
        let obs = (0..40).map(|i| {
            let cores = f64::from(i) * 0.4;
            let wakes = 500.0 + f64::from(i % 7) * 900.0;
            (cores, wakes, 4.0 + 1.5 * cores + 20e-6 * wakes)
        });
        let fit = model(obs).verdict().fit().expect("a noiseless relationship must fit");
        assert!((fit.floor_w - 4.0).abs() < 1e-6, "{fit:?}");
        assert!((fit.watts_per_core - 1.5).abs() < 1e-6, "{fit:?}");
        assert!((fit.joules_per_wakeup - 20e-6).abs() < 1e-9, "{fit:?}");
        assert!(fit.r2 > 0.999, "{fit:?}");
    }

    /// The first real result this model produced, and the reason the verdict is
    /// an enum rather than an `Option<Fit>`.
    ///
    /// Measured over 90 samples on the development machine under sustained load:
    /// cores busy swung 6.47 → 16.00 and wakeups 7,183 → 30,132 while the package
    /// stayed between 23.18 and 24.28 W, for an R² of 0.005. A settled idle
    /// reading was 24.2 W — the same number. That is an APU holding its firmware
    /// power limit and moving its clocks underneath, and calling it "a poor fit"
    /// would bury the one finding with consequences: at the cap, holding an app
    /// frees budget for whatever else is runnable rather than saving power.
    #[test]
    fn a_package_pinned_to_its_limit_is_reported_as_capped_not_as_a_bad_fit() {
        let obs = (0..90).map(|i| {
            let cores = 6.47 + f64::from(i % 20) * 0.5;
            let wakes = 7_183.0 + f64::from(i % 23) * 1_000.0;
            (cores, wakes, 23.18 + f64::from(i % 11) * 0.1)
        });
        match model(obs).verdict() {
            Verdict::PowerLimited { watts, cores_range } => {
                assert!((23.0..25.0).contains(&watts), "{watts}");
                assert!(cores_range > MIN_CORES_RANGE, "{cores_range}");
            }
            other => panic!("expected a power cap, got {other:?}"),
        }
    }

    /// A machine that sat at one load gives the regression no leverage: it would
    /// return the mean wattage as `floor` and noise for the rest, with an R² that
    /// looks respectable because there was nothing to explain.
    #[test]
    fn a_machine_that_held_one_load_is_not_fitted_against() {
        let obs = (0..60).map(|i| (4.0 + f64::from(i % 3) * 0.1, 9_000.0, 12.0 + f64::from(i % 5)));
        assert!(matches!(model(obs).verdict(), Verdict::Unvarying { .. }));
    }

    /// Cores and wakeups rise together on most real workloads. Where they rise
    /// *exactly* together, no arithmetic can say which of them the watts belong
    /// to — the coefficients still come out, enormous and opposite in sign, and
    /// they would be multiplied by real per-app rates and rendered as watts.
    #[test]
    fn cores_and_wakeups_that_moved_as_one_cannot_be_told_apart() {
        let obs = (0..60).map(|i| {
            let cores = f64::from(i) * 0.25;
            (cores, cores * 1_000.0, 5.0 + cores * 1.2)
        });
        match model(obs).verdict() {
            Verdict::Collinear { correlation } => assert!(correlation.abs() > MAX_CORRELATION),
            other => panic!("expected collinearity, got {other:?}"),
        }
    }

    /// A model that explains nothing must not be attributed by, however
    /// confidently it can multiply.
    #[test]
    fn a_fit_that_explains_nothing_is_not_attributed_by() {
        // Watts driven by something this model cannot see, and varying far too
        // much to be a power cap.
        let obs = (0..80).map(|i| {
            let cores = f64::from(i % 17);
            let wakes = 1_000.0 + f64::from(i % 5) * 3_000.0;
            (cores, wakes, 10.0 + f64::from((i * 37) % 23))
        });
        assert!(matches!(model(obs).verdict(), Verdict::Poor { .. }));
    }

    /// A core that costs negative watts, or a wakeup that refunds energy, is a
    /// fit that has latched onto something else. Refused rather than clamped: a
    /// clamped coefficient goes on to be multiplied by a real rate and rendered
    /// as a measurement.
    ///
    /// And refused *under its own name*. The first machine to reach this reported
    /// "explains only 82% of the variation; not used" — a good R² offered as the
    /// reason it was rejected — because the check shared a verdict with the
    /// poor-fit one. A wrong reason sends somebody after the wrong thing.
    #[test]
    fn a_wakeup_that_refunds_energy_is_refused_rather_than_clamped() {
        // Watts fall cleanly as wakeups rise — a tight fit, and an impossible one.
        let obs = (0..60).map(|i| {
            let cores = f64::from(i % 13);
            let wakes = f64::from(i) * 400.0;
            (cores, wakes, 40.0 + cores * 0.5 - wakes * 1e-3)
        });
        let verdict = model(obs).verdict();
        assert!(verdict.fit().is_none(), "an energy-refunding wakeup must not fit: {verdict:?}");
        match verdict {
            Verdict::Unphysical { r2, joules_per_wakeup, .. } => {
                assert!(r2 > MIN_R2, "the point is that it fits well: {r2}");
                assert!(joules_per_wakeup < 0.0);
                assert!(
                    !verdict.summary().contains("explains only"),
                    "a good fit must not be reported as a bad one: {}",
                    verdict.summary()
                );
            }
            other => panic!("expected an unphysical fit, got {other:?}"),
        }
    }

    #[test]
    fn a_model_needs_readings_before_it_is_a_model() {
        let m = model((0..5).map(|i| (f64::from(i), 1_000.0, 5.0)));
        match m.verdict() {
            Verdict::Warming { samples, needed } => {
                assert_eq!(samples, 5);
                assert_eq!(needed, MIN_SAMPLES);
            }
            other => panic!("expected warming, got {other:?}"),
        }
    }

    /// The floor is drawn whether or not this app exists, so charging a share of
    /// it to whoever happened to run is the same error `attribution::Basis`
    /// documents for the package as a whole.
    #[test]
    fn the_floor_is_not_charged_to_any_app() {
        let fit = Fit {
            floor_w: 9.0,
            watts_per_core: 2.0,
            joules_per_wakeup: 1e-4,
            r2: 0.9,
            samples: 50,
            se_watts_per_core: 0.05,
            se_joules_per_wakeup: 1e-6,
            cov_core_wakeup: 0.0,
        };
        assert_eq!(fit.watts(0.0, 0.0), 0.0, "an app doing nothing costs nothing");
        assert!((fit.watts(1.0, 0.0) - 2.0).abs() < 1e-9);
        // The whole point of the exercise: an app using almost no CPU and waking
        // the CPU 900 times a second is charged for the wakeups. Under the
        // CPU-time proxy it would have been charged for almost nothing.
        assert!((fit.watts(0.01, 900.0) - (0.02 + 0.09)).abs() < 1e-9);
    }

    #[test]
    fn the_window_does_not_grow_without_bound() {
        let m = model((0..2_000).map(|i| (f64::from(i % 16), 5_000.0, 10.0)));
        assert_eq!(m.samples.len(), WINDOW);
    }

    /// The meter already refuses implausible wattages (`energy::IMPLAUSIBLE_W`);
    /// anything that reaches here and is still not a number must not become one
    /// third of a coefficient.
    #[test]
    fn a_reading_that_is_not_a_measurement_is_not_folded_in() {
        let mut m = PowerModel::new();
        for bad in [
            Observation { cores_busy: f64::NAN, wakeups_per_sec: 1.0, package_w: 5.0 },
            Observation { cores_busy: 1.0, wakeups_per_sec: f64::INFINITY, package_w: 5.0 },
            Observation { cores_busy: 1.0, wakeups_per_sec: 1.0, package_w: 0.0 },
            Observation { cores_busy: -1.0, wakeups_per_sec: 1.0, package_w: 5.0 },
        ] {
            m.observe(bad);
        }
        assert_eq!(m.samples.len(), 0);
    }

    /// Across a suspend the counters keep running and the wall clock does not,
    /// and the machine on the other side may not be the one these coefficients
    /// describe — a fit averaged over a state change describes neither state.
    #[test]
    fn a_suspend_forgets_the_machine_it_had_measured() {
        let mut m = model((0..60).map(|i| (f64::from(i % 16), 5_000.0, 10.0)));
        assert!(!m.samples.is_empty());
        m.reset();
        assert!(matches!(m.verdict(), Verdict::Warming { samples: 0, .. }));
    }

    /// Every refusal has to say what it is, because a blank where a wattage was
    /// expected is the silence §10 exists to stop.
    #[test]
    fn every_verdict_says_what_it_is() {
        let fit = Fit {
            floor_w: 9.0,
            watts_per_core: 2.0,
            joules_per_wakeup: 2e-5,
            r2: 0.87,
            samples: 90,
            se_watts_per_core: 0.05,
            se_joules_per_wakeup: 1e-6,
            cov_core_wakeup: 0.0,
        };
        for (verdict, want) in [
            (Verdict::Warming { samples: 3, needed: 30 }, "3/30"),
            (Verdict::Unvarying { cores_range: 0.2 }, "0.2 cores"),
            (Verdict::PowerLimited { watts: 24.2, cores_range: 9.5 }, "24.2 W limit"),
            (Verdict::Collinear { correlation: 0.99 }, "moved together"),
            (Verdict::Poor { r2: 0.1 }, "10%"),
            (
                Verdict::Unphysical { r2: 0.82, watts_per_core: -0.5, joules_per_wakeup: 1e-6 },
                "fits well (82%)",
            ),
            (Verdict::Fitted(fit), "20.0 µJ per wakeup"),
        ] {
            let text = verdict.summary();
            assert!(text.contains(want), "{verdict:?} rendered as {text:?}, wanted {want:?}");
        }
        // The one verdict with a consequence spells the consequence out.
        assert!(
            Verdict::PowerLimited { watts: 24.2, cores_range: 9.5 }
                .summary()
                .contains("rather than saving power")
        );
    }
}
