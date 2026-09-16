#!/usr/bin/env python3
"""Type a string on the guest's virtio-keyboard through QMP (one press per
character, ~0.3 s apart), then Enter. Honours LEANDROS_RUN_ID like driver.py.
usage: LEANDROS_RUN_ID=iced python3 typekeys.py <text> [--no-enter]"""
import os, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "..", "..", ".claude", "skills", "run-leandros"))
import driver  # noqa: E402

QCODE = {c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
QCODE.update({" ": "spc", "-": "minus", ".": "dot", "/": "slash", "_": ("shift", "minus")})

text = sys.argv[1]
for ch in text:
    q = QCODE[ch]
    driver.qmp_inject_chord(list(q) if isinstance(q, tuple) else [q], hold_s=0.08)
    time.sleep(0.25)
if "--no-enter" not in sys.argv:
    driver.qmp_inject_chord(["ret"], hold_s=0.08)
print(f"typed {text!r}")
