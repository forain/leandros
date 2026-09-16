#!/bin/bash
# One full glitch-test cycle: build, boot, 60s MAME run, analyze.
# Usage: audio-glitch-test.sh <label> [arch] [--no-build]
set -e
cd /Users/forain/code/leandros
L=$1
ARCH=${2:-aarch64}
NO_BUILD=0
for a in "$@"; do
    [ "$a" = "--no-build" ] && NO_BUILD=1
done

GLITCH_DIR="${GLITCH_DIR:-${TMPDIR:-/tmp}}"
mkdir -p "$GLITCH_DIR"
W="$GLITCH_DIR/glitch-$L.wav"
OUT="$GLITCH_DIR/glitch-$L.out"

python3 .claude/skills/run-leandros/driver.py stop 2>/dev/null || true
if [ "$NO_BUILD" -eq 0 ]; then
    ./scripts/build-all.sh --arch "$ARCH" 2>&1 | grep -iE "^error|error\[" && exit 1
fi
rm -f "$W"
LEANDROS_AUDIO_WAV=$W python3 .claude/skills/run-leandros/driver.py start "$ARCH" > /dev/null 2>&1
# Boot lands on a login prompt (since 2026-07-21); without this the mame line is typed as a username.
python3 .claude/skills/run-leandros/driver.py login root root > /dev/null 2>&1
python3 .claude/skills/run-leandros/driver.py cmd "mame captcomm -rompath / -v -str 60 -skip_gameinfo" 100 > "$OUT" 2>&1
sleep 3
python3 .claude/skills/run-leandros/driver.py stop > /dev/null 2>&1 || true

REC=$(grep -ac "recovering stream" "$OUT" || true)
GAPS=$(grep -ac "producer gap" "$OUT" || true)
SPEED=$(grep -a "Average speed" "$OUT" | tail -1)
# QEMU's wav backend writes nothing while a stream is released, so each
# recovery time-compresses the file instead of recording zeros — the
# zero-run metric below cannot see stall glitches. The recovery/gap counts
# above are the real signal; treat the wav-derived numbers as secondary.
python3 - "$W" "$L" "$REC" "$SPEED" "$GAPS" <<'EOF'
import struct, math, sys
w, label, rec, speed, gaps = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
raw = open(w, "rb").read()
pcm = raw[44:]
samples = struct.unpack(f"<{len(pcm)//2}h", pcm[:len(pcm)//2*2])
n = len(samples)
dur = n/2/44100
win = 88200
rms = []
for i in range(0, n - win, win):
    x = samples[i:i+win]
    rms.append(math.sqrt(sum(v*v for v in x)/len(x)))
# music onset = first second with RMS > 300
onset = next((i for i, r in enumerate(rms) if r > 300), None)
# last audible second: MAME exits before the capture stops, so seconds after it are not holes
last = None if onset is None else max(i for i, r in enumerate(rms) if r > 300)
holes = [] if onset is None else [i for i in range(onset, last + 1) if rms[i] < 20]
# max zero-run after onset
mx = cur = 0
if onset is not None:
    for s in samples[onset*win:]:
        cur = cur + 1 if s == 0 else 0
        if cur > mx: mx = cur
print(f"[{label}] captured {dur:.1f}s | recoveries {rec} | gaps {gaps} | {speed.strip()}")
print(f"[{label}] music onset sec {onset} | silent-second holes after onset: {holes}")
print(f"[{label}] max zero-run after onset: {mx/2/44.1:.0f} ms")
# Startup-phase recoveries are normal; only a run that keeps recovering
# throughout is suspect.
verdict = "PASS" if (int(rec) <= 3 and onset is not None and not holes and dur > 58) else "SUSPECT"
print(f"[{label}] verdict: {verdict}")
EOF
