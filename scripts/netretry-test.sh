#!/usr/bin/env bash
# Unit test for scripts/lib/netretry.sh and scripts/netretry.py using fake curl/wget/git.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir "$T/bin"; export PATH="$T/bin:$PATH" NET_RETRY_SLEEP=true TMPDIR="$T"
# fake tool: FAKE_SEQ="6 6 0" = exit codes per call; FAKE_HTTP = status written to -D file on rc 22
for tool in curl wget git apk pip; do
cat > "$T/bin/$tool" <<'F'
#!/bin/sh
n=$(cat "$FAKE_CNT" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$FAKE_CNT"
rc=$(echo $FAKE_SEQ | cut -d' ' -f$n); [ -z "$rc" ] && rc=$(echo $FAKE_SEQ | awk '{print $NF}')
while [ $# -gt 0 ]; do [ "$1" = -D ] && printf 'HTTP/1.1 %s X\r\n' "${FAKE_HTTP:-200}" > "$2"; shift; done
[ -n "${FAKE_ERR:-}" ] && [ "$rc" != 0 ] && echo "$FAKE_ERR" >&2
exit $rc
F
chmod +x "$T/bin/$tool"; done
. "$HERE/lib/netretry.sh"
export FAKE_CNT="$T/cnt"; fail=0
t() { # name fn seq http err want_rc want_calls
  rm -f "$FAKE_CNT"; export FAKE_SEQ="$3" FAKE_HTTP="$4" FAKE_ERR="$5"
  $2 x >/dev/null 2>&1; rc=$?; calls=$(cat "$FAKE_CNT")
  if [ "$rc" = "$6" ] && [ "$calls" = "$7" ]; then echo "ok   $1"; else echo "FAIL $1: rc=$rc calls=$calls want $6/$7"; fail=1; fi
}
t "curl dns x2 then ok"      net_curl "6 6 0" 200 "" 0 3
t "curl connect then ok"     net_curl "7 0" 200 "" 0 2
t "curl timeout then ok"     net_curl "28 0" 200 "" 0 2
t "curl 503 then ok"         net_curl "22 0" 503 "" 0 2
t "curl 429 then ok"         net_curl "22 0" 429 "" 0 2
t "curl 404 no retry"        net_curl "22 0" 404 "" 22 1
t "curl 403 no retry"        net_curl "22 0" 403 "" 22 1
t "curl rc35 no retry"       net_curl "35 0" 200 "" 35 1
t "curl always dns: bounded" net_curl "6" 200 "" 6 5
t "curl success: one call"   net_curl "0" 200 "" 0 1
t "wget rc4 then ok"         net_wget "4 0" 200 "" 0 2
t "wget rc8 no retry"        net_wget "8 0" 200 "" 8 1
t "git dns then ok"          net_git "128 0" 200 "fatal: unable to access 'u': Could not resolve host: github.com" 0 2
t "git auth no retry"        net_git "128 0" 200 "fatal: Authentication failed" 128 1
t "git 503 then ok"          net_git "128 0" 200 "fatal: unable to access: The requested URL returned error: 503" 0 2
t "apk dns then ok"          net_apk "99 0" 200 "WARNING: fetching https://x/APKINDEX.tar.gz: DNS: transient error (try again later)" 0 2
t "apk temp error then ok"   net_apk "99 0" 200 "ERROR: https://x: temporary error (try again later)" 0 2
t "apk network error: bound" net_apk "99" 200 "ERROR: network error (check Internet connection and firewall)" 99 5
t "apk unsatisfiable: no retry" net_apk "99 0" 200 "ERROR: unable to select packages: foo (no such package)" 99 1
t "apk unsatisfiable2: no retry" net_apk "99 0" 200 "ERROR: unsatisfiable constraints: bar-1.0: breaks: world[bar=2]" 99 1
t "apk bad signature: no retry" net_apk "99 0" 200 "ERROR: UNTRUSTED signature" 99 1
t "apk success: one call"    net_apk "0" 200 "" 0 1
t "pip dns then ok"          net_pip "1 0" 200 "WARNING: Retrying: Failed to establish a new connection: [Errno -3] Temporary failure in name resolution" 0 2
t "pip timeout then ok"      net_pip "1 0" 200 "ReadTimeoutError: HTTPSConnectionPool: Read timed out." 0 2
t "pip no dist: no retry"    net_pip "1 0" 200 "ERROR: No matching distribution found for nosuchpkg" 1 1
t "pip 404: no retry"        net_pip "1 0" 200 "ERROR: HTTP error 404 while getting https://x" 1 1
python3 -I - "$HERE" <<'P' || fail=1
import sys, urllib.error, socket
sys.path.insert(0, sys.argv[1])
import netretry as n
n.SLEEP = lambda s: None
def mk(seq):
    c = {"n": 0}
    def f():
        e = seq[min(c["n"], len(seq) - 1)]; c["n"] += 1
        if e: raise e
        return "ok"
    return f, c
dns = urllib.error.URLError(socket.gaierror(-2, "Name or service not known"))
f, c = mk([dns, dns, None]); assert n.retry(f) == "ok" and c["n"] == 3
nf = urllib.error.HTTPError("u", 404, "nf", {}, None)
f, c = mk([nf, None])
try: n.retry(f); assert False
except urllib.error.HTTPError: assert c["n"] == 1
s5 = urllib.error.HTTPError("u", 503, "x", {}, None)
f, c = mk([s5]);
try: n.retry(f); assert False
except urllib.error.HTTPError: assert c["n"] == 5
print("ok   python retry")
P
[ $fail = 0 ] && echo "netretry-test: PASS" || { echo "netretry-test: FAIL"; exit 1; }
