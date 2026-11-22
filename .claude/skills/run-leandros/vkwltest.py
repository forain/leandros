#!/usr/bin/env python3
"""vkwltest.py [arch] -- a Vulkan swapchain on a real Wayland surface, presented to cosmic-comp (x86_64, --venus).

Own boot: `--venus` (Venus ICD over virtio-gpu, host GPU) with nvrm.desktop=0 so
greetd leaves the compositor to us. Root starts the COSMIC session by hand
(/bin/start-cosmic-leandros), waits for wayland-1, then runs /bin/vkwl
(ports/vkwl-test, dynamic musl, dlopen()s libvulkan_virtio.so) for N frames.
Checks, none of them screenshots:
  - the vkwl suite is 0 failures (instance, wl_surface, VkSurfaceKHR,
    swapchain, N acquires, N presents -> VK_SUCCESS),
  - compositor_frame_callbacks: cosmic-comp answered the wl_surface.frame
    requests that rode on the presented commits (it consumed the frames),
  - the compositor is still alive afterwards.
Venus needs virtio-gpu-gl-pci + a host GPU: the Mac QEMU lacks it, so a
non-x86_64 run (or a host without it) is a SKIP. VKWL_ROUTE=sw|nosw picks the
Mesa WSI route (default sw: wl_shm buffers, the GPU still renders).
GPU only: no software rendering anywhere on this path.
"""
import imgclone
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.setdefault("LEANDROS_RUN_ID", "vkwltest")
import driver  # noqa: E402

REPO = driver.REPO_ROOT
ARCH = os.environ.get("LEANDROS_TEST_ARCH") or (sys.argv[1] if len(sys.argv) > 1 else "x86_64")
FRAMES = int(os.environ.get("VKWL_FRAMES", "60"))
ROUTE = os.environ.get("VKWL_ROUTE", "sw")
STATS = os.environ.get("VKWL_STATS") == "1"  # opt-in: cat /proc/drm/fence_stats before/after the run
PUSH = [sys.executable, os.path.join(REPO, "scripts/pi-serial-push.py"), "--port", "unix:" + driver.SERIAL_SOCK]
ENV = dict(os.environ, LEANDROS_PI_SERIAL="unix:" + driver.SERIAL_SOCK, LEANDROS_PI_LOGIN="root:root")
DRV = [sys.executable, os.path.join(HERE, "driver.py")]


def drv(*a, timeout=300):
    p = subprocess.run(DRV + list(a), capture_output=True, text=True, timeout=timeout, cwd=REPO)
    return p.returncode, p.stdout + p.stderr


def guest(cmd, timeout=120):
    r = subprocess.run(PUSH + ["--timeout", str(timeout), "--cmd", cmd], capture_output=True, text=True, env=ENV)
    return r.returncode, r.stdout + r.stderr


def main():
    if ARCH != "x86_64":
        print("RESULT: SKIP (Venus needs x86_64 KVM + virtio-gpu-gl-pci)")
        return 0
    fails = []

    def check(name, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {name} {detail}", flush=True)
        if not ok:
            fails.append(name)

    out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), "tests-vkwltest")
    os.makedirs(out, exist_ok=True)
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{ARCH}.img")
    img = os.path.join(out, f"cmdline-{ARCH}.img")
    if os.path.exists(img):
        os.unlink(img)
    if subprocess.run([*imgclone.CPC, src, img], capture_output=True).returncode != 0:
        shutil.copyfile(src, img)
    subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, "nvrm.desktop=0"],
                   check=True, stdout=subprocess.DEVNULL)
    os.environ["LEANDROS_DISK_IMAGE"] = img
    try:
        rc, o = drv("start", ARCH, "--venus")
        if rc != 0 and re.search(r"not found|Unknown device|virtio-gpu-gl", o):
            print("RESULT: SKIP (QEMU lacks virtio-gpu-gl-pci)")
            return 0
        check("boot --venus", rc == 0, o[-200:])
        rc, o = drv("login", "root", "root", "180", timeout=400)
        check("login", rc == 0, o[-200:])
        rc, o = guest("ls /bin/vkwl /usr/lib/libvulkan_virtio.so /bin/start-cosmic-leandros")
        check("vkwl + venus ICD + session launcher shipped", rc == 0, o.strip()[-120:])
        guest("mkdir -p /run/user/0; chmod 0700 /run/user/0; "
              "(sh /bin/start-cosmic-leandros > /run/user/0/cosmic.log 2>&1 &)", timeout=60)
        rc, o = guest("i=0; while [ $i -lt 240 ]; do for w in /run/user/0/wayland-*; do [ -S $w ] && break 2; done; "
                      "sleep 1; i=$((i+1)); done; echo WAITED=$i; ls /run/user/0", timeout=300)
        sock = re.search(r"(wayland-\d+)\b", o.split("WAITED=")[-1])
        sock = sock.group(1) if sock else "wayland-1"
        check("cosmic-comp bound a wayland socket", bool(re.search(r"wayland-\d+", o.split("WAITED=")[-1])), o.strip()[-80:])
        if sock == "wayland-1" and "wayland-1" not in o.split("WAITED=")[-1]:
            print(guest("ls -l /run/user/0; tail -n 40 /run/user/0/cosmic.log; ps | tail -n 30", timeout=60)[1][-3500:])
        guest("sleep 20", timeout=60)  # outputs probed before a client's surface gets a configure
        if STATS:
            print("FENCE_STATS before:\n" + guest("cat /proc/drm/fence_stats", timeout=60)[1], flush=True)
        rc, o = guest(f"XDG_RUNTIME_DIR=/run/user/0 WAYLAND_DISPLAY={sock} /bin/vkwl {ROUTE} {FRAMES} quiet",
                      timeout=600)
        print("\n".join(o.strip().splitlines()[-30:]), flush=True)
        if STATS:
            print("FENCE_STATS after:\n" + guest("cat /proc/drm/fence_stats", timeout=60)[1], flush=True)
        m = re.search(r"vkwl done, failures = (\d+)", o)
        check("vkwl: 0 failures", bool(m) and m.group(1) == "0", m.group(0) if m else "no summary line")
        for t in ("wl_display_connect", "queue_present_all_frames", "compositor_frame_callbacks"):
            check(t, f"{t}: PASS" in o)
        fc = re.search(r"frame_callbacks done=(\d+) presented=(\d+)", o)
        check(f"compositor answered frame callbacks ({fc.group(0) if fc else 'none'})",
              bool(fc) and int(fc.group(2)) == FRAMES and int(fc.group(1)) * 2 >= FRAMES)
        rc, o = guest(f"ls /run/user/0/{sock} && grep -c cosmic-comp /proc/[0-9]*/comm | grep -v ':0' | head -1")
        check("cosmic-comp still alive", rc == 0, o.strip()[-60:])
    finally:
        try:
            drv("stop", timeout=60)
        except Exception:
            pass
    print("RESULT:", "PASS" if not fails else "FAIL " + ", ".join(fails))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
