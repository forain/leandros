#!/usr/bin/env bash
# apkroot-smoke.sh [aarch64|x86_64] [OUTDIR] -- drive apk as the live-root package
# manager in a fresh LeandrOS boot, headless, through .claude/skills/run-leandros/driver.py.
#
#   1. leandros-base is installed and in /etc/apk/world
#   2. checksums of the loader and git (base files apk must never touch)
#   3. apk update (https to dl-cdn.alpinelinux.org)
#   4. apk add jq htop: rc 0, no musl / ncurses-libs / busybox pulled in
#   5. jq and htop run
#   6. apk add nano installs nothing (the base provides it)
#   7. apk del jq htop: rc 0, files gone
#   8. base checksums unchanged, git still works
#   8b. musl is Alpine's real package: loader sha256 == the pinned .apk's,
#       libc.musl-* / usr/lib/libc.so are symlinks, `apk info -e musl` finds it, world pins it, `apk audit` is clean for it, `apk add --upgrade
#       musl` is a no-op, `apk add musl-dev` works, `apk del musl` is refused
#   9. apk del leandros-base is refused by the wrapper
#  10. as the regular user: sudo apk add jq works
#
# Exit 0 iff every step passed. LEANDROS_RUN_ID defaults to apkroot.
set -u
ARCH="${1:-aarch64}"
OUT="${2:-/tmp/apkroot-smoke-$ARCH}"
cd "$(dirname "$0")/.." || exit 2
export LEANDROS_RUN_ID="${LEANDROS_RUN_ID:-apkroot}" LEANDROS_QEMU_MEM="${LEANDROS_QEMU_MEM:-4G}"
D=.claude/skills/run-leandros/driver.py
mkdir -p "$OUT"
fails=0
step() { echo "== $*"; }
ok()   { echo "PASS $1"; }
bad()  { echo "FAIL $1"; fails=$((fails + 1)); }
run()  { python3 "$D" run "$1" "$2" "$ARCH"; }
APK=/usr/sbin/apk

step "boot $ARCH"
python3 "$D" start "$ARCH" > "$OUT/boot.log" 2>&1 || { echo "boot failed"; exit 2; }
python3 "$D" login root root >> "$OUT/boot.log" 2>&1

step "1 base package"
run "/sbin/apk.static info -e leandros-base; echo info_rc=\$?; cat /etc/apk/world" 120 > "$OUT/s1.log" 2>&1
if grep -q 'info_rc=0' "$OUT/s1.log" && grep -q '^leandros-base' "$OUT/s1.log"; then ok "leandros-base installed and in world"; else bad "base package"; fi

step "2 checksums"
run "sha256sum /lib/ld-musl-$ARCH.so.1 /usr/bin/git" 120 > "$OUT/s2.log" 2>&1
grep -E '^[0-9a-f]{64}' "$OUT/s2.log" > "$OUT/sums.before"
[ "$(wc -l < "$OUT/sums.before")" -eq 2 ] && ok "checksums recorded" || bad "checksums"

step "3 apk update"
run "time $APK update; echo update_rc=\$?" 300 > "$OUT/s3.log" 2>&1
grep -q 'update_rc=0' "$OUT/s3.log" && ok "apk update" || bad "apk update"

step "4 apk add jq htop"
run "$APK add jq htop; echo add_rc=\$?" 600 > "$OUT/s4.log" 2>&1
if grep -q 'add_rc=0' "$OUT/s4.log"; then ok "add rc 0"; else bad "add rc"; fi
if grep -Eiq '(Installing|Upgrading) (musl|ncurses-libs|busybox)' "$OUT/s4.log"; then bad "add pulled in base packages"; else ok "no musl/ncurses-libs/busybox pulled"; fi

step "5 run"
run "/usr/bin/jq --version; echo '{\"a\":1}' | /usr/bin/jq .a; /usr/bin/htop --version" 120 > "$OUT/s5.log" 2>&1
grep -q 'jq-1\.[78]' "$OUT/s5.log" && ok "jq --version" || bad "jq --version"
grep -qx '1' "$OUT/s5.log" && ok "jq .a" || bad "jq .a"
grep -q 'htop 3' "$OUT/s5.log" && ok "htop --version" || bad "htop --version"

step "6 nano satisfied by base"
run "$APK add nano; echo nano_rc=\$?" 300 > "$OUT/s6.log" 2>&1
if grep -q 'nano_rc=0' "$OUT/s6.log" && ! grep -Eq 'Installing nano' "$OUT/s6.log"; then ok "nano not installed"; else bad "nano"; fi

step "7 apk del"
run "$APK del jq htop; echo del_rc=\$?; ls /usr/bin/jq /usr/lib/libonig.so.5 2>&1" 300 > "$OUT/s7.log" 2>&1
if grep -q 'del_rc=0' "$OUT/s7.log" && ! grep -q '^/usr/bin/jq$' "$OUT/s7.log" && grep -q 'No such file' "$OUT/s7.log"; then ok "del removed files"; else bad "del"; fi

step "8 base unchanged"
run "sha256sum /lib/ld-musl-$ARCH.so.1 /usr/bin/git; /usr/bin/git --version" 120 > "$OUT/s8.log" 2>&1
grep -E '^[0-9a-f]{64}' "$OUT/s8.log" > "$OUT/sums.after"
cmp -s "$OUT/sums.before" "$OUT/sums.after" && ok "checksums unchanged" || bad "checksums changed"
grep -q 'git version' "$OUT/s8.log" && ok "git works" || bad "git"

step "8b real musl package"
WANT_LD=$(PYTHONPATH=scripts python3 -c "import apkdb,hashlib,sys;print(hashlib.sha256(apkdb.apk_file(__import__('alpinepkgs').musl_apk('$ARCH'),'lib/ld-musl-$ARCH.so.1')).hexdigest())" 2>/dev/null)
run "sha256sum /lib/ld-musl-$ARCH.so.1; readlink /lib/libc.musl-$ARCH.so.1 /usr/lib/libc.so; /sbin/apk.static info -e musl; cat /etc/apk/world; echo audit_begin; /sbin/apk.static audit 2>&1; echo audit_end;$APK add --upgrade musl; echo up_rc=\$?; $APK add musl-dev; echo dev_rc=\$?; $APK del musl; echo delmusl_rc=\$?; /lib/ld-musl-$ARCH.so.1 2>&1 | head -1" 600 > "$OUT/s8b.log" 2>&1
[ -n "$WANT_LD" ] && grep -q "^$WANT_LD" "$OUT/s8b.log" && ok "loader == pinned Alpine musl" || bad "loader sha"
grep -q "^ld-musl-$ARCH.so.1" "$OUT/s8b.log" && grep -q 'lib/ld-musl-'"$ARCH"'.so.1$' "$OUT/s8b.log" && ok "symlinks" || bad "symlinks"
grep -qx 'musl' "$OUT/s8b.log" && grep -Eqx 'musl(=[0-9.]+-r[0-9]+)?' "$OUT/s8b.log" && ok "musl installed + pinned" || bad "musl info/world"
! sed -n '/audit_begin/,/audit_end/p' "$OUT/s8b.log" | grep -Eq 'lib/(ld-musl|libc.musl)' && ok "apk audit clean for musl" || bad "audit musl"
grep -q 'up_rc=0' "$OUT/s8b.log" && ! grep -Eq '(Installing|Upgrading|Downgrading) musl \('"$OUT/s8b.log" && ok "upgrade musl no-op" || bad "upgrade musl"
grep -q 'dev_rc=0' "$OUT/s8b.log" && ok "apk add musl-dev works" || bad "musl-dev"
grep -q 'delmusl_rc=1' "$OUT/s8b.log" && ok "del musl refused" || bad "del musl"
grep -q 'musl libc' "$OUT/s8b.log" && ok "loader still runs" || bad "loader run"

step "8c vanilla Alpine packages (lock)"
run "$APK info -e firefox git ripgrep dosfstools xfsprogs dbus-libs iso-codes kbd-bkeymaps; echo info_rc=\$?; $APK info -W /usr/bin/rg; echo audit_begin; $APK audit --system 2>&1; echo audit_end; $APK fix -s git ripgrep 2>&1; echo fix_rc=\$?; $APK add -s \$(cat /etc/apk/world) 2>&1; echo world_rc=\$?; /bin/rg --version | head -1; /bin/git --version; grep -c x /etc/passwd" 300 > "$OUT/s8c.log" 2>&1
grep -q 'info_rc=0' "$OUT/s8c.log" && ok "lock packages installed" || bad "apk info -e"
grep -q 'ripgrep' "$OUT/s8c.log" && ok "rg owned by ripgrep" || bad "info -W rg"
sed -n '/audit_begin/,/audit_end/p' "$OUT/s8c.log" | grep -Eq ' (usr/(bin|lib|share)|bin|sbin|lib)/' && bad "audit reports package paths" || ok "apk audit clean"
sed -n '/fix_rc/,$p;/^fix/,/fix_rc/p' "$OUT/s8c.log" | grep -Eq '(Installing|Upgrading|Reinstalling|Purging) ' && bad "fix -s changes something" || ok "apk fix -s no-op"
sed -n '/fix_rc/,/world_rc/p' "$OUT/s8c.log" | grep -Eq '(Installing|Upgrading) ' && bad "add -s world installs" || ok "apk add -s world no-op"
grep -q 'ripgrep 1' "$OUT/s8c.log" && grep -q 'git version' "$OUT/s8c.log" && ok "rg and git run" || bad "rg/git run"

step "9 wrapper refuses leandros-base"
run "$APK del leandros-base; echo refuse_rc=\$?" 60 > "$OUT/s9.log" 2>&1
grep -q 'refuse_rc=1' "$OUT/s9.log" && ok "del leandros-base refused" || bad "refuse"

step "10 sudo as leandro"
python3 "$D" cmd "exit" 5 > /dev/null 2>&1
python3 "$D" login leandro leandro >> "$OUT/boot.log" 2>&1
run "echo leandro | sudo -S -p '' $APK add jq; echo sudo_rc=\$?; /usr/bin/jq --version" 600 > "$OUT/s10.log" 2>&1
if grep -q 'sudo_rc=0' "$OUT/s10.log" && grep -q 'jq-1\.[78]' "$OUT/s10.log"; then ok "sudo apk add jq"; else bad "sudo apk add"; fi

python3 "$D" stop > /dev/null 2>&1
echo "apkroot-smoke $ARCH: $fails failure(s); logs in $OUT"
exit $((fails > 0))
