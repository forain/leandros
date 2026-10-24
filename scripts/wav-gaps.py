#!/usr/bin/env python3
"""wav-gaps.py <capture.wav> [--from S] [--to S] [--max-gaps N] [--min-ms MS]

Count playback underruns in a QEMU audiodev WAV capture (S16LE stereo; the
RIFF sizes may be unfinalized, the rate is read from the fmt chunk).

An underrun is a hole the audio path inserted, not a quiet passage: a run of
digital silence (|s| <= 2 on both channels) of at least --min-ms (default 2)
that CUTS a signal (peak > 256 in the 1 ms before it) and that the signal
RESUMES from (peak > 256 in the 1 ms after it). Music that decays to silence,
or a sound effect that ends, is not counted.

Prints one summary line plus the gap-length histogram and the median spacing,
and exits 1 when --max-gaps N is given and more than N underruns were found
(the guard used by scripts/doom-audio-gap-check.sh)."""
import struct
import sys


def opt(name, default, conv):
    a = sys.argv
    return conv(a[a.index(name) + 1]) if name in a else default


def main():
    path = sys.argv[1]
    data = open(path, "rb").read()
    rate, off = 44100, 44
    i = 12
    while i + 8 <= len(data):
        cid, size = data[i:i + 4], struct.unpack("<I", data[i + 4:i + 8])[0]
        if cid == b"fmt ":
            rate = struct.unpack("<I", data[i + 12:i + 16])[0]
        if cid == b"data":
            off = i + 8
            break
        i += 8 + size
    pcm = data[off:]
    n = len(pcm) // 4
    s = struct.unpack("<%dh" % (2 * n), pcm[:4 * n])
    lo = int(opt("--from", 0.0, float) * rate)
    hi = min(n, int(opt("--to", n / rate, float) * rate))
    min_run = max(1, int(opt("--min-ms", 2.0, float) * rate / 1000))
    ms1 = max(1, rate // 1000)

    def peak(a, b):
        a, b = max(lo, a), min(hi, b)
        return max((abs(v) for v in s[2 * a:2 * b]), default=0)

    gaps = []
    run_start = None
    for f in range(lo, hi):
        quiet = abs(s[2 * f]) <= 2 and abs(s[2 * f + 1]) <= 2
        if quiet:
            if run_start is None:
                run_start = f
            continue
        if run_start is not None and f - run_start >= min_run:
            if peak(run_start - ms1, run_start) > 256 and peak(f, f + ms1) > 256:
                gaps.append((run_start, f - run_start))
        run_start = None
    total_ms = sum(g[1] for g in gaps) * 1000.0 / rate
    span = (hi - lo) / rate
    print(f"underruns={len(gaps)} total={total_ms:.1f}ms span={span:.2f}s rate={rate} file={path}")
    if gaps:
        hist = {}
        for _, ln in gaps:
            b = int(ln * 1000.0 / rate)
            hist[b] = hist.get(b, 0) + 1
        top = sorted(hist.items(), key=lambda kv: -kv[1])[:6]
        print("  length_ms(count):", " ".join(f"{k}({v})" for k, v in top))
        sp = sorted((gaps[k + 1][0] - gaps[k][0]) * 1000.0 / rate for k in range(len(gaps) - 1))
        if sp:
            print(f"  spacing_ms median={sp[len(sp) // 2]:.1f} p10={sp[len(sp) // 10]:.1f} p90={sp[len(sp) * 9 // 10]:.1f}")
        print("  first_at_s:", " ".join(f"{g[0] / rate:.3f}" for g in gaps[:8]))
    limit = opt("--max-gaps", None, int)
    if limit is not None and len(gaps) > limit:
        print(f"FAIL: {len(gaps)} underruns > {limit}")
        sys.exit(1)


if __name__ == "__main__":
    main()
