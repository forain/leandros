#!/usr/bin/env python3
"""coreutils-test.py — behavioral smoke/content test for uutils/coreutils on
LeandrOS, driven over driver.py against an ALREADY-RUNNING, ALREADY-IN-BRUSH
QEMU guest.

Why this isn't a plain bash script calling `brush -c '...'`: two real
platform bugs were found while building this harness (see README notes
below and the final report) that constrain what a guest command line may
contain:

  1. `$(...)` command substitution ALWAYS fails immediately with
     `error: i/o error: not a pipe` — even a single non-piped command like
     `$(cat f)`. So no guest line may use `$(...)`.
  2. Guest-side `<` input redirection ALWAYS hangs the shell forever (no
     error, no prompt, unrecoverable — needs a QEMU restart). And a `>`
     output-redirect statement, if followed *anywhere later in the same
     `;`-joined line* by a statement that opens a file (even a totally
     unrelated file, even a plain positional-arg read like `cat x`),
     ALSO hangs forever. `$?` parameter expansion also silently swallows
     the rest of the line (no hang, but no output either) — avoid it too.

What DOES work reliably:
  - `>` output redirection, as long as no later statement on the same line
    opens any file (used only for isolated fixture writes here).
  - A command's OWN file arguments (it opening files itself via its own
    open() calls, not shell redirection) — e.g. `cp a b; cat b` on one line
    is fine, `install -m 644 a b; cat b` is fine.
  - `&&` / `||` exit-status chaining and literal `echo` — no substitution
    needed.
  - Plain multi-statement `;`-joined lines with NO `>`/`<` anywhere are
    executed cleanly and in order.
  - Multi-process PIPELINES (`a | b`) are unreliable here — a 2-stage pipe
    produced un-truncated/reordered output in testing, and a 3-stage pipe
    scrambled output badly enough to wedge the shell. Pipes are avoided
    entirely in this harness (only used, in isolation, for the couple of
    commands — `tr`, `tee` — that have no other way to get input, and those
    are marked smoke-tested rather than behaviorally asserted).

A THIRD bug was found while trying to batch several fixture writes or several
tests onto one long `;`-joined guest line: brush's line editor (reedline)
redraws the ENTIRE input line, with full cursor-position escape sequences,
on every keystroke received over serial. Once a line is long enough to wrap
the 80-column terminal (in practice, anything much past ~65 characters,
given the ~11-character `brush-0.5# ` prompt), that per-keystroke redraw
becomes so expensive that submitting the line blows past any reasonable
timeout (measured: a bare 155-character `echo` took >25s to merely finish
echoing, vs. 0.4s for a 58-character line) — and a still-in-flight,
not-yet-submitted long line left the shell in a corrupted, unrecoverable
input-buffer state when a second command was sent on top of it.

Strategy, therefore: send ONE short (<= ~60 char) guest command per
driver.py call — no batching, no multi-statement marker lines. Fixtures are
written one file per call; each test is one (occasionally two, for
mutate-then-verify pairs too long to fit on one line) call(s). This is
slower in wall-clock terms but the only reliable approach found.

Usage:
  python3 coreutils-test.py [aarch64|x86_64]

Assumes QEMU is already running and already sitting at a brush prompt.
"""

import subprocess
import sys
import os
import re
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRIVER = os.path.join(HERE, "driver.py")

NOISE_RE = re.compile(r"^\[|Task::|2004|brush-0|^$")


def guest(line, timeout=20):
    """Send one line to the already-running brush shell, return filtered text."""
    r = subprocess.run(
        ["python3", DRIVER, "cmd", line, str(timeout)],
        capture_output=True, text=True, timeout=timeout + 15,
    )
    raw = r.stdout
    lines = [l for l in raw.splitlines() if not NOISE_RE.search(l)]
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Fixtures: ALL `>` writes, ALL in one line (no reads anywhere in this line
# — that combination is confirmed safe). Includes the base32/64/basenc
# encode step too (still just a `>` write, no read after it in this line).
# ---------------------------------------------------------------------------

FIXTURE_WRITES = [
    "cd /tmp",
    "printf 'abc' > hashin",
    "printf 'hi' > hi",
    "printf 'a\\nb\\n' > c1",
    "printf 'a\\nb\\nc\\n' > co_a",
    "printf 'b\\nc\\nd\\n' > co_b",
    "printf '1\\n2\\n3\\n4\\n' > cs1",
    "printf 'a:b:c' > cutin",
    "printf 'a\\tb' > expin",
    "printf 'abcdef' > foldin",
    "printf 'a b c\\n' > fmtin",
    "printf '1\\n2\\n3\\n' > h1",
    "printf '1 a\\n2 b\\n' > jo1",
    "printf '1 x\\n2 y\\n' > jo2",
    "printf 'a\\nb\\n' > pa1",
    "printf '1\\n2\\n' > pa2",
    "printf 'b\\na\\nc\\n' > s1",
    "printf 'a b\\nb c\\n' > ts1",
    "printf '1\\n2\\n3\\n' > tl1",
    "printf 'a\\na\\nb\\n' > uniqin",
    "printf '  a' > unexin",
    "printf 'a\\nb\\nc\\n' > w1",
    "printf 'x' > cg1",
    "printf 'x' > cm1",
    "printf 'x' > co1",
    "printf 'rmdata' > rmf1",
    "printf 'shreddata' > shredf1",
    "printf 'unlinkdata' > ulf1",
    "printf 'mvdata' > mvsrc",
    "printf 'abc' > sm1",
    "mkdir -p rd1",
    "base32 hi > hi.b32",
    "base64 hi > hi.b64",
    "basenc --base64 hi > hi.bc",
    "echo FIXTURES_DONE",
]

# ---------------------------------------------------------------------------
# Test cases. Each: (name, guest_cmd, check_fn(block_text) -> bool, smoke)
# guest_cmd must contain NO `>` and NO `<` and NO `$(...)`.
# ---------------------------------------------------------------------------

def contains_all(*subs):
    def f(t):
        return all(s in t for s in subs)
    return f


def equals(expected):
    def f(t):
        return t.strip() == expected
    return f


def nonempty():
    def f(t):
        return len(t.strip()) > 0
    return f


TESTS = [
    ("arch", "arch", nonempty(), False),
    ("b2sum", "b2sum hashin", nonempty(), False),
    ("base32_rt", "base32 -d hi.b32", equals("hi"), False),
    ("base64_rt", "base64 -d hi.b64", equals("hi"), False),
    ("basename", "basename /tmp/foo/bar.txt", equals("bar.txt"), False),
    ("basenc_rt", "basenc --base64 -d hi.bc", equals("hi"), False),
    ("cat", "cat c1", contains_all("a", "b"), False),
    ("chgrp", "chgrp 0 cg1", lambda t: "error" not in t.lower(), True),
    ("chmod", "chmod 644 cm1; stat -c %a cm1", contains_all("644"), False),
    ("chown", "chown 0 co1", lambda t: True, True),
    ("chroot", "chroot --help", lambda t: True, True),
    ("cksum", "cksum hashin", nonempty(), False),
    ("comm", "comm -12 co_a co_b", contains_all("b", "c"), False),
    ("cp", "cp s1 d1; cat d1", contains_all("b", "a", "c"), False),
    ("csplit", "csplit -z -f csout cs1 3; stat -c %s csout00", nonempty(), False),
    ("cut", "cut -d: -f2 cutin", equals("b"), False),
    ("date", "date +%Y", lambda t: t.strip().isdigit() and len(t.strip()) == 4, False),
    ("dd", "dd if=s1 of=ddout bs=1 count=4; stat -c %s ddout", contains_all("4"), False),
    ("df", "df", nonempty(), False),
    ("dir", "dir", contains_all("s1"), False),
    ("dircolors", "dircolors --print-database", nonempty(), False),
    ("dirname", "dirname /tmp/foo/bar.txt", equals("/tmp/foo"), False),
    ("du", "du s1", nonempty(), False),
    ("echo", "echo hi", equals("hi"), False),
    ("env", "env", contains_all("="), False),
    ("expand", "expand expin", lambda t: "\t" not in t and "a" in t and "b" in t, False),
    ("expr", "expr 2 + 3", equals("5"), False),
    ("factor", "factor 12", contains_all("2", "3"), False),
    ("false", "false && echo BAD || echo GOOD", equals("GOOD"), False),
    ("fmt", "fmt fmtin", nonempty(), False),
    ("fold", "fold -w3 foldin", contains_all("abc"), False),
    ("groups", "groups", nonempty(), False),
    ("head", "head -n1 h1", equals("1"), False),
    ("hostid", "hostid", nonempty(), True),
    ("hostname", "hostname", nonempty(), False),
    ("id", "id", nonempty(), False),
    ("install", "install -m 644 s1 iz2; cat iz2", contains_all("b", "a", "c"), False),
    ("join", "join jo1 jo2", lambda t: len(t.strip().splitlines()) == 2, False),
    ("kill", "kill -l", nonempty(), True),
    ("link", "link s1 lk2; cat lk2", contains_all("b", "a", "c"), False),
    ("ln", "ln s1 ln2; cat ln2", contains_all("b", "a", "c"), False),
    ("logname", "logname", lambda t: True, True),
    ("ls", "ls", contains_all("s1"), False),
    ("md5sum", "md5sum hashin", nonempty(), False),
    ("mkdir", "mkdir -p mkd1; stat -c %F mkd1", contains_all("directory"), False),
    ("mkfifo", "mkfifo mkf1; stat -c %F mkf1", contains_all("fifo"), False),
    ("mknod", "mknod --help", lambda t: True, True),
    ("mktemp", "mktemp", lambda t: t.strip().startswith("/"), False),
    ("more", "more --help", lambda t: True, True),
    ("mv", "mv mvsrc mvdst; cat mvdst", contains_all("mvdata"), False),
    ("nice", "nice true && echo GOOD || echo BAD", equals("GOOD"), False),
    ("nl", "nl c1", contains_all("1", "a"), False),
    ("nohup", "nohup --help", lambda t: True, True),
    ("nproc", "nproc", lambda t: t.strip().isdigit() and int(t.strip()) >= 1, False),
    ("numfmt", "numfmt --to=si 1000", contains_all("K"), False),
    ("od", "od -c hashin", nonempty(), False),
    ("paste", "paste pa1 pa2", contains_all("a", "1"), False),
    ("pathchk", "pathchk /tmp/valid_name && echo GOOD || echo BAD", equals("GOOD"), False),
    ("pinky", "pinky --help", lambda t: True, True),
    ("pr", "pr c1", nonempty(), False),
    ("printenv", "printenv", nonempty(), False),
    ("printf", "printf %s-%s a b", equals("a-b"), False),
    ("ptx", "ptx --help", lambda t: True, True),
    ("pwd", "pwd", equals("/tmp"), False),
    ("readlink", "ln -s target rl1; readlink rl1", equals("target"), False),
    ("realpath", "realpath .", equals("/tmp"), False),
    ("rm", "rm rmf1; test -f rmf1 && echo BAD || echo GOOD", equals("GOOD"), False),
    ("rmdir", "rmdir rd1; test -d rd1 && echo BAD || echo GOOD", equals("GOOD"), False),
    ("seq", "seq 1 3", equals("1\n2\n3"), False),
    ("sha1sum", "sha1sum hashin", nonempty(), False),
    ("sha224sum", "sha224sum hashin", nonempty(), False),
    ("sha256sum", "sha256sum hashin", nonempty(), False),
    ("sha384sum", "sha384sum hashin", nonempty(), False),
    ("sha512sum", "sha512sum hashin", nonempty(), False),
    ("shred", "shred -u shredf1; test -f shredf1 && echo BAD || echo GOOD", equals("GOOD"), False),
    ("shuf", "shuf w1", contains_all("a", "b", "c"), False),
    ("sleep", "sleep 0 && echo GOOD || echo BAD", equals("GOOD"), False),
    ("sort", "sort s1", lambda t: t.strip().splitlines()[:1] == ["a"], False),
    ("split", "split -l2 cs1 splout; stat -c %F splout00", contains_all("regular file"), False),
    ("stat", "stat -c %s s1", nonempty(), False),
    ("stty", "stty --help", lambda t: True, True),
    ("sum", "sum sm1", nonempty(), False),
    ("sync", "sync && echo GOOD || echo BAD", equals("GOOD"), False),
    ("tac", "tac w1", lambda t: t.strip().splitlines()[:1] == ["c"], False),
    ("tail", "tail -n1 tl1", equals("3"), False),
    ("tee", "tee --help", lambda t: True, True),
    ("test", "test 1 -eq 1 && echo GOOD || echo BAD", equals("GOOD"), False),
    ("timeout", "timeout 2 true && echo GOOD || echo BAD", equals("GOOD"), False),
    ("touch", "touch to1; stat -c %F to1", contains_all("regular file"), False),
    ("tr", "tr --help", lambda t: True, True),
    ("true", "true && echo GOOD || echo BAD", equals("GOOD"), False),
    ("truncate", "truncate -s5 trn1; stat -c %s trn1", contains_all("5"), False),
    ("tsort", "tsort ts1", equals("a\nb\nc"), False),
    ("tty", "tty", lambda t: True, True),
    ("uname", "uname", nonempty(), False),
    ("unexpand", "unexpand unexin", nonempty(), False),
    ("uniq", "uniq uniqin", equals("a\nb"), False),
    ("unlink", "unlink ulf1; test -f ulf1 && echo BAD || echo GOOD", equals("GOOD"), False),
    ("uptime", "uptime", lambda t: True, True),
    ("users", "users", lambda t: True, True),
    ("vdir", "vdir", contains_all("s1"), False),
    ("wc", "wc -l w1", contains_all("3"), False),
    ("who", "who", lambda t: True, True),
    ("whoami", "whoami", nonempty(), False),
]

TESTS.append(("yes", "timeout 1 yes", contains_all("y"), False))

assert len(TESTS) == 105, f"expected 105 tests, have {len(TESTS)}"


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else "aarch64"
    print(f"== LeandrOS coreutils test (arch={arch}) ==", flush=True)

    print("-- fixtures --", flush=True)
    for f in FIXTURE_WRITES:
        assert len(f) <= 65, f"fixture line too long ({len(f)}): {f!r}"
        out = guest(f, timeout=12)
        if "error" in out.lower() and "FIXTURES_DONE" not in f:
            print(f"  WARNING fixture may have failed: {f!r} -> {out!r}")
    print("fixtures OK", flush=True)

    results = {}
    smoke_names = {n for n, _, _, s in TESTS if s}

    for n, cmd, check, smoke in TESTS:
        assert len(cmd) <= 65, f"{n}: cmd too long ({len(cmd)}): {cmd!r}"
        out = guest(cmd, timeout=15 if n != "yes" else 12)
        try:
            ok = check(out)
        except Exception as e:
            ok = False
            out += f"\n[harness exception: {e}]"
        results[n] = (ok, cmd, out[:500])
        print(f"  {'PASS' if ok else 'FAIL'}: {n}", flush=True)

    passed = sum(1 for ok, _, _ in results.values() if ok)
    failed = len(results) - passed
    print()
    print("==================================================================")
    print(f" Summary (arch={arch})")
    print(f"   total:  {len(results)}")
    print(f"   passed: {passed}")
    print(f"   failed: {failed}")
    print("==================================================================")
    if failed:
        print("Failed commands:")
        for n, (ok, cmd, block) in results.items():
            if not ok:
                print(f"  {n}: cmd={cmd!r}")
                print(f"    output: {block!r}")
    print("Smoke-tested only (not behaviorally asserted):", sorted(smoke_names))


if __name__ == "__main__":
    main()
