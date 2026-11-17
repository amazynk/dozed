//! Linux backends for `dozed`.
//!
//! `dozed-core` decides; this crate is where decisions meet the kernel. The
//! split matters because everything here needs a real machine to test against,
//! while the planner needs none.
//!
//! Contents so far, from `DESIGN.md` §16 step 2:
//!
//! * [`cgroup`] — locating an app's scope and acting on it via `cgroup.freeze`,
//!   `cpu.max` and `memory.reclaim`.
//! * [`guard`] — the hard never-freeze list, consulted immediately before any
//!   write.
//! * [`actuator`] — [`Action`](dozed_core::Action) in, [`Outcome`](dozed_core::Outcome) out,
//!   in both dry-run and live flavours.
//! * [`energy`] — the battery and `cpuidle` counters behind the *measured*
//!   report, as opposed to the modelled one (`DESIGN.md` §11).
//! * [`thermal`] — hwmon temperatures and fan speeds, for the quiet objective
//!   (`DESIGN.md` §7.3).
//!
//! Three things measured on a live Plasma 6.6.6 session shaped all of it:
//!
//! 1. Freezing a cgroup took a 165 wakeups/s workload to **0**, and thawing put
//!    it back to 163 — which is the entire premise of the tool, confirmed.
//! 2. A frozen task still reports `State: S (sleeping)` in `/proc`. Frozen-ness
//!    is only visible in `cgroup.events`.
//! 3. A freeze outlives the process that applied it, so crash safety cannot rely
//!    on the daemon being alive to clean up.

pub mod actuator;
pub mod cgroup;
pub mod energy;
pub mod gpu;
pub mod guard;
pub mod systemd;
pub mod thermal;
pub mod tree;

// Re-exported so `dozed-linux` users need only one import for the seam, while
// the trait itself lives in core where the seam is defined (`DESIGN.md` §3).
pub use actuator::{CgroupActuator, Target, Unit};
pub use cgroup::{
    Cgroup, CgroupError, ScopeIdentity, app_unit_of, cgroup_path_of_pid, is_the_unit_for_app,
    parse_app_scope,
};
pub use dozed_core::actuator::{Actuator, DryRunActuator};
pub use guard::{Guard, NEVER_FREEZE, is_never_freeze};
pub use systemd::{SystemdError, SystemdUnits};
