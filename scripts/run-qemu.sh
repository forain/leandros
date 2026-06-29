#!/bin/bash
# LeandrOS Cross-Platform QEMU Runner Script
# Boots LeandrOS on both AArch64 and x86_64 architectures

set -e

OS=$(uname -s)
HOST_ARCH=$(uname -m)
BOOT_MODE="uefi"
ARCH="x86_64"
QEMU_EXTRA_ARGS=()

X86_64_FW_PATHS=("/usr/share/ovmf/OVMF.fd" "/usr/share/OVMF/OVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-x86_64-code.fd" "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd")
AARCH64_FW_PATHS=("/usr/share/AAVMF/AAVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-aarch64-code.fd" "/usr/share/edk2-armvirt/aarch64/QEMU_EFI-pflash.raw")

if [[ "$1" == "x86_64" || "$1" == "aarch64" ]]; then
    ARCH="$1"; shift
fi

while [[ "$#" -gt 0 ]]; do
    case $1 in
        --direct) BOOT_MODE="direct"; shift ;;
        --uefi) BOOT_MODE="uefi"; shift ;;
        -d) QEMU_EXTRA_ARGS+=("$2"); shift 2 ;;
        *) QEMU_EXTRA_ARGS+=("$1"); shift ;;
    esac
done

if [ "$ARCH" = "aarch64" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    MACHINE_ARGS="-machine virt,gic-version=2 -m 2G"
    CPU_ARGS="-cpu max"
    DISK_IMAGE="leandros-limine-aarch64.img"
else
    QEMU_SYSTEM="qemu-system-x86_64"
    MACHINE_ARGS="-machine q35"
    CPU_ARGS="-cpu max"
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


# ── F2FS data disks (created once, reused across runs) ──────────────────────
for IDX in 0 1; do
    FDISK="f2fs-data${IDX}.img"
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
    UEFI_FIRMWARE=""
    FW_PATHS=("${X86_64_FW_PATHS[@]}")
    if [ "$ARCH" = "aarch64" ]; then FW_PATHS=("${AARCH64_FW_PATHS[@]}"); fi
    for path in "${FW_PATHS[@]}"; do if [ -f "$path" ]; then UEFI_FIRMWARE="$path"; break; fi; done
    if [ -z "$UEFI_FIRMWARE" ]; then echo "❌ UEFI firmware not found"; exit 1; fi
    
    # Select audio backend
    if [[ "$OS" == "Darwin" ]]; then
        AUDIO_ARGS="-audiodev coreaudio,id=snd0"
    else
        AUDIO_ARGS="-audiodev pa,id=snd0"
    fi

    if [ "$ARCH" = "aarch64" ]; then
        VARS_FILE="aarch64_vars.fd"
        if [ ! -f "$VARS_FILE" ]; then
            # Create a local copy of vars if not present
            cp /opt/homebrew/share/qemu/edk2-arm-vars.fd "$VARS_FILE" 2>/dev/null || dd if=/dev/zero of="$VARS_FILE" bs=1M count=64
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
            -drive if=none,id=data0,format=raw,file=f2fs-data0.img \
            -device virtio-blk-pci,drive=data0,disable-legacy=on \
            -drive if=none,id=data1,format=raw,file=f2fs-data1.img \
            -device virtio-blk-pci,drive=data1,disable-legacy=on \
            -device "$GPU_DEV" \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS -no-reboot)
    else
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m 2G -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
            -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" \
            -device virtio-blk-pci,drive=drive0,bootindex=0 \
            -drive if=none,id=data0,format=raw,file=f2fs-data0.img \
            -device virtio-blk-pci,drive=data0 \
            -drive if=none,id=data1,format=raw,file=f2fs-data1.img \
            -device virtio-blk-pci,drive=data1 \
            -vga none -device "$GPU_DEV" \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS -no-reboot)

    fi
    exec $QEMU_SYSTEM "${QEMU_ARGS[@]}" "${QEMU_EXTRA_ARGS[@]}"
else
    # Select audio backend for direct boot as well
    if [[ "$OS" == "Darwin" ]]; then
        AUDIO_ARGS="-audiodev coreaudio,id=snd0"
    else
        AUDIO_ARGS="-audiodev pa,id=snd0"
    fi

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
            -device "$GPU_DEV" \
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
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg -m 2G \
            -kernel "$KERNEL_ELF" \
            -device loader,file=initrd-x86_64.cpio,addr=0x10000000,force-raw=on \
            -device "$GPU_DEV" \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -net none \
            -serial mon:stdio \
            -no-reboot \
            "${QEMU_EXTRA_ARGS[@]}"
    fi
fi

