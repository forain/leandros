#!/usr/bin/env bash
# Stage the iso-codes JSON tables (Debian/freedesktop iso-codes, data only)
# into the image at /usr/share/iso-codes/json.
#
# cosmic-settings' Region & language page builds its locale registry from
# these (locales-rs: iso_639-2, iso_639-3, iso_3166-1). Without them
# page_reload fails with "No iso-codes json data found" before it even asks
# org.freedesktop.locale1, and the page's language and formatting fields stay
# empty. Only the three tables that are read get staged (~2 MB).
#
# Output: ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/share/iso-codes/json/
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail
VERSION=4.18.0
SHA256=066bc4df7bf299561856a07119cde01d563c8c4c39906537a39363bb833d466c
URL="https://deb.debian.org/debian/pool/main/i/iso-codes/iso-codes_${VERSION}.orig.tar.xz"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$HERE/.work"
SHIP="$HOME/code/leandros-artifacts/m5-session-ship"
TABLES=(iso_639-2.json iso_639-3.json iso_3166-1.json)

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

mkdir -p "$WORK"
tarball="$WORK/iso-codes-$VERSION.tar.xz"
if [ ! -f "$tarball" ]; then
  curl -sSfL -o "$tarball.part" "$URL"
  mv "$tarball.part" "$tarball"
fi
if command -v sha256sum >/dev/null; then
  echo "$SHA256  $tarball" | sha256sum -c - >/dev/null
else
  echo "$SHA256  $tarball" | shasum -a 256 -c - >/dev/null
fi
rm -rf "$WORK/iso-codes-$VERSION"
tar -C "$WORK" -xJf "$tarball" $(printf "iso-codes-$VERSION/data/%s " "${TABLES[@]}")
for arch in "${ARCHES[@]}"; do
  dst="$SHIP/$arch/usr/share/iso-codes/json"
  install -d "$dst"
  for t in "${TABLES[@]}"; do
    install -m 0644 "$WORK/iso-codes-$VERSION/data/$t" "$dst/$t"
  done
  echo "staged $arch: usr/share/iso-codes/json (${#TABLES[@]} tables, iso-codes $VERSION)"
done
