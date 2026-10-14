#!/bin/sh
# claude — run the Claude Code CLI from the Alpine root claude-code-install made.
#
# Installed as /usr/bin/claude by claude-code-install. Enters ROOT (default
# /opt/alpine) with /proc, /dev and /tmp mounted (Bun reads /proc/self/exe and
# /proc/self/maps; the terminal is a /dev/pts node) and the current directory
# carried over when it exists inside ROOT, then runs ROOT/usr/local/bin/claude.
set -eu
ROOT="${CLAUDE_CODE_ROOT:-/opt/alpine}"
if [ ! -x "$ROOT/usr/local/bin/claude" ]; then
  echo "claude: not installed in $ROOT; run claude-code-install as root" >&2
  exit 127
fi
# (No grep in the LeandrOS root: read /proc/mounts with the shell.)
mounted() {
  while read -r _dev _mnt _rest; do [ "$_mnt" = "$1" ] && return 0; done < /proc/mounts
  return 1
}
mkdir -p "$ROOT/proc" "$ROOT/dev" "$ROOT/root" "$ROOT/tmp"
mounted "$ROOT/proc" || mount -t proc proc "$ROOT/proc"
mounted "$ROOT/dev" || mount --bind /dev "$ROOT/dev"
# The system /tmp (tmpfs): AF_UNIX sockets (tmux, Bun's IPC) can only be bound
# on tmpfs, and Claude Code keeps its scratch files there.
mounted "$ROOT/tmp" || mount --bind /tmp "$ROOT/tmp"
WD=/root
case "$PWD" in
  "$ROOT"/*) WD="${PWD#"$ROOT"}" ;;
esac
exec chroot "$ROOT" /usr/bin/env -i \
  HOME=/root USER=root LOGNAME=root SHELL=/bin/bash \
  PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  TERM="${TERM:-xterm-256color}" COLORTERM="${COLORTERM:-}" LANG=C.UTF-8 \
  ${COLUMNS:+COLUMNS="$COLUMNS"} ${LINES:+LINES="$LINES"} \
  /bin/sh -c 'cd "$1" 2>/dev/null || cd /root; shift; exec /usr/local/bin/claude "$@"' sh "$WD" "$@"
