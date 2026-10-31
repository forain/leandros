#!/usr/bin/env bash
# unloadtest.sh [arch]: kmod unload (lane U1 subset). Needs a build with KMOD_EXTRA="hello kmbad_hang kmbad_reload virtio_rng" (virtio_rng has no exit).
# hello load/unload loop, -ENOENT / -EBUSY / -EPERM, kmbad_hang Zombie, kmbad_reload stale-gate proof (kmod.leaktick=1).
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"; N="${UNLOAD_N:-50}"
RT=.claude/skills/run-leandros/runtests.py
export LEANDROS_RUN_ID="unloadtest-$arch"
M='$MD'  # guest var: the lib/modules/<id> dir whose build-id this kernel accepts (there can be several)
load() { echo "kmodctl load ${M}$1.kmod"; }
python3 "$RT" "$arch" ul1 --cmdline "kmod.leaktick=1" \
  "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; echo MD=\$MD" \
  "for i in \$(seq 1 $N); do kmodctl load $M/hello.kmod >/dev/null 2>&1 || echo LOADFAIL \$i; kmodctl rmmod hello >/dev/null 2>&1 || echo UNLOADFAIL \$i; done; echo HELLOLOOP_DONE" \
  "kmodctl rmmod hello; echo ENOENT_RC=\$?" \
  "kmodctl load $M/kmbad_hang.kmod; kmodctl rmmod kmbad_hang; echo HANGRM_RC=\$?; kmodctl load $M/kmbad_hang.kmod; echo HANGRELOAD_RC=\$?; cat /proc/modules" \
  "kmodctl load $M/kmbad_reload.kmod; sleep 1; kmodctl rmmod kmbad_reload; kmodctl load $M/kmbad_reload.kmod; sleep 1; kmodctl rmmod kmbad_reload; echo RELOAD_DONE" \
  "kmodctl load $M/virtio_rng.kmod; o=\$(kmodctl rmmod virtio_rng 2>&1); echo NOEXIT_RC=\$? OUT=\$o; grep virtio_rng /proc/modules" \
  "kmodctl load $M/hello.kmod; o=\$(/bin/sudo -u leandro /bin/kmodctl rmmod hello 2>&1); echo NONROOT_RC=\$? OUT=\$o; kmodctl rmmod hello" > /dev/null 2>&1
L=/tmp/ffsession/tests-ul1/serial-full.log
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
check 'NOEXIT_RC=1 OUT=.*delete_module: Resource busy (errno 16)' "no-exit rmmod returns EBUSY"
check 'virtio_rng .* Live' "no-exit module still Live"
check 'NONROOT_RC=[0-9]* OUT=.*delete_module: Operation not permitted (errno 1)' "non-root errno is EPERM"
nocheck 'STALE REACHED' "stale registration never reaches the new generation"
exit $fail
