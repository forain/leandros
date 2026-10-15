#!/bin/sh
# build-in-alpine.sh <x86_64|aarch64> -- stage Alpine 3.21's prebuilt git for
# LeandrOS, with the small set of command-line tools git and sshd lean on:
#
#   git                  git 2.47 (+ git-remote-https over libcurl/OpenSSL)
#   less                 git's default pager
#   nano                 an editor, so `git commit` without -m works
#   openssh-sftp-server  /usr/lib/ssh/sftp-server, the sftp subsystem that
#                        /usr/sbin/sshd (userland/sshd) execs as the user;
#                        OpenSSH's scp uses SFTP by default since 9.0
#   ncurses-terminfo-base  /etc/terminfo for less/nano (the image had none)
#
# Runs INSIDE Alpine 3.21 of the TARGET architecture (a podman/docker
# container on the host, the apk.static chroot inside LeandrOS), exactly like
# ports/pipewire. Mounts (driven by ports/git/build.sh):
#   /src           ports/git snapshot (read-only)
#   /out           ports/git/out; this writes /out/<arch>.alpine/
# Symlinks inside the staged tree are kept as symlinks; mkfs packs every
# symlink to a staged file as a hard link to it (one inode, no copy).
# Emits '=== rc=N arch=A ===' as the LAST line.
ARCH="$1"
case "$ARCH" in
  aarch64|x86_64) ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  grep -q '^3\.21\.' /etc/alpine-release || { echo "want Alpine 3.21, got $(cat /etc/alpine-release)"; exit 3; }
  PKGS="git less nano openssh-sftp-server ncurses-terminfo-base"
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

  S=/tmp/git-stage-$ARCH
  rm -rf "$S"
  mkdir -p "$S/usr/bin" "$S/usr/lib/ssh" "$S/usr/libexec" "$S/usr/share" "$S/etc"

  # -- files of the packages (no docs, man pages, locales or completion) -------
  # cp -a keeps git's ~150 libexec symlinks to ../../bin/git as symlinks.
  for p in git less nano openssh-sftp-server ncurses-terminfo-base; do
    $APK info -L "$p" 2>/dev/null | grep -v -e '^$' -e 'contains:$' | while read -r f; do
      case "$f" in
        usr/share/doc/*|usr/share/man/*|usr/share/locale/*|usr/share/bash-completion/*) continue ;;
        usr/share/nano/*) continue ;;   # syntax files: nano runs without them
      esac
      mkdir -p "$S/$(dirname "$f")"
      cp -a "$R/$f" "$S/$f"
    done
  done
  # `git init`'s templates (hooks samples, info/exclude) belong to the
  # git-init-template subpackage, which git pulls in; without them every
  # `git init` warns "templates not found".
  if [ -d "$R/usr/share/git-core" ]; then
    mkdir -p "$S/usr/share"
    cp -a "$R/usr/share/git-core" "$S/usr/share/"
  fi
  # git's own symlinks under /usr/bin, which `apk info -L` lists as files too
  ls -la "$S/usr/bin" | grep -- '->' || true

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

  echo "$VERS alpine-$(cat /etc/alpine-release)" > "$S/GIT-VERSION"
  echo "== staged size =="; du -sh "$S"
  echo "== closure =="; cat "$S/CLOSURE.txt"

  rm -rf "/out/$ARCH.alpine"
  mkdir -p "/out/$ARCH.alpine"
  cp -a "$S/." "/out/$ARCH.alpine/"
)
echo "=== rc=$? arch=$ARCH ==="
