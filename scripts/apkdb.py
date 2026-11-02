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
import struct
import time

PKG = "leandros-base"

# Paths apk must be free to create, replace or rewrite (never owned by the base).
EXCLUDE_FILES = {"etc/apk/world", "etc/apk/arch", "etc/passwd", "etc/group",
                 "etc/shadow", "etc/resolv.conf"}
EXCLUDE_PREFIXES = ("lib/apk/", "home/", "root/", "tmp/", "var/", "run/", "proc/",
                    "sys/", "dev/", "mnt/", "media/")

STATIC_PROVIDES = [
    "musl=1.2.5-r99", "/bin/sh=1", "cmd:sh=1",
    "alpine-baselayout=3.6.8-r99", "alpine-baselayout-data=3.6.8-r99",
    "alpine-keys=2.5-r0", "apk-tools-static=2.14.6-r3",
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


def build_installed(files, dir_meta, provides, arch, build_time):
    """files: iterable of absolute image paths (regular files and hard links).
    dir_meta: {absolute dir path: (uid, gid, mode)}.
    Returns the bytes of /lib/apk/db/installed."""
    ver = time.strftime("%Y.%m.%d", time.gmtime(build_time)) + "-r0"
    chk = base64.b64encode(hashlib.sha1(("leandros-base-" + ver).encode()).digest()).decode()
    by_dir = {}
    for p in files:
        rel = p.lstrip("/")
        if excluded(rel):
            continue
        d, _, n = rel.rpartition("/")
        if not d:
            continue                       # apk needs an F: line; root-level files stay unowned
        by_dir.setdefault(d, []).append(n)
    L = ["C:Q1" + chk, "P:" + PKG, "V:" + ver, "A:" + arch, "S:0", "I:0",
         "T:LeandrOS base system (owned by the image, not apk)",
         "U:https://leandros.invalid", "L:custom", "o:" + PKG, "t:%d" % build_time,
         "p:" + " ".join(provides)]
    for d in sorted(by_dir):
        L.append("F:" + d)
        uid, gid, mode = dir_meta.get("/" + d, (0, 0, 0o755))
        L.append("M:%d:%d:%o" % (uid, gid, mode & 0o7777))
        for n in sorted(by_dir[d]):
            L.append("R:" + n)
    return ("\n".join(L) + "\n\n").encode()
