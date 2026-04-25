#!/bin/bash
# CyanOS Cross-Platform QEMU Runner Script
# Boots CyanOS on both AArch64 and x86_64 architectures

set -e  # Exit on any error

# Default settings
ARCH="aarch64"
BOOT_MODE="uefi"

show_usage() {
    echo "Usage: $0 [arch] [options]"
    echo ""
    echo "Architectures:"
    echo "  aarch64, arm64   (default)"
    echo "  x86_64, amd64"
    echo ""
    echo "Options:"
    echo "  --direct         Boot kernel directly (bypasses UEFI/Limine)"
    echo "  --uefi           Boot via UEFI/Limine (default)"
    echo "  --help           Show this help message"
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        aarch64|arm64)
            ARCH="aarch64"
            shift
            ;;
        x86_64|amd64)
            ARCH="x86_64"
            shift
            ;;
        --direct)
            BOOT_MODE="direct"
            shift
            ;;
        --uefi)
            BOOT_MODE="uefi"
            shift
            ;;
        --help)
            show_usage
            exit 0
            ;;
        *)
            echo "❌ Unknown option: $1"
            show_usage
            exit 1
            ;;
    esac
done

# Function to find firmware
find_firmware() {
    local paths=("$@")
    for p in "${paths[@]}"; do
        if [ -f "$p" ]; then
            echo "$p"
            return 0
        fi
    done
    return 1
}

# Define candidate paths
AARCH64_FW_PATHS=(
    "/usr/share/edk2/aarch64/QEMU_EFI.fd"
    "/usr/share/AAVMF/AAVMF_CODE.fd"
    "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd"
)

X86_64_FW_PATHS=(
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd"
    "/usr/share/ovmf/x64/OVMF_CODE.fd"
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"
)

# Set architecture-specific parameters
if [ "$ARCH" = "aarch64" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    MACHINE_ARGS="-machine virt -cpu cortex-a57"
    DISK_IMAGE="cyanos-limine-aarch64.img"
    KERNEL_DIRECT="target/final-aarch64/kernel-direct"
    FW_PATHS=("${AARCH64_FW_PATHS[@]}")
else
    QEMU_SYSTEM="qemu-system-x86_64"
    MACHINE_ARGS="-machine q35"
    DISK_IMAGE="cyanos-limine-x86_64.img"
    KERNEL_DIRECT="target/final-x86_64/kernel-direct"
    FW_PATHS=("${X86_64_FW_PATHS[@]}")
fi

echo "🚀 Starting CyanOS ($ARCH) in $BOOT_MODE mode"
echo "=========================================="

if [ "$BOOT_MODE" = "uefi" ]; then
    UEFI_FIRMWARE=$(find_firmware "${FW_PATHS[@]}")
    if [ -z "$UEFI_FIRMWARE" ]; then
        echo "❌ UEFI firmware not found for $ARCH"
        exit 1
    fi
    
    echo "🏗️  Using UEFI: $UEFI_FIRMWARE"
    
    if [ "$ARCH" = "aarch64" ]; then
        # Check for local vars file
        if [ ! -f "aarch64_vars.fd" ]; then
            VARS_TEMPLATE="/usr/share/edk2/aarch64/QEMU_VARS.fd"
            [ -f "$VARS_TEMPLATE" ] && cp "$VARS_TEMPLATE" aarch64_vars.fd && chmod +w aarch64_vars.fd
        fi
        
        exec $QEMU_SYSTEM $MACHINE_ARGS -m 512M -nographic \
            -drive if=pflash,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            $( [ -f "aarch64_vars.fd" ] && echo "-drive if=pflash,format=raw,file=aarch64_vars.fd" ) \
            -drive file="$DISK_IMAGE",if=virtio,format=raw \
            -no-reboot
    else
        exec $QEMU_SYSTEM $MACHINE_ARGS -m 256M -nographic -serial mon:stdio \
            -drive if=pflash,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            -drive format=raw,file="$DISK_IMAGE" \
            -no-reboot
    fi
else
    # Direct boot
    if [ ! -f "$KERNEL_DIRECT" ]; then
        echo "❌ Direct kernel not found: $KERNEL_DIRECT"
        exit 1
    fi
    
    echo "🏗️  Using Kernel: $KERNEL_DIRECT"
    
    if [ "$ARCH" = "aarch64" ]; then
        exec $QEMU_SYSTEM $MACHINE_ARGS -m 256M -nographic \
            -kernel "$KERNEL_DIRECT" \
            -initrd "initrd-aarch64.cpio.gz" \
            -append "console=ttyAMA0" \
            -no-reboot
    else
        exec $QEMU_SYSTEM $MACHINE_ARGS -m 256M -nographic \
            -kernel "$KERNEL_DIRECT" \
            -initrd "initrd-x86_64.cpio.gz" \
            -append "console=ttyS0,115200" \
            -serial mon:stdio \
            -no-reboot
    fi
fi
