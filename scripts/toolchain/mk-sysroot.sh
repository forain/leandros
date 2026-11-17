#!/usr/bin/env bash
# mk-sysroot.sh <aarch64|x86_64>  -- build $TC_ROOT/<arch>/{sysroot,resource} from ports/alpine/toolchain.lock.
set -euo pipefail
arch=${1:?usage: mk-sysroot.sh <aarch64|x86_64>}
. "$(dirname "${BASH_SOURCE[0]}")/env.sh"
T=$TC_ROOT/$arch
[ -x "$CLANG" ] || { echo "toolchain: no clang at $CLANG" >&2; exit 1; }
major=$("$CLANG" --version | sed -n 's/.*version \([0-9]*\)\..*/\1/p;q')
[ "${major:-0}" -ge 21 ] || { echo "toolchain: clang >= 21 required, have $major" >&2; exit 1; }
# image musl must equal the toolchain musl (headers/static libc == runtime libc)
img=$(awk '$1=="pkg"&&$2=="musl"{print $3}' "$TC_REPO/ports/alpine/$arch.lock")
tcm=$(python3 -I "$TC_SRC/toolchain-lock.py" print "$arch" musl)
[ "$img" = "$tcm" ] || { echo "toolchain: toolchain.lock musl $tcm != ports/alpine/$arch.lock musl $img; run toolchain-lock.py refresh" >&2; exit 1; }
# rust-lld fallback when no ld.lld exists on this host
if [ ! -x "$LLD" ] && [ "$LLD" = "$TC_ROOT/bin/ld.lld" ]; then
    rl=""
    for tc in nightly-2026-04-16 stable; do
        s=$(rustc +$tc --print sysroot 2>/dev/null) || continue
        c=$(ls "$s"/lib/rustlib/*/bin/rust-lld 2>/dev/null | head -1) && [ -n "$c" ] && { rl=$c; break; }
    done
    [ -n "$rl" ] || { echo "toolchain: no ld.lld (brew install lld) and no rust-lld in rustup" >&2; exit 1; }
    mkdir -p "$TC_ROOT/bin"
    printf '#!/bin/sh\n# rust-lld fallback (no host ld.lld)\nexec "%s" -flavor gnu "$@"\n' "$rl" > "$TC_ROOT/bin/ld.lld"
    chmod +x "$TC_ROOT/bin/ld.lld"
fi
rc=$("$CLANG" -print-resource-dir)
want="clang=$("$CLANG" --version | head -1)
resource=$rc
lock=$(tc_sha256 "$TC_REPO/ports/alpine/toolchain.lock")
lld=$LLD"
if [ -f "$T/stamp" ] && [ "$(cat "$T/stamp")" = "$want" ]; then echo "toolchain $arch: up to date"; exit 0; fi
tmp=$T.new.$$
rm -rf "$tmp"; mkdir -p "$tmp/x" "$tmp/sysroot" "$tmp/resource"
python3 -I "$TC_SRC/toolchain-lock.py" extract "$arch" "$tmp/x" >/dev/null
for p in musl musl-dev linux-headers llvm-libunwind-static llvm-libunwind-dev libc++ libc++-dev libc++-static; do
    cp -R "$tmp/x/$p/." "$tmp/sysroot/"
done
chmod -R u+w "$tmp"
# absolute symlinks (musl-dev libc.so etc.) -> relative, else lld resolves them on the host
find "$tmp/sysroot" -type l | while read -r l; do
    t=$(readlink "$l")
    case $t in /*)
        rel=$(python3 -c 'import os,sys;print(os.path.relpath(sys.argv[1],os.path.dirname(sys.argv[2])))' "$tmp/sysroot$t" "$l")
        ln -sfn "$rel" "$l" ;;
    esac
done
# libc.so must resolve inside the sysroot (musl-dev ships it as a link to the runtime loader)
[ -e "$tmp/sysroot/usr/lib/libc.so" ] || { echo "toolchain: sysroot libc.so does not resolve" >&2; exit 1; }
# builtins + crt* into the private resource dir (per-target layout clang searches)
b=$(find "$tmp/x/compiler-rt" -name "libclang_rt.builtins-*.a" | head -1)
[ -n "$b" ] || { echo "toolchain: no libclang_rt.builtins in compiler-rt apk" >&2; exit 1; }
d=$tmp/resource/lib/$arch-unknown-linux-musl
mkdir -p "$d"
cp "$b" "$d/libclang_rt.builtins.a"
for c in crtbegin crtend; do
    f=$(find "$tmp/x/compiler-rt" -name "clang_rt.$c-*.o" | head -1); [ -n "$f" ] && cp "$f" "$d/clang_rt.$c.o"
done
ln -s "$rc/include" "$tmp/resource/include"
rm -rf "$tmp/x"
printf '%s\n' "$want" > "$tmp/stamp"
rm -rf "$T"; mv "$tmp" "$T"
echo "toolchain $arch: built $T"
