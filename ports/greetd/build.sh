#!/usr/bin/env bash
# Build the LeandrOS greetd (login manager / greeter IPC daemon) — static musl,
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
#   - greetd depends on pam-leandros, which supplies the libpam application ABI
#     pam-sys binds to. LeandrOS has no PAM stack.
#
# pam-sys's build script unconditionally emits `-lpam -lpam_misc`. The real
# symbols come from the pam-leandros rlib, so the two names are satisfied with
# empty archives in $WORK/linkstub — the linker finds them and pulls nothing.
#
# pam-leandros is copied OUTSIDE the greetd source tree on purpose: it declares
# its own [workspace], and a second workspace root inside greetd's workspace
# directory is a hard cargo error. From $SRC/greetd/, "../../pam-leandros"
# resolves to $WORK/pam-leandros.
#
# Toolchain: cargo +nightly + rust-lld self-contained musl CRT on macOS, exactly
# as ports/busd/build.sh does. `-C relocation-model=static` is MANDATORY (musl
# x86_64 otherwise defaults to static-PIE / ET_DYN at vaddr 0, which the
# LeandrOS loader maps onto the null page). Produces ET_EXEC, statically linked,
# no PT_INTERP, first PT_LOAD @ 0x200000.
#
# Output binaries are staged to:
#   ~/code/leandros-artifacts/greetd-lane/<arch>/greetd
# which is where scripts/mkfs-f2fs-populated.py picks them up for /bin/greetd.
set -euo pipefail

PIN=d6733e983ff7821c3044007d5555345c7553188f   # 0.10.3-22-gd6733e9
REPO=https://github.com/kennylevinsen/greetd.git

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$HERE/.work"
SHIP="$HOME/code/leandros-artifacts/greetd-lane"
SRC="$WORK/greetd-$PIN"

mkdir -p "$WORK"
if [ ! -d "$SRC" ]; then
  git clone "$REPO" "$SRC"
  git -C "$SRC" checkout --detach "$PIN"
  git -C "$SRC" apply "$HERE/patches/0001-leandros.patch"
  git -C "$SRC" apply "$HERE/patches/0002-fakegreet-leandros.patch"
  git -C "$SRC" apply "$HERE/patches/0003-socket-dir-leandros.patch"
fi

# The PAM provider is ours and lives in-repo; refresh it every run so an edit
# there does not need the source tree blown away.
rm -rf "$WORK/pam-leandros"
cp -R "$HERE/pam-leandros" "$WORK/pam-leandros"

mkdir -p "$WORK/linkstub"
printf '!<arch>\n' > "$WORK/linkstub/libpam.a"
printf '!<arch>\n' > "$WORK/linkstub/libpam_misc.a"

mkdir -p "$SRC/.cargo"
cat > "$SRC/.cargo/config.toml" <<'CFG'
[target.aarch64-unknown-linux-musl]
linker = "rust-lld"
[target.x86_64-unknown-linux-musl]
linker = "rust-lld"
CFG

for arch in aarch64 x86_64; do
  target="${arch}-unknown-linux-musl"
  (
    cd "$SRC"
    RUSTFLAGS="-C relocation-model=static -L native=$WORK/linkstub" \
      cargo +nightly build --release -p greetd -p fakegreet --target "$target"
  )
  mkdir -p "$SHIP/$arch"
  install -m 0755 "$SRC/target/$target/release/greetd" "$SHIP/$arch/greetd"
  install -m 0755 "$SRC/target/$target/release/fakegreet" "$SHIP/$arch/fakegreet"
  echo "staged $arch: $SHIP/$arch/greetd + fakegreet"
done

echo "done. Rebuild the f2fs images (scripts/mkfs-f2fs-populated.py) to pick up greetd."
