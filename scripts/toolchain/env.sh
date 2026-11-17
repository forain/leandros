# Source me. Locates the host LLVM tools and the target sysroot root. Exports:
#   TC_SRC (this dir)  TC_ROOT (sysroot root)  LLVM_BIN  CLANG  LLD  LLVM_AR LLVM_RANLIB LLVM_STRIP LLVM_NM LLVM_READELF
# Mac: Homebrew llvm; ld.lld from Homebrew `lld` if present, else the rust-lld fallback shim
# ($TC_ROOT/bin/ld.lld, written by mk-sysroot.sh from the pinned rustup toolchain). Linux: /usr/bin.
TC_SRC=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
TC_REPO=$(cd "$TC_SRC/../.." && pwd)
if [ -z "${LEANDROS_TOOLCHAIN:-}" ]; then
    _art=${LEANDROS_ARTIFACTS:-}
    [ -n "$_art" ] || for _d in "$TC_REPO/../leandros-artifacts" "$HOME/code/leandros-artifacts"; do
        [ -d "$_d" ] && { _art=$(cd "$_d" && pwd); break; }
    done
    [ -n "$_art" ] || { echo "toolchain: set LEANDROS_TOOLCHAIN (no leandros-artifacts dir found)" >&2; return 1 2>/dev/null || exit 1; }
    LEANDROS_TOOLCHAIN=$_art/llvm-musl
fi
TC_ROOT=$LEANDROS_TOOLCHAIN
if [ "$(uname -s)" = Darwin ]; then
    LLVM_BIN=${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}
    LLD=${LLD:-}
    if [ -z "$LLD" ]; then
        for _l in /opt/homebrew/opt/lld/bin/ld.lld /opt/homebrew/bin/ld.lld "$TC_ROOT/bin/ld.lld"; do
            [ -x "$_l" ] && { LLD=$_l; break; }
        done
        [ -n "$LLD" ] || LLD=$TC_ROOT/bin/ld.lld   # written by mk-sysroot.sh
    fi
else
    LLVM_BIN=${LLVM_BIN:-/usr/bin}
    LLD=${LLD:-$(command -v ld.lld || echo /usr/bin/ld.lld)}
fi
CLANG=$LLVM_BIN/clang
LLVM_AR=$LLVM_BIN/llvm-ar
LLVM_RANLIB=$LLVM_BIN/llvm-ranlib
LLVM_STRIP=$LLVM_BIN/llvm-strip
LLVM_NM=$LLVM_BIN/llvm-nm
LLVM_READELF=$LLVM_BIN/llvm-readelf
export TC_SRC TC_ROOT LLVM_BIN CLANG LLD LLVM_AR LLVM_RANLIB LLVM_STRIP LLVM_NM LLVM_READELF
tc_sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -c1-64; else shasum -a 256 "$1" | cut -c1-64; fi; }
