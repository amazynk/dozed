//! Reaching the configs that already exist.
//!
//! The shipped rules are a **starter kit**: `packaging/install.sh` writes them
//! once, on the first install, and never touches the file again (`DESIGN.md`
//! §15). That is the right default — a tool that rewrites your config on upgrade
//! is a tool that undoes your edits — and it has a consequence §9 states and
//! then leaves standing:
//!
//! > Note what curation does **not** reach: the shipped rules are a starter kit,
//! > written once on first install and never touched again (§15), so new names
//! > help new installs only. Growing these lists is not a fix that reaches the
//! > people already running it.
//!
//! For the curated app lists that is a shame. For a rule added because freezing
//! something turned out to break it, it is a defect: `shared-window-exempt`
//! exists because a shared window was being frozen in somebody's meeting, and
//! shipping it to new installs only means the people it was written for — the
//! ones already running dozed — are exactly the people who never get it.
//!
//! ## What this is not
//!
//! It is not a migration, and it does not run by itself. The installer still
//! never touches an existing config; this is a command somebody types, and it
//! prints what it would do before it does anything. Three properties, each of
//! which is the difference between a useful upgrade path and one people learn
//! to distrust:
//!
//! * **It only ever adds.** A rule the user edited is a rule they have an
//!   opinion about, and an "upgrade" that reverts it is the failure mode §15
//!   avoids by not touching the file at all. Matching is by name, and a name
//!   that is present in any form is left exactly as it is.
//! * **It never reorders or renumbers.** Priorities are the user's, and a rule
//!   whose priority collides with one already in the file is reported rather
//!   than silently slotted in — `Config::validate` has opinions about the rule
//!   table and this must not be the thing that violates them.
//! * **It says what each addition is for.** The same sentence the starter file
//!   would have carried, because a rule arriving in a config with no explanation
//!   is indistinguishable from one somebody else put there.

use crate::config::{Config, Rule, builtin_rules};

/// A rule this version ships that a config does not have.
#[derive(Debug, Clone, PartialEq)]
pub struct Missing {
    /// The rule itself, ready to append.
    pub rule: Rule,
    /// Another rule already at this priority, if there is one.
    ///
    /// Not a refusal — first-match-wins scans in priority order and a tie is
    /// resolved by position, so this is legal — but it is worth saying out
    /// loud, because which of the two wins is then decided by something the
    /// user cannot see in the file.
    pub priority_clash: Option<String>,
}

/// Rules the running version ships which `cfg` has no rule named for.
///
/// Ordered by priority so the output reads the way the file does.
#[must_use]
pub fn missing_from(cfg: &Config) -> Vec<Missing> {
    let mut missing: Vec<Missing> = builtin_rules()
        .into_iter()
        .filter(|shipped| !cfg.rules.iter().any(|have| have.name == shipped.name))
        .map(|rule| {
            let priority_clash = cfg
                .rules
                .iter()
                .find(|have| have.priority == rule.priority)
                .map(|have| have.name.clone());
            Missing { rule, priority_clash }
        })
        .collect();
    missing.sort_by_key(|m| m.rule.priority);
    missing
}

/// `cfg` with every rule in `missing` appended, validated.
///
/// Validation is not a formality here: it is the same
/// [`Config::validate`](crate::config::Config::validate) the daemon runs, so a
/// combination this produces that the daemon would reject fails here — where
/// somebody is standing in front of it — rather than at the next restart.
///
/// # Errors
///
/// If the resulting table is not one the daemon would accept.
pub fn apply(cfg: &Config, missing: &[Missing]) -> Result<Config, crate::config::ConfigError> {
    let mut out = cfg.clone();
    out.rules.extend(missing.iter().map(|m| m.rule.clone()));
    out.validate()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shipped_config_is_already_up_to_date() {
        // The obvious property, and the one that keeps this from nagging: a
        // config written by this version has nothing to add to it.
        assert_eq!(missing_from(&Config::default()), Vec::new());
    }

    #[test]
    fn a_rule_added_since_the_config_was_written_is_offered() {
        // The case this exists for. An install predating `shared-window-exempt`
        // is missing the rule that stops a shared window being frozen in
        // somebody else's meeting, and nothing else would ever tell them.
        let mut old = Config::default();
        old.rules.retain(|r| r.name != "shared-window-exempt");

        let missing = missing_from(&old);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].rule.name, "shared-window-exempt");

        let upgraded = apply(&old, &missing).unwrap();
        assert!(upgraded.rules.iter().any(|r| r.name == "shared-window-exempt"));
    }

    #[test]
    fn an_edited_rule_is_left_alone() {
        // A rule the user has an opinion about. Reverting it is the failure §15
        // avoids by never touching the file, and doing it under the word
        // "upgrade" would be worse, not better.
        let mut edited = Config::default();
        let rule = edited.rules.iter_mut().find(|r| r.name == "bg-idle").unwrap();
        rule.priority = 999;
        let before = rule.clone();

        assert!(!missing_from(&edited).iter().any(|m| m.rule.name == "bg-idle"));
        let after = apply(&edited, &missing_from(&edited)).unwrap();
        assert_eq!(*after.rules.iter().find(|r| r.name == "bg-idle").unwrap(), before);
    }

    #[test]
    fn a_deleted_rule_comes_back_and_that_is_why_nothing_applies_by_itself() {
        // Deliberately not distinguishable from "never had it": nothing on disk
        // records that a rule was deleted rather than predating the version that
        // added it. That is exactly why this prints before it writes and why
        // `--write` is a separate flag — the tool cannot tell, and the user can.
        let mut deleted = Config::default();
        deleted.rules.retain(|r| r.name != "audio-exempt");
        assert!(missing_from(&deleted).iter().any(|m| m.rule.name == "audio-exempt"));
    }

    #[test]
    fn a_priority_already_in_use_is_reported_rather_than_hidden() {
        // Legal — first-match-wins resolves a tie by position — but which of the
        // two wins is then decided by something that is not visible in the file,
        // so it is said out loud.
        let mut cfg = Config::default();
        cfg.rules.retain(|r| r.name != "shared-window-exempt");
        let clash = cfg.rules.iter_mut().find(|r| r.name == "bg-idle").unwrap();
        clash.priority = 16;
        clash.name = "mine".into();

        let missing = missing_from(&cfg);
        let shared = missing.iter().find(|m| m.rule.name == "shared-window-exempt").unwrap();
        assert_eq!(shared.priority_clash.as_deref(), Some("mine"));
    }

    #[test]
    fn the_result_is_a_config_the_daemon_would_accept() {
        // The same `validate` the daemon runs, so a table this produces cannot
        // be one that fails at the next restart.
        let mut old = Config::default();
        old.rules.clear();
        let upgraded = apply(&old, &missing_from(&old)).unwrap();
        assert_eq!(upgraded.rules.len(), builtin_rules().len());
        // Sorted into priority order by `validate`, which is what makes
        // first-match-wins a simple scan.
        assert!(upgraded.rules.windows(2).all(|w| w[0].priority <= w[1].priority));
    }
}
