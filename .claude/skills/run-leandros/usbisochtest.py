"""An isochronous OUT stream reaches a USB audio device intact (lane usbisoch).

Boots with `qemu-xhci` + `usb-audio` (UAC1, full speed, 48 kHz S16 stereo,
isochronous OUT endpoint) whose audiodev records into a WAV file, and the
kernel knob `usb.isochtest=2000`: when the audio function enumerates, the
kernel configures its isochronous endpoint (drivers/usb/src/xhci/isoch.rs)
and streams a 2 s 1 kHz sine from the timer tick. No login is needed.

Pass criteria (on the host-side WAV):
  * the kernel logs `[USB-ISOCH] selftest done` with underrun 0 and errors 0;
  * one audible segment of 2.0 s (within 15 %), both channels at 1000 Hz
    (within 3 Hz) and RMS above 3000 (the sine is 16000 peak, RMS ~11300);
  * fewer than 1 % of the segment's 1 ms windows are all-zero (dropouts).

Usage: LEANDROS_RUN_ID=<id> python3 usbisochtest.py [aarch64|x86_64]
"""
import json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
sys.path.insert(0, HERE)
import sndwav  # noqa: E402

CMDLINE = "usb.isochtest=2000"
SECONDS = 2.0


def drv(*args, timeout=200, env=None):
    p = subprocess.run(DRV + list(args), capture_output=True, text=True,
                       timeout=timeout, env=env, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def cmdline_image(arch, run_id):
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    img = os.path.join(REPO, f"usbisoch-boot-{run_id}.img")
    if os.path.exists(img): os.unlink(img)
    if subprocess.run(["cp", "-c", src, img], stderr=subprocess.DEVNULL).returncode != 0:
        import shutil; shutil.copyfile(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, CMDLINE],
                   check=True, stdout=subprocess.DEVNULL)
    return img


def serial_log(run_id):
    _, log = drv("log", timeout=30)
    try: log += open(f"/tmp/leandros-{run_id}-serial-full.log", errors="replace").read()
    except OSError: pass
    return log


def analyse(wav):
    rate, left, right = sndwav.read_wav(wav)
    segs = sndwav.segments(rate, left, right)
    print(f"wav: {rate} Hz, {len(left) / rate:.2f} s, {len(segs)} segment(s)")
    if not segs:
        return ["no audible segment in the capture"]
    a, b = max(segs, key=lambda s: s[1] - s[0])
    dur = (b - a) / rate
    fl, fr = sndwav.freq(left[a:b], rate), sndwav.freq(right[a:b], rate)
    rl, rr = sndwav.rms(left[a:b]), sndwav.rms(right[a:b])
    win = max(1, rate // 1000)
    nwin = (b - a) // win
    zero = sum(1 for w in range(nwin)
               if not any(left[a + w * win:a + (w + 1) * win]) and not any(right[a + w * win:a + (w + 1) * win]))
    zfrac = zero / max(1, nwin)
    print(f"segment {a / rate:.2f}s +{dur:.3f}s: L {fl:.1f} Hz rms {rl:.0f}, R {fr:.1f} Hz rms {rr:.0f}, "
          f"all-zero 1 ms windows {zero}/{nwin} ({100 * zfrac:.2f} %)")
    bad = []
    if abs(dur - SECONDS) > 0.15 * SECONDS: bad.append(f"duration {dur:.3f} s, want {SECONDS} s")
    if abs(fl - 1000) > 3 or abs(fr - 1000) > 3: bad.append(f"frequency L {fl:.1f} R {fr:.1f}, want 1000 Hz")
    if rl < 3000 or rr < 3000: bad.append(f"rms L {rl:.0f} R {rr:.0f} below 3000")
    if zfrac >= 0.01: bad.append(f"{100 * zfrac:.2f} % dropout windows")
    return bad


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
    run_id = os.environ.get("LEANDROS_RUN_ID", "usbisoch")
    wav = os.path.join(REPO, f"usbisoch-{run_id}.wav")
    if os.path.exists(wav): os.unlink(wav)
    env = dict(os.environ)
    env["LEANDROS_AUDIO_WAV"] = wav
    env["LEANDROS_QEMU_EXTRA"] = "-device qemu-xhci,id=xha -device usb-audio,bus=xha.0,audiodev=snd0,id=usbaud"
    boot_img = cmdline_image(arch, run_id)
    env["LEANDROS_DISK_IMAGE"] = boot_img
    bad = []
    try:
        drv("stop", timeout=60)
        rc, out = drv("start", arch, timeout=400, env=env)
        if rc != 0:
            print("FAIL: boot\n" + out[-800:]); return 1
        done, log = None, ""
        deadline = time.time() + 240
        while time.time() < deadline:
            log = serial_log(run_id)
            done = re.search(r"\[USB-ISOCH\] selftest done: .*", log)
            if done or "[USB-ISOCH] selftest: open failed" in log: break
            time.sleep(3)
        for l in re.findall(r"\[USB-ISOCH\][^\n]*|\[usbisoch\][^\n]*", log)[:8]:
            print(l.strip())
        if not done:
            print("FAIL: no '[USB-ISOCH] selftest done' line"); return 1
        m = re.search(r"underrun (\d+), missed (\d+), silence (\d+), errors (\d+)", done.group(0))
        if not m or int(m.group(1)) or int(m.group(4)):
            bad.append("kernel reported underruns or errors")
        # Unplug with the pipe still open: the slot is released, no panic.
        rc, out = drv("qmp", "device_del", json.dumps({"id": "usbaud"}), timeout=60)
        gone = False
        for _ in range(20):
            time.sleep(1)
            log = serial_log(run_id)
            if re.search(r"\[USB\] bus \d+ slot \d+: released", log): gone = True; break
        if not gone: bad.append(f"unplug: no slot release logged (device_del rc={rc})")
        if re.search(r"panic|PANIC|Synchronous exception|page fault", log): bad.append("kernel fault after unplug")
        print("unplug: slot released" if gone else "unplug: FAILED")
    finally:
        drv("stop", timeout=60)
        for p in (boot_img,):
            try: os.unlink(p)
            except OSError: pass
    bad += analyse(wav)
    if bad:
        print("FAIL: " + "; ".join(bad)); return 1
    os.unlink(wav)
    print("PASS: usbisoch")
    return 0


if __name__ == "__main__":
    sys.exit(main())
