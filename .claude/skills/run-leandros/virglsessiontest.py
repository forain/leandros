#!/usr/bin/env python3
"""virglsessiontest.py <audio|video|clickmove|udevhot|minimize|coreutils>

Runs one of the virgl-COSMIC-session suites (or, for coreutils, the brush-guest
coreutils suite) as a `host:` script of runtests.py; arch from LEANDROS_TEST_ARCH
(default aarch64). Each one boots its own VM (`--virgl` needs a host GPU QEMU:
the Mac GPU QEMU, or the box). Exit 0 iff the underlying test passes.
  audio      audio-session-test.py: pw-play tone clean (<= 1 hole of 50 ms, <= 4 discontinuities),
             volume 0.30 applied (tone2 rms ratio 0.01..0.2 of tone1), no pf/segv/panic/wdog
  video      videotest.py: 1080p AV1 playback advances in Firefox
  clickmove  clickmovetest.py: clicks batched/interleaved with motion all reach a client
  udevhot    udevhottest.py: USB keyboard hot-add/removal in a COSMIC session
  minimize   minimizetest.py: cosmic-term minimized (zcosmic_toplevel_manager), un-hidden by clicking its
             dock minimize-applet entry (geometry from the panel wltrace), activated + takes keyboard input
  coreutils  coreutils-test.py against a fresh brush guest (no GPU)
"""
import json, os, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
name = sys.argv[1]
arch = os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
rid = os.environ.get("LEANDROS_RUN_ID", "virglsess")
out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"virgl-{name}-{arch}")
os.makedirs(out, exist_ok=True)
env = dict(os.environ, CS_REPO=REPO, CS_OUT=out, AV_REPO=REPO, AV_OUT=out, AV_ARCH=arch,
           LEANDROS_RUN_ID=f"{rid}-{name}", LEANDROS_TEST_ARCH=arch)
py = sys.executable


def run(argv, **kw):
    r = subprocess.run(argv, env=env, **kw)
    return r


def drv(*a):
    return subprocess.run([py, f"{HERE}/driver.py", *a], env=env, capture_output=True, text=True)


if name == "audio":
    r = run([py, f"{HERE}/audio-session-test.py"])
    ok = r.returncode == 0
    try:
        res = json.load(open(f"{out}/results.json"))
        a = res.get("analysis") or {}
        t1, t2 = a.get("tone1", {}), a.get("tone2", {})
        ratio = t2["rms"] / t1["rms"]
        c = res.get("counts") or {}
        ok = ok and t1["holes_50ms"] <= 1 and t2["holes_50ms"] <= 1 and t1["discontinuities"] <= 4 \
            and t2["discontinuities"] == 0 and 0.01 < ratio < 0.2 and not any(c.get(k) for k in ("pf", "segv", "panic", "wdog"))
        print(f"audio {arch}: tone1 holes={t1['holes_50ms']} disc={t1['discontinuities']} tone2 ratio={ratio:.3f} counts={c}")
    except (OSError, KeyError, ValueError, ZeroDivisionError) as e:
        print(f"audio {arch}: results unreadable: {e!r}")
        ok = False
elif name == "video":
    r = run([py, f"{HERE}/videotest.py", arch, f"{rid}-vid"])
    ok = r.returncode == 0
elif name == "clickmove":
    ok = run([py, f"{HERE}/clickmovetest.py"]).returncode == 0
elif name == "udevhot":
    ok = run([py, f"{HERE}/udevhottest.py"]).returncode == 0
elif name == "minimize":
    ok = run([py, f"{HERE}/minimizetest.py"]).returncode == 0
elif name == "coreutils":
    env["LEANDROS_GPU"] = "none"
    env["LEANDROS_SERIAL_RAW"] = os.path.join(out, "serial-raw.txt")
    ok = drv("start", arch).returncode == 0
    if ok:
        ok = drv("login", "root", "root").returncode == 0
    if ok:
        drv("cmd", "brush", "15")
        ok = run([py, f"{HERE}/coreutils-test.py", arch]).returncode == 0
    drv("stop")
else:
    sys.exit(f"unknown suite {name}")
print(f"virglsessiontest {name} {arch}: {'PASS' if ok else 'FAIL'}")
sys.exit(0 if ok else 1)
