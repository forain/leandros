#!/bin/bash
# CyanOS Cross-Platform Build Script
# Builds userland, kernel, and generates disk images

set -e  # Exit on any error

# Default configuration
DEFAULT_ARCH="both"
DEFAULT_LIMINE_VERSION="11.4.1"
LIMINE_CACHE_DIR=".limine-cache"

# Parse command line arguments
ARCH="$DEFAULT_ARCH"
LIMINE_VERSION="$DEFAULT_LIMINE_VERSION"

show_usage() {
    echo "Usage: $0 [OPTIONS]"
    echo "Options:"
    echo "  --arch ARCH          Build for specific architecture: aarch64, x86_64, or both (default: both)"
    echo "  --help               Show this help message"
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --arch) ARCH="$2"; shift 2 ;;
        --help) show_usage; exit 0 ;;
        *) echo "❌ Unknown option: $1"; show_usage; exit 1 ;;
    esac
done

echo "🚀 CyanOS Build Process Started"
echo "🏗️  Architecture(s): $ARCH"

ROOT_DIR=$(pwd)

# Function to download and cache Limine
download_limine() {
    local version=$1
    local cache_dir="$LIMINE_CACHE_DIR/limine-$version-binary"
    if [ -d "$cache_dir" ]; then return 0; fi
    mkdir -p "$LIMINE_CACHE_DIR"
    local major_version=$(echo "$version" | cut -d'.' -f1)
    local url="https://github.com/limine-bootloader/limine/archive/refs/heads/v${major_version}.x-binary.tar.gz"
    cd "$LIMINE_CACHE_DIR"
    curl -L -o "limine-$version-binary.tar.gz" "$url"
    tar -xzf "limine-$version-binary.tar.gz"
    mv "Limine-${major_version}.x-binary" "limine-$version-binary"
    rm "limine-$version-binary.tar.gz"
    cd "$ROOT_DIR"
}

# Function to build userland
build_userland() {
    local arch=$1
    echo "📦 Building $arch userland..."
    if [[ "$arch" == "aarch64" ]]; then
        ./scripts/build-userland.sh --release
    else
        ./scripts/build-userland.sh --target amd64 --release
    fi
}

# Function to create initrd
create_initrd() {
    local arch=$1
    local initrd_name="initrd-$arch.cpio.gz"
    local target_arch=$([[ "$arch" == "aarch64" ]] && echo "aarch64-unknown-none" || echo "x86_64-unknown-none")
    local userland_dir="userland/target/$target_arch/release"
    
    echo "  Creating CPIO initrd..."
    local temp_dir="temp_initrd_$arch"
    rm -rf "$temp_dir"
    mkdir -p "$temp_dir/bin"
    
    cp "$userland_dir/init" "$temp_dir/init"
    cp "$userland_dir/shell" "$temp_dir/bin/shell"
    
    # Create uncompressed CPIO archive
    cd "$temp_dir"
    find . | cpio -o -H newc > "$ROOT_DIR/$initrd_name"
    cd "$ROOT_DIR"
    
    rm -rf "$temp_dir"
}

# Function to build kernel
build_kernel() {
    local arch=$1
    echo "🔧 Building $arch kernel..."
    
    local target_root="target/build-$arch"
    mkdir -p "$target_root"
    
    local target_spec="$ROOT_DIR/targets/$arch-unknown-kernel.json"
    local linker="$ROOT_DIR/linkers/$arch.ld"
    local rust_target="$arch-unknown-kernel"
    
    echo "  Running cargo build..."
    cargo clean -p kernel --target "$target_spec" --target-dir "$target_root" -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec || true
    RUSTFLAGS="-C link-arg=-T$linker -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro" \
    cargo +nightly build -p kernel --target "$target_spec" --target-dir "$target_root" --release -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec
    
    # Identify and preserve the binary
    mkdir -p "target/final-$arch"
    local built_kernel=$(find "$target_root" -name "kernel" -type f | grep release | grep -v deps | head -1)
    cp "$built_kernel" "target/final-$arch/kernel"
    cp "$built_kernel" "target/final-$arch/kernel-direct"
}

# Function to convert raw image to VDI
convert_to_vdi() {
    local arch=$1
    local raw_image="cyanos-limine-$arch.img"
    local vdi_image="cyanos-limine-$arch.vdi"
    if command -v VBoxManage &> /dev/null; then
        rm -f "$vdi_image"
        VBoxManage convertfromraw "$raw_image" "$vdi_image" --format VDI >/dev/null 2>&1
    fi
}

# Function to create disk image
create_disk_image() {
    local arch=$1
    local limine_dir="$2"
    local image_name="cyanos-limine-$arch.img"
    echo "💽 Creating $arch disk image..."
    dd if=/dev/zero of="$image_name" bs=1M count=64 2>/dev/null
    printf "g\nn\n1\n2048\n\nt\n1\nw\n" | fdisk "$image_name" >/dev/null 2>&1 || true
    local temp_fat="temp_fat_$arch.img"
    rm -f "$temp_fat"
    mkfs.fat -C "$temp_fat" 61440 -F 32 -n CYANOS >/dev/null 2>&1
    mmd -i "$temp_fat" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
    
    local boot_efi=$([[ "$arch" == "aarch64" ]] && echo "BOOTAA64.EFI" || echo "BOOTX64.EFI")
    mcopy -oi "$temp_fat" "$limine_dir/$boot_efi" ::/EFI/BOOT/$boot_efi
    mcopy -oi "$temp_fat" "$limine_dir/limine-bios.sys" ::/boot/limine/limine-bios.sys
    mcopy -oi "$temp_fat" "$limine_dir/limine-bios.sys" ::/limine-bios.sys
    mcopy -oi "$temp_fat" "target/final-$arch/kernel" ::/kernel.elf
    mcopy -oi "$temp_fat" "initrd-$arch.cpio.gz" ::/initrd.gz
    mcopy -oi "$temp_fat" limine/limine.conf ::/limine.conf
    
    dd if="$temp_fat" of="$image_name" bs=512 seek=2048 conv=notrunc 2>/dev/null
    rm -f "$temp_fat"
    
    if [[ "$arch" == "x86_64" ]]; then
        "$limine_dir/limine" bios-install "$image_name" >/dev/null 2>&1 || true
    fi

    convert_to_vdi "$arch"
}

# Main
download_limine "$LIMINE_VERSION"
LIMINE_DIR="$LIMINE_CACHE_DIR/limine-$LIMINE_VERSION-binary"

if [[ "$ARCH" == "both" || "$ARCH" == "aarch64" ]]; then
    build_userland "aarch64"
    create_initrd "aarch64"
    build_kernel "aarch64"
    create_disk_image "aarch64" "$LIMINE_DIR"
fi

if [[ "$ARCH" == "both" || "$ARCH" == "x86_64" ]]; then
    build_userland "x86_64"
    create_initrd "x86_64"
    build_kernel "x86_64"
    create_disk_image "x86_64" "$LIMINE_DIR"
fi

echo "🎉 Build Complete!"