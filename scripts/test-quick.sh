#!/usr/bin/env bash
# Tier-1 dev-loop gate — the few-minute check after EVERY change.
#
#   quick (this script) : fast lib pins + gnullvm compile check + clippy.
#   full (test-full.sh) : + the #[ignore]-marked slow set, live-JACK e2e,
#                         doctests — run before a release tag. CI also runs
#                         the FULL set on every push/PR (windows-native is
#                         the parity arbiter), so quick-green is the
#                         iteration gate, never the merge gate.
#
# What quick does NOT cover (the slow tier, marked #[ignore = "slow-tier: …"]):
# plugin-dlopen families (vsthost/probe-spawn pins) and wall-time-heavy
# pins — historically the flaky/deadlock class. A NEW pin is quick-tier by
# default: add #[ignore = "slow-tier: <why>"] only when the test dlopens a
# plugin binary, spawns probe/editor processes, or sleeps in real time
# beyond ~2 s. Then full + CI cover it.
#
# Knobs: SLOOPER_TEST_PROFILE=citest|debug|release  (default citest = CI parity)
#        SLOOPER_SKIP_CLIPPY=1    drop the clippy step (tests only, fastest)
#        SLOOPER_SKIP_WIN=1       drop the gnullvm compile check
set -uo pipefail
cd "$(dirname "$0")/.."
PROFILE="${SLOOPER_TEST_PROFILE:-citest}"
LOGDIR="target/test-logs"
mkdir -p "$LOGDIR"
rc_all=0
t0=$(date +%s)

step() { echo; echo "== $* =="; }

# cargo's exit code is NOT trustworthy for test steps: the v35 sfizz
# exit-hush (test_hush_vendor_exit_trap) can turn an aborted/failed plugin
# suite into exit 0. A test step passes only if the log contains at least
# one `test result:` line AND none of them say FAILED.
gate_log() { # logpath
  local log=$1
  local res
  res=$(grep -E "^test result:" "$log" | tail -1)
  if [ -z "$res" ]; then echo "  no 'test result' line in $log — FAILURE (hushed trap?)"; return 1; fi
  if echo "$res" | grep -q FAILED; then
    echo "$res"
    sed -n '/^failures:$/,/^$/p' "$log" | grep '^    ' | sed 's/^    /  FAILED: /' | sort -u
    return 1
  fi
  echo "  $res"
  return 0
}

step "[1/3] lib suite — quick tier (profile=$PROFILE; slow tier skipped by #[ignore])"
# Non-ignored set only. Single-threaded via .cargo/config.toml RUST_TEST_THREADS=1
# (test_app set_vars global env — do not parallelize).
cargo test --profile "$PROFILE" --lib --locked 2>&1 | tee "$LOGDIR/quick-lib.log" || true
gate_log "$LOGDIR/quick-lib.log" || rc_all=1

if [ "${SLOOPER_SKIP_WIN:-0}" != 1 ]; then
  step "[2/3] gnullvm compile check (Windows parity — compile only)"
  cargo check --target x86_64-pc-windows-gnullvm --all-targets --locked || rc_all=1
else
  step "[2/3] gnullvm compile check — SKIPPED (SLOOPER_SKIP_WIN=1)"
fi

if [ "${SLOOPER_SKIP_CLIPPY:-0}" != 1 ]; then
  step "[3/3] clippy zero (native, --all-targets)"
  cargo clippy --all-targets --locked > "$LOGDIR/quick-clippy.log" 2>&1 || true
  if grep -qE '^warning: |^error' "$LOGDIR/quick-clippy.log"; then
    echo "CLIPPY NOT ZERO — warnings/errors in $LOGDIR/quick-clippy.log"
    grep -E '^warning: |^error' "$LOGDIR/quick-clippy.log" | head -5
    rc_all=1
  fi
else
  step "[3/3] clippy — SKIPPED (SLOOPER_SKIP_CLIPPY=1)"
fi

echo
if [ $rc_all -eq 0 ]; then
  echo "QUICK GREEN ($(( $(date +%s) - t0 ))s). Slow tier + e2e NOT run: scripts/test-full.sh before releasing."
else
  echo "QUICK RED — logs in $LOGDIR/"
fi
exit $rc_all
