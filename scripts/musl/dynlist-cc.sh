#!/bin/sh
# Link step for musl's libc.so that honours -Wl,--dynamic-list=FILE.
#
# `zig cc` rejects --dynamic-list ("unsupported linker arg"), which is why
# musl's ./configure dropped it and our libc.so ended up linked with every
# internal call going through the PLT. zig's linker is ld.lld, which does
# support it, so: strip the flag, let zig cc do the link while printing its
# exact ld.lld command line (ZIG_VERBOSE_LINK=1), then re-run that ld.lld line
# with --dynamic-list appended.
#
# Usage: DYNLIST_REAL_CC=<arch>-linux-musl-cc dynlist-cc.sh <cc args...>
set -e
: "${DYNLIST_REAL_CC:?set DYNLIST_REAL_CC to the real compiler wrapper}"

dynlist=""
args=""
for a in "$@"; do
  case "$a" in
    -Wl,--dynamic-list=*) dynlist="${a#-Wl,--dynamic-list=}" ;;
    *) args="$args $a" ;;
  esac
done

if [ -z "$dynlist" ]; then
  # shellcheck disable=SC2086
  exec "$DYNLIST_REAL_CC" $args
fi

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
# shellcheck disable=SC2086
if ! ZIG_VERBOSE_LINK=1 "$DYNLIST_REAL_CC" $args 2>"$log"; then
  cat "$log" >&2
  exit 1
fi
line="$(grep '^ld\.lld ' "$log" | tail -n 1)"
if [ -z "$line" ]; then
  echo "dynlist-cc: zig did not print its ld.lld command" >&2
  cat "$log" >&2
  exit 1
fi
grep -v '^ld\.lld ' "$log" >&2 || true
# The printed line is unquoted; every path in it is space-free (checked by the
# caller), so word splitting reproduces the argv exactly.
# shellcheck disable=SC2086
exec zig ${line} --dynamic-list="$dynlist"
