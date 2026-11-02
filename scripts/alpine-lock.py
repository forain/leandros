#!/usr/bin/env python3
"""Pinned Alpine package set (latest-stable by default) for the LeandrOS image (stdlib only, no container).

  alpine-lock.py refresh <arch> [--cached] [--generated]
  alpine-lock.py fetch   <arch>
  alpine-lock.py verify  <arch>
  alpine-lock.py check   <arch>     exit 1 when latest-stable or any locked package is newer

ports/alpine/world lists the wanted packages, ports/alpine/held what
leandros-base satisfies itself, ports/alpine/scriptallow the maintainer scripts
(other than .trigger) that are accepted. refresh resolves them against the
Alpine APKINDEX (main + community) and writes ports/alpine/<arch>.lock:

  pkg <name> <ver> <repo> <C:Q1...> <sha256 of the .apk> <scripts,comma|->
  held <dep> <requirer>
  generated <sha256 of the generated tar>

fetch (offline when the cache is warm) downloads cache misses, verifies the
sha256 and the control checksum, and unpacks each .apk once into
<cache>/<arch>/x/<sha256>/. The cache is ~/.cache/leandros/apk
($LEANDROS_APK_CACHE). The mirror is trusted over TLS at refresh time only;
afterwards the lock's sha256 is the pin.
"""
import hashlib
import io
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import apkdb  # noqa: E402

ROOT = os.path.dirname(HERE)
ADIR = os.path.join(ROOT, "ports", "alpine")
MIRROR = os.environ.get("ALPINE_MIRROR", "https://dl-cdn.alpinelinux.org/alpine")
REL = os.environ.get("LEANDROS_ALPINE_RELEASE", "")   # set by resolve_latest() / read_lock()
PIN = os.environ.get("LEANDROS_ALPINE_PIN") == "1"      # 1: never move the lock to a newer release
REPOS = ("main", "community")
CACHE = os.environ.get("LEANDROS_APK_CACHE", os.path.expanduser("~/.cache/leandros/apk"))
SCRIPT_NAMES = (".pre-install", ".post-install", ".pre-upgrade", ".post-upgrade",
                ".pre-deinstall", ".post-deinstall", ".trigger")
ARCHS = ("aarch64", "x86_64")
DOCKER_PLATFORM = {"aarch64": "linux/arm64", "x86_64": "linux/amd64"}


def die(msg):
    sys.exit("alpine-lock: " + msg)


def latest_stable(arch):
    """Branch name (v3.24) the mirror's latest-stable points at."""
    p = os.path.join(adir(arch), "latest-releases.yaml")
    download("%s/latest-stable/releases/%s/latest-releases.yaml" % (MIRROR, arch), p)
    m = re.search(r"^\s*branch:\s*(v\d+\.\d+)\s*$", open(p).read(), re.M)
    if not m:
        die("no branch in latest-releases.yaml")
    return m.group(1)


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def adir(arch):
    d = os.path.join(CACHE, arch)
    os.makedirs(d, exist_ok=True)
    return d


def download(url, dest):
    last = None
    for _ in range(3):
        try:
            with urllib.request.urlopen(url, timeout=60) as r, open(dest + ".part", "wb") as f:
                shutil.copyfileobj(r, f)
            os.replace(dest + ".part", dest)
            return
        except Exception as e:  # noqa: BLE001
            last = e
            if getattr(e, "code", 0) == 404:
                break
    if os.path.exists(dest + ".part"):
        os.unlink(dest + ".part")
    die("cannot fetch %s (%s)%s" % (url, last, "; rerun refresh" if getattr(last, "code", 0) == 404 else ""))


# ---- versions (apk order, close enough) -------------------------------------
_SUF = {"alpha": -4, "beta": -3, "pre": -2, "rc": -1, "cvs": 1, "svn": 2, "git": 3, "hg": 4, "p": 5}


def vkey(v):
    rev = 0
    m = re.match(r"^(.*)-r(\d+)$", v)
    if m:
        v, rev = m.group(1), int(m.group(2))
    parts = v.split("_")
    comps = []
    for t in re.findall(r"\d+|[A-Za-z]", parts[0]):
        comps.append((1, int(t), "") if t.isdigit() else (0, 0, t))
    suf = []
    for s in parts[1:]:
        m = re.match(r"([a-z]+)(\d*)", s)
        suf.append((_SUF.get(m.group(1), 0), int(m.group(2) or 0)) if m else (0, 0))
    return comps, suf, rev


def vcmp(a, b):
    ka, kb = vkey(a), vkey(b)
    for x, y in ((ka[0], kb[0]),):
        n = max(len(x), len(y))
        pad = (1, 0, "")
        for i in range(n):
            p, q = (x[i] if i < len(x) else pad), (y[i] if i < len(y) else pad)
            if p != q:
                return -1 if p < q else 1
    sa, sb = ka[1], kb[1]
    for i in range(max(len(sa), len(sb))):
        p = sa[i] if i < len(sa) else (0, 0)
        q = sb[i] if i < len(sb) else (0, 0)
        if p != q:
            return -1 if p < q else 1
    return (ka[2] > kb[2]) - (ka[2] < kb[2])


_DEP = re.compile(r"^(!?)([^<>=~]+?)(?:(>=|<=|=~|~=|>|<|=|~)(.+))?$")


def parse_dep(d):
    m = _DEP.match(d)
    if not m:
        die("unparsable dependency %r" % d)
    return m.group(1) == "!", m.group(2), m.group(3), m.group(4)


def satisfies(have, op, want):
    if op is None or have is None:
        return True
    if op in ("=~", "~=", "~"):
        return have == want or have.startswith(want)
    c = vcmp(have, want)
    return {">=": c >= 0, "<=": c <= 0, ">": c > 0, "<": c < 0, "=": c == 0}[op]


# ---- APKINDEX ---------------------------------------------------------------------------
def load_index(arch, cached):
    pkgs = {}
    for repo in REPOS:
        p = os.path.join(adir(arch), "APKINDEX-%s-%s.tar.gz" % (REL, repo))
        if not (cached and os.path.exists(p)):
            download("%s/%s/%s/%s/APKINDEX.tar.gz" % (MIRROR, REL, repo, arch), p)
        text = tarfile.open(p).extractfile("APKINDEX").read().decode()
        for st in text.split("\n\n"):
            if not st.strip():
                continue
            d = {"deps": [], "prov": [], "iif": [], "repo": repo, "k": 0}
            for line in st.split("\n"):
                k, _, v = line.partition(":")
                if k == "P": d["name"] = v
                elif k == "V": d["ver"] = v
                elif k == "C": d["C"] = v
                elif k == "D": d["deps"] = v.split()
                elif k == "p": d["prov"] = v.split()
                elif k == "i": d["iif"] = v.split()
                elif k == "k": d["k"] = int(v)
            old = pkgs.get(d["name"])
            if old is None or vcmp(d["ver"], old["ver"]) > 0:
                pkgs[d["name"]] = d
    return pkgs


def read_list(path):
    out = []
    try:
        for line in open(path):
            line = line.split("#")[0].strip()
            if line:
                out.append(line)
    except OSError:
        pass
    return out


def resolve(idx, world, held):
    provmap = {}
    for p in idx.values():
        provmap.setdefault(p["name"], []).append((p, p["ver"]))
        for pr in p["prov"]:
            n, _, v = pr.partition("=")
            provmap.setdefault(n, []).append((p, v or None))
    chosen, heldrec, todo = {}, [], []
    for w in world:
        if w not in idx:
            die("world package %s is not in the index" % w)
        chosen[w] = idx[w]
        todo.append(w)

    def is_held(name, cands):
        return name in held or (cands and all(p["name"] in held for p, _ in cands))

    def handle(dep, req):
        neg, name, op, ver = parse_dep(dep)
        if neg:
            return
        cands = [(p, v) for p, v in provmap.get(name, []) if satisfies(v, op, ver)]
        if is_held(name, cands):
            heldrec.append((dep, req))
            return
        if not cands:
            die("%s: dependency %s has no provider (add to ports/alpine/held if the base provides it)" % (req, dep))
        names = sorted({p["name"] for p, _ in cands})
        pick = None
        have = [n for n in names if n in chosen]
        if have:
            pick = have[0]
        elif len(names) == 1:
            pick = names[0]
        elif name in names:
            pick = name
        else:
            top = max(idx[n]["k"] for n in names)
            best = [n for n in names if idx[n]["k"] == top]
            if len(best) == 1 and top > 0:
                pick = best[0]
        if pick is None:
            die("%s: dependency %s has several providers %s and none is in world" % (req, dep, names))
        if pick not in chosen:
            chosen[pick] = idx[pick]
            todo.append(pick)

    while True:
        while todo:
            n = todo.pop(0)
            for dep in chosen[n]["deps"]:
                handle(dep, n)
        added = False
        for p in sorted(idx.values(), key=lambda x: x["name"]):
            if p["name"] in chosen or not p["iif"]:
                continue
            ok = True
            for c in p["iif"]:
                _, n, op, v = parse_dep(c)
                if n not in chosen or not satisfies(chosen[n]["ver"], op, v):
                    ok = False
                    break
            if ok:
                chosen[p["name"]] = p
                todo.append(p["name"])
                added = True
        if not added:
            break
    return chosen, heldrec


# ---- lock file ----------------------------------------------------------------------------
def lock_path(arch):
    return os.path.join(ADIR, arch + ".lock")


def lock_release(arch):
    try:
        for line in open(lock_path(arch)):
            if line.startswith("# release "):
                return line.split()[2]
    except OSError:
        pass
    return ""


def read_lock(arch):
    global REL
    pkgs, held, gen = {}, [], None
    REL = lock_release(arch) or REL
    p = lock_path(arch)
    if not os.path.exists(p):
        die("%s missing; run: alpine-lock.py refresh %s" % (p, arch))
    for line in open(p):
        t = line.split()
        if not t or t[0].startswith("#"):
            continue
        if t[0] == "pkg":
            pkgs[t[1]] = {"name": t[1], "ver": t[2], "repo": t[3], "C": t[4], "sha256": t[5],
                          "scripts": [] if t[6] == "-" else t[6].split(",")}
        elif t[0] == "held":
            held.append((t[1], t[2]))
        elif t[0] == "generated":
            gen = t[1]
    return pkgs, held, gen


def write_lock(arch, pkgs, held, gen):
    with open(lock_path(arch), "w") as f:
        f.write("# release %s\n# Alpine %s lock; generated by scripts/alpine-lock.py refresh\n" % (REL, arch))
        for n in sorted(pkgs):
            p = pkgs[n]
            f.write("pkg %s %s %s %s %s %s\n" % (n, p["ver"], p["repo"], p["C"], p["sha256"],
                                                ",".join(p["scripts"]) or "-"))
        for d, r in sorted(set(held)):
            f.write("held %s %s\n" % (d, r))
        if gen:
            f.write("generated %s\n" % gen)


def apk_name(p):
    return "%s-%s.apk" % (p["name"], p["ver"])


def apk_path(arch, p):
    """Same name-version can be a different build in another release: one dir each."""
    d = os.path.join(adir(arch), REL)
    os.makedirs(d, exist_ok=True)
    return os.path.join(d, apk_name(p))


def control_info(path):
    """(C:, scripts, triggers) from a downloaded .apk."""
    raw = open(path, "rb").read()
    segs = apkdb._apk_segments(raw)
    if len(segs) != 3:
        die("%s: unexpected segment count %d" % (path, len(segs)))
    t = tarfile.open(fileobj=io.BytesIO(segs[1][1]))
    scripts = sorted(m.name[1:] for m in t if m.name in SCRIPT_NAMES)
    return apkdb._b64sha1(segs[1][0]), scripts


def policy_violations(pkgs):
    allow = {tuple(l.split()[:2]) for l in read_list(os.path.join(ADIR, "scriptallow"))}
    bad = []
    for n, p in sorted(pkgs.items()):
        for s in p["scripts"]:
            if s in ("pre-install", "post-install") and (n, s) not in allow:
                bad.append("%s %s" % (n, s))
    return bad


def cmd_refresh(arch, cached, generated):
    global REL
    if not REL:
        old_rel = lock_release(arch)
        REL = old_rel if (PIN and old_rel) else latest_stable(arch)
    world = read_list(os.path.join(ADIR, "world"))
    held = set(read_list(os.path.join(ADIR, "held")))
    idx = load_index(arch, cached)
    chosen, heldrec = resolve(idx, world, held)
    pkgs = {}
    for n, p in sorted(chosen.items()):
        e = {"name": n, "ver": p["ver"], "repo": p["repo"], "C": p["C"], "sha256": "", "scripts": []}
        f = apk_path(arch, e)
        if not os.path.exists(f):
            download("%s/%s/%s/%s/%s" % (MIRROR, REL, p["repo"], arch, apk_name(e)), f)
        e["sha256"] = sha256_file(f)
        c, e["scripts"] = control_info(f)
        if c != p["C"]:
            die("%s: control checksum %s != index %s" % (n, c, p["C"]))
        pkgs[n] = e
    old = read_lock(arch)[2] if os.path.exists(lock_path(arch)) else None
    write_lock(arch, pkgs, [(d, r) for d, r in heldrec], old)
    print("%s: %d packages, %d held references" % (arch, len(pkgs), len(heldrec)))
    bad = policy_violations(pkgs)
    if bad:
        die("maintainer scripts not allowed (ports/alpine/scriptallow): " + "; ".join(bad))
    if generated:
        cmd_generated(arch)


def extract_one(arch, p):
    f = apk_path(arch, p)
    d = os.path.join(adir(arch), "x", p["sha256"])
    if os.path.exists(os.path.join(d, ".done")):
        return d
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    segs = apkdb._apk_segments(open(f, "rb").read())
    t = tarfile.open(fileobj=io.BytesIO(segs[2][1]))
    for m in t:
        dst = os.path.join(d, m.name)
        if m.isdir():
            os.makedirs(dst, exist_ok=True)
            continue
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if m.issym():
            os.symlink(m.linkname, dst)
        elif m.islnk():
            os.link(os.path.join(d, m.linkname), dst)
        elif m.isreg():
            with open(dst, "wb") as o:
                shutil.copyfileobj(t.extractfile(m), o)
            os.chmod(dst, m.mode & 0o7777 | 0o200)
        else:
            die("%s: unsupported member type %r" % (p["name"], m.name))
    open(os.path.join(d, ".done"), "w").close()
    return d


def cmd_check(arch):
    """Exit 1 (and say why) when latest-stable or a locked package moved on."""
    global REL
    pkgs, _, _ = read_lock(arch)
    cur = REL
    new = latest_stable(arch)
    stale = []
    if new != cur:
        print("%s: latest-stable is %s, lock is %s" % (arch, new, cur))
        stale.append("release")
    REL = cur
    idx = load_index(arch, False)
    for n, p in sorted(pkgs.items()):
        q = idx.get(n)
        if q is None:
            print("%s: %s no longer in %s" % (arch, n, cur))
            stale.append(n)
        elif vcmp(q["ver"], p["ver"]) > 0:
            print("%s: %s %s -> %s" % (arch, n, p["ver"], q["ver"]))
            stale.append(n)
    print("%s: lock %s is %s" % (arch, cur, "STALE (%d changes)" % len(stale) if stale else "current"))
    sys.exit(1 if stale else 0)


def cmd_fetch(arch, verify_only=False):
    pkgs, _, _ = read_lock(arch)
    for n, p in sorted(pkgs.items()):
        f = apk_path(arch, p)
        if not os.path.exists(f):
            if verify_only:
                die("%s missing from the cache" % apk_name(p))
            download("%s/%s/%s/%s/%s" % (MIRROR, REL, p["repo"], arch, apk_name(p)), f)
        got = sha256_file(f)
        if got != p["sha256"]:
            os.unlink(f)
            die("%s: sha256 %s != lock %s (cache entry removed)" % (apk_name(p), got, p["sha256"]))
        c, scripts = control_info(f)
        if c != p["C"] or scripts != p["scripts"]:
            die("%s: control data differs from the lock" % apk_name(p))
        if not verify_only:
            extract_one(arch, p)
    bad = policy_violations(pkgs)
    if bad:
        die("maintainer scripts not allowed: " + "; ".join(bad))
    print("%s: %d packages ok" % (arch, len(pkgs)))


# ---- generated tar (container) ------------------------------------------------------------
def _held_provides(arch):
    out = []
    for h in read_list(os.path.join(ADIR, "held")):
        if h.startswith("so:"):
            out.append(h + "=0")
        elif h.startswith("/") or h.startswith("cmd:"):
            continue                 # busybox-binsh in the scaffolding provides these
        else:
            out.append(h + "=0-r99")
    return out


def make_stub_apk(path, provides):
    """A dependency-only .apk (leandros-held) so apk can install the lock packages
    whose held dependencies are normally provided by leandros-base."""
    data = io.BytesIO()
    tarfile.open(fileobj=data, mode="w").close()
    dgz = apkdb_gzip(data.getvalue())
    pi = "\n".join(["pkgname = leandros-held", "pkgver = 1-r0", "pkgdesc = held stub", "url = https://leandros.invalid",
                    "builddate = 0", "packager = leandros", "size = 0", "arch = noarch",
                    "origin = leandros-held", "license = custom",
                    ] + ["provides = " + x for x in provides] + [
                    "datahash = " + hashlib.sha256(dgz).hexdigest()]) + "\n"
    ctl = io.BytesIO()
    with tarfile.open(fileobj=ctl, mode="w") as t:
        ti = tarfile.TarInfo(".PKGINFO")
        b = pi.encode()
        ti.size = len(b)
        t.addfile(ti, io.BytesIO(b))
    open(path, "wb").write(apkdb_gzip(ctl.getvalue()) + dgz)


def apkdb_gzip(b):
    import gzip
    return gzip.compress(b, mtime=0)


CONTAINER_SH = r"""
set -eu
apk add --no-cache python3 gtk+3.0 gtk-update-icon-cache adwaita-icon-theme rsvg-convert >/dev/null
mkdir -p /R /G
apk add --root /R --initdb --keys-dir /etc/apk/keys --repositories-file /etc/apk/repositories busybox busybox-binsh >/dev/null
cd /pkgs
apk add --root /R --allow-untrusted --keys-dir /etc/apk/keys --repositories-file /dev/null \
  $(cat /out/apklist.txt) /out/leandros-held-1-r0.apk 2>&1 | tail -n 5
mkdir -p /out/db
cp /R/lib/apk/db/installed /out/db/installed
cp /R/lib/apk/db/triggers /out/db/triggers 2>/dev/null || :
cp /R/lib/apk/db/scripts.tar /out/db/scripts.tar 2>/dev/null || :
python3 /s/alpine-lock.py _unowned /R /G
python3 /s/alpine-lock.py _icons /icons.txt /G
python3 /s/alpine-lock.py _tar /G /out/generated.tar
"""


def cmd_generated(arch):
    if not shutil.which("docker"):
        die("no container tool (docker) available for --generated")
    pkgs, held, _ = read_lock(arch)
    out = tempfile.mkdtemp(prefix="alpgen-")
    try:
        open(os.path.join(out, "apklist.txt"), "w").write(
            "\n".join(apk_name(p) for _, p in sorted(pkgs.items())) + "\n")
        make_stub_apk(os.path.join(out, "leandros-held-1-r0.apk"), _held_provides(arch))
        cmd_fetch(arch)
        r = subprocess.run(
            ["docker", "run", "--rm", "--name", "apkpkgs-gen-" + arch, "--pids-limit", "512", "--platform", DOCKER_PLATFORM[arch],
             "-v", os.path.join(adir(arch), REL) + ":/pkgs:ro", "-v", out + ":/out", "-v", HERE + ":/s:ro",
             "-v", os.path.join(ROOT, "ports", "firefox", "icons.txt") + ":/icons.txt:ro",
             "alpine:" + REL.lstrip("v"), "sh", "-c", CONTAINER_SH])
        if r.returncode:
            die("container run failed (%d)" % r.returncode)
        tpath = os.path.join(out, "generated.tar")
        sha = sha256_file(tpath)
        shutil.copy(tpath, os.path.join(adir(arch), "generated-%s.tar" % sha))
        write_lock(arch, pkgs, held, sha)
        print("%s: generated-%s.tar (%d bytes)" % (arch, sha[:12], os.path.getsize(tpath)))
        import alpinepkgs
        alpinepkgs.crosscheck(arch, os.path.join(out, "db"))
    finally:
        shutil.rmtree(out, ignore_errors=True)


# ---- container-side helpers (run inside alpine:<release> with python3) -------------------------
def _owned(R):
    files, dirs = set(), set()
    cur = None
    for line in open(os.path.join(R, "lib/apk/db/installed")):
        line = line.rstrip("\n")
        if line.startswith("F:"):
            cur = line[2:]
            dirs.add(cur)
        elif line.startswith("R:") and cur is not None:
            files.add(cur + "/" + line[2:])
        elif not line:
            cur = None
    return files, dirs


UNOWNED_SKIP = ("lib/apk", "etc/apk", "var", "proc", "sys", "dev", "run", "tmp")


def _unowned(R, G, cap=None):
    """Copy files the packages did not own (trigger output) from R into G.
    Safe by construction: regular files and symlinks only (never followed),
    pseudo/host trees skipped, G must be outside R, other filesystems skipped,
    and the total copied is capped (LEANDROS_UNOWNED_CAP_MB, default 2048)."""
    if cap is None:
        cap = int(os.environ.get("LEANDROS_UNOWNED_CAP_MB", "2048")) * 1048576
    R, G = os.path.realpath(R), os.path.realpath(G)
    if G == R or G.startswith(R + os.sep) or R.startswith(G + os.sep):
        die("_unowned: output %s must not overlap the walked tree %s" % (G, R))
    files, _ = _owned(R)
    root_dev = os.lstat(R).st_dev
    total = 0
    for dp, dn, fn in os.walk(R, followlinks=False):
        rel_d = os.path.relpath(dp, R)
        if rel_d == ".":
            rel_d = ""
        if rel_d.startswith(UNOWNED_SKIP):
            dn[:] = []
            continue
        dn[:] = [d for d in dn if not os.path.islink(os.path.join(dp, d))
                 and os.lstat(os.path.join(dp, d)).st_dev == root_dev
                 and (rel_d + "/" + d).lstrip("/") not in UNOWNED_SKIP]
        for n in fn + [d for d in os.listdir(dp) if os.path.islink(os.path.join(dp, d)) and d not in fn]:
            rel = (rel_d + "/" + n).lstrip("/")
            if rel in files:
                continue
            src = os.path.join(dp, n)
            st = os.lstat(src)
            islnk = stat.S_ISLNK(st.st_mode)
            if not islnk and not stat.S_ISREG(st.st_mode):
                continue                       # devices, fifos, sockets
            if not islnk:
                total += st.st_size
                if total > cap:
                    die("_unowned: copied more than the %d MiB cap (at %s); aborting" % (cap >> 20, rel))
            dst = os.path.join(G, rel)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            if islnk:
                os.symlink(os.readlink(src), dst)
            else:
                shutil.copy2(src, dst, follow_symlinks=False)
    print("_unowned: %d bytes copied" % total)


def _icons(listfile, G):
    import ctypes
    gtk = ctypes.CDLL("libgtk-3.so.0")
    gio = ctypes.CDLL("libgio-2.0.so.0")
    gio.g_resources_enumerate_children.restype = ctypes.POINTER(ctypes.c_char_p)
    gio.g_resources_lookup_data.restype = ctypes.c_void_p
    gio.g_bytes_get_data.restype = ctypes.c_void_p
    gio.g_bytes_get_data.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_size_t)]
    gtk.gtk_init_check(None, None)
    css = []

    def walk(path):
        kids = gio.g_resources_enumerate_children(path.encode(), 0, None)
        if not kids:
            return
        i = 0
        while kids[i]:
            k = kids[i].decode()
            p = path + k
            if k.endswith("/"):
                walk(p)
            elif k.endswith(".css"):
                b = gio.g_resources_lookup_data(p.encode(), 0, None)
                if b:
                    n = ctypes.c_size_t()
                    d = gio.g_bytes_get_data(b, ctypes.byref(n))
                    css.append(ctypes.string_at(d, n.value).decode("utf8", "replace"))
            i += 1
    walk("/org/gtk/libgtk/")
    names = set(re.findall(r"gtk-icontheme\([\"']([^\"']*)", "\n".join(css)))
    for l in read_list(listfile):
        names.add(l.split()[0])
    T = os.path.join(G, "usr/share/icons/Adwaita")
    dirs, log = set(), []
    for name in sorted(names):
        src = None
        for dp, _, fn in sorted(os.walk("/usr/share/icons/Adwaita")):
            if name + ".svg" in fn:
                src = os.path.join(dp, name + ".svg")
                break
        if not src:
            log.append("missing " + name)
            continue
        ctx = os.path.basename(os.path.dirname(src))
        for sz in (16, 24, 32, 48):
            d = "%dx%d/%s" % (sz, sz, ctx)
            os.makedirs(os.path.join(T, d), exist_ok=True)
            subprocess.check_call(["rsvg-convert", "-w", str(sz), "-h", str(sz), "-o",
                                   os.path.join(T, d, name + ".png"), src])
            dirs.add((d, sz, ctx))
        log.append("icon    %s  <- %s" % (name, os.path.relpath(src, "/usr/share/icons/Adwaita")))
    if not os.path.exists(os.path.join(T, "16x16/status/image-missing.png")):
        die("no image-missing rendered")
    dirs = sorted(dirs)
    with open(os.path.join(T, "index.theme"), "w") as f:
        f.write("[Icon Theme]\nName=Adwaita\nComment=LeandrOS subset of Adwaita, pre-rendered to PNG (ports/firefox)\n"
                "Inherits=hicolor\nHidden=true\nDirectories=%s\n" % "".join(d + "," for d, _, _ in dirs))
        for d, sz, ctx in dirs:
            f.write("\n[%s]\nSize=%d\nType=Fixed\nContext=%s\n" % (d, sz, ctx[:1].upper() + ctx[1:]))
    subprocess.check_call(["gtk-update-icon-cache", "-f", "-t", T])
    open(os.path.join(G, "usr/share/icons/Adwaita", "ICONS.txt"), "w").write("\n".join(log) + "\n")


def _tar(G, out):
    with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as t:
        for dp, dn, fn in sorted(os.walk(G)):
            dn.sort()
            for n in sorted(fn + [d for d in dn if os.path.islink(os.path.join(dp, d))]):
                p = os.path.join(dp, n)
                ti = t.gettarinfo(p, os.path.relpath(p, G))
                ti.uid = ti.gid = 0
                ti.uname = ti.gname = ""
                ti.mtime = 0
                ti.pax_headers = {}
                if ti.isreg():
                    with open(p, "rb") as f:
                        t.addfile(ti, f)
                else:
                    t.addfile(ti)


def main(argv):
    if len(argv) < 2:
        die(__doc__)
    c = argv[1]
    if c == "_unowned":
        return _unowned(argv[2], argv[3])
    if c == "_icons":
        return _icons(argv[2], argv[3])
    if c == "_tar":
        return _tar(argv[2], argv[3])
    if len(argv) < 3 or argv[2] not in ARCHS:
        die("usage: alpine-lock.py refresh|fetch|verify|check <aarch64|x86_64> [--cached] [--generated]")
    arch, flags = argv[2], argv[3:]
    if c == "refresh":
        cmd_refresh(arch, "--cached" in flags, "--generated" in flags)
    elif c == "fetch":
        cmd_fetch(arch)
    elif c == "check":
        cmd_check(arch)
    elif c == "verify":
        cmd_fetch(arch, verify_only=True)
    else:
        die("unknown command " + c)


if __name__ == "__main__":
    main(sys.argv)
