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
AARCH64_FW_PATHS=("/usr/share/AAVMF/AAVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-aarch64-code.fd" "/usr/share/edk2-armvirt/aarch64/QEMU_EFI-pflash.raw" "/usr/share/edk2/aarch64/QEMU_CODE.4m.fd" "/usr/share/edk2/aarch64/QEMU_CODE.fd")

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
    case "$ACCEL" in
        hvf) CPU_ARGS="-cpu host -accel hvf" ;;
        kvm) CPU_ARGS="-cpu host -accel kvm" ;;
        *)   CPU_ARGS="-cpu max -accel tcg" ;;
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
    # x86_64: virtio-vga provides VGA registers so OVMF can set up a GOP framebuffer.
    if $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-vga; then
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
if [ "$OS" != "Darwin" ] && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    if [ "${#GL_ARGS[@]}" -gt 0 ] && $QEMU_SYSTEM -display help 2>/dev/null | grep -q egl-headless; then
        GL_ARGS=("-display" "egl-headless")
        echo "🖥️  Headless host: using egl-headless (GL preserved)"
    else
        GL_ARGS=("-display" "none")
        echo "🖥️  Headless host: using -display none"
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
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m 2G -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
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
            -device virtio-net-pci,netdev=net0,disable-legacy=on "${NETDEV_ARGS[@]}" -no-reboot)
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
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m 2G -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
            -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            "${X86_VARS_ARGS[@]}" \
            -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" \
            -device virtio-blk-pci,drive=drive0,bootindex=0 \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0 \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1 \
            -vga none -device "$GPU_DEV" \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -device virtio-net-pci,netdev=net0 "${NETDEV_ARGS[@]}" -no-reboot)

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
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -net none \
            -serial mon:stdio \
            -no-reboot \
            "${QEMU_EXTRA_ARGS[@]}"
    fi
fi

