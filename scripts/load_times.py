#!/usr/bin/env python3
"""v33 load-time driver: time open() per plugin, one child per plugin
(hostile vendors isolate). Usage: load_times.py [pattern]"""
import glob, os, subprocess, sys

HOME = os.path.expanduser("~")
env = dict(os.environ)
env.setdefault("WINELOADER", f"{HOME}/Tools/winege/bin/wine64")
env.setdefault("WINEDLLOVERRIDES", "oleacc=disabled,uiautomationcore=disabled")

cands = sorted(glob.glob(f"{HOME}/.vst3/*.vst3") + glob.glob(f"{HOME}/.vst/*.so"))
# yabridge wrappers + AudioFOSS extras
for d in (f"{HOME}/.vst/yabridge", f"{HOME}/AudioFOSS/vst", f"{HOME}/AudioFOSS/vst3"):
    cands += sorted(glob.glob(f"{d}/*.so"))
    cands += sorted(glob.glob(f"{d}/*.vst3"))
pat = sys.argv[1] if len(sys.argv) > 1 else ""
cands = [c for c in cands if not pat or pat.lower() in c.lower()]

BIN = "target/release/examples/v33_load_probe"
rows = []
for c in cands:
    kind = "V3" if c.endswith(".vst3") else "V2"
    try:
        r = subprocess.run([BIN, c], env=env, capture_output=True, text=True,
                           timeout=90, stdin=subprocess.DEVNULL)
        out = (r.stdout or "").strip().splitlines()
        rows.append((c, out[0] if out else f"rc={r.returncode}"))
        print(f"{out[0] if out else f'CRASH rc={r.returncode}'}", flush=True)
    except subprocess.TimeoutExpired:
        print(f"T V3  >90000 {c}  (TIMEOUT)", flush=True)
        rows.append((c, "TIMEOUT"))
print("\n== sorted by time ==")
def ms(row):
    try: return int(row.split()[2])
    except Exception: return 10**9
for c, r in sorted(rows, key=lambda x: ms(x[1] if isinstance(x[1], str) else "")):
    print(r)
