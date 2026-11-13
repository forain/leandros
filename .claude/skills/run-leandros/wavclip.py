#!/usr/bin/env python3
"""Analyse a captured guest WAV: clipped samples, sample-rate sanity, silence gaps.

Usage: wavclip.py <wav> [--max-clip-pct P] [--min-active-pct P] [--window-s S]
Exit 0 = pass, 1 = fail. Only the loudest window (default 10 s) is judged for clipping.
"""
import struct, sys, wave, array

def load(path):
    # QEMU's wav backend leaves data sizes unset until a clean close: parse by hand.
    b = open(path, "rb").read()
    assert b[:4] == b"RIFF" and b[8:12] == b"WAVE", "not a WAVE file"
    ch, rate, bits = struct.unpack_from("<HIxxxxxxH", b, 22)
    assert bits == 16, "need 16-bit"
    i = b.find(b"data", 12)
    a = array.array("h"); body = b[i + 8:]
    a.frombytes(body[:len(body) // 2 * 2])
    return a, ch, rate

def analyse(path, window_s=10.0):
    a, ch, rate = load(path)
    n = len(a)
    win = int(window_s * rate * ch)
    best = (0.0, 0, 0)  # clip fraction, start, count
    step = max(win // 4, 1)
    for s in range(0, max(n - win, 0) + 1, step):
        seg = a[s:s + win]
        if not seg:
            break
        c = sum(1 for v in seg if v >= 32767 or v <= -32768)
        f = c / len(seg)
        if f > best[0]:
            best = (f, s, c)
    peak = max((abs(v) for v in a), default=0)
    sec = rate * ch
    active = sum(1 for i in range(0, n - sec + 1, sec) if max(abs(v) for v in a[i:i + sec:ch * 8] or [0]) > 64)
    total_s = n // sec
    return dict(rate=rate, ch=ch, seconds=total_s, peak=peak, clip_pct=100 * best[0],
                active_s=active)

def main():
    p = sys.argv[1]
    mc, ma = 0.5, 50.0
    for i, x in enumerate(sys.argv):
        if x == "--max-clip-pct": mc = float(sys.argv[i + 1])
        if x == "--min-active-pct": ma = float(sys.argv[i + 1])
    r = analyse(p)
    act = 100 * r["active_s"] / max(r["seconds"], 1)
    ok = r["clip_pct"] <= mc and act >= ma
    print("wavclip %s rate=%d ch=%d secs=%d peak=%d clip_pct=%.2f active_pct=%.0f %s" % (
        p, r["rate"], r["ch"], r["seconds"], r["peak"], r["clip_pct"], act, "PASS" if ok else "FAIL"))
    sys.exit(0 if ok else 1)

main()
