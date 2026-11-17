//! The small amount of statistics a measured saving needs.
//!
//! §11 has always reported a **model**: CPU-seconds multiplied by a 2-4 W guess
//! at what a busy core costs. A model cannot be caught being wrong, which is the
//! problem with it — the number moves when dozed does more work, whether or not
//! that work saved anything. The honest alternative is to measure the battery,
//! and the moment you measure a battery on a machine somebody is *using*, the
//! arithmetic stops being "divide by 3600" and starts being "is this difference
//! real".
//!
//! Two deliberate choices, both of which follow from how little data a laptop
//! can produce.
//!
//! * **Paired, not two independent groups.** Discharge drifts across a session:
//!   voltage sags as the battery empties, a browser tab that was busy settles
//!   down, the room warms up and the fan starts. A paired design differences all
//!   of that away as long as it moves slowly compared with one block. It is also
//!   the reason [`crate::trial`] alternates arms *within* a pair instead of
//!   running every live block first — the same total time, arranged so the
//!   confounder cancels.
//! * **Non-parametric.** A pair is forty minutes, so an evening yields single
//!   digits of them, and discharge is visibly not normal — it is a floor with
//!   spikes on top. [`Paired::sign_flip_p`] assumes only that under the null
//!   hypothesis a pair's difference is as likely to be negative as positive,
//!   which is precisely what randomising the order inside each pair buys. No
//!   t-distribution, no normal approximation, nothing to be quietly wrong about
//!   at n = 6.
//!
//! Everything here is deterministic given its seed, because a verdict that
//! changes when you ask twice is not a verdict.

/// xorshift64\*, seeded, for the resampling below.
///
/// Deliberately not a dependency and deliberately not `rand`: this crate is pure
/// domain logic and the requirement is reproducibility, not cryptographic
/// quality. Both consumers are resamplers, where the only thing that matters is
/// that the stream is fixed by the seed and does not have short cycles.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A zero seed would make xorshift produce nothing but zeroes forever, so it
    /// is replaced rather than rejected: the caller's seed often comes from a
    /// timestamp or a config file and "0" is a plausible thing to write.
    #[must_use]
    pub const fn seeded(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A coin flip.
    pub fn flip(&mut self) -> bool {
        // The top bit, not the bottom one: xorshift's low bits are the weakest
        // part of the generator.
        self.next_u64() >> 63 == 1
    }

    /// A number below `n`, by rejection so that the result is not skewed towards
    /// small values the way `% n` is. `n == 0` has no valid answer and returns 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let n = n as u64;
        // The largest multiple of n that fits, so the tail above it can be
        // rejected instead of folded back over the low numbers.
        let limit = u64::MAX - (u64::MAX % n) - 1;
        loop {
            let v = self.next_u64();
            if v <= limit {
                return (v % n) as usize;
            }
        }
    }
}

/// Above this many pairs the permutation test is sampled rather than enumerated.
///
/// 2^16 sign patterns is a millisecond of work and an *exact* p-value; 2^20
/// would be a second of it, in a code path that runs whenever someone types
/// `dozectl trial`. Above the threshold the Monte Carlo estimate is what is
/// reported, and its resolution (1 in `PERMUTATIONS`) is better than the data
/// deserves anyway.
const EXHAUSTIVE_UP_TO: usize = 16;

/// Sign patterns drawn when there are too many to enumerate.
const PERMUTATIONS: usize = 20_000;

/// Resamples for the bootstrap interval.
const RESAMPLES: usize = 10_000;

/// Paired differences, and the questions worth asking of them.
///
/// One entry per pair, and the sign convention is the caller's business —
/// [`crate::trial`] stores `dry-run minus live`, so a positive mean is a saving.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Paired {
    pub differences: Vec<f64>,
}

impl Paired {
    #[must_use]
    pub fn new(differences: Vec<f64>) -> Self {
        Self { differences }
    }

    /// From `(a, b)` pairs, as `a - b`.
    #[must_use]
    pub fn from_pairs(pairs: &[(f64, f64)]) -> Self {
        Self { differences: pairs.iter().map(|(a, b)| a - b).collect() }
    }

    #[must_use]
    pub fn n(&self) -> usize {
        self.differences.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.differences.is_empty()
    }

    /// The mean paired difference: the effect estimate itself.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        if self.differences.is_empty() {
            return None;
        }
        Some(self.differences.iter().sum::<f64>() / self.differences.len() as f64)
    }

    /// Two-sided p-value from flipping the signs of the paired differences.
    ///
    /// The null hypothesis is that dozing changed nothing, in which case which
    /// arm of a pair happened to draw less is a coin toss — so the distribution
    /// of the mean difference under the null is obtained by trying the coin
    /// tosses. The p-value is the fraction of sign patterns whose mean is at
    /// least as extreme as the one observed.
    ///
    /// Exact for up to [`EXHAUSTIVE_UP_TO`] pairs and sampled above it, where the
    /// add-one convention (`(hits + 1) / (draws + 1)`) is used so that a Monte
    /// Carlo p-value can never come out as a flat zero it has not earned.
    ///
    /// Note the floor this puts under a small trial, which is a feature: with one
    /// pair there are two sign patterns and both are as extreme as each other, so
    /// `p = 1.0` however large the difference looked. One pair is an anecdote and
    /// the arithmetic says so without anyone having to remember to.
    #[must_use]
    pub fn sign_flip_p(&self, seed: u64) -> Option<f64> {
        let n = self.n();
        if n == 0 {
            return None;
        }
        let observed = self.mean()?.abs();
        // Compared against the sum rather than the mean: same ordering, one
        // division per pattern saved, and no rounding difference between the two.
        let observed_sum = observed * n as f64;

        if n <= EXHAUSTIVE_UP_TO {
            let patterns = 1u64 << n;
            let mut as_extreme = 0u64;
            for pattern in 0..patterns {
                let mut sum = 0.0;
                for (i, d) in self.differences.iter().enumerate() {
                    if pattern >> i & 1 == 1 { sum -= d } else { sum += d }
                }
                // `>=` and not `>`: the observed pattern is one of the ones
                // enumerated, so this can never report p = 0, which would be a
                // claim no finite experiment can make.
                if sum.abs() >= observed_sum - f64::EPSILON {
                    as_extreme += 1;
                }
            }
            return Some(as_extreme as f64 / patterns as f64);
        }

        let mut rng = Rng::seeded(seed);
        let mut as_extreme = 0usize;
        for _ in 0..PERMUTATIONS {
            let mut sum = 0.0;
            for d in &self.differences {
                if rng.flip() { sum -= d } else { sum += d }
            }
            if sum.abs() >= observed_sum - f64::EPSILON {
                as_extreme += 1;
            }
        }
        Some((as_extreme + 1) as f64 / (PERMUTATIONS + 1) as f64)
    }

    /// A percentile bootstrap interval for the mean difference.
    ///
    /// Resamples pairs with replacement, which is the assumption-free way to get
    /// an interval out of a handful of numbers whose distribution nobody knows.
    /// `conf` is the coverage, so 0.95 returns the 2.5th and 97.5th percentiles
    /// of the resampled means.
    ///
    /// Nearest-rank percentiles, not interpolated: with 10,000 resamples the
    /// difference is far below the width of the interval, and an interval that
    /// only ever reports values the data could have produced is easier to defend.
    ///
    /// An interval that spans zero is the whole reason this is reported at all —
    /// see [`crate::trial::Verdict::render`], which declines to claim a saving in
    /// that case rather than quoting the mean and hoping.
    #[must_use]
    pub fn bootstrap_interval(&self, seed: u64, conf: f64) -> Option<(f64, f64)> {
        let n = self.n();
        if n == 0 {
            return None;
        }
        let mut rng = Rng::seeded(seed);
        let mut means: Vec<f64> = Vec::with_capacity(RESAMPLES);
        for _ in 0..RESAMPLES {
            let mut sum = 0.0;
            for _ in 0..n {
                sum += self.differences[rng.below(n)];
            }
            means.push(sum / n as f64);
        }
        means.sort_by(f64::total_cmp);

        let tail = (1.0 - conf.clamp(0.0, 1.0)) / 2.0;
        let lo = ((RESAMPLES as f64 * tail) as usize).min(RESAMPLES - 1);
        let hi = ((RESAMPLES as f64 * (1.0 - tail)) as usize).min(RESAMPLES - 1);
        Some((means[lo], means[hi]))
    }
}

/// Mean of a slice, or `None` when there is nothing to average.
///
/// Exists so that "no readings" and "readings that averaged zero" cannot be
/// confused, which they were: a block with no samples reported 0 W and read as
/// the most efficient block of the trial.
#[must_use]
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_answer_twice() {
        // A verdict that changes when you ask again is not a verdict, and both
        // routines here are randomised.
        let p = Paired::new(vec![
            0.4, 0.2, 0.9, -0.1, 0.5, 0.3, 0.7, 0.1, 0.6, 0.2, 0.8, 0.4, 0.3, 0.5, 0.2, 0.6, 0.4,
        ]);
        assert_eq!(p.sign_flip_p(7), p.sign_flip_p(7));
        assert_eq!(p.bootstrap_interval(7, 0.95), p.bootstrap_interval(7, 0.95));
        assert_ne!(p.sign_flip_p(7), p.sign_flip_p(8), "and a different seed is a different draw");
    }

    #[test]
    fn a_consistent_difference_is_significant_and_a_symmetric_one_is_not() {
        // Six pairs that all point the same way is the smallest result worth
        // reporting; the same six magnitudes with alternating signs is noise.
        let real = Paired::new(vec![0.5, 0.6, 0.4, 0.7, 0.5, 0.6]);
        let noise = Paired::new(vec![0.5, -0.6, 0.4, -0.7, 0.5, -0.6]);

        let p_real = real.sign_flip_p(1).unwrap();
        let p_noise = noise.sign_flip_p(1).unwrap();
        assert!(p_real < 0.05, "six pairs agreeing should be significant, got {p_real}");
        assert!(p_noise > 0.5, "alternating signs are not evidence, got {p_noise}");
    }

    #[test]
    fn all_differences_the_same_sign_gives_the_smallest_p_the_design_allows() {
        // Only the observed pattern and its mirror image are as extreme, so the
        // floor is 2 / 2^n. This pins the exact-enumeration branch: an off-by-one
        // in the pattern loop shows up here and nowhere else.
        for n in 1..=8 {
            let p = Paired::new(vec![1.0; n]);
            let expected = 2.0 / f64::from(1u32 << n);
            assert!(
                (p.sign_flip_p(1).unwrap() - expected).abs() < 1e-12,
                "n={n}: expected {expected}, got {:?}",
                p.sign_flip_p(1)
            );
        }
    }

    #[test]
    fn one_pair_proves_nothing_however_large_it_looks() {
        // The property that keeps an eager user honest: a single 3 W difference
        // between one live block and one dry-run block is an anecdote, and there
        // is no arrangement of one pair that makes it anything else.
        let p = Paired::new(vec![3.0]);
        assert_eq!(p.sign_flip_p(1), Some(1.0));
        assert_eq!(p.mean(), Some(3.0), "the estimate is still reported; only its weight is not");
    }

    #[test]
    fn no_pairs_answers_nothing_rather_than_zero() {
        // Zero would render as "measured: no difference", which is a claim.
        let p = Paired::default();
        assert_eq!(p.mean(), None);
        assert_eq!(p.sign_flip_p(1), None);
        assert_eq!(p.bootstrap_interval(1, 0.95), None);
        assert_eq!(mean(&[]), None);
    }

    #[test]
    fn a_monte_carlo_p_value_is_never_flatly_zero() {
        // Above the enumeration threshold the p-value is an estimate from 20,000
        // draws, and (hits + 1) / (draws + 1) is what stops it claiming a
        // certainty no finite experiment has.
        let p = Paired::new(vec![1.0; EXHAUSTIVE_UP_TO + 4]);
        let got = p.sign_flip_p(3).unwrap();
        assert!(got > 0.0, "a sampled p-value must have a floor");
        assert!(got < 0.001, "but 20 pairs agreeing is still overwhelming: {got}");
    }

    #[test]
    fn the_bootstrap_interval_brackets_the_estimate() {
        let p = Paired::new(vec![0.8, 0.4, 0.6, 0.5, 0.7, 0.3, 0.9, 0.5]);
        let mean = p.mean().unwrap();
        let (lo, hi) = p.bootstrap_interval(11, 0.95).unwrap();
        assert!(lo < mean && mean < hi, "{lo} < {mean} < {hi}");
        assert!(lo > 0.0, "a sample this consistent should not straddle zero: {lo}..{hi}");

        // And a sample that genuinely says nothing must straddle it, or the
        // caller's honesty check has nothing to trigger on.
        let noise = Paired::new(vec![0.8, -0.7, 0.6, -0.9, 0.2, -0.3, 0.5, -0.4]);
        let (lo, hi) = noise.bootstrap_interval(11, 0.95).unwrap();
        assert!(lo < 0.0 && hi > 0.0, "expected an interval spanning zero, got {lo}..{hi}");
    }

    #[test]
    fn a_constant_sample_has_no_spread() {
        // Every resample is the same number, so the interval collapses. Worth
        // pinning because it is the one case where a zero-width interval is
        // correct rather than a bug.
        let p = Paired::new(vec![2.0; 5]);
        assert_eq!(p.bootstrap_interval(1, 0.95), Some((2.0, 2.0)));
    }

    #[test]
    fn a_wider_confidence_level_gives_a_wider_interval() {
        let p = Paired::new(vec![0.9, 0.2, 0.7, 0.4, 0.6, 0.1, 0.8, 0.3]);
        let (lo95, hi95) = p.bootstrap_interval(5, 0.95).unwrap();
        let (lo50, hi50) = p.bootstrap_interval(5, 0.50).unwrap();
        assert!(lo95 <= lo50 && hi50 <= hi95, "95%: {lo95}..{hi95}, 50%: {lo50}..{hi50}");
    }

    #[test]
    fn from_pairs_subtracts_in_the_documented_order() {
        // The sign convention is load-bearing: `trial` stores dry-run minus live
        // so that a positive mean reads as a saving. Getting it backwards would
        // report every real saving as a cost, plausibly and quietly.
        let p = Paired::from_pairs(&[(6.0, 5.0), (6.5, 5.5)]);
        assert_eq!(p.differences, vec![1.0, 1.0]);
        assert_eq!(p.mean(), Some(1.0));
    }

    #[test]
    fn the_generator_is_not_obviously_broken() {
        // Not a statistical test suite — just enough to catch a stuck state or a
        // modulus that cannot reach its own bound, both of which would silently
        // ruin every interval above.
        let mut rng = Rng::seeded(0);
        let first = rng.next_u64();
        assert_ne!(first, rng.next_u64(), "a stuck generator resamples one value forever");

        let mut seen = [false; 5];
        let mut heads = 0;
        for _ in 0..5_000 {
            seen[rng.below(5)] = true;
            if rng.flip() {
                heads += 1;
            }
        }
        assert!(seen.iter().all(|s| *s), "below(5) must be able to return every index");
        assert!((2_000..3_000).contains(&heads), "a coin this biased would skew every p: {heads}");
        assert_eq!(rng.below(0), 0, "and a degenerate bound must not loop forever");
        assert_eq!(rng.below(1), 0);
    }

    #[test]
    fn a_zero_seed_still_generates() {
        // xorshift stays at zero forever from a zero state, and "0" is exactly
        // what a config file or an uninitialised timestamp offers.
        let mut rng = Rng::seeded(0);
        assert_ne!(rng.next_u64(), 0);
    }
}
