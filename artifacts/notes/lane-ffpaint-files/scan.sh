#!/bin/bash
# per-run summary: freeze, WebRender OOM/reset, kernel failure traces
for d in /tmp/ffpaint-runs*/run-*; do
  [ -f $d/steps.json ] || continue
  t=$(basename $d); t=${t#run-}
  fr=$(python3 -c "import json;s=json.load(open('$d/steps.json'));print(('FROZE@%s'%s.get('freeze_at_s')) if s.get('frozen') else 'ok')" 2>/dev/null)
  echo "$t $fr wrOOM=$(grep -a -c 'wr_renderer_render: OutOfMemory (' $d/ff.log 2>/dev/null) reset=$(grep -a -c 'Detect DeviceReset' $d/ff.log 2>/dev/null) shmOOM=$(grep -a -c 'mmap failed' $d/ff.log 2>/dev/null) ffpfail=$(grep -a -c FFPFAIL $d/serial.log 2>/dev/null) enomem=$(grep -a -c '\[ENOMEM\]' $d/serial.log 2>/dev/null) late=$(grep -a -c LATEWAKE $d/serial.log 2>/dev/null) crash=$(grep -a -c 'exited on signal' $d/ff.log 2>/dev/null)"
done
