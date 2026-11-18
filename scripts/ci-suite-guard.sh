#!/usr/bin/env bash
# v84 (board card t_dc8d6666): the CI suite verdict is THE LOG, never cargo's
# exit code. The v35/v73 vendor exit-hush (src/lib.rs hush_vendor_exit_trap,
# SIGTRAP -> _exit(0), armed in TEST BUILDS ON BOTH OSes) can force exit 0
# AFTER libtest printed `test result: FAILED` — a bare `cargo test` step then
# green-lights a dead pin (bit v79 on the windows-native runner). And the
# GitHub runner log stream loses the TAIL of these streams on Windows
# (v78/v79 forensics: byte-identical ~68KB cut, zero `test result:` lines in
# the stored log), so the grep runs against a workspace-local tee file —
# never the stored job log.
#
#   usage: ci-suite-guard.sh <label> <logpath> <cargo-test command...>
#
# Streams output live (tee), keeps the hush (rc deliberately ignored), then
# fails the step unless EVERY `test result:` line reports `; 0 failed`,
# there is no `... FAILED` test line, and there is AT LEAST ONE summary line
# (a run truncated by a mid-suite vendor death must not pass vacuously —
# same doctrine as scripts/test-quick.sh gate_log). Always prints a final
# `SUMMARY-GUARD ok|FAILED` marker (step log + GITHUB_STEP_SUMMARY, which
# uploads over a different channel and survives stream truncation), and on
# failure echoes the FAILED lines + the libtest `---- <test> stdout ----`
# sections for the postmortem. ANSI codes (CARGO_TERM_COLOR=always) are
# stripped into <log>.clean before matching; the raw log stays byte-true.
set -u

label="$1"; log="$2"; shift 2
if [ "$#" -eq 0 ]; then echo "SUMMARY-GUARD FAILED ($label): no command given"; exit 2; fi
mkdir -p "$(dirname "$log")"

# Live stream + local copy; cargo's rc is the known liar — the grep decides.
# `|| true` is load-bearing: CI bash steps run with `-e -o pipefail`, and an
# honestly-nonzero cargo rc must not abort the wrapper before the verdict.
"$@" 2>&1 | tee "$log" || true

clean="$log.clean"
sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$log" >"$clean" 2>/dev/null || clean="$log"

results="$(grep -E '^test result:' "$clean" || true)"
failedtests="$(grep -E ' \.\.\. FAILED' "$clean" || true)"

verdict=ok
notes=""
if [ -z "$results" ]; then
  verdict=FAILED
  notes="NO 'test result:' summary line — truncated/aborted run (hush'd mid-suite death?)"
else
  badres="$(printf '%s\n' "$results" | grep -vE '; 0 failed' || true)"
  if [ -n "$badres" ]; then
    verdict=FAILED
    printf '%s\n' "$badres" | sed 's/^/guard: summary line with nonzero failed: /'
  fi
fi
if [ -n "$failedtests" ]; then
  verdict=FAILED
fi

if [ "$verdict" = ok ]; then
  n="$(printf '%s\n' "$results" | grep -c .)"
  echo "SUMMARY-GUARD ok ($label): $n summary line(s), every one '0 failed'"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    { echo "**suite-guard \`$label\`: ok** — $n \`test result:\` line(s), 0 failed"; echo; printf '%s\n' "$results"; } >>"$GITHUB_STEP_SUMMARY"
  fi
  exit 0
fi

echo "SUMMARY-GUARD FAILED ($label)${notes:+ — $notes}"
echo "--- guard: test result lines ---"
printf '%s\n' "$results"
echo "--- guard: FAILED test lines ---"
printf '%s\n' "$failedtests"
echo "--- guard: libtest failure sections (stdout/stderr per failing test) ---"
awk '/^---- /{p=1} p' "$clean"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  { echo "**suite-guard \`$label\`: FAILED**${notes:+ — $notes}"; echo; echo '```'; printf '%s\n' "$results" "$failedtests"; echo '```'; } >>"$GITHUB_STEP_SUMMARY"
fi
echo "guard: full log retained at $log (+ .clean)"
exit 1
