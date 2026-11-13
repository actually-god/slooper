#!/usr/bin/env bash
# w14: .rpm for Fedora/openSUSE/RHEL-family, the sibling of w13d_make_deb.sh.
# The W11 staging tree is ALREADY FHS (usr/bin/slooper + desktop entry +
# icon), so the .rpm packages that exact tree — same payload as the
# tar.zst and the .deb, never a parallel build (W13b rule: package the
# artifact, not the build).
#
#   usage: w14_make_rpm.sh <staged-pkg-dir> <version>
#     staged-pkg-dir = the "slooper-<ver>-linux-x86_64" directory the
#                      release.yml Stage+pack step produced
#     version        = that same <ver> (e.g. 0.22.0 or 0.22.0-dev.abc1234)
#
# Writes ./slooper-<nvr>.x86_64.rpm and prints the path. Needs rpmbuild
# (dnf install rpm-build); run on Fedora-family or in a fedora container.
#
# Dependency contract mirrors the deb's exactly (W13b measurement):
# hard deps are the load-time NEEDED set — left to rpm's OWN automatic
# requires so they track the real binary (libX11, libstdc++); jack,
# Wayland and GL are dlopen'd, so they enter only as weak Recommends —
# installing this package must never drag a sound server in.
set -euo pipefail

PKG="${1:?usage: w14_make_rpm.sh <staged-pkg-dir> <version>}"
VER="${2:?usage: w14_make_rpm.sh <staged-pkg-dir> <version>}"
[ -d "$PKG" ]        || { echo "FATAL: $PKG is not a directory" >&2; exit 2; }
[ -x "$PKG/usr/bin/slooper" ] || { echo "FATAL: $PKG/usr/bin/slooper missing/not executable — wrong staging dir?" >&2; exit 2; }
command -v rpmbuild >/dev/null || { echo "FATAL: rpmbuild not found (rpm-build package required)" >&2; exit 2; }

# rpm version rule: Version may not contain '-'. The dev form
# "<ver>-dev.<sha>" splits into Version=<ver> Release=0.dev.<sha> —
# the leading 0 makes it sort BELOW the final release, as it must.
if [[ "$VER" == *-dev.* ]]; then
  RPMVER="${VER%-dev.*}"
  RPMREL="0.dev.${VER##*-dev.}"
else
  RPMVER="$VER"
  RPMREL="1"
fi
case "$RPMVER" in
  *[!0-9a-zA-Z.+~]*) echo "FATAL: '$RPMVER' is not a valid rpm Version" >&2; exit 2 ;;
esac

ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
SRC="$ROOT/SOURCES"
mkdir -p "$ROOT"/{BUILD,RPMS,SOURCES,SPECS,SRPMS} "$SRC/usr/bin" "$SRC/usr/share/applications" "$SRC/usr/share/pixmaps"
cp -a "$PKG/usr/bin/slooper"               "$SRC/usr/bin/"
cp -a "$PKG/usr/share/applications/slooper.desktop" "$SRC/usr/share/applications/"
cp -a "$PKG/usr/share/pixmaps/slooper.png" "$SRC/usr/share/pixmaps/"
tar -C "$SRC" -czf "$SRC/slooper-$RPMVER-usr.tar.gz" usr

cat > "$ROOT/SPECS/slooper.spec" <<EOF
Name:           slooper
Version:        $RPMVER
Release:        $RPMREL%{?dist}
Summary:        Clip-based DAW with MIDI and keyboard control (JACK)
License:        MIT
URL:            https://github.com/actually-god/slooper
Source0:        slooper-%{version}-usr.tar.gz
BuildArch:      x86_64

# Hard deps ride rpm's automatic requires (the binary's real NEEDED set).
# jack / Wayland / GL are dlopen'd at runtime -> weak deps only.
Recommends:     pipewire-jack-libs
Recommends:     libwayland-client.so.0()(64bit)
Recommends:     libEGL.so.1()(64bit)
Suggests:       rustc
Suggests:       ffmpeg

%description
slooper is a small clip-based DAW: drop audio from the asset rack onto a
metronome-defined timeline, record audio and MIDI takes that place
themselves, play built-in and hosted instruments, mix through live effect
chains, and export. Audio runs over JACK (PipeWire's JACK shim works out
of the box); slooper never spawns the server itself.

Built against the glibc 2.35 floor, so this package runs on Fedora 38+,
openSUSE Leap 15.5+, and anything newer. See docs/INSTALL.md.

%prep
%setup -q -c -T
tar -xzf %{SOURCE0}

%install
rm -rf %{buildroot}
install -d %{buildroot}/usr/bin %{buildroot}/usr/share/applications %{buildroot}/usr/share/pixmaps
install -m 755 usr/bin/slooper                   %{buildroot}/usr/bin/
install -m 644 usr/share/applications/slooper.desktop %{buildroot}/usr/share/applications/
install -m 644 usr/share/pixmaps/slooper.png      %{buildroot}/usr/share/pixmaps/

%post
touch --no-create %{_datadir}/icons/hicolor &>/dev/null || :
gtk-update-icon-cache -q %{_datadir}/icons/hicolor &>/dev/null || :
update-desktop-database -q %{_datadir}/applications &>/dev/null || :

%postun
touch --no-create %{_datadir}/icons/hicolor &>/dev/null || :
gtk-update-icon-cache -q %{_datadir}/icons/hicolor &>/dev/null || :
update-desktop-database -q %{_datadir}/applications &>/dev/null || :

%files
%defattr(-,root,root,-)
/usr/bin/slooper
/usr/share/applications/slooper.desktop
/usr/share/pixmaps/slooper.png

%changelog
* $(date +'%a %b %d %Y') actually-god <actually-god@users.noreply.github.com> - $RPMVER-$RPMREL
- Release $RPMVER (see CHANGELOG.md upstream)
EOF

rpmbuild -bb "$ROOT/SPECS/slooper.spec" \
  --define "_topdir $ROOT" --define "_sourcedir $SRC" --quiet \
  | grep -v '^+' || true

BUILT="$(find "$ROOT/RPMS" -name 'slooper-*.x86_64.rpm' | head -1)"
[ -n "$BUILT" ] || { echo "FATAL: rpmbuild produced no rpm" >&2; exit 3; }
# self-check: rpm's own header reader agrees on name/version, and the
# payload actually contains the binary (verify the artifact, not the build)
test "$(rpm -qp --qf '%{NAME}-%{VERSION}' "$BUILT")" = "slooper-$RPMVER"
rpm -qpl "$BUILT" | grep -qx '/usr/bin/slooper' || { echo "FATAL: rpm payload missing /usr/bin/slooper" >&2; exit 4; }
OUT="$(pwd)/$(basename "$BUILT")"
cp "$BUILT" "$OUT"
echo "$OUT"
