#!/usr/bin/env bash
#
# Collect one snapshot of everything needed to judge whether dozed is working.
#
# Run it a few minutes after starting the daemon, then again after a day. It
# writes a timestamped directory and prints a verdict on the four things that can
# be wrong without being obvious:
#
#   1. Is the focus source actually reporting? Without it dozed is passive by
#      design (DESIGN.md §10.2) and will never do anything.
#   2. Is every exemption source connected? A rule matching on state nobody sets
#      can never fire, and that failure is silent.
#   3. Is anything actually being decided, or is it sitting in grace forever?
#   4. What is dozed itself costing?
#
#   ./packaging/collect-evidence.sh [OUTDIR]

set -uo pipefail

OUT="${1:-$HOME/dozed-evidence/$(date +%Y%m%d-%H%M%S)}"
DOZECTL="${DOZECTL:-dozectl}"
command -v "$DOZECTL" >/dev/null || DOZECTL="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/dozectl"

mkdir -p "$OUT"
echo "collecting into $OUT"

# --- raw captures -------------------------------------------------------
"$DOZECTL" status            > "$OUT/status.txt"       2>&1
"$DOZECTL" --json status     > "$OUT/status.json"      2>&1
"$DOZECTL" apps              > "$OUT/apps.txt"         2>&1
"$DOZECTL" report --since 24h > "$OUT/report.txt"      2>&1
"$DOZECTL" journal --since 24h > "$OUT/journal.txt"    2>&1
"$DOZECTL" trace --list      > "$OUT/traces.txt"       2>&1

# Why, for every app it knows about — the per-app derivations, with the rules
# that did not fire.
while read -r app _; do
    [ -z "$app" ] && continue
    [ "$app" = "APP" ] && continue
    "$DOZECTL" why "$app" --rules >> "$OUT/why.txt" 2>&1
    echo >> "$OUT/why.txt"
done < "$OUT/apps.txt"

# The trace is the actual evidence: what dozed saw, concluded and did.
"$DOZECTL" trace > "$OUT/trace-full.txt" 2>&1
"$DOZECTL" trace --decisions > "$OUT/trace-decisions.txt" 2>&1
# And prove the recording replays to itself, which is what makes it trustworthy.
"$DOZECTL" replay > "$OUT/replay-selfcheck.txt" 2>&1
REPLAY_RC=$?

# Environment, so a trace can be read months later.
{
    echo "date:            $(date -Is)"
    echo "XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-}"
    echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-}"
    echo "DISPLAY=${DISPLAY:-}"
    echo "XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-}"
    echo "kernel:          $(uname -r)"
    echo "dozed:           $("$DOZECTL" --version 2>/dev/null)"
    echo "on battery:      $(cat /sys/class/power_supply/BAT*/status 2>/dev/null | head -1)"
} > "$OUT/environment.txt"

# --- verdict ------------------------------------------------------------
echo
echo "── verdict ─────────────────────────────────────────────"
warn=0
say_ok()   { printf '  ok    %s\n' "$*"; }
say_warn() { printf '  WARN  %s\n' "$*"; warn=$((warn+1)); }

if ! grep -q . "$OUT/status.txt" || grep -qi "no daemon" "$OUT/status.txt"; then
    say_warn "no daemon running — start it: systemctl --user start dozed.service"
    echo; echo "collected what there was into $OUT"; exit 1
fi

# Tolerate column-width changes rather than matching exact spacing.
if awk '$1=="mode"{print $2}' "$OUT/status.txt" | grep -qx "dry-run"; then
    say_ok "dry-run (nothing is being touched)"
else
    say_warn "NOT in dry-run — check 'mode' in status.txt"
fi

if grep -q "focus source  ok" "$OUT/status.txt"; then
    say_ok "focus source reporting: $(awk '$1=="source"{$1="";print}' "$OUT/status.txt" | cut -c2-)"
else
    say_warn "focus source LOST — dozed is passive and will never act."
    say_warn "  On Wayland this means the compositor helper is not installed or not enabled."
    say_warn "  Install it: ./packaging/install.sh"
fi

apps=$(grep -c . "$OUT/apps.txt")
if [ "$apps" -gt 1 ]; then
    say_ok "$((apps-1)) app(s) discovered"
else
    say_warn "no apps discovered — the helper may be installed but not enabled"
fi

# Any source that has never reported is a rule that cannot fire.
if grep -q "NOT REPORTING" "$OUT/status.txt"; then
    say_warn "an exemption source is not reporting:"
    grep "NOT REPORTING" "$OUT/status.txt" | sed 's/^/      /'
else
    say_ok "every applicable exemption source is connected"
fi

# `grep -c` exits non-zero on no match while still printing 0, so `|| echo 0`
# would print it twice.
decisions=$(grep -c "decide" "$OUT/trace-decisions.txt" 2>/dev/null); decisions=${decisions:-0}
intents=$(grep -c "intent" "$OUT/trace-decisions.txt" 2>/dev/null); intents=${intents:-0}
say_ok "$decisions derivation(s) and $intents intent(s) recorded"
if [ "$intents" = 0 ] && [ "$decisions" -gt 0 ]; then
    # Don't guess at the cause — the daemon already recorded it per app.
    say_warn "decisions but no intents yet. dozed's own reasons, per app:"
    sort -u "$OUT/why.txt" 2>/dev/null | grep -E "^  because" \
        | sed 's/^  because */      /' | sort | uniq -c | sort -rn | head -6 \
        | sed 's/^/    /'
    say_warn "  Early in a session this is normal. If it persists on a charger, check"
    say_warn "  the config: grace is doubled on mains, and with 'on_ac = disabled'"
    say_warn "  dozed does nothing at all while plugged in."
fi

if [ "$REPLAY_RC" = 0 ] && grep -q "identical to the recording" "$OUT/replay-selfcheck.txt"; then
    say_ok "the trace replays to itself exactly (the planner is deterministic)"
else
    say_warn "self-replay did not reproduce the recording — see replay-selfcheck.txt"
fi

cost=$(grep "own wakeups" "$OUT/status.txt" | awk '{print $3}')
say_ok "dozed's own cost: ${cost:-unknown}"

echo
echo "collected into $OUT"
echo "  status.txt   is it healthy, what is it costing"
echo "  why.txt      per app: the decision, and every rule that did not fire"
echo "  report.txt   what it would have saved"
echo "  trace-*.txt  what it saw and concluded, in order"
[ "$warn" -gt 0 ] && echo && echo "$warn thing(s) to look at above."
exit 0
