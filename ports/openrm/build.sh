#!/bin/bash
# Build NVIDIA's OS-agnostic resource manager (open-gpu-kernel-modules
# src/nvidia, i.e. what Linux calls nv-kernel.o) freestanding for the LeandrOS
# x86_64 kernel, and wrap it as libnvrm.a.
#
# This drives NVIDIA's own src/nvidia/Makefile unmodified, so file selection
# (srcs.mk, libspdm's openspdm.mk) and defines (-DNVRM, PORT_*, RS_*, ...) are
# exactly what the Linux open module ships. Only the toolchain is swapped:
#
#   CC  = clang --target=x86_64-unknown-linux-gnu   (SysV ABI, ELF, __linux__
#         so cpuopsys.h selects NV_LINUX/NV_UNIX like the tested Linux build)
#   LD  = rust-lld (gnu flavour, from the pinned Rust toolchain)
#
# The Makefile already passes the kernel-model flags we need:
#   -ffreestanding -fno-pic -mcmodel=kernel -mno-red-zone -msoft-float
#   -mno-mmx -mno-sse -mno-sse2 -fno-stack-protector -ffunction-sections
# which match targets/x86_64-unknown-kernel.json (code-model kernel, static
# relocation model, red zone disabled, soft-float, no SSE). The Makefile's
# `ld -r --gc-sections` against exports_link_command.txt drops everything not
# reachable from the rm_* / nv_* entry points the OS layer calls.
#
# Output: $OPENRM_CACHE/build-x86_64/libnvrm.a (gitignored cache, outside the
# repo by default so worktrees share one copy).
#
# Usage: ports/openrm/build.sh            (runs fetch.sh first if needed)
#        JOBS=8 ports/openrm/build.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=ports/openrm/version.sh
source "$HERE/version.sh"

ARCH="${ARCH:-x86_64}"
if [[ "$ARCH" != "x86_64" ]]; then
    echo "build.sh: only x86_64 is wired up (aarch64 builds upstream, not a goal yet)" >&2
    exit 1
fi

[[ -d "$OPENRM_SRC/src/nvidia" ]] || "$HERE/fetch.sh"

LLVM_BIN="${LLVM_BIN:-}"
if [[ -z "$LLVM_BIN" ]]; then
    for d in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin /usr/lib/llvm-*/bin /usr/bin; do
        if [[ -x "$d/clang" && -x "$d/llvm-ar" ]]; then LLVM_BIN="$d"; break; fi
    done
fi
[[ -n "$LLVM_BIN" ]] || { echo "build.sh: need clang + llvm-ar (set LLVM_BIN)" >&2; exit 1; }

# rust-lld in GNU mode, from the toolchain rust-toolchain.toml pins.
SYSROOT="$(cd "$HERE/../.." && rustc --print sysroot)"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
LLD="$SYSROOT/lib/rustlib/$HOST/bin/gcc-ld/ld.lld"
[[ -x "$LLD" ]] || LLD="$(command -v ld.lld)"

TOOLS="$OPENRM_CACHE/tools"
mkdir -p "$TOOLS"
cat > "$TOOLS/cc" <<EOF
#!/bin/sh
exec "$LLVM_BIN/clang" --target=x86_64-unknown-linux-gnu "\$@"
EOF
chmod +x "$TOOLS/cc"

OUT="$OPENRM_CACHE/build-$ARCH"
mkdir -p "$OUT"
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 8)}"

echo "=== [openrm] building nv-kernel.o ($OPENRM_TAG, $ARCH, -j$JOBS) ==="
make -C "$OPENRM_SRC/src/nvidia" -j"$JOBS" \
    CC="$TOOLS/cc" LD="$LLD" \
    OBJCOPY="$LLVM_BIN/llvm-objcopy" AR="$LLVM_BIN/llvm-ar" \
    TARGET_ARCH="$ARCH" TARGET_OS=Linux \
    OUTPUTDIR="$OUT" NV_VERBOSE="${NV_VERBOSE:-0}"

# The complete list of symbols the RM needs from the OS layer.
"$LLVM_BIN/llvm-nm" -u "$OUT/nv-kernel.o" | awk '{print $2}' | sort -u > "$OUT/undefined.txt"
"$LLVM_BIN/llvm-nm" --defined-only -g "$OUT/nv-kernel.o" | awk '{print $3}' | sort -u > "$OUT/exported.txt"

# Our C glue (ports/openrm/glue) is compiled with the exact command line the
# Makefile uses for one of RM's own OS-layer files, so flags never drift from
# NVIDIA's. `make -n -W` prints that command without running it.
MAKEVARS=(CC="$TOOLS/cc" LD=true TARGET_ARCH="$ARCH" TARGET_OS=Linux OUTPUTDIR="$OUT" NV_VERBOSE=1)
REF_SRC=arch/nvalloc/unix/src/osapi.c
REF_CMD="$(make -s -n -C "$OPENRM_SRC/src/nvidia" "${MAKEVARS[@]}" -W "$REF_SRC" "$OUT/osapi.o" \
    | grep -m1 -- " -c $REF_SRC ")"
[[ -n "$REF_CMD" ]] || { echo "build.sh: could not recover RM compile flags" >&2; exit 1; }
GLUE_CMD="${REF_CMD%% -c $REF_SRC *} -Wno-incompatible-pointer-types-discards-qualifiers -I $HERE/glue -c $HERE/glue/lnv_glue.c -o $OUT/lnv_glue.o"
echo "=== [openrm] building lnv_glue.o ==="
( cd "$OPENRM_SRC/src/nvidia" && eval "$GLUE_CMD" )

# Relocatable objects in an archive: rustc's `static=` link kind wants a .a.
rm -f "$OUT/libnvrm.a"
"$LLVM_BIN/llvm-ar" rcs "$OUT/libnvrm.a" "$OUT/nv-kernel.o" "$OUT/lnv_glue.o"
"$LLVM_BIN/llvm-nm" --defined-only -g "$OUT/lnv_glue.o" | awk '{print $3}' | sort -u > "$OUT/glue-defined.txt"
# What is left for the Rust shim: RM's needs minus what the glue provides.
comm -23 "$OUT/undefined.txt" "$OUT/glue-defined.txt" > "$OUT/rust-shim.txt"

echo "=== [openrm] done ==="
ls -l "$OUT/libnvrm.a"
"$LLVM_BIN/llvm-size" "$OUT/nv-kernel.o"
echo "undefined symbols: $(wc -l < "$OUT/undefined.txt") ($OUT/undefined.txt)"
