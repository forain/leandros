#!/usr/bin/env bash
# testblob.sh <arch>: build the assembly test module against each kernel variant and stage
# target/kmod/<arch>/initrd/lib/modules/<build-id>/{kmodtest,kmodbad}.kmod for create_initrd.
# Needs target/final-<arch>/{kernel,kernel-direct}. Only used with KMOD_TEST=1.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
arch="$1"
HERE="$ROOT/scripts/kmod"
OUT="$ROOT/target/kmod/$arch"
mkdir -p "$OUT/initrd/lib/modules"
LLD="$(find "$(rustc --print sysroot)" -name rust-lld | head -n 1)"
if [[ "$arch" == "aarch64" ]]; then
    BASE=0xfffffe0000000000; triple=aarch64-unknown-none
else
    BASE=0xffffffff90000000; triple=x86_64-unknown-none
fi
TEST_SLOT=$((BASE + 0x0ff80000))
variants=(kernel kernel-direct)
for v in "${variants[@]}"; do
    python3 "$HERE/ksyms.py" "$ROOT/target/final-$arch/$v" "$OUT/ksyms-$v.ld" >/dev/null
done
build_one() { # variant name idfile slot
    local v="$1" name="$2" idf="$3" slot="$4" o="$OUT/$1-$2"
    clang --target=$triple -c -x assembler-with-cpp -DMODNAME="\"$name\"" -DBUILDID_BIN="\"$idf\"" \
        -DTEST_SLOT=$(printf '0x%x' "$slot") "$HERE/testblob/kmodtest-$arch.S" -o "$o.o"
    "$LLD" -flavor gnu -static -nostdlib -z max-page-size=4096 --no-undefined \
        --defsym=TEST_SLOT=$(printf '0x%x' "$slot") -T "$HERE/testblob/kmodtest.ld" "$OUT/ksyms-$v.ld" \
        -e kmod_init -o "$o.elf" "$o.o"
}
for i in 0 1; do
    v="${variants[$i]}"; w="${variants[$((1-i))]}"
    id="$(cat "$OUT/ksyms-$v.ld.buildid")"; bad="$(cat "$OUT/ksyms-$w.ld.buildid")"
    printf "$(echo "$id" | sed 's/../\\x&/g')" > "$OUT/$v.id"
    printf "$(echo "$bad" | sed 's/../\\x&/g')" > "$OUT/$w-as-bad.id"
    d="$OUT/initrd/lib/modules/$id"; mkdir -p "$d"
    build_one "$v" kmodtest "$OUT/$v.id" "$TEST_SLOT"
    cp "$OUT/$v-kmodtest.elf" "$d/kmodtest.kmod"
    build_one "$v" kmodbad "$OUT/$w-as-bad.id" "$((TEST_SLOT + 0x40000))"
    cp "$OUT/$v-kmodbad.elf" "$d/kmodbad.kmod"
    printf "kmodtest\n" > "$d/modules.load"   # default autoload list (cmdline modules.load= overrides)
done
echo "testblob: staged $OUT/initrd"
