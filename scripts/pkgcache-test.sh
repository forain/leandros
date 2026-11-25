#!/usr/bin/env bash
# Unit test for the shared package cache: net_apk cache/offline logic, net_fetch_verified,
# leandros_alpine_stage retry/keep-log/final error, leandros_port_srchash lock sensitivity.
# Fake apk/wget; no container, no network.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/apkcache" "$T/dist"
export PATH="$T/bin:$PATH" NET_RETRY_SLEEP=true TMPDIR="$T" NET_RETRY_MAX=3
export LEANDROS_APKCACHE_DIR="$T/apkcache" LEANDROS_DISTFILES_DIR="$T/dist"
fail=0; ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fail=1; }
# fake apk: logs its argv; fails with a DNS error unless --no-network, when FAKE_NET=down
cat > "$T/bin/apk" <<'F'
#!/bin/sh
echo "$*" >> "$APKLOG"
case " $* " in *" --no-network "*) exit 0 ;; esac
if [ "${FAKE_NET:-up}" = down ]; then echo "ERROR: DNS: transient error (try again later)" >&2; exit 99; fi
exit 0
F
cat > "$T/bin/wget" <<'F'
#!/bin/sh
echo x >> "$WGETLOG"; while [ $# -gt 0 ]; do [ "$1" = -O ] && { printf '%s' "$FAKE_BODY" > "$2"; }; shift; done; exit 0
F
chmod +x "$T/bin/apk" "$T/bin/wget"
export APKLOG="$T/apk.log" WGETLOG="$T/wget.log"
. "$HERE/lib/netretry.sh"
: > "$APKLOG"; net_apk add --no-cache foo >/dev/null 2>&1
grep -q -- "--cache-dir $T/apkcache/$(uname -m) --cache-max-age 86400 add foo" "$APKLOG" && ! grep -q -- "--no-cache" "$APKLOG" && ok "add: --no-cache replaced by --cache-dir" || bad "add rewrite: $(cat "$APKLOG")"
: > "$APKLOG"; net_apk --root /r --arch aarch64 add --initdb --no-scripts --no-cache foo >/dev/null 2>&1
grep -q -- "--cache-dir $T/apkcache/aarch64 " "$APKLOG" && ok "foreign --arch picks its own cache dir" || bad "arch dir: $(cat "$APKLOG")"
: > "$APKLOG"; net_apk info -e foo >/dev/null 2>&1
[ "$(cat "$APKLOG")" = "info -e foo" ] && ok "info untouched" || bad "info: $(cat "$APKLOG")"
: > "$APKLOG"; LEANDROS_PKGCACHE_OFF=1 net_apk add --no-cache foo >/dev/null 2>&1
[ "$(cat "$APKLOG")" = "add --no-cache foo" ] && ok "PKGCACHE_OFF=1 = old behaviour" || bad "off: $(cat "$APKLOG")"
mkdir -p "$T/apkcache/$(uname -m)"
: > "$APKLOG"; FAKE_NET=down net_apk add --no-cache foo >/dev/null 2>&1; rc=$?
[ "$rc" = 0 ] && grep -q -- "--no-network" "$APKLOG" && fail_idx=$(grep -c . "$APKLOG") && ok "net down, no cached index: still tried" || true
touch "$T/apkcache/$(uname -m)/APKINDEX.abcd.tar.gz"
: > "$APKLOG"; FAKE_NET=down net_apk add --no-cache foo >/dev/null 2>&1; rc=$?
[ "$rc" = 0 ] && tail -1 "$APKLOG" | grep -q -- "--no-network" && ok "net down + cached index: falls back to --no-network" || bad "fallback rc=$rc $(cat "$APKLOG")"
: > "$APKLOG"; LEANDROS_OFFLINE=1 FAKE_NET=down net_apk add --no-cache foo >/dev/null 2>&1; rc=$?
[ "$rc" = 0 ] && [ "$(grep -c . "$APKLOG")" = 1 ] && grep -q -- "--no-network" "$APKLOG" && ok "LEANDROS_OFFLINE=1: one offline call, no retries" || bad "offline rc=$rc $(cat "$APKLOG")"
# under `set -e` (every build-in-alpine.sh) a failing apk must still print its error
: > "$APKLOG"; out=$(FAKE_NET=down LEANDROS_PKGCACHE_OFF=1 bash -c 'set -e; . '"$HERE"'/lib/netretry.sh; net_shim; apk add --no-cache zz; echo not-reached' 2>&1); rc=$?
[ "$rc" = 99 ] && echo "$out" | grep -q "DNS: transient error" && ! echo "$out" | grep -q not-reached && ok "set -e: failing apk prints its error and exits with its rc" || bad "set -e rc=$rc out=$out"
# net_fetch_verified
body=hello; sha=$(printf '%s' "$body" | sha256sum | cut -d' ' -f1); export FAKE_BODY=$body
: > "$WGETLOG"; net_fetch_verified http://x/f "$T/o1" "$sha" 2>/dev/null; [ "$(cat "$T/o1")" = hello ] && [ -s "$T/dist/$sha" ] && ok "fetch verified + stored in distfiles" || bad "fetch store"
rm -f "$T/o1"; : > "$WGETLOG"; net_fetch_verified http://x/f "$T/o1" "$sha" 2>/dev/null; [ ! -s "$WGETLOG" ] && [ -s "$T/o1" ] && ok "second fetch: from cache, no download" || bad "fetch cached"
: > "$WGETLOG"; net_fetch_verified http://x/f "$T/o1" "$sha" 2>/dev/null; [ ! -s "$WGETLOG" ] && ok "already-verified OUT reused" || bad "reuse out"
printf corrupt > "$T/dist/$sha"; rm -f "$T/o1"; : > "$WGETLOG"; net_fetch_verified http://x/f "$T/o1" "$sha" 2>/dev/null; [ -s "$WGETLOG" ] && [ "$(cat "$T/o1")" = hello ] && ok "corrupt cache entry is not trusted, refetched" || bad "corrupt cache"
FAKE_BODY=evil net_fetch_verified http://x/f "$T/o2" "$(printf nope | sha256sum | cut -d' ' -f1)" 2>"$T/e"; rc=$?
[ "$rc" = 1 ] && [ ! -e "$T/o2" ] && grep -q "sha256 mismatch" "$T/e" && ok "mismatching download refused" || bad "mismatch rc=$rc"
LEANDROS_OFFLINE=1 net_fetch_verified http://x/f "$T/o3" "$(printf nope | sha256sum | cut -d' ' -f1)" 2>"$T/e"; rc=$?
[ "$rc" = 1 ] && grep -q "distfile cache" "$T/e" && ok "offline + uncached: clear error" || bad "offline fetch rc=$rc"
# leandros_alpine_stage with a fake leandros_alpine_run
export ROOT="$T/root"; mkdir -p "$ROOT/ports/alpine" "$ROOT/ports/p1" "$ROOT/scripts/lib"
cp "$HERE/lib/alpine-release.sh" "$ROOT/scripts/lib/"; printf '# release v3.24\n' > "$ROOT/ports/alpine/toolchain.lock"
. "$HERE/container-lib.sh"
sleep() { :; }
FAKECALLS=0
leandros_alpine_run() { FAKECALLS=$((FAKECALLS+1)); if [ "$FAKECALLS" -ge "${OKAT:-99}" ]; then echo "=== rc=0 arch=x ==="; else echo "container start lost"; echo "=== rc=52 arch=x ==="; fi; return 0; }
L="$T/st.log"; FAKECALLS=0 OKAT=3; leandros_alpine_stage "$L" -- x 2>/dev/null; rc=$?
[ "$rc" = 0 ] && [ "$FAKECALLS" = 3 ] && [ -f "$L.try1" ] && [ -f "$L.try2" ] && ok "stage: retried twice, success, failed logs kept" || bad "stage retry rc=$rc calls=$FAKECALLS"
FAKECALLS=0 OKAT=99; leandros_alpine_stage "$L" -- x 2>"$T/e"; rc=$?
[ "$rc" = 1 ] && [ "$FAKECALLS" = 3 ] && grep -q "staging step failed after 3" "$T/e" && grep -q "rc=52" "$T/e" && ok "stage: bounded, final error shows the log" || bad "stage bound rc=$rc calls=$FAKECALLS"
leandros_alpine_run() { FAKECALLS=$((FAKECALLS+1)); echo "ERROR: unable to select packages: nosuch"; echo "=== rc=99 arch=x ==="; return 0; }
FAKECALLS=0; leandros_alpine_stage "$L" -- x 2>/dev/null; [ "$FAKECALLS" = 1 ] && ok "stage: deterministic apk error not retried" || bad "stage det calls=$FAKECALLS"
# the GPU-stack ports stage through the helper too (no bare leandros_alpine_run with a swallowed rc)
for f in ports/mesa/build-gpu-stack.sh ports/nvk-tests/build.sh; do
  grep -q '^ *leandros_alpine_stage ' "$HERE/../$f" && ! grep -qE '^ *leandros_alpine_run ' "$HERE/../$f" && ok "$f uses leandros_alpine_stage" || bad "$f bypasses leandros_alpine_stage"
done
# srchash: lock sensitivity (Alpine-staged ports only)
mkdir -p "$ROOT/ports/p2"; echo a > "$ROOT/ports/p1/f"; echo a > "$ROOT/ports/p2/f"; touch "$ROOT/ports/p1/build-in-alpine.sh"
h1=$(leandros_port_srchash "$ROOT/ports/p1"); h2=$(leandros_port_srchash "$ROOT/ports/p2")
printf '# release v3.25\n' > "$ROOT/ports/alpine/toolchain.lock"
h1b=$(leandros_port_srchash "$ROOT/ports/p1"); h2b=$(leandros_port_srchash "$ROOT/ports/p2")
[ "$h1" != "$h1b" ] && [ "$h2" = "$h2b" ] && ok "srchash: lock change invalidates Alpine ports only" || bad "srchash $h1 $h1b $h2 $h2b"
exit $fail
