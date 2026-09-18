#!/usr/bin/env python3
"""Interactive job-control regression for the login shell (brush) on the serial console.

Drives the shell the way a person at the console would — raw keystrokes,
^Z / ^C, `fg`, `bg`, `jobs`, `cmd &` — and after every scenario proves the
shell is still alive by typing a probe command and requiring its output. The
three bugs this guards (TODO.md "Open work", fixed on lane/brush):

  * a pipeline whose wait fails left the terminal's foreground group pointing
    at a dead group, so the next read from the console stopped/killed the
    login shell (bash restores the shell's pgrp on every path);
  * `fg` of a stopped pipeline sent SIGCONT to one pid, not the group, so the
    other members stayed stopped and the shell hung in `fg`;
  * `cmd &` printed `[1] <pid unknown>`.

Usage:
    LEANDROS_RUN_ID=<lane> python3 scripts/shjobs.py [--login user pass | --no-login] [--timeout-scale F]

Expects a booted guest (driver.py start …) sitting at the login prompt, or —
with --no-login — already at a shell prompt. Exit status is the number of
failed checks. Every byte of the conversation goes to
/tmp/leandros-<RUN_ID>-shjobs.log.
"""

import os
import re
import select
import socket
import sys
import threading
import time

RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
TAG = f"-{RUN_ID}" if RUN_ID else ""
SOCK = f"/tmp/leandros{TAG}-serial.sock"
LOG = f"/tmp/leandros{TAG}-shjobs.log"

ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][A-Za-z0-9]|\x1b[=>78]")

# The shell's own prompt as brush paints it on the serial console.
PROMPT = re.compile(rb"brush-[0-9.]+[#$] $")

failures = []
scale = 1.0


def log(b):
    try:
        with open(LOG, "ab") as f:
            f.write(b)
    except OSError:
        pass


class Console:
    """Serial console with a reader thread that never stops draining the
    socket: QEMU's serial back end drops guest output while its client is not
    reading (the kernel's `putc` gives up on a back-pressured UART), so a
    reader that pauses between expects loses whole chunks."""

    def __init__(self):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        deadline = time.time() + 6
        while True:
            try:
                self.s.connect(SOCK)
                break
            except OSError:
                if time.time() > deadline:
                    sys.exit(f"cannot connect to {SOCK}")
                time.sleep(0.15)
        self.lock = threading.Lock()
        self.buf = b""
        self.cond = threading.Condition(self.lock)
        self.last_rx = time.time()
        t = threading.Thread(target=self._reader, daemon=True)
        t.start()

    def _reader(self):
        while True:
            try:
                chunk = self.s.recv(65536)
            except OSError:
                return
            if not chunk:
                return
            log(chunk)
            # reedline probes the cursor position; answer like a terminal.
            n = chunk.count(b"\x1b[6n")
            if n:
                try:
                    self.s.sendall(b"\x1b[24;1R" * n)
                except OSError:
                    pass
            with self.cond:
                self.buf += chunk
                self.last_rx = time.time()
                self.cond.notify_all()

    def drain(self, quiet=0.3):
        """Wait until the line has been quiet for `quiet` seconds."""
        while True:
            with self.cond:
                idle = time.time() - self.last_rx
                if idle >= quiet * scale:
                    return
                self.cond.wait(quiet * scale - idle)

    def raw(self, data):
        # PL011 RX FIFO is 16 bytes and the shell polls it: pace the bytes.
        for i in range(0, len(data), 8):
            self.s.sendall(data[i:i + 8])
            time.sleep(0.02)

    def line(self, text):
        self.raw(text.encode() + b"\n")

    def expect(self, pattern, timeout=8.0):
        """Wait until `pattern` (bytes regex, matched on ANSI-stripped text
        of everything received since the last `clear`) appears."""
        rx = re.compile(pattern) if isinstance(pattern, bytes) else pattern
        deadline = time.time() + timeout * scale
        with self.cond:
            while True:
                m = rx.search(self._text())
                if m:
                    return m
                left = deadline - time.time()
                if left <= 0:
                    return None
                self.cond.wait(min(left, 0.5))

    def expect_prompt(self, timeout=8.0):
        return self.expect(PROMPT, timeout) is not None

    def submit(self, cmd, timeout=8.0):
        """Type a command, wait for the shell to accept it (its echo followed
        by a newline — reedline repaints the line while it is typed, so the
        prompt regex alone would match a repaint before the command ran) and
        then for the prompt that follows the command's output. Returns the
        text between the echo and that prompt, or None on timeout."""
        self.clear()
        self.line(cmd)
        needle = re.escape(cmd[-16:].encode()) + rb"\n"
        m = self.expect(needle, timeout)
        if m is None:
            return None
        start = m.end()
        deadline = time.time() + timeout * scale
        with self.cond:
            while True:
                t = self._text()
                pm = PROMPT.search(t, start)
                if pm:
                    return t[start:pm.start()]
                left = deadline - time.time()
                if left <= 0:
                    return None
                self.cond.wait(min(left, 0.5))

    def _text(self):
        t = ANSI.sub(b"", self.buf)
        # A chunk cut mid-escape leaves a partial sequence at the very end.
        t = re.sub(rb"\x1b(\[[0-9;?]*)?$", b"", t)
        return t.replace(b"\r\n", b"\n")

    def text(self):
        with self.cond:
            return self._text()

    def clear(self):
        with self.cond:
            self.buf = b""


def check(name, ok, detail=""):
    tag = "PASS" if ok else "FAIL"
    print(f"[{tag}] {name}" + (f" — {detail}" if detail else ""), flush=True)
    if not ok:
        failures.append(name)
    return ok


def tail(con, n=600):
    t = con.text().decode("utf-8", "replace")
    return t[-n:].replace("\n", "\\n")


def alive(con, label):
    """The shell must still take and run a typed command."""
    token = f"ALIVE{int(time.time() * 1000) % 100000}"
    out = con.submit(f'echo "{token[:5]}""{token[5:]}"', timeout=12)
    ok = out is not None and (token.encode() + b"\n") in out
    return check(f"{label}: shell still accepts commands", ok, "" if ok else tail(con))


def login(con, user, password):
    """The guest sits at getty's `login: ` prompt (already consumed before we
    connected); type the credentials straight in."""
    con.drain(0.5)
    con.clear()
    con.line(user)
    if con.expect(rb"Password: ", timeout=15) is None:
        sys.exit("no password prompt: " + tail(con))
    con.line(password)
    if not con.expect_prompt(timeout=30):
        sys.exit("no shell prompt after login: " + tail(con))
    return True


def settle(con):
    """Return to a clean prompt: drain output, sync on a bare CR."""
    con.drain(0.3)
    con.clear()
    con.raw(b"\r")
    con.expect_prompt(timeout=6)
    con.drain(0.2)
    con.clear()


# ── scenarios ────────────────────────────────────────────────────────────────

def scenario_pipeline_stop_fg(con):
    """sleep 5 | cat, ^Z, jobs, fg — the pipeline must be continued as a
    GROUP and run to completion; fg must not re-report Stopped."""
    settle(con)
    con.line("sleep 5 | cat")
    time.sleep(1.0 * scale)
    con.raw(b"\x1a")  # ^Z
    m = con.expect(rb"\[1\]\+?\s*Stopped\s+sleep 5 \|\s?cat", timeout=8)
    check("pipeline ^Z reports Stopped", m is not None, "" if m else tail(con))
    check("prompt after ^Z", con.expect_prompt(timeout=6))
    out = con.submit("jobs", timeout=8) or b""
    m = re.search(rb"\[1\]\+?\s*Stopped\s+sleep 5 \|\s?cat", out)
    check("jobs lists the stopped pipeline", m is not None, "" if m else tail(con))
    con.clear()
    t0 = time.time()
    con.line("fg")
    con.expect(rb"sleep 5 \|\s?cat\n", timeout=6)
    # Must come back to a prompt without printing Stopped again; the sleep has
    # <= 4 s left, allow generous slack for TCG.
    got_prompt = con.expect_prompt(timeout=25)
    dt = time.time() - t0
    body = con.text()
    check("fg resumes the whole pipeline and it finishes",
          got_prompt and b"Stopped" not in body,
          f"dt={dt:.1f}s " + ("" if got_prompt and b"Stopped" not in body else tail(con)))
    out = con.submit("jobs", timeout=8)
    check("jobs is empty after the pipeline completed",
          out is not None and not re.search(rb"sleep 5 \|\s?cat", out), tail(con, 200))
    alive(con, "stop/fg")


def scenario_pipeline_stop_bg(con):
    """sleep 3 | cat, ^Z, bg — both members must be continued (the job
    finishes on its own and is reported Done)."""
    settle(con)
    con.line("sleep 3 | cat")
    time.sleep(0.8 * scale)
    con.raw(b"\x1a")
    check("bg: ^Z stops the pipeline",
          con.expect(rb"Stopped\s+sleep 3 \|\s?cat", timeout=8) is not None, tail(con))
    con.expect_prompt(timeout=6)
    out = con.submit("bg", timeout=8)
    check("bg returns to the prompt", out is not None, tail(con))
    # Give it time to finish, then poke the shell so it reports the job.
    time.sleep(4.0 * scale)
    out = con.submit("jobs", timeout=8)
    check("bg continued both members (job no longer Stopped)",
          out is not None and b"Stopped" not in out, tail(con))
    alive(con, "stop/bg")


def scenario_last_exits_first(con):
    """A pipeline whose LAST member exits before the first: the shell must
    reap it, print the first member's output, and get its terminal back."""
    settle(con)
    out = con.submit("sleep 2 | true; echo rc=$?", timeout=15)
    check("last-member-exits-first pipeline completes with rc=0",
          out is not None and b"rc=0\n" in out, tail(con))
    alive(con, "last-exits-first")
    # Same with the FIRST member exiting first, and a pipe reader that exits early.
    out = con.submit("true | sleep 1; echo rc=$?", timeout=15)
    check("first-member-exits-first pipeline completes",
          out is not None and b"rc=0\n" in out, tail(con))
    out = con.submit("yes | head -n 2; echo rc=$?", timeout=15)
    check("yes | head (SIGPIPE producer) completes",
          out is not None and re.search(rb"y\ny\n(.*\n)?rc=0\n", out) is not None, tail(con))
    alive(con, "first-exits-first")


def scenario_pipeline_error_path(con):
    """An error raised while the pipeline is being set up or waited for,
    after a member has already taken the terminal, must still hand the
    terminal back to the shell."""
    settle(con)
    # Second member fails to spawn (not found) after the first took the tty.
    out = con.submit("sleep 1 | /nonexistent/cmd; echo rc=$?", timeout=15)
    check("spawn failure mid-pipeline returns to the prompt",
          out is not None and re.search(rb"rc=\d+\n", out) is not None, tail(con))
    alive(con, "spawn-failure")
    # Redirect failure on a later member.
    out = con.submit("sleep 1 | cat < /nonexistent/file; echo rc=$?", timeout=15)
    check("redirect failure mid-pipeline returns to the prompt",
          out is not None and re.search(rb"rc=\d+\n", out) is not None, tail(con))
    alive(con, "redirect-failure")
    # An expansion error on a later member (bad substitution) after the first took the tty.
    out = con.submit("sleep 1 | cat ${x!y}; echo rc=$?", timeout=15)
    check("expansion error mid-pipeline returns to the prompt",
          out is not None and re.search(rb"rc=\d+\n", out) is not None, tail(con))
    alive(con, "expansion-error")


def scenario_ctrl_c_pipeline(con):
    settle(con)
    con.line("sleep 10 | cat")
    time.sleep(0.8 * scale)
    con.raw(b"\x03")  # ^C
    check("^C of a pipeline returns to the prompt", con.expect_prompt(timeout=10), tail(con))
    alive(con, "^C")


def scenario_background_pid(con):
    """`cmd &` must print `[N] <pid>` with a real pid, $! must match, and the
    job must be listed and reaped."""
    settle(con)
    con.line("sleep 2 &")
    m = con.expect(rb"\[(\d+)\]\+?\s+(\S+)\n", timeout=8)
    pid = m.group(2) if m else b""
    ok = m is not None and pid.isdigit() and int(pid) > 1
    check("cmd & prints a numeric pid", ok, tail(con, 200))
    con.expect_prompt(timeout=6)
    out = con.submit('echo "BG""PID=$!"', timeout=8) or b""
    m2 = re.search(rb"BGPID=(\d*)\n", out)
    check("$! equals the printed pid",
          m2 is not None and m2.group(1) == pid, f"got {m2.group(1) if m2 else None!r} want {pid!r}")
    out = con.submit("jobs -p", timeout=8) or b""
    check("jobs -p shows the pid", bool(pid) and re.search(rb"^" + pid + rb"\n", out, re.M) is not None,
          tail(con, 300))
    out = con.submit("wait; echo waited", timeout=15)
    check("wait reaps the background job", out is not None and b"waited\n" in out, tail(con))
    alive(con, "cmd &")
    # A background pipeline and a background builtin-only job.
    con.clear()
    con.line("sleep 1 | cat &")
    m = con.expect(rb"\[(\d+)\]\+?\s+(\d+)\n", timeout=8)
    check("pipeline & prints a numeric pid", m is not None, tail(con, 200))
    con.expect_prompt(timeout=6)
    out = con.submit("wait; echo waited2", timeout=15)
    check("wait reaps the background pipeline", out is not None and b"waited2\n" in out, tail(con))
    alive(con, "pipeline &")


def scenario_kill_job(con):
    """kill %1 of a stopped pipeline must reach every member."""
    settle(con)
    con.line("sleep 20 | sleep 20")
    time.sleep(0.8 * scale)
    con.raw(b"\x1a")
    check("kill: ^Z stops the pipeline",
          con.expect(rb"Stopped\s+sleep 20 \|\s?sleep 20", timeout=8) is not None, tail(con))
    con.expect_prompt(timeout=6)
    # Both members must die: `wait` returns only once the whole job is reaped (a member
    # still stopped would hold it for the full 20 s).
    t0 = time.time()
    out = con.submit("kill -9 %1; wait; echo jobs-done", timeout=15) or b""
    dt = time.time() - t0
    check("kill %1 terminates the whole stopped pipeline (wait returns at once)",
          b"jobs-done" in out and dt < 10 * scale, f"dt={dt:.1f}s " + tail(con))
    out = con.submit("jobs", timeout=8)
    check("no job left after kill %1 + wait", out is not None and b"Stopped" not in out, tail(con))
    alive(con, "kill %1")


def main():
    global scale
    args = sys.argv[1:]
    user, password = "root", "root"
    do_login = True
    while args:
        a = args.pop(0)
        if a == "--login":
            user, password = args.pop(0), args.pop(0)
        elif a == "--no-login":
            do_login = False
        elif a == "--timeout-scale":
            scale = float(args.pop(0))
        else:
            sys.exit(f"unknown arg {a}")
    try:
        os.unlink(LOG)
    except OSError:
        pass
    con = Console()
    if do_login:
        login(con, user, password)
    settle(con)
    alive(con, "start")
    for sc in (scenario_background_pid, scenario_pipeline_stop_fg, scenario_pipeline_stop_bg,
               scenario_last_exits_first, scenario_pipeline_error_path, scenario_ctrl_c_pipeline,
               scenario_kill_job):
        try:
            sc(con)
        except Exception as e:  # keep going; the summary is what matters
            check(f"{sc.__name__} raised", False, repr(e))
    print(f"shjobs: {len(failures)} failed" + (": " + ", ".join(failures) if failures else ""))
    sys.exit(len(failures))


if __name__ == "__main__":
    main()
