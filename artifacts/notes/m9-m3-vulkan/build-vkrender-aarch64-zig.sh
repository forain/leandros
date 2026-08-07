#!/bin/sh
# Build vkrender for aarch64 WITHOUT a container.
#
# WHY THIS EXISTS (measured 2026-08-06 on forain@172.16.158.150):
# the README's recipe is `docker run --platform linux/arm64 alpine:3.21`. That
# recipe cannot run on the Linux box:
#   * there is no docker, only podman;
#   * podman happily PULLS the arm64 alpine image but cannot EXECUTE it —
#     /proc/sys/fs/binfmt_misc holds only `register` and `status`, i.e. no
#     aarch64 handler is registered, and the only qemu-aarch64 present is the
#     dynamically linked one from `qemu-user` (qemu-user-static is not
#     installed and we are not allowed to install it). The failure is
#     `exec /bin/uname: exec format error`.
# So aarch64 is cross-compiled instead, with the toolchain the artifacts repo
# already carries for exactly this purpose: zig cc + the from-source dynamic
# musl sysroot in leandros-artifacts/musl-dynamic.
#
# The output is ELF-shape identical to the shipping aarch64 vktest:
#   ELF 64-bit LSB pie executable, ARM aarch64, dynamically linked,
#   interpreter /lib/ld-musl-aarch64.so.1, NEEDED=[libc.so]
# so no patchelf --replace-needed step is required (zig's musl is already
# named libc.so), and mkfs-f2fs-populated.py picks it up unchanged.
#
# PREREQ: /tmp/vkheaders must contain vulkan/ AND vk_video/ (see below).
#   mkdir -p /tmp/vkheaders && cp -r /usr/include/vulkan /usr/include/vk_video /tmp/vkheaders/
# Do NOT simply mount/point at /usr/include: -I precedes the system include
# path, so glibc headers would shadow the target libc's.
set -e

VL="${VL:-$HOME/code/leandros-artifacts/venus-lane}"
MD="${MD:-$HOME/code/leandros-artifacts/musl-dynamic}"
T="$MD/toolchain"
SR="$MD/sysroot/aarch64"
HDR="${HDR:-/tmp/vkheaders}"

cd "$VL"
mkdir -p stage-aarch64/usr/bin

# -fno-sanitize=undefined is REQUIRED: zig cc turns UBSan on by default, and
# the link then fails with undefined __ubsan_handle_* (there is no UBSan
# runtime in our musl sysroot).
# -std=gnu11, NOT -std=c11: strict ISO hides POSIX clock_gettime/nanosleep and
# CLOCK_MONOTONIC behind feature-test macros under musl. See the note in
# build-vkrender-alpine-fixed.sh — the original script's -std=c11 does not
# compile against a real musl.
CFLAGS="-I$HDR -I$VL -fPIC -O2 -fno-sanitize=undefined -std=gnu11
        -Wall -Wextra -Wno-unused-parameter
        -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0"

# shellcheck disable=SC2086
"$T/aarch64-linux-musl-cc" -c vkrender.c   -o /tmp/vkrender_a64.o $CFLAGS
# shellcheck disable=SC2086
"$T/aarch64-linux-musl-cc" -c ssp_guard.c  -o /tmp/ssp_a64.o \
    -fPIC -O2 -fno-sanitize=undefined -fno-stack-protector

# zig cc's own driver auto-manages musl CRT/libc and silently produces a STATIC
# binary, which cannot dlopen the ICD. musl-dyn-link.sh bypasses the driver and
# links a dynamic PIE with ld.lld directly. (Landmine documented in
# leandros-artifacts/musl-dynamic/NOTES.md.)
sh "$T/musl-dyn-link.sh" aarch64 exe "$SR" \
    stage-aarch64/usr/bin/vkrender /tmp/vkrender_a64.o /tmp/ssp_a64.o

echo "== result =="
file stage-aarch64/usr/bin/vkrender
readelf -d stage-aarch64/usr/bin/vkrender | grep -i needed
echo "=== rc=0 arch=aarch64 vkrender ==="
