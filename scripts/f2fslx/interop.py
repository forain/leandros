#!/usr/bin/env python3
"""Linux <-> LeandrOS f2fs interop test (aarch64 QEMU + the Linux box).

Usage (see scripts/f2fslx/interop.sh, the wrapper):
  interop.sh [--list] <variant...>        variants: lxdef lxnoi lxext (direction A)
                                          home (direction B)   orph (Linux orphan CP, S7)   fsync (Linux roll-forward)   all = every variant
  F2FSINTEROP_LOCK=<path>/lock.sh         if set, box work runs under `lock.sh box`
                                          and QEMU boots under `lock.sh qemu`.
  F2FSINTEROP_BOX=user@host               default forain@172.16.158.150
  LEANDROS_RUN_ID                         default f2fsinterop-aarch64

Direction A (Linux -> LeandrOS): on the box mkfs.f2fs + populate + MANIFEST md5,
fsck rc 0, image shipped gzipped; LeandrOS boots with it attached, mounts it by
label (rw, else ro), `md5sum -c MANIFEST`, attempts a write; a volume mounted ro
must be bit-identical afterwards.
Direction B (LeandrOS -> Linux): scripts/mkfs-f2fs-home.py volume -> LeandrOS
workload -> box `fsck.f2fs --dry-run` -> Linux rw mount, verify, write a file,
umount, fsck -> LeandrOS mounts again and verifies the Linux file.

Each step prints one line: PASS | FAIL | EXPECTED-FAIL | UNEXPECTED-PASS | SKIP.
Exit code = number of FAIL + UNEXPECTED-PASS.  Known gaps live in
scripts/f2fslx/interop-known-failures.txt (patterns `variant:step`, fnmatch,
`#` comments); the lane that fixes a gap deletes its entry (a step that then
passes while still listed is UNEXPECTED-PASS, so stale entries are caught).
Guest images/disks are scratch under $TMPDIR; box scratch is /tmp/f2fsinterop-$$
and always removed.
"""
import fnmatch, hashlib, json, os, re, shutil, struct, subprocess, sys, tempfile, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
BOX = os.environ.get("F2FSINTEROP_BOX", "forain@172.16.158.150")
LOCK = os.environ.get("F2FSINTEROP_LOCK", "")
SSH = ["ssh", "-o", "BatchMode=yes", BOX]
# name -> (mkfs options, mount options used while populating[, image size])
VARIANTS = {
    "lxdef": ("", ""),
    "lxnoi": ("", "noinline_data"),
    "lxext": ("-O extra_attr,inode_checksum,flexible_inline_xattr", ""),
    # design f2fslxrw S5: multi-segment NAT (2G: segment_count_nat 6) and a
    # volume whose version bitmaps overflow the commit block's private area
    # (32G sparse: SIT+NAT bitmaps past offset 2048). Images stay sparse.
    "lx2g": ("", "", "2G"),
    "lx32g": ("", "", "32G"),
    # design f2fslxrw S9: Linux-created 100k-entry dir read by us + a 100k-entry dir we create
    # (dentry blocks past i_addr: hash level >= 9), then Linux ls/stat + fsck rc 0.
    "lxdir": ("", "", "4G"),
    # S9b: 256G (cp_payload 0, bitmaps 3840 B in the commit block) and 4T (mkfs gives cp_payload 2:
    # the SIT version bitmap lives in payload blocks after the commit block). Images stay sparse.
    "lx256g": ("", "", "256G"),
    "lx4t": ("", "", "4T"),
}
BIGDIR = ("lxdir",)
SIZE = "128M"
results = []


def known():
    pats = []
    try:
        for l in open(os.path.join(HERE, "interop-known-failures.txt")):
            l = l.split("#")[0].strip()
            if l:
                pats.append(l)
    except OSError:
        pass
    return pats


KNOWN = known()


def step(variant, name, ok, why=""):
    key = f"{variant}:{name}"
    exp = any(fnmatch.fnmatch(key, p) for p in KNOWN)
    st = ("UNEXPECTED-PASS" if exp else "PASS") if ok else ("EXPECTED-FAIL" if exp else "FAIL")
    print(f"{st:15} {key:28} {why}"[:200], flush=True)
    results.append(st)
    return ok


def skip(variant, name, why):
    print(f"{'SKIP':15} {variant}:{name:20} {why}", flush=True)


def locked(kind, argv):
    return ([LOCK, kind] if LOCK else []) + argv


def box(script, timeout=1800):
    r = subprocess.run(locked("box", SSH + ["bash", "-s"]), input=script, capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout + r.stderr


_BOX_DUMP = r"""
import os,sys,struct
f=os.open(sys.argv[1],os.O_RDONLY); size=os.fstat(f).st_size; off=0; out=sys.stdout.buffer
while True:
    try: d=os.lseek(f,off,os.SEEK_DATA)
    except OSError: break
    h=os.lseek(f,d,os.SEEK_HOLE)
    while d<h:
        b=os.pread(f,min(1<<20,h-d),d)
        if not b: break
        out.write(struct.pack("<QQ",d,len(b))); out.write(b); d+=len(b)
    off=h
out.write(struct.pack("<QQ",size,0)); out.flush()
"""
_BOX_LOAD = r"""
import os,sys,struct
inp=sys.stdin.buffer; f=open(sys.argv[1],"wb")
while True:
    h=inp.read(16)
    if len(h)<16: sys.exit(1)
    o,n=struct.unpack("<QQ",h)
    if n==0: f.truncate(o); break
    f.seek(o); f.write(inp.read(n))
"""


def _rd(p, n):
    b = b""
    while len(b) < n:
        c = p.read(n - len(b))
        if not c:
            break
        b += c
    return b


def fetch(remote, local):
    """Data extents only (SEEK_DATA on the box), gzipped over ssh, written sparse."""
    p1 = subprocess.Popen(SSH + [f"python3 -c '{_BOX_DUMP}' {remote} | gzip -1c"], stdout=subprocess.PIPE)
    p2 = subprocess.Popen(["gunzip"], stdin=p1.stdout, stdout=subprocess.PIPE)
    p1.stdout.close()
    ok = False
    with open(local, "wb") as f:
        while True:
            h = _rd(p2.stdout, 16)
            if len(h) < 16:
                break
            o, n = struct.unpack("<QQ", h)
            if n == 0:
                f.truncate(o); ok = True; break
            c = _rd(p2.stdout, n)
            if c != bytes(len(c)):
                f.seek(o); f.write(c)
    p2.wait(); return p1.wait() == 0 and ok


def push(local, remote):
    src = ('import os,sys,struct,gzip\n'
           'f=os.open(sys.argv[1],os.O_RDONLY); size=os.fstat(f).st_size; off=0; out=gzip.GzipFile(fileobj=sys.stdout.buffer,mode="wb",compresslevel=1)\n'
           'while True:\n'
           '    try: d=os.lseek(f,off,os.SEEK_DATA)\n'
           '    except OSError: break\n'
           '    h=os.lseek(f,d,os.SEEK_HOLE)\n'
           '    while d<h:\n'
           '        b=os.pread(f,min(1<<20,h-d),d)\n'
           '        if not b: break\n'
           '        out.write(struct.pack("<QQ",d,len(b))); out.write(b); d+=len(b)\n'
           '    off=h\n'
           'out.write(struct.pack("<QQ",size,0)); out.close()\n')
    p1 = subprocess.Popen([sys.executable, "-c", src, local], stdout=subprocess.PIPE)
    p2 = subprocess.run(SSH + [f"gunzip | python3 -c '{_BOX_LOAD}' {remote}"], stdin=p1.stdout)
    p1.stdout.close(); return p1.wait() == 0 and p2.returncode == 0


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while b := f.read(1 << 20):
            h.update(b)
    return h.hexdigest()


_BOOT = {}


def boot_image(cmdline):
    """A copy of the aarch64 boot image with `cmdline` baked in (cached per cmdline)."""
    if cmdline not in _BOOT:
        d = tempfile.mkdtemp(prefix="f2fsinterop-boot-")
        dst = os.path.join(d, "boot.img")
        if subprocess.run(["cp", "-c", os.path.join(REPO, "leandros-limine-aarch64.img"), dst], capture_output=True).returncode != 0:
            shutil.copyfile(os.path.join(REPO, "leandros-limine-aarch64.img"), dst)
        subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), dst, cmdline], check=True, capture_output=True)
        _BOOT[cmdline] = dst
    return _BOOT[cmdline]


def guest(imgs, cmds, tag, cmdline=None):
    """Boot aarch64 with imgs attached, run cmds, return list of outputs."""
    env = dict(os.environ, LEANDROS_RUN_ID=os.environ.get("LEANDROS_RUN_ID", "f2fsinterop-aarch64"))
    if cmdline:
        env["LEANDROS_DISK_IMAGE"] = boot_image(cmdline)
    else:
        env.pop("LEANDROS_DISK_IMAGE", None)
    r = subprocess.run(locked("qemu", [sys.executable, os.path.abspath(__file__), "--guest", json.dumps(imgs)]),
                       input=json.dumps(cmds), capture_output=True, text=True, env=env, timeout=3000)
    for l in r.stdout.splitlines():
        if l.startswith("@@JSON@@"):
            outs = json.loads(l[8:])
            if os.environ.get("F2FSINTEROP_VERBOSE"):
                for c, o in zip(cmds, outs):
                    print(f"  $ {c[:90]}\n    {o.strip()[-1500:]}", file=sys.stderr)
            return outs
    print(f"guest run {tag} failed rc={r.returncode}: {(r.stdout + r.stderr)[-300:]}", file=sys.stderr)
    return None


def guest_main(imgs):
    sys.path.insert(0, os.path.join(REPO, ".claude/skills/run-leandros"))
    os.chdir(REPO)
    os.environ["LEANDROS_QEMU_EXTRA"] = " ".join(
        f"-drive if=none,id=up{i},format=raw,file={m} -device virtio-blk-pci,drive=up{i},disable-legacy=on"
        for i, m in enumerate(json.loads(imgs)))
    import driver
    cmds = json.loads(sys.stdin.read()); outs = []
    driver.cmd_start("aarch64"); driver.cmd_login("root", "root", timeout=180)
    try:
        for c in cmds:
            r = driver.serial_run(c, timeout=3000 if "BIGD_DONE" in c else 300, arch="aarch64")
            outs.append(r["output"] if isinstance(r, dict) else str(r))
    finally:
        driver.cmd_stop()
    print("@@JSON@@" + json.dumps(outs))


MNT = ("mkdir -p /mnt/{l}; if mount -t f2fs LABEL={l} /mnt/{l}; then echo MNT_RW; "
       "elif mount -t f2fs -o ro LABEL={l} /mnt/{l}; then echo MNT_RO; else echo MNT_FAIL; fi")


# the default QEMU config already carries a leandros-home disk (vdc); ours is the next one
MNTH = "mkdir -p /mnt/h; if mount -t f2fs /dev/vdd /mnt/h; then echo MNT_RW; elif mount -t f2fs -o ro /dev/vdd /mnt/h; then echo MNT_RO; else echo MNT_FAIL; fi"


def md5chk(l, f):
    return f"cd /mnt/{l} && md5sum -c {f} > /tmp/m.out 2>&1; echo MD5RC=$?; tail -n 2 /tmp/m.out; cd /"


def md5_ok(out):
    return "MD5RC=0" in out and "WARNING" not in out and "FAILED" not in out


# Inline write step (design f2fslxrw S2): append to inline files (one stays
# inline-sized, one outgrows the inode), overflow an inline dir with 120 long
# names, add into an empty inline dir, rmdir a non-empty inline dir (must fail)
# and an empty one (must work), then record md5s of what we wrote.
INLW = ("cd /mnt/{l} && echo more >> small/inl && cat small/inl && dd if=big/b3m bs=2000 count=1 >> small/inl3000 2>/dev/null && "
        "for i in $(seq 1 120); do echo d$i > dir1/sub/inline_dir_overflow_entry_name_$i; done; "
        "mkdir small/newd; echo x > small/newd/a; echo y > upg/u1; rm upg/u1; rmdir upg; echo RMDIR_EMPTY_RC=$?; "
        "rmdir inl2/x 2>/dev/null; echo RMDIR_NONEMPTY_RC=$?; rm inl2/x/f; rmdir inl2/x; echo RMDIR_AFTER_RC=$?; "
        "ls dir1/sub | wc -l; cat small/inl3000 | wc -c; "
        "md5sum small/inl small/inl3000 small/newd/a dir1/sub/inline_dir_overflow_entry_name_* > ours2.md5; "
        "md5sum -c ours2.md5 2>&1 | tail -n 1; sync; cd /; echo INLW_DONE")
BIGD = ("cd /mnt/{l} && echo BIGD_LS=$(ls bigdir | wc -l); i=0; bad=0; while [ $i -lt 1500 ]; do n=$(( (i*7919+13) % 100000 + 1 )); "
        "[ -e bigdir/e_$n ] || bad=$((bad+1)); i=$((i+1)); done; echo BIGD_BAD=$bad; [ -e bigdir/e_100001 ] && echo BIGD_PHANTOM; "
        "mkdir newbig; i=1; while [ $i -le 100000 ]; do : > newbig/f_$i; i=$((i+1)); done; echo BIGD_NEWLS=$(ls newbig | wc -l); "
        "rm newbig/f_7; [ -e newbig/f_7 ] && echo BIGD_RM_FAILED; sync; cd /; echo BIGD_DONE")
EXCL = "small/inl$|small/inl3000$|^[0-9a-f]+  ./(upg|inl2)"


ACLA = "02000000" "01000600ffffffff" "0200" "0600" "d2040000" "04000400ffffffff" "10000600ffffffff" "20000000ffffffff"
ACLD = "02000000" "01000700ffffffff" "0200" "0700" "4d000000" "04000500ffffffff" "10000700ffffffff" "20000500ffffffff"
# xattr step (design f2fslxrw S8): verify every Linux-set xattr/ACL via /bin/xattr (hex), then set ours
XA = ("cd /mnt/{l}; ok=0; bad=0; while read f n h; do g=$(xattr -X $n $f); if [ \"$g\" = \"$h\" ]; then ok=$((ok+1)); "
      "else bad=$((bad+1)); echo BAD $f $n; fi; done < xexp.txt; echo XA_OK=$ok XA_BAD=$bad; "
      "echo 1 > ours_acl; mkdir -p ours_dir; xattr -w user.ours hello small/f5000; xattr -W user.ourbig " + "62" * 600 + " small/f5000; echo BIG_RC=$?; "
      "xattr -w user.small v ours_acl; xattr -W system.posix_acl_access " + ACLA + " ours_acl; xattr -W system.posix_acl_default " + ACLD + " ours_dir; "
      "xattr -W security.capability 0100000200200000000000000000000000000000 ours_acl; xattr -w user.one 1 small/empty; xattr -d user.inl small/empty; echo XA_SET_RC=$?; "
      ": > xours.txt; for e in small/f5000:user.ours small/f5000:user.ourbig small/f5000:user.small ours_acl:user.small "
      "ours_acl:system.posix_acl_access ours_dir:system.posix_acl_default small/empty:user.one ours_acl:security.capability; do f=${{e%%:*}}; n=${{e##*:}}; "
      "echo $f $n $(xattr -X $n $f) >> xours.txt; done; xattr small/f5000; sync; cd /; echo XA_DONE")
# Linux check of what we wrote (runs inside the mounted image dir `m`)
XLINUX = ("while read f n h; do g=$(sudo getfattr --only-values -n $n m/$f | xxd -p | tr -d '\\n'); [ \"$g\" = \"$h\" ] || echo XBAD $f $n; done < m/xours.txt; "
          "sudo getfattr --only-values -n user.ourbig m/small/f5000 | wc -c; sudo getfattr -n user.inl m/small/empty 2>&1 | grep -c 'No such attr'; "
          "sudo getfacl -c m/ours_acl | grep -c 'user:1234:rw-'; sudo getfacl -c m/ours_dir | grep -c 'default:user:77:rwx'; sudo getcap m/ours_acl | grep -c cap_net_raw | sed 's/^/CAPOK=/'; "
          "for e in small/f5000:user.small small/f5000:user.mid small/f5000:user.big small/f5000:system.posix_acl_access small/empty:user.one dir1/sub:user.dirk dir1/sub:system.posix_acl_default; do f=${e%%:*}; n=${e##*:}; "
          "sudo getfattr --only-values -n $n m/$f >/dev/null 2>&1 || echo XGONE $f $n; done; echo LXX_DONE")


def inline_verify(v, tmp, outs_inl, outs_xa="", bigd=""):
    """Ship the guest-written image to the box: fsck rc 0, Linux mount, ours2.md5 + MANIFEST (minus
    what the step modified), stat every created name, Linux write, umount, fsck rc 0 again."""
    ok_g = "INLW_DONE" in outs_inl and "RMDIR_EMPTY_RC=0" in outs_inl and "RMDIR_NONEMPTY_RC=0" not in outs_inl \
        and "RMDIR_AFTER_RC=0" in outs_inl and "123" in outs_inl.split("RMDIR_AFTER_RC=0")[-1] and "5000" in outs_inl.split("RMDIR_AFTER_RC=0")[-1].replace("5000", "5000")
    step(v, "A.inline-write", ok_g, "append/overflow/rmdir ok in guest" if ok_g else outs_inl.strip()[-160:].replace("\n", " "))
    rd = f"/tmp/f2fsinterop-{os.getpid()}"
    try:
        box(f"mkdir -p {rd}")
        if not push(os.path.join(tmp, v + ".img"), f"{rd}/{v}.img"):
            step(v, "A.inline-ship", False, "push failed"); return
        fsck = lambda n: ("cd %s; timeout 180 fsck.f2fs --dry-run %s.img </dev/null >f%s.txt 2>&1; echo FSCK_RC=$?; "
                          "grep -ciE '\\[Fail\\]|ASSERT' f%s.txt; grep -iE 'fail|ASSERT|wrong|invalid' f%s.txt | head -n 2" % (rd, v, n, n, n))
        rc, out = box(fsck(1))
        step(v, "A.inline-fsck1", "FSCK_RC=0" in out and "\n0\n" in out, "fsck.f2fs rc 0 after inline writes" if "FSCK_RC=0" in out else out.strip()[-140:].replace("\n", " "))
        script = f"""cd {rd}; mkdir -p m; if sudo mount -o loop {v}.img m; then
 (cd m && md5sum -c ours2.md5 > /tmp/mm.$$ 2>&1; echo LXMD5A_RC=$?; grep -vE '{EXCL}' MANIFEST > /tmp/mf.$$; md5sum -c /tmp/mf.$$ > /tmp/mm2.$$ 2>&1; echo LXMD5B_RC=$?; tail -n 2 /tmp/mm2.$$
 ls dir1/sub | wc -l; stat -c %n small/newd/a dir1/sub/inline_dir_overflow_entry_name_120 >/dev/null; echo LXSTAT_RC=$?; stat -c %s small/inl3000; ls inl2 upg 2>&1 | head -n 3; rm -f /tmp/mm.$$ /tmp/mm2.$$ /tmp/mf.$$)
 {XLINUX}
 sudo sh -c 'echo fromlinux > m/dir1/sub/lx_new_entry; echo fromlinux >> m/small/inl3000'; sync; sudo umount m && echo LX_UMOUNT_OK
else echo LX_MOUNT_FAIL; sudo dmesg | tail -n 3 | cut -c1-150; fi"""
        if bigd:
            okb = "BIGD_LS=100000" in bigd and "BIGD_BAD=0" in bigd and "BIGD_PHANTOM" not in bigd and "BIGD_NEWLS=100000" in bigd \
                and "BIGD_RM_FAILED" not in bigd and "BIGD_DONE" in bigd
            step(v, "A.bigdir-guest", okb, "Linux 100k dir read (ls + 1500 lookups), 100k created by us" if okb else bigd.strip()[-200:].replace("\n", " "))
            extra = ("echo NB_LS=$(ls m/newbig | wc -l); nb=0; for i in 1 5 99 4096 7777 12345 50000 65535 99999 100000; do "
                     "stat m/newbig/f_$i >/dev/null 2>&1 || nb=$((nb+1)); done; echo NB_BAD=$nb; "
                     "stat m/newbig/f_7 >/dev/null 2>&1 && echo NB_F7_PRESENT; echo OB_LS=$(ls m/bigdir | wc -l)\n ")
            script = script.replace("sudo sh -c 'echo fromlinux", extra + "sudo sh -c 'echo fromlinux", 1)
        rc, out = box(script)
        if bigd:
            okl = "NB_LS=99999" in out and "NB_BAD=0" in out and "NB_F7_PRESENT" not in out and "OB_LS=100000" in out
            step(v, "A.bigdir-linux", okl, "Linux lists 99999 entries, stats names we hashed" if okl else out.strip()[-200:].replace("\n", " "))
        ok = ("LXMD5A_RC=0" in out and "LXMD5B_RC=0" in out and "LXSTAT_RC=0" in out and "LX_UMOUNT_OK" in out
              and "\n123\n" in out and "\n5000\n" in out)
        step(v, "A.inline-linux", ok, "Linux mounts, reads and extends our inline writes" if ok else out.strip()[-200:].replace("\n", " "))
        lxx = "LXX_DONE" in out and "XBAD" not in out and "XGONE" not in out and "\n600\n" in out and "CAPOK=1" in out
        step(v, "A.xattr-linux", lxx, "Linux reads our xattrs, spilled value and ACLs" if lxx else out.split("LXMD5B_RC")[-1].strip()[-420:].replace("\n", " "))
        xok = "XA_DONE" in outs_xa and "XA_BAD=0" in outs_xa and "XA_SET_RC=0" in outs_xa and "XA_OK=8" in outs_xa
        step(v, "A.xattr-guest", xok, "LeandrOS reads all 8 Linux xattrs/ACLs, sets ours" if xok else outs_xa.strip()[-160:].replace("\n", " "))
        rc, out = box(fsck(2))
        step(v, "A.inline-fsck2", "FSCK_RC=0" in out and "\n0\n" in out, "fsck.f2fs rc 0 after Linux write" if "FSCK_RC=0" in out else out.strip()[-140:].replace("\n", " "))
    finally:
        if not os.environ.get("F2FSINTEROP_KEEP"): box(f"rm -rf {rd}")


def direction_a(variants, tmp):
    rd = f"/tmp/f2fsinterop-{os.getpid()}"
    imgs, shas = [], {}
    try:
        for v in variants:
            mo, mnt, *sz = VARIANTS[v]
            size = sz[0] if sz else SIZE
            mnt = f",{mnt}" if mnt else ""
            script = f"""set -e; D={rd}; mkdir -p $D/m; cd $D; N={v}; rm -f $N.img
truncate -s {size} $N.img; mkfs.f2fs -q -f -l $N {mo} $N.img
sudo mount -o loop{mnt} $N.img m; cd m; sudo chown $(id -u) .
mkdir -p upg small dir1/sub big inl2/x; echo one > inl2/x/f; echo hello > small/inl; printf 'x%.0s' $(seq 3000) > small/inl3000
head -c 5000 /dev/urandom > small/f5000; : > small/empty
for i in $(seq 1 300); do echo "entry $i" > dir1/file_with_long_name_$i; done
for i in 1 2 3; do echo s$i > dir1/sub/s$i; done
head -c 3M /dev/urandom > big/b3m; head -c 12M /dev/urandom > big/b12m
dd if=/dev/urandom of=big/sparse bs=4k count=1 seek=2000 status=none
if [ "$N" = lxdir ]; then mkdir bigdir; (cd bigdir; seq 1 100000 | sed 's/^/e_/' | xargs -n 5000 touch); fi
ln -s ../small/inl small/sym; ln big/b3m big/hard; setfattr -n user.k -v val small/inl 2>/dev/null || true
setfattr -n user.small -v abc small/f5000; setfattr -n user.mid -v "$(head -c 150 /dev/zero | tr '\\0' m)" small/f5000
setfattr -n user.big -v "$(head -c 3000 /dev/zero | tr '\\0' a)" small/f5000; setfacl -m u:1234:rw,g:55:r small/f5000
setfattr -n user.inl -v hello small/empty; setfattr -n user.dirk -v dv dir1/sub; setfacl -d -m u:77:rwx dir1/sub
sudo setcap cap_net_raw+ep small/f5000 || sudo setfattr -n security.capability -v 0x0100000200200000000000000000000000000000 small/f5000
for e in small/f5000:user.small small/f5000:user.mid small/f5000:user.big small/f5000:system.posix_acl_access small/empty:user.inl dir1/sub:user.dirk dir1/sub:system.posix_acl_default small/f5000:security.capability; do
  f=${{e%%:*}}; n=${{e##*:}}; getfattr --only-values -n $n $f 2>/dev/null | xxd -p | tr -d '\\n' > /tmp/xv.$$; echo "$f $n $(cat /tmp/xv.$$)" >> xexp.txt; done; rm -f /tmp/xv.$$
find . -type f ! -name MANIFEST -print0 | sort -z | xargs -0 md5sum > MANIFEST
cd ..; sync; sudo umount m
set +e; timeout 120 fsck.f2fs --dry-run $N.img >$N.fsck0 2>&1; echo FSCK0_RC=$?"""
            rc, out = box(script)
            ok = rc == 0 and "FSCK0_RC=0" in out
            if not step(v, "A.linux-mkfs+fsck0", ok, "box mkfs/populate/fsck rc 0" if ok else out.strip()[-120:]):
                continue
            local = os.path.join(tmp, v + ".img")
            if not fetch(f"{rd}/{v}.img", local):
                step(v, "A.ship", False, "image transfer failed"); continue
            imgs.append(v); shas[v] = sha(local) if not sz else None
    finally:
        box(f"rm -rf {rd}")
    if not imgs:
        return
    # up to 16 disks per guest boot (blkmanytest covers 16 extra virtio-blk disks;
    # the old 8-device probe cap that forced batches of 4 is gone, openbugs #83)
    allimgs = imgs
    for ci in range(0, len(allimgs), 16):
        imgs = allimgs[ci:ci + 16]
        cmds = []
        for v in imgs:
            cmds += [MNT.format(l=v), md5chk(v, "MANIFEST"),
                     f"echo leandros > /mnt/{v}/ours.txt && cat /mnt/{v}/ours.txt; echo WRITE_RC=$?; sync",
                     INLW.format(l=v), XA.format(l=v)] + ([BIGD.format(l=v)] if v in BIGDIR else []) + [
                     f"umount /mnt/{v}; echo UMOUNT_RC=$?"]
        outs = guest([os.path.join(tmp, v + ".img") for v in imgs], cmds, "A")
        for i, v in enumerate(imgs):
            if outs is None:
                step(v, "A.mount", False, "guest run failed"); continue
            st = 6 + (1 if v in BIGDIR else 0)
            row = outs[sum(6 + (1 if x in BIGDIR else 0) for x in imgs[:i]):][:st]
            m, md, w, il, xa = row[:5]; u = row[-1]; bd = row[5] if v in BIGDIR else ""
            mounted = "MNT_RW" in m or "MNT_RO" in m
            step(v, "A.mount", mounted, "mounted" if mounted else "mount failed")
            if not mounted:
                for s_ in ("A.md5", "A.write", "A.ro-unchanged"):
                    step(v, s_, False, "not mounted")
                continue
            step(v, "A.md5", md5_ok(md), "MANIFEST ok" if md5_ok(md) else md.strip()[-100:])
            wok = "WRITE_RC=0" in w and "leandros" in w
            step(v, "A.write", wok, "write ok" if wok else "write refused (ro/EROFS)" if "Read-only" in w else "write failed")
            if wok:
                skip(v, "A.ro-unchanged", "volume was written")
                inline_verify(v, tmp, il, xa, bd)
            else:
                same = shas[v] is None or sha(os.path.join(tmp, v + ".img")) == shas[v]
                step(v, "A.ro-unchanged", same, "image bit-identical" if same else "image CHANGED by refused write/ro mount")


def f2fs_crc(d):
    """Linux f2fs_crc32: crc32_le seeded with the f2fs magic, no final xor."""
    return (~zlib.crc32(d, ~0xF2F52010 & 0xFFFFFFFF)) & 0xFFFFFFFF


def orphan_cp(path):
    """(CP_ORPHAN_PRESENT set, entry count of the first orphan block) of the
    newest checkpoint of a Linux image.  A Linux `sync` with an unlinked but
    open file writes exactly that (and no CP_UMOUNT_FLAG: a real umount reaps
    the orphan first), so LeandrOS sees the unclean-shutdown form."""
    with open(path, "rb") as f:
        f.seek(1024)
        sb = f.read(256)
        cp_blk = struct.unpack_from("<I", sb, 76)[0]
        bps = 1 << struct.unpack_from("<I", sb, 20)[0]
        best = None
        for pack in (0, 1):
            f.seek((cp_blk + pack * bps) * 4096)
            h = f.read(4096)
            ver = struct.unpack_from("<Q", h, 0)[0]
            if best is None or ver > best[0]:
                best = (ver, pack, h)
        _, pack, h = best
        flags = struct.unpack_from("<I", h, 132)[0]
        if not flags & 2 or flags & 1:
            return False, 0
        f.seek((cp_blk + pack * bps + 1) * 4096)
        return True, struct.unpack_from("<I", f.read(4096), 4088)[0]


def direction_orph(tmp):
    """Design f2fslxrw S7: a Linux sync checkpoint with CP_ORPHAN_PRESENT.  On the
    box a file is held open across `rm` and `sync`; the backing image is copied
    while mounted.  LeandrOS must mount it rw, reap the orphan at mount, and
    leave a volume that fsck.f2fs accepts and Linux mounts."""
    v = "orph"
    rd = f"/tmp/f2fsinterop-{os.getpid()}"
    img = os.path.join(tmp, "orph.img")
    script = f"""set -e; D={rd}; mkdir -p $D/m; cd $D; rm -f o.img
truncate -s {SIZE} o.img; mkfs.f2fs -q -f -l orph o.img
sudo mount -o loop o.img m; cd m; sudo chown $(id -u) .
mkdir keep; echo keepme > keep/k; head -c 600000 /dev/urandom > keep/big; head -c 300000 /dev/urandom > victim
( cd keep && md5sum k big > ../MANIFEST ); exec 3<victim; rm victim; sync; sleep 1; sync
cp ../o.img ../snap.img; exec 3<&-; cd ..; sudo umount m; mv snap.img o.img; echo SNAP_OK"""
    rc, out = box(script)
    if not step(v, "C.linux-orphan", rc == 0 and "SNAP_OK" in out, "box: rm while open + sync, image copied" if rc == 0 else out.strip()[-120:]):
        box(f"rm -rf {rd}"); return
    try:
        if not fetch(f"{rd}/o.img", img):
            step(v, "C.ship", False, "fetch failed"); return
        had, n = orphan_cp(img)
        if not step(v, "C.has-orphan-cp", had and n >= 1, f"CP_ORPHAN_PRESENT, {n} orphan(s), no umount flag" if had else "no orphan flag in the Linux checkpoint"):
            return
        outs = guest([img], [MNT.format(l="orph"), "cd /mnt/orph/keep && md5sum -c ../MANIFEST; echo keepme2 > k2; ls /mnt/orph | tr '\\n' ' '; sync; cd /; umount /mnt/orph; echo UMOUNT_RC=$?"], "C1")
        ok = outs is not None and "MNT_RW" in outs[0] and "UMOUNT_RC=0" in outs[1] and "victim" not in outs[1] and "FAILED" not in outs[1] and outs[1].count(": OK") >= 2
        if not step(v, "C.leandros-rw", ok, "mounted rw, orphan reaped path" if ok else (outs[0] + outs[1]).strip()[-120:] if outs else "no guest"):
            return
        box(f"mkdir -p {rd}"); push(img, f"{rd}/o.img")
        rc, out = box(f"""cd {rd}; timeout 180 fsck.f2fs --dry-run o.img </dev/null >f.txt 2>&1; echo FSCK_RC=$?
grep -ciE 'is unreachable|\\[Fail\\]|ASSERT' f.txt; grep -iE "unreachable|fail|ASSERT|mismatch|invalid|wrong|SIT|node count|valid" f.txt | head -n 30""")
        ok = "FSCK_RC=0" in out and "\n0\n" in out
        if os.environ.get("F2FSINTEROP_VERBOSE"):
            print(out[-1200:], file=sys.stderr)
        step(v, "C.fsck", ok, "fsck.f2fs rc 0, no unreachable NIDs" if ok else out.strip()[-140:].replace("\n", " "))
        rc, out = box(f"""cd {rd}; mkdir -p m; if sudo mount -o loop o.img m; then (cd m/keep && md5sum -c ../MANIFEST 2>&1 | tail -n 2; cat k2; echo LXREAD_RC=$?); ls m | tr '\\n' ' '; sync; sudo umount m && echo LX_UMOUNT_OK; else echo LX_MOUNT_FAIL; fi
timeout 180 fsck.f2fs --dry-run o.img </dev/null >f2.txt 2>&1; echo FSCK2_RC=$?""")
        ok = "LX_UMOUNT_OK" in out and "FAILED" not in out and "keepme2" in out and "FSCK2_RC=0" in out and "victim" not in out
        step(v, "C.linux-mount", ok, "Linux mounts, reads, fsck rc 0" if ok else out.strip()[-140:].replace("\n", " "))
    finally:
        box(f"rm -rf {rd}")


def cp_flags(path):
    """ckpt_flags of the newest checkpoint of a Linux image."""
    with open(path, "rb") as f:
        f.seek(1024)
        sb = f.read(256)
        cp_blk = struct.unpack_from("<I", sb, 76)[0]
        bps = 1 << struct.unpack_from("<I", sb, 20)[0]
        best = None
        for pack in (0, 1):
            f.seek((cp_blk + pack * bps) * 4096)
            h = f.read(4096)
            ver = struct.unpack_from("<Q", h, 0)[0]
            if best is None or ver > best[0]:
                best = (ver, h)
        return struct.unpack_from("<I", best[1], 132)[0]


FSYNC_SCENARIOS = [
    # (name, shell ops run in the mount root after a checkpoint, files in the MANIFEST)
    ("base", "dd if=/dev/urandom of=keep/newbig bs=1k count=5000 conv=fsync 2>/dev/null\n"
             "echo small-inline > keep/newsmall; fs keep/newsmall\n"
             "dd if=/dev/urandom of=keep/big bs=4k count=10 seek=5 conv=notrunc,fsync 2>/dev/null", "k big newbig newsmall"),
    ("indirect", "dd if=/dev/urandom of=keep/huge bs=1M count=40 conv=fsync 2>/dev/null", "k big huge"),
    ("xattr", 'V=$(printf "x%.0s" $(seq 1 700)); setfattr -n user.big -v "$V" keep/k; fs keep/k\n'
              'getfattr -n user.big --only-values keep/k | md5sum > ../xa.md5', "k big"),
    ("rename", "echo moved > keep/r0; sync; sleep 1; sync; mv keep/r0 keep/r1; fs keep/r1", "k big r1"),
    ("newdir", "mkdir keep/nd; echo indir > keep/nd/f; fs keep/nd/f", "k big nd/f"),
    # Linux check_index_in_prev_nodes: pa is checkpointed, then pb is fsynced and the image is forged so
    # that pb's chained inode points its first block at pa's first block (Linux itself will not reuse a
    # checkpoint-valid block before the next checkpoint, so the overlap cannot be produced honestly).
    # LeandrOS must drop pa's stale pointer: pa reads a hole there, pb reads pa's old block, fsck rc 0.
    ("prevowner", "head -c 1048576 /dev/urandom > keep/pa; sync; sleep 1; sync\n"
                  "head -c 40960 /dev/urandom > keep/pb; fs keep/pb\n"
                  "{ head -c 4096 keep/pa; tail -c +4097 keep/pb; } | md5sum | awk '{print $1\"  pb\"}' > ../EXP\n"
                  "{ head -c 4096 /dev/zero; tail -c +4097 keep/pa; } | md5sum | awk '{print $1\"  pa\"}' >> ../EXP", "k big", "forge_prevowner"),
]


def forge_prevowner(img):
    """Point the newest 'pb' inode's i_addr[0] at the newest 'pa' inode's i_addr[0]."""
    best = {}
    with open(img, "rb") as f:
        n = os.path.getsize(img) // 4096
        for i in range(n):
            f.seek(i * 4096)
            b = f.read(4096)
            nid, ino = struct.unpack_from("<II", b, 4072)
            if nid < 3 or nid != ino or b[88:92] != b"\x02\0\0\0" or b[92:94] not in (b"pa", b"pb"):
                continue
            ver = struct.unpack_from("<Q", b, 4084)[0]
            k = b[92:94]
            if k not in best or ver > best[k][0]:
                best[k] = (ver, i, b)
        if b"pa" not in best or b"pb" not in best:
            return False
        a0 = struct.unpack_from("<I", best[b"pa"][2], 360)[0]
        if a0 == 0:
            return False
        bb = bytearray(best[b"pb"][2])
        struct.pack_into("<I", bb, 360, a0)
        bb[348:360] = bytes(12)  # drop pb's i_ext hint, which still describes its old block
        f.close()
    with open(img, "r+b") as f:
        f.seek(best[b"pb"][1] * 4096)
        f.write(bytes(bb))
    return True


def direction_fsync(tmp):
    for name, ops, files, *forge in FSYNC_SCENARIOS:
        fsync_scenario(tmp, name, ops, files, globals()[forge[0]] if forge else None)


def fsync_scenario(tmp, name, ops, files, forge=None):
    """Linux roll-forward (recover_fsync_data): after a checkpoint, files are created
    and fsynced on the box (new files with a DENT mark, an in-place overwrite
    of a checkpointed file) and the backing image is copied while still mounted
    (no sysrq, no umount).  LeandrOS must mount it rw, roll the node chain
    forward (data present, md5), and leave a volume that fsck.f2fs accepts and
    Linux mounts."""
    v = f"fsync-{name}"
    rd = f"/tmp/f2fsinterop-{os.getpid()}"
    img = os.path.join(tmp, f"fsync-{name}.img")
    script = f"""set -e; D={rd}; mkdir -p $D/m; cd $D; rm -f f.img
truncate -s {SIZE} f.img; mkfs.f2fs -q -f -l fsync f.img
sudo mount -o loop f.img m; cd m; sudo chown $(id -u) .
mkdir keep; echo keepme > keep/k; head -c 600000 /dev/urandom > keep/big; sync; sleep 1; sync
fs() {{ python3 -c "import os,sys; f=os.open(sys.argv[1], os.O_RDONLY); os.fsync(f)" "$1"; }}
{ops}
( cd keep && {{ md5sum {files}; cat ../EXP 2>/dev/null || true; }} | dd of=../MANIFEST conv=fsync 2>/dev/null )
cp ../f.img ../snap.img; cd ..; sudo umount m; mv snap.img f.img; echo SNAP_OK"""
    rc, out = box(script)
    if not step(v, "D.linux-fsync", rc == 0 and "SNAP_OK" in out, "box: files created+fsynced after a checkpoint, image copied mounted" if rc == 0 else out.strip()[-120:]):
        box(f"rm -rf {rd}"); return
    try:
        if not fetch(f"{rd}/f.img", img):
            step(v, "D.ship", False, "fetch failed"); return
        if forge is not None and not step(v, "D.forge", forge(img), "forged: pb's first block now also pa's"):
            return
        fl = cp_flags(img)
        if not step(v, "D.unclean-cp", not fl & 1, "newest checkpoint has no CP_UMOUNT_FLAG" if not fl & 1 else "clean checkpoint"):
            return
        outs = guest([img], [MNT.format(l="fsync"), "cd /mnt/fsync/keep && md5sum -c ../MANIFEST 2>&1 ; echo LS=$(ls /mnt/fsync); echo keepme2 > k2; ls /mnt/fsync | tr '\\n' ' '; sync; cd /; umount /mnt/fsync; echo UMOUNT_RC=$?"], "D1")
        ok = outs is not None and "MNT_RW" in outs[0] and "UMOUNT_RC=0" in outs[1] and "FAILED" not in outs[1] and outs[1].count(": OK") >= len(files.split())
        if not step(v, "D.leandros-rw", ok, "mounted rw, fsynced files present" if ok else (outs[0] + outs[1]).strip()[-120:] if outs else "no guest"):
            return
        box(f"mkdir -p {rd}"); push(img, f"{rd}/f.img")
        rc, out = box(f"""cd {rd}; timeout 180 fsck.f2fs --dry-run f.img </dev/null >f.txt 2>&1; echo FSCK_RC=$?
grep -ciE 'is unreachable|\\[Fail\\]|ASSERT' f.txt; grep -iE "unreachable|fail|ASSERT|mismatch|invalid|wrong|SIT|node count|valid" f.txt | head -n 30""")
        ok = "FSCK_RC=0" in out and "\n0\n" in out
        if os.environ.get("F2FSINTEROP_VERBOSE"):
            print(out[-1200:], file=sys.stderr)
        step(v, "D.fsck", ok, "fsck.f2fs rc 0, no unreachable NIDs" if ok else out.strip()[-140:].replace("\n", " "))
        rc, out = box(f"""cd {rd}; mkdir -p m; if sudo mount -o loop f.img m; then (cd m/keep && md5sum -c ../MANIFEST 2>&1 | tail -n 2; cat k2; echo LXREAD_RC=$?; getfattr -n user.big --only-values k | md5sum | cmp - ../../xa.md5 && echo LX_XATTR_OK; ls nd); ls m | tr '\\n' ' '; sync; sudo umount m && echo LX_UMOUNT_OK; else echo LX_MOUNT_FAIL; fi
timeout 180 fsck.f2fs --dry-run f.img </dev/null >f2.txt 2>&1; echo FSCK2_RC=$?""")
        ok = "LX_UMOUNT_OK" in out and "FAILED" not in out and "keepme2" in out and "FSCK2_RC=0" in out and ("LX_XATTR_OK" in out or "xattr" not in name)
        step(v, "D.linux-mount", ok, "Linux mounts, reads, fsck rc 0" if ok else out.strip()[-140:].replace("\n", " "))
    finally:
        box(f"rm -rf {rd}")




# ---- direction E: LeandrOS fsync -> crash (QEMU quit, no umount) -> Linux (design f2fsnodelog T2) ----
# (name, guest ops run in /mnt/h after mounting; they print "EXP <md5>  <path>" lines, files the
#  chain must carry, min chain nodes marked FSYNC/DENT, chain-only: a no-recovery Linux mount must miss the content)
XV = "XV $(getfattr --only-values -n user.big t/k 2>/dev/null | md5sum | cut -c1-32)"
# name, ops (all fsyncs are real fsync(2): `f2fstest fsync` or dd conv=fsync, never `sync FILE`), minmark (chain nodes
# chaindump must see), cponly (P-b: what a norecovery mount of the CP shows), cpd (P-a: cp_total delta, None = any),
# chained (P-a: minimum lx_chained delta), files (inode numbers printed, P-c), pc (per file: "inode" | "data" = also recover_data),
# absent (files whose ino must NOT be replayed), xv (xattr digest compared on Linux)
ESCEN = [
    dict(name="append", ops="dd if=/dev/urandom bs=4k count=20 conv=notrunc,fsync oflag=append of=t/big 2>/dev/null; md5sum t/big",
         minmark=1, cponly="MISS", cpd=0, chained=1, pc={"t/big": "data"}),
    dict(name="newfile", ops="dd if=/dev/urandom of=t/nf bs=1k count=300 conv=fsync 2>/dev/null; md5sum t/nf",
         minmark=1, cponly="MISS", cpd=0, chained=1, pc={"t/nf": "data"}),
    dict(name="inline", ops="echo inl > t/ni; f2fstest fsync t/ni >/dev/null; md5sum t/ni",
         minmark=1, cponly="MISS", cpd=0, chained=1, pc={"t/ni": "inode"}),
    dict(name="overwrite", ops="dd if=/dev/urandom of=t/big bs=4k count=10 seek=5 conv=notrunc,fsync 2>/dev/null; md5sum t/big",
         minmark=1, cponly="PRESENT", cpd=0, chained=1, pc={"t/big": "inode"}),
    dict(name="overwrite-ext", ops="dd if=/dev/urandom of=t/big bs=4k count=10 seek=5 conv=notrunc 2>/dev/null; "
         "dd if=/dev/urandom bs=3k count=1 conv=notrunc,fsync oflag=append of=t/big 2>/dev/null; md5sum t/big",
         minmark=1, cponly="MISS", cpd=0, chained=1, pc={"t/big": "data"}),
    dict(name="indirect", ops="dd if=/dev/urandom of=t/huge bs=1M count=40 conv=fsync 2>/dev/null; md5sum t/huge",
         minmark=1, cponly="MISS", cpd=None, chained=1, pc={"t/huge": "data"}),
    dict(name="xattr", ops="V=$(printf 'x%.0s' $(seq 1 700)); setfattr -n user.big -v \"$V\" t/k && f2fstest fsync t/k >/dev/null; md5sum t/k; echo " + XV,
         minmark=1, cponly="MISS", cpd=0, chained=1, pc={"t/k": "inode"}, xv=True),
    dict(name="ab", ops="dd if=/dev/urandom of=t/A bs=1k count=100 conv=fsync 2>/dev/null; dd if=/dev/urandom of=t/B bs=1k count=100 2>/dev/null; md5sum t/A",
         minmark=1, cponly="MISS", cpd=0, chained=1, pc={"t/A": "data"}, absent=["t/B"]),
    dict(name="era", ops="dd if=/dev/urandom of=t/A bs=1k count=100 conv=fsync 2>/dev/null; dd if=/dev/urandom of=t/A bs=1k count=100 conv=notrunc 2>/dev/null; "
         "sync; dd if=/dev/urandom of=t/B bs=1k count=100 conv=fsync 2>/dev/null; md5sum t/A t/B",
         minmark=1, cponly="MISS", cpd=1, chained=2, pc={"t/B": "data"}, absent=["t/A"]),
    dict(name="full", ops="for i in $(seq 1 600); do echo $i > t/f$i; f2fstest fsync t/f$i >/dev/null; done; md5sum t/f*",
         minmark=1, cponly=None, cpd=None, chained=1, pc={}, fallback=1),
    dict(name="rename", ops="echo m > t/r0; sync; mv t/r0 t/r1; sync t/r1; md5sum t/r1",
         minmark=0, cponly=None, cpd=None, chained=0, pc={}),
    dict(name="trunc", ops="dd if=/dev/urandom of=t/tr bs=1M count=2 2>/dev/null; sync; truncate -s 100K t/tr; f2fstest fsync t/tr >/dev/null; md5sum t/tr",
         minmark=0, cponly=None, cpd=None, chained=0, pc={}),
    dict(name="xattrrm", ops="V=$(printf 'x%.0s' $(seq 1 700)); setfattr -n user.big -v \"$V\" t/k; sync; setfattr -x user.big t/k; f2fstest fsync t/k >/dev/null; md5sum t/k; echo " + XV,
         minmark=0, cponly=None, cpd=None, chained=0, pc={}, xv=True),
]

ECMD = os.environ.get("F2FSINTEROP_CMDLINE", "f2fs.lxchain=1")
STAT = "echo $(grep -E '^(cp_total|lx_chained|lx_fallback) ' /proc/f2fsstat | tr '\\n' ' ')"


def direction_e(tmp, only=None):
    for sc in ESCEN:
        if only and sc["name"] not in only:
            continue
        e_scenario(tmp, sc)


def stat_map(line):
    m = re.findall(r"(cp_total|lx_chained|lx_fallback) (\d+)", line)
    return {k: int(v) for k, v in m}


def e_scenario(tmp, sc):
    name, ops, minmark = sc["name"], sc["ops"], sc["minmark"]
    v = f"E-{name}"
    img, snap = os.path.join(tmp, f"e-{name}.img"), os.path.join(tmp, f"e-{name}.crash")
    r = subprocess.run([sys.executable, os.path.join(REPO, "scripts/mkfs-f2fs-home.py"), "create", img, "--size", "256M"], capture_output=True, text=True)
    if not step(v, "E.mkfs", r.returncode == 0, "mkfs-f2fs-home create"):
        return
    base = ("mkdir -p t; dd if=/dev/urandom of=t/big bs=4k count=150 2>/dev/null; echo keepme > t/k; sync; cd /; "
            "umount /mnt/h; echo UMOUNT_RC=$?")
    outs = guest([img], [MNTH, "cd /mnt/h && " + base], "E1", ECMD)
    if outs is None or "MNT_RW" not in outs[0] or "UMOUNT_RC=0" not in outs[1]:
        step(v, "E.boot1", False, "base volume failed"); return
    # boot 2: scenario, then the harness stops QEMU without umount (driver.cmd_stop = monitor quit)
    inos = sorted(set(list(sc["pc"]) + sc.get("absent", [])))
    ino_cmd = "; ".join(f"echo INO {f} $(stat -c %i {f})" for f in inos)
    outs = guest([img], [MNTH, "cd /mnt/h && (" + STAT.replace("echo ", "echo STAT0 ", 1) + "; " + ops + ") 2>&1; "
                         + STAT.replace("echo ", "echo STAT1 ", 1) + "; " + (ino_cmd + "; " if ino_cmd else "") + "echo SCEN_DONE; cd /"], "E2", ECMD)
    if outs is None or "MNT_RW" not in outs[0] or "SCEN_DONE" not in outs[1]:
        step(v, "E.boot2", False, (outs[1].strip()[-100:] if outs else "no guest")); return
    out2 = outs[1]
    exp = [l.strip().replace("t/", "") for l in out2.splitlines()
           if len(l.strip()) > 36 and l.strip()[32:34] == "  " and l.strip()[:32].isalnum() and l.strip()[34:36] == "t/"]
    if not exp:
        step(v, "E.boot2", False, "no md5 lines from the scenario: " + out2.strip()[-100:]); return
    # P-a: no checkpoint during the fsyncs, and the chain was used
    s0 = next((stat_map(l) for l in out2.splitlines() if l.startswith("STAT0")), {})
    s1 = next((stat_map(l) for l in out2.splitlines() if l.startswith("STAT1")), {})
    if s0 and s1 and ECMD:
        d = {k: s1.get(k, 0) - s0.get(k, 0) for k in s0}
        ok = d.get("lx_chained", 0) >= sc["chained"] and (sc["cpd"] is None or d.get("cp_total", -1) == sc["cpd"]) \
            and d.get("lx_fallback", 0) >= sc.get("fallback", 0)
        step(v, "E.stat", ok, f"cp_total +{d.get('cp_total')} (want {sc['cpd']}), lx_chained +{d.get('lx_chained')} (want >= {sc['chained']}), lx_fallback +{d.get('lx_fallback')}")
    elif ECMD:
        step(v, "E.stat", False, "no /proc/f2fsstat lines")
    ino_of = {m[0]: int(m[1]) for m in re.findall(r"INO (\S+) (\d+)", out2)}
    xv = re.search(r"XV ([0-9a-f]{32})", out2)
    shutil.copyfile(img, snap)
    fl = cp_flags(snap)
    cd = subprocess.run([sys.executable, os.path.join(HERE, "chaindump.py"), snap, "--quiet"] + (["--strict"] if ECMD else []), capture_output=True, text=True)
    m = [int(x.split("=")[1]) for x in cd.stdout.split() if x.startswith("marked=")]
    marked = m[0] if m else -1
    step(v, "E.chain", not fl & 1 and marked >= minmark and (marked >= 1 or minmark == 0) and cd.returncode == 0,
         f"chain marked={marked} (want >= {minmark}) cp_umount={fl & 1} dump rc={cd.returncode}")
    rd = f"/tmp/f2fsinterop-{os.getpid()}"
    mf = os.path.join(tmp, f"e-{name}.md5")
    open(mf, "w").write("\n".join(exp) + "\n")
    xchk = f"; echo XVL $(getfattr --only-values -n user.big {{m}}/t/k 2>/dev/null | md5sum | cut -c1-32)" if sc.get("xv") else ""
    xsh = (xchk.replace('{m}', 'm').lstrip('; ') + "; ") if xchk else ""
    try:
        box(f"mkdir -p {rd}")
        if not push(snap, f"{rd}/c.img") or not push(mf, f"{rd}/MANIFEST"):
            step(v, "E.ship", False, "push failed"); return
        # P-b: the checkpoint alone (norecovery) must lack what only the chain holds.
        rc, out = box(f"""cd {rd}; mkdir -p m; cp --sparse=always c.img ro.img; if sudo mount -o loop,ro,norecovery ro.img m; then (cd m/t && md5sum -c ../../MANIFEST >/tmp/cp.$$ 2>&1; echo CPONLY_RC=$?); {xsh}sudo umount m; else echo CPONLY_MNTFAIL; fi""")
        if sc["cponly"]:
            miss = "CPONLY_RC=" in out and "CPONLY_RC=0" not in out
            if sc.get("xv") and xv:
                miss = miss or (re.search(r"XVL ([0-9a-f]{32})", out) or [None, ""])[1] != xv.group(1)
            ok = miss if sc["cponly"] == "MISS" else ("CPONLY_RC=0" in out)
            step(v, "E.cponly", ok, f"norecovery mount of the CP: {'content missing' if miss else 'content present'} (want {sc['cponly']})")
        rc, out = box(f"""cd {rd}; cp --sparse=always c.img f.img; timeout 180 fsck.f2fs --dry-run -d 1 f.img </dev/null >fk.txt 2>&1; echo FSCK_RC=$?; grep -c 'do_record_fsync_data: \\[node\\]' fk.txt""")
        nrec = int(out.strip().splitlines()[-1]) if out.strip().splitlines()[-1].isdigit() else -1
        step(v, "E.fsck0", "FSCK_RC=0" in out and nrec >= max(1, minmark) * (minmark > 0), f"fsck rc0, {nrec} node records (want >= {minmark})")
        # P-c / P-d: Linux rw mount replays the chain (marker in dmesg), then content + fsck
        rc, out = box(f"""cd {rd}; mkdir -p m; cp --sparse=always c.img l.img; echo LXMARK_{os.getpid()} | sudo tee /dev/kmsg >/dev/null
if sudo mount -o loop l.img m; then (cd m/t && md5sum -c ../../MANIFEST 2>&1 | grep -c FAILED; ); {xsh}sudo dmesg | sed -n '/LXMARK_{os.getpid()}/,$p' > dm.txt; sync; sudo umount m && echo LX_UMOUNT_OK; else echo LX_MOUNT_FAIL; fi
timeout 180 fsck.f2fs --dry-run l.img </dev/null >f2.txt 2>&1; echo FSCK2_RC=$?; grep -E 'recover_(inode|data)' dm.txt | head -40""")
        ok = "LX_UMOUNT_OK" in out and out.strip().splitlines()[0].strip() == "0" and "FSCK2_RC=0" in out
        if ok and sc.get("xv") and xv:
            xl = re.search(r"XVL ([0-9a-f]{32})", out)
            ok = bool(xl) and xl.group(1) == xv.group(1)
        step(v, "E.linux", ok, "Linux recovery: content present, fsck rc 0" if ok else out.strip()[-140:].replace("\n", " "))
        if ECMD and sc["pc"] is not None and (sc["pc"] or sc.get("absent")):
            bad = []
            for f, kind in sc["pc"].items():
                h = "%x" % ino_of.get(f, -1)
                if not re.search(rf"recover_inode: ino = {h}\b", out): bad.append(f"{f}: no recover_inode")
                if kind == "data" and not re.search(rf"recover_data: ino = {h}\b.*recovered = [1-9]", out): bad.append(f"{f}: no recover_data")
            for f in sc.get("absent", []):
                if re.search(rf"recover_(inode|data): ino = {'%x' % ino_of.get(f, -1)}\b", out): bad.append(f"{f}: replayed")
            step(v, "E.replay", not bad, "dmesg shows the chain replayed per file" if not bad else "; ".join(bad) + " | " + " / ".join(l for l in out.splitlines() if "recover_" in l)[-160:])
        back = os.path.join(tmp, f"e-{name}.back")
        if ok and fetch(f"{rd}/l.img", back):
            ok2 = e_guest_check(back, exp, "E3")
            step(v, "E.back", ok2, "LeandrOS mounts Linux's recovered image, content ok" if ok2 else "guest check failed")
        else:
            skip(v, "E.back", "Linux recovery failed")
    finally:
        box(f"rm -rf {rd}")
    ok3 = e_guest_check(snap, exp, "E4")
    step(v, "E.ours", ok3, "LeandrOS recovers its own crash image (LRF1), content ok" if ok3 else "guest check failed")


def e_guest_check(image, exp, tag):
    """Mount image in the guest, re-create the manifest from the harness's md5 lines, verify."""
    mf = "; ".join(f"echo '{l}' >> /tmp/MF" for l in exp)
    outs = guest([image], [MNTH, "rm -f /tmp/MF; " + mf + "; cd /mnt/h/t && md5sum -c /tmp/MF 2>&1 | grep -c ': OK'; echo CHK_DONE; cd /"], tag)
    return bool(outs) and "MNT_FAIL" not in outs[0] and "CHK_DONE" in outs[1] and str(len(exp)) in [x.strip() for x in outs[1].splitlines()]


def direction_b(tmp):
    v = "home"
    img = os.path.join(tmp, "home.img")
    r = subprocess.run([sys.executable, os.path.join(REPO, "scripts/mkfs-f2fs-home.py"), "create", img, "--size", "256M"],
                       capture_output=True, text=True)
    if not step(v, "B.lx-mkfs", r.returncode == 0, "mkfs-f2fs-home create" if r.returncode == 0 else r.stderr.strip()[-100:]):
        return
    W = ("cd /mnt/h && mkdir -p w/d1/d2 w/many && dd if=/dev/urandom of=w/big bs=4096 count=3000 2>/dev/null && "
         "for i in $(seq 1 300); do echo f$i > w/many/file_with_long_name_$i; done && echo hi > w/d1/d2/s && "
         "ln w/d1/d2/s w/hard && ln -s d1/d2/s w/sym && mv w/many/file_with_long_name_7 w/d1/moved && "
         "rm w/many/file_with_long_name_1* && dd if=/dev/urandom of=w/big bs=4096 count=10 seek=5 conv=notrunc 2>/dev/null && "
         "(cd w && md5sum big d1/moved d1/d2/s > ours.md5); ls w/many | wc -l; sync; cd /; echo WORK_OK")
    outs = guest([img], [MNTH, W, "umount /mnt/h; echo UMOUNT_RC=$?"], "B1")
    if outs is None or "MNT_RW" not in outs[0] or "WORK_OK" not in outs[1] or "UMOUNT_RC=0" not in outs[2]:
        step(v, "B.work", False, "LeandrOS mount/workload/umount failed"); return
    step(v, "B.work", True, "workload + umount ok")
    rd = f"/tmp/f2fsinterop-{os.getpid()}"
    try:
        box(f"mkdir -p {rd}")
        if not push(img, f"{rd}/home.img"):
            step(v, "B.ship", False, "push failed"); return
        fsck = lambda n: ("cd %s; timeout 180 fsck.f2fs --dry-run home.img </dev/null >f%s.txt 2>&1; echo FSCK_RC=$?; "
                          "grep -ciE '\\[Fail\\]|ASSERT' f%s.txt; grep -iE 'fail|ASSERT|wrong|invalid' f%s.txt | head -n 2" % (rd, n, n, n))
        rc, out = box(fsck(1))
        step(v, "B.fsck1", "FSCK_RC=0" in out and "\n0\n" in out, "fsck.f2fs rc 0" if "FSCK_RC=0" in out else out.strip()[-120:].replace("\n", " "))
        script = f"""cd {rd}; mkdir -p m; if sudo mount -o loop home.img m; then
 (cd m/w && md5sum -c ours.md5 > /tmp/mm.$$ 2>&1; echo LXMD5_RC=$?; tail -n 2 /tmp/mm.$$; ls many | wc -l; stat -c %n many/file_with_long_name_299 d1/moved >/dev/null; echo LXSTAT_RC=$?; rm -f /tmp/mm.$$)
 sudo sh -c 'echo fromlinux > m/w/linux.txt; head -c 1M /dev/urandom > m/w/lbig; md5sum m/w/lbig | sed s,m/w/,, > m/w/linux.md5'; sync; sudo umount m && echo LX_UMOUNT_OK
else echo LX_MOUNT_FAIL; sudo dmesg | tail -n 3 | cut -c1-150; fi"""
        rc, out = box(script)
        ok = "LXMD5_RC=0" in out and "LXSTAT_RC=0" in out and "LX_UMOUNT_OK" in out
        step(v, "B.linux-rw", ok, "Linux mount+verify+write+umount" if ok else out.strip()[-140:].replace("\n", " "))
        rc, out = box(fsck(2))
        step(v, "B.fsck2", "FSCK_RC=0" in out and "\n0\n" in out, "fsck.f2fs rc 0 after Linux write" if "FSCK_RC=0" in out else out.strip()[-120:].replace("\n", " "))
        if not ok:
            skip(v, "B.back-verify", "Linux write step failed"); return
        if not fetch(f"{rd}/home.img", img):
            step(v, "B.ship-back", False, "fetch failed"); return
    finally:
        box(f"rm -rf {rd}")
    outs = guest([img], [MNTH,
                         "cd /mnt/h/w && cat linux.txt; md5sum -c linux.md5 2>&1; md5sum -c ours.md5 2>&1; echo END; cd /; umount /mnt/h"], "B2")
    if outs is None or "MNT_FAIL" in outs[0]:
        step(v, "B.back-verify", False, "LeandrOS cannot mount the Linux-written volume"); return
    ok = outs is not None and "fromlinux" in outs[1] and "FAILED" not in outs[1] and "WARNING" not in outs[1] and outs[1].count(": OK") >= 4
    step(v, "B.back-verify", ok, "LeandrOS reads Linux's files" if ok else "verify failed: " + (outs[1].strip()[-100:] if outs else "no guest"))


def main(argv):
    if argv[:1] == ["--guest"]:
        return guest_main(argv[1])
    allv = list(VARIANTS) + ["home", "orph", "fsync", "fsynce"]
    if not argv or argv[0] in ("-h", "--help", "--list"):
        print(__doc__ if argv[:1] != ["--list"] else " ".join(allv)); return 0
    vs = allv if argv == ["all"] else argv
    bad = [v for v in vs if v not in allv]
    if bad:
        print("unknown variant:", bad, "known:", allv); return 2
    tmp = tempfile.mkdtemp(prefix="f2fsinterop-")
    try:
        a = [v for v in vs if v in VARIANTS]
        if a:
            direction_a(a, tmp)
        if "home" in vs:
            direction_b(tmp)
        if "orph" in vs:
            direction_orph(tmp)
        if "fsync" in vs:
            direction_fsync(tmp)
        if "fsynce" in vs:
            direction_e(tmp, os.environ.get("F2FSINTEROP_E", "").split() or None)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    bad = sum(1 for r in results if r in ("FAIL", "UNEXPECTED-PASS"))
    print(f"summary: {results.count('PASS')} PASS, {results.count('EXPECTED-FAIL')} EXPECTED-FAIL, {bad} FAIL/UNEXPECTED-PASS")
    return bad


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
