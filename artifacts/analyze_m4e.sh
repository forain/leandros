#!/bin/bash
S=~/code/leandros-artifacts/notes/m4-screenshots/m4d-exit-aarch64-hvf-serial.log
echo "===== EXEC (pid -> binary) ====="
grep "EXEC p=" "$S" | sort -u
echo "===== UXTR (connect/accept) ====="
grep "UXTR" "$S"
echo "===== LSN (listener probe: pid fd lsid psid) ====="
grep "LSN" "$S" | sort | uniq -c | tail -30
echo "===== last PARK block (grouped) ====="
grep "PARK p=" "$S" | tail -25
