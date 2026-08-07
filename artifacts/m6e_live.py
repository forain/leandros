#!/usr/bin/env python3
# Drive the ALREADY-BOOTED live guest (qemu up) to capture the Mesa kms_map /
# add_from_prime W2DIAG lines. Uses the nobreak driver (full-window reads).
import subprocess, os, re, time, sys
DRV = os.path.expanduser("~/code/leandros-artifacts/driver_nobreak.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m6-screenshots")
def cmd(c, t):
    r = subprocess.run(["python3", DRV, "cmd", c, str(t)], capture_output=True, text=True, timeout=t+15)
    return (r.stdout or "") + (r.stderr or "")
def clean(s):
    s = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', s); s = re.sub(r'\x1b[78=]', '', s)
    return s
ENV = [
    "export XDG_RUNTIME_DIR=/run/user/0",
    "export HOME=/root ICED_BACKEND=tiny-skia",
    "export COSMIC_BACKEND=kms GBM_ALWAYS_SOFTWARE=1",
    "export COSMIC_RENDER_DEVICE=226:0 SMITHAY_USE_LEGACY=1",
    "export COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1",
    "export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0",
    "unset DISPLAY WAYLAND_DISPLAY",
]
def main():
    for e in ENV:
        cmd(e, 8)
    v = clean(cmd("echo RTDCHK=[$XDG_RUNTIME_DIR]:[$COSMIC_BACKEND]", 8))
    m = re.search(r'RTDCHK=\[[^\]]*\]:\[[^\]]*\]', v)
    print("ENV:", m.group(0) if m else "(not found)", flush=True)
    # make sure runtime dir exists
    cmd("mkdir -p /run/user/0", 8)
    # launch comp (short command to avoid FIFO drop)
    cmd("rm -f /tmp/comp.log", 8)
    out = clean(cmd("cosmic-comp --no-xwayland >/tmp/comp.log 2>&1 &", 12))
    print("LAUNCH tail:", out.strip().splitlines()[-2:] if out.strip() else "(empty)", flush=True)
    # silent full-window read: comp inits, imports scanout dmabuf, maps, maybe crashes
    win = clean(cmd("sleep 55; echo WEND", 66))
    open(f"{OUT}/m6e-aarch64-live3-window.txt", "w").write(win)
    # read comp.log (Mesa W2DIAG lines land here since comp stderr -> comp.log)
    log = clean(cmd("cat /tmp/comp.log", 30))
    open(f"{OUT}/m6e-aarch64-live3-complog.txt", "w").write(log)
    # extract decisive lines from BOTH the serial window and comp.log
    both = win + "\n===COMPLOG===\n" + log
    print("==== DECISIVE ====", flush=True)
    for pat in [r'W2DIAG[^\n]{0,130}', r'W2K[^\n]{0,90}', r'\[MMAP\][^\n]{0,80}',
                r'\[FAULT\][^\n]{0,150}', r'far=0x[0-9a-fA-F]+[^\n]{0,90}',
                r'panic[^\n]{0,120}', r'RRuntimeDir|RuntimeDirNotSet',
                r'MESA-LOADER[^\n]{0,90}', r'\[EXIT\][^\n]{0,40}']:
        for mm in re.finditer(pat, both):
            print(repr(mm.group(0)), flush=True)
if __name__ == "__main__":
    main()
