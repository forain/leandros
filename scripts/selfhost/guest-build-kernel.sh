#!/bin/sh
# guest-build-kernel.sh URL [JOBS] — build the LeandrOS kernel inside LeandrOS.
#
# Runs INSIDE an Alpine chroot in the guest (Alpine's musl, gcc as the linker
# driver for build scripts and proc macros). URL is scripts/selfhost/serve.py on
# the host, serving what scripts/selfhost/stage.sh staged. Results (the kernel
# ELF, the log, timings) go back to the host with HTTP PUT. See docs/selfhost.md.
#
# The cargo invocation is build-all.sh's "standard" (Limine) kernel, verbatim
# apart from --offline, -j and the paths.
set -eu
URL="${1:?usage: guest-build-kernel.sh URL [JOBS]}"
JOBS="${2:-2}"
ARCH=$(uname -m)
W="${SELFHOST_WORK:-/build}"
RUST=/opt/rust
export CARGO_HOME=/root/.cargo
export PATH="$RUST/bin:$PATH"
mkdir -p "$W/dl" "$CARGO_HOME"
LOG="$W/build-$ARCH.log"
: > "$LOG"
say() { echo "[selfhost] $*" | tee -a "$LOG"; }
put() { curl -fsS -T "$1" "$URL/$2" || say "upload of $2 failed"; }
now() { date +%s; }

T0=$(now)
cd "$W/dl"
curl -fsS -o SHA256SUMS "$URL/SHA256SUMS"
while read -r _sum f; do
    if [ -f "$f" ] && echo "$_sum  $f" | sha256sum -c -s; then continue; fi
    say "fetch $f"
    curl -fsS -o "$f" "$URL/$f"
done < SHA256SUMS
sha256sum -c SHA256SUMS | tee -a "$LOG"
T1=$(now)

if [ ! -x "$RUST/bin/cargo" ] || [ "$(cat "$RUST/.stamp" 2>/dev/null)" != "$(grep -E 'rustc-|cargo-|rust-std-|rust-src-' SHA256SUMS | sha256sum)" ]; then
    say "installing toolchain into $RUST"
    rm -rf "$RUST"; mkdir -p "$RUST"
    # Each tarball is <pkg>/<component>/{bin,lib,...}: strip both levels so
    # the components merge in place (no second ~1 GB copy on the guest disk).
    # The installer's own files sit one level up and strip to nothing.
    for t in rustc-*.tar.xz cargo-*.tar.xz rust-std-*.tar.xz rust-src-*.tar.xz; do
        tar -C "$RUST" -xJf "$t" --strip-components=2 --exclude='*/manifest.in'
    done
    grep -E 'rustc-|cargo-|rust-std-|rust-src-' SHA256SUMS | sha256sum > "$RUST/.stamp"
fi
rm -rf "$CARGO_HOME/registry"
tar -C "$CARGO_HOME" -xzf cargo-home.tar.gz
rm -rf "$W/leandros"
tar -C "$W" -xzf leandros-src.tar.gz
T2=$(now)
rustc -Vv | tee -a "$LOG"
cargo -V | tee -a "$LOG"

SRC="$W/leandros"
TD="$W/target"
SPEC="$SRC/targets/$ARCH-unknown-kernel.json"
cd "$SRC"
say "cargo build -j$JOBS (source $(cat "$W/dl/leandros-src.rev" | cut -c1-12))"
set +e
RUSTFLAGS="-C link-arg=-T$SRC/linkers/$ARCH.ld -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro -Z emit-stack-sizes ${SELFHOST_RUSTFLAGS:-}" \
    cargo build --offline -j "$JOBS" -p kernel --target "$SPEC" --target-dir "$TD" --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec >> "$LOG" 2>&1
RC=$?
set -e
T3=$(now)
K="$TD/$ARCH-unknown-kernel/release/kernel"
say "cargo rc=$RC; fetch+verify $((T1 - T0)) s, unpack $((T2 - T1)) s, cargo build $((T3 - T2)) s"
if [ "$RC" = 0 ] && [ -f "$K" ]; then
    say "kernel: $(wc -c < "$K") bytes, sha256 $(sha256sum "$K" | cut -d' ' -f1)"
    put "$K" "kernel-$ARCH-guest"
fi
put "$LOG" "build-$ARCH.log"
echo "SELFHOST-DONE rc=$RC"
exit "$RC"
