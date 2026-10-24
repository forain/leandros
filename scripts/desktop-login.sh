#!/usr/bin/env bash
# Log a user into the COSMIC desktop of a LeandrOS machine over SSH, with no
# one at the keyboard, and wait until the session's compositor and panel run.
#
#   LEANDROS_REMOTE=<ip> scripts/desktop-login.sh CREDENTIALS_FILE [--wait SECS]
#
# CREDENTIALS_FILE holds `user=NAME` and `pass=PASSWORD` lines (or just the
# password on its first line; the user then defaults to leandro). The password
# goes from that file straight into the guest's /bin/greetd-login on stdin; it
# is never put on a command line, printed or logged. greetd-login speaks
# greetd's IPC to the running greeter (docs/development/remote.md, "Starting
# the desktop remotely"). The binary is pushed first if the guest lacks it or
# has an older one.
#
# Exit status: 0 when cosmic-comp and cosmic-panel run as the user, 1 otherwise.
# Remember the screen-lit rule: COSMIC blanks after ~15 min without input, so
# measure within 15 minutes of this login.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="python3 $ROOT/scripts/remote.py"
creds="${1:?usage: desktop-login.sh CREDENTIALS_FILE [--wait SECS]}"
wait_s=90
[[ "${2:-}" == "--wait" ]] && wait_s="${3:?--wait needs seconds}"

user="$(sed -n 's/^user=//p' "$creds" | head -1)"
user="${user:-leandro}"
if ! grep -q '^pass=' "$creds"; then
    pwsrc() { head -1 "$creds"; }
else
    pwsrc() { sed -n 's/^pass=//p' "$creds" | head -1; }
fi

arch="$($REMOTE run 'uname -m' | tr -d '\r')"
case "$arch" in
    x86_64|aarch64) ;;
    *) echo "desktop-login: unexpected guest arch '$arch'" >&2; exit 1 ;;
esac
bin="$ROOT/userland/greetdlogin/target/$arch-unknown-linux-musl/release/greetd-login"
[[ -x "$bin" ]] || "$ROOT/userland/greetdlogin/build.sh" "$arch" >/dev/null
$REMOTE deploy-files "$bin:/bin/greetd-login:755" >/dev/null

uid="$($REMOTE run "while IFS=: read n x u rest; do [ \"\$n\" = $user ] && echo \$u; done < /etc/passwd" | tr -d '\r')"
[[ -n "$uid" ]] || { echo "desktop-login: no account $user" >&2; exit 1; }

# Session processes of the user already up? Then there is nothing to do.
probe="for p in /proc/[0-9]*; do c=\$(cat \$p/comm 2>/dev/null); case \$c in cosmic-comp|cosmic-panel) \
while read k v rest; do [ \"\$k\" = Uid: ] && [ \"\$v\" = $uid ] && echo \$c; done < \$p/status;; esac; done"
have() { $REMOTE run "$probe" 2>/dev/null | tr -d '\r' | sort -u | tr '\n' ' '; }
if [[ "$(have)" == *cosmic-comp*cosmic-panel* ]]; then
    echo "desktop-login: $user already has a session (cosmic-comp + cosmic-panel)"
    exit 0
fi

pwsrc | $REMOTE run "/bin/greetd-login --user $user"

for ((i = 0; i < wait_s; i += 3)); do
    sleep 3
    if [[ "$(have)" == *cosmic-comp*cosmic-panel* ]]; then
        echo "desktop-login: session up after ~$((i + 3)) s (cosmic-comp + cosmic-panel as uid $uid)"
        exit 0
    fi
done
echo "desktop-login: no cosmic-comp + cosmic-panel as uid $uid after $wait_s s (seen: $(have))" >&2
exit 1
