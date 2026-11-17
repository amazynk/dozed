//! Tests for [`super`], in a file of their own, so that the file a reader opens to
//! understand what the code does is not mostly assertions about it. Still a child
//! module rather than an integration test: these reach private items, and the
//! `#[path]` attribute on the declaration keeps `super::*` meaning what it would at
//! the bottom of that file.

use super::*;

/// A word or a number, and the number survives the round trip.
///
/// The hand-written `Serialize`/`Deserialize` pair is the only place in this
/// file where the format is not derived, so it is the only place the format
/// can be wrong without anything else noticing.
mod aggression {
    use super::*;

    fn parse(s: &str) -> Result<Aggression, toml::de::Error> {
        #[derive(Deserialize)]
        struct Holder {
            aggression: Aggression,
        }
        toml::from_str::<Holder>(s).map(|h| h.aggression)
    }

    #[test]
    fn the_four_words_still_parse_exactly_as_they_did() {
        // The upgrade case: every config.toml already on disk says a word.
        for (word, expected) in [
            ("gentle", Aggression::Gentle),
            ("balanced", Aggression::Balanced),
            ("aggressive", Aggression::Aggressive),
            ("maximum", Aggression::Maximum),
        ] {
            let got = parse(&format!("aggression = \"{word}\"")).expect(word);
            assert_eq!(got, expected, "{word}");
        }
    }

    #[test]
    fn a_bare_number_is_a_custom_factor() {
        assert_eq!(parse("aggression = 0.75").unwrap(), Aggression::Custom(0.75));
        assert!((parse("aggression = 0.75").unwrap().factor() - 0.75).abs() < f64::EPSILON);
        // An integer is what TOML calls `aggression = 1`, and refusing it
        // for not being `1.0` would be pedantry with an error message.
        assert_eq!(parse("aggression = 1").unwrap(), Aggression::Custom(1.0));
    }

    #[test]
    fn a_custom_factor_is_written_back_as_the_number_and_not_as_a_table() {
        let mut defaults = Defaults { aggression: Aggression::Custom(0.75), ..Defaults::default() };
        let text = toml::to_string(&defaults).unwrap();
        assert!(text.contains("aggression = 0.75"), "{text}");
        // And a word is still a word, so annotating a file does not rewrite
        // every preset into a number.
        defaults.aggression = Aggression::Gentle;
        let text = toml::to_string(&defaults).unwrap();
        assert!(text.contains("aggression = \"gentle\""), "{text}");
    }

    #[test]
    fn a_factor_that_would_break_the_grace_product_is_refused_at_parse_time() {
        // Zero is the dangerous one: it multiplies every grace in the file
        // to nothing, so it does not misbehave, it acts instantly on
        // everything. Negative is a duration running backwards.
        for bad in ["0", "-1.0", "0.001", "100.0", "nan"] {
            let err = parse(&format!("aggression = {bad}"))
                .expect_err("{bad} must not be accepted")
                .to_string();
            assert!(err.contains("aggression"), "{bad}: {err}");
        }
    }

    #[test]
    fn the_word_custom_says_what_to_write_instead() {
        // Somebody typing this has the right idea and the wrong syntax, and
        // "unknown aggression" would send them looking for a fifth word.
        let err = parse(r#"aggression = "custom""#).unwrap_err().to_string();
        assert!(err.contains("0.75"), "the message must show the form: {err}");
    }

    #[test]
    fn a_custom_factor_multiplies_the_grace_like_any_other_level() {
        // The point of the whole variant: it is the same term of the same
        // product, not a second mechanism.
        assert!((Aggression::Custom(0.5).factor() - 0.5).abs() < f64::EPSILON);
        assert_eq!(Aggression::Custom(0.5).as_str(), "custom");
    }
}

/// The §15 presets, checked against what they promise rather than by shape.
mod presets {
    use crate::testing::Scenario;
    use std::time::Duration;

    use super::*;

    fn fired(name: &str, after: Duration) -> Option<crate::intent::Action> {
        let mut s = Scenario::new(Config::default());
        let app = s.spawn(name);
        s.hide(&app);
        s.advance(after).first().map(|i| i.action)
    }

    #[test]
    fn chat_apps_are_throttled_rather_than_frozen() {
        // §7: a capped Slack stays connected and still notifies. Freezing it
        // would drop the connection and cost a reconnect on every thaw.
        assert_eq!(
            fired("slack", Duration::from_secs(200)),
            Some(crate::intent::Action::Throttle { pct: 5 })
        );
        assert_eq!(
            fired("signal-desktop", Duration::from_secs(200)),
            Some(crate::intent::Action::Throttle { pct: 5 })
        );
    }

    #[test]
    fn vms_containers_and_sync_clients_are_never_touched() {
        // Background work is the whole point of these, so freezing one
        // breaks the job it exists to do.
        for app in ["syncthing", "virt-manager", "nextcloud", "dropbox"] {
            assert_eq!(fired(app, Duration::from_secs(3600)), None, "{app} must be exempt");
        }
    }

    #[test]
    fn slow_to_wake_apps_get_a_longer_leash_but_are_not_exempt() {
        // Spotify paused and hidden is a perfectly good freeze candidate; it
        // is just slow to come back, so it waits 15m rather than 5m.
        assert_eq!(fired("spotify", Duration::from_secs(600)), None, "not yet");
        assert_eq!(
            fired("spotify", Duration::from_secs(1000)),
            Some(crate::intent::Action::Freeze { reclaim: false })
        );
    }

    #[test]
    fn on_ac_is_the_only_switch_needed_to_act_while_plugged_in() {
        // The bug this guards, found by replaying a real 9-hour night: the
        // shipped rule gated on `on_battery = true` *as well as* the global
        // `on_ac` policy, so setting `on_ac = "enabled"` changed nothing and
        // said nothing. One question, one knob.
        let mut cfg = Config::default();
        cfg.defaults.on_ac = crate::config::AcPolicy::Enabled;

        let mut s = Scenario::new(cfg);
        s.set_power(crate::testing::ON_AC);
        let app = s.spawn("some-editor");
        s.hide(&app);
        // Long enough to clear the AC grace, which is `ac_factor` times the
        // battery one — acting on AC is still acting, just far more
        // patiently about it.
        let intents = s.advance(Duration::from_secs(2000));
        assert_eq!(
            intents.first().map(|i| i.action),
            Some(crate::intent::Action::Freeze { reclaim: false }),
            "on_ac = enabled must be sufficient on its own"
        );
    }

    #[test]
    fn the_default_acts_on_ac_and_takes_its_time_about_it() {
        // The premise, in the smallest test that can hold it: a watt is a
        // watt, so being plugged in is not a reason to leave an idle app
        // waking the CPU. This used to assert the opposite.
        let mut s = Scenario::new(Config::default());
        s.set_power(crate::testing::ON_AC);
        let app = s.spawn("some-editor");
        s.hide(&app);

        // `bg-idle` is 5m, doubled by `ac_factor`, so nothing yet at 5m and
        // a freeze once 10m has passed. Both halves asserted: acting on AC
        // is the change, acting on AC *as urgently as on battery* is not.
        assert!(s.advance(Duration::from_secs(300)).is_empty(), "not at the battery pace");
        assert_eq!(
            s.advance(Duration::from_secs(600)).first().map(|i| i.action),
            Some(crate::intent::Action::Freeze { reclaim: false }),
            "but it does act"
        );
    }

    #[test]
    fn opting_out_of_ac_still_works() {
        let mut cfg = Config::default();
        cfg.defaults.on_ac = AcPolicy::Disabled;
        let mut s = Scenario::new(cfg);
        s.set_power(crate::testing::ON_AC);
        let app = s.spawn("some-editor");
        s.hide(&app);
        assert!(
            s.advance(Duration::from_secs(3600)).is_empty(),
            "`on_ac = disabled` must still mean disabled"
        );
    }

    #[test]
    fn an_app_with_no_preset_falls_through_to_the_catch_all() {
        assert_eq!(
            fired("some-editor", Duration::from_secs(400)),
            Some(crate::intent::Action::Freeze { reclaim: false })
        );
    }

    #[test]
    fn a_glance_at_a_chat_app_throttles_and_never_freezes() {
        // The interlock that makes shipping this on by default safe. A
        // single glance rule ranked high enough to beat `bg-idle` also beats
        // `chatty-apps`, so glancing at Slack would freeze it — dropping the
        // connection and costing a reconnect, the exact harm §7's throttle
        // exists to avoid, on the most-glanced-at apps on the machine.
        let mut cfg = Config::default();
        cfg.validate().unwrap();
        let by_name = |n: &str| cfg.rules.iter().find(|r| r.name == n).unwrap();

        let chat = by_name("redoze-chat-after-a-glance");
        let generic = by_name("redoze-after-a-glance");
        let chatty = by_name("chatty-apps");
        assert!(chat.priority < chatty.priority, "chat glance must outrank chatty-apps");
        assert!(generic.priority > chatty.priority, "a chat app must never reach the freeze");
        assert_eq!(chat.action, RuleAction::Throttle(5));
        assert_eq!(generic.action, RuleAction::Freeze);

        // And it must beat every rule that would otherwise hold the app for
        // minutes, or shipping it on by default changes nothing.
        for slower in ["slow-to-wake", "bg-idle"] {
            assert!(
                generic.priority < by_name(slower).priority,
                "the glance rule must outrank {slower}"
            );
        }
        // Losing to the exemptions is equally load-bearing.
        for never in ["never-system", "audio-exempt", "long-job-exempt"] {
            assert!(
                generic.priority > by_name(never).priority,
                "{never} must still win over a glance"
            );
        }
    }

    #[test]
    fn the_two_glance_rules_match_the_same_chat_apps() {
        // One list, shared: two copies would drift, and a spelling in one
        // and not the other means an app is handled on one path and not the
        // other with nothing to say so.
        let mut cfg = Config::default();
        cfg.validate().unwrap();
        let chatty = cfg.rules.iter().find(|r| r.name == "chatty-apps").unwrap();
        let glance = cfg.rules.iter().find(|r| r.name == "redoze-chat-after-a-glance").unwrap();
        assert_eq!(chatty.match_.app, glance.match_.app);
    }

    #[test]
    fn signal_is_throttled_whatever_the_packaging_calls_it() {
        // Regression: the preset listed only `signal-desktop`, and patterns
        // match exactly, so on a machine that reports plain `signal` the
        // rule was dead — a whole recorded session went by with Signal
        // hidden and unthrottled and nothing to show it had been skipped.
        for name in ["signal", "signal-desktop", "org.signal.Signal"] {
            assert_eq!(
                fired(name, Duration::from_secs(200)),
                Some(crate::intent::Action::Throttle { pct: 5 }),
                "{name} must be matched by the chatty-apps preset"
            );
        }
    }

    #[test]
    fn security_prompt_uis_are_never_touched() {
        // A frozen connection prompt cannot answer, so the daemon behind it
        // applies its own default to everything new for as long as the
        // freeze lasts: either a broken network or a silently permitted
        // connection. Both are worse than the power it saves.
        for app in ["opensnitch_ui", "opensnitch-ui"] {
            assert_eq!(
                fired(app, Duration::from_secs(3600)),
                None,
                "{app} must be exempt: it is idle by construction, which is \
                 exactly what bg-idle selects for"
            );
        }
    }

    #[test]
    fn remote_desktop_is_capped_at_worst_and_never_frozen() {
        // Freezing one of these does not cost a redraw, it costs the session:
        // it stops draining its socket and the peer drops it. Found with a real
        // VNC session, hidden for 1h58m, in the would-doze list the moment
        // acting on AC was switched on.
        //
        // A cap does not have that failure — the app still runs, it just cannot
        // spin — so the ceiling is a throttle rather than the blanket `never`
        // this used to be. The blanket was wrong for the always-on half in the
        // other direction too: a RustDesk in the tray with nobody connected had
        // bought a permanent exemption by being *capable* of holding a session.
        for app in ["rustdesk", "anydesk", "teamviewer", "x11vnc", "gnome-remote-de"] {
            assert_eq!(
                fired(app, Duration::from_secs(3600)),
                Some(crate::intent::Action::Throttle { pct: 50 }),
                "{app} accepts connections while nobody is watching it and must \
                 stay reachable: capped, never stopped"
            );
        }
        for app in ["gvncviewer", "virt-viewer", "remmina", "org.remmina.Remmina", "rdesktop"] {
            assert_eq!(
                fired(app, Duration::from_secs(3600)),
                Some(crate::intent::Action::Throttle { pct: 5 }),
                "{app} may hold a live session the user has switched away from"
            );
        }
    }

    #[test]
    fn no_remote_desktop_app_can_reach_a_rule_that_stops_it() {
        // The invariant behind the two rules above, checked against the table
        // rather than against one scenario: whatever else matches, the first
        // rule that does must not be one that freezes or cycles. A later
        // addition to `bg-idle`'s side of the table cannot quietly take one of
        // these back without this failing.
        let cfg = Config::default();
        for pattern in ["rustdesk", "anydesk", "teamviewer", "x11vnc", "vncviewer", "moonlight"] {
            let key = AppKey::comm(pattern);
            let first = cfg
                .rules
                .iter()
                .find(|r| r.match_.app.as_ref().is_some_and(|p| p.matches(&key)))
                .unwrap_or_else(|| panic!("{pattern} must be named by a rule"));
            assert!(
                matches!(first.action, RuleAction::Throttle(_) | RuleAction::Never),
                "{pattern} is first claimed by `{}`, which is a {:?}",
                first.name,
                first.action
            );
        }
    }

    #[test]
    fn the_shipped_config_is_valid_and_ordered() {
        // Every rule the installer writes must parse back, or the starter
        // config is a trap.
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).expect("serialises");
        let back = Config::parse(&text).expect("and parses back");
        assert_eq!(back.rules.len(), cfg.rules.len());
        assert!(
            cfg.rules.windows(2).all(|w| w[0].priority <= w[1].priority),
            "validate() must leave the table in priority order"
        );
    }
}

/// Configs that parse but cannot do what they look like they do.
mod lints {
    use super::*;

    fn battery_gated_freeze() -> Rule {
        Rule::new("bg-idle-battery", 100, RuleAction::Freeze).when(Predicates {
            visibility: Some(Visibility::Hidden),
            on_battery: Some(true),
            ..Default::default()
        })
    }

    #[test]
    fn the_shipped_default_trips_no_lint_of_its_own() {
        // A lint the starter config fails is a lint everyone learns to
        // ignore.
        assert!(Config::default().warnings().is_empty());
    }

    #[test]
    fn a_rule_gated_on_battery_under_on_ac_disabled_is_reported() {
        // The exact config found live: two gates that have to agree, where
        // the visible knob (`on_ac`) is not the one refusing. Flipping
        // `on_ac` to "enabled" produces no intents and no explanation, which
        // is indistinguishable from a quiet machine.
        let mut cfg = Config::default();
        cfg.defaults.on_ac = AcPolicy::Disabled;
        cfg.rules = vec![battery_gated_freeze()];

        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("bg-idle-battery"), "{}", warnings[0]);
        assert!(warnings[0].contains("on_ac"), "it must name the other gate: {}", warnings[0]);
    }

    #[test]
    fn the_same_rule_is_fine_once_on_ac_is_enabled() {
        // Then the two gates agree and the rule means what it says: act,
        // but only on battery.
        let mut cfg = Config::default();
        cfg.defaults.on_ac = AcPolicy::Enabled;
        cfg.rules = vec![battery_gated_freeze()];
        assert!(cfg.warnings().is_empty());
    }

    #[test]
    fn a_never_rule_may_be_battery_gated_without_complaint() {
        // An exemption that only applies on battery exempts less than it
        // could, and exempting less on AC costs nothing when nothing acts
        // on AC in the first place.
        let mut cfg = Config::default();
        cfg.defaults.on_ac = AcPolicy::Disabled;
        cfg.rules = vec![
            Rule::new("keep-out", 5, RuleAction::Never)
                .when(Predicates { on_battery: Some(true), ..Default::default() }),
        ];
        assert!(cfg.warnings().is_empty());
    }

    /// Half a signal moved out of range is worse than none of it, and it is
    /// The fan's two thresholds are one signal, and setting one of them
    /// leaves it half chosen and half learned.
    #[test]
    fn half_a_fan_threshold_pair_is_reported() {
        let mut cfg = Config::default();
        cfg.thermal.hot_rpm = Some(6000);
        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("one signal"), "{}", warnings[0]);
        assert!(warnings[0].contains("cool_rpm"), "{}", warnings[0]);

        // The other half alone is the same mistake wearing the other shoe.
        cfg.thermal.hot_rpm = None;
        cfg.thermal.cool_rpm = Some(5500);
        assert_eq!(cfg.warnings().len(), 1);

        // Both, or neither, are quiet.
        cfg.thermal.hot_rpm = Some(6000);
        assert!(cfg.warnings().is_empty());
        assert!(cfg.validate().is_ok(), "and it is a legal pair, not merely a quiet one");
        assert!(ThermalConfig::default().hot_rpm.is_none(), "neither is the shipped state");
    }

    /// No RPM number ships, because an RPM number is a claim about one fan.
    #[test]
    fn no_absolute_fan_speed_is_shipped_as_a_default() {
        let cfg = ThermalConfig::default();
        assert_eq!(cfg.hot_rpm, None);
        assert_eq!(cfg.cool_rpm, None);
        // Degrees are absolute and do ship: 80C means the same thing on any
        // silicon, which is exactly what an RPM does not.
        assert_eq!(cfg.hot_celsius, 80.0);
        assert_eq!(cfg.cool_celsius, 72.0);
        // And the written starter file must not sprout one either.
        let written = toml::to_string(&Config::default()).unwrap();
        assert!(!written.contains("hot_rpm"), "an unset pair is not written out: {written}");
    }

    #[test]
    fn inverted_thermal_hysteresis_is_refused_rather_than_shipped() {
        // A state machine that fires at the lower number and demands the
        // higher one to clear can never leave `hot`.
        let mut cfg = Config::default();
        cfg.thermal.hot_rpm = Some(3000);
        cfg.thermal.cool_rpm = Some(3000);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("cool_rpm"), "{err}");
        assert!(err.contains("hysteresis"), "{err}");

        let mut cfg = Config::default();
        cfg.thermal.cool_celsius = 90.0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("cool_celsius"), "{err}");
    }

    #[test]
    fn the_thermal_section_round_trips_through_the_file() {
        // Someone who knows their hardware can still say so.
        let toml = "[thermal]\nhot_rpm = 6000\ncool_rpm = 5500\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.thermal.hot_rpm, Some(6000));
        assert_eq!(cfg.thermal.cool_rpm, Some(5500));
        // Unmentioned fields keep the shipped values rather than zeroing.
        assert_eq!(cfg.thermal.hot_celsius, 80.0);
        assert_eq!(cfg.thermal.cool_celsius, 72.0);
        // And a file written before this section existed still parses.
        let old: Config = toml::from_str("[defaults]\nbase_grace = \"5m\"\n").unwrap();
        assert_eq!(old.thermal, ThermalConfig::default());
    }

    #[test]
    fn an_aggression_past_maximum_is_reported_with_what_it_does_to_ac() {
        // The config this was written against, read off a live machine: the
        // floor of the accepted range, three times past the most impatient
        // named level, silently inverting the AC policy sitting six lines
        // below it in the same file.
        let mut cfg = Config::default();
        cfg.defaults.aggression = Aggression::custom(0.05).unwrap();
        assert_eq!(cfg.battery.ac_factor, 2.0, "the shipped default");

        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("maximum"), "{}", warnings[0]);
        // 2.0 x 0.05 = 0.1, so a tenth of the base grace on mains.
        assert!(warnings[0].contains("x0.10"), "{}", warnings[0]);
        assert!(warnings[0].contains("10 times sooner"), "{}", warnings[0]);

        // A named level says nothing, however impatient.
        cfg.defaults.aggression = Aggression::Maximum;
        assert!(cfg.warnings().is_empty());
        // Nor does a custom value inside the range the words already cover:
        // "a bit more patient than aggressive" is exactly what it is for.
        cfg.defaults.aggression = Aggression::custom(0.75).unwrap();
        assert!(cfg.warnings().is_empty());
    }

    #[test]
    fn factor_mode_with_a_zero_away_factor_is_reported_as_inert() {
        // Not "shorter" — inert. The zero cancels the factor outright, and a
        // user who opted into `factor` mode deserves to be told that rather
        // than discovering it by replaying a locked session.
        let mut cfg = Config::default();
        cfg.defaults.resume_cooldown_mode = CooldownMode::Factor;
        assert_eq!(cfg.activity.away_factor, 0.0, "the shipped default");

        let warnings = cfg.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("resume_cooldown_mode"), "{}", warnings[0]);
        assert!(warnings[0].contains("locked"), "{}", warnings[0]);

        // The same mode is fine once `away` is not a zero.
        cfg.activity.away_factor = 0.5;
        assert!(cfg.warnings().is_empty());
    }

    #[test]
    fn the_gate_is_found_in_the_match_clause_too() {
        // `match` and `when` are the same predicate set; a lint that only
        // reads one of them is a lint with a blind spot.
        let mut cfg = Config::default();
        cfg.defaults.on_ac = AcPolicy::Disabled;
        cfg.rules = vec![
            Rule::new("m", 100, RuleAction::Freeze)
                .matching(Predicates { on_battery: Some(true), ..Default::default() }),
        ];
        assert_eq!(cfg.warnings().len(), 1);
    }
}

/// The example from `DESIGN.md` §9, which is also what the docs promise
/// users they can write.
const DESIGN_EXAMPLE: &str = r#"
mode = "dry-run"

[defaults]
base_grace          = "5m"
cpu_sample_window   = "60s"
reclaim_on_freeze   = false
freeze_visible      = false
on_ac               = "disabled"
resume_cooldown      = "2m"
resume_cooldown_mode = "floor"   # floor | factor -- see the grace model, §8

[activity]
active_threshold = "30s"
active_factor    = 4.0
away_factor      = 0.0

[battery]
low_pct    = 30
low_factor = 0.5

[[rule]]
name     = "never-system"
priority = 0
match    = { class = "system" }
action   = "never"

[[rule]]
name     = "chatty-apps"
priority = 50
match    = { app = ["com.slack.Slack", "com.discordapp.Discord"] }
when     = { visibility = "hidden", for = "2m" }
action   = { throttle = 5 }

[[rule]]
name     = "idle-wakeup-hog"
priority = 60
when     = { visibility = "hidden", wakeups_per_sec = { gt = 50 }, for = "60s" }
action   = "freeze"

[[rule]]
name     = "bg-idle"
priority = 100
when     = { visibility = "hidden", for = "5m" }
action   = "freeze"

[apps."org.mozilla.firefox"]
mode = "monitor"
"#;

#[test]
fn parses_the_design_example() {
    let cfg = Config::parse(DESIGN_EXAMPLE).unwrap();
    assert_eq!(cfg.mode, Mode::DryRun);
    assert_eq!(cfg.defaults.base_grace.get(), Duration::from_secs(300));
    assert_eq!(cfg.activity.active_factor, 4.0);
    assert_eq!(cfg.rules.len(), 4);

    let chatty = cfg.rules.iter().find(|r| r.name == "chatty-apps").unwrap();
    assert_eq!(chatty.action, RuleAction::Throttle(5));
    assert_eq!(chatty.grace(), Some(Duration::from_secs(120)));
    assert!(chatty.match_.app.as_ref().unwrap().matches(&AppKey::flatpak("com.slack.Slack")));

    let hog = cfg.rules.iter().find(|r| r.name == "idle-wakeup-hog").unwrap();
    assert!(hog.when.wakeups_per_sec.unwrap().allows(51.0));
    assert!(!hog.when.wakeups_per_sec.unwrap().allows(50.0));
}

#[test]
fn per_app_override_resolves() {
    let cfg = Config::parse(DESIGN_EXAMPLE).unwrap();
    let ff = AppKey::desktop("org.mozilla.firefox");
    assert_eq!(cfg.mode_for(&ff), Mode::Monitor);
    assert_eq!(cfg.mode_for(&AppKey::flatpak("com.slack.Slack")), Mode::DryRun);
}

#[test]
fn rules_are_sorted_by_priority() {
    let cfg = Config::parse(
        r#"
[[rule]]
name = "late"
priority = 90
action = "freeze"

[[rule]]
name = "early"
priority = 10
action = "never"
"#,
    )
    .unwrap();
    assert_eq!(cfg.rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["early", "late"]);
}

#[test]
fn threshold_accepts_bare_number() {
    let cfg = Config::parse(
        r#"
[[rule]]
name = "hog"
when = { wakeups_per_sec = 50 }
action = "freeze"
"#,
    )
    .unwrap();
    assert_eq!(cfg.rules[0].when.wakeups_per_sec, Some(Threshold::Above(50.0)));
}

#[test]
fn typo_in_a_predicate_is_an_error() {
    // The important one: a silently-ignored predicate widens the rule to
    // everything, which is how you freeze a desktop.
    let err = Config::parse(
        r#"
[[rule]]
name = "oops"
when = { visibilty = "hidden" }
action = "freeze"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)), "{err}");
}

#[test]
fn a_doze_cycle_reads_from_toml_the_way_it_is_documented() {
    // The whole surface of the new rung is this one line of TOML, so it is
    // worth pinning that it parses as written rather than as guessed.
    let cfg = Config::parse(
        r#"
[[rule]]
name = "chatty-apps"
priority = 50
action = { doze-cycle = { every = "15m", wake = "20s" } }

[rule.when]
visibility = "hidden"
for = "2m"
"#,
    )
    .unwrap();
    assert_eq!(
        cfg.rules[0].action,
        RuleAction::DozeCycle { every: HumanDuration::secs(900), wake: HumanDuration::secs(20) }
    );
    assert_eq!(cfg.cycling_rules().len(), 1);
}

#[test]
fn the_quiet_and_latency_predicates_read_as_documented() {
    // The whole user-facing surface of both objectives is these two keys, and
    // naming either is what opts a rule out of the AC policy — so the parse
    // and the opt-out are pinned together.
    let cfg = Config::parse(
        r#"
[[rule]]
name     = "quiet-down"
priority = 40
action   = "freeze"

[rule.when]
visibility = "hidden"
thermal    = "hot"
for        = "1m"

[[rule]]
name     = "clear-the-decks-for-the-game"
priority = 45
action   = { throttle = 5 }

[rule.when]
other_app_fullscreen = true
for                  = "30s"

[[rule]]
name   = "bg-idle"
action = "freeze"
"#,
    )
    .unwrap();
    assert_eq!(cfg.rules[0].when.thermal, Some(crate::event::ThermalState::Hot));
    assert_eq!(cfg.rules[1].when.other_app_fullscreen, Some(true));

    assert!(cfg.rules[0].acts_on_ac(), "a rule about noise is not about endurance");
    assert!(cfg.rules[1].acts_on_ac(), "nor is one about dropped frames");
    assert!(
        !cfg.rules[2].acts_on_ac(),
        "but an ordinary rule keeps the AC policy: this is not a general escape hatch"
    );
}

#[test]
fn a_window_at_least_as_long_as_its_period_is_refused() {
    // Such a rule looks like it dozes an app and never does: the window opens
    // before the app has been down for a period, forever. There is nothing
    // sensible to clamp it to, so it is an error rather than a warning.
    for (every, wake) in [("10m", "10m"), ("1m", "5m")] {
        let err = Config::parse(&format!(
            r#"
[[rule]]
name = "never-really-dozes"
action = {{ doze-cycle = {{ every = "{every}", wake = "{wake}" }} }}
"#
        ))
        .unwrap_err();
        assert!(matches!(err, ConfigError::CycleNeverDozes { .. }), "{every}/{wake}: {err}");
    }

    // And the sane shape is accepted.
    assert!(
        Config::parse(
            r#"
[[rule]]
name = "fine"
action = { doze-cycle = { every = "15m", wake = "20s" } }
"#
        )
        .is_ok()
    );
}

#[test]
fn duplicate_rule_names_are_rejected() {
    let err = Config::parse(
        r#"
[[rule]]
name = "dup"
action = "freeze"

[[rule]]
name = "dup"
action = "never"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::DuplicateRule(_)), "{err}");
}

#[test]
fn bad_throttle_is_rejected() {
    let err = Config::parse(
        r#"
[[rule]]
name = "silly"
action = { throttle = 0 }
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::BadThrottle { .. }), "{err}");
}

#[test]
fn impossible_bound_is_rejected() {
    let err = Config::parse(
        r#"
[[rule]]
name = "nope"
when = { cpu_pct = { gt = 90, lt = 10 } }
action = "freeze"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::ImpossibleBound(_)), "{err}");
}

#[test]
fn global_monitor_is_rejected() {
    let err = Config::parse(r#"mode = "monitor""#).unwrap_err();
    assert!(matches!(err, ConfigError::GlobalMonitor), "{err}");
}

#[test]
fn missing_sections_fall_back_to_defaults() {
    let cfg = Config::parse("").unwrap();
    assert_eq!(cfg.defaults, Defaults::default());
    assert_eq!(
        cfg.defaults.on_ac,
        AcPolicy::Enabled,
        "wasted power is wasted whoever is paying for it"
    );
    assert_eq!(
        cfg.defaults.resume_cooldown_mode,
        CooldownMode::Floor,
        "the cooldown must default to something no other factor can cancel"
    );
    assert!(cfg.rules.is_empty(), "an empty file means no rules, not the built-ins");
}

#[test]
fn the_cooldown_mode_is_written_the_way_a_person_would_say_it() {
    for (text, want) in [("floor", CooldownMode::Floor), ("factor", CooldownMode::Factor)] {
        let cfg = Config::parse(&format!("[defaults]\nresume_cooldown_mode = \"{text}\"")).unwrap();
        assert_eq!(cfg.defaults.resume_cooldown_mode, want);
        // And survives the round trip the installer writes it through.
        let back = Config::parse(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.defaults.resume_cooldown_mode, want);
    }
    assert!(
        Config::parse("[defaults]\nresume_cooldown_mode = \"floors\"").is_err(),
        "a typo must not silently pick a mode"
    );
}

#[test]
fn shipped_config_roundtrips() {
    let cfg = Config::default();
    let text = toml::to_string(&cfg).unwrap();
    assert_eq!(Config::parse(&text).unwrap(), cfg, "\n{text}");
}
