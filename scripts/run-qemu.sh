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
    MACHINE_ARGS="-machine virt"
    CPU_ARGS="-cpu max"
    DISK_IMAGE="leandros-limine-aarch64.img"
else
    QEMU_SYSTEM="qemu-system-x86_64"
    MACHINE_ARGS="-machine q35"
    CPU_ARGS="-cpu max"
    DISK_IMAGE="leandros-limine-x86_64.img"
fi


echo "🚀 Starting LeandrOS ($ARCH) in $BOOT_MODE mode"
echo "=========================================="

if [ "$BOOT_MODE" = "uefi" ]; then
    UEFI_FIRMWARE=""
    FW_PATHS=("${X86_64_FW_PATHS[@]}")
    if [ "$ARCH" = "aarch64" ]; then FW_PATHS=("${AARCH64_FW_PATHS[@]}"); fi
    for path in "${FW_PATHS[@]}"; do if [ -f "$path" ]; then UEFI_FIRMWARE="$path"; break; fi; done
    if [ -z "$UEFI_FIRMWARE" ]; then echo "❌ UEFI firmware not found"; exit 1; fi
    
    QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m 1G -serial mon:stdio -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" -device virtio-blk-pci,drive=drive0,bootindex=0 -no-reboot)
    exec $QEMU_SYSTEM "${QEMU_ARGS[@]}" "${QEMU_EXTRA_ARGS[@]}"
else
    if [ "$ARCH" = "aarch64" ]; then
        # Use FLAT BINARY for AArch64 to trigger QEMU Linux-style loader
        KERNEL_BIN="target/final-aarch64/kernel-direct.bin"
        if [ ! -f "$KERNEL_BIN" ]; then echo "❌ Direct kernel binary not found: $KERNEL_BIN"; exit 1; fi
        echo "🏗️  Using Direct Kernel Binary: $KERNEL_BIN"
        
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg -m 1G \
            -kernel "$KERNEL_BIN" \
            -initrd "initrd-aarch64.cpio" \
            -device virtio-gpu-pci \
            -net none \
            -serial mon:stdio \
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
        
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg -m 1G \
            -kernel "$KERNEL_ELF" \
            -initrd "initrd-x86_64.cpio" \
            -device virtio-gpu-pci \
            -net none \
            -serial mon:stdio \
            -no-reboot \
            "${QEMU_EXTRA_ARGS[@]}"
    fi
fi
