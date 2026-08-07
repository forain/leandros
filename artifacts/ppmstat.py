#!/usr/bin/env python3
# Report non-black pixel fraction + a coarse color histogram of a PPM (P6).
import sys
def stat(path):
    with open(path, "rb") as f:
        data = f.read()
    if not data.startswith(b"P6"):
        print(f"{path}: NOT P6 ({data[:16]!r})"); return
    # parse header: P6 W H MAXVAL then binary
    idx = 2
    vals = []
    while len(vals) < 3:
        while idx < len(data) and data[idx:idx+1].isspace():
            idx += 1
        if data[idx:idx+1] == b"#":
            while idx < len(data) and data[idx:idx+1] != b"\n":
                idx += 1
            continue
        start = idx
        while idx < len(data) and not data[idx:idx+1].isspace():
            idx += 1
        vals.append(int(data[start:idx]))
    w, h, maxv = vals
    idx += 1  # single whitespace after maxval
    px = data[idx:]
    n = w * h
    nonblack = 0
    colors = {}
    step = max(1, n // 200000)  # sample
    sampled = 0
    for i in range(0, n, step):
        o = i * 3
        if o + 3 > len(px): break
        r, g, b = px[o], px[o+1], px[o+2]
        sampled += 1
        if r > 8 or g > 8 or b > 8:
            nonblack += 1
        key = (r >> 5, g >> 5, b >> 5)
        colors[key] = colors.get(key, 0) + 1
    frac = nonblack / sampled if sampled else 0
    top = sorted(colors.items(), key=lambda kv: -kv[1])[:6]
    print(f"{path}: {w}x{h} maxv={maxv} sampled={sampled} nonblack={nonblack} frac={frac:.4f}")
    print("  top colors (r>>5,g>>5,b>>5 : count):", top)
if __name__ == "__main__":
    for p in sys.argv[1:]:
        try: stat(p)
        except Exception as e: print(f"{p}: ERR {e}")
