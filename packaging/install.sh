#!/usr/bin/env bash
#
# Install dozed for the current user (DESIGN.md §15).
#
# Everything goes under $HOME: dozed is same-UID only and unprivileged, so there
# is nothing here that wants root. Deliberately not a Flatpak — a sandboxed
# daemon cannot see or freeze host processes, which is the entire job.
#
#   ./packaging/install.sh            install binaries, unit and the right helper
#   ./packaging/install.sh --uninstall
#   ./packaging/install.sh --no-helper    binaries and unit only
#   ./packaging/install.sh --no-restart   leave a running daemon on the old binary
#   ./packaging/install.sh --no-build     install what is already in target/release
#
# The compositor helper is the one piece that touches your desktop's own
# configuration, so it is called out at every step and can be skipped.

set -euo pipefail

BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_ROOT="$DATA_DIR/icons/hicolor"
ICON_DIR="$ICON_ROOT/scalable/apps"
MAN_DIR="$DATA_DIR/man/man1"
APPS_DIR="$DATA_DIR/applications"
BASH_COMP_DIR="$DATA_DIR/bash-completion/completions"
ZSH_COMP_DIR="$DATA_DIR/zsh/site-functions"
FISH_COMP_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/dozed"
GNOME_EXT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions"
GNOME_UUID="dozed-focus-source@dozed"
KWIN_ID="dozed-focus-source"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WITH_HELPER=1
WITH_RESTART=1
WITH_BUILD=1
UNINSTALL=0

for arg in "$@"; do
    case "$arg" in
        --no-helper) WITH_HELPER=0 ;;
        --no-restart) WITH_RESTART=0 ;;
        --no-build) WITH_BUILD=0 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

say()  { printf '  %s\n' "$*"; }
step() { printf '\n%s\n' "$*"; }

# --- talking to systemd, and when not to ---------------------------------
#
# Every path above is redirected by the XDG variables, which is what lets
# `test-install.sh` run this script against a throwaway tree. **`systemctl
# --user` is not.** It talks to the running `systemd --user` manager over D-Bus,
# and that manager knows nothing about the environment of whoever invoked the
# client — so a `systemctl --user disable --now dozed.service` issued while every
# file path points at `/tmp/tmp.XXXX` still reaches the daemon somebody is
# actually running.
#
# That is not hypothetical. `test-install.sh` runs this script five times and
# then runs it once more with `--uninstall`; against a real session that was five
# forced restarts of a live daemon followed by a `disable --now` which switched
# it off and removed its enable symlink. Nothing said so — the test printed
# "all install.sh checks passed" — and the daemon stayed off until somebody
# noticed it was not running two days later. A battery tool quietly switching
# itself off is the failure `test-install.sh` exists to prevent, and this script
# was committing it while proving it did not.
#
# The guard is a question about *this* installation rather than a flag the caller
# has to remember: is the unit file I am writing the unit file the running
# manager would load? `UnitPath` is the manager's own search list, so if
# `$UNIT_DIR` is not on it then whatever `systemctl` would act on is a different
# installation from the one being changed here, and must be left alone.
#
# Cached, because it is asked once per systemctl call and the answer cannot
# change mid-run.
MANAGES_OUR_UNIT=""
manages_our_unit() {
    if [ -z "$MANAGES_OUR_UNIT" ]; then
        MANAGES_OUR_UNIT=no
        local paths
        if paths="$(systemctl --user show -P UnitPath 2>/dev/null)"; then
            for p in $paths; do
                if [ "$p" = "$UNIT_DIR" ]; then MANAGES_OUR_UNIT=yes; break; fi
            done
        fi
    fi
    [ "$MANAGES_OUR_UNIT" = yes ]
}

# `systemctl --user`, but only when it would act on the unit we are installing.
#
# Silent when it declines, except once: a run that cannot reach systemd should
# say so one time rather than at every call, since the interesting case is a test
# harness where the answer is expected and the noise is not.
SAID_NO_SYSTEMD=0
sctl() {
    if manages_our_unit; then
        systemctl --user "$@"
        return $?
    fi
    if [ "$SAID_NO_SYSTEMD" = 0 ]; then
        SAID_NO_SYSTEMD=1
        say "not touching systemd: $UNIT_DIR is not on the running user manager's"
        say "unit path, so anything started or stopped here would belong to a"
        say "different installation than the one being changed"
    fi
    return 0
}

# Whether a unit actually *runs* live.
#
# Anchored to ExecStart on purpose. The shipped unit's own comments explain how
# to switch to --live, so a plain `grep -- --live` matches every unit ever
# written and reported a fresh dry-run install as "applications will actually be
# frozen" — a summary contradicting the very line it had just written.
runs_live() { [ -e "$1" ] && grep -qE '^ExecStart=.*--live( |$)' "$1"; }

# Which desktop are we on? Same logic as the daemon's own detection, and for the
# same reason: under Wayland the compositor is the only thing that can see
# windows, while X11 needs no helper at all.
detect_desktop() {
    local desktop="${XDG_CURRENT_DESKTOP:-}"
    if [ -n "${WAYLAND_DISPLAY:-}" ] || [ "${XDG_SESSION_TYPE:-}" = wayland ]; then
        case "${desktop,,}" in
            *gnome*) echo gnome ;;
            *kde*)   echo kwin ;;
            *)       echo wayland-unknown ;;
        esac
    elif [ -n "${DISPLAY:-}" ]; then
        echo x11
    else
        echo none
    fi
}

uninstall() {
    step "Removing dozed"
    sctl disable --now dozed.service 2>/dev/null || true
    rm -f "$UNIT_DIR/dozed.service" "$UNIT_DIR/dozed.service.new"
    sctl daemon-reload 2>/dev/null || true
    say "removed the systemd unit"

    for b in dozed dozectl dozed-gui; do rm -f "$BIN_DIR/$b"; done
    say "removed binaries from $BIN_DIR"

    for i in dozed dozed-holding dozed-snoozed; do rm -f "$ICON_DIR/$i.svg"; done
    say "removed the application icons"

    rm -f "$BASH_COMP_DIR/dozectl" "$ZSH_COMP_DIR/_dozectl" "$FISH_COMP_DIR/dozectl.fish"
    say "removed the shell completions"

    # Every page this script could have written, including the per-subcommand
    # ones, and nothing else in a directory that is not ours to empty.
    rm -f "$MAN_DIR/dozed.1" "$MAN_DIR/dozectl.1" "$MAN_DIR"/dozectl-*.1
    say "removed the manual pages"

    rm -f "$APPS_DIR/dozed-gui.desktop"
    command -v update-desktop-database >/dev/null \
        && update-desktop-database -q "$APPS_DIR" 2>/dev/null || true
    say "removed the desktop entry"

    # Switched off before it is removed, so neither desktop is left with a
    # setting that names a helper which is no longer there.
    if command -v kwriteconfig6 >/dev/null; then
        kwriteconfig6 --file kwinrc --group Plugins --key "${KWIN_ID}Enabled" --delete \
            >/dev/null 2>&1 || true
    fi
    if command -v kpackagetool6 >/dev/null; then
        kpackagetool6 --type=KWin/Script --remove "$KWIN_ID" >/dev/null 2>&1 \
            && say "removed the KWin script" || true
    fi
    if command -v gnome-extensions >/dev/null; then
        gnome-extensions disable "$GNOME_UUID" >/dev/null 2>&1 || true
    fi
    rm -rf "${GNOME_EXT_DIR:?}/$GNOME_UUID" && say "removed the GNOME extension" || true

    say "left your config at $CONFIG_DIR and state at ${XDG_STATE_HOME:-$HOME/.local/state}/dozed"
    exit 0
}

[ "$UNINSTALL" = 1 ] && uninstall

# Was a daemon already running when we got here?
#
# Read before anything is installed, because that is the state the answer is
# about: "there was a dozed running, and this script has just replaced the
# binary underneath it". Nothing below changes it — a running process keeps the
# inode it started from — which is exactly the problem: without a restart the
# install silently has no effect until the next login, and the version somebody
# just built is not the one deciding anything.
WAS_RUNNING=0
if sctl is-active --quiet dozed.service 2>/dev/null; then
    WAS_RUNNING=1
fi

step "Building (release)"
if [ "$WITH_BUILD" = 0 ]; then
    say "skipping the build (--no-build)"
elif ! command -v cargo >/dev/null; then
    # Named before the failure rather than after it: `cargo: command not found`
    # from inside a subshell, three steps into a script that has already printed
    # two green ticks, reads as the script being broken.
    echo "cargo is not on PATH, and this script builds from source." >&2
    echo "Install Rust (https://rustup.rs), or use --no-build if you have already" >&2
    echo "built into $REPO/target/release." >&2
    exit 1
else
    ( cd "$REPO" && cargo build --release --workspace )
    say "built"
fi
for b in dozed dozectl; do
    if [ ! -x "$REPO/target/release/$b" ]; then
        echo "missing $REPO/target/release/$b — build first, or drop --no-build." >&2
        exit 1
    fi
done

step "Installing binaries into $BIN_DIR"
mkdir -p "$BIN_DIR"
for b in dozed dozectl; do
    install -m 0755 "$REPO/target/release/$b" "$BIN_DIR/$b"
    say "$b"
done
if [ -x "$REPO/target/release/dozed-gui" ]; then
    install -m 0755 "$REPO/target/release/dozed-gui" "$BIN_DIR/dozed-gui"
    say "dozed-gui"
fi
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "NOTE: $BIN_DIR is not on your PATH" ;;
esac

# The application icon. NOTE: the tray does **not** use these — it draws its own
# and sends pixels over D-Bus, because a themed name is only as good as the
# panel's icon cache at the moment it looks up, and a panel already running when
# this ran would show nothing at all. Editing these SVGs will not change the tray;
# `crates/dozed/src/icon.rs` is where that geometry lives. They are installed for
# the desktop entry and the window icon.
step "Installing the application icons into $ICON_DIR"
mkdir -p "$ICON_DIR"
for i in dozed dozed-holding dozed-snoozed; do
    install -m 0644 "$REPO/packaging/icons/$i.svg" "$ICON_DIR/$i.svg"
done
say "3 icons"
# The cache is only a cache: a missing or stale one costs a slower first lookup,
# never a missing icon, so a machine without the tool is not a failure.
if command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -qtf "$ICON_ROOT" 2>/dev/null || true
fi

# Generated by the binaries themselves, from the same clap definition they parse
# with, so a subcommand cannot exist without its completion or its manual entry.
# Nothing is checked in, which is what makes that guarantee free.
step "Installing shell completions"
DOZECTL="$REPO/target/release/dozectl"
mkdir -p "$BASH_COMP_DIR" "$ZSH_COMP_DIR" "$FISH_COMP_DIR"
if "$DOZECTL" completions bash > "$BASH_COMP_DIR/dozectl" 2>/dev/null; then
    "$DOZECTL" completions zsh  > "$ZSH_COMP_DIR/_dozectl"
    "$DOZECTL" completions fish > "$FISH_COMP_DIR/dozectl.fish"
    say "bash, zsh, fish"
    # zsh is the one that does not just work: bash-completion and fish both scan
    # XDG data dirs, and zsh only reads what is on its fpath.
    case ":${FPATH:-}:" in
        *":$ZSH_COMP_DIR:"*) ;;
        *) say "for zsh, add to ~/.zshrc: fpath=($ZSH_COMP_DIR \$fpath)" ;;
    esac
else
    # A noexec $BIN_DIR or a cross-built binary. Not fatal — completions are a
    # convenience, and the daemon does not care.
    rm -f "$BASH_COMP_DIR/dozectl"
    say "could not run dozectl to generate them; skipped"
fi

step "Installing manual pages into $MAN_DIR"
mkdir -p "$MAN_DIR"
# One page per subcommand, not just the top level: dozectl(1) cross-references
# dozectl-status(1) and the rest, and shipping it alone leaves every one of
# those pointing at nothing.
if "$DOZECTL" man --to "$MAN_DIR" 2>/dev/null && "$REPO/target/release/dozed" --man > "$MAN_DIR/dozed.1" 2>/dev/null; then
    say "dozed(1), dozectl(1) and one page per subcommand"
    case ":${MANPATH:-}:" in
        *":$DATA_DIR/man:"*) ;;
        # man-db has searched ~/.local/share/man by default since 2.9; older ones
        # and mandoc need telling. Cheap to say, confusing to discover.
        *) say "if \`man dozectl\` cannot find it: export MANPATH=\"$DATA_DIR/man:\$MANPATH\"" ;;
    esac
else
    say "could not generate manual pages; skipped"
fi

step "Installing the desktop entry"
mkdir -p "$APPS_DIR"
install -m 0644 "$REPO/packaging/dozed-gui.desktop" "$APPS_DIR/dozed-gui.desktop"
say "$APPS_DIR/dozed-gui.desktop"
if command -v update-desktop-database >/dev/null; then
    update-desktop-database -q "$APPS_DIR" 2>/dev/null || true
fi

step "Installing the systemd user unit"
mkdir -p "$UNIT_DIR"
# The unit is treated exactly like the config below: written once, then left
# alone. It is the *more* important of the two to leave alone, because the one
# thing anybody edits in it is `--dry-run` to `--live` — and this script told
# them to. Overwriting it silently reverted that choice on every upgrade, which
# is a tool quietly switching itself off; observed on a live machine, where a
# reinstall put a deliberately live daemon back into dry-run and nothing said so.
#
# A new unit is written alongside instead, so a genuine change to the packaging
# is still visible rather than lost.
UNIT="$UNIT_DIR/dozed.service"
# Substituted with awk rather than sed, because the replacement is a path from the
# environment and sed would interpret it. `&` in a sed replacement means "the whole
# match", and `|` is the delimiter here — so a $HOME or $XDG_BIN_HOME containing
# either produced a unit with a mangled or truncated ExecStart, which is the one
# line that has to be right. `index`/`substr` treat the text as text.
render_unit() {
    BIN_DIR="$BIN_DIR" awk '
        BEGIN { from = "%h/.local/bin/dozed"; to = ENVIRON["BIN_DIR"] "/dozed" }
        {
            while ((i = index($0, from)) > 0) {
                $0 = substr($0, 1, i - 1) to substr($0, i + length(from))
            }
            print
        }
    ' "$REPO/packaging/dozed.service" > "$1"
}
if [ -e "$UNIT" ]; then
    render_unit "$UNIT.new"
    if cmp -s "$UNIT" "$UNIT.new"; then
        rm -f "$UNIT.new"
        say "keeping $UNIT (unchanged)"
    else
        say "keeping your existing $UNIT"
        say "the shipped unit differs; it is at $UNIT.new for you to compare"
        # Named explicitly, because this is the line that matters: a user who
        # went live and then upgrades must not be told their unit is fine when
        # the shipped one would have moved them back.
        if runs_live "$UNIT"; then
            say "NOTE: yours runs --live. That is preserved, and deliberately so."
        fi
    fi
else
    render_unit "$UNIT"
    say "wrote $UNIT"
    say "it starts in dry-run; edit ExecStart to go live, deliberately"
fi
sctl daemon-reload

step "Installing the starter config"
mkdir -p "$CONFIG_DIR"
if [ -e "$CONFIG_DIR/config.toml" ]; then
    say "keeping your existing $CONFIG_DIR/config.toml"
# Run the copy we just built rather than the installed one: $BIN_DIR may be on a
# noexec mount, and failing here must not abort an otherwise complete install —
# without a config file dozed simply uses the built-in defaults.
elif "$REPO/target/release/dozed" --write-default-config >/dev/null 2>&1; then
    say "wrote $CONFIG_DIR/config.toml"
else
    say "could not write a starter config; dozed will use its built-in defaults"
    say "write one later with: dozed --write-default-config"
fi

DESKTOP="$(detect_desktop)"
step "Focus source: detected $DESKTOP"

if [ "$WITH_HELPER" = 0 ]; then
    say "skipping the helper (--no-helper)"
elif [ "$DESKTOP" = x11 ]; then
    say "X11 needs no helper at all: the daemon reads EWMH from the root window."
elif [ "$DESKTOP" = kwin ]; then
    if command -v kpackagetool6 >/dev/null; then
        # --upgrade covers the reinstall case; a plain --install fails if the
        # script is already there.
        kpackagetool6 --type=KWin/Script --install "$REPO/helpers/kwin" >/dev/null 2>&1 \
          || kpackagetool6 --type=KWin/Script --upgrade "$REPO/helpers/kwin" >/dev/null 2>&1 \
          || { say "kpackagetool6 could not install the script"; exit 1; }
        # Installing is not enabling: the script has to be switched on in kwinrc
        # and KWin has to be told to re-read it.
        kwriteconfig6 --file kwinrc --group Plugins --key "${KWIN_ID}Enabled" true
        if command -v qdbus6 >/dev/null; then
            qdbus6 org.kde.KWin /KWin reconfigure >/dev/null 2>&1 || true
        else
            gdbus call --session --dest org.kde.KWin --object-path /KWin \
                --method org.kde.KWin.reconfigure >/dev/null 2>&1 || true
        fi
        say "installed and enabled the KWin script"
        say "check it is running: journalctl --user -u plasma-kwin_wayland -f"
    else
        say "kpackagetool6 not found; install helpers/kwin through System Settings"
        say "  Settings > Window Management > KWin Scripts > Install from File"
    fi
elif [ "$DESKTOP" = gnome ]; then
    mkdir -p "$GNOME_EXT_DIR/$GNOME_UUID"
    cp "$REPO/helpers/gnome/extension.js" "$REPO/helpers/gnome/metadata.json" \
       "$GNOME_EXT_DIR/$GNOME_UUID/"
    say "installed to $GNOME_EXT_DIR/$GNOME_UUID"
    if command -v gnome-extensions >/dev/null; then
        # Under Wayland the Shell cannot be restarted in place, so a newly
        # installed extension is not loadable until the session restarts.
        if gnome-extensions enable "$GNOME_UUID" 2>/dev/null; then
            say "enabled"
        else
            say "could not enable it yet — log out and back in, then:"
            say "  gnome-extensions enable $GNOME_UUID"
        fi
    else
        say "enable it with: gnome-extensions enable $GNOME_UUID"
    fi
else
    say "no compositor helper for '$DESKTOP'."
    say "dozed will run but stay passive, holding nothing, until it can see focus."
fi

step "Restarting the daemon"
if [ "$WAS_RUNNING" = 0 ]; then
    say "none was running, so there is nothing to restart"
elif [ "$WITH_RESTART" = 0 ]; then
    say "left the running daemon alone (--no-restart)"
    say "it is still the old binary until: systemctl --user restart dozed.service"
else
    # Restarting is not free, and the cost is the same one §10.3 relies on: the
    # daemon restores everything it holds on SIGTERM, so the new one starts with
    # nothing held and every grace clock at zero. That is a few minutes of no
    # saving, and it is strictly better than the alternative — an install that
    # appears to have worked while the old binary goes on making the decisions.
    sctl restart dozed.service
    say "restarted, so it is running the binary this script just built"
    # Restarting is not the same as running. A unit that fails on start leaves
    # this script printing a cheerful summary about a daemon that is not there,
    # and the next thing the user does is wonder why nothing is happening.
    if ! sctl is-active --quiet dozed.service; then
        say ""
        say "WARNING: it is not running. What it said:"
        sctl status --no-pager --lines=10 dozed.service 2>&1 | sed 's/^/    /' || true
        say ""
        say "full log: journalctl --user -u dozed.service -b"
        exit 1
    fi
    # A trial survives this — the finished blocks are on disk and a live daemon
    # picks the rest up — so the only thing worth saying is what the restart
    # actually costs.
    say "everything it held was restored on the way out; grace starts again from now"
fi

step "Done"
if [ "$WAS_RUNNING" = 0 ]; then
    say "start it:   systemctl --user enable --now dozed.service"
fi
say "watch it:   dozectl status && dozectl watch"
say "after a day: dozectl report --since 24h"
say ""
if runs_live "$UNIT_DIR/dozed.service"; then
    say "This unit runs --live: applications will actually be frozen."
else
    say "It is in dry-run. Live mode is a separate, deliberate step:"
    say "  edit $UNIT_DIR/dozed.service, change --dry-run to --live"
    say "  then: systemctl --user restart dozed.service"
fi
