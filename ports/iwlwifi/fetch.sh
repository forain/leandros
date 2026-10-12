#!/usr/bin/env bash
# Fetch the Intel Wi-Fi firmware the kernel's iwlwifi driver loads
# (drivers/src/iwlwifi.rs) from linux-firmware into $IWLWIFI_CACHE (default
# ~/.cache/leandros/iwlwifi). scripts/mkfs-f2fs-populated.py stages whatever
# is there as /lib/firmware/iwlwifi-*.ucode on the x86_64 root image.
#
# The files are redistributable (linux-firmware LICENCE.iwlwifi_firmware) but
# never committed here.
#
#   ports/iwlwifi/fetch.sh            fetch missing files (idempotent)
#   ports/iwlwifi/fetch.sh --force    re-download everything
#
# Pinned to one linux-firmware tag; each file is checked against its sha256.
#
# The driver's behavioural reference, Linux drivers/net/wireless/intel/iwlwifi,
# was read at linux-6.6 (cdn.kernel.org linux-6.6.tar.xz, sha256
# d926a06c63dd8ac7df3f86ee1ffc2ce2a3b81a2d168484e76b5b389aba8e56d0); it is not
# needed to build.

set -euo pipefail

TAG="20240909"
BASE="https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain"
CACHE="${IWLWIFI_CACHE:-$HOME/.cache/leandros/iwlwifi}"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# name sha256 (of the file at $TAG)
FILES=(
"iwlwifi-8265-36.ucode d7d84c9b6a3b1976acb97a2bec7102e24a6b5d794110c0dcbd107ab5d9877e5f"
)

mkdir -p "$CACHE"
fail=0
for entry in "${FILES[@]}"; do
    name="${entry%% *}"
    sum="${entry##* }"
    out="$CACHE/$name"
    if [[ $FORCE == 0 && -f "$out" ]] && [[ "$(shasum -a 256 "$out" | cut -d' ' -f1)" == "$sum" ]]; then
        continue
    fi
    tmp="$out.part"
    if ! curl -sfL --retry 3 -o "$tmp" "$BASE/$name?h=$TAG"; then
        echo "iwlwifi: download of $name failed" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    got="$(shasum -a 256 "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$sum" ]]; then
        echo "iwlwifi: $name sha256 mismatch ($got)" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    mv "$tmp" "$out"
    echo "iwlwifi: $name ($(wc -c < "$out" | tr -d ' ') bytes)"
done
echo "iwlwifi: $(ls "$CACHE"/iwlwifi-*.ucode 2>/dev/null | wc -l | tr -d ' ') files in $CACHE"
exit $fail
