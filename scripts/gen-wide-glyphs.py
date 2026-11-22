#!/usr/bin/env python3
"""Generate drivers/data/wide-glyphs.bin: the 16x16 glyphs for every double-width
code point (drivers/src/glyphs.rs `wide`) the console can draw.

Regenerate (inputs live outside the repo; sha256 is verified before use):

  D=/data/vtwide-src   # any dir holding the two pinned files below
  curl -LO https://ftpmirror.gnu.org/gnu/unifont/unifont-18.0.01/unifont_all-18.0.01.hex.gz
  gunzip -k unifont_all-18.0.01.hex.gz
  curl -L -o NotoEmoji-Regular-v2.034.ttf \\
    https://github.com/googlefonts/noto-emoji/raw/v2.034/fonts/NotoEmoji-Regular.ttf
  python3 scripts/gen-wide-glyphs.py $D/unifont_all-18.0.01.hex \\
    $D/NotoEmoji-Regular-v2.034.ttf drivers/data/wide-glyphs.bin

Inputs: GNU Unifont 18.0.01 unifont_all hex (GPLv2+ with the font embedding
exception) for everything it covers, and the monochrome Noto Emoji (SIL OFL 1.1,
googlefonts/noto-emoji tag v2.034) rasterised to 16x16 and thresholded for the
supplementary-plane emoji Unifont lacks. Only code points `wide()` accepts and
the fonts have a glyph for are kept. See drivers/data/README-glyphs.md.

Layout (little endian): u32 nruns; nruns * (u32 first_cp, u32 len, u32 first_glyph);
then 32 bytes per glyph, 16 rows of u16 big-endian-bit-order (bit 15 = leftmost).
"""
import hashlib, re, struct, sys

PINNED = {
    'unifont': ('unifont_all-18.0.01.hex', '3a107e6b6d0f68d685dafb54fd4a03d232f4df8e0eb9afdd6db7a855ec848901'),
    'emoji': ('NotoEmoji-Regular.ttf (googlefonts/noto-emoji v2.034)', '415dc6290378574135b64c808dc640c1df7531973290c4970c51fdeb849cb0c5'),
}
if len(sys.argv) != 4:
    sys.exit(__doc__)
hexf, emoji_ttf, out = sys.argv[1], sys.argv[2], sys.argv[3]
for key, path in (('unifont', hexf), ('emoji', emoji_ttf)):
    name, want = PINNED[key]
    got = hashlib.sha256(open(path, 'rb').read()).hexdigest()
    print('source %s: %s sha256 %s' % (name, path, got))
    if got != want:
        sys.exit('sha256 mismatch for %s: expected %s' % (name, want))
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

if True:
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
