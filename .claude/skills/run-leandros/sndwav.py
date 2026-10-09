#!/usr/bin/env python3
"""Analyse a QEMU wav-backend capture of `sndtest all` (lane hda).

Usage: sndwav.py <capture.wav> [--expect sndtest]

Splits the capture into audible segments (per-channel RMS over 20 ms windows,
gaps of >= 60 ms end a segment), then prints for each segment its start,
duration, per-channel RMS and per-channel frequency (zero crossings with
linear interpolation). With --expect sndtest it checks the pattern that
`sndtest all` plays (repeated any number of times):
  2.0 s 440 Hz both channels, 1.0 s 440 Hz left only, 1.0 s 660 Hz right only,
  1.0 s 440 Hz both channels (44.1 kHz stream)
and exits 0 only if every expected segment is found with the frequency within
3 Hz, the silent channel at least 30 dB below the other, and the duration
within 15 %.
"""
import math
import struct
import sys


def read_wav(path):
    data = open(path, "rb").read()
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise SystemExit(f"{path}: not a RIFF/WAVE file")
    pos, fmt, pcm = 12, None, None
    while pos + 8 <= len(data):
        cid, size = data[pos:pos + 4], struct.unpack("<I", data[pos + 4:pos + 8])[0]
        body = data[pos + 8:pos + 8 + size]
        if cid == b"fmt ":
            fmt = struct.unpack("<HHIIHH", body[:16])
        elif cid == b"data":
            pcm = body if size else data[pos + 8:]  # QEMU may leave size 0 if killed
            if size == 0 or size > len(data):
                pcm = data[pos + 8:]
        pos += 8 + size + (size & 1)
        if size == 0:
            break
    if fmt is None or pcm is None:
        raise SystemExit(f"{path}: missing fmt/data chunk")
    tag, ch, rate, _, _, bits = fmt
    if tag != 1 or bits != 16:
        raise SystemExit(f"{path}: need 16-bit PCM, got tag {tag} bits {bits}")
    n = len(pcm) // (2 * ch)
    s = struct.unpack(f"<{n * ch}h", pcm[:n * ch * 2])
    chans = [s[c::ch] for c in range(ch)]
    if ch == 1:
        chans = [chans[0], chans[0]]
    return rate, chans[0], chans[1]


def rms(x):
    return math.sqrt(sum(v * v for v in x) / len(x)) if x else 0.0


def freq(x, rate):
    """Frequency from rising zero crossings, interpolated."""
    cross = []
    for i in range(1, len(x)):
        a, b = x[i - 1], x[i]
        if a < 0 <= b and b != a:
            cross.append(i - 1 + (-a) / (b - a))
    if len(cross) < 3:
        return 0.0
    return (len(cross) - 1) * rate / (cross[-1] - cross[0])


def segments(rate, left, right, thresh=300.0):
    win = rate // 50  # 20 ms
    n = min(len(left), len(right)) // win
    loud = []
    for w in range(n):
        a, b = w * win, (w + 1) * win
        loud.append(max(rms(left[a:b]), rms(right[a:b])) > thresh)
    segs, start, quiet = [], None, 0
    for w, on in enumerate(loud + [False] * 4):
        if on:
            if start is None:
                start = w
            quiet = 0
        elif start is not None:
            quiet += 1
            if quiet >= 3:
                segs.append((start * win, (w - quiet + 1) * win))
                start, quiet = None, 0
    return segs


def split_on_channel_change(rate, left, right, segs):
    """sndtest's segments can be back-to-back with only a few ms between
    them; split a segment where the L/R balance or frequency changes."""
    out = []
    win = rate // 20  # 50 ms
    for a, b in segs:
        cur = a
        prev_sig = None
        w = a
        while w + win <= b:
            l, r = left[w:w + win], right[w:w + win]
            sig = (rms(l) > 300, rms(r) > 300, round(freq(l if rms(l) > 300 else r, rate) / 50))
            if prev_sig is not None and sig != prev_sig:
                out.append((cur, w))
                cur = w
            prev_sig = sig
            w += win
        out.append((cur, b))
    return [(a, b) for a, b in out if b - a >= rate // 10]


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    rate, left, right = read_wav(sys.argv[1])
    expect = "--expect" in sys.argv
    segs = split_on_channel_change(rate, left, right, segments(rate, left, right))
    print(f"{sys.argv[1]}: {rate} Hz, {len(left) / rate:.2f} s, {len(segs)} audible segment(s)")
    found = []
    for a, b in segs:
        l, r = left[a:b], right[a:b]
        # Trim 30 ms at both ends (fades) for the measurements.
        t = int(rate * 0.03)
        lm, rm_ = l[t:-t] or l, r[t:-t] or r
        lr, rr = rms(lm), rms(rm_)
        lf = freq(lm, rate) if lr > 100 else 0.0
        rf = freq(rm_, rate) if rr > 100 else 0.0
        found.append((a / rate, (b - a) / rate, lr, rr, lf, rf))
        print(f"  t={a / rate:7.2f}s dur {(b - a) / rate:5.2f}s  L rms {lr:7.1f} f {lf:7.1f} Hz | "
              f"R rms {rr:7.1f} f {rf:7.1f} Hz")
    if not expect:
        return 0

    def db(x, y):
        return 20 * math.log10(max(x, 1e-3) / max(y, 1e-3))

    pattern = [("both 440", 2.0, 440, 440), ("left 440", 1.0, 440, 0),
               ("right 660", 1.0, 0, 660), ("both 440 @44.1k", 1.0, 440, 440)]
    k, rounds, ok = 0, 0, True
    fails = []
    i = 0
    while i < len(found):
        name, dur, fl, fr = pattern[k]
        _, d, lr, rr, lf, rf = found[i]
        good = abs(d - dur) <= 0.15 * dur
        if fl:
            good &= abs(lf - fl) <= 3
        else:
            good &= db(rr, lr) >= 30
        if fr:
            good &= abs(rf - fr) <= 3
        else:
            good &= db(lr, rr) >= 30
        if good:
            k += 1
            if k == len(pattern):
                rounds += 1
                k = 0
        else:
            fails.append(f"segment {i} at {found[i][0]:.2f}s does not match '{name}'")
            k = 0
        i += 1
    for f in fails:
        print("  MISMATCH:", f)
    ok = rounds >= 1
    print(f"sndwav: {'PASS' if ok else 'FAIL'}: {rounds} complete sndtest round(s) matched")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
