#!/bin/bash
# doom-music-check.sh <arch> [min_loud_pct=80] [doomsession args...]
# Guard for DOOM music (lane doomsnd, L12 #57): plays DOOM with -nosfx in a
# desktop session (doomsession.py, guest audio captured to a WAV), so every
# sound in the capture is the MIDI synth. PASS needs (a) the synth's own level
# report (`music_peak=` in the stats line) above 1000 and (b) at least
# min_loud_pct % of the DOOM seconds loud (RMS > 100). With -nomusic instead
# (DOOMSND_ARGS=-nomusic) it must fail: that is the red check.
set -u
ARCH=${1:?arch}; MIN=${2:-80}; shift; [ $# -gt 0 ] && shift
ROOT=$(cd "$(dirname "$0")/.." && pwd); cd "$ROOT" || exit 2
D=$(mktemp -d /tmp/doomsnd-guard.XXXXXX); O=$D/out
cp ${CP_FLAGS:-} f2fs-data0-$ARCH.img $D/data0.img && cp ${CP_FLAGS:-} leandros-limine-$ARCH.img $D/limine.img || exit 2
LEANDROS_ROOT_IMAGE=$D/data0.img LEANDROS_DISK_IMAGE=$D/limine.img DOOMSESSION_OUT=$O \
  LEANDROS_RUN_ID=${LEANDROS_RUN_ID:-doomsnd-guard-$ARCH} \
  timeout 1500 python3 .claude/skills/run-leandros/doomsession.py "$ARCH" guard --no-vt \
  --doom-args "${DOOMSND_ARGS:--nosfx}" "$@" > $D/session.log 2>&1
python3 - "$O/run-guard" "$MIN" <<'PY'
import json, re, sys
d, mn = sys.argv[1], float(sys.argv[2])
try:
    a = json.load(open(f"{d}/audio.json"))["desktop-doom"]
    log = open(f"{d}/doom.log", errors="replace").read()
except (OSError, KeyError, ValueError) as e:
    print(f"FAIL: no capture ({e})"); sys.exit(2)
peaks = [int(x) for x in re.findall(r"music_peak=(\d+)", log)]
pct = 100.0 * a["loud_seconds"] / max(1.0, a["seconds"])
ok = bool(peaks) and max(peaks) > 1000 and pct >= mn
print(f"{'PASS' if ok else 'FAIL'}: music_peak={peaks} loud={a['loud_seconds']}/{a['seconds']}s ({pct:.0f}% >= {mn:.0f}%) rms={a['rms']}")
sys.exit(0 if ok else 1)
PY
rc=$?
rm -rf $O/run-guard/*.wav $O/run-guard/*.ppm $D/data0.img $D/limine.img
exit $rc
