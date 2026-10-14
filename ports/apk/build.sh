#!/bin/sh
# build.sh <x86_64|aarch64|all> — stage Alpine's static apk-tools for the image
# into ports/apk/out/<arch>/, which scripts/mkfs-f2fs-populated.py overlays:
#
#   usr/sbin/apk.static              apk-tools-static (statically linked: runs on
#                                    LeandrOS as-is, no libc, no soname fix-up)
#   usr/sbin/alpine-sysroot          the wrapper (alpine-sysroot.sh) that stages
#                                    an Alpine 3.21 root without touching /
#   usr/sbin/claude-code-install     installs the Claude Code CLI (a musl Bun
#   usr/sbin/claude-code-run         binary) into an Alpine root; -run is the
#                                    /usr/bin/claude launcher it installs
#   etc/apk/keys/*.rsa.pub           alpine-keys: the trust root apk verifies
#                                    every APKINDEX and package against
#   etc/apk/repositories             Alpine 3.21 main + community
#   etc/ssl/certs/ca-certificates.crt, etc/ssl/cert.pem
#                                    ca-certificates-bundle, so apk (and any
#                                    OpenSSL/rustls-native client) can verify
#                                    https:// — the image had no CA store
#
# Nothing is compiled and nothing runs in a container: the three .apk files are
# fetched over https from dl-cdn.alpinelinux.org into $LEANDROS_APK_CACHE
# (default ~/.cache/leandros/apk/<arch>/) once, and unpacked with tar (an .apk
# is concatenated gzip'ed tar segments). apk-tools-static and alpine-keys are
# pinned by version and sha256 — the keys are the trust root, so a mismatch is
# fatal; ca-certificates-bundle is re-released whenever Mozilla's store moves,
# so it is taken at whatever version the signed APKINDEX names today.
set -eu
WHAT="${1:?usage: $0 <x86_64|aarch64|all>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"
CACHE="${LEANDROS_APK_CACHE:-$HOME/.cache/leandros/apk}"
MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
REL=v3.21

APK_TOOLS_VER=2.14.6-r3
ALPINE_KEYS_VER=2.5-r0
pin() {  # pin <arch> <pkg-ver> -> expected sha256 (empty: unpinned)
  case "$1/$2" in
    x86_64/apk-tools-static-2.14.6-r3)  echo f0e0d34d6a8f1f9d8704bae6612b4627b96f13cd20db759e9b43085135cd234f ;;
    aarch64/apk-tools-static-2.14.6-r3) echo 910015ebcdb11f92966f5590cf5b538b3e4dddbc1b56bc441fae3a622d05dd0e ;;
    x86_64/alpine-keys-2.5-r0)          echo f68a8cf46058b77d2ebccc33fec2a645d8da9e3791ffc385cf80403ec4810512 ;;
    aarch64/alpine-keys-2.5-r0)         echo a70d3c55ee676d7d670714aa729285d5ab6fcf18146eb03e05319098bcb715c0 ;;
  esac
}
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

case "$WHAT" in
  all) ARCHS="aarch64 x86_64" ;;
  aarch64|x86_64) ARCHS="$WHAT" ;;
  *) echo "usage: $0 <x86_64|aarch64|all>"; exit 2 ;;
esac

fetch() {  # fetch <arch> <file> -> path in the cache
  _d="$CACHE/$1"; mkdir -p "$_d"
  if [ ! -s "$_d/$2" ]; then
    curl -fsSL --retry 3 -o "$_d/$2.part" "$MIRROR/$REL/main/$1/$2" \
      || { rm -f "$_d/$2.part"; echo "❌ cannot fetch $MIRROR/$REL/main/$1/$2" >&2; return 1; }
    mv "$_d/$2.part" "$_d/$2"
  fi
  echo "$_d/$2"
}

for ARCH in $ARCHS; do
  # ca-certificates-bundle's current version, from the APKINDEX (cached for a
  # day so an offline rebuild still works with what was fetched last).
  IDX="$CACHE/$ARCH/APKINDEX.tar.gz"
  mkdir -p "$CACHE/$ARCH"
  if [ -z "$(find "$IDX" -mtime -1 2>/dev/null)" ]; then
    curl -fsSL --retry 3 -o "$IDX.part" "$MIRROR/$REL/main/$ARCH/APKINDEX.tar.gz" \
      && mv "$IDX.part" "$IDX" || { rm -f "$IDX.part"; echo "⚠️  APKINDEX fetch failed, using the cached one"; }
  fi
  [ -s "$IDX" ] || { echo "❌ no APKINDEX for $ARCH"; exit 1; }
  CAVER=$(tar -xzf "$IDX" -O APKINDEX 2>/dev/null \
          | awk -v RS= '/(^|\n)P:ca-certificates-bundle\n/' | sed -n 's/^V://p' | head -1)
  [ -n "$CAVER" ] || { echo "❌ ca-certificates-bundle not in the $ARCH APKINDEX"; exit 1; }

  T="$OUT/$ARCH.new"
  rm -rf "$T"; mkdir -p "$T/x" "$T/usr/sbin" "$T/etc/apk/keys" "$T/etc/ssl/certs"
  for PV in "apk-tools-static-$APK_TOOLS_VER" "alpine-keys-$ALPINE_KEYS_VER" "ca-certificates-bundle-$CAVER"; do
    F=$(fetch "$ARCH" "$PV.apk")
    WANT=$(pin "$ARCH" "$PV")
    GOT=$(sha256 "$F")
    if [ -n "$WANT" ] && [ "$GOT" != "$WANT" ]; then
      echo "❌ sha256 mismatch for $ARCH/$PV.apk (got $GOT, want $WANT)"; rm -f "$F"; exit 1
    fi
    tar -xzf "$F" -C "$T/x" 2>/dev/null || true
  done
  cp "$T/x/sbin/apk.static" "$T/usr/sbin/apk.static"
  chmod 0755 "$T/usr/sbin/apk.static"
  # All Alpine release keys (alpine-keys keeps the current arches' keys at the
  # top level of usr/share/apk/keys); apk picks the one a signature names.
  cp "$T"/x/usr/share/apk/keys/*.rsa.pub "$T/etc/apk/keys/"
  cp "$T/x/etc/ssl/certs/ca-certificates.crt" "$T/etc/ssl/certs/ca-certificates.crt"
  cp "$T/x/etc/ssl/certs/ca-certificates.crt" "$T/etc/ssl/cert.pem"
  cp "$HERE/repositories" "$T/etc/apk/repositories"
  cp "$HERE/alpine-sysroot.sh" "$T/usr/sbin/alpine-sysroot"
  chmod 0755 "$T/usr/sbin/alpine-sysroot"
  cp "$HERE/claude-code-install.sh" "$T/usr/sbin/claude-code-install"
  cp "$HERE/claude-code-run.sh" "$T/usr/sbin/claude-code-run"
  chmod 0755 "$T/usr/sbin/claude-code-install" "$T/usr/sbin/claude-code-run"
  chmod 0644 "$T"/etc/apk/keys/* "$T/etc/apk/repositories" "$T/etc/ssl/cert.pem" "$T/etc/ssl/certs/ca-certificates.crt"
  rm -rf "$T/x"
  printf 'apk-tools-static-%s alpine-keys-%s ca-certificates-bundle-%s\n' \
    "$APK_TOOLS_VER" "$ALPINE_KEYS_VER" "$CAVER" > "$T/APK-VERSION"
  rm -rf "$OUT/$ARCH"; mv "$T" "$OUT/$ARCH"
  touch "$OUT/$ARCH/.stamp"
  echo "✅ $OUT/$ARCH: $(cat "$OUT/$ARCH/APK-VERSION")"
done
