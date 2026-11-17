"""hostclean: keep host-side tests from leaking /tmp (the box's /tmp is a tmpfs; a full one
pauses guests and fails every run).

    hostclean.track(path)                       work dir of this run: always removed at exit
    hostclean.track(path, glob="/tmp/x-*")      artifact dir: removed on success; on failure
                                                shrunk (files > SMALL bytes dropped, except under
                                                failed/); stale siblings matching glob
                                                (older than STALE_S) are pruned now
    sys.exit(hostclean.run(main, argv))         runs main in try/finally, then cleans
"""
import glob
import os
import shutil
import sys
import time

SMALL = 4 << 20
STALE_S = 2 * 3600
_tracked = []


def prune(pattern, keep=0):
    """Remove dirs matching pattern older than STALE_S, newest `keep` always spared."""
    ds = sorted((d for d in glob.glob(pattern) if os.path.isdir(d)), key=os.path.getmtime, reverse=True)
    for d in ds[keep:]:
        try:
            if time.time() - os.path.getmtime(d) > STALE_S:
                shutil.rmtree(d, ignore_errors=True)
        except OSError:
            pass


def track(path, glob=None):
    _tracked.append((path, glob is None))
    if glob:
        prune(glob)


def _shrink(path):
    for root, dirs, files in os.walk(path):
        if os.path.basename(root) == "failed" or "/failed/" in root + "/":
            continue
        for f in files:
            p = os.path.join(root, f)
            try:
                if os.path.getsize(p) > SMALL:
                    os.unlink(p)
            except OSError:
                pass


def cleanup(ok):
    if "--keep" in sys.argv:
        return
    for path, always in _tracked:
        if always or ok:
            shutil.rmtree(path, ignore_errors=True)
        elif os.path.isdir(path):
            _shrink(path)
    _tracked.clear()


def run(main, *args):
    rc = 1
    try:
        rc = main(*args)
        return rc
    finally:
        cleanup(rc == 0)
