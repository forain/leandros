#!/usr/bin/env bash
# Pin/verify the brush sibling checkout that scripts/build-all.sh builds.
#
# brush is built straight from ../brush (see README.md), so the pin lives in
# that checkout's git history rather than in a .work tree here. This script
# keeps the two in step:
#
#   sync.sh check [dir]   the checkout is clean and its content equals
#                         upstream $PIN + patches/*.patch (exit 1 otherwise)
#   sync.sh apply [dir]   reset the checkout to $PIN and apply the patches
#                         as commits on a `leandros` branch (refuses on a
#                         dirty tree)
#   sync.sh export [dir]  regenerate patches/ from the checkout's
#                         $PIN..HEAD (after committing a change there)
#
# dir defaults to ../brush relative to the repo root.
set -euo pipefail

PIN=e46b4ae410eea5c2d52e637dcb650e6251206ee1
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
MODE="${1:-check}"
DIR="${2:-$ROOT/../brush}"

[[ -d "$DIR/.git" ]] || { echo "sync.sh: $DIR is not a git checkout" >&2; exit 2; }

# The pin is identified by its abbreviated hash in README.md; resolve the
# full one from the checkout so a typo above cannot silently pin nothing.
pin="$(git -C "$DIR" rev-parse --verify "${PIN:0:7}^{commit}")"

case "$MODE" in
  check)
    if [[ -n "$(git -C "$DIR" status --porcelain --untracked-files=no)" ]]; then
      echo "sync.sh: $DIR has uncommitted changes" >&2; exit 1
    fi
    want="$(mktemp -d)"; trap 'rm -rf "$want"' EXIT
    git -C "$DIR" worktree add -q --detach "$want/wt" "$pin"
    for p in "$HERE"/patches/*.patch; do
      git -C "$want/wt" -c user.name=sync -c user.email=sync@localhost am -q "$p"
    done
    if git -C "$DIR" diff --quiet "$(git -C "$want/wt" rev-parse HEAD)" HEAD; then
      echo "sync.sh: $DIR == $pin + $(ls "$HERE"/patches/*.patch | wc -l | tr -d ' ') patches"
      rc=0
    else
      echo "sync.sh: $DIR differs from $pin + patches/ (git diff below)" >&2
      git -C "$DIR" diff --stat "$(git -C "$want/wt" rev-parse HEAD)" HEAD >&2
      rc=1
    fi
    git -C "$DIR" worktree remove -f "$want/wt"
    exit $rc
    ;;
  apply)
    if [[ -n "$(git -C "$DIR" status --porcelain --untracked-files=no)" ]]; then
      echo "sync.sh: $DIR has uncommitted changes; commit or stash them first" >&2; exit 1
    fi
    git -C "$DIR" checkout -q -B leandros "$pin"
    for p in "$HERE"/patches/*.patch; do
      git -C "$DIR" am -q "$p"
    done
    echo "sync.sh: $DIR is now leandros = $pin + patches/"
    ;;
  export)
    rm -f "$HERE"/patches/*.patch
    git -C "$DIR" format-patch -q --no-signature -o "$HERE/patches" "$pin..HEAD"
    ls "$HERE"/patches
    ;;
  *)
    echo "usage: $0 check|apply|export [dir]" >&2; exit 2
    ;;
esac
