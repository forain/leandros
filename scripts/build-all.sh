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

# The sibling repos (brush, coreutils, bottom) are built from their own
# directories, which rust-toolchain.toml does not reach — it only applies inside
# this tree. Without an explicit toolchain they fall back to the machine default,
# which on a fresh box is stable and has no musl std, so they fail with
# "can't find crate for `core`". Pass the pinned toolchain explicitly, read from
# the toolchain file so the pin stays in one place.
PINNED_TOOLCHAIN="+$(sed -n 's/^channel = "\(.*\)"/\1/p' "$ROOT_DIR/rust-toolchain.toml" | head -1)"

# Emit `.stack_sizes` (exact per-function frame sizes, straight from the
# backend) into the linked kernel, and refuse to ship one whose frames threaten
# the kernel stack. A frame larger than the 128 KiB stack does not fault — the
# stack has no guard page — it overwrites the neighbouring buddy allocation, and
# the resulting corruption surfaces somewhere else entirely. That is how
# TODO.md item 15 spent a day looking like a host-dependent boot failure.
# The section is non-alloc: ~10 KB of file, nothing at runtime.
STACK_SIZES_FLAG="-Z emit-stack-sizes"
# Ceiling for a single frame. The largest legitimate frame in the tree is
# ~34 KB (`vfs_close_all_for`, inflated by LTO inlining the VFS/net/epoll
# teardown into it), so this leaves real headroom while still catching the
# 72 KB and 156 KB frames that caused item 15.
STACK_FRAME_BUDGET=49152

check_stack_frames() {
    local kernel="$1" label="$2"
    echo "  🪜 Checking $label kernel stack frames..."
    python3 "$ROOT_DIR/scripts/check-stack-frames.py" "$kernel" --budget "$STACK_FRAME_BUDGET"
}

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
    RUSTFLAGS="-C link-arg=-T$linker -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro $STACK_SIZES_FLAG" \
    cargo build -p kernel $features_arg --target "$target_spec" --target-dir "$target_root_std" --release -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec

    mkdir -p "target/final-$arch"
    cp "$target_root_std/$target_triple/release/kernel" "target/final-$arch/kernel"

    check_stack_frames "target/final-$arch/kernel" "$arch standard"

    # 2. Direct boot kernel
    echo "  Building direct-boot kernel..."
    local target_root_dir="target/build-$arch-direct"
    mkdir -p "$target_root_dir"
    local direct_linker="$ROOT_DIR/linkers/$arch-direct.ld"
    if [[ "$arch" == "aarch64" && "$RPI5" == "true" ]]; then
        # Derive the RPi5 direct-boot linker from the generic one so the two
        # cannot drift: KERNEL_PHYS is the only line that differs.
        #
        # The generic 0x40080000 is QEMU virt's RAM base, not a Pi address. On
        # real hardware the VideoCore does the file loading and reaches only low
        # memory, so a kernel_address up at 1GiB is refused before the file is
        # even read -- the firmware log shows the "Loading 'kernel.img' to ..."
        # line with no matching "Read kernel.img bytes ..." completion, and then
        # nothing at all. 0x200000 is where the firmware loads its own kernel.
        direct_linker="$ROOT_DIR/target/aarch64-direct-rpi5.ld"
        mkdir -p "$ROOT_DIR/target"
        sed 's/^KERNEL_PHYS = .*/KERNEL_PHYS = 0x00200000;/' \
            "$ROOT_DIR/linkers/aarch64-direct.ld" > "$direct_linker"
    fi
    cargo clean -p kernel --target "$target_spec" --target-dir "$target_root_dir" -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec || true
    RUSTFLAGS="-C link-arg=-T$direct_linker -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro $STACK_SIZES_FLAG" \
    cargo build -p kernel $features_arg --target "$target_spec" --target-dir "$target_root_dir" --release -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec

    cp "$target_root_dir/$target_triple/release/kernel" "target/final-$arch/kernel-direct"

    check_stack_frames "target/final-$arch/kernel-direct" "$arch direct"
    
    # Generate flat binary and 32-bit ELF for direct boot
    local sysroot
    sysroot=$(rustc --print sysroot)
    local host
    host=$(rustc -vV | grep host | cut -d' ' -f2)
    # The llvm-tools rustup component installs this binary under two different
    # names depending on the toolchain: `llvm-objcopy` on some, `rust-objcopy`
    # on others (aarch64-apple-darwin nightly ships only the latter). Probe for
    # both rather than assuming, then fall back to anything on PATH.
    local objcopy=""
    for cand in "$sysroot/lib/rustlib/$host/bin/llvm-objcopy" \
                "$sysroot/lib/rustlib/$host/bin/rust-objcopy" \
                "$(command -v llvm-objcopy 2>/dev/null)" \
                "$(command -v rust-objcopy 2>/dev/null)"; do
        if [[ -n "$cand" && -x "$cand" ]]; then objcopy="$cand"; break; fi
    done

    if [[ -n "$objcopy" ]]; then
        "$objcopy" -O binary "target/final-$arch/kernel-direct" "target/final-$arch/kernel-direct.bin"
        echo "  Flat binary generated: target/final-$arch/kernel-direct.bin"
        if [[ "$arch" == "x86_64" ]]; then
            "$objcopy" -O elf32-i386 "target/final-$arch/kernel-direct" "target/final-$arch/kernel-direct-32.elf"
            echo "  32-bit ELF generated: target/final-$arch/kernel-direct-32.elf"
        fi
    else
        # Delete rather than leave behind. A stale kernel-direct.bin is worse
        # than a missing one: scripts/prepare-rpi5-sdcard.sh and
        # scripts/deploy-rpi5.sh both only test that the file *exists*, so an
        # image left over from an earlier build would be flashed to hardware
        # without complaint while this warning scrolls past in the build log.
        rm -f "target/final-$arch/kernel-direct.bin" "target/final-$arch/kernel-direct-32.elf"
        echo "⚠️  No objcopy found (tried llvm-objcopy and rust-objcopy in $sysroot/lib/rustlib/$host/bin and on PATH)."
        echo "⚠️  Skipping flat binary generation; removed any stale target/final-$arch/kernel-direct.bin."
        echo "⚠️  Install it with: rustup component add llvm-tools"
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

    # doomgeneric is a shared, non-git sibling checkout: `../doomgeneric`
    # resolves to the SAME physical directory from every worktree of this
    # repo on a machine. Vendor the OBJDIR/atomic-link Makefile.leandros
    # (scripts/vendor/doomgeneric/Makefile.leandros) into that shared
    # checkout whenever it differs, so every worktree — and every machine,
    # the next time it runs build-all.sh — picks up the collision fix instead
    # of only the one checkout someone hand-edited. Idempotent: skipped once
    # the sibling already matches, so it doesn't perturb mtimes/incremental
    # state on every build. See artifacts/notes/lane-buildobj-2026-09-24.md.
    local vendored_makefile="$ROOT_DIR/scripts/vendor/doomgeneric/Makefile.leandros"
    if [[ -f "$vendored_makefile" ]] && ! cmp -s "$vendored_makefile" "$doom_dir/Makefile.leandros" 2>/dev/null; then
        echo "  Updating $doom_dir/Makefile.leandros from vendored copy..."
        cp "$vendored_makefile" "$doom_dir/Makefile.leandros"
    fi

    # Per-worktree, per-arch object directory, and no shared `make clean`:
    # two worktrees (or two arches) building concurrently against this one
    # shared sibling never read or clobber each other's .o files. The final
    # doom-$arch binary is still a shared, fixed path (mkfs-f2fs-populated.py
    # reads it by that name), but Makefile.leandros now links it atomically
    # (temp name + mv), so a concurrent reader never sees a torn file.
    local objdir="$doom_dir/.obj-$(basename "$ROOT_DIR")-$arch"
    (
        cd "$doom_dir" || exit 1
        make -f Makefile.leandros ARCH="$arch" LEANDROS_ROOT="$ROOT_DIR" OBJDIR="$objdir"
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
        cargo "$PINNED_TOOLCHAIN" build --target "$target_triple" --release
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
        cargo "$PINNED_TOOLCHAIN" build --target "$target_triple" --release \
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
        cargo "$PINNED_TOOLCHAIN" build -p brush-shell --target "$target_triple" --release
    )
}

# Function to build mkfs.fat — the FAT32 formatter disks-rs execs by bare name
# for every ESP and XBOOTLDR partition it lays down. Unlike brush/coreutils this
# is in-tree Rust (ports/mkfs-fat), so it has its own build.sh that owns the
# musl/ET_EXEC toolchain details, exactly as ports/greetd and ports/busd do; all
# that belongs here is the call. A failure is fatal, not a warning: the source
# is checked in, so the only way it breaks is a real regression.
build_mkfs_fat() {
    local arch="$1"
    echo "🗂️  Building $arch mkfs.fat..."
    if [[ ! -x "$ROOT_DIR/ports/mkfs-fat/build.sh" ]]; then
        echo "⚠️  ports/mkfs-fat not found, skipping"
        return 0
    fi
    "$ROOT_DIR/ports/mkfs-fat/build.sh" "$arch"
}

# Function to build spawnwedge — the musl thread/fork lock-handoff regression
# test (userland/spawnwedge). It cannot live in the userland workspace: that
# builds relibc-linked no_std binaries, and this test must run musl's own
# pthread_create/fork/pthread_exit protocol, so it is a std/musl crate with
# its own build.sh, in the mold of ports/mkfs-fat. In-tree Rust, so a failure
# is fatal.
build_spawnwedge() {
    local arch="$1"
    echo "🧵 Building $arch spawnwedge..."
    if [[ ! -x "$ROOT_DIR/userland/spawnwedge/build.sh" ]]; then
        echo "⚠️  userland/spawnwedge not found, skipping"
        return 0
    fi
    "$ROOT_DIR/userland/spawnwedge/build.sh" "$arch"
}

# Function to build killmt — kill -9 of a multithreaded process (userland/
# killmt). Same shape as spawnwedge: a std/musl crate with its own build.sh,
# because it needs musl's pthreads, fork() and waitpid(). In-tree Rust, so a
# failure is fatal.
build_killmt() {
    local arch="$1"
    echo "🔪 Building $arch killmt..."
    if [[ ! -x "$ROOT_DIR/userland/killmt/build.sh" ]]; then
        echo "⚠️  userland/killmt not found, skipping"
        return 0
    fi
    "$ROOT_DIR/userland/killmt/build.sh" "$arch"
}

# Function to build mkfs.xfs — the XFS v5 formatter for the root partition.
# Same shape as build_mkfs_fat; its build.sh takes one arch (or "all") and
# installs the binary, which cargo builds as mkfs_xfs because a target name
# cannot contain a dot, to ports/mkfs-xfs/out/<arch>/mkfs.xfs.
build_mkfs_xfs() {
    local arch="$1"
    echo "🗃️  Building $arch mkfs.xfs..."
    if [[ ! -x "$ROOT_DIR/ports/mkfs-xfs/build.sh" ]]; then
        echo "⚠️  ports/mkfs-xfs not found, skipping"
        return 0
    fi
    "$ROOT_DIR/ports/mkfs-xfs/build.sh" "$arch"
}

# Function to build disktester — AerynOS's disks-rs end-to-end provisioning
# driver, which is what actually exercises the block layer: sparse file, loop
# device, GPT write, BLKPG partition sync, then mkfs.fat/mkfs.xfs on the
# resulting partition nodes. The checkout is a sibling repo and stays
# UNMODIFIED, so it is built exactly like brush: same pinned nightly, same musl
# linker wrapper, and skipped with a warning when it is not present.
build_disktester() {
    local arch="$1"
    echo "🧪 Building $arch disktester (disks-rs)..."
    local disks_dir="$ROOT_DIR/../disks-rs"
    if [[ ! -d "$disks_dir" ]]; then
        echo "⚠️  disks-rs source not found at $disks_dir, skipping"
        return 0
    fi
    local target_triple
    if [[ "$arch" == "aarch64" ]]; then
        target_triple="aarch64-unknown-linux-musl"
    else
        target_triple="x86_64-unknown-linux-musl"
    fi
    (
        cd "$disks_dir" || exit 1
        RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no" \
        cargo "$PINNED_TOOLCHAIN" build -p disktester --target "$target_triple" --release --locked
    )
}

# Function to build the input-stack ABI shims (libseat, libudev). These are
# tracked C source (ports/input-stack/shims) that the image previously packed
# as a prebuilt blob from ~/code/leandros-artifacts/m4-input-ship because
# nothing in build-all.sh ever invoked the build script; wiring it in here
# closes that drift hole. Builds both arches in one call; degrades to a
# warning (not a failure) if zig is unavailable, matching the other
# out-of-repo build dependencies below.
build_input_stack_shims() {
    echo "🔌 Building input-stack shims (libseat, libudev)..."
    ./ports/input-stack/build-shims.sh
}


# Function to build + stage the D-Bus session package (busd, dbus-run-session,
# session.conf).
#
# Same job as build_input_stack_shims above and for the same reason: the image
# is packed from ~/code/leandros-artifacts/m5-session-ship/<arch>/, which is
# hand-synced and gitignored, so anything not regenerated from tracked source
# every build drifts away from the repo without saying so. It did — commit
# 84ec91a's .service activation sat in the repo and in no image for two days,
# and a boot test that only asked "does the desktop come up" called it green.
#
# A failure here is a warning, not a build stop: this needs cargo +nightly and
# the musl targets, which not every machine has, and there is a hard gate
# downstream — mkfs-f2fs-populated.py's verify_dbus_staging() refuses to build
# an image whose staged payload is older than its source. Warn here, refuse
# there; never silent in either place.
stage_dbus_session() {
    local arch="$1"
    echo "🚌 Building + staging D-Bus session package ($arch)..."
    if ! ./ports/busd/build.sh "$arch"; then
        echo ""
        echo "⚠️  ================================================================"
        echo "⚠️  ports/busd/build.sh FAILED for $arch."
        echo "⚠️  The staged busd/session.conf/dbus-run-session are whatever was"
        echo "⚠️  there before. mkfs will refuse to build the image if they are"
        echo "⚠️  older than ports/ — fix the toolchain, do not work around it."
        echo "⚠️  ================================================================"
        echo ""
    fi
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
    # relibc's C sources go through the cc crate, which with no CC set falls
    # back to the HOST compiler. That only works by accident on an aarch64 host:
    # relibc passes -mno-outline-atomics, an aarch64-only flag that x86_64 gcc
    # rejects outright ("unrecognized command-line option"), so an aarch64 build
    # dies on an x86_64 box. Point the cc crate at the same zig wrapper the rest
    # of the cross-build uses, keyed by the custom target's name, so the result
    # does not depend on which machine ran the build.
    local target_name="$arch-unknown-leandros"
    (
        cd "$relibc_dir" || exit 1
        # Build relibc using cargo
        env "CC_$target_name=$ROOT_DIR/scripts/cc-$arch-musl.sh" \
            "AR_$target_name=$ROOT_DIR/scripts/ar-musl.sh" \
        cargo build --target "$target_spec" --release -Z build-std=core,alloc,compiler_builtins

        # Also build ld_so and crt if they are part of the workspace and needed
        # (Already handled by workspace if configured correctly, but relibc's Makefile 
        # is the traditional way to get the full sysroot. For now we use cargo to get libc.a)
    )
}

# Main
download_limine "$LIMINE_VERSION"
LIMINE_DIR="$LIMINE_CACHE_DIR/limine-$LIMINE_VERSION-binary"

build_input_stack_shims

# Doom's General MIDI music uses the same pinned SoundFont on both CPUs.
if [[ -d "$ROOT_DIR/../doomgeneric" ]]; then
    python3 "$ROOT_DIR/scripts/soundfont.py"
fi

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
    build_mkfs_fat "$arch"
    build_mkfs_xfs "$arch"
    build_spawnwedge "$arch"
    build_killmt "$arch"
    build_disktester "$arch"
    stage_dbus_session "$arch"
    create_initrd "$arch"
    build_kernel "$arch"
    create_disk_image "$arch" "$LIMINE_DIR"
    echo "💾 Creating populated F2FS images for $arch..."
    python3 scripts/mkfs-f2fs-populated.py "f2fs-data0-$arch.img" "$arch"
    cp "f2fs-data0-$arch.img" "f2fs-data1-$arch.img"
done

echo "🎉 Build Complete!"
