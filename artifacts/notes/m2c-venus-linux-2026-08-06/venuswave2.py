#!/usr/bin/env python3
"""Venus host round-trip confirmation wave on the softfloat-kernel HEAD.

Usage: venuswave.py <x86_64|aarch64>

Boots LeandrOS under QEMU with a real venus-capable virtio-gpu-gl device on the
Linux host, logs in as root, and runs venustest / vktest / drmsmoke / vfstest.
Counts are taken from the serial log, never from a truncated step capture.
"""
import subprocess, sys, os, time, threading, re, json

REPO = "/home/forain/Projects/leandros"
ARCH = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
LOG = f"/tmp/venuswave2_{ARCH}_serial.log"
RESULT = f"/tmp/venuswave2_{ARCH}_results.json"

GPU = ["-device", "virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G",
       "-display", "egl-headless"]

if ARCH == "x86_64":
    QEMU = [
        "qemu-system-x86_64", "-machine", "q35", "-accel", "kvm", "-cpu", "host",
        "-smp", "4", "-m", "2G",
        "-drive", "if=pflash,unit=0,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd",
        "-drive", "if=pflash,unit=1,format=raw,file=./x86_64_vars_linux.fd",
        "-drive", "if=none,id=drive0,format=raw,file=leandros-limine-x86_64.img",
        "-device", "virtio-blk-pci,drive=drive0,bootindex=0",
        "-drive", "if=none,id=data0,format=raw,file=f2fs-data0-x86_64.img",
        "-device", "virtio-blk-pci,drive=data0",
        "-drive", "if=none,id=data1,format=raw,file=f2fs-data1-x86_64.img",
        "-device", "virtio-blk-pci,drive=data1",
        "-device", "virtio-keyboard-pci", "-serial", "stdio", "-no-reboot",
    ] + GPU
    BOOT_T, TEST_T = 300, 420
else:
    FW = "/usr/share/edk2/aarch64/QEMU_EFI.fd"
    QEMU = [
        "qemu-system-aarch64", "-machine", "virt,gic-version=2",
        "-accel", "tcg", "-cpu", "max,lpa2=off", "-smp", "4", "-m", "2G",
        "-boot", "menu=on,splash-time=0",
        "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={FW}",
        "-drive", "if=pflash,unit=1,format=raw,file=./aarch64_vars.fd",
        "-drive", "if=none,id=drive0,format=raw,file=leandros-limine-aarch64.img",
        "-device", "virtio-blk-pci,drive=drive0,bootindex=0,disable-legacy=on",
        "-drive", "if=none,id=data0,format=raw,file=f2fs-data0-aarch64.img",
        "-device", "virtio-blk-pci,drive=data0,disable-legacy=on",
        "-drive", "if=none,id=data1,format=raw,file=f2fs-data1-aarch64.img",
        "-device", "virtio-blk-pci,drive=data1,disable-legacy=on",
        "-device", "virtio-keyboard-pci", "-serial", "stdio", "-no-reboot",
        "-parallel", "none",
    ] + GPU
    BOOT_T, TEST_T = 1500, 2400

print("QEMU:", " ".join(QEMU), flush=True)

buf = bytearray()
lock = threading.Lock()
logf = open(LOG, "wb", buffering=0)
proc = subprocess.Popen(QEMU, cwd=REPO, stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)

def reader():
    while True:
        c = proc.stdout.read(1)
        if not c:
            break
        with lock:
            buf.extend(c)
        logf.write(c)

threading.Thread(target=reader, daemon=True).start()

def snap():
    with lock:
        return bytes(buf)

def send(s):
    data = s.encode()
    for i in range(0, len(data), 8):
        proc.stdin.write(data[i:i+8]); proc.stdin.flush(); time.sleep(0.02)

answered = [0]

def _cpr():
    b = snap()
    n = b.count(b"\x1b[6n")
    if n > answered[0]:
        for _ in range(n - answered[0]):
            proc.stdin.write(b"\x1b[24;1R"); proc.stdin.flush()
        answered[0] = n
        return True
    return False

def wait_for(pattern, timeout, start=0):
    rx = re.compile(pattern)
    t0 = time.time()
    while time.time() - t0 < timeout:
        _cpr()
        b = snap()
        if rx.search(b[start:].decode("utf-8", "replace")):
            return time.time() - t0
        time.sleep(0.2)
    return None

def idle_wait(idle=2.0, maxwait=60.0):
    t0 = time.time()
    last = len(snap()); lastchange = time.time()
    while time.time() - t0 < maxwait:
        if _cpr():
            lastchange = time.time()
        n = len(snap())
        if n != last:
            last = n; lastchange = time.time()
        elif time.time() - lastchange >= idle:
            return
        time.sleep(0.2)

results = []

def step(name, cmd, maxwait):
    start = len(snap())
    t0 = time.time()
    send(cmd + "\n")
    hit = wait_for(r"brush-[0-9.]+#", maxwait, start)
    idle_wait(idle=2.0, maxwait=30)
    out = snap()[start:].decode("utf-8", "replace")
    dt = time.time() - t0
    results.append({"name": name, "seconds": round(dt, 1),
                    "completed": hit is not None, "output": out})
    tail = [l for l in out.splitlines()
            if re.search(r"failures? =|SUMMARY|PASS=|FAIL=|passed|failed", l)]
    print(f"### {name} ({dt:.0f}s) completed={hit is not None}", flush=True)
    for l in tail[-6:]:
        print("    " + l.strip(), flush=True)
    return out

try:
    el = wait_for(r"login:", BOOT_T)
    if el is None:
        print("!!! never reached login prompt", flush=True)
        raise SystemExit(1)
    print(f"boot->login {el:.1f}s", flush=True)
    time.sleep(2.0)
    send("root\n")
    wait_for(r"[Pp]assword", 60, 0)
    time.sleep(1.0)
    send("root\n")
    idle_wait(idle=3.0, maxwait=120)
    print("logged in", flush=True)

    plan = [
        ("venustest", "venustest", TEST_T),
        
        
        ("drmsmoke", "drmsmoke", TEST_T),
        ("vfstest", "vfstest", TEST_T),
    ]
    t0 = time.time()
    for name, cmd, mx in plan:
        step(name, cmd, mx)
    print(f"WAVE TOTAL {time.time()-t0:.0f}s", flush=True)
finally:
    try:
        json.dump(results, open(RESULT, "w"), indent=1)
    except Exception as e:
        print("json fail", e)
    time.sleep(1)
    proc.kill()
    try:
        proc.wait(timeout=10)
    except Exception:
        pass
    logf.close()
    print("qemu terminated", flush=True)
