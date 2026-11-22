#!/usr/bin/env python3
"""Regenerate userland/vttest/data/btm-100x30.grid, the reference cell grid for
the recorded `btm` frame (btm-100x30.raw, captured in the guest with
`script -q -c 'stty cols 100 rows 30; btm'` from Alpine's packaged bottom),
using the pyte terminal emulator as the reference.

    python3 -m venv /tmp/v && /tmp/v/bin/pip install pyte
    /tmp/v/bin/python scripts/btm-replay-gen.py
Format: `row <r>: <hex code point per cell>`, blank = 0, second half of a wide
character = 80000000, trailing blanks trimmed (same as /proc/vtgrid)."""
import os, pyte
d = os.path.join(os.path.dirname(__file__), '..', 'userland', 'vttest', 'data')
raw = open(os.path.join(d, 'btm-100x30.raw'), 'rb').read()
s = pyte.Screen(100, 30)
pyte.ByteStream(s).feed(raw)
out = []
for r in range(30):
    cells = []
    for c in range(100):
        ch = s.buffer[r][c].data
        cells.append(0x80000000 if ch == '' else (0 if ch == ' ' else ord(ch)))
    while cells and cells[-1] == 0: cells.pop()
    out.append('row %d: %s' % (r, ' '.join('%x' % x for x in cells)))
open(os.path.join(d, 'btm-100x30.grid'), 'w').write('\n'.join(out) + '\n')
