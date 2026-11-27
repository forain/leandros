#!/usr/bin/env python3
"""Stage the locked vanilla Alpine packages (ports/alpine/<arch>.lock) host-side.

  alpinepkgs.py stage    <arch> <dir>    materialise the packages + apk db into <dir>
  alpinepkgs.py selftest [arch ...]      stage into a scratch dir and sanity-check

stage(arch) returns the image entries and the per-package installed-db stanzas
that mkfs-f2fs-populated.py packs. Nothing runs in a container; the extracted
trees come from `alpine-lock.py fetch` (the cache). Stdlib only.
"""
import importlib.util
import os
import shutil
import sys
import tarfile
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import apkdb  # noqa: E402

_spec = importlib.util.spec_from_file_location("alpine_lock", os.path.join(HERE, "alpine-lock.py"))
lk = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(lk)

ALLOWED_SCRIPTS = ("trigger", "pre-deinstall", "post-deinstall", "pre-upgrade", "post-upgrade")


def stage(arch):
    """-> (entries, stanzas, pkgs). entries: dicts {path, kind file|dir|symlink|hardlink,
    mode, uid, gid, host | linkname, size, pkg}; stanzas: [(name, bytes)] in name order;
    pkgs: [{name, ver, chk, scripts{name: bytes}, globs}] for scripts.tar / triggers."""
    lock, _, _ = lk.read_lock(arch)
    entries, stanzas, pkgs = [], [], []
    owner = {}
    for name in sorted(lock):
        p = lock[name]
        f = lk.apk_path(arch, p)
        if not os.path.exists(f):
            sys.exit("alpinepkgs: %s not in the cache; run alpine-lock.py fetch %s" % (lk.apk_name(p), arch))
        tree = lk.extract_one(arch, p)
        _size, _gz, _info, members = apkdb.read_apk(f, content=False)
        for m, _ in members:
            path = "/" + m.name.rstrip("/")
            # linux-pam (via shadow-libs): unix_chkpwd is root:shadow (gid 42 on 3.x, 148 on edge) setgid; no PAM here, image owner stays root
            if m.uid == 0 and m.gid in (42, 148) and path == "/usr/sbin/unix_chkpwd":
                pass
            elif m.uid or m.gid:
                sys.exit("alpinepkgs: %s: %s owned by %d:%d (only root expected)" % (name, path, m.uid, m.gid))
            e = {"path": path, "mode": m.mode & 0o7777, "uid": 0, "gid": 0, "pkg": name, "size": 0}
            if path == "/usr/sbin/unix_chkpwd":
                # Alpine ships 2755 root:shadow and a 0640 root:shadow /etc/shadow, so the
                # helper reads the file through its group. Our /etc/shadow is 0600 root:root
                # and the image has no `shadow` group, so setgid would be setgid gid 0 and
                # read nothing: a non-root caller (lock screen, su -l from a user) could never
                # verify its own password. Use the RHEL/Fedora arrangement that fits root-only
                # shadow: setuid root (4755), no group dependence. unix_chkpwd itself only lets
                # a non-root caller check its own account.
                e["mode"] = 0o4755
            if m.isdir():
                e["kind"] = "dir"
            else:
                if path in owner:
                    sys.exit("alpinepkgs: %s owned by both %s and %s" % (path, owner[path], name))
                owner[path] = name
                if m.issym():
                    e.update(kind="symlink", linkname=m.linkname)
                elif m.islnk():
                    e.update(kind="hardlink", linkname="/" + m.linkname)
                else:
                    e.update(kind="file", host=os.path.join(tree, m.name), size=m.size)
            entries.append(e)
        stanzas.append((name, apkdb.apk_stanza(f)))
        scripts, globs = apkdb.control_files(f)
        pkgs.append({"name": name, "ver": p["ver"], "chk": p["C"], "scripts": scripts, "globs": globs})
    for e in entries:
        if e["kind"] == "dir" and e["path"] in owner:
            sys.exit("alpinepkgs: %s is both a directory and a file" % e["path"])
    return entries, stanzas, pkgs


def generated_entries(arch):
    """Entries of the lock's generated tar (trigger outputs: gschemas, mime db, icon caches,
    fonts.dir, the Firefox icon subset), owned by leandros-base. Busybox applet symlinks
    that the scaffolding's busybox post-install left behind are skipped."""
    _, _, gen = lk.read_lock(arch)
    if not gen:
        sys.exit("alpinepkgs: %s.lock has no generated tar; run alpine-lock.py refresh %s --generated" % (arch, arch))
    f = os.path.join(lk.adir(arch), "generated-%s.tar" % gen)
    if not os.path.exists(f):
        sys.exit("alpinepkgs: %s missing (the generated tar is part of the cache)" % f)
    d = os.path.join(lk.adir(arch), "x", "gen-" + gen)
    if not os.path.exists(os.path.join(d, ".done")):
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        with tarfile.open(f) as t:
            t.extractall(d, filter="tar")
        open(os.path.join(d, ".done"), "w").close()
    out, dirs = [], set()
    with tarfile.open(f) as t:
        for m in t:
            path = "/" + m.name.rstrip("/").lstrip("./")
            if m.isdir():
                continue
            if m.issym() and (m.linkname.endswith("/busybox") or m.linkname == "busybox"):
                continue
            e = {"path": path, "mode": m.mode & 0o7777, "uid": 0, "gid": 0, "pkg": "leandros-base", "size": 0}
            if m.issym():
                e.update(kind="symlink", linkname=m.linkname)
            elif m.isreg():
                e.update(kind="file", host=os.path.join(d, m.name), size=m.size)
            else:
                sys.exit("alpinepkgs: generated tar: unexpected member %s" % m.name)
            out.append(e)
            dirs.add(os.path.dirname(path))
    return out, dirs


def musl_apk(arch):
    """Path of the locked musl .apk in the cache."""
    lock, _, _ = lk.read_lock(arch)
    return lk.apk_path(arch, lock["musl"])


def world_names(arch):
    lock, _, _ = lk.read_lock(arch)
    return [n for n in lk.world_for(arch) if n in lock]


def materialize(entries, dest):
    for e in sorted(entries, key=lambda e: (e["kind"] != "dir", e["path"])):
        p = os.path.join(dest, e["path"].lstrip("/"))
        if e["kind"] == "dir":
            os.makedirs(p, exist_ok=True)
            continue
        os.makedirs(os.path.dirname(p), exist_ok=True)
        if e["kind"] == "symlink":
            os.symlink(e["linkname"], p)
        elif e["kind"] == "hardlink":
            os.link(os.path.join(dest, e["linkname"].lstrip("/")), p)
        else:
            try:
                os.link(e["host"], p)
            except OSError:
                shutil.copy2(e["host"], p)


def write_db(pkgs, stanzas, dest):
    d = os.path.join(dest, "lib/apk/db")
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, "installed"), "wb").write(b"".join(s for _, s in stanzas))
    open(os.path.join(d, "triggers"), "wb").write(
        apkdb.triggers_lines([(p["chk"], p["globs"]) for p in pkgs]))
    open(os.path.join(d, "scripts.tar.gz"), "wb").write(lk.apkdb_gzip(
        apkdb.scripts_tar([(p["name"], p["ver"], p["chk"], p["scripts"]) for p in pkgs if p["scripts"]], x1=True)))


def db_files(pkgs, stanzas):
    """{name: bytes} of the vanilla part of /lib/apk/db (installed is appended to by mkfs)."""
    return {
        "installed": b"".join(s for _, s in stanzas),
        "triggers": apkdb.triggers_lines([(p["chk"], p["globs"]) for p in pkgs]),
        "scripts.tar.gz": lk.apkdb_gzip(apkdb.scripts_tar(
            [(p["name"], p["ver"], p["chk"], p["scripts"]) for p in pkgs if p["scripts"]], x1=True)),
    }


def base_provides():
    """Stand-in for leandros-base's provides (step 1: the held list only)."""
    out = ["/bin/sh=1", "cmd:sh=1"]
    for arch in lk.ARCHS:
        out += [h for h in lk._held_provides(arch) if h not in out]
    # so: deps held because every provider is held (mesa-egl -> so:libEGL.so.1) are recorded in the lock
    for arch in lk.ARCHS:
        try:
            for l in open(lk.lock_path(arch)):
                f = l.split()
                if len(f) >= 2 and f[0] == "held" and f[1].startswith("so:"):
                    out.append(f[1] + "=0")
        except OSError:
            pass
    return out


# ---- container cross-check ----------------------------------------------------------------
def canon(st):
    head, blocks, cur = [], [], None
    for line in st.strip("\n").split("\n"):
        if line[:1] == "F":
            cur = [line]
            blocks.append(cur)
        elif cur is not None and line[:1] in "MRaZ":
            if line[:1] == "R":
                cur.append([line])
            elif line[:1] == "M":
                cur.append(line)
            else:
                cur[-1].append(line)
        elif line[:2] == "f:":
            continue        # container artefact: apk flags a package "s" when its post-install failed in the scaffold
        else:
            head.append(line)
    blocks = [(b[0], [x for x in b[1:] if isinstance(x, str)], sorted(map(tuple, (x for x in b[1:] if isinstance(x, list)))))
              for b in blocks]
    return sorted(head), sorted(blocks)


def _q1(name):
    """The container's apk-tools 3 names scripts.tar members <pkg>-<ver>.X1<hex sha1>.<script>;
    the target apk-tools 2.14 (ports/apk) uses Q1<base64 sha1> of the same control hash. Normalise."""
    import base64, re
    return re.sub(r"\.X1([0-9a-f]{40})\.", lambda m: ".Q1" + base64.b64encode(bytes.fromhex(m.group(1))).decode() + ".", name)


def crosscheck(arch, dbdir):
    lock, _, _ = lk.read_lock(arch)
    _, stanzas, pkgs = stage(arch)
    ours = {n: s.decode() for n, s in stanzas}
    theirs = {}
    for st in open(os.path.join(dbdir, "installed")).read().split("\n\n"):
        for line in st.split("\n"):
            if line.startswith("P:"):
                theirs[line[2:]] = st
    bad = 0
    for n in sorted(lock):
        if n not in theirs:
            print("crosscheck: %s missing from the container db" % n)
            bad += 1
        elif canon(ours[n]) != canon(theirs[n]):
            a, b = canon(ours[n]), canon(theirs[n])
            print("crosscheck: %s differs: head ours-only %s theirs-only %s" % (
                n, [x for x in a[0] if x not in b[0]][:3], [x for x in b[0] if x not in a[0]][:3]))
            if a[1] != b[1]:
                print("    file blocks differ (%d vs %d dirs)" % (len(a[1]), len(b[1])))
                for x, y in zip(a[1], b[1]):
                    if x != y:
                        print("      ours   %s" % (x,))
                        print("      theirs %s" % (y,))
                        break
            bad += 1
    # scripts.tar names and triggers
    import tarfile
    want = {"%s-%s.%s.%s" % (p["name"], p["ver"], p["chk"], s) for p in pkgs for s in p["scripts"]}
    try:
        st = os.path.join(dbdir, "scripts.tar")
        st = st if os.path.exists(st) else st + ".gz"       # apk-tools 3.x names it scripts.tar.gz
        have = {_q1(m.name) for m in tarfile.open(st)}
    except (OSError, tarfile.TarError):
        have = set()
    have_l = {h for h in have if any(h.startswith("%s-%s." % (p["name"], p["ver"])) for p in pkgs)}
    if want != have_l:
        print("crosscheck: scripts.tar has %d entries (%d for lock pkgs), e.g. %s" % (len(have), len(have_l), sorted(have)[:3]))
        print("crosscheck: scripts.tar differs: ours-only %s theirs-only %s" % (
            sorted(want - have_l)[:3], sorted(have_l - want)[:3]))
        bad += 1
    wt = set(apkdb.triggers_lines([(p["chk"], p["globs"]) for p in pkgs]).decode().splitlines())
    try:
        ht = {l for l in open(os.path.join(dbdir, "triggers")).read().splitlines()
              if l.split()[0] in {p["chk"] for p in pkgs}}
    except OSError:
        ht = set()
    if wt != ht:
        print("crosscheck: triggers differ: ours-only %s theirs-only %s" % (sorted(wt - ht)[:2], sorted(ht - wt)[:2]))
        bad += 1
    print("crosscheck %s: %d packages compared, %d differ" % (arch, len(lock), bad))
    return bad


def script_allow():
    return {tuple(l.split()[:2]) for l in lk.read_list(os.path.join(lk.ADIR, "scriptallow"))}


# ---- self-test ----------------------------------------------------------------------------
def selftest(archs):
    rc = 0
    scratch = tempfile.mkdtemp(prefix="alpinepkgs-")
    try:
        for arch in archs:
            lk.cmd_fetch(arch)
            entries, stanzas, pkgs = stage(arch)
            dest = os.path.join(scratch, arch)
            materialize(entries, dest)
            write_db(pkgs, stanzas, dest)
            parsed = apkdb.parse_db(b"".join(s for _, s in stanzas))
            bad = apkdb.check_deps(parsed, base_provides())
            mib = sum(e["size"] for e in entries) / 1048576.0
            bad_scripts = [(p["name"], s) for p in pkgs for s in p["scripts"] if s not in ALLOWED_SCRIPTS and (p["name"], s) not in script_allow()]
            trig = sorted((p["name"], p["globs"]) for p in pkgs if p["globs"])
            print("%s: %d packages, %.1f MiB installed, %d entries" % (arch, len(pkgs), mib, len(entries)))
            print("  install scripts (must be empty): %s" % bad_scripts)
            print("  unsatisfied deps (must be empty): %s" % bad)
            print("  triggers: %s" % ", ".join("%s%s" % (n, g) for n, g in trig))
            if bad or bad_scripts:
                rc = 1
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return rc


def main(argv):
    if len(argv) >= 2 and argv[1] == "selftest":
        sys.exit(selftest(argv[2:] or list(lk.ARCHS)))
    if len(argv) == 4 and argv[1] == "stage":
        entries, stanzas, pkgs = stage(argv[2])
        materialize(entries, argv[3])
        write_db(pkgs, stanzas, argv[3])
        return
    sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
