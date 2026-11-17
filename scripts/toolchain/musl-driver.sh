#!/usr/bin/env bash
# musl-driver.sh <aarch64|x86_64> <c|c++> args...  -- clang for the static musl world.
# Our flags come first, user args after (they win). Static ET_EXEC, non-PIE codegen,
# no outline atomics, -DNDEBUG at -O2/-O3/-Os/-Oz. cc-rs --target spellings are stripped (one triple).
set -u
arch=$1 lang=$2; shift 2
. "$(dirname "${BASH_SOURCE[0]}")/env.sh" || exit 1
T=$TC_ROOT/$arch
[ -f "$T/stamp" ] || { echo "toolchain: $T missing; run scripts/toolchain/mk-sysroot.sh $arch" >&2; exit 1; }
args=() skip=0 link=1 static=1 opt=0
for a in "$@"; do
    if ((skip)); then skip=0; continue; fi
    case "$a" in
        --target=*) continue ;;
        -target)    skip=1; continue ;;
        -c|-S|-E|-M|-MM|-fsyntax-only|-###|-print-*|--version|-v) ;;
    esac
    case "$a" in
        -c|-S|-E|-M|-MM|-fsyntax-only|-print-*|--version) link=0 ;;
        -shared|-pie|-static-pie|-dynamic|-dynamic-linker|-rdynamic) static=0 ;;
        -O2|-O3|-Os|-Oz) opt=1 ;;
    esac
    args+=("$a")
done
# -Wl,--version-script <p> / -Wl,--dynamic-list <p> -> joined form (clang passes the bare path to lld as an input).
out=() n=${#args[@]} i=0
while ((i < n)); do
    a=${args[i]}
    case "$a" in
        -Wl,--version-script|-Wl,--dynamic-list) out+=("$a=${args[i+1]}"); i=$((i+2)); continue ;;
    esac
    out+=("$a"); i=$((i+1))
done
pre=(--no-default-config -Wno-unused-command-line-argument "--target=$arch-unknown-linux-musl" "--sysroot=$T/sysroot" "-resource-dir=$T/resource"
     "--ld-path=$LLD" --rtlib=compiler-rt -fno-pie)
[ "$arch" = aarch64 ] && pre+=(-mno-outline-atomics)
((opt)) && pre+=(-DNDEBUG)
if [ "$lang" = c++ ]; then
    pre+=(-stdlib=libc++ --unwindlib=libunwind -nostdinc++ -isystem "$T/sysroot/usr/include/c++/v1")
    bin=$LLVM_BIN/clang++
else
    pre+=(--unwindlib=none)
    bin=$CLANG
fi
((link && static)) && pre+=(-static -no-pie)
post=()
[ "$lang" = c++ ] && ((link)) && post=(-lc++abi)   # libc++.a leaves the ABI half to libc++abi.a
exec "$bin" "${pre[@]}" "${out[@]}" ${post[@]+"${post[@]}"}
