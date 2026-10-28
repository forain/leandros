"""pi-sync: bring a live LeandrOS root (the Pi 5's SD-card f2fs) up to date with
the current build's aarch64 rootfs, in place, without reflashing.

Driven by `scripts/remote.py pi-sync [--dry-run] [--list]`.

1. Manifest: scripts/mkfs-f2fs-populated.py (the same packer card prep runs,
   LEANDROS_BOARD=rpi5, with the pinned CYW43455 Wi-Fi + BCM4345C0.hcd BT
   firmware fetched first) is run with LEANDROS_ROOT_MANIFEST set, so it writes
   manifest.json (every path, mode, owner, sha256) instead of an image.
2. Remote state: two scripts run on the target: `stat` of every manifest path,
   then `sha256sum` of the regular files whose size already matches.
3. Plan: per path one of new / changed / meta (mode or owner only) / link
   (hardlink to a sibling name) / symlink. What is protected:
     skip   never read or written: /boot (pi-deploy's kernel slots), /proc, /dev,
            /tmp, /run/user, /mnt, /old_root, intercepted /sys nodes,
            /etc/shadow, /etc/gshadow, any .ssh/, ssh host keys, wifi.conf,
            /etc/leandros/network/profiles.*
     keep   created when missing, never overwritten: /etc/leandros/**, /home/**,
            /root/**, /var/**
     merge  /etc/passwd and /etc/group: lines the target lacks are appended and
            group members the build has are added; nothing is removed.
   Existing directories keep their mode/owner; missing ones are created.
   Files on the target that the build does not have are left alone.
4. Upload: changed contents are concatenated into <=64 MiB bundles, each sent
   with remote.py's resumable sha256-verified uploader to /tmp/pisync, then an
   extraction script cuts every file out into <dest>.pisync-new, checks its
   sha256, sets mode/owner and renames it over <dest> (atomic per file).
"""
import hashlib
import json
import os
import shlex
import subprocess
import sys
import tempfile
import time

REQUIRED_BINS = "rg,grep,dmesg,coreutils"
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RTMP = "/tmp/pisync"
BUNDLE_MAX = int(os.environ.get("LEANDROS_PISYNC_BUNDLE", 64 << 20))
CHUNK_PATHS = 120
S_IFMT, S_IFLNK, S_IFREG, S_IFDIR = 0o170000, 0o120000, 0o100000, 0o040000

SKIP_PREFIXES = ("/boot/", "/proc/", "/dev/", "/tmp/", "/run/user/", "/mnt/", "/old_root/",
                 "/sys/class/block/", "/sys/dev/char/226:")
SKIP_EXACT = {"/boot", "/proc", "/dev", "/tmp", "/run/user", "/mnt", "/old_root",
              "/sys/class/block", "/etc/shadow", "/etc/gshadow", "/etc/shadow-", "/etc/gshadow-"}
KEEP_PREFIXES = ("/etc/leandros/", "/home/", "/root/", "/var/")
MERGE = {"/etc/passwd", "/etc/group"}


def classify(path):
    """skip | keep | merge | sync"""
    if path in SKIP_EXACT or path.startswith(SKIP_PREFIXES):
        return "skip"
    base = os.path.basename(path)
    if "/.ssh/" in path or path.endswith("/.ssh") or base.startswith("ssh_host_") \
            or base == "wifi.conf" or (path.startswith("/etc/leandros/network/") and base.startswith("profiles.")):
        return "skip"
    if path in MERGE:
        return "merge"
    if path.startswith(KEEP_PREFIXES):
        return "keep"
    return "sync"


def q(s):
    return shlex.quote(s)


# ── 1. manifest ─────────────────────────────────────────────────────────────

def build_manifest(out_dir, board, log, allow_missing=False):
    env = dict(os.environ)
    for k in ("LEANDROS_WIFI_CONF", "LEANDROS_AUTHORIZED_KEYS"):
        env.pop(k, None)          # never stage credentials into the manifest
    home = os.path.expanduser("~")
    env.setdefault("BRCMFMAC_CACHE", os.path.join(home, ".cache/leandros/brcmfmac"))
    env.setdefault("BTFW_CACHE", os.path.join(home, ".cache/leandros/bluez-firmware"))
    with open(log, "w") as lf:
        for fetch in ("ports/brcmfmac/fetch.sh", "ports/bluez-firmware/fetch.sh"):
            r = subprocess.run([os.path.join(REPO, fetch)], cwd=REPO, env=env, stdout=lf, stderr=lf)
            if r.returncode != 0:
                print(f"WARNING: {fetch} failed (rc {r.returncode}); firmware may be missing, see {log}")
        if board:
            env["LEANDROS_BOARD"] = board
        else:
            env.pop("LEANDROS_BOARD", None)
        env["LEANDROS_ROOT_MANIFEST"] = out_dir
        # A sync must never again silently drop the userland tools the Pi
        # root depends on (rg/grep/dmesg/coreutils). Override: --allow-missing
        # or LEANDROS_PISYNC_ALLOW_MISSING=1.
        if allow_missing or os.environ.get("LEANDROS_PISYNC_ALLOW_MISSING") == "1":
            env.pop("LEANDROS_REQUIRE", None)
        else:
            env["LEANDROS_REQUIRE"] = REQUIRED_BINS
        r = subprocess.run([sys.executable, "scripts/mkfs-f2fs-populated.py",
                            os.path.join(out_dir, "unused.img"), "aarch64"],
                           cwd=REPO, env=env, stdout=lf, stderr=lf)
    mf = os.path.join(out_dir, "manifest.json")
    if r.returncode != 0 or not os.path.exists(mf):
        miss = [l.strip() for l in open(log, errors="replace") if l.startswith(("MISSING", "FATAL"))]
        if miss:
            print("\n".join(miss[-8:]))
        sys.exit(f"manifest generation failed (rc {r.returncode}); see {log} "
                 f"(run ./scripts/build-all.sh --arch aarch64 first)")
    return json.load(open(mf))


# ── 2. remote state ─────────────────────────────────────────────────────────

def run_script(t, R, name, text, timeout):
    """Uploads `text` as RTMP/name (verified) and runs it with sh."""
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write(text)
        local = f.name
    try:
        R.upload_one(t, local, f"{RTMP}/{name}", R.sha256_file(local), "600", quiet=True)
    finally:
        os.unlink(local)
    p = R.rrun(t, f"sh {RTMP}/{name}", timeout=timeout, tries=2, what=name)
    return p.stdout.decode(errors="replace")


def run_detached(t, R, name, text, timeout):
    """Uploads `text` as RTMP/name and runs it detached from the ssh session,
    then polls for its done marker with short ssh calls. A big extraction on
    the Pi's SD card can outlast the ssh connection (keepalives starve while
    the card is busy); sshd's channel close then SIGHUPs the session's process
    group (userland/sshd/src/session.rs), which used to kill `head` midway.
    Here the job ignores SIGHUP (inherited by every child through exec), has
    no tie to the session's stdio, and a dropped poll is simply retried.
    Starting is idempotent (a `.started` marker), so an ssh retry of the
    start command never runs the script twice. Returns the script's output
    ("" if it did not finish within `timeout`)."""
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write("trap '' HUP\n" + text)
        local = f.name
    try:
        R.upload_one(t, local, f"{RTMP}/{name}", R.sha256_file(local), "600", quiet=True)
    finally:
        os.unlink(local)
    base = f"{RTMP}/{name}"
    start = (f"[ -e {base}.started ] || {{ : > {base}.started; rm -f {base}.log {base}.done; "
             f"sh -c 'trap \"\" HUP; sh {base} > {base}.log 2>&1; : > {base}.done' "
             f"< /dev/null > /dev/null 2>&1 & }}; echo started")
    R.rrun(t, start, timeout=60, tries=3, what=f"start {name}")
    deadline, last = time.time() + timeout, ""
    while time.time() < deadline:
        try:
            p = subprocess.run(t.ssh_argv(f"cat {base}.log; [ -e {base}.done ] && echo PISYNC-DONE"),
                               capture_output=True, timeout=30)
            out = p.stdout.decode(errors="replace")
            if p.returncode != 255:
                last = out
                if "PISYNC-DONE" in out.splitlines()[-1:]:
                    R.rrun(t, f"rm -f {base} {base}.log {base}.done {base}.started", timeout=60, tries=3,
                           what=f"clean {name}")
                    return out
        except subprocess.TimeoutExpired:
            pass
        time.sleep(5)
    return last if "PISYNC-DONE" in last else ""


def remote_stat(t, R, paths):
    """{path: (fmt_mode, uid, gid, size)} for the paths that exist."""
    lines = []
    for i in range(0, len(paths), CHUNK_PATHS):
        lines.append("stat -c '%n|%f|%u|%g|%s' -- " + " ".join(q(p) for p in paths[i:i + CHUNK_PATHS])
                     + " 2>/dev/null")
    lines.append("echo PISYNC-END")
    out = run_script(t, R, "stat.sh", "\n".join(lines) + "\n", timeout=1800)
    if "PISYNC-END" not in out:
        sys.exit("remote stat script did not finish")
    res = {}
    for line in out.splitlines():
        parts = line.rsplit("|", 4)
        if len(parts) == 5:
            try:
                res[parts[0]] = (int(parts[1], 16), int(parts[2]), int(parts[3]), int(parts[4]))
            except ValueError:
                pass
    return res


def remote_hashes(t, R, paths, links):
    lines = []
    for i in range(0, len(paths), CHUNK_PATHS):
        lines.append("sha256sum -- " + " ".join(q(p) for p in paths[i:i + CHUNK_PATHS]) + " 2>/dev/null")
    for p in links:
        lines.append(f'echo "L|{p}|$(readlink -- {q(p)} 2>/dev/null)"')
    lines.append("echo PISYNC-END")
    out = run_script(t, R, "hash.sh", "\n".join(lines) + "\n", timeout=3600)
    if "PISYNC-END" not in out:
        sys.exit("remote hash script did not finish")
    shas, targets = {}, {}
    for line in out.splitlines():
        if line.startswith("L|"):
            _, p, tgt = line.split("|", 2)
            targets[p] = tgt
            continue
        parts = line.split(None, 1)
        if len(parts) == 2 and len(parts[0]) == 64:
            shas[parts[1].lstrip("*")] = parts[0]
    return shas, targets


# ── passwd / group merge ────────────────────────────────────────────────────

def merge_db(remote_text, build_text, is_group):
    """Union by name; for groups also the member lists. Never removes."""
    out = remote_text.splitlines()
    idx = {l.split(":", 1)[0]: i for i, l in enumerate(out) if l and not l.startswith("#")}
    notes = []
    for bl in build_text.splitlines():
        if not bl or bl.startswith("#"):
            continue
        name = bl.split(":", 1)[0]
        if name not in idx:
            out.append(bl)
            idx[name] = len(out) - 1
            notes.append(f"+{name}")
        elif is_group:
            r = out[idx[name]].split(":")
            b = bl.split(":")
            if len(r) == 4 and len(b) == 4:
                have = [m for m in r[3].split(",") if m]
                add = [m for m in b[3].split(",") if m and m not in have]
                if add:
                    r[3] = ",".join(have + add)
                    out[idx[name]] = ":".join(r)
                    notes.append(f"{name}+={','.join(add)}")
    return "\n".join(out) + "\n", notes


# ── 3. plan ─────────────────────────────────────────────────────────────────

def plan(t, R, man, blobdir):
    entries = []      # (path, entry)
    for e in man["files"]:
        for p in e["paths"]:
            entries.append((p, e))
    paths = [p for p, e in entries if classify(p) != "skip"]
    dirs = [d for d in man["dirs"] if classify(d["path"]) != "skip" and d["path"] != "/"]
    st = remote_stat(t, R, paths + [d["path"] for d in dirs])
    to_hash, links = [], []
    for p, e in entries:
        c = classify(p)
        if c == "skip" or p not in st:
            continue
        if (e["mode"] & S_IFMT) == S_IFLNK:
            if (st[p][0] & S_IFMT) == S_IFLNK:
                links.append(p)
        elif (st[p][0] & S_IFMT) == S_IFREG and st[p][3] == e["size"] and c != "keep":
            to_hash.append(p)
    shas, targets = remote_hashes(t, R, to_hash, links)

    acts = {"mkdir": [], "new": [], "changed": [], "meta": [], "link": [], "symlink": [], "merge": []}
    notes = []
    for d in dirs:
        if d["path"] not in st:
            acts["mkdir"].append(d)
    for e in man["files"]:
        primary = None    # first name of this inode that already has (or will get) the content
        need = []
        for p in e["paths"]:
            c = classify(p)
            if c == "skip":
                continue
            r = st.get(p)
            if c == "keep" and r is not None:
                continue
            if c == "merge":
                acts["merge"].append((p, e))
                continue
            if (e["mode"] & S_IFMT) == S_IFLNK:
                if r is None or (r[0] & S_IFMT) != S_IFLNK or targets.get(p) != e["target"]:
                    acts["symlink"].append((p, e))
                continue
            if r is not None and (r[0] & S_IFMT) == S_IFREG and shas.get(p) == e["sha256"]:
                primary = primary or p
                if (r[0] & 0o7777) != (e["mode"] & 0o7777) or (r[1], r[2]) != (e["uid"], e["gid"]):
                    acts["meta"].append((p, e))
                continue
            need.append((p, e, "changed" if r is not None else "new"))
        for p, e2, kind in need:
            if primary is None:
                acts[kind].append((p, e2))
                primary = p
            else:
                acts["link"].append((p, e2, primary))

    # passwd/group: merge against the target's copy (contents are never printed)
    if acts["merge"]:
        p = R.rrun(t, "cat /etc/passwd; echo PISYNC-SPLIT; cat /etc/group", what="passwd/group")
        rp, _, rg = p.stdout.decode(errors="replace").partition("PISYNC-SPLIT\n")
        merged = []
        for path, e in acts["merge"]:
            build = open(e["src"], "rb").read().decode(errors="replace")
            remote = rp if path == "/etc/passwd" else rg
            if not remote.strip():
                continue            # unreadable target copy: leave it alone
            text, n = merge_db(remote, build, path == "/etc/group")
            if n:
                data = text.encode()
                sha = hashlib.sha256(data).hexdigest()
                blob = os.path.join(blobdir, "merge-" + sha)
                open(blob, "wb").write(data)
                e2 = dict(e, src=blob, sha256=sha, size=len(data), paths=[path], mode=0o100644, uid=0, gid=0)
                merged.append((path, e2, n))
                notes.append(f"{path}: {' '.join(n)}")
                if path == "/etc/passwd":
                    notes.append("  (new users get no /etc/shadow entry: no password login until set)")
        acts["merge"] = merged
    return acts, notes


def total_bytes(acts):
    return sum(e["size"] for k in ("new", "changed") for _, e in acts[k]) + \
        sum(e["size"] for _, e, _ in acts["merge"])


def describe(acts, list_all):
    lines = []
    for d in acts["mkdir"]:
        lines.append(f"mkdir   {d['path']}")
    for k in ("new", "changed"):
        for p, e in acts[k]:
            lines.append(f"{k:<8}{p} ({e['size']} B)")
    for p, e, _ in acts["merge"]:
        lines.append(f"merge   {p}")
    for p, e, prim in acts["link"]:
        lines.append(f"link    {p} = {prim}")
    for p, e in acts["symlink"]:
        lines.append(f"symlink {p} -> {e['target']}")
    for p, e in acts["meta"]:
        lines.append(f"meta    {p} {oct(e['mode'] & 0o7777)} {e['uid']}:{e['gid']}")
    by_top = {}
    for l in lines:
        path = l.split()[1]
        top = "/".join(path.split("/")[:3]) or "/"
        by_top[top] = by_top.get(top, 0) + 1
    return lines, by_top


# ── 4. apply ────────────────────────────────────────────────────────────────

def apply(t, R, acts):
    pre = ["umask 022"]
    for d in sorted(acts["mkdir"], key=lambda d: d["path"]):
        pre.append(f"mkdir -p {q(d['path'])} && chmod {d['mode'] & 0o7777:o} {q(d['path'])} && "
                   f"chown {d['uid']}:{d['gid']} {q(d['path'])} || echo 'FAIL mkdir {d['path']}'")
    for p, e in acts["meta"]:
        pre.append(f"chmod {e['mode'] & 0o7777:o} {q(p)} && chown {e['uid']}:{e['gid']} {q(p)} "
                   f"|| echo 'FAIL meta {p}'")
    pre.append("echo PISYNC-END")
    out = run_script(t, R, "pre.sh", "\n".join(pre) + "\n", timeout=600)
    fails = [l for l in out.splitlines() if l.startswith("FAIL")]

    content = [(p, e) for k in ("new", "changed") for p, e in acts[k]] + \
              [(p, e) for p, e, _ in acts["merge"]]
    bundles, cur, size = [], [], 0
    for p, e in content:
        if cur and size + e["size"] > BUNDLE_MAX:
            bundles.append(cur)
            cur, size = [], 0
        cur.append((p, e))
        size += e["size"]
    if cur:
        bundles.append(cur)
    t0, sent = time.time(), 0
    for bi, b in enumerate(bundles):
        with tempfile.NamedTemporaryFile(suffix=".bundle", delete=False) as bf:
            local = bf.name
            lines, off = ["umask 077", f"B={RTMP}/bundle"], 0
            for p, e in b:
                with open(e["src"], "rb") as sf:
                    data = sf.read()
                if hashlib.sha256(data).hexdigest() != e["sha256"]:
                    sys.exit(f"{e['src']} changed while syncing; rerun")
                bf.write(data)
                n = len(data)
                x = q(p + ".pisync-new")
                if n == 0:
                    cut = f": > {x}"
                elif off == 0:
                    cut = f"head -c {n} $B > {x}"
                else:
                    cut = f"tail -c +{off + 1} $B | head -c {n} > {x}"
                lines.append(f"{cut} && set -- $(sha256sum {x}) && [ \"$1\" = {e['sha256']} ] && "
                             f"chmod {e['mode'] & 0o7777:o} {x} && chown {e['uid']}:{e['gid']} {x} && "
                             f"mv -f {x} {q(p)} || {{ rm -f {x}; echo 'FAIL {p}'; }}")
                off += n
            lines.append(f"rm -f $B\necho PISYNC-END")
        try:
            R.upload_one(t, local, f"{RTMP}/bundle", R.sha256_file(local), "600", quiet=True)
        finally:
            os.unlink(local)
        # Budget: 1800 s, or 200 kB/s of SD write for a big bundle (the Pi's
        # card wrote ~0.5 MB/s single-sector, and each byte is written twice:
        # the upload, then the cut).
        out = run_detached(t, R, f"extract{bi}.sh", "\n".join(lines) + "\n",
                           timeout=max(1800, off / 200e3))
        if "PISYNC-END" not in out:
            fails.append(f"FAIL bundle {bi}: extraction script did not finish")
        fails += [l for l in out.splitlines() if l.startswith("FAIL")]
        sent += off
        dt = time.time() - t0
        print(f"  bundle {bi + 1}/{len(bundles)}: {len(b)} file(s), {sent >> 20} MiB total, "
              f"{sent / dt / 1e6 if dt else 0:.1f} MB/s", flush=True)

    post = []
    for p, e, prim in acts["link"]:
        x = q(p + ".pisync-new")
        post.append(f"rm -f {x}; ln -f {q(prim)} {x} && mv -f {x} {q(p)} || echo 'FAIL link {p}'")
    for p, e in acts["symlink"]:
        post.append(f"rm -f {q(p)}; ln -s {q(e['target'])} {q(p)} || echo 'FAIL symlink {p}'")
    post.append(f"sync; rm -rf {RTMP}; echo PISYNC-END")
    out = run_script(t, R, "post.sh", "\n".join(post) + "\n", timeout=1800)
    fails += [l for l in out.splitlines() if l.startswith("FAIL")]
    return fails


def main(t, R, a):
    t0 = time.time()
    work = tempfile.mkdtemp(prefix="pisync-")
    try:
        if a.manifest:
            man = json.load(open(a.manifest))
        else:
            print("building root manifest (mkfs-f2fs-populated.py, board "
                  f"{a.board or 'virt'}) ...", flush=True)
            man = build_manifest(work, a.board, os.path.join(work, "mkfs.log"),
                                 getattr(a, "allow_missing", False))
        print(f"manifest: {len(man['files'])} inodes, {len(man['dirs'])} dirs; probing {t.label()} ...",
              flush=True)
        R.rrun(t, f"mkdir -p {RTMP}", what="mkdir")
        acts, notes = plan(t, R, man, work)
        lines, by_top = describe(acts, a.list)
        nbytes = total_bytes(acts)
        if a.plan_out:
            with open(a.plan_out, "w") as f:
                f.write("\n".join(lines) + "\n")
        if a.list:
            print("\n".join(lines))
        else:
            for top in sorted(by_top):
                print(f"  {by_top[top]:6d}  {top}")
        for n in notes:
            print("  " + n)
        print(f"plan: {len(lines)} change(s): " +
              ", ".join(f"{k} {len(v)}" for k, v in acts.items() if v) +
              f"; {nbytes} bytes ({nbytes / 1e6:.1f} MB) to upload"
              + (f"; full list in {a.plan_out}" if a.plan_out else ""))
        if a.dry_run or not lines:
            R.rrun(t, f"rm -rf {RTMP}", what="cleanup")
            if not lines:
                print("up to date: 0 bytes uploaded")
            return 0
        fails = apply(t, R, acts)
        for f in fails[:20]:
            print("  " + f)
        print(f"pi-sync: {len(lines) - len(fails)}/{len(lines)} change(s) applied, {nbytes} bytes, "
              f"{time.time() - t0:.0f} s" + (f"; {len(fails)} FAILED" if fails else ""))
        return 1 if fails else 0
    finally:
        subprocess.run(["rm", "-rf", work])
