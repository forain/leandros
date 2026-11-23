#!/usr/bin/env python3
"""Build-time Alpine -dev packages for the cross sysroot (stdlib only, no container).

  alpine-dev.py lock  <arch>              resolve ports/alpine/devworld -> ports/alpine/<arch>.devlock
  alpine-dev.py stage <arch> <dest>       extract the -dev packages + their locked runtime packages into <dest>
  alpine-dev.py merge <arch> <sysroot>    stage and replace those files in the shared sysroot (flocked, atomic)

The runtime half is the very package ports/alpine/<arch>.lock ships in the image, so the
libraries consumers link against are byte-identical to the image's. The -dev half is pinned
(sha256) in <arch>.devlock, which must name the same release and the same version as the lock.
"""
import fcntl
import importlib.util
import os
import re
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("alpine_lock", os.path.join(HERE, "alpine-lock.py"))
lk = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(lk)

SKIP_TOP = ("usr/share/man", "usr/share/doc", "usr/share/aclocal", "usr/share/bash-completion", "usr/share/info")


def devlock_path(arch):
    return os.path.join(lk.ADIR, arch + ".devlock")


def bases(p):
    return [d.split("=")[0] for d in p["deps"] if re.match(r"^[a-z0-9][\w.+-]*=", d) and not d.startswith(("so:", "pc:", "cmd:"))]


def cmd_lock(arch):
    lock, _, _ = lk.read_lock(arch)
    idx = lk.load_index(arch, True)
    out = {}
    for n in lk.read_list(os.path.join(lk.ADIR, "devworld")):
        q = idx.get(n) or lk.die("devworld package %s is not in the index" % n)
        for b in bases(q):
            if b in lock and lock[b]["ver"] != q["ver"]:
                lk.die("%s %s is out of step with locked %s %s; refresh the lock" % (n, q["ver"], b, lock[b]["ver"]))
        e = {"name": n, "ver": q["ver"], "repo": q["repo"], "C": q["C"], "sha256": "", "scripts": []}
        f = lk.apk_path(arch, e)
        if not os.path.exists(f):
            lk.download("%s/%s/%s/%s/%s" % (lk.MIRROR, lk.REL, q["repo"], arch, lk.apk_name(e)), f)
        e["sha256"] = lk.sha256_file(f)
        c, e["scripts"] = lk.control_info(f)
        if c != q["C"]:
            lk.die("%s: control checksum %s != index %s" % (n, c, q["C"]))
        if e["scripts"]:
            lk.die("%s has maintainer scripts" % n)
        out[n] = e
    with open(devlock_path(arch), "w") as f:
        f.write("# release %s\n# build-time -dev packages for the cross sysroot; scripts/alpine-dev.py lock\n" % lk.REL)
        for n in sorted(out):
            p = out[n]
            f.write("pkg %s %s %s %s %s -\n" % (n, p["ver"], p["repo"], p["C"], p["sha256"]))
    print("%s: %d -dev packages" % (arch, len(out)))


def read_devlock(arch):
    lock, _, _ = lk.read_lock(arch)
    if lk.lock_release(arch) not in open(devlock_path(arch)).readline():
        sys.exit("alpine-dev: %s is for another release than the lock; run alpine-dev.py lock %s" % (devlock_path(arch), arch))
    dev = {}
    for line in open(devlock_path(arch)):
        t = line.split()
        if t and t[0] == "pkg":
            dev[t[1]] = {"name": t[1], "ver": t[2], "repo": t[3], "C": t[4], "sha256": t[5], "scripts": []}
    return lock, dev


def trees(arch):
    """[(pkg, extracted dir)] : the -dev packages and the locked runtime packages they pair with."""
    lock, dev = read_devlock(arch)
    idx = lk.load_index(arch, True)
    want = {}
    for n, p in dev.items():
        f = lk.apk_path(arch, p)
        if not os.path.exists(f):
            lk.download("%s/%s/%s/%s/%s" % (lk.MIRROR, lk.REL, p["repo"], arch, lk.apk_name(p)), f)
        if lk.sha256_file(f) != p["sha256"]:
            sys.exit("alpine-dev: %s sha256 differs from %s" % (lk.apk_name(p), devlock_path(arch)))
        want[n] = p
        for b in bases(idx[n]):
            if b in lock:
                want[b] = lock[b]
    for n, p in lock.items():  # fetch the runtime halves the same way the image does
        if n in want:
            f = lk.apk_path(arch, p)
            if not os.path.exists(f):
                lk.download("%s/%s/%s/%s/%s" % (lk.MIRROR, lk.REL, p["repo"], arch, lk.apk_name(p)), f)
    return [(n, lk.extract_one(arch, want[n])) for n in sorted(want)]


def cmd_stage(arch, dest):
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest)
    for n, d in trees(arch):
        for r, ds, fs in os.walk(d):
            rel = os.path.relpath(r, d)
            if rel != "." and any(rel == s or rel.startswith(s + "/") for s in SKIP_TOP):
                ds[:] = []
                continue
            os.makedirs(os.path.join(dest, rel), exist_ok=True)
            for fn in fs + [x for x in ds if os.path.islink(os.path.join(r, x))]:
                if fn == ".done":
                    continue
                src, dst = os.path.join(r, fn), os.path.join(dest, rel, fn)
                if os.path.lexists(dst):
                    os.unlink(dst)
                if os.path.islink(src):
                    os.symlink(os.readlink(src), dst)
                else:
                    shutil.copy2(src, dst)
                    os.chmod(dst, 0o644 if not os.access(src, os.X_OK) else 0o755)
    for r, ds, fs in os.walk(dest):  # a -dev symlink whose runtime half is not wanted (libxkbcommon-x11.so)
        for fn in fs + ds:
            p = os.path.join(r, fn)
            if os.path.islink(p) and not os.path.exists(p):
                os.unlink(p)
    print("staged %s into %s" % (arch, dest))


def cmd_merge(arch, sysroot):
    """Replace the staged files in the shared sysroot, and drop older versioned libraries of the same
    sonames (the from-source builds this replaces). Serialised on <sysroot>.lock; each file lands by rename."""
    if not os.path.isdir(sysroot):
        sys.exit("alpine-dev: no sysroot %s" % sysroot)
    import hashlib
    want = hashlib.sha256(open(devlock_path(arch), "rb").read() + open(lk.lock_path(arch), "rb").read()).hexdigest()
    stamp = os.path.join(sysroot, "usr", ".alpine-dev.stamp")
    try:
        if open(stamp).read().strip() == want and not os.environ.get("LEANDROS_ALPINE_DEV_REBUILD"):
            print("alpine-dev %s: sysroot up to date" % arch)
            return
    except OSError:
        pass
    tmp = tempfile.mkdtemp(prefix="alpine-dev-", dir=os.path.dirname(sysroot))
    try:
        cmd_stage(arch, tmp)
        with open(sysroot + ".lock", "a") as lf:
            fcntl.flock(lf, fcntl.LOCK_EX)
            n = 0
            for r, ds, fs in os.walk(tmp):
                rel = os.path.relpath(r, tmp)
                os.makedirs(os.path.join(sysroot, rel), exist_ok=True)
                names = fs + [x for x in ds if os.path.islink(os.path.join(r, x))]
                new = set(names)
                for fn in names:
                    src, dst = os.path.join(r, fn), os.path.join(sysroot, rel, fn)
                    t = os.path.join(sysroot, rel, ".merge.%d.%s" % (os.getpid(), fn))
                    if os.path.lexists(t):
                        os.unlink(t)
                    if os.path.islink(src):
                        os.symlink(os.readlink(src), t)
                    else:
                        shutil.copy2(src, t)
                    if os.path.isdir(dst) and not os.path.islink(dst):
                        shutil.rmtree(t, ignore_errors=True)
                        continue
                    os.replace(t, dst)
                    n += 1
                # stale versioned libraries of a replaced soname (libfoo.so.1.0.0 when libfoo.so.1.0.1 arrives)
                if rel.endswith("usr/lib"):
                    stems = {m.group(1) for f in names for m in [re.match(r"^(lib.+\.so)\.\d", f)] if m}
                    for f in os.listdir(os.path.join(sysroot, rel)):
                        m = re.match(r"^(lib.+\.so)\.\d", f)
                        if m and m.group(1) in stems and f not in new:
                            os.unlink(os.path.join(sysroot, rel, f))
            with open(stamp + ".tmp", "w") as sf:
                sf.write(want + "\n")
            os.replace(stamp + ".tmp", stamp)
            print("merged %d files into %s" % (n, sysroot))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main(a):
    if len(a) >= 3 and a[1] == "lock" and a[2] in lk.ARCHS:
        lk.read_lock(a[2])
        return cmd_lock(a[2])
    if len(a) >= 4 and a[1] in ("stage", "merge") and a[2] in lk.ARCHS:
        lk.read_lock(a[2])
        return (cmd_stage if a[1] == "stage" else cmd_merge)(a[2], a[3])
    sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
