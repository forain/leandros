"""Write a disk image as a sparse file.

The image is truncated to its full size and only the non-zero 4 KiB-aligned
runs are written; zero runs become holes (APFS/ext4/xfs all support them), so
the free-space margin of an image costs no disk blocks. The file reads back
byte-identical to a dense write.
"""
import os

_BS = 4096
_ZERO = bytes(_BS)


def write_sparse(path, image):
    mv = memoryview(image)
    n = len(mv)
    with open(path, "wb") as f:
        f.truncate(n)
        pos = 0
        while pos < n:
            # skip zero blocks
            while pos < n and mv[pos:pos + _BS] == _ZERO[:min(_BS, n - pos)]:
                pos += _BS
            if pos >= n:
                break
            start = pos
            while pos < n and mv[pos:pos + _BS] != _ZERO[:min(_BS, n - pos)]:
                pos += _BS
            f.seek(start)
            f.write(mv[start:min(pos, n)])


def inject(dst, src, offset):
    """Copy `src` into the existing file `dst` at byte `offset` (dd conv=notrunc
    seek=...), skipping all-zero 4 KiB blocks so holes in `dst` stay holes.
    `offset` must be 4 KiB aligned."""
    assert offset % _BS == 0
    with open(src, "rb") as s, open(dst, "r+b") as d:
        pos = 0
        while True:
            blk = s.read(_BS)
            if not blk:
                break
            if blk != _ZERO[:len(blk)]:
                d.seek(offset + pos)
                d.write(blk)
            pos += len(blk)


if __name__ == "__main__":
    import sys
    if len(sys.argv) == 5 and sys.argv[1] == "inject":
        inject(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    else:
        sys.exit("usage: sparseimg.py inject DST SRC OFFSET_BYTES")
