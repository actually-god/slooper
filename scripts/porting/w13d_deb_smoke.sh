#!/usr/bin/env bash
# W13d (docs/WINDOWS-PORT.md): distro smoke for the .deb — install the
# PACKAGE (never a rebuild) in the Debian-family tier containers and
# prove, inside each: apt resolves + configures it, /usr/bin/slooper is
# on PATH afterwards, the desktop entry and icon landed, and the probe
# mode exits 0 with one JSON verdict line (loads without jack — same
# W13b proof, now through the installer). Purge must also succeed.
#
#   usage: w13d_deb_smoke.sh /path/to/slooper_*_amd64.deb [image ...]
#     images default to debian:12 ubuntu:22.04 (deb tier only — Arch
#     and Fedora get the tar.zst + W13b matrix). Requires docker.
#
# Install runs WITHOUT recommends so the gate proves Depends alone is
# complete — pipewire-jack arriving on a CI container must never be the
# difference between PASS and FAIL. A deb packaged from a newer-glibc
# build fails here at the probe exec (exit 127, 'GLIBC_2.xx not found'),
# which is the sabotage pair the release gate relies on.
set -u

DEB="${1:?usage: w13d_deb_smoke.sh /path/to/slooper_*_amd64.deb [images...]}"
shift || true
DEB="$(readlink -f "$DEB")"
[ -f "$DEB" ] || { echo "FATAL: $DEB not a file"; exit 2; }

IMAGES=("$@")
if [ ${#IMAGES[@]} -eq 0 ]; then
  IMAGES=(debian:12 ubuntu:22.04)
fi

run_container() {
  local img="$1"
  timeout 900 docker run --rm --cpus=2 --memory=2g -v "$DEB:/slooper.deb:ro" "$img" bash -c '
set -u
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq --no-install-recommends /slooper.deb >/tmp/i.log 2>&1; echo INSTALL_RC=$?
echo "WICH=$(command -v slooper || true)"
dpkg -L slooper 2>/dev/null | grep -cE "^/usr/share/(applications/slooper\.desktop|pixmaps/slooper\.png)$" | sed "s/^/DOCS_COUNT=/"
slooper --slooper-probe 2 /nonexistent >/tmp/p.out 2>/tmp/p.err; echo PROBE_RC=$?
apt-get purge -y -qq slooper >/dev/null 2>&1; echo PURGE_RC=$?
echo "<<<W13D-OUT>>>"; cat /tmp/p.out
echo "<<<W13D-ERR>>>"; cat /tmp/p.err
echo "<<<W13D-ILOG>>>"; tail -4 /tmp/i.log
echo "<<<W13D-END>>>"
' 2>&1
}

verdict() {
  local cap="$1"
  local rc which docs prc purge out nlines
  rc="$(sed -n 's/^INSTALL_RC=//p' <<<"$cap" | head -1)"
  which="$(sed -n 's/^WICH=//p' <<<"$cap" | head -1)"
  docs="$(sed -n 's/^DOCS_COUNT=//p' <<<"$cap" | head -1)"
  prc="$(sed -n 's/^PROBE_RC=//p' <<<"$cap" | head -1)"
  purge="$(sed -n 's/^PURGE_RC=//p' <<<"$cap" | head -1)"
  out="$(sed -n '/<<<W13D-OUT>>>/,/<<<W13D-ERR>>>/p' <<<"$cap" | sed '1d;$d')"
  nlines="$(grep -c . <<<"$out")"
  local ok=1 why=""
  [ "$rc" = 0 ]            || { ok=0; why+=" install_rc='${rc:-?}'"; }
  [ "$which" = /usr/bin/slooper ] || { ok=0; why+=" path='${which:-none}'"; }
  [ "$docs" = 2 ]          || { ok=0; why+=" desktop/icon_count='${docs:-?}'"; }
  [ "$prc" = 0 ] && [ "$nlines" = 1 ] && [[ "$out" == \[* ]] && [[ "$out" == *\] ]] \
                   && [[ "$out" == *nonexistent* ]] || { ok=0; why+=" probe_rc='${prc:-?}' stdout_lines=${nlines}"; }
  [ "$purge" = 0 ]         || { ok=0; why+=" purge_rc='${purge:-?}'"; }
  if [ $ok = 1 ]; then
    echo "  PASS: installed from Depends only, probe ran from PATH, files placed, purge clean"
    return 0
  fi
  echo "  FAIL:$why"
  echo "  stdout: ${out:0:200}"
  grep -oE "GLIBC_[0-9.]+.{0,3}not found|error while loading shared libraries.*|E: .*" <<<"$cap" | head -2 | sed 's/^/  cause:  /'
  return 1
}

echo "W13d .deb smoke — package: $DEB"
echo "  version: $(dpkg-deb -f "$DEB" Version 2>/dev/null || echo '(dpkg-deb not on this host)')  sha256: $(sha256sum "$DEB" | cut -c1-16)"
fail=0
for img in "${IMAGES[@]}"; do
  echo "── $img"
  cap="$(run_container "$img")" || { echo "  FAIL: container error"; fail=1; continue; }
  verdict "$cap" || fail=1
done
[ $fail = 0 ] && echo "DEBSMOKE: all ${#IMAGES[@]} distros PASS" || echo "DEBSMOKE: FAILURES"
exit $fail
