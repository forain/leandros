#!/bin/bash
# doom-audio-gap-check.sh <arch> [max_underruns=10] [doomsession args...]
# Guard for DOOM audio underruns (lane doomgap, L11.13): plays DOOM in a desktop
# session with the guest audio captured to a WAV (doomsession.py), then counts
# holes the audio path cut into the signal (scripts/wav-gaps.py). Clock-paced
# PCM gave 285 x 21.3 ms holes every 85.3 ms on x86_64 KVM; fill-paced gives <= 2.
# Runs on private copies of the images, deletes the WAV afterwards. For the box:
#   ssh box 'cd ~/Projects/leandros-wave && scripts/doom-audio-gap-check.sh x86_64'
set -u
ARCH=${1:?arch}; MAX=${2:-10}; shift; [ $# -gt 0 ] && shift
ROOT=$(cd "$(dirname "$0")/.." && pwd); cd "$ROOT" || exit 2
D=$(mktemp -d /tmp/doomgap-guard.XXXXXX); O=$D/out
cp ${CP_FLAGS:-} f2fs-data0-$ARCH.img $D/data0.img && cp ${CP_FLAGS:-} leandros-limine-$ARCH.img $D/limine.img || exit 2
LEANDROS_ROOT_IMAGE=$D/data0.img LEANDROS_DISK_IMAGE=$D/limine.img DOOMSESSION_OUT=$O \
  LEANDROS_RUN_ID=${LEANDROS_RUN_ID:-doomgap-guard-$ARCH} \
  timeout 1500 python3 .claude/skills/run-leandros/doomsession.py "$ARCH" guard --no-vt "$@" > $D/session.log 2>&1
grep -a "audio:" $O/run-guard/doom.log 2>/dev/null | head -4
python3 scripts/wav-gaps.py $O/run-guard/audio.wav --max-gaps "$MAX"; rc=$?
[ -s $O/run-guard/audio.wav ] || { echo "FAIL: no WAV captured (see $D/session.log)"; rc=2; }
rm -rf $O/run-guard/*.wav $O/run-guard/*.ppm $D/data0.img $D/limine.img
exit $rc
