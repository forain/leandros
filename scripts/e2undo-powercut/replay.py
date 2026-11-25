#!/usr/bin/env python3
"""replay.py LOG PRE_IMG UNDO_OUTDIR... : power-cut crash-state builder.
usage: replay.py LOG PRE T_IMG CUTS SEED OUTDIR
Reads the ordered write trace (file 0 = image, file 1 = undo file). For each
sampled cut point k it writes the disk state a power cut right after op k could
leave: image = every image write up to k (worst case: the OS happened to write
all of it back), undo = only what was fsynced before k (adversarial) -- plus a
'rand' variant keeping a random subset of the unflushed undo writes.
Prints lines 'case<N> k=.. mode=.. durable_undo_ops=..' ; files OUTDIR/case<N>.{img,undo}."""
import sys, struct, random, os, shutil
log, pre, cuts, seed, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
random.seed(seed)
ops = []
with open(log, 'rb') as f:
    while True:
        h = f.read(24)
        if len(h) < 24: break
        op, fi = h[0], h[1]
        off, ln = struct.unpack('<QQ', h[8:24])
        data = f.read(ln) if op == 1 else b''
        ops.append((op, fi, off, ln, data))
# cut candidates: right after each image write that follows an undo write (the dangerous moments)
cand, last_undo = [], False
for i, (op, fi, off, ln, d) in enumerate(ops):
    if op == 1 and fi == 1: last_undo = True
    if op == 1 and fi == 0 and last_undo: cand.append(i); last_undo = False
if not cand: print("NO-CANDIDATES"); sys.exit(3)
step = max(1, len(cand) // cuts)
sel = cand[::step][:cuts]
os.makedirs(out, exist_ok=True)
def apply(path, wl, base=None):
    if base: shutil.copyfile(base, path)
    else: open(path, 'wb').close()
    with open(path, 'r+b') as g:
        for (op, fi, off, ln, d) in wl:
            if op == 1: g.seek(off); g.write(d)
            elif op == 3: g.truncate(ln)
OKR = [(1072,1075),(1082,1083),(1088,1091),(1400,1407),(2044,2047),(1652,1655)]
import subprocess
def diffbytes(a, b, limit, cap=100000):
    """bytes differing outside the documented superblock fields (blockwise; counting stops at cap)."""
    bad = 0
    with open(a, 'rb') as fa, open(b, 'rb') as fb:
        pos = 0
        while pos < limit and bad < cap:
            x, y = fa.read(4096), fb.read(4096)
            x = x[:limit - pos]; y = y[:limit - pos]
            if not x and not y: break
            if x != y:
                n = min(len(x), len(y))
                for j in range(n):
                    if x[j] != y[j] and not any(lo <= pos + j <= hi for lo, hi in OKR): bad += 1
                bad += abs(len(x) - len(y))
            pos += max(len(x), len(y), 1)
    return bad
LIM = int(os.environ.get('CMP_LIMIT', 64 << 20))
fails = total = 0
n = 0
for k in sel:
    pref = ops[:k + 1]
    img = [o for o in pref if o[1] == 0 and o[0] == 1]
    und = [o for o in pref if o[1] == 1]
    lastsync = max([i for i, o in enumerate(und) if o[0] == 2], default=-1)
    durable = und[:lastsync + 1]
    unflushed = [o for o in und[lastsync + 1:] if o[0] in (1, 3)]
    for mode in ('adv', 'rand'):
        keep = [o for o in unflushed if mode == 'rand' and random.random() < 0.5]
        u = durable + keep
        base = f"{out}/case"
        apply(base + '.img', img, pre); apply(base + '.undo', u)
        r = subprocess.run(['e2undo', base + '.undo', base + '.img'], capture_output=True)
        bad = diffbytes(pre, base + '.img', LIM)
        total += 1
        verdict = 'ok' if bad == 0 else 'CORRUPT'
        if bad: fails += 1
        print(f"case{n} k={k}/{len(ops)} {mode} durable_undo_ops={len(durable)} lost={len(unflushed)-len(keep)} e2undo_rc={r.returncode} diff={bad} {verdict}", flush=True)
        n += 1
for p in ('case.img', 'case.undo'):
    try: os.unlink(f"{out}/{p}")
    except OSError: pass
print(f"POWERCUT cases={total} corrupt={fails}")
