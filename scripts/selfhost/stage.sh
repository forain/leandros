#!/usr/bin/env bash
# scripts/selfhost/stage.sh [aarch64|x86_64] — stage everything an in-guest
# kernel build needs into $LEANDROS_SELFHOST_DIR/<arch>/ (default
# ~/.cache/leandros/selfhost/<arch>), for scripts/selfhost/serve.py to hand to
# the guest over HTTP. See docs/selfhost.md.
#
#   rustc-*.tar.xz cargo-*.tar.xz rust-std-*.tar.xz rust-src-*.tar.xz
#        the pinned nightly (rust-toolchain.toml), downloaded from
#        static.rust-lang.org once into ~/.cache/leandros/rust-nightly/<date>/
#        and checked against the channel manifest's sha256
#   cargo-home.tar.gz   a CARGO_HOME holding only registry/{index,cache}: every
#        crate the workspace and the build-std'd library need, so the guest
#        builds with --offline and nothing changes underneath a comparison
#   leandros-src.tar.gz `git archive HEAD` (committed state only, minus artifacts/)
#   guest-build-kernel.sh  the in-guest driver, run inside an Alpine chroot
#   SHA256SUMS
set -euo pipefail
ARCH="${1:-aarch64}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64]" >&2; exit 2 ;; esac
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${LEANDROS_SELFHOST_DIR:-$HOME/.cache/leandros/selfhost}/$ARCH"
CHANNEL=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml")
DATE=${CHANNEL#nightly-}
[[ "$DATE" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || { echo "unexpected channel '$CHANNEL'" >&2; exit 1; }
DIST="https://static.rust-lang.org/dist/$DATE"
CACHE="$HOME/.cache/leandros/rust-nightly/$DATE"
HOST_TRIPLE="$ARCH-unknown-linux-musl"
mkdir -p "$OUT" "$CACHE"

sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

# ── 1. the toolchain, verified against the channel manifest ─────────────────
[[ -s "$CACHE/channel-rust-nightly.toml" ]] || curl -fsSL -o "$CACHE/channel-rust-nightly.toml" "$DIST/channel-rust-nightly.toml"
COMPONENTS=("rustc:$HOST_TRIPLE" "cargo:$HOST_TRIPLE" "rust-std:$HOST_TRIPLE" "rust-src:*")
for c in "${COMPONENTS[@]}"; do
    pkg=${c%%:*} tgt=${c#*:}
    read -r url hash < <(python3 -I - "$CACHE/channel-rust-nightly.toml" "$pkg" "$tgt" <<'EOF'
import sys, tomllib
m = tomllib.load(open(sys.argv[1], "rb"))
t = m["pkg"][sys.argv[2]]["target"][sys.argv[3]]
assert t.get("available"), f"{sys.argv[2]} not available for {sys.argv[3]}"
print(t["xz_url"], t["xz_hash"])
EOF
)
    f="$CACHE/$(basename "$url")"
    [[ -s "$f" ]] || { curl -fsSL --retry 3 -o "$f.part" "$url"; mv "$f.part" "$f"; }
    [[ "$(sha256 "$f")" == "$hash" ]] || { echo "❌ sha256 mismatch: $f" >&2; rm -f "$f"; exit 1; }
    cp -c "$f" "$OUT/" 2>/dev/null || cp "$f" "$OUT/"
done

# ── 2. the source: committed state only ─────────────────────────────────────
# artifacts/ (test evidence, ~90 MB) and the Limine binary cache are not inputs.
git -C "$ROOT" archive --format=tar.gz --prefix=leandros/ -o "$OUT/leandros-src.tar.gz" HEAD \
    -- . ':!artifacts' ':!.limine-cache'
git -C "$ROOT" rev-parse HEAD > "$OUT/leandros-src.rev"

# ── 3. the crates, fetched with the pinned cargo into a fresh CARGO_HOME ────
CH="$OUT/.cargo-home"
rm -rf "$CH"; mkdir -p "$CH"
SYSROOT=$(cd "$ROOT" && rustc --print sysroot)
( cd "$ROOT"
  # .cargo/config.toml defaults to a JSON target cargo fetch cannot load; the
  # built-in bare-metal triples select the same cfg(target_arch) dependencies.
  CARGO_HOME="$CH" cargo fetch --locked --target aarch64-unknown-none --target x86_64-unknown-none )
( cd "$CH"; export RUSTUP_TOOLCHAIN="$CHANNEL"
  # build-std resolves against library/Cargo.lock; fetch the whole library
  # workspace so core/alloc/compiler_builtins and their deps are all cached.
  CARGO_HOME="$CH" cargo fetch --locked --manifest-path "$SYSROOT/lib/rustlib/src/rust/library/Cargo.toml" )
tar -C "$CH" -czf "$OUT/cargo-home.tar.gz" registry/index registry/cache
rm -rf "$CH"

# ── 4. the guest driver ─────────────────────────────────────────────────────
cp "$ROOT/scripts/selfhost/guest-build-kernel.sh" "$OUT/"
( cd "$OUT"; rm -f SHA256SUMS
  for f in *.tar.xz *.tar.gz guest-build-kernel.sh leandros-src.rev; do echo "$(sha256 "$f")  $f"; done > SHA256SUMS.tmp
  mv SHA256SUMS.tmp SHA256SUMS )
echo "✅ staged $ARCH self-host inputs in $OUT ($(du -sh "$OUT" | cut -f1)), source $(cut -c1-12 "$OUT/leandros-src.rev")"
