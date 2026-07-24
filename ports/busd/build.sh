#!/usr/bin/env bash
# Build the LeandrOS busd (D-Bus broker) — static musl, both arches.
#
# busd 0.5.0 (crates.io) with a single one-line LeandrOS patch:
# current-thread-runtime.patch (see that file / README.md for the W1 rationale).
#
# Toolchain: cargo +nightly + rust-lld self-contained musl CRT on macOS.
# The `-C relocation-model=static` rustflag is MANDATORY (musl x86_64 otherwise
# defaults to static-PIE / ET_DYN at vaddr 0, which the LeandrOS loader maps onto
# the null page). Produces ET_EXEC, statically linked, first PT_LOAD @ 0x200000.
#
# Output binaries are staged to:
#   ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/libexec/busd
set -euo pipefail

VERSION=0.5.0
WORK="$(cd "$(dirname "$0")" && pwd)/.work"
SHIP="$HOME/code/leandros-artifacts/m5-session-ship"
SRC="$WORK/busd-$VERSION"

mkdir -p "$WORK"
if [ ! -d "$SRC" ]; then
  curl -sSL -o "$WORK/busd-$VERSION.crate" \
    "https://static.crates.io/crates/busd/busd-$VERSION.crate"
  tar -C "$WORK" -xzf "$WORK/busd-$VERSION.crate"
  # apply the LeandrOS patch
  patch -p1 -d "$SRC" < "$(dirname "$0")/current-thread-runtime.patch"
fi

mkdir -p "$SRC/.cargo"
cat > "$SRC/.cargo/config.toml" <<'CFG'
[target.aarch64-unknown-linux-musl]
linker = "rust-lld"
rustflags = ["-C", "relocation-model=static"]
[target.x86_64-unknown-linux-musl]
linker = "rust-lld"
rustflags = ["-C", "relocation-model=static"]
CFG

for arch in aarch64 x86_64; do
  target="${arch}-unknown-linux-musl"
  ( cd "$SRC" && cargo +nightly build --release --target "$target" )
  install -m 0755 "$SRC/target/$target/release/busd" \
    "$SHIP/$arch/usr/libexec/busd"
  echo "staged $arch: $SHIP/$arch/usr/libexec/busd"
done
echo "done. Rebuild the f2fs images (scripts/mkfs-f2fs-populated.py) to pick up busd."
