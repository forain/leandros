#!/usr/bin/env python3
"""magcount.py IMAGE.ppm... — count pure magenta (#FF00FF) pixels in binary
PPM screenshots (driver.py screenshot output). On the Mac host stack (virgl ->
ANGLE/Vulkan -> MoltenVK) a texel nobody wrote reads back as exactly #FF00FF,
so this counts "unwritten" pixels. Prints count, share and bounding box."""
import sys

def read_ppm(path):
    data = open(path, "rb").read()
    fields, pos = [], 0
    while len(fields) < 4:
        while data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b"#":
            pos = data.index(b"\n", pos) + 1
            continue
        end = pos
        while not data[end:end + 1].isspace():
            end += 1
        fields.append(data[pos:end]); pos = end
    assert fields[0] == b"P6", "binary PPM only"
    w, h = int(fields[1]), int(fields[2])
    return w, h, data[pos + 1:pos + 1 + w * h * 3]

for path in sys.argv[1:]:
    w, h, px = read_ppm(path)
    n, x0, y0, x1, y1 = 0, w, h, -1, -1
    idx = px.find(b"\xff\x00\xff")
    while idx != -1:
        if idx % 3 == 0:
            p = idx // 3; x, y = p % w, p // w
            n += 1; x0 = min(x0, x); x1 = max(x1, x); y0 = min(y0, y); y1 = max(y1, y)
            idx = px.find(b"\xff\x00\xff", idx + 3)
        else:
            idx = px.find(b"\xff\x00\xff", idx + 1)
    bbox = f"bbox=({x0},{y0})-({x1},{y1})" if n else ""
    print(f"{path}: magenta={n} ({100.0 * n / (w * h):.3f}%) {w}x{h} {bbox}")
