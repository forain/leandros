#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> -- stage Alpine 3.21's prebuilt e2fsprogs
# (e2fsck, mke2fs, dumpe2fs, e2undo from e2fsprogs; resize2fs from
# e2fsprogs-extra) for LeandrOS at /usr/sbin, with their DT_NEEDED closure.
# Same shape as ports/git. Mounts (driven by ports/e2fsprogs/build.sh):
#   /src  snapshot (read-only)   /out  ports/e2fsprogs/out -> /out/<arch>.alpine/
# Emits '=== rc=N arch=A ===' as the LAST line.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  grep -q '^3\.21\.' /etc/alpine-release || { echo "want Alpine 3.21, got $(cat /etc/alpine-release)"; exit 3; }
  PKGS="e2fsprogs e2fsprogs-extra"
  if [ "$(uname -m)" = "$ARCH" ]; then
    R=""
    apk add --no-cache $PKGS binutils file patchelf build-base >/dev/null
    CC="cc"
    APK="apk"
  else
    # Foreign arch without binfmt emulation: install the target's packages
    # into a root without running them (as ports/pipewire does).
    R=/tmp/root-$ARCH
    apk add --no-cache binutils file patchelf clang lld alpine-keys >/dev/null
    mkdir -p "$R/etc/apk/keys"
    cp /usr/share/apk/keys/$ARCH/* "$R/etc/apk/keys/"
    cp /etc/apk/repositories "$R/etc/apk/"
    APK="apk --root $R --arch $ARCH"
    $APK add --initdb --no-scripts --no-cache $PKGS musl-dev >/dev/null
    CC="clang --target=$ARCH-alpine-linux-musl --sysroot=$R -fuse-ld=lld"
    echo "foreign build: container $(uname -m), target $ARCH, root $R"
  fi
  VERS=$(for p in $PKGS; do $APK info -e -v "$p"; done | tr '\n' ' ')
  echo "package: $VERS(alpine $(cat /etc/alpine-release))"

  S=/tmp/e2fs-stage-$ARCH
  rm -rf "$S"
  mkdir -p "$S/usr/sbin" "$S/usr/lib" "$S/etc"

  # -- files of the packages (binaries + libs + mke2fs.conf, no docs) ----------
  for p in $PKGS; do
    $APK info -L "$p" 2>/dev/null | grep -v -e '^$' -e 'contains:$' | while read -r f; do
      case "$f" in
        usr/share/*|etc/e2scrub*|*/e2scrub*|*/fstrim*|*/udev/*|*/cron*|*/systemd/*|lib/systemd/*|usr/lib/systemd/*) continue ;;
      esac
      [ -d "$R/$f" ] && continue
      mkdir -p "$S/$(dirname "$f")"
      cp -a "$R/$f" "$S/$f"
    done
  done
  # Alpine ships the tools in /sbin and /usr/sbin; LeandrOS wants them all in
  # /usr/sbin. Libraries stay where the package put them (found via DT_NEEDED
  # closure below and copied to /usr/lib).
  mkdir -p "$S/usr/sbin"
  for d in sbin; do
    if [ -d "$S/$d" ]; then
      for f in "$S/$d"/*; do mv "$f" "$S/usr/sbin/"; done
      rmdir "$S/$d"
    fi
  done
  # Keep only the tools the installer needs (+ mke2fs for tests).
  for f in "$S"/usr/sbin/*; do
    case "$(basename "$f")" in
      e2fsck|resize2fs|dumpe2fs|e2undo|mke2fs|debugfs|e2image|tune2fs) ;;
      *) [ -L "$f" ] && case "$(readlink "$f")" in e2fsck|mke2fs) continue ;; esac; rm -f "$f" ;;
    esac
  done
  for l in fsck.ext2 fsck.ext3 fsck.ext4; do ln -sf e2fsck "$S/usr/sbin/$l"; done
  for l in mkfs.ext2 mkfs.ext3 mkfs.ext4; do ln -sf mke2fs "$S/usr/sbin/$l"; done
  rm -rf "$S/lib" "$S/usr/lib/e2fsprogs" "$S/usr/bin"
  ls -la "$S/usr/sbin"

  # -- closure ------------------------------------------------------------------
  is_excluded() {
    case "$1" in
      libc.musl-*|ld-musl-*|libc.so) return 0 ;;
    esac
    return 1
  }
  find_lib() {
    for d in "$R/usr/lib" "$R/lib"; do
      [ -e "$d/$1" ] && { echo "$d/$1"; return 0; }
    done
    return 1
  }
  needed_of() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'; }
  is_elf() { [ -f "$1" ] && [ ! -L "$1" ] && head -c 4 "$1" | grep -q 'ELF'; }
  find "$S" -type f | while read -r f; do if is_elf "$f"; then echo "$f"; fi; done > /tmp/roots
  for f in $(cat /tmp/roots); do needed_of "$f"; done > /tmp/queue
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

  # -- ELF fix-ups (as ports/pipewire) --------------------------------------------
  $CC -shared -fPIC -fno-stack-protector -nostdlib -Wl,-soname,libleandros_ssp.so.1 \
    -o "$S/usr/lib/libleandros_ssp.so.1" /src/ssp_guard.c
  find "$S" -type f | while read -r f; do if is_elf "$f"; then echo "$f"; fi; done > /tmp/all-elves
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

  echo "$VERS alpine-$(cat /etc/alpine-release)" > "$S/E2FSPROGS-VERSION"
  echo "== staged size =="; du -sh "$S"
  echo "== closure =="; cat "$S/CLOSURE.txt"

  rm -rf "/out/$ARCH.alpine"
  mkdir -p "/out/$ARCH.alpine"
  cp -a "$S/." "/out/$ARCH.alpine/"
)
echo "=== rc=$? arch=$ARCH ==="
