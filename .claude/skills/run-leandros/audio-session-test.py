#!/usr/bin/env python3
"""audio-session-test.py: PipeWire audio in a real COSMIC session, measured on
the host through QEMU's wav capture.

One boot (driver.py start --virgl with LEANDROS_AUDIO_WAV), serial root login,
greeter login as leandro, then from the root serial shell (talking to the
session's PipeWire at /run/user/1000):
  probe    process list, wpctl status, pipewire.log, the sink's own stats
  tone     pw-play of a 10 s 440 Hz sine at the current volume (marker: tone1)
  volume   AT_VOLUME (default 0.30) set either by wpctl or (AV_SETTINGS=1) by
           clicking cosmic-settings' Sound page slider; then the tone again
  report   per-segment RMS ratio and a sine-model glitch count from the wav
Steps via AV_STEPS (default "probe,tone,volume,report").

env: AV_REPO AV_OUT AV_ARCH [AV_GPU=virgl] [AV_KEEP=1]
out: events.log serial.log capture.wav results.json *.png
"""
import json
import math
import os
import re
import struct
import subprocess
import sys
import time

REPO = os.environ["AV_REPO"]
OUT = os.environ["AV_OUT"]
ARCH = os.environ.get("AV_ARCH", "aarch64")
GPU = os.environ.get("AV_GPU", "virgl")
STEPS = os.environ.get("AV_STEPS", "probe,tone,volume,report").split(",")
os.makedirs(OUT, exist_ok=True)
WAV = os.path.join(OUT, "capture.wav")
os.environ["LEANDROS_AUDIO_WAV"] = WAV
os.environ["CS_REPO"] = REPO
os.environ["CS_OUT"] = OUT
os.environ.setdefault("LEANDROS_RUN_ID", "audiotest")
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
import importlib.util  # noqa: E402
spec = importlib.util.spec_from_file_location("cs", f"{REPO}/.claude/skills/run-leandros/coldstart-bench.py")
cs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cs)
driver = cs.driver
log = cs.log
res = {}
ENV = "XDG_RUNTIME_DIR=/run/user/1000"
MARKS = []   # (label, wav_byte_offset_at_start, offset_at_end)


def wav_size():
    try:
        return os.path.getsize(WAV)
    except OSError:
        return 0


def sh(c, t=30):
    o = cs.SER.cmd(c, t)
    log("$", c, "->", o.replace("\r", "")[-1500:])
    return o


def tone(label):
    a = wav_size()
    t0 = time.time()
    o = sh(f"{ENV} /usr/bin/pw-play /usr/share/sounds/leandros/tone-440-10s.wav; echo rc=$?", 40)
    b = wav_size()
    MARKS.append((label, a, b))
    res[label] = dict(secs=round(time.time() - t0, 2), wav_bytes=b - a, out=o[-300:])


def analyze():
    raw = open(WAV, "rb").read()
    pcm = raw[44:]
    n = len(pcm) // 4
    s = struct.unpack(f"<{n*2}h", pcm[:n * 4])
    left = s[0::2]
    rate = 44100
    out = {}
    for label, a, b in MARKS:
        i0, i1 = max(0, (a - 44) // 4), max(0, (b - 44) // 4)
        seg = left[i0:i1]
        # 50 ms windows; tone present where rms > 200
        w = rate // 20
        rms = [math.sqrt(sum(v * v for v in seg[i:i + w]) / w) for i in range(0, len(seg) - w, w)]
        on = [i for i, r in enumerate(rms) if r > 20]
        if not on:
            out[label] = dict(tone_windows=0)
            continue
        first, last = on[0], on[-1]
        body = rms[first + 2:last - 1]
        holes = [i for i in range(first, last + 1) if rms[i] < 0.5 * (sum(body) / max(1, len(body)))]
        # sine model: x[n+1] + x[n-1] = 2cos(w) x[n]; a dropout/repeat breaks it
        c2 = 2 * math.cos(2 * math.pi * 440 / rate)
        x = seg[(first + 2) * w:(last - 1) * w]
        amp = max(1, max(abs(v) for v in x)) if x else 1
        resid = [abs(x[k + 1] + x[k - 1] - c2 * x[k]) / amp for k in range(1, len(x) - 1)]
        spikes, k, at = 0, 0, []
        while k < len(resid):
            if resid[k] > 0.05:
                spikes += 1
                at.append(round((first + 2) / 20 + k / rate, 3))
                k += w // 10   # one event per 5 ms
            else:
                k += 1
        out[label] = dict(tone_secs=round((last - first + 1) / 20, 2),
                          rms=round(sum(body) / max(1, len(body)), 1),
                          rms_min=round(min(body), 1) if body else None,
                          rms_max=round(max(body), 1) if body else None,
                          holes_50ms=len(holes), discontinuities=spikes, at_s=at[:20],
                          max_resid=round(max(resid), 4) if resid else None)
    return out


def main():
    r = cs.drv("start", ARCH, f"--{GPU}")
    log("start", r[-300:].replace("\n", " | "))
    qpid = driver._qemu_pid()
    open(f"{OUT}/qemu.pid", "w").write(str(qpid))
    cs.HS = cs.HostSampler(qpid)
    log("login", cs.drv("login", "root", "root", timeout=240)[-120:].replace("\n", " | "))
    cs.SER = cs.Serial()
    time.sleep(20)
    q = cs.settle_quiet(120)
    log("greeter settled after", q)
    base = cs.grab()
    cs.typ("leandro")
    tl = time.time()
    cs.key("ret", 0)
    f, s = cs.wait_change(base, tl, thresh=0.10, timeout=240, stable_needed=4)
    log("session first=", f, "settled=", s)
    time.sleep(int(os.environ.get("AV_SETTLE", "30")))
    cs.save("2-session")
    for st in STEPS:
        if st == "probe":
            res["ps"] = sh("for p in /proc/[0-9]*; do read c < $p/comm; echo ${p#/proc/} $c; done 2>/dev/null", 30)
            res["pwlog"] = sh("cat /run/user/1000/pipewire.log | tail -40", 20)
            res["status"] = sh(f"{ENV} /usr/bin/wpctl status", 30)
            res["vol0"] = sh(f"{ENV} /usr/bin/wpctl get-volume @DEFAULT_AUDIO_SINK@", 20)
        elif st == "tone":
            tone("tone1")
        elif st == "volume":
            v = os.environ.get("AV_VOLUME", "0.30")
            if os.environ.get("AV_SETTINGS") == "1":
                res["settings"] = settings_slider()
            else:
                sh(f"{ENV} /usr/bin/wpctl set-volume @DEFAULT_AUDIO_SINK@ {v}", 20)
            res["vol1"] = sh(f"{ENV} /usr/bin/wpctl get-volume @DEFAULT_AUDIO_SINK@", 20)
            tone("tone2")
        elif st == "screencast":
            # Start the portal probe in the background; COSMIC's picker then
            # needs a selection + Share (AV_SC_CLICKS="x,y;x,y", 1280x800 coords).
            sh("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus XDG_RUNTIME_DIR=/run/user/1000 "
               "/usr/bin/pw-screencast-probe 8 /tmp/sc.ppm > /tmp/sc.out 2>&1 &", 15)
            time.sleep(int(os.environ.get("AV_SC_WAIT", "15")))
            cs.save("sc-picker")
            for xy in filter(None, os.environ.get("AV_SC_CLICKS", "").split(";")):
                x, y = (int(v) for v in xy.split(","))
                cs.click(x, y)
                time.sleep(3)
            if os.environ.get("AV_SC_CLICKS"):
                time.sleep(20)
                cs.save("sc-after")
                res["screencast"] = sh("cat /tmp/sc.out", 20)
        elif st == "report":
            res["pwlog_end"] = sh("cat /run/user/1000/pipewire.log | tail -30", 20)
        json.dump(res, open(f"{OUT}/results.json", "w"), indent=1, default=str)
    sl = open(f"{OUT}/serial.log", errors="replace").read()
    res["counts"] = dict(pf=len(re.findall(r"\[PF\]", sl)), segv=sl.count("SEGV"),
                         panic=len(re.findall(r"(?i)panic", sl)), wdog=sl.count("[WDOG]"),
                         snd_stalls=sl.count("TX stalled"), pw_gaps=sl.count("producer gap"))
    if os.environ.get("AV_KEEP") != "1":
        cs.SER.stop = True
        log("stop", cs.drv("stop")[-100:])
        time.sleep(2)
        res["marks"] = MARKS
        res["analysis"] = analyze()
    json.dump(res, open(f"{OUT}/results.json", "w"), indent=1, default=str)
    log("ANALYSIS", json.dumps(res.get("analysis")))
    log("COUNTS", json.dumps(res["counts"]))
    log("DONE")


def settings_slider():
    """Open Settings -> Sound via the launcher, click the output volume slider
    at AV_SLIDER_X (fraction of its track), read back the volume."""
    r = cs.launch("sound", "sound", close=False)
    cs.key("meta_l-m", 0.5)
    cs.settle_quiet(30)
    cs.save("sound-page")
    xy = os.environ.get("AV_CLICK")
    if xy:
        x, y = (int(v) for v in xy.split(","))
        cs.click(x, y)
        time.sleep(2)
        cs.save("sound-page-after")
    return r


if __name__ == "__main__":
    main()
