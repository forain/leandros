#!/bin/bash
# One full glitch-test cycle on aarch64/HVF: build, boot, 60s MAME run, analyze.
# Usage: audio_glitch_test.sh <label>
set -e
cd /Users/forain/code/leandros
L=$1
W=/Users/forain/.claude-forain/jobs/978c75c4/tmp/glitch-$L.wav
OUT=/Users/forain/.claude-forain/jobs/978c75c4/tmp/glitch-$L.out

python3 .claude/skills/run-leandros/driver.py stop 2>/dev/null || true
./scripts/build-all.sh --arch aarch64 2>&1 | grep -iE "^error|error\[" && exit 1
rm -f "$W"
LEANDROS_AUDIO_WAV=$W python3 .claude/skills/run-leandros/driver.py start aarch64 > /dev/null 2>&1
python3 .claude/skills/run-leandros/driver.py cmd "mame captcomm -rompath / -v -str 60 -skip_gameinfo" 100 > "$OUT" 2>&1
sleep 3
python3 .claude/skills/run-leandros/driver.py stop > /dev/null 2>&1 || true

REC=$(grep -ac "recovering stream" "$OUT" || true)
SPEED=$(grep -a "Average speed" "$OUT" | tail -1)
python3 - "$W" "$L" "$REC" "$SPEED" <<'EOF'
import struct, math, sys
w, label, rec, speed = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
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
holes = [] if onset is None else [i for i in range(onset, len(rms)) if rms[i] < 20]
# max zero-run after onset
mx = cur = 0
if onset is not None:
    for s in samples[onset*win:]:
        cur = cur + 1 if s == 0 else 0
        if cur > mx: mx = cur
print(f"[{label}] captured {dur:.1f}s | recoveries {rec} | {speed.strip()}")
print(f"[{label}] music onset sec {onset} | silent-second holes after onset: {holes}")
print(f"[{label}] max zero-run after onset: {mx/2/44.1:.0f} ms")
verdict = "PASS" if (rec == "1" and onset is not None and not holes and dur > 58) else "SUSPECT"
print(f"[{label}] verdict: {verdict}")
EOF
