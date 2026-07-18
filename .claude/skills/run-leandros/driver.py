#!/usr/bin/env python3
"""LeandrOS QEMU driver for agent interaction.

Usage:
  driver.py start [aarch64|x86_64] [mode]   Launch QEMU, wait for shell prompt
  driver.py cmd "<command>"           Send shell command, print output
  driver.py screenshot [out.ppm]      Capture GPU framebuffer via monitor
  driver.py stop                      Quit QEMU cleanly
  driver.py status                    Check if QEMU is running
  driver.py log                       Dump accumulated serial log

`mode` (aarch64 only, default "uefi"): on an Apple Silicon host, "uefi" now
boots with HVF acceleration automatically (fixed 2026-07-15). Pass "uefi-tcg"
to force software emulation instead, or "uefi-hvf" to force HVF even on a
non-Apple-Silicon host (where it will fail to launch). "direct" and
"raspi4b" remain TCG-only regardless of host.

All paths relative to the repo root (three levels up from this file).
"""

import socket
import subprocess
import sys
import time
import os
import re
import select
import platform

SERIAL_SOCK  = "/tmp/leandros-serial.sock"
MONITOR_SOCK = "/tmp/leandros-monitor.sock"
PID_FILE     = "/tmp/leandros-qemu.pid"
SERIAL_LOG   = "/tmp/leandros-serial.log"
QEMU_STDERR_LOG = "/tmp/leandros-qemu-stderr.log"

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


SOCKET_VMNET_PREFIXES = ["/opt/homebrew", "/usr/local"]


def _socket_vmnet_prefix():
    """Find socket_vmnet_client + its daemon socket (see run-qemu.sh's matching
    comment for why the uefi path's -netdev socket,...,fd=3 needs this wrapper:
    vmnet.framework networking requires root, and socket_vmnet is the properly
    signed/notarized helper that holds that privilege so QEMU doesn't have to).
    Daemon must already be running (`sudo brew services start socket_vmnet`)."""
    for prefix in SOCKET_VMNET_PREFIXES:
        client = os.path.join(prefix, "opt/socket_vmnet/bin/socket_vmnet_client")
        if os.path.exists(client):
            return client, os.path.join(prefix, "var/run/socket_vmnet")
    sys.exit("ERROR: socket_vmnet_client not found (brew install socket_vmnet)")


def _cleanup_socks():
    for p in [SERIAL_SOCK, MONITOR_SOCK]:
        try:
            os.unlink(p)
        except OSError:
            pass


def _is_apple_silicon():
    return sys.platform == "darwin" and platform.machine() == "arm64"


def _audiodev_args():
    """Guest audio backend. Default discards audio (headless). Set
    LEANDROS_AUDIO_WAV=/path/out.wav to capture everything the guest plays
    through virtio-sound into a wav file — the only way to verify audio
    output headlessly."""
    wav = os.environ.get("LEANDROS_AUDIO_WAV")
    if wav:
        return ["-audiodev", f"wav,id=snd0,path={wav}"]
    return ["-audiodev", "none,id=snd0"]


def _build_cmd(arch, mode="uefi"):
    if mode == "direct":
        return _build_direct_cmd(arch)
    if mode == "raspi4b":
        return _build_raspi4b_cmd()
    if arch == "aarch64":
        fw = _find_fw(AARCH64_FW_PATHS)
        if not fw:
            sys.exit("ERROR: AArch64 UEFI firmware not found")
        # HVF passthrough requires `-cpu host` (real Apple Silicon ID registers) and
        # `-accel hvf`; this is now the default for the UEFI/Limine boot path on an
        # Apple Silicon host (fixed 2026-07-15, see drivers/src/virtio_gpu.rs's
        # volatile-MMIO fix) since it's a large (order-of-magnitude class) speedup
        # over TCG for CPU-bound guest workloads. "uefi-tcg" forces software
        # emulation back on (for comparison/debugging); "uefi-hvf" forces HVF on
        # even on a non-Apple-Silicon host, where it will simply fail to launch.
        # The direct-kernel-boot path stays TCG-only regardless — it hits a
        # separate, still-unfixed QEMU PL011-timer hang under HVF.
        if mode == "uefi-tcg":
            use_hvf = False
        elif mode == "uefi-hvf":
            use_hvf = True
        else:
            use_hvf = _is_apple_silicon()
        cpu_flags = ["-cpu", "host", "-accel", "hvf"] if use_hvf else ["-cpu", "max"]
        vars_fd = os.path.join(REPO_ROOT, "aarch64_vars.fd")
        disk    = os.path.join(REPO_ROOT, "leandros-limine-aarch64.img")
        data0   = os.path.join(REPO_ROOT, "f2fs-data0-aarch64.img")
        data1   = os.path.join(REPO_ROOT, "f2fs-data1-aarch64.img")
        return [
            "qemu-system-aarch64",
            "-machine", "virt,gic-version=2", "-smp", "4", *cpu_flags, "-m", "2G",
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
            *_audiodev_args(),
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on",
            "-netdev", "socket,id=net0,fd=3",
            "-no-reboot", "-parallel", "none",
            "-display", "none",
            "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
            "-serial", "chardev:serial0",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
        ]
    elif arch == "x86_64":
        if mode == "uefi-hvf":
            sys.exit("ERROR: uefi-hvf is aarch64-only (Hypervisor.framework can't "
                      "virtualize an x86_64 guest on Apple Silicon)")
        fw = _find_fw(X86_64_FW_PATHS)
        if not fw:
            sys.exit("ERROR: x86_64 UEFI firmware not found")
        disk  = os.path.join(REPO_ROOT, "leandros-limine-x86_64.img")
        data0 = os.path.join(REPO_ROOT, "f2fs-data0-x86_64.img")
        data1 = os.path.join(REPO_ROOT, "f2fs-data1-x86_64.img")
        return [
            "qemu-system-x86_64",
            "-machine", "q35", "-smp", "4,sockets=1,cores=2,threads=2", "-cpu", "max", "-m", "2G",
            "-boot", "menu=on,splash-time=0",
            "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={fw}",
            "-drive", f"if=none,id=drive0,format=raw,file={disk}",
            "-device", "virtio-blk-pci,drive=drive0,bootindex=0",
            "-drive", f"if=none,id=data0,format=raw,file={data0}",
            "-device", "virtio-blk-pci,drive=data0",
            "-drive", f"if=none,id=data1,format=raw,file={data1}",
            "-device", "virtio-blk-pci,drive=data1",
            "-vga", "none", "-device", "virtio-vga",
            *_audiodev_args(),
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-device", "virtio-net-pci,netdev=net0",
            "-netdev", "socket,id=net0,fd=3",
            "-no-reboot", "-parallel", "none",
            "-display", "none",
            "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
            "-serial", "chardev:serial0",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
        ]
    else:
        sys.exit(f"ERROR: unknown arch '{arch}'")


def _build_direct_cmd(arch):
    """Direct-boot (bare ELF, no Limine/UEFI) headless variant of run-qemu.sh's
    --direct path: same -kernel/-device loader placement, but with the
    chardev-socket serial/monitor/display setup used everywhere else in this
    driver instead of run-qemu.sh's `-serial mon:stdio` + graphical window.
    virtio-keyboard-pci is dropped (documented QEMU 10.x hang on this host)."""
    data0 = os.path.join(REPO_ROOT, f"f2fs-data0-{arch}.img")
    data1 = os.path.join(REPO_ROOT, f"f2fs-data1-{arch}.img")
    if arch == "aarch64":
        kernel = os.path.join(REPO_ROOT, "target/final-aarch64/kernel-direct")
        initrd = os.path.join(REPO_ROOT, "initrd-aarch64.cpio")
        if not os.path.exists(kernel):
            sys.exit(f"ERROR: direct-boot kernel not found: {kernel}")
        return [
            "qemu-system-aarch64",
            "-machine", "virt,gic-version=2", "-smp", "4", "-cpu", "max", "-m", "2G", "-accel", "tcg",
            "-kernel", kernel,
            "-device", f"loader,file={initrd},addr=0x48000000,force-raw=on",
            "-drive", f"if=none,id=data0,format=raw,file={data0}",
            "-device", "virtio-blk-pci,drive=data0,disable-legacy=on",
            "-drive", f"if=none,id=data1,format=raw,file={data1}",
            "-device", "virtio-blk-pci,drive=data1,disable-legacy=on",
            "-device", "virtio-gpu-pci",
            *_audiodev_args(),
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-net", "none", "-parallel", "none", "-no-reboot",
            "-display", "none",
            "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
            "-serial", "chardev:serial0",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
        ]
    elif arch == "x86_64":
        kernel = os.path.join(REPO_ROOT, "target/final-x86_64/kernel-direct-32.elf")
        if not os.path.exists(kernel):
            kernel = os.path.join(REPO_ROOT, "target/final-x86_64/kernel-direct")
        initrd = os.path.join(REPO_ROOT, "initrd-x86_64.cpio")
        if not os.path.exists(kernel):
            sys.exit(f"ERROR: direct-boot kernel not found: {kernel}")
        return [
            "qemu-system-x86_64",
            "-machine", "q35", "-smp", "4,sockets=1,cores=2,threads=2", "-cpu", "max", "-m", "2G", "-accel", "tcg",
            "-kernel", kernel,
            "-device", f"loader,file={initrd},addr=0x10000000,force-raw=on",
            "-drive", f"if=none,id=data0,format=raw,file={data0}",
            "-device", "virtio-blk-pci,drive=data0",
            "-drive", f"if=none,id=data1,format=raw,file={data1}",
            "-device", "virtio-blk-pci,drive=data1",
            "-vga", "none", "-device", "virtio-vga",
            *_audiodev_args(),
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-net", "none", "-no-reboot",
            "-display", "none",
            "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
            "-serial", "chardev:serial0",
            "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
        ]
    else:
        sys.exit(f"ERROR: unknown arch '{arch}'")


def _build_raspi4b_cmd():
    """QEMU -M raspi4b headless variant of run-qemu.sh's --raspi4b path —
    testable stepping stone for the sdhci driver (drivers/src/sdhci.rs), not
    a hardware target. aarch64-only. No PCI bus exists on this board
    (confirmed via QMP `info mtree`), so the F2FS test image attaches
    through the SD card slot (`-drive if=sd`, routed by QEMU to the second
    of two generic-sdhci instances at 0xfe340000 — matching SDHCI_BASE for
    this feature) instead of virtio-blk-pci. No GPU/sound/keyboard devices
    exist on this board — serial log only, same chardev-socket setup as
    every other mode here."""
    kernel = os.path.join(REPO_ROOT, "target/final-aarch64/kernel-direct")
    initrd = os.path.join(REPO_ROOT, "initrd-aarch64.cpio")
    data0 = os.path.join(REPO_ROOT, "f2fs-data0-aarch64.img")
    if not os.path.exists(kernel):
        sys.exit(f"ERROR: direct-boot kernel not found: {kernel} "
                  "(build with: ./scripts/build-all.sh --arch aarch64 --raspi4b)")
    return [
        "qemu-system-aarch64",
        "-machine", "raspi4b", "-m", "2G", "-smp", "4", "-accel", "tcg",
        "-kernel", kernel,
        "-device", f"loader,file={initrd},addr=0x48000000,force-raw=on",
        "-drive", f"if=sd,format=raw,file={data0}",
        "-net", "none", "-parallel", "none", "-no-reboot",
        "-display", "none",
        "-chardev", f"socket,id=serial0,path={SERIAL_SOCK},server=on,wait=off",
        "-serial", "chardev:serial0",
        "-monitor", f"unix:{MONITOR_SOCK},server,nowait",
    ]


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


def cmd_start(arch="aarch64", mode="uefi"):
    if _qemu_pid() is not None:
        print("QEMU already running. Run 'stop' first.")
        sys.exit(1)

    _cleanup_socks()
    open(SERIAL_LOG, "wb").close()

    qemu_cmd = _build_cmd(arch, mode)
    # Debug escape hatch: extra QEMU args, e.g.
    #   LEANDROS_QEMU_EXTRA='-trace enable=virtio_snd_*,file=/tmp/t.log'
    extra = os.environ.get("LEANDROS_QEMU_EXTRA")
    if extra:
        import shlex
        qemu_cmd += shlex.split(extra)
    if mode in ("uefi", "uefi-hvf", "uefi-tcg"):
        client, sock = _socket_vmnet_prefix()
        qemu_cmd = [client, sock] + qemu_cmd
    # stderr goes to a file, not a pipe: the pipe's read end vanishes when
    # this process exits, so a chatty QEMU (audio warnings, tracing) would
    # eventually block on a full pipe. The file also survives for post-mortem.
    stderr_f = open(QEMU_STDERR_LOG, "wb")
    proc = subprocess.Popen(
        qemu_cmd,
        stdout=subprocess.DEVNULL,
        stderr=stderr_f,
        close_fds=True,
    )
    with open(PID_FILE, "w") as f:
        f.write(str(proc.pid))

    print(f"QEMU started (PID {proc.pid}, arch={arch})")

    # Wait for socket file
    deadline = time.time() + 15
    while not os.path.exists(SERIAL_SOCK):
        if time.time() > deadline:
            with open(QEMU_STDERR_LOG, "rb") as ef:
                err = ef.read(2048).decode(errors="replace")
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
    # Pace the write: the guest PL011 RX FIFO is 16 bytes and the shell polls
    # it, so a single burst longer than ~16 bytes silently drops the head of
    # the command. 8-byte chunks with a small gap keep long command lines
    # intact.
    payload = (command + "\n").encode()
    for i in range(0, len(payload), 8):
        s.sendall(payload[i:i + 8])
        time.sleep(0.02)
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


def cmd_cmd(command, timeout=8):
    if _qemu_pid() is None:
        sys.exit("ERROR: QEMU not running. Run 'start' first.")
    result = _serial_send(command, timeout=timeout)
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
        arch = args[1] if len(args) > 1 else "aarch64"
        mode = args[2] if len(args) > 2 else "uefi"
        cmd_start(arch, mode)
    elif sub == "cmd":
        if len(args) < 2:
            sys.exit("Usage: driver.py cmd <shell-command> [timeout_seconds]")
        timeout = int(args[2]) if len(args) > 2 else 8
        cmd_cmd(args[1], timeout=timeout)
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
