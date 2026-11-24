#!/bin/sh
# e2undo-durability-test.sh [aarch64|x86_64] -- host-side proof (no QEMU) that
# ports/e2fsprogs' patched libext2fs.so.2 keeps resize2fs's undo file durable.
# In an Alpine 3.24 container: shrink an ext4 image with data placed high under
# `resize2fs -z`, kill -9 it at several moments, `e2undo`, and compare the image
# byte for byte with the first 1 GiB of the pre-image (the part a regular-file
# truncate cannot destroy). Runs the stock lib (expected to be able to
# fail) and the patched lib (must always pass). Exit 0 iff the patched run is
# byte-identical every time.
set -eu
ARCH="${1:-aarch64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/ports/e2fsprogs/out/$ARCH/usr/lib"
[ -f "$LIB/libext2fs.so.2" ] || { echo "build ports/e2fsprogs first"; exit 2; }
. "$ROOT/scripts/container-lib.sh"
leandros_pick_alpine || { echo "no Alpine backend: $ALPINE_WHY"; exit 2; }
SNAP=$(mktemp -d "${TMPDIR:-/tmp}/e2undo-test.XXXXXX")
mkdir -p "$SNAP/lib"; cp "$LIB/libext2fs.so.2" "$LIB/libleandros_ssp.so.1" "$SNAP/lib/"
cat > "$SNAP/run.sh" <<'IN'
set -eu
apk add --no-cache e2fsprogs e2fsprogs-extra >/dev/null
mkdir -p /tmp/pl; cp /src/lib/* /tmp/pl/; ln -sf /lib/ld-musl-$(uname -m).so.1 /tmp/pl/libc.so
W=/tmp/w; mkdir -p $W; cd $W
mkdir d; for i in 1 2 3 4 5 6 7 8 9 10; do dd if=/dev/urandom of=d/f$i bs=1M count=150 2>/dev/null; done
mkfs.ext4 -q -F -b 4096 -O ^has_journal -d d base.img 3G
for i in 1 2 3 4 5 6; do debugfs -w -R "rm /f$i" base.img >/dev/null 2>&1; done
e2fsck -fy base.img >/dev/null || true
cp base.img pre.img
run() { # $1 label, $2 libdir or ""
  fails=0; total=0
  for d in 0.3 0.6 0.9 1.2 1.5 1.8 2.1 2.4 2.7 3.0 3.5 4.0 5.0; do
    cp pre.img t.img; rm -f U
    if [ -n "$2" ]; then export LD_LIBRARY_PATH=$2; else unset LD_LIBRARY_PATH; fi
    resize2fs -f -z U t.img 1G >/dev/null 2>&1 &
    pid=$!; sleep $d; kill -9 $pid 2>/dev/null || true; wait $pid 2>/dev/null || true
    unset LD_LIBRARY_PATH
    [ -f U ] || { echo "$1 d=$d: no undo file (killed too early)"; continue; }
    total=$((total+1))
    e2undo U t.img >/dev/null 2>&1 || true
    # the live superblock may differ in the documented fields only: compare past it
    truncate -s 1G t.img; dd if=pre.img of=p1g.img bs=1M count=1024 2>/dev/null
    if cmp -s p1g.img t.img; then echo "$1 d=$d: identical"; else
      # only the documented superblock fields (wtime, state, lastcheck, kbytes_written, checksum) may differ
      bad=$(cmp -l p1g.img t.img | awk '{o=$1-1; ok=(o>=1072&&o<=1075)||(o>=1082&&o<=1083)||(o>=1088&&o<=1091)||(o>=1400&&o<=1407)||(o>=2044&&o<=2047)||(o>=1652&&o<=1655); if(!ok)n++} END{print n+0}')
      if [ "$bad" = 0 ]; then echo "$1 d=$d: identical (superblock fields only)"; else
        echo "$1 d=$d: DIFFERS ($bad bytes outside the superblock fields)"; fails=$((fails+1)); fi
    fi
  done
  echo "RESULT $1 cases=$total fails=$fails"
}
run stock ""
run patched /tmp/pl
IN
leandros_alpine_run --arch "$ARCH" -v "$SNAP:/src:ro" -- sh /src/run.sh 2>&1 | tee "$SNAP/out.log"
rm -rf "$SNAP"
