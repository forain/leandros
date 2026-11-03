#!/usr/bin/env bash
# Build libdbus-1 (the reference C client library only -- no dbus-daemon, no
# tools) for the guest, because cosmic-applet-bluetooth reaches org.bluez
# through bluer 0.17 -> dbus 0.9 -> libdbus-sys, which links libdbus-1.so.3.
# The bus itself stays busd; libdbus only speaks the wire protocol to it
# (DBUS_SYSTEM_BUS_ADDRESS points at the session busd, see start-cosmic-leandros).
#
# Installs into the m3 sysroot (so pkg-config finds dbus-1 when the applet is
# cross-built) and stages the runtime library at
#   ~/code/leandros-artifacts/libdbus/out/<arch>/libdbus-1.so.3
# which is no longer shipped: the image's libdbus-1.so.3 is Alpine's dbus-libs
# (ports/alpine); this port only provides the build-time sysroot.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail

VER=1.16.2
SHA256=0ba2a1a4b16afe7bceb2c07e9ce99a8c2c3508e5dec290dbb643384bd6beb7e2
A="$HOME/code/leandros-artifacts"
G="$A/m3-gl-stack"
TC="$A/m6-session-bins/toolchain"
W="$A/libdbus"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

mkdir -p "$W/src" "$W/logs"
tarball="$W/src/dbus-$VER.tar.xz"
if [[ ! -f "$tarball" ]]; then
  curl -fsSL -o "$tarball.part" "https://dbus.freedesktop.org/releases/dbus/dbus-$VER.tar.xz"
  mv "$tarball.part" "$tarball"
fi
got=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
if [[ "$got" != "$SHA256" ]]; then
  echo "libdbus: $tarball sha256 $got != $SHA256" >&2
  exit 1
fi
srcdir="$W/src/dbus-$VER"
[[ -d "$srcdir" ]] || tar -C "$W/src" -xf "$tarball"

for arch in "${ARCHES[@]}"; do
  sysroot="$G/sysroot-$arch"
  out="$W/out/$arch"
  stamp="$out/.built-$VER"
  if [[ -f "$stamp" && -f "$out/libdbus-1.so.3" && -f "$sysroot/usr/lib/pkgconfig/dbus-1.pc" ]]; then
    echo "libdbus $VER $arch: up to date ($out/libdbus-1.so.3)"
    continue
  fi
  cross="$W/cross-$arch.ini"
  cat > "$cross" <<EOF
[binaries]
c = ['$TC/$arch-linux-musl-cc']
cpp = ['$TC/$arch-linux-musl-c++']
ar = ['$TC/$arch-linux-musl-ar']
ranlib = ['$TC/$arch-linux-musl-ranlib']
pkg-config = ['pkg-config']

[properties]
sys_root = '$sysroot'
pkg_config_libdir = '$sysroot/usr/lib/pkgconfig'
needs_exe_wrapper = true

[host_machine]
system = 'linux'
cpu_family = '$arch'
cpu = '$arch'
endian = 'little'
EOF
  build="$W/build/$arch"
  log="$W/logs/libdbus-$arch.log"
  rm -rf "$build"
  if ! {
    meson setup "$build" "$srcdir" --cross-file "$cross" \
      --prefix=/usr --libdir=lib --buildtype=release --default-library=shared \
      -Dmessage_bus=false -Dtools=false -Dmodular_tests=disabled \
      -Ddoxygen_docs=disabled -Dducktype_docs=disabled -Dxml_docs=disabled \
      -Dqt_help=disabled -Dsystemd=disabled -Dselinux=disabled \
      -Dapparmor=disabled -Dlibaudit=disabled -Dintrusive_tests=false \
      -Dinstalled_tests=false -Dasserts=false \
      -Dchecks=false -Dverbose_mode=false -Dstats=false -Dkqueue=disabled \
      -Dinotify=disabled -Dlaunchd=disabled -Dvalgrind=disabled \
      -Dsystem_socket=/run/dbus/system_bus_socket &&
    ninja -C "$build" &&
    DESTDIR="$sysroot" ninja -C "$build" install
  } > "$log" 2>&1; then
    echo "libdbus: build failed for $arch (log: $log)" >&2
    exit 1
  fi
  mkdir -p "$out"
  install -m 0755 "$sysroot/usr/lib/libdbus-1.so.3" "$out/libdbus-1.so.3"
  touch "$stamp"
  echo "staged $arch: $out/libdbus-1.so.3"
done
