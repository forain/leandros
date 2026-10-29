#!/usr/bin/env bash
# selftest.sh: host-only negative tests for the module build guards (aarch64 target spec).
# Needs target/final-aarch64/kernel (any built kernel, for the kapi symbol list).
# Each fixture must be rejected by its named guard; the positive control (hello) must pass.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"; cd "$ROOT"
HERE="$ROOT/scripts/kmod"; arch=aarch64
SPEC="$ROOT/targets/$arch-unknown-kernel.json"
T="$ROOT/target/kmod-selftest"; rm -rf "$T"; mkdir -p "$T"
find_tool() { for c in "$1" "/opt/homebrew/opt/llvm/bin/$1" "/usr/local/opt/llvm/bin/$1"; do command -v "$c" 2>/dev/null && return 0; done; return 1; }
NM="$(find_tool llvm-nm)"; OBJDUMP="$(find_tool llvm-objdump)"
LLD="$(find "$(rustc --print sysroot)" -name rust-lld | head -n 1)"
python3 "$HERE/ksyms.py" "$ROOT/target/final-$arch/kernel" "$T/ksyms.ld" >/dev/null || exit 1
read -r base size < <(python3 "$HERE/kmodpost.py" slot $arch hello)
fail=0
build() { RUSTFLAGS= cargo build -q -p "kmod-$1" --target "$SPEC" --target-dir "$ROOT/target/kmod-build" --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec >"$T/build-$1.log" 2>&1; }
lib() { echo "$ROOT/target/kmod-build/$arch-unknown-kernel/release/libkmod_$1.a"; }
link() { # link <lib> <ld> <out>
    "$LLD" -flavor gnu -static -nostdlib --gc-sections -z max-page-size=4096 -z norelro --build-id=none \
        --no-undefined -T "$2" --defsym=SLOT_BASE="$base" --defsym=SLOT_SIZE="$size" "$T/ksyms.ld" \
        -u kmod_init -e kmod_init "$1" -o "$3"; }
expect() { # expect <label> <guard> <cmd...>: cmd must fail with "kmodpost: <guard>:"
    local label="$1" guard="$2"; shift 2
    local out; out="$("$@" 2>&1)"; local rc=$?
    if [[ $rc -ne 0 && "$out" == *"kmodpost: $guard:"* ]]; then echo "PASS $label rejected by $guard: ${out##*kmodpost: }"
    else echo "FAIL $label expected '$guard' rejection, rc=$rc: $out"; fail=1; fi; }
python3 -c "print('00'*20)" > "$T/bid"

# positive control
build hello || { echo "FAIL hello build"; cat "$T/build-hello.log" | tail -n 5; exit 1; }
python3 "$HERE/kmodpost.py" lib $arch kmod-hello "$SPEC" "$(lib hello)" "$NM" || fail=1
link "$(lib hello)" "$HERE/kmod.ld" "$T/hello.elf" || fail=1
python3 "$HERE/kmodpost.py" elf $arch "$T/hello.elf" "$T/hello.kmod" "$T/bid" "$OBJDUMP" || fail=1
[[ $fail -eq 0 ]] && echo "PASS positive control (hello)"

# badsym: undefined non-kapi symbol
build badsym; expect badsym undefined python3 "$HERE/kmodpost.py" lib $arch kmod-badsym "$SPEC" "$(lib badsym)" "$NM"
# baddep: depends on mm
build baddep; expect baddep deps python3 "$HERE/kmodpost.py" lib $arch kmod-baddep "$SPEC" "$(lib baddep)" "$NM"
# badsimd: FP instruction in the text
build badsimd || { echo "FAIL badsimd build"; tail -n 5 "$T/build-badsimd.log"; fail=1; }
link "$(lib badsimd)" "$HERE/kmod.ld" "$T/badsimd.elf" && \
    expect badsimd simd python3 "$HERE/kmodpost.py" elf $arch "$T/badsimd.elf" "$T/badsimd.kmod" "$T/bid" "$OBJDUMP"
# badwx: linker script variant with an RWX data segment
sed 's/FLAGS(6)/FLAGS(7)/' "$HERE/kmod.ld" > "$T/wx.ld"
link "$(lib hello)" "$T/wx.ld" "$T/badwx.elf" && \
    expect badwx wx python3 "$HERE/kmodpost.py" elf $arch "$T/badwx.elf" "$T/badwx.kmod" "$T/bid" "$OBJDUMP"
# .init_array must fail the link, not be dropped
cat > "$T/ia.s" <<'S'
.section .init_array,"aw"
.quad 0
S
clang --target=aarch64-unknown-none -c "$T/ia.s" -o "$T/ia.o" && \
    if "$LLD" -flavor gnu -static -nostdlib -T "$HERE/kmod.ld" --defsym=SLOT_BASE="$base" --defsym=SLOT_SIZE="$size" \
        "$T/ksyms.ld" -u kmod_init -e kmod_init "$(lib hello)" "$T/ia.o" -o "$T/ia.elf" 2>"$T/ia.err"; then
        echo "FAIL init_array accepted"; fail=1
    else echo "PASS init_array rejected by the link: $(grep -m1 -o 'init_array[^"]*' "$T/ia.err" | head -c 80)"; fi
exit $fail
