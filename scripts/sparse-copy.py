#!/usr/bin/env python3
"""sparse-copy.py SRC DST: copy SRC to DST, skipping all-zero 1 MiB chunks (holes)."""
import os, sys
src, dst = sys.argv[1], sys.argv[2]
CH = 1 << 20
Z = bytes(CH)
size = os.path.getsize(src)
data = 0
with open(src, 'rb') as f, open(dst, 'wb') as g:
    while True:
        b = f.read(CH)
        if not b:
            break
        if b == Z[:len(b)]:
            g.seek(len(b), 1)
        else:
            g.write(b)
            data += len(b)
    g.truncate(size)
    g.flush()
    os.fsync(g.fileno())
print(f"{dst}: {data >> 20} MiB data of {size >> 30} GiB (sparse)")
