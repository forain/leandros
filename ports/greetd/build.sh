#!/usr/bin/env bash
. "$(cd "$(dirname "$0")/../.." && pwd)/scripts/lib/netretry.sh"
# Build the LeandrOS greetd (login manager / greeter IPC daemon) — dynamic musl,
# both arches.
#
# Upstream: https://github.com/kennylevinsen/greetd, GPL-3.0-only, pinned to the
# commit in PIN below. Only the `greetd` binary is built; `agreety` (a tty
# greeter we do not use), `fakegreet` (see README) and `greetd_ipc` (a library
# consumed by both) are left out of the build with -p.
#
# Two LeandrOS deltas, both in patches/0001-leandros.patch (see README for the
# rationale of each):
#   - /proc/self/exe is resolved with std::env::current_exe() before the fork
#     rather than handed to execv, because LeandrOS synthesises that path inside
#     readlink(2) only and execv on it returns ENOENT.
#   - greetd links Alpine's REAL Linux-PAM (libpam.so.0, dynamic musl PIE, see
#     scripts/lib/realpam.sh); /etc/pam.d/greetd + pam_unix do the work. The
#     session-leandros rlib only adds utmpx/cgroup bookkeeping (patch 0005).
#
# session-leandros is copied OUTSIDE the greetd source tree on purpose: it
# declares its own [workspace], and a second workspace root inside greetd's
# workspace directory is a hard cargo error. From $SRC/greetd/,
# "../../session-leandros" resolves to $WORK/session-leandros.
#
# Output binaries are staged to:
#   ~/code/leandros-artifacts/greetd-lane/<arch>/<srchash>/{greetd,fakegreet,greetd.srchash}
# keyed by the sha256 of the session-leandros sources, so lanes building different
# sources never overwrite each other. scripts/mkfs-f2fs-populated.py picks the
# directory matching its own checkout (and runs this script when it is missing).
# <arch>/greetd (+ fakegreet, greetd.srchash) is still refreshed as a
# latest-build copy for older tooling; nothing in the image build trusts it.
# Env: ARCHES="aarch64" limits the build; FORCE=1 rebuilds a cached hash.
set -euo pipefail
# macOS: pick an SDK whose libSystem the linker can read (see darwin-sdk.sh).
source "$(dirname "${BASH_SOURCE[0]}")/../../scripts/darwin-sdk.sh"

PIN=d6733e983ff7821c3044007d5555345c7553188f   # 0.10.3-22-gd6733e9
REPO=https://github.com/kennylevinsen/greetd.git

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$HERE/.work"
SHIP="$HOME/code/leandros-artifacts/greetd-lane"
SRC="$WORK/greetd-$PIN"

mkdir -p "$WORK"
if [ ! -d "$SRC" ]; then
  net_git clone "$REPO" "$SRC"
  git -C "$SRC" checkout --detach "$PIN"
  git -C "$SRC" apply "$HERE/patches/0001-leandros.patch"
  git -C "$SRC" apply "$HERE/patches/0002-fakegreet-leandros.patch"
  git -C "$SRC" apply "$HERE/patches/0003-socket-dir-leandros.patch"
  git -C "$SRC" apply "$HERE/patches/0004-trace-leandros.patch"
  git -C "$SRC" apply "$HERE/patches/0005-session-leandros.patch"
fi

# The PAM provider is ours and lives in-repo; refresh it every run so an edit
# there does not need the source tree blown away.
rm -rf "$WORK/session-leandros"
cp -R "$HERE/session-leandros" "$WORK/session-leandros"

SRCHASH=$(cd "$HERE/session-leandros/src" && LC_ALL=C cat $(LC_ALL=C ls) | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d" " -f1)

for arch in ${ARCHES:-aarch64 x86_64}; do
  KEYED="$SHIP/$arch/$SRCHASH"
  if [ -z "${FORCE:-}" ] && [ -x "$KEYED/greetd" ] && [ -x "$KEYED/fakegreet" ]; then
    echo "cached $arch: $KEYED"
    continue
  fi
  target="${arch}-unknown-linux-musl"
  # Dynamic musl PIE linked against Alpine's REAL libpam.so.0 (linux-pam from
  # ports/alpine/<arch>.lock), loaded at run time next to pam_unix & friends.
  source "$HERE/../../scripts/lib/realpam.sh"
  realpam_stub "$arch" "$WORK/linkstub-$arch"
  DYN_TC="$HERE/../../scripts/toolchain/dyn" sh "$HERE/../../scripts/toolchain/dyn/gen-cargo-config.sh" "$SRC" >/dev/null
  sed -i "s|^rustflags = \[|rustflags = [\n  \"-L\", \"native=$WORK/linkstub-$arch\",|" "$SRC/.cargo/config.toml"
  (
    cd "$SRC"
    cargo +nightly build --release -p greetd -p fakegreet --target "$target"
  )
  mkdir -p "$KEYED"
  install -m 0755 "$SRC/target/$target/release/greetd" "$KEYED/greetd"
  install -m 0755 "$SRC/target/$target/release/fakegreet" "$KEYED/fakegreet"
  echo "$SRCHASH" > "$KEYED/greetd.srchash"
  # Latest-build compatibility copies (not trusted by mkfs-f2fs-populated.py).
  cp -f "$KEYED/greetd" "$KEYED/fakegreet" "$KEYED/greetd.srchash" "$SHIP/$arch/"
  echo "staged $arch: $KEYED"
done

echo "done. Rebuild the f2fs images (scripts/mkfs-f2fs-populated.py) to pick up greetd."
