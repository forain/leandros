#!/usr/bin/env python3
# Console-vs-scanout verification.
#
# One boot per arch:
#   1. positive control (`nosuchbinary_xyz42`) — the harness MUST report it failing,
#      so "absent" and "failing" are distinguishable for everything after it;
#   2. vfstest, exactly once on this image;
#   3. drmsmoke — carries FB0_SHOWS_SCANOUT + CONSOLE_YIELDS_TO_SCANOUT, the
#      in-guest byte-identity census of the shared surface;
#   4. screendump after drmsmoke exits — proves the console reclaimed the scanout;
#   5. the rest of the suite;
#   6. drmsmoke --hold in the FOREGROUND (x86_64 produced no serial output when
#      backgrounded), then screendump + exact pixel census of the held image.
#
# Usage: m10_console_scanout.py <arch> <tag>
import subprocess, sys, os, time, re, hashlib

HOME = os.path.expanduser("~")
DRIVER = f"{HOME}/code/leandros/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{HOME}/code/leandros/scripts/scmrun.py"
OUT = sys.argv[3] if len(sys.argv) > 3 else "/tmp/m10"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "fixed"
os.makedirs(OUT, exist_ok=True)

SOCK = "/tmp/leandros-serial.sock"
PIDF = "/tmp/leandros-qemu.pid"

SUITE = [("scmtest", 55), ("forktest", 25), ("epolltest", 30), ("polltest", 30),
         ("waittest", 40), ("sigtest", 25), ("timertest", 30), ("memtest", 25)]


def log(*a):
    print(*a, flush=True)


def d(*a, t=240):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def scm(cmd, dur):
    try:
        r = subprocess.run(["python3", SCMRUN, cmd, str(dur)], capture_output=True,
                           text=True, timeout=dur + 60)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {cmd})"


def clean_ansi(t):
    return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', t))


def clean():
    d("stop", t=40)
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]"], capture_output=True)
    time.sleep(2)


def vm_alive():
    if not os.path.exists(SOCK):
        return False
    try:
        pid = int(open(PIDF).read().strip())
    except Exception:
        return True
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    if not data.startswith(b"P6"):
        return None
    idx, vals = 2, []
    while len(vals) < 3:
        while idx < len(data) and data[idx:idx + 1].isspace():
            idx += 1
        if data[idx:idx + 1] == b"#":
            while idx < len(data) and data[idx:idx + 1] != b"\n":
                idx += 1
            continue
        s = idx
        while idx < len(data) and not data[idx:idx + 1].isspace():
            idx += 1
        vals.append(int(data[s:idx]))
    w, h, _ = vals
    return w, h, data[idx + 1:]


def census(path, label):
    """Exact colour census of a screendump, plus its md5."""
    md5 = hashlib.md5(open(path, "rb").read()).hexdigest()
    r = read_ppm(path)
    if r is None:
        log(f"  [{label}] NOT P6 -- md5={md5}")
        return
    w, h, px = r
    counts = {}
    for i in range(w * h):
        o = i * 3
        if o + 3 > len(px):
            break
        counts[(px[o], px[o + 1], px[o + 2])] = counts.get((px[o], px[o + 1], px[o + 2]), 0) + 1
    top = sorted(counts.items(), key=lambda kv: -kv[1])[:5]
    log(f"  [{label}] {w}x{h} md5={md5} distinct_colours={len(counts)} total={w*h}")
    for (cr, cg, cb), n in top:
        log(f"      #{cr:02x}{cg:02x}{cb:02x} = {n}")
    log(f"      0xff0000={counts.get((255,0,0),0)}  0x181818={counts.get((24,24,24),0)}")


def shot(name):
    p = f"{OUT}/m10-{TAG}-{ARCH}-{name}.ppm"
    d("screenshot", p, t=60)
    if os.path.exists(p):
        census(p, name)
    else:
        log(f"  [{name}] no screendump produced")
    return p


def main():
    log(f"==== m10 console-vs-scanout {ARCH} tag={TAG} {time.ctime()} ====")
    booted = False
    out = ""
    for attempt in (1, 2):
        log(f"#### BOOT {attempt} ####")
        clean()
        out = d("start", ARCH, "uefi", t=240)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True
            break
    if not booted:
        log("NO BOOT"); log(out[-2000:]); clean(); return
    d("login", "root", "root", t=45)
    scm("echo WARMUP", 4)

    # 1. Positive control — must be reported as FAILING, not absent.
    ctl = clean_ansi(scm("nosuchbinary_xyz42", 10))
    open(f"{OUT}/m10-{TAG}-{ARCH}-control.txt", "w").write(ctl)
    ctl_ok = any(m in ctl for m in ("not found", "No such file", "cannot", "command not found"))
    log(f"  [POSITIVE CONTROL] harness reports failure = {ctl_ok}")

    shot("boot")

    # 2. vfstest — exactly once per image.
    vt = clean_ansi(scm("vfstest", 70))
    open(f"{OUT}/m10-{TAG}-{ARCH}-vfstest.txt", "w").write(vt)
    log(f"  [vfstest] PASS={len(re.findall(r'\\bPASS\\b', vt))} FAIL={len(re.findall(r'\\bFAIL\\b', vt))}")

    # 3. drmsmoke — the census lives here.
    # x86_64 on an ARM host is TCG; the census reads 8.3 MB through /dev/fb0
    # twice, so give it room rather than truncating the log into a clean grep.
    ds = clean_ansi(scm("drmsmoke", 240 if ARCH == "x86_64" else 90))
    open(f"{OUT}/m10-{TAG}-{ARCH}-drmsmoke.txt", "w").write(ds)
    for ln in ds.splitlines():
        if any(k in ln for k in ("CONSOLE_YIELDS", "FB0_SHOWS_SCANOUT", "SETCRTC:", "drmsmoke done")):
            log(f"      | {ln.strip()}")
    log(f"  [drmsmoke] PASS={len(re.findall(r'\\bPASS\\b', ds))} FAIL={len(re.findall(r'\\bFAIL\\b', ds))}")

    # 4. The console must have reclaimed the scanout when drmsmoke closed card0.
    shot("after-drmsmoke")

    # 5. Rest of the suite.
    summary = []
    for cmd, dur in SUITE:
        if not vm_alive():
            log(f"  [{cmd}] VM GONE"); break
        t = clean_ansi(scm(cmd, dur))
        open(f"{OUT}/m10-{TAG}-{ARCH}-{cmd}.txt", "w").write(t)
        p = len(re.findall(r'\bPASS\b', t)); f = len(re.findall(r'\bFAIL\b', t))
        summary.append((cmd, p, f))
        log(f"  [{cmd}] PASS={p} FAIL={f}")

    # 6. --hold, foreground, then photograph the held image.
    if vm_alive():
        hd = clean_ansi(scm("drmsmoke --hold", 200 if ARCH == "x86_64" else 60))
        open(f"{OUT}/m10-{TAG}-{ARCH}-hold.txt", "w").write(hd)
        for ln in hd.splitlines():
            if any(k in ln for k in ("CONSOLE_YIELDS", "FB0_SHOWS", "HOLD READY")):
                log(f"      | {ln.strip()}")
        time.sleep(3)
        shot("hold")

    log("==== SUMMARY ====")
    log(f"  positive control reported failing: {ctl_ok}")
    for cmd, p, f in summary:
        log(f"  {cmd:14s} PASS={p:3d} FAIL={f:3d}")
    clean()
    log("==== m10 DONE ====")


if __name__ == "__main__":
    main()
