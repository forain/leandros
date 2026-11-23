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
import socket
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
        self.proxy = None  # ProxyCommand reaching a second sshd through the first one

    def base_opts(self):
        return ["-F", "/dev/null", "-o", "StrictHostKeyChecking=no",
                "-o", f"UserKnownHostsFile={self.known}", "-o", "ConnectTimeout=20",
                "-o", "LogLevel=ERROR", *(["-o", f"ProxyCommand={self.proxy}"] if self.proxy else [])]

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
    # Native readers (lane utmpread): w / users inside a pty session list it.
    rc, out, err = ssh.run(user, "/bin/w; /bin/users", password=pw, extra=("-tt",))
    o = out.replace("\r", "")
    step("utmp_w_users_ssh", rc == 0 and "load average:" in o and "LOGIN@" in o
         and any(l.split()[:2] and l.split()[0] == user and l.split()[1].startswith("pts/") for l in o.splitlines())
         and o.strip().splitlines()[-1].split().count(user) >= 1, f"rc={rc} out={o.strip()[-400:]!r}")
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
    rc, out, err = ssh.run(user, f"/bin/lastlog -u {user}", password=pw)
    step("utmp_lastlog_reader", rc == 0 and any(l.split()[:1] == [user] and "pts/" in l and "Never" not in l
         for l in out.splitlines()), f"rc={rc} out={out.strip()!r}")
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
    if not os.path.exists(ssh.key):  # options() already made it
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

    # -R on a privileged port as a non-root user: refused (OpenSSH rule); a high port still works
    p = fwd_client(ssh, user, "-R", f"127.0.0.1:1023:127.0.0.1:{echo_port}")
    try:
        rc = p.wait(timeout=25)
    except subprocess.TimeoutExpired:
        rc = None
    stop(p)
    step("fwd_remote_privport_refused", rc not in (None, 0), f"rc={rc}")

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


def sudo_write(ssh, user, pw, path, text, then=""):
    """Replace `path` with `text` as root (single-quoted printf args; '$' escaped for the outer double quotes).
    `then` is more shell (no '$' or double quotes) run in the SAME sudo: with real PAM (musl crypt) a
    legacy $sha256$ hash cannot authenticate sudo, so nothing may need a second sudo while one is planted."""
    args = " ".join("'" + l.replace("'", "'\\''") + "'" for l in text.splitlines())
    return sudo_sh(ssh, user, pw, ("printf '%s\\n' " + args + f" > {path}").replace("$", "\\$") + (f"; {then}" if then else ""))


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
    rc, mid, err = sudo_write(ssh, user, pw, "/etc/shadow", "\n".join(lines) + "\n", then="cat /etc/shadow")
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


SSHD2_PORT = 2222
SSHD2_BIN = os.environ.get("SSHDCFG_BIN", "/usr/sbin/sshd")


def sshd2_start(ssh, user, pw, conf, env=""):
    """(Re)start a second sshd (root, port 2222, config `conf`) in the guest; True when it listens."""
    sudo_write(ssh, user, pw, "/tmp/sshdcfg.conf", conf)
    sudo_write(ssh, user, pw, "/tmp/sshdcfg-start.sh", "\n".join([
        "trap '' HUP",  # the ssh session's hangup must not take the daemon down (no setsid in the guest)
        "kill $(cat /tmp/sshdcfg.pid 2>/dev/null) 2>/dev/null; sleep 1",
        ": > /tmp/sshdcfg.log",
        f"{env} {SSHD2_BIN} -f /tmp/sshdcfg.conf -p {SSHD2_PORT} -e </dev/null >/tmp/sshdcfg.log 2>&1 &",
        "echo $! > /tmp/sshdcfg.pid",
        "for i in 1 2 3 4 5 6 7 8 9 10; do grep -q ready /tmp/sshdcfg.log && exit 0; sleep 1; done; exit 1",
    ]))
    rc, out, err = sudo_sh(ssh, user, pw, "sh /tmp/sshdcfg-start.sh")
    if rc != 0:
        _, lg, _ = sudo_sh(ssh, user, pw, "cat /tmp/sshdcfg.log /tmp/sshdcfg-start.sh")
        print(f"sshdcfg: second sshd did not start: rc={rc} out={out.strip()[-200:]!r} err={err.strip()[-200:]!r} log={lg[-600:]!r}", flush=True)
    return rc == 0


def cert_run(ssh2, user, ukey, cert, cmd, extra=()):
    argv = ["ssh", *ssh2.base_opts(), "-o", "PreferredAuthentications=publickey", "-o", "BatchMode=yes",
            "-o", "IdentitiesOnly=yes", "-i", ukey, *(["-o", f"CertificateFile={cert}"] if cert else []), *extra,
            "-p", str(ssh2.port), f"{user}@{ssh2.host}", cmd]
    r = subprocess.run(argv, capture_output=True, timeout=90, stdin=subprocess.DEVNULL)
    return r.returncode, r.stdout.decode(errors="replace"), r.stderr.decode(errors="replace")


def sshdcfg(ssh, user, pw, tmp):
    """sshd_config Match blocks, ForceCommand, AllowTcpForwarding and user certificates
    (TrustedUserCAKeys, authorized_keys cert-authority), AuthorizedPrincipalsFile, RevokedKeys (plain + KRL),
    Match Host/LocalAddress/RDomain, PermitUserEnvironment and the read-only /etc rehash, on a second sshd (port 2222) reached
    through the first one with ProxyCommand -W."""
    import socket
    import threading
    pub = open(ssh.key + ".pub").read().strip()
    set_keys(ssh, user, pw, pub)
    global SSHD2_BIN
    if os.environ.get("SSHDCFG_UPLOAD"):  # fail-first runs: test another sshd build as the second daemon
        with open(os.environ["SSHDCFG_UPLOAD"], "rb") as f:
            ssh.run(user, "cat > /tmp/sshd-under-test && chmod 755 /tmp/sshd-under-test", key=True, stdin=f.read(), timeout=300)
        SSHD2_BIN = "/tmp/sshd-under-test"
    ssh2 = Ssh("127.0.0.1", SSHD2_PORT, tmp)
    ssh2.key = ssh.key
    ssh2.known = os.path.join(tmp, "known_hosts2")
    ssh2.askpass = ssh.askpass
    ssh2.proxy = " ".join(["ssh", *ssh.key_opts(), "-p", str(ssh.port), "-W", "%h:%p", f"{user}@{ssh.host}"])
    base = "HostKey /etc/ssh/ssh_host_ed25519_key\nPermitRootLogin no\nPasswordAuthentication yes\nPubkeyAuthentication yes\n"

    def conf(extra):
        ok = sshd2_start(ssh, user, pw, base + extra)
        if not ok:
            step("cfg_second_sshd_starts", False, extra[:80].replace("\n", "|"))
        return ok

    def login(cmd="id -un", password=False):
        if password:
            return ssh2.run(user, cmd, password=pw)
        return ssh2.run(user, cmd, key=True)

    rc, gname, _ = ssh.run(user, "id -gn", key=True)
    gname = gname.strip() or user

    # --- Match blocks, ForceCommand ---------------------------------------
    if not conf("Match User nobody-such\n  PasswordAuthentication no\n"):
        return
    rc, out, err = login(password=True)
    step("cfg_match_user_other_keeps_password", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    conf("Match User " + user + "\n  PasswordAuthentication no\n")
    rc, out, err = login(password=True)
    step("cfg_match_user_disables_password", rc == 255, f"rc={rc}")
    rc, out, err = login()
    step("cfg_match_user_pubkey_still_ok", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    conf("Match User !" + user + "\n  PasswordAuthentication no\n")
    rc, out, err = login(password=True)
    step("cfg_match_user_negation", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    conf("PasswordAuthentication no\nMatch User " + user + "\n  PasswordAuthentication yes\n")
    rc, out, err = login(password=True)
    step("cfg_match_overrides_global", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    conf("PubkeyAuthentication no\nMatch User " + user + "\n  PubkeyAuthentication yes\n")
    rc, out, err = login()
    step("cfg_match_enables_pubkey", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")

    conf(f"Match Group {gname}\n  ForceCommand echo grp-forced\n")
    rc, out, err = login()
    step("cfg_match_group", out.strip() == "grp-forced", f"rc={rc} out={out.strip()!r}")
    conf("Match Address 127.0.0.0/8\n  ForceCommand echo addr-forced\n")
    rc, out, err = login()
    step("cfg_match_address", out.strip() == "addr-forced", f"rc={rc} out={out.strip()!r}")
    conf("Match Address 10.99.*,!10.99.1.1\n  ForceCommand echo addr-forced\n")
    rc, out, err = login()
    step("cfg_match_address_miss", rc == 0 and out.strip() == user, f"rc={rc} out={out.strip()!r}")
    conf(f"Match User {user} LocalPort {SSHD2_PORT}\n  ForceCommand echo lp-forced\nMatch User {user} LocalPort 1\n  ForceCommand echo wrong\n")
    rc, out, err = login()
    step("cfg_match_localport_all_criteria", out.strip() == "lp-forced", f"rc={rc} out={out.strip()!r}")
    conf(f"Match User {user} LocalPort 1\n  ForceCommand echo wrong\nMatch all\n  ForceCommand echo all-forced\n")
    rc, out, err = login()
    step("cfg_match_all", out.strip() == "all-forced", f"rc={rc} out={out.strip()!r}")
    conf(f"Match User {user}\n  ForceCommand echo first\nMatch all\n  ForceCommand echo second\n")
    rc, out, err = login()
    step("cfg_match_first_value_wins", out.strip() == "first", f"rc={rc} out={out.strip()!r}")
    conf("ForceCommand echo orig=$SSH_ORIGINAL_COMMAND\n")
    rc, out, err = login("hello world")
    step("cfg_forcecommand_original_command", out.strip() == "orig=hello world", f"rc={rc} out={out.strip()!r}")
    rc, out, err = sudo_sh(ssh, user, pw, f"printf 'Match Bogus x\\\\n' > /tmp/sshdcfg-bad.conf; {SSHD2_BIN} -t -f /tmp/sshdcfg-bad.conf")
    step("cfg_match_bad_criterion_rejected", rc != 0, f"rc={rc} err={err.strip()[-100:]!r}")

    # --- AllowTcpForwarding -----------------------------------------------
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(8)
    echo_port = srv.getsockname()[1]

    def echo():
        while True:
            try:
                c, _ = srv.accept()
            except OSError:
                return
            try:
                c.sendall(c.recv(256))
            except OSError:
                pass
            c.close()

    threading.Thread(target=echo, daemon=True).start()

    def can_local():  # -L to the guest's own port 22 (the target is resolved by the guest)
        lp = free_port()
        p = fwd_client(ssh2, user, "-L", f"{lp}:127.0.0.1:22")
        got = tcp_exchange(lp, want=b"SSH-", wait=15)
        stop(p)
        return got.startswith(b"SSH-")

    def can_remote():  # -R on the second sshd, read back through a -L on the first
        rp, lp = free_port(), free_port()
        p = fwd_client(ssh2, user, "-R", f"127.0.0.1:{rp}:127.0.0.1:{echo_port}")
        p2 = fwd_client(ssh, user, "-L", f"{lp}:127.0.0.1:{rp}")
        got = tcp_exchange(lp, payload=b"ping-R\n", want=b"ping-R", wait=15)
        stop(p)
        stop(p2)
        return b"ping-R" in got

    for mode, want_l, want_r in (("yes", True, True), ("all", True, True), ("local", True, False),
                                 ("remote", False, True), ("no", False, False)):
        conf(f"AllowTcpForwarding {mode}\n")
        l, r = can_local(), can_remote()
        step(f"cfg_allowtcpforwarding_{mode}", (l, r) == (want_l, want_r), f"local={l} remote={r}")
    conf(f"AllowTcpForwarding yes\nMatch User {user}\n  AllowTcpForwarding local\n")
    l, r = can_local(), can_remote()
    step("cfg_allowtcpforwarding_match", (l, r) == (True, False), f"local={l} remote={r}")
    set_keys(ssh, user, pw, pub)
    conf("AllowTcpForwarding no\n")  # the key allows forwarding, the config does not
    l, r = can_local(), can_remote()
    set_keys(ssh, user, pw, f"no-port-forwarding {pub}")
    conf("AllowTcpForwarding yes\n")  # the key forbids it, the config allows
    l2 = can_local()
    set_keys(ssh, user, pw, pub)
    step("cfg_allowtcpforwarding_most_restrictive", not l and not r and not l2, f"cfg_no: l={l} r={r} key_no: l={l2}")
    srv.close()

    # --- user certificates --------------------------------------------------
    def keygen(*a):
        return subprocess.run(["ssh-keygen", "-q", "-N", "", *a], capture_output=True).returncode == 0

    ca, ca2, uk = (os.path.join(tmp, n) for n in ("ca", "ca2", "ukey"))
    ok = keygen("-t", "ed25519", "-f", ca) and keygen("-t", "ed25519", "-f", ca2) and keygen("-t", "ed25519", "-f", uk)

    def sign(n, caf=ca, principals=user, *opts):
        cp = os.path.join(tmp, f"{n}.pub")
        shutil.copy(uk + ".pub", cp)
        r = subprocess.run(["ssh-keygen", "-q", "-s", caf, "-I", n, "-n", principals, *opts, cp], capture_output=True)
        return os.path.join(tmp, f"{n}-cert.pub") if r.returncode == 0 else None

    if not ok:
        step("cert_keygen", False, "ssh-keygen failed")
        return
    cap = open(ca + ".pub").read().strip()
    sudo_write(ssh, user, pw, "/tmp/sshdcfg-ca.pub", cap)
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\n")
    rc, out, err = cert_run(ssh2, user, uk, sign("ok"), "id -un")
    step("cert_trusted_ca_login", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, None, "id -un")
    step("cert_bare_key_refused", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("badp", ca, "someoneelse"), "id -un")
    step("cert_wrong_principal", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("two", ca, f"x,{user},y"), "id -un")
    step("cert_principal_in_list", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("exp", ca, user, "-V", "20200101:20200102"), "id -un")
    step("cert_expired", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("future", ca, user, "-V", "20990101:20991231"), "id -un")
    step("cert_not_yet_valid", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("other", ca2), "id -un")
    step("cert_untrusted_ca", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("fc", ca, user, "-O", "force-command=echo cert-forced"), "id -un")
    step("cert_force_command", out.strip() == "cert-forced", f"rc={rc} out={out.strip()!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("srcno", ca, user, "-O", "source-address=10.99.0.0/16"), "id -un")
    step("cert_source_address_refused", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("srcok", ca, user, "-O", "source-address=127.0.0.0/8"), "id -un")
    step("cert_source_address_allowed", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("unk", ca, user, "-O", "critical:xyz=1"), "id -un")
    step("cert_unknown_critical_option", rc == 255, f"rc={rc}")
    c = sign("hostc", ca, user, "-h")
    rc, out, err = cert_run(ssh2, user, uk, c, "id -un") if c else (255, "", "")
    step("cert_host_cert_refused", rc == 255, f"rc={rc}")
    for kt in ("ecdsa", "rsa"):
        k2 = os.path.join(tmp, "uk-" + kt)
        if keygen("-t", kt, "-f", k2):
            subprocess.run(["ssh-keygen", "-q", "-s", ca, "-I", kt, "-n", user, k2 + ".pub"], capture_output=True)
            rc, out, err = cert_run(ssh2, user, k2, k2 + "-cert.pub", "id -un")
            step(f"cert_{kt}_user_key", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")

    # authorized_keys cert-authority
    conf("")
    set_keys(ssh, user, pw, f"{pub}\ncert-authority {cap}")
    rc, out, err = cert_run(ssh2, user, uk, sign("ak1"), "id -un")
    step("cert_authority_line_login", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, ca, None, "id -un")
    step("cert_authority_key_is_not_a_login_key", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("ak2", ca2), "id -un")
    step("cert_authority_other_ca_refused", rc == 255, f"rc={rc}")
    set_keys(ssh, user, pw, f'{pub}\ncert-authority,principals="deploy,ops-*" {cap}')
    rc, out, err = cert_run(ssh2, user, uk, sign("ak3", ca, "deploy"), "id -un")
    step("cert_authority_principals_option", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("ak4", ca, user), "id -un")
    step("cert_authority_principals_excludes_username", rc == 255, f"rc={rc}")
    set_keys(ssh, user, pw, f'{pub}\ncert-authority,from="10.99.*" {cap}')
    rc, out, err = cert_run(ssh2, user, uk, sign("ak5"), "id -un")
    step("cert_authority_from_option", rc == 255, f"rc={rc}")
    set_keys(ssh, user, pw, f'{pub}\ncommand="echo line-forced",cert-authority {cap}')
    rc, out, err = cert_run(ssh2, user, uk, sign("ak6"), "id -un")
    step("cert_authority_command_option", out.strip() == "line-forced", f"rc={rc} out={out.strip()!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("ak7", ca, user, "-O", "force-command=echo other"), "id -un")
    step("cert_force_command_conflict", rc == 255, f"rc={rc}")
    # --- Match Host / LocalAddress / RDomain ---------------------------------
    set_keys(ssh, user, pw, pub)

    def match_case(name, crit, forced):
        conf(f"Match {crit}\n  ForceCommand echo mx\n")
        rc, out, err = login()
        want = "mx" if forced else user
        step(name, rc == 0 and out.strip() == want, f"rc={rc} out={out.strip()!r} want={want!r}")

    match_case("cfg_match_host_pattern", "Host 127.0.0.*", True)
    match_case("cfg_match_host_other", "Host 10.*,192.168.*", False)
    match_case("cfg_match_host_negated", "Host !127.0.0.1,*", False)
    match_case("cfg_match_host_star_negated_other", "Host *,!10.1.1.1", True)
    match_case("cfg_match_host_and_user", f"Host 127.0.0.1 User {user}", True)
    match_case("cfg_match_localaddress_cidr", "LocalAddress 127.0.0.0/8", True)
    match_case("cfg_match_localaddress_other_cidr", "LocalAddress 10.0.0.0/8", False)
    match_case("cfg_match_localaddress_list", "LocalAddress 192.168.0.0/16,127.0.0.1", True)
    match_case("cfg_match_localaddress_negated", "LocalAddress 127.0.0.0/8,!127.0.0.1", False)
    match_case("cfg_match_localaddress_exact", "LocalAddress 127.0.0.1", True)
    match_case("cfg_match_rdomain_default", "RDomain default", True)
    match_case("cfg_match_rdomain_other", "RDomain blue", False)
    match_case("cfg_match_rdomain_negated", "RDomain !default", False)
    match_case("cfg_match_rdomain_star", "RDomain *", True)
    match_case("cfg_match_and_mismatch", "Host 127.0.0.1 LocalAddress 10.0.0.0/8", False)
    match_case("cfg_match_and_all_match", f"Host 127.0.0.1 LocalAddress 127.0.0.0/8 RDomain default LocalPort {SSHD2_PORT}", True)

    # --- AuthorizedPrincipalsFile (certificates signed by a TrustedUserCAKeys CA) ----
    def put(path, data):
        r = ssh.run(user, f"cat > {path}", key=True, stdin=data if isinstance(data, bytes) else data.encode())
        return r[0] == 0

    sudo_sh(ssh, user, pw, "mkdir -p /etc/ssh/principals; chmod 755 /etc/ssh/principals")
    sudo_write(ssh, user, pw, f"/etc/ssh/principals/{user}", "# principals\ndeploy\n\nops\nrelease")
    sudo_sh(ssh, user, pw, f"chmod 644 /etc/ssh/principals/{user}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\nAuthorizedPrincipalsFile /etc/ssh/principals/%u\n")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf1", ca, "deploy"), "id -un")
    step("cfg_principals_file_listed", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf2", ca, "x,release"), "id -un")
    step("cfg_principals_file_second_cert_principal", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf3", ca, user), "id -un")
    step("cfg_principals_file_username_not_implicit", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf4", ca, "stranger"), "id -un")
    step("cfg_principals_file_unlisted", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf5", ca, "# principals"), "id -un")
    step("cfg_principals_file_comment_not_a_name", rc == 255, f"rc={rc}")
    sudo_sh(ssh, user, pw, f"chmod 664 /etc/ssh/principals/{user}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf6", ca, "deploy"), "id -un")
    step("cfg_principals_file_strict_modes", rc == 255, f"rc={rc}")
    sudo_sh(ssh, user, pw, f"chmod 644 /etc/ssh/principals/{user}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\nAuthorizedPrincipalsFile /etc/ssh/principals/missing-%u\n")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf7", ca, user), "id -un")
    step("cfg_principals_file_missing_refuses", rc == 255, f"rc={rc}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\nAuthorizedPrincipalsFile none\n")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf8", ca, user), "id -un")
    step("cfg_principals_file_none_uses_username", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\nAuthorizedPrincipalsFile %h/.sshd-principals\n")
    put(".sshd-principals", 'viahome\ncommand="echo princ-forced" cmdname\nfrom="10.99.*" remote-only\n')
    sudo_sh(ssh, user, pw, f"chmod 600 ~{user}/.sshd-principals 2>/dev/null; chown {user} ~{user}/.sshd-principals 2>/dev/null; true")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf9", ca, "viahome"), "id -un")
    step("cfg_principals_file_home_token", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf10", ca, "cmdname"), "id -un")
    step("cfg_principals_file_line_command_option", rc == 0 and out.strip() == "princ-forced", f"rc={rc} out={out.strip()!r}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf11", ca, "remote-only"), "id -un")
    step("cfg_principals_file_line_from_option", rc == 255, f"rc={rc}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\nAuthorizedPrincipalsFile %u/%q\n")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf12", ca, user), "id -un")
    step("cfg_principals_file_unknown_token", rc == 255, f"rc={rc}")
    sudo_write(ssh, user, pw, f"/etc/ssh/principals/%{user}", "pct")
    sudo_sh(ssh, user, pw, f"chmod 644 /etc/ssh/principals/%{user}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nAuthorizedKeysFile /dev/null\nAuthorizedPrincipalsFile /etc/ssh/principals/%%%u\n")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf13", ca, "pct"), "id -un")
    step("cfg_principals_file_percent_token", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    # not consulted for a cert-authority line of authorized_keys (that has principals=)
    conf("AuthorizedPrincipalsFile /etc/ssh/principals/missing-%u\n")
    set_keys(ssh, user, pw, f"{pub}\ncert-authority {cap}")
    rc, out, err = cert_run(ssh2, user, uk, sign("pf14", ca, user), "id -un")
    step("cfg_principals_file_not_for_cert_authority_line", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    set_keys(ssh, user, pw, pub)
    sudo_sh(ssh, user, pw, "rm -rf /etc/ssh/principals")

    # --- RevokedKeys: plain list ------------------------------------------------
    cabits = open(ca + ".pub").read()
    ca2bits = open(ca2 + ".pub").read()
    ukbits = open(uk + ".pub").read()
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nRevokedKeys /tmp/sshdcfg-revoked\n")
    put("/tmp/sshdcfg-revoked", "# nothing revoked yet\n" + ca2bits)
    rc, out, err = login()
    step("cfg_revoked_plain_other_key_listed_login_ok", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    c1 = sign("rv1")
    rc, out, err = cert_run(ssh2, user, uk, c1, "id -un")
    step("cfg_revoked_plain_cert_ok", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    put("/tmp/sshdcfg-revoked", "# the login key\n" + pub + "\n")
    rc, out, err = login()
    step("cfg_revoked_plain_key_refused", rc == 255, f"rc={rc}")
    put("/tmp/sshdcfg-revoked", cabits)
    rc, out, err = cert_run(ssh2, user, uk, sign("rv2"), "id -un")
    step("cfg_revoked_plain_ca_refused", rc == 255, f"rc={rc}")
    rc, out, err = login()
    step("cfg_revoked_plain_ca_leaves_plain_key", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")
    put("/tmp/sshdcfg-revoked", ukbits)
    rc, out, err = cert_run(ssh2, user, uk, sign("rv3"), "id -un")
    step("cfg_revoked_plain_cert_subject_key_refused", rc == 255, f"rc={rc}")
    put("/tmp/sshdcfg-revoked", open(c1).read())
    rc, out, err = cert_run(ssh2, user, uk, c1, "id -un")
    step("cfg_revoked_plain_cert_itself_refused", rc == 255, f"rc={rc}")
    sudo_sh(ssh, user, pw, "rm -f /tmp/sshdcfg-revoked")
    rc, out, err = login()
    step("cfg_revoked_unreadable_refuses_key", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("rv4"), "id -un")
    step("cfg_revoked_unreadable_refuses_cert", rc == 255, f"rc={rc}")
    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nRevokedKeys none\n")
    rc, out, err = login()
    step("cfg_revoked_none_login_ok", rc == 0 and out.strip() == user, f"rc={rc} err={err.strip()[-100:]!r}")

    # --- RevokedKeys: OpenSSH binary KRL (ssh-keygen -k) -------------------------
    def krl(name, spec, ca_pub=None):
        sp, out = os.path.join(tmp, name + ".spec"), os.path.join(tmp, name + ".krl")
        with open(sp, "w") as f:
            f.write("\n".join(spec) + "\n")
        if os.path.exists(out):
            os.unlink(out)
        r = subprocess.run(["ssh-keygen", "-q", "-k", "-f", out, *(["-s", ca_pub] if ca_pub else []), sp], capture_output=True)
        if r.returncode != 0:
            print(f"sshdcfg: ssh-keygen -k {name}: {r.stderr.decode(errors='replace')[-200:]!r}", flush=True)
            return None
        return open(out, "rb").read()

    def use_krl(data):
        return data is not None and put("/tmp/sshdcfg-revoked", data)

    conf("TrustedUserCAKeys /tmp/sshdcfg-ca.pub\nRevokedKeys /tmp/sshdcfg-revoked\n")
    ok = use_krl(krl("k1", ["key: " + ca2bits.strip()]))
    rc, out, err = login()
    step("cfg_krl_unrelated_key_login_ok", ok and rc == 0 and out.strip() == user, f"ok={ok} rc={rc} err={err.strip()[-100:]!r}")
    ok = use_krl(krl("k2", ["key: " + pub]))
    rc, out, err = login()
    step("cfg_krl_explicit_key_refused", ok and rc == 255, f"ok={ok} rc={rc}")
    ok = use_krl(krl("k3", ["sha256: " + pub]))
    rc, out, err = login()
    step("cfg_krl_sha256_key_refused", ok and rc == 255, f"ok={ok} rc={rc}")
    ok = use_krl(krl("k4", ["key: " + ca2bits.strip()]))
    ok = use_krl(krl("k4b", ["sha256: " + ca2bits.strip()])) and ok
    rc, out, err = login()
    step("cfg_krl_sha256_other_key_login_ok", ok and rc == 0 and out.strip() == user, f"ok={ok} rc={rc} err={err.strip()[-100:]!r}")
    ok = use_krl(krl("k5", ["key: " + cabits.strip()]))
    rc, out, err = cert_run(ssh2, user, uk, sign("kr1"), "id -un")
    step("cfg_krl_ca_key_refuses_cert", ok and rc == 255, f"ok={ok} rc={rc}")
    ok = use_krl(krl("k6", ["key: " + ukbits.strip()]))
    rc, out, err = cert_run(ssh2, user, uk, sign("kr2"), "id -un")
    step("cfg_krl_cert_subject_key_refused", ok and rc == 255, f"ok={ok} rc={rc}")
    ok = use_krl(krl("k7", ["serial: 1234", "serial: 2000-2005", "serial: 3001", "serial: 3003", "serial: 3005", "serial: 3007", "id: revoked-id"], ca + ".pub"))
    for n, serial, expect in (("ser", "1234", False), ("rng_lo", "2000", False), ("rng_mid", "2003", False), ("rng_hi", "2005", False),
                              ("rng_after", "2006", True), ("bm_hit", "3005", False), ("bm_gap", "3004", True), ("bm_last", "3007", False),
                              ("unlisted", "77", True)):
        rc, out, err = cert_run(ssh2, user, uk, sign("ks_" + n, ca, user, "-z", serial), "id -un")
        step(f"cfg_krl_serial_{n}", ok and (rc == 0 and out.strip() == user if expect else rc == 255), f"ok={ok} serial={serial} rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("revoked-id"), "id -un")
    step("cfg_krl_key_id_refused", ok and rc == 255, f"ok={ok} rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("other-id", ca, user, "-z", "55"), "id -un")
    step("cfg_krl_other_key_id_ok", ok and rc == 0 and out.strip() == user, f"ok={ok} rc={rc} err={err.strip()[-100:]!r}")
    ok = use_krl(krl("k8", ["serial: 4000"], ca2 + ".pub"))
    rc, out, err = cert_run(ssh2, user, uk, sign("kc1", ca, user, "-z", "4000"), "id -un")
    step("cfg_krl_serial_is_scoped_to_its_ca", ok and rc == 0 and out.strip() == user, f"ok={ok} rc={rc} err={err.strip()[-100:]!r}")
    put("/tmp/sshdcfg-revoked", b"SSHKRL\n\x00\x00\x00\x00\x01garbage")
    rc, out, err = login()
    step("cfg_krl_malformed_refuses_key", rc == 255, f"rc={rc}")
    rc, out, err = cert_run(ssh2, user, uk, sign("kc2"), "id -un")
    step("cfg_krl_malformed_refuses_cert", rc == 255, f"rc={rc}")

    # --- PermitUserEnvironment yes + environment= (second config) ----------------
    envkeys = f'environment="SSHSMOKE_ENV=seen",environment="SSHSMOKE_B=two words" {pub}'
    set_keys(ssh, user, pw, envkeys)
    conf("PermitUserEnvironment yes\n")
    rc, out, err = login("echo [$SSHSMOKE_ENV][$SSHSMOKE_B]")
    step("cfg_environment_option_permitted", rc == 0 and out.strip() == "[seen][two words]", f"rc={rc} out={out.strip()!r}")
    rc, out, err = login("echo [$HOME][$USER]")
    step("cfg_environment_option_keeps_login_env", rc == 0 and out.strip() != "[][]", f"rc={rc} out={out.strip()!r}")
    conf("PermitUserEnvironment no\n")
    rc, out, err = login("echo [$SSHSMOKE_ENV]")
    step("cfg_environment_option_denied_when_no", rc == 0 and out.strip() == "[]", f"rc={rc} out={out.strip()!r}")
    conf(f"PermitUserEnvironment no\nMatch User {user}\n  PermitUserEnvironment yes\n")
    rc, out, err = login("echo [$SSHSMOKE_ENV]")
    step("cfg_environment_option_via_match", rc == 0 and out.strip() == "[seen]", f"rc={rc} out={out.strip()!r}")
    conf(f"PermitUserEnvironment yes\nMatch User {user}\n  PermitUserEnvironment no\n")
    rc, out, err = login("echo [$SSHSMOKE_ENV]")
    step("cfg_environment_option_match_turns_off", rc == 0 and out.strip() == "[]", f"rc={rc} out={out.strip()!r}")
    set_keys(ssh, user, pw, pub)
    conf("PermitUserEnvironment yes\n")
    rc, out, err = login("echo [$SSHSMOKE_ENV]")
    step("cfg_environment_permitted_but_none_set", rc == 0 and out.strip() == "[]", f"rc={rc} out={out.strip()!r}")

    # --- $sha256$ rehash with a read-only root (/etc is on it) ---------------------
    import hashlib
    rc, orig, err = sudo_sh(ssh, user, pw, "cat /etc/shadow")
    if rc != 0 or f"\n{user}:" not in "\n" + orig:
        step("cfg_rehash_readonly_etc", False, f"cannot read /etc/shadow rc={rc}")
    else:
        legacy = "$sha256$lnd0$" + hashlib.sha256(b"lnd0" + pw.encode()).hexdigest()
        lines = [(f"{user}:{legacy}:" + l.split(":", 2)[2]) if l.startswith(user + ":") else l for l in orig.splitlines()]
        conf("")
        # The guest script remounts / read-only for a window, records what it sees, and remounts it read-write.
        sudo_write(ssh, user, pw, "/tmp/sshdcfg-ro.sh", "\n".join([
            "trap '' HUP",
            "rm -f /tmp/sshdcfg-ro-*",
            "sleep 3",
            "mount -o remount,ro /",
            "touch /etc/sshdcfg-ro-probe 2>/dev/null; echo $? > /tmp/sshdcfg-ro-probe",
            "sleep 14",
            f"grep '^{user}:' /etc/shadow | cut -d: -f2 | cut -c1-8 > /tmp/sshdcfg-ro-field",
            "ls /etc/shadow.sshd-new > /tmp/sshdcfg-ro-new 2>&1; echo $? >> /tmp/sshdcfg-ro-new",
            "cp /tmp/sshdcfg.log /tmp/sshdcfg-ro-log",
            "mount -o remount,rw /",
            "rm -f /etc/sshdcfg-ro-probe",
            "chmod 644 /tmp/sshdcfg-ro-*",
            "echo done > /tmp/sshdcfg-ro-done",
        ]))
        # One sudo plants the legacy hash and starts the script (sudo cannot authenticate against it afterwards).
        sudo_write(ssh, user, pw, "/etc/shadow", "\n".join(lines) + "\n",
                   then="chmod 755 /tmp/sshdcfg-ro.sh; sh /tmp/sshdcfg-ro.sh > /dev/null 2>&1 < /dev/null &")
        time.sleep(7)  # the root is read-only from here on
        rc, out, err = login(password=True)
        step("cfg_rehash_readonly_login_still_works", rc == 0 and out.strip() == user, f"rc={rc} out={out.strip()!r} err={err.strip()[-100:]!r}")
        for _ in range(40):
            rc, done, _ = ssh.run(user, "cat /tmp/sshdcfg-ro-done", key=True)
            if rc == 0 and "done" in done:
                break
            time.sleep(3)
        rc, got, _ = ssh.run(user, "cd /tmp; cat sshdcfg-ro-probe sshdcfg-ro-field sshdcfg-ro-new sshdcfg-ro-log", key=True)
        parts = got.splitlines()
        step("cfg_rehash_etc_was_readonly", bool(parts) and parts[0].strip() not in ("", "0"), f"got={got[:200]!r}")
        step("cfg_rehash_readonly_keeps_legacy_hash", len(parts) > 2 and parts[1].strip() == "$sha256$" and "No such file" in got, f"got={got[:300]!r}")
        step("cfg_rehash_readonly_failure_logged", "shadow hash upgrade failed" in got and "upgraded from" not in got, f"got={got[-300:]!r}")
        rc, out, err = login(password=True)
        time.sleep(1)
        rc2, after, _ = sudo_sh(ssh, user, pw, "cat /etc/shadow")
        new = [l for l in after.splitlines() if l.startswith(user + ":")]
        step("cfg_rehash_after_remount_rw_upgrades", rc == 0 and bool(new) and new[0].split(":")[1].startswith("$6$"), f"rc={rc} field={(new[0][:30] if new else None)!r}")
        sudo_write(ssh, user, pw, "/etc/shadow", orig)
    set_keys(ssh, user, pw, pub)
    sudo_sh(ssh, user, pw, "mount -o remount,rw / 2>/dev/null; kill $(cat /tmp/sshdcfg.pid); rm -f /tmp/sshdcfg*; rm -rf /etc/ssh/principals")


PS_SSHD = r"""for p in /proc/[0-9]*; do
read k n < $p/status
m=${n#sshd}
if [ x$m != x$n ]; then pp=$(grep PPid $p/status); echo ${p#/proc/} $pp $n; fi
done"""


def sshd_procs(ssh, user, pw):
    """[(pid, ppid, comm)] of every sshd* process in the guest."""
    sudo_write(ssh, user, pw, "/tmp/sshps.sh", PS_SSHD)
    rc, out, err = sudo_sh(ssh, user, pw, "sh /tmp/sshps.sh")
    res = []
    if not out.strip():
        print(f"sshd_procs: empty listing rc={rc} out={out[-200:]!r} err={err.strip()[-200:]!r}", flush=True)
    for ln in out.splitlines():
        p = ln.split()
        if len(p) == 4 and p[1] == "PPid:" and p[0].isdigit() and p[2].isdigit():
            res.append((int(p[0]), int(p[2]), p[3]))
    return res


def listener_pid(procs):
    pids = {p for p, _, _ in procs}
    c = [p for p, pp, n in procs if n == "sshd" and pp not in pids]
    return c[0] if c else None


def probe_open(ssh):
    s = socket.create_connection((ssh.host, ssh.port), timeout=10)
    banner = s.recv(256)
    s.sendall(b"SSH-2.0-sshsmoke-probe\r\n")
    return s, banner.startswith(b"SSH-")


def probe_eof(s, secs):
    """True when the peer closes the probe socket within `secs`."""
    end = time.time() + secs
    while time.time() < end:
        s.settimeout(max(0.2, end - time.time()))
        try:
            if s.recv(4096) == b"":
                return True
        except socket.timeout:
            pass
        except OSError:
            return True
    return False


def new_preauth(ssh, user, pw, before):
    """Open a probe; return (sock, pid owning it): the new sshd-preauth if one exists, else the listener."""
    s, ok = probe_open(ssh)
    time.sleep(1)
    procs = sshd_procs(ssh, user, pw)
    known = {p for p, _, _ in before}
    pre = [p for p, _, n in procs if n == "sshd-preauth" and p not in known]
    return s, (pre[0] if pre else listener_pid(procs)), bool(pre)


PS_DETAIL = r"""for p in /proc/[0-9]*; do
read k n < $p/status
m=${n#sshd}
if [ x$m != x$n ]; then
pp=; uid=; nnp=; sc=
while read k v w; do
case $k in PPid:) pp=$v;; Uid:) uid=$v,$w;; NoNewPrivs:) nnp=$v;; Seccomp:) sc=$v;; esac
done < $p/status
rt=$(readlink $p/root)
kf=0
for f in $p/fd/*; do case $(readlink $f) in /etc/ssh*|/etc/shadow*) kf=$((kf+1));; esac; done
echo ${p#/proc/}\\|$pp\\|$n\\|$uid\\|$nnp\\|$sc\\|$rt\\|$kf
fi
done"""


def sshd_detail(ssh, user, pw):
    """[dict(pid, ppid, comm, uid, nnp, seccomp, root, keyfds)] for every sshd* process."""
    sudo_write(ssh, user, pw, "/tmp/sshdet.sh", PS_DETAIL)
    _, out, _ = sudo_sh(ssh, user, pw, "sh /tmp/sshdet.sh")
    res = []
    for ln in out.splitlines():
        f = ln.split("|")
        if len(f) == 8 and f[0].isdigit():
            res.append(dict(pid=int(f[0]), ppid=int(f[1] or 0), comm=f[2], uid=f[3], nnp=f[4], seccomp=f[5],
                            root=f[6], keyfds=int(f[7] or 0)))
    return res


def privsep_isolation(ssh, user, pw):
    """The pre-auth network child is unprivileged, jailed, NNP and seccomp'd, and holds no host-key fd."""
    s, ok = probe_open(ssh)
    time.sleep(1)
    d = sshd_detail(ssh, user, pw)
    pre = [p for p in d if p["comm"] == "sshd-preauth"]
    det = f"preauth={[(p['pid'], p['uid']) for p in pre]} all={[(p['pid'], p['comm'], p['uid']) for p in d]}"
    step("privsep_preauth_uid", ok and bool(pre) and all(
        all(u != "0" for u in p["uid"].split(",")) for p in pre), det)
    step("privsep_preauth_chroot_nnp", bool(pre) and all(p["nnp"] == "1" and p["root"] == "/var/empty" for p in pre),
         str([(p["nnp"], p["root"]) for p in pre]))
    step("privsep_preauth_seccomp", bool(pre) and all(p["seccomp"] == "2" for p in pre),
         str([p["seccomp"] for p in pre]))
    step("privsep_no_hostkey_fd", bool(pre) and all(p["keyfds"] == 0 for p in d if p["comm"].startswith("sshd-")),
         str([(p["comm"], p["keyfds"]) for p in d]))
    s.close()
    # Post-auth: the network process stays uid 22; a root monitor (not the listener) is its parent.
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe("ssh", ["ssh", *ssh.pw_opts(), "-tt", "-p", str(ssh.port), f"{user}@{ssh.host}", "cat"], ssh.env(pw))
    try:
        time.sleep(4)
        d = sshd_detail(ssh, user, pw)
        byp = {p["pid"]: p for p in d}
        nets = [p for p in d if p["comm"] == "sshd-net"]
        good = [p for p in nets if p["uid"].split(",")[0] == "22" and byp.get(p["ppid"], {}).get("uid", "x").split(",")[0] == "0"
                and byp.get(p["ppid"], {}).get("comm") == "sshd" and byp[p["ppid"]]["ppid"] in byp]
        step("privsep_postauth_uid", bool(good), f"nets={[(p['pid'], p['uid'], p['ppid']) for p in nets]}")
    finally:
        try:
            os.write(fd, b"\x04")
            os.close(fd)
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except OSError:
            pass


def privsep_hooks(ssh, user, pw, tmp):
    """Test-only hooks (env SSHD_TEST_HOOK, honoured by the net child): a forbidden syscall kills only that
    connection, and a forged 'auth OK' from the child is refused by the monitor."""
    conf = "PasswordAuthentication yes\nPubkeyAuthentication yes\n"
    for hook, name in (("badsyscall", "privsep_forbidden_syscall_kills_conn"), ("forgeauth", "privsep_forged_auth_refused")):
        if not sshd2_start(ssh, user, pw, conf, env=f"SSHD_TEST_HOOK={hook}"):
            step(name, False, "second sshd did not start")
            continue
        lp = free_port()
        tun = subprocess.Popen(["ssh", *ssh.pw_opts(), "-N", "-L", f"{lp}:127.0.0.1:{SSHD2_PORT}", "-p", str(ssh.port),
                                f"{user}@{ssh.host}"], env=ssh.env(pw), stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(3)
        key = os.path.join(tmp, "hookkey")
        if not os.path.exists(key):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
        # The key must be authorized so the child reaches the signature step (where forgeauth tampers).
        set_keys(ssh, user, pw, open(key + ".pub").read().strip() + "\n" + open(ssh.key + ".pub").read().strip())
        r = subprocess.run(["ssh", "-i", key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
                            "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=20", "-p", str(lp), f"{user}@127.0.0.1", "id -u"],
                           capture_output=True, text=True, timeout=60)
        _, lg, _ = sudo_sh(ssh, user, pw, "cat /tmp/sshdcfg.log")
        procs = sshd_procs(ssh, user, pw)
        listener_up = listener_pid(procs) is not None
        if hook == "badsyscall":
            ok = r.returncode != 0 and "uid=" not in r.stdout and listener_up and ssh.run(user, "id -un", pw, timeout=30)[0] == 0
        else:
            ok = r.returncode != 0 and r.stdout.strip() != "0" and "forged" in lg
        step(name, ok, f"rc={r.returncode} out={r.stdout.strip()!r} listener={listener_up} log={lg[-200:]!r}")
        stop(tun)
        sudo_sh(ssh, user, pw, "kill $(cat /tmp/sshdcfg.pid) 2>/dev/null; true")
    if os.path.exists(ssh.key + ".pub"):
        set_keys(ssh, user, pw, open(ssh.key + ".pub").read().strip())


def privsep_grace(ssh, user, pw, tmp):
    """A connection process that hangs pre-auth is killed by the listener after LoginGraceTime."""
    conf = "LoginGraceTime 4\nPasswordAuthentication yes\n"
    if not sshd2_start(ssh, user, pw, conf):
        step("privsep_grace_kill", False, "second sshd did not start")
        return
    lp = free_port()
    tun = subprocess.Popen(["ssh", *ssh.pw_opts(), "-N", "-L", f"{lp}:127.0.0.1:{SSHD2_PORT}", "-p", str(ssh.port),
                            f"{user}@{ssh.host}"], env=ssh.env(pw), stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(40):
        try:
            s = socket.create_connection(("127.0.0.1", lp), timeout=5)
            if s.recv(256).startswith(b"SSH-"):
                break
            s.close()
        except OSError:
            time.sleep(0.5)
    else:
        stop(tun)
        step("privsep_grace_kill", False, "cannot reach the second sshd through the tunnel")
        return
    s.sendall(b"SSH-2.0-sshsmoke-probe\r\n")
    time.sleep(1)
    procs = sshd_procs(ssh, user, pw)
    pre = [p for p, _, n in procs if n == "sshd-preauth"]
    if pre:
        sudo_sh(ssh, user, pw, f"kill -STOP {pre[0]}")  # hung: it cannot disconnect itself
    ok = bool(pre) and probe_eof(s, 25)
    if pre:
        sudo_sh(ssh, user, pw, f"kill -KILL {pre[0]} 2>/dev/null; true")
    s.close()
    stop(tun)
    sudo_sh(ssh, user, pw, "kill $(cat /tmp/sshdcfg.pid) 2>/dev/null; true")
    step("privsep_grace_kill", ok, "" if ok else f"preauth child {'found' if pre else 'missing'}, no EOF within 25 s")


def second_tunnel(ssh, user, pw, conf, env=""):
    """Start the second sshd and an ssh -L tunnel to it; return (tunnel Popen, local port) or (None, why)."""
    if not sshd2_start(ssh, user, pw, conf, env=env):
        return None, "second sshd did not start"
    lp = free_port()
    tun = subprocess.Popen(["ssh", *ssh.pw_opts(), "-N", "-L", f"{lp}:127.0.0.1:{SSHD2_PORT}", "-p", str(ssh.port),
                            f"{user}@{ssh.host}"], env=ssh.env(pw), stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(40):
        try:
            s = socket.create_connection(("127.0.0.1", lp), timeout=5)
            if s.recv(256).startswith(b"SSH-"):
                s.close()
                return tun, lp
            s.close()
        except OSError:
            time.sleep(0.5)
    stop(tun)
    return None, "cannot reach the second sshd through the tunnel"


def privsep_stalled_monitor(ssh, user, pw):
    """A monitor that stops (SIGSTOP) pre-auth cannot enforce LoginGraceTime itself: the listener's backstop
    kills the connection process after LoginGraceTime + 10 s and the client sees EOF."""
    tun, lp = second_tunnel(ssh, user, pw, "LoginGraceTime 4\nPasswordAuthentication yes\n")
    if tun is None:
        step("privsep_stalled_monitor_backstop", False, lp)
        return
    s = socket.create_connection(("127.0.0.1", lp), timeout=10)
    s.recv(256)
    s.sendall(b"SSH-2.0-sshsmoke-probe\r\n")
    time.sleep(1)
    procs = sshd_procs(ssh, user, pw)
    pre = [(p, pp) for p, pp, n in procs if n == "sshd-preauth"]
    if pre:
        sudo_sh(ssh, user, pw, f"kill -STOP {pre[0][1]}")  # only the monitor hangs; the child stays live
    ok = bool(pre) and probe_eof(s, 40)
    if pre:
        sudo_sh(ssh, user, pw, f"kill -KILL {pre[0][1]} {pre[0][0]} 2>/dev/null; true")
    s.close()
    stop(tun)
    sudo_sh(ssh, user, pw, "kill $(cat /tmp/sshdcfg.pid) 2>/dev/null; true")
    step("privsep_stalled_monitor_backstop", ok, "" if ok else f"preauth {'found' if pre else 'missing'}, no EOF within 40 s")


def privsep_highfd(ssh, user, pw):
    """An fd above 1023 open in the monitor (hook highfd) must not survive into the net child."""
    tun, lp = second_tunnel(ssh, user, pw, "PasswordAuthentication yes\n", env="SSHD_TEST_HOOK=highfd")
    if tun is None:
        step("privsep_confine_closes_high_fds", False, lp)
        return
    s = socket.create_connection(("127.0.0.1", lp), timeout=10)
    s.recv(256)
    s.sendall(b"SSH-2.0-sshsmoke-probe\r\n")
    time.sleep(2)
    det = [d for d in sshd_detail(ssh, user, pw) if d["comm"] == "sshd-preauth"]
    ok = bool(det) and all(d["keyfds"] == 0 for d in det)
    s.close()
    stop(tun)
    sudo_sh(ssh, user, pw, "kill $(cat /tmp/sshdcfg.pid) 2>/dev/null; true")
    step("privsep_confine_closes_high_fds", ok, f"preauth={[(d['pid'], d['keyfds']) for d in det]}")


PS_RSS = r"""for p in /proc/[0-9]*; do
read k n < $p/status
m=${n#sshd}
if [ x$m != x$n ]; then
pp=; rss=; uid=
while read k v w; do
case $k in PPid:) pp=$v;; VmRSS:) rss=$v;; esac
done < $p/status
echo ${p#/proc/}\\|$pp\\|$n\\|$rss
fi
done"""


def privsep_perf(ssh, user, pw, tmp):
    """Measurements, not gates: RSS (kB) of the listener, a monitor and a net child with one authenticated
    session open, and ssh pipe / scp throughput over the forwarded QEMU port. Printed as PERF lines."""
    sudo_write(ssh, user, pw, "/tmp/sshrss.sh", PS_RSS)
    holders = [subprocess.Popen(["ssh", *ssh.pw_opts(), "-p", str(ssh.port), f"{user}@{ssh.host}", "sleep 60"],
                                env=ssh.env(pw), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL) for _ in range(4)]
    time.sleep(6)
    rc_, out, err_ = sudo_sh(ssh, user, pw, "sh /tmp/sshrss.sh")
    if not out.strip():
        print(f"PERF rss script empty: rc={rc_} err={err_.strip()[-200:]!r}", flush=True)
    rows = [ln.split("|") for ln in out.splitlines() if ln.count("|") == 3]
    pids = {r[0] for r in rows}
    lis = [r for r in rows if r[2] == "sshd" and r[1] not in pids]
    mon = [r for r in rows if r[2] == "sshd" and r[1] in {x[0] for x in lis}]
    pre = [r for r in rows if r[2] in ("sshd-preauth", "sshd-net")]
    def kb(rs):
        v = [int(r[3].split()[0]) for r in rs if r[3].split()]
        return f"{min(v)}..{max(v)}" if v else "n/a"
    print(f"PERF rss_kB listener={kb(lis)} monitor_or_session={kb(mon)} netchild={kb(pre)} connections=4 rows={len(rows)}", flush=True)
    print("PERF rows " + " ".join(f"{r[0]}:{r[1]}:{r[2]}:{r[3].split()[0] if r[3].split() else 0}" for r in rows), flush=True)
    for h in holders:
        stop(h)
    mb = 16
    src = os.path.join(tmp, "perf.bin")
    with open(src, "wb") as f:
        f.write(os.urandom(1 << 20) * mb)
    t0 = time.time()
    r1 = subprocess.run(["ssh", *ssh.pw_opts(), "-p", str(ssh.port), f"{user}@{ssh.host}", "cat > /tmp/perf.bin"],
                        env=ssh.env(pw), stdin=open(src, "rb"), capture_output=True, timeout=300)
    up = time.time() - t0
    t0 = time.time()
    r2 = subprocess.run(["ssh", *ssh.pw_opts(), "-p", str(ssh.port), f"{user}@{ssh.host}", "cat /tmp/perf.bin"],
                        env=ssh.env(pw), stdin=subprocess.DEVNULL, capture_output=True, timeout=300)
    down = time.time() - t0
    ok = r1.returncode == 0 and r2.returncode == 0 and len(r2.stdout) == mb << 20
    print(f"PERF ssh_pipe MiB/s up={mb / up:.2f} down={mb / down:.2f} size={mb}MiB ok={ok}", flush=True)
    t0 = time.time()
    r4 = subprocess.run(["scp", *ssh.pw_opts(), "-P", str(ssh.port), src, f"{user}@{ssh.host}:/tmp/perf3.bin"],
                        env=ssh.env(pw), stdin=subprocess.DEVNULL, capture_output=True, timeout=300)
    sc = time.time() - t0
    print(f"PERF scp(sftp) MiB/s up={mb / sc:.2f} rc={r4.returncode} {r4.stderr.decode()[-80:]!r}", flush=True)
    sudo_sh(ssh, user, pw, "rm -f /tmp/perf.bin /tmp/perf2.bin /tmp/perf3.bin")
    step("privsep_perf_measured", ok and bool(pre), f"rows={len(rows)} netchild_found={bool(pre)}")


def privsep_crash(ssh, user, pw):
    """Crash isolation: a SEGV in one connection's process must not take the listener or other sessions down."""
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe("ssh", ["ssh", *ssh.pw_opts(), "-tt", "-p", str(ssh.port), f"{user}@{ssh.host}", "cat"], ssh.env(pw))

    def say(line, want, secs=10):
        os.write(fd, line.encode() + b"\n")
        end, got = time.time() + secs, b""
        while time.time() < end and want.encode() not in got:
            r, _, _ = select.select([fd], [], [], 0.5)
            if r:
                try:
                    got += os.read(fd, 4096)
                except OSError:
                    break
        return want.encode() in got

    try:
        b_ok = say("hello-b", "hello-b", 20)
        before = sshd_procs(ssh, user, pw)
        lpid = listener_pid(before)
        a, victim, isolated = new_preauth(ssh, user, pw, before)
        if victim:
            # A Rust process survives the first user-sent SIGSEGV exactly as on Linux: std's
            # stack-overflow handler (installed in every Rust main) is not fatal for a signal that
            # is not a guard-page fault, resets the disposition to SIG_DFL and returns. The second
            # one is then the real crash.
            sig = os.environ.get("CRASH_SIG", "SEGV")
            sudo_sh(ssh, user, pw, f"kill -{sig} {victim}; sleep 1; kill -{sig} {victim} 2>/dev/null; true")
        t0 = time.time()
        a_eof = probe_eof(a, 8)
        eof_s = time.time() - t0
        if not a_eof and victim:
            _, dg, _ = sudo_sh(ssh, user, pw, f"grep -E '^(Name|State|PPid|SigPnd|ShdPnd|SigBlk|SigCgt)' /proc/{victim}/status; cat /proc/{victim}/wchan; ls -l /proc/{lpid}/fd | wc -l; ls -l /proc/{lpid}/fd | grep -c pidfd; ls /proc/{victim}/fd 2>&1 | wc -l")
            print(f"DIAG victim={victim} listener={lpid}: {dg.strip()!r}", flush=True)
        a.close()
        after = sshd_procs(ssh, user, pw)
        gone = victim not in {p for p, _, _ in after}
        same = listener_pid(after) == lpid and lpid is not None
        b_still = say("hello-b2", "hello-b2")
        rc, out, _ = ssh.run(user, "id -un", pw, timeout=30)
        login = rc == 0 and user in out
        step("privsep_crash_isolated", b_ok and isolated and same and b_still and login and a_eof,
             f"procs={after} victim={victim} gone={gone} eof_s={eof_s:.1f} session_b={b_ok} preauth_child={isolated} probe_eof={a_eof} listener_unchanged={same} b_survives={b_still} new_login={login}")
    finally:
        try:
            os.write(fd, b"\x04")
            os.close(fd)
        except OSError:
            pass
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except OSError:
            pass


def privsep_count(ssh, user, pw):
    """Six crashes (init gives up after 5 restarts in 60 s): the listener must outlive them all. Run LAST."""
    procs = sshd_procs(ssh, user, pw)
    lpid = listener_pid(procs)
    for _ in range(6):
        s, victim, _iso = new_preauth(ssh, user, pw, sshd_procs(ssh, user, pw))
        if victim:
            # A Rust process survives the first user-sent SIGSEGV exactly as on Linux: std's
            # stack-overflow handler (installed in every Rust main) is not fatal for a signal that
            # is not a guard-page fault, resets the disposition to SIG_DFL and returns. The second
            # one is then the real crash.
            sig = os.environ.get("CRASH_SIG", "SEGV")
            sudo_sh(ssh, user, pw, f"kill -{sig} {victim}; sleep 1; kill -{sig} {victim} 2>/dev/null; true")
        probe_eof(s, 5)
        s.close()
        time.sleep(0.5)
    time.sleep(2)
    r = subprocess.run(["ssh-keyscan", "-T", "5", "-p", str(ssh.port), ssh.host], capture_output=True, timeout=30)
    same = listener_pid(sshd_procs(ssh, user, pw)) == lpid and lpid is not None
    step("privsep_listener_survives_count", b"ssh-" in r.stdout and same,
         f"keyscan={b'ssh-' in r.stdout} listener_unchanged={same}")


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
    sshdcfg(ssh, user, pw, tmp)
    if "--only-crash" in argv:
        for _ in range(int(os.environ.get("CRASH_REPEAT", "1"))):
            privsep_crash(ssh, user, pw)
        bad = [n for n, ok in RESULTS if not ok]
        print(f"SSHSMOKE summary: {len(RESULTS) - len(bad)}/{len(RESULTS)} passed" + (f"; failed: {' '.join(bad)}" if bad else ""), flush=True)
        return 0 if not bad else 1
    if full_mode:
        full(ssh, user, pw, tmp)
    privsep_isolation(ssh, user, pw)
    privsep_hooks(ssh, user, pw, tmp)
    privsep_grace(ssh, user, pw, tmp)
    privsep_stalled_monitor(ssh, user, pw)
    privsep_highfd(ssh, user, pw)
    privsep_perf(ssh, user, pw, tmp)
    privsep_crash(ssh, user, pw)
    privsep_count(ssh, user, pw)  # last: on a tree without per-connection processes it leaves the guest without sshd
    shutil.rmtree(tmp, ignore_errors=True)
    bad = [n for n, ok in RESULTS if not ok]
    print(f"SSHSMOKE summary: {len(RESULTS) - len(bad)}/{len(RESULTS)} passed"
          + (f"; failed: {' '.join(bad)}" if bad else ""), flush=True)
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
