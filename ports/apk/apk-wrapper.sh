#!/bin/sh
# /usr/sbin/apk -- LeandrOS wrapper around apk.static for the live root.
# The base system is owned by the generated leandros-base package (see
# scripts/apkdb.py); removing it, or purging config, would delete the OS.
for a in "$@"; do
  case "$a" in
    leandros-base|musl|--purge)
      echo "apk: '$a' is part of the LeandrOS base image and cannot be used here" >&2
      exit 1 ;;
  esac
done
mkdir -p /lib/apk/exec /var/cache/misc /var/cache/apk
exec /usr/sbin/apk.static "$@"
