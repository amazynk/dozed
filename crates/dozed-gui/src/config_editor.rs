//! Editing the config from the dashboard, without being able to break it.
//!
//! Three obligations, in the order they matter:
//!
//! 1. **You cannot save something invalid.** Every save goes through
//!    [`Config::parse`] — the same function the daemon uses at startup — and a
//!    file it rejects is never written. In the form the button is disabled until
//!    the parser returns `Ok`; in the text tab it is live and refuses, for the
//!    reason below.
//! 2. **Most mistakes cannot be typed in the first place.** A setting with three
//!    legal values is a dropdown with three entries, not a text box that
//!    validates afterwards. Booleans are checkboxes, factors are drag values
//!    with floors. That leaves only durations as free text, and those show their
//!    parse result as you type.
//! 3. **Every field says what it does, in the words the config file uses.** The
//!    prose comes from `dozed_core::starter`, which is where the comments in
//!    `config.toml` come from too. One source: a tooltip nobody diffs is a
//!    tooltip that goes stale invisibly.
//!
//! Two tabs, because a form cannot express a rule table and a text box cannot
//! prevent a typo. Settings get the form; the whole file — rules included — is
//! reachable in the text tab, under the same parse-before-save rule.
//!
//! The two tabs report that rule differently, and deliberately. In the form,
//! invalid means one field is half-typed and the field itself says which, so the
//! error belongs on screen immediately. In the text tab invalid is the *ordinary*
//! state of a file being edited — deleting a line to retype it is not a mistake —
//! and a red strip that appears on the first keystroke and stays until the last
//! is noise a reader learns to scroll past. So that tab says nothing while the
//! text does not parse, beyond withholding the "valid" tick, and produces the
//! parser's message when Save is pressed, which is the moment somebody has
//! actually asked whether it is good yet.
//!
//! Rules are shown read-only in the form tab rather than hidden. "This is what
//! is currently deciding things, edit it in the other tab" is a useful thing for
//! a window to say; pretending the rules do not exist is not.

use std::path::{Path, PathBuf};

use eframe::egui;

use dozed_core::config::{AcPolicy, Aggression, CooldownMode};
use dozed_core::event::{DesktopState, PowerState};
use dozed_core::grace::Grace;
use dozed_core::{Coarse, Config, HumanDuration, Mode};

use crate::Explain;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Settings,
    File,
}

pub struct ConfigEditor {
    pub open: bool,
    path: PathBuf,
    tab: Tab,
    /// The form's model. Edited directly by the widgets.
    draft: Config,
    /// The text tab's model, and what actually gets written when that tab is
    /// showing — a file the user has typed into by hand is not necessarily
    /// round-trippable through `Config`, and silently reformatting somebody's
    /// file on save would be its own kind of damage.
    text: String,
    /// Durations are free text, so they are held as strings while being typed:
    /// "5" is not a duration yet and must not be rejected in the middle of
    /// becoming "5m".
    durations: Durations,
    /// What the parser says right now. `None` means saveable.
    error: Option<String>,
    /// Whether that error is on show yet.
    ///
    /// Always true for the form, where an error means one named field is
    /// half-typed. In the text tab it starts false and is turned on by pressing
    /// Save: see the module note — a file mid-edit is invalid most of the time,
    /// and reporting that continuously is how a real error gets ignored.
    showing_error: bool,
    /// The last thing that happened, for the strip along the bottom.
    status: Option<Note>,
}

/// Something that has happened, and whether it went wrong. A failed write is
/// not a parse error — the file is fine, the disk was not — so it cannot live in
/// `error`, which is recomputed from the parser every frame and would erase it.
#[derive(Debug, PartialEq, Eq)]
struct Note {
    text: String,
    bad: bool,
}

impl Note {
    fn info(text: impl Into<String>) -> Self {
        Self { text: text.into(), bad: false }
    }

    fn bad(text: impl Into<String>) -> Self {
        Self { text: text.into(), bad: true }
    }
}

/// The one line above the buttons.
#[derive(Debug, PartialEq)]
enum Strip<'a> {
    /// The parser's message, because somebody asked for it.
    Error(&'a str),
    /// Invalid, and nobody has asked yet. Says nothing — and in particular does
    /// not claim the file is valid.
    Quiet,
    /// The last thing that happened.
    Note(&'a Note),
    Valid,
}

/// The duration fields, as text.
#[derive(Default)]
struct Durations {
    base_grace: String,
    cpu_sample_window: String,
    resume_cooldown: String,
    active_threshold: String,
    silence_after: String,
    probe_every: String,
    probe_window: String,
}

impl Durations {
    fn from(cfg: &Config) -> Self {
        Self {
            base_grace: cfg.defaults.base_grace.to_string(),
            cpu_sample_window: cfg.defaults.cpu_sample_window.to_string(),
            resume_cooldown: cfg.defaults.resume_cooldown.to_string(),
            active_threshold: cfg.activity.active_threshold.to_string(),
            silence_after: cfg.audio.silence_after.to_string(),
            probe_every: cfg.audio.probe_every.to_string(),
            probe_window: cfg.audio.probe_window.to_string(),
        }
    }
}

impl Default for ConfigEditor {
    fn default() -> Self {
        Self {
            open: false,
            path: dozed_core::paths::config_file(),
            tab: Tab::Settings,
            draft: Config::default(),
            text: String::new(),
            durations: Durations::default(),
            error: None,
            showing_error: false,
            status: None,
        }
    }
}

impl ConfigEditor {
    /// Open on the file the daemon actually loaded, which is not always the
    /// default path — `dozed --config` is a real thing and editing the wrong
    /// file while the daemon ignores it would be a maddening bug to hit.
    pub fn open(&mut self, daemon_path: Option<&str>) {
        self.path = daemon_path.map_or_else(dozed_core::paths::config_file, PathBuf::from);
        self.reload_from_disk();
        self.open = true;
    }

    fn reload_from_disk(&mut self) {
        // A missing file is not an error: it means the daemon is running on
        // built-in defaults, and offering to write those out is exactly what
        // this window is for.
        let text = std::fs::read_to_string(&self.path).unwrap_or_else(|_| {
            self.status = Some(Note::info(format!(
                "{} does not exist yet — these are the built-in defaults",
                self.path.display()
            )));
            dozed_core::starter::starter_config()
        });
        match Config::parse(&text) {
            Ok(cfg) => {
                self.durations = Durations::from(&cfg);
                self.draft = cfg;
                self.error = None;
                self.showing_error = false;
            }
            Err(e) => {
                // Land on the text tab: a file that does not parse cannot be
                // shown as a form, and the fix is in the text anyway.
                self.tab = Tab::File;
                self.error = Some(e.to_string());
                // Nobody typed this one — it was already on disk — so it is
                // shown without waiting to be asked. The rule is about not
                // narrating an edit in progress, not about withholding the
                // reason a file failed to load.
                self.showing_error = true;
            }
        }
        self.text = text;
    }

    /// The config the form describes, or the reason it is not one yet.
    ///
    /// Durations are re-parsed from their text here rather than on every
    /// keystroke, so a half-typed `5` is simply not saveable instead of being
    /// destroyed as it is typed.
    fn form_config(&self) -> Result<Config, String> {
        let mut cfg = self.draft.clone();
        for (label, text, field) in [
            ("base_grace", &self.durations.base_grace, 0),
            ("cpu_sample_window", &self.durations.cpu_sample_window, 1),
            ("resume_cooldown", &self.durations.resume_cooldown, 2),
            ("active_threshold", &self.durations.active_threshold, 3),
            ("silence_after", &self.durations.silence_after, 4),
            ("probe_every", &self.durations.probe_every, 5),
            ("probe_window", &self.durations.probe_window, 6),
        ] {
            let parsed: HumanDuration = text.parse().map_err(|_| {
                format!("`{label} = \"{text}\"` is not a duration — try 30s, 5m, 2h")
            })?;
            match field {
                0 => cfg.defaults.base_grace = parsed,
                1 => cfg.defaults.cpu_sample_window = parsed,
                2 => cfg.defaults.resume_cooldown = parsed,
                3 => cfg.activity.active_threshold = parsed,
                4 => cfg.audio.silence_after = parsed,
                5 => cfg.audio.probe_every = parsed,
                _ => cfg.audio.probe_window = parsed,
            }
        }
        // The real check, and deliberately the same one the daemon makes:
        // render the file this form would write and put it through the parser.
        // A separate "is this valid" routine here would be a second opinion,
        // and the one that matters is the daemon's.
        //
        // Rendering the *annotated* text rather than bare TOML is not
        // incidental — it is byte for byte what `write` will save, so what gets
        // validated and what gets written cannot differ.
        Config::parse(&dozed_core::starter::annotate(&cfg)).map_err(|e| e.to_string())
    }

    /// Re-parse whichever tab is showing, and decide whether the answer is one
    /// the user is owed yet.
    ///
    /// Run every frame. It is a few hundred bytes of TOML and the answer has to
    /// be current at the instant the save button is drawn, not one edit behind
    /// it. What changes per tab is only whether a failure is *said out loud*:
    /// the form reports as it goes, because it can name the field; the text tab
    /// waits to be asked. Either way a config that has come good has nothing
    /// left to report, so the next error starts unasked again.
    fn validate(&mut self) -> Result<Config, String> {
        let validated = match self.tab {
            Tab::Settings => self.form_config(),
            Tab::File => Config::parse(&self.text).map_err(|e| e.to_string()),
        };
        self.error = validated.as_ref().err().cloned();
        if self.error.is_none() {
            self.showing_error = false;
        } else if self.tab == Tab::Settings {
            self.showing_error = true;
        }
        validated
    }

    /// Pressing Save. Writes if it can, and if it cannot, answers the question
    /// that pressing it was asking.
    fn save(&mut self, valid: Option<Config>) -> bool {
        match valid {
            Some(cfg) => match self.write(&cfg) {
                Ok(()) => {
                    self.status = Some(Note::info(format!("saved {}", self.path.display())));
                    true
                }
                Err(e) => {
                    self.status = Some(Note::bad(format!("could not write the file: {e}")));
                    false
                }
            },
            // Asked, so answered.
            None => {
                self.showing_error = true;
                false
            }
        }
    }

    /// What the strip along the bottom says. One line, and only one of these.
    fn strip(&self) -> Strip<'_> {
        match (&self.error, self.showing_error) {
            (Some(error), true) => Strip::Error(error),
            // Mid-edit and not yet asked: withholding the tick is the whole
            // signal.
            (Some(_), false) => Strip::Quiet,
            (None, _) => self.status.as_ref().map_or(Strip::Valid, Strip::Note),
        }
    }

    /// Draw the window. Returns true when the config was saved, so the caller
    /// can tell the daemon to reload it.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        let mut saved = false;
        let mut open = self.open;
        egui::Window::new("Configuration")
            .open(&mut open)
            .default_size([620.0, 620.0])
            .vscroll(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Settings, "Settings")
                        .explain(crate::help::TAB_SETTINGS);
                    ui.selectable_value(&mut self.tab, Tab::File, "Whole file")
                        .explain(crate::help::TAB_FILE);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(self.path.display().to_string())
                                .small()
                                .color(egui::Color32::GRAY),
                        )
                        .explain(crate::help::CONFIG_PATH);
                    });
                });
                ui.separator();

                let validated = self.validate();

                // Room kept for the footer, which is taller while it is showing
                // a parse error: those run to three or four lines and the
                // buttons underneath must not be pushed off the window. Two
                // extra lines, measured rather than guessed, because the font
                // is the desktop's and not this window's to assume.
                let line = ui.text_style_height(&egui::TextStyle::Body);
                let footer =
                    if matches!(self.strip(), Strip::Error(_)) { 92.0 + 3.0 * line } else { 92.0 };
                egui::ScrollArea::vertical().max_height(ui.available_height() - footer).show(
                    ui,
                    |ui| match self.tab {
                        Tab::Settings => self.settings_tab(ui),
                        Tab::File => self.file_tab(ui),
                    },
                );

                ui.separator();
                saved = self.footer(ui, validated.ok());
            });
        // `Window::open` is the close button; anything else that hides the
        // window has to go through the same field.
        self.open = open;
        saved
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        // Aggression first and alone at the top: it is the answer to the
        // question most people open this window with, and burying it among
        // eight other fields would be the same mistake the config file avoids
        // by putting it first.
        ui.add_space(4.0);
        ui.heading("How soon should dozed act?");
        explain(ui, "defaults", "aggression");
        ui.horizontal(|ui| {
            for level in [
                Aggression::Gentle,
                Aggression::Balanced,
                Aggression::Aggressive,
                Aggression::Maximum,
            ] {
                ui.selectable_value(
                    &mut self.draft.defaults.aggression,
                    level,
                    capitalise(level.as_str()),
                )
                .explain(crate::help::aggression(level));
            }
            // Not a `selectable_value`: that one compares against a single
            // fixed value, and every `Custom(f)` is a different value. Selected
            // here means "some custom factor", whatever it happens to be.
            let current = self.draft.defaults.aggression;
            let is_custom = matches!(current, Aggression::Custom(_));
            if ui
                .selectable_label(is_custom, "Custom")
                .explain(crate::help::aggression(Aggression::Custom(current.factor())))
                .clicked()
                && !is_custom
            {
                // Seeded from the level being left, so the slider opens where
                // the user already is instead of jumping somewhere they did not
                // ask for. Picking Custom is a decision to adjust, not to change.
                self.draft.defaults.aggression = Aggression::Custom(current.factor());
            }
        });

        // Only while Custom is chosen. A slider greyed out under four words that
        // already answer the question is furniture, and this window has enough.
        if let Aggression::Custom(factor) = self.draft.defaults.aggression {
            let mut chosen = factor;
            ui.add(
                egui::Slider::new(&mut chosen, Aggression::CUSTOM_MIN..=Aggression::CUSTOM_MAX)
                    .logarithmic(true)
                    .text("x grace"),
            )
            .explain(crate::help::CUSTOM_FACTOR);
            // Compared before assigning so that dragging is the only thing that
            // marks the form dirty; reading the slider back every frame would
            // otherwise report an edit on every repaint.
            if (chosen - factor).abs() > f64::EPSILON {
                self.draft.defaults.aggression = Aggression::Custom(chosen);
            }
        }
        self.preview(ui);

        ui.add_space(10.0);
        ui.collapsing("Mode", |ui| {
            explain(ui, "", "mode");
            ui.horizontal(|ui| {
                // `Monitor` is deliberately absent: it is per-app only and the
                // parser rejects it as a global mode. A dropdown that offers a
                // value the parser refuses is a trap, not a choice.
                ui.selectable_value(&mut self.draft.mode, Mode::DryRun, "Dry run");
                ui.selectable_value(&mut self.draft.mode, Mode::Live, "Live");
            });
        });

        ui.add_space(6.0);
        egui::CollapsingHeader::new("Defaults").default_open(true).show(ui, |ui| {
            section(ui, "defaults");
            duration(ui, "defaults", "base_grace", &mut self.durations.base_grace);
            duration(ui, "defaults", "cpu_sample_window", &mut self.durations.cpu_sample_window);
            boolean(
                ui,
                "defaults",
                "reclaim_on_freeze",
                &mut self.draft.defaults.reclaim_on_freeze,
            );
            boolean(ui, "defaults", "freeze_visible", &mut self.draft.defaults.freeze_visible);

            field(ui, "defaults", "on_ac", |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.draft.defaults.on_ac,
                        AcPolicy::Enabled,
                        "Enabled",
                    );
                    ui.selectable_value(
                        &mut self.draft.defaults.on_ac,
                        AcPolicy::Disabled,
                        "Disabled",
                    );
                });
            });
            duration(ui, "defaults", "resume_cooldown", &mut self.durations.resume_cooldown);
            factor(
                ui,
                "defaults",
                "resume_cooldown_factor",
                &mut self.draft.defaults.resume_cooldown_factor,
            );
            field(ui, "defaults", "resume_cooldown_mode", |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.draft.defaults.resume_cooldown_mode,
                        CooldownMode::Floor,
                        "Floor",
                    );
                    ui.selectable_value(
                        &mut self.draft.defaults.resume_cooldown_mode,
                        CooldownMode::Factor,
                        "Factor",
                    );
                });
            });
        });

        egui::CollapsingHeader::new("Activity").default_open(false).show(ui, |ui| {
            section(ui, "activity");
            duration(ui, "activity", "active_threshold", &mut self.durations.active_threshold);
            factor(ui, "activity", "active_factor", &mut self.draft.activity.active_factor);
            factor(ui, "activity", "away_factor", &mut self.draft.activity.away_factor);
        });

        egui::CollapsingHeader::new("Battery").default_open(false).show(ui, |ui| {
            section(ui, "battery");
            factor(ui, "battery", "ac_factor", &mut self.draft.battery.ac_factor);
            ui.add_space(4.0);
            explain(ui, "battery", "low_pct");
        });

        egui::CollapsingHeader::new("Audio").default_open(false).show(ui, |ui| {
            section(ui, "audio");
            boolean(ui, "audio", "detect_silence", &mut self.draft.audio.detect_silence);
            duration(ui, "audio", "silence_after", &mut self.durations.silence_after);
            duration(ui, "audio", "probe_every", &mut self.durations.probe_every);
            duration(ui, "audio", "probe_window", &mut self.durations.probe_window);
            // Negative dBFS only: full scale is zero and there is nothing above
            // it, so the range is what stops a typo asking for a threshold no
            // sample can be under.
            ui.add_space(8.0);
            ui.label(egui::RichText::new("silence_dbfs").monospace().strong());
            ui.add(
                egui::DragValue::new(&mut self.draft.audio.silence_dbfs)
                    .speed(1.0)
                    .range(-120.0..=0.0),
            );
            explain(ui, "audio", "silence_dbfs");
        });

        egui::CollapsingHeader::new("Interface").default_open(false).show(ui, |ui| {
            section(ui, "ui");
            boolean(ui, "ui", "tray", &mut self.draft.ui.tray);
        });

        egui::CollapsingHeader::new(format!("Rules ({})", self.draft.rules.len()))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Read-only here — a form cannot express a rule table honestly. \
                         Edit them in the \"Whole file\" tab, where the same \
                         cannot-save-it-broken rule applies.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
                ui.add_space(4.0);
                egui::Grid::new("rules").num_columns(3).striped(true).spacing([12.0, 3.0]).show(
                    ui,
                    |ui| {
                        for rule in &self.draft.rules {
                            let about = crate::help::rule_row(rule);
                            ui.label(egui::RichText::new(rule.priority.to_string()).monospace())
                                .explain(&about);
                            ui.label(egui::RichText::new(&rule.name).monospace()).explain(&about);
                            ui.label(
                                egui::RichText::new(rule.action.describe())
                                    .small()
                                    .color(egui::Color32::GRAY),
                            )
                            .explain(&about);
                            ui.end_row();
                        }
                    },
                );
            });
    }

    /// What the current settings actually mean, in seconds, right now.
    ///
    /// The reason the form is worth having over the file: `aggression =
    /// "aggressive"` is a word, and "your hidden browser freezes after 2m"
    /// is the thing being chosen. Computed with the real [`Grace`] rather
    /// than a copy of its arithmetic, so it cannot drift from what the
    /// daemon will do.
    fn preview(&self, ui: &mut egui::Ui) {
        let Ok(cfg) = self.form_config() else { return };
        let base = cfg
            .rules
            .iter()
            .find(|r| r.name == "bg-idle")
            .and_then(dozed_core::Rule::grace)
            .unwrap_or_else(|| cfg.defaults.base_grace.get());

        let at = |power: &PowerState| {
            Grace::compute(&cfg, base, power, DesktopState::IdlePresent, false)
                .effective()
                .map_or_else(|| "never".to_owned(), |d| Coarse(d).to_string())
        };
        let battery = PowerState { on_ac: false, percentage: Some(100.0) };
        let low = PowerState { on_ac: false, percentage: Some(10.0) };
        let ac = PowerState { on_ac: true, percentage: Some(80.0) };

        ui.add_space(6.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(
                egui::RichText::new("With these settings, a hidden app is acted on after:").small(),
            )
            .explain(crate::help::PREVIEW);
            egui::Grid::new("preview").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
                for (label, value) in [
                    ("on battery, full", at(&battery)),
                    ("on battery, 10%", at(&low)),
                    ("plugged in", at(&ac)),
                    (
                        "while you are away",
                        at(&PowerState { on_ac: false, percentage: Some(100.0) }),
                    ),
                ] {
                    ui.label(egui::RichText::new(label).small().color(egui::Color32::GRAY));
                    ui.label(egui::RichText::new(value).small().monospace());
                    ui.end_row();
                }
            });
            ui.label(
                egui::RichText::new(
                    "…if no other rule matches it first, and four times longer while you are \
                     actively typing.",
                )
                .small()
                .color(egui::Color32::GRAY),
            );
        });
    }

    fn file_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "The whole file, including the rule table. Nothing is checked while you type; \
                 Save runs the daemon's parser and refuses a file it would not start on, so \
                 the worst you can do here is not be allowed to save.",
            )
            .small()
            .color(egui::Color32::GRAY),
        )
        .explain(crate::help::TAB_FILE);
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut self.text)
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(28),
        );
    }

    /// The error strip and the buttons. Returns true if a save happened.
    fn footer(&mut self, ui: &mut egui::Ui, valid: Option<Config>) -> bool {
        let mut saved = false;
        let bad = egui::Color32::from_rgb(220, 120, 60);
        match self.strip() {
            // The parser's own words. A friendlier paraphrase here would be a
            // second error message to keep in step with the first, and the
            // daemon prints the original at startup anyway.
            // Bounded and scrollable. A TOML error is several lines — message,
            // the offending line, a caret under it — and left to grow it pushed
            // the Save button off the bottom of the window, which is the one
            // control somebody looking at a parse error is about to want.
            Strip::Error(error) => {
                let height = 4.0 * ui.text_style_height(&egui::TextStyle::Body);
                egui::ScrollArea::vertical()
                    .id_salt("error")
                    .max_height(height)
                    // Visible always, so a message taller than the box reads as
                    // one that continues rather than one that got cut off.
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .show(ui, |ui| {
                        ui.colored_label(bad, format!("✖ {error}")).explain(
                            "What the daemon's own parser says about this file. Nothing is written \
                         while it says it.",
                        );
                    });
            }
            // A blank line rather than nothing at all: the strip must not change
            // height under the buttons as it comes and goes.
            Strip::Quiet => {
                ui.label(" ");
            }
            Strip::Note(note) => {
                ui.colored_label(if note.bad { bad } else { egui::Color32::GRAY }, &note.text);
            }
            Strip::Valid => {
                ui.label(
                    egui::RichText::new("✔ valid").color(egui::Color32::from_rgb(120, 180, 120)),
                )
                .explain(crate::help::VALID);
            }
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // In the form a broken config means a named field is half-typed and
            // the button is disabled until it is not. In the text tab the button
            // stays live and *refuses*, because pressing Save is how somebody
            // asks whether the file is good yet — a greyed-out button with no
            // error beside it would be a locked door with no sign on it.
            let deferred = self.tab == Tab::File;
            let save =
                ui.add_enabled(deferred || valid.is_some(), egui::Button::new("Save and reload"));
            if save
                .explain(if deferred {
                    "Parses the file and, if it parses, writes it and asks the daemon to pick \
                     it up. If it does not, the parser's message appears above and nothing is \
                     written."
                } else {
                    "Writes the file and asks the daemon to pick it up"
                })
                .on_disabled_hover_text(
                    "Fix the error above first — this cannot save a broken config",
                )
                .clicked()
            {
                saved = self.save(valid);
            }

            if ui
                .button("Revert")
                .explain("Throw away these edits and re-read the file from disk")
                .clicked()
            {
                self.reload_from_disk();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Explain everything")
                    .explain(
                        "Rewrite the file with an up-to-date comment on every setting, \
                         keeping your values. The same as `dozed --annotate-config`.",
                    )
                    .clicked()
                {
                    match self
                        .form_config()
                        .or_else(|_| Config::parse(&self.text).map_err(|e| e.to_string()))
                    {
                        Ok(cfg) => {
                            self.text = dozed_core::starter::annotate(&cfg);
                            self.tab = Tab::File;
                            self.status = Some(Note::info(
                                "annotated — nothing saved yet, press Save to keep it",
                            ));
                        }
                        // Nothing to annotate a file with that nobody can parse,
                        // and the reason is the parser's, so it is shown rather
                        // than held back: this was asked for too.
                        Err(e) => {
                            self.error = Some(e);
                            self.showing_error = true;
                        }
                    }
                }
            });
        });
        saved
    }

    /// Write, keeping a copy of what was there.
    ///
    /// The same bargain `--annotate-config` makes: this window is a convenience,
    /// and a convenience that can lose a config somebody spent a week tuning is
    /// not one. The backup is only ever written once, so it stays the version
    /// from before the GUI first touched the file.
    fn write(&self, cfg: &Config) -> Result<(), String> {
        let text = match self.tab {
            // The user's own text, verbatim: it parsed, so it is valid, and
            // reformatting somebody's file underneath them is not this
            // window's business.
            Tab::File => self.text.clone(),
            Tab::Settings => dozed_core::starter::annotate(cfg),
        };
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(parent) = self.path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .or_else(|e| if parent.is_dir() { Ok(()) } else { Err(e) })
                .map_err(|e| e.to_string())?;
        }
        if self.path.exists() {
            let backup = backup_path(&self.path);
            if !backup.exists() {
                // `copy` carries the original's mode across, which is the right
                // answer here: the backup is that file, and should be no more
                // readable than it was.
                std::fs::copy(&self.path, &backup).map_err(|e| e.to_string())?;
            }
        }
        // `mode` applies only when creating, so a config that already exists keeps
        // whatever the user gave it — this window is not the place to re-permission
        // somebody's file. A *new* one is owner-only, which is the same answer
        // `dozed --write-default-config` gives, so the mode does not depend on
        // which of the two wrote it.
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()))
            .map_err(|e| e.to_string())
    }
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.bak-pregui",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// One labelled control with its explanation underneath.
fn field(ui: &mut egui::Ui, table: &str, key: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(key).monospace().strong());
    control(ui);
    explain(ui, table, key);
}

fn duration(ui: &mut egui::Ui, table: &str, key: &str, value: &mut String) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(key).monospace().strong());
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(90.0));
        // Immediate, per-field feedback: the save button says *something* is
        // wrong, and this says which box it is in.
        match value.parse::<HumanDuration>() {
            Ok(d) => ui.label(
                egui::RichText::new(format!("= {}", HumanDuration(d.get())))
                    .small()
                    .color(egui::Color32::GRAY),
            ),
            Err(_) => ui.colored_label(
                egui::Color32::from_rgb(220, 120, 60),
                "not a duration — try 30s, 5m, 2h",
            ),
        };
    });
    explain(ui, table, key);
}

fn boolean(ui: &mut egui::Ui, table: &str, key: &str, value: &mut bool) {
    ui.add_space(8.0);
    ui.checkbox(value, egui::RichText::new(key).monospace().strong());
    explain(ui, table, key);
}

/// A multiplier. Clamped at zero because a negative one is not a preference,
/// it is a config error the grace model has to defend itself against.
fn factor(ui: &mut egui::Ui, table: &str, key: &str, value: &mut f64) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(key).monospace().strong());
    ui.add(egui::DragValue::new(value).speed(0.1).range(0.0..=100.0));
    explain(ui, table, key);
}

fn section(ui: &mut egui::Ui, table: &str) {
    if let Some(doc) = dozed_core::starter::explain_section(table) {
        ui.label(egui::RichText::new(doc.join(" ")).small().color(egui::Color32::GRAY));
    }
}

/// The field's explanation, in the words `config.toml` uses for it.
fn explain(ui: &mut egui::Ui, table: &str, key: &str) {
    let Some(doc) = dozed_core::starter::explain(table, key) else { return };
    // Paragraph breaks are blank entries in the doc; the first paragraph is
    // always the what, so it is shown and the rest is a click away. Dumping
    // twenty lines of rationale under every control would make the window
    // unreadable and the rationale unread.
    let mut paragraphs = doc.split(|l| l.is_empty());
    let first = paragraphs.next().unwrap_or_default().join(" ");
    ui.label(egui::RichText::new(first).small().color(egui::Color32::GRAY));
    let rest: Vec<String> = paragraphs.map(|p| p.join(" ")).collect();
    if !rest.is_empty() {
        // Salted: egui ids a collapsing header by its label, and every one of
        // these is labelled "why", so unsalted they are all the same widget.
        egui::CollapsingHeader::new(egui::RichText::new("why").small())
            .id_salt(("why", table, key))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(rest.join("\n\n")).small().color(egui::Color32::GRAY));
            });
    }
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().to_string() + chars.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything except the drawing. The window is a thin layer over these
    /// three operations — validate, write, reload — and they are where the
    /// promises live.
    fn editor(dir: &Path) -> ConfigEditor {
        ConfigEditor { path: dir.join("config.toml"), ..ConfigEditor::default() }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dozed-gui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The headline promise: the save path cannot produce a file the daemon
    /// would refuse to start on.
    #[test]
    fn a_half_typed_duration_is_not_saveable() {
        let dir = tmp("duration");
        let mut e = editor(&dir);
        e.reload_from_disk();

        e.durations.base_grace = "5".into();
        let err = e.form_config().unwrap_err();
        assert!(err.contains("base_grace"), "{err}");
        assert!(
            err.contains("30s, 5m, 2h"),
            "the error has to say what a duration looks like: {err}"
        );

        // And the moment it becomes one, it is saveable again — the point of
        // holding durations as text rather than rejecting them mid-keystroke.
        e.durations.base_grace = "5m".into();
        assert!(e.form_config().is_ok());
    }

    #[test]
    fn what_the_form_writes_is_always_what_it_validated() {
        let dir = tmp("write");
        let mut e = editor(&dir);
        e.reload_from_disk();
        e.draft.defaults.aggression = Aggression::Maximum;
        e.draft.mode = Mode::Live;

        let validated = e.form_config().expect("valid");
        e.write(&validated).unwrap();

        let on_disk = std::fs::read_to_string(&e.path).unwrap();
        let reparsed = Config::parse(&on_disk).expect("what was written must parse");
        assert_eq!(reparsed, validated, "the file on disk is not what was validated");
        assert_eq!(reparsed.defaults.aggression, Aggression::Maximum);
        assert!(on_disk.contains("HOW SOON DOZED ACTS"), "the written file explains itself");
    }

    /// A file somebody has hand-edited is theirs. Saving from the text tab must
    /// not reformat it, reorder it, or drop the comments they wrote.
    #[test]
    fn saving_from_the_text_tab_writes_the_users_own_bytes() {
        let dir = tmp("text");
        let mut e = editor(&dir);
        e.tab = Tab::File;
        e.text = "# my own note\nmode = \"live\"\n\n[defaults]\naggression = \"gentle\"\n".into();

        let validated = Config::parse(&e.text).expect("valid");
        e.write(&validated).unwrap();

        assert_eq!(std::fs::read_to_string(&e.path).unwrap(), e.text);
    }

    #[test]
    fn a_file_that_does_not_parse_opens_on_the_text_tab_and_says_why() {
        let dir = tmp("broken");
        let path = dir.join("config.toml");
        std::fs::write(&path, "mode = \"live\"\n[[rule]]\nname = \"x\"\nvisibilty = \"hidden\"\n")
            .unwrap();

        let mut e = editor(&dir);
        e.reload_from_disk();

        assert_eq!(e.tab, Tab::File, "a broken file cannot be shown as a form");
        assert!(e.error.is_some(), "and the parser's reason has to be on screen");
        assert!(e.text.contains("visibilty"), "with the text left exactly as it was");
    }

    /// The editor is a convenience, and a convenience that can lose a config
    /// somebody spent a week tuning is not one.
    #[test]
    fn the_first_save_backs_the_original_up_and_later_saves_keep_that_copy() {
        let dir = tmp("backup");
        let mut e = editor(&dir);
        let original = "mode = \"live\"\n[defaults]\nbase_grace = \"9m\"\n";
        std::fs::write(&e.path, original).unwrap();
        e.reload_from_disk();

        e.draft.defaults.base_grace = "1m".parse().unwrap();
        e.durations = Durations::from(&e.draft);
        e.write(&e.form_config().unwrap()).unwrap();
        let backup = backup_path(&e.path);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

        e.draft.defaults.base_grace = "2m".parse().unwrap();
        e.durations = Durations::from(&e.draft);
        e.write(&e.form_config().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            original,
            "the pre-GUI original is the copy worth keeping"
        );
    }

    /// The whole-file tab, mid-edit. Half a config is invalid nearly all of the
    /// time, and a red strip on every keystroke is how the one that matters gets
    /// ignored.
    #[test]
    fn the_text_tab_says_nothing_until_it_is_asked() {
        let dir = tmp("deferred");
        let mut e = editor(&dir);
        e.tab = Tab::File;
        e.text = "mode = \"live\"\n[defaults]\naggression = ".into();

        assert!(e.validate().is_err());
        assert_eq!(e.strip(), Strip::Quiet, "an unfinished line is not an error report");

        // Pressing Save is the question. This is the answer to it.
        assert!(!e.save(None), "and nothing was written");
        assert!(matches!(e.strip(), Strip::Error(_)), "{:?}", e.strip());
        assert!(!e.path.exists(), "a file the parser refused must never reach the disk");

        // Still invalid after another keystroke: the error stays, because it has
        // been asked for and the answer has not changed.
        e.text.push('"');
        assert!(e.validate().is_err());
        assert!(matches!(e.strip(), Strip::Error(_)));

        // And once it parses, the tick is back and the next error starts unasked.
        e.text.push_str("gentle\"\n");
        let valid = e.validate().expect("valid now");
        assert_eq!(e.strip(), Strip::Valid);
        assert!(e.save(Some(valid)));
        e.text.push_str("aggression = ");
        assert!(e.validate().is_err());
        assert_eq!(e.strip(), Strip::Quiet, "a fresh mistake is not shouted about either");
    }

    /// The form is the other half of that bargain: it knows which field is
    /// wrong, so it says so without being asked.
    #[test]
    fn the_form_reports_as_it_goes() {
        let dir = tmp("immediate");
        let mut e = editor(&dir);
        e.reload_from_disk();
        e.durations.base_grace = "5".into();

        assert!(e.validate().is_err());
        assert!(matches!(e.strip(), Strip::Error(_)), "{:?}", e.strip());
    }

    /// A failed write is not a parse error — the file is fine, the disk was not
    /// — and it used to be erased by the next frame's validation.
    #[test]
    fn a_write_that_fails_stays_on_screen() {
        let dir = tmp("unwritable");
        let mut e = editor(&dir);
        e.reload_from_disk();
        // A path whose parent is a file, so `create_dir_all` cannot succeed.
        std::fs::write(dir.join("wall"), "not a directory").unwrap();
        e.path = dir.join("wall").join("config.toml");
        let valid = e.validate().expect("the defaults are valid");

        assert!(!e.save(Some(valid)));
        assert!(matches!(e.strip(), Strip::Note(n) if n.bad), "{:?}", e.strip());
        assert!(e.validate().is_ok(), "the config itself is still fine");
        assert!(
            matches!(e.strip(), Strip::Note(n) if n.bad && n.text.contains("could not write")),
            "and revalidating must not erase what went wrong: {:?}",
            e.strip()
        );
    }

    /// A daemon running on built-in defaults has no file at all. That is the
    /// case the window is most useful in, so it must not be an error.
    #[test]
    fn a_missing_file_offers_the_defaults_rather_than_failing() {
        let dir = tmp("missing");
        let mut e = editor(&dir);
        e.reload_from_disk();

        assert!(e.error.is_none());
        assert_eq!(e.draft, Config::default());
        assert!(e.status.as_ref().is_some_and(|n| n.text.contains("does not exist yet")));
        assert!(e.form_config().is_ok(), "and is immediately saveable");
    }

    /// Every control in the form is a field the config file explains. A widget
    /// with no prose beside it is the thing this window exists to avoid.
    #[test]
    fn every_field_the_form_edits_has_an_explanation() {
        for (table, key) in [
            ("", "mode"),
            ("defaults", "aggression"),
            ("defaults", "base_grace"),
            ("defaults", "cpu_sample_window"),
            ("defaults", "reclaim_on_freeze"),
            ("defaults", "freeze_visible"),
            ("defaults", "on_ac"),
            ("defaults", "resume_cooldown"),
            ("defaults", "resume_cooldown_factor"),
            ("defaults", "resume_cooldown_mode"),
            ("activity", "active_threshold"),
            ("activity", "active_factor"),
            ("activity", "away_factor"),
            ("battery", "ac_factor"),
            ("ui", "tray"),
        ] {
            assert!(
                dozed_core::starter::explain(table, key).is_some(),
                "the form edits `{key}` with nothing to tell the user what it is"
            );
        }
    }
}
