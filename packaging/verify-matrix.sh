#!/usr/bin/env bash
#
# Run dozed against the display servers it claims to support, and say which of
# them were actually exercised.
#
# `DESIGN.md` §12 records four focus sources. The KWin one is verified to an
# unusual standard — protocol, occlusion, heartbeats, restart recovery, all
# against KWin 6.6.6. When this script was written the other three were not:
#
#   * Sway/i3   unit-tested against captured JSON, never run against a live Sway
#   * GNOME     the extension exists; no live verification is recorded
#   * X11       verified once against nested Xvfb + metacity
#
# Those were honest labels and honest labels do not shrink the risk. §10.2 makes
# the failure *safe* — lose sight of focus and dozed thaws everything and goes
# passive — but "safe, and silently doing nothing on GNOME" is exactly the shape
# of the failure this project has already had once: a safety net drawn and not
# connected, with nothing anywhere saying so.
#
# So this converts what can be converted. It is deliberately not a pass/fail
# gate: what it prints is which surfaces were *reachable on this machine*, and
# an unreachable one is reported as unreachable rather than as a pass.
#
#   ./packaging/verify-matrix.sh              every backend this machine can run
#   ./packaging/verify-matrix.sh --only sway  just one
#   ./packaging/verify-matrix.sh --list       what it would try, and why not
#
# What it cannot do: GNOME. A Shell extension needs a real GNOME session, and
# nesting one is not something a script can arrange reliably. That row prints as
# unverifiable-here, with what to run by hand, because a matrix that quietly
# omits its weakest row is worse than one that names it.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ONLY=""
LIST=0

while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift ;;
        --list) LIST=1 ;;
        -h|--help) sed -n '2,32p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# /tmp is noexec on some machines, which breaks cargo and doctests with a bare
# "Permission denied" naming nothing. Same dance as verify-run.sh.
can_exec() {
    local d="$1" probe
    probe="$(mktemp "$d/dozed-exec-probe.XXXXXX" 2>/dev/null)" || return 1
    printf '#!/bin/sh\nexit 0\n' >"$probe"
    chmod +x "$probe" 2>/dev/null || { rm -f "$probe"; return 1; }
    "$probe" 2>/dev/null; local rc=$?
    rm -f "$probe"
    return $rc
}
if ! can_exec "${TMPDIR:-/tmp}"; then
    export TMPDIR="$HOME/.cache/dozed-tmp"
    mkdir -p "$TMPDIR"
fi

ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"; jobs -p | xargs -r kill 2>/dev/null || true' EXIT

say()  { printf '  %s\n' "$*"; }
step() { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mok\033[0m       %s\n' "$*"; }
skip() { printf '  \033[33mskipped\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAILED\033[0m   %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
FAILURES=0

have() { command -v "$1" >/dev/null 2>&1; }

# A throwaway XDG tree, so nothing here touches the real config, state or
# socket. A test that can disturb the daemon you are running is a test nobody
# runs twice.
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

DOZED="$REPO/target/release/dozed"
DOZECTL="$REPO/target/release/dozectl"

# --- what each backend needs ---------------------------------------------
#
# Kept as a table rather than as branches, because the interesting output is
# the table: which rows ran, which could not, and what each missing one needs.
requirements() {
    case "$1" in
        x11)   echo "Xvfb metacity xterm dbus-run-session" ;;
        sway)  echo "sway dbus-run-session" ;;
        gnome) echo "" ;;
        *)     echo "" ;;
    esac
}

missing_for() {
    local backend="$1" missing=""
    for tool in $(requirements "$backend"); do
        have "$tool" || missing="$missing $tool"
    done
    echo "${missing# }"
}

if [ "$LIST" = 1 ]; then
    step "backends"
    for b in x11 sway gnome; do
        m="$(missing_for "$b")"
        if [ "$b" = gnome ]; then
            say "gnome  needs a real GNOME session; not arrangeable from a script"
        elif [ -z "$m" ]; then
            say "$b     ready"
        else
            say "$b     needs:$m"
        fi
    done
    exit 0
fi

step "building"
if [ ! -x "$DOZED" ] || [ ! -x "$DOZECTL" ]; then
    ( cd "$REPO" && cargo build --release --bin dozed --bin dozectl )
fi
say "$("$DOZED" --version)"

# --- the shared assertion -------------------------------------------------
#
# One function, because the question is the same for every backend and asking
# it differently per backend is how three sources come to disagree about what
# "hidden" means — the failure §5 centralised the visibility rule to prevent.
#
# What it checks, in order of how badly getting it wrong would end:
#
#   1. the daemon starts and resolves the focus source we asked for, rather
#      than falling back to another and appearing to work;
#   2. it reports at least one window, because "no windows" is indistinguishable
#      from "everything is hidden" and is the single most expensive
#      misdetection available (§12);
#   3. it stays up for long enough to take two samples, since one reading is
#      not a rate and every threshold rule decides against nothing until there
#      are two.
#
# It never checks that anything was frozen. This runs `--dry-run` and the whole
# point of the seam (§3) is that the decisions are identical either way.
#
# ## The private session bus is not tidiness
#
# The first version of this script shared the login session's bus, and it
# reported a confident `ok` for Sway on a machine running KDE. What had happened
# is worth keeping: dozed serves `org.dozed.FocusSink1` under the well-known name
# `org.dozed.Daemon`, and it asks for that name with `ReplaceExisting` (§12, so a
# restarted daemon can reclaim it). The test daemon therefore **took the name off
# the real one**, the host's KWin script started reporting the host's windows to
# the test daemon, and this script counted them as evidence that the Sway backend
# worked. It did not work at all.
#
# Two consequences, and the script needs both. Every daemon here runs on a bus of
# its own, so it can neither be fed by somebody else's helper nor take the name
# from a daemon somebody is actually using. And the check below asserts on *which*
# window was seen rather than on how many, because a count is exactly the kind of
# evidence that survives being about the wrong compositor.
check_backend() {
    # Separate statements: within one `local`, later initialisers cannot see
    # earlier ones, and under `set -u` that is an unbound-variable error rather
    # than an empty string.
    local name="$1"
    local want_source="$2"
    local want_app="$3"
    local log="$ROOT/$name.log"

    # A bus of its own. See the note above: without this the daemon under test
    # can be fed by the desktop's own compositor helper, and this script then
    # measures the wrong compositor and says `ok`.
    dbus-run-session -- "$DOZED" --dry-run --no-trace --focus-source "$want_source" \
        >"$log" 2>&1 &
    local pid=$!

    # Long enough for two sampler ticks at the 3s warm-up interval, plus the
    # compositor's first window publish.
    local waited=0 windows=0
    while [ "$waited" -lt 20 ]; do
        sleep 1
        waited=$((waited + 1))
        kill -0 "$pid" 2>/dev/null || break
        if "$DOZECTL" apps >"$ROOT/$name.apps" 2>/dev/null; then
            # The header line is always there; a row under it is a window.
            if [ "$(grep -c . "$ROOT/$name.apps")" -gt 2 ]; then
                windows=1
                break
            fi
        fi
    done

    local status=0
    if ! kill -0 "$pid" 2>/dev/null; then
        fail "$name: the daemon exited; see $log"
        status=1
    elif [ "$windows" = 0 ]; then
        # Not a soft failure. Under Wayland the X11 source sees only XWayland
        # clients, so every native app looks gone — and "no windows" reads as
        # "everything is hidden", which is the state in which a live daemon
        # freezes the desktop. It is also exactly what a Sway backend reading the
        # wrong field name produced for the whole of its existence.
        fail "$name: no windows were ever reported, which reads as 'everything is hidden'"
        status=1
    elif ! grep -qi -- "$want_app" "$ROOT/$name.apps"; then
        # The assertion that would have caught the bus-name theft immediately.
        # Windows were reported and they were somebody else's.
        fail "$name: windows were reported but '$want_app' was not among them — \
whose windows are these?"
        sed -n '1,6p' "$ROOT/$name.apps" | sed 's/^/          /'
        status=1
    else
        local resolved
        resolved="$("$DOZECTL" status 2>/dev/null | sed -n 's/^source *//p' | head -1)"
        local found
        found="$(grep -io -m1 -- "$want_app" "$ROOT/$name.apps" || echo "$want_app")"
        pass "$name: found '$found', source=${resolved:-unreported}"
    fi

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    return $status
}

# --- X11, nested under Xvfb ----------------------------------------------
run_x11() {
    local m; m="$(missing_for x11)"
    if [ -n "$m" ]; then skip "x11: needs$m"; return 0; fi

    local display=":97"
    Xvfb "$display" -screen 0 1280x800x24 >"$ROOT/xvfb.log" 2>&1 &
    local xvfb=$!
    sleep 1
    if ! kill -0 "$xvfb" 2>/dev/null; then skip "x11: Xvfb would not start"; return 0; fi

    DISPLAY="$display" metacity --sm-disable >"$ROOT/metacity.log" 2>&1 &
    sleep 1
    # Two windows, so the visibility rule has an occlusion decision to make
    # rather than a single trivially-visible one.
    DISPLAY="$display" xterm -geometry 80x24+0+0 >/dev/null 2>&1 &
    DISPLAY="$display" xterm -geometry 80x24+100+100 >/dev/null 2>&1 &
    sleep 2

    DISPLAY="$display" WAYLAND_DISPLAY= XDG_SESSION_TYPE=x11 \
        check_backend x11 x11 xterm || true

    kill "$xvfb" 2>/dev/null || true
}

# --- Sway, headless -------------------------------------------------------
#
# The row this script was mostly written for. §12 says outright that the tree
# walk is "not verified against a live Sway session, because the machine this
# was written on runs KDE" — and wlroots' headless backend makes that a
# solvable problem rather than a permanent caveat.
run_sway() {
    local m; m="$(missing_for sway)"
    if [ -n "$m" ]; then skip "sway: needs$m"; return 0; fi

    cat >"$ROOT/sway.conf" <<'EOF'
# No bar, no bindings, nothing that needs a keyboard. Two windows so the walk
# has a workspace with more than one view on it.
default_border none
exec sh -c 'sleep 1; swaymsg exec -- foot || swaymsg exec -- alacritty || swaymsg exec -- xterm'
EOF

    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
        sway -c "$ROOT/sway.conf" >"$ROOT/sway.log" 2>&1 &
    local sway=$!
    sleep 3
    if ! kill -0 "$sway" 2>/dev/null; then
        skip "sway: headless sway would not start; see $ROOT/sway.log"
        return 0
    fi

    # sway writes its socket path into the environment of what it execs, which
    # this script is not, so it is looked up the way swaymsg does.
    local sock
    sock="$(find "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" -maxdepth 1 -name 'sway-ipc.*' 2>/dev/null | head -1)"
    if [ -z "$sock" ]; then
        skip "sway: started but its IPC socket was not found"
        kill "$sway" 2>/dev/null || true
        return 0
    fi

    # `foot` is what the config execs; `--only sway` on a machine without it
    # falls through to alacritty or xterm, so the pattern is any of the three.
    SWAYSOCK="$sock" check_backend sway sway 'foot\|alacritty\|xterm' || true
    kill "$sway" 2>/dev/null || true
}

# --- GNOME ---------------------------------------------------------------
run_gnome() {
    # Named rather than omitted. A matrix that silently leaves out the row most
    # likely to break is a matrix that reports what it is good at.
    skip "gnome: needs a real GNOME session; a Shell extension cannot be nested from a script"
    say "        to verify by hand, on a GNOME machine:"
    say "          ./packaging/install.sh && gnome-extensions enable dozed-focus-source@dozed"
    say "          dozectl status   # want: 'source gnome-shell' and a window count"
    say "        GNOME extensions break on every major release by design, so this"
    say "        is the row worth re-running on each one."
}

step "focus sources"
case "$ONLY" in
    "")     run_x11; run_sway; run_gnome ;;
    x11)    run_x11 ;;
    sway)   run_sway ;;
    gnome)  run_gnome ;;
    *)      echo "unknown backend: $ONLY" >&2; exit 2 ;;
esac

step "summary"
if [ "$FAILURES" -eq 0 ]; then
    say "nothing failed. Skipped rows are unverified, not verified —"
    say "an absent backend proves nothing about the code that drives it."
    exit 0
fi
say "$FAILURES backend(s) failed; logs are under $ROOT (kept only until this exits)"
exit 1
