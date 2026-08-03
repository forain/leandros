#!/bin/bash
# m7z2: targeted standard-kernel rebuild + boot-image recreation for one arch.
# Rebuilds only the Limine (standard) kernel — the UEFI boot path driver.py uses —
# and re-embeds it into leandros-limine-<arch>.img. Does NOT touch userland,
# initrd, or the f2fs data image.
set -e
ROOT_DIR="$PWD"
ARCH="${1:-aarch64}"
LIMINE_DIR="$ROOT_DIR/.limine-cache/limine-11.4.1-binary"

if [[ "$ARCH" == "aarch64" ]]; then
    TT="aarch64-unknown-kernel"; BOOT_EFI="BOOTAA64.EFI"
else
    TT="x86_64-unknown-kernel"; BOOT_EFI="BOOTX64.EFI"
fi
SPEC="$ROOT_DIR/targets/$ARCH-unknown-kernel.json"
LINKER="$ROOT_DIR/linkers/$ARCH.ld"
STD="target/build-$ARCH-standard"

echo "=== [m7z2] building standard kernel for $ARCH ==="
RUSTFLAGS="-C link-arg=-T$LINKER -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro" \
cargo build -p kernel --target "$SPEC" --target-dir "$STD" --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec

mkdir -p "target/final-$ARCH"
cp "$STD/$TT/release/kernel" "target/final-$ARCH/kernel"

echo "=== [m7z2] recreating boot image leandros-limine-$ARCH.img ==="
IMG="leandros-limine-$ARCH.img"
dd if=/dev/zero of="$IMG" bs=1M count=512 2>/dev/null
if command -v sgdisk &> /dev/null; then
    sgdisk -n 1:2048:0 -t 1:ef00 "$IMG" >/dev/null 2>&1
else
    printf "g\nn\n1\n2048\n\nt\n1\nw\n" | fdisk "$IMG" >/dev/null 2>&1 || true
fi
TFAT="temp_fat_$ARCH.img"; rm -f "$TFAT"
mkfs.fat -C "$TFAT" 491520 -F 32 -n LEANDROS >/dev/null 2>&1
mmd -i "$TFAT" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
mcopy -oi "$TFAT" "$LIMINE_DIR/$BOOT_EFI" ::/EFI/BOOT/"$BOOT_EFI"
mcopy -oi "$TFAT" "$LIMINE_DIR/limine-bios.sys" ::/boot/limine/limine-bios.sys
mcopy -oi "$TFAT" "$LIMINE_DIR/limine-bios.sys" ::/limine-bios.sys
mcopy -oi "$TFAT" "target/final-$ARCH/kernel" ::/kernel.elf
mcopy -oi "$TFAT" "initrd-$ARCH.cpio" ::/initrd.gz
mcopy -oi "$TFAT" limine/limine.conf ::/limine.conf
dd if="$TFAT" of="$IMG" bs=512 seek=2048 conv=notrunc 2>/dev/null
rm -f "$TFAT"
if [[ "$ARCH" == "x86_64" ]]; then
    "$LIMINE_DIR/limine" bios-install "$IMG" >/dev/null 2>&1 || true
fi
echo "=== [m7z2] DONE $ARCH ==="
