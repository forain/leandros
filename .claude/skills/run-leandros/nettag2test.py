#!/usr/bin/env python3
"""nettag2test.py -- blocking socket I/O and unbound/ICMP sockets park narrow.

Runs `/bin/nettag2` in the running VM (same LEANDROS_RUN_ID). For a blocking
recv() on TCP, on UDP (bound and wildcard), on an unbound UDP socket and on a
ping socket, an EPOLLET waiter and a poll() waiter on unbound UDP/ICMP sockets,
and a blocking connect() whose SYN cannot leave, the guest checks by itself
that the waiter parks with a mask that is not all ones, that 1 s of unrelated
AF_UNIX/pipe traffic wakes it 0 times (the traffic includes a pipe whose key
shares the waiter's bloom bit, so a bits-only park fails on every arch), and that the real event (data, FIN,
bind + datagram, echo reply, shutdown) returns it within 20 ms. It also
times 20 loopback connects accepted and 20 refused (each within 20 ms).
Exit status 0 iff every check passed (connect_pending may SKIP without a NIC).

    LEANDROS_RUN_ID=x python3 nettag2test.py
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

CASES = ("tcp_recv", "udp_recv", "udpany_recv", "unbound_recv", "unbound_epollet",
         "icmp_recv", "icmp_unbound", "connect_lo")


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    r = driver.serial_run("/bin/nettag2", timeout=240, arch=arch) or {}
    out = r.get("output", "")
    print(out.strip())
    ok = (r.get("rc") == 0 and "FAIL" not in out
          and all(f"{c}: PASS" in out for c in CASES)
          and ("connect_pending: PASS" in out or "connect_pending: SKIP" in out))
    print(f"nettag2test: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
