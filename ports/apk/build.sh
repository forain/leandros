#!/usr/bin/env bash
# Stage the LeandrOS-owned files of the live-root apk setup into
# ports/apk/out/<arch>/ (a rootfs-shaped tree mkfs overlays). The Alpine
# packages themselves (apk-tools-static, alpine-keys, busybox-static, musl,
# the CA bundle, firefox, git, ...) are NOT handled here: they come from the
# lock (ports/alpine/<arch>.lock, scripts/alpine-lock.py fetch) and are staged
# by scripts/mkfs-f2fs-populated.py. /etc/apk/{world,repositories} are generated
# by mkfs from the lock.
set -eu
WHAT="${1:?usage: $0 <x86_64|aarch64|all>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"

case "$WHAT" in
  all) ARCHS="aarch64 x86_64" ;;
  aarch64|x86_64) ARCHS="$WHAT" ;;
  *) echo "usage: $0 <x86_64|aarch64|all>"; exit 2 ;;
esac

for ARCH in $ARCHS; do
  T="$OUT/$ARCH.new"
  rm -rf "$T"; mkdir -p "$T/usr/sbin" "$T/usr/bin" "$T/etc/apk" "$T/lib/apk/db"
  cp "$HERE/alpine-sysroot.sh" "$T/usr/sbin/alpine-sysroot"
  cp "$HERE/claude-code-install.sh" "$T/usr/sbin/claude-code-install"
  cp "$HERE/apk-wrapper.sh" "$T/usr/sbin/apk"
  cp "$HERE/bwrap.sh" "$T/usr/bin/bwrap"
  for _a in adduser addgroup deluser delgroup; do
    printf '#!/bin/sh\nexec /bin/busybox.static %s "$@"\n' "$_a" > "$T/usr/sbin/$_a"
    chmod 0755 "$T/usr/sbin/$_a"
  done
  chmod 0755 "$T/usr/bin/bwrap" "$T/usr/sbin/apk" "$T/usr/sbin/alpine-sysroot" "$T/usr/sbin/claude-code-install"
  printf '%s\n' "$ARCH" > "$T/etc/apk/arch"
  : > "$T/lib/apk/db/lock"
  chmod 0644 "$T/etc/apk/arch" "$T/lib/apk/db/lock"
  rm -rf "$OUT/$ARCH"; mv "$T" "$OUT/$ARCH"
  touch "$OUT/$ARCH/.stamp"
  echo "✅ $OUT/$ARCH"
done
