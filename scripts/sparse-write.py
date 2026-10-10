#!/usr/bin/env python3
"""Copy a LeandrOS USB image onto a device or file, writing only what matters.

Cheap flash treats every block it was ever written as live; dd'ing an image
full of zeros leaves the controller no erased blocks and every later random
write crawls. This writes only the image's non-zero 4 KiB blocks (coalesced
into runs, gaps under 1 MiB are bridged), plus ranges that MUST be written even
when they are zero, because the filesystem on top reads them back:

  head      [0, ESP) : MBR/GPT, Limine BIOS stage; zeros here erase a stale
            GPT on the stick that firmware might prefer to our MBR.
  ESP       FAT32: reserved area + both FATs (free entries are zeros) + every
            cluster up to the highest one the FAT chain allocates (directory
            clusters are zero-terminated; file tails may be zero).
  f2fs      everything below main_blkaddr (superblocks, checkpoints, SIT, NAT,
            SSA: unmapped NAT/SIT entries are zero) and every main-area block
            the image's SIT marks valid (zero-filled file blocks are data).
            Free main-area blocks are skipped: the server's allocator zeroes
            each data block it hands out, and writes node blocks whole.
  log       the 4 KiB header page of each of the 4 log slots, zeroed so stale
            LEANLOG1 headers cannot pose as this stick's boots.

Everything else keeps whatever the stick held. Needs 4 KiB aligned writes, so
it works on macOS /dev/rdiskN, Linux block devices and plain files.
"""
import argparse, os, stat, struct, sys, time

BS = 4096
GAP = 1 << 20
CHUNK = 4 << 20


def merge(ranges):
    ranges = sorted(r for r in ranges if r[1] > r[0])
    out = []
    for a, b in ranges:
        if out and a <= out[-1][1]:
            out[-1][1] = max(out[-1][1], b)
        else:
            out.append([a, b])
    return out


def rd(f, off, n):
    f.seek(off)
    return f.read(n).ljust(n, b"\0")


def esp_forced(f, off, length):
    bpb = rd(f, off, 512)
    if bpb[510:512] != b"\x55\xaa":
        return [(off, off + length)]            # not what we expect: all of it
    bps, spc, rsvd, nfats = struct.unpack_from("<HBHB", bpb, 11)
    fatsz = struct.unpack_from("<I", bpb, 36)[0]
    data_start = (rsvd + nfats * fatsz) * bps
    fat = rd(f, off + rsvd * bps, fatsz * bps)
    maxc = 1
    for c in range(len(fat) // 4 - 1, 1, -1):
        if struct.unpack_from("<I", fat, c * 4)[0] & 0x0FFFFFFF:
            maxc = c
            break
    end = data_start + max(maxc - 1, 0) * spc * bps
    return [(off, off + min(end, length))]


def f2fs_forced(f, off):
    sb = rd(f, off + 1024, 128)
    if struct.unpack_from("<I", sb, 0)[0] != 0xF2F52010:
        sys.exit("sparse-write: no f2fs superblock at the root partition")
    segs = struct.unpack_from("<I", sb, 68)[0]
    sit, main = struct.unpack_from("<II", sb, 80)[0], struct.unpack_from("<I", sb, 92)[0]
    forced = [(off, off + main * BS)]
    first = main // 512                     # SIT is indexed by partition-absolute segment
    nseg = first + segs
    sit_buf = rd(f, off + sit * BS, ((nseg + 54) // 55) * BS)
    for s in range(first, nseg):
        e = (s // 55) * BS + (s % 55) * 74
        vmap = sit_buf[e + 2:e + 66]
        base = off + s * 512 * BS
        for b in range(512):
            if vmap[b >> 3] >> (b & 7) & 1:
                forced.append((base + b * BS, base + (b + 1) * BS))
    return forced


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src"); ap.add_argument("dst")
    ap.add_argument("--esp", nargs=2, type=int, metavar=("OFF", "LEN"))
    ap.add_argument("--root", type=int, metavar="OFF")
    ap.add_argument("--log", nargs=2, type=int, metavar=("OFF", "LEN"))
    a = ap.parse_args()

    src = open(a.src, "rb")
    size = os.fstat(src.fileno()).st_size
    forced = []
    if a.esp:
        forced += [(0, a.esp[0])] + esp_forced(src, *a.esp)
    if a.root is not None:
        forced += f2fs_forced(src, a.root)
    zero_only = []
    if a.log:
        slot = min(16 << 20, a.log[1] // 4)
        zero_only = [(a.log[0] + i * slot, a.log[0] + i * slot + BS) for i in range(4)]
    forced += zero_only

    # Non-zero blocks of the source.
    nz = []
    zero = bytes(BS)
    t0 = time.time()
    pos = 0
    src.seek(0)
    while pos < size:
        buf = src.read(CHUNK)
        if not buf:
            break
        if any(buf):
            for i in range(0, len(buf), BS):
                blk = buf[i:i + BS]
                if blk != zero[:len(blk)]:
                    nz.append((pos + i, pos + i + BS))
        pos += len(buf)
    # Align forced ranges to blocks, cap the nonzero scan at the source size.
    al = lambda r: (r[0] // BS * BS, (r[1] + BS - 1) // BS * BS)
    runs = merge([al(r) for r in forced] + nz)
    # Bridge gaps under 1 MiB.
    bridged = []
    for a_, b_ in runs:
        if bridged and a_ - bridged[-1][1] < GAP:
            bridged[-1][1] = b_
        else:
            bridged.append([a_, b_])
    total = sum(b - a_ for a_, b in bridged)
    print(f"   scanned {size >> 20} MiB in {time.time() - t0:.1f}s: "
          f"{len(bridged)} runs, {total / 1048576:.1f} MiB to write", flush=True)

    flags = os.O_WRONLY
    if hasattr(os, "O_BINARY"): flags |= os.O_BINARY
    try:
        is_blk = stat.S_ISBLK(os.stat(a.dst).st_mode) or stat.S_ISCHR(os.stat(a.dst).st_mode)
    except FileNotFoundError:
        is_blk = False
        flags |= os.O_CREAT
    fd = os.open(a.dst, flags, 0o644)
    done, t0, last = 0, time.time(), 0.0
    for a_, b_ in bridged:
        p = a_
        while p < b_:
            n = min(CHUNK, b_ - p)
            data = rd(src, p, n) if p < size else bytes(n)
            os.pwrite(fd, data, p)
            p += n
            done += n
            now = time.time()
            if now - last > 1.0 or done == total:
                last = now
                print(f"\r   {done / 1048576:8.1f} / {total / 1048576:.1f} MiB "
                      f"({100 * done // max(total, 1)}%)  {done / 1048576 / max(now - t0, 1e-6):6.1f} MiB/s",
                      end="", flush=True)
    print()
    if not is_blk and os.fstat(fd).st_size < size:
        os.ftruncate(fd, size)
    os.fsync(fd)
    os.close(fd)
    print(f"   wrote {total / 1048576:.1f} MiB of the {size / 1048576:.1f} MiB image "
          f"({100 * total / max(size, 1):.1f}%); skipped {(size - total) / 1048576:.1f} MiB of zeros")


if __name__ == "__main__":
    main()
