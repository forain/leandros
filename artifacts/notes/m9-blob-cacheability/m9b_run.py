#!/usr/bin/env python3
"""Boot LeandrOS in QEMU over a pty, log in, run commands, capture serial.

One process owns the whole run: no watcher, no poller, no second script.
"""
import argparse, os, pty, re, select, signal, subprocess, sys, time

ROOT = "/home/forain/Projects/leandros"


class Run:
    def __init__(self, argv, logpath, env):
        self.master, slave = pty.openpty()
        self.log = open(logpath, "wb", buffering=0)
        self.buf = b""
        self.proc = subprocess.Popen(
            argv, cwd=ROOT, stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True, env=env)
        os.close(slave)

    def pump(self, timeout):
        r, _, _ = select.select([self.master], [], [], timeout)
        if not r:
            return False
        try:
            chunk = os.read(self.master, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        self.log.write(chunk)
        self.buf += chunk
        if b"\x1b[6n" in chunk:
            self.send_raw(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
        return True

    def send_raw(self, data):
        for i in range(0, len(data), 8):
            os.write(self.master, data[i:i + 8])
            time.sleep(0.02)

    def send_line(self, line):
        self.send_raw(line.encode() + b"\n")

    def mark(self):
        """Position to search from. Taken BEFORE sending, never after: a
        lookback window here silently re-matches the PREVIOUS command's
        end-sentinel, which makes every command after the first appear to
        succeed instantly without running."""
        return len(self.buf)

    def expect(self, pattern, timeout, label="", start=None):
        rx = re.compile(pattern.encode())
        deadline = time.time() + timeout
        if start is None:
            start = len(self.buf)
        while time.time() < deadline:
            m = rx.search(self.buf, start)
            if m:
                return m
            if self.proc.poll() is not None:
                self.pump(0.5)
                m = rx.search(self.buf, start)
                if m:
                    return m
                raise RuntimeError(f"qemu exited before {label or pattern!r}")
            self.pump(0.5)
        raise TimeoutError(f"timeout waiting for {label or pattern!r}")

    def kill(self):
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
        except Exception:
            pass
        try:
            self.proc.wait(timeout=10)
        except Exception:
            pass
        # Drain whatever is left so the log is complete.
        t = time.time() + 2
        while time.time() < t and self.pump(0.2):
            pass
        self.log.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", default="x86_64")
    ap.add_argument("--accel", default=None, choices=[None, "kvm", "tcg"])
    ap.add_argument("--venus", action="store_true")
    ap.add_argument("--log", required=True)
    ap.add_argument("--cmd", action="append", default=[])
    ap.add_argument("--cmd-timeout", type=float, default=180.0)
    ap.add_argument("--boot-timeout", type=float, default=420.0)
    ap.add_argument("--user", default="root")
    args = ap.parse_args()

    argv = [f"{ROOT}/scripts/run-qemu.sh", args.arch]
    if args.accel:
        argv.append(f"--{args.accel}")
    if args.venus:
        argv.append("--venus")

    env = dict(os.environ)
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)

    r = Run(argv, args.log, env)
    rc = 0
    try:
        r.expect(r"login:", args.boot_timeout, "login prompt")
        r.send_line(args.user)
        r.expect(r"[Pp]assword", 60, "password prompt")
        r.send_line(args.user)
        # Shell is up once it answers something we asked for.
        r.send_line('echo SHELL""_UP')
        r.expect(r"SHELL_UP", 60, "shell prompt")
        for n, c in enumerate(args.cmd):
            print(f"### CMD[{n}]: {c}", flush=True)
            mk = r.mark()
            # Sentinel numbered per command as well as marked-from, so a stale
            # match is impossible even if the mark logic ever regresses.
            r.send_line(f'{c}; echo "RC=""$?"" ""ZZEND{n}"')
            try:
                m = r.expect(rf"RC=(-?\d+) ZZEND{n}\b", args.cmd_timeout,
                             f"end of {c!r}", start=mk)
                print(f"### rc={m.group(1).decode()}", flush=True)
            except TimeoutError as e:
                print(f"### TIMEOUT: {e}", flush=True)
                rc = 2
                break
    except Exception as e:
        print(f"### FATAL: {e}", flush=True)
        rc = 3
    finally:
        r.kill()
    print(f"### log: {args.log}", flush=True)
    return rc


if __name__ == "__main__":
    sys.exit(main())
