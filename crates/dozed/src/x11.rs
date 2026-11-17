//! The X11 focus source: one in-process source for both X11 desktops.
//!
//! On X11 no compositor extension is needed at all. EWMH puts everything on the
//! root window, so the daemon reads the display server directly and the same
//! code covers X11/KDE and X11/GNOME — which makes it the best value of the
//! focus sources, and one that needs nothing installed.
//!
//! Everything here goes through `x11rb`, which is pure Rust. That is not
//! incidental: it is what lets the workspace keep `#![forbid(unsafe_code)]`,
//! which rules out every C-binding X11 crate.
//!
//! ## No polling
//!
//! `PropertyChangeMask | SubstructureNotifyMask` on the root catches window
//! creation, destruction, restacking, focus changes and desktop switches;
//! `PropertyChangeMask` per window catches state and geometry changes. The
//! daemon therefore blocks on the X connection and wakes only when the desktop
//! actually changes, exactly like the Wayland helpers.
//!
//! ## What this platform gives us that Wayland does not
//!
//! XFixes `SelectSelectionInput` reports selection-owner changes, and the owning
//! window maps to a PID. That closes `DESIGN.md` §10.7 — never freeze the
//! clipboard owner, because the contents live in the source app and freezing it
//! hangs the next paste — which is simply not solvable on Wayland today.
//!
//! Note that XWayland clients under a Wayland session are already reported by
//! the compositor helper, so this source is only ever run for a true X11
//! session (`crate::session`).

use std::collections::{BTreeSet, HashMap};

use tokio::sync::mpsc;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, EventMask, Window, change_window_attributes,
};
use x11rb::rust_connection::RustConnection;

use dozed_core::{Exemption, Rect, WindowFacts};

use crate::sources::Incoming;

/// Atoms we look up once and then use by number.
struct Atoms {
    client_list_stacking: Atom,
    active_window: Atom,
    wm_pid: Atom,
    wm_state: Atom,
    wm_desktop: Atom,
    current_desktop: Atom,
    wm_window_type: Atom,
    state_hidden: Atom,
    state_fullscreen: Atom,
    state_attention: Atom,
    type_normal: Atom,
    type_dialog: Atom,
    clipboard: Atom,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, X11Error> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self, X11Error> {
        Ok(Self {
            client_list_stacking: intern(conn, "_NET_CLIENT_LIST_STACKING")?,
            active_window: intern(conn, "_NET_ACTIVE_WINDOW")?,
            wm_pid: intern(conn, "_NET_WM_PID")?,
            wm_state: intern(conn, "_NET_WM_STATE")?,
            wm_desktop: intern(conn, "_NET_WM_DESKTOP")?,
            current_desktop: intern(conn, "_NET_CURRENT_DESKTOP")?,
            wm_window_type: intern(conn, "_NET_WM_WINDOW_TYPE")?,
            state_hidden: intern(conn, "_NET_WM_STATE_HIDDEN")?,
            state_fullscreen: intern(conn, "_NET_WM_STATE_FULLSCREEN")?,
            state_attention: intern(conn, "_NET_WM_STATE_DEMANDS_ATTENTION")?,
            type_normal: intern(conn, "_NET_WM_WINDOW_TYPE_NORMAL")?,
            type_dialog: intern(conn, "_NET_WM_WINDOW_TYPE_DIALOG")?,
            clipboard: intern(conn, "CLIPBOARD")?,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum X11Error {
    #[error("cannot connect to the X server: {0}")]
    Connect(#[from] x11rb::errors::ConnectError),
    #[error("X11 connection lost: {0}")]
    Connection(#[from] x11rb::errors::ConnectionError),
    #[error("X11 request failed: {0}")]
    Reply(#[from] x11rb::errors::ReplyError),
}

/// Read a window property as a list of 32-bit values.
fn prop32(conn: &RustConnection, win: Window, prop: Atom) -> Vec<u32> {
    let Ok(cookie) = conn.get_property(false, win, prop, AtomEnum::ANY, 0, 1024) else {
        return Vec::new();
    };
    cookie.reply().ok().and_then(|r| r.value32().map(Iterator::collect)).unwrap_or_default()
}

/// `WM_CLASS` is two NUL-terminated strings: instance then class. The class is
/// the closer analogue of a Wayland `app_id`, so it is what we report.
fn wm_class(conn: &RustConnection, win: Window) -> String {
    let Ok(cookie) = conn.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
    else {
        return String::new();
    };
    let Ok(reply) = cookie.reply() else { return String::new() };
    let mut parts = reply.value.split(|b| *b == 0);
    let _instance = parts.next();
    parts.next().map(|c| String::from_utf8_lossy(c).into_owned()).unwrap_or_default()
}

/// The PID behind a window.
///
/// `_NET_WM_PID` is advisory and some clients simply omit it, so this falls back
/// to the X-Resource extension, which asks the server who actually owns the
/// resource and cannot be lied about. Without the fallback those windows would
/// be invisible to `dozed` entirely.
fn pid_of(conn: &RustConnection, win: Window, atoms: &Atoms) -> Option<u32> {
    if let Some(pid) = prop32(conn, win, atoms.wm_pid).first().copied().filter(|p| *p != 0) {
        return Some(pid);
    }
    let spec = ClientIdSpec { client: win, mask: ClientIdMask::LOCAL_CLIENT_PID };
    let reply = conn.res_query_client_ids(&[spec]).ok()?.reply().ok()?;
    reply
        .ids
        .iter()
        .filter(|id| id.spec.mask == ClientIdMask::LOCAL_CLIENT_PID)
        .find_map(|id| id.value.first().copied())
        .filter(|p| *p != 0)
}

/// Whether this is an application window rather than desktop furniture.
///
/// A notification or a dock popping up over your terminal does not make the
/// terminal hidden, which is the same rule the KWin helper applies.
fn is_application(conn: &RustConnection, win: Window, atoms: &Atoms) -> bool {
    let types = prop32(conn, win, atoms.wm_window_type);
    // A window with no type at all is a normal window by EWMH convention.
    types.is_empty() || types.iter().any(|t| *t == atoms.type_normal || *t == atoms.type_dialog)
}

/// Build one complete snapshot of the desktop.
fn snapshot(conn: &RustConnection, root: Window, atoms: &Atoms) -> Vec<WindowFacts> {
    // Bottom to top, which is exactly the stacking order `visibility::resolve`
    // wants: a window's index is its depth.
    let stacking = prop32(conn, root, atoms.client_list_stacking);
    let active = prop32(conn, root, atoms.active_window).first().copied().unwrap_or(0);
    let current_desktop = prop32(conn, root, atoms.current_desktop).first().copied();

    let mut out = Vec::with_capacity(stacking.len());
    for (index, win) in stacking.iter().copied().enumerate() {
        if !is_application(conn, win, atoms) {
            continue;
        }
        let Some(pid) = pid_of(conn, win, atoms) else { continue };

        let state = prop32(conn, win, atoms.wm_state);
        let desktop = prop32(conn, win, atoms.wm_desktop).first().copied();
        // 0xFFFF_FFFF is EWMH's "on all desktops".
        let on_current = match (desktop, current_desktop) {
            (Some(0xFFFF_FFFF), _) | (None, _) | (_, None) => true,
            (Some(d), Some(c)) => d == c,
        };

        out.push(WindowFacts {
            pid,
            app_id: wm_class(conn, win),
            // X11 has no per-window desktop-file id. Identity falls back to the
            // WM_CLASS rung, then to the executable (`DESIGN.md` §4).
            desktop_file: String::new(),
            focused: win == active,
            minimized: state.contains(&atoms.state_hidden),
            on_current_desktop: on_current,
            geometry: geometry_of(conn, root, win),
            stack: u32::try_from(index).unwrap_or(u32::MAX),
            // Occlusion is per-monitor, and X11 geometry is already in one
            // shared root coordinate space, so a single output name is correct
            // here: overlap in root coordinates *is* overlap on screen.
            output: "root".to_owned(),
            fullscreen: state.contains(&atoms.state_fullscreen),
            attention: state.contains(&atoms.state_attention),
            translucent: false,
            visibility: None,
        });
    }
    out
}

/// A window's rectangle in root coordinates.
///
/// `GetGeometry` reports coordinates relative to the parent, which under a
/// reparenting window manager is the frame rather than the root — so the result
/// has to be translated before it can be compared with another window's.
fn geometry_of(conn: &RustConnection, root: Window, win: Window) -> Option<Rect> {
    let geom = conn.get_geometry(win).ok()?.reply().ok()?;
    let trans = conn.translate_coordinates(win, root, 0, 0).ok()?.reply().ok()?;
    Some(Rect::new(
        i32::from(trans.dst_x),
        i32::from(trans.dst_y),
        i32::from(geom.width),
        i32::from(geom.height),
    ))
}

/// Subscribe to per-window property changes, so state and geometry updates
/// arrive without polling.
fn watch_windows(conn: &RustConnection, windows: &[Window], watched: &mut BTreeSet<Window>) {
    for win in windows {
        if !watched.insert(*win) {
            continue;
        }
        let attrs = x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY);
        // A window can be destroyed between the snapshot and this call; that is
        // routine and not worth reporting.
        let _ = change_window_attributes(conn, *win, &attrs);
    }
}

/// Who owns the clipboard, as a PID.
///
/// §10.7: clipboard contents live in the source application, so freezing the
/// owner hangs the next paste. This is the X11 half of that invariant; Wayland
/// has no equivalent and reports the exemption as unavailable.
fn clipboard_owner_pid(conn: &RustConnection, atoms: &Atoms) -> Option<u32> {
    let owner = conn.get_selection_owner(atoms.clipboard).ok()?.reply().ok()?.owner;
    if owner == x11rb::NONE {
        return None;
    }
    pid_of(conn, owner, atoms)
}

/// Run the X11 focus source until the connection dies.
///
/// Returning at all means we can no longer see focus, and the daemon's helper
/// watchdog will time out and thaw everything (`DESIGN.md` §10.2) — the same
/// path a crashed compositor helper takes.
pub fn run(tx: &mpsc::Sender<Incoming>) -> Result<(), X11Error> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let atoms = Atoms::new(&conn)?;

    // XFixes has to be negotiated before any of its requests are legal.
    let xfixes = conn.xfixes_query_version(5, 0).is_ok_and(|c| c.reply().is_ok());
    if xfixes {
        let _ = conn.xfixes_select_selection_input(
            root,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        );
    } else {
        tracing::warn!("no XFixes: the clipboard-owner exemption is unavailable");
    }

    let attrs = x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::SUBSTRUCTURE_NOTIFY);
    change_window_attributes(&conn, root, &attrs)?.check()?;
    conn.flush()?;

    tracing::info!("X11 focus source running; reading EWMH from the root window");
    let _ = tx.blocking_send(Incoming::HelperAlive { compositor: Some("x11".to_owned()) });

    let mut watched = BTreeSet::new();
    let mut last: Option<Vec<WindowFacts>> = None;
    let mut last_clipboard: Option<u32> = None;
    // Cheap dedupe of the property atoms worth reacting to, so a caption change
    // does not cost a full snapshot.
    let interesting: HashMap<Atom, ()> = [
        atoms.client_list_stacking,
        atoms.active_window,
        atoms.current_desktop,
        atoms.wm_state,
        atoms.wm_desktop,
    ]
    .into_iter()
    .map(|a| (a, ()))
    .collect();

    let publish = |conn: &RustConnection,
                   last: &mut Option<Vec<WindowFacts>>,
                   watched: &mut BTreeSet<Window>| {
        let windows = snapshot(conn, root, &atoms);
        let stacking: Vec<Window> = prop32(conn, root, atoms.client_list_stacking);
        watch_windows(conn, &stacking, watched);
        // Suppress unchanged snapshots for the same reason the KWin helper
        // does: most events do not change anything dozed cares about, and
        // sending those is pure wakeup cost on both ends.
        if last.as_deref() == Some(windows.as_slice()) {
            return;
        }
        *last = Some(windows.clone());
        let _ = tx.blocking_send(Incoming::WindowFacts(windows));
    };

    publish(&conn, &mut last, &mut watched);
    if xfixes {
        last_clipboard = clipboard_owner_pid(&conn, &atoms);
        let _ = tx.blocking_send(Incoming::Exemptions {
            exemption: Exemption::ClipboardOwner,
            pids: last_clipboard.into_iter().collect(),
        });
    }

    loop {
        let event = conn.wait_for_event()?;
        let mut resnapshot = false;
        let mut recheck_clipboard = false;

        // Drain whatever else arrived in the same burst before doing any work:
        // a desktop switch restacks everything and produces a flood.
        let mut pending = Some(event);
        while let Some(ev) = pending.take() {
            match ev {
                XEvent::PropertyNotify(e) => {
                    if interesting.contains_key(&e.atom) {
                        resnapshot = true;
                    }
                }
                XEvent::ConfigureNotify(_)
                | XEvent::MapNotify(_)
                | XEvent::UnmapNotify(_)
                | XEvent::DestroyNotify(_)
                | XEvent::CreateNotify(_) => resnapshot = true,
                XEvent::XfixesSelectionNotify(_) => recheck_clipboard = true,
                _ => {}
            }
            pending = conn.poll_for_event()?;
        }

        if resnapshot {
            publish(&conn, &mut last, &mut watched);
        }
        if recheck_clipboard {
            let owner = clipboard_owner_pid(&conn, &atoms);
            if owner != last_clipboard {
                last_clipboard = owner;
                let _ = tx.blocking_send(Incoming::Exemptions {
                    exemption: Exemption::ClipboardOwner,
                    pids: owner.into_iter().collect(),
                });
            }
        }
    }
}

/// Run the source on a blocking thread, restarting it if the server drops us.
///
/// `x11rb`'s connection is synchronous, so it gets a thread of its own rather
/// than blocking a tokio worker. The daemon sees the same `Incoming` stream a
/// compositor helper would produce.
pub fn spawn(tx: mpsc::Sender<Incoming>) {
    std::thread::Builder::new()
        .name("dozed-x11".to_owned())
        .spawn(move || {
            match run(&tx) {
                Ok(()) => tracing::warn!("X11 focus source stopped"),
                Err(e) => tracing::error!(error = %e, "X11 focus source failed"),
            }
            // Losing the source means we can no longer see focus. Saying so
            // explicitly beats waiting for the watchdog (`DESIGN.md` §10.2).
            let _ = tx.blocking_send(Incoming::Event(dozed_core::Event::FocusSourceHealth {
                healthy: false,
            }));
        })
        .expect("spawning the X11 source thread");
}

#[cfg(test)]
mod tests {
    #[test]
    fn on_all_desktops_is_not_another_workspace() {
        // EWMH spells "sticky" as 0xFFFFFFFF, and reading it as a desktop number
        // would make every pinned window look hidden.
        let sticky: u32 = 0xFFFF_FFFF;
        let on_current = match (Some(sticky), Some(3u32)) {
            (Some(0xFFFF_FFFF), _) | (None, _) | (_, None) => true,
            (Some(d), Some(c)) => d == c,
        };
        assert!(on_current);
    }

    #[test]
    fn an_unknown_desktop_never_reads_as_hidden() {
        // Fail towards "never touch this": if either side of the comparison is
        // missing we must not conclude the window is on another workspace.
        for pair in [(None, Some(2u32)), (Some(2u32), None), (None, None)] {
            let on_current = match pair {
                (Some(0xFFFF_FFFF), _) | (None, _) | (_, None) => true,
                (Some(d), Some(c)) => d == c,
            };
            assert!(on_current, "{pair:?}");
        }
    }

    #[test]
    fn wm_class_takes_the_class_not_the_instance() {
        // WM_CLASS is "instance\0class\0"; the class is the app_id analogue.
        let raw = b"navigator\0Firefox\0";
        let mut parts = raw.split(|b| *b == 0);
        let _instance = parts.next();
        let class = parts.next().map(|c| String::from_utf8_lossy(c).into_owned()).unwrap();
        assert_eq!(class, "Firefox");
    }

    #[test]
    fn stacking_index_is_depth_bottom_to_top() {
        // _NET_CLIENT_LIST_STACKING is bottom-to-top, which is exactly what
        // visibility::resolve means by `stack`: higher is nearer the user.
        let stacking = [10u32, 20, 30];
        let top = stacking.iter().enumerate().max_by_key(|(i, _)| *i).unwrap();
        assert_eq!(*top.1, 30);
    }
}
