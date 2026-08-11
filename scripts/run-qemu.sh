#!/bin/bash
# LeandrOS Cross-Platform QEMU Runner Script
# Boots LeandrOS on both AArch64 and x86_64 architectures

set -e

OS=$(uname -s)
HOST_ARCH=$(uname -m)
BOOT_MODE="uefi"
ARCH="x86_64"
# HVF (Hypervisor.framework) auto-selects on an Apple Silicon host below, once
# ARCH/BOOT_MODE are known — only the aarch64 UEFI/Limine path supports it
# (fixed 2026-07-15, see drivers/src/virtio_gpu.rs's volatile-MMIO fix). Force
# override with --tcg (software emulation, e.g. for comparison/debugging) or
# --hvf (force HVF even off Apple Silicon, where it will fail to launch).
ACCEL=""
QEMU_EXTRA_ARGS=()
# Venus (Vulkan over virtio-gpu) mode. Opt-in only, via --venus below or
# LEANDROS_VENUS=1 for harnesses that cannot pass a flag. See the --venus block
# after the display selection for what it changes and why it never autodetects.
VENUS=0
if [ "${LEANDROS_VENUS:-0}" = "1" ]; then VENUS=1; fi
# --venus opens a real window when the host has a display server; this forces
# the offscreen egl-headless path instead, for harnesses that must not open one.
VENUS_HEADLESS=0
if [ "${LEANDROS_VENUS_HEADLESS:-0}" = "1" ]; then VENUS_HEADLESS=1; fi
# virgl (OpenGL passthrough) on x86_64 via virtio-vga-gl.
#
# OPT-IN, not default. The plumbing works end to end — `kmscube` inside the
# guest reports `renderer: "virgl (AMD Ryzen 9 7950X ... radeonsi ...)"`, i.e.
# real host-GPU OpenGL — but **cosmic-comp still dies with SIGSEGV** somewhere
# in the classic virgl resource path (RESOURCE_CREATE_3D / TRANSFER_*_3D, which
# Venus never exercised because it uses blob resources). Until that is fixed,
# the default has to stay on the device that gives a working desktop.
#
# `--virgl` (or LEANDROS_VIRGL=1) selects virtio-vga-gl. Note the guest's DRM
# identity follows the device automatically: with virgl negotiated card0 reports
# `virtio_gpu` so Mesa loads the virgl driver, otherwise it reports
# `leandros-drm` and Mesa falls through to softpipe.
VIRGL=0
if [ "${LEANDROS_VIRGL:-0}" = "1" ]; then VIRGL=1; fi

# Hardware acceleration only applies when the guest architecture matches the
# host's — a hypervisor virtualises, it does not translate. Map uname's arch
# spelling onto ours so "arm64" (macOS) and "aarch64" (Linux) compare equal.
host_arch_normalized() {
    case "$1" in
        arm64|aarch64) echo "aarch64" ;;
        x86_64|amd64)  echo "x86_64" ;;
        *)             echo "$1" ;;
    esac
}
HOST_ARCH_N=$(host_arch_normalized "$HOST_ARCH")

# Pick an audio backend that can actually open on this host, and echo the
# -audiodev argument for it. PulseAudio is not a safe default on Linux: a
# headless/SSH build box usually has no sound daemon, and QEMU ABORTS AT STARTUP
# when the backend cannot open — so guessing wrong costs the whole run, not just
# the sound. Probe what this QEMU build actually supports rather than assuming.
# Reads $QEMU_SYSTEM, so it must be called after the boot-mode dispatch sets it.
select_audio_args() {
    if [ "$OS" = "Darwin" ]; then
        echo "-audiodev coreaudio,id=snd0"
        return
    fi
    local backends
    backends=$($QEMU_SYSTEM -audiodev help 2>/dev/null || true)
    # A live PulseAudio/PipeWire-pulse session is the only positive evidence
    # that `pa` will connect; presence in -audiodev help only means it compiled.
    if [ -n "${PULSE_SERVER:-}" ] || [ -S "${XDG_RUNTIME_DIR:-/nonexistent}/pulse/native" ]; then
        echo "-audiodev pa,id=snd0"
    elif grep -q '\bpipewire\b' <<<"$backends"; then
        echo "-audiodev pipewire,id=snd0"
    elif grep -q '\balsa\b' <<<"$backends"; then
        echo "-audiodev alsa,id=snd0"
    else
        echo "-audiodev none,id=snd0"
    fi
}

# Firmware search paths. Ordered most-specific first; the first hit wins.
# Arch/EndeavourOS keeps edk2 under /usr/share/edk2/<arch>/ with a 4 MB split
# CODE/VARS pair, which is why the plain OVMF.fd names below do not match there.
X86_64_FW_PATHS=("/usr/share/ovmf/OVMF.fd" "/usr/share/OVMF/OVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-x86_64-code.fd" "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd" "/usr/share/edk2/x64/OVMF_CODE.4m.fd" "/usr/share/edk2/x64/OVMF_CODE.fd")
AARCH64_FW_PATHS=("/usr/share/AAVMF/AAVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-aarch64-code.fd" "/usr/share/edk2-armvirt/aarch64/QEMU_EFI-pflash.raw" "/usr/share/edk2/aarch64/QEMU_CODE.4m.fd" "/usr/share/edk2/aarch64/QEMU_CODE.fd" "/usr/share/edk2/aarch64/QEMU_EFI.fd")

# Matching writable VARS templates, same ordering convention. A split firmware
# build needs its own VARS pflash; a combined image (OVMF.fd) does not.
X86_64_VARS_PATHS=("/opt/homebrew/share/qemu/edk2-i386-vars.fd" "/usr/share/edk2/x64/OVMF_VARS.4m.fd" "/usr/share/edk2/x64/OVMF_VARS.fd" "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd" "/usr/share/OVMF/OVMF_VARS.fd")
AARCH64_VARS_PATHS=("/opt/homebrew/share/qemu/edk2-arm-vars.fd" "/usr/share/edk2/aarch64/QEMU_VARS.4m.fd" "/usr/share/edk2/aarch64/QEMU_VARS.fd" "/usr/share/edk2-armvirt/aarch64/vars-template-pflash.raw" "/usr/share/AAVMF/AAVMF_VARS.fd")

while [[ "$#" -gt 0 ]]; do
    case $1 in
        x86_64|aarch64) ARCH="$1"; shift ;;
        --direct) BOOT_MODE="direct"; shift ;;
        --uefi) BOOT_MODE="uefi"; shift ;;
        # QEMU raspi4b (BCM2711) — testable stepping stone for the sdhci
        # driver (drivers/src/sdhci.rs); aarch64-only, no PCI bus, no
        # GPU/sound/keyboard devices. Not a hardware target.
        --raspi4b) BOOT_MODE="raspi4b"; ARCH="aarch64"; shift ;;
        --hvf) ACCEL="hvf"; shift ;;
        --kvm) ACCEL="kvm"; shift ;;
        --tcg) ACCEL="tcg"; shift ;;
        --venus) VENUS=1; shift ;;
        --venus-headless) VENUS=1; VENUS_HEADLESS=1; shift ;;
        --virgl) VIRGL=1; shift ;;
        --no-virgl) VIRGL=0; shift ;;
        -d) QEMU_EXTRA_ARGS+=("$2"); shift 2 ;;
        *) QEMU_EXTRA_ARGS+=("$1"); shift ;;
    esac
done

if [ -z "$ACCEL" ]; then
    # Pick the fastest accelerator this host can actually provide for this
    # guest. Requires arch match; anything else falls back to TCG emulation.
    if [ "$HOST_ARCH_N" != "$ARCH" ]; then
        ACCEL="tcg"
    elif [ "$OS" = "Darwin" ]; then
        # HVF is only wired up for the aarch64 UEFI/Limine path; direct boot
        # hangs on an upstream PL011/HVF timer-starvation bug.
        if [ "$ARCH" = "aarch64" ] && [ "$BOOT_MODE" = "uefi" ]; then
            ACCEL="hvf"
        else
            ACCEL="tcg"
        fi
    elif [ "$OS" = "Linux" ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        ACCEL="kvm"
    else
        ACCEL="tcg"
    fi
fi

# Validate an explicit request rather than letting QEMU fail obscurely later.
case "$ACCEL" in
    hvf)
        if [ "$OS" != "Darwin" ]; then echo "❌ --hvf requires a macOS host"; exit 1; fi
        if [ "$ARCH" != "aarch64" ] || [ "$BOOT_MODE" != "uefi" ]; then
            echo "❌ --hvf only works with aarch64 --uefi (the default boot mode for that arch)"; exit 1
        fi ;;
    kvm)
        if [ "$OS" != "Linux" ]; then echo "❌ --kvm requires a Linux host"; exit 1; fi
        if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
            echo "❌ --kvm requested but /dev/kvm is not readable/writable (add your user to the 'kvm' group)"; exit 1
        fi
        if [ "$HOST_ARCH_N" != "$ARCH" ]; then
            echo "❌ --kvm cannot run a $ARCH guest on a $HOST_ARCH_N host"; exit 1
        fi ;;
esac

echo "⚡ Accelerator: $ACCEL (host ${OS}/${HOST_ARCH_N}, guest ${ARCH})"

if [ "$BOOT_MODE" = "raspi4b" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    # No gic-version=/-cpu override: raspi4b is a fixed-SoC board (4x
    # cortex-a72, GIC-400), unlike the generic `virt` machine.
    MACHINE_ARGS="-machine raspi4b -m 2G -smp 4"
    DISK_IMAGE="leandros-limine-aarch64.img" # unused in raspi4b mode
elif [ "$ARCH" = "aarch64" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    # -smp 4: SMP bringup via PSCI CPU_ON (GICv2 supports up to 8 CPUs).
    MACHINE_ARGS="-machine virt,gic-version=2 -m 2G -smp 4"
    # -cpu host: real host ID registers, required by HVF/KVM passthrough
    # (vs. -cpu max's synthesized model, which is TCG-only).
    #
    # lpa2=off on the TCG model: `max` otherwise advertises FEAT_LPA2 (52-bit
    # physical addresses) and Limine 11.4.1 wedges on it during its final
    # handoff — spinning forever on one instruction with the kernel entry
    # already in x0, so the kernel never prints. Nothing here uses 52-bit PAs.
    case "$ACCEL" in
        hvf) CPU_ARGS="-cpu host -accel hvf" ;;
        kvm) CPU_ARGS="-cpu host -accel kvm" ;;
        *)   CPU_ARGS="-cpu max,lpa2=off -accel tcg" ;;
    esac
    DISK_IMAGE="leandros-limine-aarch64.img"
else
    QEMU_SYSTEM="qemu-system-x86_64"
    # 2 cores × 2 threads: exercises the scheduler's SMT-aware idle-CPU
    # selection (CPUID leaf 0xB reports the hyperthread topology).
    MACHINE_ARGS="-machine q35 -smp 4,sockets=1,cores=2,threads=2"
    case "$ACCEL" in
        kvm) CPU_ARGS="-cpu host -accel kvm" ;;
        *)   CPU_ARGS="-cpu max -accel tcg" ;;
    esac
    DISK_IMAGE="leandros-limine-x86_64.img"
fi

# Select GPU device.
# x86_64: prefer virtio-vga — it is VGA-compatible so UEFI/OVMF exposes a GOP
#         framebuffer that Limine can use.  virtio-gpu-pci has no VGA interface
#         and leaves UEFI with no display device to hand to Limine.
# aarch64: virtio-gpu-pci is correct; VGA is an x86 concept.
GL_ARGS=()
if [ "$ARCH" = "aarch64" ]; then
    if $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-gpu-gl-pci; then
        GPU_DEV="virtio-gpu-gl-pci"
        GL_ARGS=("-display" "default,gl=on")
    else
        GPU_DEV="virtio-gpu-pci"
    fi
else
    # x86_64 needs a VGA-compatible device or OVMF has no GOP to hand Limine —
    # which is why virtio-gpu-gl-pci is NOT a candidate here, however much we
    # want its virgl. virtio-vga-gl is the device that satisfies both: it is
    # virtio-vga plus a virglrenderer context, so OVMF still sees VGA registers
    # and the guest still gets 3D. Prefer it, and keep plain virtio-vga as the
    # fallback for a QEMU built without virglrenderer.
    #
    # Ordering matters: `grep -q virtio-vga` also matches "virtio-vga-gl", so the
    # GL probe has to come FIRST or it can never be reached. That exact shadowing
    # is what made the old virtio-gpu-gl-pci branch below dead code on x86_64.
    if [ "$VIRGL" = "1" ] && $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-vga-gl; then
        GPU_DEV="virtio-vga-gl"
        GL_ARGS=("-display" "default,gl=on")
    elif $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-vga; then
        GPU_DEV="virtio-vga"
    elif $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-gpu-gl-pci; then
        GPU_DEV="virtio-gpu-gl-pci"
        GL_ARGS=("-display" "default,gl=on")
    else
        GPU_DEV="virtio-gpu-pci"
    fi
fi

# Select display. Without X or Wayland, QEMU's default GTK/SDL backend cannot
# open and the run dies at startup — so a headless host (an SSH session on a
# build box) must be told explicitly. egl-headless keeps the guest's virtio-gpu
# GL-capable, which venus needs; it just renders offscreen. Applies to every
# boot mode, so it lives before the boot-mode dispatch below.
# --venus makes its own display choice below and would only override this one,
# so skip it here rather than print a message that the next block contradicts.
if [ "$VENUS" = "0" ] && [ "$OS" != "Darwin" ] && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    if [ "${#GL_ARGS[@]}" -gt 0 ] && $QEMU_SYSTEM -display help 2>/dev/null | grep -q egl-headless; then
        GL_ARGS=("-display" "egl-headless")
        echo "🖥️  Headless host: using egl-headless (GL preserved)"
    else
        GL_ARGS=("-display" "none")
        echo "🖥️  Headless host: using -display none"
    fi
fi

# q35 adds a default std-VGA adapter that would become the primary display, so
# the x86_64 UEFI path suppresses it and lets virtio-vga be the sole device.
# Venus is the one case that wants it back (see below); nothing else changes it.
X86_UEFI_VGA_ARGS=(-vga none)

# Venus needs one specific device line, and every way of getting it wrong fails
# silently rather than loudly — a non-GL device, a -display that gets overridden,
# a QEMU built without virglrenderer, and a macOS host all produce a guest that
# merely reports "no Venus capset". So --venus never autodetects and never
# degrades: it either produces the proven line or refuses with the reason.
if [ "$VENUS" = "1" ]; then
    if [ "$OS" = "Darwin" ]; then
        echo "❌ --venus needs a host EGL implementation; macOS has none, so"
        echo "   virtio-gpu-gl-pci,venus=on cannot initialise. Use the Linux box."
        exit 1
    fi
    if [ "$BOOT_MODE" = "raspi4b" ]; then
        echo "❌ --venus is meaningless with --raspi4b: that board has no PCI bus and"
        echo "   the raspi4b command line attaches no GPU device at all."
        exit 1
    fi
    if ! $QEMU_SYSTEM -device help 2>&1 | grep -qE 'virtio-gpu-gl-pci|virtio-vga-gl'; then
        echo "❌ --venus needs a GL virtio-gpu device (virtio-vga-gl or"
        echo "   virtio-gpu-gl-pci), and this $QEMU_SYSTEM provides neither (a QEMU"
        echo "   built without virglrenderer). Fix the host QEMU."
        exit 1
    fi
    # -nographic implies -display none and silently wins over any -display
    # earlier on the command line, killing Venus with no diagnostic whatsoever.
    case " ${QEMU_EXTRA_ARGS[*]} " in
        *" -nographic "*)
            echo "❌ --venus is incompatible with -nographic: it implies -display none and"
            echo "   silently overrides the -display this block sets. Drop it — this script"
            echo "   already uses -serial mon:stdio."
            exit 1 ;;
    esac
    # Device: ONE head if the host can give us one.
    #
    # virtio-gpu-gl-pci has no VGA interface, so on x86_64/UEFI it has to be
    # paired with q35's default std-VGA to give OVMF/Limine a GOP. That leaves
    # the VM with TWO display consoles — std-VGA is console 0 and carries the
    # framebuffer text console, the GL device is console 1 and carries whatever
    # cosmic-comp scans out. A working desktop then looks like a black screen,
    # because the window, VNC and screendump all show console 0; you have to
    # switch to View #2 (or pass `screendump -d venusgpu`) to see anything.
    # Worse, under `-display gtk,gl=on` the two consoles fight over EGL contexts
    # and the host spams `Gdk-WARNING: eglMakeCurrent failed` while the UI stalls.
    #
    # virtio-vga-gl is virtio-vga PLUS a virglrenderer context, and it accepts
    # venus=on/blob=on/hostmem= just like virtio-gpu-gl-pci — so it satisfies
    # OVMF and Venus with a single device, one console, and `-vga none` intact.
    # Prefer it; keep the two-device layout only as the fallback for a QEMU that
    # lacks it. aarch64 has no VGA at all, so it always takes the -pci device.
    #
    # hostmem= backs the host-visible blob window Mesa's Venus ring maps.
    if [ "$ARCH" != "aarch64" ] && $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-vga-gl; then
        GPU_DEV="virtio-vga-gl,venus=on,blob=on,hostmem=4G,id=venusgpu"
    else
        GPU_DEV="virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G,id=venusgpu"
        # Only this path drops `-vga none`, and only on x86_64/UEFI, where the
        # GL device cannot give OVMF a GOP by itself.
        [ "$ARCH" = "aarch64" ] || X86_UEFI_VGA_ARGS=()
    fi
    # Display: a window when the host has a display server to open one on,
    # egl-headless otherwise. egl-headless keeps the GL pipeline alive but
    # attaches no window, which is right for an SSH session or a harness and
    # useless when you are trying to look at the desktop. LEANDROS_VENUS_DISPLAY
    # overrides with a literal QEMU -display spec; --venus-headless forces the
    # offscreen path even on a desktop (harnesses that must not open a window).
    if [ -n "${LEANDROS_VENUS_DISPLAY:-}" ]; then
        GL_ARGS=("-display" "$LEANDROS_VENUS_DISPLAY")
    elif [ "$VENUS_HEADLESS" = "0" ] && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; } \
         && $QEMU_SYSTEM -display help 2>/dev/null | grep -qx gtk; then
        GL_ARGS=("-display" "gtk,gl=on")
    else
        GL_ARGS=("-display" "egl-headless")
    fi
    echo "🌋 Venus: -device $GPU_DEV ${GL_ARGS[*]}"
    if [ "${#X86_UEFI_VGA_ARGS[@]}" -eq 0 ] && [ "$ARCH" != "aarch64" ]; then
        echo "   ⚠ two display consoles (std-VGA + GL): the desktop is on View #2."
    fi
fi


# ── Network backend ─────────────────────────────────────────────────────────
#
# Two mutually exclusive backends, and the choice changes the QEMU command line
# in TWO places (the -netdev argument, and whether QEMU is exec'd directly or
# through a wrapper), so it is decided once, here.
#
#   * vmnet, via socket_vmnet — macOS only. vmnet.framework requires root, and
#     socket_vmnet is the signed/notarized helper daemon that holds that
#     privilege so QEMU need not (see github.com/lima-vm/socket_vmnet). Its
#     client wrapper connects to the daemon's unix socket and hands QEMU the
#     resulting fd as fd 3 — which is the ONLY reason `-netdev socket,fd=3`
#     works. Start the daemon once with `sudo brew services start socket_vmnet`.
#     Guest gets a routable 192.168.105.2 by DHCP.
#
#   * user-mode (SLIRP) — everywhere else: Linux, and a Mac where socket_vmnet
#     is not installed or its daemon is not running. Needs no privilege and no
#     wrapper. Guest gets 10.0.2.15 behind QEMU's NAT, gateway/DNS 10.0.2.2.
#
# Either way the guest configures itself by DHCP (servers/net's smoltcp dhcpv4
# client), so nothing in the guest has to know which one it got. Only inbound
# connections differ: vmnet is reachable from the host, SLIRP needs -netdev
# user,hostfwd=... to expose a port.
#
# Before this, the socket_vmnet wrapper was exec'd unconditionally, so every
# UEFI run on Linux died with a not-found on the hardcoded Homebrew path.
SOCKET_VMNET_CLIENT=""
SOCKET_VMNET_SOCK=""
if [ "$OS" = "Darwin" ]; then
    HOMEBREW_PREFIX=$(brew --prefix 2>/dev/null || echo /opt/homebrew)
    _svc="$HOMEBREW_PREFIX/opt/socket_vmnet/bin/socket_vmnet_client"
    _svs="$HOMEBREW_PREFIX/var/run/socket_vmnet"
    if [ -x "$_svc" ] && [ -S "$_svs" ]; then
        SOCKET_VMNET_CLIENT="$_svc"
        SOCKET_VMNET_SOCK="$_svs"
    elif [ -x "$_svc" ]; then
        # Installed but not running: the wrapper would fail to connect and take
        # the whole run down with it. Say why, then fall back.
        echo "⚠️  socket_vmnet installed but its daemon is not running ($_svs missing)"
        echo "    → falling back to user-mode networking; start it with: sudo brew services start socket_vmnet"
    fi
fi

if [ -n "$SOCKET_VMNET_CLIENT" ]; then
    NETDEV_ARGS=(-netdev socket,id=net0,fd=3)
    NET_DESC="vmnet (via socket_vmnet), guest gets 192.168.105.2 via DHCP, host gateway 192.168.105.1"
else
    NETDEV_ARGS=(-netdev user,id=net0)
    NET_DESC="user-mode/SLIRP, guest gets 10.0.2.15 via DHCP, gateway+DNS 10.0.2.2"
fi

# ── F2FS data disks (created once, reused across runs) ──────────────────────
DATA0_IMG="f2fs-data0-${ARCH}.img"
DATA1_IMG="f2fs-data1-${ARCH}.img"
for IDX in 0 1; do
    FDISK="f2fs-data${IDX}-${ARCH}.img"
    if [ ! -f "$FDISK" ]; then
        echo "Creating $FDISK (64 MB)..."
        dd if=/dev/zero of="$FDISK" bs=1M count=64 2>/dev/null
        if command -v mkfs.f2fs &>/dev/null; then
            mkfs.f2fs -f -O "^extra_attr,^inline_data,^inline_dentry" "$FDISK"
        elif command -v python3 &>/dev/null; then
            python3 "$(dirname "$0")/mkfs-f2fs-minimal.py" "$FDISK"
        else
            echo "WARNING: neither mkfs.f2fs nor python3 found — $FDISK is blank"
        fi
    fi
done

# ── QMP socket (TODO.md item 18 gap 1) ──────────────────────────────────────
# HMP (this script's `-serial mon:stdio`) can't hold a chord — `sendkey`
# presses and releases a scancode in one shot, so Ctrl+Alt+Fn (the VT-switch
# combo) has no HMP equivalent — and HMP `mouse_move` is relative while our
# virtio-tablet is absolute-only, so it's silently dropped. A permanent QMP
# endpoint (mirroring driver.py's) fixes both. Unique per run (arch + this
# shell's pid) so two QEMUs never collide on one socket. Skip adding our own
# if the caller already passed one via extra args (e.g. `-d -qmp ...`).
QMP_ARGS=()
case " ${QEMU_EXTRA_ARGS[*]} " in
    *" -qmp "*) ;;  # caller already set one — don't add a second
    *)
        QMP_SOCK="/tmp/leandros-qmp-${ARCH}-$$.sock"
        rm -f "$QMP_SOCK"
        QMP_ARGS=(-qmp "unix:${QMP_SOCK},server=on,wait=off")
        echo "🔌 QMP: unix:${QMP_SOCK}"
        ;;
esac

echo "🚀 Starting LeandrOS ($ARCH) in $BOOT_MODE mode"
echo "=========================================="
if [ "$BOOT_MODE" = "uefi" ]; then
    echo "🌐 Network: $NET_DESC"
fi

if [ "$BOOT_MODE" = "raspi4b" ]; then
    KERNEL_ELF="target/final-aarch64/kernel-direct"
    if [ ! -f "$KERNEL_ELF" ]; then
        echo "❌ Direct kernel ELF not found: $KERNEL_ELF (build with: ./scripts/build-all.sh --arch aarch64 --raspi4b)"
        exit 1
    fi
    echo "🏗️  Using Direct Kernel ELF: $KERNEL_ELF (QEMU raspi4b — sdhci driver test path)"

    # No PCI bus exists on raspi4b (confirmed via QMP `info mtree`), so the
    # F2FS test image attaches through the SD card slot instead of
    # virtio-blk-pci. QEMU routes `-drive if=sd` to the second of two
    # generic-sdhci instances (0xfe340000), matching SDHCI_BASE in
    # drivers/src/sdhci.rs for this feature. No GPU/sound/keyboard devices
    # exist on this board — verification is serial-log only.
    # -accel tcg: force software emulation, matching the other direct-boot
    # paths below (avoids any host-acceleration mismatch with the new
    # EL3->EL2 boot prologue in kernel/src/entry_aarch64.s).
    exec $QEMU_SYSTEM $MACHINE_ARGS -accel tcg \
        -kernel "$KERNEL_ELF" \
        -device loader,file=initrd-aarch64.cpio,addr=0x48000000,force-raw=on \
        -drive if=sd,format=raw,file="$DATA0_IMG" \
        -net none \
        -serial mon:stdio \
        -parallel none \
        -no-reboot \
        "${QMP_ARGS[@]}" \
        "${QEMU_EXTRA_ARGS[@]}"
elif [ "$BOOT_MODE" = "uefi" ]; then
    UEFI_FIRMWARE=""
    FW_PATHS=("${X86_64_FW_PATHS[@]}")
    if [ "$ARCH" = "aarch64" ]; then FW_PATHS=("${AARCH64_FW_PATHS[@]}"); fi
    for path in "${FW_PATHS[@]}"; do if [ -f "$path" ]; then UEFI_FIRMWARE="$path"; break; fi; done
    if [ -z "$UEFI_FIRMWARE" ]; then echo "❌ UEFI firmware not found"; exit 1; fi
    
    # Locate a writable VARS template matching the firmware we picked.
    VARS_TEMPLATE=""
    VARS_PATHS=("${X86_64_VARS_PATHS[@]}")
    if [ "$ARCH" = "aarch64" ]; then VARS_PATHS=("${AARCH64_VARS_PATHS[@]}"); fi
    for path in "${VARS_PATHS[@]}"; do if [ -f "$path" ]; then VARS_TEMPLATE="$path"; break; fi; done

    AUDIO_ARGS=$(select_audio_args)

    if [ "$ARCH" = "aarch64" ]; then
        VARS_FILE="aarch64_vars.fd"
        if [ ! -f "$VARS_FILE" ]; then
            # Copy the host's VARS template; fall back to a blank 64 MB region
            # (edk2 will initialise it on first boot).
            cp "$VARS_TEMPLATE" "$VARS_FILE" 2>/dev/null || dd if=/dev/zero of="$VARS_FILE" bs=1M count=64
        fi

        # disable-legacy=on forces non-transitional (modern) VirtIO for block
        # devices.  Transitional devices (0x1001) trigger a QEMU 10.x deadlock
        # in the doorbell write handler on the virt machine because the new
        # coroutine-based block I/O path needs the iothread event loop — which
        # cannot run while inside the MMIO write handler.  Modern non-
        # transitional devices use a different notification path that doesn't
        # have this issue.
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m ${LEANDROS_QEMU_MEM:-2G} -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
            -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            -drive if=pflash,unit=1,format=raw,file="$VARS_FILE" \
            -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" \
            -device virtio-blk-pci,drive=drive0,bootindex=0,disable-legacy=on \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0,disable-legacy=on \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1,disable-legacy=on \
            -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -device virtio-net-pci,netdev=net0,disable-legacy=on "${NETDEV_ARGS[@]}" -no-reboot \
            "${QMP_ARGS[@]}")
    else
        # A split firmware (OVMF_CODE*) is read-only and needs its writable VARS
        # half as a second pflash unit; a combined image (OVMF.fd) does not.
        # Arch ships only the split pair, which is why this is not optional.
        X86_VARS_ARGS=()
        if [[ "$(basename "$UEFI_FIRMWARE")" == *CODE* ]] && [ -n "$VARS_TEMPLATE" ]; then
            X86_VARS_FILE="x86_64_vars.fd"
            if [ ! -f "$X86_VARS_FILE" ]; then cp "$VARS_TEMPLATE" "$X86_VARS_FILE"; fi
            X86_VARS_ARGS=(-drive "if=pflash,unit=1,format=raw,file=$X86_VARS_FILE")
        fi
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m ${LEANDROS_QEMU_MEM:-2G} -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
            -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            "${X86_VARS_ARGS[@]}" \
            -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" \
            -device virtio-blk-pci,drive=drive0,bootindex=0 \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0 \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1 \
            "${X86_UEFI_VGA_ARGS[@]}" -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -device virtio-net-pci,netdev=net0 "${NETDEV_ARGS[@]}" -no-reboot \
            "${QMP_ARGS[@]}")

    fi
    # The vmnet backend selected above is `-netdev socket,fd=3`, and that fd only
    # exists when QEMU is launched *through* socket_vmnet's client wrapper — so
    # the wrapper is part of the netdev choice, not an unconditional prefix. The
    # SLIRP backend needs no wrapper and no privilege, so it execs QEMU directly.
    if [ -n "$SOCKET_VMNET_CLIENT" ]; then
        exec "$SOCKET_VMNET_CLIENT" "$SOCKET_VMNET_SOCK" \
            $QEMU_SYSTEM "${QEMU_ARGS[@]}" "${QEMU_EXTRA_ARGS[@]}"
    else
        exec $QEMU_SYSTEM "${QEMU_ARGS[@]}" "${QEMU_EXTRA_ARGS[@]}"
    fi
else
    AUDIO_ARGS=$(select_audio_args)

    if [ "$ARCH" = "aarch64" ]; then
        # Use ELF for AArch64
        KERNEL_ELF="target/final-aarch64/kernel-direct"
        if [ ! -f "$KERNEL_ELF" ]; then echo "❌ Direct kernel ELF not found: $KERNEL_ELF"; exit 1; fi
        echo "🏗️  Using Direct Kernel ELF: $KERNEL_ELF"
        
        # QEMU's -initrd is part of the Linux boot protocol and is NOT loaded
        # for a bare ELF entered at its own entry point. Place the initrd at a
        # fixed physical address with -device loader instead; the kernel scans
        # RAM for the CPIO 070701 magic and finds it there. 0x48000000 is well
        # clear of the kernel image at 0x40080000.
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg \
            -kernel "$KERNEL_ELF" \
            -device loader,file=initrd-aarch64.cpio,addr=0x48000000,force-raw=on \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0,disable-legacy=on \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1,disable-legacy=on \
            -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -net none \
            -serial mon:stdio \
            -parallel none \
            -no-reboot \
            "${QMP_ARGS[@]}" \
            "${QEMU_EXTRA_ARGS[@]}"
    else
        # Use 32-bit ELF for x86_64 (PVH/Multiboot)
        KERNEL_ELF="target/final-x86_64/kernel-direct-32.elf"
        if [ ! -f "$KERNEL_ELF" ]; then 
            # Fallback to standard name if 32-bit specific one is missing
            KERNEL_ELF="target/final-x86_64/kernel-direct"
        fi
        if [ ! -f "$KERNEL_ELF" ]; then echo "❌ Direct kernel ELF not found: $KERNEL_ELF"; exit 1; fi
        echo "🏗️  Using Direct Kernel ELF: $KERNEL_ELF"
        
        # As on aarch64 direct boot, the kernel locates the initrd by scanning
        # for the CPIO magic; place it at a fixed physical address with
        # -device loader. 0x1000_0000 (256 MiB) is clear of the kernel image at
        # 0x10_0000 and within the trampoline's low-2 GiB HHDM window.
        # -vga none: q35 otherwise adds a default std VGA adapter that becomes
        # the primary display (showing only SeaBIOS), leaving the kernel's
        # VirtIO-GPU console on a secondary, unseen head.  Disabling it makes
        # VirtIO-GPU the sole display — matching the UEFI path above.
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg -m 2G \
            -kernel "$KERNEL_ELF" \
            -device loader,file=initrd-x86_64.cpio,addr=0x10000000,force-raw=on \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0 \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1 \
            -vga none -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -net none \
            -serial mon:stdio \
            -no-reboot \
            "${QMP_ARGS[@]}" \
            "${QEMU_EXTRA_ARGS[@]}"
    fi
fi

