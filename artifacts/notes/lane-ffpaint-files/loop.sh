#!/bin/bash
# loop.sh WAITTAG SCRIPT HOLDDIR PREFIX N [args...]: wait for the run WAITTAG to end, then N runs
W=$1; S=$2; H=$3; P=$4; N=$5; shift 5
while pgrep -f "ffsession.py aarch64 $W " >/dev/null; do sleep 5; done
for i in $(seq 1 $N); do FFSESSION_HOLD=$H/HOLD $S aarch64 $P$i 600 "$@"; done
echo LOOPDONE $P
