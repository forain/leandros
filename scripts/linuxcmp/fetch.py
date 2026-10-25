import sys, os, io, tarfile, urllib.request
base = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/"
out = sys.argv[1]; want = sys.argv[2:]
os.makedirs(out, exist_ok=True)
idx = urllib.request.urlopen(base + "APKINDEX.tar.gz").read()
t = tarfile.open(fileobj=io.BytesIO(idx))
txt = t.extractfile("APKINDEX").read().decode()
pk = {}; prov = {}
for blk in txt.split("\n\n"):
    d = {}; deps=[]; provs=[]
    for l in blk.splitlines():
        if len(l) > 2 and l[1] == ":":
            k, v = l[0], l[2:]
            if k == "D": deps = v.split()
            elif k == "p": provs = v.split()
            else: d[k] = v
    if "P" not in d: continue
    d["deps"] = deps; pk[d["P"]] = d
    for p in provs: prov.setdefault(p.split("=")[0], d["P"])
    prov.setdefault(d["P"], d["P"])
skip = {"linux-firmware-any", "mkinitfs", "busybox", "busybox-binsh", "alpine-baselayout", "openrc", "ifupdown-ng", "/bin/sh"}
seen = set(); todo = list(want)
while todo:
    n = todo.pop()
    n = n.lstrip("!") if not n.startswith("!") else None
    if not n: continue
    n = n.split(">")[0].split("<")[0].split("=")[0].split("~")[0]
    if n in skip: continue
    p = prov.get(n)
    if not p: print("unresolved", n); continue
    if p in seen or p in skip: continue
    seen.add(p); todo += pk[p]["deps"]
for p in sorted(seen):
    f = f"{p}-{pk[p]['V']}.apk"
    print(f, pk[p].get("S"))
    if not os.path.exists(os.path.join(out, f)):
        urllib.request.urlretrieve(base + f, os.path.join(out, f))
