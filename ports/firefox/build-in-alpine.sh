#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> — stage Alpine's prebuilt Firefox for
# LeandrOS. Nothing is compiled except the 1-line __stack_chk_guard shim.
#
# Runs INSIDE an Alpine 3.21 container of the TARGET architecture, the same
# release as the GPU Mesa stack (ports/mesa/build-gpu-stack-alpine.sh), so
# musl (1.2.5), libstdc++ (GCC 14.2) and the Wayland/GL ABI Firefox was built
# against match what the image already runs. 3.21 carries firefox 136.0.4 in
# community for both aarch64 and x86_64. Driven by ports/firefox/build.sh,
# which mounts:
#   /src           ports/firefox (read-only snapshot)
#   /out           ports/firefox/out; this writes /out/<arch>/
#   /leandros-libc the guest's musl libc.so (read-only, optional) for the
#                  unresolved-symbol audit
#   /imglib/N      the image's own /usr/lib sources (read-only, optional),
#                  so the audit checks against what the guest really loads
#
# Output: /out/<arch>/ is a rootfs-shaped tree that
# scripts/mkfs-f2fs-populated.py overlays onto the image:
#   usr/lib/firefox/...           Firefox itself (libxul.so, omni.ja, ...)
#   usr/lib/<soname>              the DT_NEEDED closure (+ known dlopen()s),
#                                 each stored under its SONAME with symlinks
#                                 dereferenced — the image's soname convention
#   usr/lib/gdk-pixbuf-2.0/...    pixbuf loaders + loaders.cache
#   usr/share/fonts/dejavu/...    fonts, etc/fonts/... fontconfig config
#   usr/share/glib-2.0/schemas/gschemas.compiled   GTK's GSettings schemas
#   FIREFOX-VERSION, CLOSURE.txt, SYMCHECK.txt      provenance / audit
#
# What is deliberately NOT shipped: Alpine's Mesa (libEGL/libGLESv2/libGL/
# libgbm/libglapi/libgallium/dri), libdrm*, libvulkan and libwayland-{client,
# server,egl}. Firefox dlopen()s EGL/GLES/GBM/DRM and GTK links Wayland; all of
# them must be the image's own copies (the LeandrOS GPU ship-set, built with
# the Venus/zink/virgl patches). Shipping Alpine's would silently put its
# stock Mesa — with no Venus ICD — in front of ours. Every other soname the
# image already has (libffi, libxkbcommon, libpixman, libudev shim, ...) is
# ALSO staged here, but mkfs-f2fs-populated.py lets the image's copy win.
#
# ELF fix-ups, the Mesa-stack recipe:
#   * DT_NEEDED libc.musl-<arch>.so.1 -> libc.so (the guest's soname).
#   * PT_INTERP is left alone: Alpine's /lib/ld-musl-<arch>.so.1 is exactly the
#     path the image packs its libc.so under (hardlinked), as for cosmic-comp.
#   * Alpine builds everything -fstack-protector-strong. On aarch64 GCC reads
#     the canary from the GLOBAL __stack_chk_guard, which LeandrOS libc.so does
#     not export (x86_64 uses %fs:0x28 and needs nothing). Every ELF that
#     imports it gets libleandros_ssp.so.1 added to its DT_NEEDED; the check is
#     done per ELF on the real symbol table, not assumed per arch.
#
# Emits '=== rc=N arch=A ===' as the LAST line — trust that, not log content.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  [ "$(uname -m)" = "$ARCH" ] || { echo "container is $(uname -m), wanted $ARCH"; exit 3; }
  grep -q '^3\.21\.' /etc/alpine-release || { echo "want Alpine 3.21, got $(cat /etc/alpine-release)"; exit 3; }
  apk add --no-cache firefox font-dejavu fontconfig gdk-pixbuf gtk+3.0 \
    pciutils-libs nss alsa-lib binutils file patchelf build-base
  FFVER=$(apk info -e -v firefox)
  echo "package: $FFVER (alpine $(cat /etc/alpine-release))"

  S=/tmp/firefox-stage-$ARCH
  rm -rf "$S"
  mkdir -p "$S/usr/lib" "$S/usr/share" "$S/etc"

  # -- Firefox itself ---------------------------------------------------------
  cp -a /usr/lib/firefox "$S/usr/lib/firefox"
  # The firefox launcher stays linked against scudo (libscudo.so), Alpine's
  # malloc for Firefox (built --disable-jemalloc). Its RW segment carries a
  # 512 MiB .bss, so musl's first whole-span file mmap is ~512 MiB; that
  # needs the kernel's demand-paged file-mmap limit above 256 MiB (it used to
  # fail with "Error loading shared library libscudo.so: Invalid argument").

  # -- closure ----------------------------------------------------------------
  # Sonames never shipped from Alpine (see header). Globs, matched by `case`.
  is_excluded() {
    case "$1" in
      libc.musl-*|ld-musl-*|libc.so) return 0 ;;
      libEGL.so*|libGL.so*|libGLX*|libGLdispatch*|libGLESv1_CM*|libGLESv2.so*) return 0 ;;
      libgbm.so*|libglapi.so*|libgallium*|libdrm*.so*|libvulkan.so*) return 0 ;;
      libwayland-client.so*|libwayland-server.so*|libwayland-egl.so*) return 0 ;;
      # The image's libudev is a LeandrOS shim over its synthetic /sys
      # (ports/input-stack/shims); Firefox dlopen()s it for gamepads.
      libudev.so*) return 0 ;;
    esac
    return 1
  }
  find_lib() {
    for d in /usr/lib/firefox /usr/lib /lib; do
      [ -e "$d/$1" ] && { echo "$d/$1"; return 0; }
    done
    return 1
  }
  needed_of() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'; }

  # Roots: every ELF in the Firefox tree, the pixbuf loaders, and the libraries
  # Firefox / NSS / GTK dlopen() by soname (none of these show up as NEEDED):
  #   libsoftokn3/libfreeblpriv3/libfreebl3/libnssckbi/libnssdbm3  NSS PKCS#11
  #     modules, loaded by libnss3 on NSS_Init — without them there is no
  #     profile crypto store and no TLS.
  #   libpci.so.3   glxtest's GPU identification.
  #   libgcc_s.so.1 musl's unwinder for C++ exceptions / pthread_cancel.
  #   libudev.so.1  gamepad/device probing (recorded as image-provided).
  #   libxkbcommon.so.0, libwayland-cursor.so.0  GTK's Wayland backend.
  #   libepoxy.so.0 GTK's GL entry points (it in turn dlopen()s OUR libEGL).
  : > /tmp/queue
  find "$S/usr/lib/firefox" -type f | while read -r f; do
    if file -b "$f" | grep -q '^ELF'; then echo "$f"; fi
  done > /tmp/ff-elves
  for f in $(cat /tmp/ff-elves); do needed_of "$f"; done >> /tmp/queue
  for so in libsoftokn3.so libfreeblpriv3.so libfreebl3.so libnssckbi.so libnssdbm3.so \
            libpci.so.3 libgcc_s.so.1 libudev.so.1 libxkbcommon.so.0 \
            libwayland-cursor.so.0 libepoxy.so.0; do
    echo "$so" >> /tmp/queue
  done
  PIXDIR=$(ls -d /usr/lib/gdk-pixbuf-2.0/2.10.0 2>/dev/null || true)
  if [ -n "$PIXDIR" ]; then
    mkdir -p "$S$PIXDIR"
    cp -a "$PIXDIR/." "$S$PIXDIR/"
    find "$S$PIXDIR" -name '*.so' | while read -r f; do needed_of "$f"; done >> /tmp/queue
  fi

  : > /tmp/seen
  : > "$S/CLOSURE.txt"
  : > /tmp/missing
  while [ -s /tmp/queue ]; do
    sort -u /tmp/queue > /tmp/q2; : > /tmp/queue
    for so in $(cat /tmp/q2); do
      grep -qx "$so" /tmp/seen && continue
      echo "$so" >> /tmp/seen
      if is_excluded "$so"; then
        echo "image  $so" >> "$S/CLOSURE.txt"; continue
      fi
      src=$(find_lib "$so") || { echo "$so" >> /tmp/missing; echo "MISSING $so" >> "$S/CLOSURE.txt"; continue; }
      case "$src" in
        /usr/lib/firefox/*) echo "ff     $so" >> "$S/CLOSURE.txt" ;;
        *) cp -L "$src" "$S/usr/lib/$so"
           echo "alpine $so  <- $(readlink -f "$src") ($(apk info -W "$(readlink -f "$src")" 2>/dev/null | awk '{print $NF}'))" >> "$S/CLOSURE.txt" ;;
      esac
      needed_of "$(readlink -f "$src")" >> /tmp/queue
    done
  done
  # The five NSS module names above are optional (libnssdbm3 is gone upstream);
  # anything else missing is a real closure hole.
  grep -v -e '^libnssdbm3\.so$' -e '^libfreebl3\.so$' /tmp/missing > /tmp/missing-hard || true
  if [ -s /tmp/missing-hard ]; then echo "unresolved DT_NEEDED:"; cat /tmp/missing-hard; exit 4; fi

  # -- system FFmpeg ------------------------------------------------------------
  # H.264/AAC decode: libavcodec + libavutil (dlopen()ed by Firefox's FFmpeg
  # PDM) and their closure. ffmpeg-libavcodec is a dependency of the firefox
  # package, so it is installed already; ffmpeg-in-alpine.sh explains why it
  # is needed. The ELF fix-ups and the audit below cover what it copies.
  sh /src/ffmpeg-in-alpine.sh "$ARCH" "$S" /

  # -- data ---------------------------------------------------------------------
  # fontconfig: its conf.d is a directory of symlinks into conf.avail; the
  # image writer does not follow symlinks, so dereference them here.
  mkdir -p "$S/etc/fonts"
  cp -RL /etc/fonts/. "$S/etc/fonts/"
  mkdir -p "$S/usr/share/fonts"
  cp -RL /usr/share/fonts/dejavu "$S/usr/share/fonts/"
  # GTK3's compiled GSettings schemas (org.gtk.Settings.*); GTK aborts when a
  # file chooser or colour chooser asks for a schema that is absent.
  if [ -f /usr/share/glib-2.0/schemas/gschemas.compiled ]; then
    mkdir -p "$S/usr/share/glib-2.0/schemas"
    cp /usr/share/glib-2.0/schemas/gschemas.compiled "$S/usr/share/glib-2.0/schemas/"
  fi

  # -- icon theme -----------------------------------------------------------------
  # GTK aborts the process when an icon lookup misses and 'image-missing' (its
  # last resort) is not in the theme either ("Icon 'image-missing' not present
  # in theme Adwaita"). GTK3 on Wayland with no org.gnome.desktop.interface
  # schema resolves the theme name "Adwaita", so the minimal theme lives at
  # /usr/share/icons/Adwaita, inheriting hicolor (already in the image).
  #
  # It is NOT adwaita-icon-theme (13.6 MB, 10 MB of cursors, SVG-only, which
  # would also need librsvg's gdk-pixbuf loader at runtime). It is those of
  # Adwaita's icons that are actually requested, rendered to PNG here with
  # rsvg-convert (container-only) at the sizes GTK asks for:
  #   * every -gtk-icontheme() name in GTK's built-in CSS theme, extracted
  #     from libgtk's registered resources by a throwaway program below;
  #   * the names listed in /src/icons.txt — observed at runtime with the
  #     icontrace preload (LEANDROS_FIREFOX_ICON_TRACE=1, see firefox.sh).
  # Symbolic icons become plain PNGs in Adwaita's own grey: GTK loads
  # "<name>-symbolic.png" as-is rather than recolouring it.
  # (No gtk+3.0-dev: it drags in icu-data-en, which conflicts with Firefox's
  # icu-data-full. The few GLib/GTK entry points are declared by hand.)
  apk add --no-cache adwaita-icon-theme rsvg-convert
  cat > /tmp/gtkcss.c <<'EOF'
#include <stdio.h>
#include <string.h>
typedef struct _GBytes GBytes;
int gtk_init_check(int *argc, char ***argv);
char **g_resources_enumerate_children(const char *path, int flags, void **err);
GBytes *g_resources_lookup_data(const char *path, int flags, void **err);
const void *g_bytes_get_data(GBytes *b, size_t *n);
static void walk(const char *path) {
  char **kids = g_resources_enumerate_children(path, 0, NULL);
  if (!kids) return;
  for (int i = 0; kids[i]; i++) {
    char p[1024];
    snprintf(p, sizeof p, "%s%s", path, kids[i]);
    size_t k = strlen(kids[i]);
    if (k && kids[i][k - 1] == '/') walk(p);
    else if (k > 4 && !strcmp(kids[i] + k - 4, ".css")) {
      GBytes *b = g_resources_lookup_data(p, 0, NULL);
      if (b) { size_t n; const char *d = g_bytes_get_data(b, &n); fwrite(d, 1, n, stdout); }
    }
  }
}
int main(void) { gtk_init_check(NULL, NULL); walk("/org/gtk/libgtk/"); return 0; }
EOF
  cc -o /tmp/gtkcss /tmp/gtkcss.c /usr/lib/libgtk-3.so.0 /usr/lib/libgio-2.0.so.0 /usr/lib/libglib-2.0.so.0
  /tmp/gtkcss | grep -o "gtk-icontheme([\"'][^\"']*" | sed "s/gtk-icontheme([\"']//" > /tmp/icon-names
  grep -v '^#' /src/icons.txt | awk 'NF{print $1}' >> /tmp/icon-names
  sort -u /tmp/icon-names -o /tmp/icon-names
  T="$S/usr/share/icons/Adwaita"
  mkdir -p "$T"
  : > /tmp/icon-dirs
  : > "$S/ICONS.txt"
  for name in $(cat /tmp/icon-names); do
    src=$(find /usr/share/icons/Adwaita -name "$name.svg" | sort | head -1)
    if [ -z "$src" ]; then echo "missing $name" >> "$S/ICONS.txt"; continue; fi
    ctx=$(basename "$(dirname "$src")")
    for sz in 16 24 32 48; do
      d="${sz}x${sz}/$ctx"
      mkdir -p "$T/$d"
      rsvg-convert -w "$sz" -h "$sz" -o "$T/$d/$name.png" "$src"
      echo "$d $sz $ctx" >> /tmp/icon-dirs
    done
    echo "icon    $name  <- ${src#/usr/share/icons/Adwaita/}" >> "$S/ICONS.txt"
  done
  [ -f "$T/16x16/status/image-missing.png" ] || { echo "no image-missing rendered"; exit 6; }
  sort -u /tmp/icon-dirs -o /tmp/icon-dirs
  {
    echo "[Icon Theme]"
    echo "Name=Adwaita"
    echo "Comment=LeandrOS subset of Adwaita, pre-rendered to PNG (ports/firefox)"
    echo "Inherits=hicolor"
    echo "Hidden=true"
    printf 'Directories='; awk '{printf "%s,", $1}' /tmp/icon-dirs; echo
    while read -r d sz ctx; do
      echo; echo "[$d]"; echo "Size=$sz"; echo "Type=Fixed"
      echo "Context=$(echo "$ctx" | awk '{print toupper(substr($1,1,1)) substr($1,2)}')"
    done < /tmp/icon-dirs
  } > "$T/index.theme"
  gtk-update-icon-cache -f -t "$T"
  echo "icon theme: $(grep -c '^icon ' "$S/ICONS.txt") icons, $(grep -c '^missing ' "$S/ICONS.txt") names with no Adwaita source, $(du -sh "$T" | cut -f1)"

  # gdk-pixbuf picks a loader by sniffing the content type through GIO, which
  # needs shared-mime-info's compiled database; without it every icon PNG
  # fails with "Unrecognized image file format" and GTK aborts just the same.
  # Ship the compiled files only (mime.cache, magic, globs2, ...), not the
  # per-type XML under packages/ and the media-type directories (~5 MB).
  mkdir -p "$S/usr/share/mime"
  find /usr/share/mime -maxdepth 1 -type f -exec cp {} "$S/usr/share/mime/" \;
  [ -f "$S/usr/share/mime/mime.cache" ] || { echo "no shared-mime-info cache"; exit 6; }

  # The icon-lookup tracer (LD_PRELOAD'ed by /bin/firefox on request).
  cc -shared -fPIC -O2 -fno-stack-protector -o "$S/usr/lib/firefox/libleandros-icontrace.so" /src/icontrace.c

  # -- ELF fix-ups --------------------------------------------------------------
  cc -shared -fPIC -fno-stack-protector -Wl,-soname,libleandros_ssp.so.1 \
    -o "$S/usr/lib/libleandros_ssp.so.1" /src/ssp_guard.c
  NSSP=0
  find "$S" -type f | while read -r f; do if file -b "$f" | grep -q '^ELF'; then echo "$f"; fi; done > /tmp/all-elves
  for f in $(cat /tmp/all-elves); do
    case "$f" in */libleandros_ssp.so.1) continue ;; esac
    if readelf -d "$f" | grep -q "libc.musl-$ARCH.so.1"; then
      patchelf --replace-needed "libc.musl-$ARCH.so.1" libc.so "$f"
    fi
    if readelf --dyn-syms -W "$f" | awk '$7=="UND"{print $8}' | sed 's/@.*//' | grep -qx '__stack_chk_guard'; then
      patchelf --add-needed libleandros_ssp.so.1 "$f"
      NSSP=$((NSSP + 1))
    fi
  done
  echo "ssp shim added to $NSSP ELF(s)"
  for f in $(cat /tmp/all-elves); do
    if readelf -d "$f" | grep -q 'libc\.musl'; then echo "musl soname still present: $f"; exit 5; fi
    if readelf --dyn-syms -W "$f" | awk '$7=="UND"{print $8}' | sed 's/@.*//' | grep -qx '__stack_chk_guard'; then
      readelf -d "$f" | grep -q libleandros_ssp || { echo "unresolved __stack_chk_guard: $f"; exit 5; }
    fi
  done

  # -- unresolved-symbol audit ----------------------------------------------------
  # Every non-weak undefined symbol of every shipped ELF must be defined by
  # something the GUEST will actually have loaded: its libc.so, the staged
  # tree, and — for every soname the image already packs — the IMAGE's copy,
  # because mkfs-f2fs-populated.py lets the image's copy win a name clash
  # (build.sh mounts the image's library sources at /imglib/N). musl resolves
  # data and non-PLT relocations eagerly, so a hole found here is a load-time
  # failure. Results: SYMCHECK.txt (holes), COLLISIONS.txt (image-won names).
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
      if [ "$f" = "$S/usr/lib/$b" ] && il=$(img_lib "$b"); then
        echo "$b  image=$il" >> "$S/COLLISIONS.txt"; echo "$il" >> /tmp/deflibs
      else
        echo "$f" >> /tmp/deflibs
      fi
    done
    # Image-provided (never staged) sonames, plus what Firefox dlopen()s from
    # the image: EGL/GLES/GBM/DRM.
    for so in $(awk '$1=="image"{print $2}' "$S/CLOSURE.txt") \
              libEGL.so.1 libGLESv2.so.2 libgbm.so.1 libdrm.so.2; do
      if il=$(img_lib "$so"); then echo "$il" >> /tmp/deflibs; fi
    done
    for f in $(cat /tmp/deflibs); do
      readelf --dyn-syms -W "$f" | awk '$7!="UND" && $8!="" {sub(/@.*/,"",$8); print $8}'
    done | sort -u > /tmp/defined
    for f in $(cat /tmp/all-elves); do
      readelf --dyn-syms -W "$f" | awk '$7=="UND" && $5!="WEAK" && $8!="" {sub(/@.*/,"",$8); print $8}' | sort -u \
        | comm -23 - /tmp/defined | sed "s|^|${f#$S}: |"
    done > "$S/SYMCHECK.txt"
    echo "image libraries mounted: $(ls -d /imglib/* 2>/dev/null | wc -l) dir(s)"
    echo "sonames where the image's copy wins: $(wc -l < "$S/COLLISIONS.txt")"
    cat "$S/COLLISIONS.txt"
    echo "unresolved strong symbols (guest libc + image libs + stage): $(wc -l < "$S/SYMCHECK.txt")"
    head -60 "$S/SYMCHECK.txt"
  else
    echo "no /leandros-libc/libc.so mounted; symbol audit skipped" > "$S/SYMCHECK.txt"
  fi

  echo "$FFVER alpine-$(cat /etc/alpine-release)" > "$S/FIREFOX-VERSION"
  echo "== staged size =="; du -sh "$S" "$S/usr/lib/firefox" "$S/usr/lib" "$S/usr/share" "$S/etc"
  echo "== closure =="; cat "$S/CLOSURE.txt"

  # Never copy anything but a finished stage.
  [ -n "$S" ] && [ "$S" != / ] && [ -f "$S/usr/lib/firefox/libxul.so" ]
  rm -rf "/out/$ARCH"
  mkdir -p "/out/$ARCH"
  cp -a "$S/." "/out/$ARCH/"
)
echo "=== rc=$? arch=$ARCH ==="
