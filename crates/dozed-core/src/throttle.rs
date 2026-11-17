//! The dynamic throttle: a CPU cap that converges on a wakeup budget.
//!
//! `action = { throttle = 5 }` picks a number and hopes. This picks it by
//! measurement: `action = { throttle-to = 50 }` says "cap this app wherever it
//! needs to be so it costs about 50 wakeups a second", and the cap moves until
//! it does.
//!
//! # The cap does not control the metric directly
//!
//! Worth stating plainly, because it bounds what this can do. `cpu.max` limits
//! CPU *time*, not wakeup frequency. A timer that fires at 60 Hz still fires at
//! 60 Hz under a 5% cap; what changes is how much work the app completes per
//! wake, which lengthens its event loop and *indirectly* coalesces the wakeups
//! that follow from that work. So the loop has real gain for an app whose
//! wakeups are its own churn, and close to none for one whose wakeups are a
//! fixed-rate timer or inbound network traffic.
//!
//! Rather than pretend otherwise, the controller measures whether it is
//! working. Tightening the cap without moving the rate counts as a stall, and
//! after [`STALL_LIMIT`] of them it stops tightening and says so in the journal.
//! An app that cannot be quietened by capping should be left at a cap that at
//! least bounds its CPU, not squeezed to the floor for nothing.
//!
//! # Why multiplicative, and why damped
//!
//! `cap *= target / measured` is the natural law here: both quantities are
//! rates, and the correction needed is a ratio rather than a difference. It also
//! degrades gracefully — an app at ten times its budget gets a tenfold cut in
//! intent, and the damping decides how fast it actually arrives.
//!
//! The damping matters more than the law. The sample this reads is an average
//! over `cpu_sample_window` (60s by default) taken on an interval that itself
//! moves between 2s and 60s, so the loop is running against a lagged, noisy
//! measurement. Undamped, a single spike would slam an app to the floor and the
//! recovery would overshoot back up. [`MAX_STEP`] bounds any one move to a
//! halving or a doubling, which costs a few samples of convergence and buys
//! immunity to one bad reading.

use serde::{Deserialize, Serialize};

/// Never cap below this. A cap low enough to stop an app responding at all
/// makes it look crashed, which §1 forbids more strongly than it wants the
/// battery.
pub const FLOOR_PCT: u8 = 5;

/// The most any single step may move the cap, as a ratio. One sample can halve
/// the cap or double it, and no more.
const MAX_STEP: f32 = 2.0;

/// Consecutive fruitless tightenings before the controller gives up.
pub const STALL_LIMIT: u8 = 3;

/// A tightening that fails to move the rate by at least this fraction did not
/// work. Loose on purpose: the measurement is noisy, and demanding a large
/// improvement would read normal variation as progress.
const PROGRESS: f32 = 0.10;

/// Caps are rounded to this, so a cap wandering between 23% and 24% does not
/// produce a journal entry and a `cpu.max` write on every sample.
const QUANTUM: u8 = 5;

/// What the controller knows about one app, carried in the planner's memory and
/// recorded in the trace so a cap can be explained after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ThrottleState {
    /// The cap now in force, as a percentage of one core. `None` before the
    /// first step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u8>,
    /// The rate measured when that cap was chosen, to tell progress from stall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rate: Option<f32>,
    /// Consecutive tightenings that did not move the rate.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stalls: u8,
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

/// One step of the loop: where the cap should go, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub cap: u8,
    pub state: ThrottleState,
    /// Phrased for the journal, which is the only place a moving number can be
    /// justified to the person whose app it is.
    pub detail: String,
}

impl ThrottleState {
    /// Whether the controller has stopped trying because capping was not
    /// working. Held at the last useful cap rather than released: it still
    /// bounds the app's CPU even when it cannot bound its wakeups.
    #[must_use]
    pub const fn is_stalled(&self) -> bool {
        self.stalls >= STALL_LIMIT
    }

    /// Compute the next cap from the newest sample. Pure: the planner stays a
    /// pure function of (world, event, time), so a recorded trace replays to
    /// the identical cap sequence.
    #[must_use]
    pub fn step(&self, measured: f32, target: u32) -> Step {
        let target = target.max(1) as f32;
        // No cap yet means the app is running free, so that is the starting
        // point the ratio is applied to.
        let current = self.cap.unwrap_or(100);

        if self.is_stalled() {
            return Step {
                cap: current,
                state: *self,
                detail: format!(
                    "holding at {current}% — capping is not moving the rate \
                     ({measured:.0}/s against a {target:.0}/s budget)"
                ),
            };
        }

        let ratio = (target / measured.max(1.0)).clamp(1.0 / MAX_STEP, MAX_STEP);
        let raw = (f32::from(current) * ratio).round();
        let cap = quantise(raw.clamp(f32::from(FLOOR_PCT), 100.0) as u8);

        // A tightening is fruitless if the rate did not fall meaningfully since
        // the cap that produced it. Only tightenings count: leaving the cap
        // alone, or relaxing it, is not the controller failing to have an
        // effect.
        let tightened = cap < current;
        let improved = self
            .last_rate
            .is_none_or(|prev| measured <= prev * (1.0 - PROGRESS) || measured < target);
        let stalls = if tightened && !improved { self.stalls + 1 } else { 0 };

        let detail = if cap == current {
            format!("holding at {cap}% — {measured:.0}/s against a {target:.0}/s budget")
        } else {
            let verb = if cap < current { "tightening" } else { "relaxing" };
            format!("{verb} {current}% → {cap}% — {measured:.0}/s against a {target:.0}/s budget")
        };
        Step {
            cap,
            state: ThrottleState { cap: Some(cap), last_rate: Some(measured), stalls },
            detail,
        }
    }
}

/// Round to the nearest [`QUANTUM`], never below the floor.
fn quantise(pct: u8) -> u8 {
    let q = QUANTUM;
    let rounded = ((u16::from(pct) + u16::from(q) / 2) / u16::from(q)) * u16::from(q);
    (rounded as u8).clamp(FLOOR_PCT, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the loop against a plant where capping genuinely reduces wakeups,
    /// with the rate proportional to the CPU the app is allowed.
    fn converge(target: u32, free_rate: f32, steps: usize) -> Vec<u8> {
        let mut state = ThrottleState::default();
        let mut caps = Vec::new();
        for _ in 0..steps {
            let measured = free_rate * f32::from(state.cap.unwrap_or(100)) / 100.0;
            let step = state.step(measured, target);
            state = step.state;
            caps.push(step.cap);
        }
        caps
    }

    #[test]
    fn it_converges_on_the_budget_without_overshooting_to_the_floor() {
        let caps = converge(50, 1000.0, 12);
        let settled = *caps.last().unwrap();
        // 50 of 1000 is 5% of the app's free rate, so that is where it belongs.
        assert!((5..=10).contains(&settled), "settled at {settled}%, caps were {caps:?}");
        assert!(caps.iter().all(|c| *c >= FLOOR_PCT), "never below the floor: {caps:?}");
    }

    #[test]
    fn no_single_sample_can_slam_the_cap_to_the_floor() {
        // The measurement is a lagged average taken on a moving interval, so one
        // spike must cost a few samples of convergence rather than the app's
        // responsiveness.
        let state = ThrottleState { cap: Some(80), last_rate: Some(60.0), stalls: 0 };
        let step = state.step(100_000.0, 50);
        assert_eq!(step.cap, 40, "a 2000x overshoot still only halves the cap");
    }

    #[test]
    fn a_quiet_app_gets_its_cpu_back() {
        let state = ThrottleState { cap: Some(20), last_rate: Some(200.0), stalls: 0 };
        let step = state.step(5.0, 50);
        assert!(step.cap > 20, "well under budget should relax the cap, got {}", step.cap);
        assert!(step.detail.contains("relaxing"), "{}", step.detail);
    }

    #[test]
    fn capping_something_it_cannot_quieten_gives_up_rather_than_squeezing() {
        // The case the module doc warns about: wakeups from a fixed-rate timer
        // do not care about the CPU cap. Squeezing such an app to the floor
        // costs it responsiveness and buys nothing.
        let mut state = ThrottleState::default();
        let mut caps = Vec::new();
        for _ in 0..8 {
            let step = state.step(400.0, 50); // rate never moves
            state = step.state;
            caps.push(step.cap);
        }
        assert!(state.is_stalled(), "should have given up: {caps:?}");
        let settled = *caps.last().unwrap();
        assert!(settled > FLOOR_PCT, "gave up at {settled}%, not squeezed to the floor");
        assert!(state.step(400.0, 50).detail.contains("not moving the rate"));
    }

    #[test]
    fn progress_resets_the_patience() {
        // A cap that is working must not accumulate stalls from earlier steps,
        // or a slow-converging app would be abandoned halfway.
        let state = ThrottleState { cap: Some(50), last_rate: Some(400.0), stalls: 2 };
        let step = state.step(100.0, 50);
        assert_eq!(step.state.stalls, 0, "the rate fell, so the cap is working");
    }

    #[test]
    fn caps_are_quantised_so_the_cap_does_not_churn() {
        assert_eq!(quantise(23), 25);
        assert_eq!(quantise(22), 20);
        assert_eq!(quantise(1), FLOOR_PCT);
        assert_eq!(quantise(100), 100);
    }
}
