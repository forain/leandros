#!/bin/sh
# claude-code-install — install the Claude Code CLI into an Alpine root in LeandrOS.
#
# Runs in the LeandrOS root shell, as root. Installed as /usr/sbin/claude-code-install
# (ports/apk stages it next to alpine-sysroot).
#
# The npm package @anthropic-ai/claude-code carries no JavaScript any more: its
# postinstall copies the platform package's native executable, a single Bun
# (JavaScriptCore) binary linked against musl (interpreter /lib/ld-musl-<arch>.so.1,
# NEEDED libc.musl-<arch>.so.1 only). It therefore runs in an Alpine root and needs
# neither node nor npm. This script:
#   1. stages (or extends) the Alpine root with alpine-sysroot: bash (Claude Code's
#      shell), curl, CA certificates, ripgrep (its search tool), git;
#   2. downloads the linux-<arch>-musl package straight from registry.npmjs.org and
#      checks it against the sha512 npm publishes for that version (latest by default);
#   3. unpacks the executable to ROOT/opt/claude-code/<version>/claude and links
#      ROOT/usr/local/bin/claude to it.
# Any user then runs it with /bin/claude (userland/claude-run, set-user-ID root),
# which mounts /proc, /dev, /tmp and /home into the root, enters it and drops
# back to the caller's ids before starting the CLI.
#
# Environment: CLAUDE_CODE_ROOT (default /opt/alpine; /bin/claude honours it for
# root only), CLAUDE_CODE_VERSION to pin a release instead of the latest (its
# sha512 comes from the registry, or CLAUDE_CODE_SHA512 (hex) when given).
set -eu
ROOT="${CLAUDE_CODE_ROOT:-/opt/alpine}"
ARCH=$(uname -m)
case "$ARCH" in
  aarch64) PKG=linux-arm64-musl ;;
  x86_64)  PKG=linux-x64-musl ;;
  *) echo "claude-code-install: no Claude Code build for $ARCH" >&2; exit 1 ;;
esac
say() { echo "[claude-code-install] $*"; }
REG="https://registry.npmjs.org/@anthropic-ai/claude-code-$PKG"

# Latest release by default; CLAUDE_CODE_VERSION pins one (then the registry
# is asked for that version's integrity unless CLAUDE_CODE_SHA512 is given).
# The sha512 is the integrity npm publishes for the tarball, hex-encoded here.
# Alpine's curl runs inside the root: the LeandrOS image has no curl of its own.
say "Alpine root $ROOT"
/usr/sbin/alpine-sysroot "$ROOT" busybox bash curl ca-certificates ripgrep git
VER="${CLAUDE_CODE_VERSION:-latest}"
SUM="${CLAUDE_CODE_SHA512:-}"
if [ "$VER" = latest ] || [ -z "$SUM" ]; then
  META="$ROOT/tmp/claude-code-meta.json"
  say "registry lookup: $REG/$VER"
  chroot "$ROOT" /usr/bin/curl -fsSL --retry 3 -o /tmp/claude-code-meta.json "$REG/$VER"
  # the metadata is one JSON object; pick "version" and dist.integrity
  # (the LeandrOS root has no sed/tr/head: busybox of the Alpine root does the parsing)
  BB="chroot $ROOT /bin/busybox"
  RVER=$($BB tr ',' '\n' < "$META" | $BB sed -n 's/^.*"version":"\([0-9][^"]*\)".*$/\1/p' | $BB head -n 1)
  INTEG=$($BB tr ',' '\n' < "$META" | $BB sed -n 's/^.*"integrity":"sha512-\([^"]*\)".*$/\1/p' | $BB head -n 1)
  [ -n "$RVER" ] && [ -n "$INTEG" ] || { echo "claude-code-install: no version/sha512 integrity in the registry answer" >&2; exit 1; }
  [ "$VER" = latest ] || [ "$VER" = "$RVER" ] || { echo "claude-code-install: asked $VER, registry says $RVER" >&2; exit 1; }
  rm -f "$META"
  VER="$RVER"
  if [ -z "$SUM" ]; then
    SUM=$(echo "$INTEG" | $BB base64 -d | $BB od -An -tx1 -v | $BB tr -d ' \n')
  fi
fi
case "$SUM" in
  *[!0-9a-f]*|"") echo "claude-code-install: bad sha512" >&2; exit 1 ;;
esac
[ ${#SUM} -eq 128 ] || { echo "claude-code-install: sha512 must be 128 hex digits" >&2; exit 1; }
URL="$REG/-/claude-code-$PKG-$VER.tgz"


DEST="/opt/claude-code/$VER"
if [ -x "$ROOT$DEST/claude" ]; then
  say "$DEST/claude already installed"
else
  TGZ="$ROOT/tmp/claude-code-$PKG-$VER.tgz"
  say "fetch $URL"
  # Alpine's curl, inside the root: the LeandrOS image has no curl of its own.
  # It only needs DNS (alpine-sysroot copied resolv.conf) and the CA bundle.
  chroot "$ROOT" /usr/bin/curl -fL --retry 3 -o "${TGZ#"$ROOT"}" "$URL"
  echo "$SUM  ${TGZ#"$ROOT"}" > "$ROOT/tmp/claude-code.sha512"
  chroot "$ROOT" /bin/busybox sha512sum -c /tmp/claude-code.sha512
  mkdir -p "$ROOT$DEST"
  chroot "$ROOT" /bin/busybox tar -xzf "${TGZ#"$ROOT"}" -C "$DEST" --strip-components=1 package/claude
  rm -f "$TGZ" "$ROOT/tmp/claude-code.sha512"
fi
mkdir -p "$ROOT/usr/local/bin"
# Relative, so it resolves the same from inside and outside the root.
ln -sf "../../..$DEST/claude" "$ROOT/usr/local/bin/claude"
# The launcher of earlier versions of this script; /bin/claude replaces it.
rm -f /usr/bin/claude
say "installed $VER ($PKG); run: claude --version"
