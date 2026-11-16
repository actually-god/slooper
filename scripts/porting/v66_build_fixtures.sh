#!/bin/sh
# v66: build the VST3 program-list fixture for BOTH OSes (see the C header).
# Same doctrine as w6_build_fixtures.sh: the .dll is statically linked — a
# fixture importing mingw runtime DLLs would LoadLibrary-fail on a clean
# Windows CI runner. The binaries are COMMITTED (tests/fixtures/v66) — CI
# never rebuilds them, matching the w6 flow.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out="$root/tests/fixtures/v66"
mkdir -p "$out"

cc_host=${CC:-cc}
"$cc_host" -O2 -fPIC -shared -o "$out/v66-vst3.so" "$here/v66_vst3_fixture.c"
echo "built $out/v66-vst3.so"

mingw_gcc=${W6_MINGW_GCC:-$HOME/Tools/llvm-mingw-20261104-ucrt-ubuntu-22.04-x86_64/bin/x86_64-w64-mingw32-gcc}
if [ -x "$mingw_gcc" ]; then
	"$mingw_gcc" -O2 -static -shared -o "$out/v66-vst3.dll" "$here/v66_vst3_fixture.c"
	echo "built $out/v66-vst3.dll (via $mingw_gcc)"
else
	echo "WARN: llvm-mingw not at $mingw_gcc — .dll NOT rebuilt (set W6_MINGW_GCC)" >&2
fi
