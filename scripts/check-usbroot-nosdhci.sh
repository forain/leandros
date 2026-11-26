#!/usr/bin/env bash
# Gate: a -usbroot Pi 5 kernel must contain no SDHCI driver code.
# Usage: check-usbroot-nosdhci.sh <kernel-elf> [kmod-list]
set -euo pipefail
k="${1:?kernel path}"
n="$(strings -a "$k" | grep -c '\[sdhci\]' || true)"
[[ "$n" == 0 ]] || { echo "FAIL: $n '[sdhci]' strings in $k" >&2; exit 1; }
if [[ -n "${2:-}" ]] && grep -qi sdhci "$2"; then echo "FAIL: sdhci in kmod list $2" >&2; exit 1; fi
echo "ok: no [sdhci] code in $k"
