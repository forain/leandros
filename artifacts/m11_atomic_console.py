#!/usr/bin/env python3
# Atomic-KMS console-yield verification (m11).
#
# Successor to m10_console_scanout.py. Same boot choreography, but the thing
# being measured is the pair of console-yield guards in drmsmoke:
#
#   CONSOLE_YIELDS_TO_ATOMIC   — an ATOMIC-only present must claim the console
#   CONSOLE_YIELDS_TO_SCANOUT  — the legacy SETCRTC present must claim it too
#
# plus the atomic present's own pixel checks (ATOMIC_TEST_ONLY_NO_PRESENT /
# ATOMIC_COMMIT / ATOMIC_PRESENTS_PIXELS).
#
# One boot per arch:
#   1. positive control (`nosuchbinary_xyz42`) — the harness MUST report it
#      failing, so "absent" and "failing" stay distinguishable afterwards;
#   2. vfstest, exactly once on this image (full mode only);
#   3. drmsmoke — where the census lives;
#   4. screendump after drmsmoke exits — proves the console reclaimed;
#   5. the rest of the suite (full mode only);
#   6. drmsmoke --hold in the FOREGROUND (x86_64 produced no serial output at
#      all when backgrounded), then screendump + exact pixel census.
#
# Usage: m11_atomic_console.py <arch> <tag> [outdir] [full|quick]
import subprocess, sys, os, time, re, hashlib

HOME = os.path.expanduser("~")
REPO = f"{HOME}/code/leandros"
DRIVER = f"{REPO}/.claude/skills/run-leandros/driver.py"
SCMRUN = f"{REPO}/scripts/scmrun.py"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
TAG = sys.argv[2] if len(sys.argv) > 2 else "control"
OUT = sys.argv[3] if len(sys.argv) > 3 else "/tmp/m11"
MODE = sys.argv[4] if len(sys.argv) > 4 else "full"
os.makedirs(OUT, exist_ok=True)

SOCK = "/tmp/leandros-serial.sock"
PIDF = "/tmp/leandros-qemu.pid"

# The census reads the whole surface six times per drmsmoke run (8.3 MB each on
# x86_64), and x86_64 on this host is TCG. Ceilings, not fixed waits: the reader
# returns on the completion marker.
DRM_CEIL = 900 if ARCH == "x86_64" else 300
HOLD_CEIL = 900 if ARCH == "x86_64" else 300

SUITE = [("scmtest", 90), ("forktest", 25), ("epolltest", 40), ("polltest", 40),
         ("waittest", 60), ("sigtest", 25), ("timertest", 30), ("memtest", 25)]

# The two files a mutation is allowed to touch. Their md5s are the proof that a
# "restore" run really is the control build's source and not a near-miss.
WATCHED = ["drivers/src/drm_device_interface.rs", "drivers/src/framebuffer.rs",
           "userland/drmsmoke/src/main.rs"]

GUARDS = ["ATOMIC_TEST_ONLY_NO_PRESENT", "ATOMIC_COMMIT", "ATOMIC_PRESENTS_PIXELS",
          "CONSOLE_YIELDS_TO_ATOMIC", "FB0_SHOWS_SCANOUT", "CONSOLE_YIELDS_TO_SCANOUT"]


def log(*a):
    print(*a, flush=True)


def d(*a, t=300):
    try:
        r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {a})"


def scm(cmd, dur, marker=None):
    args = ["python3", SCMRUN, cmd, str(dur)]
    if marker:
        args.append(marker)
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=dur + 90)
        return (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        return f"(TIMEOUT {cmd})"


def clean_ansi(t):
    return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', re.sub(r'\x1b[=>78]', '', t))


def clean():
    d("stop", t=60)
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
        return md5
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
    return md5


def shot(name):
    p = f"{OUT}/m11-{TAG}-{ARCH}-{name}.ppm"
    d("screenshot", p, t=90)
    if os.path.exists(p):
        return census(p, name)
    log(f"  [{name}] no screendump produced")
    return None


def source_md5s():
    out = {}
    for rel in WATCHED:
        out[rel] = hashlib.md5(open(f"{REPO}/{rel}", "rb").read()).hexdigest()
    return out


def tally(text):
    """(pass, fail) over `<name>: PASS/FAIL` lines. Anchored on the colon so a
    prose line mentioning the word is not counted as a result."""
    return (len(re.findall(r': PASS\b', text)), len(re.findall(r': FAIL\b', text)))


def verdicts(text):
    """name -> PASS/FAIL for every named guard present in the log."""
    v = {}
    for name in GUARDS:
        m = re.search(rf'^{name}: (PASS|FAIL)$', text, re.M)
        v[name] = m.group(1) if m else "ABSENT"
    return v


def main():
    log(f"==== m11 atomic console-yield {ARCH} tag={TAG} mode={MODE} {time.ctime()} ====")
    srcs = source_md5s()
    for rel, h in srcs.items():
        log(f"  [src md5] {h}  {rel}")

    booted = False
    out = ""
    for attempt in (1, 2):
        log(f"#### BOOT {attempt} ####")
        clean()
        out = d("start", ARCH, "uefi", t=300)
        if any(m in out for m in ("Login prompt ready", "login:", "Shell ready")):
            booted = True
            break
    if not booted:
        log("NO BOOT")
        log(out[-2000:])
        clean()
        return
    d("login", "root", "root", t=60)
    scm("echo WARMUP", 6)

    # 1. Positive control — must be reported as FAILING, not absent.
    ctl = clean_ansi(scm("nosuchbinary_xyz42", 12))
    open(f"{OUT}/m11-{TAG}-{ARCH}-control.log", "w").write(ctl)
    ctl_ok = any(m in ctl for m in ("not found", "No such file", "cannot", "command not found"))
    log(f"  [POSITIVE CONTROL] harness reports failure = {ctl_ok}")

    shot("boot")

    # 2. vfstest — exactly once per image.
    if MODE == "full":
        vt = clean_ansi(scm("vfstest", 120, "--- vfstest done ---"))
        open(f"{OUT}/m11-{TAG}-{ARCH}-vfstest.log", "w").write(vt)
        vp, vf = tally(vt)
        log(f"  [vfstest] PASS={vp} FAIL={vf}")

    # 3. drmsmoke — the census lives here.
    ds = clean_ansi(scm("drmsmoke", DRM_CEIL, "--- drmsmoke done ---"))
    open(f"{OUT}/m11-{TAG}-{ARCH}-drmsmoke.log", "w").write(ds)
    for ln in ds.splitlines():
        if any(k in ln for k in ("ATOMIC", "CONSOLE_YIELDS", "FB0_SHOWS_SCANOUT",
                                 "SETCRTC:", "drmsmoke done")):
            log(f"      | {ln.strip()}")
    dv = verdicts(ds)
    dp, df = tally(ds)
    log(f"  [drmsmoke] PASS={dp} FAIL={df}")
    log(f"  [guards]   " + "  ".join(f"{k}={v}" for k, v in dv.items()))

    # 4. The console must have reclaimed the scanout when drmsmoke closed card0.
    shot("after-drmsmoke")

    # 5. Rest of the suite.
    summary = []
    if MODE == "full":
        for cmd, dur in SUITE:
            if not vm_alive():
                log(f"  [{cmd}] VM GONE")
                break
            t = clean_ansi(scm(cmd, dur))
            open(f"{OUT}/m11-{TAG}-{ARCH}-{cmd}.log", "w").write(t)
            p, f = tally(t)
            summary.append((cmd, p, f))
            log(f"  [{cmd}] PASS={p} FAIL={f}")

    # 6. --hold, foreground, then photograph the held image.
    hold_md5 = None
    if vm_alive():
        hd = clean_ansi(scm("drmsmoke --hold", HOLD_CEIL, "DRMSMOKE: HOLD READY"))
        open(f"{OUT}/m11-{TAG}-{ARCH}-hold.log", "w").write(hd)
        for ln in hd.splitlines():
            if any(k in ln for k in ("ATOMIC", "CONSOLE_YIELDS", "FB0_SHOWS", "HOLD READY")):
                log(f"      | {ln.strip()}")
        hv = verdicts(hd)
        log(f"  [hold guards] " + "  ".join(f"{k}={v}" for k, v in hv.items()))
        time.sleep(4)
        hold_md5 = shot("hold")

    log("==== SUMMARY ====")
    log(f"  tag={TAG} arch={ARCH}")
    for rel, h in srcs.items():
        log(f"  src md5 {h}  {rel}")
    log(f"  positive control reported failing: {ctl_ok}")
    for k, v in dv.items():
        log(f"  guard {k:30s} = {v}")
    log(f"  hold screendump md5 = {hold_md5}")
    for cmd, p, f in summary:
        log(f"  {cmd:14s} PASS={p:3d} FAIL={f:3d}")
    clean()
    log("==== m11 DONE ====")


if __name__ == "__main__":
    main()
