"""Retry helper for host-side Python fetches (see scripts/lib/netretry.sh).

Retries only transient failures: DNS resolution, connect failure, timeouts,
HTTP 429/5xx.  Never retries 404 or other 4xx; callers verify checksums
after the fetch.  Bounded attempts with 1,2,4,8 s backoff.
"""
import socket
import sys
import time
import urllib.error

MAX_ATTEMPTS = 5
SLEEP = time.sleep


def is_transient(exc):
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code == 429 or 500 <= exc.code <= 599
    if isinstance(exc, urllib.error.URLError):
        return isinstance(exc.reason, (socket.gaierror, socket.timeout, ConnectionError, TimeoutError))
    return isinstance(exc, (socket.gaierror, socket.timeout, TimeoutError, ConnectionError))


def retry(fn, what="fetch"):
    delay = 1
    for attempt in range(1, MAX_ATTEMPTS + 1):
        try:
            return fn()
        except Exception as e:  # noqa: BLE001
            if attempt >= MAX_ATTEMPTS or not is_transient(e):
                raise
            print("netretry: %s failed (%s), retry %d/%d in %ds"
                  % (what, e, attempt, MAX_ATTEMPTS - 1, delay), file=sys.stderr)
            SLEEP(delay)
            delay *= 2


def curl_retry(args):
    """subprocess curl with retry on exit 6/7/28 (HTTP status not visible here)."""
    import subprocess
    delay = 1
    for attempt in range(1, MAX_ATTEMPTS + 1):
        rc = subprocess.run(["curl"] + list(args)).returncode
        if rc == 0:
            return 0
        if rc not in (6, 7, 28) or attempt >= MAX_ATTEMPTS:
            return rc
        print("netretry: curl failed (rc=%d), retry %d/%d in %ds"
              % (rc, attempt, MAX_ATTEMPTS - 1, delay), file=sys.stderr)
        SLEEP(delay)
        delay *= 2
