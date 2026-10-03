#!/bin/sh
# dlopen-in-alpine.sh <x86_64|aarch64> <stage> [<pkgroot> [--fixup]]
#
# Adds the libraries Firefox dlopen()s by soname (so they never show up as
# DT_NEEDED and the main closure walk misses them), with their own DT_NEEDED
# closure, to a Firefox stage tree:
#
#   * system FFmpeg (ffmpeg-libavcodec: libavcodec + libavutil). Firefox has
#     no H.264/AAC decoder of its own: its bundled libmozavcodec (ffvpx) only
#     does VP8/VP9/AV1/Opus/Vorbis/FLAC/MP3, and H.264/AAC/HEVC go through the
#     system libavcodec, which the FFmpeg PDM dlopen()s (libavcodec.so.53..61).
#     Alpine's firefox package depends on ffmpeg-libavcodec for exactly this.
#     Without it, H.264 video either fails or falls back to Cisco's OpenH264
#     GMP plugin, a glibc build a musl process cannot dlopen: the GMP child
#     then dies in MOZ_CRASH("Cannot load plugin as library") and Firefox shows
#     "The gmpopenh264 plugin crashed" (see leandros-prefs.js).
#   * libpulse (libpulse.so.0, + libpulsecommon-17.0.so from
#     /usr/lib/pulseaudio). Audio output: cubeb (in the parent process, via
#     audioipc) tries its PulseAudio backend first (dlopen("libpulse.so.0")),
#     then ALSA. LeandrOS has no /dev/snd, so ALSA can only fail, and without
#     libpulse every video played silently. The server side is pipewire-pulse
#     (ports/pipewire, $XDG_RUNTIME_DIR/pulse/native).
#
# Two ways to run it:
#   * from build-in-alpine.sh, with <pkgroot> = / (the packages are added to
#     that container if the firefox package did not pull them in already);
#     that script's own ELF fix-ups and symbol audit then cover the copied
#     libraries.
#   * standalone through `build.sh <arch> --dlopen-only`, to add them to an
#     existing out/<arch> without restaging Firefox. The container may be of
#     ANY architecture: the packages are installed into a separate <pkgroot>
#     with `apk --arch`, nothing of the target architecture is executed, and
#     --fixup applies the same musl-soname / __stack_chk_guard fix-ups as
#     build-in-alpine.sh. This is what lets the aarch64 tree be updated from an
#     x86_64 podman host that has no binfmt emulation.
#
# Appends "alpine <soname>  <- ..." lines to <stage>/CLOSURE.txt, like the
# main closure walk.
set -eu
ARCH="$1"; S="$2"; R="${3:-/}"; FIXUP="${4:-}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "bad arch $ARCH"; exit 2 ;; esac
[ -d "$S/usr/lib" ] || { echo "no stage at $S"; exit 2; }

if [ "$R" != / ]; then
  # Foreign-root install: the target arch's packages, verified with that
  # arch's Alpine signing keys (alpine-keys ships every arch's keys).
  mkdir -p "$R/etc/apk/keys"
  cp /usr/share/apk/keys/"$ARCH"/*.pub "$R/etc/apk/keys/"
  cp /etc/apk/repositories "$R/etc/apk/" 2>/dev/null || true
  apk add --root "$R" --arch "$ARCH" --initdb --no-scripts --no-cache \
    --repositories-file /etc/apk/repositories ffmpeg-libavcodec libpulse
else
  apk add --no-cache ffmpeg-libavcodec libpulse >/dev/null
fi

# The libavcodec soname this Alpine ships (6.1 => .60); Firefox 136 accepts
# 53..61.
AVC=$(cd "$R/usr/lib" && ls libavcodec.so.* 2>/dev/null | grep -E '^libavcodec\.so\.[0-9]+$' | head -1)
AVU=$(cd "$R/usr/lib" && ls libavutil.so.* 2>/dev/null | grep -E '^libavutil\.so\.[0-9]+$' | head -1)
[ -n "$AVC" ] && [ -n "$AVU" ] || { echo "ffmpeg-libavcodec not installed in $R"; exit 3; }
echo "system FFmpeg: $AVC $AVU ($(apk --root "$R" info -e -v ffmpeg-libavcodec 2>/dev/null))"
[ -e "$R/usr/lib/libpulse.so.0" ] || { echo "libpulse not installed in $R"; exit 3; }
echo "libpulse: $(apk --root "$R" info -e -v libpulse 2>/dev/null)"

# Never shipped from Alpine: the image provides these (see build-in-alpine.sh).
is_excluded() {
  case "$1" in
    libc.musl-*|ld-musl-*|libc.so) return 0 ;;
    libEGL.so*|libGL.so*|libGLX*|libGLdispatch*|libGLESv1_CM*|libGLESv2.so*) return 0 ;;
    libgbm.so*|libglapi.so*|libgallium*|libdrm*.so*|libvulkan.so*) return 0 ;;
    libwayland-client.so*|libwayland-server.so*|libwayland-egl.so*) return 0 ;;
    libudev.so*) return 0 ;;
  esac
  return 1
}
find_lib() {
  # /usr/lib/pulseaudio: libpulsecommon-<ver>.so (libpulse's RUNPATH). It is
  # staged into /usr/lib like everything else; the loader's default path finds it.
  for d in "$R/usr/lib" "$R/lib" "$R/usr/lib/pulseaudio"; do
    [ -e "$d/$1" ] && { echo "$d/$1"; return 0; }
  done
  return 1
}
needed_of() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'; }

echo "$AVC $AVU libpulse.so.0" | tr ' ' '\n' > /tmp/ffq
: > /tmp/ffseen
: > /tmp/ffnew
while [ -s /tmp/ffq ]; do
  sort -u /tmp/ffq > /tmp/ffq2; : > /tmp/ffq
  for so in $(cat /tmp/ffq2); do
    grep -qx "$so" /tmp/ffseen && continue
    echo "$so" >> /tmp/ffseen
    is_excluded "$so" && continue
    src=$(find_lib "$so") || { echo "unresolved DT_NEEDED in the dlopen closure: $so"; exit 4; }
    real=$(readlink -f "$src")
    # Already staged (GTK's closure shares X11, libstdc++, libgcc_s, ...).
    if [ ! -e "$S/usr/lib/$so" ] && [ ! -e "$S/usr/lib/firefox/$so" ]; then
      cp -L "$src" "$S/usr/lib/$so"
      echo "$so" >> /tmp/ffnew
      echo "alpine $so  <- ${real#"${R%/}"} (dlopen closure)" >> "$S/CLOSURE.txt"
    fi
    needed_of "$real" >> /tmp/ffq
  done
done
echo "dlopen closure (FFmpeg + libpulse): $(wc -l < /tmp/ffseen) sonames, $(wc -l < /tmp/ffnew) newly staged"

# libpulse client config: never try to autospawn a PulseAudio daemon (there is
# none; pipewire-pulse is started by the session's PipeWire).
mkdir -p "$S/etc/pulse"
printf '%s\n' '# LeandrOS: the server is pipewire-pulse ($XDG_RUNTIME_DIR/pulse/native).' \
  'autospawn = no' > "$S/etc/pulse/client.conf"

[ "$FIXUP" = --fixup ] || exit 0
# Same ELF fix-ups as build-in-alpine.sh, for the newly staged files only.
[ -f "$S/usr/lib/libleandros_ssp.so.1" ] || { echo "stage has no libleandros_ssp.so.1"; exit 5; }
for so in $(cat /tmp/ffnew); do
  f="$S/usr/lib/$so"
  if readelf -d "$f" | grep -q "libc.musl-$ARCH.so.1"; then
    patchelf --replace-needed "libc.musl-$ARCH.so.1" libc.so "$f"
  fi
  if readelf --dyn-syms -W "$f" | awk '$7=="UND"{print $8}' | sed 's/@.*//' | grep -qx '__stack_chk_guard'; then
    patchelf --add-needed libleandros_ssp.so.1 "$f"
  fi
  if readelf -d "$f" | grep -q 'libc\.musl'; then echo "musl soname still present: $f"; exit 5; fi
done
echo "fix-ups applied to $(wc -l < /tmp/ffnew) file(s)"
