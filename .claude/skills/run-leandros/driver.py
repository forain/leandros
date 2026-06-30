#!/usr/bin/env python3
"""LeandrOS QEMU driver for agent interaction.

Usage:
  driver.py start [aarch64|x86_64]   Launch QEMU, wait for shell prompt
  driver.py cmd "<command>"           Send shell command, print output
  driver.py screenshot [out.ppm]      Capture GPU framebuffer via monitor
  driver.py stop                      Quit QEMU cleanly
  driver.py status                    Check if QEMU is running
  driver.py log                       Dump accumulated serial log

All paths relative to the repo root (three levels up from this file).
"""

import socket
import subprocess
import sys
import time
import os
import re
import select

SERIAL_SOCK  = "/tmp/leandros-serial.sock"
MONITOR_SOCK = "/tmp/leandros-monitor.sock"
PID_FILE     = "/tmp/leandros-qemu.pid"
SERIAL_LOG   = "/tmp/leandros-serial.log"

REPO_ROOT = os.path.normpath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../..")
)

AARCH64_FW_PATHS = [
    "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
    "/usr/share/AAVMF/AAVMF_CODE.fd",
    "/usr/share/edk2-armvirt/aarch64/QEMU_EFI-pflash.raw",
]
X86_64_FW_PATHS = [
    "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
]

# VT100/ANSI escape sequence pattern — strips monitor line-editing noise
_ANSI_RE = re.compile(rb"\x1b\[[^a-zA-Z]*[a-zA-Z]|[\x08]|\x1b=|\x1b>")


def _strip_ansi(data: bytes) -> bytes:
    # Also strip [K (erase-to-EOL without ESC prefix, sent by QEMU monitor)
    data = re.sub(rb"\[[0-9;]*[A-Za-z]", b"", data)
    return _ANSI_RE.sub(b"", data)


def _find_fw(paths):
    for p in paths:
        if os.path.exists(p):
            return p
    return None


def _cleanup_socks():
    for p in [SERIAL_SOCK, MONITOR_SOCK]:
        try:
            os.unlink(p)
        except OSError:
            pass


def _build_cmd(arch):
    if arch == "aarch64":
        fw = _find_fw(AARCH64_FW_PATHS)
        if not fw:
            sys.exit("ERROR: AArch64 UEFI firmware not found")
        vars_fd = os.path.join(REPO_ROOT, "aarch64_vars.fd")
        disk    = os.path.join(REPO_ROOT, "leandros-limine-aarch64.img")
        data0   = os.path.join(REPO_ROOT, "f2fs-data0.img")
        data1   = os.path.join(REPO_ROOT, "f2fs-data1.img")
        return [
            "qemu-system-aarch64",
            "-machine", "virt,gic-version=2", "-cpu", "max", "-m", "2G",
            "-boot", "menu=on,splash-time=0",
            "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={fw}",
            "-drive", f"if=pflash,unit=1,format=raw,file={vars_fd}",
            "-drive", f"if=none,id=drive0,format=raw,file={disk}",
            "-device", "virtio-blk-pci,drive=drive0,bootindex=0,disable-legacy=on",
            "-drive", f"if=none,id=data0,format=raw,file={data0}",
            "-device", "virtio-blk-pci,drive=data0,disable-legacy=on",
            "-drive", f"if=none,id=data1,format=raw,file={data1}",
            "-device", "virtio-blk-pci,drive=data1,disable-legacy=on",
            "-device", "virtio-gpu-pci",
            "-audiodev", "none,id=snd0",
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-no-reboot", "-parallel", "none",
            "-display", "none",
            "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
            "-serial", "chardev:serial0",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
        ]
    elif arch == "x86_64":
        fw = _find_fw(X86_64_FW_PATHS)
        if not fw:
            sys.exit("ERROR: x86_64 UEFI firmware not found")
        disk  = os.path.join(REPO_ROOT, "leandros-limine-x86_64.img")
        data0 = os.path.join(REPO_ROOT, "f2fs-data0.img")
        data1 = os.path.join(REPO_ROOT, "f2fs-data1.img")
        return [
            "qemu-system-x86_64",
            "-machine", "q35", "-cpu", "max", "-m", "2G",
            "-boot", "menu=on,splash-time=0",
            "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={fw}",
            "-drive", f"if=none,id=drive0,format=raw,file={disk}",
            "-device", "virtio-blk-pci,drive=drive0,bootindex=0",
            "-drive", f"if=none,id=data0,format=raw,file={data0}",
            "-device", "virtio-blk-pci,drive=data0",
            "-drive", f"if=none,id=data1,format=raw,file={data1}",
            "-device", "virtio-blk-pci,drive=data1",
            "-vga", "none", "-device", "virtio-vga",
            "-audiodev", "none,id=snd0",
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-no-reboot", "-parallel", "none",
            "-display", "none",
            "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
            "-serial", "chardev:serial0",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
        ]
    else:
        sys.exit(f"ERROR: unknown arch '{arch}'")


def _qemu_pid():
    try:
        with open(PID_FILE) as f:
            pid = int(f.read().strip())
        os.kill(pid, 0)
        return pid
    except (FileNotFoundError, ProcessLookupError, ValueError, PermissionError):
        return None


def _connect_with_retry(sock_path, retries=40, delay=0.15):
    """Connect to a Unix socket with retries to handle server startup lag."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    for _ in range(retries):
        try:
            s.connect(sock_path)
            return s
        except (ConnectionRefusedError, FileNotFoundError):
            time.sleep(delay)
    s.close()
    return None


def _read_serial_until(sentinel, timeout=120):
    """
    Connect to serial socket and read until sentinel appears or timeout.
    Returns accumulated text, or None on failure.
    """
    sentinel_b = sentinel.encode() if isinstance(sentinel, str) else sentinel
    s = _connect_with_retry(SERIAL_SOCK)
    if s is None:
        return None

    s.setblocking(False)
    buf = b""
    lf  = open(SERIAL_LOG, "ab")
    deadline = time.time() + timeout

    try:
        while time.time() < deadline:
            ready = select.select([s], [], [], 0.5)[0]
            if ready:
                try:
                    chunk = s.recv(4096)
                    if not chunk:
                        break
                    buf += chunk
                    lf.write(chunk)
                    lf.flush()
                    if sentinel_b in buf:
                        return buf.decode("utf-8", errors="replace")
                except BlockingIOError:
                    pass
            elif _qemu_pid() is None:
                print("ERROR: QEMU died during boot", file=sys.stderr)
                return None
    finally:
        s.close()
        lf.close()

    return None


def cmd_start(arch="aarch64"):
    if _qemu_pid() is not None:
        print("QEMU already running. Run 'stop' first.")
        sys.exit(1)

    _cleanup_socks()
    open(SERIAL_LOG, "wb").close()

    qemu_cmd = _build_cmd(arch)
    proc = subprocess.Popen(
        qemu_cmd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,  # capture so we can report crashes
        close_fds=True,
    )
    with open(PID_FILE, "w") as f:
        f.write(str(proc.pid))

    print(f"QEMU started (PID {proc.pid}, arch={arch})")

    # Wait for socket file
    deadline = time.time() + 15
    while not os.path.exists(SERIAL_SOCK):
        if time.time() > deadline:
            err = proc.stderr.read(2048).decode(errors="replace")
            sys.exit(f"ERROR: serial socket did not appear.\nQEMU stderr:\n{err}")
        time.sleep(0.1)

    print("Serial socket up. Waiting for shell prompt (up to 120s)...")
    text = _read_serial_until("> ", timeout=120)

    if text is None:
        # Try to report what QEMU said
        try:
            with open(SERIAL_LOG, "rb") as lf:
                tail = lf.read()[-4096:].decode(errors="replace")
        except Exception:
            tail = "(no log)"
        print("WARNING: shell prompt not seen. Serial tail:", file=sys.stderr)
        print(tail, file=sys.stderr)
        sys.exit(1)

    print("Shell ready.")
    # Print last 1 KB of boot output so the caller can see the prompt
    try:
        with open(SERIAL_LOG, "rb") as lf:
            sys.stdout.buffer.write(lf.read()[-1024:])
            print()
    except Exception:
        pass


def _serial_send(command, timeout=8):
    """Send one command to the shell, return output up to next prompt."""
    s = _connect_with_retry(SERIAL_SOCK)
    if s is None:
        sys.exit("ERROR: cannot connect to serial socket")

    # Drain stale output
    s.setblocking(False)
    time.sleep(0.05)
    try:
        while select.select([s], [], [], 0.1)[0]:
            s.recv(4096)
    except Exception:
        pass

    s.setblocking(True)
    s.sendall((command + "\n").encode())
    s.setblocking(False)

    buf = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if select.select([s], [], [], 0.1)[0]:
            try:
                chunk = s.recv(4096)
                if not chunk:
                    break
                buf += chunk
                try:
                    with open(SERIAL_LOG, "ab") as lf:
                        lf.write(chunk)
                except Exception:
                    pass
                # Stop at prompt: anything that ends a line with "> "
                decoded = buf.decode("utf-8", errors="replace")
                if "> " in decoded[len(command):]:
                    break
            except BlockingIOError:
                pass
    s.close()

    text = buf.decode("utf-8", errors="replace")
    # Strip echoed command and trailing prompt lines
    lines = text.splitlines()
    out = []
    skip_echo = True
    for line in lines:
        if skip_echo and command.rstrip() in line:
            skip_echo = False
            continue
        stripped = line.strip()
        if stripped.endswith("> ") or stripped == ">":
            continue
        out.append(line)
    return "\n".join(out)


def cmd_cmd(command):
    if _qemu_pid() is None:
        sys.exit("ERROR: QEMU not running. Run 'start' first.")
    result = _serial_send(command)
    print(result)


def _monitor_send(command, timeout=10):
    """Send a QEMU monitor command, return stripped response."""
    s = _connect_with_retry(MONITOR_SOCK)
    if s is None:
        return "ERROR: cannot connect to monitor"

    s.setblocking(False)
    # Drain banner
    time.sleep(0.4)
    try:
        while select.select([s], [], [], 0.2)[0]:
            s.recv(4096)
    except Exception:
        pass

    s.setblocking(True)
    s.sendall((command + "\n").encode())
    s.setblocking(False)

    buf = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if select.select([s], [], [], 0.2)[0]:
            try:
                chunk = s.recv(4096)
                if not chunk:
                    break
                buf += chunk
                if b"(qemu)" in _strip_ansi(buf):
                    break
            except BlockingIOError:
                pass
    s.close()
    cleaned = _strip_ansi(buf).decode("utf-8", errors="replace")
    # Remove the echoed command from the response
    lines = [l for l in cleaned.splitlines()
             if l.strip() and l.strip() != "(qemu)" and command.strip() not in l]
    return "\n".join(lines)


def cmd_screenshot(outfile=None):
    if _qemu_pid() is None:
        sys.exit("ERROR: QEMU not running.")
    if outfile is None:
        outfile = "/tmp/leandros-screen.ppm"
    _monitor_send(f"screendump {outfile}", timeout=15)
    if os.path.exists(outfile):
        sz = os.path.getsize(outfile)
        print(f"Screenshot: {outfile} ({sz} bytes)")
        # Offer PNG conversion on macOS
        png = outfile.replace(".ppm", ".png")
        if os.system(f"sips -s format png {outfile} --out {png} 2>/dev/null") == 0:
            print(f"PNG:        {png}")
    else:
        print(f"WARNING: screendump did not create {outfile}")


def cmd_stop():
    pid = _qemu_pid()
    if pid is None:
        print("QEMU not running.")
        return
    _monitor_send("quit")
    time.sleep(1)
    try:
        os.kill(pid, 0)
        os.kill(pid, 15)
        time.sleep(2)
    except ProcessLookupError:
        pass
    for f in [PID_FILE]:
        try:
            os.unlink(f)
        except FileNotFoundError:
            pass
    _cleanup_socks()
    print("QEMU stopped.")


def cmd_status():
    pid = _qemu_pid()
    if pid:
        print(f"QEMU running (PID {pid})")
        print(f"  Serial socket : {'exists' if os.path.exists(SERIAL_SOCK) else 'MISSING'}")
        print(f"  Monitor socket: {'exists' if os.path.exists(MONITOR_SOCK) else 'MISSING'}")
        print(f"  Serial log    : {SERIAL_LOG}")
    else:
        print("QEMU not running.")


def cmd_log():
    try:
        with open(SERIAL_LOG, "rb") as f:
            sys.stdout.buffer.write(f.read())
    except FileNotFoundError:
        print("No log file yet.")


if __name__ == "__main__":
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        sys.exit(0)

    sub = args[0]
    if sub == "start":
        cmd_start(args[1] if len(args) > 1 else "aarch64")
    elif sub == "cmd":
        if len(args) < 2:
            sys.exit("Usage: driver.py cmd <shell-command>")
        cmd_cmd(args[1])
    elif sub == "screenshot":
        cmd_screenshot(args[1] if len(args) > 1 else None)
    elif sub == "stop":
        cmd_stop()
    elif sub == "status":
        cmd_status()
    elif sub == "log":
        cmd_log()
    else:
        print(f"Unknown subcommand: {sub}")
        print(__doc__)
        sys.exit(1)
