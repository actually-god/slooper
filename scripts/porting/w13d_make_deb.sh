#!/usr/bin/env bash
# W13d (docs/WINDOWS-PORT.md): optional Debian/Ubuntu package. The W11
# staging tree is ALREADY FHS (`usr/bin/slooper` + desktop entry + icon),
# so the .deb is that tree plus a control file — no parallel install logic,
# the tar.zst and the deb ship identical payloads from the same stage step.
#
#   usage: w13d_make_deb.sh <staged-pkg-dir> <version>
#     staged-pkg-dir = the "slooper-<ver>-linux-x86_64" directory the
#                      release.yml Stage+pack step produced
#     version        = that same <ver> (e.g. 0.21.0 or 0.21.0-dev.abc1234)
#
# Writes ./slooper-<debver>_amd64.deb and prints the path. Needs
# dpkg-deb (any Debian-family system or container; ubuntu CI runners
# have it). The binary itself is NEVER rebuilt or touched here — W13b
# rule: verify/package the artifact, not the build.
#
# Deps in the control file mirror the W13b measurement EXACTLY:
# Depends = the load-time NEEDED set the base image lacks (libx11-6,
# libstdc++6; libc/libm/libgcc_s are base-system). jack/Wayland/GL are
# dlopen'd, hence Recommends/Suggests, never Depends — installing this
# .deb must not drag a sound server in against the user's will.
set -euo pipefail

PKG="${1:?usage: w13d_make_deb.sh <staged-pkg-dir> <version>}"
VER="${2:?usage: w13d_make_deb.sh <staged-pkg-dir> <version>}"
[ -d "$PKG" ]        || { echo "FATAL: $PKG is not a directory" >&2; exit 2; }
[ -x "$PKG/usr/bin/slooper" ] || { echo "FATAL: $PKG/usr/bin/slooper missing/not executable — wrong staging dir?" >&2; exit 2; }
command -v dpkg-deb >/dev/null || { echo "FATAL: dpkg-deb not found (Debian-family host/container required)" >&2; exit 2; }

# dpkg version rule: after the last '-', the revision must START with a
# digit — "0.21.0-dev.abc1234" would parse as revision "dev.abc1234" and
# be rejected. "~dev.<sha>" is the canonical pre-release form (sorts
# BELOW the eventual release, which is exactly right for a dev build).
DEBVER="${VER/-dev./~dev.}"
case "$DEBVER" in
  *[!0-9a-zA-Z.+~]*) echo "FATAL: '$DEBVER' is not a valid dpkg version" >&2; exit 2 ;;
esac

ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT/DEBIAN"
# ONLY the usr/ subtree goes into the package — README/LICENSE at the
# staging root belong to the tar.zst, not to /.
cp -a "$PKG/usr" "$ROOT/usr"

cat > "$ROOT/DEBIAN/control" <<EOF
Package: slooper
Version: $DEBVER
Architecture: amd64
Section: sound
Priority: optional
Maintainer: actually-god <actually-god@users.noreply.github.com>
Homepage: https://github.com/actually-god/slooper
Depends: libx11-6, libstdc++6
Recommends: pipewire-jack | libjack-jackd2-0
Suggests: rustc, ffmpeg
Description: clip-based DAW with MIDI and keyboard control (JACK)
 slooper is a small clip-based DAW: drop audio from the asset rack onto
 a metronome-defined timeline, record audio and MIDI takes that place
 themselves, play built-in and hosted instruments (VST2/VST3/CLAP),
 mix live effect chains, and export. Audio runs over JACK (PipeWire's
 JACK shim works out of the box); slooper never spawns the server.
 .
 Built against the glibc 2.35 floor, so this package runs on Ubuntu
 22.04+, Debian 12+, and anything newer. See docs/INSTALL.md for the
 dependency contract.
EOF

OUT="$(pwd)/slooper-${DEBVER}_amd64.deb"
# dpkg-deb's progress chatter goes to stdout on some versions — keep OUR
# stdout to exactly one line (the artifact path) so callers can capture it
dpkg-deb --root-owner-group -Zzstd -b "$ROOT" "$OUT" >/dev/null
# self-check: dpkg's own field reader says the package at least parses,
# and never silently rewrites the version we computed
test "$(dpkg-deb -f "$OUT" Version)" = "$DEBVER"
test "$(dpkg-deb -f "$OUT" Package)" = "slooper"
echo "$OUT"
