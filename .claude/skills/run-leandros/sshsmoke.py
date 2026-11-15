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

    def kbd_opts(self):
        return self.base_opts() + ["-o", "PreferredAuthentications=keyboard-interactive", "-o", "PubkeyAuthentication=no",
                                   "-o", "KbdInteractiveAuthentication=yes", "-o", "NumberOfPasswordPrompts=1"]

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
    step("session_path_standard",
         len(lines) > 3 and lines[3] == "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
         f"PATH={lines[3] if len(lines) > 3 else None!r}")
    rc, out, err = ssh.run(user, "echo to-out; echo to-err >&2; exit 7", password=pw)
    step("exit_status_stderr", rc == 7 and out.strip() == "to-out" and err.strip() == "to-err",
         f"rc={rc} out={out.strip()!r} err={err.strip()!r}")
    rc, out, err = ssh.run(user, "cat | wc -c", password=pw, stdin=b"x" * 100000)
    step("stdin_eof", rc == 0 and out.strip() == "100000", f"rc={rc} out={out.strip()!r}")
    rc, out, err = ssh.run(user, "tty", password=pw, extra=("-tt",))
    step("exec_pty", rc == 0 and "/dev/pts/" in out, f"rc={rc} out={out.strip()!r}")
    # sshd listens on 0.0.0.0:22 and [::]:22 (V6ONLY): both loopback addresses
    # answer with the SSH banner (lane ip6dual).
    for tag, dev in (("v4", "127.0.0.1"), ("v6", "::1")):
        rc, out, err = ssh.run(user, f"/bin/ip6test banner {dev} 22", password=pw)
        step(f"loopback_banner_{tag}", rc == 0 and out.startswith("SSH-"), f"rc={rc} out={out.strip()!r} err={err.strip()[-120:]!r}")
    utmp(ssh, user, pw)
    rc, out, err = ssh.run(user, "cat /proc/self/cgroup", password=pw)
    step("session_cgroup", rc == 0 and "/user.slice/user-" in out and "/session-" in out,
         f"rc={rc} out={out.strip()!r}")


def utmp(ssh, user, pw):
    """utmpx/wtmp/lastlog (lane utmp). Alpine busybox reads them: `who` through
    the utmps socket (served by /bin/utmpd), `last` straight from wtmp."""
    bb = "/bin/busybox.static"
    # An ssh pty session is a login: `who` run inside it lists itself, and the
    # console root session the harness logged in on is listed too (/bin/login).
    rc, out, err = ssh.run(user, f"{bb} who", password=pw, extra=("-tt",))
    who = [l.split() for l in out.replace("\r", "").splitlines() if l.strip()]
    step("utmp_who_ssh", any(w[0] == user and w[1].startswith("pts/") for w in who),
         f"rc={rc} out={out.strip()!r} err={err.strip()[-120:]!r}")
    step("utmp_who_console_login", any(w[0] == "root" and (w[1].startswith("tty") or w[1] == "console") for w in who),
         f"out={out.strip()!r}")
    # The first session has ended: DEAD_PROCESS, so a plain exec (no pty, never
    # recorded) sees only the current console and ssh-less state.
    rc, out, err = ssh.run(user, f"{bb} who", password=pw)
    lines = [l.split() for l in out.splitlines() if l.strip()]
    step("utmp_dead_after_exit", rc == 0 and not any(w[0] == user for w in lines),
         f"rc={rc} out={out.strip()!r}")
    rc, out, err = ssh.run(user, f"{bb} last", password=pw)
    # busybox `last` lists logins only (no boot rows); the BOOT_TIME record
    # (type 2, first word of wtmp) is checked on the raw file.
    rc2, wt, _ = ssh.run(user, f"{bb} od -An -tx2 -N2 /var/log/wtmp", password=pw)
    step("utmp_last_history", rc == 0 and any(l.split()[:1] == [user] and "pts/" in l for l in out.splitlines())
         and wt.split() == ["0002"], f"rc={rc} out={out.strip()[-300:]!r} wtmp0={wt.strip()!r}")
    rc, out, err = ssh.run(user, "ls -l /var/log/lastlog /var/log/wtmp /run/utmps/utmp", password=pw)
    step("utmp_files", rc == 0 and out.count("/var/log/") == 2, f"rc={rc} out={out.strip()!r}")


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


def set_keys(ssh, user, pw, line):
    """Install `line` as the user's whole authorized_keys (over a password login)."""
    rc, out, err = ssh.run(user, "umask 077; mkdir -p .ssh && cat > .ssh/authorized_keys && echo ok",
                           password=pw, stdin=line.encode() + b"\n")
    return rc == 0 and "ok" in out


def options(ssh, user, pw, tmp):
    """authorized_keys options (command=, from=, no-pty, restrict) and MaxStartups."""
    if not os.path.exists(ssh.key):
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", ssh.key], check=True)
    pub = open(ssh.key + ".pub").read().strip()

    def with_opts(opts):
        return set_keys(ssh, user, pw, f"{opts} {pub}" if opts else pub)

    # command=: forced, the client's command arrives in SSH_ORIGINAL_COMMAND
    ok = with_opts('command="echo FORCED[$SSH_ORIGINAL_COMMAND]"')
    rc, out, err = ssh.run(user, "echo hi there", key=True)
    step("akopt_command", ok and rc == 0 and out.strip() == "FORCED[echo hi there]", f"rc={rc} out={out.strip()!r} err={err.strip()[-120:]!r}")
    # sftp is refused for a command= key
    rc, out, err = ssh.run(user, "true", key=True, extra=("-s",))
    # (ssh -s sftp: the forced command key must not hand out the subsystem)
    step("akopt_command_no_subsystem", rc != 0 or "FORCED" not in out, f"rc={rc}")

    # from=: a non-matching list refuses the key, a matching/negated one accepts it
    ok = with_opts('from="192.0.2.*,198.51.100.0/24"')
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("akopt_from_refused", ok and rc == 255, f"rc={rc}")
    ok = with_opts('from="!192.0.2.1,*"')
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("akopt_from_allowed", ok and rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-120:]!r}")
    ok = with_opts('from="0.0.0.0/0,::/0"')
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("akopt_from_cidr", ok and rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-120:]!r}")

    # no-pty / restrict: -tt gets no tty
    for opt in ("no-pty", "restrict"):
        ok = with_opts(opt)
        rc, out, err = ssh.run(user, "tty", key=True, extra=("-tt",))
        step(f"akopt_{opt.replace('-', '_')}", ok and "/dev/pts/" not in out
             and "PTY allocation request failed" in err, f"rc={rc} out={out.strip()!r} err={err.strip()[-120:]!r}")
    # an option this server does not implement: the line is ignored, never half-applied
    ok = with_opts('tunnel="0"')
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("akopt_unknown_skipped", ok and rc == 255, f"rc={rc}")
    set_keys(ssh, user, pw, pub)

    # MaxStartups (default 10:30:100): 130 idle connections cannot all be held
    import socket
    socks, banners = [], 0
    for _ in range(130):
        try:
            c = socket.create_connection((ssh.host, ssh.port), timeout=10)
            c.settimeout(3)
            socks.append(c)
        except OSError:
            continue
    for c in socks:
        try:
            if c.recv(4).startswith(b"SSH-"):
                banners += 1
        except OSError:
            pass
    for c in socks:
        c.close()
    step("maxstartups_cap", 0 < banners <= 100, f"banners={banners}/{len(socks)}")
    time.sleep(2)
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("maxstartups_recovers", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-120:]!r}")


def free_port():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def fwd_client(ssh, user, *fwd):
    """ssh -N with -L/-R specs; ExitOnForwardFailure so a refused forward ends it."""
    argv = ["ssh", *ssh.key_opts(), "-N", "-o", "ExitOnForwardFailure=yes", *fwd,
            "-p", str(ssh.port), f"{user}@{ssh.host}"]
    return subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def stop(p):
    if p.poll() is None:
        p.terminate()
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()


def tcp_exchange(port, payload=b"", want=b"", wait=25):
    """Connect to 127.0.0.1:port until `want` shows up in the reply (or timeout)."""
    import socket
    end = time.time() + wait
    got = b""
    while time.time() < end:
        try:
            c = socket.create_connection(("127.0.0.1", port), timeout=5)
            c.settimeout(5)
            if payload:
                c.sendall(payload)
            got = b""
            try:
                while want not in got:
                    d = c.recv(256)
                    if not d:
                        break
                    got += d
            except OSError:
                pass
            c.close()
            if want in got:
                return got
        except OSError:
            pass
        time.sleep(1)
    return got


def forwarding(ssh, user, pw, tmp):
    """TCP forwarding: -L (direct-tcpip), -R (tcpip-forward), no-port-forwarding, permitopen, permitlisten."""
    import socket
    import threading
    pub = open(ssh.key + ".pub").read().strip()

    def with_opts(opts):
        return set_keys(ssh, user, pw, f"{opts} {pub}" if opts else pub)

    # host-side echo server, the target of the -R tests
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(8)
    echo_port = srv.getsockname()[1]

    def serve():
        while True:
            try:
                c, _ = srv.accept()
            except OSError:
                return
            def one(c=c):
                try:
                    c.settimeout(10)
                    c.sendall(c.recv(256))
                except OSError:
                    pass
                c.close()
            threading.Thread(target=one, daemon=True).start()
    threading.Thread(target=serve, daemon=True).start()

    # -L to the guest's own sshd banner
    with_opts("")
    lp = free_port()
    p = fwd_client(ssh, user, "-L", f"{lp}:127.0.0.1:22")
    got = tcp_exchange(lp, want=b"SSH-")
    stop(p)
    step("fwd_local", got.startswith(b"SSH-"), f"got={got[:20]!r}")

    # -R back to the host echo server, read through a second -L to the guest's remote port
    rp = free_port() % 1000 + 17100
    p1 = fwd_client(ssh, user, "-R", f"127.0.0.1:{rp}:127.0.0.1:{echo_port}")
    lp2 = free_port()
    p2 = fwd_client(ssh, user, "-L", f"{lp2}:127.0.0.1:{rp}")
    got = tcp_exchange(lp2, payload=b"ping-R\n", want=b"ping-R")
    stop(p1)
    stop(p2)
    step("fwd_remote", b"ping-R" in got, f"got={got[:20]!r}")

    # -R with port 0: the server picks a port and reports it
    p = fwd_client(ssh, user, "-R", f"0:127.0.0.1:{echo_port}", "-v")
    time.sleep(6)
    alive = p.poll() is None
    stop(p)
    err = p.stderr.read().decode(errors="replace")
    step("fwd_remote_dynamic_port", alive and "Allocated port" in err, f"alive={alive} err={err.strip()[-120:]!r}")

    # no-port-forwarding / restrict: both directions refused
    for opt in ("no-port-forwarding", "restrict"):
        with_opts(opt)
        lp = free_port()
        p = fwd_client(ssh, user, "-L", f"{lp}:127.0.0.1:22")
        got = tcp_exchange(lp, want=b"SSH-", wait=12)
        stop(p)
        step(f"fwd_local_denied_{opt.replace('-', '_')}", not got.startswith(b"SSH-"), f"got={got[:20]!r}")
        p = fwd_client(ssh, user, "-R", f"127.0.0.1:{rp}:127.0.0.1:{echo_port}")
        try:
            rc = p.wait(timeout=25)
        except subprocess.TimeoutExpired:
            rc = None
        stop(p)
        step(f"fwd_remote_denied_{opt.replace('-', '_')}", rc not in (None, 0), f"rc={rc}")

    # permitopen: only the listed destination
    with_opts('permitopen="127.0.0.1:22"')
    lp = free_port()
    p = fwd_client(ssh, user, "-L", f"{lp}:127.0.0.1:22")
    got = tcp_exchange(lp, want=b"SSH-")
    stop(p)
    step("fwd_permitopen_allowed", got.startswith(b"SSH-"), f"got={got[:20]!r}")
    lp = free_port()
    p = fwd_client(ssh, user, "-L", f"{lp}:127.0.0.1:{echo_port}")
    got = tcp_exchange(lp, payload=b"x\n", want=b"x", wait=12)
    stop(p)
    step("fwd_permitopen_refused", b"x" not in got, f"got={got[:20]!r}")

    # permitlisten: only the listed remote port
    with_opts(f'permitlisten="{rp}"')
    p = fwd_client(ssh, user, "-R", f"127.0.0.1:{rp + 1}:127.0.0.1:{echo_port}")
    try:
        rc = p.wait(timeout=25)
    except subprocess.TimeoutExpired:
        rc = None
    stop(p)
    step("fwd_permitlisten_refused", rc not in (None, 0), f"rc={rc}")
    p = fwd_client(ssh, user, "-R", f"127.0.0.1:{rp}:127.0.0.1:{echo_port}")
    lp2 = free_port()
    p2 = fwd_client(ssh, user, "-L", f"{lp2}:127.0.0.1:{rp}")
    got = tcp_exchange(lp2, payload=b"ping-P\n", want=b"ping-P")
    stop(p)
    stop(p2)
    step("fwd_permitlisten_allowed", b"ping-P" in got, f"got={got[:20]!r}")
    srv.close()
    set_keys(ssh, user, pw, pub)


def sudo_sh(ssh, user, pw, script):
    # key login: a password login would itself trigger the rehash being tested
    return ssh.run(user, f"sudo -S -p '' sh -c \"{script}\"", key=True, stdin=(pw + "\n").encode())


def sudo_write(ssh, user, pw, path, text):
    """Replace `path` with `text` as root (single-quoted printf args; '$' escaped for the outer double quotes)."""
    args = " ".join("'" + l.replace("'", "'\\''") + "'" for l in text.splitlines())
    return sudo_sh(ssh, user, pw, ("printf '%s\\n' " + args + f" > {path}").replace("$", "\\$"))


def accounts_extra(ssh, user, pw, tmp):
    """keyboard-interactive, legacy $sha256$ rehash on login, expiry-time=, environment=."""
    import hashlib
    pub = open(ssh.key + ".pub").read().strip()

    def with_opts(opts):
        return set_keys(ssh, user, pw, f"{opts} {pub}" if opts else pub)

    # keyboard-interactive: one hidden Password prompt through the conversation
    argv = ["ssh", *ssh.kbd_opts(), "-p", str(ssh.port), f"{user}@{ssh.host}", "id -un"]
    r = subprocess.run(argv, capture_output=True, timeout=60, env=ssh.env(pw), stdin=subprocess.DEVNULL)
    step("kbdint_login", r.returncode == 0 and r.stdout.decode().strip() == user,
         f"rc={r.returncode} err={r.stderr.decode()[-120:]!r}")
    r = subprocess.run(argv, capture_output=True, timeout=60, env=ssh.env("wrong-" + pw), stdin=subprocess.DEVNULL)
    step("kbdint_wrong_password", r.returncode == 255, f"rc={r.returncode}")

    # expiry-time=
    ok = with_opts('expiry-time="20200101"')
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("akopt_expired", ok and rc == 255, f"rc={rc}")
    ok = with_opts('expiry-time="20990101Z"')
    rc, out, err = ssh.run(user, "id -un", key=True)
    step("akopt_not_expired", ok and rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")

    # environment= is ignored while PermitUserEnvironment is no (the default)
    ok = with_opts('environment="SSHSMOKE_ENV=leaked"')
    rc, out, err = ssh.run(user, "echo [$SSHSMOKE_ENV]", key=True)
    step("akopt_environment_default_no", ok and rc == 0 and out.strip() == "[]", f"rc={rc} out={out.strip()!r}")
    set_keys(ssh, user, pw, pub)

    # legacy $sha256$ shadow field: verifies, and a successful login rewrites it to $6$
    rc, orig, err = sudo_sh(ssh, user, pw, "cat /etc/shadow")
    if rc != 0 or f"\n{user}:" not in "\n" + orig:
        step("rehash_legacy", False, f"cannot read /etc/shadow via sudo rc={rc} err={err.strip()[-100:]!r}")
        return
    legacy = "$sha256$lnd0$" + hashlib.sha256(b"lnd0" + pw.encode()).hexdigest()
    lines = [(f"{user}:{legacy}:" + l.split(":", 2)[2]) if l.startswith(user + ":") else l for l in orig.splitlines()]
    rc, wout, err = sudo_write(ssh, user, pw, "/etc/shadow", "\n".join(lines) + "\n")
    rc2, mid, _ = sudo_sh(ssh, user, pw, "cat /etc/shadow")
    primed = rc == 0 and f"{user}:$sha256$" in mid
    rc, out, err = ssh.run(user, "id -un", password=pw)
    login_ok = rc == 0 and out.strip() == user
    time.sleep(1)
    rc3, after, _ = sudo_sh(ssh, user, pw, "cat /etc/shadow")
    new = [l for l in after.splitlines() if l.startswith(user + ":")]
    others_same = [l for l in after.splitlines() if not l.startswith(user + ":")] == \
                  [l for l in orig.splitlines() if not l.startswith(user + ":")]
    step("rehash_legacy", primed and login_ok and new and new[0].split(":")[1].startswith("$6$") and others_same,
         f"primed={primed} login={login_ok} field={(new[0][:30] if new else None)!r} others_same={others_same}")
    rc, out, err = ssh.run(user, "id -un", password=pw)
    step("rehash_login_again", rc == 0 and out.strip() == user, f"rc={rc}")
    sudo_write(ssh, user, pw, "/etc/shadow", orig)


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
    options(ssh, user, pw, tmp)
    forwarding(ssh, user, pw, tmp)
    accounts_extra(ssh, user, pw, tmp)
    if full_mode:
        full(ssh, user, pw, tmp)
    shutil.rmtree(tmp, ignore_errors=True)
    bad = [n for n, ok in RESULTS if not ok]
    print(f"SSHSMOKE summary: {len(RESULTS) - len(bad)}/{len(RESULTS)} passed"
          + (f"; failed: {' '.join(bad)}" if bad else ""), flush=True)
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
