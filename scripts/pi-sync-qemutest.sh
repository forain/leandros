#!/bin/bash
# End-to-end check of `remote.py pi-sync` against an aarch64 QEMU guest whose
# root is made stale first (a stand-in for the Pi's old SD card): the Bluetooth
# userland, BT firmware, a /usr/share subtree and the bluetooth group are
# removed, a binary is corrupted and a mode changed.
#   scripts/pi-sync-qemutest.sh [root.img]   (default f2fs-data0-aarch64.img; after
#   ./scripts/build-all.sh --arch aarch64; hold the qemu lock around it)
# The image is cloned (cp -c) and never modified. Checks: sync applies, a second
# dry run plans 0 changes, protected files keep their sha256, passwd/group gain
# the bluetooth group without losing entries, the guest reboots fine afterwards.
set -u
OLD=${1:-f2fs-data0-aarch64.img}
export LEANDROS_RUN_ID=${LEANDROS_RUN_ID:-piuserland-a64}
export LEANDROS_NET=user LEANDROS_REMOTE=qemu
W=$(mktemp -d /tmp/pisync-test.XXXX)
cp -c "$OLD" "$W/root.img"
export LEANDROS_ROOT_IMAGE=$W/root.img
R="python3 scripts/remote.py"
D=".claude/skills/run-leandros/driver.py"
fail=0; ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; fail=1; }
cleanup() { python3 $D stop >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT

python3 $D start aarch64 > "$W/start.log" 2>&1 || { tail -5 "$W/start.log"; exit 1; }
$R wait --timeout 240 > /dev/null 2>&1 || $R status > /dev/null 2>&1 || { echo "guest not reachable"; exit 1; }

# Seed: protected files (hashes only, never contents) and some drift.
$R run 'mkdir -p /etc/leandros && echo keep-me > /etc/leandros/pisync-marker; mkdir -p /home/leandro && echo mine > /home/leandro/pisync-marker; cat /bin/ps >> /bin/ps; chmod 700 /bin/ping; rm -f /bin/btd /bin/btctl /bin/bthid /lib/firmware/brcm/BCM4345C0.hcd; rm -rf /usr/share/X11/xkb/symbols' > /dev/null 2>&1
$R run 'cat /etc/group' 2>/dev/null | awk -F: '$1!="bluetooth"' > "$W/group.stale"
$R push "$W/group.stale" /etc/group > /dev/null 2>&1
$R run 'cat /etc/group' 2>/dev/null | grep -q '^bluetooth:' && bad "seed: bluetooth group still present"
$R run 'ls /bin/btd' 2>/dev/null | grep -q btd && bad "seed: /bin/btd still present"
before=$($R run 'sha256sum /etc/shadow /etc/leandros/pisync-marker /home/leandro/pisync-marker 2>/dev/null' 2>/dev/null | awk '{print $1}' | tr '\n' ' ')
groups0=$($R run 'cat /etc/group' 2>/dev/null | wc -l)

$R pi-sync --board '' --dry-run 2>&1 | tail -n 3 | tee "$W/dry1"
grep -q 'bytes' "$W/dry1" && ok "dry run plans changes" || bad "dry run"
$R pi-sync --board '' 2>&1 | tail -n 4 | tee "$W/apply"
grep -q 'change(s) applied' "$W/apply" && ! grep -q FAILED "$W/apply" && ok "sync applied" || bad "sync"
$R pi-sync --board '' --dry-run 2>&1 | tail -n 2 | tee "$W/dry2"
grep -q 'up to date: 0 bytes uploaded' "$W/dry2" && ok "idempotent: second run plans 0 bytes" || bad "not idempotent"

after=$($R run 'sha256sum /etc/shadow /etc/leandros/pisync-marker /home/leandro/pisync-marker 2>/dev/null' 2>/dev/null | awk '{print $1}' | tr '\n' ' ')
[ -n "$before" ] && [ "$before" = "$after" ] && ok "shadow + /etc/leandros + /home untouched" || bad "protected files changed"
g=$($R run 'cat /etc/group' 2>/dev/null)
groups1=$(echo "$g" | wc -l)
echo "$g" | awk -F: '$1=="bluetooth"' | grep -q leandro && ok "bluetooth group with leandro" || bad "bluetooth group"
[ "$groups1" -ge "$groups0" ] && ok "group lines $groups0 -> $groups1 (none lost)" || bad "group lines lost"
l=$(sha256sum userland/target/aarch64-unknown-none/release/btd | awk '{print $1}')
r=$($R run 'sha256sum /bin/btd' 2>/dev/null | awk '{print $1}')
[ "$l" = "$r" ] && ok "/bin/btd matches the build" || bad "/bin/btd"
$R run 'stat -c %a /bin/ping' 2>/dev/null | grep -qx 755 && ok "mode drift fixed" || bad "mode of /bin/ping"

$R reboot --timeout 300 > "$W/reboot.log" 2>&1 || { tail -n 3 "$W/reboot.log"; }
n=$($R run 'ls /bin/btd /bin/btctl /lib/firmware/brcm/BCM4345C0.hcd /usr/share/X11/xkb/symbols/us' 2>&1 | tee "$W/ls.log" | grep -c '^/')
[ "$n" = 4 ] && ok "guest reboots on the synced root" || { bad "after reboot ($n/4)"; tail -n 4 "$W/ls.log"; }
exit $fail
