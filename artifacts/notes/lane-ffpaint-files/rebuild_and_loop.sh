#!/bin/bash
# rebuild_and_loop.sh WAITTAG TREE SCRIPT HOLDDIR PREFIX N [args]
W=$1; T=$2; S=$3; H=$4; P=$5; N=$6; shift 6
while pgrep -f "ffsession.py aarch64 $W " >/dev/null; do sleep 5; done
cd ${WT:?set WT to the directory holding the worktrees}/$T && ./scripts/build-all.sh --arch aarch64 > /tmp/ffpaint-build-$T.log 2>&1 || { echo BUILDFAIL $T; exit 1; }
for i in $(seq 1 $N); do FFSESSION_HOLD=$H/HOLD $S aarch64 $P$i 600 "$@"; done
echo LOOPDONE $P
