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
# The log is flushed every 10 s, on sync, on poweroff/reboot and on panic, so
# after a hang the tail may be up to ~10 s short; the header flags say if the
# slot filled up (FULL), the ring overran (LOST) or the kernel panicked.

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
    slots.append(dict(slot=s, boot=boot, seq=seq, len=length, crc=dcrc, flags=flags))
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
    banner = (f"===== boot {sl['boot']} (slot {sl['slot']}, {sl['len']} bytes, flush #{sl['seq']}) =====\n"
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
