#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> — stage Alpine 3.21's prebuilt
# xdg-desktop-portal (the org.freedesktop.portal.Desktop frontend) and
# xdg-permission-store for LeandrOS, plus a GLib SDK the host-side Rust build
# of xdg-desktop-portal-cosmic links against. Nothing but two tiny stubs is
# compiled here.
#
# Runs INSIDE an Alpine 3.21 container of the TARGET architecture (same release
# as ports/firefox and the GPU Mesa stack, so musl 1.2.5 and the GLib ABI match
# what the image already runs). Driven by ports/portal/build.sh, which mounts:
#   /src           ports/portal (read-only snapshot)
#   /out           ports/portal/out; this writes /out/<arch>/ and /out/sdk-<arch>/
#   /leandros-libc the guest's musl libc.so (optional) for the symbol audit
#   /imglib/N      the image's own /usr/lib sources (optional), same as firefox
#
# Output:
#   /out/<arch>/   rootfs-shaped tree overlaid onto the image by
#                  scripts/mkfs-f2fs-populated.py (same rules as ports/firefox):
#     usr/libexec/xdg-desktop-portal, usr/libexec/xdg-permission-store
#     usr/lib/<soname>   the DT_NEEDED closure (GLib, json-glib, pcre2, ...)
#     usr/lib/libpipewire-0.3.so.0   the inert PipeWire stub, a SUPERSET of
#                  the one cosmic-settings-daemon was linked against
#                  (pw-stub-symbols.txt): every function returns 0/NULL, so
#                  pw_main_loop_new fails and nothing is dereferenced. The
#                  frontend only touches PipeWire for the Camera portal.
#     CLOSURE.txt, SYMCHECK.txt, PORTAL-VERSION   provenance / audit
#   /out/sdk-<arch>/  headers + the same patched .so files, for the host build
#                  (glib-sys/gobject-sys/gio-sys via system-deps overrides)
#
# NOT shipped: xdg-document-portal. It is a FUSE filesystem (/dev/fuse does not
# exist on LeandrOS) and is only needed to export files INTO sandboxed apps;
# for host apps the FileChooser portal returns plain file:// URIs.
#
# ELF fix-ups exactly as ports/firefox: DT_NEEDED libc.musl-<arch>.so.1 ->
# libc.so, and libleandros_ssp.so.1 added where __stack_chk_guard is imported.
# Emits '=== rc=N arch=A ===' as the LAST line.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  [ "$(uname -m)" = "$ARCH" ] || { echo "container is $(uname -m), wanted $ARCH"; exit 3; }
  grep -q '^3\.21\.' /etc/alpine-release || { echo "want Alpine 3.21, got $(cat /etc/alpine-release)"; exit 3; }
  apk add --no-cache xdg-desktop-portal glib-dev binutils file patchelf build-base
  XDPVER=$(apk info -e -v xdg-desktop-portal)
  echo "package: $XDPVER $(apk info -e -v glib) (alpine $(cat /etc/alpine-release))"

  S=/tmp/portal-stage-$ARCH
  K=/tmp/portal-sdk-$ARCH
  rm -rf "$S" "$K"
  mkdir -p "$S/usr/lib" "$S/usr/libexec" "$K/usr/lib" "$K/usr/include"

  cp -L /usr/libexec/xdg-desktop-portal /usr/libexec/xdg-permission-store "$S/usr/libexec/"

  # -- PipeWire stub (superset) -----------------------------------------------
  {
    echo '/* Inert libpipewire-0.3 for LeandrOS (ports/portal). Every function'
    echo '   returns 0/NULL: constructors fail, nothing is ever dereferenced. */'
    echo 'typedef long v;'
    grep -v '^#' /src/pw-stub-symbols.txt | awk 'NF{printf "v %s(void){return 0;}\n", $1}'
  } > /tmp/pwstub.c
  cc -shared -fPIC -O2 -nostdlib -fno-stack-protector -Wl,-soname,libpipewire-0.3.so.0 \
    -o "$S/usr/lib/libpipewire-0.3.so.0" /tmp/pwstub.c
  echo "pipewire stub: $(grep -c '^v ' /tmp/pwstub.c) functions"

  # -- closure ------------------------------------------------------------------
  is_excluded() {
    case "$1" in
      libc.musl-*|ld-musl-*|libc.so) return 0 ;;
      libpipewire-0.3.so.0) return 0 ;;   # the stub above
    esac
    return 1
  }
  find_lib() {
    for d in /usr/lib /lib; do
      [ -e "$d/$1" ] && { echo "$d/$1"; return 0; }
    done
    return 1
  }
  needed_of() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'; }
  # Roots: the two services, plus GIO's own dlopen-free closure and the
  # libraries the host Rust build links (glib/gobject/gio/gmodule).
  { needed_of "$S/usr/libexec/xdg-desktop-portal"; needed_of "$S/usr/libexec/xdg-permission-store"
    echo libglib-2.0.so.0; echo libgobject-2.0.so.0; echo libgio-2.0.so.0; echo libgmodule-2.0.so.0
  } > /tmp/queue
  : > /tmp/seen; : > "$S/CLOSURE.txt"; : > /tmp/missing
  while [ -s /tmp/queue ]; do
    sort -u /tmp/queue > /tmp/q2; : > /tmp/queue
    for so in $(cat /tmp/q2); do
      grep -qx "$so" /tmp/seen && continue
      echo "$so" >> /tmp/seen
      if is_excluded "$so"; then echo "image  $so" >> "$S/CLOSURE.txt"; continue; fi
      src=$(find_lib "$so") || { echo "$so" >> /tmp/missing; echo "MISSING $so" >> "$S/CLOSURE.txt"; continue; }
      cp -L "$src" "$S/usr/lib/$so"
      echo "alpine $so  <- $(readlink -f "$src") ($(apk info -W "$(readlink -f "$src")" 2>/dev/null | awk '{print $NF}'))" >> "$S/CLOSURE.txt"
      needed_of "$(readlink -f "$src")" >> /tmp/queue
    done
  done
  if [ -s /tmp/missing ]; then echo "unresolved DT_NEEDED:"; cat /tmp/missing; exit 4; fi

  # -- data ---------------------------------------------------------------------
  # The frontend's own .portal dir must exist (it scans it for backends);
  # the backend files are added by build.sh from ports/portal/data.
  mkdir -p "$S/usr/share/xdg-desktop-portal/portals"
  # GLib's compiled GSettings schemas are not needed: neither service uses
  # GSettings. GIO's content-type sniffing for the FileChooser filters needs
  # the shared-mime-info cache, which ports/firefox already ships; ship the
  # compiled files here too (identical bytes, same Alpine release).
  apk add --no-cache shared-mime-info
  mkdir -p "$S/usr/share/mime"
  find /usr/share/mime -maxdepth 1 -type f -exec cp {} "$S/usr/share/mime/" \;

  # -- ELF fix-ups --------------------------------------------------------------
  cc -shared -fPIC -fno-stack-protector -Wl,-soname,libleandros_ssp.so.1 \
    -o "$S/usr/lib/libleandros_ssp.so.1" /src/ssp_guard.c
  find "$S" -type f | while read -r f; do if file -b "$f" | grep -q '^ELF'; then echo "$f"; fi; done > /tmp/all-elves
  for f in $(cat /tmp/all-elves); do
    case "$f" in */libleandros_ssp.so.1|*/libpipewire-0.3.so.0) continue ;; esac
    if readelf -d "$f" | grep -q "libc.musl-$ARCH.so.1"; then
      patchelf --replace-needed "libc.musl-$ARCH.so.1" libc.so "$f"
    fi
    if readelf --dyn-syms -W "$f" | awk '$7=="UND"{print $8}' | sed 's/@.*//' | grep -qx '__stack_chk_guard'; then
      patchelf --add-needed libleandros_ssp.so.1 "$f"
    fi
  done
  for f in $(cat /tmp/all-elves); do
    if readelf -d "$f" | grep -q 'libc\.musl'; then echo "musl soname still present: $f"; exit 5; fi
  done

  # -- unresolved-symbol audit (same method as ports/firefox) --------------------
  img_lib() {
    grep -qx "$1" /src/image-sonames.txt 2>/dev/null || return 1
    for d in /imglib/*; do
      [ -e "$d/$1" ] && { readlink -f "$d/$1"; return 0; }
    done
    return 1
  }
  if [ -f /leandros-libc/libc.so ]; then
    : > "$S/COLLISIONS.txt"
    echo /leandros-libc/libc.so > /tmp/deflibs
    for f in $(cat /tmp/all-elves); do
      b=$(basename "$f")
      if [ "$f" = "$S/usr/lib/$b" ] && [ "$b" != libpipewire-0.3.so.0 ] && il=$(img_lib "$b"); then
        echo "$b  image=$il" >> "$S/COLLISIONS.txt"; echo "$il" >> /tmp/deflibs
      else
        echo "$f" >> /tmp/deflibs
      fi
    done
    for f in $(cat /tmp/deflibs); do
      readelf --dyn-syms -W "$f" | awk '$7!="UND" && $8!="" {sub(/@.*/,"",$8); print $8}'
    done | sort -u > /tmp/defined
    for f in $(cat /tmp/all-elves); do
      readelf --dyn-syms -W "$f" | awk '$7=="UND" && $5!="WEAK" && $8!="" {sub(/@.*/,"",$8); print $8}' | sort -u \
        | comm -23 - /tmp/defined | sed "s|^|${f#$S}: |"
    done > "$S/SYMCHECK.txt"
    echo "sonames where the image's copy wins: $(wc -l < "$S/COLLISIONS.txt")"; cat "$S/COLLISIONS.txt"
    echo "unresolved strong symbols: $(wc -l < "$S/SYMCHECK.txt")"; head -40 "$S/SYMCHECK.txt"
  else
    echo "no /leandros-libc/libc.so mounted; symbol audit skipped" > "$S/SYMCHECK.txt"
  fi

  # -- SDK for the host Rust build ------------------------------------------------
  cp -a /usr/include/glib-2.0 /usr/include/gio-unix-2.0 "$K/usr/include/"
  mkdir -p "$K/usr/lib/glib-2.0"
  cp -a /usr/lib/glib-2.0/include "$K/usr/lib/glib-2.0/"
  for f in "$S"/usr/lib/*.so*; do
    b=$(basename "$f"); cp "$f" "$K/usr/lib/$b"
    case "$b" in lib*.so.*) ln -sf "$b" "$K/usr/lib/${b%%.so.*}.so" ;; esac
  done

  echo "$XDPVER $(apk info -e -v glib) alpine-$(cat /etc/alpine-release)" > "$S/PORTAL-VERSION"
  echo "== staged size =="; du -sh "$S" "$K"
  echo "== closure =="; cat "$S/CLOSURE.txt"

  [ -x "$S/usr/libexec/xdg-desktop-portal" ]
  rm -rf "/out/$ARCH.alpine" "/out/sdk-$ARCH"
  mkdir -p "/out/$ARCH.alpine" "/out/sdk-$ARCH"
  cp -a "$S/." "/out/$ARCH.alpine/"
  cp -a "$K/." "/out/sdk-$ARCH/"
)
echo "=== rc=$? arch=$ARCH ==="
