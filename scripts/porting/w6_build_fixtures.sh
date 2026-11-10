#!/bin/sh
# WINDOWS-PORT W6: build the VST2 fixture for BOTH OSes (see the C header).
# The .dll must be statically linked against the mingw runtimes — a fixture
# importing libgcc_s-win32-x86-64-*.dll would LoadLibrary-fail on a clean
# Windows CI runner, turning green pins grey.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out="$root/tests/fixtures/w6"
mkdir -p "$out"

cc_host=${CC:-cc}
"$cc_host" -O2 -fPIC -shared -o "$out/w6-vst2.so" "$here/w6_vst2_fixture.c"
echo "built $out/w6-vst2.so"

mingw_gcc=${W6_MINGW_GCC:-$HOME/Tools/llvm-mingw-20261104-ucrt-ubuntu-22.04-x86_64/bin/x86_64-w64-mingw32-gcc}
if [ -x "$mingw_gcc" ]; then
	"$mingw_gcc" -O2 -static -shared -o "$out/w6-vst2.dll" "$here/w6_vst2_fixture.c"
	echo "built $out/w6-vst2.dll (via $mingw_gcc)"
else
	echo "WARN: llvm-mingw not at $mingw_gcc — .dll NOT rebuilt (set W6_MINGW_GCC)" >&2
fi
