#!/usr/bin/env python3
# M9 item-9 controls: drmsmoke (must be 22/0), idletest, and vfstest EXACTLY
# ONCE against the fresh image. Boots, logs in as root, runs them in order.
# vfstest goes FIRST so its own results are against a pristine f2fs image.
import subprocess, sys, os, time, re

DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
SERIAL = "/tmp/leandros-serial.log"
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m9-fb-damage-clips")
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "regress"


def d(*a, t=260, env=None):
    e = dict(os.environ); e.update(env or {})
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True,
                           text=True, timeout=t, env=e)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def log(*a):
    print(*a, flush=True)


def clean():
    d("stop", t=30)
    subprocess.run(["pkill", "-9", "-f", "qemu-system"], capture_output=True)
    time.sleep(2)


def main():
    log(f"==== M9 controls {ARCH} tag={TAG} {time.ctime()} ====")
    try:
        os.remove(SERIAL)
    except OSError:
        pass

    booted, out = False, ""
    for attempt in (1, 2):
        log(f"#### BOOT {attempt} ####")
        clean()
        out = d("start", ARCH, "uefi", t=220)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True
            break
    if not booted:
        log("NO BOOT"); log(out[-1500:]); clean(); return 1

    d("login", "root", "root", t=45)

    # vfstest FIRST and EXACTLY ONCE against the fresh f2fs image.
    for name, cmd, tmo in (("vfstest", "/bin/vfstest", 180),
                           ("drmsmoke", "/bin/drmsmoke", 90),
                           ("idletest", "/bin/idletest", 120)):
        log(f"\n######## {name} ########")
        t = d("session", str(tmo), cmd, t=tmo + 60)
        open(f"{OUT}/{TAG}-{name}.txt", "w").write(t)
        clean_t = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", t)
        for l in clean_t.splitlines():
            if re.search(r"PASS|FAIL|passed|failed|TOTAL|Summary|===", l):
                log("  " + l[:200])

    clean()
    return 0


if __name__ == "__main__":
    sys.exit(main())
