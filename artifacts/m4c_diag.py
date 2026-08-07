#!/usr/bin/env python3
# M4c diagnostic: one persistent serial connection. Prompt-DRIVEN (waits for
# markers, never fixed sleeps) login -> anvil -> wlclient -> dump. Captures
# everything (incl. kernel UXTR unix-socket trace) to stdout.
# QEMU must already be up via `driver.py start aarch64 uefi-tcg`.
import socket, time, select, sys

SOCK = "/tmp/leandros-serial.sock"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(80):
    try:
        s.connect(SOCK); break
    except OSError:
        time.sleep(0.2)
else:
    print("CONNECT_FAIL"); sys.exit(1)
s.setblocking(False)

def _dsr(chunk):
    if b"\x1b[6n" in chunk:
        s.setblocking(True)
        s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
        s.setblocking(False)

def read_until(markers, timeout):
    """Read+echo until any marker (bytes) seen or timeout. Returns accumulated buf."""
    end = time.time() + timeout
    buf = b""
    while time.time() < end:
        if select.select([s], [], [], 0.2)[0]:
            try:
                c = s.recv(4096)
            except BlockingIOError:
                continue
            if not c:
                break
            sys.stdout.buffer.write(c); sys.stdout.flush()
            _dsr(c); buf += c
            if markers and any(m in buf for m in markers):
                return buf
    return buf

def pump(dur):
    """Read+echo for dur seconds (no early exit) — for long anvil/client waits."""
    read_until(None, dur)

def send_line(line):
    # 2-space pad: the PL011 RX FIFO (16B) drops the head of a burst if the
    # shell isn't drawing its prompt; a dropped head then eats spaces, not the
    # command (brush ignores leading spaces). Mirrors driver.py's committed fix.
    s.setblocking(True)
    p = ("  " + line + "\n").encode()
    for i in range(0, len(p), 8):
        s.sendall(p[i:i + 8]); time.sleep(0.03)
    s.setblocking(False)

def cmd(line, settle=b"# ", to=10):
    """Wait for a shell prompt, send a command, wait for the next prompt."""
    read_until([b"# ", b"$ ", b"> "], to)
    send_line(line)
    return read_until([settle], to) if settle else b""

print("\n==== M4C DIAG: sync to shell (already logged in by driver.py) ====", flush=True)
send_line("")                          # nudge shell to redraw its prompt
read_until([b"# ", b"$ ", b"> "], 10)
print("\n==== env + anvil ====", flush=True)

# Short exported env (proven reliable over serial) + short plain launch lines.
# Children inherit exports (verified: export->child propagation works). NO inline
# env prefixes and NO long launch lines (those were serial-corrupted).
cmd("mkdir -p /run/user/0")
cmd("export ANVIL_DRM_DEVICE=/dev/dri/card0")
cmd("export SMITHAY_USE_LEGACY=1")
cmd("export XDG_RUNTIME_DIR=/run/user/0")
cmd("echo RTDIR=[$XDG_RUNTIME_DIR]")   # must show /run/user/0
read_until([b"# "], 6)
send_line("anvil --tty-udev >/tmp/anvil.log 2>&1 &")
read_until([b"# ", b"]"], 6)
print("\n==== anvil launched; waiting 150s (TCG softpipe, patient) ====", flush=True)
pump(150)

print("\n==== launch wlclient ====", flush=True)
cmd("export WAYLAND_DISPLAY=wayland-1")
cmd("echo ENV=[$XDG_RUNTIME_DIR][$WAYLAND_DISPLAY]")  # must show both
read_until([b"# "], 8)
send_line("wlclient >/tmp/wl.log 2>&1 &")
print("\n==== client launched; 90s decisive UXTR window ====", flush=True)
pump(90)

print("\n==== dump wl.log ====", flush=True)
cmd("cat /tmp/wl.log", to=12)
pump(3)

print("\n==== anvil.log (filtered) ====", flush=True)
read_until([b"# "], 8)
send_line("while IFS= read -r l; do case \"$l\" in *WARN*|*ERROR*|*Output*|*client*|*Failed*|*panic*|*commit*|*Vblank*|*flip*|*Native*) echo \"A> ${l}\";; esac; done < /tmp/anvil.log")
pump(14)

print("\n==== ps ====", flush=True)
cmd("ps", to=10)
pump(3)
print("\n==== M4C DIAG DONE ====", flush=True)
s.close()
