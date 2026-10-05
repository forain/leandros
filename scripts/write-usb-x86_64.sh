#!/usr/bin/env bash
# Superseded by write-usb.sh; kept as a thin wrapper.
exec "$(dirname "${BASH_SOURCE[0]}")/write-usb.sh" x86_64 "$@"
