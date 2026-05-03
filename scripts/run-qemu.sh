#!/bin/bash
# LeandrOS Cross-Platform QEMU Runner Script
# Boots LeandrOS on both AArch64 and x86_64 architectures

set -e  # Exit on any error

# Detect Host OS and Architecture
OS=$(uname -s)
HOST_ARCH=$(uname -m)

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
    "/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
    "/usr/local/share/qemu/edk2-aarch64-code.fd"
)

X86_64_FW_PATHS=(
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd"
    "/usr/share/ovmf/x64/OVMF_CODE.fd"
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd"
    "/opt/homebrew/share/qemu/edk2-x86_64-code.fd"
    "/usr/local/share/qemu/edk2-x86_64-code.fd"
)

AARCH64_VARS_TEMPLATES=(
    "/usr/share/edk2/aarch64/QEMU_VARS.fd"
    "/opt/homebrew/share/qemu/edk2-arm-vars.fd"
    "/usr/local/share/qemu/edk2-arm-vars.fd"
)

X86_64_VARS_TEMPLATES=(
    "/usr/share/edk2/x64/OVMF_VARS.4m.fd"
    "/usr/share/ovmf/x64/OVMF_VARS.fd"
    "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"
    "/opt/homebrew/share/qemu/edk2-i386-vars.fd"
    "/usr/local/share/qemu/edk2-i386-vars.fd"
)

# Set architecture-specific parameters
if [ "$ARCH" = "aarch64" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    MACHINE_ARGS="-machine virt"
    CPU_ARGS="-cpu cortex-a57"
    ACCEL_ARGS=""
    
    # macOS Acceleration
    if [ "$OS" = "Darwin" ] && [ "$HOST_ARCH" = "arm64" ]; then
        ACCEL_ARGS="-accel hvf"
        CPU_ARGS="-cpu host"
    fi
    
    DISK_IMAGE="leandros-limine-aarch64.img"
    KERNEL_DIRECT="target/final-aarch64/kernel-direct"
    FW_PATHS=("${AARCH64_FW_PATHS[@]}")
    VARS_TEMPLATES=("${AARCH64_VARS_TEMPLATES[@]}")
    VARS_FILE="aarch64_vars.fd"
else
    QEMU_SYSTEM="qemu-system-x86_64"
    MACHINE_ARGS="-machine q35"
    CPU_ARGS=""
    ACCEL_ARGS=""
    
    # macOS Acceleration
    if [ "$OS" = "Darwin" ] && [ "$HOST_ARCH" = "x86_64" ]; then
        ACCEL_ARGS="-accel hvf"
        CPU_ARGS="-cpu host"
    fi
    
    DISK_IMAGE="leandros-limine-x86_64.img"
    KERNEL_DIRECT="target/final-x86_64/kernel-direct"
    FW_PATHS=("${X86_64_FW_PATHS[@]}")
    VARS_TEMPLATES=("${X86_64_VARS_TEMPLATES[@]}")
    VARS_FILE="x86_64_vars.fd"
fi

echo "🚀 Starting LeandrOS ($ARCH) in $BOOT_MODE mode"
echo "💻 Host: $OS ($HOST_ARCH)"
echo "=========================================="

if [ "$BOOT_MODE" = "uefi" ]; then
    UEFI_FIRMWARE=$(find_firmware "${FW_PATHS[@]}")
    if [ -z "$UEFI_FIRMWARE" ]; then
        echo "❌ UEFI firmware not found for $ARCH"
        exit 1
    fi
    
    echo "🏗️  Using UEFI: $UEFI_FIRMWARE"
    
    # Check for local vars file
    if [ ! -f "$VARS_FILE" ]; then
        VARS_TEMPLATE=$(find_firmware "${VARS_TEMPLATES[@]}")
        if [ -n "$VARS_TEMPLATE" ]; then
            echo "📝 Initializing $VARS_FILE from template"
            cp "$VARS_TEMPLATE" "$VARS_FILE"
            chmod +w "$VARS_FILE"
        fi
    fi

    # Build QEMU arguments
    # Using virtio-blk-pci with bootindex=0 is the most reliable way to boot UEFI images
    QEMU_ARGS=(
        $MACHINE_ARGS
        $CPU_ARGS
        $ACCEL_ARGS
        -m 512M
        -serial mon:stdio
        -device virtio-gpu-pci
        -boot menu=on,splash-time=0
        -net none
        -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE"
        -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE"
        -device virtio-blk-pci,drive=drive0,bootindex=0
        -no-reboot
    )

    if [ -f "$VARS_FILE" ]; then
        QEMU_ARGS+=(-drive if=pflash,unit=1,format=raw,file="$VARS_FILE")
    fi

    # Display settings
    if [ "$OS" != "Darwin" ]; then
        QEMU_ARGS+=(-display gtk)
    fi

    exec $QEMU_SYSTEM "${QEMU_ARGS[@]}"
else
    # Direct boot
    if [ ! -f "$KERNEL_DIRECT" ]; then
        echo "❌ Direct kernel not found: $KERNEL_DIRECT"
        exit 1
    fi
    
    echo "🏗️  Using Kernel: $KERNEL_DIRECT"
    
    if [ "$ARCH" = "aarch64" ]; then
        exec $QEMU_SYSTEM $MACHINE_ARGS $CPU_ARGS $ACCEL_ARGS -m 256M -serial mon:stdio \
            -kernel "$KERNEL_DIRECT" \
            -initrd "initrd-aarch64.cpio.gz" \
            -append "console=ttyAMA0" \
            -no-reboot
    else
        exec $QEMU_SYSTEM $MACHINE_ARGS $CPU_ARGS $ACCEL_ARGS -m 256M \
            -kernel "$KERNEL_DIRECT" \
            -initrd "initrd-x86_64.cpio.gz" \
            -append "console=ttyS0,115200" \
            -serial mon:stdio \
            -no-reboot
    fi
fi
