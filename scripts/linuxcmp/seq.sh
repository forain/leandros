#!/bin/bash
# deploy -> boot Linux once -> collect -> verify good -> cleanup. Runs under lock.sh laptop.
D="${LINUXCMP_DIR:-${TMPDIR:-/tmp}/linuxcmp}"; mkdir -p "$D"
cd "$(dirname "$0")/../.."; export LEANDROS_REMOTE="${LEANDROS_REMOTE:-172.16.149.179}"
L=$D/seq.log; log(){ echo "$(date +%T) $*" >> $L; }
R(){ timeout ${T:-90} python3 scripts/remote.py "$@" 2>&1 | sed -E 's/((wifi|iwlwifi)\.psk=)[^ ]*/\1<r>/g'; }
[ -e /private/tmp/claude-501/-Users-forain-code-leandros/3a5d881b-73e3-4b64-8356-b3e09ebd0877/scratchpad/tkiprekey.active ] && { log "tkiprekey active, releasing lock"; exit 9; }
log "lock acquired"; log "pre: $(R run 'uname -v' | tail -1)"
R run 'espctl status' | grep -E '^(booted|trial=|good=|default|armed)' | tr '\n' ' ' >> $L; echo >> $L
pkill -f "scripts/server.py $D/results" ; (/usr/bin/python3 -I $D/scripts/server.py $D/results 9470 >> $D/results/server.err 2>&1 &); sleep 1
T=200 R push $D/root/boot/vmlinuz-lts /tmp/lcmp-vmlinuz | tail -1 >> $L
T=200 R push $D/out/initrd.gz /tmp/lcmp-initrd.gz | tail -1 >> $L
R push $D/out/entry.txt /tmp/lcmp-entry | tail -1 >> $L
R deploy-files $D/out/espctl-lcmp:/tmp/espctl-lcmp:755 | tail -1 >> $L
OUT=$(T=200 R run 'set -e; espctl put /tmp/lcmp-vmlinuz /linuxcmp/vmlinuz; espctl put /tmp/lcmp-initrd.gz /linuxcmp/initrd.gz; espctl get /linuxcmp/vmlinuz /tmp/v1; espctl get /linuxcmp/initrd.gz /tmp/v2; [ "$(sha256sum < /tmp/v1)" = "$(sha256sum < /tmp/lcmp-vmlinuz)" ]; [ "$(sha256sum < /tmp/v2)" = "$(sha256sum < /tmp/lcmp-initrd.gz)" ]; /tmp/espctl-lcmp oneshot /tmp/lcmp-entry /linuxcmp/good.conf; echo STAGED_OK')
echo "$OUT" | tail -5 >> $L
echo "$OUT" | grep -q STAGED_OK || { log "ABORT: staging failed"; R run '/tmp/espctl-lcmp oneshot-restore /linuxcmp/good.conf' | tail -2 >> $L; exit 2; }
GOODSHA=$(echo "$OUT" | sed -n 's/.*sha256=\([0-9a-f]*\).*/\1/p' | tail -1)
CONF=$(R run 'espctl cat /limine.conf | cut -c1-26 | head -n 60')
echo "$CONF" >> $L
L5=$(echo "$CONF" | sed -n 5p); L10=$(echo "$CONF" | sed -n 10p)
if [ "$L5" != "default_entry: 1" ] || [ "$L10" != "/Linux linuxcmp" ] || ! echo "$CONF" | grep -q '^/LeandrOS good' || [ -z "$GOODSHA" ]; then
  log "ABORT: conf structure check failed; restoring"; R run '/tmp/espctl-lcmp oneshot-restore /linuxcmp/good.conf' | tail -2 >> $L; exit 3; fi
log "conf switched (good sha $GOODSHA)"
log "rebooting into Linux"; T=20 R run 'sync; reboot' > /dev/null; S=$(date +%s)
back=0
while [ $(( $(date +%s) - S )) -lt 720 ]; do
  sleep 20
  [ $(( $(date +%s) - S )) -lt 90 ] && continue
  V=$(T=25 R run 'uname -v' | tail -1); case "$V" in "#1 "*) back=1; break;; esac
done
log "elapsed $(( $(date +%s) - S )) s back=$back uname=$V"
[ $back = 1 ] || { log "FAILURE: laptop not back within 12 min"; exit 4; }
R run 'espctl status' | grep -E '^(booted|trial=|good=|default|armed)' | tr '\n' ' ' >> $L; echo >> $L
mkdir -p $D/results/esp
for f in result.txt dmesg.txt scan.txt; do R run "espctl get /linuxcmp/$f /tmp/lcmp-$f" > /dev/null; T=60 R pull /tmp/lcmp-$f $D/results/esp/ | tail -1 >> $L; done
OUT=$(R run 'echo CONFSHA $(espctl cat /limine.conf | sha256sum); for f in vmlinuz initrd.gz good.conf result.txt dmesg.txt scan.txt; do espctl rm /linuxcmp/$f; done; espctl rm /linuxcmp; rm -f /tmp/lcmp-* /tmp/espctl-lcmp /tmp/v1 /tmp/v2 /tmp/v3 /tmp/v4; espctl ls /; echo CLEANED')
echo "$OUT" | tail -12 >> $L
echo "$OUT" | grep -q "CONFSHA $GOODSHA" && log "limine.conf == pre-trial good conf" || log "WARN limine.conf differs from pre-trial (another lane? check)"
log "done"
