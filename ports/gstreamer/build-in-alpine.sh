#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> -- stage Alpine 3.21's prebuilt GStreamer
# 1.24 (core, plugins-base/good, gst-libav + FFmpeg 6.1, the PipeWire plugin
# 1.2.7 matching ports/pipewire) for LeandrOS, plus the -dev files (headers,
# .pc, linkable .so) for the cross sysroot, so cosmic-player's gstreamer-rs
# crates link and run. Same Alpine release as ports/pipewire and ports/firefox
# (musl 1.2.5, same GLib 2.82), so the shared libs are byte-compatible with the
# ones those ports already stage; mkfs overlays them onto the same paths.
# Output: /out/<arch>.alpine/ (the image tree), /out/<arch>.sysroot/ (-dev).
# Mounts: /src (this dir), /out, /leandros-libc, /imglib/N as in ports/pipewire.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  grep -q '^3\.21\.' /etc/alpine-release || { echo "want Alpine 3.21"; exit 3; }
  PKGS="gstreamer gstreamer-tools gst-plugins-base gst-plugins-good gst-libav gst-plugin-pipewire"
  DEVPKGS="pipewire-dev gstreamer-dev gst-plugins-base-dev glib-dev util-linux-dev pcre2-dev"
  if [ "$(uname -m)" = "$ARCH" ]; then
    R=""; APK="apk"
    apk add --no-cache $PKGS $DEVPKGS binutils file patchelf build-base >/dev/null
    CC="cc"
  else
    R=/tmp/root-$ARCH
    apk add --no-cache binutils file patchelf clang lld alpine-keys >/dev/null
    mkdir -p "$R/etc/apk/keys"; cp /usr/share/apk/keys/$ARCH/* "$R/etc/apk/keys/"; cp /etc/apk/repositories "$R/etc/apk/"
    APK="apk --root $R --arch $ARCH"
    $APK add --initdb --no-scripts --no-cache $PKGS $DEVPKGS >/dev/null
    CC="clang --target=$ARCH-alpine-linux-musl --sysroot=$R -fuse-ld=lld"
  fi
  GSTVER=$($APK info -e -v gstreamer); PWV=$($APK info -e -v gst-plugin-pipewire)
  echo "package: $GSTVER $PWV $($APK info -e -v gst-libav) (alpine $(cat /etc/alpine-release))"
  S=/tmp/gst-stage-$ARCH
  rm -rf "$S"; mkdir -p "$S/usr/bin" "$S/usr/lib/gstreamer-1.0" "$S/usr/libexec/gstreamer-1.0"
  for b in gst-launch-1.0 gst-inspect-1.0 gst-discoverer-1.0; do cp -L "$R/usr/bin/$b" "$S/usr/bin/"; done
  cp -L "$R/usr/libexec/gstreamer-1.0/gst-plugin-scanner" "$S/usr/libexec/gstreamer-1.0/"
  # plugin whitelist (no alsa/pulse/v4l2/soup/gtk/qt/jpeg/png/cairo/pango/gl:
  # LeandrOS has none of those devices and they drag in large closures)
  for p in coreelements coretracers adder app audioconvert audiomixer audiorate audioresample audiotestsrc \
           compositor encoding gio overlaycomposition pbtypes playback rawparse subparse tcp typefindfunctions \
           videoconvertscale videorate videotestsrc volume ogg vorbis opus theora \
           audioparsers autodetect wavparse wavenc isomp4 matroska avi id3demux apetag flac multifile \
           audiofx videofilter level equalizer deinterlace flv imagefreeze interleave replaygain udp \
           videocrop videobox libav pipewire; do
    f="$R/usr/lib/gstreamer-1.0/libgst$p.so"
    if [ -e "$f" ]; then cp -L "$f" "$S/usr/lib/gstreamer-1.0/"; else echo "plugin not in alpine: $p"; fi
  done
  # libgstpipewire rebuilt from the vendored 1.2.7 sources: upstream's pool-activation
  # wake-up is lost (data/gst-pipewire-1.2.7/gstpipewiresink.c, on_param_changed).
  G=/tmp/gstpw-$ARCH; rm -rf "$G"; mkdir -p "$G"; mkdir -p "$G/gst"; cp /src/data/gst-pipewire-1.2.7/*.[ch] "$G/gst/"; ln -s gst/gstpipewire.c "$G/x" 2>/dev/null; rm -f "$G/x"
  printf '#define PACKAGE "pipewire"\n#define PACKAGE_VERSION "1.2.7"\n#define HAVE_GSTREAMER_DEVICE_PROVIDER 1\n' > "$G/config.h"; cp "$G/config.h" "$G/gst/config.h"
  INC=""; for i in gstreamer-1.0 glib-2.0 pipewire-0.3 spa-0.2 ../lib/glib-2.0/include; do INC="$INC -I$R/usr/include/$i"; done
  $CC -shared -fPIC -O2 -DHAVE_CONFIG_H -DGST_USE_UNSTABLE_API -I"$G" $INC "$G"/gst/gstpipewire*.c \
    -L"$R/usr/lib" -lgstvideo-1.0 -lgstaudio-1.0 -lgstallocators-1.0 -lgstbase-1.0 -lgstreamer-1.0 -lgobject-2.0 -lglib-2.0 -lpipewire-0.3 \
    -o "$S/usr/lib/gstreamer-1.0/libgstpipewire.so"
  echo "libgstpipewire rebuilt with the pool wake-up fix"
  ls "$S/usr/lib/gstreamer-1.0" | tr '\n' ' '; echo
  # -- closure ------------------------------------------------------------------
  is_excluded() {
    case "$1" in
      libc.musl-*|ld-musl-*|libc.so) return 0 ;;
    esac
    [ -e "$S/usr/lib/gstreamer-1.0/$1" ] && return 0
    return 1
  }
  find_lib() {
    for d in "$R/usr/lib" "$R/lib"; do
      [ -e "$d/$1" ] && { echo "$d/$1"; return 0; }
    done
    return 1
  }
  needed_of() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'; }
  find "$S" -type f | while read -r f; do if file -b "$f" | grep -q '^ELF'; then echo "$f"; fi; done > /tmp/roots
  { for f in $(cat /tmp/roots); do needed_of "$f"; done; echo libgstreamer-1.0.so.0; } > /tmp/queue
  : > /tmp/seen; : > "$S/CLOSURE.txt"; : > /tmp/missing
  while [ -s /tmp/queue ]; do
    sort -u /tmp/queue > /tmp/q2; : > /tmp/queue
    for so in $(cat /tmp/q2); do
      grep -qx "$so" /tmp/seen && continue
      echo "$so" >> /tmp/seen
      if is_excluded "$so"; then echo "image  $so" >> "$S/CLOSURE.txt"; continue; fi
      src=$(find_lib "$so") || { echo "$so" >> /tmp/missing; echo "MISSING $so" >> "$S/CLOSURE.txt"; continue; }
      cp -L "$src" "$S/usr/lib/$so"
      real=$(cd "$(dirname "$src")" && readlink -f "$src")
      echo "alpine $so  <- ${real#$R} ($($APK info -W "${real#$R}" 2>/dev/null | awk '{print $NF}'))" >> "$S/CLOSURE.txt"
      needed_of "$real" >> /tmp/queue
    done
  done
  if [ -s /tmp/missing ]; then echo "unresolved DT_NEEDED:"; cat /tmp/missing; exit 4; fi

  # -- ELF fix-ups (exactly as ports/portal) ----------------------------------------
  $CC -shared -fPIC -fno-stack-protector -nostdlib -Wl,-soname,libleandros_ssp.so.1 \
    -o "$S/usr/lib/libleandros_ssp.so.1" /src/ssp_guard.c
  find "$S" -type f | while read -r f; do if file -b "$f" | grep -q '^ELF'; then echo "$f"; fi; done > /tmp/all-elves
  # Runtime libc is Alpine musl itself and exports __stack_chk_guard: no soname
  # rewrite and no libleandros_ssp injection. Guard: the libc we build against
  # (same package as the runtime one) must export it.
  readelf --dyn-syms -W /lib/ld-musl-$ARCH.so.1 | grep -qw __stack_chk_guard || { echo "libc does not export __stack_chk_guard"; exit 5; }

  # -- unresolved-symbol audit (same method as ports/portal) ------------------------
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
      if [ "$f" = "$S/usr/lib/$b" ] && [ "$b" != libgstreamer-1.0.so.0 ] && il=$(img_lib "$b"); then
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
    echo "unresolved strong symbols: $(wc -l < "$S/SYMCHECK.txt")"; head -60 "$S/SYMCHECK.txt"
  else
    echo "no /leandros-libc/libc.so mounted; symbol audit skipped" > "$S/SYMCHECK.txt"
  fi

  # -- test clip (native arch only; the same bytes serve every arch) -------------
  if [ "$(uname -m)" = "$ARCH" ]; then
    mkdir -p "$S/usr/share/sounds/leandros"
    gst-launch-1.0 -q matroskamux name=m ! filesink location="$S/usr/share/sounds/leandros/clip-mpeg4-vorbis.mkv" \
      videotestsrc num-buffers=750 pattern=ball ! video/x-raw,width=320,height=240,framerate=25/1 ! videoconvert ! avenc_mpeg4 ! m. \
      audiotestsrc num-buffers=1500 freq=440 samplesperbuffer=960 ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! vorbisenc ! m. \
      && ls -l "$S/usr/share/sounds/leandros/clip-mpeg4-vorbis.mkv"
  fi
  # -- dev tree for the cross sysroot: headers, .pc, linkable libs (ELF-fixed) ----
  D=/tmp/gst-sysroot-$ARCH; rm -rf "$D"; mkdir -p "$D/usr/lib/pkgconfig" "$D/usr/include"
  for d in gstreamer-1.0 glib-2.0 orc-0.4; do [ -d "$R/usr/include/$d" ] && cp -a "$R/usr/include/$d" "$D/usr/include/"; done
  cp -a "$R/usr/lib/glib-2.0" "$D/usr/lib/" 2>/dev/null || true
  for pc in $(ls "$R/usr/lib/pkgconfig" | grep -E '^(gstreamer|gst|glib|gobject|gio|gmodule|gthread|libpcre2|orc|mount|blkid|libmount|uuid)'); do cp -L "$R/usr/lib/pkgconfig/$pc" "$D/usr/lib/pkgconfig/"; done
  for so in $(cat /tmp/seen); do
    s=$(find_lib "$so" 2>/dev/null) || continue
    case "$so" in libc.*) continue ;; esac
    cp -L "$s" "$D/usr/lib/$so"
  done
  for so in $(cd "$R/usr/lib" && ls | grep -E '^lib(gst[a-z0-9.-]*|glib-2.0|gobject-2.0|gio-2.0|gmodule-2.0|gthread-2.0|orc-0.4|mount|blkid|pcre2-8|uuid)\.so$'); do
    cp -L "$R/usr/lib/$so" "$D/usr/lib/$so"
  done
  # libintl: musl-libintl ships only the versioned lib in the runtime package
  [ -e "$D/usr/lib/libintl.so" ] || { [ -e "$D/usr/lib/libintl.so.8" ] && ln -s libintl.so.8 "$D/usr/lib/libintl.so"; }
  cp "$S/usr/lib/libleandros_ssp.so.1" "$D/usr/lib/"
  echo "sysroot dev tree: $(du -sh "$D" | cut -f1)"
  echo "$GSTVER $PWV alpine-$(cat /etc/alpine-release)" > "$S/GSTREAMER-VERSION"
  echo "== staged size =="; du -sh "$S"
  rm -rf "/out/$ARCH.alpine" "/out/$ARCH.sysroot"; mkdir -p "/out/$ARCH.alpine" "/out/$ARCH.sysroot"
  cp -a "$S/." "/out/$ARCH.alpine/"; cp -a "$D/." "/out/$ARCH.sysroot/"
)
echo "=== rc=$? arch=$ARCH ==="
