#!/bin/sh
# kernel-cargo.sh SRC TARGET_DIR ARCH [JOBS] — the kernel build both sides of
# a self-host comparison run: build-all.sh's "standard" (Limine) kernel
# command, plus --remap-path-prefix for the three absolute paths that end up
# in panic locations (the checkout, CARGO_HOME, the sysroot's rust-src), so a
# host build and an in-guest build of the same commit can be compared byte
# for byte. Extra cargo flags (e.g. --offline) come from KERNEL_CARGO_FLAGS.
# POSIX sh: it runs under busybox in the guest's Alpine chroot.
set -eu
SRC=$(cd "${1:?usage: kernel-cargo.sh SRC TARGET_DIR ARCH [JOBS]}" && pwd)
TD="${2:?}"
ARCH="${3:?}"
JOBS="${4:-}"
cd "$SRC"
SYSROOT=$(rustc --print sysroot)
CH="${CARGO_HOME:-$HOME/.cargo}"
REMAP="--remap-path-prefix=$SRC=/leandros --remap-path-prefix=$CH=/cargo --remap-path-prefix=$SYSROOT=/sysroot"
export RUSTFLAGS="-C link-arg=-T$SRC/linkers/$ARCH.ld -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro -Z emit-stack-sizes $REMAP"
# shellcheck disable=SC2086
exec cargo build ${KERNEL_CARGO_FLAGS:-} ${JOBS:+-j "$JOBS"} -p kernel \
    --target "$SRC/targets/$ARCH-unknown-kernel.json" --target-dir "$TD" --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec
