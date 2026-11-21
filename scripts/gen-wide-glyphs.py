#!/usr/bin/env python3
"""Generate drivers/data/wide-glyphs.bin: the 16x16 glyphs for every double-width
code point (drivers/src/glyphs.rs `wide`) the console can draw.

  gen-wide-glyphs.py UNIFONT_HEX OUT.bin [EMOJI_TTF]

UNIFONT_HEX is GNU Unifont's unifont.hex (BMP, GPLv2+ with the font embedding
exception; the OFL Noto Emoji monochrome font supplies the supplementary-plane
emoji, rasterised to 16x16 and thresholded). Only code points `wide()` accepts
and the font has a 16x16 glyph for are kept.

Layout (little endian): u32 nruns; nruns * (u32 first_cp, u32 len, u32 first_glyph);
then 32 bytes per glyph, 16 rows of u16 big-endian-bit-order (bit 15 = leftmost).
"""
import re, struct, sys

hexf, out = sys.argv[1], sys.argv[2]
emoji_ttf = sys.argv[3] if len(sys.argv) > 3 else None
src = open(__file__.rsplit('/scripts/', 1)[0] + '/drivers/src/glyphs.rs').read()
body = re.search(r'pub fn wide\(u: u32\) -> bool \{\n    matches!\(u,(.*?)\)\n\}', src, re.S).group(1)
ranges = []
for lo, hi in re.findall(r'0x([0-9A-Fa-f]+)(?:\.\.=0x([0-9A-Fa-f]+))?', body):
    lo = int(lo, 16); ranges.append((lo, int(hi, 16) if hi else lo))

uni = {}
for line in open(hexf):
    cp, h = line.strip().split(':')
    if len(h) == 64: uni[int(cp, 16)] = bytes.fromhex(h)
    elif len(h) == 32:  # 8x16 glyph (many symbols): centre it in the 2-cell box
        uni[int(cp, 16)] = b''.join(struct.pack('>H', b << 4) for b in bytes.fromhex(h))

glyphs = {}
for lo, hi in ranges:
    for c in range(lo, min(hi, 0x2FFFF) + 1):
        if c in uni: glyphs[c] = uni[c]

if emoji_ttf:
    from PIL import Image, ImageDraw, ImageFont
    f = ImageFont.truetype(emoji_ttf, 15)
    for lo, hi in ranges:
        for c in range(max(lo, 0x10000), min(hi, 0x1FFFF) + 1):
            if c in glyphs: continue
            im = Image.new('L', (16, 16), 0)
            d = ImageDraw.Draw(im)
            bb = d.textbbox((0, 0), chr(c), font=f)
            if bb[2] - bb[0] <= 0: continue
            d.text(((16 - (bb[2] - bb[0])) // 2 - bb[0], (16 - (bb[3] - bb[1])) // 2 - bb[1]), chr(c), font=f, fill=255)
            rows = b''
            px = im.load()
            for y in range(16):
                v = 0
                for x in range(16):
                    if px[x, y] >= 96: v |= 0x8000 >> x
                rows += struct.pack('>H', v)
            if any(rows): glyphs[c] = rows

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
