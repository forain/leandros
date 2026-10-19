#!/usr/bin/env python3
"""listentagtest.py -- a TCP listener's pollers park on the listener's key.

Runs `/bin/listentag` in the running VM (same LEANDROS_RUN_ID). The guest
checks by itself that an EPOLLET acceptor, a poll() and a select() waiter on a
127.0.0.1 listener park with a mask that is not all ones, that 1 s of
unrelated AF_UNIX/pipe traffic wakes them 0 times, that 50 connects with idle
gaps of 25-150 ms are each accepted within 20 ms, and that a connect returns
the poll() and select() waiters within 20 ms. It also lists sshd's sleeping
threads and their masks (informational). Exit status 0 iff every check passed.

    LEANDROS_RUN_ID=x python3 listentagtest.py
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

CASES = ("epollet_quiet", "epollet_accept", "poll_listener", "select_listener")


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    r = driver.serial_run("/bin/listentag", timeout=180, arch=arch) or {}
    out = r.get("output", "")
    print(out.strip())
    ok = (r.get("rc") == 0 and "FAIL" not in out
          and all(f"{c}: PASS" in out for c in CASES))
    print(f"listentagtest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
