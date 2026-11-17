#!/usr/bin/env bash
#
# Rebuild dozed, stand the systemd service down, and run a foreground dry-run
# against packaging/verify.toml — the config that acts on AC and in 45s, so
# decisions are visible in minutes instead of requiring an unplugged laptop.
#
#   ./packaging/verify-run.sh              rebuild and run
#   ./packaging/verify-run.sh --no-build   skip the rebuild
#   ./packaging/verify-run.sh --config X   use a different config
#
# Ctrl-C to stop. The service is put back the way it was found, because a
# temporary test should not leave the system in a different state.
#
# Nothing here can freeze anything: verify.toml is dry-run and --dry-run is
# passed explicitly as well, so both would have to change for this to act.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="$REPO/packaging/verify.toml"
BUILD=1
UNIT=dozed.service

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) BUILD=0 ;;
        --config) CONFIG="$2"; shift ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

say()  { printf '  %s\n' "$*"; }
step() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# --- a writable, executable TMPDIR ---------------------------------------
# /tmp is mounted noexec on this machine, which breaks cargo build scripts and
# doctests with a bare "Permission denied" that names nothing useful.
can_exec() {
    local d="$1" f rc
    [ -d "$d" ] && [ -w "$d" ] || return 1
    f="$(mktemp "$d/.dozed-exec-XXXXXX" 2>/dev/null)" || return 1
    printf '#!/bin/sh\nexit 0\n' > "$f"
    chmod +x "$f" 2>/dev/null || { rm -f "$f"; return 1; }
    "$f" >/dev/null 2>&1; rc=$?
    rm -f "$f"
    return $rc
}
if ! can_exec "${TMPDIR:-/tmp}"; then
    export TMPDIR="$HOME/.cache/dozed-build-tmp"
    mkdir -p "$TMPDIR"
fi

[ -r "$CONFIG" ] || { echo "no config at $CONFIG" >&2; exit 1; }

# --- build ----------------------------------------------------------------
if [ "$BUILD" = 1 ]; then
    step "Rebuilding (release)"
    ( cd "$REPO" && cargo build --release --workspace )
    say "built"
else
    step "Skipping the rebuild (--no-build)"
fi
DOZED="$REPO/target/release/dozed"
[ -x "$DOZED" ] || { echo "no binary at $DOZED — run without --no-build" >&2; exit 1; }

# Fail before stopping anything if the config is bad.
step "Checking the config"
"$DOZED" --config "$CONFIG" --check | sed 's/^/  /'

# --- stand the service down, remembering how we found it ------------------
WAS="$(systemctl --user is-active "$UNIT" 2>/dev/null || true)"
restore() {
    printf '\n'
    if [ "$WAS" = active ]; then
        step "Restoring $UNIT"
        systemctl --user start "$UNIT" 2>/dev/null \
            && say "started again — back to the packaged dry-run config" \
            || say "could not restart it: systemctl --user start $UNIT"
    else
        say "$UNIT was $WAS before this run; leaving it alone"
    fi
}
trap restore EXIT

if [ "$WAS" = active ]; then
    step "Stopping $UNIT"
    # Only one daemon can own org.dozed.Daemon, so the service has to go before
    # this one can claim it — and a clean stop thaws anything it holds (§10.3).
    systemctl --user stop "$UNIT"
    say "stopped (it will be started again on exit)"
fi

# --- run ------------------------------------------------------------------
step "Running dry-run against $(basename "$CONFIG")"
say "config forces on_ac = enabled and a 45s base grace, so decisions are"
say "visible in a couple of minutes rather than needing an unplugged laptop."
say ""
say "In another terminal:"
say "  dozectl apps                  # visibility, CPU and wakeups per app"
say "  dozectl watch                 # decisions as they happen"
say "  dozectl why <app> --rules     # the decision, and every rule that did not fire"
say "  ./packaging/collect-evidence.sh"
say ""
say "Sampling needs two ticks, so wakeups read 0 for the first minute or two."
say "Ctrl-C to stop."
printf '\n'

# Backgrounded and waited on, rather than run in the foreground, so that signals
# are forwarded explicitly.
#
# Ctrl-C in a terminal signals the whole foreground process group and both would
# get it anyway — but run under nohup, systemd-run or a CI harness there is no
# such group, and a bare `kill` on this script would leave the daemon orphaned
# and still holding the bus name. Forwarding covers both.
#
# Not `exec`, because the EXIT trap has to run afterwards. State goes to the
# usual XDG location, which is what lets dozectl find this run's trace with no
# env juggling.
RUST_LOG="${RUST_LOG:-dozed=info}" "$DOZED" --dry-run --config "$CONFIG" &
DAEMON=$!
# SIGTERM is dozed's clean-exit path: it thaws everything on the way out (§10.3).
trap 'kill -TERM "$DAEMON" 2>/dev/null' INT TERM
wait "$DAEMON" || true
