#!/usr/bin/env python3
# Re-parse [DRMSTAT] lines from any serial capture and derive the item-9 verdict.
# Order-independent key=0xHEX parsing: immune to the m8_cursor.py positional-regex
# defect where the five new dmg_* fields sit between flip_us and curs_up.
#
# usage: m9_analyze.py <serial-or-drmstat-file> [W] [H]
import sys, re

PATH = sys.argv[1]
W = int(sys.argv[2]) if len(sys.argv) > 2 else 1280
H = int(sys.argv[3]) if len(sys.argv) > 3 else 800
FULL = W * H

KV = re.compile(r"([a-z_]+)=0x([0-9A-Fa-f]+)")
txt = open(PATH, errors="replace").read()
txt = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", re.sub(r"\x1b[=>78]", "", txt))

stats = []
for line in txt.splitlines():
    i = line.find("[DRMSTAT]")
    if i < 0:
        continue
    rec = {k: int(v, 16) for k, v in KV.findall(line[i:])}
    if "t" in rec and "dmg_full" in rec:
        rec["_raw"] = line[i:].strip()
        stats.append(rec)

print(f"parsed {len(stats)} DRMSTAT samples carrying dmg_* fields from {PATH}")
if not stats:
    sys.exit("no usable samples")

FIELDS = ["flips_sub", "flips_del", "dmg_full", "dmg_rect", "dmg_skip",
          "dmg_px", "blobs", "curs_up", "curs_mv", "atomic", "atest",
          "cplane", "evpush", "flip_us", "dirtyfb"]


def g(s, k):
    return s.get(k, 0)


print("\n--- all samples ---")
for s in stats:
    print(f"t={s['t']/100:8.2f}s " +
          " ".join(f"{k}={g(s,k)}" for k in FIELDS))


def report(a, b, label):
    dt = (b["t"] - a["t"]) / 100.0
    if dt <= 0:
        print(f"\n{label}: zero-length window")
        return
    D = {k: g(b, k) - g(a, k) for k in FIELDS}
    print(f"\n==== {label} ({dt:.1f}s, guest ticks {a['t']}..{b['t']}) ====")
    print(f"  evpush/s     : {D['evpush']/dt:8.2f}   [CONTROL: must be ~60]")
    if D["evpush"] <= 0:
        print("  *** NO POINTER ACTIVITY IN THIS WINDOW — evpush did not move. ***")
        print("  *** Any flips/s or cursor_mv/s below is NOT a measurement.  ***")
    print(f"  cursor_mv/s  : {D['curs_mv']/dt:8.2f}   [CONTROL: must NOT fall vs pre-patch 6.0/8.5]")
    print(f"  cursor_up/s  : {D['curs_up']/dt:8.2f}")
    print(f"  flips/s      : {D['flips_sub']/dt:8.2f}   [pre-patch baseline 6.0; pass <= 2.0]")
    print(f"  delivered/s  : {D['flips_del']/dt:8.2f}")
    print(f"  atomic/s     : {D['atomic']/dt:8.2f}")
    print(f"  atest/s      : {D['atest']/dt:8.2f}")
    print(f"  flip_us/s    : {D['flip_us']/dt:8.0f}")
    print(f"  dirtyfb/s    : {D['dirtyfb']/dt:8.2f}")
    s3 = D["dmg_full"] + D["dmg_rect"] + D["dmg_skip"]
    print(f"  dmg full/rect/skip = {D['dmg_full']} / {D['dmg_rect']} / {D['dmg_skip']}")
    print(f"  SANITY  full+rect+skip = {s3}   atomic = {D['atomic']}   "
          f"{'OK (exact)' if s3 == D['atomic'] else 'DELTA %+d (commits naming no primary FB_ID)' % (D['atomic'] - s3)}")
    print(f"  blobs created/s : {D['blobs']/dt:.2f}")
    if D["dmg_rect"] > 0:
        per = D["dmg_px"] / D["dmg_rect"]
        print(f"  dmg_px / dmg_rect = {per:,.0f} px/present = "
              f"{100.0*per/FULL:.2f}% of {W}x{H} ({FULL:,} px = 0x{FULL:X})")
        if per >= 0.90 * FULL:
            print("  >>> NEAR-FULL: compositor damages (nearly) the whole output "
                  "every frame -> BLOCKER IS CLIENT-SIDE.")
        elif per <= 0.05 * FULL:
            print("  >>> SMALL DAMAGE (<=5%): damage tracking works; judge on flips/s.")
        else:
            print("  >>> INTERMEDIATE (5%..90%): neither branch cleanly.")
    else:
        print("  dmg_px/dmg_rect : n/a (no rect-path presents)")
    return D


# window keyed on evpush
best = None
for a, b in zip(stats, stats[1:]):
    dt = (b["t"] - a["t"]) / 100.0
    if dt <= 0:
        continue
    dk = g(b, "evpush") - g(a, "evpush")
    if best is None or dk > best[0]:
        best = (dk, a, b)
if best:
    report(best[1], best[2], "BUSIEST 2s WINDOW BY evpush")

# widest contiguous span where evpush is moving (the whole burst)
moving = [i for i in range(len(stats) - 1)
          if g(stats[i + 1], "evpush") - g(stats[i], "evpush") > 30]
if moving:
    report(stats[moving[0]], stats[moving[-1] + 1],
           f"FULL BURST SPAN (all samples with evpush delta > 30)")

report(stats[0], stats[-1], "CUMULATIVE (first -> last sample)")
