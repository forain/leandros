#!/usr/bin/env python3
"""pi-serial-push.py -- push a file to LeandrOS over its serial console.

For a machine with no network (the Raspberry Pi 5): the only link is the
uart10 console (115200 8N1) with a root shell on it. The script types
`serrx <remote>` at the shell and streams the file as framed, CRC-checked,
ACKed chunks (go-back-N, retransmit until good); serrx writes <remote>.part,
fsyncs, checks the whole-file sha256 and renames. See userland/serrx/serrx.c.

  pi-serial-push.py [--port P] <local> <remote>      push one file
  pi-serial-push.py [--port P] --cmd 'shell cmd'     run a command, print output
  pi-serial-push.py [--port P] --kexec-exec ARGS     kexec -e, wait for the new
                                                     kernel's shell, check ARGS
                                                     are in /proc/cmdline

--port: a device (default /dev/cu.usbserial-1410) or unix:<socket> (QEMU).
If /bin/serrx is missing on the target it is bootstrapped by typing its
base64 into `head -c N | base64 -d` (~70 KB, ~7 s at 115200).

The port is needed exclusively: if another process (a `cat` logger) has the
device open the script refuses and names the PID; stop the logger yourself.
"""
import argparse
import base64
import hashlib
import os
import re
import select
import socket
import subprocess
import sys
import termios
import threading
import time
import zlib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_PORT = "/dev/cu.usbserial-1410"
LOCAL_SERRX = os.path.join(REPO, "userland/target/aarch64-unknown-none/release/serrx")
CHUNK = 768           # raw bytes per frame -> 1024 base64 chars
WINDOW = 3            # frames in flight: ~3.1 KB < the Pi's 4 KiB backlog + 1 KiB ring
RESEND_S = 3.0
MAX_STALLS = 15


sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cmdline_redact import redact  # noqa: E402


def die(msg):
    print(f"pi-serial-push: {msg}", file=sys.stderr)
    sys.exit(1)


class Port:
    """Raw serial device or QEMU unix-socket chardev, with a reader thread."""

    def __init__(self, spec, baud, log):
        self.buf = bytearray()
        self.lock = threading.Condition()
        self.log = open(log, "ab") if log else None
        self.closed = False
        if spec.startswith("unix:"):
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.sock.connect(spec[5:])
            self.fd = self.sock.fileno()
        else:
            self.sock = None
            check_exclusive(spec)
            self.fd = os.open(spec, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
            try:
                import fcntl
                fcntl.ioctl(self.fd, termios.TIOCEXCL)
            except OSError:
                pass
            speed = getattr(termios, f"B{baud}", None)
            if speed is None:
                die(f"unsupported baud {baud}")
            a = termios.tcgetattr(self.fd)
            a[0] = 0                                        # iflag: no IXON/ICRNL
            a[1] = 0                                        # oflag: raw
            a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
            a[3] = 0                                        # lflag: raw
            a[4] = a[5] = speed
            a[6][termios.VMIN] = 0
            a[6][termios.VTIME] = 0
            termios.tcsetattr(self.fd, termios.TCSANOW, a)
            termios.tcflush(self.fd, termios.TCIOFLUSH)
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        while not self.closed:
            try:
                r, _, _ = select.select([self.fd], [], [], 0.2)
                if not r:
                    continue
                d = os.read(self.fd, 4096)
            except OSError:
                d = b""
            if not d:
                if self.sock:
                    self.closed = True
                    with self.lock:
                        self.lock.notify_all()
                    return
                continue
            if self.log:
                self.log.write(d)
                self.log.flush()
            # brush probes the cursor position; answer like a terminal would.
            if b"\x1b[6n" in d:
                self.write(b"\x1b[24;1R")
            with self.lock:
                self.buf += d
                self.lock.notify_all()

    def write(self, data):
        mv = memoryview(data)
        while mv:
            try:
                n = os.write(self.fd, mv)
                mv = mv[n:]
            except BlockingIOError:
                select.select([], [self.fd], [], 1.0)

    def mark(self):
        with self.lock:
            return len(self.buf)

    def wait(self, pat, start, timeout):
        """Wait for regex `pat` (bytes) in buf[start:]; return the match or None."""
        rx = re.compile(pat, re.S)
        end = time.time() + timeout
        with self.lock:
            while True:
                m = rx.search(self.buf, start)
                if m:
                    return m
                left = end - time.time()
                if left <= 0 or self.closed:
                    return None
                self.lock.wait(min(left, 0.5))

    def close(self):
        self.closed = True
        time.sleep(0.25)
        if self.sock:
            self.sock.close()
        else:
            os.close(self.fd)


def check_exclusive(dev):
    try:
        out = subprocess.run(["lsof", "-t", dev], capture_output=True, text=True).stdout.split()
    except FileNotFoundError:
        return
    pids = [p for p in out if p.strip() and int(p) != os.getpid()]
    if pids:
        desc = []
        for p in pids:
            c = subprocess.run(["ps", "-o", "command=", "-p", p], capture_output=True, text=True).stdout.strip()
            desc.append(f"{p} ({c[:60]})")
        die(f"{dev} is open by: {', '.join(desc)}\n"
            f"  This tool needs the port exclusively. Stop the serial logger first "
            f"(e.g. kill that cat yourself), run this, then restart the logger.")


def shell(port, cmd, timeout=60, want_status=True):
    """Type `cmd` at the shell, wait for its exit status; return (status, output)."""
    start = port.mark()
    # ^U clears any half-typed line; the marker is split by "" so the echoed
    # command line never matches it, only the real output does.
    port.write(b"\x15" + cmd.encode() + b'; echo "@DO""NE$?."\r')
    if not want_status:
        return None, ""
    m = port.wait(rb"@DONE(\d+)\.", start, timeout)
    if not m:
        die(f"no response to `{cmd}` within {timeout}s (is a shell on the console? "
            f"is a serrx still running? it times out after 30 s idle)")
    out = port.buf[start:m.start()].decode(errors="replace")
    # Drop the echoed command line.
    nl = out.find("\n")
    out = out[nl + 1:] if nl >= 0 else ""
    return int(m.group(1)), out


def ensure_serrx(port, remote_bin, local_bin, bootstrap):
    st, out = shell(port, f"{remote_bin} -V 2>/dev/null", timeout=30)
    if "@SERRX1." in out:
        return
    if not bootstrap:
        die(f"{remote_bin} missing on the target (use --bootstrap)")
    if not os.path.exists(local_bin):
        die(f"{local_bin} not built (./scripts/build-userland.sh / build-all.sh)")
    raw = open(local_bin, "rb").read()
    b64 = base64.encodebytes(raw)                   # 76-char lines
    print(f"bootstrapping {remote_bin} ({len(raw)} B, {len(b64)} B base64)...", file=sys.stderr)
    start = port.mark()
    port.write(f'\x15echo "@G""O."; head -c {len(b64)} | base64 -d > {remote_bin}.tmp && '
               f'chmod 755 {remote_bin}.tmp && mv {remote_bin}.tmp {remote_bin}; echo "@DO""NE$?."\r'.encode())
    if not port.wait(rb"@GO\.", start, 20):
        die("bootstrap: shell did not start head")
    time.sleep(0.3)
    for i in range(0, len(b64), 512):
        port.write(b64[i:i + 512])
        time.sleep(512 * 10 / 115200 * 0.9)
    m = port.wait(rb"@DONE(\d+)\.", start, 60)
    if not m or m.group(1) != b"0":
        die("bootstrap: write failed")
    st, out = shell(port, f"{remote_bin} -s {remote_bin}", timeout=30)
    if f"@SHA{hashlib.sha256(raw).hexdigest()}." not in out:
        die(f"bootstrap: sha256 mismatch: {out.strip()[:120]}")


def frame(kind, seq, payload):
    return (f"#{kind}{seq:08x}{len(payload):04x}{zlib.crc32(payload) & 0xffffffff:08x}:".encode()
            + base64.b64encode(payload) + b"\n")


def push(port, local, remote, remote_bin):
    data = open(local, "rb").read()
    sha = hashlib.sha256(data).hexdigest()
    frames = [("H", f"{len(data):x} {sha}".encode())]
    frames += [("D", data[i:i + CHUNK]) for i in range(0, len(data), CHUNK)]
    frames.append(("E", b""))
    total = len(frames)

    start = port.mark()
    port.write(f"\x15{remote_bin} {remote}\r".encode())
    m = port.wait(rb"@READY1\.", start, 20)
    if not m:
        die("serrx did not start")
    pos = m.end()
    rx = re.compile(rb"@([AN])([0-9a-f]{8})\.|@(OK|BAD)([0-9a-f]{64})\.|@(ERR\w*|TIMEOUT|ABORT)\.")
    base = nxt = 0
    resends = stalls = 0
    t0 = last_progress = last_print = time.time()
    try:
        while True:
            while nxt < total and nxt < base + WINDOW:
                k, p = frames[nxt]
                port.write(frame(k, nxt, p))
                nxt += 1
            with port.lock:
                m = rx.search(port.buf, pos)
                if not m:
                    port.lock.wait(0.2)
                    m = rx.search(port.buf, pos)
            now = time.time()
            if m:
                pos = m.end()
                if m.group(1):
                    v = int(m.group(2), 16)
                    if v > base:
                        base, stalls, last_progress = v, 0, now
                    if m.group(1) == b"N":
                        resends += nxt - v
                        nxt = v
                    nxt = max(nxt, base)
                elif m.group(3) == b"OK":
                    if m.group(4).decode() != sha:
                        die(f"sha mismatch: remote {m.group(4).decode()} local {sha}")
                    dt = time.time() - t0
                    print(f"\r{remote}: {len(data)} B in {dt:.1f}s ({len(data) / dt / 1024:.1f} KiB/s), "
                          f"{resends} frames resent, sha256 {sha} OK" + " " * 8, file=sys.stderr)
                    return
                elif m.group(3) == b"BAD":
                    die(f"remote sha256 {m.group(4).decode()} != {sha} (file discarded)")
                else:
                    die(f"serrx: {m.group(5).decode()}")
            elif now - last_progress > RESEND_S:
                stalls += 1
                if stalls > MAX_STALLS:
                    raise RuntimeError("no progress, giving up")
                resends += nxt - base
                nxt, last_progress = base, now
            if now - last_print > 1.0:
                last_print = now
                done = min(len(data), max(0, base - 1) * CHUNK)
                rate = done / max(now - t0, 1e-3)
                eta = (len(data) - done) / rate if rate > 0 else 0
                print(f"\r{remote}: {100 * done / max(len(data), 1):5.1f}% {done}/{len(data)} B "
                      f"{rate / 1024:.1f} KiB/s ETA {eta:.0f}s resent {resends}  ", end="", file=sys.stderr)
    except BaseException:
        # Never leave serrx holding the console in raw mode.
        for _ in range(2):
            port.write(b"\n#X00000000000000000000:\n")
        port.wait(rb"@ABORT\.|@OK", pos, 5)
        raise


def kexec_exec(port, marker, timeout):
    start = port.mark()
    shell(port, "/bin/kexec -e", want_status=False)
    m = port.wait(rb"Starting new kernel|kexec_core", start, 30)
    if not m:
        die("kexec -e: no 'Starting new kernel' (still the old kernel?)")
    print("kexec -e: new kernel starting", file=sys.stderr)
    end = time.time() + timeout
    pos = m.end()
    while time.time() < end:
        lm = port.wait(rb"login: |[#$] $", pos, 5)
        if lm and lm.group(0) == b"login: " and port.login:
            pos = lm.end()
            user, _, pw = port.login.partition(":")
            port.write(user.encode() + b"\r")
            if port.wait(rb"assword: ", pos, 5):
                port.write(pw.encode() + b"\r")
            time.sleep(1)
        elif lm:
            pos = lm.end()
        s = port.mark()
        port.write(b'\x15cat /proc/cmdline; echo "@DO""NE$?."\r')
        r = port.wait(rb"@DONE\d+\.", s, 5)
        if r:
            text = port.buf[s:r.start()].decode(errors="replace")
            if all(a in text for a in marker.split()):
                print(f"new kernel up, cmdline has: {marker}", file=sys.stderr)
                return
            die(f"new kernel up but cmdline lacks {marker}:\n{redact(text)}")
    die(f"new kernel shell not reached in {timeout}s")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("local", nargs="?")
    ap.add_argument("remote", nargs="?")
    ap.add_argument("--port", default=os.environ.get("LEANDROS_PI_SERIAL", DEFAULT_PORT))
    ap.add_argument("--baud", type=int, default=115200,
                    help="host side rate; the Pi console is fixed at 115200 (no kernel baud knob yet)")
    ap.add_argument("--serrx", default=os.environ.get("LEANDROS_SERRX", "/bin/serrx"), help="receiver path on the target")
    ap.add_argument("--local-serrx", default=LOCAL_SERRX)
    ap.add_argument("--no-bootstrap", action="store_true")
    ap.add_argument("--cmd", help="run a shell command and print its output")
    ap.add_argument("--timeout", type=int, default=120)
    ap.add_argument("--kexec-exec", metavar="ARGS", help="kexec -e and verify ARGS in the new /proc/cmdline")
    ap.add_argument("--login", default=os.environ.get("LEANDROS_PI_LOGIN", ""),
                    help="user:password to answer a login prompt after kexec")
    ap.add_argument("--log", help="append everything received to this file")
    a = ap.parse_args()
    if a.baud != 115200:
        die("only 115200: the LeandrOS console has no baud ioctl or /proc knob yet")
    port = Port(a.port, a.baud, a.log)
    port.login = a.login
    try:
        if a.cmd:
            st, out = shell(port, a.cmd, timeout=a.timeout)
            sys.stdout.write(out)
            sys.exit(st)
        if a.kexec_exec is not None:
            kexec_exec(port, a.kexec_exec, a.timeout)
            return
        if not (a.local and a.remote):
            ap.error("need <local> <remote>, --cmd or --kexec-exec")
        ensure_serrx(port, a.serrx, a.local_serrx, not a.no_bootstrap)
        push(port, a.local, a.remote, a.serrx)
    finally:
        port.close()


if __name__ == "__main__":
    main()
