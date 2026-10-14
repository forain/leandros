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
#      checks it against the sha512 npm publishes for that version;
#   3. unpacks the executable to ROOT/opt/claude-code/<version>/claude, links
#      ROOT/usr/local/bin/claude to it, and installs /usr/bin/claude (the launcher,
#      claude-code-run) in the LeandrOS root.
#
# Environment: CLAUDE_CODE_ROOT (default /opt/alpine), CLAUDE_CODE_VERSION plus
# CLAUDE_CODE_SHA512 (hex) to pin another release.
set -eu
ROOT="${CLAUDE_CODE_ROOT:-/opt/alpine}"
VER="${CLAUDE_CODE_VERSION:-2.1.296}"
ARCH=$(uname -m)
case "$ARCH" in
  aarch64) PKG=linux-arm64-musl
           SUM=73a95945592fb87207828e12cc5db2429f915b03c32cb2e35ee33a48db49911f0419c391e4584b2bb726b2b8c3702fede4008fe0de290917d10f38c2df571840 ;;
  x86_64)  PKG=linux-x64-musl
           SUM=99b159d1010db325b3983a3679673c331182016157736c35bf6acb6afbb4258776a58efed12f2925207a455f66cc9d741555d3d3d6d75add2c1cce9f81e033e7 ;;
  *) echo "claude-code-install: no Claude Code build for $ARCH" >&2; exit 1 ;;
esac
if [ -n "${CLAUDE_CODE_VERSION:-}" ]; then
  SUM="${CLAUDE_CODE_SHA512:?CLAUDE_CODE_VERSION needs CLAUDE_CODE_SHA512 (hex sha512 of the npm tarball)}"
fi
URL="https://registry.npmjs.org/@anthropic-ai/claude-code-$PKG/-/claude-code-$PKG-$VER.tgz"
say() { echo "[claude-code-install] $*"; }

say "Alpine root $ROOT"
/usr/sbin/alpine-sysroot "$ROOT" busybox bash curl ca-certificates ripgrep git

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
if [ -f /usr/sbin/claude-code-run ]; then
  cp /usr/sbin/claude-code-run /usr/bin/claude
  chmod 0755 /usr/bin/claude
fi
say "installed $VER ($PKG); run: claude --version"
