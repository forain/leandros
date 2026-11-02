#!/usr/bin/env python3
"""hidkeytest.py -- press and release KEY_A on the QEMU keyboard via QMP.

A `host:` step for runtests.py, run between guest commands: the guest has a
background reader on /dev/input/event* before this step and inspects its
output after. Prints HOSTKEY_SENT on success.

    LEANDROS_RUN_ID=x python3 hidkeytest.py
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

driver.qmp_inject_chord(["a"], hold_s=0.2)
print("HOSTKEY_SENT")
