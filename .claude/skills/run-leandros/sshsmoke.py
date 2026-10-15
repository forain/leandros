#!/usr/bin/env python3
"""sshsmoke.py [--full] [--user U --password P] -- drive the guest's sshd from
the host with the real OpenSSH client (ssh, scp, sftp, git).

The VM must be running (driver.py start, same LEANDROS_RUN_ID); the target
comes from `driver.py ssh-target` (127.0.0.1:<hostfwd port> on SLIRP, the
guest's own address under socket_vmnet). Passwords go through SSH_ASKPASS
(SSH_ASKPASS_REQUIRE=force, OpenSSH >= 8.4), so no sshpass is needed.

Quick (the regress step): sshd answers with its banner, password login as
leandro runs a command with the right uid/home/PATH, the exit status comes
back, stderr arrives separately, and a pty session gets a tty.

--full adds: root password login refused (prohibit-password), public-key
login (key installed over ssh), window-change reaching the pty, sftp and scp
round trips, git clone/push over ssh against a repository in the guest, and
git in the guest (version, init/add/commit/log, clone over https).

Prints `ssh_<step>: PASS|FAIL <detail>` per step and a summary; exit 0
iff all passed.
"""
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import fcntl
import termios

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

RESULTS = []


def step(name, ok, detail=""):
    RESULTS.append((name, ok))
    print(f"ssh_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)
    return ok


def target():
    r = subprocess.run([sys.executable, os.path.join(HERE, "driver.py"), "ssh-target"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"sshsmoke: no ssh target: {r.stdout}{r.stderr}")
    host, port = r.stdout.split()
    return host, int(port)


class Ssh:
    def __init__(self, host, port, tmp):
        self.host, self.port, self.tmp = host, port, tmp
        self.askpass = os.path.join(tmp, "askpass.sh")
        with open(self.askpass, "w") as f:
            f.write("#!/bin/sh\nprintf '%s\\n' \"$SSHSMOKE_PW\"\n")
        os.chmod(self.askpass, 0o700)
        self.known = os.path.join(tmp, "known_hosts")
        self.key = os.path.join(tmp, "id_ed25519")

    def base_opts(self):
        return ["-F", "/dev/null", "-o", "StrictHostKeyChecking=no",
                "-o", f"UserKnownHostsFile={self.known}", "-o", "ConnectTimeout=20",
                "-o", "LogLevel=ERROR"]

    def pw_opts(self):
        return self.base_opts() + ["-o", "PreferredAuthentications=password,keyboard-interactive",
                                   "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1"]

    def key_opts(self):
        return self.base_opts() + ["-o", "PreferredAuthentications=publickey", "-o", "BatchMode=yes",
                                   "-o", "IdentitiesOnly=yes", "-i", self.key]

    def env(self, password):
        e = dict(os.environ)
        e.update(SSH_ASKPASS=self.askpass, SSH_ASKPASS_REQUIRE="force", DISPLAY=":0",
                 SSHSMOKE_PW=password or "")
        return e

    def run(self, user, cmd, password=None, key=False, stdin=None, timeout=120, extra=()):
        opts = self.key_opts() if key else self.pw_opts()
        argv = ["ssh", *opts, *extra, "-p", str(self.port), f"{user}@{self.host}", cmd]
        r = subprocess.run(argv, input=stdin, capture_output=True, timeout=timeout,
                           env=self.env(password), stdin=None if stdin is not None else subprocess.DEVNULL)
        return r.returncode, r.stdout.decode(errors="replace"), r.stderr.decode(errors="replace")


def pty_session(ssh, user, password, script, rows=24, cols=80, resize=None, timeout=60):
    """Run `ssh -tt` under a real pty of the given size, feed `script` lines,
    optionally resize (SIGWINCH to ssh) before the lines after a 'RESIZE'
    marker. Returns everything the session printed."""
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe("ssh", ["ssh", *ssh.pw_opts(), "-tt", "-p", str(ssh.port),
                           f"{user}@{ssh.host}"], ssh.env(password))
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    out = b""

    def read_for(secs, until=None):
        nonlocal out
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.2)
            if r:
                try:
                    d = os.read(fd, 4096)
                except OSError:
                    return False
                if not d:
                    return False
                out += d
                # Answer cursor-position queries (DSR, ESC[6n) as a real
                # terminal does: brush (reedline) asks at every prompt and
                # gives up on the line editor when nobody replies.
                for _ in range(d.count(b"\x1b[6n")):
                    os.write(fd, b"\x1b[1;1R")
                if until and until in out:
                    return True
        return True

    read_for(15, until=b"$")  # the prompt (brush: user@host:dir$ or #)
    for line in script:
        if line == "RESIZE" and resize:
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", resize[0], resize[1], 0, 0))
            os.kill(pid, signal.SIGWINCH)
            read_for(1.5)
            continue
        try:
            os.write(fd, (line + "\r").encode())
        except OSError:
            break
        read_for(3)
    read_for(timeout / 4)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    _, status = os.waitpid(pid, 0)
    os.close(fd)
    return out.decode(errors="replace"), status


def quick(ssh, user, pw):
    r = subprocess.run(["ssh-keyscan", "-T", "15", "-t", "ed25519", "-p", str(ssh.port), ssh.host],
                       capture_output=True, text=True, timeout=60)
    m = re.search(r"ssh-ed25519 (\S+)", r.stdout)
    step("hostkey", bool(m), m.group(1)[:24] + "..." if m else (r.stderr.strip()[-200:]))
    rc, out, err = ssh.run(user, 'id -un; id -u; echo "$HOME"; echo "$PATH"; echo "$SSH_CONNECTION"', password=pw)
    lines = out.split("\n")
    step("password_exec", rc == 0 and lines[:1] == [user],
         f"rc={rc} out={out.strip()!r} err={err.strip()[-200:]!r}")
    rc, out, err = ssh.run(user, "echo to-out; echo to-err >&2; exit 7", password=pw)
    step("exit_status_stderr", rc == 7 and out.strip() == "to-out" and err.strip() == "to-err",
         f"rc={rc} out={out.strip()!r} err={err.strip()!r}")
    rc, out, err = ssh.run(user, "cat | wc -c", password=pw, stdin=b"x" * 100000)
    step("stdin_eof", rc == 0 and out.strip() == "100000", f"rc={rc} out={out.strip()!r}")
    rc, out, err = ssh.run(user, "tty", password=pw, extra=("-tt",))
    step("exec_pty", rc == 0 and "/dev/pts/" in out, f"rc={rc} out={out.strip()!r}")


def full(ssh, user, pw, tmp):
    # root by password: refused (PermitRootLogin prohibit-password)
    rc, out, err = ssh.run("root", "true", password="root")
    step("root_password_refused", rc == 255, f"rc={rc} err={err.strip()[-120:]!r}")
    # wrong password
    rc, out, err = ssh.run(user, "true", password="wrong-" + pw)
    step("bad_password_refused", rc == 255, f"rc={rc}")

    # public key: install, then BatchMode login
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", ssh.key], check=True)
    pub = open(ssh.key + ".pub", "rb").read()
    rc, out, err = ssh.run(user, "umask 077; mkdir -p .ssh && cat > .ssh/authorized_keys && echo ok",
                           password=pw, stdin=pub)
    step("install_key", rc == 0 and "ok" in out, f"rc={rc} err={err.strip()[-200:]!r}")
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("pubkey_login", rc == 0 and out.strip() == user, f"rc={rc} out={out.strip()!r} err={err.strip()[-200:]!r}")

    # interactive pty: tty, window size before and after a resize
    out, status = pty_session(ssh, user, pw,
                              ["tty", "stty size", "RESIZE", "stty size", "exit 0"],
                              rows=30, cols=100, resize=(41, 132))
    step("pty_shell", "/dev/pts/" in out, repr(out[-300:]))
    step("pty_winsize", "30 100" in out, "")
    step("pty_window_change", "41 132" in out, "")

    # sftp + scp (scp speaks SFTP by default since OpenSSH 9.0)
    src = os.path.join(tmp, "blob.bin")
    with open(src, "wb") as f:
        f.write(os.urandom(300000))
    back = os.path.join(tmp, "blob.back")
    batch = os.path.join(tmp, "sftp.batch")
    with open(batch, "w") as f:
        f.write(f"put {src} blob.bin\nget blob.bin {back}\nrm blob.bin\n")
    r = subprocess.run(["sftp", *ssh.key_opts(), "-P", str(ssh.port), "-b", batch, f"{user}@{ssh.host}"],
                       capture_output=True, timeout=180)
    step("sftp_roundtrip", r.returncode == 0 and os.path.exists(back) and open(back, "rb").read() == open(src, "rb").read(),
         f"rc={r.returncode} {r.stderr.decode(errors='replace')[-200:]!r}")
    back2 = os.path.join(tmp, "blob.scp")
    r1 = subprocess.run(["scp", *ssh.key_opts(), "-P", str(ssh.port), src, f"{user}@{ssh.host}:blob2.bin"],
                        capture_output=True, timeout=180)
    r2 = subprocess.run(["scp", *ssh.key_opts(), "-P", str(ssh.port), f"{user}@{ssh.host}:blob2.bin", back2],
                        capture_output=True, timeout=180)
    step("scp_roundtrip", r1.returncode == 0 and r2.returncode == 0 and os.path.exists(back2)
         and open(back2, "rb").read() == open(src, "rb").read(),
         f"rc={r1.returncode},{r2.returncode} {r1.stderr.decode(errors='replace')[-200:]!r}")

    # git in the guest, as the user
    script = ("set -e; git --version; rm -rf /tmp/gt-$$ && mkdir -p /tmp/gt-$$ && cd /tmp/gt-$$ && "
              "git init -q r && cd r && git config user.email t@leandros && git config user.name t && "
              "echo hello > a.txt && git add a.txt && git commit -q -m first && git log --oneline && "
              "git status --short && echo GIT-LOCAL-OK")
    rc, out, err = ssh.run(user, script, key=True, timeout=300)
    step("guest_git_local", rc == 0 and "GIT-LOCAL-OK" in out and "first" in out,
         f"rc={rc} out={out.strip()[-200:]!r} err={err.strip()[-300:]!r}")
    rc, out, err = ssh.run(user, "cd /tmp && rm -rf hello-world && "
                           "git clone -q https://github.com/octocat/Hello-World.git hello-world && "
                           "git -C hello-world log --oneline -1 && ls hello-world", key=True, timeout=600)
    step("guest_git_clone_https", rc == 0 and "README" in out,
         f"rc={rc} out={out.strip()[-200:]!r} err={err.strip()[-300:]!r}")

    # git over ssh: clone a repo that lives in the guest, push a commit back
    rc, out, err = ssh.run(user, "rm -rf srv.git src && git init -q --bare srv.git && git init -q src && cd src && "
                           "git config user.email t@leandros && git config user.name t && "
                           "echo one > f && git add f && git commit -q -m one && git push -q ../srv.git HEAD:main && "
                           "echo SRV-OK", key=True, timeout=300)
    step("guest_bare_repo", rc == 0 and "SRV-OK" in out, f"rc={rc} err={err.strip()[-300:]!r}")
    gitssh = " ".join(["ssh", *ssh.key_opts()])
    genv = dict(os.environ, GIT_SSH_COMMAND=gitssh, GIT_TERMINAL_PROMPT="0")
    wc = os.path.join(tmp, "wc")
    url = f"ssh://{user}@{ssh.host}:{ssh.port}/home/{user}/srv.git"
    r = subprocess.run(["git", "clone", "-q", url, wc], capture_output=True, env=genv, timeout=300)
    ok = r.returncode == 0 and os.path.exists(os.path.join(wc, "f"))
    step("git_clone_over_ssh", ok, f"rc={r.returncode} {r.stderr.decode(errors='replace')[-300:]!r}")
    if ok:
        with open(os.path.join(wc, "g"), "w") as f:
            f.write("two\n")
        subprocess.run(["git", "-C", wc, "add", "g"], check=True)
        subprocess.run(["git", "-C", wc, "-c", "user.email=h@host", "-c", "user.name=h", "commit", "-q", "-m", "two"], check=True)
        r = subprocess.run(["git", "-C", wc, "push", "-q", "origin", "HEAD:main"], capture_output=True, env=genv, timeout=300)
        rc, out, err = ssh.run(user, "git -C srv.git log --oneline main", key=True)
        step("git_push_over_ssh", r.returncode == 0 and "two" in out,
             f"rc={r.returncode} log={out.strip()!r} {r.stderr.decode(errors='replace')[-300:]!r}")


def main(argv):
    full_mode = "--full" in argv
    user, pw = "leandro", "leandro"
    if "--user" in argv:
        user = argv[argv.index("--user") + 1]
    if "--password" in argv:
        pw = argv[argv.index("--password") + 1]
    if shutil.which("ssh") is None:
        print("sshsmoke: no ssh client on the host")
        return 2
    host, port = target()
    print(f"sshsmoke: target {host}:{port} user {user}", flush=True)
    tmp = tempfile.mkdtemp(prefix="sshsmoke-")
    ssh = Ssh(host, port, tmp)
    # sshd starts with init; give it a moment after the login prompt.
    deadline = time.time() + 60
    while time.time() < deadline:
        r = subprocess.run(["ssh-keyscan", "-T", "5", "-p", str(port), host], capture_output=True, timeout=30)
        if b"ssh-" in r.stdout:
            break
        time.sleep(2)
    quick(ssh, user, pw)
    if full_mode:
        full(ssh, user, pw, tmp)
    shutil.rmtree(tmp, ignore_errors=True)
    bad = [n for n, ok in RESULTS if not ok]
    print(f"SSHSMOKE summary: {len(RESULTS) - len(bad)}/{len(RESULTS)} passed"
          + (f"; failed: {' '.join(bad)}" if bad else ""), flush=True)
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
