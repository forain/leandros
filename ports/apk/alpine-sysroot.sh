#!/bin/sh
# alpine-sysroot [--arch ARCH] ROOT [PKG...]
#
# Stage (or extend) an Alpine 3.21 root at ROOT with the image's static
# apk-tools, never touching LeandrOS's own /. Installed as
# /usr/sbin/alpine-sysroot by ports/apk; scripts/container-lib.sh's local
# backend uses it to build the root that replaces the alpine:3.21 container
# when a port is built inside LeandrOS.
#
#   alpine-sysroot /var/lib/alp busybox make
#   chroot /var/lib/alp /bin/busybox ...       # Alpine's own musl: runs as-is
#
# The first call creates the database (--initdb) and seeds ROOT/etc/apk with
# the image's keys (/etc/apk/keys) and repositories (/etc/apk/repositories, or
# ALPINE_REPO=<base> for <base>/main + <base>/community). Later calls just
# `apk add` more. Native architecture only: apk would happily install foreign
# packages, but nothing in the guest could run them.
#
# Why not plain `apk.static add`: without --root it would install Alpine's musl
# over the guest's /lib/ld-musl-<arch>.so.1 (the image's libc.so), so the image
# deliberately has no `apk` command, only apk.static and this wrapper.
set -eu
APK="${APK:-/usr/sbin/apk.static}"
ARCH=$(uname -m)
if [ "${1:-}" = --arch ]; then
  [ "$2" = "$ARCH" ] || {
    echo "alpine-sysroot: wanted $2, this machine is $ARCH: in-guest Alpine roots are native only" >&2
    exit 3; }
  shift 2
fi
ROOT="${1:?usage: alpine-sysroot [--arch ARCH] ROOT [PKG...]}"
shift
case "$ROOT" in
  /*) ;;
  *) ROOT="$(pwd)/$ROOT" ;;
esac
ROOT=${ROOT%/}
[ -n "$ROOT" ] || { echo "alpine-sysroot: refusing to install into /" >&2; exit 2; }
[ -x "$APK" ] || { echo "alpine-sysroot: no $APK" >&2; exit 1; }

mkdir -p "$ROOT/etc/apk/keys" "$ROOT/tmp" "$ROOT/proc" "$ROOT/dev"
for k in /etc/apk/keys/*.pub; do
  [ -f "$ROOT/etc/apk/keys/${k##*/}" ] || cp "$k" "$ROOT/etc/apk/keys/"
done
if [ -n "${ALPINE_REPO:-}" ]; then
  printf '%s/main\n%s/community\n' "$ALPINE_REPO" "$ALPINE_REPO" > "$ROOT/etc/apk/repositories"
elif [ ! -s "$ROOT/etc/apk/repositories" ]; then
  cp /etc/apk/repositories "$ROOT/etc/apk/repositories"
fi
# For whatever later runs chroot'ed in here (DNS, TLS).
cp /etc/resolv.conf "$ROOT/etc/resolv.conf" 2>/dev/null || true

INIT=""
[ -f "$ROOT/lib/apk/db/installed" ] || INIT=--initdb
# shellcheck disable=SC2086
exec "$APK" --root "$ROOT" $INIT --arch "$ARCH" --no-progress ${APK_FLAGS:-} add "$@"
