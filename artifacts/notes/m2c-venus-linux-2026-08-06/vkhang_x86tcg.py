#!/usr/bin/env python3
"""Localize the x86_64 vktest hang: reproduce, then probe guest liveness."""
import subprocess, sys, os, time, threading, re, socket

REPO = "/home/forain/Projects/leandros"
LOG = "/tmp/vkhang_x86tcg_serial.log"
MON = "/tmp/vkhang_x86tcg_mon.sock"
FW = "/usr/share/edk2/x64/OVMF_CODE.4m.fd"

for p in (MON,):
    try: os.unlink(p)
    except OSError: pass

QEMU = [
    "qemu-system-x86_64", "-machine", "q35",
    "-accel", "tcg", "-cpu", "max", "-smp", "4", "-m", "2G",
    "-boot", "menu=on,splash-time=0",
    "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={FW}",
    "-drive", "if=pflash,unit=1,format=raw,file=./x86_64_vars_linux.fd",
    "-drive", "if=none,id=drive0,format=raw,file=leandros-limine-x86_64.img",
    "-device", "virtio-blk-pci,drive=drive0,bootindex=0",
    "-drive", "if=none,id=data0,format=raw,file=f2fs-data0-x86_64.img",
    "-device", "virtio-blk-pci,drive=data0",
    "-drive", "if=none,id=data1,format=raw,file=f2fs-data1-x86_64.img",
    "-device", "virtio-blk-pci,drive=data1",
    "-device", "virtio-keyboard-pci", "-serial", "stdio", "-no-reboot",
    "-parallel", "none",
    "-device", "virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G",
    "-display", "egl-headless",
    "-monitor", f"unix:{MON},server,nowait",
]

buf = bytearray(); lock = threading.Lock()
logf = open(LOG, "wb", buffering=0)
proc = subprocess.Popen(QEMU, cwd=REPO, stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)

def reader():
    while True:
        c = proc.stdout.read(1)
        if not c: break
        with lock: buf.extend(c)
        logf.write(c)
threading.Thread(target=reader, daemon=True).start()

def snap():
    with lock: return bytes(buf)

def send(s):
    d = s.encode() if isinstance(s, str) else s
    for i in range(0, len(d), 8):
        proc.stdin.write(d[i:i+8]); proc.stdin.flush(); time.sleep(0.02)

answered = [0]
def _cpr():
    b = snap(); n = b.count(b"\x1b[6n")
    if n > answered[0]:
        for _ in range(n - answered[0]):
            proc.stdin.write(b"\x1b[24;1R"); proc.stdin.flush()
        answered[0] = n

def wait_for(pat, timeout, start=0):
    rx = re.compile(pat); t0 = time.time()
    while time.time() - t0 < timeout:
        _cpr()
        if rx.search(snap()[start:].decode("utf-8", "replace")):
            return time.time() - t0
        time.sleep(0.2)
    return None

def mon(cmd, wait=2.0):
    try:
        s = socket.socket(socket.AF_UNIX); s.settimeout(10); s.connect(MON)
        time.sleep(0.4); s.recv(65536)
        s.sendall((cmd + "\n").encode()); time.sleep(wait)
        out = b""
        s.settimeout(3)
        try:
            while True:
                d = s.recv(65536)
                if not d: break
                out += d
        except Exception: pass
        s.close()
        return out.decode("utf-8", "replace")
    except Exception as e:
        return f"MONITOR FAIL: {e}"

try:
    if wait_for(r"login:", 600) is None:
        print("!!! no login"); raise SystemExit(1)
    time.sleep(2); send("root\n")
    wait_for(r"[Pp]assword", 60, 0); time.sleep(1); send("root\n")
    time.sleep(6)
    print("logged in", flush=True)

    start = len(snap())
    send("vktest\n")
    hit = wait_for(r"SUMMARY: \d+ failure", 300, start)
    body = snap()[start:].decode("utf-8", "replace")
    if hit is not None:
        print(f"vktest COMPLETED in {hit:.0f}s", flush=True)
        print("\n".join(body.splitlines()[-12:]), flush=True)
    else:
        print("vktest DID NOT COMPLETE in 300s", flush=True)
        print("--- last serial lines ---", flush=True)
        print("\n".join([l for l in body.splitlines() if l.strip()][-8:]), flush=True)

        # 1) is output truly frozen?
        n1 = len(snap()); time.sleep(20); n2 = len(snap())
        print(f"serial bytes {n1} -> {n2} over 20s", flush=True)

        # 2) where are the vCPUs?
        regs = mon("info registers -a", wait=3.0)
        open("/tmp/vkhang_regs.txt", "w").write(regs)
        pcs = re.findall(r"PC=([0-9a-fx]+)|pc=([0-9a-fx]+)|ELR_EL1=([0-9a-f]+)", regs)
        print("PC/ELR samples:", pcs[:12], flush=True)
        print("info status:", mon("info status").strip()[:200], flush=True)

        # 3) does the guest respond at all? Ctrl-C then Enter.
        before = len(snap())
        send("\x03"); time.sleep(5); send("\n")
        r = wait_for(r"brush-[0-9.]+#", 40, before)
        print("Ctrl-C returned to prompt:" , r is not None, flush=True)
        after = snap()[before:].decode("utf-8", "replace")
        print("post-^C bytes:", len(after), flush=True)
finally:
    time.sleep(1); proc.kill()
    try: proc.wait(timeout=10)
    except Exception: pass
    logf.close(); print("qemu terminated", flush=True)
