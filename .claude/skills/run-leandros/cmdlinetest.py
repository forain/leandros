#!/usr/bin/env python3
"""The kernel command line is not silently truncated (lane cmdline, 2026-11-21).

Before this lane the kernel kept only 512 bytes of the bootloader's command
line, so a knob at the END of a long line (e.g. an iwlwifi.* knob after root=,
blk.rw=, console and display options) never reached its driver, with no
warning. The limit is now boot::COMMAND_LINE_SIZE (2048 like Linux) and a
longer line is cut at a token boundary with a logged warning.

Boots the Limine image twice with a rewritten `cmdline:`:
  1. ~1700 bytes of pad knobs, last token test.cmdline_tail=<marker>:
     /proc/cmdline is long and ends with the marker (nothing lost).
  2. ~2300 bytes: /proc/cmdline fits the limit, the marker is gone, and the
     kernel log carries `cmdline truncated at N of M bytes`.
Usage: LEANDROS_RUN_ID=<lane> python3 cmdlinetest.py [aarch64|x86_64]
"""
import os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import imgclone
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
LIMIT = 2047


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def pad_line(total, marker):
    toks = []
    i = 0
    while len(" ".join(toks)) < total - 40:
        toks.append(f"test.pad{i:03d}=0123456789abcdef")
        i += 1
    toks.append(f"test.cmdline_tail={marker}")
    return " ".join(toks)


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="cmdline-")
    fails = []

    def check(name, cond, detail=""):
        print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" ({detail})" if detail and not cond else ""))
        if not cond:
            fails.append(name)

    def boot_with(line, tag):
        img = os.path.join(tmp, f"boot-{tag}.img")
        root = os.path.join(tmp, f"root-{tag}.img")
        for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                         (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
            if subprocess.run(["cp", "-c", src, dst], capture_output=True).returncode != 0:
                imgclone.clone_file(src, dst)
        r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, line], capture_output=True, text=True)
        if r.returncode != 0:
            return None, None, r.stdout + r.stderr
        env = dict(os.environ)
        env.update({"LEANDROS_RUN_ID": f"{caller}-cmdline{tag}" if caller else f"cmdline{tag}",
                    "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root})
        rc, out = drv("start", arch, timeout=300, env=env)
        if rc != 0:
            return None, None, out[-300:]
        try:
            rc, out = drv("login", "root", "root", "180", timeout=400, env=env)
            if rc != 0:
                return None, None, out[-300:]
            _, cl = drv("run", "cat /proc/cmdline", "60", timeout=120, env=env)
            _, log = drv("log", timeout=60, env=env)
            # `run` output carries the echoed command and prompt: keep the longest line.
            cl = max(cl.replace("\r", "").splitlines() or [""], key=len)
            return cl, log, ""
        finally:
            try: drv("stop", timeout=60, env=env)
            except Exception: pass
            for f in (img, root):
                try: os.unlink(f)
                except OSError: pass

    try:
        m1 = "TAILOK1500"
        line = pad_line(1700, m1)
        check("test line is >1500 bytes", len(line) > 1500, str(len(line)))
        cl, log, err = boot_with(line, "a")
        check("boot a (1700 B cmdline)", cl is not None, err)
        if cl is not None:
            check("/proc/cmdline carries the last token", f"test.cmdline_tail={m1}" in cl, cl[-120:])
            check("/proc/cmdline is long", cl.count("test.pad") > 50, str(len(cl)))
            check("no truncation warning", "cmdline truncated" not in log)
        m2 = "TAILGONE"
        line = pad_line(2300, m2)
        cl, log, err = boot_with(line, "b")
        check("boot b (2300 B cmdline)", cl is not None, err)
        if cl is not None:
            check("over-long line is cut to the limit", len(cl.strip()) <= LIMIT, str(len(cl)))
            check("cut line lost the tail (so the warning matters)", f"tail={m2}" not in cl)
            check("root= survives (it comes first)", "root=" in cl)
            w = re.search(r"cmdline truncated at (\d+) of (\d+) bytes", log)
            check("log says 'cmdline truncated at N of M bytes'", bool(w), log[-200:])
            if w:
                check("M is the real length", int(w.group(2)) > len(line), w.group(0))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"cmdlinetest {arch}: {'PASS' if not fails else 'FAIL ' + ', '.join(fails)}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
