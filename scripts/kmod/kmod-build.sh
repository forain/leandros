#!/usr/bin/env bash
# kmod-build.sh <arch>: build, link, check and stage the loadable modules.
#   modules = target/kconfig-<arch>/kmod-list (CONFIG_X=m, dash names) + $KMOD_EXTRA (space separated)
#   KMOD_EXTRA=all: every loadable module crate that applies to <arch> (the test fixtures kmbad_* excluded); sdhci
#     builds with KMOD_BOARD (default rpi5 in this mode). Every module is attempted; the failures are listed at the end.
#   KMOD_STRICT=1: an empty module list is an error (default: a quiet no-op, build-all calls this unconditionally).
# Exit status: 1 when any requested module failed to build, link or validate.
#   autoload: $KMOD_AUTOLOAD (space separated names) is appended to lib/modules/<id>/modules.load
# Needs target/final-<arch>/{kernel,kernel-direct} (the build-id directories, one per variant).
# Output: target/kmod/<arch>/{initrd,rootfs}/lib/modules/<build-id>/{<name>.kmod,modules.alias}
# A no-op (before any cargo work) when there is nothing to build.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
arch="$1"
HERE="$ROOT/scripts/kmod"
OUT="${KMOD_OUT:-$ROOT/target/kmod/$arch}"   # KMOD_OUT: stage elsewhere (the kmodbuild regress check)

names=""
if [[ -s "$ROOT/target/kconfig-$arch/kmod-list" ]]; then
    names="$(tr '\n' ' ' < "$ROOT/target/kconfig-$arch/kmod-list")"
fi
extra="${KMOD_EXTRA:-}"
if [[ " $extra " == *" all "* ]]; then
    extra="$(printf '%s\n' $extra | { grep -vx all || true; } | tr '\n' ' ')"
    # Per-arch exclusions: crates whose driver source only exists on the other architecture. Keep this short.
    case "$arch" in
        x86_64) skip=" v3d " ;;   # Pi 5 V3D (driver module gated on aarch64)
        *)      skip=" " ;;
    esac
    for d in "$ROOT"/modules/*/Cargo.toml; do
        m="$(basename "$(dirname "$d")")"; m="${m//-/_}"
        case "$m" in kmbad_*) continue;; esac
        [[ "$skip" == *" $m "* ]] && continue
        extra="$extra $m"
    done
    : "${KMOD_BOARD:=rpi5}"
fi
names="$(printf '%s\n' $names $extra | sed '/^$/d; s/-/_/g' | sort -u | tr '\n' ' ')"
if [[ -z "${names// /}" ]]; then
    [[ "${KMOD_STRICT:-}" == 1 ]] && { echo "kmod-build: no modules requested for $arch (kmod-list empty, KMOD_EXTRA unset)" >&2; exit 1; }
    exit 0
fi

cd "$ROOT"
python3 "$HERE/kmodpost.py" lint

find_tool() { # find_tool <name>: PATH, versioned, homebrew
    local t="$1" c
    for c in "$t" "$t-20" "$t-19" "$t-18" "$t-17" "/opt/homebrew/opt/llvm/bin/$t" "/usr/local/opt/llvm/bin/$t"; do
        if command -v "$c" >/dev/null 2>&1; then command -v "$c"; return 0; fi
    done
    echo "kmod-build: $t not found (install llvm)" >&2; return 1
}
LLD="$(find "$(rustc --print sysroot)" -name rust-lld | head -n 1)"
NM="$(find_tool llvm-nm)"; OBJDUMP="$(find_tool llvm-objdump)"
SPEC="$ROOT/targets/$arch-unknown-kernel.json"
variants=(kernel kernel-direct)
mkdir -p "$OUT/initrd/lib/modules" "$OUT/rootfs/lib/modules"

bid_of() { cat "$OUT/ksyms-$1.ld.buildid"; }
for v in "${variants[@]}"; do
    python3 "$HERE/ksyms.py" "$ROOT/target/final-$arch/$v" "$OUT/ksyms-$v.ld" >/dev/null
    mkdir -p "$OUT/initrd/lib/modules/$(bid_of $v)" "$OUT/rootfs/lib/modules/$(bid_of $v)"
    : > "$OUT/alias-$v"
done

# Host-side check with the loader's own validator.
HOST="$(rustc -vV | sed -n 's/^host: //p')"
RUSTFLAGS= cargo build -q -p kmod-elf --example kmodcheck --target "$HOST" --target-dir "$ROOT/target/kmod-host" --release
CHECK="$ROOT/target/kmod-host/$HOST/release/examples/kmodcheck"

build_one() { # build_one <name>: 0 = staged; any failure returns 1 after saying why on stderr
    local name="$1" dash base size feat lib v elf id d
    dash="${name//_/-}"
    [[ -f "$ROOT/modules/$dash/Cargo.toml" ]] || { echo "kmod-build: no module crate modules/$dash" >&2; return 1; }
    read -r base size < <(python3 "$HERE/kmodpost.py" slot "$arch" "$name") || { echo "kmod-build: $name has no slot (modules/SLOTS)" >&2; return 1; }
    echo "kmod-build: $name ($arch) slot $base size $size"
    # Board-specific modules (sdhci): the crate features mirror the kernel build's board.
    feat=""
    if [[ "$dash" == "sdhci" ]]; then
        [[ -n "${KMOD_BOARD:-}" ]] || { echo "kmod-build: sdhci needs KMOD_BOARD=rpi5 or raspi4b (the driver has no board-less build)" >&2; return 1; }
        feat="--features $KMOD_BOARD"
    fi
    if ! RUSTFLAGS= cargo build -q -p "kmod-$dash" $feat --target "$SPEC" --target-dir "$ROOT/target/kmod-build" --release \
        -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec >"$OUT/cargo-$name.log" 2>&1; then
        grep -E '^error' -A3 "$OUT/cargo-$name.log" | head -n 30 >&2; echo "kmod-build: cargo build of kmod-$dash failed (log: $OUT/cargo-$name.log)" >&2; return 1
    fi
    lib="$ROOT/target/kmod-build/$arch-unknown-kernel/release/libkmod_${name}.a"
    [[ -f "$lib" ]] || { echo "kmod-build: $lib missing" >&2; return 1; }
    python3 "$HERE/kmodpost.py" lib "$arch" "kmod-$dash" "$SPEC" "$lib" "$NM" || return 1
    for v in "${variants[@]}"; do
        elf="$OUT/$v-$name.elf"
        "$LLD" -flavor gnu -static -nostdlib --gc-sections -z max-page-size=4096 -z norelro --build-id=none \
            --no-undefined -T "$HERE/kmod.ld" --defsym=SLOT_BASE="$base" --defsym=SLOT_SIZE="$size" \
            "$OUT/ksyms-$v.ld" -u kmod_init -e kmod_init "$lib" -o "$elf" 2>"$OUT/ld-$name.log" \
            || { grep -E 'undefined symbol' "$OUT/ld-$name.log" | sort -u | head -n 10 >&2; echo "kmod-build: link of $name ($v) failed" >&2; return 1; }
        id="$(bid_of $v)"
        d="$OUT/initrd/lib/modules/$id"
        python3 "$HERE/kmodpost.py" elf "$arch" "$elf" "$d/$name.kmod" "$OUT/ksyms-$v.ld.buildid" "$OBJDUMP" \
            --alias "$OUT/alias-$v" || return 1
        "$CHECK" "$d/$name.kmod" "$arch" "$id" || return 1
        cp "$d/$name.kmod" "$OUT/rootfs/lib/modules/$id/$name.kmod" || return 1
    done
}

failed=""
for name in $names; do
    build_one "$name" || failed="$failed $name"
done

for v in "${variants[@]}"; do
    id="$(bid_of $v)"
    sort -u "$OUT/alias-$v" > "$OUT/initrd/lib/modules/$id/modules.alias"
    cp "$OUT/initrd/lib/modules/$id/modules.alias" "$OUT/rootfs/lib/modules/$id/modules.alias"
    # modules.load = modules/BOOT (only =m names that were built, in BOOT order) then $KMOD_AUTOLOAD;
    # duplicates dropped WITHOUT sorting (the order is the root-disk probe order).
    load="$OUT/initrd/lib/modules/$id/modules.load"
    : > "$load"
    bootf="${LEANDROS_BOOT_FILE:-$ROOT/modules/BOOT}"
    if [[ -f "$bootf" ]]; then
        for m in $(sed 's/#.*//' "$bootf"); do
            case " $names " in *" ${m//-/_} "*) echo "${m//-/_}" >> "$load";; esac
        done
    fi
    for m in ${KMOD_AUTOLOAD:-}; do echo "$m" >> "$load"; done
    # KMOD_TEST: testblob.sh staged kmodtest.kmod here and listed it; keep it autoloaded (kmodctltest needs it).
    [[ "${KMOD_TEST:-}" == "1" && -f "$OUT/initrd/lib/modules/$id/kmodtest.kmod" ]] && echo kmodtest >> "$load"
    awk '!seen[$0]++' "$load" > "$load.tmp" && mv "$load.tmp" "$load"
    [[ -s "$load" ]] || rm -f "$load"
    rm -f "$OUT/alias-$v"
done
echo "kmod-build: staged $OUT"
if [[ -n "$failed" ]]; then echo "kmod-build: FAILED ($arch):$failed" >&2; exit 1; fi
echo "kmod-build: OK ($arch):" $names
