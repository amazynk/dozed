//! Working out which display server and desktop we are on, and therefore which
//! focus source to run (`DESIGN.md` §5 and §12).
//!
//! Four combinations, two shapes:
//!
//! * **Helper-fed** — a compositor extension pushes window snapshots to the
//!   daemon over D-Bus. Required on Wayland, where a client cannot enumerate
//!   other clients' windows at all. One for KWin, one for GNOME Shell.
//! * **In-process** — the daemon reads the display server itself. On X11 no
//!   extension is needed: EWMH puts everything on the root window, so one source
//!   covers X11/KDE and X11/GNOME together.
//!
//! Detection is a guess from the environment, and a wrong guess is expensive:
//! picking the X11 source under Wayland would see only XWayland clients and
//! silently report every native Wayland app as gone. So the result is logged,
//! reported in `dozectl status`, and overridable with `--focus-source`.

use std::fmt;

/// Which focus source to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum FocusSourceKind {
    /// Pick from the environment. The default.
    #[default]
    Auto,
    /// Wait for a compositor helper to call in over D-Bus (KWin or GNOME).
    Helper,
    /// Read EWMH from the X server directly.
    X11,
    /// Read Sway's (or i3's) own IPC socket.
    ///
    /// A per-compositor source rather than the portable Wayland one, because
    /// neither `ext-foreign-toplevel-list-v1` nor
    /// `wlr-foreign-toplevel-management-v1` carries a PID — see `crate::sway`.
    Sway,
    /// Run no focus source at all. The daemon stays passive, which is what
    /// `DESIGN.md` §10.2 requires of a daemon that cannot see focus.
    None,
}

impl fmt::Display for FocusSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Helper => "helper",
            Self::X11 => "x11",
            Self::Sway => "sway",
            Self::None => "none",
        })
    }
}

/// What the environment says about this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub kind: FocusSourceKind,
    /// `kwin`, `gnome-shell`, `x11`, or whatever we could work out.
    pub desktop: String,
    /// The reasoning, for `dozectl status` and for the log line at startup.
    ///
    /// A statement about **detection**, which is a fact settled at startup — and
    /// deliberately not a claim about the current state. It used to read "waiting
    /// for the kwin helper to call in", which was true for the four seconds before
    /// the helper arrived and then wrong for the rest of the session: `dozectl
    /// status` printed `focus source ok` and, on the line below it, that dozed was
    /// still waiting for the thing that was answering. Liveness is
    /// `focus_source_healthy`, and one field saying two things is how they came to
    /// disagree.
    pub why: String,
}

impl Session {
    /// What to log at startup, where "waiting" *is* the true state.
    #[must_use]
    pub fn startup_message(&self) -> String {
        match self.kind {
            FocusSourceKind::Helper => {
                format!("{}; waiting for it to call in", self.why)
            }
            _ => self.why.clone(),
        }
    }
}

/// Raw environment, so detection can be tested without one.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub session_type: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub current_desktop: Option<String>,
    /// `SWAYSOCK` or `I3SOCK`. Its mere presence is the detection: a compositor
    /// that exports it is one whose IPC this daemon can read.
    pub sway_socket: Option<String>,
}

impl Env {
    #[must_use]
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            session_type: get("XDG_SESSION_TYPE"),
            wayland_display: get("WAYLAND_DISPLAY"),
            display: get("DISPLAY"),
            current_desktop: get("XDG_CURRENT_DESKTOP"),
            sway_socket: get("SWAYSOCK").or_else(|| get("I3SOCK")),
        }
    }
}

/// Decide which source to run.
///
/// `WAYLAND_DISPLAY` is trusted over `XDG_SESSION_TYPE`, because a Wayland
/// session that also exports `DISPLAY` for XWayland is the normal case and
/// `XDG_SESSION_TYPE` is not always set by the greeter. Under Wayland the X11
/// source would see only XWayland clients and report every native app as gone —
/// the worst of the available failures, since "no windows" reads as "everything
/// is hidden".
#[must_use]
pub fn detect(env: &Env) -> Session {
    let desktop = env.current_desktop.clone().unwrap_or_default();
    let wayland = env.wayland_display.is_some()
        || env.session_type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("wayland"));

    // Before the helper path, because a Sway session is a Wayland session and
    // there is no helper coming: nothing is going to install a KWin script into
    // Sway, so waiting for one means staying passive forever.
    if let Some(sock) = &env.sway_socket {
        return Session {
            kind: FocusSourceKind::Sway,
            desktop: if desktop.is_empty() { "sway".to_owned() } else { desktop },
            why: format!("{sock} is set; reading the compositor's own IPC, no helper needed"),
        };
    }

    if wayland {
        let which = if desktop.to_ascii_lowercase().contains("gnome") {
            "gnome-shell"
        } else if desktop.to_ascii_lowercase().contains("kde") {
            "kwin"
        } else {
            "unknown"
        };
        return Session {
            kind: FocusSourceKind::Helper,
            desktop: which.to_owned(),
            why: format!(
                "Wayland session ({}); window reports come from the {which} helper",
                env.wayland_display.as_deref().unwrap_or("XDG_SESSION_TYPE=wayland")
            ),
        };
    }

    if let Some(display) = &env.display {
        return Session {
            kind: FocusSourceKind::X11,
            desktop: if desktop.is_empty() { "x11".to_owned() } else { desktop },
            why: format!("X11 session on {display}; reading EWMH directly, no helper needed"),
        };
    }

    Session {
        kind: FocusSourceKind::None,
        desktop,
        // Neither display server: a TTY, a container, or a service started
        // before the session. Staying passive is the only safe answer.
        why: "no WAYLAND_DISPLAY and no DISPLAY; running passive".to_owned(),
    }
}

/// Apply a command-line override, keeping the detected desktop for reporting.
#[must_use]
pub fn resolve(env: &Env, override_kind: FocusSourceKind) -> Session {
    let detected = detect(env);
    if override_kind == FocusSourceKind::Auto {
        return detected;
    }
    Session {
        kind: override_kind,
        desktop: detected.desktop,
        why: format!("forced with --focus-source={override_kind} (detected {})", detected.kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(session: &str, wayland: &str, display: &str, desktop: &str) -> Env {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        Env {
            session_type: some(session),
            wayland_display: some(wayland),
            display: some(display),
            current_desktop: some(desktop),
            sway_socket: None,
        }
    }

    #[test]
    fn wayland_kde_waits_for_the_kwin_helper() {
        let s = detect(&env("wayland", "wayland-0", ":0", "KDE"));
        assert_eq!(s.kind, FocusSourceKind::Helper);
        assert_eq!(s.desktop, "kwin");
    }

    #[test]
    fn wayland_gnome_waits_for_the_shell_extension() {
        let s = detect(&env("wayland", "wayland-0", ":0", "ubuntu:GNOME"));
        assert_eq!(s.kind, FocusSourceKind::Helper);
        assert_eq!(s.desktop, "gnome-shell");
    }

    #[test]
    fn a_wayland_session_never_picks_the_x11_source() {
        // The expensive misdetection: under Wayland the X11 source sees only
        // XWayland clients, so every native app looks gone — and "no windows"
        // reads as "everything is hidden".
        let s = detect(&env("wayland", "wayland-0", ":0", "KDE"));
        assert_ne!(s.kind, FocusSourceKind::X11);

        // Even when XDG_SESSION_TYPE is missing, which some greeters do.
        let s = detect(&env("", "wayland-0", ":0", "KDE"));
        assert_eq!(s.kind, FocusSourceKind::Helper);
    }

    #[test]
    fn x11_needs_no_helper_on_either_desktop() {
        for desktop in ["KDE", "GNOME", "XFCE"] {
            let s = detect(&env("x11", "", ":0", desktop));
            assert_eq!(s.kind, FocusSourceKind::X11, "{desktop}");
            assert_eq!(s.desktop, desktop);
        }
    }

    #[test]
    fn the_description_does_not_claim_to_still_be_waiting() {
        // The bug this fixes was visible in `dozectl status` for a whole session:
        // `focus source ok` on one line and, below it, that dozed was waiting for
        // the helper that was answering. `why` is about detection, which is
        // settled at startup; liveness is `focus_source_healthy`, and one field
        // saying both is how they came to disagree.
        let s = detect(&env("wayland", "wayland-0", ":0", "KDE"));
        assert!(!s.why.contains("waiting"), "{}", s.why);
        assert!(s.why.contains("kwin helper"), "but it must still say where reports come from");

        // At startup, though, "waiting" is exactly the true state, and the log
        // line is where that belongs.
        assert!(
            s.startup_message().contains("waiting for it to call in"),
            "{}",
            s.startup_message()
        );
    }

    #[test]
    fn an_in_process_source_is_never_waiting_for_anything() {
        // Nothing has to call in, so the startup line must not invent a wait.
        let mut sway = env("wayland", "wayland-1", "", "sway");
        sway.sway_socket = Some("/run/user/1000/sway.sock".to_owned());
        for s in
            [detect(&env("x11", "", ":0", "KDE")), detect(&sway), detect(&env("tty", "", "", ""))]
        {
            assert_eq!(s.startup_message(), s.why, "{s:?}");
            assert!(!s.startup_message().contains("waiting"), "{s:?}");
        }
    }

    #[test]
    fn a_sway_session_reads_the_compositors_own_ipc() {
        // Sway is a Wayland session, so without this it would fall into the helper
        // branch and wait forever for a KWin script nobody is going to install.
        let mut e = env("wayland", "wayland-1", "", "sway");
        e.sway_socket = Some("/run/user/1000/sway-ipc.1000.42.sock".to_owned());
        let s = detect(&e);
        assert_eq!(s.kind, FocusSourceKind::Sway);
        assert!(s.why.contains("no helper needed"), "{}", s.why);
    }

    #[test]
    fn the_sway_socket_outranks_the_desktop_name() {
        // A wlroots compositor that sets neither XDG_CURRENT_DESKTOP nor
        // XDG_SESSION_TYPE — which several do — must still be recognised, and the
        // socket is the only reliable evidence.
        let mut e = env("", "wayland-1", "", "");
        e.sway_socket = Some("/run/user/1000/sway.sock".to_owned());
        assert_eq!(detect(&e).kind, FocusSourceKind::Sway);
    }

    #[test]
    fn no_display_server_means_passive() {
        // A TTY or a unit started before the session. §10.2: a daemon that
        // cannot see focus must not hold anything.
        let s = detect(&env("tty", "", "", ""));
        assert_eq!(s.kind, FocusSourceKind::None);
        assert!(s.why.contains("passive"));
    }

    #[test]
    fn the_override_wins_and_says_so() {
        let e = env("wayland", "wayland-0", ":0", "KDE");
        let s = resolve(&e, FocusSourceKind::X11);
        assert_eq!(s.kind, FocusSourceKind::X11);
        assert!(s.why.contains("forced"), "{}", s.why);
        assert!(s.why.contains("detected helper"), "{}", s.why);
        // And auto leaves detection alone.
        assert_eq!(resolve(&e, FocusSourceKind::Auto), detect(&e));
    }

    #[test]
    fn detection_survives_this_machine() {
        // Whatever this box is, detection must produce something coherent
        // rather than panicking on an unexpected environment.
        let s = detect(&Env::from_process());
        assert!(!s.why.is_empty());
    }
}
