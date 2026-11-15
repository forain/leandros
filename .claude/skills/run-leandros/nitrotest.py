"""Nitro-like x86_64 platform check (lane/nitrox86, 2026-11-14).
Run inside `runtests.py x86_64 <tag> --suite nitrox86`, which boots the guest
with LEANDROS_NITRO=1 (q35,pit=off,hpet=off,i8042=off, no VGA/GPU: serial
console only, as on AWS EC2 Nitro) and runs the guest tests; this script only
inspects that VM's serial log: a [TSC] line exists, the frequency came from a
stated source (never the 1 GHz last-resort guess, checked when /dev/kvm exists
because TCG states nothing), and the PIT/i8042 were seen absent.
"""
import os, sys
RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
LOG = f"/tmp/leandros{'-' + RUN_ID if RUN_ID else ''}-serial-full.log"


def main():
    log = open(LOG, "rb").read().decode("utf-8", "replace")
    ok = True
    tsc = [l[l.index("[TSC]"):] for l in log.splitlines() if "[TSC]" in l]
    print(tsc[-1] if tsc else "no [TSC] line")
    if not tsc:
        ok = False; print("FAIL: no [TSC] line")
    elif tsc[-1].startswith("[TSC] 0.000 MHz"):
        ok = False; print("FAIL: TSC frequency measured as 0 (absent PIT trusted)")
    elif "(unresolved)" in tsc[-1] and os.path.exists("/dev/kvm"):
        ok = False; print("FAIL: no stated TSC source (leaves/kvmclock) on a KVM host")
    if "no pit" not in (tsc[-1] if tsc else ""):
        ok = False; print("FAIL: PIT was not detected as absent")
    if "PS/2 controller present" in log:
        ok = False; print("FAIL: i8042 present: the machine was not started without it")
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
