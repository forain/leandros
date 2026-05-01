#!/bin/bash
# LeandrOS Kernel-Direct QEMU Runner Script
# Boots LeandrOS kernel directly using QEMU -kernel flag (bypasses bootloader)

set -e  # Exit on any error

# Default to AArch64 if no architecture specified
ARCH="${1:-aarch64}"

# Validate architecture
case "$ARCH" in
    aarch64|arm64)
        ARCH="aarch64"
        KERNEL_PATH="target/aarch64-unknown-kernel/release/kernel-direct"
        QEMU_SYSTEM="qemu-system-aarch64"
        MACHINE_ARGS="-machine virt -cpu cortex-a57"
        ;;
    x86_64|amd64)
        ARCH="x86_64"
        KERNEL_PATH="target/x86_64-unknown-kernel/release/kernel-direct"
        QEMU_SYSTEM="qemu-system-x86_64"
        MACHINE_ARGS="-machine q35"
        ;;
    *)
        echo "❌ Unsupported architecture: $ARCH"
        echo "💡 Usage: $0 [aarch64|x86_64|amd64]"
        echo "   Examples:"
        echo "     $0 aarch64    # Boot AArch64 kernel directly"
        echo "     $0 x86_64     # Boot x86_64 kernel directly"
        echo "     $0            # Boot AArch64 kernel (default)"
        exit 1
        ;;
esac

echo "🚀 Starting LeandrOS kernel directly ($ARCH)"
echo "=========================================="

# Check if kernel exists
if [ ! -f "$KERNEL_PATH" ]; then
    echo "❌ Kernel not found: $KERNEL_PATH"
    echo "💡 Run './scripts/build-all.sh --arch $ARCH' to build the kernel"
    exit 1
fi

# Check if QEMU system is available
if ! command -v "$QEMU_SYSTEM" &> /dev/null; then
    echo "❌ QEMU system not found: $QEMU_SYSTEM"
    echo "💡 Install QEMU with Homebrew: brew install qemu"
    exit 1
fi

echo "🏗️  Architecture: $ARCH"
echo "📁 Using kernel: $KERNEL_PATH"
echo "⚡ Using QEMU: $QEMU_SYSTEM"
echo ""
echo "🎮 Boot sequence:"
echo "   1. QEMU loads kernel directly (no bootloader)"
echo "   2. LeandrOS kernel boots immediately"
echo "   3. Init process starts → '@' debug marker appears"
echo ""
echo "⏹️  Press Ctrl+C to exit QEMU"
echo "🔄 Booting..."
echo ""

# Launch QEMU with kernel-direct boot
if [ "$ARCH" = "aarch64" ]; then
    exec $QEMU_SYSTEM \
        $MACHINE_ARGS \
        -m 256M \
        -nographic \
        -kernel "$KERNEL_PATH" \
        -append "console=ttyAMA0" \
        -no-reboot
else
    # x86_64 - use standard BIOS boot for ELF kernel
    exec $QEMU_SYSTEM \
        $MACHINE_ARGS \
        -m 256M \
        -nographic \
        -kernel "$KERNEL_PATH" \
        -append "console=ttyS0,115200" \
        -serial mon:stdio \
        -no-reboot
fi