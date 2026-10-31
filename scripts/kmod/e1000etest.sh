#!/usr/bin/env bash
# e1000etest.sh: x86_64 e1000e unload/reload x10 with traffic on QEMU's 82574L (LEANDROS_NIC=e1000e).
# Needs KMOD_EXTRA="hello ..." build and an untracked .config with CONFIG_MODULES=y CONFIG_E1000E=m (never commit it).
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"
RT=.claude/skills/run-leandros/runtests.py
export FFSESSION_OUT="${FFSESSION_OUT:-/tmp/ffsession-$(basename "$PWD")}"
if ps aux | grep "[b]uild-all.sh" | grep -q "$PWD" >/dev/null; then echo "build-all running in this worktree; wait" >&2; exit 2; fi
export LEANDROS_RUN_ID="e1000etest-x86_64"
export LEANDROS_NIC=e1000e
M='$MD'
N="${E1000E_N:-10}"
python3 "$RT" x86_64 e1k \
  "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; echo MD=\$MD; grep eth /proc/net/dev; cat /proc/modules | grep e1000e" \
  "(ping -c 100000 -i 0.5 10.0.2.2 >/dev/null 2>&1 &); for i in \$(seq 1 $N); do kmodctl rmmod e1000e >/dev/null 2>&1 || echo E1KUNLOADFAIL \$i; kmodctl load $M/e1000e.kmod >/dev/null 2>&1 || echo E1KLOADFAIL \$i; sleep 5; echo E1KCNT \$i \$(grep eth0 /proc/net/dev); done; echo E1KLOOP_DONE; cat /proc/modules | grep e1000e" > /dev/null 2>&1
L="$FFSESSION_OUT/tests-e1k/serial-full.log"
fail=0
check() { if grep -aq -- "$1" "$L"; then echo "ok   $2"; else echo "FAIL $2"; fail=1; fi; }
nocheck() { if grep -aq -- "$1" "$L"; then echo "FAIL $2"; fail=1; else echo "ok   $2"; fi; }
check E1KLOOP_DONE "e1000e loop finished"
nocheck '^E1K\(UNLOAD\|LOAD\)FAIL' "e1000e x$N unload/reload clean (no Zombie / EBUSY)"
nocheck 'e1000e ZOMBIE' "e1000e never Zombie"
nocheck 'e1000e .* Zombie' "no Zombie in /proc/modules"
python3 - "$L" <<'PY' || fail=1
import re,sys
t=open(sys.argv[1],errors='replace').read()
c=re.findall(r'^E1KCNT (\d+) eth0: *(\d+) +(\d+) .*?(?:\s)(\d+) +(\d+) ',t,re.M)
rx=[int(x[2]) for x in c]
ok=len(rx)>=2 and rx[-1]>rx[0]
print(("ok   " if ok else "FAIL ")+"eth0 rx packets rise across %d reloads (%s..%s)"%(len(rx),rx[:1],rx[-1:]))
sys.exit(0 if ok else 1)
PY
nocheck 'panic\|PANIC\|Oops' "no crash"
exit $fail
