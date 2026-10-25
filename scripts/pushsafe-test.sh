#!/bin/bash
# Lossy-link push test (lane pushsafe). Needs a QEMU VM with ssh (LEANDROS_RUN_ID=$1) and
# scripts/lossyproxy.py. Pushes a SIZE_MB file N times through the proxy; after each run the
# remote target must be the OLD or the NEW file, never partial, and no .part left on success.
# usage: pushsafe-test.sh <run-id> [new|old|naive] [runs]
set -u
cd "$(dirname "$0")/.."
ID=$1; MODE=${2:-new}; RUNS=${3:-3}; SIZE_MB=${SIZE_MB:-200}
SCRIPT=scripts/remote.py; [ "$MODE" = old ] && SCRIPT=scripts/remote_old.py
read H P < /tmp/leandros-$ID-ssh
python3 scripts/lossyproxy.py --listen 2801 --to $H:$P --mean-bytes ${MEAN:-8000000} --stall ${STALL:-0.4} \
    --state /tmp/leandros-lossy-ssh & PX=$!
trap 'kill $PX 2>/dev/null' EXIT
sleep 1
D=${TMPDIR:-/tmp}/pushsafe-data; mkdir -p $D
R() { LEANDROS_RUN_ID=lossy LEANDROS_REMOTE=qemu:lossy python3 $SCRIPT "$@"; }
D0() { LEANDROS_RUN_ID=$ID LEANDROS_REMOTE=qemu python3 scripts/remote.py "$@"; }   # direct, healthy path
rsha() { D0 run "sha256sum /tmp/big.bin" 2>/dev/null | awk '{print $1}'; }
head -c $((SIZE_MB<<20)) /dev/urandom > $D/old.bin; OLD=$(shasum -a 256 $D/old.bin | cut -d' ' -f1)
D0 push $D/old.bin /tmp/big.bin >/dev/null 2>&1
for i in $(seq 1 $RUNS); do
  head -c $((SIZE_MB<<20)) /dev/urandom > $D/new.bin; NEW=$(shasum -a 256 $D/new.bin | cut -d' ' -f1)
  t0=$(date +%s)
  case $MODE in
    naive) for k in 1 2 3; do timeout 40 scp -q -P 2801 -i ~/.ssh/leandros_remote -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR $D/new.bin root@127.0.0.1:/tmp/big.bin && break; done ;;
    old)   timeout 150 python3 $SCRIPT push $D/new.bin /tmp/big.bin >/dev/null 2>&1 ;;
    *)     LEANDROS_PUSH_STALL=${PSTALL:-6} R push $D/new.bin /tmp/big.bin 2>&1 | tail -n 3 | cut -c1-150 ;;
  esac
  rc=$?; S=$(rsha); st=CORRUPT/PARTIAL
  [ "$S" = "$OLD" ] && st="old-intact"; [ "$S" = "$NEW" ] && st="new-verified"
  echo "run $i mode=$MODE rc=$rc $(( $(date +%s)-t0 ))s target=$st parts=$(D0 run 'ls /tmp' | grep -c 'big.bin.part\|remdbg-tmp' 2>/dev/null)"
  [ "$st" = new-verified ] && { cp $D/new.bin $D/old.bin; OLD=$NEW; }
done
