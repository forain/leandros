#!/bin/sh
# alpine-parity.sh [aarch64|x86_64 ...] — fail if the pinned Alpine musl's
# exported dynsym drifts from the golden list. Host-only. See alpine-parity.py.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
[ $# -gt 0 ] || set -- aarch64 x86_64
rc=0
for a in "$@"; do python3 -I "$HERE/alpine-parity.py" "$a" || rc=1; done
exit $rc
