#!/usr/bin/env python3
"""M9: is the aarch64 vfstest `xattr_list_f2fs` failure caused by the epoll fix?

ONE blocking script. No watchers, no pollers.

Hypothesis under test: the failure is the recorded DIRTY-IMAGE artifact —
vfstest fails `xattr_list_f2fs` iff vfstest has ALREADY run against that image
(O_TRUNC clears file data but not xattrs, and /data survives reboots). In the
wave that produced the 35/1, the aarch64 image had vfstest run against it TWICE
(once by an earlier suite, once by the final one); the x86_64 image had it run
ONCE and scored 36/0.

Design: each phase boots a FRESH image and runs vfstest TWICE in the same boot.
The predicted signature of the artifact is run1=36/0, run2=35/1. If both phases
show that signature, the failure is a property of the image, not of the kernel
change, and the epoll fix is exonerated.

  Phase A — fix IN place
  Phase B — fix REVERTED (the control the coordinator asked for)
  Phase C — fix restored + rebuilt, so the tree and images end consistent

Phase C always runs, even if an earlier phase fails, so the tree is never left
holding a reverted fix.
"""
import os
import re
import subprocess
import sys
import time

REPO = os.path.expanduser("~/code/leandros")
ART = os.path.expanduser("~/code/leandros-artifacts")
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{ART}/scmrun.py"
SCRATCH = ("/private/tmp/claude-501/-Users-forain-code-leandros/"
           "07b19cad-edf7-479d-84b0-21f06bc8ec0a/scratchpad")
RESULTS = f"{SCRATCH}/m9-vfstest-control.txt"
FILES = ["kernel/src/syscall.rs", "userland/epolltest/src/main.rs"]


def emit(line):
    print(line, flush=True)
    with open(RESULTS, "a") as f:
        f.write(line + "\n")


def sh(argv, cwd=REPO, timeout=3000):
    r = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def kill_qemu():
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(3)


def rebuild_aarch64(tag):
    """Fresh kernel + FRESH f2fs image."""
    img = f"{REPO}/f2fs-data0-aarch64.img"
    if os.path.exists(img):
        os.remove(img)
    rc, out = sh(["./scripts/build-all.sh", "--arch", "aarch64"], timeout=3000)
    ok = "Build Complete" in out and os.path.exists(img)
    emit(f"  [{tag}] rebuild rc={rc} fresh_image={os.path.exists(img)} ok={ok}")
    if not ok:
        emit("    " + "\n    ".join(out.splitlines()[-8:]))
    return ok


def parse_vfstest(txt):
    clean = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", re.sub(r"\x1b[=>78]", "", txt))
    npass = len(re.findall(r"\bPASS\b", clean))
    fails = re.findall(r"^(\w+): FAIL", clean, re.M)
    return npass, fails, clean


def phase(tag, expect_note):
    """Boot a fresh image, run vfstest twice in the SAME boot."""
    emit(f"\n===== {tag} ({expect_note}) =====")
    kill_qemu()
    rc, out = sh(["python3", "-u", DRIVER, "start", "aarch64", "uefi"], timeout=400)
    if not any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
        emit(f"  [{tag}] NO BOOT")
        emit("    " + "\n    ".join(out.splitlines()[-6:]))
        kill_qemu()
        return None
    sh(["python3", "-u", DRIVER, "login", "root", "root"], timeout=90)
    subprocess.run(["python3", SCMRUN, "echo WARMUP", "4"], capture_output=True, timeout=60)

    results = []
    for i in (1, 2):
        r = subprocess.run(["python3", SCMRUN, "vfstest", "70"],
                           capture_output=True, text=True, timeout=140)
        npass, fails, clean = parse_vfstest((r.stdout or "") + (r.stderr or ""))
        with open(f"{SCRATCH}/m9ctl-{tag}-vfstest-run{i}.txt", "w") as f:
            f.write(clean)
        emit(f"  [{tag}] vfstest run{i}: PASS={npass} FAIL={len(fails)} "
             f"failed={fails if fails else 'none'}")
        results.append((npass, fails))
    kill_qemu()
    return results


def main():
    open(RESULTS, "w").write(f"==== M9 vfstest dirty-image control {time.ctime()} ====\n")
    emit("Predicted artifact signature per phase: run1=36/0, run2=35/1 xattr_list_f2fs")

    # Preserve the working-tree change out-of-band; git stash pop can conflict,
    # a plain file copy cannot.
    backup = {}
    for rel in FILES:
        backup[rel] = open(f"{REPO}/{rel}").read()
        with open(f"{SCRATCH}/backup-{os.path.basename(rel)}", "w") as f:
            f.write(backup[rel])
    emit(f"backed up {len(FILES)} modified files")

    a = b = None
    try:
        # Phase A — fix IN place.
        if rebuild_aarch64("A/fix-in"):
            a = phase("A-fix-in", "epoll fix PRESENT")

        # Phase B — control: fix reverted.
        rc, out = sh(["git", "checkout", "--"] + FILES)
        emit(f"\nreverted fix: rc={rc}; git status now:")
        _, st = sh(["git", "status", "--short"])
        emit("  " + (st.strip() or "(clean)"))
        if rebuild_aarch64("B/fix-out"):
            b = phase("B-fix-out", "epoll fix ABSENT — control")
    finally:
        # Phase C — always restore, even on failure above.
        for rel in FILES:
            with open(f"{REPO}/{rel}", "w") as f:
                f.write(backup[rel])
        emit("\nrestored the epoll fix into the working tree")
        rebuild_aarch64("C/restore")
        _, st = sh(["git", "status", "--short"])
        emit("final git status:\n  " + (st.strip() or "(clean)"))
        for rel, pat in (("kernel/src/syscall.rs", "const DBG_SERIAL_WRITE: bool = false;"),
                         ("kernel/src/syscall.rs", "const EPOLL_MAX_NEST: u32 = 4;")):
            present = pat in open(f"{REPO}/{rel}").read()
            emit(f"  check {rel!r} contains {pat!r}: {present}")

    emit("\n===== VERDICT INPUTS =====")
    emit(f"  A (fix in):  {a}")
    emit(f"  B (fix out): {b}")
    if a and b:
        sig = lambda r: (r[0][0], r[0][1], r[1][0], r[1][1])
        emit(f"  A signature: run1 PASS={a[0][0]} fails={a[0][1]} | "
             f"run2 PASS={a[1][0]} fails={a[1][1]}")
        emit(f"  B signature: run1 PASS={b[0][0]} fails={b[0][1]} | "
             f"run2 PASS={b[1][0]} fails={b[1][1]}")
        emit(f"  IDENTICAL WITH AND WITHOUT THE FIX: {sig(a) == sig(b)}")
    emit(f"\n==== DONE {time.ctime()} ====")


if __name__ == "__main__":
    sys.exit(main())
