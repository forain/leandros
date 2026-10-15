#!/usr/bin/env bash
# Fetch the Intel Gen9 display firmware the i915 driver can load from
# linux-firmware into $I915_FW_CACHE (default ~/.cache/leandros/i915).
#
# Only the Kaby Lake DMC (display microcontroller: DC5/DC6 power states) is
# listed: Gen9 submits through execlists, so neither GuC nor HuC is needed for
# rendering. The kernel's DMC loader is not written yet; this pins the blob
# (drivers/src/i915/hw.rs `parse_dmc` reads it, drivers/i915-tests checks the
# real file when it is present) so the image can stage it as
# /lib/firmware/i915/ once the loader lands.
#
# The file is redistributable (linux-firmware LICENSE.i915) but never
# committed here.
#
#   ports/i915_fw/fetch.sh            fetch missing files (idempotent)
#   ports/i915_fw/fetch.sh --force    re-download everything
#
# Pinned to one linux-firmware tag; each file is checked against its sha256.

set -euo pipefail

TAG="20240909"
BASE="https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain/i915"
CACHE="${I915_FW_CACHE:-$HOME/.cache/leandros/i915}"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# name sha256 (of the file at $TAG)
FILES=(
"kbl_dmc_ver1_04.bin 2cde41c3e5ad181423bcc3e98ff9c49f743c88f18646af4d0b3c3a9664b831a1"
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
        echo "i915_fw: download of $name failed" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    got="$(shasum -a 256 "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$sum" ]]; then
        echo "i915_fw: $name sha256 mismatch (got $got, want $sum)" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    mv "$tmp" "$out"
    echo "i915_fw: $name"
done
exit $fail
