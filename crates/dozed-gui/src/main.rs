//! `dozed-gui` — the dashboard (`DESIGN.md` §2, §16 step 3).
//!
//! Worth building only now that real traces exist to display. It shows four
//! things, in the order they answer questions:
//!
//! 1. **Is it working, and what is it costing me?** Mode, focus source, the
//!    daemon's own wakeups, and whether each exemption source is connected —
//!    the last because a rule matching on state nobody sets can never fire, and
//!    that failure was silent until it was surfaced.
//! 2. **What would it save?** The counterfactual report, which is the whole
//!    argument for switching live mode on (§11).
//! 3. **What is it doing to each app, and why?**
//! 4. **What happened?** The live timeline.
//!
//! Plus the panic-thaw button §10.5 requires, which works unconditionally.
//!
//! The window is not the point and must not become a cost of its own: it
//! repaints when the daemon says something, not on a frame clock, so an open
//! dashboard on an idle desktop draws nothing.
//!
//! Every figure and every state word on screen carries its explanation on hover,
//! from [`help`] — what the number is measured over, what the state means, and
//! what it changes. A dashboard of unlabelled rates is a dashboard only its
//! author can read, and the config editor already showed that the fix is prose
//! beside the value rather than a manual somewhere else.

use std::sync::{Arc, Mutex};

use eframe::egui;

use dozed_core::coverage::Coverage;
use dozed_core::ipc::PendingChange;
use dozed_core::{Action, Coarse, Mode};

mod client;
mod config_editor;
mod help;
mod single_instance;
mod window_state;

use client::{Client, Command};
use config_editor::ConfigEditor;

/// Shortest gap between repaints. Five a second is well past what a dashboard
/// of second-granularity numbers can show, and an order of magnitude below what
/// the data alone was producing.
const MIN_FRAME: std::time::Duration = std::time::Duration::from_millis(200);

/// The width a tooltip is drawn between.
///
/// The minimum is the load-bearing half. Left to itself egui sizes the box from
/// the space beside the widget it hangs off, and beside a label in the timeline
/// panel that is almost none — a three-sentence explanation came out as a column
/// two words wide (egui#5167). The maximum is the ordinary one: a line of text
/// stops being readable somewhere past this.
const TOOLTIP_MIN_WIDTH: f32 = 260.0;
const TOOLTIP_MAX_WIDTH: f32 = 420.0;

/// Attaching an explanation to something on screen.
///
/// Every hover in this crate goes through here rather than `on_hover_text`, so
/// that the box is the same shape wherever it appears and cannot be squeezed
/// into a column by whatever happens to be beside it.
/// The modes the header's badge offers, in the order it offers them.
///
/// `Mode::Monitor` is deliberately absent, and this list is a named constant so
/// that the absence is testable rather than a thing somebody has to notice.
/// Monitor is a *per-app* mode — "record for this one while the rest run live" —
/// so a global Monitor would do exactly what dry-run does while reading as
/// something else, and the row of apps below already offers it per app.
///
/// Each entry says what the mode *does* rather than which direction it moves.
/// "Go live" was fine on a button, because a button only ever means "do this";
/// in a list beside its alternative, a verb reads as a description of the
/// current state and the wrong one gets clicked.
const GLOBAL_MODES: [(Mode, &str, &str); 2] = [
    (Mode::DryRun, "Dry-run — decide, record, touch nothing", help::BACK_TO_DRY_RUN),
    (Mode::Live, "Live — actually freeze and throttle", help::GO_LIVE),
];

/// The snooze lengths the header offers, shortest first.
///
/// A named constant for the same reason [`GLOBAL_MODES`] is: it is the whole of
/// what a user can pick, so it is worth being able to test rather than read.
///
/// The set is chosen from what a snooze is *for* — the meeting, the
/// presentation, the build, the afternoon of not wanting to think about it — and
/// stops at four hours deliberately. A snooze is the control you reach for
/// because switching dozed off is permanent and nobody remembers to switch it
/// back on; one long enough to forget is the same failure wearing a timer.
const SNOOZE_OPTIONS: [(u64, &str); 5] = [
    (15 * 60, "15 minutes"),
    (30 * 60, "30 minutes"),
    (60 * 60, "1 hour"),
    (2 * 60 * 60, "2 hours"),
    (4 * 60 * 60, "4 hours"),
];

trait Explain {
    /// Returns the response, so hovers can be chained or dropped.
    fn explain(self, text: impl Into<egui::WidgetText>) -> Self;
}

impl Explain for egui::Response {
    fn explain(self, text: impl Into<egui::WidgetText>) -> Self {
        let text = text.into();
        self.on_hover_ui(|ui| {
            ui.set_min_width(TOOLTIP_MIN_WIDTH);
            ui.set_max_width(TOOLTIP_MAX_WIDTH);
            ui.add(egui::Label::new(text));
        })
    }
}

/// `text` cut to `max` characters, with an ellipsis when something was cut.
///
/// Counts `char`s rather than bytes. Slicing a `str` at a byte index that is not
/// a character boundary panics, and the strings reaching here are app names and
/// daemon messages — both of which carry non-ASCII in the ordinary course of
/// things, so a byte-indexed version of this would be a crash waiting for
/// somebody to name a window in Greek.
fn elide(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_none() { head } else { format!("{head}…") }
}

fn main() -> eframe::Result {
    // Before anything is created, and before a window could flash: the tray
    // spawns this binary on every click of "Open dashboard", so a second run is
    // the ordinary case rather than the exceptional one. It tells the first to
    // come forward and stops here.
    //
    // The waker is filled in once the context exists — `acquire` needs a
    // callback now and egui does not exist yet, so it goes through a slot the
    // listener thread reads. Repaint-on-data is the whole reason: this window
    // does not run on a frame clock, so a raise nobody woke would sit unread
    // until the daemon next said something.
    let waker: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
    let for_listener = Arc::clone(&waker);
    let raise = match single_instance::acquire(move || {
        if let Ok(ctx) = for_listener.lock()
            && let Some(ctx) = ctx.as_ref()
        {
            ctx.request_repaint();
        }
    }) {
        single_instance::Instance::First(flag) => flag,
        single_instance::Instance::AlreadyRunning => return Ok(()),
    };
    // Unlinks the socket on the way out, so the next launch does not have to
    // prove a leftover one is stale.
    let _guard = single_instance::Guard::new();

    // Where it was last time, or the shipped size on a first run. A status
    // window is opened and closed dozens of times, and one that forgets is one
    // you resize dozens of times.
    let geometry = window_state::WindowState::load();
    let options = eframe::NativeOptions {
        viewport: geometry.restore(
            egui::ViewportBuilder::default()
                .with_min_inner_size([560.0, 360.0])
                .with_title("dozed")
                // Must match the basename of `packaging/dozed-gui.desktop`, and
                // that is the whole point of setting it: on Wayland the app id
                // is the only thing tying a window to its desktop entry, and
                // without the tie the window gets no icon and no name in the
                // task switcher — it shows up as whatever winit defaulted to.
                .with_app_id("dozed-gui"),
        ),
        ..Default::default()
    };
    eframe::run_native(
        "dozed",
        options,
        Box::new(move |cc| {
            // Before the first frame, so the window is laid out at the size the
            // user reads it at rather than snapping a frame later.
            cc.egui_ctx.set_zoom_factor(geometry.zoom);
            // The listener thread has been able to set the flag since before
            // this point; what it could not do was wake us. Filling the slot
            // here closes that gap, and the flag it may already have set is read
            // on the first frame regardless.
            if let Ok(mut slot) = waker.lock() {
                *slot = Some(cc.egui_ctx.clone());
            }
            let ctx = cc.egui_ctx.clone();
            // Repaint on data, not on a timer: an idle dashboard should cost
            // nothing, which is awkward to argue for a tool that exists to stop
            // things waking the CPU.
            //
            // Rate-limited on top of that, because "on data" is not the same as
            // "rarely". One poll cycle brings three replies — status, apps,
            // report — and each used to repaint on arrival, so a 2s poll drew
            // three frames back to back and the journal stream added more on
            // top. Measured on a visible window: 81 wakeups/s.
            //
            // Deliberately not `request_repaint_after` on a fixed interval,
            // which would turn an idle dashboard into a 5 Hz timer and cost
            // more than it saves. This only ever *delays* a repaint that data
            // has already asked for, so with nothing arriving the cost stays
            // zero.
            let last = std::sync::Mutex::new(Option::<std::time::Instant>::None);
            Ok(Box::new(Gui::new(
                geometry,
                Client::spawn(move || {
                    let wait = {
                        let Ok(mut last) = last.lock() else { return };
                        let now = std::time::Instant::now();
                        match *last {
                            Some(prev) if now.duration_since(prev) < MIN_FRAME => {
                                Some(MIN_FRAME - now.duration_since(prev))
                            }
                            _ => {
                                *last = Some(now);
                                None
                            }
                        }
                    };
                    match wait {
                        // Already drawn recently: ask egui for one frame at the
                        // end of the window instead. Repeated calls inside the same
                        // window collapse into that single frame.
                        Some(remaining) => ctx.request_repaint_after(remaining),
                        None => ctx.request_repaint(),
                    }
                }),
                raise,
            )))
        }),
    )
}

struct Gui {
    client: Client,
    /// Remembers the window's size and place across restarts.
    geometry: window_state::Tracker,
    /// Which app's detail is expanded, if any.
    selected: Option<String>,
    /// The config editor, which owns its own window.
    config: ConfigEditor,
    /// Set by another instance that wants this window brought forward.
    raise: single_instance::RaiseFlag,
}

impl Gui {
    fn new(
        geometry: window_state::WindowState,
        client: Client,
        raise: single_instance::RaiseFlag,
    ) -> Self {
        Self {
            client,
            geometry: window_state::Tracker::new(geometry),
            selected: None,
            config: ConfigEditor::default(),
            raise,
        }
    }

    /// Come forward, if another instance has asked us to.
    ///
    /// Three commands, because "raise" is three different things depending on
    /// where the window was, and the click means all of them:
    ///
    /// * un-minimise, since a minimised window cannot be focused;
    /// * focus, which is the actual request;
    /// * ask for attention, which is the honest fallback. **Under Wayland a
    ///   client cannot simply raise itself** — focus stealing prevention is the
    ///   compositor's, not ours, and `Focus` is a request that may be declined.
    ///   When it is, the window still gets its taskbar entry highlighted, which
    ///   is the difference between the click doing nothing and the click doing
    ///   what the compositor permits.
    ///
    /// Taken with `swap`, so a raise is consumed once however many frames it
    /// takes to be honoured.
    fn raise_if_asked(&self, ctx: &egui::Context) {
        if !self.raise.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
            egui::UserAttentionType::Informational,
        ));
    }
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First thing in the frame: the click that asked for this is waiting on
        // it, and everything below is about what to draw rather than whether to
        // be visible at all.
        self.raise_if_asked(ctx);
        // Minimised or fully hidden means nobody is reading this, so the poll
        // thread should stop asking the daemon for data it will not draw. Read
        // from the viewport rather than guessed: `focused` would be wrong, since
        // a dashboard is most useful sitting visible beside the thing it
        // describes.
        let on_screen = ctx.input(|i| {
            let v = &i.viewport();
            !v.minimized.unwrap_or(false)
        });
        self.client.set_visible(on_screen);

        // Costs a comparison per frame and a write only once the window has
        // stopped moving — a drag-resize repaints continuously, and saving on
        // every frame would make two seconds of resizing a hundred file writes.
        self.geometry.observe(ctx, std::time::Instant::now());
        // Catch a resize made in the last moments before the window shut. The
        // settle timer means a change made and immediately followed by a close
        // would otherwise never be written, which is the exact shape of "resize
        // it, decide that will do, close it". Done here rather than in
        // `App::on_exit`, which is handed no `Context` to read the window from.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.geometry.flush(ctx);
        }

        let snapshot = match self.client.shared.lock() {
            Ok(s) => s,
            // A poisoned lock means the IPC thread panicked. Say so rather than
            // panicking in turn and taking the panic-thaw button with us.
            Err(e) => e.into_inner(),
        };

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("dozed").explain(help::slogan(&snapshot));
                ui.separator();

                match &snapshot.status {
                    Some(s) => {
                        let (label, colour) = match s.mode {
                            Mode::Live => ("LIVE", egui::Color32::from_rgb(220, 120, 60)),
                            Mode::DryRun => ("dry-run", egui::Color32::GRAY),
                            Mode::Monitor => ("monitor", egui::Color32::GRAY),
                        };
                        // The badge *is* the control. It used to be a bare word
                        // with a "Back to dry-run" button at the far end of the
                        // same row, and that button was the widest thing in a
                        // right-aligned group laid out from the edge inwards — so
                        // it ate the far end of the status text opposite, which is
                        // where the power figures had just been added. Putting the
                        // control on the badge frees the width *and* puts the mode
                        // where the mode is already displayed, rather than eleven
                        // characters and half a window away from it.
                        //
                        // A badge saying `dry-run` over a config file saying
                        // `live` is the one place the window can leave somebody
                        // arguing with it, so the hover still carries the reason.
                        //
                        // `Monitor` is deliberately not offered. It is a per-app
                        // mode — "record for this one while the rest run live" —
                        // and a global one would mean the same thing as dry-run
                        // while reading as something else.
                        ui.menu_button(egui::RichText::new(label).color(colour), |ui| {
                            // Each entry says what it does rather than which
                            // direction it moves. "Go live" was fine as a button
                            // because a button only ever means "do this"; in a
                            // list beside its alternative, a verb reads as the
                            // current state and the wrong one gets clicked.
                            for (mode, entry, hover) in GLOBAL_MODES {
                                let text = egui::RichText::new(entry);
                                // The warning colour follows the consequence, not
                                // the selection: live is the one entry here that
                                // changes what happens to your applications, and
                                // it should look like it whether or not it is the
                                // mode in force.
                                let text = if mode == Mode::Live {
                                    text.color(egui::Color32::from_rgb(220, 120, 60))
                                } else {
                                    text
                                };
                                if ui
                                    .selectable_label(s.mode == mode, text)
                                    .explain(hover)
                                    .clicked()
                                {
                                    // Sent even when it is already the mode in
                                    // force. It costs one message and it is the
                                    // honest response to a click: a control that
                                    // silently does nothing is indistinguishable
                                    // from one that is broken.
                                    self.client.send(Command::SetMode(mode));
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .explain(help::mode(s));
                        ui.separator();
                        ui.label(if s.power.on_ac { "AC" } else { "battery" })
                            .explain(help::power(s));
                        if let Some(pct) = s.power.percentage {
                            ui.label(format!("{pct:.0}%")).explain(help::power(s));
                        }
                        ui.separator();
                        ui.label(s.desktop.as_str()).explain(help::desktop(s.desktop));
                        ui.separator();
                        // §10.2: without focus we hold nothing, and the window
                        // should say so loudly rather than looking healthy.
                        if s.focus_source_healthy {
                            ui.label(format!("{} apps, {} held", s.apps_known, s.apps_held))
                                .explain(help::apps(s));
                        } else {
                            // Two words, not a sentence. This row is shared with
                            // the button group, which is laid out right-to-left
                            // from the far edge, so anything long on this side
                            // does not wrap — it runs into the buttons and gets
                            // clipped, and what gets clipped is the end of the
                            // warning. "focus source LOST — holding nothing" was
                            // thirty-five characters standing in for a count of
                            // about twelve. The consequence it used to spell out
                            // is the first line of the hover, where there is room
                            // for the whole of it.
                            ui.colored_label(egui::Color32::from_rgb(220, 120, 60), "focus LOST")
                                .explain(help::FOCUS_LOST);
                        }
                        // What the machine is drawing, on hardware that will say.
                        //
                        // Outside the focus-source branch above, because power
                        // draw is a fact about the machine and stays worth
                        // reading when dozed has lost track of focus — arguably
                        // more so, since that is the state in which it is holding
                        // nothing.
                        //
                        // Absent rather than zero where the hardware will not say:
                        // on mains `power_now` reads 0 with status `Not charging`,
                        // and a confident `0.0 W` is a worse answer than no
                        // answer (`dozed_linux::energy`).
                        if let Some(watts) = s.energy.as_ref().and_then(|e| e.discharge_w) {
                            ui.separator();
                            ui.label(format!("{watts:.1} W")).explain(help::draw(watts, s));
                        }
                        // The CPU package, on the machines whose RAPL counter is
                        // readable at all. Labelled rather than bare: next to a
                        // whole-machine discharge figure, a second unlabelled "W"
                        // reads as a disagreement about the same quantity.
                        if let Some(watts) = s.energy.as_ref().and_then(|e| e.package_w) {
                            ui.separator();
                            ui.label(format!("cpu {watts:.1} W"))
                                .explain(help::package_draw(watts, s));
                        }
                        // The GPU wattage is **not** here, and taking it out
                        // again is the honest end of a measurement.
                        //
                        // This row is one `horizontal`: status laid out from the
                        // left, and a button group laid out right-to-left from
                        // the window edge. Neither reserves space from the other,
                        // so they do not wrap when they meet — they overlap. The
                        // file already works around this twice, in the two-word
                        // focus warning and the one-word trial state, and both
                        // comments say the same thing: every character on this
                        // side is paid for out of a fixed budget.
                        //
                        // The budget was already spent before a GPU figure was
                        // added. On screen, `gpu 23.1 W` and a running trial's
                        // `trial 1/12 (settling)` were drawn on top of one
                        // another, and shortening the label to the bone did not
                        // buy enough back. So it is not shown here. It is in
                        // `dozectl status`, the GPU's *utilisation* is in the
                        // coverage footer below where there is room to wrap, and
                        // the relationship between the two chips is in the hover
                        // on the cpu figure beside this.
                        //
                        // The real fix is structural — reserve the button group's
                        // width before laying out the status, which is egui's own
                        // idiom for this and would retire all three workarounds —
                        // and it is a bigger change than a wattage justifies.
                    }
                    None => {
                        ui.colored_label(egui::Color32::GRAY, "connecting…")
                            .explain(help::CONNECTING);
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // §10.5. Ignores rules, ignores mode, always available.
                    if ui.button("Thaw everything").explain(help::THAW_ALL).clicked() {
                        self.client.send(Command::ThawAll);
                    }
                    if ui.button("Reload config").explain(help::RELOAD).clicked() {
                        self.client.send(Command::Reload);
                    }
                    // Opened against the path the daemon reported, not the
                    // default one: `dozed --config` exists, and editing a file
                    // the daemon is ignoring would be a maddening bug to hit.
                    if ui.button("Config").explain(help::CONFIGURE).clicked() {
                        self.config
                            .open(snapshot.status.as_ref().and_then(|s| s.config_path.as_deref()));
                    }
                    // Snooze needs no confirmation in either direction: it only
                    // ever makes the daemon do less, and it undoes itself.
                    //
                    // A menu rather than a button, for the reason the mode badge
                    // is one: "Snooze 1h" hardcoded the one length somebody might
                    // want, and the meeting that runs to ninety minutes had no
                    // answer but to click it twice.
                    if let Some(s) = &snapshot.status {
                        let label = match s.snooze_remaining {
                            Some(left) => format!("Snoozed — {} left", Coarse(left)),
                            None => "Snooze".to_owned(),
                        };
                        ui.menu_button(label, |ui| {
                            // First, and set apart, because it is the only entry
                            // here that is not a length: someone who opened this
                            // menu while snoozed most likely wants out of it.
                            if s.snooze_remaining.is_some() {
                                if ui.button("Wake now").explain(help::WAKE_NOW).clicked() {
                                    self.client.send(Command::Snooze(None));
                                    ui.close();
                                }
                                ui.separator();
                            }
                            for (secs, entry) in SNOOZE_OPTIONS {
                                if ui.button(entry).clicked() {
                                    // Picking a length while already snoozed
                                    // restarts the clock at that length rather
                                    // than adding to it, and the daemon keeps the
                                    // mode it was going to restore
                                    // (`Daemon::snooze`) — so a second pick
                                    // cannot strand you in dry-run.
                                    self.client.send(Command::Snooze(Some(
                                        std::time::Duration::from_secs(secs),
                                    )));
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .explain(match s.snooze_remaining {
                            Some(left) => help::snoozing(left),
                            None => help::SNOOZE.to_owned(),
                        });
                    }
                    // While a trial is running it, not the badge at the other
                    // end of this row, decides the mode — and the dashboard used
                    // to say nothing about that at all, so the mode appeared to
                    // move on its own. The daemon refuses a hand-set mode now,
                    // but a refusal the user could not have anticipated is still
                    // a surprise; this line is what stops it being one.
                    if let Some(t) = snapshot
                        .status
                        .as_ref()
                        .and_then(|s| s.trial.as_ref())
                        .filter(|t| t.blocks_done < t.blocks_total)
                    {
                        // One word each, for the same reason the focus warning is
                        // two: this sits in the button group, and every character
                        // here is taken from the status text at the other end of
                        // the row. "holding for battery" was mine and was too long
                        // the day it was written.
                        let state = if t.waiting {
                            "waiting".to_owned()
                        } else {
                            t.arm.clone().unwrap_or_else(|| "settling".to_owned())
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "trial {}/{} ({state})",
                                t.blocks_done, t.blocks_total
                            ))
                            .color(egui::Color32::from_rgb(150, 150, 150)),
                        )
                        .explain(help::TRIAL_OWNS_MODE);
                    }
                    // The mode control used to live here too, at the far end of a
                    // group laid out right-to-left from the window edge. It is on
                    // the mode badge now (see the header's left-hand side): this
                    // group grows leftwards into the status text, and the widest
                    // button in it decided how much of that text survived.
                });
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let connection = if snapshot.connected {
                    ui.colored_label(egui::Color32::from_rgb(120, 180, 120), "connected")
                } else {
                    ui.colored_label(egui::Color32::from_rgb(220, 120, 60), "no daemon")
                };
                connection.explain(help::connection(snapshot.connected));
                if let Some(s) = &snapshot.status {
                    ui.separator();
                    // A battery tool that cannot show its own cost is asking to
                    // be taken on faith (§13).
                    ui.label(format!("own cost {:.1} wakeups/s", s.self_wakeups_per_sec))
                        .explain(help::self_cost(s));
                    // A recorder that stopped is a fault and says so louder; a
                    // recorder that is running still says how much it has, because
                    // the size it reports goes *down* when it breaks and that is
                    // only legible against a number you have been watching.
                    if let Some(t) = &s.trace {
                        match &t.stopped {
                            // Present tense, and it clears itself: the daemon
                            // retries every minute, so this describes what is
                            // wrong now rather than what went wrong once.
                            Some(why) => {
                                ui.separator();
                                ui.label(
                                    egui::RichText::new(if t.retrying {
                                        format!("recording stopped: {why} (retrying)")
                                    } else {
                                        format!("recording stopped: {why}")
                                    })
                                    .color(egui::Color32::from_rgb(220, 120, 120)),
                                )
                                .explain(help::trace(t));
                            }
                            // A recorder that recovered is healthy and its file
                            // still has a hole. Two different facts, and only the
                            // first of them stops being true.
                            None if t.gaps > 0 => {
                                ui.separator();
                                ui.label(
                                    egui::RichText::new(format!(
                                        "recording {} events, {} gap{}",
                                        t.records,
                                        t.gaps,
                                        if t.gaps == 1 { "" } else { "s" }
                                    ))
                                    .color(egui::Color32::from_rgb(210, 170, 90)),
                                )
                                .explain(help::trace(t));
                            }
                            None => {
                                ui.separator();
                                ui.label(format!("recording {} events", t.records))
                                    .explain(help::trace(t));
                            }
                        }
                    }
                }
                if let Some(err) = &snapshot.error {
                    ui.separator();
                    // Elided to one line, with the whole of it on hover. This
                    // strip is a single row shared with the connection state and
                    // the wakeup cost, and the daemon's errors are written to be
                    // read rather than to fit — a config parse failure or a
                    // rejected aggression factor is a sentence or two. Laid out
                    // in full it pushed the row past the window and took the
                    // things either side of it off the edge, so the price of one
                    // long message was the rest of the footer.
                    ui.colored_label(egui::Color32::from_rgb(220, 120, 60), elide(err, 64))
                        .explain(format!(
                            "{err}\n\nThe last thing that went wrong on the connection to the \
                             daemon. It clears itself the moment the next reply arrives.",
                        ));
                }
            });
            ui.add_space(4.0);
        });

        egui::SidePanel::right("timeline")
            .default_width(340.0)
            .width_range(200.0..=700.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("Timeline").explain(help::timeline(client::Snapshot::MAX_JOURNAL));
                ui.label(
                    egui::RichText::new("every decision, newest last")
                        .small()
                        .color(egui::Color32::GRAY),
                )
                .explain(help::timeline(client::Snapshot::MAX_JOURNAL));
                ui.separator();
                egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    if snapshot.journal.is_empty() {
                        ui.label(
                            egui::RichText::new("nothing yet").italics().color(egui::Color32::GRAY),
                        )
                        .explain(
                            "Nothing has been decided since this window opened. On a quiet \
                             desktop that is the expected state — `dozectl journal` has the \
                             history from before it.",
                        );
                    }
                    // One line is all that fits, and it drops the derivation
                    // that makes the decision checkable: which rule, what the
                    // grace was made of, whether it actually landed.
                    for entry in &snapshot.journal {
                        ui.label(egui::RichText::new(entry.line()).small().monospace())
                            .explain(help::entry(entry));
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(report) = &snapshot.report {
                // The counterfactual is the argument for live mode (§11), so it
                // goes above the fold rather than behind a tab.
                egui::CollapsingHeader::new("What this would save")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(report.render(report.battery_wh)).monospace())
                            .explain(help::REPORT);
                    })
                    .header_response
                    .explain(help::REPORT);
                ui.add_space(8.0);
            }

            ui.heading("Applications").explain(help::APPLICATIONS);
            ui.separator();
            // The width the panel actually has, captured *before* entering the
            // scroll area below.
            //
            // Inside `ScrollArea::both` the available width is effectively
            // infinite — that is the point of a horizontal scroll area — so
            // `Label::wrap` there does nothing and a long line simply runs off
            // the edge. Right for the table, whose columns want scrolling; wrong
            // for the sentences under it. Measured on screen: the coverage footer
            // lost the words `0% of gpu` off the right-hand edge and the power
            // model's verdict was cut at "too little to". What is lost that way
            // is always the end of the sentence, which is where the qualifier is.
            let panel_width = ui.available_width();
            // Horizontal as well as vertical: six columns of a long app name,
            // a rate and a countdown do not fit a narrow window beside the
            // timeline, and a column that cannot be reached at all is worse
            // than one that has to be scrolled to.
            egui::ScrollArea::both().show(ui, |ui| {
                if snapshot.apps.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "no applications yet — is the compositor helper installed?",
                        )
                        .italics()
                        .color(egui::Color32::GRAY),
                    )
                    .explain(
                        "dozed sees applications through the compositor. On Wayland that \
                         needs the helper for your desktop installed and enabled; on X11 it \
                         is in-process and an empty list here means something else is wrong.",
                    );
                }
                // A grid rather than a right-to-left layout: with the latter the
                // label, visibility and exemption columns drew on top of each
                // other as soon as an app name was long.
                egui::Grid::new("apps").num_columns(7).striped(true).spacing([12.0, 4.0]).show(
                    ui,
                    |ui| {
                        // Whether what this row shows is actually being done to
                        // the app: dry-run globally, or `monitor` for this one,
                        // and the state column reads identically either way.
                        let live = snapshot.status.as_ref().is_some_and(|s| s.mode.acts());
                        for app in &snapshot.apps {
                            let key = app.key.to_string();
                            let acting = live && app.mode.acts();
                            let text_colour = ui.visuals().text_color();
                            ui.label(
                                egui::RichText::new(state_word(app))
                                    .monospace()
                                    .color(state_colour_for(app, acting, text_colour)),
                            )
                            .explain(help::state_of_app(app, acting));

                            let selected = self.selected.as_deref() == Some(&key);
                            if ui
                                .selectable_label(selected, &app.label)
                                .explain(help::app_row(app))
                                .clicked()
                            {
                                self.selected = if selected { None } else { Some(key.clone()) };
                            }

                            ui.label(
                                egui::RichText::new(app.visibility.as_str())
                                    .small()
                                    .color(egui::Color32::GRAY),
                            )
                            .explain(help::visibility(app.visibility));
                            // An app with no measurement yet says so. A rate
                            // needs two readings a minute apart, so a fresh
                            // daemon has none — and a column of `0/s 0.0%`
                            // across every row reads as a broken tool rather
                            // than a young one.
                            // The rates, and the estimated watts with them.
                            //
                            // One cell rather than two: they answer the same
                            // question — "how do I know" — and the watts are the
                            // rates divided by something, not an independent
                            // reading. Absent where there is no measured total to
                            // apportion, which on a stock kernel is everywhere
                            // (`dozed_core::attribution`).
                            //
                            // An app with no measurement yet says so with a dash.
                            // A rate needs two readings a minute apart, so a fresh
                            // daemon has none — and a column of `0/s 0.0%` across
                            // every row reads as a broken tool rather than a young
                            // one.
                            let watts = snapshot.machine.basis.filter(|_| app.sampled).map_or_else(
                                String::new,
                                |b| {
                                    b.watts(app.sample.cpu_pct, app.sample.wakeups_per_sec)
                                        .map_or_else(|| " ~—W".to_owned(), |w| format!(" ~{w:.2}W"))
                                },
                            );
                            ui.label(
                                egui::RichText::new(if app.sampled {
                                    // GPU only where this app holds a DRM
                                    // client, which is most of the interesting
                                    // ones and none of the rest. A measurement,
                                    // unlike the watts beside it, so it is not
                                    // marked with a tilde.
                                    let gpu = app
                                        .sample
                                        .gpu_pct
                                        .map_or_else(String::new, |p| format!(" {p:>5.1}%gpu"));
                                    format!(
                                        "{:>5.0}/s {:>5.1}%{gpu}{watts}",
                                        app.sample.wakeups_per_sec, app.sample.cpu_pct
                                    )
                                } else {
                                    format!("{:>5}/s {:>5}%", "—", "—")
                                })
                                .small()
                                .monospace(),
                            )
                            .explain(help::measurement(
                                app,
                                snapshot.machine.sample_window,
                                snapshot.machine.basis,
                            ));
                            // A hand-set limit, in its own cell beside them. It
                            // used to share this column with the rates and be
                            // swapped for them, which meant the one row somebody
                            // had deliberately made different was the row whose
                            // reason vanished the moment they went looking for the
                            // numbers behind it. A dash where there is none, so
                            // that "nothing is holding this app back" is something
                            // the column can be asked about too.
                            ui.label(
                                egui::RichText::new(
                                    app.held_off.clone().unwrap_or_else(|| "—".to_owned()),
                                )
                                .small()
                                .color(egui::Color32::from_rgb(150, 150, 110)),
                            )
                            .explain(help::held_off(app));
                            // Abbreviated: the full names are in the Exemption
                            // sources section below, and spelling them out here
                            // pushed the column off the edge of the panel. A row
                            // with no exemption gets a dash rather than nothing,
                            // so that "nothing is holding this app back" is
                            // something the column can be asked about too.
                            //
                            // The colour is keyed on whether anything here
                            // actually *holds* the app, not on whether the cell
                            // is empty. `audio-silent` is reported in this
                            // column and exempts nothing — it is the reason
                            // `audio` is absent — so drawing it in the exempt
                            // colour would say the opposite of what it means.
                            ui.label(
                                egui::RichText::new(if app.exempt.is_empty() {
                                    "—".to_owned()
                                } else {
                                    abbreviate(&app.exempt)
                                })
                                .small()
                                .color(
                                    if app.exempt.iter().any(|e| help::exempting(e)) {
                                        egui::Color32::from_rgb(120, 160, 200)
                                    } else {
                                        egui::Color32::GRAY
                                    },
                                ),
                            )
                            .explain(help::exempt(app));
                            // When the planner next intends to act, and what it
                            // intends. The daemon's own deadline rather than a
                            // grace product recomputed here, so the countdown
                            // cannot promise something it will not do — and a
                            // duration rather than an instant, because the
                            // deadline is on the daemon's monotonic clock.
                            ui.label(
                                egui::RichText::new(
                                    app.pending
                                        .as_ref()
                                        .map_or_else(|| "—".to_owned(), PendingChange::summary),
                                )
                                .small()
                                .color(egui::Color32::GRAY),
                            )
                            .explain(help::pending(app.pending.as_ref(), acting));
                            ui.end_row();

                            if selected {
                                ui.label("");
                                ui.horizontal(|ui| {
                                    if ui.button("Thaw").explain(help::thaw(acting)).clicked() {
                                        self.client.send(Command::Thaw(key.clone()));
                                    }
                                    if ui.button("Freeze").explain(help::freeze(acting)).clicked() {
                                        self.client.send(Command::Freeze(key.clone()));
                                    }
                                    if ui
                                        .button("Throttle 5%")
                                        .explain(help::throttle(5, acting))
                                        .clicked()
                                    {
                                        self.client.send(Command::Throttle(key.clone(), 5));
                                    }
                                    // What follows is a different kind of action
                                    // from the three above: those act once and the
                                    // rules take the app back on the next tick,
                                    // these change what the rules are allowed to do
                                    // to it until they wear off.
                                    //
                                    // They used to be a grid cell of their own with
                                    // an empty cell in front, which put a whole
                                    // column of gap between "Throttle 5%" and
                                    // "Leave alone 1h" — wide enough that the two
                                    // groups read as unrelated rather than as one
                                    // row of things you can do to this app. The
                                    // distinction is in the hover text, where it
                                    // can be stated rather than implied by
                                    // whitespace.
                                    if app.held_off.is_some() {
                                        if ui
                                            .button("Give it back to the rules")
                                            .explain(help::UNHOLD)
                                            .clicked()
                                        {
                                            self.client.send(Command::SetHold(
                                                key.clone(),
                                                None,
                                                None,
                                            ));
                                        }
                                    } else {
                                        for (label, hours) in
                                            [("Leave alone 1h", 1u64), ("Leave alone 4h", 4)]
                                        {
                                            if ui.button(label).explain(help::ease(hours)).clicked()
                                            {
                                                self.client.send(Command::SetHold(
                                                    key.clone(),
                                                    Some(dozed_core::Ceiling::LeaveAlone),
                                                    Some(std::time::Duration::from_secs(
                                                        hours * 3600,
                                                    )),
                                                ));
                                            }
                                        }
                                        if ui
                                            .button("Cap, do not freeze (1h)")
                                            .explain(help::CAP_ONLY)
                                            .clicked()
                                        {
                                            self.client.send(Command::SetHold(
                                                key.clone(),
                                                Some(dozed_core::Ceiling::CapOnly { pct: None }),
                                                Some(std::time::Duration::from_secs(3600)),
                                            ));
                                        }
                                    }
                                });
                                ui.label(
                                    egui::RichText::new(format!("{} pids", app.pids))
                                        .small()
                                        .color(egui::Color32::GRAY),
                                )
                                .explain(help::pids(app.pids));
                                ui.label(
                                    egui::RichText::new(format!("mode {}", app.mode.as_str()))
                                        .small()
                                        .color(egui::Color32::GRAY),
                                )
                                .explain(help::app_mode(app.mode));
                                ui.label("");
                                ui.label("");
                                ui.end_row();

                                // Which process is actually responsible. An app
                                // is a tree, and the total on its row says
                                // nothing about where inside that tree the
                                // wakeups are coming from — the question every
                                // unexpected number raises.
                                for proc in &app.top_processes {
                                    ui.label("");
                                    ui.label(
                                        egui::RichText::new(if proc.role.is_empty() {
                                            format!("  {}", proc.comm)
                                        } else {
                                            // `comm` is identical across an
                                            // Electron app's processes, so the
                                            // role is the only thing that tells
                                            // these rows apart.
                                            format!("  {} [{}]", proc.comm, proc.role)
                                        })
                                        .small()
                                        .monospace(),
                                    )
                                    .explain(help::process(proc));
                                    ui.label(
                                        egui::RichText::new(format!("pid {}", proc.pid))
                                            .small()
                                            .color(egui::Color32::GRAY),
                                    )
                                    .explain(help::process(proc));
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:>5.0}/s",
                                            proc.wakeups_per_sec
                                        ))
                                        .small()
                                        .monospace(),
                                    )
                                    .explain(help::process(proc));
                                    ui.label("");
                                    ui.label("");
                                    ui.end_row();
                                }
                            }
                        }
                    },
                );

                // The scale every row above is read against: 900/s means one
                // thing on a machine doing 1200 and another on one doing
                // 40,000. It also moves live — a frozen app samples at ~0, so
                // this is the one place holding an app is visible as it
                // happens rather than in the report afterwards.
                //
                // The note is a tooltip rather than a second line: it changes
                // wording in the case where the rows lead the machine figure,
                // and a footer that reflows on a two-second poll is worse than
                // one nobody has to read twice.
                if !snapshot.apps.is_empty() {
                    let coverage = Coverage::of(&snapshot.apps, snapshot.machine);
                    ui.add_space(4.0);
                    ui.scope(|ui| {
                        ui.set_max_width(panel_width);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(coverage.summary())
                                    .small()
                                    .monospace()
                                    .color(egui::Color32::GRAY),
                            )
                            .wrap(),
                        )
                        .explain(help::coverage(coverage.note(), snapshot.machine.basis));
                    });

                    // What this machine's watts actually respond to, fitted to
                    // its own counters — and, as often, why no fit is on offer.
                    // Directly under the coverage line because it is the sentence
                    // that says how to read the `~W` column above it.
                    //
                    // A power cap is drawn in the warning colour rather than the
                    // grey the other verdicts get. It is not a shortfall in the
                    // model, it is a fact about the machine that inverts what the
                    // column means: at the cap, holding an app frees budget for
                    // another rather than saving power, and a caveat that changes
                    // the sign of the answer must not look like a footnote.
                    if let Some(v) = snapshot
                        .status
                        .as_ref()
                        .and_then(|s| s.energy.as_ref())
                        .and_then(|e| e.power_model.as_ref())
                    {
                        let capped =
                            matches!(v, dozed_core::power_model::Verdict::PowerLimited { .. });
                        ui.scope(|ui| {
                            ui.set_max_width(panel_width);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(v.summary()).small().monospace().color(
                                        if capped {
                                            egui::Color32::from_rgb(220, 150, 90)
                                        } else {
                                            egui::Color32::GRAY
                                        },
                                    ),
                                )
                                .wrap(),
                            )
                            .explain(help::power_model(v));
                        });
                    }
                }

                // Inline rather than a floating window, which opened on top of
                // the report and covered it.
                // Always, never behind a control. §10 was written about a rule
                // matching on state nothing ever set: it could never fire, and
                // that was silent until it was surfaced. What each source *can*
                // see is the same question as whether it is broken, asked one
                // reading earlier.
                if let Some(status) = &snapshot.status
                    && !status.exemption_sources.is_empty()
                {
                    ui.add_space(12.0);
                    egui::CollapsingHeader::new("Exemption sources")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("exemptions").num_columns(2).spacing([12.0, 2.0]).show(
                                ui,
                                |ui| {
                                    for e in &status.exemption_sources {
                                        // Both cells hover the same text: the
                                        // name says what is watched and the
                                        // state says whether it can fire, and
                                        // neither is readable without the other.
                                        let about = help::exemption_source(e);
                                        ui.label(egui::RichText::new(&e.name).monospace())
                                            .explain(&about);
                                        match (&e.unavailable, &e.degraded, e.reported) {
                                            (Some(why), _, _) => {
                                                ui.colored_label(egui::Color32::GRAY, why)
                                                    .explain(&about);
                                            }
                                            // Amber rather than grey: unlike
                                            // "unavailable here", this one is
                                            // something the user can fix, and a
                                            // colour that reads as inert is how
                                            // a fixable fault goes unfixed.
                                            (None, Some(why), _) => {
                                                ui.colored_label(
                                                    egui::Color32::from_rgb(220, 170, 60),
                                                    format!("degraded — {why}"),
                                                )
                                                .explain(&about);
                                            }
                                            (None, None, false) => {
                                                // A rule matching on state nobody
                                                // sets can never fire, and that
                                                // failure was silent until it was
                                                // surfaced.
                                                ui.colored_label(
                                                    egui::Color32::from_rgb(220, 120, 60),
                                                    "NOT REPORTING — this rule cannot fire",
                                                )
                                                .explain(&about);
                                            }
                                            (None, None, true) => {
                                                ui.label(format!("ok, {} process(es)", e.pids))
                                                    .explain(&about);
                                            }
                                        }
                                        ui.end_row();
                                    }
                                },
                            );
                            if let Some(src) = &status.focus_source {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(src).small().color(egui::Color32::GRAY),
                                )
                                .explain(help::FOCUS_SOURCE);
                            }
                            ui.label(
                                egui::RichText::new(format!("uptime {}", Coarse(status.uptime)))
                                    .small()
                                    .color(egui::Color32::GRAY),
                            )
                            .explain(help::UPTIME);
                        })
                        .header_response
                        .explain(help::EXEMPTION_SOURCES);
                }
            });
        });

        // Drawn last so it floats above the dashboard, and outside the borrow
        // of `snapshot`: saving writes the file and the daemon is told to pick
        // it up, which is the same path the "Reload config" button takes.
        if self.config.open && self.config.show(ctx) {
            self.client.send(Command::Reload);
        }
    }
}

/// Short badges for the app table. `dbus-name-owner` spelled out is wider than
/// the column it lives in, and the full list is one hover away.
///
/// `audio-silent` becomes `silent` rather than being hidden: an app holding an
/// audio stream and making no sound with it is the one thing in this column a
/// user is most likely to be surprised by, and "why is my browser dozing when
/// it has a call tab open" has to be answerable from the table rather than only
/// from the tooltip.
fn abbreviate(exempt: &[String]) -> String {
    exempt
        .iter()
        .map(|e| match e.as_str() {
            "dbus-name-owner" => "dbus",
            "clipboard-owner" => "clip",
            "long-running-job" => "job",
            "audio-silent" => "silent",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn state_of(action: Action) -> &'static str {
    match action {
        Action::Restore => "  running  ",
        Action::Throttle { .. } => "  throttled",
        Action::Freeze { .. } => "  dozing   ",
    }
}

/// The state word for a row, which is not always the applied action.
///
/// An app dozed may never touch carries `applied = Restore` like any untouched
/// app, so this column said **running** — and a user who saw `plasmashell` in
/// this window had no way to tell that from a browser about to be frozen. It was
/// asked about, which is the measurement that matters: the one fact they needed
/// was the one fact not on screen (`ipc::AppSummary::never_actionable`).
fn state_word(app: &dozed_core::ipc::AppSummary) -> &'static str {
    if app.never_actionable { " protected " } else { state_of(app.applied) }
}

/// The colour of the state column for one row.
///
/// `protected` takes the theme's own text colour at reduced strength rather than
/// a hue of its own. It is not a state on the ladder — nothing is being done and
/// nothing ever will be — so giving it a fourth colour would put it in the same
/// visual language as three states that mean the daemon is deciding something.
/// Quiet is the correct claim, and it is also the right instruction to the eye:
/// this is the row to skip.
fn state_colour_for(
    app: &dozed_core::ipc::AppSummary,
    acting: bool,
    text: egui::Color32,
) -> egui::Color32 {
    if app.never_actionable {
        let t = text.to_array();
        let muted = |i: usize| ((u16::from(t[i]) * 55) / 100) as u8;
        return egui::Color32::from_rgb(muted(0), muted(1), muted(2));
    }
    state_colour(app.applied, acting, text)
}

/// The colour of an action: one hue per state, muted in dry-run.
///
/// The point is scanning. A table of thirty rows where every state word is the
/// same colour has to be read; one where "dozing" is blue and "throttled" is
/// amber can be glanced at, and the question people actually bring to this
/// window — *what is it doing to my machine right now* — is a glance question.
///
/// **Colour is never the only signal.** The word is there in every row and says
/// the same thing, so a reader who cannot separate these hues loses nothing but
/// speed. That is also why the hues are not red and green.
///
/// **Dry-run gets the same hue at reduced strength**, which is this file's
/// standing rule applied to colour: half of what the dashboard shows is a
/// counterfactual there, and a window that renders it identically to the real
/// thing owes the reader that distinction wherever it appears. A full-strength
/// blue "dozing" on a daemon that has frozen nothing is a stronger claim than
/// the word alone ever made.
///
/// Blended toward the theme's own text colour rather than toward a fixed grey,
/// and rather than made translucent. Two reasons, both found by rendering it:
///
/// * A fixed mid-grey is the wrong direction on half the themes. egui draws text
///   at `gray(140)` on dark and `gray(80)` on light, so "muted" has to mean
///   *lighter* on one and *darker* on the other — blending toward grey washed
///   the dry-run colours out to near-invisible on a light background, which is
///   the common case twice over, dry-run being the default mode.
/// * The rows are striped, so an alpha blend has two different backgrounds to
///   land on and one state would come out two colours down the column.
fn state_colour(action: Action, acting: bool, text: egui::Color32) -> egui::Color32 {
    let (r, g, b) = match action {
        // The same green as the "connected" marker in the footer, so the window
        // does not use two greens for "this is fine".
        Action::Restore => (120, 180, 120),
        // The same amber as a degraded exemption source, for the same reason.
        Action::Throttle { .. } => (220, 170, 60),
        // The blue of the tray snowflake: one idea, one colour, in both places
        // a user sees dozed holding something.
        Action::Freeze { .. } => (90, 160, 220),
    };
    // Even the live colours are pulled a little toward the theme, so a saturated
    // word does not sit on the page looking like it belongs to another program.
    let toward_text = if acting { 15 } else { 45 };
    let t = text.to_array();
    let blend = |c: u8, i: usize| {
        ((u16::from(c) * (100 - toward_text) + u16::from(t[i]) * toward_text) / 100) as u8
    };
    egui::Color32::from_rgb(blend(r, 0), blend(g, 1), blend(b, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [Action; 3] =
        [Action::Restore, Action::Throttle { pct: 5 }, Action::Freeze { reclaim: false }];

    /// The exemption column has to distinguish "something is holding this app"
    /// from "here is why nothing is".
    ///
    /// `audio-silent` is reported in the same list as the real exemptions and
    /// means the opposite of them, so the badge is visible and the colour that
    /// says *exempt* is reserved for the ones that are.
    #[test]
    fn a_silent_stream_is_shown_but_is_not_drawn_as_an_exemption() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        let silent = names(&["audio-silent"]);
        assert_eq!(abbreviate(&silent), "silent");
        assert!(
            !silent.iter().any(|e| help::exempting(e)),
            "a silent stream holds nothing, so the column must not read as exempt"
        );

        // A browser doing both at once: the exemption decides the colour, and
        // the observation still gets its badge.
        let both = names(&["audio-silent", "dbus-name-owner"]);
        assert_eq!(abbreviate(&both), "silent,dbus");
        assert!(both.iter().any(|e| help::exempting(e)));

        // A name this build has never heard of is assumed to hold, which is the
        // reading that cannot suggest dozed is free to act when it is not.
        assert!(names(&["something-invented"]).iter().any(|e| help::exempting(e)));
    }

    /// The footer is one row, and the daemon's errors are written to be read.
    #[test]
    fn a_long_error_is_elided_rather_than_pushing_the_footer_off_screen() {
        assert_eq!(elide("short", 64), "short");
        // Exactly at the limit is not "longer than", so it keeps its last
        // character and gains no ellipsis.
        assert_eq!(elide("abcde", 5), "abcde");
        assert_eq!(elide("abcdef", 5), "abcde…");

        let real = "a trial is driving the mode; `dozectl trial stop` first — finished blocks \
                    are kept";
        assert!(elide(real, 64).chars().count() <= 65, "one row's worth, plus the ellipsis");

        // Byte indexing here would panic rather than truncate: the cut for
        // `max = 3` falls inside a two-byte character.
        assert_eq!(elide("héllo", 3), "hél…");
        assert_eq!(elide("日本語です", 2), "日本…");
    }

    /// egui's own text colours, which are what the blend targets.
    const DARK_TEXT: egui::Color32 = egui::Color32::from_gray(140);
    const LIGHT_TEXT: egui::Color32 = egui::Color32::from_gray(80);
    /// And roughly the panel behind them.
    const DARK_BG: u8 = 27;
    const LIGHT_BG: u8 = 248;

    /// How far apart two colours are, summed across the channels. Crude on
    /// purpose: the question is "could someone tell these apart at a glance in a
    /// table", not "are these perceptually N units distant".
    fn distance(a: egui::Color32, b: egui::Color32) -> u32 {
        [0, 1, 2].iter().map(|&i| u32::from(a.to_array()[i].abs_diff(b.to_array()[i]))).sum()
    }

    /// Rough perceived brightness, for contrast against a background.
    fn luma(c: egui::Color32) -> f32 {
        let [r, g, b, _] = c.to_array();
        0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
    }

    #[test]
    fn every_state_is_a_different_colour() {
        // The whole point of the column. Two states sharing a hue would make the
        // table look colour-coded while coding nothing.
        for text in [DARK_TEXT, LIGHT_TEXT] {
            for (i, a) in STATES.iter().enumerate() {
                for b in &STATES[i + 1..] {
                    let (ca, cb) = (state_colour(*a, true, text), state_colour(*b, true, text));
                    assert!(distance(ca, cb) > 80, "{ca:?} and {cb:?} too close: {a:?} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn dry_run_is_never_the_colour_live_would_be() {
        // This file's standing rule, applied to colour: half of what the
        // dashboard shows is a counterfactual in dry-run, and a full-strength
        // "dozing" on a daemon that has frozen nothing is a stronger claim than
        // the word alone ever made.
        for text in [DARK_TEXT, LIGHT_TEXT] {
            for state in STATES {
                assert_ne!(
                    state_colour(state, true, text),
                    state_colour(state, false, text),
                    "{state:?} looks identical in both modes"
                );
            }
        }
    }

    #[test]
    fn the_muted_colours_are_still_told_apart() {
        // Dry-run is the *common* case — it is the default mode — so muting must
        // not collapse the coding into three shades nobody can read.
        for text in [DARK_TEXT, LIGHT_TEXT] {
            for (i, a) in STATES.iter().enumerate() {
                for b in &STATES[i + 1..] {
                    let (ca, cb) = (state_colour(*a, false, text), state_colour(*b, false, text));
                    assert!(distance(ca, cb) > 40, "muted {ca:?} and {cb:?} are too close");
                }
            }
        }
    }

    #[test]
    fn every_colour_is_legible_on_the_theme_it_was_mixed_for() {
        // The bug this exists for: blending toward a fixed mid-grey made the
        // dry-run colours *lighter* on a light theme, which is the wrong
        // direction, and they came out near-invisible on the background they
        // are actually drawn on. Caught by rendering them, so now it is caught
        // by a test instead.
        for (text, bg) in [(DARK_TEXT, DARK_BG), (LIGHT_TEXT, LIGHT_BG)] {
            let background = luma(egui::Color32::from_gray(bg));
            for state in STATES {
                for acting in [true, false] {
                    let c = state_colour(state, acting, text);
                    let contrast = (luma(c) - background).abs();
                    assert!(
                        contrast > 45.0,
                        "{state:?} (acting={acting}) is {c:?} on gray({bg}): contrast {contrast:.0}"
                    );
                }
            }
        }
    }

    #[test]
    fn muting_keeps_the_hue_rather_than_picking_a_new_one() {
        // A user should learn one colour per state, not two. Every channel moves
        // toward the same target, so the dry-run colour is recognisably the live
        // one and never overshoots past it.
        for text in [DARK_TEXT, LIGHT_TEXT] {
            let target = i32::from(text.to_array()[0]);
            for state in STATES {
                let live = state_colour(state, true, text).to_array();
                let dry = state_colour(state, false, text).to_array();
                for i in 0..3 {
                    let (l, d) = (i32::from(live[i]), i32::from(dry[i]));
                    assert!(
                        (d - target).abs() <= (l - target).abs(),
                        "channel {i} of {state:?} moved away from the text colour"
                    );
                }
            }
        }
    }

    /// The header badge is the only global mode control, so what it offers is
    /// the whole of what a user can pick — and `Monitor` must not be in it.
    ///
    /// Monitor is a per-app mode: "record for this one while the rest run live".
    /// Globally it would do precisely what dry-run does while reading as
    /// something else, and the app rows below already offer it where it means
    /// something. Worth a test rather than a comment because the list is three
    /// lines away from an enum with three variants, and the obvious "tidy-up"
    /// is to complete the set.
    #[test]
    fn monitor_is_not_offered_as_a_global_mode() {
        let offered: Vec<Mode> = GLOBAL_MODES.iter().map(|(m, _, _)| *m).collect();
        assert_eq!(offered, [Mode::DryRun, Mode::Live]);
        assert!(!offered.contains(&Mode::Monitor));
    }

    /// Each entry names the mode, not the move.
    ///
    /// "Go live" and "Back to dry-run" were fine as buttons, because a button
    /// only ever means "do this". Side by side in a list, a verb reads as a
    /// description of where you already are, and the wrong one gets clicked.
    #[test]
    fn each_mode_entry_names_the_mode_rather_than_the_move() {
        for (mode, entry, hover) in GLOBAL_MODES {
            assert!(
                entry.to_lowercase().starts_with(mode.as_str()),
                "{entry:?} should open with the mode it selects"
            );
            assert!(entry.contains('—'), "{entry:?} should say what the mode does");
            assert!(!hover.is_empty(), "every entry explains itself on hover");
        }
    }

    /// A snooze is the control you reach for *instead* of switching dozed off,
    /// because switching it off is permanent and nobody remembers to switch it
    /// back on. One long enough to forget about is that same failure wearing a
    /// timer, so the longest offered stays within a working day's attention.
    #[test]
    fn snooze_lengths_are_ordered_shortest_first_and_stay_short_enough_to_remember() {
        let secs: Vec<u64> = SNOOZE_OPTIONS.iter().map(|(s, _)| *s).collect();
        let mut sorted = secs.clone();
        sorted.sort_unstable();
        assert_eq!(secs, sorted, "a menu whose lengths jump around is read wrong");
        assert!(secs.iter().all(|s| *s >= 60), "a snooze shorter than a minute is a mis-click");
        assert!(
            *secs.last().unwrap() <= 4 * 60 * 60,
            "past a few hours, 'off' is the honest control and this one is a trap"
        );
        // Every label has to name its own length: this menu has no other
        // context, and "1" beside "2" beside "4" is three different questions.
        for (_, entry) in SNOOZE_OPTIONS {
            assert!(
                entry.contains("minute") || entry.contains("hour"),
                "{entry:?} should say what unit it is"
            );
        }
    }
}
