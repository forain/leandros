#!/usr/bin/env bash
# Fetch the Alpine appstream snapshot cosmic-store reads (names, summaries,
# icons for the apk packages of the image's Alpine branch).
#
# Same files alpine-appstream-downloader fetches in a stock Alpine:
#   https://appstream.alpinelinux.org/data/<branch>/<repo>/Components-<arch>.xml.gz
#   https://appstream.alpinelinux.org/data/<branch>/<repo>/icons-64x64.tar.gz
# for repo in main community, both arches, into
#   ports/appstream-alpine/out/<arch>/{Components-<repo>.xml.gz,icons-<repo>.tar.gz,branch}
# scripts/mkfs-f2fs-populated.py stages them as /var/lib/swcatalog/xml/
# alpinelinux-<branch>-<repo>.xml.gz and icons/alpinelinux-<branch>-<repo>/64x64/.
#
# The branch is the release the image's lock pins (ports/alpine/<arch>.lock,
# "# release vX.Y" = latest-stable at the last `alpine-lock.py refresh`).
# A fresh (< 7 days) file is kept; offline, an existing file is kept with a
# warning. Missing files with no network are an error only when none exist.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BASE="${APPSTREAM_MIRROR:-https://appstream.alpinelinux.org/data}"

case "${1:-both}" in
  aarch64) ARCHES=(aarch64) ;;
  x86_64)  ARCHES=(x86_64) ;;
  both|"") ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

branch_of() {
  local lock="$ROOT/ports/alpine/$1.lock" b=""
  [[ -f "$lock" ]] && b="$(sed -n 's/^# release \(v[0-9.]*\).*/\1/p' "$lock" | head -1)"
  if [[ -z "$b" ]]; then
    b="$(curl -fsSL --max-time 30 "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/$1/latest-releases.yaml" \
         | sed -n 's/^ *branch: *\(v[0-9.]*\).*/\1/p' | head -1 || true)"
  fi
  echo "$b"
}

rc=0
for arch in "${ARCHES[@]}"; do
  branch="$(branch_of "$arch")"
  if [[ -z "$branch" ]]; then echo "appstream-alpine: cannot determine the Alpine branch for $arch" >&2; rc=1; continue; fi
  out="$HERE/out/$arch"
  mkdir -p "$out"
  for repo in main community; do
    for pair in "Components-$arch.xml.gz:Components-$repo.xml.gz" "icons-64x64.tar.gz:icons-$repo.tar.gz"; do
      remote="${pair%%:*}"; local_name="${pair##*:}"
      dest="$out/$local_name"
      if [[ -s "$dest" && -f "$out/branch" && "$(cat "$out/branch")" == "$branch" && -z "$(find "$dest" -mtime +7 2>/dev/null)" ]]; then
        continue   # fresh
      fi
      if curl -fsSL --retry 2 --max-time 300 -o "$dest.tmp" "$BASE/$branch/$repo/$remote"; then
        mv "$dest.tmp" "$dest"
        echo "appstream-alpine: $arch $branch/$repo/$remote ($(du -h "$dest" | cut -f1))"
      else
        rm -f "$dest.tmp"
        if [[ -s "$dest" ]]; then
          echo "appstream-alpine: WARNING cannot refresh $arch $repo/$remote; keeping the existing file" >&2
        else
          echo "appstream-alpine: cannot fetch $BASE/$branch/$repo/$remote" >&2; rc=1
        fi
      fi
    done
  done
  [[ $rc -eq 0 ]] && echo "$branch" > "$out/branch"
done
exit $rc
