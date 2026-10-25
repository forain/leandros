#!/bin/bash
# Linux <-> LeandrOS f2fs interop test. See scripts/f2fslx/interop.py header.
#   scripts/f2fslx/interop.sh lxdef lxnoi lxext home     (or: all)
# Hold the locks: export F2FSINTEROP_LOCK=$SP/lock.sh (box + qemu slots taken inside).
exec python3 "$(dirname "$0")/interop.py" "$@"
