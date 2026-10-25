import sys, os, gzip, shutil, stat
R, FW, SRC, OUT = sys.argv[1:5]   # extracted alpine root, firmware file, src dir, output initrd.gz
KV = os.listdir(os.path.join(R, "lib/modules"))[0]; MD = os.path.join(R, "lib/modules", KV)
want = ["xhci_pci","usb_storage","uas","sd_mod","vfat","nls_cp437","nls_iso8859_1","nls_utf8","f2fs","crc32_generic","crc32c_generic","crc32c_intel","crc32_pclmul",
        "af_packet","iTCO_wdt","lpc_ich","i2c_i801","libarc4","ccm","gcm","ctr","cmac","ghash_generic","seqiv","aes_generic","aesni_intel","iwlmvm"]
dep = {}; byname = {}
for l in open(os.path.join(MD, "modules.dep")):
    k, v = l.split(":", 1); dep[k] = v.split(); byname[os.path.basename(k).split(".ko")[0].replace("-", "_")] = k
order = []; seen = set()
def visit(k):
    if k in seen: return
    seen.add(k)
    for d in dep[k]: visit(d)
    order.append(k)
missing = [w for w in want if w not in byname]
for w in want:
    if w in byname: visit(byname[w])
print("not modules (builtin or absent):", missing, "| loading", len(order))
st = "/tmp/lcmp-stage"  # placeholder replaced below
st = os.path.join(os.path.dirname(OUT), "stage"); shutil.rmtree(st, ignore_errors=True)
def cp(src, dst, mode=None):
    d = os.path.join(st, dst.lstrip("/")); os.makedirs(os.path.dirname(d), exist_ok=True)
    if os.path.islink(src): os.symlink(os.readlink(src), d)
    else: shutil.copyfile(src, d); os.chmod(d, mode or 0o755)
nmd = os.path.join(st, "lib/modules", KV); os.makedirs(nmd)
for k in order:
    o = os.path.join(nmd, k[:-3] if k.endswith(".gz") else k); os.makedirs(os.path.dirname(o), exist_ok=True)
    open(o, "wb").write(gzip.decompress(open(os.path.join(MD, k), "rb").read()))
strip = lambda k: k[:-3] if k.endswith(".gz") else k
with open(os.path.join(nmd, "modules.dep"), "w") as f:
    for k in order: f.write(strip(k) + ": " + " ".join(strip(d) for d in dep[k]) + "\n")
for x in ("modules.alias", "modules.builtin", "modules.builtin.modinfo", "modules.order", "modules.symbols"):
    if os.path.exists(os.path.join(MD, x)): shutil.copyfile(os.path.join(MD, x), os.path.join(nmd, x))
open(os.path.join(st, "modlist"), "w").write("\n".join(strip(k) for k in order) + "\n")
cp(os.path.join(R, "bin/busybox.static"), "/bin/busybox")
for d in ("lib", "usr/lib"):
    for root, dirs, files in os.walk(os.path.join(R, d)):
        if "/modules" in root or "/firmware" in root: continue
        for fn in files + [x for x in dirs if os.path.islink(os.path.join(root, x))]:
            p = os.path.join(root, fn)
            if ".so" in fn or fn.startswith("ld-musl"): cp(p, "/" + os.path.relpath(p, R))
cp(os.path.join(R, "sbin/wpa_supplicant"), "/sbin/wpa_supplicant"); cp(os.path.join(R, "usr/sbin/iw"), "/usr/sbin/iw")
cp(FW, "/lib/firmware/" + os.path.basename(FW), 0o644)
cp(os.path.join(SRC, "init"), "/init"); cp(os.path.join(SRC, "udhcpc.sh"), "/etc/udhcpc.sh")
cp(os.path.join(os.path.dirname(SRC), "f2rd", "f2rd-x86"), "/bin/f2rd")
# newc cpio, all owned by root
ino = [1000]; out = bytearray()
def ent(name, mode, data=b""):
    ino[0] += 1; n = name.encode() + b"\0"
    h = "070701" + "".join("%08X" % v for v in (ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(n), 0))
    out.extend(h.encode() + n); out.extend(b"\0" * ((4 - len(out) % 4) % 4)); out.extend(data); out.extend(b"\0" * ((4 - len(out) % 4) % 4))
for root, dirs, files in os.walk(st):
    dirs.sort()
    for x in sorted(dirs + files):
        p = os.path.join(root, x); rel = os.path.relpath(p, st); s = os.lstat(p)
        if stat.S_ISLNK(s.st_mode): ent(rel, 0o120777, os.readlink(p).encode())
        elif stat.S_ISDIR(s.st_mode): ent(rel, 0o040755)
        else: ent(rel, 0o100000 | (s.st_mode & 0o777), open(p, "rb").read())
ent("TRAILER!!!", 0)
open(OUT, "wb").write(gzip.compress(bytes(out), 9))
print("initrd", OUT, len(out), "->", os.path.getsize(OUT))
