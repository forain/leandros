"""A slow virtio-blk request must not kill the disk; a page-in I/O error is
SIGBUS (lane/blkdead, 2026-10-25, openbugs L12 #50).
Boots a private image copy with test knobs:
  virtio_blk.inject_delay=N:3000  the N-th request answers 3 s late
  virtio_blk.slow_ms=1000         slow-request threshold 1 s (default 30 s)
  mm.pagein_eio=hello             every file page-in of 'hello' fails (EIO)
Until 2026-10-25 a request past the threshold marked the disk dead (f2fs went
read-only, later page-ins SEGV'd), and an ELF page-in error was SIGSEGV.
Checks: the delayed request completes, the disk stays writable, no 'disk
marked dead' / read-only line, and /bin/hello dies of SIGBUS (status 135).
Usage: LEANDROS_RUN_ID=<id> python3 blkslowtest.py [aarch64|x86_64]
"""
import imgclone
import os, shutil, subprocess, sys
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
LOG = f"/tmp/leandros{'-' + RUN_ID if RUN_ID else ''}-serial-full.log"
NTH = os.environ.get("BLKSLOW_NTH", "1500")
CMDLINE = f"virtio_blk.inject_delay={NTH}:3000 virtio_blk.slow_ms=1000 mm.pagein_eio=hello"


def drv(*args, timeout=200, env=None):
    try:
        p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                           timeout=timeout, env=env, cwd=REPO)
    except subprocess.TimeoutExpired:
        return 124, f"driver {args[0]} timed out after {timeout}s"
    return p.returncode, p.stdout + p.stderr


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"blkslow-{RUN_ID or arch}")
    os.makedirs(out, exist_ok=True)
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    img = os.path.join(out, f"blkslow-{arch}.img")
    if os.path.exists(img): os.unlink(img)
    if subprocess.run([*imgclone.CPC, src, img]).returncode != 0:
        shutil.copyfile(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, CMDLINE],
                   check=True, stdout=subprocess.DEVNULL)
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img)
    ok = True
    drv("stop", timeout=120)
    try:
        rc, o = drv("start", arch, timeout=400, env=env)
        if rc != 0:
            print("FAIL: boot\n" + o[-800:]); ok = False
        elif drv("login", "root", "root", "240", timeout=300)[0] != 0:
            print("FAIL: root login"); ok = False
        else:
            # Enough I/O to pass request N if boot did not.
            rc, o = drv("run", "dd if=/dev/zero of=/root/blkslow.dat bs=64k count=64 && sync && "
                        "cat /bin/* > /dev/null 2>&1; rm -f /root/blkslow.dat; sync; "
                        "echo alive > /root/blkslow.ok && sync && cat /root/blkslow.ok", "240", arch, env=env, timeout=300)
            print(f"disk write/read rc={rc} {o.strip().splitlines()[-2:] if o.strip() else ''}")
            if rc != 0 or "alive" not in o:
                ok = False; print("FAIL: disk not writable after the delayed request")
            rc, o = drv("run", "/bin/hello; echo hello_status=$?", "60", arch, env=env, timeout=120)
            st = [l for l in o.splitlines() if l.startswith("hello_status=")]
            print(st[-1] if st else "no hello_status line")
            if not st or st[-1] != "hello_status=135":
                ok = False; print("FAIL: page-in I/O error was not SIGBUS (want 135 = 128+SIGBUS)")
        log = open(LOG, "rb").read().decode("utf-8", "replace").splitlines()
        hits = [l for l in log if "[VIRTIO-BLK]" in l or "mount is now read-only" in l]
        print("\n".join(hits[:8]) or "no VIRTIO-BLK lines")
        if not any("inject: request" in l for l in hits):
            ok = False; print(f"FAIL: request #{NTH} never issued (lower BLKSLOW_NTH)")
        if any("marked dead" in l or "read-only" in l for l in hits):
            ok = False; print("FAIL: a slow request killed the disk")
        if not any("completed after" in l for l in hits):
            ok = False; print("FAIL: no 'completed after' line for the slow request")
    finally:
        drv("stop", timeout=60)
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
