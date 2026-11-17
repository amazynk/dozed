#!/usr/bin/env bash
#
# Tests that `install.sh` does not undo a deliberate choice.
#
# The unit file is the one place a user makes the decision that matters — editing
# `--dry-run` to `--live`, which install.sh itself tells them to do — and the
# script used to overwrite it on every run. Observed on a live machine: a
# reinstall put a deliberately live daemon back into dry-run, and nothing in the
# output said so. A battery tool quietly switching itself off is the failure that
# is hardest to notice, because everything keeps working and simply stops saving
# anything.
#
# Runs the real script against a throwaway XDG tree, so it tests the shipped code
# path rather than a copy of it. `--no-helper` keeps it away from the desktop's
# own configuration; the only thing it touches outside the temp tree is
# `systemctl --user daemon-reload`, which re-reads unit files and does nothing
# else.
#
#   ./packaging/test-install.sh

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT

export XDG_BIN_HOME="$ROOT/bin"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_DATA_HOME="$ROOT/share"
UNIT="$XDG_CONFIG_HOME/systemd/user/dozed.service"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
ok()   { printf 'ok: %s\n' "$*"; }
skip() { printf 'skipped: %s\n' "$*"; }

# --- the daemon this test must not touch ---------------------------------
#
# Every path above is redirected into `$ROOT`, and `systemctl --user` is not: it
# talks to the running user manager over D-Bus, which knows nothing about the
# environment of whoever invoked the client.
#
# So this test used to reach straight past its own sandbox. It runs `install.sh`
# five times and then once more with `--uninstall`, and against a real session
# that was five forced restarts of a live daemon followed by a `disable --now`
# — which switched dozed off and removed its enable symlink. The test printed
# "all install.sh checks passed" and the daemon stayed off until somebody
# noticed, two days later, that it was not running.
#
# The irony is the point: this file exists to prove `install.sh` does not undo a
# deliberate choice, and it was undoing the largest one there is. `install.sh`
# now guards every systemctl call behind `manages_our_unit`, and what follows is
# the check that the guard is still there — recorded as state read before and
# after, because the property is "nothing moved" and only a before and an after
# can say that.
BEFORE_STATE=""
if command -v systemctl >/dev/null 2>&1 \
    && systemctl --user show dozed.service >/dev/null 2>&1; then
    BEFORE_STATE="$(systemctl --user show -p ActiveState -p UnitFileState -p MainPID \
        --value dozed.service 2>/dev/null | tr '\n' ' ')"
fi

if [ ! -x "$REPO/target/release/dozed" ]; then
    echo "build first: cargo build --release" >&2
    exit 2
fi

run() { "$REPO/packaging/install.sh" --no-helper >"$ROOT/out.txt" 2>&1 || { cat "$ROOT/out.txt"; fail "install.sh exited non-zero"; }; }

# 1. A fresh install writes the unit, in dry-run.
run
[ -f "$UNIT" ] || fail "no unit written"
grep -qE '^ExecStart=.*--dry-run' "$UNIT" || fail "a fresh unit must start in dry-run"
[ ! -e "$UNIT.new" ] || fail "a fresh install must not leave a .new alongside"
# The summary must agree with the unit it just wrote. It did not: the unit's own
# comments mention --live, so an unanchored grep reported a fresh dry-run install
# as "applications will actually be frozen".
grep -q "It is in dry-run" "$ROOT/out.txt" || fail "the summary should say dry-run"
! grep -q "runs --live" "$ROOT/out.txt" \
    || fail "the summary claims live on a unit whose ExecStart says --dry-run"
ok "a fresh install writes a dry-run unit, and says so"

# 2. The deliberate edit: dry-run to live.
#
# Anchored to the ExecStart line, because that is the line a person edits — and
# because the unit's own comments mention both spellings, so an unanchored `sed`
# rewrites the prose too and the file then differs from the shipped one for a
# reason that has nothing to do with the setting. That is exactly what the first
# version of this test did, and it failed against a correct install.sh.
sed -i '/^ExecStart=/s|--dry-run|--live|' "$UNIT"

# 3. Reinstalling must not undo it. This is the whole point of the file.
run
grep -qE '^ExecStart=.*--live' "$UNIT" || fail "reinstalling reverted --live to --dry-run"
[ -f "$UNIT.new" ] || fail "the shipped unit should be offered alongside as .new"
grep -qE '^ExecStart=.*--dry-run' "$UNIT.new" || fail ".new should hold the shipped version"
grep -q "keeping your existing" "$ROOT/out.txt" || fail "it must say it kept the unit"
grep -q -- "yours runs --live" "$ROOT/out.txt" \
    || fail "it must call out that the live choice was preserved"
ok "reinstalling preserves --live and offers the shipped unit as .new"

# 4. The closing advice must not tell a live user they are in dry-run.
grep -q "runs --live: applications will actually be frozen" "$ROOT/out.txt" \
    || fail "the summary still claims dry-run on a live unit"
! grep -q "It is in dry-run" "$ROOT/out.txt" \
    || fail "the summary contradicts itself on a live unit"
ok "the closing summary reflects the unit that is actually installed"

# 5. An unchanged unit leaves no clutter. Otherwise every upgrade drops a .new
#    nobody needs to read, and the ones that matter stop standing out.
rm -f "$UNIT.new"
sed -i '/^ExecStart=/s|--live|--dry-run|' "$UNIT"
run
if [ -e "$UNIT.new" ]; then
    echo "--- unexpected difference ---" >&2
    diff "$UNIT" "$UNIT.new" >&2 || true
    fail "an identical unit must not produce a .new"
fi
grep -q "unchanged" "$ROOT/out.txt" || fail "it should say the unit is unchanged"
ok "an unchanged unit produces no .new"

# 6. The config keeps its existing promise, which is where this behaviour was
#    copied from.
printf '# mine\n' > "$XDG_CONFIG_HOME/dozed/config.toml"
run
grep -q "^# mine" "$XDG_CONFIG_HOME/dozed/config.toml" || fail "the config was overwritten"
ok "an existing config is still left alone"

# 7. Everything the installer writes, it must also take away. A tool that
#    leaves completions and manual pages behind after `--uninstall` is one whose
#    stale `dozectl-snooze(1)` outlives the subcommand it documents.
run
for f in "$XDG_DATA_HOME/bash-completion/completions/dozectl" \
         "$XDG_DATA_HOME/zsh/site-functions/_dozectl" \
         "$XDG_CONFIG_HOME/fish/completions/dozectl.fish" \
         "$XDG_DATA_HOME/man/man1/dozectl.1" \
         "$XDG_DATA_HOME/man/man1/dozectl-snooze.1" \
         "$XDG_DATA_HOME/man/man1/dozed.1" \
         "$XDG_DATA_HOME/applications/dozed-gui.desktop"; do
    [ -e "$f" ] || fail "the installer did not write $f"
done
ok "completions, manual pages and the desktop entry are installed"

"$REPO/packaging/install.sh" --uninstall >"$ROOT/out.txt" 2>&1 \
    || fail "uninstall failed: $(cat "$ROOT/out.txt")"
for f in "$XDG_DATA_HOME/bash-completion/completions/dozectl" \
         "$XDG_DATA_HOME/zsh/site-functions/_dozectl" \
         "$XDG_CONFIG_HOME/fish/completions/dozectl.fish" \
         "$XDG_DATA_HOME/man/man1/dozectl.1" \
         "$XDG_DATA_HOME/man/man1/dozectl-snooze.1" \
         "$XDG_DATA_HOME/man/man1/dozed.1" \
         "$XDG_DATA_HOME/applications/dozed-gui.desktop"; do
    [ -e "$f" ] && fail "uninstall left $f behind"
done
# The two things it must never take: they are the user's, not the package's.
[ -e "$XDG_CONFIG_HOME/dozed/config.toml" ] || fail "uninstall removed the config"
ok "and uninstall removes every one of them, keeping the config"

# The daemon somebody is actually running must be exactly where it was: same
# active state, same enable state, same pid. A restart would show as a new pid
# and a `disable --now` as both of the others — and either is this test reaching
# out of its sandbox and switching off the tool it is testing.
if [ -n "$BEFORE_STATE" ]; then
    AFTER_STATE="$(systemctl --user show -p ActiveState -p UnitFileState -p MainPID \
        --value dozed.service 2>/dev/null | tr '\n' ' ')"
    [ "$BEFORE_STATE" = "$AFTER_STATE" ] || fail \
        "this test disturbed the real dozed.service: was [$BEFORE_STATE], now [$AFTER_STATE]"
    ok "and the daemon on this machine was not touched at all"
else
    # A CI runner has no user session and no dozed.service, so there is nothing
    # to disturb and nothing to prove. Said out loud rather than passing quietly:
    # a check that silently does nothing is indistinguishable from one that
    # passed, which is the shape of the bug this whole block is about.
    skip "no dozed.service on this machine, so nothing could be disturbed"
fi

printf '\nall install.sh checks passed\n'
