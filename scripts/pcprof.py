#!/usr/bin/env python3
"""pcprof.py - host-side symboliser for the guest's /proc/pcsample profiler.

Guest side (always on, see sched/src/pcsample.rs): /proc/pcsample returns the
timer-tick PC samples taken since the previous read, one line
`cpu tgid pid sc mode pc` (mode u/k/i). Capture a window plus the maps of the
processes of interest in ONE file, e.g. from the serial root shell:

    cat /proc/pcsample >/dev/null; sleep 20; cat /proc/pcsample > /tmp/pcs.txt
    for p in $(ls /proc); do [ -r /proc/$p/maps ] && { echo "=== MAPS $p"; cat /proc/$p/maps; }; done >> /tmp/pcs.txt

(ffsession.py --post runs such a command; its output lands in post.txt.)

    pcprof.py CAPTURE --root ports/firefox/out/x86_64 [--tgid N] [--top 25] [--disasm 8]

--root is the host copy of the guest's root filesystem (a guest path /usr/lib/x
is read from ROOT/usr/lib/x). Output: samples per thread, per mapped file, and
the hottest symbols (ELF .symtab/.dynsym; stripped binaries - Firefox's libxul -
fall back to `file+0xVADDR`, with the nearest dynamic symbol). --disasm N prints
the instructions around the N hottest addresses via `objdump` so the SIMD level
(xmm = SSE, ymm = AVX2, scalar) is visible even without symbols.
"""
import argparse, bisect, collections, os, re, struct, subprocess, sys

def elf_info(path, _c={}):
    """(load segments [(vaddr, off, filesz)], sorted [(addr, size, name)]) of an ELF64."""
    if path in _c: return _c[path]
    segs, syms = [], []
    try:
        with open(path, 'rb') as f:
            h = f.read(64)
            if h[:4] != b'\x7fELF' or h[4] != 2: raise ValueError
            phoff, shoff = struct.unpack_from('<QQ', h, 32)
            phentsize, phnum, shentsize, shnum = struct.unpack_from('<HHHH', h, 54)
            f.seek(phoff); ph = f.read(phentsize * phnum)
            for i in range(phnum):
                t, fl, off, va, pa, fsz, msz, al = struct.unpack_from('<IIQQQQQQ', ph, i * phentsize)
                if t == 1: segs.append((va, off, fsz))
            f.seek(shoff); sh = f.read(shentsize * shnum)
            secs = [struct.unpack_from('<IIQQQQIIQQ', sh, i * shentsize) for i in range(shnum)]
            for (nm, ty, fl, ad, off, sz, link, info, al, es) in secs:
                if ty not in (2, 11): continue   # SYMTAB, DYNSYM
                f.seek(off); data = f.read(sz)
                lo, lsz = secs[link][4], secs[link][5]
                f.seek(lo); strs = f.read(lsz)
                for j in range(0, len(data) - 23, 24):
                    n, info_, oth, shn, val, size = struct.unpack_from('<IBBHQQ', data, j)
                    if (info_ & 0xf) in (1, 2) and shn != 0 and val:
                        e = strs.index(b'\0', n)
                        syms.append((val, size, strs[n:e].decode('latin1')))
            syms.sort()
    except (OSError, ValueError, struct.error):
        pass
    _c[path] = (segs, syms, [s[0] for s in syms])
    return _c[path]

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('capture'); ap.add_argument('--root', required=True)
    ap.add_argument('--tgid', type=int, action='append'); ap.add_argument('--top', type=int, default=25)
    ap.add_argument('--disasm', type=int, default=0)
    ap.add_argument('--bucket', type=lambda x: int(x, 0), default=0, help='also aggregate hottest addresses into BUCKET-byte blocks (find the hot function without symbols)')
    a = ap.parse_args()
    samples, maps, cur = [], collections.defaultdict(list), None
    for line in open(a.capture, errors='replace'):
        line = line.rstrip('\n')
        m = re.match(r'=== MAPS (\d+)', line)
        if m: cur = int(m.group(1)); continue
        if cur is not None:
            m = re.match(r'([0-9a-f]+)-([0-9a-f]+) (\S+) ([0-9a-f]+) \S+ \d+\s*(.*)', line)
            if m: maps[cur].append((int(m.group(1), 16), int(m.group(2), 16), int(m.group(4), 16), m.group(5)))
            continue
        p = line.split()
        if len(p) == 6 and p[0].isdigit():
            samples.append((int(p[1]), int(p[2]), p[4], int(p[5], 16)))
    if a.tgid: samples = [s for s in samples if s[0] in a.tgid]
    tot = len(samples) or 1
    print(f'{len(samples)} samples')
    for k, label in ((lambda s: (s[0], s[1]), 'tgid/pid'), (lambda s: s[2], 'mode')):
        c = collections.Counter(k(s) for s in samples)
        print(f'-- by {label}:', ', '.join(f'{x}={n} ({100*n//tot}%)' for x, n in c.most_common(10)))
    files, syms, addrs = collections.Counter(), collections.Counter(), collections.Counter()
    for tgid, pid, mode, pc in samples:
        if mode != 'u': files['<' + mode + '>'] += 1; continue
        hit = next((m for m in maps.get(tgid, ()) if m[0] <= pc < m[1]), None)
        if not hit: files['<unmapped>'] += 1; continue
        name = hit[3] or '<anon>'
        files[name] += 1
        path = a.root + name
        segs, sy, keys = elf_info(path) if name.startswith('/') else ([], [], [])
        off = pc - hit[0] + hit[2]
        va = next((s[0] + off - s[1] for s in segs if s[1] <= off < s[1] + s[2]), off)
        i = bisect.bisect_right(keys, va) - 1
        sym = f'{sy[i][2]}+{va - sy[i][0]:#x}' if i >= 0 and (sy[i][1] == 0 or va < sy[i][0] + sy[i][1]) else \
              (f'~{sy[i][2]}+{va - sy[i][0]:#x}?' if i >= 0 else f'+{va:#x}')
        syms[(os.path.basename(name), sym.split('+')[0])] += 1
        addrs[(path, va)] += 1
    print('-- by file:')
    for n, c in files.most_common(12): print(f'  {c:6} {100*c/tot:5.1f}%  {n}')
    print('-- hottest symbols (stripped => nearest dynsym marked ~):')
    for (f, s), c in syms.most_common(a.top): print(f'  {c:6} {100*c/tot:5.1f}%  {f}  {s}')
    print('-- hottest addresses (file, vaddr):')
    for (p, va), c in addrs.most_common(a.top): print(f'  {c:6} {100*c/tot:5.1f}%  {os.path.basename(p)}+{va:#x}')
    if a.bucket:
        bk = collections.Counter()
        for (p, va), c in addrs.items(): bk[(os.path.basename(p), va // a.bucket * a.bucket)] += c
        print(f'-- hottest {a.bucket:#x}-byte blocks:')
        for (f, b), c in bk.most_common(a.top): print(f'  {c:6} {100*c/tot:5.1f}%  {f}+{b:#x}')
    for (p, va), c in addrs.most_common(a.disasm):
        print(f'== {os.path.basename(p)}+{va:#x} ({c} samples)')
        r = subprocess.run(['objdump', '-d', '--no-show-raw-insn', f'--start-address={va-48:#x}',
                            f'--stop-address={va+48:#x}', p], capture_output=True, text=True)
        print('\n'.join(r.stdout.splitlines()[-30:]))

if __name__ == '__main__': main()
