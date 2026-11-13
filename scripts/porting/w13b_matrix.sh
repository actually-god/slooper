#!/usr/bin/env bash
# W13b (docs/WINDOWS-PORT.md): distro smoke matrix for the SHIPPED Linux
# artifact — run the binary as-built (NEVER a rebuild) on each supported
# distro container that has ONLY the runtime libraries installed, and prove
# the one CLI mode that exists works there:
#
#   slooper --slooper-probe 2 /nonexistent
#
# prints ONE JSON verdict line on stdout and exits 0 with no window and no
# JACK (the W7 probe intercept runs before Backend::start). A binary built
# on a newer glibc than the container's dies at exec with exit 127
# ("GLIBC_2.xx' not found") — that is exactly what this matrix catches.
#
# usage: w13b_matrix.sh /abs/or/rel/path/to/slooper [image ...]
#   images default to the W13b set: archlinux:latest debian:12
#   ubuntu:22.04 fedora:40.  Requires docker; no root on the host needed.
#
# CI: release.yml runs this against the freshly built exe (release gate).
set -u

BIN="${1:?usage: w13b_matrix.sh /path/to/slooper [images...]}"
shift || true
BIN="$(readlink -f "$BIN")"
[ -x "$BIN" ] || { echo "FATAL: $BIN not executable"; exit 2; }

IMAGES=("$@")
if [ ${#IMAGES[@]} -eq 0 ]; then
  IMAGES=(archlinux:latest debian:12 ubuntu:22.04 fedora:40)
fi

# Runtime packages = the exe's NEEDED set (libX11, libstdc++, libgcc_s,
# libc/libm) MINUS what each distro base image already ships. libjack,
# Wayland and GL are dlopen'd by design and must NOT be needed for the
# probe to run — installing them here would hide a missing hard dep.
install_for() {
  case "$1" in
    archlinux*) echo 'pacman -Sy --noconfirm --needed libx11 >/dev/null 2>/dev/null' ;;
    debian*|ubuntu*) echo 'apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends libx11-6 libstdc++6 >/dev/null' ;;
    fedora*) echo 'dnf -y -q install libX11 libstdc++ >/dev/null' ;;
    *) echo '' ;;
  esac
}

# Inside the container: install, then run the probe capturing the child's
# stdout/stderr to files so packaging noise can never impersonate a verdict.
run_probe() {
  local img="$1" inst
  inst="$(install_for "$img")"
  timeout 600 docker run --rm --cpus=2 --memory=2g -v "$BIN:/slooper:ro" "$img" bash -c "
set -u
$inst
/slooper --slooper-probe 2 /nonexistent >/tmp/p.out 2>/tmp/p.err
echo PROBE_RC=\$?
echo '<<<W13B-OUT>>>'; cat /tmp/p.out
echo '<<<W13B-ERR>>>'; cat /tmp/p.err
echo '<<<W13B-END>>>'
" 2>&1
}

verdict() { # $1=image $2=full capture -> 0 pass
  local img="$1" cap="$2"
  local rc out
  rc="$(sed -n 's/^PROBE_RC=//p' <<<"$cap" | head -1)"
  out="$(sed -n '/<<<W13B-OUT>>>/,/<<<W13B-ERR>>>/p' <<<"$cap" | sed '1d;$d')"
  local nlines; nlines="$(grep -c . <<<"$out")"
  # verdict shape: ONE line JSON array [{...}] naming the probe target
  if [ "$rc" = 0 ] && [ "$nlines" = 1 ] && [[ "$out" == \[* ]] && [[ "$out" == *\] ]] && [[ "$out" == *nonexistent* ]]; then
    echo "  PASS: one JSON verdict line, exit 0 (no window, no JACK)"
    return 0
  fi
  echo "  FAIL: rc='${rc:-?}' stdout_lines=${nlines}"
  echo "  stdout: ${out:0:200}"
  sed -n '/<<<W13B-ERR>>>/,/<<<W13B-END>>>/p' <<<"$cap" | sed '1d;$d' | grep -v '^$' | head -4 | sed 's/^/  stderr: /'
  grep -oE "version \`GLIBC[^\x27]*' not found|error while loading shared libraries[^$]*" <<<"$cap" | head -1 | sed 's/^/  cause:  /'
  return 1
}

echo "W13b distro smoke matrix — binary: $BIN"
echo "  demand: $(objdump -T "$BIN" 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -uV | tail -1 | sed 's/^/GLIBC /')  sha256: $(sha256sum "$BIN" | cut -c1-16)"
fail=0
for img in "${IMAGES[@]}"; do
  echo "── $img"
  cap="$(run_probe "$img")" || { echo "  FAIL: container error"; fail=1; continue; }
  verdict "$img" "$cap" || fail=1
done
[ $fail = 0 ] && echo "MATRIX: all ${#IMAGES[@]} distros PASS" || echo "MATRIX: FAILURES"
exit $fail
