//! A tray indicator: what dozed is holding, and the two buttons for it.
//!
//! A daemon that can stop your applications should be visible without being
//! asked. Until this existed the only way to see what dozed was doing was to run
//! `dozectl status` or open the dashboard — which is fine when you already
//! suspect it, and no use at all for the case that matters: an app behaving
//! oddly and no reason to connect that to a background service you installed a
//! fortnight ago. The panic thaw §10.5 requires was equally two steps away.
//!
//! ## It is a client, not a second control path
//!
//! Every menu item sends an ordinary [`Request`] down the same channel the unix
//! socket uses, so the tray can do exactly what `dozectl` can do and nothing
//! more, through exactly the same code. There is no tray-only path into the
//! daemon that could behave differently from the documented one — which also
//! means every click a user makes here is journalled and traced like any other.
//!
//! ## What it costs
//!
//! Nothing on an idle desktop. The indicator is redrawn only when the state it
//! shows actually changes, which is what the `watch` channel is for: the daemon
//! writes its state after every tick, and `send_if_modified` means an unchanged
//! state wakes nobody. §13 applies to a status icon exactly as it does to a
//! sampler.

use std::time::Duration;

use ksni::TrayMethods as _;
use tokio::sync::{mpsc, oneshot, watch};

use dozed_core::ipc::Request;
use dozed_core::{HumanDuration, Mode};

use crate::ipcserver::Command;

/// Everything the indicator draws, and nothing else.
///
/// Deliberately not a reference to the daemon: the tray runs in its own task and
/// the daemon is not `Sync`. Equality is what suppresses redraws, so every field
/// here should be something a user would notice changing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrayState {
    pub mode: Mode,
    pub held: usize,
    pub known: usize,
    /// How long is left of a snooze, if one is running.
    pub snoozed: Option<Duration>,
    /// Whether this daemon has a live actuator at all. Without one, offering
    /// "go live" would be offering something that cannot happen.
    pub can_act: bool,
    /// `ui.tray` from the config, so `dozectl reload` can take the icon away
    /// and put it back without a restart. Carried in the state rather than read
    /// once at startup because a setting that needs a restart to apply is one
    /// people conclude is broken.
    pub enabled: bool,
}

impl TrayState {
    /// The one-line summary, used for the tooltip and the disabled first item.
    fn summary(&self) -> String {
        if let Some(left) = self.snoozed {
            return format!("snoozed, {} left", HumanDuration(left));
        }
        match (self.mode, self.held) {
            (Mode::Live, 0) => format!("live; nothing held, {} apps watched", self.known),
            (Mode::Live, n) => format!("live; {n} of {} apps held", self.known),
            // In dry-run the count is of what it *would* be holding, and saying
            // "held" there would be a straightforward lie about the state of the
            // machine.
            (_, 0) => format!("dry-run; would hold nothing, {} apps watched", self.known),
            (_, n) => format!("dry-run; would be holding {n} of {}", self.known),
        }
    }
}

struct DozedTray {
    state: TrayState,
    commands: mpsc::Sender<Command>,
}

impl DozedTray {
    /// Fire and forget, because a menu callback cannot await.
    ///
    /// The reply is dropped on purpose: the tray asks for things it cannot fail
    /// to be allowed to do, and the result arrives as a state change through the
    /// watch channel anyway. A full channel means the daemon is busy and will be
    /// asked again on the next click, which is better than blocking the menu.
    fn send(&self, request: Request) {
        let (reply, _ignored) = oneshot::channel();
        if self.commands.try_send(Command { requests: vec![request], reply }).is_err() {
            tracing::warn!("the daemon did not take a tray command");
        }
    }

    /// Which of the three icons this state calls for.
    ///
    /// Split from the drawing so the choice can be tested without rasterising
    /// anything, and so the one rule that matters is stated once: **dry-run
    /// holds nothing**, whatever counterfactual count the planner is keeping. An
    /// icon saying otherwise would be §11's lie, in the panel.
    fn icon_state(&self) -> crate::icon::State {
        if self.state.snoozed.is_some() {
            crate::icon::State::Snoozed
        } else if self.state.mode.acts() && self.state.held > 0 {
            crate::icon::State::Holding
        } else {
            crate::icon::State::Idle
        }
    }
}

impl ksni::Tray for DozedTray {
    /// Left click opens the menu.
    ///
    /// Without this, ksni's default routes a left click to `Tray::activate`,
    /// which this item does not implement — so the icon appeared to respond to
    /// nothing but a middle click, and the menu holding the panic thaw could
    /// only be reached by knowing to right-click. Everything this indicator does
    /// is in the menu, so every button that opens the menu is the correct
    /// behaviour.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "dozed".into()
    }

    fn title(&self) -> String {
        "dozed".into()
    }

    /// A snowflake, in one of three weights, sent as pixels.
    ///
    /// Deliberately *not* `icon_name`, which is what the first version did and
    /// why the tray came up empty. The names resolved perfectly well when asked
    /// directly — `Gtk.IconTheme` finds all three — but a name is only as good
    /// as the host's icon cache at the moment it looks up, and a panel already
    /// running when the installer wrote the files may never see them. A name
    /// that fails to resolve is not a fallback to anything; it is a blank space
    /// where the icon should be, which reads as a broken daemon.
    ///
    /// Pixels have nothing to find and nothing to cache: they travel in the
    /// D-Bus message. Several sizes are offered so a high-DPI panel gets a sharp
    /// one, and [`crate::icon`] draws rather than decodes, so any size the panel
    /// prefers is exact rather than scaled.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        crate::icon::pixmaps(self.icon_state())
    }

    fn status(&self) -> ksni::Status {
        // `Passive` hides the icon on most desktops. Never used: a daemon that
        // is holding nothing right now is still a daemon that will, and an icon
        // that comes and go is one nobody learns the meaning of.
        ksni::Status::Active
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "dozed".into(),
            description: self.state.summary(),
            // Empty: the item's own icon is already a pixmap, and repeating it
            // here would double every message for a picture the host draws in
            // the same place.
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{StandardItem, SubMenu};

        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem { label: self.state.summary(), enabled: false, ..Default::default() }
                .into(),
            ksni::MenuItem::Separator,
            // First and always available, per §10.5. Ignores rules, ignores
            // mode, and needs no submenu between it and the pointer.
            StandardItem {
                label: "Thaw everything".into(),
                activate: Box::new(|this: &mut Self| this.send(Request::ThawAll)),
                ..Default::default()
            }
            .into(),
        ];

        items.push(match self.state.snoozed {
            Some(left) => StandardItem {
                label: format!("Wake now ({} left)", HumanDuration(left)),
                activate: Box::new(|this: &mut Self| this.send(Request::Snooze { duration: None })),
                ..Default::default()
            }
            .into(),
            None => SubMenu {
                label: "Snooze".into(),
                submenu: [15u64, 60, 180]
                    .into_iter()
                    .map(|minutes| {
                        StandardItem {
                            label: format!("{}", HumanDuration(Duration::from_secs(minutes * 60))),
                            activate: Box::new(move |this: &mut Self| {
                                this.send(Request::Snooze {
                                    duration: Some(Duration::from_secs(minutes * 60)),
                                });
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
        });

        items.push(ksni::MenuItem::Separator);

        // Going live is offered only where it can happen. On a daemon started
        // without `--live` the item would be a button that silently does
        // nothing, which is worse than no button.
        if self.state.can_act {
            items.push(match self.state.mode {
                Mode::Live => StandardItem {
                    label: "Back to dry-run".into(),
                    activate: Box::new(|this: &mut Self| {
                        this.send(Request::SetMode { mode: Mode::DryRun });
                    }),
                    ..Default::default()
                }
                .into(),
                _ => StandardItem {
                    label: "Go live".into(),
                    activate: Box::new(|this: &mut Self| {
                        this.send(Request::SetMode { mode: Mode::Live });
                    }),
                    ..Default::default()
                }
                .into(),
            });
        }

        items.push(
            StandardItem {
                label: "Open dashboard".into(),
                activate: Box::new(|_: &mut Self| open_dashboard()),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Where `dozed-gui` is, given where this daemon is.
///
/// **Beside the running binary first, `PATH` only as a fallback.** Trusting
/// `PATH` alone was wrong and failed in the ordinary case: `install.sh` puts
/// both binaries in `~/.local/bin`, and a `systemd --user` service does not
/// inherit a login shell's `PATH` — the unit gets whatever systemd's own
/// environment has, which on most distributions is `/usr/local/bin:/usr/bin`
/// and nothing under `$HOME`. So a correctly installed pair produced `No such
/// file or directory` on every click.
///
/// The two binaries ship together and are installed together, so the daemon's
/// own directory is the strongest evidence available about where its dashboard
/// is — stronger than a search path that was never meant to describe this
/// service.
#[must_use]
fn dashboard_path() -> std::path::PathBuf {
    dashboard_beside(std::env::current_exe().ok().as_deref())
}

/// The resolution itself, taking the daemon's own path so it can be tested.
#[must_use]
fn dashboard_beside(exe: Option<&std::path::Path>) -> std::path::PathBuf {
    let beside = exe
        .and_then(std::path::Path::parent)
        .map(|dir| dir.join("dozed-gui"))
        .filter(|p| p.is_file());
    // A bare name is resolved against `PATH` by `Command`, which is the right
    // fallback for a distribution package that splits the two up.
    beside.unwrap_or_else(|| std::path::PathBuf::from("dozed-gui"))
}

/// Launch the dashboard, and say so where a user will see it when it fails.
///
/// The daemon's rule for external programs applies here too: anything it cannot
/// run, it reports rather than swallowing. There is nowhere to put a status
/// field for this one — the click is the whole interaction — so the report is a
/// desktop notification, which is where the user is already looking. It names
/// the path that was tried, because "not found" without the path sends people
/// looking in the wrong place.
fn open_dashboard() {
    let path = dashboard_path();
    match std::process::Command::new(&path).spawn() {
        Ok(mut child) => {
            tracing::debug!(path = %path.display(), "opened the dashboard");
            // Reaped, or it becomes a zombie in our own process table. Found by
            // counting them: three `dozed-gui` entries in state `Z` parented to
            // this daemon, one per click, because `spawn` without a `wait`
            // leaves the exit status for somebody to collect and nobody did.
            //
            // It matters more now than it did. The dashboard is single-instance
            // (`dozed_gui::single_instance`), so every click after the first
            // starts a process that tells the running window to come forward and
            // exits within milliseconds — which is to say every click after the
            // first used to leave a zombie almost immediately, where before it
            // took closing a window to make one.
            //
            // A thread rather than `tokio::process`, which would mean adding the
            // `process` feature and its SIGCHLD reaper to a daemon that spawns
            // exactly one kind of child. The thread is parked in `waitpid` and
            // costs a stack; for a dashboard that stays open it is one thread,
            // and for the raise-and-exit case it is gone before the click is.
            std::thread::Builder::new()
                .name("dozed-dashboard-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not start dozed-gui");
            let body = format!(
                "Could not run {} ({e}).\n\nIt is installed beside the daemon by \
                 `packaging/install.sh`. `dozectl status` answers the same questions from a \
                 terminal.",
                path.display()
            );
            tokio::spawn(async move { crate::notify::send("dozed: no dashboard", &body).await });
        }
    }
}

/// Run the indicator for as long as the config asks for one.
///
/// Three states rather than two, and the loop exists to move between them:
/// showing an icon, not showing one because `ui.tray = false`, and not showing
/// one because the desktop has no StatusNotifierItem host. The last is silent —
/// a user with no system tray is not missing an icon they expected — while the
/// middle one has to be reversible, because `dozectl reload` is how every other
/// setting in the file takes effect and a lone exception reads as a bug.
pub async fn run(mut state: watch::Receiver<TrayState>, commands: mpsc::Sender<Command>) {
    let mut handle: Option<ksni::Handle<DozedTray>> = None;

    loop {
        let next = state.borrow().clone();
        match (&handle, next.enabled) {
            // Wanted and running: redraw it.
            (Some(h), true) => {
                let draw = next.clone();
                if h.update(move |t: &mut DozedTray| t.state = draw).await.is_none() {
                    tracing::info!("the tray host went away");
                    return;
                }
            }
            // Wanted and not running: start it. A host that is not there is not
            // an error and not worth retrying at — the icon comes back on the
            // next `dozectl reload`, or the next daemon start.
            (None, true) => {
                let tray = DozedTray { state: next.clone(), commands: commands.clone() };
                match tray.spawn().await {
                    Ok(h) => {
                        tracing::info!("tray indicator running");
                        handle = Some(h);
                    }
                    Err(e) => tracing::info!(error = %e, "no system tray; the indicator is off"),
                }
            }
            // No longer wanted: take it down, and keep watching in case the
            // setting comes back.
            (Some(h), false) => {
                tracing::info!("ui.tray is off; removing the indicator");
                h.shutdown().await;
                handle = None;
            }
            (None, false) => {}
        }

        // Only ever woken by a real change: the daemon publishes with
        // `send_if_modified`, so a tick that changed nothing does not reach here.
        if state.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboard_is_looked_for_beside_the_daemon_first() {
        // The bug: `Command::new("dozed-gui")` searches `PATH`, and a
        // `systemd --user` service does not inherit a login shell's — so the
        // binaries `install.sh` puts in ~/.local/bin were invisible and every
        // click produced `No such file or directory` (os error 2).
        let dir = std::env::temp_dir().join(format!("dozed-tray-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let daemon = dir.join("dozed");
        std::fs::write(&daemon, b"").unwrap();

        // Nothing beside it yet: fall back to the bare name, which `Command`
        // resolves against PATH for a distribution that splits them up.
        assert_eq!(dashboard_beside(Some(&daemon)), std::path::PathBuf::from("dozed-gui"));

        std::fs::write(dir.join("dozed-gui"), b"").unwrap();
        assert_eq!(dashboard_beside(Some(&daemon)), dir.join("dozed-gui"));

        // And an unknown own path is not a crash, it is the fallback.
        assert_eq!(dashboard_beside(None), std::path::PathBuf::from("dozed-gui"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tray_with(state: TrayState) -> DozedTray {
        let (commands, _rx) = mpsc::channel(1);
        DozedTray { state, commands }
    }

    #[test]
    fn the_icon_says_which_of_the_three_states_it_is_in() {
        let held = TrayState { mode: Mode::Live, held: 2, known: 5, ..Default::default() };
        assert_eq!(tray_with(held.clone()).icon_state(), crate::icon::State::Holding);

        // Dry-run holds nothing, whatever the planner's counterfactual count
        // says — an icon claiming otherwise would be the §11 lie in the panel.
        let dry = TrayState { mode: Mode::DryRun, held: 2, known: 5, ..Default::default() };
        assert_eq!(tray_with(dry).icon_state(), crate::icon::State::Idle);

        let snoozed = TrayState { snoozed: Some(Duration::from_secs(60)), ..held };
        assert_eq!(tray_with(snoozed).icon_state(), crate::icon::State::Snoozed);
    }

    #[test]
    fn the_item_ships_pixels_rather_than_a_name_to_look_up() {
        use ksni::Tray as _;

        // The regression this pins: naming a themed icon left the tray empty on
        // a panel whose icon cache predated the install, and a name that does
        // not resolve shows as nothing at all rather than falling back.
        let t = tray_with(TrayState::default());
        let pixmaps = t.icon_pixmap();
        assert_eq!(pixmaps.len(), crate::icon::SIZES.len(), "one per offered size");
        assert!(t.icon_name().is_empty(), "a name would be preferred over the pixels");
        assert!(
            pixmaps.iter().all(|i| i.data.iter().any(|&b| b != 0)),
            "an all-zero buffer looks exactly like the bug it replaced"
        );
    }

    #[test]
    fn the_menu_opens_on_a_left_click() {
        // ksni's default sends a left click to `Tray::activate`, which this item
        // does not implement — so the icon looked dead to every button but the
        // middle one, and the menu holding the panic thaw could only be reached
        // by knowing to right-click.
        const { assert!(<DozedTray as ksni::Tray>::MENU_ON_ACTIVATE) };
    }

    #[test]
    fn the_summary_says_would_rather_than_did_in_dry_run() {
        // The counterfactual has to survive into the tooltip. "2 apps held" on a
        // daemon that has held nothing is the same lie the report was fixed for
        // in §11, in the one place a user looks without asking.
        let dry = TrayState { mode: Mode::DryRun, held: 2, known: 9, ..Default::default() };
        assert!(dry.summary().contains("would be holding"), "{}", dry.summary());

        let live = TrayState { mode: Mode::Live, held: 2, known: 9, ..Default::default() };
        assert!(live.summary().contains("2 of 9 apps held"), "{}", live.summary());
    }

    #[test]
    fn a_snooze_outranks_everything_else_in_the_summary() {
        // Because it is the state a user is most likely to be in without
        // remembering it, and the one that explains why nothing is happening.
        let s = TrayState {
            mode: Mode::Live,
            held: 0,
            known: 9,
            snoozed: Some(Duration::from_secs(600)),
            can_act: true,
            enabled: true,
        };
        assert!(s.summary().starts_with("snoozed"), "{}", s.summary());
    }

    #[test]
    fn an_unchanged_state_is_equal_so_it_never_redraws() {
        // The property the whole cost argument rests on.
        let a = TrayState {
            mode: Mode::Live,
            held: 1,
            known: 4,
            snoozed: None,
            can_act: true,
            enabled: true,
        };
        assert_eq!(a, a.clone());
    }
}
