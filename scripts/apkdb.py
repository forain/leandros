#!/usr/bin/env python3
"""Generate the apk "installed" database entry that owns the LeandrOS base image.

apk (Alpine's package manager, ports/apk) runs on the live root. For it never to
replace or delete a base file, mkfs-f2fs-populated.py writes /lib/apk/db/installed
with one synthetic package, leandros-base, that lists every base file and
provides the sonames and Alpine packages the image already contains. See
docs/userland/ports.md "Live-root apk". Stdlib only.
"""
import base64
import hashlib
import os
import re
import io
import struct
import tarfile
import time
import zlib

PKG = "leandros-base"

# Paths apk must be free to create, replace or rewrite (never owned by the base).
EXCLUDE_FILES = {"etc/apk/world", "etc/apk/arch", "etc/passwd", "etc/group",
                 "etc/shadow", "etc/resolv.conf"}
EXCLUDE_PREFIXES = ("lib/apk/", "home/", "root/", "tmp/", "var/", "run/", "proc/",
                    "sys/", "dev/", "mnt/", "media/")

# musl=1.2.5-r99 (the fake provide) is added by mkfs only for LEANDROS_LIBC=zig;
# with alpine, musl is a real package record (apk_stanza) leandros-base depends on.
STATIC_PROVIDES = [
    "/bin/sh=1", "cmd:sh=1",
    "alpine-baselayout=3.6.8-r99", "alpine-baselayout-data=3.6.8-r99",
]


def elf_soname(path):
    """DT_SONAME of an ELF64 little-endian shared object, or None."""
    try:
        with open(path, "rb") as f:
            hdr = f.read(64)
            if len(hdr) < 64 or hdr[:4] != b"\x7fELF" or hdr[4] != 2 or hdr[5] != 1:
                return None
            phoff, = struct.unpack_from("<Q", hdr, 32)
            phentsize, phnum = struct.unpack_from("<HH", hdr, 54)
            loads, dyn = [], None
            for i in range(phnum):
                f.seek(phoff + i * phentsize)
                p_type, _fl, p_off, p_vaddr, _pa, p_filesz, _ms, _al = struct.unpack(
                    "<IIQQQQQQ", f.read(56))
                if p_type == 1:
                    loads.append((p_vaddr, p_off, p_filesz))
                elif p_type == 2:
                    dyn = (p_off, p_filesz)
            if dyn is None:
                return None
            f.seek(dyn[0])
            data = f.read(dyn[1])
            strtab = soname_off = None
            for off in range(0, len(data) - 15, 16):
                tag, val = struct.unpack_from("<qQ", data, off)
                if tag == 0:
                    break
                if tag == 5:
                    strtab = val
                elif tag == 14:
                    soname_off = val
            if strtab is None or soname_off is None:
                return None
            for va, fo, fsz in loads:
                if va <= strtab < va + fsz:
                    f.seek(fo + (strtab - va) + soname_off)
                    buf = f.read(256)
                    return buf.split(b"\0", 1)[0].decode("ascii", "replace") or None
    except (OSError, struct.error):
        return None
    return None


_PKGVER = re.compile(r"^(.+)-(\d[^-]*-r\d+)$")


def closure_provides(port_out_dirs):
    """name=ver provides for every Alpine package shipped through the ports:
    CLOSURE.txt `alpine <soname> <- <path> (<pkg>-<ver>-rN)` and GIT-VERSION."""
    out = []
    for d in port_out_dirs:
        toks = []
        try:
            with open(os.path.join(d, "CLOSURE.txt")) as f:
                for line in f:
                    if line.startswith("alpine ") and line.rstrip().endswith(")"):
                        toks.append(line.rstrip().rsplit("(", 1)[1][:-1])
        except OSError:
            pass
        try:
            with open(os.path.join(d, "GIT-VERSION")) as f:
                toks += f.read().split()
        except OSError:
            pass
        for t in toks:
            m = _PKGVER.match(t)
            if m:
                out.append("%s=%s" % (m.group(1), m.group(2)))
    return out


def _vkey(v):
    return [(0, int(x), "") if x.isdigit() else (1, 0, x)
            for x in re.split(r"[.\-_]|(?<=\d)(?=\D)|(?<=\D)(?=\d)", v) if x]


def dedupe_provides(provs):
    best = {}
    for p in provs:
        name, _, ver = p.partition("=")
        if name not in best or _vkey(ver) > _vkey(best[name]):
            best[name] = ver
    return ["%s=%s" % (n, v) if v else n for n, v in sorted(best.items())]


def so_provides(lib_entries):
    """lib_entries: (image_name, host_path) for files directly in /lib and
    /usr/lib. Returns so:<SONAME>=<ver>; ver is the digits after .so. in the
    longest (real) file name sharing a host file, else 0."""
    longest = {}
    for name, host in lib_entries:
        if not isinstance(host, str):
            continue
        if host not in longest or len(name) > len(longest[host]):
            longest[host] = name
    out = []
    for host, name in longest.items():
        so = elf_soname(host)
        if not so:
            continue
        m = re.search(r"\.so\.([0-9][0-9.]*)$", name)
        out.append("so:%s=%s" % (so, m.group(1) if m else "0"))
    return out


def excluded(rel):
    return rel in EXCLUDE_FILES or rel.startswith(EXCLUDE_PREFIXES)


def musl_excluded(rel):
    """Paths owned by the real musl package (LEANDROS_LIBC=alpine) or left
    unowned (/usr/lib/libc.so: a symlink a later `apk add musl-dev` may take)."""
    return (rel.startswith("lib/ld-musl-") or rel.startswith("lib/libc.musl-")
            or rel == "usr/lib/libc.so")


def _b64sha1(data):
    return "Q1" + base64.b64encode(hashlib.sha1(data).digest()).decode()


def _apk_segments(data):
    """Split an .apk (concatenated gzip members) -> list of (gz bytes, plain bytes)."""
    segs = []
    while data:
        o = zlib.decompressobj(31)
        plain = o.decompress(data)
        n = len(data) - len(o.unused_data)
        segs.append((data[:n], plain))
        data = o.unused_data
    return segs


def read_apk(apk_path, content=True):
    """Returns (size, control_gz, pkginfo{key: [values]}, [(tarinfo, content bytes)]);
    with content=False the member contents are left empty (headers only)."""
    raw = open(apk_path, "rb").read()
    segs = _apk_segments(raw)
    if len(segs) != 3:
        raise ValueError("%s: expected signature+control+data segments, got %d" % (apk_path, len(segs)))
    ctl_gz, ctl = segs[1]
    t = tarfile.open(fileobj=io.BytesIO(ctl))
    info = {}
    for line in t.extractfile(".PKGINFO").read().decode().splitlines():
        if line and not line.startswith("#") and " = " in line:
            k, v = line.split(" = ", 1)
            info.setdefault(k, []).append(v)
    members = []
    d = tarfile.open(fileobj=io.BytesIO(segs[2][1]))
    for m in d:
        members.append((m, d.extractfile(m).read() if content and m.isreg() else b""))
    return len(raw), ctl_gz, info, members


def apk_file(apk_path, name):
    """Content of one regular file of the .apk's data tar."""
    for m, content in read_apk(apk_path)[3]:
        if m.name == name and m.isreg():
            return content
    raise KeyError(name)


def index_checksum(index_tar_gz, pkg):
    """The C: line of package `pkg` in a signed APKINDEX.tar.gz."""
    t = tarfile.open(index_tar_gz)
    idx = t.extractfile("APKINDEX").read().decode()
    for stanza in idx.split("\n\n"):
        lines = stanza.split("\n")
        if "P:" + pkg in lines:
            return [l for l in lines if l.startswith("C:")][0][2:]
    raise KeyError(pkg)


def apk_stanza(apk_path, index_tar_gz=None):
    """Installed-db record of a real .apk (what `apk add` would have written).
    With index_tar_gz, asserts C: equals the repo index's (so apk sees the
    repository package as identical to the installed one)."""
    size, ctl_gz, info, members = read_apk(apk_path)
    chk = _b64sha1(ctl_gz)
    pkg = info["pkgname"][0]
    if index_tar_gz:
        want = index_checksum(index_tar_gz, pkg)
        if want != chk:
            raise SystemExit("apkdb: %s C: %s != repository index %s" % (pkg, chk, want))
    one = lambda k: info.get(k, [""])[0]
    L = ["C:" + chk, "P:" + pkg, "V:" + one("pkgver"), "A:" + one("arch"),
         "S:%d" % size, "I:%s" % one("size"), "T:" + one("pkgdesc"), "U:" + one("url"),
         "L:" + one("license"), "o:" + one("origin"), "m:" + one("maintainer"),
         "t:" + one("builddate")]
    L = [l for l in L if len(l) > 2 or l[0] in "CPVAS"]        # apk omits empty fields
    if one("commit"):
        L.append("c:" + one("commit"))
    if one("provider_priority"):
        L.append("k:" + one("provider_priority"))
    # apk-tools keeps one D:/p:/r:/i: line per record, space separated
    if info.get("depend"):
        L.append("D:" + " ".join(info["depend"]))
    if info.get("provides"):
        L.append("p:" + " ".join(info["provides"]))
    if info.get("replaces"):
        L.append("r:" + " ".join(info["replaces"]))
    if info.get("install_if"):
        L.append("i:" + " ".join(info["install_if"]))
    # file records grouped per directory, like apk's db (a package need not ship
    # explicit directory entries for every directory it puts files in)
    groups = {}
    for m, content in members:
        if m.isdir():
            g = groups.setdefault(m.name.rstrip("/"), {"meta": None, "files": []})
            g["meta"] = (m.uid, m.gid, m.mode & 0o7777)
            continue
        d, _, n = m.name.rpartition("/")
        f = ["R:" + n]
        if m.mode & 0o7777 != 0o644 or m.uid or m.gid:
            f.append("a:%d:%d:%o" % (m.uid, m.gid, m.mode & 0o7777))
        if m.issym():
            f.append("Z:" + _b64sha1(m.linkname.encode()))
        else:
            sha = m.pax_headers.get("APK-TOOLS.checksum.SHA1")
            f.append("Z:Q1" + base64.b64encode(bytes.fromhex(sha)).decode() if sha
                     else "Z:" + _b64sha1(content))
        groups.setdefault(d, {"meta": None, "files": []})["files"].extend(f)
    for d, g in groups.items():
        L.append("F:" + d)
        if g["meta"] and g["meta"] != (0, 0, 0o755):      # apk omits the default dir mode
            L.append("M:%d:%d:%o" % g["meta"])
        L.extend(g["files"])
    return ("\n".join(L) + "\n\n").encode()


def build_installed(files, dir_meta, provides, arch, build_time, depends=(), musl_owned=False,
                    owned_elsewhere=frozenset()):
    """files: iterable of absolute image paths (regular files and hard links).
    dir_meta: {absolute dir path: (uid, gid, mode)}.
    owned_elsewhere: absolute paths owned by another package record (vanilla
    Alpine packages); leandros-base lists none of them. musl_owned is the older
    spelling for the musl subset and goes away once musl is a lock entry.
    Returns the bytes of /lib/apk/db/installed."""
    ver = time.strftime("%Y.%m.%d", time.gmtime(build_time)) + "-r0"
    chk = base64.b64encode(hashlib.sha1(("leandros-base-" + ver).encode()).digest()).decode()
    by_dir = {}
    for p in files:
        rel = p.lstrip("/")
        if excluded(rel) or (musl_owned and musl_excluded(rel)) or ("/" + rel) in owned_elsewhere:
            continue
        d, _, n = rel.rpartition("/")
        if not d:
            continue                       # apk needs an F: line; root-level files stay unowned
        by_dir.setdefault(d, []).append(n)
    L = ["C:Q1" + chk, "P:" + PKG, "V:" + ver, "A:" + arch, "S:0", "I:0",
         "T:LeandrOS base system (owned by the image, not apk)",
         "U:https://leandros.invalid", "L:custom", "o:" + PKG, "t:%d" % build_time,
         "p:" + " ".join(provides)]
    if depends:
        L.append("D:" + " ".join(depends))
    for d in sorted(by_dir):
        L.append("F:" + d)
        uid, gid, mode = dir_meta.get("/" + d, (0, 0, 0o755))
        L.append("M:%d:%d:%o" % (uid, gid, mode & 0o7777))
        for n in sorted(by_dir[d]):
            L.append("R:" + n)
    return ("\n".join(L) + "\n\n").encode()


# ---- vanilla packages: scripts, triggers, dependency satisfaction ------------------------
def control_files(apk_path):
    """({name: bytes} of the control tar's maintainer scripts, [trigger globs])."""
    segs = _apk_segments(open(apk_path, "rb").read())
    t = tarfile.open(fileobj=io.BytesIO(segs[1][1]))
    scripts = {m.name[1:]: t.extractfile(m).read() for m in t
               if m.name in (".pre-install", ".post-install", ".pre-upgrade", ".post-upgrade",
                             ".pre-deinstall", ".post-deinstall", ".trigger")}
    globs = []
    for line in t.extractfile(".PKGINFO").read().decode().splitlines():
        if line.startswith("triggers = "):
            globs += line[len("triggers = "):].split()
    return scripts, globs


def _x1(chk):
    """apk-tools 3 names script members by the hex sha1 (X1...), the db keeps Q1<base64>."""
    return "X1" + base64.b64decode(chk[2:]).hex()


def scripts_tar(pkgs, x1=False):
    """/lib/apk/db/scripts.tar. pkgs: (name, ver, chk 'Q1...', {script: bytes});
    members are `<name>-<ver>.<chk>.<script>` (apk 2 format), mode 0755."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for name, ver, chk, scripts in sorted(pkgs, key=lambda x: x[0]):
            for sc in sorted(scripts):
                ti = tarfile.TarInfo("%s-%s.%s.%s" % (name, ver, _x1(chk) if x1 else chk, sc))
                ti.size = len(scripts[sc])
                ti.mode = 0o755
                t.addfile(ti, io.BytesIO(scripts[sc]))
    return buf.getvalue()


def triggers_lines(pkgs):
    """/lib/apk/db/triggers. pkgs: (chk 'Q1...', [globs]) -> `Q1.. glob glob` lines."""
    return "".join("%s %s\n" % (chk, " ".join(globs)) for chk, globs in pkgs if globs).encode()


def parse_db(data):
    """installed-db bytes -> list of {'P','V','D':[..],'p':[..],'F':{dir:[files]},...}."""
    out = []
    for st in data.decode().split("\n\n"):
        if not st.strip():
            continue
        d = {"D": [], "p": [], "F": {}, "M": {}}
        cur = None
        for line in st.split("\n"):
            k, v = line[:1], line[2:]
            if k == "F":
                cur = v
                d["F"].setdefault(cur, [])
            elif k == "R" and cur is not None:
                d["F"][cur].append(v)
            elif k == "M" and cur is not None:
                d["M"][cur] = v
            elif k == "D":
                d["D"] += v.split()
            elif k == "p":
                d["p"] += v.split()
            elif k in ("P", "V", "C", "A", "I", "S"):
                d[k] = v
        out.append(d)
    return out


def _lock_mod():
    global _LOCK
    if "_LOCK" not in globals():
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "alpine_lock", os.path.join(os.path.dirname(os.path.abspath(__file__)), "alpine-lock.py"))
        _LOCK = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(_LOCK)
    return _LOCK


def check_deps(stanzas, base_provides):
    """Dependency satisfaction over parsed stanzas plus leandros-base's provides
    (list of 'name[=ver]'). Returns [(package, dependency)] nothing satisfies."""
    lk = _lock_mod()
    prov = {}
    for s in stanzas:
        prov.setdefault(s["P"], []).append(s.get("V"))
        for p in s["p"]:
            n, _, v = p.partition("=")
            prov.setdefault(n, []).append(v or None)
    for p in base_provides:
        n, _, v = p.partition("=")
        prov.setdefault(n, []).append(v or None)
    bad = []
    for s in stanzas:
        for dep in s["D"]:
            neg, name, op, ver = lk.parse_dep(dep)
            if neg:
                continue
            if not any(lk.satisfies(v, op, ver) for v in prov.get(name, [])):
                bad.append((s["P"], dep))
    return bad
