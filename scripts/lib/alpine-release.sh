# alpine-release.sh -- the ONE place the Alpine release the ports build/stage in
# comes from: the image's own lock (ports/alpine/toolchain.lock, first line
# "# release vX.Y|edge", written by scripts/alpine-lock.py refresh). Sourced by
# scripts/container-lib.sh. Sets, unless already set in the environment:
#   ALPINE_REL    X.Y | edge     (3.24 | edge)
#   ALPINE_IMAGE  alpine:X.Y     (the container image tag)
#   ALPINE_REPO_BASE https://dl-cdn.alpinelinux.org/alpine/vX.Y | .../alpine/edge
# Bump the image's Alpine with `alpine-lock.py refresh`; every port follows.
if [ -z "${ALPINE_REL:-}" ]; then
  for _ar_l in "${LEANDROS_ALPINE_LOCK:-}" "${ROOT:-/nonexistent}/ports/alpine/toolchain.lock" \
      "${ROOT_DIR:-/nonexistent}/ports/alpine/toolchain.lock" \
      "${HERE:-/nonexistent}/../../ports/alpine/toolchain.lock" "${HERE:-/nonexistent}/../ports/alpine/toolchain.lock" \
      "${LEANDROS_ALPINE_RELEASE_DIR:-/nonexistent}/toolchain.lock"; do
    [ -f "$_ar_l" ] || continue
    ALPINE_REL=$(sed -n 's/^# release v\{0,1\}\([0-9][0-9]*\.[0-9][0-9]*\|edge\)$/\1/p;T;q' "$_ar_l" 2>/dev/null)
    [ -n "$ALPINE_REL" ] && break
  done
  : "${ALPINE_REL:?alpine-release.sh: cannot read the Alpine release from ports/alpine/toolchain.lock (set ALPINE_REL=X.Y)}"
fi
ALPINE_IMAGE=${ALPINE_IMAGE:-alpine:$ALPINE_REL}
if [ "$ALPINE_REL" = edge ]; then _ar_br=edge; else _ar_br=v$ALPINE_REL; fi
ALPINE_REPO_BASE=${ALPINE_REPO_BASE:-https://dl-cdn.alpinelinux.org/alpine/$_ar_br}
export ALPINE_REL ALPINE_IMAGE ALPINE_REPO_BASE
