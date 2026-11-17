//! Pure domain logic for `dozed`.
//!
//! Nothing in this crate performs I/O, makes a syscall, or reads a clock.
//! Everything OS-specific lives in `dozed-linux` behind traits, and the daemon
//! wires the two together. The payoff is the seam described in `DESIGN.md` §3:
//!
//! ```text
//!   sources ─► Event ─► WorldState ─► Planner ─► Vec<Intent> ─► Actuator
//! ```
//!
//! The planner never learns whether the actuator is the real one or the dry-run
//! recorder, so dry-run is not a simulation that can drift — it is the identical
//! intent stream with the last step removed.
//!
//! ```
//! use std::time::Duration;
//! use dozed_core::{Config, testing::Scenario};
//!
//! let mut s = Scenario::new(Config::default());
//! // An app with no preset of its own, so the catch-all rule owns it.
//! let app = s.spawn("some-editor");
//! s.hide(&app);
//!
//! assert!(s.advance(Duration::from_secs(60)).is_empty()); // still in grace
//! let intents = s.advance(Duration::from_secs(300));
//! assert_eq!(intents[0].reason.rule.as_deref(), Some("bg-idle"));
//! ```

pub mod actuator;
pub mod app;
pub mod attribution;
pub mod ceiling;
pub mod config;
pub mod coverage;
pub mod energy;
pub mod event;
pub mod explain;
pub mod grace;
pub mod intent;
pub mod ipc;
pub mod journal;
pub mod paths;
pub mod planner;
pub mod power_model;
pub mod presets;
pub mod starter;
pub mod stats;
pub mod testing;
pub mod throttle;
pub mod time;
pub mod trace;
pub mod trial;
pub mod tune;
pub mod visibility;
pub mod world;

pub use app::{AppClass, AppKey, IdentityEvidence, IdentityKind};
pub use ceiling::{Ceiling, Hold};
pub use config::{AppOverride, Config, ConfigError, Mode, Rule, RuleAction};
pub use event::{
    AppSeen, DesktopState, Event, Exemption, PowerState, ProcContrib, ProcSample, ThermalState,
    Visibility,
};
pub use explain::{Clause, DecisionInputs, Mismatch, RuleOutcome};
pub use grace::Grace;
pub use intent::{Action, Intent, Outcome, Reason, ReasonKind};
pub use ipc::{Request, Response, StatusReport};
pub use journal::{JournalEntry, Report, ReportAccumulator};
pub use planner::{Decision, Explanation, Planner};
pub use time::{Coarse, HumanDuration, MonoTime, Rounded};
pub use trace::{ReplayResult, TRACE_VERSION, TraceHeader, TraceRecord, replay};
pub use visibility::{Rect, WindowFacts};
pub use world::{AppState, WorldState};
