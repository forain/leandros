#!/usr/bin/env bash
# selftest.sh <aarch64|x86_64> [--guest]   -- G1 toolchain self-test (scratch dir only).
#   build + ELF audit of: static C (long double printf, -lunwind, linux/*.h, NDEBUG, no outline atomics),
#   dynamic PIE, -shared with split/joined --version-script and --dynamic-list, static C++ throw/catch,
#   Rust static std + catch_unwind through the cc wrapper.
#   run: --guest = in the LeandrOS guest (driver.py start, ssh, then stop) ; default = natively when the host
#   arch matches, else under qemu-<arch> user mode when installed, else not run (reported as such).
set -uo pipefail
arch=${1:?usage: selftest.sh <aarch64|x86_64> [--guest]}; mode=${2:-}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$HERE/env.sh" || exit 1
"$HERE/check.sh" "$arch" || exit 1
CC=$HERE/$arch-linux-musl-cc CXX=$HERE/$arch-linux-musl-c++
W=$(mktemp -d "${TMPDIR:-/tmp}/tcself.XXXXXX"); trap 'rm -rf "$W"' EXIT
fail=0
ok()  { echo "PASS $*"; }
bad() { echo "FAIL $*"; fail=1; }
t()   { local n=$1; shift; if "$@" >"$W/$n.log" 2>&1; then ok "$n"; else bad "$n ($(tail -n 3 "$W/$n.log" | tr '\n' ' '))"; fi; }
hdr() { "$LLVM_READELF" -h -l -d "$1" 2>/dev/null; }

cat >"$W/a.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <linux/netlink.h>
#include <linux/if_link.h>
#include <linux/seccomp.h>
#include <unwind.h>
static _Unwind_Reason_Code cb(struct _Unwind_Context *c, void *p) { (void)c; ++*(int *)p; return _URC_NO_REASON; }
int main(int argc, char **argv) {
    volatile long double x = 1.5L; x *= (long double)(argc + 2); x += 0.25L;
    int frames = 0; _Unwind_Backtrace(cb, &frames);
    char b[64]; snprintf(b, sizeof b, "%Lf", (long double)x);
#ifdef NDEBUG
    const char *nd = "ndebug";
#else
    const char *nd = "debug";
#endif
    printf("selftest %s ld=%s frames=%d nl=%d %s\n", argv[0] ? "ok" : "", b, frames > 0, NETLINK_ROUTE, nd);
    return strtold("2.5", 0) == 2.5L ? 0 : 3;
}
EOF
t c-static "$CC" -O2 -Wall -Werror "$W/a.c" -lunwind -o "$W/a-static"
h=$(hdr "$W/a-static")
echo "$h" | grep -q "Type:.*EXEC" && ! echo "$h" | grep -q "INTERP\|DYNAMIC" && ok elf-static-exec-no-interp || bad "elf-static-exec-no-interp"
"$LLVM_NM" -u "$W/a-static" | grep -v "^ *w \|_DYNAMIC" | grep . && bad "static-undefined-symbols" || ok static-no-undefined
"$LLVM_NM" "$W/a-static" | grep -q " T _Unwind_Backtrace" && ok libunwind-linked || bad libunwind-linked
if [ "$arch" = aarch64 ]; then
    "$LLVM_NM" "$W/a-static" | grep -q "__addtf3" && ok aarch64-softfp-ld128 || bad aarch64-softfp-ld128
    "$LLVM_NM" "$W/a-static" | grep -q "__aarch64_have_lse_atomics\|__aarch64_cas" && bad "aarch64-outline-atomics-present" || ok aarch64-no-outline-atomics
fi
t c-O0-no-ndebug bash -c "! '$CC' -O0 -dM -E -x c /dev/null | grep -q NDEBUG && '$CC' -O2 -dM -E -x c /dev/null | grep -q NDEBUG"
# dynamic PIE
t c-dynamic-pie "$CC" -O2 -fPIE -pie "$W/a.c" -lunwind -o "$W/a-dyn"
h=$(hdr "$W/a-dyn")
echo "$h" | grep -q "Requesting program interpreter: /lib/ld-musl-$arch.so.1" && ok elf-dyn-interp || bad elf-dyn-interp
# shared libs: version script / dynamic list, joined and split
cat >"$W/l.c" <<'EOF'
int pub_fn(void) { return 1; }
int priv_fn(void) { return 2; }
EOF
printf 'V1 { global: pub_fn; local: *; };\n' >"$W/l.map"
printf '{ pub_fn; };\n' >"$W/l.dyn"
t so-vs-joined "$CC" -O2 -fPIC -shared "$W/l.c" "-Wl,--version-script=$W/l.map" -o "$W/l1.so"
t so-vs-split "$CC" -O2 -fPIC -shared "$W/l.c" -Wl,--version-script "$W/l.map" -o "$W/l2.so"
for s in l1 l2; do
    "$LLVM_NM" -D --defined-only "$W/$s.so" | grep -q pub_fn && ! "$LLVM_NM" -D --defined-only "$W/$s.so" | grep -q priv_fn && ok "so-$s-exports" || bad "so-$s-exports"
done
t so-dynlist-split "$CC" -O2 -fPIC -shared "$W/l.c" -Wl,--dynamic-list "$W/l.dyn" -o "$W/l3.so"
# C++ static, throw/catch, libc++
cat >"$W/b.cc" <<'EOF'
#include <cstdio>
#include <stdexcept>
#include <string>
#include <vector>
int main() {
    std::vector<std::string> v{"a", "b"};
    try { throw std::runtime_error("boom"); } catch (const std::exception &e) { std::printf("cxx %s %zu\n", e.what(), v.size()); return 0; }
    return 1;
}
EOF
t cxx-static "$CXX" -O2 -Wall "$W/b.cc" -o "$W/b-static"
h=$(hdr "$W/b-static"); echo "$h" | grep -q "Type:.*EXEC" && ! echo "$h" | grep -q "INTERP\|DYNAMIC" && ok elf-cxx-static || bad elf-cxx-static
# Rust static std + catch_unwind through the cc wrapper
RC=rustc; command -v rustup >/dev/null 2>&1 && rustup toolchain list 2>/dev/null | grep -q nightly-2026-04-16 && RC="rustc +nightly-2026-04-16"
tgt=$arch-unknown-linux-musl
if $RC --print target-list >/dev/null 2>&1 && [ -d "$($RC --print sysroot)/lib/rustlib/$tgt/lib" ]; then
    cat >"$W/r.rs" <<'EOF'
fn main() {
    let r = std::panic::catch_unwind(|| { if std::env::args().count() > 0 { panic!("p"); } });
    println!("rust caught={}", r.is_err());
}
EOF
    t rust-static $RC --target $tgt -C panic=unwind -C target-feature=+crt-static -C relocation-model=static \
        -C linker="$CC" -C link-self-contained=no -O "$W/r.rs" -o "$W/r-static"
    h=$(hdr "$W/r-static"); echo "$h" | grep -q "Type:.*EXEC" && ! echo "$h" | grep -q "INTERP\|DYNAMIC" && ok elf-rust-static || bad elf-rust-static
else
    echo "SKIP rust-static (no $tgt std for $RC)"
fi
# ---- run ----
runners=()
runlist="a-static b-static r-static"
if [ "$mode" = --guest ]; then
    REPO=$TC_REPO; D=$REPO/.claude/skills/run-leandros/driver.py
    export LEANDROS_RUN_ID=tcself-$arch
    python3 "$D" start "$arch" >"$W/start.log" 2>&1 || { bad "guest-start ($(tail -n 2 "$W/start.log"|tr '\n' ' '))"; exit 1; }
    trap 'python3 "$D" stop >/dev/null 2>&1; rm -rf "$W"' EXIT
    python3 "$D" login root root >/dev/null 2>&1
    read -r H P < <(python3 "$D" ssh-target)
    printf '#!/bin/sh\necho leandro\n' >"$W/ask"; chmod +x "$W/ask"
    export SSH_ASKPASS="$W/ask" SSH_ASKPASS_REQUIRE=force DISPLAY=x
    O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o PreferredAuthentications=password -o LogLevel=ERROR -o ConnectTimeout=30)
    up=""; for i in $(seq 1 30); do ssh "${O[@]}" -p "$P" leandro@"$H" true 2>/dev/null && { up=1; break; }; sleep 5; done
    [ -n "$up" ] || { bad guest-ssh; exit 1; }
    for b in a-static a-dyn b-static r-static; do [ -f "$W/$b" ] && scp -q "${O[@]}" -P "$P" "$W/$b" leandro@"$H":/tmp/tc-$b; done
    scp -q "${O[@]}" -P "$P" "$W/l1.so" leandro@"$H":/tmp/tc-l1.so
    for b in a-static a-dyn b-static r-static; do
        [ -f "$W/$b" ] || continue
        out=$(ssh "${O[@]}" -p "$P" leandro@"$H" "chmod +x /tmp/tc-$b && /tmp/tc-$b; echo rc=\$?" 2>&1 | tail -n 3 | tr '\n' ' ')
        case "$out" in *rc=0*) ok "guest-run-$b: $out";; *) bad "guest-run-$b: $out";; esac
    done
else
    host=$(uname -m); [ "$host" = arm64 ] && host=aarch64
    pre=""
    if [ "$(uname -s)" = Linux ]; then
        if [ "$host" = "$arch" ]; then pre="exec"; elif command -v qemu-$arch >/dev/null 2>&1; then pre="qemu-$arch"; fi
    fi
    if [ -n "$pre" ]; then
        for b in a-static b-static r-static; do
            [ -f "$W/$b" ] || continue
            [ "$pre" = exec ] && out=$("$W/$b" 2>&1; echo rc=$?) || out=$($pre "$W/$b" 2>&1; echo rc=$?)
            case "$out" in *rc=0*) ok "run-$b: $(echo $out)";; *) bad "run-$b: $(echo $out)";; esac
        done
    else
        echo "NOTRUN host cannot execute $arch binaries (use --guest)"
    fi
fi
[ $fail = 0 ] && echo "selftest $arch: ALL PASS" || echo "selftest $arch: FAILED"
exit $fail
