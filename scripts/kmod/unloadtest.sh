#!/usr/bin/env bash
# unloadtest.sh [arch]: kmod unload (lane U1 subset). Needs a build with KMOD_EXTRA="hello kmbad_hang kmbad_reload virtio_rng" (U2 adds kmbad_thread kmbad_freectx virtio_net).
# hello load/unload loop, -ENOENT / -EBUSY / -EPERM, kmbad_hang Zombie, kmbad_reload stale-gate proof (kmod.leaktick=1).
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"; N="${UNLOAD_N:-50}"
RT=.claude/skills/run-leandros/runtests.py
export FFSESSION_OUT="${FFSESSION_OUT:-/tmp/ffsession-$(basename "$PWD")}"
# Guard: refuse to test a stale image or a half-written one.
if ps aux | grep "[l]eandros-kmodlower.*build-all" >/dev/null; then echo "build-all running in this worktree; wait" >&2; exit 2; fi
img=leandros-limine-$arch.img
if [ -n "$img" ]; then for e in target/kmod/$arch/*.elf; do [ -e "$e" ] && [ "$e" -nt "$img" ] && { echo "image $img older than $e; rebuild" >&2; exit 2; }; done; fi
export LEANDROS_RUN_ID="unloadtest-$arch"
M='$MD'  # guest var: the lib/modules/<id> dir whose build-id this kernel accepts (there can be several)
load() { echo "kmodctl load ${M}$1.kmod"; }
python3 "$RT" "$arch" ul1 --cmdline "kmod.leaktick=1" \
  "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; echo MD=\$MD" \
  "for i in \$(seq 1 $N); do kmodctl load $M/hello.kmod >/dev/null 2>&1 || echo LOADFAIL \$i; kmodctl rmmod hello >/dev/null 2>&1 || echo UNLOADFAIL \$i; done; echo HELLOLOOP_DONE" \
  "kmodctl rmmod hello; echo ENOENT_RC=\$?" \
  "kmodctl load $M/kmbad_hang.kmod; kmodctl rmmod kmbad_hang; echo HANGRM_RC=\$?; kmodctl load $M/kmbad_hang.kmod; echo HANGRELOAD_RC=\$?; cat /proc/modules" \
  "kmodctl load $M/kmbad_reload.kmod; sleep 1; kmodctl rmmod kmbad_reload; kmodctl load $M/kmbad_reload.kmod; sleep 1; kmodctl rmmod kmbad_reload; echo RELOAD_DONE" \
  "o=\$(kmodctl rmmod i915 2>&1); echo NOEXIT_RC=\$? OUT=\$o" \
  "kmodctl load $M/kmbad_thread.kmod; sleep 1; kmodctl rmmod kmbad_thread; echo THREADRM_RC=\$?; kmodctl load $M/kmbad_freectx.kmod; sleep 1; kmodctl rmmod kmbad_freectx; echo FREECTXRM_RC=\$?; sleep 1; echo BAD_DONE" \
  "kmodctl rmmod virtio_rng; kmodctl rmmod virtio_net; for i in \$(seq 1 $N); do kmodctl load $M/virtio_rng.kmod >/dev/null 2>&1 || echo RNGLOADFAIL \$i; kmodctl rmmod virtio_rng >/dev/null 2>&1 || echo RNGUNLOADFAIL \$i; done; echo RNGLOOP_DONE" \
  "(ping -c 100000 -i 1 192.168.105.1 >/dev/null 2>&1 &); for i in \$(seq 1 ${NET_N:-20}); do kmodctl load $M/virtio_net.kmod >/dev/null 2>&1 || echo NETLOADFAIL \$i; sleep 4; echo NETRELOAD_OK \$i; echo NETCNT \$i \$(grep eth0 /proc/net/dev);  kmodctl rmmod virtio_net >/dev/null 2>&1 || echo NETUNLOADFAIL \$i; done; echo NETLOOP_DONE" \
  "kmodctl load $M/hello.kmod; o=\$(/bin/sudo -u leandro /bin/kmodctl rmmod hello 2>&1); echo NONROOT_RC=\$? OUT=\$o; kmodctl rmmod hello" > /dev/null 2>&1
L="$FFSESSION_OUT/tests-ul1/serial-full.log"
fail=0
check() { if grep -aq -- "$1" "$L"; then echo "ok   $2"; else echo "FAIL $2"; fail=1; fi; }
nocheck() { if grep -aq -- "$1" "$L"; then echo "FAIL $2"; fail=1; else echo "ok   $2"; fi; }
check HELLOLOOP_DONE "hello loop finished"
nocheck '^\(LOADFAIL\|UNLOADFAIL\) [0-9]' "hello x$N load/unload clean"
check 'ENOENT_RC=1' "rmmod of unloaded module fails"
check 'No such file' "ENOENT reported"
check 'ZOMBIE: exit timeout' "kmbad_hang becomes Zombie"
check 'HANGRELOAD_RC=1' "zombie reload refused"
check 'kmbad_hang .* Zombie' "Zombie in /proc/modules"
check 'kmbad_reload tick ok gen' "reload fixture ticks"
check 'NOEXIT_RC=1' "no-exit/unknown rmmod fails"
check 'BAD_DONE' "kmbad_thread/kmbad_freectx run finished"
check 'kmbad_thread body stopped' "kthread saw should_stop"
check 'kmbad_freectx exit: ctx freed' "freectx exit ran"
nocheck 'panic\|PANIC\|Data abort\|Oops' "no crash in the bad fixtures"
check 'RNGLOOP_DONE' "virtio_rng x$N finished"
nocheck '^RNG\(LOAD\|UNLOAD\)FAIL' "virtio_rng x$N clean"
check 'NETLOOP_DONE' "virtio_net loop finished"
nocheck '^NET\(UNLOAD\|LOAD\)FAIL' "virtio_net reload clean"
nocheck '\[NET\] netif eth1' "reload never creates eth1 (eth0 keeps its name)"
# Counters per cycle: rx/tx packets (fields 3 and 11 after "eth0:") must rise across reloads.
grep -a '^NETCNT ' "$L" | tr -d '\r' | sed 's/eth0:/eth0: /' > "${L%.log}-netcnt.txt"
cat "${L%.log}-netcnt.txt"
python3 - "${L%.log}-netcnt.txt" <<'PY' || fail=1
import sys
rows=[l.split() for l in open(sys.argv[1])]
rx=[];tx=[]
for r in rows:
    f=r[r.index('eth0:')+1:]
    if len(f)>=10: rx.append(int(f[1])); tx.append(int(f[9]))
ok=len(rx)>=2 and all(b>a for a,b in zip(rx,rx[1:])) and all(b>a for a,b in zip(tx,tx[1:]))
print(("ok   " if ok else "FAIL ")+"eth0 rx/tx packets rise across reloads (%d cycles, rx %s..%s, tx %s..%s)"%(len(rx),rx[:1],rx[-1:],tx[:1],tx[-1:]))
sys.exit(0 if ok else 1)
PY
n=$(grep -a 'DHCP configured, address: .* (eth0)' "$L" | wc -l)
if [ "$n" -ge "${NET_N:-20}" ]; then echo "ok   eth0 DHCP + traffic after each reload ($n)"; else echo "FAIL eth0 DHCP after reloads ($n)"; fail=1; fi
check 'NONROOT_RC=[0-9]* OUT=.*delete_module: Operation not permitted (errno 1)' "non-root errno is EPERM"
nocheck 'STALE REACHED' "stale registration never reaches the new generation"
exit $fail
