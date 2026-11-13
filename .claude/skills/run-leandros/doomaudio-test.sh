#!/bin/bash
# DOOM audio capture test: boot, run doom ~40 s with the guest audio captured to a wav,
# then judge clipping with wavclip.py. Usage: doomaudio-test.sh <arch> [tag]
# Run under the qemu lock. Exit 0 = PASS.
set -u
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
ARCH=${1:-x86_64}; TAG=${2:-$ARCH}
export LEANDROS_RUN_ID=doomaudio-$TAG
D=.claude/skills/run-leandros/driver.py
W=${TMPDIR:-/tmp}/doomaudio-$TAG.wav
rm -f "$W"
python3 $D stop >/dev/null 2>&1
LEANDROS_AUDIO_WAV=$W python3 $D start "$ARCH" >/dev/null 2>&1 || { echo "FAIL start"; exit 2; }
python3 $D login root root >/dev/null 2>&1
python3 $D cmd 'LEANDROS_DOOM_AUDIO_STATS=1 timeout 45 doom > /tmp/d.log 2>&1; tail -n 4 /tmp/d.log' 90 2>&1 | tail -n 5
python3 $D stop >/dev/null 2>&1
python3 .claude/skills/run-leandros/wavclip.py "$W"
