#!/bin/bash
# vtloop.sh [N] [OUTDIR]: with a live desktop session, switch Ctrl+Alt+F2 / F1 N times,
# screenshot after each return, print pixel diff vs a pre-switch shot and /proc/vtstat.
# Run from the repo root, e.g. ffsession.py aarch64 t --nofirefox --gpu virgl --hostpost "sh .claude/skills/run-leandros/vtloop.sh 10"
N=${1:-10}; O=${2:-/tmp/vtloop-shots}; mkdir -p "$O"; rm -f "$O"/*.ppm
D=.claude/skills/run-leandros
python3 $D/driver.py screenshot $O/s0.ppm 2>&1 | tail -n 1
for i in $(seq 1 $N); do
  python3 $D/driver.py chord ctrl alt f2 >/dev/null 2>&1; sleep 4
  python3 $D/driver.py screenshot $O/f2-$i.ppm >/dev/null 2>&1
  python3 $D/driver.py chord ctrl alt f1 >/dev/null 2>&1; sleep 6
  python3 $D/driver.py screenshot $O/f1-$i.ppm >/dev/null 2>&1
done
python3 $D/driver.py cmd "grep -E 'stale_presents|vt_restores|vt_handoff' /proc/vtstat" 2>&1 | grep -E "stale|restores|handoff"
python3 - "$O" <<'PY'
import glob,sys,re
O=sys.argv[1]
def rd(p):
    d=open(p,'rb').read(); m=re.match(rb'P6\s+(\d+)\s+(\d+)\s+(\d+)\s',d); return d[m.end():]
b=rd(O+'/s0.ppm')
for f in sorted(glob.glob(O+'/*.ppm')):
    x=rd(f); n=min(len(x),len(b))
    diff=sum(1 for i in range(0,n,3*7) if x[i:i+3]!=b[i:i+3])
    nz=sum(1 for i in range(0,n,3*37) if x[i:i+3]!=b'\0\0\0')
    print(f.split('/')[-1],'diff_vs_s0_pct %.2f'%(100*diff/(n/21)),'nonblack %.1f%%'%(100*nz/(n/111)))
PY
