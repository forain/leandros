#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> — stage Alpine 3.21's prebuilt PipeWire
# 1.2.7 + WirePlumber 0.5 for LeandrOS, and compile leandros-snd-sink (and the
# ScreenCast probe) against pipewire-dev in the same container.
#
# Runs INSIDE an Alpine 3.21 container of the TARGET architecture (same release
# as ports/portal and ports/firefox, so musl 1.2.5 and the GLib ABI match). The
# shipped cosmic-settings-daemon and xdg-desktop-portal-cosmic were linked
# against an inert libpipewire stub generated from 1.2.7 headers
# (~/code/leandros-artifacts/pipewire-gap): this real 1.2.7 library is the same
# ABI, so they need no rebuild.
#
# Mounts (driven by ports/pipewire/build.sh):
#   /src           ports/pipewire (read-only snapshot)
#   /out           ports/pipewire/out; this writes /out/<arch>.alpine/
#   /leandros-libc the guest's musl libc.so (optional) for the symbol audit
#   /imglib/N      the image's own /usr/lib sources (optional)
#
# Not shipped: the ALSA/v4l2/libcamera/bluez/avb SPA plugins (LeandrOS has no
# /dev/snd, /dev/video*, bluetooth), JACK/ROC/AVB/RTP/zeroconf modules.
# Emits '=== rc=N arch=A ===' as the LAST line.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  grep -q '^3\.21\.' /etc/alpine-release || { echo "want Alpine 3.21, got $(cat /etc/alpine-release)"; exit 3; }
  # pipewire-pulse: the PulseAudio protocol server (module-protocol-pulse
  # under `pipewire -c pipewire-pulse.conf`). Firefox's cubeb, like most
  # desktop apps, only speaks PulseAudio (libpulse, staged by ports/firefox);
  # without this server it finds no backend and plays every video silently.
  PKGS="pipewire pipewire-tools pipewire-pulse wireplumber pipewire-dev glib-dev"
  if [ "$(uname -m)" = "$ARCH" ]; then
    R=""
    apk add --no-cache $PKGS binutils file patchelf build-base >/dev/null
    CC="cc"
    APK="apk"
  else
    # Foreign arch without binfmt emulation: install the target's packages
    # into a root WITHOUT running them (--no-scripts) and cross-compile the
    # two small C programs with clang against that root. Same bytes as a
    # native container would stage; nothing of the target arch executes.
    R=/tmp/root-$ARCH
    apk add --no-cache binutils file patchelf clang lld pkgconf alpine-keys >/dev/null
    mkdir -p "$R/etc/apk/keys"
    cp /usr/share/apk/keys/$ARCH/* "$R/etc/apk/keys/"
    cp /etc/apk/repositories "$R/etc/apk/"
    APK="apk --root $R --arch $ARCH"
    $APK add --initdb --no-scripts --no-cache $PKGS build-base >/dev/null
    CC="clang --target=$ARCH-alpine-linux-musl --sysroot=$R -fuse-ld=lld"
    export PKG_CONFIG_SYSROOT_DIR="$R"
    export PKG_CONFIG_LIBDIR="$R/usr/lib/pkgconfig:$R/usr/share/pkgconfig"
    echo "foreign build: container $(uname -m), target $ARCH, root $R"
  fi
  PWVER=$($APK info -e -v pipewire)
  WPVER=$($APK info -e -v wireplumber)
  echo "package: $PWVER $WPVER (alpine $(cat /etc/alpine-release))"

  S=/tmp/pw-stage-$ARCH
  rm -rf "$S"
  mkdir -p "$S/usr/bin" "$S/usr/lib/spa-0.2" "$S/usr/lib/pipewire-0.3" "$S/usr/share"

  # -- binaries ------------------------------------------------------------------
  for b in pipewire wireplumber wpctl pw-cli pw-cat pw-dump pw-link pw-metadata pw-top pw-mon; do
    cp -L "$R/usr/bin/$b" "$S/usr/bin/"
  done
  # pw-play/pw-record are pw-cat under another argv[0]
  ln -sf pw-cat "$S/usr/bin/pw-play"
  ln -sf pw-cat "$S/usr/bin/pw-record"
  # pipewire-pulse is pipewire under another argv[0] (it then loads
  # pipewire-pulse.conf: the pulse server on $XDG_RUNTIME_DIR/pulse/native).
  ln -sf pipewire "$S/usr/bin/pipewire-pulse"

  # -- our sink + the ScreenCast probe -------------------------------------------
  $CC -O2 -Wall -o "$S/usr/bin/leandros-snd-sink" /src/leandros-snd-sink.c \
     $(pkg-config --cflags --libs libpipewire-0.3) -lm
  if [ -f /src/pw-screencast-probe.c ]; then
    $CC -O2 -Wall -o "$S/usr/bin/pw-screencast-probe" /src/pw-screencast-probe.c \
       $(pkg-config --cflags --libs libpipewire-0.3 gio-2.0 gio-unix-2.0) -lm
  fi

  # -- SPA plugins / PipeWire modules (whitelist) ---------------------------------
  for p in support audioconvert audiomixer control videoconvert audiotestsrc videotestsrc; do
    cp -a "$R/usr/lib/spa-0.2/$p" "$S/usr/lib/spa-0.2/"
  done
  for m in protocol-native client-node client-device adapter metadata spa-node-factory \
           spa-device-factory spa-node spa-device link-factory session-manager access rt \
           rtkit profiler portal loopback combine-stream fallback-sink protocol-pulse; do
    cp -L "$R/usr/lib/pipewire-0.3/libpipewire-module-$m.so" "$S/usr/lib/pipewire-0.3/"
  done
  cp -a "$R/usr/lib/wireplumber-0.5" "$S/usr/lib/"
  cp -a "$R/usr/share/pipewire" "$R/usr/share/wireplumber" "$S/usr/share/"

  # -- closure ------------------------------------------------------------------
  is_excluded() {
    case "$1" in
      libc.musl-*|ld-musl-*|libc.so) return 0 ;;
    esac
    # modules linking other modules (RUNPATH /usr/lib/pipewire-0.3)
    [ -e "$S/usr/lib/pipewire-0.3/$1" ] && return 0
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
  { for f in $(cat /tmp/roots); do needed_of "$f"; done; echo libpipewire-0.3.so.0; } > /tmp/queue
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
  for f in $(cat /tmp/all-elves); do
    case "$f" in */libleandros_ssp.so.1) continue ;; esac
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
    echo "unresolved strong symbols: $(wc -l < "$S/SYMCHECK.txt")"; head -60 "$S/SYMCHECK.txt"
  else
    echo "no /leandros-libc/libc.so mounted; symbol audit skipped" > "$S/SYMCHECK.txt"
  fi

  echo "$PWVER $WPVER alpine-$(cat /etc/alpine-release)" > "$S/PIPEWIRE-VERSION"
  echo "== staged size =="; du -sh "$S"
  echo "== closure =="; cat "$S/CLOSURE.txt"

  rm -rf "/out/$ARCH.alpine"
  mkdir -p "/out/$ARCH.alpine"
  cp -a "$S/." "/out/$ARCH.alpine/"
)
echo "=== rc=$? arch=$ARCH ==="
