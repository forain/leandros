#!/usr/bin/env python3
"""Create a pre-populated F2FS image for LeandrOS with all userland binaries.

Matches the on-disk layout expected by servers/f2fs/src/lib.rs.
"""
import hashlib
import re
import struct
import sys
import os
import subprocess
import soundfont

def _image_missing(what):
    """A required payload is absent: fail the build, do not ship a broken image.
    LEANDROS_ALLOW_PARTIAL_IMAGE=1 downgrades to a warning (kernel-only work)."""
    if os.environ.get("LEANDROS_ALLOW_PARTIAL_IMAGE") == "1":
        print(f"WARNING (LEANDROS_ALLOW_PARTIAL_IMAGE=1): image lacks {what}", file=sys.stderr)
        return
    raise SystemExit(f"ERROR: required image payload missing: {what}\n"
                     "  (set LEANDROS_ALLOW_PARTIAL_IMAGE=1 to build a partial image anyway)")


def sibling_dir(name):
    """The shared sibling checkout ../NAME (same fallback as build-all.sh):
    a worktree elsewhere has no sibling, so use the main checkout's."""
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    d = os.path.join(here, "..", name)
    if not os.path.isdir(d):
        try:
            common = subprocess.check_output(
                ["git", "-C", here, "rev-parse", "--path-format=absolute", "--git-common-dir"],
                stderr=subprocess.DEVNULL, text=True).strip()
            c = os.path.join(os.path.dirname(os.path.dirname(common)), name)
            if os.path.isdir(c):
                d = c
        except Exception:
            pass
    return d


# The image uses Linux's on-disk node layout ("lxnode2", scripts/f2fslx.py),
# written by scripts/f2fslxpop.py; this script gathers the tree.

BLOCK_SIZE     = 4096
BLOCKS_PER_SEG = 512       # 2 MB segments (log_blocks_per_seg = 9)
ROOT_LABEL     = "leandros-root"
# Linux include/linux/f2fs_fs.h: CP_UMOUNT_FLAG is 0x1 (0x8 is CP_ERROR_FLAG).
# The kernel's f2fs reports a newest checkpoint without it as an unclean shutdown.
CP_UMOUNT_FLAG = 0x00000001

SEG_CNT_CKPT = 2
# Linux mirrors the SIT and the NAT: two copies each, the live one per block
# chosen by the checkpoint's version bitmaps (lane f2fslayout).
SEG_CNT_SIT  = 2
SEG_CNT_NAT  = 2
SEG_CNT_SSA  = 1


META_SEGS  = 1 + SEG_CNT_CKPT + SEG_CNT_SIT + SEG_CNT_NAT + SEG_CNT_SSA


NODE_FOOTER_OFF = 4076


# Symlink. The f2fs server stores the Linux d_type byte in the dentry rather
# than the F2FS_FT_* enum (servers/f2fs/src/lib.rs:169-174), and its path walker
# follows a component iff that byte is DT_LNK (resolve_path_ex, :1766). A
# symlink inode is otherwise an ordinary file inode whose mode is S_IFLNK|0777
# and whose data is the target path — exactly what handle_symlink writes
# (:2333-2341) — so add_files_to_dir needs no new machinery beyond emitting the
# right ftype for a link mode.
DT_LNK = 10

S_IFLNK = 0o120000


# The Pi 5 V3D's uevent as Linux's v3d platform driver exposes it (libdrm
# reads OF_FULLNAME for the bus info and OF_COMPATIBLE_N/_<i> for the device
# info; both must parse or the device is skipped).
RPI5_V3D_UEVENT = (
    "DRIVER=v3d\n"
    "OF_NAME=v3d\n"
    "OF_FULLNAME=/axi/v3d@2000000\n"
    "OF_COMPATIBLE_0=brcm,2712-v3d\n"
    "OF_COMPATIBLE_N=1\n"
    "MODALIAS=of:Nv3dT(null)Cbrcm,2712-v3d\n"
)


def elf_has_gnu_build_id(path):
    """True if the ELF64 file carries an NT_GNU_BUILD_ID note in any PT_NOTE."""
    import struct as _st
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"\x7fELF" or data[4] != 2:
        return False
    e_phoff, = _st.unpack_from("<Q", data, 0x20)
    e_phentsize, e_phnum = _st.unpack_from("<HH", data, 0x36)
    for i in range(e_phnum):
        p_type, _flags, p_off, _va, _pa, p_filesz = _st.unpack_from("<IIQQQQ", data, e_phoff + i * e_phentsize)
        if p_type != 4:  # PT_NOTE
            continue
        off, end = p_off, p_off + p_filesz
        while off + 12 <= end:
            namesz, descsz, ntype = _st.unpack_from("<III", data, off)
            name = data[off + 12: off + 12 + namesz]
            if ntype == 3 and name.rstrip(b"\0") == b"GNU" and descsz > 0:
                return True
            off += 12 + ((namesz + 3) & ~3) + ((descsz + 3) & ~3)
    return False


def _greetd_src_hash():
    """sha256 over the pam-leandros sources (sorted by name, concatenated).
    ports/greetd/build.sh writes the same value to greetd.srchash."""
    import hashlib
    src_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                           "..", "ports", "greetd", "pam-leandros", "src")
    h = hashlib.sha256()
    for n in sorted(os.listdir(src_dir)):
        with open(os.path.join(src_dir, n), "rb") as f:
            h.update(f.read())
    return h.hexdigest()


def _greetd_stage_dir(arch):
    """Directory holding the greetd built from THIS checkout's pam-leandros
    sources: greetd-lane/<arch>/<srchash>/. Never the shared latest-build copy
    (another lane may have built different sources there). Missing -> run
    ports/greetd/build.sh for this arch (cached per hash). Returns None when
    the cache is absent and GREETD_NO_AUTOBUILD is set (image ships no greetd,
    as before when nothing was staged)."""
    import subprocess
    d = os.path.expanduser(
        f"~/code/leandros-artifacts/greetd-lane/{arch}/{_greetd_src_hash()}")
    if os.path.exists(os.path.join(d, "greetd")):
        return d
    if os.environ.get("GREETD_NO_AUTOBUILD"):
        return None
    build = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                         "..", "ports", "greetd", "build.sh")
    print(f"greetd for {arch} ({_greetd_src_hash()[:12]}) not staged; "
          f"running ports/greetd/build.sh", file=sys.stderr)
    r = subprocess.run(["bash", build], env=dict(os.environ, ARCHES=arch))
    if r.returncode != 0 or not os.path.exists(os.path.join(d, "greetd")):
        sys.exit(f"error: ports/greetd/build.sh failed for {arch}; "
                 f"expected {d}/greetd")
    return d


def _check_greetd_fresh(path):
    """Refuse a greetd whose embedded PAM shim predates sha512-crypt shadow.

    /etc/shadow holds `$6$rounds=` hashes; an old greetd only verifies
    `$sha256$` and every greeter login fails with pam_authenticate AUTH_ERR
    (L11.15). The shim's `rounds=` parser string is the marker. If the
    greetd.srchash sidecar (written by build.sh) exists it must match the
    current pam-leandros sources. File mtimes are NOT compared: a fresh git
    worktree gives every source a new mtime."""
    with open(path, "rb") as f:
        data = f.read()
    problems = []
    if b"rounds=" not in data:
        problems.append("lacks the sha512-crypt 'rounds=' marker")
    sidecar = path + ".srchash"
    if os.path.exists(sidecar):
        with open(sidecar) as f:
            want = f.read().strip()
        if want != _greetd_src_hash():
            problems.append("was built from different pam-leandros sources")
    else:
        print(f"warning: {sidecar} missing; checked the rounds= marker only",
              file=sys.stderr)
    if problems:
        sys.exit(f"error: stale greetd {path} " + " and ".join(problems)
                 + ": greeter login would fail with AUTH_ERR. "
                 "Run ports/greetd/build.sh and rebuild the image.")



# Inode timestamps. The f2fs server keeps `{u64 sec, u32 nsec}` for atime,
# mtime and ctime at these offsets (servers/f2fs INO_ATIME..INO_CTIME_NS; this
# volume format is not Linux's). Left zero, every staged file stat'ed as
# 1970-01-01 00:00. Files copied from the host carry their source's mtime;
# directories and generated content the build time (SOURCE_DATE_EPOCH when
# set, for a reproducible image).
INO_ATIME, INO_MTIME, INO_CTIME = 36, 48, 60
BUILD_TIME = int(os.environ.get("SOURCE_DATE_EPOCH") or __import__("time").time())







def coreutils_command_names(coreutils_dir):
    """The exact set of commands the coreutils multicall binary answers to.

    Derived from upstream's own feature graph rather than a listing of
    src/uu, so it stays in step with what build-all.sh actually compiled.
    Listing the directory instead would install names the binary cannot
    dispatch (chcon and runcon are SELinux-only and not in the musl set) plus
    checksum_common, which is a shared module rather than a command.

    Must mirror build_coreutils in build-all.sh: it builds
    --features feat_os_unix_musl, which is upstream's musl set and already
    excludes stdbuf (that util needs a cdylib, which a static musl target
    cannot produce).
    """
    import tomllib

    with open(os.path.join(coreutils_dir, "Cargo.toml"), "rb") as f:
        features = tomllib.load(f)["features"]

    visited = set()
    def expand(name):
        if name in visited:
            return
        visited.add(name)
        for entry in features.get(name, []):
            expand(entry.split("/")[0].rstrip("?"))

    expand("feat_os_unix_musl")
    # Intersect with src/uu so the feature-graph bookkeeping drops out and only
    # real command names survive: the feat_* group names, and dependency
    # aliases like uu_test (which backs the `test` feature — that one has to be
    # matched on the feature name, since its leaf is the alias, not `test`).
    uu_dir = os.path.join(coreutils_dir, "src", "uu")
    available = set(os.listdir(uu_dir))
    return sorted(visited & available)


_B64 = "./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
_SHA512_ORDER = [(0, 21, 42), (22, 43, 1), (44, 2, 23), (3, 24, 45), (25, 46, 4),
                 (47, 5, 26), (6, 27, 48), (28, 49, 7), (50, 8, 29), (9, 30, 51),
                 (31, 52, 10), (53, 11, 32), (12, 33, 54), (34, 55, 13), (56, 14, 35),
                 (15, 36, 57), (37, 58, 16), (59, 17, 38), (18, 39, 60), (40, 61, 19),
                 (62, 20, 41)]


def sha512_crypt(password, salt, rounds=5000):
    """glibc/Drepper sha512-crypt ($6$). Returns the full `$6$...` string
    (with `rounds=N$` unless rounds is the 5000 default)."""
    pw = password.encode('utf-8')
    sl = salt.encode('ascii')[:16]
    sha = hashlib.sha512
    rounds = max(1000, min(999999999, rounds))
    b = sha(pw + sl + pw).digest()
    ctx = pw + sl + (b * (len(pw) // 64 + 1))[:len(pw)]
    n = len(pw)
    while n > 0:
        ctx += b if n & 1 else pw
        n >>= 1
    a = sha(ctx).digest()
    dp = sha(pw * len(pw)).digest()
    p = (dp * (len(pw) // 64 + 1))[:len(pw)]
    ds = sha(sl * (16 + a[0])).digest()
    s = (ds * (len(sl) // 64 + 1))[:len(sl)]
    for i in range(rounds):
        c = (p if i & 1 else a)
        if i % 3:
            c += s
        if i % 7:
            c += p
        c += a if i & 1 else p
        a = sha(c).digest()
    out = ""
    for x, y, z in _SHA512_ORDER:
        w = (a[x] << 16) | (a[y] << 8) | a[z]
        for _ in range(4):
            out += _B64[w & 63]
            w >>= 6
    w = a[63]
    for _ in range(2):
        out += _B64[w & 63]
        w >>= 6
    prefix = f"$6$rounds={rounds}$" if rounds != 5000 else "$6$"
    return f"{prefix}{sl.decode('ascii')}${out}"


SHADOW_ROUNDS = 65536


def shadow_hash(salt, password):
    """sha512-crypt `$6$rounds=65536$<salt>$<hash>`, verified by
    lib/shadowhash/shadowhash.rs (login, sshd, greetd PAM). `salt` is a fixed
    per-user string (<=16 chars) so images stay deterministic."""
    return sha512_crypt(password, salt, SHADOW_ROUNDS)


def session_data(name):
    """Resolve one m6-session-data entry, artifacts tree first, repo second.

    These guest-side driver scripts live in ~/code/leandros-artifacts, which is
    NOT the repo and which a `git merge` therefore does not populate. Several of
    them are also committed under artifacts/m6-session-data/, and every caller
    gates on os.path.exists — so a script that arrived in a merge but not in the
    artifacts tree was skipped SILENTLY, and the only symptom was the guest
    answering `failed to source file: /bin/<name>` much later.

    The artifacts tree keeps precedence, because at least one entry (m4-vkwl)
    legitimately differs per machine and the local copy is the one that should
    win. The repo copy is a fallback for absence only, never an override.
    """
    tree = os.path.expanduser(f"~/code/leandros-artifacts/m6-session-data/{name}")
    if os.path.exists(tree):
        return tree
    repo = os.path.normpath(os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..", "artifacts", "m6-session-data", name))
    # Returning `tree` when neither exists keeps the callers' os.path.exists
    # gate meaningful and their error messages pointing at the intended source.
    return repo if os.path.exists(repo) else tree


def _find_cosmic_epoch():
    """Locate the cosmic-epoch checkout that the /usr/share/cosmic tree comes from.

    Order: $LEANDROS_COSMIC_EPOCH; a sibling of this checkout (`../cosmic-epoch`,
    which is where every other sibling repo lives); a sibling of the MAIN
    checkout when this is a git worktree placed elsewhere; ~/code/cosmic-epoch
    (the Mac layout). Returns the first candidate that has cosmic-comp/data,
    else the first candidate (so the caller's error names a real path).
    """
    here = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
    cands = []
    env = os.environ.get("LEANDROS_COSMIC_EPOCH")
    if env:
        cands.append(os.path.expanduser(env))
    cands.append(os.path.join(os.path.dirname(here), "cosmic-epoch"))
    try:
        common = subprocess.run(["git", "-C", here, "rev-parse", "--git-common-dir"],
                                capture_output=True, text=True, check=True).stdout.strip()
        main = os.path.dirname(os.path.normpath(os.path.join(here, common)))
        cands.append(os.path.join(os.path.dirname(main), "cosmic-epoch"))
    except Exception:
        pass
    cands.append(os.path.expanduser("~/code/cosmic-epoch"))
    for c in cands:
        if os.path.isdir(os.path.join(c, "cosmic-comp", "data")):
            return c
    return cands[0]


def verify_dbus_staging(arch):
    """Refuse to build an image whose staged D-Bus payload is older than its source.

    The m5-session-ship tree is hand-synced and gitignored, and mkfs packs
    whatever it finds there. That is how commit 84ec91a (busd .service
    activation) came to be committed, host-tested and absent from every image:
    the staged busd was byte-identical to the pre-activation build and the
    staged session.conf had no <servicedir>, while the repo had both. Nothing
    said so. A boot test asking "does the desktop come up" answered yes, twice.

    ports/busd/build.sh now writes all three staged files from tracked sources
    every run. This is the backstop for the case that script was not run:
    the two verbatim files are compared byte-for-byte against ports/, and busd
    -- which cannot be rebuilt from here (it needs cargo +nightly and the musl
    CRT) -- is compared by mtime against the patch set that defines it. Adding
    a patch and forgetting to rebuild is exactly the failure this catches.

    Loud, not silent: this raises SystemExit rather than warning, because a
    warning in a build that then prints a green image is the thing that failed.
    """
    ship = os.path.join(os.environ.get("LEANDROS_M5_SHIP") or os.path.expanduser("~/code/leandros-artifacts/m5-session-ship"), arch)
    if not os.path.isdir(ship):
        return                      # no D-Bus payload staged at all; nothing to check
    repo = os.path.normpath(os.path.join(
        os.path.dirname(os.path.abspath(__file__)), ".."))
    pkg = os.path.join(repo, "ports", "dbus", "session-pkg")

    problems = []
    for staged_rel, tracked in (
        ("usr/bin/dbus-run-session",          os.path.join(pkg, "dbus-run-session")),
        ("usr/share/dbus-1/session.conf",     os.path.join(pkg, "session.conf")),
    ):
        staged = os.path.join(ship, staged_rel)
        if not os.path.exists(staged):
            problems.append(f"  MISSING  {staged}")
            continue
        if not os.path.exists(tracked):
            continue                # tracked source gone; not this check's business
        with open(staged, "rb") as a, open(tracked, "rb") as b:
            if a.read() != b.read():
                problems.append(f"  STALE    {staged_rel}\n"
                                f"           differs from {tracked}")

    busd = os.path.join(ship, "usr", "libexec", "busd")
    patches = [os.path.join(repo, "ports", "busd", f)
               for f in os.listdir(os.path.join(repo, "ports", "busd"))
               if f.endswith(".patch")]
    if not os.path.exists(busd):
        problems.append(f"  MISSING  {busd}")
    elif patches:
        newest = max(patches, key=os.path.getmtime)
        if os.path.getmtime(newest) > os.path.getmtime(busd):
            problems.append(
                f"  STALE    usr/libexec/busd ({arch})\n"
                f"           {os.path.relpath(newest, repo)} is newer than the staged binary,\n"
                f"           so the staged busd was built without it.")

    # Every Exec= binary a packaged .service names must be staged too, or busd
    # activates nothing and every caller waits out ACTIVATION_TIMEOUT (5 s)
    # before getting ServiceUnknown -- slower than having no .service at all.
    svc_dir = os.path.join(pkg, "services")
    for fn in sorted(os.listdir(svc_dir)) if os.path.isdir(svc_dir) else []:
        if not fn.endswith(".service"):
            continue
        with open(os.path.join(svc_dir, fn)) as f:
            exec_line = next((l.split("=", 1)[1].split() for l in f
                              if l.startswith("Exec=")), [])
        if exec_line and exec_line[0].startswith("/usr/"):
            staged_bin = os.path.join(ship, exec_line[0].lstrip("/"))
            if not os.path.exists(staged_bin):
                problems.append(f"  MISSING  {exec_line[0]} (Exec= of services/{fn})\n"
                                f"           build it: ./ports/sysbus/build.sh {arch}")
            elif exec_line[0].endswith("/leandros-sysbus"):
                # Feature check, not just mtime: the staged binary lives in the
                # SHARED ~/code/leandros-artifacts, so a lane that rebuilds
                # sysbus from another tree overwrites it with a NEWER binary
                # that lacks a subcommand (bluez, power, ...) and the mtime
                # test below passes. The binary carries the subcommand list
                # "leandros-sysbus-services:a,b,...;" (ports/sysbus/src/main.rs
                # SERVICES); it must equal this tree's, and name this .service.
                _pat = rb"leandros-sysbus-services:([a-z0-9,-]*);"
                with open(staged_bin, "rb") as _bf:
                    _m = re.search(_pat, _bf.read())
                with open(os.path.join(repo, "ports", "sysbus", "src", "main.rs"), "rb") as _sf:
                    _t = re.search(_pat, _sf.read())
                _have = _m.group(1).decode() if _m else "no marker"
                _tree = _t.group(1).decode() if _t else ""
                _want = exec_line[1] if len(exec_line) > 1 else ""
                if _have != _tree or _want not in _tree.split(","):
                    problems.append(f"  STALE    {exec_line[0]} ({arch}) was built from another tree:\n"
                                    f"           staged serves [{_have}], this tree [{_tree}] "
                                    f"(services/{fn} needs '{_want}').\n"
                                    f"           rebuild from this tree: ./ports/sysbus/build.sh {arch}")
                # lib/netctl-abi too: the NetworkManager shim links it, and an
                # ABI change the staged binary predates is exactly the stale
                # build this check exists for.
                srcs = [os.path.join(dp, n)
                        for d in (("ports", "sysbus", "src"), ("lib", "netctl-abi", "src"))
                        for dp, _d, ns in os.walk(os.path.join(repo, *d))
                        for n in ns] + [os.path.join(repo, "ports", "sysbus", "Cargo.toml"),
                                        os.path.join(repo, "lib", "netctl-abi", "Cargo.toml")]
                newest = max(srcs, key=os.path.getmtime)
                if os.path.getmtime(newest) > os.path.getmtime(staged_bin):
                    problems.append(f"  STALE    {exec_line[0]} ({arch})\n"
                                    f"           {os.path.relpath(newest, repo)} is newer; "
                                    f"./ports/sysbus/build.sh {arch}")

    if problems:
        sys.stderr.write(
            "\nD-Bus session payload is stale — refusing to build a misleading image.\n"
            + "\n".join(problems)
            + f"\n\n  Fix:  ./ports/busd/build.sh {arch}\n\n")
        raise SystemExit(1)


def main():
    if len(sys.argv) < 3:
        print("Usage: mkfs-f2fs-populated.py <output_img> <arch>")
        sys.exit(1)
        
    out_file = sys.argv[1]
    arch = sys.argv[2]
    if os.path.exists(out_file + ".gpu"):
        os.remove(out_file + ".gpu")  # stale stamp from a previous build
    
    # 1. Gather files to place in F2FS
    target_arch = "aarch64-unknown-none" if arch == "aarch64" else "x86_64-unknown-none"
    userland_dir = f"userland/target/{target_arch}/release"
    
    bin_files = []
    bins = [
        "shell", "login", "utmpd", "greeter-launch", "hello", "aplay", "memtest", "vfstest", "permtest", "f2fstest", "tput",
        "pthreadtest", "timertest", "sigtest", "polltest", "fdmaxtest", "mqtest", "pipebw", "conloss", "ptytest", "forktest", "shmtest", "racetest", "mmapbench",
        "waittest", "sigchldtest", "sigtest2", "jobtest", "exectest", "scmtest", "epolltest", "wakepolltest", "smpwaketest", "idletest", "drmsmoke", "gpuqtest", "evtest2", "uinputtest", "evsplit", "vttest", "venustest", "i915test",
        "mount", "umount", "fstab", "lsblk", "lspci", "lsusb", "ping", "xattr",
        "meminfo", "dbusprobe", "uptrtest", "strtest", "loadkmap", "privtest", "memcpybench-musl", "nettest", "ip6test", "netlinktest", "nfttest", "udpselftest", "udptest", "pollnarrow", "listentag", "vfstag", "nettag2", "kparks", "netcheck", "ec2init", "fsbench", "pcachetest",
        "sndtest", "syscalltest", "uaccesstest", "camtest", "proctest", "proctest-cold.bin", "execdev", "chroottest", "nstest", "tuntest", "brtest", "cgtest", "ovltest", "ovlmknodtest", "idmaptest", "inotifytest", "captest", "mnttest", "seccomptest", "bpftest", "preadtest", "kexectest", "memfdtest", "conmontest", "exitracetest", "exitwrap", "kexec", "kmodctl", "kmodctltest", "suspendtest", "serrx", "netctl", "netctltest", "ps", "pstest", "btd", "btctl", "bttest", "bthid", "fusetest", "w", "users", "lastlog", "utmptest",
    ]
    for b in bins:
        p = os.path.join(userland_dir, b)
        if os.path.exists(p):
            bin_files.append((b, p, 0o100755))
    # poweroff/reboot/halt: one binary dispatching on argv[0]; the same host
    # path makes all three names hardlinks to one inode.
    p = os.path.join(userland_dir, "poweroff")
    if os.path.exists(p):
        for name in ("poweroff", "reboot", "halt", "systemctl"):
            bin_files.append((name, p, 0o100755))

    # exectest's `#!` fixtures. The modes are the test: exectest-noexec.sh must
    # have no execute bit (EACCES), everything else is 0755 so the kernel's
    # binfmt_script path accepts it; exectest-data.txt is executable but is
    # neither ELF nor `#!` (ENOEXEC).
    _exectest_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                 "..", "userland", "exectest", "scripts")
    for _s in ("exectest-echo.sh", "exectest-env.sh", "exectest-nested.sh",
               "exectest-trail.sh", "exectest-data.txt", "exectest-missing.sh",
               "exectest-loop.sh"):
        _p = os.path.join(_exectest_dir, _s)
        if os.path.exists(_p):
            bin_files.append((_s, _p, 0o100755))
    _p = os.path.join(_exectest_dir, "exectest-noexec.sh")
    if os.path.exists(_p):
        bin_files.append(("exectest-noexec.sh", _p, 0o100644))

    # Ad-hoc binaries staged from outside the tree: point LEANDROS_EXTRA_BIN at
    # a directory and every regular file in it lands in /bin. A name ending in
    # "-aarch64" or "-x86_64" is per-architecture and is staged, without the
    # suffix, only into that architecture's image — so one directory can hold
    # both builds of the same program and each image gets the right one.
    extra_dir = os.environ.get("LEANDROS_EXTRA_BIN")
    if extra_dir and os.path.isdir(extra_dir):
        for name in sorted(os.listdir(extra_dir)):
            p = os.path.join(extra_dir, name)
            if not os.path.isfile(p):
                continue
            for suffix in ("-aarch64", "-x86_64"):
                if name.endswith(suffix):
                    if suffix != f"-{arch}":
                        name = None
                    else:
                        name = name[: -len(suffix)]
                    break
            if name:
                bin_files.append((name, p, 0o100755))

    # Static-musl tokio binaries from the S1 spike — K2 acceptance
    # (tokio-echo-selftest regression + the idle-CPU cross-check).
    musl_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    tokio_dir = os.path.expanduser(
        f"~/code/leandros-artifacts/s1-musl-spike/target/{musl_target}/release")
    for tb in ("tokio-echo-selftest", "tokioidle"):
        p = os.path.join(tokio_dir, tb)
        if os.path.exists(p):
            bin_files.append((tb, p, 0o100755))
            
    # DOOM as linked by THIS worktree's build_doom (target/doom/), falling back
    # to the shared ../doomgeneric/doom-<arch> only for a tree that predates
    # it: that path holds whichever worktree linked last, which once put
    # another tree's DOOM into this tree's image.
    p = f"target/doom/doom-{arch}"
    if not os.path.exists(p):
        p = f"../doomgeneric/doom-{arch}"
    if os.path.exists(p):
        print(f"  doom: {p}")
        bin_files.append(("doom", p, 0o100755))
    # C float-ABI test linked against libleandros_libc.a like DOOM (lane floatabi).
    p = f"target/floattest/floattest-{arch}"
    if os.path.exists(p):
        bin_files.append(("floattest", p, 0o100755))
    # wl_shm frame-copy benchmark linked like DOOM (lane shmslow).
    p = f"target/memcpybench/memcpybench-{arch}"
    if os.path.exists(p):
        bin_files.append(("memcpybench", p, 0o100755))
    p = "../doomgeneric/doom1.wad"
    if os.path.exists(p):
        bin_files.append(("doom1.wad", p, 0o100644))
        
    bottom_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    p = f"../bottom-leandros/target/{bottom_target}/release/btm"
    if os.path.exists(p):
        bin_files.append(("btm", p, 0o100755))
        bin_files.append(("bottom", p, 0o100755))

    mame_dir = os.environ.get("LEANDROS_MAME_DIR", "../mame")
    p = f"{mame_dir}/mame-{arch}"
    if os.path.exists(p):
        bin_files.append(("mame", p, 0o100755))

    brush_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    p = os.path.join(os.environ.get("LEANDROS_BRUSH_DIR", "../brush"), "target", brush_target, "release", "brush")
    if os.path.exists(p):
        bin_files.append(("brush", p, 0o100755))

    # uutils/coreutils — cat, ls, cp, mv, rm and friends. One multicall binary
    # that dispatches on argv[0], so every name below is a hardlink to the same
    # inode (add_files_to_dir dedupes by host path); the content is stored once.
    coreutils_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    # Kept in its own name because /usr/bin/env is staged from it further down,
    # long after the loop below has reused `p`.
    coreutils_bin = f"../coreutils/target/{coreutils_target}/release/coreutils"
    p = coreutils_bin
    if os.path.exists(p):
        bin_files.append(("coreutils", p, 0o100755))
        for util in coreutils_command_names("../coreutils"):
            bin_files.append((util, p, 0o100755))

    # No shell / no coreutils = an image that boots to nothing usable. This used
    # to be a build-all.sh warning followed by exit 0 (TODO "no shell").
    _bn = {n for n, _p, _m in bin_files}
    if "brush" not in _bn:
        _image_missing(f"/bin/brush (no shell): expected {brush_target} build under "
                       f"{os.environ.get('LEANDROS_BRUSH_DIR', '../brush')}; needs the ../crossterm sibling too")
    if "coreutils" not in _bn:
        _image_missing(f"/bin/coreutils (expected {coreutils_bin})")
    if "btm" not in _bn:
        print(f"  WARNING: /bin/btm not staged (expected ../bottom-leandros/target/{bottom_target}/release/btm)")


    lib_files = []
    root_files = []
    p = "userland/aplay/car-horn.wav"
    if os.path.exists(p):
        root_files.append(("car-horn.wav", p, 0o100644))

    # MAME ROM sets as zips in / — run with `mame -rompath / <game>`.
    # A zip sidesteps this script's flat directory model (no nested
    # /roms/<game>/ dirs needed); MAME's util/unzip.cpp reads it directly.
    p = f"{os.environ.get('LEANDROS_MAME_DIR', '../mame')}/roms/captcomm.zip"
    if os.path.exists(p):
        root_files.append(("captcomm.zip", p, 0o100644))

    # Optional local OCI/docker image archive (tracing podman without registry access): /<basename>
    _img_arch = os.environ.get("LEANDROS_IMAGE_ARCHIVE")
    if _img_arch and os.path.isfile(_img_arch):
        root_files.append((os.path.basename(_img_arch), _img_arch, 0o100644))

    # /etc/fstab — /dev/root is the root F2FS volume userland/init already
    # mounts via a hardcoded bootstrap call before pivot_root (chicken-and-egg:
    # fstab itself lives on that filesystem); the kernel resolves it (root= on
    # the command line, else QEMU's vdb, else the one removable f2fs volume).
    # /home is the persistent home volume, found by its f2fs label wherever it
    # is: QEMU's vdc (f2fs-data1-<arch>.img, scripts/mkfs-f2fs-home.py) or a
    # USB stick's home partition (scripts/write-usb.sh). The kernel resolves
    # LABEL= (drivers/src/blkdev.rs resolve_source). `nofail`: without one
    # (the Pi SD card, an old stick) init warns and /home is this root's own
    # /home, which therefore keeps its seeded skeleton below.
    etc_files = [("fstab", (
        b"# <device>           <mountpoint>  <fstype>  <options>  <dump>  <pass>\n"
        b"/dev/root            /             f2fs      rw         0       1\n"
        b"LABEL=leandros-home  /home         f2fs      rw,nofail  0       2\n"
    ), 0o100644)]

    # /etc/passwd and /etc/group — musl's getpwuid/getgrgid (used by brush via
    # the uzers crate) read these; /bin/login also parses passwd directly.
    # cosmic-greeter is a real system account, not decoration. The greeter
    # binary has no flag and no environment variable that selects its role: it
    # runs greeter::main() only when getpwuid(getuid()) comes back named
    # "cosmic-greeter", and locker::main() for every other name
    # (cosmic-greeter/src/main.rs). So the account IS the role selector, and
    # /bin/greeter-launch exists to become it.
    #
    # The uid must be BELOW 1000. cosmic-greeter's own UserFilter defaults to
    # UID_MIN 1000 / UID_MAX 65000 when there is no /etc/login.defs — and there
    # is none here — and it offers every account inside that window as a login
    # choice. A greeter at uid >= 1000 would list itself on its own login
    # screen. 990 sits in the conventional system band (1..999), clear of 0 and
    # far enough below 1000 to leave room for hand-assigned low ids.
    #
    # The shell is /bin/false for the same reason twice over: UserFilter also
    # skips any account whose shell file name is "false" or "nologin", so the
    # account stays off the login list even if UID_MIN is ever raised. Nothing
    # executes it — greeter-launch execve()s the greeter directly.
    passwd_bytes = (
        b"root:x:0:0:root:/root:/bin/brush\n"
        b"cosmic-greeter:x:990:990:COSMIC Greeter:/home/cosmic-greeter:/bin/false\n"
        b"leandro:x:1000:1000:leandro:/home/leandro:/bin/brush\n"
        b"sshd:x:22:22:sshd privsep:/var/empty:/sbin/nologin\n"
        b"nobody:x:65534:65534:nobody:/:/bin/false\n"
    )
    etc_files.append(("passwd", passwd_bytes, 0o100644))
    # Subordinate id ranges for containers, read by newuidmap/newgidmap
    # (shadow-subids). 100000-165535 is RESERVED for leandro and
    # 200000-265535 for root: no passwd/group entry may use them
    # (design-idmap). Root-owned 0644 as shadow-utils requires.
    etc_files.append(("subuid", b"leandro:100000:65536\nroot:200000:65536\n", 0o100644))
    etc_files.append(("subgid", b"leandro:100000:65536\nroot:200000:65536\n", 0o100644))
    # Firefox's sandbox builds its library path policy from /etc/ld.so.conf;
    # list musl's default search dirs so policy and loader agree.
    etc_files.append(("ld.so.conf", b"/lib\n/usr/lib\n/usr/local/lib\n", 0o100644))
    # /etc/localtime is a symlink into the Alpine tzdata package's zoneinfo
    # (ports/alpine/world); timedate1 SetTimezone repoints it.
    etc_files.append(("localtime", b"/usr/share/zoneinfo/UTC", S_IFLNK | 0o777))
    # There is deliberately no /etc/passwd.greeter here any more. The bring-up
    # scaffold reached the greeter role by adding a SECOND, uid-0
    # "cosmic-greeter" entry ahead of root and copying that file over
    # /etc/passwd at boot, so getpwuid(0) answered with the greeter's name. It
    # worked, and its cost was that getpwuid(0) then answered "cosmic-greeter"
    # for every caller in the image and the change persisted across boots on a
    # writable root. /bin/greeter-launch replaces it by becoming the account for
    # real, so /etc/passwd is left alone.
    # Supplementary memberships are real: /bin/login (and musl's initgroups,
    # which greetd's session worker calls) read the member lists and
    # setgroups(2) them, and every filesystem permission check consults the
    # list. `video`/`input` carry the conventional Debian gids: the VFS makes
    # /dev/dri/card* and /dev/fb0 root:video 0660 and /dev/input/* root:input
    # 0660, so the two accounts that run a compositor (the greeter's and the
    # session user's) are members and nobody else is. `audio` (Debian's 29)
    # owns the playback device /dev/pipewire (116:16, root:audio 0660, single
    # writer -- servers/pipewire): the session user's PipeWire sink opens it.
    # The greeter plays no sound and is not a member. `netdev` (108) may manage
    # the network: /dev/netctl is root:netdev 0664 and only root and members
    # may open it for writing (docs/servers/netctl.md); the saved connections
    # in /etc/leandros/network are root:netdev too. The greeter only reads.
    # `bluetooth` (112, a Debian-style system group) may drive Bluetooth:
    # btd.sock and /run/bluetooth/bthid.connect are root:bluetooth 0660
    # (userland/btd, userland/bthid); the desktop user's org.bluez shim needs it.
    etc_files.append(("group", (
        b"root:x:0:\n"
        b"tty:x:5:\n"
        b"sshd:x:22:\n"
        b"wheel:x:10:leandro\n"
        b"audio:x:29:leandro\n"
        b"video:x:44:leandro,cosmic-greeter\n"
        b"input:x:104:leandro,cosmic-greeter\n"
        b"netdev:x:108:leandro\n"
        b"bluetooth:x:112:leandro\n"
        b"cosmic-greeter:x:990:\n"
        b"leandro:x:1000:\n"
        b"nobody:x:65534:\n"
        b"nogroup:x:65533:\n"
    ), 0o100644))

    # /etc/shadow — /bin/login's only source of password hashes. Salts are
    # fixed per user so this Python hasher and the Rust verifier in
    # userland/login/src/main.rs (verify_password) agree byte-for-byte.
    shadow_lines = (
        f"root:{shadow_hash('lnd0rootsalt0000', 'root')}:\n"
        f"leandro:{shadow_hash('lnd0leandrosalt1', 'leandro')}:\n"
        "sshd:!:\n"
    )
    etc_files.append(("shadow", shadow_lines.encode('ascii'), 0o100600))

    # sudo-rs (ports/sudo-rs): the policy and the login-shell list are data
    # files of the port, so a userland-only deploy can push the same bytes.
    # /etc/sudoers must be root-owned and neither group- nor world-writable or
    # sudo refuses to read it. `wheel` (10, above) is the admin group.
    etc_files.append(("sudoers", open("ports/sudo-rs/data/sudoers", "rb").read(), 0o100440))
    # /etc/shells: su -s only accepts a shell listed here for non-root callers.
    etc_files.append(("shells", open("ports/sudo-rs/data/shells", "rb").read(), 0o100644))
    # The files the VFS's static RamFS also carries (servers/vfs RAMFS). Once
    # this volume is the root, its /etc wins over those built-ins (they only
    # stand in before the root is mounted), so the same defaults must be here.
    # Keep the bytes in step with the RAMFS table.
    for _name, _data in (
        ("motd",          b"Welcome to Leandros!\nType 'help' for available commands.\n"),
        ("hostname",      b"leandros\n"),
        ("hosts",         b"127.0.0.1\tlocalhost\n::1\t\tlocalhost\n127.0.0.1\tleandros\n"),
        ("services",      b"http\t80/tcp\nhttps\t443/tcp\nssh\t22/tcp\nftp\t21/tcp\n"),
        ("protocols",     b"ip\t0\tIP\ntcp\t6\tTCP\nudp\t17\tUDP\nicmp\t1\tICMP\n"),
        ("nsswitch.conf", b"hosts: files dns\npasswd: files\ngroup: files\n"),
        ("os-release",    b"NAME=\"Leandros\"\nVERSION=\"1.0\"\nID=leandros\nPRETTY_NAME=\"Leandros 1.0\"\n"),
    ):
        if not any(n == _name for n, _p, _m in etc_files):
            etc_files.append((_name, _data, 0o100644))

    # ── K3 dynamic-linking corpus (dynamic test ladder) ───────────────────────
    # The runtime libc is Alpine's unmodified musl (ports/alpine/<arch>.lock,
    # apk-owned). /lib/ld-musl-<arch>.so.1 is its regular file (PT_INTERP target:
    # the kernel never resolves a symlink for the interpreter); libc.musl-<arch>.so.1
    # and /usr/lib/libc.so are symlinks to it. A byte copy of the loader is kept as
    # /lib/ld-musl-<arch>.so.1.recovery (added after the lock is staged below) for
    # recovery from a static shell. The test ladder binaries are dynamic-PIE ELFs
    # whose PT_INTERP points at /lib/ld-musl-<arch>.so.1.
    dyn_root = os.path.expanduser("~/code/leandros-artifacts/musl-dynamic")
    usr_lib_files = [("libc.so", f"/lib/ld-musl-{arch}.so.1".encode(), S_IFLNK | 0o777)]
    import apkdb
    print(f"libc: Alpine musl from ports/alpine/{arch}.lock (apk-owned); recovery copy ld-musl-{arch}.so.1.recovery")
    for name, rel in (
        ("hello-dyn",    f"test/hello-dyn/hello-dyn-{arch}"),
        ("hello-dyn-rs", f"test/hello-dyn-rs/hello-dyn-rs-{arch}"),
        ("dlopen-host",  f"test/dlopen-host/dlopen-host-{arch}"),
    ):
        p = f"{dyn_root}/{rel}"
        if os.path.exists(p):
            bin_files.append((name, p, 0o100755))
    # plugin.so is dlopen("./plugin.so")'d by dlopen-host: place it both in /bin
    # (next to dlopen-host) and in / so a relative open resolves from either cwd.
    plugin = f"{dyn_root}/test/dlopen-host/plugin-{arch}.so"
    if os.path.exists(plugin):
        bin_files.append(("plugin.so", plugin, 0o100755))
        root_files.append(("plugin.so", plugin, 0o100755))

    # ── K4 GL ship set (kmscube, M3) ──────────────────────────────────────────
    # Runtime libraries for the kms_swrast GBM + GLES2 path, packed under their
    # SONAMEs into /usr/lib (the loader resolves DT_NEEDED by soname, so no
    # symlink support is needed — same trick as ld-musl above). The soname paths
    # in the merged sysroot are symlinks that Python follows to the real
    # versioned .so, so the content is stored once under the soname name.
    gl_root = os.path.expanduser("~/code/leandros-artifacts/m3-gl-stack")
    gl_lib_dir = f"{gl_root}/sysroot-{arch}/usr/lib"
    # The GPU ship-set (ports/mesa/build-gpu-stack.sh): one megadriver with
    # zink + virgl + softpipe, the Venus ICD, the Vulkan loader zink dlopen()s,
    # libzstd, and /bin/gpuprobe. Every file it provides WINS over the
    # softpipe-era sysroot above; everything it does not provide (libdrm,
    # expat, wayland, ...) still comes from the sysroot. Without it the image
    # has no GPU renderer and /bin/gpu-env refuses to start COSMIC — say so
    # here, at build time, rather than on a black screen.
    gpu_stage = f"{gl_root}/gpu-stage-{arch}"
    # LEANDROS_GPU_STAGE=<dir> (containing gpu-stage-<arch>/) tests a rebuilt
    # ship-set without replacing the shared one.
    # INTEL=1 (or LEANDROS_GPU_INTEL=1) picks the side stage built with
    # `INTEL=1 OUT_SUFFIX=-intel ports/mesa/build-gpu-stack.sh x86_64`: the
    # same ship-set plus iris + ANV (laptops with Intel graphics, i915.uapi=1).
    # x86_64 only; aarch64 keeps the shared stage.
    if arch == "x86_64" and "1" in (os.environ.get("INTEL"), os.environ.get("LEANDROS_GPU_INTEL")):
        gpu_stage = f"{gl_root}/gpu-stage-x86_64-intel"
        print(f"  GPU ship-set: Intel (iris + ANV) stage {gpu_stage}")
    if os.environ.get("LEANDROS_GPU_STAGE"):
        gpu_stage = os.path.join(os.path.expanduser(os.environ["LEANDROS_GPU_STAGE"]), f"gpu-stage-{arch}")
        print(f"  GPU ship-set override: {gpu_stage}")
    gpu_lib_dir = f"{gpu_stage}/usr/lib"
    # Intel ship-set check: iris in the megadriver plus ANV and its ICD json.
    # Requested (INTEL=1) but missing is fatal; otherwise the flavour is
    # stamped next to the image (<img>.gpu) so write-usb.sh can refuse a
    # laptop stick built from a generic image.
    _gal = f"{gpu_lib_dir}/libgallium-25.3.6.so"
    _has_intel = (os.path.exists(_gal) and b"iris_screen_create" in open(_gal, "rb").read()
                  and os.path.exists(f"{gpu_lib_dir}/libvulkan_intel.so")
                  and os.path.exists(f"{gpu_stage}/usr/share/vulkan/icd.d/intel_icd.{arch}.json"))
    if arch == "x86_64" and "1" in (os.environ.get("INTEL"), os.environ.get("LEANDROS_GPU_INTEL")) and not _has_intel:
        sys.exit(f"❌ INTEL=1 but {gpu_stage} lacks iris/libvulkan_intel/intel_icd json "
                 f"(build: INTEL=1 OUT_SUFFIX=-intel ports/mesa/build-gpu-stack.sh x86_64)")
    GPU_FLAVOR = "intel" if _has_intel else "generic"
    print(f"  GPU flavour: {GPU_FLAVOR}")
    if not os.path.exists(f"{gpu_lib_dir}/libgallium-25.3.6.so"):
        print(f"⚠️  no GPU Mesa ship-set at {gpu_stage} — COSMIC will have NO GPU "
              f"renderer in this image (build it: ports/mesa/build-gpu-stack.sh {arch})")
    def gl_lib(name):
        p = f"{gpu_lib_dir}/{name}"
        return p if os.path.exists(p) else f"{gl_lib_dir}/{name}"
    _gpuprobe = f"{gpu_stage}/usr/bin/gpuprobe"
    if os.path.exists(_gpuprobe):
        bin_files.append(("gpuprobe", _gpuprobe, 0o100755))
    # Pi GPU smoke tests (scripts/pi-gpu-smoke.sh), built with the stage:
    # es31test = GLES 3.1 compute (V3D SUBMIT_CSD), vkprobe = Vulkan fill via
    # the Khronos loader. Absent from older stages: simply not packed.
    for _smoke in ("es31test", "vkprobe"):
        _sp = f"{gpu_stage}/usr/bin/{_smoke}"
        if os.path.exists(_sp):
            bin_files.append((_smoke, _sp, 0o100755))
    # Mesa's disk cache dereferences the driver's NT_GNU_BUILD_ID note; a
    # megadriver/ICD linked without one faults at screen creation with
    # FAR=0x10 (lane kmscube2, #98 tail). Refuse to pack one.
    for _bid in ("libgallium-25.3.6.so", "libvulkan_virtio.so", "libvulkan_broadcom.so",
                 "libvulkan_nouveau.so", "libvulkan_intel.so"):
        _bp = f"{gpu_lib_dir}/{_bid}"
        if os.path.exists(_bp) and not elf_has_gnu_build_id(_bp):
            sys.exit(f"❌ {_bp} has no GNU build-id note (Mesa faults at FAR=0x10 without one); "
                     f"relink with -Wl,--build-id (ports/mesa/build-gpu-stack.sh {arch})")
    gbm_files = []
    for so in ("libEGL.so.1", "libGLESv2.so.2", "libgbm.so.1", "libdrm.so.2",
               "libgallium-25.3.6.so", "libexpat.so.1", "libz.so.1",
               # New DT_NEEDEDs of the virgl-enabled megadriver. The previous
               # softpipe-only build (cross-musl) needed only z/expat/drm;
               # the Alpine-native build that adds virgl pulls in zstd for its
               # shader cache and the C++ runtime that gallium's C++ sources
               # require. Missing them is not a soft failure: the Mesa loader
               # reports "failed to open dri: Error loading shared library
               # libzstd.so.1", GBM device creation fails, and cosmic-comp
               # panics in smithay's `Failed to load LibEGL`.
               "libzstd.so.1", "libstdc++.so.6", "libgcc_s.so.1",
               # The Khronos Vulkan loader, needed only by Zink (GL-on-Vulkan).
               # vktest reaches Venus by dlopen()ing libvulkan_virtio.so at an
               # absolute path and never wanted a loader; zink dlopen()s
               # "libvulkan.so.1" and has no other way to find an ICD, so
               # without this the whole GL stack silently falls back.
               "libvulkan.so.1",
               # __stack_chk_guard for Alpine's prebuilt loader (aarch64 GCC
               # reads the global guard; LeandrOS libc.so does not export it).
               # The stage patchelf-adds it to libvulkan.so.1's DT_NEEDED.
               "libleandros_ssp.so.1",
               "libwayland-client.so.0", "libwayland-server.so.0",
               # libwayland-egl.so.1 is dlopen()ed at runtime by wayland-sys
               # (wayland-egl.rs egl.rs:25 tries "libwayland-egl.so.1" then
               # ".so") the moment a client creates a WlEglSurface. cosmic-panel
               # is the first client to use client-side Wayland-EGL; without it
               # the panel panics ("Library libwayland-egl.so could not be
               # loaded.") right after "Waiting for configure event".
               "libwayland-egl.so.1", "libffi.so.8",
               # libpam.so.0 is the LeandrOS shadow-auth shim (source:
               # m6-session-bins/src/libpam-shim); verifies $sha256$ /etc/shadow
               # like /bin/login. It is DT_NEEDED by the cosmic-greeter ELF
               # itself — pam-sys is an unconditional dependency of that build —
               # so the dynamic loader resolves it on EVERY launch regardless of
               # which role the binary takes. The login role makes no PAM call at
               # all (greeter.rs has none; authentication crosses greetd IPC),
               # but the library still has to be present or the greeter fails to
               # load. Do not unstage it on the grounds that nothing calls it.
               "libpam.so.0"):
        p = gl_lib(so)
        if os.path.exists(p):
            usr_lib_files.append((so, p, 0o100755))
    # GBM backend, dlopened by absolute path /usr/lib/gbm/dri_gbm.so.
    dri = gl_lib("gbm/dri_gbm.so")
    if os.path.exists(dri):
        gbm_files.append(("dri_gbm.so", dri, 0o100755))
    # kmscube is Alpine's package (ports/alpine/world; /bin/kmscube -> /usr/bin/kmscube below).

    # ── M4 input/XKB ship set (anvil compositor) ──────────────────────────────
    # anvil (ET_DYN, PT_INTERP=/lib/ld-musl-<arch>.so.1) + the wl_shm/xdg_shell
    # test client, plus anvil's input stack. Input libraries are packed under
    # their SONAME into /usr/lib (same soname trick as the GL set: the loader
    # resolves DT_NEEDED by soname and open() follows the staged soname symlink
    # to the real versioned file). The XKB keymap data + libinput quirks form a
    # deep file tree under /usr/share, enumerated here for size accounting and
    # packed recursively further below.
    m4_root   = os.path.expanduser(f"~/code/leandros-artifacts/m4-input-ship/{arch}")
    anvil_bin = os.path.expanduser(f"~/code/leandros-artifacts/m3-gl-stack/out/anvil-{arch}")
    wlclient  = os.path.expanduser(f"~/code/leandros-artifacts/m4-client/wlclient-{arch}")
    if os.path.exists(anvil_bin):
        bin_files.append(("anvil", anvil_bin, 0o100755))
    if os.path.exists(wlclient):
        bin_files.append(("wlclient", wlclient, 0o100755))
    m4_lib_dir = f"{m4_root}/usr/lib"
    # libseat.so.1 / libudev.so.1 are the two shims whose C source is tracked
    # (ports/input-stack/shims) and built by ports/input-stack/build-shims.sh,
    # wired into the normal build via scripts/build-all.sh. Prefer that fresh
    # output over the m4-input-ship blob so a tracked-source edit can never
    # again be silently absent from the image; fall back to the blob if the
    # shim wasn't rebuilt this run (e.g. toolchain unavailable). The other libraries
    # here (libxkbcommon, libinput, ...) are upstream C, not built from this
    # repo, and always come from the blob.
    shim_build_dir = f"target/input-stack-sysroot/{arch}/usr/lib"
    # libxkbcommon, libinput, libevdev, libmtdev, libpixman, libdisplay-info, the XKB data and the
    # libinput quirks are Alpine packages now (ports/alpine/world).
    for so in ("libseat.so.1", "libudev.so.1"):
        built = f"{shim_build_dir}/{so}"
        p = built if os.path.exists(built) else f"{m4_lib_dir}/{so}"
        if os.path.exists(p):
            usr_lib_files.append((so, p, 0o100755))
        elif so in ("libseat.so.1", "libudev.so.1"):
            _image_missing(f"{so} (neither {built} nor {p}; run ports/input-stack/build-shims.sh, needs the clang toolchain)")
    # Recursively enumerate the /usr/share data tree. m4_share_files holds
    # (image_dir_abspath, name, hostpath); m4_share_dirs holds every directory
    # (and its ancestors down to /usr/share) that must be created. Directory
    # inodes are registered into dir_nodes/subdirs after those are defined; the
    # files are packed after the other add_files_to_dir calls. No symlinks exist
    # in the staged tree (they were dereferenced when staged), so a plain walk
    # yields only regular files.
    m4_share_files = []
    m4_share_dirs  = set()
    m4_share_src   = f"{m4_root}/usr/share"
    if False and os.path.isdir(m4_share_src):   # XKB data + libinput quirks come from Alpine packages
        for dirpath, _dirnames, filenames in os.walk(m4_share_src):
            rel = os.path.relpath(dirpath, m4_root)   # e.g. usr/share/X11/xkb/symbols
            parts = rel.split("/")
            for i in range(2, len(parts) + 1):        # register usr/share ... down (usr already exists)
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            image_dir = "/" + rel
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp):
                    m4_share_files.append((image_dir, fn, hp))

    # ── Synthetic sysfs for the DRM nodes ────────────────────────────────────
    #
    # This tree used to be four empty directories, enough only for anvil's
    # drm-rs (DrmNode::from_path -> is_device_drm() stat()s
    # /sys/dev/char/<maj>:<min>/device/drm, and node_with_type(Primary)
    # read_dir()s it for a "card0" entry). Mesa's Venus Vulkan ICD needs much
    # more: virtgpu_open() (mesa/src/virtio/vulkan/vn_renderer_virtgpu.c:1677)
    # calls drmGetDevices2(), which classifies a node ENTIRELY from sysfs and
    # drops any node it cannot classify. The exact contract, read off libdrm
    # 2.4.134's xf86drm.c (the version ports/mesa/build-libdrm.sh builds), for
    # each /dev/dri/<name> that readdir(3) turns up:
    #
    #   1. stat("/sys/dev/char/<maj>:<min>/device/drm")            [drmNodeIsDRM, :3324]
    #      must succeed, or process_device() rejects the node outright.
    #   2. readlink("/sys/dev/char/<maj>:<min>/device/subsystem")  [get_subsystem_type, :3577]
    #      must succeed; libdrm takes strrchr(target,'/') and matches it against
    #      "/pci", "/usb", "/platform", "/virtio", ... A failed readlink yields a
    #      negative subsystem type, which process_device()'s switch (:4558) sends
    #      to `default: return -1`. THIS IS THE ONE HARD SYMLINK REQUIREMENT —
    #      no regular file will do. The target is never resolved, only string-
    #      matched, so it may dangle (we point it at a real dir anyway).
    #   3. realpath("/sys/dev/char/<maj>:<min>/device") -> pci_path             [get_pci_path, :3651]
    #      then, if pci_path's last component starts with "/virtio", strip it.
    #      Because `device` here is a real directory rather than Linux's symlink
    #      into /sys/devices/..., realpath() returns the path unchanged and the
    #      "/virtio" strip is a no-op — i.e. we present the shape of a plain PCI
    #      GPU, which is libdrm's primary path and needs no virtio parent walk.
    #      (musl 1.2.5's realpath is a pure userspace readlink loop and treats a
    #      non-symlink component's EINVAL as "not a link", which is exactly what
    #      the f2fs server returns — servers/f2fs/src/lib.rs:2387.)
    #   4. fopen(pci_path + "/uevent") and find a "PCI_SLOT_NAME=" line, parsed
    #      with "%04x:%02x:%02x.%1u".        [sysfs_uevent_get :3536, drmParsePciBusInfo :3725]
    #   5. fopen(pci_path + "/{vendor,device,subsystem_vendor,subsystem_device}"),
    #      each read with fscanf("%x"); "revision" is read first only when the
    #      caller passes DRM_DEVICE_GET_PCI_REVISION.   [parse_separate_sysfs_files :3836]
    #      If that whole step fails, libdrm falls back to reading 64 raw bytes
    #      from pci_path + "/config".                   [parse_config_sysfs_file :3876]
    #
    # Venus calls drmGetDevices2(0, ...) — no revision flag — and then requires
    # vendor_id==0x1af4 && device_id==0x1050 and available_nodes & (1<<
    # DRM_NODE_RENDER) (vn_renderer_virtgpu.c:1598-1611). The render bit is why
    # both 226:0 and 226:128 get a full attribute set with IDENTICAL contents:
    # drmFoldDuplicatedDevices (:4589) merges two nodes into one device iff
    # their drmPciBusInfo compare equal, and only then does the surviving device
    # carry both the primary and the render node.
    DRM_MAJOR = 226
    # The virtio-gpu PCI identity. vendor/device/class are what the guest's own
    # scan reports (drivers/src/pci.rs::scan prints 0x1AF4:0x1050; class_rev's
    # top byte is 0x03 = display, subclass 0x80 = other) and are fixed
    # properties of QEMU's virtio-gpu-pci / virtio-gpu-gl-pci — the two differ
    # only in whether virgl is enabled, never in PCI ids.
    PCI_VENDOR      = 0x1af4
    PCI_DEVICE      = 0x1050   # 0x1040 + 16 (VIRTIO_ID_GPU), i.e. modern virtio
    PCI_REVISION    = 0x01     # QEMU stamps revision 1 on every modern-only virtio device
    PCI_SUBVENDOR   = 0x1af4
    PCI_SUBDEVICE   = 0x1100   # PCI_SUBDEVICE_ID_QEMU
    PCI_CLASS       = 0x038000 # display / other / prog-if 0
    # Bus address. The image is built before the guest enumerates anything, so
    # this has to be baked; it is the slot QEMU actually assigns, with the GPU
    # the 4th -device on the bus after drive0/data0/data1 (scripts/run-qemu.sh
    # and .claude/skills/run-leandros/driver.py agree on that order for both
    # machines: aarch64 "virt" puts the host bridge at 00:00.0, q35 puts the MCH
    # there and its ICH9 functions at 00:1f.x, so either way the four -device
    # entries land on slots 1..4). Nothing in this stack makes a decision from
    # the value — libdrm only parses it into drmPciBusInfo, and its one load-
    # bearing property is that BOTH nodes report the same string so
    # drmFoldDuplicatedDevices merges them.
    PCI_SLOT_NAME = "0000:00:04.0"

    # sysfs_files carries (image_dir, name, content_bytes, mode); it is folded
    # into the same per-inode packing table as the /usr/share tree below. The
    # content is inline bytes rather than a host path, which add_files_to_dir
    # already understands (it skips the hardlink dedup for those).
    sysfs_files = []

    def _sysfs_attr(image_dir, name, content, mode=0o100444):
        data = content.encode('ascii') if isinstance(content, str) else content
        sysfs_files.append((image_dir, name, data, mode))

    # A 64-byte slice of PCI configuration space, laid out per the PCI spec at
    # exactly the offsets libdrm's parse_config_sysfs_file reads: vendor 0x00,
    # device 0x02, revision 0x08, class triple 0x09..0x0b, subsystem 0x2c/0x2e.
    # Only reached if the individual attribute files above ever fail to open.
    _cfg = bytearray(64)
    struct.pack_into('<HH', _cfg, 0x00, PCI_VENDOR, PCI_DEVICE)
    _cfg[0x08] = PCI_REVISION
    _cfg[0x09] = PCI_CLASS & 0xFF          # prog-if
    _cfg[0x0a] = (PCI_CLASS >> 8) & 0xFF   # subclass
    _cfg[0x0b] = (PCI_CLASS >> 16) & 0xFF  # base class
    struct.pack_into('<HH', _cfg, 0x2c, PCI_SUBVENDOR, PCI_SUBDEVICE)

    _uevent = (
        "DRIVER=virtio-pci\n"
        f"PCI_CLASS={PCI_CLASS:X}\n"
        f"PCI_ID={PCI_VENDOR:04X}:{PCI_DEVICE:04X}\n"
        f"PCI_SUBSYS_ID={PCI_SUBVENDOR:04X}:{PCI_SUBDEVICE:04X}\n"
        f"PCI_SLOT_NAME={PCI_SLOT_NAME}\n"
        f"MODALIAS=pci:v{PCI_VENDOR:08X}d{PCI_DEVICE:08X}"
        f"sv{PCI_SUBVENDOR:08X}sd{PCI_SUBDEVICE:08X}"
        f"bc{(PCI_CLASS >> 16) & 0xFF:02X}sc{(PCI_CLASS >> 8) & 0xFF:02X}"
        f"i{PCI_CLASS & 0xFF:02X}\n"
    )

    # The bus directory the "subsystem" links point at. Only its name matters to
    # libdrm (it string-matches the final component), but a link that resolves
    # is cheaper to reason about than one that dangles.
    #
    # /sys/class and /sys/class/block are staged here too: the vfs only
    # synthesizes the *contents* of /sys/class/block (see block::is_sysfs_path),
    # so the directories themselves must exist on disk for `ls /sys/class` to
    # list `block` at all. The same holds for the power_supply/backlight
    # overlay (servers/vfs/src/sysclass.rs); with no devices registered they
    # are simply empty, as on Linux.
    if os.environ.get("LEANDROS_BOARD") == "rpi5":
        m4_share_dirs.add("/sys/bus/platform")
    for d in ("/sys", "/sys/bus", "/sys/bus/pci", "/sys/bus/pci/devices",
              "/sys/class", "/sys/class/block",
              "/sys/class/drm", "/sys/class/input",
              "/sys/class/power_supply", "/sys/class/backlight",
              "/sys/firmware", "/sys/firmware/acpi", "/sys/firmware/acpi/tables",
              "/sys/fs", "/sys/fs/cgroup"):
        m4_share_dirs.add(d)

    # Linux-shaped DRM tree: the PCI (or platform) device owns real
    # `drm/<node>` directories under /sys/devices; /sys/class/drm/<node> and
    # /sys/dev/char/226:<minor> are symlinks into them, and each node has
    # `device` (-> the parent device) and `subsystem` (-> class/drm) links, so a
    # realpath()/readlink walk (libdrm, udev, drm-rs) resolves as on Linux.
    _rpi5 = os.environ.get("LEANDROS_BOARD") == "rpi5"
    if _rpi5:
        _devdir = "/sys/devices/platform/axi/2000000.v3d"
        _dev_to_sys = "../../../.."          # device dir -> /sys
        _devparents = ("/sys/devices", "/sys/devices/platform", "/sys/devices/platform/axi")
    else:
        _devdir = f"/sys/devices/pci0000:00/{PCI_SLOT_NAME}"
        _dev_to_sys = "../../.."
        _devparents = ("/sys/devices", "/sys/devices/pci0000:00")
    _devrel = _devdir[len("/sys"):]          # /devices/...
    for d in ("/sys/dev", "/sys/dev/char") + _devparents + (_devdir, _devdir + "/drm"):
        m4_share_dirs.add(d)
    if _rpi5:
        # Raspberry Pi 5: the DRM device is the V3D 7.1 platform device
        # (DT /axi/v3d@2000000). libdrm's drmGetDevices2 classifies it from
        # the `subsystem` link name and reads OF_FULLNAME + OF_COMPATIBLE_*
        # from uevent; v3dv only accepts DRM_BUS_PLATFORM devices, and
        # Mesa's GL loader falls back to DRM_IOCTL_VERSION ("v3d") only
        # when there is no PCI id. The PCI virtio identity would send
        # GL to virtio_gpu and hide the GPU from Vulkan.
        _sysfs_attr(_devdir, "subsystem", _dev_to_sys + "/bus/platform", 0o120777)
        _sysfs_attr(_devdir, "uevent", RPI5_V3D_UEVENT)
    else:
        _sysfs_attr(_devdir, "subsystem", _dev_to_sys + "/bus/pci", 0o120777)
        _sysfs_attr(_devdir, "uevent",           _uevent)
        _sysfs_attr(_devdir, "vendor",           f"0x{PCI_VENDOR:04x}\n")
        _sysfs_attr(_devdir, "device",           f"0x{PCI_DEVICE:04x}\n")
        _sysfs_attr(_devdir, "subsystem_vendor", f"0x{PCI_SUBVENDOR:04x}\n")
        _sysfs_attr(_devdir, "subsystem_device", f"0x{PCI_SUBDEVICE:04x}\n")
        _sysfs_attr(_devdir, "revision",         f"0x{PCI_REVISION:02x}\n")
        _sysfs_attr(_devdir, "class",            f"0x{PCI_CLASS:06x}\n")
        _sysfs_attr(_devdir, "config",           bytes(_cfg))
    for _minor, _node in ((0, "card0"), (128, "renderD128")):
        _nd = f"{_devdir}/drm/{_node}"
        m4_share_dirs.add(_nd)
        _sysfs_attr(_nd, "uevent",
                    f"MAJOR={DRM_MAJOR}\nMINOR={_minor}\nDEVNAME=dri/{_node}\nDEVTYPE=drm_minor\n")
        _sysfs_attr(_nd, "dev", f"{DRM_MAJOR}:{_minor}\n")
        _sysfs_attr(_nd, "device", "../..", 0o120777)
        _sysfs_attr(_nd, "subsystem", "../../" + _dev_to_sys + "/class/drm", 0o120777)
        _sysfs_attr("/sys/class/drm", _node, f"../..{_devrel}/drm/{_node}", 0o120777)
        _sysfs_attr("/sys/dev/char", f"{DRM_MAJOR}:{_minor}", f"../..{_devrel}/drm/{_node}", 0o120777)

    # Linux has /sys/block (the whole-disk view); libflatpak's bwrap setup binds it unconditionally.
    # The vfs synthesizes /sys/class/block, so the staged /sys/block points there.
    _sysfs_attr("/sys", "block", "class/block", 0o120777)
    # /sys/class/input and /sys/dev/char/13:* are NOT staged: the vfs
    # synthesizes them from the live evdev registry (servers/vfs/src/sysinput.rs),
    # so hot-plugged nodes (event2..) appear and disappear with the device.
    # /sys/class/input itself (an empty directory) stays staged above.

    # ── M2 Venus ship set (Mesa's virtio Vulkan ICD + the vktest smoke test) ──
    # Sourced from the venus-lane stage dir directly rather than by copying into
    # the m3-gl-stack sysroot. Every ship set above already reads from the stage
    # dir of the lane that produced it (m3-gl-stack, m4-input-ship, m4-client,
    # m5-session-*), so a venus_root of its own is the established shape; folding
    # these into another lane's sysroot would mutate an artifact tree this script
    # does not own and lose the file whenever that sysroot is rebuilt.
    #
    # libvulkan_virtio.so's DT_NEEDED closure is libz.so.1, libdrm.so.2,
    # libwayland-client.so.0, libexpat.so.1 (all in the GL loop above),
    # libdisplay-info.so.3 (the M4 input loop) and libc.so — i.e. nothing new to
    # stage. There is deliberately NO Khronos loader on LeandrOS, so the ICD is
    # never resolved through DT_NEEDED; vktest dlopen()s it by absolute path and
    # the JSON below exists so anything that does read an ICD manifest finds the
    # same library_path.
    venus_root = os.path.expanduser(f"~/code/leandros-artifacts/venus-lane/stage-{arch}")
    # The ICD + manifest come from the GPU ship-set when it exists: built in
    # the same tree as the zink megadriver, so the two are ABI-consistent.
    _icd_root = gpu_stage if os.path.exists(f"{gpu_stage}/usr/lib/libvulkan_virtio.so") else venus_root
    _icd_so = f"{_icd_root}/usr/lib/libvulkan_virtio.so"
    if os.path.exists(_icd_so):
        usr_lib_files.append(("libvulkan_virtio.so", _icd_so, 0o100755))
    _vktest = f"{venus_root}/usr/bin/vktest"
    if os.path.exists(_vktest):
        bin_files.append(("vktest", _vktest, 0o100755))
    _vkrender = f"{venus_root}/usr/bin/vkrender"
    if os.path.exists(_vkrender):
        bin_files.append(("vkrender", _vkrender, 0o100755))
    # vkswap — the VK_EXT_headless_surface swapchain test. Same staging shape
    # as vkrender; absent from the artifact tree it is simply not packed.
    _vkswap = f"{venus_root}/usr/bin/vkswap"
    if os.path.exists(_vkswap):
        bin_files.append(("vkswap", _vkswap, 0o100755))
    # vkwl — M4: the same swapchain, but on a real Wayland surface handed to
    # cosmic-comp instead of a headless one. Its DT_NEEDED closure is libc.so
    # plus libwayland-client.so.0, both already packed by the GL loop above,
    # because it is built with the m3-gl-stack musl toolchain that produced
    # wlclient rather than in the Alpine container the rest of venus-lane uses.
    _vkwl = f"{venus_root}/usr/bin/vkwl"
    if os.path.exists(_vkwl):
        bin_files.append(("vkwl", _vkwl, 0o100755))
    # The ICD manifest rides the shared /usr/share tree machinery. EVERY ancestor
    # has to be in m4_share_dirs — /usr is static (ino 15) but /usr/share only
    # exists because the M4 walk happens to create it, which is not a dependency
    # worth having.
    _icd_json = f"{_icd_root}/usr/share/vulkan/icd.d/virtio_icd.{arch}.json"
    if os.path.exists(_icd_json):
        for d in ("/usr/share", "/usr/share/vulkan", "/usr/share/vulkan/icd.d"):
            m4_share_dirs.add(d)
        m4_share_files.append(("/usr/share/vulkan/icd.d",
                               f"virtio_icd.{arch}.json", _icd_json))

    # NVK (NVIDIA Vulkan, nouveau ICD) from the same GPU ship-set. Present so
    # real-NVIDIA hosts/passthrough work; on a Venus guest there is no nouveau
    # device, NVK enumerates zero physical devices and the loader moves on to
    # the virtio ICD. Optional: a stage built with NVK=0 simply lacks it.
    _nvk_so = f"{gpu_stage}/usr/lib/libvulkan_nouveau.so"
    _nvk_json = f"{gpu_stage}/usr/share/vulkan/icd.d/nouveau_icd.{arch}.json"
    if os.path.exists(_nvk_so) and os.path.exists(_nvk_json):
        usr_lib_files.append(("libvulkan_nouveau.so", _nvk_so, 0o100755))
        for d in ("/usr/share", "/usr/share/vulkan", "/usr/share/vulkan/icd.d"):
            m4_share_dirs.add(d)
        m4_share_files.append(("/usr/share/vulkan/icd.d",
                               f"nouveau_icd.{arch}.json", _nvk_json))

    # ANV (Intel Vulkan) from the same ship-set (INTEL=1 stages); optional.
    _anv_so = f"{gpu_stage}/usr/lib/libvulkan_intel.so"
    _anv_json = f"{gpu_stage}/usr/share/vulkan/icd.d/intel_icd.{arch}.json"
    if os.path.exists(_anv_so) and os.path.exists(_anv_json):
        usr_lib_files.append(("libvulkan_intel.so", _anv_so, 0o100755))
        for d in ("/usr/share", "/usr/share/vulkan", "/usr/share/vulkan/icd.d"):
            m4_share_dirs.add(d)
        m4_share_files.append(("/usr/share/vulkan/icd.d",
                               f"intel_icd.{arch}.json", _anv_json))

    # v3dv (Raspberry Pi 5 V3D 7.1 Vulkan) from the same ship-set; aarch64
    # stages only. On a non-Pi guest it finds no DRM_BUS_PLATFORM v3d device,
    # enumerates zero physical devices and the loader moves on.
    _v3dv_so = f"{gpu_stage}/usr/lib/libvulkan_broadcom.so"
    _v3dv_json = f"{gpu_stage}/usr/share/vulkan/icd.d/broadcom_icd.{arch}.json"
    if os.path.exists(_v3dv_so) and os.path.exists(_v3dv_json):
        usr_lib_files.append(("libvulkan_broadcom.so", _v3dv_so, 0o100755))
        for d in ("/usr/share", "/usr/share/vulkan", "/usr/share/vulkan/icd.d"):
            m4_share_dirs.add(d)
        m4_share_files.append(("/usr/share/vulkan/icd.d",
                               f"broadcom_icd.{arch}.json", _v3dv_json))
    elif os.environ.get("LEANDROS_BOARD") == "rpi5":
        print(f"⚠️  LEANDROS_BOARD=rpi5 but {gpu_stage} lacks libvulkan_broadcom.so + broadcom ICD json: "
              f"no Vulkan on the Pi (rebuild: ports/mesa/build-gpu-stack.sh aarch64)")

    # nvktest — the NVK bring-up test (ports/nvk-tests/build.sh), dlopen()s
    # /usr/lib/libvulkan_nouveau.so directly like vkrender does the Venus ICD;
    # its DT_NEEDED is libc.so only. /bin/nvk-autotest (from the repo) waits
    # for the render node and runs it with output on the console. Either file
    # absent -> simply not packed.
    _nvkt_art = os.path.expanduser(os.environ.get("LEANDROS_ARTIFACTS", "~/code/leandros-artifacts"))
    _nvkt_out = os.environ.get("NVK_TESTS_OUT") or os.path.join(_nvkt_art, "nvk-tests")
    _nvktest = os.path.join(_nvkt_out, f"stage-{arch}", "usr", "bin", "nvktest")
    # nvkmstest — the KMS test (lane nvkms), run by nvk-autotest after nvktest.
    _nvkmstest = os.path.join(_nvkt_out, f"stage-{arch}", "usr", "bin", "nvkmstest")
    _nvk_auto = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                             "ports", "nvk-tests", "nvk-autotest.sh")
    _nvk_staged = []
    if os.path.isfile(_nvktest):
        bin_files.append(("nvktest", _nvktest, 0o100755))
        _nvk_staged.append(f"/bin/nvktest ({os.path.getsize(_nvktest)} bytes)")
    if os.path.isfile(_nvkmstest):
        bin_files.append(("nvkmstest", _nvkmstest, 0o100755))
        _nvk_staged.append(f"/bin/nvkmstest ({os.path.getsize(_nvkmstest)} bytes)")
    # nvgltest — GLES via GBM/EGL (zink on NVK); links the GPU stage's
    # libEGL/libGLESv2/libgbm, so it is only useful (and only staged) next to them.
    _nvgltest = os.path.join(os.path.dirname(_nvktest), "nvgltest")
    if os.path.isfile(_nvgltest) and os.path.exists(f"{gpu_lib_dir}/libEGL.so.1"):
        bin_files.append(("nvgltest", _nvgltest, 0o100755))
        _nvk_staged.append(f"/bin/nvgltest ({os.path.getsize(_nvgltest)} bytes)")
    # i915gltest — deterministic 3D on the real i915 (lane gpuctx); init
    # runs it after `i915test coh` for i915.autotest=1.
    # Built by ports/nvk-tests/build-i915gltest.sh (clang musl wrapper, no container).
    _i915gl = os.path.join(_nvkt_art, "i915gltest", f"stage-{arch}", "i915gltest")
    if os.path.isfile(_i915gl) and os.path.exists(f"{gpu_lib_dir}/libEGL.so.1"):
        bin_files.append(("i915gltest", _i915gl, 0o100755))
        _nvk_staged.append(f"/bin/i915gltest ({os.path.getsize(_i915gl)} bytes)")
    if os.path.isfile(_nvk_auto):
        bin_files.append(("nvk-autotest", _nvk_auto, 0o100755))
        _nvk_staged.append("/bin/nvk-autotest")
    if _nvk_staged:
        print(f"  NVK tests: {', '.join(_nvk_staged)}")

    # /bin/snd-autotest (lane hda): the boot-time sound self-test init runs
    # when the kernel cmdline has snd.test=1; it drives /bin/sndtest.
    _snd_auto = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                             "userland", "sndtest", "snd-autotest.sh")
    if os.path.isfile(_snd_auto):
        bin_files.append(("snd-autotest", _snd_auto, 0o100755))

    # NVIDIA GSP firmware for the OpenRM driver (kernel feature `nvidia`,
    # x86_64 only): /lib/firmware/nvidia/<tag>/gsp_{ga10x,tu10x}.bin, where the
    # kernel's nv_get_firmware reads it (drivers/nvidia/src/fw.rs). Taken from
    # the OpenRM cache that ports/openrm/fetch.sh fills ($OPENRM_CACHE, default
    # ~/.cache/leandros/openrm); NVIDIA-redistributable but never committed.
    # Absent cache -> silently skipped. ga10x covers GA10x/AD10x (RTX 30/40),
    # tu10x Turing + GA100.
    if arch == "x86_64":
        _orm_ver = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ports", "openrm", "version.sh")
        _orm_tag = None
        if os.path.exists(_orm_ver):
            for _l in open(_orm_ver):
                if _l.startswith("OPENRM_TAG="):
                    _orm_tag = _l.split("=", 1)[1].strip().strip('"')
        _orm_cache = os.path.expanduser(os.environ.get("OPENRM_CACHE", os.path.join(os.environ.get("XDG_CACHE_HOME", "~/.cache"), "leandros/openrm")))
        if _orm_tag:
            _fw_src = os.path.join(_orm_cache, "firmware", _orm_tag)
            _fw_dir = f"/lib/firmware/nvidia/{_orm_tag}"
            for _fw in ("gsp_ga10x.bin", "gsp_tu10x.bin"):
                _p = os.path.join(_fw_src, _fw)
                if os.path.isfile(_p):
                    for d in ("/lib/firmware", "/lib/firmware/nvidia", _fw_dir):
                        m4_share_dirs.add(d)
                    m4_share_files.append((_fw_dir, _fw, _p))
                    print(f"  NVIDIA firmware: {_fw_dir}/{_fw} ({os.path.getsize(_p)} bytes)")

    # The boot-time network self-test (net.test=1 on the kernel command line:
    # init runs /bin/net-autotest, which runs /bin/netcheck).
    _net_auto = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                             "userland", "netcheck", "net-autotest.sh")
    if os.path.isfile(_net_auto):
        bin_files.append(("net-autotest", _net_auto, 0o100755))

    # /bin/dmesg: the kernel's RAM log ring (/proc/klog), optional substring filter.
    _dmesg = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                          "userland", "dmesg", "dmesg.sh")
    if os.path.isfile(_dmesg):
        bin_files.append(("dmesg", _dmesg, 0o100755))
    # /bin/wifiscan: the iwlwifi driver's last scan (/proc/net/wifi).
    _wifiscan = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                             "userland", "wifiscan", "wifiscan.sh")
    if os.path.isfile(_wifiscan):
        bin_files.append(("wifiscan", _wifiscan, 0o100755))

    # /bin/brcmfctl: runtime retry/diag of the Pi 5 Wi-Fi bring-up (/etc/brcmf.ctl).
    _brcmfctl = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                             "userland", "brcmfctl", "brcmfctl.sh")
    if os.path.isfile(_brcmfctl):
        bin_files.append(("brcmfctl", _brcmfctl, 0o100755))

    # Device firmware (iwlwifi, Intel BT, i915 DMC, r8169 PHY patches on x86_64;
    # CYW43455 Wi-Fi + BT patch on aarch64): exactly the files the drivers load,
    # taken from Alpine's linux-firmware-* packages at the locked release
    # (ports/alpine/firmware.{list,lock}, scripts/alpine_firmware.py), decompressed
    # to the plain paths the kernel requests. Not part of the apk db / world.
    # Wi-Fi credentials are NEVER in the repo: $LEANDROS_WIFI_CONF may name a
    # host file (`ssid=...` / `psk=...` lines) to stage as /etc/leandros/wifi.conf.
    import alpine_firmware
    _fw = alpine_firmware.stage(arch)
    for _d, _n, _h, _s in _fw:
        m4_share_dirs.add("/lib/firmware")
        m4_share_dirs.add(_d)
        m4_share_files.append((_d, _n, _h))
    if _fw:
        print(f"  firmware (Alpine): {len(_fw)} files in /lib/firmware ({sum(os.path.getsize(h) for _, _, h, _ in _fw)} bytes)")
    _wifi_conf = os.environ.get("LEANDROS_WIFI_CONF")
    if _wifi_conf and os.path.isfile(os.path.expanduser(_wifi_conf)):
        m4_share_dirs.add("/etc/leandros")
        m4_share_files.append(("/etc/leandros", "wifi.conf", os.path.expanduser(_wifi_conf)))
        print("  Wi-Fi credentials: /etc/leandros/wifi.conf (from $LEANDROS_WIFI_CONF)")
    # Root's SSH keys for images with no ESP key import (Raspberry Pi 5 card):
    # $LEANDROS_AUTHORIZED_KEYS names a host file staged as /root/.ssh/authorized_keys.
    _auth_keys = os.environ.get("LEANDROS_AUTHORIZED_KEYS")
    if _auth_keys and os.path.isfile(os.path.expanduser(_auth_keys)):
        m4_share_dirs.add("/root/.ssh")
        m4_share_files.append(("/root/.ssh", "authorized_keys", os.path.expanduser(_auth_keys)))
        print("  SSH keys: /root/.ssh/authorized_keys (from $LEANDROS_AUTHORIZED_KEYS)")

    # Saved network connections (/dev/netctl, docs/servers/netctl.md): the
    # directory root:netdev 0770 and both A/B slots pre-created empty,
    # root:netdev 0660. netdev members (the NM shim, the netctl CLI) rewrite a
    # slot in place and ask the kernel to reload; nobody else can read the
    # secrets in them. Owner/mode are applied where the tree is packed.
    m4_share_dirs.add("/etc/leandros")
    m4_share_dirs.add("/etc/leandros/network")

    # Kernel modules for kmodctl / kmodctltest: target/kmod/<arch>/rootfs/lib/modules/<id>/*
    # (kmod-build), else the initrd copy (testblob). Absent without KMOD_TEST / =m.
    for _kd in (f"target/kmod/{arch}/rootfs", f"target/kmod/{arch}/initrd"):
        _km = os.path.join(_kd, "lib", "modules")
        if os.path.isdir(_km):
            m4_share_dirs.update(("/lib", "/lib/modules"))
            for _id in sorted(os.listdir(_km)):
                _idd = os.path.join(_km, _id)
                if not os.path.isdir(_idd):
                    continue
                m4_share_dirs.add(f"/lib/modules/{_id}")
                for _f in sorted(os.listdir(_idd)):
                    if not any(x[0] == f"/lib/modules/{_id}" and x[1] == _f for x in m4_share_files):
                        m4_share_files.append((f"/lib/modules/{_id}", _f, os.path.join(_idd, _f)))

    # kexec image (lane kexecload): the direct-boot kernel ELF the in-guest
    # kexec_file_load loads, as /boot/kexec. Omitted when not built.
    _kexec_img = f"target/final-{arch}/kernel-direct"
    if os.path.isfile(_kexec_img):
        m4_share_dirs.add("/boot")
        m4_share_files.append(("/boot", "kexec", _kexec_img))
        # Its initrd (lane kexecexec): the same cpio Limine loads as a module.
        _kexec_initrd = f"initrd-{arch}.cpio"
        if os.path.isfile(_kexec_initrd):
            m4_share_files.append(("/boot", "kexec-initrd", _kexec_initrd))
            # Pi 5 slots (scripts/remote.py pi-*): the flashed kernel is also the
            # starting `good` and `trial`, so a serial-console kexec works at once.
            if arch == "aarch64":
                import subprocess as _sp, tempfile as _tf, time as _tm
                try:
                    _rev = _sp.check_output(["git", "rev-parse", "--short=8", "HEAD"], text=True).strip()
                except Exception:
                    _rev = "unknown"
                _info = _tf.NamedTemporaryFile("w", suffix=".info", delete=False)
                _info.write(f"{_rev} {_tm.strftime('%Y-%m-%dT%H:%M:%S')} flashed\n")
                _info.close()
                for _slot in ("good", "trial"):
                    _d = f"/boot/pi/{_slot}"
                    m4_share_dirs.update(("/boot/pi", _d))
                    m4_share_files.append((_d, "kernel", _kexec_img))
                    m4_share_files.append((_d, "initrd", _kexec_initrd))
                    m4_share_files.append((_d, "info", _info.name))

    # ── M5 session/compositor ship set (cosmic-comp + D-Bus session + fonts) ──
    # cosmic-comp (ET_DYN, PT_INTERP=/lib/ld-musl-<arch>.so.1) reuses the exact
    # M3 GL + M4 input ship sets already packed above; its DT_NEEDED closure is
    # fully covered there (verified: m5-session-ship/verify-closure.sh), so no
    # new libraries are needed — only the compositor binary, the session bus
    # (busd + its POSIX-sh launcher + config) and the default UI fonts.
    #
    # The bus binary must be executable — dbus-run-session gates on `test -x`
    # $BUSD_BIN and busd is exec'd directly (static ET_EXEC) — so busd and
    # dbus-run-session are packed 0755 via m5_exec_files (resolved to their
    # target-dir inode after the /usr tree registration below). session.conf
    # and the fonts are plain data (0644) and ride the shared /usr tree walk
    # (m4_share_files/m4_share_dirs, generalized here beyond /usr/share).
    verify_dbus_staging(arch)
    m5_arch_root = os.path.join(os.environ.get("LEANDROS_M5_SHIP") or os.path.expanduser("~/code/leandros-artifacts/m5-session-ship"), arch)
    m5_fonts_src = os.path.join(os.environ.get("LEANDROS_M5_SHIP") or os.path.expanduser("~/code/leandros-artifacts/m5-session-ship"), "share/fonts")
    # cosmic-comp: LEANDROS_COSMIC_COMP=<file> wins; else the m6 rebuild that
    # carries ports/cosmic-comp's patches (ports/cosmic-comp/build.sh);
    # else the original M5 probe build.
    cosmic_comp = os.environ.get("LEANDROS_COSMIC_COMP") or next(
        (p for p in (os.path.expanduser(f"~/code/leandros-artifacts/{d}/cosmic-comp-{arch}")
                     for d in ("m6-session-bins/out", "m3-gl-stack/out"))
         if os.path.exists(p)), "")
    if cosmic_comp:
        print(f"  cosmic-comp: {cosmic_comp}")
        bin_files.append(("cosmic-comp", cosmic_comp, 0o100755))
    # (image_dir_abspath, name, hostpath) packed 0755 after inode registration.
    m5_exec_files = []
    if os.path.isdir(m5_arch_root):
        for dirpath, _dn, filenames in os.walk(m5_arch_root):
            rel = os.path.relpath(dirpath, m5_arch_root)   # e.g. "usr/libexec"
            if rel == ".":
                continue
            parts = rel.split("/")
            for i in range(1, len(parts) + 1):             # register /usr, /usr/bin, ...
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            image_dir = "/" + rel
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if not os.path.isfile(hp):
                    continue
                if fn in ("busd", "dbus-run-session", "leandros-sysbus", "busctl", "pkcon", "leandros-pk-helper"):  # must be executable
                    m5_exec_files.append((image_dir, fn, hp))
                else:                                       # session.conf etc. = data
                    m4_share_files.append((image_dir, fn, hp))
    # Default UI fonts -> /usr/share/fonts/<family>/… — the exact scan dir
    # fontdb's load_no_fontconfig() walks on LeandrOS (no fontconfig config
    # present). Zero fonts is a soft-fail (blank text, no panic); packing them
    # makes the compositor UI legible. Plain data (0644).
    if os.path.isdir(m5_fonts_src):
        for dirpath, _dn, filenames in os.walk(m5_fonts_src):
            rel = os.path.relpath(dirpath, m5_fonts_src)   # e.g. "open-sans" or "."
            sub = "" if rel == "." else "/" + rel
            image_dir = "/usr/share/fonts" + sub
            parts = ("usr/share/fonts" + sub).split("/")
            for i in range(2, len(parts) + 1):
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp):
                    m4_share_files.append((image_dir, fn, hp))

    # ── M6 full COSMIC session ship set ───────────────────────────────────────
    # The desktop session (cosmic-session + its fatal-at-spawn set and tolerant
    # applets) on top of the M5 compositor/bus/font base. launch-pad spawns every
    # child by BARE NAME via PATH, so all session binaries install into /bin
    # (which is on the launcher's PATH) under the EXACT name cosmic-session execs
    # them by — note cosmic-session spawns "cosmic-app-library" but the built
    # binary file is named cosmic-applibrary (main.rs:330).
    #
    # cosmic-settings-daemon is one of the four fatal-at-spawn children (a missing
    # binary panics the whole session); it links libpipewire-0.3.so.0, satisfied
    # by the inert stub staged into /usr/lib. All eight libcosmic binaries reuse
    # the M3 GL + M4 input closures already packed (Wayland/EGL are dlopen'd, not
    # DT_NEEDED); only libudev (cosmic-settings) and the pipewire stub are new,
    # both already present. No source patches — feature flags only (see manifest).
    m6_out = os.path.expanduser("~/code/leandros-artifacts/m6-session-bins/out")
    # LEANDROS_COSMIC_BINS_OVERLAY=<dir>: a file there named like one in
    # m6_out (e.g. m6-session-bins/out-wgpu/cosmic-greeter-x86_64, the wgpu
    # renderer builds from ports/cosmic-wgpu) is staged instead of it. Lets a
    # lane test rebuilt clients without touching the shared out/ directory.
    m6_overlay = os.environ.get("LEANDROS_COSMIC_BINS_OVERLAY", "")

    def m6_pick(src):
        if m6_overlay:
            cand = os.path.join(os.path.expanduser(m6_overlay), os.path.basename(src))
            if os.path.exists(cand):
                print(f"  overlay: {os.path.basename(src)} <- {cand}")
                return cand
        return src
    pw_out = os.path.expanduser("~/code/leandros-artifacts/pipewire-gap/out")
    m6_session_bins = [
        ("cosmic-session",         f"{m6_out}/cosmic-session-{arch}"),
        ("cosmic-panel",           f"{m6_out}/cosmic-panel-{arch}"),
        ("cosmic-notifications",   f"{m6_out}/cosmic-notifications-{arch}"),
        ("cosmic-bg",              f"{m6_out}/cosmic-bg-{arch}"),
        ("cosmic-osd",             f"{m6_out}/cosmic-osd-{arch}"),
        ("cosmic-launcher",        f"{m6_out}/cosmic-launcher-{arch}"),
        ("cosmic-app-library",     f"{m6_out}/cosmic-applibrary-{arch}"),  # spawn name != file name
        ("cosmic-settings",        f"{m6_out}/cosmic-settings-{arch}"),
        ("cosmic-settings-daemon", f"{pw_out}/cosmic-settings-daemon-{arch}"),
        # M7z tolerant-children completion: the names cosmic-session spawns.
        # workspaces is built --no-default-features --features
        # force-shm-screencopy (no wgpu, wl_shm capture only); files-applet drops
        # the gvfs feature (no glib/gio on the image); idle is featureless.
        ("cosmic-workspaces",      f"{m6_out}/cosmic-workspaces-{arch}"),
        ("cosmic-files-applet",    f"{m6_out}/cosmic-files-applet-{arch}"),
        # ports/cosmic-idle/build.sh stages its patched build here;
        # LEANDROS_COSMIC_IDLE=<file> overrides it.
        ("cosmic-idle",            os.environ.get("LEANDROS_COSMIC_IDLE") or f"{m6_out}/cosmic-idle-{arch}"),
        # cosmic-term is NOT spawned by cosmic-session. It is launched on demand
        # from the launcher / app library, which resolve Exec= as a bare name
        # against PATH=/usr/bin:/bin, so /bin is the right home and the staged
        # name must match Exec= in
        #   artifacts/m6-session-data/shared/usr/share/applications/com.system76.CosmicTerm.desktop
        # Built --no-default-features --features wayland,wgpu: password_manager
        # is dropped (it pulls secret-service, and nothing on the image owns
        # org.freedesktop.secrets) and dbus-config is dropped (no cosmic-config
        # D-Bus daemon is staged; cosmic-config falls back to Default, which
        # already asks for "Noto Sans Mono" -- the exact family we stage).
        # Runtime gap: it needs a working /dev/ptmx + TIOCSPTLCK/TIOCGPTPEER
        # (or ENOSYS so the TIOCGPTN + /dev/pts/N fallback engages).
        ("cosmic-term",            f"{m6_out}/cosmic-term-{arch}"),
        # cosmic-randr (pop-os/cosmic-randr cli, built unmodified at the rev
        # cosmic-settings' Cargo.lock pins, 6e8e795). Settings > Displays has no
        # output-management client of its own: cosmic-randr-shell::list() execs
        # `cosmic-randr list --kdl`, and every change execs `cosmic-randr mode/
        # scale/...`. Without this file the spawn fails, the page model stays
        # empty and Displays renders blank.
        ("cosmic-randr",           f"{m6_out}/cosmic-randr-{arch}"),
        # The greeter, DELIBERATELY NOT staged as "cosmic-greeter".
        #
        # This one binary is both the login screen and the in-session lock
        # screen, and it chooses between them from the invoking username alone:
        # greeter::main() when getpwuid(getuid()) is named "cosmic-greeter",
        # locker::main() otherwise (cosmic-greeter/src/main.rs). cosmic-session
        # spawns it in-session by bare name, where it therefore always takes the
        # LOCKER arm — and upstream's non-logind locker locks immediately at
        # start, then process::exit(0)s on unlock, which under cosmic-session's
        # restart supervision is an unbreakable lock/unlock/restart loop. That
        # loop is the entire reason a permanent COSMIC source patch used to exist
        # here.
        #
        # Staging the file under a name cosmic-session does not spawn retires
        # that patch with no COSMIC change at all: start_component
        # ("cosmic-greeter") now fails at Command::spawn, and launch_pad's
        # ProcessManager::start returns that error BEFORE it spawns the
        # supervising process_loop — so the cost is one error line per boot and
        # there is no restart storm. The lock screen is simply never started.
        #
        # The login role is reached the other way round: /bin/greeter-launch
        # drops to the real cosmic-greeter account and execve()s this path, so
        # getpwuid returns the name the greeter arm matches on. The file name is
        # irrelevant to the role — only the account is.
        ("cosmic-greeter-login",   f"{m6_out}/cosmic-greeter-{arch}"),
    ]
    for name, src in m6_session_bins:
        src = m6_pick(src)
        if os.path.exists(src):
            bin_files.append((name, src, 0o100755))

    # Real COSMIC panel applets. cosmic-panel resolves each name in its
    # plugins_wings/plugins_center config to a .desktop under
    # /usr/share/applications and spawns its Exec; a name with no desktop file
    # resolves to exec: None and is silently never spawned
    # (cosmic-panel-bin wrapper_space.rs:460-520), so the binary and the
    # desktop file must be staged together or the applet is invisible with no
    # error anywhere.
    #
    # cosmic-panel-button backs THREE config names from ONE binary — it takes
    # the target's app id as argv[1] (LauncherButton/AppButton/
    # WorkspacesButton -> CosmicLauncher/CosmicAppLibrary/CosmicWorkspaces).
    # Two consequences worth knowing before touching this:
    #
    #   * It resolves that argument to a FILE, not a D-Bus name:
    #     run() (cosmic-panel-button/src/lib.rs:215-244) walks
    #     fde::default_paths() for "<id>.desktop" and PANICS with
    #     "Failed to find valid desktop file" if none matches. So the three
    #     LAUNCH TARGETS' desktop files are staged alongside the applets' own
    #     (see m6-session-data/shared) even though nothing in the panel config
    #     names them. Missing target => the applet dies at startup, not at press.
    #   * A PANEL BUTTON WEARS ITS TARGET'S ICON, NOT ITS OWN. run()
    #     (cosmic-panel-button/src/lib.rs:215-244) parses the TARGET's desktop
    #     file and keeps that file's Name and Icon; the button's own desktop
    #     file contributes nothing but the panel-config name and the Exec line.
    #     So the icons that must exist in the theme are
    #     com.system76.Cosmic{Launcher,AppLibrary,Workspaces} -- NOT
    #     com.system76.CosmicPanel*Button, which nothing ever looks up.
    #     Proven in-guest with a temporary probe printing Named::path():
    #         LEANDROS_ICON_PROBE name=com.system76.CosmicAppLibrary path=None
    #     A miss is SILENT AND SHAPED LIKE SUCCESS: libcosmic's bundle::get is
    #     `None` on unix (widget/icon/bundle.rs:7-10), so Named::handle() falls
    #     back to Svg::from_memory(&[]) -- an EMPTY svg. The widget still takes
    #     its explicit width/height, so the applet lays out at full size, the
    #     dock sizes itself correctly around it, and exactly zero pixels are
    #     painted. It looks identical to "the applet never started".
    #     The three target icons are staged into m6-icons-pruned for this reason;
    #     the CosmicPanel*Button icons are kept only because the panel's own
    #     .desktop metadata references them.
    #   * Pressing it does Command::new("sh").arg("-c").arg("exec <Exec>")
    #     (:126-130) — NOT D-Bus activation. busd's omitted <servicedir> is
    #     therefore not on this path at all. The Exec strings are bare names
    #     (cosmic-launcher, cosmic-app-library, cosmic-workspaces), all three
    #     already staged into /bin above under exactly those names, and /bin/sh
    #     is brush.
    #
    # All three binaries are dynamic musl PIEs whose DT_NEEDED closure is
    # libxkbcommon.so.0 + libc.so — both already packed for the other libcosmic
    # binaries, so these add no new libraries.
    #
    # ── THERE IS NO APPLET-COUNT CEILING — root-caused 2026-08-09 ──
    # All five .desktop files ship, and all eight applet processes now run.
    #
    # The recorded hypothesis here was "more than ~4 concurrent applets kills
    # the panel", with descriptor exhaustion as the leading suspect and a count
    # bisect as the next step. Both were wrong, and the count framing is what
    # made them wrong: a name in the panel config is not a process. `entries` is
    # ["Panel","Dock"], the two spaces resolve their plugin lists independently,
    # and WorkspacesButton/AppButton appear in BOTH — so "stub + 3 buttons" is
    # six processes, not four, and adding two .desktop files takes it to eight.
    # Nothing about that crosses a resource limit; the working and failing sets
    # differ QUALITATIVELY, not by count:
    #
    #   CosmicAppletTiling and CosmicAppletMinimize are exactly the applets
    #   carrying X-HostWaylandDisplay=true, and that flag — and only that flag —
    #   makes cosmic-panel call wp_security_context_v1.create_listener to hand
    #   the applet a privileged socket to the real compositor
    #   (wrapper_space.rs:562-588).
    #
    # That request marshals a LISTENING AF_UNIX socket, and libwayland dups
    # every fd it marshals. Two LeandrOS gaps sat on that path, both now fixed
    # in servers/net (see handle_dup and xfer_export):
    #   1. dup() of a listening socket returned EINVAL — bound addresses were
    #      not refcounted, so a second fd could not be allowed to name one.
    #   2. SCM_RIGHTS could not carry a socket fd at all: vfs::export_fd rejects
    #      anything >= its MAX_FDS of 128, and socket fds start at 0x100.
    #
    # The exit 1 was the FIRST of those, and it is worth remembering why it was
    # fatal rather than degrading: a marshalling failure poisons libwayland's
    # whole connection, so the panel's next event_loop.dispatch()? propagated an
    # Err out of run() and `fn main() -> Result<()>`
    # (cosmic-panel-bin/src/main.rs:72) turned it into exit 1. That is also why
    # the bar vanished entirely — nothing rendered because the process was gone,
    # which reads as "the applets did not render" and is not that at all.
    #
    # Verified both arches 2026-08-09, fresh images, full session:
    #   before  7 x `dup failed: Invalid argument` -> 7 x exit 1 (restart loop)
    #   after   0 dup failures, 0 marshalling errors, 0 exits; 8 applets spawn
    #           once each; desktop alive at t+3 min (aarch64) / t+8 min (x86_64)
    # scmtest 35/35 and vfstest 36/36 on both arches.
    #
    # RESOLVED, and deliberately not conflated with the spawn fix above: the
    # tiling icon now paints, after 0f56aab+cdd613e (fork_dup dropped
    # UnixPendingAccept fds; f2fs MAX_OPEN_FILES 32->256) and 4085b7f (nested
    # epoll fd read as POLLNVAL readiness, which is what unblocked the clock).
    # cosmic-applet-minimize stays a 1x1 hidden surface with zero toplevels —
    # correct with no windows open, by upstream design, not a bug.
    m6_applets = [
        ("cosmic-panel-button",     f"{m6_out}/cosmic-panel-button-{arch}"),
        ("cosmic-applet-minimize",  f"{m6_out}/cosmic-applet-minimize-{arch}"),
        ("cosmic-applet-tiling",    f"{m6_out}/cosmic-applet-tiling-{arch}"),
    ]
    for name, src in m6_applets:
        src = m6_pick(src)
        if os.path.exists(src):
            bin_files.append((name, src, 0o100755))
        else:
            print(f"  WARNING: applet binary absent, panel entry will be dead: {src}")

    # cosmic-applet-network, the panel's Network applet: unmodified upstream
    # from the pinned cosmic-applets tree (e544cb5), built with the m6 recipe
    # by ports/cosmic-applet-network/build.sh. It speaks NetworkManager only
    # through nmrs 3.4.0, i.e. to leandros-sysbus networkmanager. Binary and
    # .desktop are staged together or not at all: the panel resolves the name
    # in its config to the desktop file and spawns its Exec, so either one
    # alone is an applet that silently never appears.
    _netapplet = m6_pick(f"{m6_out}/cosmic-applet-network-{arch}")
    if os.path.exists(_netapplet):
        bin_files.append(("cosmic-applet-network", _netapplet, 0o100755))
        m4_share_dirs.add("/usr/share/applications")
        m4_share_files.append(("/usr/share/applications", "com.system76.CosmicAppletNetwork.desktop",
                               "ports/cosmic-applet-network/com.system76.CosmicAppletNetwork.desktop"))
    else:
        print(f"  WARNING: {_netapplet} absent, no Network applet in the panel: "
              f"./ports/cosmic-applet-network/build.sh {arch}")

    # cosmic-applet-bluetooth, the panel's Bluetooth applet (already named in
    # upstream's default plugins_wings): unmodified upstream, built by
    # ports/cosmic-applet-bluetooth/build.sh. It reaches leandros-sysbus bluez
    # through bluer 0.17.4, i.e. the C libdbus-1 (Alpine dbus-libs) -- its only
    # DT_NEEDED beyond libxkbcommon/libc, so binary, library and .desktop are
    # staged together or not at all (a binary without the library dies in the
    # loader and the panel just respawns it).
    _btapplet = m6_pick(f"{m6_out}/cosmic-applet-bluetooth-{arch}")
    # libdbus-1.so.3 comes from the vanilla Alpine dbus-libs package (ports/alpine).
    if os.path.exists(_btapplet):
        bin_files.append(("cosmic-applet-bluetooth", _btapplet, 0o100755))
        m4_share_dirs.add("/usr/share/applications")
        m4_share_files.append(("/usr/share/applications", "com.system76.CosmicAppletBluetooth.desktop",
                               "ports/cosmic-applet-bluetooth/com.system76.CosmicAppletBluetooth.desktop"))
    else:
        print(f"  WARNING: {_btapplet} absent, no Bluetooth applet in the panel: "
              f"./ports/cosmic-applet-bluetooth/build.sh {arch}")

    # cosmic-applet-power, the panel's Power menu (already named in upstream's
    # default plugins_wings): unmodified upstream, built by
    # ports/cosmic-applet-power/build.sh. Binary and .desktop are staged
    # together or not at all, like the Network applet above.
    _pwrapplet = m6_pick(f"{m6_out}/cosmic-applet-power-{arch}")
    if os.path.exists(_pwrapplet):
        bin_files.append(("cosmic-applet-power", _pwrapplet, 0o100755))
        m4_share_dirs.add("/usr/share/applications")
        m4_share_files.append(("/usr/share/applications", "com.system76.CosmicAppletPower.desktop",
                               "ports/cosmic-applet-power/com.system76.CosmicAppletPower.desktop"))
    else:
        print(f"  WARNING: {_pwrapplet} absent, no Power applet in the panel: "
              f"./ports/cosmic-applet-power/build.sh {arch}")

    # COSMIC apps for the Applications menu (ports/cosmic-apps/build.sh):
    # cosmic-files, cosmic-edit, cosmic-screenshot (Print key target). Each is
    # staged as /bin/<app> together with its .desktop and hicolor icons, or
    # not at all.
    cosapps_icon_dirs = []
    for _app, _appid in (("cosmic-files", "com.system76.CosmicFiles"),
                         ("cosmic-edit", "com.system76.CosmicEdit"),
                         ("cosmic-store", "com.system76.CosmicStore"),
                         ("cosmic-player", "com.system76.CosmicPlayer"),
                         ("cosmic-screenshot", "com.system76.CosmicScreenshot"),
                         ("cosmic-monitor", "com.system76.CosmicMonitor"),
                         ("cosmic-initial-setup", "com.system76.CosmicInitialSetup")):
        _appbin = m6_pick(f"{m6_out}/{_app}-{arch}")
        if os.path.exists(_appbin):
            bin_files.append((_app, _appbin, 0o100755))
            m4_share_dirs.add("/usr/share/applications")
            m4_share_files.append(("/usr/share/applications", f"{_appid}.desktop",
                                   f"ports/cosmic-apps/{_appid}.desktop"))
            # cosmic-initial-setup is NOT autostarted (our image creates the
            # accounts itself): only its NoDisplay .desktop is staged, so it can
            # be launched by hand, and the Autostart .desktop is left out.
            _appres = "resources" if _app == "cosmic-screenshot" else "res"
            cosapps_icon_dirs.append(os.path.expanduser(
                f"~/code/leandros-artifacts/m6-session-bins/src/{_app}/{_appres}/icons/hicolor"))
        else:
            print(f"  WARNING: {_appbin} absent, no {_app} in the Applications menu: "
                  f"./ports/cosmic-apps/build.sh {arch} {_app}")
    # LeandrOS installer GUI (ports/leandros-installer): /bin binary + launcher
    # entry, or neither. Uses a stock themed icon, so no hicolor dir.
    _inst = m6_pick(f"{m6_out}/leandros-installer-{arch}")
    if os.path.exists(_inst):
        bin_files.append(("leandros-installer", _inst, 0o100755))
        m4_share_dirs.add("/usr/share/applications")
        m4_share_files.append(("/usr/share/applications", "com.leandros.Installer.desktop",
                               "ports/leandros-installer/com.leandros.Installer.desktop"))
    else:
        print(f"  WARNING: {_inst} absent, no installer GUI: ./ports/leandros-installer/build.sh {arch}")
    real_time_applet = False
    # The rest of the panel's applets (Time, Battery, Notifications, Status Area,
    # Workspaces, Input Sources, A11y) and the dock's App List: unmodified
    # upstream, one binary per crate, built together by
    # ports/cosmic-applets-panel/build.sh. Upstream's default panel config
    # (/usr/share/cosmic/com.system76.CosmicPanel.*/v1/plugins_*) already names
    # all of them, so binary + .desktop is all that puts them on the panel.
    # Each binary and its .desktop are staged together or not at all.
    for _ap, _dk in (("cosmic-applet-time", "CosmicAppletTime"),
                     ("cosmic-applet-battery", "CosmicAppletBattery"),
                     ("cosmic-applet-notifications", "CosmicAppletNotifications"),
                     ("cosmic-applet-status-area", "CosmicAppletStatusArea"),
                     ("cosmic-applet-workspaces", "CosmicAppletWorkspaces"),
                     ("cosmic-applet-input-sources", "CosmicAppletInputSources"),
                     ("cosmic-applet-a11y", "CosmicAppletA11y"),
                     ("cosmic-app-list", "CosmicAppList"),
                     ("cosmic-applet-audio", "CosmicAppletAudio")):
        _apbin = m6_pick(f"{m6_out}/{_ap}-{arch}")
        if os.path.exists(_apbin):
            bin_files.append((_ap, _apbin, 0o100755))
            m4_share_dirs.add("/usr/share/applications")
            m4_share_files.append(("/usr/share/applications", f"com.system76.{_dk}.desktop",
                                   f"ports/cosmic-applets-panel/com.system76.{_dk}.desktop"))
            if _ap == "cosmic-applet-time":
                real_time_applet = True
        else:
            print(f"  WARNING: {_apbin} absent, no {_ap} in the panel: "
                  f"./ports/cosmic-applets-panel/build.sh {arch}")

    # Upstream's default plugins_wings (the compiled-in default and the
    # initial-setup layout) lists the Workspaces BUTTON, not the Workspaces
    # APPLET. System default (cosmic-config reads /usr/share/cosmic) = upstream's
    # list plus com.system76.CosmicAppletWorkspaces after the app button.
    if "cosmic-applet-workspaces" in [b[0] for b in bin_files]:
        for _d in ("/usr/share/cosmic", "/usr/share/cosmic/com.system76.CosmicPanel.Panel",
                   "/usr/share/cosmic/com.system76.CosmicPanel.Panel/v1"):
            m4_share_dirs.add(_d)
        m4_share_files.append(("/usr/share/cosmic/com.system76.CosmicPanel.Panel/v1", "plugins_wings",
                               "ports/cosmic-applets-panel/panel_plugins_wings"))

    # leandros-applet — a minimal dependency-free wl_shm xdg_toplevel panel applet.
    # cosmic-panel refuses to render its bar with no applet content (render()
    # early-returns while actual_size<=20), and the real cosmic applets pull
    # tokio+zbus+system services (timedate1/logind/upower) absent on LeandrOS. This
    # tiny client draws one solid block so the panel has real content and commits
    # frame 0. It is spawned by cosmic-panel via the desktop file staged into
    # /usr/share/applications (m6-session-data/shared) whose stem matches the panel
    # config's center applet name (com.system76.CosmicAppletTime). Pure-Rust wayland
    # backend => only ld-musl at runtime.
    m7w_applet = os.path.expanduser(
        f"~/code/leandros-artifacts/m7w-applet/out/leandros-applet-{arch}")
    if os.path.exists(m7w_applet):
        bin_files.append(("leandros-applet", m7w_applet, 0o100755))

    # wl-globals — M9 Stage 0a instrument. Dumps the wl_registry of EVERY
    # wayland-* socket in $XDG_RUNTIME_DIR and exits, ignoring the environment
    # (so it reaches cosmic-comp, not the panel's embedded server that
    # leandros-applet is handed via WAYLAND_SOCKET). Measurement only; nothing
    # in the session depends on it.
    m9_wlglobals = os.path.expanduser(
        f"~/code/leandros-artifacts/m9-wlglobals/out/wl-globals-{arch}")
    if os.path.exists(m9_wlglobals):
        bin_files.append(("wl-globals", m9_wlglobals, 0o100755))
    else:
        print(f"  note: /bin/wl-globals not staged ({m9_wlglobals} absent); measurement-only, nothing depends on it")

    # liprobe: runs libinput directly, with its log priority raised to DEBUG, so
    # the input path can be observed without patching COSMIC. It is what showed
    # libinput itself produces events (motion_abs, key, dispatch_err=0) while the
    # compositor acts on none of them, narrowing the break to smithay's drain or
    # cosmic-comp's routing. Source is in-repo at artifacts/m13-liprobe/.
    m13_liprobe = os.path.expanduser(
        f"~/code/leandros-artifacts/m13-liprobe/out/liprobe-{arch}")
    if os.path.exists(m13_liprobe):
        bin_files.append(("liprobe", m13_liprobe, 0o100755))

    # wlinput: a Wayland client that maps a real xdg_toplevel and counts every
    # wl_pointer / wl_keyboard / wl_touch event the compositor sends it. liprobe
    # instruments the layer BELOW cosmic-comp; this one instruments the layer
    # ABOVE it, so between them the compositor is bracketed and "cosmic-comp
    # never received input" and "cosmic-comp received it and routed it nowhere"
    # stop looking the same. Source is in-repo at artifacts/m14-wlinput/.
    m14_wlinput = os.path.expanduser(
        f"~/code/leandros-artifacts/m14-wlinput/out/wlinput-{arch}")
    if os.path.exists(m14_wlinput):
        bin_files.append(("wlinput", m14_wlinput, 0o100755))

    # greetd — the greeter IPC daemon behind cosmic-greeter. Upstream
    # kennylevinsen/greetd built static musl by ports/greetd/build.sh (ET_EXEC,
    # no PT_INTERP, first PT_LOAD @ 0x200000 — the shape the loader needs), so
    # it is packed straight into /bin like any other static binary. Its config,
    # its PAM service marker and the /etc/profile that carries the session
    # environment ride the /etc directory registration further down.
    greetd_dir = _greetd_stage_dir(arch)
    greetd_bin = os.path.join(greetd_dir, "greetd") if greetd_dir else None
    if greetd_bin and os.path.exists(greetd_bin):
        _check_greetd_fresh(greetd_bin)
        bin_files.append(("greetd", greetd_bin, 0o100755))

    # fakegreet — upstream greetd's protocol test harness, built alongside the
    # daemon by the same ports/greetd/build.sh. No PAM, no VT, no session
    # worker, no fork, no /proc/self/exe, no mlockall: it binds the greetd IPC
    # socket, sets GREETD_SOCK itself and answers start_session with Success
    # without starting anything. It is how the greeter half (render environment,
    # DRM, socket, the DBUS_SYSTEM_BUS_ADDRESS trap) gets proven with none of
    # the daemon's risk surface in frame.
    fakegreet_bin = os.path.join(greetd_dir, "fakegreet") if greetd_dir else None
    if fakegreet_bin and os.path.exists(fakegreet_bin):
        bin_files.append(("fakegreet", fakegreet_bin, 0o100755))

    # The two greeter launchers and the environment they share, POSIX-sh
    # scripts run as `sh /bin/<name>` (they carry no `#!` line). Staged
    # into /bin next to start-cosmic-leandros for the same reason that one is:
    # committing the whole launch up front keeps the typed command short, and
    # serial RX drops characters once a session is live.
    _greetd_scripts = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                   "..", "ports", "greetd", "data")
    for _s in ("greeter-env", "greeter-fake", "greeter-real", "gpu-env"):
        _p = os.path.normpath(os.path.join(_greetd_scripts, _s))
        if os.path.exists(_p):
            bin_files.append((_s, _p, 0o100755))

    # greetd's data files. These need two directories that do not exist in the
    # image yet, so they ride the m4_share_dirs/m4_share_files tables rather
    # than the static dir_nodes list: that machinery allocates an inode per new
    # directory and wires it into dir_nodes/subdirs, and its only requirement is
    # that the PARENT already exists — /etc does (ino 9). Despite the "m4_share"
    # name it is not limited to /usr/share; the M5 ship set already uses it for
    # arbitrary paths. Files packed through it are 0644, which is right for all
    # three.
    #
    #   /etc/greetd/greetd.conf  the config greetd reads with no arguments
    #   /etc/pam.d/greetd        an existence marker; greetd refuses to start
    #                            without it (server.rs pam_service_exists) and
    #                            nothing ever reads the contents
    #   /etc/profile             the session environment, sourced by greetd's
    #                            source_profile wrapper — greetd passes NOTHING
    #                            of its own environment to a session
    _greetd_data = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "..", "ports", "greetd", "data")
    #   /usr/share/wayland-sessions/cosmic.desktop
    #                            the session list the greeter offers. Its Exec
    #                            is what greetd runs after a successful login;
    #                            with none of these the greeter still renders,
    #                            with an empty session dropdown
    # Console keymaps for /bin/loadkmap come from Alpine's kbd-bkeymaps
    # (/usr/share/bkeymaps/<dir>/<name>.bmap.gz, ports/alpine/world);
    # /etc/vconsole.conf KEYMAP=<name> selects one at boot (loadkmap aliases
    # kbd's names such as de-latin1 onto them).
    for _dirpath, _name, _srcname in (("/etc/greetd", "greetd.conf", "greetd.conf"),
                                      ("/etc/pam.d",  "greetd",      "pam.d-greetd"),
                                      ("/etc",        "profile",     "profile"),
                                      ("/usr/share/wayland-sessions",
                                       "cosmic.desktop", "cosmic.desktop")):
        _src = os.path.normpath(os.path.join(_greetd_data, _srcname))
        if not os.path.exists(_src):
            continue
        if _dirpath != "/etc":
            m4_share_dirs.add(_dirpath)
        m4_share_files.append((_dirpath, _name, _src))

    # /usr/bin/env — the POSIX path, as a second name for the coreutils
    # multicall. It is what makes an authenticated login actually reach a
    # session: cosmic-greeter builds EVERY session command as
    #   ["/usr/bin/env", "K=V", ..., <the .desktop Exec words>]
    # ("Session exec may contain environmental variables",
    # cosmic-greeter/src/greeter.rs), so the path is not optional and is not
    # something greetd or the .desktop file can steer.
    #
    # Its absence is invisible at every layer, which is why it cost a lane a
    # day. brush reports a missing command only on its INTERACTIVE path; the
    # non-interactive `sh -c 'exec <missing>'` form — which is exactly the form
    # greetd's source_profile wrapper uses — prints nothing at all and exits
    # 127. Measured on target, both arms in one boot: `sh -c 'exec
    # /usr/bin/nosuchabs'` and `sh -c 'exec nosuchbare'` each produced no
    # output and rc=127, while a bare name typed at the prompt did print
    # "command not found". So the path form is NOT the discriminator, the
    # interactive/non-interactive split is. greetd then throws the session's
    # exit status away (session/worker.rs: `waitpid(child, None)` ->
    # `Ok(_) => break`), so the 127 is discarded too. The only symptom is the
    # greeter reappearing a second after a correct password.
    #
    # A second directory entry costs no content: add_files_to_dir dedupes by
    # host path and hardlinks, exactly as the ~100 names in /bin already do.
    # Dispatch is safe — the musl build of uutils takes its util name from
    # Path::file_stem(argv[0]) (src/common/validation.rs `binary_path`, the
    # `target_env = "musl"` arm), so "/usr/bin/env" selects `env`.
    if os.path.exists(coreutils_bin):
        m4_share_dirs.add("/usr/bin")
        m5_exec_files.append(("/usr/bin", "env", coreutils_bin))

    # pop-launcher — the search backend behind cosmic-launcher. The frontend
    # spawns it as `Command::new("pop-launcher")` with piped stdin/stdout
    # (pop-launcher-service client.rs), so it is a PATH lookup and /usr/bin is
    # the natural home. Without it the launcher window opens, logs
    # `pop-launcher failed to start`, and every query returns nothing.
    #
    # One multicall binary serves the service AND every plugin, dispatching on
    # the basename of argv[0] (bin/src/main.rs). The service discovers plugins
    # by walking /usr/lib/pop-launcher/plugins/<name>/plugin.ron and execs
    # `<that dir>/<bin.path>` — upstream's justfile symlinks each plugin name
    # back to the main binary; here each is a HARDLINK (add_files_to_dir dedupes
    # by host path), so no symlink resolution is needed at exec time and the
    # image grows by one dentry per plugin, not one 10 MB copy.
    #
    # Only plugins whose runtime exists on this image are staged:
    #   desktop_entries  .desktop search over XDG_DATA_DIRS/applications — the
    #                    launcher's core function. Its startup GPU probe asks
    #                    net.hadess.SwitcherooControl on the system bus (aliased
    #                    to busd); busd's ServiceUnknown reply lets it fall
    #                    through instead of hanging before the first search.
    #   cosmic_toplevel  open-window switching over cosmic-comp's toplevel-info
    #                    protocol; needs only WAYLAND_DISPLAY.
    # Left out: calc (execs qalc), pulse (PulseAudio), pop_shell (GNOME ext),
    # web/terminal/files/find/recent/scripts (browser, terminal-by-name, fd,
    # recently-used.xbel, user scripts — nothing here provides them yet).
    #
    # Sources: the binary is a musl cross-build of upstream pop-os/launcher at
    # the exact revision cosmic-launcher's Cargo.lock pins (a332a3a, the
    # epoch-1.3.0 submodule), unmodified; the plugin.ron files come from the
    # same pinned checkout in the ../cosmic-epoch sibling.
    popl_bin = os.path.expanduser(
        f"~/code/leandros-artifacts/m6-session-bins/out/pop-launcher-{arch}")
    #
    # The plugins are NOT optional. With none registered, the service answers
    # a Search by querying nobody, so `finished()` never runs and no Update is
    # ever sent (service/src/lib.rs search/finished); cosmic-launcher shows its
    # window only on that first Update (app.rs, SurfaceState::WaitingToBeShown),
    # so Super does nothing at all. That was every Linux-built image until
    # 2026-09-27: popl_src was hardcoded to the Mac's ~/code/cosmic-epoch, the
    # same trap sessmisc found for the Shortcuts defaults — so it resolves
    # through _find_cosmic_epoch() now, and a missing plugin.ron stops the
    # build instead of printing a warning nobody reads.
    popl_src = os.path.join(_find_cosmic_epoch(), "pop-launcher", "plugins", "src")
    if os.path.exists(popl_bin):
        m4_share_dirs.add("/usr/bin")
        m5_exec_files.append(("/usr/bin", "pop-launcher", popl_bin))
        for _plugin in ("desktop_entries", "cosmic_toplevel"):
            _ron = f"{popl_src}/{_plugin}/plugin.ron"
            if not os.path.exists(_ron):
                raise SystemExit(
                    f"ERROR: pop-launcher plugin {_plugin}: {_ron} absent. Without "
                    "plugins the launcher never opens. Point LEANDROS_COSMIC_EPOCH at a "
                    "cosmic-epoch checkout with pop-launcher/plugins/src.")
            _pdir = f"/usr/lib/pop-launcher/plugins/{_plugin}"
            for _d in ("/usr/lib/pop-launcher", "/usr/lib/pop-launcher/plugins", _pdir):
                m4_share_dirs.add(_d)
            m4_share_files.append((_pdir, "plugin.ron", _ron))
            m5_exec_files.append((_pdir, _plugin.replace("_", "-"), popl_bin))

    # ── disks-rs: the disk-provisioning end-to-end path ──────────────────────
    # disktester (AerynOS's disks-rs, built UNMODIFIED from the sibling
    # checkout) drives the whole block layer in one run: ftruncate a 32 GiB
    # sparse image, attach it to /dev/loopN, write a GPT, sync the partition
    # nodes through BLKPG, then format them. The two formatters are ours
    # (Alpine dosfstools and xfsprogs) and are execed BY BARE NAME through
    # std::process::Command — partitioning/src/formatter.rs hardcodes
    # "mkfs.fat" and "mkfs.xfs" — so they have to sit on the guest's PATH,
    # which is /usr/bin:/bin. /bin is where every other static binary lives
    # and is what the names resolve against; there is no /sbin on this image.
    #
    # Each of the three is staged only if it was built: mkfs.fat and mkfs.xfs
    # come from in-tree ports whose build.sh may not have run, and disktester
    # needs a sibling repo build-all.sh skips with a warning when absent.
    _musl_triple = ("aarch64-unknown-linux-musl" if arch == "aarch64"
                    else "x86_64-unknown-linux-musl")
    p = f"../disks-rs/target/{_musl_triple}/release/disktester"
    if os.path.exists(p):
        bin_files.append(("disktester", p, 0o100755))
    # grep/egrep/fgrep: userland/grep, a POSIX grep front end that runs /bin/rg
    # (argv0 selects -E / -F). One binary, three names.
    p = f"userland/grep/target/{_musl_triple}/release/grep"
    if os.path.exists(p):
        for _g in ("grep", "egrep", "fgrep"):
            bin_files.append((_g, p, 0o100755))

    # spawnwedge: the musl thread/fork lock-handoff regression test (the
    # cosmic-comp keybinding wedge). A std/musl crate with its own build.sh
    # (userland/spawnwedge), not a workspace member, hence staged here rather
    # than through `bins` above. Run as `spawnwedge`; PASS/FAIL per subtest.
    p = f"userland/spawnwedge/target/{_musl_triple}/release/spawnwedge"
    if os.path.exists(p):
        bin_files.append(("spawnwedge", p, 0o100755))

    # killmt: kill -9 of a multithreaded process must reap every thread and
    # wedge no CPU. A std/musl crate with its own build.sh (userland/killmt),
    # staged like spawnwedge. Run as .
    p = f"userland/killmt/target/{_musl_triple}/release/killmt"
    if os.path.exists(p):
        bin_files.append(("killmt", p, 0o100755))

    # futextest: futex keying, robust lists, PI hand-off (lane futex).
    p = f"userland/futextest/target/{_musl_triple}/release/futextest"
    if os.path.exists(p):
        bin_files.append(("futextest", p, 0o100755))

    # schedstress: scheduler stress (lane percpu) -- lost wakes, stop/cont,
    # group exit, MT fork, non-leader exec, fairness, reaping storm.
    p = f"userland/schedstress/target/{_musl_triple}/release/schedstress"
    if os.path.exists(p):
        bin_files.append(("schedstress", p, 0o100755))

    # procmax: fork/thread limits at the wall (clean EAGAIN) and an exit
    # burst before wait4 (no lost status). std/musl, staged like killmt.
    p = f"userland/procmax/target/{_musl_triple}/release/procmax"
    if os.path.exists(p):
        bin_files.append(("procmax", p, 0o100755))

    # blanktest: inside a Wayland session, powers every output off and checks
    # that frame callbacks keep coming (and a window mapped while off gets its
    # first one) without the compositor turning the output back on.
    # `blanktest --off` / `--on` just switch the outputs. std/musl, like procmax.
    p = f"userland/blanktest/target/{_musl_triple}/release/blanktest"
    if os.path.exists(p):
        bin_files.append(("blanktest", p, 0o100755))

    # mintest: COSMIC toplevel probe (zcosmic_toplevel_info/manager) for the
    # minimize session suite: `mintest list`, `mintest min|unmin <app_id>`.
    p = f"userland/mintest/target/{_musl_triple}/release/mintest"
    if os.path.exists(p):
        bin_files.append(("mintest", p, 0o100755))

    # greetd-login: from a root shell, logs a user in through the running
    # greeter's greetd (password on stdin) and starts their COSMIC session.
    p = f"userland/greetdlogin/target/{_musl_triple}/release/greetd-login"
    if os.path.exists(p):
        bin_files.append(("greetd-login", p, 0o100755))

    # nmtest: drives org.freedesktop.NetworkManager (leandros-sysbus
    # networkmanager) through nmrs 3.4.0, the library the COSMIC network
    # applet uses (userland/nmtest). Run as `dbus-run-session -- nmtest --spawn`
    # (mock backend); PASS/FAIL per check, NMTEST pass=N fail=M.
    p = f"userland/nmtest/target/{_musl_triple}/release/nmtest"
    if os.path.exists(p):
        bin_files.append(("nmtest", p, 0o100755))

    # claude: the Claude Code CLI launcher (userland/claude-run). Set-user-ID
    # root (04755, owned by root like everything staged here): it mounts into
    # and chroots to the Alpine root claude-code-install made, then drops to
    # the caller's ids before running the CLI.
    p = f"userland/claude-run/target/{_musl_triple}/release/claude-run"
    if os.path.exists(p):
        bin_files.append(("claude", p, 0o104755))
        # Same binary (a hard link): run any command of the Alpine root as
        # the caller, e.g. `alpine-run tmux`.
        bin_files.append(("alpine-run", p, 0o104755))

    # su and sudo: sudo-rs (ports/sudo-rs), set-user-ID root. PAM is
    # pam-leandros (/etc/shadow), policy /etc/sudoers (above).
    for name in ("sudo", "su"):
        p = f"ports/sudo-rs/out/{arch}/{name}"
        if os.path.exists(p):
            bin_files.append((name, p, 0o104755))

    # sudotest: su/sudo end to end as root, leandro and cosmic-greeter
    # (userland/sudotest; in runtests.py's regress suite).
    p = f"userland/sudotest/target/{_musl_triple}/release/sudotest"
    if os.path.exists(p):
        bin_files.append(("sudotest", p, 0o100755))

    # sshd: the SSH server (userland/sshd, built on russh), started by init at
    # boot on 0.0.0.0:22. /etc/ssh holds its configuration; the ed25519 host
    # key is generated there on first boot (not at image build time, so no two
    # images share a key).
    p = f"userland/sshd/target/{_musl_triple}/release/sshd"
    if os.path.exists(p):
        m4_share_dirs.update(("/usr/sbin", "/etc/ssh"))
        m5_exec_files.append(("/usr/sbin", "sshd", p))
        m4_share_files.append(("/etc/ssh", "sshd_config", "userland/sshd/sshd_config"))

    # espctl: the EFI system partition from the running system (userland/
    # espctl): kernel slots in limine.conf for remote kernel updates, and the
    # once-per-boot hook init runs (one-shot trial kernels, key import).
    p = f"userland/espctl/target/{_musl_triple}/release/espctl"
    if os.path.exists(p):
        m4_share_dirs.add("/usr/sbin")
        m5_exec_files.append(("/usr/sbin", "espctl", p))
        m5_exec_files.append(("/usr/sbin", "mbrtool", p))

    # leandros-install: the installer backend (userland/leandros-install),
    # run as root (the GUI goes through sudo): list disks, install to one.
    p = f"userland/leandros-install/target/{_musl_triple}/release/leandros-install"
    if os.path.exists(p):
        m4_share_dirs.add("/usr/sbin")
        m5_exec_files.append(("/usr/sbin", "leandros-install", p))

    # disktester loads its provisioning strategy from a path RELATIVE to the
    # working directory — load_provisioning("crates/provisioning/tests/
    # use_whole_disk.kdl") in its main.rs — so the file has to be reachable
    # from wherever it is started, and the documented way to run it is
    #     cd /root/disks-rs && disktester
    # Mirroring the upstream tree layout under /root/disks-rs keeps that one
    # instruction true and leaves disks-rs itself untouched. Copied straight
    # out of the sibling checkout so the guest reads the same strategy the
    # upstream tests do; without it the run reaches "Loopback device:
    # /dev/loop0" and then fails at the KDL parse before touching the disk.
    _kdl = "../disks-rs/crates/provisioning/tests/use_whole_disk.kdl"
    if os.path.exists(_kdl):
        _kdl_dir = "/root/disks-rs/crates/provisioning/tests"
        for _d in ("/root/disks-rs", "/root/disks-rs/crates",
                   "/root/disks-rs/crates/provisioning", _kdl_dir):
            m4_share_dirs.add(_d)
        m4_share_files.append((_kdl_dir, "use_whole_disk.kdl", _kdl))

    # The session launcher itself (a POSIX-sh script with a `#!/bin/sh` line).
    # execve() honours `#!` since the jobctl lane, so both
    # `/bin/start-cosmic-leandros` and the historical
    # `sh /bin/start-cosmic-leandros` work; init and greetd keep the latter.
    # The session launcher carries the renderer policy (/bin/gpu-env: GPU or a
    # loud refusal, never silent softpipe), so the TRACKED copy wins here —
    # unlike the other session_data entries. Hand-synced artifacts trees held
    # a months-old copy that exported GBM_ALWAYS_SOFTWARE=1 and silently
    # shadowed every repo change to this file.
    _repo_launcher = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                  "..", "artifacts", "m6-session-data", "start-cosmic-leandros")
    m6_launcher = (os.path.normpath(_repo_launcher) if os.path.exists(_repo_launcher)
                   else session_data("start-cosmic-leandros"))
    if os.path.exists(m6_launcher):
        bin_files.append(("start-cosmic-leandros", m6_launcher, 0o100755))
    # m4-vkwl — the M4 driver: backgrounds start-cosmic-leandros with its log
    # redirected to a file, waits for the wayland-1 socket, then runs vkwl
    # against it. No `#!` line either: run as `brush /bin/m4-vkwl`.
    m4_drv = session_data("m4-vkwl")
    if os.path.exists(m4_drv):
        bin_files.append(("m4-vkwl", m4_drv, 0o100755))
    # m12-caps — the capability probe: brings the session up the same way and
    # then runs a fixed choreography (idle, pointer, click, three keybindings,
    # wl-globals, one window, two windows, move/resize/close, an application)
    # announcing each window as "M12: MARK <name> <secs>" so artifacts/
    # m12_caps.py can inject QMP input and photograph the scanout inside it.
    # No `#!` line either: run as `brush /bin/m12-caps`.
    m12_drv = session_data("m12-caps")
    if os.path.exists(m12_drv):
        bin_files.append(("m12-caps", m12_drv, 0o100755))
    # m12c-input — the follow-up that attributes m12-caps input null result to
    # a layer: /dev/input and /sys/class/input against what the libudev shim
    # claims, then evtest2 (raw evdev, no libinput in the path), then the same
    # session with RUST_LOG turned up on the smithay input backends.
    m12c_drv = session_data("m12c-input")
    if os.path.exists(m12c_drv):
        bin_files.append(("m12c-input", m12c_drv, 0o100755))
    # m14-input — the guest half of artifacts/m14_input.py: brings the session
    # up, dumps the COSMIC input config (an `input_devices` entry with
    # `state: Disabled` produces exactly the observed symptom), then runs
    # /bin/wlinput against cosmic-comp's socket so the same injection can be
    # counted BELOW the compositor ([EVSTAT]) and ABOVE it ([WLI]) in one run.
    # No `#!` line either: run as `brush /bin/m14-input`.
    m14_drv = session_data("m14-input")
    if os.path.exists(m14_drv):
        bin_files.append(("m14-input", m14_drv, 0o100755))
    # m15-iced — the guest half of artifacts/m15_iced.py. Brings the session up
    # with its output silenced to a file, then launches a raw wl_shm control and
    # cosmic-settings SIDE BY SIDE outside cosmic-session, so the toolkit app's
    # own stderr survives (launch_pad pipes child stderr and registers no
    # on_stderr handler, cosmic-session/src/comp.rs:122-134) and WAYLAND_DEBUG
    # can say whether it never commits or commits blank buffers.
    # No `#!` line either: run as `brush /bin/m15-iced`.
    m15_drv = session_data("m15-iced")
    if os.path.exists(m15_drv):
        bin_files.append(("m15-iced", m15_drv, 0o100755))
    # m15b-iced — the discriminator m15-iced pointed at: the same cosmic-settings
    # binary run twice one environment variable apart, COSMIC_SINGLE_INSTANCE
    # unset then =false, to separate "blocked in libcosmic's blocking D-Bus
    # single-instance probe" from "reaches iced and renders nothing".
    m15b_drv = session_data("m15b-iced")
    if os.path.exists(m15b_drv):
        bin_files.append(("m15b-iced", m15b_drv, 0o100755))
    # m17-census — the guest half of artifacts/m17_census.py. Brings the session
    # up the normal way but starts busd ITSELF, so that busd's stderr is a file
    # of its own: `busd::peers: unknown destination: <name>` is the census, and
    # dbus-run-session would otherwise hand busd the same fd every other
    # component writes to, where two writers keep independent offsets and
    # overwrite each other. Then re-runs each autostarted single-instance
    # component by hand, each with its own stderr, because the stderr byte count
    # separates "blocked in the D-Bus probe" from "ran".
    m17_drv = session_data("m17-census")
    if os.path.exists(m17_drv):
        bin_files.append(("m17-census", m17_drv, 0o100755))

    # m20-term — the guest half of artifacts/m20_term.py: first light for
    # cosmic-term inside a real COSMIC session. Launches it OUTSIDE
    # cosmic-session, because cosmic-session pipes child stderr and reads it
    # nowhere (cosmic-session/src/comp.rs:122-134), and every documented way
    # cosmic-term dies — the pw_uid assert, the F_SETFL assert, die!() on
    # TIOCSCTTY or TIOCSWINSZ — announces itself only there.
    m20_drv = session_data("m20-term")
    if os.path.exists(m20_drv):
        bin_files.append(("m20-term", m20_drv, 0o100755))

    # m4-vkwl-a64 — the same driver with the compositor choice and every wait
    # made an argument, for aarch64 under TCG where "the session never came up"
    # and "the WSI chain does not work here" have to be told apart and a silent
    # timeout tells you neither. Staged exactly like m4-vkwl above; absent from
    # the artifact tree it is simply not packed.
    m4_drv_a64 = session_data("m4-vkwl-a64")
    if os.path.exists(m4_drv_a64):
        bin_files.append(("m4-vkwl-a64", m4_drv_a64, 0o100755))

    # /bin/sh -> brush (hardlinked; add_files_to_dir dedupes by host path).
    # Shell scripts (start-cosmic-leandros, dbus-run-session) are still invoked
    # as `sh <script>` by init/greetd — that predates the kernel's `#!` support
    # and keeps working; scripts with a `#!/bin/sh` line now also exec directly.
    _brush_p = os.path.join(os.environ.get("LEANDROS_BRUSH_DIR", "../brush"), "target", brush_target, "release", "brush")
    if os.path.exists(_brush_p):
        bin_files.append(("sh", _brush_p, 0o100755))

    # brush-nofix — the SAME brush, built against UNPATCHED crossterm 0.29.0.
    #
    # Item 17's fix lives in a sibling fork (`../crossterm`) wired in through
    # brush's `[patch.crates-io]`, so once it is built there is no way to see
    # the old behaviour again except by rebuilding — and "output is visible
    # now" measured against a remembered screenshot from the OTHER machine is
    # not a measurement, it is a recollection. Staging both binaries lets the
    # broken and the fixed shell be run back to back in ONE boot, against one
    # image, inside one cosmic-term window, so the difference cannot be
    # attributed to arch, image, timing or emulator state.
    #
    # Purely diagnostic and entirely optional: absent from the build tree it is
    # simply not packed, exactly like the m4-vkwl/m20-term drivers above.
    _brush_nofix_p = f"../brush-nofix-src/target/{brush_target}/release/brush"
    if os.path.exists(_brush_nofix_p):
        bin_files.append(("brush-nofix", _brush_nofix_p, 0o100755))

    # libpipewire-0.3 stub (inert) -> /usr/lib, resolved by soname for the
    # settings-daemon's DT_NEEDED. Same soname trick as the GL/input libs.
    # ports/portal builds a SUPERSET of that stub (same 0/NULL bodies, plus the
    # pw_* the portal frontend imports); prefer it when staged, since the
    # image's copy of a soname always wins over a port tree's.
    m6_pw_lib = os.path.expanduser(f"~/code/leandros-artifacts/pipewire-gap/lib/{arch}")
    _portal_pw = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                              "..", "ports", "portal", "out", arch, "usr", "lib")
    if os.path.isfile(os.path.join(_portal_pw, "libpipewire-0.3.so.0")):
        m6_pw_lib = os.path.normpath(_portal_pw)
    # ports/pipewire stages the REAL libpipewire 1.2.7 (same ABI the stub was
    # generated from); when it is staged the stub is not packed at all, and the
    # real library comes in with the rest of that tree (overlay further down).
    _pw_root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                             "..", "ports", "pipewire", "out", arch))
    _pw_real = (os.path.isfile(os.path.join(_pw_root, "usr", "bin", "pipewire"))
                and os.path.isfile(os.path.join(_pw_root, "usr", "lib", "libpipewire-0.3.so.0")))
    for so in ("libpipewire-0.3.so.0",):
        sp = f"{m6_pw_lib}/{so}"
        if os.path.exists(sp) and not _pw_real:
            usr_lib_files.append((so, sp, 0o100755))

    # Cosmic + hicolor icon themes -> /usr/share/icons/{Cosmic,hicolor}/… and the
    # default wallpaper -> the exact hardcoded fallback path cosmic-bg expects
    # (/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg). Icons are
    # soft-fail data at RUNTIME (a blank button, never a crash), which is exactly
    # what made the gap expensive at BUILD time: it was invisible until someone
    # photographed the greeter.
    #
    # Until 2026-09-25 this pulled a 94-file HAND-PRUNED subset from a host-only
    # artifacts tree (~/code/leandros-artifacts/m6-icons-pruned), staged only on
    # machines where someone had manually assembled that tree -- which is why one
    # box had ten applet icons and another had none of them (lane sessmisc,
    # 2026-09-24), and why cosmic-greeter's OWN buttons -- never part of that
    # hand-picked set -- rendered as empty circles no matter which machine built
    # the image. The greeter's [SCPATH] stat trace found ~750 failed icon-path
    # lookups per repaint (lane greeterlag, 2026-09-24): with the f2fs block cache
    # at its old 4 slots each one cost ~3 ms; fixed to 192 slots that dropped to
    # ~1% of the UI thread, but the lookups still failed, because most of the
    # requested names had no file to find at ANY cache size. Cross-referencing
    # cosmic-greeter's source (src/{greeter,locker,common}.rs) against what was
    # staged: object-select-symbolic, input-keyboard-symbolic,
    # system-{suspend,reboot,shutdown}-symbolic, system-users-symbolic,
    # application-menu-symbolic, applications-accessibility-symbolic, six
    # network-*-symbolic names and the cosmic-applet-battery-level-* family were
    # ALL absent from the pruned set (a live screendump showed exactly 5 of the
    # greeter's 7 button icons blank, matching this list one for one).
    #
    # Fix: stage the actual upstream theme SOURCE, not a hand-picked subset, split
    # across Cosmic and hicolor exactly as each project's own justfile/Makefile
    # installs it (checked against cosmic-icons/justfile, cosmic-applets/justfile
    # `_install_icons` -> $prefix/share/icons/hicolor, cosmic-launcher and
    # cosmic-applibrary's nested data/icons/justfile -> hicolor/scalable/apps,
    # cosmic-workspaces-epoch's Makefile -> hicolor/scalable/apps, cosmic-term's
    # justfile -> hicolor/<size>/apps). Getting this split right matters: a panel
    # button WEARS ITS TARGET'S ICON (cosmic-panel-button/src/lib.rs:215-244), so
    # com.system76.Cosmic{Launcher,AppLibrary,Workspaces} silently painting zero
    # pixels looks exactly like "the applet never started" (see the comment on
    # m6_session_bins above) -- the old pruned set covered these ten by accident
    # of where someone put them, and staging only cosmic-icons here would have
    # REGRESSED all ten (caught by the Icon= cross-check below before this
    # landed: 1 pre-existing miss became 10 on the first build with only Cosmic
    # staged). Sourced from the SAME cosmic-epoch checkout as the
    # /usr/share/cosmic config tree below (same _find_cosmic_epoch() lookup
    # order: $LEANDROS_COSMIC_EPOCH; a sibling of this checkout; a sibling of the
    # main checkout; ~/code/cosmic-epoch) so every machine resolves the identical
    # source instead of depending on who happened to hand-copy an artifacts tree
    # where. Fails loudly (SystemExit) if cosmic-icons is missing there -- a
    # silent skip is exactly the bug this replaces, and blank buttons must not go
    # undiagnosed a second time.
    cosmic_epoch_icons = _find_cosmic_epoch()
    _icon_seen = {}      # (theme, image_dir, fn) -> source root; first wins
    _icon_file_count = 0
    _icon_byte_count = 0

    def _stage_icon_tree(src, theme, prefix=""):
        """Walk `src`, staging every file under /usr/share/icons/<theme>/<prefix>/<relpath>."""
        nonlocal _icon_file_count, _icon_byte_count
        if not os.path.isdir(src):
            return 0
        n = 0
        for dirpath, _dn, filenames in os.walk(src):
            rel = os.path.relpath(dirpath, src)
            sub = "" if rel == "." else "/" + rel
            image_dir = f"/usr/share/icons/{theme}/{prefix}{sub}" if prefix else f"/usr/share/icons/{theme}{sub}"
            parts = image_dir.lstrip("/").split("/")
            for i in range(2, len(parts) + 1):
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if not os.path.isfile(hp):
                    continue
                key = (theme, image_dir, fn)
                if key in _icon_seen:
                    continue
                _icon_seen[key] = src
                m4_share_files.append((image_dir, fn, hp))
                _icon_file_count += 1
                _icon_byte_count += os.path.getsize(hp)
                n += 1
        return n

    def _stage_icon_file(hp, theme, image_dir):
        nonlocal _icon_file_count, _icon_byte_count
        if not os.path.isfile(hp):
            return 0
        fn = os.path.basename(hp)
        key = (theme, image_dir, fn)
        if key in _icon_seen:
            return 0
        parts = image_dir.lstrip("/").split("/")
        for i in range(2, len(parts) + 1):
            m4_share_dirs.add("/" + "/".join(parts[:i]))
        _icon_seen[key] = hp
        m4_share_files.append((image_dir, fn, hp))
        _icon_file_count += 1
        _icon_byte_count += os.path.getsize(hp)
        return 1

    # Cosmic theme: cosmic-icons' own freedesktop/ and extra/ scalable trees,
    # both merged into Cosmic/scalable/… (extra winning the handful of
    # same-named files, matching cosmic-icons/justfile's install order).
    _cosmic_freedesktop = os.path.join(cosmic_epoch_icons, "cosmic-icons", "freedesktop", "scalable")
    if not os.path.isdir(_cosmic_freedesktop):
        sys.exit(
            f"mkfs: {os.path.join(cosmic_epoch_icons, 'cosmic-icons')} is missing "
            f"(no freedesktop/scalable) -- the Cosmic icon theme cannot be staged "
            f"and the greeter/panel/applet buttons that depend on it would render "
            f"blank (this is the 2026-09-25 icons-lane bug, do not silently skip "
            f"it again). Set LEANDROS_COSMIC_EPOCH, or check out cosmic-icons "
            f"beside cosmic-epoch at {cosmic_epoch_icons} (a data-only subset is "
            f"enough -- see README.subset next to it).")
    _n = _stage_icon_tree(_cosmic_freedesktop, "Cosmic", "scalable")
    _n += _stage_icon_tree(os.path.join(cosmic_epoch_icons, "cosmic-icons", "extra", "scalable"),
                           "Cosmic", "scalable")
    print(f"  Cosmic icon theme (cosmic-icons): {_n} file(s) from {cosmic_epoch_icons}")

    # Cosmic's own index.theme (Inherits=Pop,hicolor -- Pop is not staged; a
    # missing inherited theme is a spec-legal no-op for freedesktop-icons
    # resolution, not an error). Fail loudly rather than silently shipping a
    # theme with no index (every lookup would miss regardless of what's on disk).
    _cosmic_index = os.path.join(cosmic_epoch_icons, "cosmic-icons", "index.theme")
    if not os.path.isfile(_cosmic_index):
        sys.exit(f"mkfs: {_cosmic_index} is missing -- cannot stage Cosmic/index.theme")
    m4_share_dirs.add("/usr/share/icons/Cosmic")
    m4_share_files.append(("/usr/share/icons/Cosmic", "index.theme", _cosmic_index))

    # hicolor: every per-component app/applet icon upstream installs there
    # directly (NOT into Cosmic -- see the comment above). Each is optional here
    # (a missing one only re-blanks that one button, caught by the Icon=
    # cross-check below) except the three panel-button LAUNCH TARGETS, which are
    # load-bearing for the dock/panel and therefore fail loudly like Cosmic does.
    _hicolor_n0 = _icon_file_count
    _capp = os.path.join(cosmic_epoch_icons, "cosmic-applets")
    for _name in ("cosmic-app-list", "cosmic-applet-a11y", "cosmic-applet-audio",
                  "cosmic-applet-input-sources", "cosmic-applet-battery",
                  "cosmic-applet-bluetooth", "cosmic-applet-minimize",
                  "cosmic-applet-network", "cosmic-applet-notifications",
                  "cosmic-applet-power", "cosmic-applet-status-area",
                  "cosmic-applet-tiling", "cosmic-applet-time",
                  "cosmic-applet-workspaces", "cosmic-panel-app-button",
                  "cosmic-panel-launcher-button", "cosmic-panel-workspaces-button"):
        _stage_icon_tree(os.path.join(_capp, _name, "data", "icons"), "hicolor")
    # The three panel-button launch targets: a single flat {APPID}.svg installed
    # by a nested data/icons/justfile (launcher, app library) or a Makefile
    # (workspaces) straight to hicolor/scalable/apps -- load-bearing, so missing
    # ANY of the three fails loudly (a blank dock button is otherwise silent and
    # shaped exactly like success, per the comment above).
    _launch_targets = (
        (os.path.join(cosmic_epoch_icons, "cosmic-launcher", "data", "icons",
                      "com.system76.CosmicLauncher.svg"), "com.system76.CosmicLauncher"),
        (os.path.join(cosmic_epoch_icons, "cosmic-applibrary", "data", "icons",
                      "com.system76.CosmicAppLibrary.svg"), "com.system76.CosmicAppLibrary"),
        (os.path.join(cosmic_epoch_icons, "cosmic-workspaces-epoch", "data",
                      "com.system76.CosmicWorkspaces.svg"), "com.system76.CosmicWorkspaces"),
    )
    for _hp, _appid in _launch_targets:
        if not os.path.isfile(_hp):
            sys.exit(
                f"mkfs: {_hp} is missing -- {_appid} is a panel-button LAUNCH "
                f"TARGET (cosmic-panel-button wears the target's icon, not its "
                f"own); without it the dock/panel button paints zero pixels and "
                f"looks exactly like the applet never started.")
        _stage_icon_file(_hp, "hicolor", "/usr/share/icons/hicolor/scalable/apps")
    # cosmic-term ships pre-rendered SVGs under its own hicolor/<size>/apps/
    # tree (not scalable) -- stage it as-is, same relative layout.
    _stage_icon_tree(os.path.join(cosmic_epoch_icons, "cosmic-term", "res", "icons", "hicolor"),
                     "hicolor")
    # cosmic-settings: same pre-rendered <size>/apps layout, installed to
    # hicolor by cosmic-settings/justfile. Its .desktop file
    # (com.system76.CosmicSettings, staged from m6-session-data/shared) is what
    # puts Settings in the launcher and the app library; without this icon the
    # entry is listed with a generic placeholder. Taken from the source tree the
    # staged binary is built from first (m6-session-bins/src), then cosmic-epoch:
    # the desktop's cosmic-epoch is a data-only subset with no cosmic-settings.
    for _src in (os.path.expanduser("~/code/leandros-artifacts/m6-session-bins/src/"
                                    "cosmic-settings/resources/icons"),
                 os.path.join(cosmic_epoch_icons, "cosmic-settings", "resources", "icons")):
        if os.path.isdir(_src):
            _stage_icon_tree(_src, "hicolor")
            break
    # icons of the apps staged by ports/cosmic-apps (see above)
    for _d in cosapps_icon_dirs:
        _stage_icon_tree(_d, "hicolor")
    # cosmic-applet-time's .desktop asks for Icon=application-default-icon, a
    # name no upstream icon theme ships (cosmic-icons has application-default).
    # Stage the upstream application-default.svg under the requested name in
    # hicolor so the lookup resolves instead of warning on every panel start.
    _adi = os.path.join(cosmic_epoch_icons, "cosmic-icons", "extra", "scalable", "apps",
                        "application-default.svg")
    if os.path.isfile(_adi):
        _adi_dir = "/usr/share/icons/hicolor/scalable/apps"
        m4_share_files.append((_adi_dir, "application-default-icon.svg", _adi))
        _icon_file_count += 1
        _icon_byte_count += os.path.getsize(_adi)
    print(f"  hicolor icon theme (per-component app/applet icons): "
          f"{_icon_file_count - _hicolor_n0} file(s)")

    # hicolor's own index.theme: minimal but REAL (not index-only) -- cosmic-term
    # stages real files into fixed 16..256 apps/ dirs and the applets above into
    # scalable/{apps,status}, and freedesktop-icons needs the matching
    # Directories/Type/Size sections to find them, not just the directories on
    # disk. Pop is also inherited by Cosmic but not staged; that is a spec-legal
    # no-op, not something hicolor needs to cover.
    m4_share_dirs.add("/usr/share/icons/hicolor")
    _hicolor_index = (
        b"[Icon Theme]\n"
        b"Name=Hicolor\n"
        b"Comment=Fallback icon theme\n"
        b"Hidden=true\n"
        b"Directories=16x16/apps,24x24/apps,32x32/apps,48x48/apps,64x64/apps,"
        b"128x128/apps,256x256/apps,scalable/apps,scalable/status\n"
        b"\n"
        b"[16x16/apps]\nSize=16\nContext=Applications\nType=Fixed\n\n"
        b"[24x24/apps]\nSize=24\nContext=Applications\nType=Fixed\n\n"
        b"[32x32/apps]\nSize=32\nContext=Applications\nType=Fixed\n\n"
        b"[48x48/apps]\nSize=48\nContext=Applications\nType=Fixed\n\n"
        b"[64x64/apps]\nSize=64\nContext=Applications\nType=Fixed\n\n"
        b"[128x128/apps]\nSize=128\nContext=Applications\nType=Fixed\n\n"
        b"[256x256/apps]\nSize=256\nContext=Applications\nType=Fixed\n\n"
        b"[scalable/apps]\nSize=48\nMinSize=8\nMaxSize=512\nContext=Applications\nType=Scalable\n\n"
        b"[scalable/status]\nSize=48\nMinSize=8\nMaxSize=512\nContext=Status\nType=Scalable\n"
    )
    m4_share_files.append(("/usr/share/icons/hicolor", "index.theme", _hicolor_index))
    print(f"  icon themes total: {_icon_file_count} file(s), "
          f"{_icon_byte_count / 1024 / 1024:.2f} MiB, from {cosmic_epoch_icons}")
    # `shared` is the one session-data entry that is a TREE, and that makes
    # session_data()'s whole-entry precedence the wrong rule for it. Resolving it
    # like a file means an artifacts tree that merely EXISTS shadows the repo copy
    # entirely, so every .desktop committed afterwards is invisible on that
    # machine — the same silent skip session_data() was written to kill, one level
    # up. It cost a session: the box's tree predates the applet .desktop files, so
    # its panel launched the clock stub alone while this machine drew all of them,
    # which reads as an x86_64-vs-aarch64 rendering bug and is nothing of the kind.
    #
    # The union below keeps per-FILE artifacts precedence (the m4-vkwl rule, which
    # is what the precedence was actually for) and takes the repo copy only where
    # the artifacts tree has no such path. Neither side can win wholesale, and
    # neither may: the wallpaper is gitignored and lives only in the artifacts
    # tree, the .desktop files are committed and travel with a merge.
    _shared_roots = []
    for _r in (os.path.expanduser("~/code/leandros-artifacts/m6-session-data/shared"),
               os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                             "..", "artifacts", "m6-session-data", "shared"))):
        if os.path.isdir(_r) and _r not in _shared_roots:
            _shared_roots.append(_r)
    _shared_seen = {}                             # (image_dir, fn) -> root that supplied it
    for _root in _shared_roots:                   # artifacts tree first, repo second
        for dirpath, _dn, filenames in os.walk(_root):
            rel = os.path.relpath(dirpath, _root)  # e.g. "usr/share/backgrounds/cosmic"
            if rel == ".":
                continue
            parts = rel.split("/")
            for i in range(2, len(parts) + 1):
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            image_dir = "/" + rel
            for fn in sorted(filenames):
                if fn.endswith(".orig"):          # keep the placeholder off-image
                    continue
                hp = os.path.join(dirpath, fn)
                if not os.path.isfile(hp):
                    continue
                # A duplicate would pack two directory entries of the same name
                # into one inode, so the shadowed copy is dropped, not appended.
                if (image_dir, fn) in _shared_seen:
                    continue
                # The real Time applet supersedes the leandros-applet stand-in's
                # .desktop of the same name.
                if real_time_applet and fn == "com.system76.CosmicAppletTime.desktop":
                    continue
                _shared_seen[(image_dir, fn)] = _root
                m4_share_files.append((image_dir, fn, hp))
    if len(_shared_roots) > 1:
        _from_repo = sorted(f"{d}/{n}" for (d, n), r in _shared_seen.items()
                            if r == _shared_roots[1])
        if _from_repo:
            print(f"  session-data/shared: {len(_from_repo)} file(s) taken from the "
                  f"repo copy, absent from the artifacts tree:")
            for _p in _from_repo:
                print(f"    {_p}")

    # COSMIC system-default config tree -> /usr/share/cosmic/<component>/v<N>/<key>
    #
    # cosmic-config resolves a component's system defaults as
    #     system_path = xdg::BaseDirectories::with_prefix("cosmic")
    #                       .find_data_file("<name>/v<version>")
    # (libcosmic cosmic-config/src/lib.rs:203,236). find_data_file walks
    # XDG_DATA_HOME then XDG_DATA_DIRS and returns the first path that EXISTS,
    # so what it is really testing for is the DIRECTORY
    # /usr/share/cosmic/<name>/v<N>/. Each config key inside it is a BARE,
    # EXTENSIONLESS FILE whose entire content is one RON value: get_system_default
    # is `system_path.join(key)` -> read_to_string -> ron::from_str (:481-487).
    # If the directory is missing, system_path is None and EVERY key lookup
    # returns Error::NoConfigDirectory — which Error::is_err() reports as *not*
    # an error (:120-123), so callers swallow it quietly.
    #
    # Two of those quiet swallows are why the desktop had no working keyboard:
    #   * `defaults` (from cosmic-comp/data/keybindings.ron) IS the entire
    #     keybinding table. shortcuts::shortcuts() falls back to
    #     Shortcuts::default(), an EMPTY HashMap (cosmic-settings-daemon/config/
    #     src/shortcuts/mod.rs:35-38), so without this file cosmic-comp has no
    #     key bindings at all — not merely no system actions.
    #   * `system_actions` maps Action::System(..) to a command line. Handling is
    #     `if let Some(command) = ...system_actions.get(&system)` (cosmic-comp/
    #     src/input/actions.rs:1016-1021), so an empty map makes every system
    #     binding a silent no-op. cosmic-launcher, cosmic-app-library and
    #     cosmic-workspaces are all staged and spawned every boot, and the only
    #     thing that can raise them is such an action.
    # cosmic-panel's "Panel Entry Error: NoConfigDirectory" is the same cause on
    # com.system76.CosmicPanel{,.Panel,.Dock}: container_config.rs:116 reads the
    # `entries` key, then one Config per entry, then one get() per struct field.
    #
    # Sourced from the checked-out ../cosmic-epoch sibling (submodules pinned at
    # epoch-1.3.0), reproducing exactly what upstream's own install rules place
    # under $prefix/share/cosmic — better provenance than the unversioned
    # ~/code/leandros-artifacts staging dirs the rest of the session ship uses.
    cosmic_epoch = _find_cosmic_epoch()

    def _stage_cosmic_default(rel, hostpath):
        """rel is "<component>/v<N>/<key>"; register every ancestor directory."""
        image_dir = "/usr/share/cosmic/" + os.path.dirname(rel)
        parts = image_dir.lstrip("/").split("/")
        for i in range(2, len(parts) + 1):     # /usr is static (ino 15)
            m4_share_dirs.add("/" + "/".join(parts[:i]))
        m4_share_files.append((image_dir, os.path.basename(rel), hostpath))

    # Recursive installs. These source trees are already laid out as
    # <component>/v<N>/<key>, and upstream copies them wholesale
    # (`find ... -exec install -Dm0644`), so no renaming is involved.
    #   cosmic-panel/justfile:47-49            45 files (Panel + Dock + entries)
    #   cosmic-applets/justfile:16,40-41        2 files (CosmicAppList)
    #   cosmic-settings/justfile:8,36         211 files (themes + CosmicComp)
    #   cosmic-bg/data/justfile:4-7             2 files, under its own APPID
    for _src, _prefix in (
        (f"{cosmic_epoch}/cosmic-panel/data/default_schema", ""),
        (f"{cosmic_epoch}/cosmic-applets/cosmic-app-list/data/default_schema", ""),
        (f"{cosmic_epoch}/cosmic-settings/resources/default_schema", ""),
        (f"{cosmic_epoch}/cosmic-bg/data/v1", "com.system76.CosmicBackground/v1"),
    ):
        if not os.path.isdir(_src):
            continue
        for dirpath, _dn, filenames in os.walk(_src):
            _sub = os.path.relpath(dirpath, _src)
            if _sub == ".":
                _base = _prefix
            elif _prefix:
                _base = _prefix + "/" + _sub
            else:
                _base = _sub
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp) and _base:
                    _stage_cosmic_default(f"{_base}/{fn}", hp)

    # cosmic-panel's CosmicPanelConfig has two fields the epoch-1.3.0
    # default_schema does not ship (panel_config.rs:461,464). A missing key
    # makes CosmicPanelContainerConfig::load() fail, and main.rs then calls
    # write_entries(), which rewrites every panel key of both entries into
    # ~/.config/cosmic before the panel even connects to Wayland: 9-14 s on
    # x86_64 KVM virgl (lane panellate), so the Panel/Dock layer surfaces
    # mapped after the first terminal and stole its keyboard focus. Ship the
    # code defaults (panel_config.rs:521-522) for any entry lacking them.
    import tempfile as _tf
    _gen = _tf.mkdtemp(prefix="cosmic-panel-defaults-")
    for _entry in ("Panel", "Dock"):
        _sd = f"{cosmic_epoch}/cosmic-panel/data/default_schema/com.system76.CosmicPanel.{_entry}/v1"
        if not os.path.isdir(_sd):
            continue
        for _key, _val in (("padding_overlap", "0.5"), ("keep_style_on_maximize", "false")):
            if os.path.exists(f"{_sd}/{_key}"):
                continue
            _hp = os.path.join(_gen, f"{_entry}-{_key}")
            with open(_hp, "w") as _f:
                _f.write(_val)
            _stage_cosmic_default(f"com.system76.CosmicPanel.{_entry}/v1/{_key}", _hp)

    # The three files upstream installs one at a time, each RENAMED: the ".ron"
    # suffix is stripped, and keybindings.ron additionally becomes "defaults".
    #   cosmic-comp/Makefile:26-27,55-57
    #   cosmic-settings-daemon/Makefile:22,36
    for _src, _rel in (
        (f"{cosmic_epoch}/cosmic-comp/data/keybindings.ron",
         "com.system76.CosmicSettings.Shortcuts/v1/defaults"),
        (f"{cosmic_epoch}/cosmic-settings-daemon/data/system_actions.ron",
         "com.system76.CosmicSettings.Shortcuts/v1/system_actions"),
        (f"{cosmic_epoch}/cosmic-comp/data/tiling-exceptions.ron",
         "com.system76.CosmicSettings.WindowRules/v1/tiling_exception_defaults"),
    ):
        if os.path.isfile(_src):
            _stage_cosmic_default(_rel, _src)

    # LOUD, not silent. Every lookup above is gated on isdir/isfile, so a host
    # without the cosmic-epoch checkout used to produce an image with NO
    # /usr/share/cosmic at all -- and therefore a desktop with an empty
    # keybinding table (Super+T, Super, Super+F9 all silently dead) and
    # `NoConfigDirectory` in every shortcuts lookup. That is exactly what both
    # Linux hosts shipped until 2026-09-24: the path was hardcoded to
    # ~/code/cosmic-epoch, which exists only on the Mac. The keybinding table
    # is not optional, so refuse to build without it.
    _staged = {os.path.join(d, f) for d, f, _h in m4_share_files}
    _required = (
        "/usr/share/cosmic/com.system76.CosmicSettings.Shortcuts/v1/defaults",
        "/usr/share/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions",
    )
    _missing = [r for r in _required if r not in _staged]
    if _missing:
        raise SystemExit(
            "ERROR: COSMIC system defaults not staged -- the desktop would have "
            "no keybindings.\n  missing: " + ", ".join(_missing) +
            f"\n  cosmic-epoch looked up at: {cosmic_epoch}"
            "\n  Check out ../cosmic-epoch (epoch-1.3.0) next to the repo, or set "
            "LEANDROS_COSMIC_EPOCH=<path>.")
    print(f"  COSMIC system defaults: {sum(1 for d, _f, _h in m4_share_files if d.startswith('/usr/share/cosmic/'))} "
          f"file(s) from {cosmic_epoch}")

    # v2 theme schema is missing `list_button`, and that ONE absent file makes the
    # whole system theme fail to load in EVERY libcosmic process.
    #
    # libcosmic (the applets' pin, 511384f) declares cosmic-theme's `Theme` as
    # `#[version = 2]` and its struct still carries a `list_button` field, so
    # cosmic-config reads `com.system76.CosmicTheme.<Dark|Light>/v2/list_button`.
    # Upstream's epoch-1.3.0 schema ships 30 keys in v1 (list_button among them)
    # and 37 in v2 (list_button NOT among them -- v2 adds alpha_map, frosted*,
    # transparent*, and drops list_button/is_frosted). The read therefore returns
    # ENOENT and `Theme::get_entry` fails the whole entry, which every libcosmic
    # app reports once per start as
    #     ERROR cosmic::theme: error loading system dark theme
    #     error=GetKey("list_button", Os { code: 2, kind: NotFound, ... })
    # It is a version skew between the applets' libcosmic pin and the epoch-1.3.0
    # data, not a staging mistake -- the file is correctly absent upstream.
    #
    # Fix on our side (packaging only, no COSMIC source change): carry v1's value
    # forward into v2. Verified on aarch64 -- the `list_button` error goes 1 -> 0
    # per libcosmic process and `error loading system dark theme` disappears
    # entirely from a full session log.
    #
    # SCOPE, stated because it is easy to over-read: this fixes the THEME LOAD.
    # It was tested against the blank Dock applet icons and did NOT fix them --
    # the Dock is still empty with the theme loading cleanly, so that is a
    # separate defect and this comment is not evidence about it.
    for _theme in ("com.system76.CosmicTheme.Dark", "com.system76.CosmicTheme.Light"):
        _v1 = f"{cosmic_epoch}/cosmic-settings/resources/default_schema/{_theme}/v1/list_button"
        _v2_dir = f"{cosmic_epoch}/cosmic-settings/resources/default_schema/{_theme}/v2"
        if os.path.isfile(_v1) and os.path.isdir(_v2_dir):
            if os.path.exists(os.path.join(_v2_dir, "list_button")):
                print(f"  NOTE: {_theme}/v2/list_button now exists upstream; "
                      f"the compatibility copy in mkfs is redundant and can go")
            else:
                _stage_cosmic_default(f"{_theme}/v2/list_button", _v1)

    # ── Firefox (optional, ports/firefox) ────────────────────────────────────
    # Alpine 3.21's prebuilt Firefox + its GTK3 closure + fonts, staged by
    # ports/firefox/build.sh (scripts/build-all.sh runs it when docker/podman
    # is available) as a rootfs-shaped tree. Optional like doom/MAME: absent
    # output, nothing staged. The tree is overlaid onto the image as-is, with
    # two rules:
    #   * usr/lib/<soname> joins usr_lib_files, but a soname the image already
    #     packs keeps the IMAGE's copy (libffi, libexpat, libz, libzstd,
    #     libxkbcommon, libpixman, libleandros_ssp...): those are shared with
    #     the desktop and the Mesa ship-set and must not change underneath it.
    #     The port's symbol audit (out/<arch>/SYMCHECK.txt) is computed against
    #     exactly these image copies.
    #   * any other path joins the /usr + /etc tree tables; files with an
    #     execute bit on the host (Firefox's ELFs) are packed 0755, the rest
    #     0644. A path some earlier ship set already staged wins, again.
    # /bin/firefox (the launcher) and the LeandrOS default prefs come straight
    # from the tracked port sources, so editing them needs no container run.
    def _overlay_port_tree(_root):
        """Overlay a ports/<name>/out/<arch> rootfs-shaped tree (firefox, portal).

        Returns (files, bytes, image_won). Rules in the comment above."""
        _lib_names = {n for n, _p, _m in usr_lib_files}
        _taken = ({(d, n) for d, n, _h in m4_share_files}
                  | {(d, n) for d, n, _h in m5_exec_files})
        _won, _n, _bytes = [], 0, 0
        for _dirpath, _dn, _filenames in os.walk(_root):
            _rel = os.path.relpath(_dirpath, _root)
            if _rel == ".":
                continue                     # CLOSURE.txt, SYMCHECK.txt, .stamp: host-side only
            _image_dir = "/" + _rel
            for _fn in sorted(_filenames):
                _hp = os.path.join(_dirpath, _fn)
                if not os.path.isfile(_hp):
                    continue
                # A symlink to a staged file (git's ~150 libexec/git-core
                # entries -> ../../bin/git, pw-play -> pw-cat) is packed under
                # its target's host path, which add_files_to_dir turns into a
                # hard link to the one inode instead of another full copy.
                if os.path.islink(_hp):
                    _hp = os.path.realpath(_hp)
                if _image_dir == "/usr/lib":
                    if _fn in _lib_names:
                        _won.append(_fn)
                        continue
                    usr_lib_files.append((_fn, _hp, 0o100755))
                else:
                    if (_image_dir, _fn) in _taken:
                        _won.append(f"{_image_dir}/{_fn}")
                        continue
                    _parts = _rel.split("/")
                    for _i in range(1, len(_parts) + 1):
                        m4_share_dirs.add("/" + "/".join(_parts[:_i]))
                    if os.stat(_hp).st_mode & 0o111:
                        m5_exec_files.append((_image_dir, _fn, _hp))
                    else:
                        m4_share_files.append((_image_dir, _fn, _hp))
                _n += 1
                _bytes += os.path.getsize(_hp)
        return _n, _bytes, _won

    # ── PipeWire + WirePlumber (optional, ports/pipewire) ────────────────────
    # Overlaid FIRST so its real libpipewire-0.3.so.0 wins over the inert stub
    # inside ports/portal's tree (first staged copy of a soname wins). Absent
    # output: the stub above is packed instead and audio stays kernel-only.
    if _pw_real:
        _pw_n, _pw_bytes, _pw_won = _overlay_port_tree(_pw_root)
        print(f"  PipeWire: {_pw_n} file(s), {_pw_bytes // (1024 * 1024)} MiB from {_pw_root}; "
              f"image copy kept for {len(_pw_won)}: {' '.join(sorted(_pw_won))}")
    else:
        print(f"  (no PipeWire staged: {_pw_root} missing — ports/pipewire/build.sh {arch}; "
              f"packing the inert libpipewire stub)")

    # ── xdg-desktop-portal (optional, ports/portal) ──────────────────────────
    # org.freedesktop.portal.Desktop: Alpine 3.21's frontend + permission store
    # (+ GLib closure) and xdg-desktop-portal-cosmic, with their busd .service
    # files and cosmic.portal / portals.conf, staged by ports/portal/build.sh.
    # Same overlay rules as Firefox. Optional: absent output, nothing staged,
    # and the session keeps its "portal ServiceUnknown" warnings.
    _xdp_root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                              "..", "ports", "portal", "out", arch))
    if (os.path.isfile(os.path.join(_xdp_root, "usr", "libexec", "xdg-desktop-portal"))
            and os.path.isfile(os.path.join(_xdp_root, "usr", "libexec", "xdg-desktop-portal-cosmic"))):
        _xdp_n, _xdp_bytes, _xdp_won = _overlay_port_tree(_xdp_root)
        print(f"  xdg-desktop-portal: {_xdp_n} file(s), {_xdp_bytes // (1024 * 1024)} MiB "
              f"from {_xdp_root}; image copy kept for {len(_xdp_won)}: {' '.join(sorted(_xdp_won))}")
    else:
        print(f"  (no xdg-desktop-portal staged: {_xdp_root} missing — ports/portal/build.sh {arch})")

    # ── GStreamer (optional, ports/gstreamer) ───────────────────────────────
    # Alpine 3.21's GStreamer 1.24 + plugins-base/good + gst-libav + the
    # PipeWire plugin (cosmic-player's media stack), gst-launch/inspect, and a
    # short test clip. Shared libs (GLib, FFmpeg) are byte-identical to the
    # Firefox/PipeWire ports' copies, so overlaying them is free. Same overlay rules.
    _gst_root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                              "..", "ports", "gstreamer", "out", arch))
    if os.path.isfile(os.path.join(_gst_root, "usr", "lib", "libgstreamer-1.0.so.0")):
        _gst_n, _gst_bytes, _gst_won = _overlay_port_tree(_gst_root)
        print(f"  GStreamer: {_gst_n} file(s), {_gst_bytes // (1024 * 1024)} MiB "
              f"from {_gst_root}; image copy kept for {len(_gst_won)}: {' '.join(sorted(_gst_won))}")
    else:
        print(f"  (no GStreamer staged: {_gst_root} missing — ports/gstreamer/build.sh {arch})")

    # ── libflatpak (optional, ports/libflatpak) ─────────────────────────────
    # libflatpak.so.0 + closure (ostree, json-glib, libarchive, gpgme, ...) so
    # cosmic-store's Flatpak backend starts without `apk add flatpak`. Same overlay rules.
    _lfp_root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                              "..", "ports", "libflatpak", "out", arch))
    if os.path.isfile(os.path.join(_lfp_root, "usr", "lib", "libflatpak.so.0")):
        _lfp_n, _lfp_bytes, _lfp_won = _overlay_port_tree(_lfp_root)
        print(f"  libflatpak: {_lfp_n} file(s), {_lfp_bytes // (1024 * 1024)} MiB "
              f"from {_lfp_root}; image copy kept for {len(_lfp_won)}: {' '.join(sorted(_lfp_won))}")
    else:
        print(f"  (no libflatpak staged: {_lfp_root} missing — ports/libflatpak/build.sh {arch})")

    # ── e2fsprogs (optional, ports/e2fsprogs) ─────────────────────────────────
    # Alpine's e2fsck/resize2fs/dumpe2fs/e2undo/mke2fs in /usr/sbin (+ closure
    # in /usr/lib, /etc/mke2fs.conf). Also hard-linked into /bin (the console
    # login's PATH is /bin alone).
    _e2_root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                             "..", "ports", "e2fsprogs", "out", arch))
    if os.path.isfile(os.path.join(_e2_root, "usr", "sbin", "e2fsck")):
        _e2_n, _e2_bytes, _e2_won = _overlay_port_tree(_e2_root)
        for _name in ("e2fsck", "resize2fs", "dumpe2fs", "e2undo", "mke2fs"):
            _p = os.path.realpath(os.path.join(_e2_root, "usr", "sbin", _name))
            if os.path.isfile(_p):
                bin_files.append((_name, _p, 0o100755))
        print(f"  e2fsprogs: {_e2_n} file(s), {_e2_bytes // 1024} KiB from {_e2_root}")
    else:
        print(f"  (no e2fsprogs staged: {_e2_root} missing — ports/e2fsprogs/build.sh {arch})")

    # ── Vanilla Alpine packages (ports/alpine/<arch>.lock) ───────────────────
    # Firefox, git, ripgrep, dosfstools, xfsprogs, musl, apk-tools, ... are the
    # unmodified Alpine packages from the lock cache (scripts/alpine-lock.py
    # fetch). They are staged host-side with their own /lib/apk/db records;
    # leandros-base owns everything else. A path some other ship set also
    # staged is dropped in favour of the package's (apk-shadow below).
    import alpinepkgs, filecmp
    _ap_entries, _ap_stanzas, _ap_pkgs = alpinepkgs.stage(arch)
    # Recovery copy of Alpine's loader: a distinct host file (own inode, not a hard
    # link to the apk-owned one) so a damaged /lib/ld-musl-<arch>.so.1 can be
    # replaced from busybox.static: cp /lib/ld-musl-<arch>.so.1.recovery ...
    _ld_path = f"/lib/ld-musl-{arch}.so.1"
    _ld_ent = next((e for e in _ap_entries if e["path"] == _ld_path and e.get("kind") == "file"), None)
    if _ld_ent is None:
        sys.exit(f"{_ld_path} is not a regular file of the musl package in ports/alpine/{arch}.lock")
    import shutil, tempfile
    _rec_dir = tempfile.mkdtemp(prefix="leandros-ldrec-")
    _rec = os.path.join(_rec_dir, os.path.basename(_ld_path) + ".recovery")
    shutil.copyfile(_ld_ent["host"], _rec)
    lib_files.append((f"ld-musl-{arch}.so.1.recovery", _rec, 0o100755))
    _ap_gen, _ap_gen_dirs = alpinepkgs.generated_entries(arch)
    _apk_root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                              "..", "ports", "apk", "out", arch))
    if not os.path.isfile(os.path.join(_apk_root, "usr", "sbin", "apk")):
        sys.exit(f"{_apk_root} missing (ports/apk/build.sh {arch})")
    _overlay_port_tree(_apk_root)
    bin_files.append(("apk", os.path.join(_apk_root, "usr", "sbin", "apk"), 0o100755))
    _ap_world = ["leandros-base", "musl=" + alpinepkgs.lk.read_lock(arch)[0]["musl"]["ver"]]

    _ap_world += [n for n in alpinepkgs.world_names(arch) if n != "musl"]
    m4_share_dirs.add("/etc/apk")
    # Alpine appstream snapshot for cosmic-store (ports/appstream-alpine/build.sh):
    # /var/lib/swcatalog/xml/alpinelinux-<branch>-<repo>.xml.gz and the 64x64
    # icons under icons/alpinelinux-<branch>-<repo>/64x64/, the layout of
    # alpine-appstream-downloader. Missing = no catalog (store shows no
    # Alpine apps), so it is a warning, not a refusal.
    _as_out = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                            "..", "ports", "appstream-alpine", "out", arch))
    _as_branch = alpinepkgs.lk.REL
    _as_n = 0
    for _repo in ("main", "community"):
        _x = os.path.join(_as_out, f"Components-{_repo}.xml.gz")
        _i = os.path.join(_as_out, f"icons-{_repo}.tar.gz")
        if not (os.path.isfile(_x) and os.path.isfile(_i)):
            print(f"  WARNING appstream {_repo}: {_x} missing (./ports/appstream-alpine/build.sh {arch})")
            continue
        _origin = f"alpinelinux-{_as_branch}-{_repo}"
        _idir = f"/var/lib/swcatalog/icons/{_origin}/64x64"
        for _d in ("/var", "/var/lib", "/var/lib/swcatalog", "/var/lib/swcatalog/xml",
                   "/var/lib/swcatalog/icons", f"/var/lib/swcatalog/icons/{_origin}", _idir):
            m4_share_dirs.add(_d)
        m4_share_files.append(("/var/lib/swcatalog/xml", f"{_origin}.xml.gz", _x))
        import tarfile as _tf
        with _tf.open(_i) as _t:
            for _m in _t.getmembers():
                if _m.isfile() and "/" not in _m.name.lstrip("./"):
                    m4_share_files.append((_idir, _m.name.lstrip("./"), _t.extractfile(_m).read()))
                    _as_n += 1
    if _as_n:
        print(f"appstream: {_as_branch} catalog + {_as_n} icons staged in /var/lib/swcatalog")
    m4_share_files.append(("/etc/apk", "world", ("\n".join(_ap_world) + "\n").encode()))
    _ap_rel = alpinepkgs.lk.REL
    m4_share_files.append(("/etc/apk", "repositories", "".join(
        f"{alpinepkgs.lk.MIRROR}/{_ap_rel}/{r}\n" for r in ("main", "community")).encode()))
    # LeandrOS files that sit beside the packages
    _ff_port = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ports", "firefox")
    bin_files.append(("firefox", os.path.join(_ff_port, "firefox.sh"), 0o100755))
    m4_share_dirs.update(("/usr/lib/firefox", "/usr/lib/firefox/defaults", "/usr/lib/firefox/defaults/pref",
                          "/usr/local", "/usr/local/lib", "/usr/local/share", "/usr/local/share/applications"))
    m4_share_files.append(("/usr/lib/firefox/defaults/pref", "leandros-prefs.js", os.path.join(_ff_port, "leandros-prefs.js")))
    # Exec=/bin/firefox keeps the LeandrOS environment; /usr/local/share shadows Alpine's entry.
    m4_share_files.append(("/usr/local/share/applications", "firefox.desktop", os.path.join(_ff_port, "firefox.desktop")))
    # gitconfig stays ours (ports/git/data)
    _gitconf = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ports", "git", "data", "gitconfig")
    if os.path.isfile(_gitconf):
        etc_files.append(("gitconfig", _gitconf, 0o100644))
    # /bin compat names (PATH on the console login is /bin alone); symlinks, unowned by the db.
    for _n in ("git", "git-upload-pack", "git-receive-pack", "git-upload-archive", "less", "nano", "rg"):
        bin_files.append((_n, f"/usr/bin/{_n}".encode(), S_IFLNK | 0o777))
    bin_files.append(("kmscube", b"/usr/bin/kmscube", S_IFLNK | 0o777))
    for _n in ("mkfs.fat", "mkfs.xfs"):
        bin_files.append((_n, f"/sbin/{_n}".encode(), S_IFLNK | 0o777))

    # apk-shadow: the choke point. After every ship set is appended, a non-apk
    # entry whose image path a package owns is dropped.
    # shadow-subids ships empty /etc/subuid|subgid; ours (etc_files) carry the rootless ranges
    _ap_entries = [e for e in _ap_entries if e["path"] not in ("/etc/subuid", "/etc/subgid")]
    _ap_owned = {e["path"]: e for e in _ap_entries if e["kind"] != "dir"}
    _deny = re.compile(r"libEGL|libGLES|libgbm|libgallium|libvulkan|libdrm|libpipewire|libgst|libext2fs")
    _sh = {"n": 0, "diff": 0}
    def _shadow(d, n, h):
        e = _ap_owned.get(("" if d == "/" else d) + "/" + n)
        if e is None:
            return False
        _sh["n"] += 1
        same = e["kind"] == "file" and isinstance(h, str) and filecmp.cmp(e["host"], h, shallow=False)
        if not same:
            _sh["diff"] += 1
            if _deny.search(n):
                sys.exit(f"apk-shadow: {d}/{n} differs from the Alpine package {e['pkg']}'s copy")
        return True
    for _d, _lst in (("/bin", bin_files), ("/lib", lib_files), ("/usr/lib", usr_lib_files),
                     ("/usr/lib/gbm", gbm_files), ("/", root_files), ("/etc", etc_files)):
        _lst[:] = [t for t in _lst if not _shadow(_d, t[0], t[1])]
    for _lst in (m4_share_files, m5_exec_files):
        _lst[:] = [t for t in _lst if not _shadow(t[0], t[1], t[2])]
    print(f"apk-shadow: {_sh['n']} dropped ({_sh['diff']} differ)")
    # the generated (trigger output) tar: base-owned, loses to packages and ship sets
    _base_paths = set()
    for _d, _lst in (("/bin", bin_files), ("/lib", lib_files), ("/usr/lib", usr_lib_files),
                     ("/usr/lib/gbm", gbm_files), ("/", root_files), ("/etc", etc_files)):
        _base_paths |= {("" if _d == "/" else _d) + "/" + t[0] for t in _lst}
    for _lst in (m4_share_files, m5_exec_files):
        _base_paths |= {t[0] + "/" + t[1] for t in _lst}
    _ap_gen = [e for e in _ap_gen if e["path"] not in _ap_owned and e["path"] not in _base_paths]
    # podman: storage.conf is ours (vfs default until overlay works; ports/podman/storage.conf)
    _podconf = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ports", "podman", "storage.conf")
    for e in _ap_entries:
        if e["path"] == "/etc/containers/storage.conf" and e["kind"] == "file" and os.path.isfile(_podconf):
            e["host"] = _podconf
            e["size"] = os.path.getsize(_podconf)
    # podman: containers.conf selects netavark's "none" firewall driver (no iptables/nft in the image)
    _podcc = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ports", "podman", "containers.conf")
    _stor = next((e for e in _ap_entries if e["path"] == "/etc/containers/storage.conf" and e["kind"] == "file"), None)
    if _stor is not None and os.path.isfile(_podcc):
        _ap_entries = [e for e in _ap_entries if e["path"] != "/etc/containers/containers.conf"]
        _cc = dict(_stor)
        _cc.update(path="/etc/containers/containers.conf", host=_podcc, size=os.path.getsize(_podcc))
        _ap_entries.append(_cc)
    # at-spi2-core ships org.a11y.Bus.service running at-spi-bus-launcher, which
    # spawns /usr/bin/dbus-daemon (absent: busd is our broker) and fails every
    # GetAddress. ours (ports/dbus/session-pkg, leandros-sysbus a11y) wins.
    _a11y = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ports", "dbus", "session-pkg", "services", "org.a11y.Bus.service")
    if os.path.isfile(_a11y):
        for e in _ap_entries:
            if e["path"] == "/usr/share/dbus-1/services/org.a11y.Bus.service" and e["kind"] == "file":
                e["host"] = _a11y
                e["size"] = os.path.getsize(_a11y)
    # Alpine gives newuidmap/newgidmap file capabilities (security.capability xattr), which the image does not
    # carry; setuid root is the equivalent until file caps exist (see podtrace-findings).
    for e in _ap_entries:
        if e["path"] in ("/usr/bin/newuidmap", "/usr/bin/newgidmap"):
            e["mode"] |= 0o4000
    _ap_hosts = {e["path"]: e["host"] for e in _ap_entries + _ap_gen if e["kind"] == "file"}
    alpine_files = []          # (image dir, name, host path | symlink bytes, full mode)
    _ap_dirs, _ap_dirmode = set(), {}
    for e in _ap_entries + _ap_gen:
        d, n = os.path.split(e["path"])
        if e["kind"] == "dir":
            _ap_dirs.add(e["path"])
            if e["pkg"] != "leandros-base":
                _ap_dirmode[e["path"]] = e["mode"]
            continue
        _ap_dirs.add(d)
        if e["kind"] == "symlink":
            alpine_files.append((d, n, e["linkname"].encode(), S_IFLNK | 0o777))
        else:
            h = e["host"] if e["kind"] == "file" else _ap_hosts[e["linkname"]]
            alpine_files.append((d, n, h, 0o100000 | e["mode"]))
    # /var/run -> /run (conmon's default socket dir /var/run/crio; Alpine's
    # baselayout ships the link, a stripped image may not).
    if not any(e["path"] == "/var/run" for e in _ap_entries + _ap_gen):
        alpine_files.append(("/var", "run", b"../run", S_IFLNK | 0o777))
        _ap_dirs.add("/var")
    # /var/tmp: containers/storage and podman stage image copies there
    _ap_dirs.add("/var/tmp")
    _ap_dirmode["/var/tmp"] = 0o1777
    # sshd privilege separation: the network child chroots here (root-owned,
    # not group/other writable, empty; sshd refuses to start otherwise).
    _ap_dirs.add("/var/empty")
    _ap_dirmode["/var/empty"] = 0o755
    for _d in set(_ap_dirs):
        while _d not in ("/", ""):
            _ap_dirs.add(_d)
            _d = os.path.dirname(_d)
    m4_share_dirs.update(_ap_dirs - {"/"})
    _ap_bytes = sum(e["size"] for e in _ap_entries if e["kind"] == "file")
    print(f"apk: {len(_ap_pkgs)} Alpine packages, {len(alpine_files)} files, {_ap_bytes // (1024 * 1024)} MiB staged")

    # Optional binaries are skipped silently when their build output is absent.
    # Say so, and let LEANDROS_REQUIRE=rg,grep,dmesg,coreutils make it fatal.
    _opt_src = {
        "grep": f"userland/grep/target/{_musl_triple}/release/grep",
        "dmesg": "userland/dmesg/dmesg.sh",
        "coreutils": coreutils_bin,
        "disktester": f"../disks-rs/target/{_musl_triple}/release/disktester",
    }
    _have = {name for name, _path, _mode in bin_files}
    _missing = [n for n in _opt_src if n not in _have]
    for _n in _missing:
        print(f"MISSING optional: {_n} ({_opt_src[_n]})")
    if _missing:
        print(f"MISSING optional summary: {len(_missing)} skipped: {' '.join(_missing)}")
    _req = [r for r in os.environ.get("LEANDROS_REQUIRE", "").split(",") if r]
    _fatal = [r for r in _req if r in _missing or r not in _have]
    if _fatal:
        print(f"FATAL: required binaries missing (LEANDROS_REQUIRE): {' '.join(_fatal)}")
        sys.exit(1)

    if any(name == "doom" for name, _path, _mode in bin_files):
        m4_share_dirs.update(("/usr/share", "/usr/share/soundfonts"))
        m4_share_files.extend(soundfont.packaged_files())

    # 2. Dynamically calculate required blocks and image size
    # Each meta segment takes 512 blocks. We have 8 meta segments (4096 blocks).
    # Plus safety margin and blocks for directories.
    def content_size(path):
        return len(path) if isinstance(path, (bytes, bytearray)) else os.path.getsize(path)

    # Cross-check staged .desktop Icon= names against staged icon files.
    #
    # The icon tree now comes from cosmic-epoch (see above; SystemExit already
    # fires there if it is missing), but this check stays: it catches a name an
    # upstream .desktop asks for that genuinely has no file anywhere in
    # cosmic-icons/cosmic-applet-battery, not just a staging gap. Icons are
    # soft-fail at runtime (a blank button, never a crash), which is exactly what
    # makes the gap expensive: the panel comes up looking *almost* right and the
    # missing glyph reads as a rendering bug, invisible until someone photographs
    # it (this is exactly how the greeter's blank buttons went unnoticed).
    #
    # A name is satisfied if any staged icon's stem matches it — freedesktop
    # resolves Icon= by stem, and one name legitimately has several files
    # (…Tiling.On.svg / …Tiling.Off.svg back …Tiling-symbolic).
    _icon_stems = set()
    for _d, _n, _p in m4_share_files:
        if "/icons/" in _d and _n.endswith((".svg", ".png")):
            _icon_stems.add(_n[:-4])
    _missing_icons = {}
    for _d, _n, _p in m4_share_files:
        if _d != "/usr/share/applications" or not _n.endswith(".desktop"):
            continue
        try:
            with open(_p, "r", encoding="utf-8", errors="replace") as _f:
                for _line in _f:
                    if _line.startswith("Icon="):
                        _icon = _line[5:].strip()
                        if _icon and _icon not in _icon_stems:
                            _missing_icons.setdefault(_icon, []).append(_n)
        except OSError:
            pass
    if _missing_icons:
        print(f"  WARNING: {len(_missing_icons)} Icon= name(s) have no staged icon "
              f"file; those buttons render blank:")
        for _icon in sorted(_missing_icons):
            print(f"    {_icon}  (wanted by {', '.join(sorted(_missing_icons[_icon]))})")

    required_blocks = 4096 + 100
    # Count each distinct host path once: repeated paths become hardlinks
    # sharing a single inode and its data blocks, so charging the image for
    # every name would over-allocate by ~100x once coreutils is installed.
    _sized = set()
    for name, path, mode in (bin_files + lib_files + usr_lib_files + gbm_files + root_files + etc_files
                             + [(n, p, 0) for (_d, n, p) in m4_share_files]
                             + [(n, p, 0) for (_d, n, p) in m5_exec_files]
                             + [(n, p, 0) for (_d, n, p, _m) in alpine_files]
                             + [(n, c, 0) for (_d, n, c, _m) in sysfs_files]):
        if not isinstance(path, (bytes, bytearray)):
            if path in _sized:
                continue
            _sized.add(path)
        size = content_size(path)
        k = (size + BLOCK_SIZE - 1) // BLOCK_SIZE
        # Inode block + data blocks + potential direct nodes.
        # ADDRS_PER_DNODE is 1019 in this simplified F2FS: the node footer
        # sits at byte 4076 (5xu32 = 20 bytes), leaving 4076/4 = 1019 slots.
        # This MUST match servers/f2fs/src/lib.rs (NODE_FOOTER_OFF / 4);
        # a 1018 here shifted every block past ~7.9 MB when read back.
        required_blocks += k + 1 + (k + 1018) // 1019
    # Reserve two full segments beyond the statically-populated content for the
    # runtime node/data allocator curseg pointers (see below) so first writes
    # never land on blocks already occupied by pre-populated files/directories.
    required_blocks += 2 * BLOCKS_PER_SEG
        
    # Align to segments (512 blocks) and set a minimum size of 64MB (16384 blocks).
    # Double the segment count beyond what's needed for the pre-populated content
    # so the image ships with roughly 50% free space — tests (f2fstest et al.)
    # write new files at runtime and would otherwise exhaust a snugly-sized image.
    # (Measured 2026-07-21: 2x gives ~51-52% free on both arches even after the
    # coreutils/brush/xattr userland growth — the margin scales with content, so
    # this constant should not need revisiting as the userland grows.)
    # The image is written sparse, so the free half costs no disk space.
    segs = (required_blocks + 511) // 512
    total_segs = max(32, segs * 2)
    # LEANDROS_F2FS_MAX_MIB caps the doubled size so the image fits a small
    # stick; the content plus a 64-segment (128 MiB) margin is never cut.
    _cap = os.environ.get("LEANDROS_F2FS_MAX_MIB")
    if _cap:
        _cap_segs = int(_cap) * 1024 * 1024 // (BLOCKS_PER_SEG * BLOCK_SIZE)
        if _cap_segs < segs + 64:
            sys.exit(f"LEANDROS_F2FS_MAX_MIB={_cap} is below the content ({segs * 2} MiB) plus 128 MiB")
        total_segs = min(total_segs, _cap_segs)
    total_blocks = total_segs * BLOCKS_PER_SEG
    main_segs = total_segs - META_SEGS
    
    print(f"Sizing F2FS image dynamically to {total_blocks * BLOCK_SIZE // (1024 * 1024)} MB ({total_blocks} blocks, {total_segs} segments, {main_segs} main segments)...")
    
    # 3. Define statically allocated directory inodes and data blocks
    dir_nodes = {
        3: ("/"),
        4: ("/bin"),
        5: ("/old_root"),
        6: ("/dev"),
        7: ("/proc"),
        8: ("/tmp"),
        9: ("/etc"),
        10: ("/mnt"),
        11: ("/lib"),
        12: ("/root"),
        13: ("/home"),
        14: ("/home/leandro"),
        15: ("/usr"),
        16: ("/usr/lib"),
        17: ("/usr/lib/gbm"),
        # XDG runtime dir for the COSMIC session — the wayland-N socket (bound by
        # cosmic-comp) and the D-Bus session socket (bound by busd) both live in
        # /run/user/0. Pre-created because runtime `mkdir -p /run/user/0` on this
        # f2fs returns 0 without actually creating the deepest level when multiple
        # nested levels are new in one call (a second mkdir completes it) — so the
        # session must not depend on creating it at boot. /run/user/0 is 0700 root
        # per the XDG spec (libwayland warns otherwise).
        18: ("/run"),
        19: ("/run/user"),
        20: ("/run/user/0"),
        # /root's XDG base dirs, pre-created for the same reason as /run/user/0:
        # cosmic-comp/cosmic-* call create_dir_all() under $HOME (=/root) at
        # startup, and this f2fs's runtime mkdir does not reliably materialize a
        # freshly-created directory (the launcher's `mkdir -p` leaves broken
        # ?-type inodes — mode --------- — which turn a later create_dir_all into
        # an ENOTDIR panic). Shipping .config/.cache/.local as real dirs lets the
        # session's create_dir_all of a one-level-deeper path (e.g. .config/cosmic)
        # succeed. Root-owned 0700 to match /root.
        21: ("/root/.config"),
        22: ("/root/.cache"),
        23: ("/root/.local"),
        # The cosmic-greeter account's home and XDG base dirs. Pre-created for
        # exactly the reason /root's are: cosmic-config create_dir_all()s under
        # $XDG_CONFIG_HOME at startup (CosmicGreeterConfig::load goes through
        # it), and this f2fs does not reliably materialise several new levels in
        # one runtime mkdir. Owned by the greeter so the account can write them
        # after /bin/greeter-launch has dropped to it.
        24: ("/home/cosmic-greeter"),
        25: ("/home/cosmic-greeter/.config"),
        26: ("/home/cosmic-greeter/.cache"),
        27: ("/home/cosmic-greeter/.local"),
    }

    # Per-directory mode/owner overrides; anything not listed here defaults
    # to 0755 root:root (matching what every directory got before ownership
    # was tracked at all).
    dir_owner = {
        12: (0o040700, 0, 0),        # /root
        14: (0o040700, 1000, 1000),  # /home/leandro
        20: (0o040700, 0, 0),        # /run/user/0 (XDG runtime dir, 0700 root)
        21: (0o040700, 0, 0),        # /root/.config
        22: (0o040700, 0, 0),        # /root/.cache
        23: (0o040700, 0, 0),        # /root/.local
        24: (0o040700, 990, 990),    # /home/cosmic-greeter
        25: (0o040700, 990, 990),    # /home/cosmic-greeter/.config
        26: (0o040700, 990, 990),    # /home/cosmic-greeter/.cache
        27: (0o040700, 990, 990),    # /home/cosmic-greeter/.local
    }

    # Subdirectories per parent, used both to emit "name -> child_ino" dentries
    # below and to compute each parent's link count (every child directory's
    # ".." entry adds one hardlink to its parent).
    subdirs = {
        3: [("bin", 4), ("old_root", 5), ("dev", 6), ("proc", 7), ("tmp", 8),
            ("etc", 9), ("mnt", 10), ("lib", 11), ("root", 12), ("home", 13),
            ("usr", 15), ("run", 18)],
        12: [(".config", 21), (".cache", 22), (".local", 23)],
        13: [("leandro", 14), ("cosmic-greeter", 24)],
        24: [(".config", 25), (".cache", 26), (".local", 27)],
        15: [("lib", 16)],
        16: [("gbm", 17)],
        18: [("user", 19)],
        19: [("0", 20)],
    }

    # ── Register the M4 /usr/share data tree as directory inodes ──────────────
    # Allocate a fresh ino/nid for every directory under /usr (usr/share,
    # usr/share/X11, ..., usr/share/X11/xkb/symbols, usr/share/libinput, ...) and
    # wire each into dir_nodes/subdirs so the standard inode/data-block
    # allocation and dentry-emit loops below handle them identically to the
    # static directories. Sorted order guarantees a parent is registered before
    # its children (a child path is its parent plus "/name", and "/" sorts
    # before any name char). /usr itself is static (ino 15). Files are attached
    # to these inos after packing (see m4_tree_files_by_ino below).
    _path_to_ino  = {p: ino for ino, p in dir_nodes.items()}
    _next_dir_ino = max(dir_nodes) + 1
    for d in sorted(m4_share_dirs):
        if d in _path_to_ino:
            continue
        parent_ino = _path_to_ino[os.path.dirname(d)]
        ino = _next_dir_ino
        _next_dir_ino += 1
        dir_nodes[ino] = d
        _path_to_ino[d] = ino
        subdirs.setdefault(parent_ino, []).append((os.path.basename(d), ino))
    for _p, _m in _ap_dirmode.items():
        _i = _path_to_ino.get(_p)
        if _i is not None and _i not in dir_owner and (_m & 0o7777) != 0o755:
            dir_owner[_i] = (0o040000 | (_m & 0o7777), 0, 0)
    # /etc/leandros/network: root:netdev 0770, slots root:netdev 0660.
    GID_NETDEV = 108
    dir_owner[_path_to_ino["/etc/leandros/network"]] = (0o040770, 0, GID_NETDEV)
    m4_tree_files_by_ino = {}
    for _slot in ("profiles.a", "profiles.b"):
        m4_tree_files_by_ino.setdefault(_path_to_ino["/etc/leandros/network"], []).append(
            (_slot, b"", 0o100660, (0, GID_NETDEV)))
    for image_dir, name, hostpath in m4_share_files:
        ino = _path_to_ino[image_dir]
        # The Wi-Fi credentials are root-only (0600), like /etc/shadow.
        _mode = 0o100600 if (image_dir, name) in (("/etc/leandros", "wifi.conf"), ("/root/.ssh", "authorized_keys")) else 0o100644
        m4_tree_files_by_ino.setdefault(ino, []).append((name, hostpath, _mode))
    # The synthetic sysfs attributes ride the same packing table; they carry
    # their own mode because one of them (the "subsystem" link) is S_IFLNK and
    # the rest are 0444 like real sysfs.
    for image_dir, name, content, mode in sysfs_files:
        ino = _path_to_ino[image_dir]
        m4_tree_files_by_ino.setdefault(ino, []).append((name, content, mode))

    import f2fslxpop
    lx_pop = f2fslxpop.Pop(max(dir_nodes) + 1)
    def add_files_to_dir(parent_ino, files):
        lx_pop.add(parent_ino, files)

    print("Packing binaries into /bin...")
    add_files_to_dir(4, bin_files)
    print("Packing libraries into /lib...")
    add_files_to_dir(11, lib_files)
    print("Packing libraries into /usr/lib...")
    add_files_to_dir(16, usr_lib_files)
    print("Packing GBM backends into /usr/lib/gbm...")
    add_files_to_dir(17, gbm_files)
    print("Packing files into /...")
    add_files_to_dir(3, root_files)
    print("Packing files into /etc...")
    add_files_to_dir(9, etc_files)
    if m4_tree_files_by_ino:
        print("Packing /usr/share data tree (XKB keymaps + libinput quirks)...")
        for ino in sorted(m4_tree_files_by_ino):
            add_files_to_dir(ino, m4_tree_files_by_ino[ino])
    if m5_exec_files:
        print("Packing M5 session executables (busd, dbus-run-session) 0755...")
        m5_exec_by_ino = {}
        for image_dir, name, hostpath in m5_exec_files:
            ino = _path_to_ino[image_dir]
            # leandros-pk-helper is the set-user-ID-root half of the PackageKit
            # facade (ports/sysbus): it authorises the real uid itself.
            m5_exec_by_ino.setdefault(ino, []).append(
                (name, hostpath, 0o104755 if name == "leandros-pk-helper" else 0o100755))
        for ino in sorted(m5_exec_by_ino):
            add_files_to_dir(ino, m5_exec_by_ino[ino])

    _ap_by_ino = {}
    for _d, _n, _h, _m in alpine_files:
        _ap_by_ino.setdefault(_path_to_ino[_d], []).append((_n, _h, _m))
    print("Packing Alpine packages...")
    for ino in sorted(_ap_by_ino):
        add_files_to_dir(ino, _ap_by_ino[ino])

    # Live-root apk: the generated leandros-base entry in /lib/apk/db/installed
    # owns every base file, so apk add/del never replaces or removes one.
    if "/lib/apk/db" in _path_to_ino:
        _p_dirs = [os.path.normpath(os.path.join(_apk_root, "..", "..", "..", n, "out", arch))
                   for n in sorted(os.listdir(os.path.join(_apk_root, "..", "..", "..")))]
        _provs = list(apkdb.STATIC_PROVIDES)
        _lock_names = {x[0] for x in _ap_stanzas}
        _provs += [p for p in apkdb.closure_provides(_p_dirs) if p.split("=")[0] not in _lock_names]
        _libs = [(n, h) for n, h, _m in lib_files + usr_lib_files]
        _libs += [(n, h) for d, n, h in m4_share_files + m5_exec_files if d in ("/lib", "/usr/lib")]
        # The libc soname belongs to the real musl package (lock entry): the
        # loader's .recovery copy carries the same SONAME, and a second
        # so:libc.musl-<arch>.so.1 provider in leandros-base makes apk 3
        # report musl "conflicts: leandros-base", so every `apk add` failed.
        _sop = [p for p in apkdb.so_provides(_libs) if not p.startswith("so:libc.musl-")]
        _provs += _sop
        _held_explicit = set(alpinepkgs.lk.read_list(os.path.join(alpinepkgs.lk.ADIR, "held")))
        for _h in alpinepkgs.lk._held_provides(arch):
            if _h.startswith("so:") and _h.split("=")[0] in _held_explicit:
                if not any(p.startswith(_h.split("=")[0] + "=") for p in _sop):
                    sys.exit(f"apk: held dependency {_h.split('=')[0]} is provided by no base library")
            else:
                _provs.append(_h)
        _provs = apkdb.dedupe_provides(_provs)
        _files = [pth for pth, nid in lx_pop.paths(dir_nodes)
                  if (lx_pop.files[nid]["mode"] & 0o170000) == 0o100000]
        _dmeta = {pth: (o[1], o[2], o[0]) for ino, pth in dir_nodes.items()
                  for o in [dir_owner.get(ino, (0o040755, 0, 0))]}
        _db = apkdb.build_installed(_files, _dmeta, _provs, arch, BUILD_TIME,
                                    depends=["musl"],
                                    owned_elsewhere=frozenset(_ap_owned))
        _vdb = alpinepkgs.db_files(_ap_pkgs, _ap_stanzas)
        _bad = apkdb.check_deps(apkdb.parse_db(_db + _vdb["installed"]), [])
        if _bad:
            sys.exit("apk: unsatisfied dependencies: " + "; ".join(f"{a} -> {b}" for a, b in _bad[:20]))
        add_files_to_dir(_path_to_ino["/lib/apk/db"], [
            ("installed", _db + _vdb["installed"], 0o100644),
            ("triggers", _vdb["triggers"], 0o100644),
            ("scripts.tar.gz", _vdb["scripts.tar.gz"], 0o100644)])
        _nr = _db.count(b"\nR:")
        print(f"apk: leandros-base owns {_nr} files, "
              f"{len(_provs)} provides ({sum(p.startswith('so:') for p in _provs)} so:), "
              f"db {(len(_db) + len(_vdb['installed'])) // 1024} KiB")

    # scripts/pi_sync.py: describe the tree (paths, modes, owners, sha256) and
    # stop; no image is written.
    _manifest_dir = os.environ.get("LEANDROS_ROOT_MANIFEST")
    if _manifest_dir:
        n = lx_pop.manifest(_manifest_dir, dir_nodes, dir_owner)
        print(f"Wrote root manifest ({n} inodes) to {_manifest_dir}/manifest.json")
        return
    import time
    t0 = time.time()
    info = lx_pop.finish(out_file, total_segs, dir_nodes, dir_owner, subdirs, BUILD_TIME,
                         label=ROOT_LABEL)
    open(out_file + ".gpu", "w").write(GPU_FLAVOR + "\n")
    print(f"Created lx populated F2FS image at {out_file} in {time.time() - t0:.1f}s: {info}")


if __name__ == '__main__':
    main()
