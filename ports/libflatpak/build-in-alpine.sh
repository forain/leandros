#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> -- stage Alpine 3.24's libflatpak.so.0 and
# its DT_NEEDED closure (ostree, json-glib, libxmlb, libcurl, libarchive, gpgme,
# polkit client libs ...) for LeandrOS, plus the -dev files (headers, .pc,
# linkable .so) for the cross sysroot, so cosmic-store's `flatpak` feature
# (the libflatpak crate, pkg-config `flatpak`) links and runs. Same Alpine
# release and method as ports/gstreamer (musl 1.2.5, GLib 2.82): mkfs overlays
# the closure; the image's own copy of a soname wins.
# Output: /out/<arch>.alpine/ (the image tree), /out/<arch>.sysroot/ (-dev).
# Mounts: /src (this dir), /out, /leandros-libc, /imglib/N as in ports/gstreamer.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  [ -f /netretry/netretry.sh ] && { . /netretry/netretry.sh; net_shim; }
  grep -q '^3\.24\.' /etc/alpine-release || { echo "want Alpine 3.24"; exit 3; }
  PKGS="flatpak ostree"
  DEVPKGS="flatpak-dev ostree-dev json-glib-dev libxmlb-dev glib-dev util-linux-dev pcre2-dev"
  if [ "$(uname -m)" = "$ARCH" ]; then
    R=""; APK="apk"
    apk add --no-cache $PKGS $DEVPKGS binutils file patchelf build-base >/dev/null
    CC="cc"
  else
    R=$(mktemp -d /tmp/fp-root.XXXXXX)
    apk add --no-cache binutils file patchelf clang lld alpine-keys >/dev/null
    mkdir -p "$R/etc/apk/keys"; cp /usr/share/apk/keys/$ARCH/* "$R/etc/apk/keys/"; cp /etc/apk/repositories "$R/etc/apk/"
    APK="apk --root $R --arch $ARCH"
    $APK add --initdb --no-scripts --no-cache $PKGS $DEVPKGS >/dev/null
    CC="clang --target=$ARCH-alpine-linux-musl --sysroot=$R -fuse-ld=lld"
  fi
  FPV=$($APK info -e -v flatpak)
  echo "package: $FPV (alpine $(cat /etc/alpine-release))"
  W=$(mktemp -d /tmp/fp-work.XXXXXX); S=$W/stage; mkdir -p "$S/usr/lib"
  LIBFP=$(cd "$R/usr/lib" && ls libflatpak.so.0* | head -1)
  [ -n "$LIBFP" ] || { echo "libflatpak.so.0 not found in the flatpak packages"; exit 6; }
  cp -L "$R/usr/lib/$LIBFP" "$S/usr/lib/libflatpak.so.0"
  # The ostree CLI: the image's apk db is told it provides `ostree` (the closure ships
  # libostree from that package), so `apk add ostree` is a no-op there -- ship the binary
  # too, or the package would be claimed but its command missing.
  mkdir -p "$S/usr/bin"; cp -L "$R/usr/bin/ostree" "$S/usr/bin/ostree"
  # Same for the flatpak CLI (the closure names the flatpak package) and its sandbox
  # helper bwrap: the in-guest flatpaktest runs apps through the real bwrap, network-free.
  for b in flatpak bwrap; do cp -L "$R/usr/bin/$b" "$S/usr/bin/$b"; done
  # -- closure ------------------------------------------------------------------
  is_excluded() {
    case "$1" in
      libc.musl-*|ld-musl-*|libc.so) return 0 ;;
    esac
    [ "$1" = libflatpak.so.0 ] && return 0
    return 1
  }
  find_lib() {
    for d in "$R/usr/lib" "$R/lib"; do
      [ -e "$d/$1" ] && { echo "$d/$1"; return 0; }
    done
    return 1
  }
  needed_of() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'; }
  { needed_of "$S/usr/lib/libflatpak.so.0"; needed_of "$S/usr/bin/ostree"; needed_of "$S/usr/bin/flatpak"; needed_of "$S/usr/bin/bwrap"; echo libflatpak.so.0; } > $W/queue
  : > $W/seen; : > "$S/CLOSURE.txt"; : > $W/missing
  while [ -s $W/queue ]; do
    sort -u $W/queue > $W/q2; : > $W/queue
    for so in $(cat $W/q2); do
      grep -qx "$so" $W/seen && continue
      echo "$so" >> $W/seen
      if is_excluded "$so"; then echo "skip   $so" >> "$S/CLOSURE.txt"; continue; fi
      src=$(find_lib "$so") || { echo "$so" >> $W/missing; echo "MISSING $so" >> "$S/CLOSURE.txt"; continue; }
      cp -L "$src" "$S/usr/lib/$so"
      real=$(cd "$(dirname "$src")" && readlink -f "$src")
      echo "alpine $so  <- ${real#$R} ($($APK info -W "${real#$R}" 2>/dev/null | awk '{print $NF}'))" >> "$S/CLOSURE.txt"
      needed_of "$real" >> $W/queue
    done
  done
  if [ -s $W/missing ]; then echo "unresolved DT_NEEDED:"; cat $W/missing; exit 4; fi
  readelf --dyn-syms -W /lib/ld-musl-$ARCH.so.1 | grep -qw __stack_chk_guard || { echo "libc does not export __stack_chk_guard"; exit 5; }
  find "$S" -type f | while read -r f; do if file -b "$f" | grep -q '^ELF'; then echo "$f"; fi; done > $W/all-elves
  # -- unresolved-symbol audit (same method as ports/gstreamer) ----------------------
  img_lib() {
    grep -qx "$1" /src/image-sonames.txt 2>/dev/null || return 1
    for d in /imglib/*; do
      [ -e "$d/$1" ] && { readlink -f "$d/$1"; return 0; }
    done
    return 1
  }
  if [ -f /leandros-libc/libc.so ]; then
    : > "$S/COLLISIONS.txt"
    echo /leandros-libc/libc.so > $W/deflibs
    for f in $(cat $W/all-elves); do
      b=$(basename "$f")
      if [ "$f" = "$S/usr/lib/$b" ] && [ "$b" != libflatpak.so.0 ] && il=$(img_lib "$b"); then
        echo "$b  image=$il" >> "$S/COLLISIONS.txt"; echo "$il" >> $W/deflibs
      else
        echo "$f" >> $W/deflibs
      fi
    done
    for f in $(cat $W/deflibs); do
      readelf --dyn-syms -W "$f" | awk '$7!="UND" && $8!="" {sub(/@.*/,"",$8); print $8}'
    done | sort -u > $W/defined
    for f in $(cat $W/all-elves); do
      readelf --dyn-syms -W "$f" | awk '$7=="UND" && $5!="WEAK" && $8!="" {sub(/@.*/,"",$8); print $8}' | sort -u \
        | comm -23 - $W/defined | sed "s|^|${f#$S}: |"
    done > "$S/SYMCHECK.txt"
    echo "sonames where the image's copy wins: $(wc -l < "$S/COLLISIONS.txt")"; cat "$S/COLLISIONS.txt"
    echo "unresolved strong symbols: $(wc -l < "$S/SYMCHECK.txt")"; head -60 "$S/SYMCHECK.txt"
  else
    echo "no /leandros-libc/libc.so mounted; symbol audit skipped" > "$S/SYMCHECK.txt"
  fi
  # -- dev tree for the cross sysroot: headers, .pc, linkable libs --------------------
  D=$W/sysroot; mkdir -p "$D/usr/lib/pkgconfig" "$D/usr/include" "$D/usr/share/pkgconfig"
  for d in flatpak ostree-1 json-glib-1.0 libxmlb-2 xmlb gio-unix-2.0 glib-2.0; do [ -d "$R/usr/include/$d" ] && cp -a "$R/usr/include/$d" "$D/usr/include/"; done
  cp -a "$R/usr/lib/glib-2.0" "$D/usr/lib/" 2>/dev/null || true
  cp -L "$R/usr/lib/pkgconfig/"*.pc "$D/usr/lib/pkgconfig/" 2>/dev/null || true
  cp -L "$R/usr/share/pkgconfig/"*.pc "$D/usr/share/pkgconfig/" 2>/dev/null || true
  for so in $(cat $W/seen); do
    s=$(find_lib "$so" 2>/dev/null) || continue
    case "$so" in libc.*) continue ;; esac
    cp -L "$s" "$D/usr/lib/$so"
  done
  for so in $(cd "$R/usr/lib" && ls | grep -E '^lib(flatpak|ostree-1|json-glib-1\.0|xmlb|gio-2\.0|glib-2\.0|gobject-2\.0|gmodule-2\.0)\.so$'); do
    cp -L "$R/usr/lib/$so" "$D/usr/lib/$so"
  done
  [ -e "$D/usr/lib/libintl.so" ] || { [ -e "$D/usr/lib/libintl.so.8" ] && ln -s libintl.so.8 "$D/usr/lib/libintl.so"; } || true
  echo "sysroot dev tree: $(du -sh "$D" | cut -f1)"
  echo "$FPV alpine-$(cat /etc/alpine-release)" > "$S/LIBFLATPAK-VERSION"
  echo "== staged size =="; du -sh "$S"
  rm -rf "/out/$ARCH.alpine" "/out/$ARCH.sysroot"; mkdir -p "/out/$ARCH.alpine" "/out/$ARCH.sysroot"
  cp -a "$S/." "/out/$ARCH.alpine/"; cp -a "$D/." "/out/$ARCH.sysroot/"
)
echo "=== rc=$? arch=$ARCH ==="
