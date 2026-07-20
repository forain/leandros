#!/bin/bash
# LeandrOS Cross-Platform Build Script
# Builds userland, kernel, and generates disk images

set -e  # Exit on any error

# Default configuration
DEFAULT_ARCH="both"
DEFAULT_LIMINE_VERSION="11.4.1"
LIMINE_CACHE_DIR=".limine-cache"

# Parse command line arguments
ARCH="$DEFAULT_ARCH"
LIMINE_VERSION="$DEFAULT_LIMINE_VERSION"
RPI5="false"
RASPI4B="false"

show_usage() {
    echo "Usage: $0 [OPTIONS]"
    echo "Options:"
    echo "  --arch ARCH          Build for specific architecture: aarch64, x86_64, or both (default: both)"
    echo "  --rpi5               Build with features for Raspberry Pi 5"
    echo "  --raspi4b            Build with features for QEMU -M raspi4b (sdhci driver test path)"
    echo "  --help               Show this help message"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) ARCH="$2"; shift 2 ;;
        --rpi5) RPI5="true"; shift ;;
        --raspi4b) RASPI4B="true"; shift ;;
        --help) show_usage; exit 0 ;;
        *) echo "❌ Unknown option: $1"; show_usage; exit 1 ;;
    esac
done

echo "🚀 LeandrOS Build Process Started"
echo "🏗️  Architecture(s): $ARCH"

ROOT_DIR="$PWD"

# Function to download and cache Limine
download_limine() {
    local version="$1"
    local cache_dir="$LIMINE_CACHE_DIR/limine-$version-binary"
    if [[ -d "$cache_dir" ]]; then return 0; fi
    mkdir -p "$LIMINE_CACHE_DIR"
    local major_version
    major_version=$(echo "$version" | cut -d'.' -f1)
    local url="https://github.com/limine-bootloader/limine/archive/refs/heads/v${major_version}.x-binary.tar.gz"
    
    (
        cd "$LIMINE_CACHE_DIR" || exit 1
        curl -L -o "limine-$version-binary.tar.gz" "$url"
        tar -xzf "limine-$version-binary.tar.gz"
        mv "Limine-${major_version}.x-binary" "limine-$version-binary"
        rm "limine-$version-binary.tar.gz"
    )
}

# Function to build userland
build_userland() {
    local arch="$1"
    echo "📦 Building $arch userland..."
    if [[ "$arch" == "aarch64" ]]; then
        ./scripts/build-userland.sh --release
    else
        ./scripts/build-userland.sh --target amd64 --release
    fi
}

# Function to create initrd
create_initrd() {
    local arch="$1"
    local initrd_name="initrd-$arch.cpio"
    local target_arch
    target_arch=$([[ "$arch" == "aarch64" ]] && echo "aarch64-unknown-none" || echo "x86_64-unknown-none")
    local userland_dir="userland/target/$target_arch/release"

    local temp_dir="temp_initrd_$arch"
    rm -rf "$temp_dir"
    mkdir -p "$temp_dir/bin"

    cp "$userland_dir/init" "$temp_dir/bin/init"

    (
        cd "$temp_dir" || exit 1
        find . -print0 | cpio -0 -o -H newc > "$ROOT_DIR/$initrd_name"
        # gzip -c "$ROOT_DIR/$initrd_name" > "$ROOT_DIR/$initrd_name.gz"
        cp "$ROOT_DIR/$initrd_name" "$ROOT_DIR/$initrd_name.gz"
    )

    rm -rf "$temp_dir"
}

# Function to build kernel
build_kernel() {
    local arch="$1"
    echo "🔧 Building $arch kernel..."
    
    local target_triple
    target_triple=$([[ "$arch" == "aarch64" ]] && echo "aarch64-unknown-kernel" || echo "x86_64-unknown-kernel")
    local target_spec="$ROOT_DIR/targets/$arch-unknown-kernel.json"
    
    # 1. Standard (Limine) kernel
    echo "  Building standard kernel..."
    local target_root_std="target/build-$arch-standard"
    mkdir -p "$target_root_std"
    local linker="$ROOT_DIR/linkers/$arch.ld"
    local features_arg=""
    if [[ "$arch" == "aarch64" && "$RPI5" == "true" ]]; then
        features_arg="--features rpi5"
    elif [[ "$arch" == "aarch64" && "$RASPI4B" == "true" ]]; then
        features_arg="--features raspi4b"
    fi
    cargo clean -p kernel --target "$target_spec" --target-dir "$target_root_std" -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec || true
    RUSTFLAGS="-C link-arg=-T$linker -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro" \
    cargo +nightly build -p kernel $features_arg --target "$target_spec" --target-dir "$target_root_std" --release -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec
    
    mkdir -p "target/final-$arch"
    cp "$target_root_std/$target_triple/release/kernel" "target/final-$arch/kernel"

    # 2. Direct boot kernel
    echo "  Building direct-boot kernel..."
    local target_root_dir="target/build-$arch-direct"
    mkdir -p "$target_root_dir"
    local direct_linker="$ROOT_DIR/linkers/$arch-direct.ld"
    cargo clean -p kernel --target "$target_spec" --target-dir "$target_root_dir" -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec || true
    RUSTFLAGS="-C link-arg=-T$direct_linker -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro" \
    cargo +nightly build -p kernel $features_arg --target "$target_spec" --target-dir "$target_root_dir" --release -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec
    
    cp "$target_root_dir/$target_triple/release/kernel" "target/final-$arch/kernel-direct"
    
    # Generate flat binary and 32-bit ELF for direct boot
    local sysroot
    sysroot=$(rustc --print sysroot)
    local host
    host=$(rustc -vV | grep host | cut -d' ' -f2)
    local objcopy="$sysroot/lib/rustlib/$host/bin/llvm-objcopy"

    if [[ -f "$objcopy" ]]; then
        "$objcopy" -O binary "target/final-$arch/kernel-direct" "target/final-$arch/kernel-direct.bin"
        echo "  Flat binary generated: target/final-$arch/kernel-direct.bin"
        if [[ "$arch" == "x86_64" ]]; then
            "$objcopy" -O elf32-i386 "target/final-$arch/kernel-direct" "target/final-$arch/kernel-direct-32.elf"
            echo "  32-bit ELF generated: target/final-$arch/kernel-direct-32.elf"
        fi
    else
        echo "⚠️  llvm-objcopy not found at $objcopy, skipping flat binary generation"
    fi
}

# Function to convert raw image to VDI
convert_to_vdi() {
    local arch="$1"
    local raw_image="leandros-limine-$arch.img"
    local vdi_image="leandros-limine-$arch.vdi"
    if command -v VBoxManage &> /dev/null; then
        rm -f "$vdi_image"
        VBoxManage convertfromraw "$raw_image" "$vdi_image" --format VDI >/dev/null 2>&1
    fi
}

# Function to create disk image
create_disk_image() {
    local arch="$1"
    local limine_dir="$2"
    local image_name="leandros-limine-$arch.img"
    echo "💽 Creating $arch disk image..."
    dd if=/dev/zero of="$image_name" bs=1M count=512 2>/dev/null
    if command -v sgdisk &> /dev/null; then
        sgdisk -n 1:2048:0 -t 1:ef00 "$image_name" >/dev/null 2>&1
    else
        printf "g\nn\n1\n2048\n\nt\n1\nw\n" | fdisk "$image_name" >/dev/null 2>&1 || true
    fi
    local temp_fat="temp_fat_$arch.img"
    rm -f "$temp_fat"
    mkfs.fat -C "$temp_fat" 491520 -F 32 -n LEANDROS >/dev/null 2>&1
    mmd -i "$temp_fat" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
    
    local boot_efi
    boot_efi=$([[ "$arch" == "aarch64" ]] && echo "BOOTAA64.EFI" || echo "BOOTX64.EFI")
    mcopy -oi "$temp_fat" "$limine_dir/$boot_efi" ::/EFI/BOOT/"$boot_efi"
    mcopy -oi "$temp_fat" "$limine_dir/limine-bios.sys" ::/boot/limine/limine-bios.sys
    mcopy -oi "$temp_fat" "$limine_dir/limine-bios.sys" ::/limine-bios.sys
    mcopy -oi "$temp_fat" "target/final-$arch/kernel" ::/kernel.elf
    # Use uncompressed for now as our simple parser doesn't handle .gz
    mcopy -oi "$temp_fat" "initrd-$arch.cpio" ::/initrd.gz
    mcopy -oi "$temp_fat" limine/limine.conf ::/limine.conf
    
    dd if="$temp_fat" of="$image_name" bs=512 seek=2048 conv=notrunc 2>/dev/null
    rm -f "$temp_fat"
    
    if [[ "$arch" == "x86_64" ]]; then
        "$limine_dir/limine" bios-install "$image_name" >/dev/null 2>&1 || true
    fi

    convert_to_vdi "$arch"
}

# Function to build doomgeneric
build_doom() {
    local arch="$1"
    echo "🎮 Building $arch doomgeneric..."
    local doom_dir="$ROOT_DIR/../doomgeneric"
    if [[ ! -d "$doom_dir" ]]; then
        echo "⚠️  doomgeneric source not found at $doom_dir, skipping"
        return 0
    fi
    (
        cd "$doom_dir" || exit 1
        make -f Makefile.leandros ARCH="$arch" LEANDROS_ROOT="$ROOT_DIR" clean
        make -f Makefile.leandros ARCH="$arch" LEANDROS_ROOT="$ROOT_DIR"
    )
}

# Function to build MAME
build_mame() {
    local arch="$1"
    echo "🕹️  Building $arch MAME..."
    local mame_dir="$ROOT_DIR/../mame"
    if [[ ! -d "$mame_dir" ]]; then
        echo "⚠️  MAME source not found at $mame_dir, skipping"
        return 0
    fi
    (
        ulimit -n 65536 2>/dev/null || true
        cd "$mame_dir" || exit 1
        make -f Makefile.leandros ARCH="$arch" \
            LEANDROS_ROOT="$ROOT_DIR" \
        || echo "⚠️  MAME $arch build failed, skipping"
    )
}

# Function to build bottom
build_bottom() {
    local arch="$1"
    echo "📊 Building $arch bottom..."
    local bottom_dir="$ROOT_DIR/../bottom-leandros"
    if [[ ! -d "$bottom_dir" ]]; then
        echo "⚠️  bottom source not found at $bottom_dir, skipping"
        return 0
    fi
    local target_triple
    if [[ "$arch" == "aarch64" ]]; then
        target_triple="aarch64-unknown-linux-musl"
    else
        target_triple="x86_64-unknown-linux-musl"
    fi
    (
        cd "$bottom_dir" || exit 1
        RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no" \
        cargo +nightly build --target "$target_triple" --release
    )

}

# Function to build uutils/coreutils (cat, ls, cp, mv, rm, ...)
build_coreutils() {
    local arch="$1"
    echo "🧰 Building $arch coreutils..."
    local coreutils_dir="$ROOT_DIR/../coreutils"
    if [[ ! -d "$coreutils_dir" ]]; then
        echo "⚠️  coreutils source not found at $coreutils_dir, skipping"
        return 0
    fi
    local target_triple
    if [[ "$arch" == "aarch64" ]]; then
        target_triple="aarch64-unknown-linux-musl"
    else
        target_triple="x86_64-unknown-linux-musl"
    fi
    local cc_var="CC_${target_triple//-/_}"
    local ar_var="AR_${target_triple//-/_}"
    (
        cd "$coreutils_dir" || exit 1
        # feat_os_unix_musl rather than the usual `unix`: it is upstream's own
        # musl set, which drops stdbuf (that util needs a cdylib, and a static
        # musl target cannot produce one).
        #
        # CC_<triple> points at the cc wrapper, not the linker wrapper, because
        # blake3 and oniguruma compile C/.S sources through cc-rs, and cc-rs
        # appends a --target spelling that zig rejects.
        #
        # AR_<triple> matters just as much: cc-rs otherwise defaults to the host
        # macOS ar, whose Mach-O-format archives ld.lld cannot read — the C
        # objects compile correctly and then every symbol in them comes back
        # undefined at link time.
        env "$cc_var=$ROOT_DIR/scripts/cc-$arch-musl.sh" \
            "$ar_var=$ROOT_DIR/scripts/ar-musl.sh" \
        RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no" \
        cargo +nightly build --target "$target_triple" --release \
            --no-default-features --features feat_os_unix_musl
    )
}

# Function to build brush (bash-compatible shell)
build_brush() {
    local arch="$1"
    echo "🐚 Building $arch brush..."
    local brush_dir="$ROOT_DIR/../brush"
    if [[ ! -d "$brush_dir" ]]; then
        echo "⚠️  brush source not found at $brush_dir, skipping"
        return 0
    fi
    local target_triple
    if [[ "$arch" == "aarch64" ]]; then
        target_triple="aarch64-unknown-linux-musl"
    else
        target_triple="x86_64-unknown-linux-musl"
    fi
    (
        cd "$brush_dir" || exit 1
        RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no" \
        cargo +nightly build -p brush-shell --target "$target_triple" --release
    )
}

# Function to build relibc
build_relibc() {
    local arch="$1"
    echo "📚 Building $arch relibc..."
    local target_spec="$ROOT_DIR/targets/$arch-unknown-leandros.json"
    local relibc_dir="$ROOT_DIR/../relibc"
    if [[ ! -d "$relibc_dir" ]]; then
        echo "⚠️  relibc source not found at $relibc_dir, skipping"
        return 0
    fi
    (
        cd "$relibc_dir" || exit 1
        # Build relibc using cargo
        cargo build --target "$target_spec" --release -Z build-std=core,alloc,compiler_builtins
        
        # Also build ld_so and crt if they are part of the workspace and needed
        # (Already handled by workspace if configured correctly, but relibc's Makefile 
        # is the traditional way to get the full sysroot. For now we use cargo to get libc.a)
    )
}

# Main
download_limine "$LIMINE_VERSION"
LIMINE_DIR="$LIMINE_CACHE_DIR/limine-$LIMINE_VERSION-binary"

# Determine architectures to build
if [[ "$ARCH" == "both" ]]; then
    ARCHS=("aarch64" "x86_64")
else
    ARCHS=("$ARCH")
fi

for arch in "${ARCHS[@]}"; do
    build_relibc "$arch"
    build_userland "$arch"
    build_doom "$arch"
    build_mame "$arch"
    build_bottom "$arch"
    build_brush "$arch"
    build_coreutils "$arch"
    create_initrd "$arch"
    build_kernel "$arch"
    create_disk_image "$arch" "$LIMINE_DIR"
    echo "💾 Creating populated F2FS images for $arch..."
    python3 scripts/mkfs-f2fs-populated.py "f2fs-data0-$arch.img" "$arch"
    cp "f2fs-data0-$arch.img" "f2fs-data1-$arch.img"
done

echo "🎉 Build Complete!"
