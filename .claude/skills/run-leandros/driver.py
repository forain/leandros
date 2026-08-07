#!/usr/bin/env python3
"""LeandrOS QEMU driver for agent interaction.

Usage:
  driver.py start [aarch64|x86_64] [mode] [--venus]   Launch QEMU, wait for shell prompt
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

`--venus` (any position after `start`): boots the exact
`virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G` device line proven by
`scripts/run-qemu.sh --venus` (landed b2260b4), under `-display egl-headless`
instead of the default `-display none`. UEFI boot modes only — refused on
"direct"/"raspi4b" and on macOS (no host EGL; verify on the Linux box).
`cmd_screenshot`'s bare `screendump` (no `device=`) already works unchanged in
this mode and returns a valid PPM. Passing `device=` was tried and initially
failed with `DeviceNotFound` — QMP resolves `device=` as a qdev id, and the
device line above carries no `id=`. Adding `,id=venusgpu` makes `device=`
resolve, but only before a frame is presented; once one is, it fails with
`"no surface"` instead, because a virgl-backed scanout has no `DisplaySurface`
for QMP to dump. Bare `screendump` sidesteps both failure modes, which is why
`cmd_screenshot` stays unchanged here.

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
import shutil

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
    # Arch/EndeavourOS layout. edk2-armvirt renamed the firmware to QEMU_CODE
    # only recently; older packages still ship it as QEMU_EFI.fd, already padded
    # to the 64 MiB pflash size (unlike the same name on some other distros).
    "/usr/share/edk2/aarch64/QEMU_CODE.4m.fd",
    "/usr/share/edk2/aarch64/QEMU_CODE.fd",
    "/usr/share/edk2/aarch64/QEMU_EFI.fd",
]
X86_64_FW_PATHS = [
    "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
    # Arch/EndeavourOS layout
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/edk2/x64/OVMF_CODE.fd",
]
# Writable VARS templates matching the split CODE firmwares above. A combined
# image (OVMF.fd) carries its own vars and needs none of these.
AARCH64_VARS_PATHS = [
    "/opt/homebrew/share/qemu/edk2-arm-vars.fd",
    "/usr/share/edk2/aarch64/QEMU_VARS.4m.fd",
    "/usr/share/edk2/aarch64/QEMU_VARS.fd",
    "/usr/share/edk2-armvirt/aarch64/vars-template-pflash.raw",
    "/usr/share/AAVMF/AAVMF_VARS.fd",
]
X86_64_VARS_PATHS = [
    "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
    "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
    "/usr/share/edk2/x64/OVMF_VARS.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd",
    "/usr/share/OVMF/OVMF_VARS.fd",
]

# Venus (Vulkan over virtio-gpu) device line. Must stay byte-identical to
# run-qemu.sh's --venus block (landed b2260b4) — that is the exact line
# measured to work (venustest 68/68, vktest 0 failures, both arches), and any
# drift here would silently break it. hostmem= backs the host-visible blob
# window Mesa's Venus ring maps.
VENUS_GPU_DEV = "virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G"


def _host_arch():
    """uname -m spelling normalised so arm64/aarch64 and x86_64/amd64 compare."""
    m = platform.machine().lower()
    if m in ("arm64", "aarch64"):
        return "aarch64"
    if m in ("x86_64", "amd64"):
        return "x86_64"
    return m


def _kvm_usable(guest_arch: str) -> bool:
    """KVM virtualises, it does not translate — the guest arch must match the
    host's, and /dev/kvm must be usable by this user."""
    return (
        platform.system() == "Linux"
        and _host_arch() == guest_arch
        and os.access("/dev/kvm", os.R_OK | os.W_OK)
    )


def _accel_flags(guest_arch: str, mode: str):
    """Best available accelerator for this host/guest pair.

    -cpu host is required for hypervisor passthrough (real host ID registers);
    -cpu max is a synthesised model that only TCG can implement.
    """
    # lpa2=off on the aarch64 TCG model: `max` otherwise advertises FEAT_LPA2
    # (52-bit physical addresses), and Limine 11.4.1 wedges on it during its
    # final handoff — it spins forever on a single instruction in its
    # higher-half map with our kernel's entry already in x0, so the kernel
    # never prints a byte and the hang looks like a kernel fault. Nothing in
    # LeandrOS uses 52-bit PAs. Only reproduces where TCG is the accelerator
    # for an aarch64 guest (i.e. off Apple Silicon).
    tcg_cpu = "max,lpa2=off" if guest_arch == "aarch64" else "max"
    if mode == "uefi-tcg":
        return ["-cpu", tcg_cpu, "-accel", "tcg"]
    if mode == "uefi-hvf" or (guest_arch == "aarch64" and _is_apple_silicon()):
        return ["-cpu", "host", "-accel", "hvf"]
    if _kvm_usable(guest_arch):
        return ["-cpu", "host", "-accel", "kvm"]
    return ["-cpu", tcg_cpu, "-accel", "tcg"]

# VT100/ANSI escape sequence pattern — strips monitor line-editing noise
_ANSI_RE = re.compile(rb"\x1b\[[^a-zA-Z]*[a-zA-Z]|[\x08]|\x1b=|\x1b>")


def _strip_ansi(data: bytes) -> bytes:
    # Also strip [K (erase-to-EOL without ESC prefix, sent by QEMU monitor)
    data = re.sub(rb"\[[0-9;]*[A-Za-z]", b"", data)
    return _ANSI_RE.sub(b"", data)


# A shell prompt sitting at the very END of what we have received so far: a
# line of its own, a run of non-space characters, then "#", "$" or ">" and one
# space. brush's is "brush-0.5# ".
#
# Do NOT go back to "'> ' appears anywhere in the stream". Test programs print
# "-> " diagnostics by the dozen (scmtest emits one per subtest, e.g.
# "read via received fd -> 5 bytes"), so that heuristic cut a 25-subtest run
# off after the first one and the truncation read as a hang.
_PROMPT_TAIL_RE = re.compile(r"\n\S*[#$>] \Z")


def _at_prompt(buf: bytes) -> bool:
    """True iff `buf` ends at an interactive shell prompt."""
    # Only the tail can match, and this runs per received chunk, so never
    # rescan a megabyte of `mame` output to answer it.
    text = _strip_ansi(buf[-512:]).decode("utf-8", errors="replace")
    # reedline's cursor save/restore (ESC 7 / ESC 8) and keypad-mode toggles
    # sit between the prompt and end-of-buffer; _strip_ansi drops the CSI body
    # but leaves the ESC that introduced it, so clear both before matching.
    text = re.sub(r"\x1b[=>78]", "", text).replace("\x1b", "")
    return bool(_PROMPT_TAIL_RE.search(text))


def _find_fw(paths):
    for p in paths:
        if os.path.exists(p):
            return p
    return None


SOCKET_VMNET_PREFIXES = ["/opt/homebrew", "/usr/local"]


def _socket_vmnet_prefix():
    """Find socket_vmnet_client + its daemon socket, or None (see run-qemu.sh's
    matching comment for why the uefi path's -netdev socket,...,fd=3 needs this
    wrapper: vmnet.framework networking requires root, and socket_vmnet is the
    properly signed/notarized helper that holds that privilege so QEMU doesn't
    have to). Both the client AND a live daemon socket are required — the fd=3
    backend only exists when QEMU is exec'd *through* the client.

    socket_vmnet is macOS-only, so returning None is the normal case on Linux
    (and on a Mac whose daemon isn't running); the caller falls back to
    user-mode SLIRP, exactly as run-qemu.sh does."""
    for prefix in SOCKET_VMNET_PREFIXES:
        client = os.path.join(prefix, "opt/socket_vmnet/bin/socket_vmnet_client")
        sock = os.path.join(prefix, "var/run/socket_vmnet")
        if os.path.exists(client) and os.path.exists(sock):
            return client, sock
    return None


def _netdev_args():
    """The -netdev backend matching whatever _socket_vmnet_prefix() found.
    vmnet is reachable from the host; SLIRP needs -netdev user hostfwd for
    inbound connections, but outbound guest traffic works either way."""
    return (["-netdev", "socket,id=net0,fd=3"] if _socket_vmnet_prefix()
            else ["-netdev", "user,id=net0"])


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


def _build_cmd(arch, mode="uefi", venus=False):
    if venus:
        # Mirrors run-qemu.sh's --venus guards: it never autodetects and never
        # degrades, because every way of getting this wrong (wrong boot mode,
        # wrong host) fails silently rather than loudly downstream (a guest
        # that merely reports "no Venus capset", or a screendump that comes
        # back blank).
        if mode not in ("uefi", "uefi-hvf", "uefi-tcg"):
            sys.exit(f"ERROR: --venus only supports UEFI boot modes (got mode={mode!r})")
        if sys.platform == "darwin":
            sys.exit("ERROR: --venus needs a host EGL implementation; macOS has none, so "
                      "virtio-gpu-gl-pci,venus=on cannot initialise. Use the Linux box.")
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
        cpu_flags = _accel_flags("aarch64", mode)
        # Auto-create the writable VARS pflash the same way the x86_64 branch
        # below does. Without this file QEMU fails to open the pflash drive
        # and exits within the first second, before the serial chardev socket
        # ever binds — which used to make `start` report a launched guest
        # against a 0-byte serial log instead of failing (see cmd_start's
        # poll() check).
        vars_fd = os.path.join(REPO_ROOT, "aarch64_vars.fd")
        if not os.path.exists(vars_fd):
            vars_tpl = _find_fw(AARCH64_VARS_PATHS)
            if not vars_tpl:
                sys.exit("ERROR: AArch64 UEFI vars template not found "
                          "(and aarch64_vars.fd missing)")
            shutil.copyfile(vars_tpl, vars_fd)
        disk    = os.path.join(REPO_ROOT, "leandros-limine-aarch64.img")
        data0   = os.path.join(REPO_ROOT, "f2fs-data0-aarch64.img")
        data1   = os.path.join(REPO_ROOT, "f2fs-data1-aarch64.img")
        # venus=False (the default) is byte-identical to the pre-venus command:
        # virtio-gpu-pci under -display none.
        gpu_dev = VENUS_GPU_DEV if venus else "virtio-gpu-pci"
        display_arg = "egl-headless" if venus else "none"
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
            "-device", gpu_dev,
            "-device", "virtio-keyboard-pci",
            "-device", "virtio-tablet-pci",
            *_audiodev_args(),
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on",
            *_netdev_args(),
            "-no-reboot", "-parallel", "none",
            "-display", display_arg,
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
        cpu_flags = _accel_flags("x86_64", mode)
        # A split firmware (OVMF_CODE*) is read-only and needs its writable VARS
        # half as a second pflash unit; a combined OVMF.fd carries its own.
        vars_args = []
        if "CODE" in os.path.basename(fw):
            vars_tpl = _find_fw(X86_64_VARS_PATHS)
            if vars_tpl:
                vars_fd = os.path.join(REPO_ROOT, "x86_64_vars.fd")
                if not os.path.exists(vars_fd):
                    shutil.copyfile(vars_tpl, vars_fd)
                vars_args = ["-drive", f"if=pflash,unit=1,format=raw,file={vars_fd}"]
        # venus=False (the default) is byte-identical to the pre-venus command:
        # -vga none + virtio-vga under -display none. In venus mode, drop -vga
        # none and let q35's implicit std-VGA back in — screendump's bare form
        # (no device=) captures THAT surface, since the GL device has none of
        # its own to dump (see VENUS_GPU_DEV above, and the module docstring's
        # `--venus` section for why `device=` isn't used instead). Matches
        # run-qemu.sh's --venus resetting X86_UEFI_VGA_ARGS to empty.
        vga_args = [] if venus else ["-vga", "none"]
        gpu_args = ["-device", VENUS_GPU_DEV] if venus else ["-device", "virtio-vga"]
        display_arg = "egl-headless" if venus else "none"
        return [
            "qemu-system-x86_64",
            "-machine", "q35", "-smp", "4,sockets=1,cores=2,threads=2", *cpu_flags, "-m", "2G",
            "-boot", "menu=on,splash-time=0",
            "-drive", f"if=pflash,unit=0,format=raw,readonly=on,file={fw}",
            *vars_args,
            "-drive", f"if=none,id=drive0,format=raw,file={disk}",
            "-device", "virtio-blk-pci,drive=drive0,bootindex=0",
            "-drive", f"if=none,id=data0,format=raw,file={data0}",
            "-device", "virtio-blk-pci,drive=data0",
            "-drive", f"if=none,id=data1,format=raw,file={data1}",
            "-device", "virtio-blk-pci,drive=data1",
            *vga_args, *gpu_args,
            "-device", "virtio-keyboard-pci",
            "-device", "virtio-tablet-pci",
            *_audiodev_args(),
            "-device", "virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on",
            "-device", "virtio-net-pci,netdev=net0",
            *_netdev_args(),
            "-no-reboot", "-parallel", "none",
            "-display", display_arg,
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


def _read_serial_until(sentinel, timeout=120, at_prompt=False):
    """
    Connect to serial socket and read until any sentinel appears or timeout.
    `sentinel` is a str/bytes or a list of them. With `at_prompt`, a shell
    prompt ending the stream also ends the read. Returns accumulated text,
    or None on failure.
    """
    if not isinstance(sentinel, (list, tuple)):
        sentinel = [sentinel]
    sentinels = [s.encode() if isinstance(s, str) else s for s in sentinel]
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
                    if any(sb in buf for sb in sentinels):
                        return buf.decode("utf-8", errors="replace")
                    if at_prompt and _at_prompt(buf):
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


def cmd_start(arch="aarch64", mode="uefi", venus=False):
    if _qemu_pid() is not None:
        print("QEMU already running. Run 'stop' first.")
        sys.exit(1)

    _cleanup_socks()
    open(SERIAL_LOG, "wb").close()

    qemu_cmd = _build_cmd(arch, mode, venus=venus)
    # Debug escape hatch: extra QEMU args, e.g.
    #   LEANDROS_QEMU_EXTRA='-trace enable=virtio_snd_*,file=/tmp/t.log'
    extra = os.environ.get("LEANDROS_QEMU_EXTRA")
    if extra:
        import shlex
        qemu_cmd += shlex.split(extra)
    if mode in ("uefi", "uefi-hvf", "uefi-tcg"):
        vmnet = _socket_vmnet_prefix()
        if vmnet:
            client, sock = vmnet
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

    print(f"Launching QEMU (PID {proc.pid}, arch={arch}{', venus' if venus else ''})...")

    # Wait for socket file. Report failure loudly rather than a started
    # guest: a missing/broken pflash file (the aarch64_vars.fd gap this fixes)
    # makes QEMU exit within the first second, before the serial chardev ever
    # binds its socket. Poll the process too, so that case is caught
    # immediately with QEMU's own exit code and stderr instead of silently
    # waiting out the full 15 s timeout and only then failing.
    deadline = time.time() + 15
    while not os.path.exists(SERIAL_SOCK):
        ret = proc.poll()
        if ret is not None:
            with open(QEMU_STDERR_LOG, "rb") as ef:
                err = ef.read(4096).decode(errors="replace")
            try:
                os.unlink(PID_FILE)
            except FileNotFoundError:
                pass
            sys.exit(f"ERROR: QEMU exited immediately (code {ret}) before the "
                      f"serial socket appeared.\nQEMU stderr:\n{err}")
        if time.time() > deadline:
            with open(QEMU_STDERR_LOG, "rb") as ef:
                err = ef.read(2048).decode(errors="replace")
            try:
                os.unlink(PID_FILE)
            except FileNotFoundError:
                pass
            sys.exit(f"ERROR: serial socket did not appear.\nQEMU stderr:\n{err}")
        time.sleep(0.1)

    # Only now is it true that a guest actually started.
    print(f"QEMU started (PID {proc.pid}, arch={arch}{', venus' if venus else ''})")
    print("Serial socket up. Waiting for login/shell prompt (up to 120s)...")
    # "> " used to be a sentinel here; it matched the boot log's own
    # "[INPUT] -> keyboard (event0)" and declared the guest ready ~40 s into a
    # boot that had not yet mounted its root. Only a real prompt counts now.
    text = _read_serial_until(["login: "], timeout=120, at_prompt=True)

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

    if "login: " in text:
        print("Login prompt ready (use: driver.py login <user> <password>).")
    else:
        print("Shell ready.")
    # Print last 1 KB of boot output so the caller can see the prompt
    try:
        with open(SERIAL_LOG, "rb") as lf:
            sys.stdout.buffer.write(lf.read()[-1024:])
            print()
    except Exception:
        pass


def cmd_login(user, password, timeout=20):
    """Answer the login:/Password: prompts, wait for the shell prompt."""
    s = _connect_with_retry(SERIAL_SOCK)
    if s is None:
        sys.exit("ERROR: cannot connect to serial socket")
    s.setblocking(False)

    def send_line(line):
        payload = (line + "\n").encode()
        s.setblocking(True)
        for i in range(0, len(payload), 8):
            s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        s.setblocking(False)

    def read_until(markers, deadline):
        buf = b""
        while time.time() < deadline:
            if select.select([s], [], [], 0.2)[0]:
                try:
                    chunk = s.recv(4096)
                except BlockingIOError:
                    continue
                if not chunk:
                    break
                buf += chunk
                if b"\x1b[6n" in chunk:
                    s.setblocking(True)
                    s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
                    s.setblocking(False)
                try:
                    with open(SERIAL_LOG, "ab") as lf:
                        lf.write(chunk)
                except Exception:
                    pass
                if any(m in buf for m in markers):
                    return buf
        return buf

    deadline = time.time() + timeout
    send_line(user)
    read_until([b"Password: "], deadline)
    send_line(password)
    out = read_until([b"> ", b"$ ", b"# ", b"Login incorrect"], deadline)
    s.close()
    text = out.decode("utf-8", errors="replace")
    print(text)
    if "Login incorrect" in text:
        sys.exit(1)


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
    # Sync on the shell prompt before sending. The brush/reedline line editor
    # only consumes RX once it is redrawing at its prompt; if we start writing
    # while it is still busy (right after a long-running command, a screenshot,
    # or QMP input) the first 8-byte chunk is silently dropped and the command
    # head is eaten ("export XDG..." -> "DG..."). Send a bare CR, wait until the
    # prompt "# " comes back, and only then write the real command.
    try:
        s.sendall(b"\r")
        sync = b""
        sync_deadline = time.time() + 2.0
        while time.time() < sync_deadline:
            if select.select([s], [], [], 0.1)[0]:
                c = s.recv(4096)
                if not c:
                    break
                if b"\x1b[6n" in c:
                    s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                sync += c
                if b"#" in _strip_ansi(sync)[-24:]:
                    break
    except Exception:
        pass
    time.sleep(0.05)
    # Pace the write: the guest PL011 RX FIFO is 16 bytes and the shell polls
    # it, so a single burst longer than ~16 bytes silently drops the head of
    # the command. 8-byte chunks with a small gap keep long command lines
    # intact. A leading space is insurance: if a head chunk is still dropped it
    # eats whitespace, not the command (brush ignores leading spaces).
    payload = ("  " + command + "\n").encode()
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
                # Minimal VT emulation: full-screen/interactive programs
                # (reedline/crossterm — e.g. /bin/brush) probe the terminal
                # with a cursor-position report request (ESC[6n) and hang
                # or bail if nothing answers. Reply like a real terminal.
                if b"\x1b[6n" in chunk:
                    try:
                        s.setblocking(True)
                        s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
                        s.setblocking(False)
                    except Exception:
                        pass
                try:
                    with open(SERIAL_LOG, "ab") as lf:
                        lf.write(chunk)
                except Exception:
                    pass
                # Stop once the shell is back at its prompt — and only at the
                # END of the stream, never on a "-> " in the middle of a line.
                if _at_prompt(buf[len(command):]):
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
    # Deliberately bare (no device=): under a --venus session this captures
    # the primary console (q35's implicit std-VGA, since venus mode drops -vga
    # none) and gives a valid non-blank PPM. Passing device=<gl-dev-id> fails
    # too — DeviceNotFound without an id= on the device line, "no surface"
    # once a frame is presented if one is added — see the module docstring's
    # `--venus` section. This is already correct for the non-venus default
    # path too, so no branching here.
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


def cmd_session(cmds, step_timeout=6):
    """Interactive session: send each command in sequence over ONE socket,
    answering terminal probes (cursor-position ESC[6n) like a real terminal
    the whole time. Needed for full-screen/line-editor programs (brush,
    reedline/crossterm) that bail out when the CPR query goes unanswered.
    Prints the full raw transcript."""
    s = _connect_with_retry(SERIAL_SOCK)
    if s is None:
        sys.exit("ERROR: cannot connect to serial socket")
    s.setblocking(False)
    transcript = b""
    answered = 0  # CPR probes already replied to

    def pump(duration):
        nonlocal transcript, answered
        end = time.time() + duration
        while time.time() < end:
            if select.select([s], [], [], 0.1)[0]:
                try:
                    chunk = s.recv(4096)
                except BlockingIOError:
                    continue
                if not chunk:
                    return
                transcript += chunk
                try:
                    with open(SERIAL_LOG, "ab") as lf:
                        lf.write(chunk)
                except Exception:
                    pass
                # Match against the whole transcript: the 4-byte ESC[6n probe
                # routinely arrives split across serial chunks.
                total = transcript.count(b"\x1b[6n")
                if total > answered:
                    try:
                        s.setblocking(True)
                        # Send the 7-byte cursor-position report as ONE burst:
                        # it fits the guest PL011's 16-byte RX FIFO, and a
                        # terminal escape sequence MUST arrive contiguously —
                        # crossterm times a lone ESC out as an Escape keypress
                        # and never assembles a byte-trickled CPR reply.
                        for _ in range(total - answered):
                            s.sendall(b"\x1b[24;1R")
                            time.sleep(0.02)
                        s.setblocking(False)
                    except Exception:
                        pass
                    answered = total

    pump(0.3)  # drain stale output, answer any pending probe
    for c in cmds:
        payload = (c + "\n").encode()
        s.setblocking(True)
        for i in range(0, len(payload), 8):
            s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        s.setblocking(False)
        pump(step_timeout)
    s.close()
    sys.stdout.write(transcript.decode("utf-8", errors="replace"))


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
        # --venus is a flag, not positional, so it can land anywhere after
        # "start" (e.g. "start x86_64 --venus" or "start --venus x86_64
        # uefi-tcg") without disturbing the arch/mode positions.
        venus = "--venus" in args[1:]
        positional = [a for a in args[1:] if not a.startswith("--")]
        arch = positional[0] if len(positional) > 0 else "aarch64"
        mode = positional[1] if len(positional) > 1 else "uefi"
        cmd_start(arch, mode, venus=venus)
    elif sub == "cmd":
        if len(args) < 2:
            sys.exit("Usage: driver.py cmd <shell-command> [timeout_seconds]")
        timeout = int(args[2]) if len(args) > 2 else 8
        cmd_cmd(args[1], timeout=timeout)
    elif sub == "login":
        if len(args) < 3:
            sys.exit("Usage: driver.py login <user> <password> [timeout_seconds]")
        cmd_login(args[1], args[2], timeout=int(args[3]) if len(args) > 3 else 20)
    elif sub == "screenshot":
        cmd_screenshot(args[1] if len(args) > 1 else None)
    elif sub == "stop":
        cmd_stop()
    elif sub == "status":
        cmd_status()
    elif sub == "log":
        cmd_log()
    elif sub == "session":
        # driver.py session <step_timeout_s> <cmd1> [<cmd2> ...]
        if len(args) < 3:
            sys.exit("Usage: driver.py session <step_timeout_s> <cmd> [<cmd> ...]")
        cmd_session(args[2:], step_timeout=int(args[1]))
    else:
        print(f"Unknown subcommand: {sub}")
        print(__doc__)
        sys.exit(1)
