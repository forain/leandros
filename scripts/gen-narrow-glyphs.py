#!/usr/bin/env python3
"""Generate drivers/data/narrow-glyphs.bin: the 8x16 glyphs for single-width
non-ASCII code points (Latin-1 Supplement, Latin Extended, IPA, Greek, Cyrillic,
punctuation, currency, arrows, math, box drawing, ... up to U+2BFF, plus the
Latin ligatures U+FB00..FB06 and U+FFFD) the console can draw.

Regenerate (input sha256 verified; same pinned Unifont as gen-wide-glyphs.py):

  python3 scripts/gen-narrow-glyphs.py /data/vtwide-src/unifont_all-18.0.01.hex \\
    drivers/data/narrow-glyphs.bin

Source: GNU Unifont 18.0.01 (GPLv2+ with the font embedding exception, see
drivers/data/UNIFONT-LICENSE.txt). Only 8x16 glyphs whose wcwidth (drivers/src/
wcwidth.rs) is 1 are kept; ASCII 0x20..0x7E stays with the built-in font.

Layout (little endian): u32 nruns; nruns * (u32 first_cp, u32 len, u32 first_glyph);
then 16 bytes per glyph, one byte per row (bit 7 = leftmost).
"""
import hashlib, re, struct, sys
WANT = '3a107e6b6d0f68d685dafb54fd4a03d232f4df8e0eb9afdd6db7a855ec848901'
if len(sys.argv) != 3:
    sys.exit(__doc__)
hexf, out = sys.argv[1:]
got = hashlib.sha256(open(hexf, 'rb').read()).hexdigest()
if got != WANT:
    sys.exit('sha256 mismatch: %s' % got)
src = open(__file__.rsplit('/scripts/', 1)[0] + '/drivers/src/wcwidth.rs').read()
def table(name):
    body = re.search(r'const %s: &\[\(u32, u32\)\] = &\[(.*?)\];' % name, src, re.S).group(1)
    return [(int(a, 16), int(b, 16)) for a, b in re.findall(r'\(0x([0-9A-Fa-f]+), 0x([0-9A-Fa-f]+)\)', body)]
bad = table('WIDE') + table('ZERO')
def single(c): return not any(a <= c <= b for a, b in bad)
glyphs = {}
for line in open(hexf):
    cp, h = line.strip().split(':')
    c = int(cp, 16)
    if len(h) != 32 or not single(c): continue
    if not (0xA0 <= c <= 0x2BFF or 0xFB00 <= c <= 0xFB06 or c == 0xFFFD): continue
    if 0xD800 <= c <= 0xF8FF: continue
    glyphs[c] = bytes.fromhex(h)
cps = sorted(glyphs)
runs = []
for c in cps:
    if runs and runs[-1][0] + runs[-1][1] == c: runs[-1][1] += 1
    else: runs.append([c, 1, 0])
g = 0
for r in runs: r[2] = g; g += r[1]
blob = struct.pack('<I', len(runs)) + b''.join(struct.pack('<III', *r) for r in runs) + b''.join(glyphs[c] for c in cps)
open(out, 'wb').write(blob)
print(len(cps), 'glyphs', len(runs), 'runs', len(blob), 'bytes')
