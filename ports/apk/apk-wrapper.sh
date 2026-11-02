#!/bin/sh
# /usr/sbin/apk -- LeandrOS wrapper around apk.static for the live root.
# The base system is owned by the generated leandros-base package (see
# scripts/apkdb.py); removing it, or purging config, would delete the OS.
# musl is a real apk package (Alpine's, pinned in /etc/apk/world): add, upgrade,
# fix and `add musl-dev` work; only removing it is refused.
del=0
for a in "$@"; do
  case "$a" in
    leandros-base|--purge)
      echo "apk: '$a' is part of the LeandrOS base image and cannot be used here" >&2
      exit 1 ;;
    del) del=1 ;;
    musl)
      if [ "$del" = 1 ]; then
        echo "apk: 'musl' is the system libc and cannot be removed" >&2
        exit 1
      fi ;;
  esac
done
mkdir -p /lib/apk/exec /var/cache/misc /var/cache/apk
exec /usr/sbin/apk.static "$@"
