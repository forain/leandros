#!/bin/sh
# e2undo-powercut-test.sh [aarch64|x86_64] -- power-cut model for the e2undo file
# that ports/e2fsprogs' patched libext2fs keeps durable (undo-durable.patch).
# kill -9 keeps the host page cache, so it cannot tell a durable undo file from
# one that only reached cache. Here resize2fs -z runs under an LD_PRELOAD tracer
# (e2undo-powercut/wtrace.c) that records every write/fsync/ftruncate to the image
# and the undo file, in order. replay.py then builds the disk a power cut after
# each sampled image write could leave (image: all writes so far reached disk;
# undo: ONLY writes covered by an fsync, or, for 'rand', a random half of the
# rest), runs e2undo, and compares with the pre-image. A case where the image
# was overwritten before its undo record was durable shows up as CORRUPT.
# Runs the stock lib (must be red on >=1 case) and the patched lib (must be all
# green). Exit 0 iff patched is clean AND stock is red.
set -eu
ARCH="${1:-x86_64}"; CUTS="${CUTS:-40}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/ports/e2fsprogs/out/$ARCH/usr/lib"
[ -f "$LIB/libext2fs.so.2" ] || { echo "build ports/e2fsprogs first"; exit 2; }
. "$ROOT/scripts/container-lib.sh"
leandros_pick_alpine || { echo "no Alpine backend: $ALPINE_WHY"; exit 2; }
SNAP=$(mktemp -d "${TMPDIR:-/tmp}/e2undo-pc.XXXXXX")
mkdir -p "$SNAP/lib"; cp "$LIB/libext2fs.so.2" "$LIB/libleandros_ssp.so.1" "$SNAP/lib/"
cp "$ROOT/scripts/e2undo-powercut/wtrace.c" "$ROOT/scripts/e2undo-powercut/replay.py" "$SNAP/"
cat > "$SNAP/run.sh" <<'IN'
set -eu
apk add --no-cache e2fsprogs e2fsprogs-extra gcc musl-dev python3 >/dev/null
mkdir -p /tmp/pl; cp /src/lib/* /tmp/pl/; ln -sf /lib/ld-musl-$(uname -m).so.1 /tmp/pl/libc.so
gcc -O1 -shared -fPIC -o /tmp/wtrace.so /src/wtrace.c
W=/tmp/w; mkdir -p $W; cd $W
mkdir d; for i in 1 2 3 4 5 6; do dd if=/dev/urandom of=d/f$i bs=1M count=24 2>/dev/null; done
mkfs.ext4 -q -F -b 4096 -O ^has_journal -d d base.img 256M
for i in 1 2 3 4; do debugfs -w -R "rm /f$i" base.img >/dev/null 2>&1; done
e2fsck -fy base.img >/dev/null || true
cp base.img pre.img
run() { # $1 label, $2 libdir or ""
  cp pre.img t.img; rm -f U
  if [ -n "$2" ]; then export LD_LIBRARY_PATH=$2; else unset LD_LIBRARY_PATH; fi
  WT_LOG=$W/trace.bin WT_FILES=t.img,U LD_PRELOAD=/tmp/wtrace.so resize2fs -f -z U t.img 100M >/dev/null 2>&1 || { echo "$1: resize2fs failed"; return 1; }
  unset LD_LIBRARY_PATH
  echo "$1 trace $(stat -c %s trace.bin) bytes"
  CMP_LIMIT=$((100*1024*1024)) python3 /src/replay.py trace.bin pre.img ${CUTS:-40} 1 $W/cases | tail -n ${TAIL:-6}
}
echo "== stock"; run stock ""
echo "== patched"; run patched /tmp/pl
IN
leandros_alpine_run --arch "$ARCH" -e "CUTS=$CUTS" -v "$SNAP:/src:ro" -- sh /src/run.sh 2>&1 | tee "$SNAP/out.log"
S=$(sed -n '/== stock/,/== patched/p' "$SNAP/out.log" | sed -n 's/^POWERCUT cases=[0-9]* corrupt=//p')
P=$(sed -n '/== patched/,$p' "$SNAP/out.log" | sed -n 's/^POWERCUT cases=[0-9]* corrupt=//p')
rm -rf "$SNAP"
echo "stock corrupt=${S:-?} patched corrupt=${P:-?}"
[ "${P:-1}" = 0 ] && [ "${S:-0}" != 0 ] && { echo PASS; exit 0; }
echo FAIL; exit 1
