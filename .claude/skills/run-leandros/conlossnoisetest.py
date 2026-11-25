#!/usr/bin/env python3
"""conlossnoisetest.py -- conlosstest with kernel diagnostic lines printed during the flood
(lane x86splice). A background loop of fsyncs (the long [fsync] summary line) and forks
([FORK] lines) runs next to /bin/conloss; the same lossless + no-splice checks apply.
Reports how many kernel lines landed inside the flood (a run with none proves nothing)."""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402
import conlosstest  # noqa: E402

_orig = driver.serial_run
NOISE = ("(i=0; while [ $i -lt 400 ]; do dd if=/dev/zero of=/home/.cn bs=4k count=1 conv=fsync 2>/dev/null; "
         "/bin/true; i=$((i+1)); done; rm -f /home/.cn) &")


def run(cmd, *a, **k):
    if cmd.startswith("/bin/conloss"):
        cmd = f"{NOISE} {cmd}; wait"
    return _orig(cmd, *a, **k)


driver.serial_run = run
_seg = conlosstest.segment


def seg(buf):
    s = _seg(buf)
    if s:
        k = len(re.findall(rb"(?m)^\[ *\d+\.\d+\] ", s))
        print(f"  kernel lines inside the flood: {k}")
    return s


conlosstest.segment = seg
sys.exit(conlosstest.main())
