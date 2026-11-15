#!/usr/bin/env bash
# Tier-2 FULL gate — run before a release tag (release docs §1) and whenever
# you want belt-and-braces. CI runs this same coverage on every push/PR, so
# the local quick gate (scripts/test-quick.sh) is the day-to-day loop.
#
# Steps (every step RUNS even after a failure; nonzero exit if any failed):
#  1. lib suite, fast tier      (same set as the quick gate — catches edits
#                                that broke the slow set's shared fixtures)
#  2. lib suite, SLOW tier      (only the #[ignore = "slow-tier: …"] pins,
#                                in ITS OWN process — this isolation is the
#                                old 3-chunk dlopen-deadlock protocol, now
#                                a mechanism instead of a filter string)
#  3. e2e                       (live JACK/PipeWire in real time; the tests
#                                self-skip when no server is up)
#  4. doctests
#  5. clippy zero (native, --all-targets)
#  6. gnullvm cross: cargo check --all-targets (Windows parity gate)
#  7. CHANGELOG/lock/version drift check (--locked already; informational)
#
# Read the VERDICT from the `test result:` lines in the summary table below
# — the tail of a plugin-heavy log is sfizz leak-detector shutdown noise.
#
# Knobs: SLOOPER_TEST_PROFILE=citest (default)  SLOOPER_E2E=0 to skip step 3
set -uo pipefail
cd "$(dirname "$0")/.."
PROFILE="${SLOOPER_TEST_PROFILE:-citest}"
LOGDIR="target/test-logs"
mkdir -p "$LOGDIR"
declare -a NAMES STEPS RCS LOGS
t0=$(date +%s)

run_step() { # name timeout cmd... (output -> $LOGDIR/<name>.log)
  local name=$1 tmo=$2; shift 2
  echo "== $name =="
  timeout "$tmo" "$@" > "$LOGDIR/$name.log" 2>&1; local rc=$?
  local res; res=$(grep -E "^test result:" "$LOGDIR/$name.log" | tail -1)
  echo "   rc=$rc ${res:+| }$res"
  NAMES+=("$name"); STEPS+=("$*"); RCS+=("$rc"); LOGS+=("$LOGDIR/$name.log")
  [ $rc -eq 0 ] || return 1
  # libtest EXIT 0 with no results line = a mid-run trap was hushed: FAILURE
  case "$*" in
    *cargo*test*) [ -n "$res" ] || { echo "   no 'test result' line — treat as FAILED"; return 1; } ;;
  esac
}

fail=0
run_step lib-fast 900 cargo test --profile "$PROFILE" --lib --locked || fail=1
run_step lib-slow 900 cargo test --profile "$PROFILE" --lib --locked -- --ignored || fail=1
if [ "${SLOOPER_E2E:-1}" = 1 ]; then
  run_step e2e 900 cargo test --profile "$PROFILE" --test e2e --locked || fail=1
fi
run_step doctests 600 cargo test --profile "$PROFILE" --doc --locked || fail=1
run_step clippy-native 900 cargo clippy --all-targets --locked || fail=1
if grep -qE '^warning: |^error' "$LOGDIR/clippy-native.log"; then
  echo "   clippy NOT zero (rc was 0 but warnings present) — failing"
  NAMES+=(clippy-zero); STEPS+=("grep clippy-zero"); RCS+=(1); LOGS+=("$LOGDIR/clippy-native.log")
  fail=1
else
  NAMES+=(clippy-zero); STEPS+=("grep clippy-zero"); RCS+=(0); LOGS+=("$LOGDIR/clippy-native.log")
fi
run_step gnullvm-check 900 cargo check --target x86_64-pc-windows-gnullvm --all-targets --locked || fail=1

echo
echo "================= FULL SUITE SUMMARY ($(( $(date +%s) - t0 ))s) ================="
for i in "${!NAMES[@]}"; do
  printf "  %-16s rc=%-3s  %s\n" "${NAMES[$i]}" "${RCS[$i]}" "$(grep -E '^test result:' "${LOGS[$i]}" | tail -1 | cut -c1-72)"
done
if [ $fail -eq 0 ]; then
  echo "FULL GREEN — ok to proceed with the release dance (see release-build.md)."
else
  echo "FULL RED — see $LOGDIR/<step>.log; failing test names above."
fi
exit $fail
