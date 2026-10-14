#!/usr/bin/env bash
# claudecode-smoke.sh [aarch64|x86_64] [OUTDIR] — install and drive the Claude Code CLI in a
# fresh LeandrOS boot, headless, through .claude/skills/run-leandros/driver.py.
#
#   1. boot, root login on the serial console
#   2. /usr/sbin/claude-code-install (Alpine root /opt/alpine + the musl Bun binary), plus tmux
#   3. as root: claude --version
#   4. as root: the interactive TUI in a pty: tmux (run in the Alpine root through
#      /bin/alpine-run) runs `claude`, the screen is captured with capture-pane, keys go in
#      with send-keys: the theme picker, Down moves the selection, Enter advances to the
#      login-method screen
#   5. as root: HTTPS to api.anthropic.com: `claude -p hi` with a made-up API key must come
#      back with the server's 401 authentication_error (no credential is ever involved)
#   6. as the regular user leandro (serial login): /bin/claude is set-user-ID root and drops
#      back to leandro before the CLI runs: --version, id/HOME/whoami inside the root, the
#      TUI in a tmux pty, the 401 check, and ~/.claude written to the real /home/leandro
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
run()  { python3 "$D" run "$1" "$2" "$ARCH"; }
# One line each: the serial shell submits at every newline.
TUI="alpine-run tmux kill-server 2>/dev/null; alpine-run tmux new-session -d -s cc -x 120 -y 40 claude; sleep 30; \
alpine-run tmux capture-pane -p -t cc > /tmp/s1.\$\$.txt; alpine-run tmux send-keys -t cc Down; sleep 3; \
alpine-run tmux capture-pane -p -t cc > /tmp/s2.\$\$.txt; alpine-run tmux send-keys -t cc Enter; sleep 15; \
alpine-run tmux capture-pane -p -t cc > /tmp/s3.\$\$.txt; alpine-run tmux kill-server; \
for f in 1 2 3; do echo \"<<<SCREEN \$f>>>\"; cat /tmp/s\$f.\$\$.txt; rm -f /tmp/s\$f.\$\$.txt; done"
NET="ANTHROPIC_API_KEY=sk-ant-not-a-real-key claude -p hi --debug-file /tmp/dbg.\$\$.txt < /dev/null; \
echo claude_rc=\$?; alpine-run grep -E authentication_error /tmp/dbg.\$\$.txt | alpine-run head -3; rm -f /tmp/dbg.\$\$.txt"
check_tui() { # $1 log, $2 label
  awk '/<<<SCREEN 1>>>/{f=1} f' "$1" | grep -v '^\[' > "${1%.log}-screens.txt"
  local s1 s2 s3
  s1=$(awk '/<<<SCREEN 1>>>/{f=1;next} /<<<SCREEN 2>>>/{f=0} f' "${1%.log}-screens.txt")
  s2=$(awk '/<<<SCREEN 2>>>/{f=1;next} /<<<SCREEN 3>>>/{f=0} f' "${1%.log}-screens.txt")
  s3=$(awk '/<<<SCREEN 3>>>/{f=1;next} f' "${1%.log}-screens.txt")
  if grep -q 'Choose the text style' <<<"$s1" && grep -q '❯ ✔ Dark mode' <<<"$s1"; then ok "$2 TUI renders"; else bad "$2 TUI renders"; fi
  if grep -q '❯ *Light mode' <<<"$s2"; then ok "$2 Down key moves the selection"; else bad "$2 Down key"; fi
  if grep -q 'Select login method' <<<"$s3"; then ok "$2 Enter advances to login"; else bad "$2 Enter key"; fi
}

step "boot $ARCH"
python3 "$D" start "$ARCH" > "$OUT/boot.log" 2>&1 || { echo "boot failed"; exit 2; }
python3 "$D" login root root >> "$OUT/boot.log" 2>&1

step "install"
run "/usr/sbin/claude-code-install > /tmp/cci.log 2>&1; echo install_rc=\$?; \
/usr/sbin/alpine-sysroot /opt/alpine tmux > /dev/null 2>&1; echo tmux_rc=\$?" 1200 > "$OUT/install.log" 2>&1
grep -q 'install_rc=0' "$OUT/install.log" && grep -q 'tmux_rc=0' "$OUT/install.log" && ok install || bad install

step "root: claude --version"
run "claude --version" 300 > "$OUT/version.log" 2>&1
if grep -q '(Claude Code)' "$OUT/version.log"; then ok "root version: $(grep -o '[0-9][0-9.]* (Claude Code)' "$OUT/version.log")"; else bad "root version"; fi

step "root: TUI in a PTY (tmux)"
run "$TUI" 900 > "$OUT/tui.log" 2>&1
check_tui "$OUT/tui.log" root

step "root: HTTPS to api.anthropic.com"
run "cd /root; $NET" 900 > "$OUT/network.log" 2>&1
if grep -q '"authentication_error"' "$OUT/network.log"; then ok "root: server answered 401 authentication_error"; else bad "root network"; fi

step "leandro: set-user-ID launcher"
python3 "$D" cmd "exit" 5 > /dev/null 2>&1
python3 "$D" login leandro leandro >> "$OUT/boot.log" 2>&1
run "id; cd /home/leandro; claude --version; alpine-run sh -c 'echo inside: \$(id -u) \$(whoami) \$HOME \$(pwd)'" 300 > "$OUT/user-version.log" 2>&1
if grep -q '(Claude Code)' "$OUT/user-version.log"; then ok "leandro version"; else bad "leandro version"; fi
if grep -q 'inside: 1000 leandro /home/leandro /home/leandro' "$OUT/user-version.log"; then ok "leandro identity inside the root"; else bad "leandro identity"; fi
run "cd /home/leandro; $TUI" 900 > "$OUT/user-tui.log" 2>&1
check_tui "$OUT/user-tui.log" leandro
run "cd /home/leandro; $NET; ls -a /home/leandro" 900 > "$OUT/user-network.log" 2>&1
if grep -q '"authentication_error"' "$OUT/user-network.log"; then ok "leandro: server answered 401 authentication_error"; else bad "leandro network"; fi
if grep -q '\.claude\.json' "$OUT/user-network.log"; then ok "leandro: ~/.claude.json in the real home"; else bad "leandro home"; fi

python3 "$D" stop > /dev/null 2>&1
echo "claudecode-smoke $ARCH: $fails failure(s); logs in $OUT"
exit $((fails > 0))
