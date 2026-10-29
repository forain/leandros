#!/usr/bin/env python3
"""remote.py - iterate on a running LeandrOS machine over SSH.

The real laptop (or any machine booted from a write-usb.sh stick), or a QEMU
VM as a stand-in. Uses the Mac's OpenSSH client (ssh, scp, sftp) against the
guest's sshd (userland/sshd) as root, by key, and /usr/sbin/espctl in the
guest for the boot partition (kernel slots, console log).

Target:
  LEANDROS_REMOTE=host[:port] | user@host[:port] | qemu[:<run-id>]
      (or --target). "qemu" reads the hostfwd port that driver.py /
      run-qemu.sh recorded for LEANDROS_RUN_ID (/tmp/leandros-<id>-ssh).
  LEANDROS_REMOTE_KEY    private key (default ~/.ssh/leandros_remote)
  LEANDROS_REMOTE_USER   default root

Commands:
  keygen                 create ~/.ssh/leandros_remote (ed25519) if missing
  setup                  one-time steps, and a check that key login works
  discover [--subnet CIDR]  find LeandrOS sshds (russh banner) on the LAN
  status                 kernel, uptime, cmdline, kernel slots
  run <cmd...>           run a command (exit status passed through)
  shell                  interactive shell
  push <local> <remote>  upload a file or directory, sha256-verified,
                         atomically, skipping identical files. Never writes
                         the target in place: chunks go to <dest>.part.<id>,
                         resumed after stalls/drops from the verified prefix
                         (LEANDROS_PUSH_STALL s, _RETRIES, _CHUNK), full-file
                         sha256 checked, then mv over <dest>; a mismatch
                         deletes the part and leaves the old file intact
  pull <remote> <local>  download a file or directory, sha256-verified
  deploy-files <pairs|--manifest F>  upload many files: LOCAL:REMOTE[:MODE]
  deploy-kernel [--arch A] [--kernel F] [--initrd F] [--cmdline ARGS] [--netwatch S] [--no-arm]
                [--reboot [--promote]]
                         install the release build's kernel + initrd as the
                         one-shot "trial" boot entry (sha256-verified on the
                         ESP); with --reboot boot it and check it is running.
                         --netwatch S appends netwatch=S to the trial entry's
                         cmdline only (never the good entry's): a trial kernel
                         with no IPv4 address S s after boot, or whose gateway
                         stops answering for 300 s (netwatch.reach=) at any
                         time, reboots itself to the good entry. Default 180 on
                         real machines, off on qemu; --netwatch 0 = off.
                         --trial-max M adds netwatch.trial_max=M (minutes): an
                         un-promoted trial reboots to good after M minutes
  promote | discard      trial -> good / drop the trial entry
  espctl <args...>       run espctl in the guest
  logs [--out DIR]       console log (all 4 boots), status, /var/log, ... into
                         artifacts/remote-logs/<time>-<host>/
  reboot [--no-wait] [--timeout S] [--no-logs]
                         sync + reboot, wait for sshd, report boot time,
                         fetch logs
  wait [--timeout S]     wait until sshd answers
  forget-host            drop the target's host key (after a re-flash)

Raspberry Pi 5 (LEANDROS_REMOTE=pi, host from LEANDROS_PI_HOST, or pi:<host>).
The Pi's boot FAT is not reachable from LeandrOS (the SD driver exposes only
the f2fs root partition), so kernels live in the root at /boot/pi/<slot>/ and
are entered by kexec. The card's kernel.img (what the firmware boots after a
power cycle) is never written: a bad trial is recovered by power-cycling.
  pi-deploy [--kernel F] [--initrd F] [--append ARGS] [--no-exec] [--timeout S]
                         upload the rpi5 build (target/final-aarch64/kernel-direct
                         + initrd-aarch64.cpio) to /boot/pi/trial, kexec into it,
                         wait for sshd and check uname -v changed
  pi-boot <good|prev|trial> [--append ARGS]   kexec into a slot (e.g. back to
                         good after a power cycle booted the card's kernel.img)
  pi-promote             trial -> good (old good -> prev), in the root only
  pi-status              slots, running kernel, cmdline
  pi-sync [--dry-run] [--list] [--board B]
                         update the root fs in place from the current aarch64
                         build (sha256 diff, changed files only; keeps
                         /etc/leandros, /home, /root, shadow, ssh keys; merges
                         passwd/group). See scripts/pi_sync.py.
"""

import argparse
import hashlib
import ipaddress
import os
import re
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor

REPO = os.path.realpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
DEFAULT_KEY = os.path.expanduser("~/.ssh/leandros_remote")
KNOWN_HOSTS = os.path.expanduser("~/.ssh/leandros_known_hosts")
STAGE_DIR = "/tmp/remdbg"


def die(msg, code=1):
    print(f"remote: {msg}", file=sys.stderr)
    sys.exit(code)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# ── Target and ssh plumbing ─────────────────────────────────────────────────

class Target:
    def __init__(self, spec):
        self.qemu = False
        self.user = os.environ.get("LEANDROS_REMOTE_USER", "root")
        if not spec:
            die("no target: set LEANDROS_REMOTE=host[:port] (the laptop's address; "
                "`remote.py discover` finds it) or LEANDROS_REMOTE=qemu for a VM")
        self.pi = False
        if spec == "pi" or spec.startswith("pi:"):
            spec = spec[3:] if spec.startswith("pi:") else os.environ.get("LEANDROS_PI_HOST", "")
            if not spec:
                die("LEANDROS_REMOTE=pi needs the Pi's address: LEANDROS_PI_HOST=<ip> or LEANDROS_REMOTE=pi:<ip>")
            self.pi = True
        if spec == "qemu" or spec.startswith("qemu:"):
            run_id = spec[5:] if spec.startswith("qemu:") else os.environ.get("LEANDROS_RUN_ID", "")
            state = f"/tmp/leandros{'-' + run_id if run_id else ''}-ssh"
            try:
                parts = open(state).read().split()
            except FileNotFoundError:
                die(f"{state} missing: start the VM with driver.py/run-qemu.sh (LEANDROS_RUN_ID={run_id})")
            if parts[0] == "vmnet":
                # socket_vmnet: the guest's own DHCP address, from its serial log.
                log = f"/tmp/leandros{'-' + run_id if run_id else ''}-serial-full.log"
                # Every interface prints a lease line `... address: X (ifname)`;
                # the vmnet NIC is eth0, so take eth0's last one (a line
                # without `(ifname)` comes from an older kernel: eth0).
                try:
                    found = [m.group(1) for m in re.finditer(
                        rb"DHCP configured, address: ([0-9.]+)(?: \(([A-Za-z0-9_.-]+)\))?",
                        open(log, "rb").read()) if m.group(2) in (None, b"eth0")]
                except FileNotFoundError:
                    found = []
                if not found:
                    die(f"vmnet VM: no eth0 DHCP lease in {log} yet")
                self.host, self.port = found[-1].decode(), 22
            else:
                self.host, self.port = parts[0], int(parts[1])
            self.qemu = True
        else:
            if "@" in spec:
                self.user, spec = spec.split("@", 1)
            if spec.count(":") == 1:
                h, p = spec.split(":")
                self.host, self.port = h, int(p)
            else:
                self.host, self.port = spec, 22
        key = os.environ.get("LEANDROS_REMOTE_KEY", DEFAULT_KEY)
        self.key = os.path.expanduser(key) if os.path.exists(os.path.expanduser(key)) else None
        # Not tempfile.gettempdir(): macOS's per-user $TMPDIR is long enough that
        # ssh's ".<16 random chars>" suffix overflows sun_path (104 bytes).
        self.control = f"/tmp/lr-{os.getuid()}-{self.host}-{self.port}.sock"

    def label(self):
        return f"{self.user}@{self.host}:{self.port}"

    def opts(self, master=True):
        o = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=8",
             "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=4"]
        if self.key:
            o += ["-i", self.key, "-o", "IdentitiesOnly=yes"]
        if self.qemu:
            # A VM's host key changes with every image; it is local anyway.
            o += ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
        else:
            o += ["-o", f"UserKnownHostsFile={KNOWN_HOSTS}", "-o", "StrictHostKeyChecking=accept-new"]
        if master:
            o += ["-o", "ControlMaster=auto", "-o", f"ControlPath={self.control}", "-o", "ControlPersist=60"]
        return o

    def ssh_argv(self, cmd=None, tty=False, master=True):
        a = ["ssh", "-p", str(self.port), *self.opts(master)]
        if tty:
            a.append("-t")
        a.append(f"{self.user}@{self.host}")
        if cmd is not None:
            a.append(cmd)
        return a

    def run(self, cmd, check=True, capture=True, timeout=None, input=None, master=True):
        """Run `cmd` (a shell string for the guest's shell)."""
        try:
            p = subprocess.run(self.ssh_argv(cmd, master=master), capture_output=capture,
                               timeout=timeout, input=input)
        except subprocess.TimeoutExpired:
            if check:
                die(f"timed out: {cmd}")
            return None
        if capture and p.returncode == 255:
            explain_ssh_failure(self, p.stderr.decode(errors="replace"))
            if check:
                die(f"ssh to {self.label()} failed")
        if check and p.returncode != 0:
            out = (p.stdout or b"").decode(errors="replace") + (p.stderr or b"").decode(errors="replace")
            die(f"`{cmd}` exited {p.returncode}:\n{out.rstrip()}")
        return p

    def out(self, cmd, **kw):
        return self.run(cmd, **kw).stdout.decode(errors="replace")

    def close_master(self):
        subprocess.run(["ssh", "-p", str(self.port), "-o", f"ControlPath={self.control}", "-O", "exit",
                        f"{self.user}@{self.host}"], capture_output=True)

    def scp(self, *args):
        a = ["scp", "-q", "-P", str(self.port), *self.opts(), *args]
        p = subprocess.run(a, capture_output=True)
        if p.returncode != 0:
            die(f"scp failed: {p.stderr.decode(errors='replace').strip()}")

    def sftp_batch(self, lines):
        with tempfile.NamedTemporaryFile("w", suffix=".sftp", delete=False) as f:
            f.write("\n".join(lines) + "\n")
            batch = f.name
        try:
            a = ["sftp", "-q", "-P", str(self.port), *self.opts(), "-b", batch, f"{self.user}@{self.host}"]
            p = subprocess.run(a, capture_output=True)
            if p.returncode != 0:
                die(f"sftp failed: {p.stderr.decode(errors='replace').strip()}")
        finally:
            os.unlink(batch)

    def remote(self, path):
        return f"{self.user}@{self.host}:{path}"


def explain_ssh_failure(t, err):
    sys.stderr.write(err)
    if "REMOTE HOST IDENTIFICATION HAS CHANGED" in err or "Host key verification failed" in err:
        print(f"remote: the host key of {t.host} changed (a re-flashed stick generates a new one on first "
              f"boot). If that is expected: scripts/remote.py forget-host", file=sys.stderr)
    elif "Permission denied" in err:
        print("remote: key login refused. The stick needs your key: scripts/remote.py setup", file=sys.stderr)
    elif "Connection refused" in err or "timed out" in err or "No route" in err:
        print(f"remote: {t.host}:{t.port} is not answering (booting? wrong address? `remote.py discover`)",
              file=sys.stderr)


def port_open(host, port, timeout=1.5):
    try:
        with socket.create_connection((host, port), timeout=timeout) as s:
            s.settimeout(timeout)
            return s.recv(64)
    except OSError:
        return None


# ── File transfer ───────────────────────────────────────────────────────────

def remote_shas(t, paths):
    """{path: sha256} of the remote files that exist."""
    res = {}
    for i in range(0, len(paths), 100):
        chunk = paths[i:i + 100]
        p = rrun(t, "sha256sum -- " + " ".join(shlex.quote(x) for x in chunk) + " 2>/dev/null",
                 timeout=900, tries=4, what="sha256sum")
        for line in p.stdout.decode(errors="replace").splitlines():
            parts = line.split(None, 1)
            if len(parts) == 2 and len(parts[0]) == 64:
                res[parts[1].lstrip("*")] = parts[0]
    return res


# ── Robust file upload: .part + resume + verify + atomic rename ─────────────
#
# Never writes the destination in place. Each file goes to <dest>.part.<id>
# (id = first 12 hex of the NEW content's sha256, so a later run of the same
# push resumes the same part file), in chunks over `ssh ... cat >> part`, each
# chunk with a stall watchdog and a remote-side offset check + chunk hash.
# After any failure the remote part is re-measured, its un-verified tail is
# hash-compared with the local file, and the transfer resumes from there. Only a
# part whose full sha256 matches is renamed over <dest> (the old file stays
# until that rename); on a mismatch the part is deleted and <dest> is untouched.

CHUNK = int(os.environ.get("LEANDROS_PUSH_CHUNK", 4 << 20))
STALL_S = float(os.environ.get("LEANDROS_PUSH_STALL", 30))      # no progress for this long = stalled
MAX_FAILS = int(os.environ.get("LEANDROS_PUSH_RETRIES", 10))    # consecutive failures without progress
BACKOFF_MAX = 30


def sha256_range(path, a, b):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        f.seek(a)
        left = b - a
        while left > 0:
            d = f.read(min(1 << 20, left))
            if not d:
                break
            h.update(d)
            left -= len(d)
    return h.hexdigest()


def rrun(t, cmd, timeout=60, tries=6, what=None):
    """t.run with a timeout and bounded retries (exponential backoff) for the
    small control commands. Returns the CompletedProcess of the first run that
    got a real answer from the remote shell (rc != 255, no timeout)."""
    for i in range(tries):
        try:
            p = subprocess.run(t.ssh_argv(cmd), capture_output=True, timeout=timeout)
            if p.returncode != 255:
                return p
        except subprocess.TimeoutExpired:
            pass
        t.close_master()
        time.sleep(min(BACKOFF_MAX, 2 ** i))
    die(f"remote unreachable: {what or cmd[:60]} failed {tries} times")


def remote_size(t, path):
    p = rrun(t, f"stat -c %s {shlex.quote(path)} 2>/dev/null || echo -1", what="stat")
    try:
        return int(p.stdout.decode().split()[-1])
    except (ValueError, IndexError):
        return -1


def send_chunk(t, part, off, data, stall_s):
    """Append `data` at remote offset `off` of `part`. Returns True only if the
    remote saw exactly `off` bytes before, and hashes the appended bytes to the
    same sha256 afterwards. Kills ssh when no byte moved for stall_s seconds."""
    q = shlex.quote(part)
    n = len(data)
    cmd = (f'[ "$(stat -c %s {q} 2>/dev/null || echo 0)" = {off} ] || exit 42; '
           f"cat >> {q} || exit 43; tail -c {n} {q} | sha256sum")
    p = subprocess.Popen(t.ssh_argv(cmd), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE)
    st = {"last": time.time(), "done": False}

    def writer():
        try:
            for i in range(0, n, 65536):
                p.stdin.write(data[i:i + 65536])
                p.stdin.flush()
                st["last"] = time.time()
            p.stdin.close()
            st["last"] = time.time()
            st["done"] = True
        except (BrokenPipeError, OSError, ValueError):
            pass

    th = threading.Thread(target=writer, daemon=True)
    th.start()
    while p.poll() is None:
        if time.time() - st["last"] > stall_s:
            p.kill()
            p.wait()
            return False, "stalled"
        time.sleep(0.1)
    th.join(2)
    out = p.stdout.read().decode(errors="replace").split()
    if p.returncode != 0:
        return False, f"rc {p.returncode}"
    if not out or out[0] != hashlib.sha256(data).hexdigest():
        return False, "chunk hash mismatch"
    return True, ""


def stabilize(t, part):
    """After a failed chunk the remote cat may still be draining: wait until the
    part's size stops changing, then return it."""
    last = remote_size(t, part)
    for _ in range(10):
        time.sleep(1.5)
        cur = remote_size(t, part)
        if cur == last:
            return cur
        last = cur
    return last


class Progress:
    def __init__(self, name, total):
        self.name, self.total, self.t0, self.last = name, total, time.time(), 0
        self.base = 0
        self.retries = 0

    def show(self, done, force=False):
        now = time.time()
        if not force and now - self.last < 2:
            return
        self.last = now
        rate = (done - self.base) / max(now - self.t0, 1e-3) / 1e6
        pct = 100 * done / self.total if self.total else 100
        sys.stderr.write(f"  {self.name}: {done / 1e6:.1f}/{self.total / 1e6:.1f} MB {pct:.0f}%  "
                         f"{rate:.2f} MB/s  retries {self.retries}\n")
        sys.stderr.flush()


def upload_one(t, local, dest, sha, mode, quiet=False):
    """Returns after dest has been atomically replaced by `local`, verified."""
    total = os.path.getsize(local)
    part = f"{dest}.part.{sha[:12]}"
    qp, qd = shlex.quote(part), shlex.quote(dest)
    prog = Progress(os.path.basename(dest), total)
    for restart in range(3):
        verified = 0
        fails = 0
        S = remote_size(t, part)
        if S > total:
            rrun(t, f"rm -f {qp}")
            S = 0
        # resume: hash-check the un-verified remote prefix against the local file
        if S > 0:
            got = rrun(t, f"head -c {S} {qp} | sha256sum", timeout=max(60, S / 1e6)).stdout.decode().split()
            if got and got[0] == sha256_range(local, 0, S):
                verified = S
                prog.base = S
                if not quiet:
                    sys.stderr.write(f"  {prog.name}: resuming at {S} bytes (prefix sha256 verified)\n")
            else:
                rrun(t, f"rm -f {qp}")
                S = 0
        with open(local, "rb") as f:
            while verified < total:
                f.seek(verified)
                data = f.read(min(CHUNK, total - verified))
                ok, why = send_chunk(t, part, verified, data, STALL_S)
                if ok:
                    verified += len(data)
                    fails = 0
                    if not quiet:
                        prog.show(verified)
                    continue
                fails += 1
                prog.retries += 1
                t.close_master()
                if fails > MAX_FAILS:
                    die(f"{dest}: {fails} consecutive failures ({why}); gave up. "
                        f"Old file untouched; partial upload kept as {part} (rerun to resume)")
                wait = min(BACKOFF_MAX, 2 ** fails)
                sys.stderr.write(f"  {prog.name}: chunk at {verified} failed ({why}); retry {fails}/{MAX_FAILS} "
                                 f"in {wait}s\n")
                time.sleep(wait)
                S = stabilize(t, part)
                if S == verified:
                    continue
                if S < verified or S > total:
                    # remote lost data (or grew weirdly): re-verify from scratch
                    verified = 0
                    rrun(t, f"rm -f {qp}")
                    continue
                # S > verified: a partial chunk landed; hash-check [verified, S)
                got = rrun(t, f"tail -c +{verified + 1} {qp} | head -c {S - verified} | sha256sum").stdout.decode().split()
                if got and got[0] == sha256_range(local, verified, S):
                    verified = S
                else:
                    rrun(t, f"truncate -s {verified} {qp}")
        # whole-file check, then atomic rename
        got = rrun(t, f"sha256sum -- {qp}", timeout=max(120, total / 1e6)).stdout.decode().split()
        if got and got[0] == sha:
            rrun(t, f"chmod {mode} {qp} && mv -f {qp} {qd} && rm -f {qd}.part.* ; sync")
            return got[0]
        sys.stderr.write(f"  {prog.name}: FINAL sha256 MISMATCH (remote {got[0][:16] if got else '?'} != {sha[:16]}); "
                         f"deleting part, {dest} left untouched\n")
        rrun(t, f"rm -f {qp}")
    die(f"{dest}: final sha256 mismatch after 3 attempts; refused to rename, old file intact")


def upload(t, pairs, force=False, quiet=False):
    """pairs: [(local, remote, mode or None)]. Changed files only, each via
    <remote>.part.<id> (resumable, sha256-verified), then atomically renamed."""
    local = {r: (l, sha256_file(l), m) for l, r, m in pairs}
    have = {} if force else remote_shas(t, list(local))
    todo = [(r, l, s, m) for r, (l, s, m) in local.items() if have.get(r) != s]
    skipped = len(local) - len(todo)
    if not todo:
        if not quiet:
            print(f"all {len(local)} file(s) already up to date")
        return 0
    dirs = sorted({os.path.dirname(r) for r, *_ in todo if os.path.dirname(r)})
    if dirs:
        rrun(t, "mkdir -p -- " + " ".join(shlex.quote(d) for d in dirs))
    t0 = time.time()
    total = sum(os.path.getsize(l) for _, l, _, _ in todo)
    for r, l, s, m in todo:
        mode = m if m is not None else oct(os.stat(l).st_mode & 0o7777)[2:]
        got = upload_one(t, l, r, s, mode, quiet=quiet)
        if not quiet:
            print(f"{r}: sha256 {got} OK")
    dt = time.time() - t0
    if not quiet:
        rate = total / dt / 1e6 if dt > 0 else 0
        print(f"uploaded {len(todo)} file(s), {total} bytes in {dt:.1f} s ({rate:.1f} MB/s), "
              f"sha256 verified; {skipped} unchanged")
    return len(todo)


def cmd_push(t, a):
    src = a.local.rstrip("/") or "/"
    pairs = []
    if os.path.isdir(src):
        for root, _, files in os.walk(src):
            for f in files:
                lp = os.path.join(root, f)
                if os.path.islink(lp):
                    continue
                pairs.append((lp, os.path.join(a.remote, os.path.relpath(lp, src)), None))
    elif os.path.isfile(src):
        dst = a.remote
        if dst.endswith("/"):
            dst += os.path.basename(src)
        pairs.append((src, dst, a.mode))
    else:
        die(f"no such file: {src}")
    upload(t, pairs, force=a.force)


def cmd_pull(t, a):
    is_dir = t.run(f"test -d {shlex.quote(a.remote)}", check=False).returncode == 0
    t0 = time.time()
    if is_dir:
        os.makedirs(a.local, exist_ok=True)
        dst = os.path.join(a.local, os.path.basename(a.remote.rstrip("/")))
        t.scp("-r", t.remote(a.remote), a.local)
        files = [os.path.join(r, f) for r, _, fs in os.walk(dst) for f in fs]
        rel = {os.path.join(a.remote, os.path.relpath(f, dst)): f for f in files}
    else:
        dst = a.local
        if os.path.isdir(dst):
            dst = os.path.join(dst, os.path.basename(a.remote))
        t.scp(t.remote(a.remote), dst)
        rel = {a.remote: dst}
    shas = remote_shas(t, list(rel))
    bad = [r for r, l in rel.items() if shas.get(r) != sha256_file(l)]
    total = sum(os.path.getsize(l) for l in rel.values())
    if bad:
        die("checksum mismatch (file changed while copying?): " + ", ".join(bad))
    print(f"pulled {len(rel)} file(s), {total} bytes in {time.time() - t0:.1f} s to {dst}, sha256 verified")


def cmd_deploy_files(t, a):
    specs = list(a.pairs)
    if a.manifest:
        with open(a.manifest) as f:
            specs += [l.strip() for l in f if l.strip() and not l.lstrip().startswith("#")]
    pairs = []
    for s in specs:
        bits = s.split(":")
        if len(bits) < 2:
            die(f"bad spec {s!r}: LOCAL:REMOTE[:MODE]")
        l, r = os.path.expanduser(bits[0]), bits[1]
        if not os.path.isfile(l):
            die(f"no such file: {l}")
        pairs.append((l, r, bits[2] if len(bits) > 2 else None))
    if not pairs:
        die("nothing to deploy")
    upload(t, pairs, force=a.force)


# ── Kernel slots ────────────────────────────────────────────────────────────

def espctl_status(t):
    st = {}
    for line in t.out("espctl status", check=False).splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            st[k] = v
    return st


F2FSYNC_REV = "da51b45e"   # first kernel with f2fs roll-forward fsync records


def kernel_rev(t, kid):
    """Build revision (the LEANDROS_KERNEL_TAG prefix) of the kernel stored for slot `kid`, read from the ESP copy; None if unknown."""
    if not kid or kid in ("-", "factory"):
        return None
    out = t.out(f"espctl cat /leandros/k/{shlex.quote(kid)}/kernel.elf 2>/dev/null"
                " | grep -a -o -E '[0-9a-f]{10}(-dirty)?-[0-9]{8}T[0-9]{4}Z' | head -n 1", check=False).strip()
    return out.split("-")[0] if out else None


def rf_guard_needed(t):
    """True when the kernel the stick falls back to (the good entry) predates the f2fs roll-forward records.

    A pre-f2fsync kernel ignores LRF1 records, so after an unclean shutdown of a
    new kernel it would lose every record-only fsync. Unknown revision counts as
    predating. The factory entry predates f2fsync until the stick is re-flashed;
    that case is documented, not guarded.
    """
    rev = kernel_rev(t, espctl_status(t).get("good", ""))
    if not rev:
        return True, "good kernel revision unknown"
    r = subprocess.run(["git", "-C", REPO, "merge-base", "--is-ancestor", F2FSYNC_REV, rev],
                       capture_output=True)
    if r.returncode == 0:
        return False, f"good kernel {rev} postdates f2fsync"
    return True, f"good kernel {rev} predates f2fsync ({F2FSYNC_REV}) or is not in this repo"


def promote(t):
    """espctl promote (which also disarms the running kernel's netwatch), then drop
    the trial-only netwatch=/netwatch.* knobs the good entry inherited."""
    p = t.run("espctl promote", check=False)
    out = p.stdout.decode(errors="replace") + p.stderr.decode(errors="replace")
    if p.returncode == 0:
        args = espctl_status(t).get("good_cmdline", "").split()
        # netwatch= and the deploy-time f2fs.rf=0 guard are trial-only knobs.
        trial_only = lambda x: x.startswith("netwatch=") or x.startswith("netwatch.") or x == "f2fs.rf=0"
        if any(trial_only(x) for x in args):
            keep = " ".join(shlex.quote(x) for x in args if not trial_only(x))
            out += t.out(f"espctl cmdline good {keep}".rstrip())
    return p.returncode, out


def bid_check(kernel, initrd):
    """Refuse a kernel/initrd pair where the initrd carries lib/modules but not this kernel's build-id."""
    sys.path.insert(0, os.path.join(REPO, "scripts", "kmod"))
    import bidcheck
    err = bidcheck.pair(kernel, initrd)
    if err:
        die(err)


def cmd_deploy_kernel(t, a):
    arch = a.arch or t.out("uname -m").strip()
    if arch not in ("x86_64", "aarch64"):
        die(f"unknown arch {arch!r}")
    kernel = a.kernel or os.path.join(REPO, f"target/final-{arch}/kernel")
    initrd = a.initrd or os.path.join(REPO, f"initrd-{arch}.cpio")
    for f in (kernel, initrd):
        if not os.path.isfile(f):
            die(f"missing {f}: build first (./scripts/build-all.sh --arch {arch})")
    bid_check(kernel, initrd)
    remote_arch = t.out("uname -m").strip()
    if remote_arch != arch:
        die(f"the target runs {remote_arch}, not {arch}")
    ks, is_ = sha256_file(kernel), sha256_file(initrd)
    age = (time.time() - os.path.getmtime(kernel)) / 60
    print(f"kernel {kernel}: {os.path.getsize(kernel)} bytes, sha256 {ks[:16]}..., built {age:.0f} min ago")
    print(f"initrd {initrd}: {os.path.getsize(initrd)} bytes, sha256 {is_[:16]}...")
    rk, ri = f"{STAGE_DIR}/kernel.elf", f"{STAGE_DIR}/initrd.gz"
    upload(t, [(kernel, rk, "644"), (initrd, ri, "644")], force=True, quiet=True)
    if not getattr(a, "no_rf_guard", False):
        need, why = rf_guard_needed(t)
        print(f"f2fs rf guard: {why}; " + ("trial gets f2fs.rf=0" if need else "no change"))
        if need:
            base = a.cmdline if a.cmdline is not None else espctl_status(t).get("good_cmdline", "").strip()
            if "f2fs.rf=" not in base:
                a.cmdline = (base + " " if base else "") + "f2fs.rf=0"
    if a.netwatch is None:
        a.netwatch = 0 if t.qemu else 180
    if a.trial_max:
        a.append = ((a.append or "") + f" netwatch.trial_max={a.trial_max}").strip()
    if a.netwatch or a.append:
        # Trial entry only: espctl --cmdline is applied to the new (trial) entry;
        # the good entry keeps its own. Base = --cmdline if given, else the good entry's.
        base = a.cmdline
        if base is None:
            base = espctl_status(t).get("good_cmdline", "").strip()
        extra = " ".join(x for x in (a.append or "", f"netwatch={a.netwatch}" if a.netwatch else "") if x)
        a.cmdline = (base + " " if base else "") + extra
    cmd = (f"espctl install {rk} --initrd {ri} --sha256 {ks} --initrd-sha256 {is_}"
           + (" --no-arm" if a.no_arm else "")
           + (f" --cmdline {shlex.quote(a.cmdline)}" if a.cmdline is not None else ""))
    p = t.run(cmd, check=False)
    out = p.stdout.decode(errors="replace")
    sys.stdout.write(out)
    t.run(f"rm -f {rk} {ri}", check=False)
    if p.returncode != 0:
        sys.stderr.write(p.stderr.decode(errors="replace"))
        die("espctl install failed; the boot entries are unchanged")
    kid = next((l.split("=", 1)[1] for l in out.splitlines() if l.startswith("slot id=")), None)
    if not a.reboot:
        print(f"\nnext: scripts/remote.py reboot   (boots trial {kid} once; then `remote.py promote`)")
        return
    if a.no_arm:
        die("--reboot with --no-arm would not boot the new kernel")
    ok = do_reboot(t, timeout=a.timeout, logs=not a.no_logs, expect_kid=kid)
    if not ok:
        sys.exit(1)
    if ok and a.promote:
        print(promote(t)[1])


def cmd_espctl(t, a):
    p = t.run("espctl " + " ".join(shlex.quote(x) for x in a.args), check=False, capture=False)
    sys.exit(p.returncode)


# ── Logs, status ────────────────────────────────────────────────────────────

STATUS_CMD = ("echo \"== uname\"; uname -a; echo \"== uptime\"; cat /proc/uptime; "
              "echo \"== cmdline\"; cat /proc/cmdline; echo \"== espctl status\"; espctl status; "
              "echo \"== mounts\"; cat /proc/mounts; echo \"== meminfo\"; head -5 /proc/meminfo")


def cmd_status(t, a):
    st = espctl_status(t)
    uname = t.out("uname -srvm").strip()
    up = t.out("cat /proc/uptime").split()[0]
    print(f"{t.label()}: {uname}, up {float(up):.0f} s")
    print(f"  booted: {st.get('booted_slot', '?')} {st.get('booted_kid', '')}; "
          f"entries: trial={st.get('trial')} good={st.get('good')} default={st.get('default')} "
          f"armed={st.get('armed')}; ESP {st.get('esp')} free {int(st.get('esp_free_bytes', 0)) >> 20} MiB")
    if a.verbose:
        print(t.out(STATUS_CMD, check=False))


def redact(b):
    """Blank secrets (the Wi-Fi PSK rides on the kernel cmdline) before a log hits disk."""
    b = re.sub(rb"((?:wifi|iwlwifi)\.psk=)\S+", rb"\1<redacted>", b)
    # Passwords/secrets passed as assignments on a command line (GC_PW=..., PASS=..., password=...).
    return re.sub(rb"(?i)\b(GC_PW|[A-Z_]*PASS(?:WORD|WD)?|[A-Z_]*SECRET|[A-Z_]*TOKEN)=(?:'[^']*'|\"[^\"]*\"|\S+)", rb"\1=<redacted>", b)


def fetch_logs(t, out=None):
    stamp = time.strftime("%Y%m%d-%H%M%S")
    out = out or os.path.join(REPO, "artifacts", "remote-logs", f"{stamp}-{t.host}")
    os.makedirs(out, exist_ok=True)
    t.run("sync", check=False, timeout=60)

    def save(name, cmd, timeout=120):
        p = t.run(cmd, check=False, timeout=timeout)
        if p is None:
            return
        with open(os.path.join(out, name), "wb") as f:
            f.write(redact(p.stdout))
            if p.stderr:
                f.write(b"\n--- stderr ---\n" + redact(p.stderr))

    save("status.txt", STATUS_CMD)
    save("klog-current.txt", "espctl klog")
    save("klog-all.txt", "espctl klog --all")
    save("partitions.txt", "espctl parts")
    save("limine.conf", "espctl cat /limine.conf")
    # /var/log (sshd, greeter/session logs) and loose logs in /tmp.
    if t.run("test -d /var/log", check=False).returncode == 0:
        subprocess.run(["scp", "-q", "-r", "-P", str(t.port), *t.opts(), t.remote("/var/log"),
                        os.path.join(out, "var-log")], capture_output=True)
    tmp_logs = t.out("ls /tmp/*.log /run/user/*/*.log 2>/dev/null", check=False).split()
    if tmp_logs:
        os.makedirs(os.path.join(out, "tmp-logs"), exist_ok=True)
        for f in tmp_logs:
            subprocess.run(["scp", "-q", "-P", str(t.port), *t.opts(), t.remote(f),
                            os.path.join(out, "tmp-logs", f.strip("/").replace("/", "_"))], capture_output=True)
    n = sum(len(fs) for _, _, fs in os.walk(out))
    print(f"logs: {n} file(s) in {out}")
    return out


def cmd_logs(t, a):
    fetch_logs(t, a.out)


# ── Reboot ──────────────────────────────────────────────────────────────────

def probe(t, cmd, timeout=15):
    """Exit status of `cmd` on a fresh connection, None when unreachable."""
    try:
        p = subprocess.run(t.ssh_argv(cmd, master=False), capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None
    return None if p.returncode == 255 else p.returncode


def wait_up(t, timeout):
    t0 = time.time()
    while time.time() - t0 < timeout:
        b = port_open(t.host, t.port)
        if b and b.startswith(b"SSH-") and probe(t, "true", timeout=20) == 0:
            return time.time() - t0
        time.sleep(2)
    return None


def do_reboot(t, timeout, expect_kid=None, logs=True, wait=True):
    before = espctl_status(t)
    t.run("echo remdbg > /tmp/.remdbg-alive && sync", check=False, timeout=60)
    armed = before.get("armed") == "1"
    print(f"rebooting {t.label()} (next boot: "
          f"{'trial ' + before.get('trial', '') + ', one-shot' if armed else before.get('default', '?')})")
    t0 = time.time()
    t.run("/bin/reboot", check=False, timeout=30)
    t.close_master()
    if not wait:
        return True
    # Up again = sshd answers and the /tmp marker (tmpfs) is gone.
    down_at = None
    back = False
    while time.time() - t0 < timeout:
        r = probe(t, "test -e /tmp/.remdbg-alive")
        if r is None:
            down_at = down_at or time.time()
        elif r != 0:
            back = True
            break
        time.sleep(2)
    if not back:
        fb = before.get("good") if before.get("good") not in (None, "-") else "factory"
        print(f"\nREBOOT FAILED: no new boot answering on {t.host}:{t.port} after {timeout} s"
              f"{'' if down_at else ' (the old system never went down: did /bin/reboot run?)'}.", file=sys.stderr)
        if armed:
            print(f"The trial kernel {before.get('trial')} was armed. Power-cycle the machine:\n"
                  f"  - if the trial got as far as init, the next boot is the fallback ({fb}) by itself;\n"
                  f"  - if not, pick 'LeandrOS {'good' if fb != 'factory' else 'factory'}' in the Limine menu "
                  f"(shown {before.get('timeout', '3')} s at power-on).\n"
                  f"Then `remote.py logs`: klog-all.txt has the failed boot's console log if it got that far.\n"
                  f"Last resort: re-flash the stick (scripts/write-usb.sh keeps /home).", file=sys.stderr)
        else:
            print("Look at the screen / power-cycle. The failed boot's console log is on the stick's log "
                  "partition: `remote.py logs` once it is up, or scripts/read-usb-log.sh on the Mac.",
                  file=sys.stderr)
        return False
    total = time.time() - t0
    after = espctl_status(t)
    uname = t.out("uname -v").strip()
    print(f"back after {total:.0f} s: booted {after.get('booted_slot')} {after.get('booted_kid')}, "
          f"uname -v: {uname}")
    ok = True
    if expect_kid:
        if after.get("booted_kid") == expect_kid:
            print(f"NEW KERNEL RUNNING: trial {expect_kid}. Keep it with `remote.py promote` "
                  f"(otherwise the next boot falls back to {after.get('default')}).")
        else:
            print(f"NOT the new kernel: expected trial {expect_kid}, running {after.get('booted_slot')} "
                  f"{after.get('booted_kid')}", file=sys.stderr)
            ok = False
    if logs:
        fetch_logs(t)
    return ok


# ── Raspberry Pi 5: kernels in the root, entered by kexec ─────────────────
PI_DIR = "/boot/pi"
PI_APPEND_MARK = "pi.append=1"   # marks where pi-boot/pi-deploy extras start on the kernel cmdline
PI_KERNEL_PHYS = 0x20_0000  # build-all.sh --rpi5 links kernel-direct here


def pi_check_elf(path):
    """Refuse anything but an rpi5 direct-boot ELF (lowest PT_LOAD at 0x200000,
    arm64 Image magic): a QEMU-virt kernel (0x40080000) would kexec into a
    machine it cannot drive."""
    import struct
    b = open(path, "rb").read()
    if b[:4] != b"\x7fELF" or b[4] != 2:
        die(f"{path}: not an ELF64")
    phoff, = struct.unpack_from("<Q", b, 0x20)
    phentsz, phnum = struct.unpack_from("<HH", b, 0x36)
    loads = []
    for i in range(phnum):
        o = phoff + i * phentsz
        typ, = struct.unpack_from("<I", b, o)
        if typ == 1:
            off, _va, pa = struct.unpack_from("<QQQ", b, o + 8)
            loads.append((pa, off))
    if not loads:
        die(f"{path}: no PT_LOAD")
    pa, off = min(loads)
    if pa != PI_KERNEL_PHYS:
        die(f"{path}: lowest PT_LOAD at {pa:#x}, not {PI_KERNEL_PHYS:#x}: not an rpi5 build "
            f"(./scripts/build-all.sh --arch aarch64 --rpi5)")
    if b[off + 0x38:off + 0x3c] != b"ARM\x64":
        die(f"{path}: no arm64 Image header")


def pi_kexec(t, slot, append, timeout):
    if not t.pi:
        die("pi-* commands need LEANDROS_REMOTE=pi (or pi:<host>)")
    d = f"{PI_DIR}/{slot}"
    if t.run(f"test -s {d}/kernel && test -s {d}/initrd", check=False).returncode != 0:
        die(f"no kernel/initrd in {d}")
    before = t.out("uname -v").strip()
    # --append is per boot, not cumulative: the running cmdline is cut at the
    # PI_APPEND_MARK an earlier pi-boot/pi-deploy put before its own extras.
    base = t.out("cat /proc/cmdline").strip().split(f" {PI_APPEND_MARK}")[0].strip()
    cmd = f"{base} {PI_APPEND_MARK} {append}" if append else base
    t.out(f"/bin/kexec -l {d}/kernel --initrd={d}/initrd --command-line={shlex.quote(cmd)}")
    t.run("echo remdbg > /tmp/.remdbg-alive && sync", check=False, timeout=60)
    print(f"kexec into {slot} on {t.label()} (was: {before})")
    t0 = time.time()
    # In the FOREGROUND, like `/bin/reboot` in do_reboot: the syscall never
    # returns on success and the ssh call just times out. A backgrounded
    # `(sleep 1; kexec -e) &` does not survive: our sshd SIGHUPs the session's
    # process group when the channel closes (userland/sshd/src/session.rs
    # channel_close), and a non-interactive shell keeps `&` jobs in that group.
    # This must also work against the old, already-deployed userland.
    r = t.run("/bin/kexec -e </dev/null 2>&1", check=False, timeout=20, master=False)
    t.close_master()
    if r is not None and r.returncode != 255:
        msg = (r.stdout or b"").decode(errors="replace").strip()
        print(f"\nKEXEC NOT EXECUTED: kexec loaded but `kexec -e` returned rc={r.returncode} "
              f"on the old kernel ({before}): {msg[-400:] or '(no output)'}", file=sys.stderr)
        return False
    back = False
    while time.time() - t0 < timeout:
        r = probe(t, "test -e /tmp/.remdbg-alive")
        if r is not None and r != 0:
            back = True
            break
        time.sleep(2)
    if not back:
        if probe(t, "test -e /tmp/.remdbg-alive") == 0:
            u = t.run("uname -v", check=False, timeout=15, master=False)
            same = u is not None and u.stdout.decode(errors="replace").strip() == before
            print(f"\nKEXEC NOT EXECUTED: kexec loaded but the old kernel is still answering on {t.host} "
                  f"after {timeout} s ({'same uname -v' if same else 'uname -v changed?'}, /tmp/.remdbg-alive "
                  f"still present). Run `/bin/kexec -e` on the serial console, or retry pi-boot.", file=sys.stderr)
            return False
        print(f"\nKEXEC FAILED: nothing new answering on {t.host} after {timeout} s.\n"
              f"Power-cycle the Pi: the firmware boots the card's kernel.img (never touched by\n"
              f"remote.py). The serial console (uart10) shows how far the new kernel got.", file=sys.stderr)
        return False
    after = t.out("uname -v").strip()
    print(f"back after {time.time() - t0:.0f} s: uname -v: {after}")
    t.run(f"echo {slot} > /tmp/.pi-booted-slot", check=False)
    if after == before:
        print("WARNING: uname -v unchanged (same build, or the old kernel came back)", file=sys.stderr)
    return True


def cmd_pi_deploy(t, a):
    if not t.pi:
        die("pi-deploy needs LEANDROS_REMOTE=pi (or pi:<host>)")
    k = a.kernel or os.path.join(REPO, "target/final-aarch64/kernel-direct")
    i = a.initrd or os.path.join(REPO, "initrd-aarch64.cpio")
    pi_check_elf(k)
    bid_check(k, i)
    t.run(f"mkdir -p {PI_DIR}/trial")
    upload(t, [(k, f"{PI_DIR}/trial/kernel", None), (i, f"{PI_DIR}/trial/initrd", None)])
    rev = subprocess.run(["git", "-C", REPO, "rev-parse", "--short=12", "HEAD"], capture_output=True, text=True).stdout.strip()
    t.run(f"echo {shlex.quote(rev + ' ' + time.strftime('%Y-%m-%dT%H:%M:%S'))} > {PI_DIR}/trial/info && sync")
    if a.no_exec:
        print(f"staged in {PI_DIR}/trial (not booted)")
        return
    sys.exit(0 if pi_kexec(t, "trial", a.append, a.timeout) else 1)


def cmd_pi_boot(t, a):
    sys.exit(0 if pi_kexec(t, a.slot, a.append, a.timeout) else 1)


def cmd_pi_promote(t, a):
    if not t.pi:
        die("pi-promote needs LEANDROS_REMOTE=pi")
    booted = t.out(f"cat /tmp/.pi-booted-slot 2>/dev/null; true").strip()
    if booted != "trial":
        die(f"the running kernel was not kexec'd from trial (booted slot: {booted or 'card kernel.img'}); "
            f"run pi-deploy first")
    t.out(f"cd {PI_DIR} && rm -rf prev.tmp && (test -d good && mv good prev.tmp || true) && "
          f"mkdir good && cp trial/kernel trial/initrd trial/info good/ && sync && "
          f"rm -rf prev && (test -d prev.tmp && mv prev.tmp prev || true) && sync")
    print(t.out(f"cat {PI_DIR}/good/info").strip() + " is now good. NOTE: the card's kernel.img is unchanged; "
          "after a power cycle run `remote.py pi-boot good`.")


def cmd_pi_sync(t, a):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import pi_sync
    sys.exit(pi_sync.main(t, sys.modules[__name__], a))


def cmd_pi_status(t, a):
    print(t.out(f"uname -v; cat /proc/cmdline; echo booted slot: $(cat /tmp/.pi-booted-slot 2>/dev/null || echo card); "
                f"for s in trial good prev; do echo \"$s: $(cat {PI_DIR}/$s/info 2>/dev/null || echo -)\"; done").rstrip())


def cmd_reboot(t, a):
    ok = do_reboot(t, timeout=a.timeout, logs=not a.no_logs, wait=not a.no_wait)
    sys.exit(0 if ok else 1)


def cmd_wait(t, a):
    up = wait_up(t, a.timeout)
    if up is None:
        die(f"no sshd on {t.host}:{t.port} after {a.timeout} s")
    print(f"sshd up ({up:.0f} s)")


# ── Setup ───────────────────────────────────────────────────────────────────

def cmd_keygen(_t, _a):
    if os.path.exists(DEFAULT_KEY):
        print(f"{DEFAULT_KEY} exists")
    else:
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "leandros-remote",
                        "-f", DEFAULT_KEY], check=True)
        print(f"created {DEFAULT_KEY} (+ .pub)")
    print(open(DEFAULT_KEY + ".pub").read().strip())


SETUP_TEXT = """One-time setup (root logs in by key only; the key travels on the stick):

 1. scripts/remote.py keygen            -> ~/.ssh/leandros_remote{,.pub}
 2. ./scripts/build-all.sh --arch x86_64 (release)
 3. Flash the stick: ./scripts/write-usb.sh x86_64 /dev/diskN
    It puts ~/.ssh/leandros_remote.pub on the ESP as /leandros/authorized_keys
    (or --authorized-keys F); every boot imports it into /root/.ssh.
    A stick flashed earlier: copy the .pub to the ESP (FAT, mounts on the Mac)
    as /leandros/authorized_keys, no re-flash needed.
 4. Boot the laptop from the stick, network up (ethernet DHCP, or WPA2 from
    /etc/leandros/wifi.conf). Find it: scripts/remote.py discover
 5. export LEANDROS_REMOTE=<address>; scripts/remote.py status
"""


def cmd_setup(t, a):
    print(SETUP_TEXT)
    if not os.path.exists(DEFAULT_KEY):
        print(f"step 1 missing: {DEFAULT_KEY} does not exist")
        return
    spec = a.target or os.environ.get("LEANDROS_REMOTE")
    if not spec:
        print("(set LEANDROS_REMOTE to check the login)")
        return
    t = Target(spec)
    p = t.run("id -u; uname -a", check=False, master=False)
    if p.returncode == 0:
        print(f"OK: key login to {t.label()} works: {p.stdout.decode().split(chr(10))[1]}")
    else:
        print(f"key login to {t.label()} failed (see above)")


def cmd_forget_host(t, _a):
    name = t.host if t.port == 22 else f"[{t.host}]:{t.port}"
    subprocess.run(["ssh-keygen", "-R", name, "-f", KNOWN_HOSTS], capture_output=True)
    print(f"forgot the host key of {name} in {KNOWN_HOSTS}")


def local_subnet():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))
        ip = s.getsockname()[0]
    finally:
        s.close()
    return ipaddress.ip_network(f"{ip}/24", strict=False)


def cmd_discover(_t, a):
    net = ipaddress.ip_network(a.subnet, strict=False) if a.subnet else local_subnet()
    if net.num_addresses > 4096:
        die("subnet too large (max /20)")
    print(f"scanning {net} port {a.port} for LeandrOS sshd (russh banner)...")

    def probe(ip):
        b = port_open(str(ip), a.port, timeout=0.6)
        return (str(ip), b) if b else None

    with ThreadPoolExecutor(128) as ex:
        found = [r for r in ex.map(probe, net.hosts()) if r]
    hits = 0
    for ip, banner in found:
        txt = banner.decode(errors="replace").strip()
        mark = "LeandrOS?" if "russh" in txt.lower() else ""
        if mark or a.all:
            print(f"  {ip}:{a.port}  {txt}  {mark}")
            hits += bool(mark)
    if not hits:
        print("no russh sshd found (is the machine booted and on this network? try --subnet)")


# ── main ────────────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    ap.add_argument("--target", help="host[:port], user@host[:port] or qemu[:run-id] (default $LEANDROS_REMOTE)")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("keygen")
    sub.add_parser("setup")
    p = sub.add_parser("discover"); p.add_argument("--subnet"); p.add_argument("--port", type=int, default=22)
    p.add_argument("--all", action="store_true", help="also list non-LeandrOS ssh servers")
    p = sub.add_parser("status"); p.add_argument("-v", "--verbose", action="store_true")
    p = sub.add_parser("run"); p.add_argument("command", nargs=argparse.REMAINDER)
    p.add_argument("-t", "--tty", action="store_true")
    sub.add_parser("shell")
    p = sub.add_parser("push"); p.add_argument("local"); p.add_argument("remote")
    p.add_argument("--mode", help="octal mode for a single file (default: the local one)")
    p.add_argument("--force", action="store_true", help="upload even identical files")
    p = sub.add_parser("pull"); p.add_argument("remote"); p.add_argument("local")
    p = sub.add_parser("deploy-files"); p.add_argument("pairs", nargs="*", help="LOCAL:REMOTE[:MODE]")
    p.add_argument("--manifest"); p.add_argument("--force", action="store_true")
    p = sub.add_parser("deploy-kernel"); p.add_argument("--arch"); p.add_argument("--kernel")
    p.add_argument("--initrd"); p.add_argument("--no-arm", action="store_true")
    p.add_argument("--cmdline", help="extra kernel arguments for this entry (default: the good entry's); '' = none")
    p.add_argument("--no-rf-guard", action="store_true", help="do not add f2fs.rf=0 to the trial when the good kernel predates f2fs roll-forward (da51b45e)"); p.add_argument("--netwatch", type=int, metavar="S", default=None, help="append netwatch=S to the TRIAL entry's cmdline only: if the trial kernel has no IPv4 address S seconds after boot, or its gateway stops answering pings for netwatch.reach= (300 s), it reboots itself to the good entry (default 180 on real machines, off on qemu; 0 = off)"); p.add_argument("--trial-max", type=int, metavar="M", help="append netwatch.trial_max=M: an un-promoted trial reboots to the good entry after M minutes")
    p.add_argument("--append", metavar="ARGS", help="append ARGS (e.g. 'sched.wakestat=1') to the TRIAL entry's cmdline only, keeping the good entry's arguments")
    p.add_argument("--reboot", action="store_true"); p.add_argument("--promote", action="store_true")
    p.add_argument("--timeout", type=int, default=300); p.add_argument("--no-logs", action="store_true")
    sub.add_parser("promote"); sub.add_parser("discard")
    p = sub.add_parser("espctl"); p.add_argument("args", nargs=argparse.REMAINDER)
    p = sub.add_parser("logs"); p.add_argument("--out")
    p = sub.add_parser("reboot"); p.add_argument("--timeout", type=int, default=300)
    p.add_argument("--no-wait", action="store_true"); p.add_argument("--no-logs", action="store_true")
    p = sub.add_parser("wait"); p.add_argument("--timeout", type=int, default=300)
    sub.add_parser("forget-host")
    p = sub.add_parser("pi-deploy"); p.add_argument("--kernel"); p.add_argument("--initrd"); p.add_argument("--append")
    p.add_argument("--no-exec", action="store_true"); p.add_argument("--timeout", type=int, default=240)
    p = sub.add_parser("pi-boot"); p.add_argument("slot", choices=["good", "prev", "trial"]); p.add_argument("--append")
    p.add_argument("--timeout", type=int, default=240)
    sub.add_parser("pi-promote"); sub.add_parser("pi-status")
    p = sub.add_parser("pi-sync", help="update the target's root fs in place from the current aarch64 build")
    p.add_argument("--dry-run", action="store_true", help="only print what would change and the bytes")
    p.add_argument("--list", action="store_true", help="print every change, not a per-directory summary")
    p.add_argument("--plan-out", help="also write the full change list to this file")
    p.add_argument("--board", default="rpi5", help="LEANDROS_BOARD for the manifest ('' = QEMU virt)")
    p.add_argument("--manifest", help="use this manifest.json instead of generating one")
    a = ap.parse_args()

    if a.cmd == "keygen":
        return cmd_keygen(None, a)
    if a.cmd == "setup":
        return cmd_setup(None, a)
    if a.cmd == "discover":
        return cmd_discover(None, a)
    t = Target(a.target or os.environ.get("LEANDROS_REMOTE"))
    if a.cmd == "run":
        if not a.command:
            die("run: no command")
        cmd = " ".join(a.command) if len(a.command) > 1 else a.command[0]
        sys.exit(subprocess.run(t.ssh_argv(cmd, tty=a.tty)).returncode)
    if a.cmd == "shell":
        sys.exit(subprocess.run(t.ssh_argv(None, tty=True)).returncode)
    if a.cmd == "promote":
        rc, out = promote(t)
        sys.stdout.write(out)
        sys.exit(rc)
    if a.cmd == "discard":
        p = t.run(f"espctl {a.cmd}", check=False)
        sys.stdout.write(p.stdout.decode(errors="replace"))
        sys.stderr.write(p.stderr.decode(errors="replace"))
        sys.exit(p.returncode)
    {
        "status": cmd_status, "push": cmd_push, "pull": cmd_pull, "deploy-files": cmd_deploy_files,
        "deploy-kernel": cmd_deploy_kernel, "espctl": cmd_espctl, "logs": cmd_logs,
        "reboot": cmd_reboot, "wait": cmd_wait, "forget-host": cmd_forget_host,
        "pi-deploy": cmd_pi_deploy, "pi-boot": cmd_pi_boot, "pi-promote": cmd_pi_promote, "pi-status": cmd_pi_status, "pi-sync": cmd_pi_sync,
    }[a.cmd](t, a)


if __name__ == "__main__":
    main()
