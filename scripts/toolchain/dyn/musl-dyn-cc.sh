#!/usr/bin/env bash
# musl-dyn-cc.sh <aarch64|x86_64> <c|c++> args...  -- clang for the DYNAMIC musl world (COSMIC/GL stack).
# Dynamic-musl cc/c++ wrappers installed (as forwarders) into $ART/{m3-gl-stack,m6-session-bins}/toolchain.
# Compile/link against the merged m3 sysroot (libc.so, crt*.o, GL stack); the L0 sysroot only supplies the
# Linux UAPI headers (linux/, asm*/), compiler-rt builtins and libc++ headers, placed AFTER the m3 dirs.
# Default link is clang's dynamic PIE (PT_INTERP /lib/ld-musl-<arch>.so.1); the static world uses scripts/toolchain/<a>-linux-musl-cc.
set -u
arch=$1 lang=$2; shift 2
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh" || exit 1
T=$TC_ROOT/$arch
[ -f "$T/stamp" ] || { echo "toolchain: $T missing; run scripts/toolchain/mk-sysroot.sh $arch" >&2; exit 1; }
if [ -z "${DYN_SYSROOT:-}" ]; then
    DYN_SYSROOT=${LEANDROS_ARTIFACTS:-$(cd "$TC_ROOT/.." && pwd)}/m3-gl-stack/sysroot-$arch
fi
[ -d "$DYN_SYSROOT/usr/lib" ] || { echo "toolchain: dynamic sysroot $DYN_SYSROOT missing (set DYN_SYSROOT)" >&2; exit 1; }
args=() skip=0 opt=0 link=1
for a in "$@"; do
    if ((skip)); then skip=0; continue; fi
    case "$a" in
        --target=*) continue ;;   # cc-rs appends the Rust-style triple; ours is already pinned
        -target)    skip=1; continue ;;
        -c|-S|-E|-M|-MM|-fsyntax-only|-print-*|--version) link=0 ;;
        -O2|-O3|-Os|-Oz) opt=1 ;;
    esac
    args+=("$a")
done
# -Wl,--version-script <p> / -Wl,--dynamic-list <p> -> joined form (clang hands the bare path to lld as an input).
out=() n=${#args[@]} i=0
while ((i < n)); do
    a=${args[i]}
    case "$a" in
        -Wl,--version-script|-Wl,--dynamic-list) out+=("$a=${args[i+1]}"); i=$((i+2)); continue ;;
    esac
    out+=("$a"); i=$((i+1))
done
pre=(--no-default-config -Wno-unused-command-line-argument "--target=$arch-unknown-linux-musl" "--sysroot=$DYN_SYSROOT"
     "-resource-dir=$T/resource" "-idirafter" "$T/sysroot/usr/include")
# link-only flags (a compile-only run would trip meson/cc-rs probes that pass -Werror=unused-command-line-argument)
((link)) && pre+=("--ld-path=$LLD" --rtlib=compiler-rt --unwindlib=none)
[ "$arch" = aarch64 ] && pre+=(-mno-outline-atomics)
# Distro clangs (Arch) default to -fstack-protector-strong; upstream/Homebrew clang does not. The dynamic
# sysroot libc.so exports no __stack_chk_guard (cf. ports/mesa/ssp_guard.c), so aarch64 links of C deps
# (zstd-sys) fail. Pin upstream's default; an explicit -fstack-protector* from the caller still wins.
pre+=(-fno-stack-protector)
((opt)) && pre+=(-DNDEBUG)
bin=$CLANG
if [ "$lang" = c++ ]; then
    pre+=(-stdlib=libc++ -nostdinc++ -isystem "$T/sysroot/usr/include/c++/v1")
    ((link)) && pre+=("-L$T/sysroot/usr/lib")
    bin=$LLVM_BIN/clang++
fi
# trailing -Wno-...: wrapper-only flags a given invocation does not use must not trip -Werror=unused-command-line-argument probes
exec "$bin" "${pre[@]}" "${out[@]}" -Wno-unused-command-line-argument
