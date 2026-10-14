#!/usr/bin/env bash
# claudecode-smoke.sh [aarch64|x86_64] [OUTDIR] — install and drive the Claude Code CLI in a
# fresh LeandrOS boot, headless, through .claude/skills/run-leandros/driver.py.
#
#   1. boot, root login on the serial console
#   2. /usr/sbin/claude-code-install (Alpine root /opt/alpine + the musl Bun binary)
#   3. claude --version
#   4. the interactive TUI in a PTY: tmux (in the Alpine root) runs `claude`, the screen is
#      captured with capture-pane, keys go in with send-keys: the theme picker, Down moves the
#      selection, Enter advances to the login-method screen
#   5. HTTPS to api.anthropic.com: `claude -p hi` with a made-up API key must come back with
#      the server's 401 authentication_error (no credential is ever involved)
#
# Screens and logs land in OUTDIR (default /tmp/claudecode-smoke-<arch>). Exit 0 iff every
# step passed. Uses LEANDROS_RUN_ID=${LEANDROS_RUN_ID:-ccsmoke}, so it does not touch other runs.
set -u
ARCH="${1:-aarch64}"
OUT="${2:-/tmp/claudecode-smoke-$ARCH}"
cd "$(dirname "$0")/.." || exit 2
export LEANDROS_RUN_ID="${LEANDROS_RUN_ID:-ccsmoke}" LEANDROS_QEMU_MEM="${LEANDROS_QEMU_MEM:-8G}"
D=.claude/skills/run-leandros/driver.py
mkdir -p "$OUT"
fails=0
step() { echo "== $*"; }
ok()   { echo "PASS $1"; }
bad()  { echo "FAIL $1"; fails=$((fails + 1)); }
# Run a command inside the Alpine root (the launcher has mounted /proc, /dev and /tmp).
inroot() {
  python3 "$D" run "chroot /opt/alpine /usr/bin/env -i HOME=/root TERM=xterm-256color LANG=C.UTF-8 \
SHELL=/bin/bash PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin /bin/sh -c '$1'" "$2" "$ARCH"
}

step "boot $ARCH"
python3 "$D" start "$ARCH" > "$OUT/boot.log" 2>&1 || { echo "boot failed"; exit 2; }
python3 "$D" login root root >> "$OUT/boot.log" 2>&1

step "install"
python3 "$D" run "/usr/sbin/claude-code-install > /tmp/cci.log 2>&1; echo install_rc=\$?; \
/usr/sbin/alpine-sysroot /opt/alpine tmux > /dev/null 2>&1; echo tmux_rc=\$?" 1200 "$ARCH" > "$OUT/install.log" 2>&1
grep -q 'install_rc=0' "$OUT/install.log" && grep -q 'tmux_rc=0' "$OUT/install.log" && ok install || bad install

step "claude --version"
python3 "$D" run "/usr/bin/claude --version" 300 "$ARCH" > "$OUT/version.log" 2>&1
if grep -q '(Claude Code)' "$OUT/version.log"; then ok "version: $(grep -o '[0-9][0-9.]* (Claude Code)' "$OUT/version.log")"; else bad version; fi

step "TUI in a PTY (tmux)"
# (One line each: the serial shell submits at every newline.)
inroot 'tmux kill-server 2>/dev/null; tmux new-session -d -s cc -x 120 -y 40 claude; sleep 30; '\
'tmux capture-pane -p -t cc > /tmp/s1.txt; tmux send-keys -t cc Down; sleep 3; '\
'tmux capture-pane -p -t cc > /tmp/s2.txt; tmux send-keys -t cc Enter; sleep 15; '\
'tmux capture-pane -p -t cc > /tmp/s3.txt; tmux kill-server; '\
'for f in 1 2 3; do echo "<<<SCREEN $f>>>"; cat /tmp/s$f.txt; done' 900 > "$OUT/tui.log" 2>&1
awk '/<<<SCREEN 1>>>/{f=1} f' "$OUT/tui.log" | grep -v '^\[' > "$OUT/tui-screens.txt"
s1=$(awk '/<<<SCREEN 1>>>/{f=1;next} /<<<SCREEN 2>>>/{f=0} f' "$OUT/tui-screens.txt")
s2=$(awk '/<<<SCREEN 2>>>/{f=1;next} /<<<SCREEN 3>>>/{f=0} f' "$OUT/tui-screens.txt")
s3=$(awk '/<<<SCREEN 3>>>/{f=1;next} f' "$OUT/tui-screens.txt")
if grep -q 'Choose the text style' <<<"$s1" && grep -q '❯ ✔ Dark mode' <<<"$s1"; then ok "TUI renders"; else bad "TUI renders"; fi
if grep -q '❯ *Light mode' <<<"$s2"; then ok "Down key moves the selection"; else bad "Down key"; fi
if grep -q 'Select login method' <<<"$s3"; then ok "Enter advances to login"; else bad "Enter key"; fi

step "HTTPS to api.anthropic.com"
inroot 'cd /root; ANTHROPIC_API_KEY=sk-ant-not-a-real-key timeout 300 claude -p hi --debug-file /tmp/dbg.txt < /dev/null; '\
'echo claude_rc=$?; grep -E "authentication_error" /tmp/dbg.txt | head -3' 900 > "$OUT/network.log" 2>&1
if grep -q '"authentication_error"' "$OUT/network.log"; then ok "server answered 401 authentication_error"; else bad network; fi

python3 "$D" stop > /dev/null 2>&1
echo "claudecode-smoke $ARCH: $fails failure(s); logs in $OUT"
exit $((fails > 0))
