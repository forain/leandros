#!/usr/bin/env bash
# Build and stage the LeandrOS D-Bus session package — static musl, both arches.
#
# busd 0.5.0 (crates.io) with every *.patch in this directory applied in name
# order. Each patch carries its own rationale in its header; see README.md.
#
# Toolchain: cargo +nightly + rust-lld self-contained musl CRT on macOS.
# The `-C relocation-model=static` rustflag is MANDATORY (musl x86_64 otherwise
# defaults to static-PIE / ET_DYN at vaddr 0, which the LeandrOS loader maps onto
# the null page). Produces ET_EXEC, statically linked, first PT_LOAD @ 0x200000.
#
# ── This script owns the whole staged D-Bus payload, on purpose ───────────────
#
# `scripts/mkfs-f2fs-populated.py` builds the image by walking
# ~/code/leandros-artifacts/m5-session-ship/<arch>/ and packing whatever it
# finds. That tree is hand-synced and gitignored, so anything staged into it by
# hand drifts away from the repo silently — and it drifted in BOTH directions at
# once:
#
#   * `busd` and `session.conf` were staged before commit 84ec91a (D-Bus
#     activation) and never restaged, so the activation code was committed,
#     host-tested, and absent from every image. A boot test answering "does the
#     desktop come up" reported green while testing none of it.
#   * `dbus-run-session` drifted the OTHER way: the tree held the working
#     launcher and `ports/dbus/session-pkg/` held the superseded one that reads
#     `$!` synchronously after `&` (empty under brush -> "busd exited before
#     signaling readiness" -> exit 1, no session at all). Treating the repo as
#     the source of truth without checking would have shipped a desktop that
#     does not start.
#
# So all three files are now written here, every run, from tracked sources: two
# copied out of ports/dbus/session-pkg/ and one built. The staged tree is a
# build output for this subtree, not a place anyone edits. `scripts/build-all.sh`
# calls this before it makes an image, and mkfs re-verifies the two copied files
# byte-for-byte and refuses to build a stale image (see `_verify_dbus_staging`).
#
# Output:
#   ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/libexec/busd
#   ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/bin/dbus-run-session
#   ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/share/dbus-1/session.conf
#   ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/share/dbus-1/services/*.service
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail

VERSION=0.5.0
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$HERE/.work"
PKG="$HERE/../dbus/session-pkg"
SHIP="$HOME/code/leandros-artifacts/m5-session-ship"
SRC="$WORK/busd-$VERSION"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

mkdir -p "$WORK"
if [ ! -f "$WORK/busd-$VERSION.crate" ]; then
  curl -sSL -o "$WORK/busd-$VERSION.crate" \
    "https://static.crates.io/crates/busd/busd-$VERSION.crate"
fi

# Re-extract and re-patch EVERY run. The previous version of this script only
# did this when $SRC was absent, which is precisely how the activation patch
# came to exist in the repo and not in the build: adding a new *.patch to this
# directory left the already-extracted tree untouched, the next run rebuilt the
# old sources, and the staged binary was byte-identical to the pre-patch one.
# Re-extraction costs a couple of seconds against a cached .crate; a silently
# unapplied patch cost a whole session.
rm -rf "$SRC"
tar -C "$WORK" -xzf "$WORK/busd-$VERSION.crate"
# Applied in name order, and the order is load-bearing:
# start-service-activation.patch extends the `send_msg` that
# service-unknown-reply.patch introduces, so "st" must sort after "se". It does,
# but a future patch name that lands between them would break silently — hence
# the explicit echo of what was applied.
for p in "$HERE"/*.patch; do
  echo "applying $(basename "$p")"
  patch -p1 -d "$SRC" < "$p"
done
# ports/ lives inside the repo, and the repo root is a cargo workspace that
# does not list this tree as a member. Cargo refuses to build a package that
# "believes it's in a workspace when it's not" rather than ignoring the outer
# manifest, so the extracted crate is declared its own workspace root. (This
# is invisible on a host where the crate is unpacked outside the repo, which
# is why it only appeared when the build first ran on the Linux box.)
printf '\n[workspace]\n' >> "$SRC/Cargo.toml"

mkdir -p "$SRC/.cargo"
cat > "$SRC/.cargo/config.toml" <<'CFG'
[target.aarch64-unknown-linux-musl]
linker = "rust-lld"
rustflags = ["-C", "relocation-model=static"]
[target.x86_64-unknown-linux-musl]
linker = "rust-lld"
rustflags = ["-C", "relocation-model=static"]
CFG

for arch in "${ARCHES[@]}"; do
  target="${arch}-unknown-linux-musl"
  ( cd "$SRC" && cargo +nightly build --release --target "$target" )
  install -d "$SHIP/$arch/usr/libexec" "$SHIP/$arch/usr/bin" \
             "$SHIP/$arch/usr/share/dbus-1" "$SHIP/$arch/usr/share/dbus-1/services"
  install -m 0755 "$SRC/target/$target/release/busd" \
    "$SHIP/$arch/usr/libexec/busd"
  # Arch-independent, but staged per-arch because mkfs walks one arch root.
  install -m 0755 "$PKG/dbus-run-session" "$SHIP/$arch/usr/bin/dbus-run-session"
  install -m 0644 "$PKG/session.conf"     "$SHIP/$arch/usr/share/dbus-1/session.conf"
  # The servicedir session.conf points at. Staged rather than left empty
  # because busd scans it once at startup and never again: a .service file has
  # to be in the image before the session begins or it does not exist at all.
  rm -f "$SHIP/$arch/usr/share/dbus-1/services"/*.service
  install -m 0644 "$PKG/services"/*.service \
    "$SHIP/$arch/usr/share/dbus-1/services/"
  echo "staged $arch: busd + dbus-run-session + session.conf + $(ls -1 "$PKG/services" | wc -l) .service"
done
echo "done. Rebuild the f2fs images (scripts/mkfs-f2fs-populated.py) to pick up busd."
