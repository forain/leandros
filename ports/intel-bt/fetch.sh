#!/usr/bin/env bash
# Fetch the Intel Bluetooth firmware the kernel's btusb driver loads
# (drivers/src/bt/usb.rs + btintel.rs: the 8265's bootloader image and its
# DDC parameters) from linux-firmware into $INTEL_BT_CACHE (default
# ~/.cache/leandros/intel-bt). scripts/mkfs-f2fs-populated.py stages whatever
# is there as /lib/firmware/intel/ibt-* on the x86_64 root image.
#
# The files are redistributable (linux-firmware LICENCE.ibt_firmware) but
# never committed here.
#
#   ports/intel-bt/fetch.sh            fetch missing files (idempotent)
#   ports/intel-bt/fetch.sh --force    re-download everything

sha256_of() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }  # shasum is not on every Linux host

set -euo pipefail

TAG="20240909"
BASE="https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain"
CACHE="${INTEL_BT_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/leandros/intel-bt}"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# name sha256 (of the file at $TAG)
FILES=(
"ibt-12-16.sfi 75b8b8cc10abd616a96066bce9ce62be80ebc293d6a4e9a0a11c3ecc5d3ca373"
"ibt-12-16.ddc 3ca252ad20e8f76d6d906536bf9e5abe6654c657183584a48e90a0dcf40ac4d5"
)

mkdir -p "$CACHE"
fail=0
for entry in "${FILES[@]}"; do
    name="${entry%% *}"
    sum="${entry##* }"
    out="$CACHE/$name"
    if [[ $FORCE == 0 && -f "$out" ]] && [[ "$(sha256_of "$out" | cut -d' ' -f1)" == "$sum" ]]; then
        continue
    fi
    tmp="$out.part"
    if ! curl -sfL --retry 3 -o "$tmp" "$BASE/intel/$name?h=$TAG"; then
        echo "intel-bt: download of $name failed" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    got="$(sha256_of "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$sum" ]]; then
        echo "intel-bt: $name sha256 mismatch ($got)" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    mv "$tmp" "$out"
    echo "intel-bt: $name ($(wc -c < "$out" | tr -d ' ') bytes)"
done
echo "intel-bt: $(ls "$CACHE"/ibt-* 2>/dev/null | wc -l | tr -d ' ') files in $CACHE"
exit $fail
