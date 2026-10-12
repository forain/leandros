#!/usr/bin/env bash
# Print the kernel log that LeandrOS keeps on its USB stick.
#
# Usage:
#   ./scripts/read-usb-log.sh <device|image> [--all]
#
#   <device|image>  macOS: /dev/rdiskN (needs sudo; unmount first with
#                   `diskutil unmountDisk /dev/diskN`), Linux: /dev/sdX (sudo),
#                   or a raw disk image (e.g. one made by write-usb.sh --image).
#   --all           print every slot that holds a log, oldest boot first, each
#                   under a banner; the default prints the latest boot only.
#
# The log lives in a raw partition of type MBR 0xDA or GPT type GUID
# 042B7470-1302-4E61-9347-A0130B276754 ("LEANDROS-LOG"), found by type, not by
# number. It holds 4 slots (boots N, N-1, N-2, N-3; boot N uses slot
# (N-1) % 4), each a 4096-byte header (magic LEANLOG1, boot counter, flush
# sequence, length, flags, CRC) followed by plain text. See kernel/src/klog.rs.
#
# Kernels since lane readfirst do not rewrite the header page on periodic
# flushes: each flush appends a "journal" copy of the header (magic LEANLOGJ)
# in the page right after the data. The newest valid one of the slot's boot
# (highest flush sequence, at the page its own length says) wins over the
# header page. Older images, which have no journal pages, read as before.
#
# Boot id: header and journal pages carry a random per-boot id (bytes 48..56).
# A journal is only accepted if its id equals the slot's header page id, so
# journals left behind by a previous flash (sparse flashes zero only the
# header page) are ignored. A zeroed header page means the slot is skipped.
# Older images have id 0 everywhere and read as before.
#
# The log is flushed on sync, on poweroff/reboot, on panic, on hung-task
# reports, and periodically: every 2 s on fast media (or klog.flush=<s> on the kernel command line), backed off to up to
# 5 min on a slow stick, so after a hard hang the tail may be that short; the
# header flags say if the slot filled up (FULL), the ring overran (LOST) or
# the kernel panicked.

set -euo pipefail

usage() { sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit "${1:-1}"; }

DEV=""
ALL=0
for a in "$@"; do
    case "$a" in
        --all) ALL=1 ;;
        -h|--help) usage 0 ;;
        -*) echo "unknown option: $a" >&2; usage ;;
        *) [[ -z "$DEV" ]] || usage; DEV="$a" ;;
    esac
done
[[ -n "$DEV" ]] || usage
[[ -e "$DEV" ]] || { echo "no such device or image: $DEV" >&2; exit 1; }

exec python3 - "$DEV" "$ALL" <<'PY'
import os, struct, sys, uuid, zlib

path, show_all = sys.argv[1], sys.argv[2] == "1"
LOG_GUID = uuid.UUID("042B7470-1302-4E61-9347-A0130B276754").bytes_le
SLOTS, PAGE, MAX_SLOT = 4, 4096, 16 << 20
FLAGS = ((1, "FULL (slot filled, later output not stored)"),
         (2, "PANIC (kernel panicked; this is its last flush)"),
         (4, "LOST (output overran the RAM ring before a flush)"))

try:
    fd = os.open(path, os.O_RDONLY)
except OSError as e:
    sys.exit(f"cannot open {path}: {e.strerror} (raw devices need sudo)")

def rd(off, n):
    """Read n bytes at off, using 4096-aligned reads (raw devices need it)."""
    a = off & ~(PAGE - 1)
    end = (off + n + PAGE - 1) & ~(PAGE - 1)
    out = bytearray()
    pos = a
    while pos < end:
        chunk = os.pread(fd, min(1 << 20, end - pos), pos)
        if not chunk:
            break
        out += chunk
        pos += len(chunk)
    return bytes(out[off - a: off - a + n])

mbr = rd(0, 512)
if mbr[510:512] != b"\x55\xaa":
    sys.exit("no partition table (no MBR signature)")
ents = [struct.unpack("<B3sB3sII", mbr[446 + 16 * i: 462 + 16 * i]) for i in range(4)]
start = size = None
if any(e[2] == 0xEE for e in ents):
    hdr = rd(512, 512)
    if hdr[:8] != b"EFI PART":
        sys.exit("protective MBR but no GPT header")
    eslba, n, esz = struct.unpack("<QII", hdr[72:88])
    table = rd(eslba * 512, n * esz)
    for i in range(n):
        e = table[i * esz:(i + 1) * esz]
        if e[:16] == LOG_GUID:
            first, last = struct.unpack("<QQ", e[32:48])
            start, size = first * 512, (last - first + 1) * 512
            break
else:
    for e in ents:
        if e[2] == 0xDA:
            start, size = e[4] * 512, e[5] * 512
            break
if start is None:
    sys.exit("no LeandrOS log partition on this disk (MBR type 0xDA / GPT LEANDROS-LOG)."
             " Was the stick written with a current scripts/write-usb.sh?")

slot_size = min(size // SLOTS, MAX_SLOT) & ~(PAGE - 1)
slots = []
for s in range(SLOTS):
    h = rd(start + s * slot_size, PAGE)
    if h[:8] != b"LEANLOG1" or zlib.crc32(h[:40]) != struct.unpack("<I", h[40:44])[0]:
        continue
    boot, seq, length, ssz, dcrc, flags = struct.unpack("<IIQQII", h[8:40])
    hid = h[48:56]   # boot id (all zero on images from older kernels)
    sl = dict(slot=s, boot=boot, seq=seq, len=length, crc=dcrc, flags=flags, journal=False)
    # Journal pages (see the header comment): scan the slot's data area for
    # the newest valid LEANLOGJ page of this boot sitting at roundup(length).
    area = rd(start + s * slot_size + PAGE, slot_size - PAGE)
    i = area.find(b"LEANLOGJ")
    while i >= 0:
        if i % PAGE == 0:
            j = area[i:i + 44]
            if len(j) == 44 and zlib.crc32(j[:40]) == struct.unpack("<I", j[40:44])[0]:
                jb, js, jl, jss, jc, jf = struct.unpack("<IIQQII", j[8:40])
                if (area[i + 48:i + 56] == hid and jb == boot and jss == ssz and (jl + PAGE - 1) // PAGE * PAGE == i
                        and ((js - sl["seq"]) & 0xFFFFFFFF) < 0x80000000 and js != sl["seq"]):
                    sl.update(seq=js, len=jl, crc=jc, flags=jf, journal=True)
        i = area.find(b"LEANLOGJ", i + 1)
    slots.append(sl)
if not slots:
    sys.exit("log partition found, but it holds no log yet (stick never booted with this kernel?)")
slots.sort(key=lambda x: x["boot"])
if not show_all:
    slots = slots[-1:]

for sl in slots:
    n = min(sl["len"], slot_size - PAGE)
    data = rd(start + sl["slot"] * slot_size + PAGE, n) if n else b""
    ok = zlib.crc32(data) == sl["crc"]
    notes = [txt for bit, txt in FLAGS if sl["flags"] & bit]
    if not ok:
        notes.append("CRC MISMATCH (a flush was interrupted; the last lines may be damaged)")
    src = "journal" if sl["journal"] else "header"
    banner = (f"===== boot {sl['boot']} (slot {sl['slot']}, {sl['len']} bytes, flush #{sl['seq']} from {src}) =====\n"
              + "".join(f"===== {t} =====\n" for t in notes))
    if show_all:
        sys.stdout.write(banner)
        sys.stdout.flush()
    elif notes:
        sys.stderr.write(banner)
        sys.stderr.flush()
    text = data.replace(b"\0", b"")
    sys.stdout.buffer.write(text)
    sys.stdout.buffer.write(b"\n")
    sys.stdout.buffer.flush()
    # Always say what was read, on stderr, so a redirected run that
    # produced an empty or short file is never silent about why.
    sys.stderr.write(f"read-usb-log: boot {sl['boot']} (slot {sl['slot']}): "
                     f"{len(text)} bytes{', ' + '; '.join(notes) if notes else ''}\n")
PY
