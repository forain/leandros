"""ffsdiag: run ffsession.py under a budget and print failure-only diagnostics.

run() kills the whole ffsession process group (and stops the driver's QEMU) when the
budget is exceeded, so a host-timeout in runtests cannot orphan them or lose the log.
"""
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
KEY = re.compile(r"ready after|NOT ready|sh: |driver (start|login|stop) rc|boot failed|"
                 r"login failed|Traceback|Error|Timeout")
ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-_]")


def _run_argv(argv, budget, stop_driver=True):
    p = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         text=True, errors="replace", start_new_session=True)
    lines = []

    def reader():
        for l in p.stdout:
            l = l.rstrip("\n")
            lines.append(l)
            print(l, flush=True)

    t = threading.Thread(target=reader, daemon=True)
    t.start()
    t0 = time.time()
    try:
        p.wait(timeout=budget)
        rc = p.returncode
    except subprocess.TimeoutExpired:
        rc = None
        print(f"DIAG ffsession killed after {time.time() - t0:.0f}s (budget {budget:.0f}s)",
              flush=True)
        for sig, wait in ((signal.SIGTERM, 5), (signal.SIGKILL, 5)):
            try:
                os.killpg(p.pid, sig)
            except OSError:
                break
            try:
                p.wait(timeout=wait)
                break
            except subprocess.TimeoutExpired:
                pass
        if stop_driver:
            try:
                subprocess.run([sys.executable, os.path.join(HERE, "driver.py"), "stop"],
                               capture_output=True, timeout=120)
            except Exception:
                pass
    t.join(timeout=5)
    return rc, lines, time.time() - t0


def run(arch, tag, args, budget):
    argv = [sys.executable, os.path.join(HERE, "ffsession.py"), arch, tag, *args]
    return _run_argv(argv, budget)


def _tail(path, n):
    with open(path, errors="replace") as f:
        ls = [ANSI.sub("", l).rstrip() for l in f.read().splitlines()]
    return [l[:160] for l in ls if l.strip()][-n:]


def diag(lines, out_dir, ok):
    if ok:
        import re
        t = {}
        for l in lines:
            m = re.search(r"(greeter|desktop|terminal): (?:ready after ([\d.]+)s|NOT ready|skipped)", l)
            if m:
                t[m.group(1)] = m.group(2) or ("skipped" if "skipped" in l else "NOT")
        print("TIMING " + " ".join(f"{k}={v}" for k, v in t.items()))
        sys.stdout.flush()
        return
    hits = [l[:200] for l in lines if KEY.search(l)][:40]
    for l in hits:
        print("DIAG " + l)
    serial = os.path.join(out_dir, "serial.log")
    if not os.path.exists(serial):
        try:
            sys.path.insert(0, HERE)
            import driver
            serial = driver.SERIAL_LOG
        except Exception:
            serial = None
    if serial and os.path.exists(serial):
        try:
            for l in _tail(serial, 25):
                print("DIAG serial: " + l)
        except OSError:
            pass
    steps = os.path.join(out_dir, "steps.json")
    if os.path.exists(steps):
        try:
            print("DIAG steps.json: " + open(steps).read().strip())
        except OSError:
            pass
    sys.stdout.flush()


if __name__ == "__main__":
    rc, ls, secs = _run_argv([sys.executable, "-c",
                              "import time; print('x', flush=True); time.sleep(10)"],
                             2, stop_driver=False)
    print(rc, len(ls), round(secs, 1))
