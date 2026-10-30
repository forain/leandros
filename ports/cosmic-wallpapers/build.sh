#!/usr/bin/env bash
# Stage the cosmic-wallpapers images (data only) next to the Orion Nebula
# default, in /usr/share/backgrounds/cosmic/, where cosmic-bg / cosmic-settings
# look. The upstream checkout holds git-lfs pointers, so the originals are
# fetched from GitHub's LFS media endpoint at the pinned commit and downscaled
# to 1280x720 (same as the existing Orion default, ~0.2 MB each instead of
# 1-4 MB). The Orion default itself is never touched.
#
# Output: ~/code/leandros-artifacts/m6-session-data/shared/usr/share/backgrounds/cosmic/
# (picked up by mkfs-f2fs-populated.py's shared-tree walk). Arch independent.
# Usage: build.sh [arch]   (arch ignored)
set -euo pipefail
SRC="$HOME/code/cosmic-epoch/cosmic-wallpapers"
DST="$HOME/code/leandros-artifacts/m6-session-data/shared/usr/share/backgrounds/cosmic"
[[ -d "$SRC/original" ]] || { echo "cosmic-wallpapers: no $SRC" >&2; exit 1; }
REV=$(git -C "$SRC" rev-parse HEAD)
mkdir -p "$DST"
for f in "$SRC"/original/*.jpg; do
  n=$(basename "$f")
  [[ "$n" == orion_nebula_nasa_heic0601a.jpg ]] && continue   # keep the staged default
  [[ -f "$DST/$n" ]] && continue
  tmp=$(mktemp -d)
  curl -fsSL "https://media.githubusercontent.com/media/pop-os/cosmic-wallpapers/$REV/original/$n" -o "$tmp/$n"
  sips -Z 1280 -s format jpeg -s formatOptions 80 "$tmp/$n" --out "$DST/$n" >/dev/null
  rm -rf "$tmp"
  echo "staged $DST/$n"
done
du -ch "$DST"/*.jpg | tail -1
