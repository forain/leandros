import sys, struct, math
f = open(sys.argv[1], 'rb').read()
# parse RIFF
assert f[:4] == b'RIFF'
i = 12; fmt = None; data = None
while i < len(f) - 8:
    cid = f[i:i+4]; sz = struct.unpack('<I', f[i+4:i+8])[0]
    if cid == b'fmt ': fmt = struct.unpack('<HHIIHH', f[i+8:i+24])
    if cid == b'data': data = f[i+8:i+8+sz] if sz and sz != 0xffffffff else f[i+8:]; break
    i += 8 + sz
ch, rate = fmt[1], fmt[2]
n = len(data) // (2 * ch)
s = struct.unpack('<%dh' % (n * ch), data[:n * ch * 2])
L = s[0::ch]
win = rate // 10
print(f"rate {rate} ch {ch} dur {n/rate:.2f}s")
def goertzel(x, fr):
    k = 2 * math.cos(2 * math.pi * fr / rate); a = b = 0.0
    for v in x: a, b = v + k * a - b, a
    return math.sqrt(max(a*a + b*b - k*a*b, 0)) / len(x) * 2
rows = []
for w in range(n // win):
    x = L[w*win:(w+1)*win]
    rms = math.sqrt(sum(v*v for v in x) / len(x))
    zc = sum(1 for j in range(1, len(x)) if (x[j-1] < 0) != (x[j] < 0)) * rate / len(x) / 2
    rows.append((w/10, rms, zc))
# segments of sound
segs = []; cur = None
for t, r, z in rows:
    on = r > 200
    if on and cur is None: cur = [t, t, [], []]
    if on: cur[1] = t; cur[2].append(r); cur[3].append(z)
    if not on and cur is not None:
        segs.append(cur); cur = None
if cur: segs.append(cur)
for a, b, rs, zs in segs:
    zs2 = sorted(zs); rs2 = sorted(rs)
    print(f"sound {a:6.1f}-{b:6.1f}s  len {b-a+0.1:5.1f}s  RMS med {rs2[len(rs2)//2]:7.0f} min {rs2[0]:6.0f}  freq(zc) med {zs2[len(zs2)//2]:6.1f}")
# fine holes: 10 ms windows with near-zero inside segments
fw = rate // 100; holes = 0
for a, b, rs, zs in segs:
    if b - a < 1: continue
    for w in range(int(a*100)+5, int(b*100)-5):
        x = L[w*fw:(w+1)*fw]
        if max(abs(v) for v in x) < 300: holes += 1
print("10ms near-silent windows inside sound segments:", holes)
