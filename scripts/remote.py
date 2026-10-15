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
                         atomically (temp name + rename), skipping
                         identical files
  pull <remote> <local>  download a file or directory, sha256-verified
  deploy-files <pairs|--manifest F>  upload many files: LOCAL:REMOTE[:MODE]
  deploy-kernel [--arch A] [--kernel F] [--initrd F] [--cmdline ARGS] [--no-arm]
                [--reboot [--promote]]
                         install the release build's kernel + initrd as the
                         one-shot "trial" boot entry (sha256-verified on the
                         ESP); with --reboot boot it and check it is running
  promote | discard      trial -> good / drop the trial entry
  espctl <args...>       run espctl in the guest
  logs [--out DIR]       console log (all 4 boots), status, /var/log, ... into
                         artifacts/remote-logs/<time>-<host>/
  reboot [--no-wait] [--timeout S] [--no-logs]
                         sync + reboot, wait for sshd, report boot time,
                         fetch logs
  wait [--timeout S]     wait until sshd answers
  forget-host            drop the target's host key (after a re-flash)
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
                try:
                    found = re.findall(rb"DHCP configured, address: ([0-9.]+)", open(log, "rb").read())
                except FileNotFoundError:
                    found = []
                if not found:
                    die(f"vmnet VM: no DHCP lease in {log} yet")
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
        self.control = os.path.join(tempfile.gettempdir(), f"leandros-remote-{self.host}-{self.port}.sock")

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
        p = t.run("sha256sum -- " + " ".join(shlex.quote(x) for x in chunk) + " 2>/dev/null", check=False)
        for line in p.stdout.decode(errors="replace").splitlines():
            parts = line.split(None, 1)
            if len(parts) == 2 and len(parts[0]) == 64:
                res[parts[1].lstrip("*")] = parts[0]
    return res


def upload(t, pairs, force=False, quiet=False):
    """pairs: [(local, remote, mode or None)]. Changed files only, each via
    <remote>.remdbg-tmp, verified, then renamed over the target."""
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
        t.run("mkdir -p -- " + " ".join(shlex.quote(d) for d in dirs))
    t0 = time.time()
    total = sum(os.path.getsize(l) for _, l, _, _ in todo)
    lines = [f"put {shlex.quote(l)} {shlex.quote(r + '.remdbg-tmp')}" for r, l, _, _ in todo]
    for i in range(0, len(lines), 200):
        t.sftp_batch(lines[i:i + 200])
    dt = time.time() - t0
    got = remote_shas(t, [r + ".remdbg-tmp" for r, *_ in todo])
    bad = [r for r, _, s, _ in todo if got.get(r + ".remdbg-tmp") != s]
    if bad:
        t.run("rm -f -- " + " ".join(shlex.quote(r + ".remdbg-tmp") for r in bad), check=False)
        die("checksum mismatch after upload (targets left untouched): " + ", ".join(bad))
    cmds = []
    for r, l, s, m in todo:
        mode = m if m is not None else oct(os.stat(l).st_mode & 0o7777)[2:]
        tmp = shlex.quote(r + ".remdbg-tmp")
        cmds.append(f"chmod {mode} {tmp} && mv -f {tmp} {shlex.quote(r)}")
    for i in range(0, len(cmds), 50):
        t.run(" && ".join(cmds[i:i + 50]) + " && sync")
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


def cmd_deploy_kernel(t, a):
    arch = a.arch or t.out("uname -m").strip()
    if arch not in ("x86_64", "aarch64"):
        die(f"unknown arch {arch!r}")
    kernel = a.kernel or os.path.join(REPO, f"target/final-{arch}/kernel")
    initrd = a.initrd or os.path.join(REPO, f"initrd-{arch}.cpio")
    for f in (kernel, initrd):
        if not os.path.isfile(f):
            die(f"missing {f}: build first (./scripts/build-all.sh --arch {arch})")
    remote_arch = t.out("uname -m").strip()
    if remote_arch != arch:
        die(f"the target runs {remote_arch}, not {arch}")
    ks, is_ = sha256_file(kernel), sha256_file(initrd)
    age = (time.time() - os.path.getmtime(kernel)) / 60
    print(f"kernel {kernel}: {os.path.getsize(kernel)} bytes, sha256 {ks[:16]}..., built {age:.0f} min ago")
    print(f"initrd {initrd}: {os.path.getsize(initrd)} bytes, sha256 {is_[:16]}...")
    rk, ri = f"{STAGE_DIR}/kernel.elf", f"{STAGE_DIR}/initrd.gz"
    upload(t, [(kernel, rk, "644"), (initrd, ri, "644")], force=True, quiet=True)
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
        print(t.out("espctl promote"))


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
            f.write(p.stdout)
            if p.stderr:
                f.write(b"\n--- stderr ---\n" + p.stderr)

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
    p.add_argument("--reboot", action="store_true"); p.add_argument("--promote", action="store_true")
    p.add_argument("--timeout", type=int, default=300); p.add_argument("--no-logs", action="store_true")
    sub.add_parser("promote"); sub.add_parser("discard")
    p = sub.add_parser("espctl"); p.add_argument("args", nargs=argparse.REMAINDER)
    p = sub.add_parser("logs"); p.add_argument("--out")
    p = sub.add_parser("reboot"); p.add_argument("--timeout", type=int, default=300)
    p.add_argument("--no-wait", action="store_true"); p.add_argument("--no-logs", action="store_true")
    p = sub.add_parser("wait"); p.add_argument("--timeout", type=int, default=300)
    sub.add_parser("forget-host")
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
    if a.cmd in ("promote", "discard"):
        p = t.run(f"espctl {a.cmd}", check=False)
        sys.stdout.write(p.stdout.decode(errors="replace"))
        sys.stderr.write(p.stderr.decode(errors="replace"))
        sys.exit(p.returncode)
    {
        "status": cmd_status, "push": cmd_push, "pull": cmd_pull, "deploy-files": cmd_deploy_files,
        "deploy-kernel": cmd_deploy_kernel, "espctl": cmd_espctl, "logs": cmd_logs,
        "reboot": cmd_reboot, "wait": cmd_wait, "forget-host": cmd_forget_host,
    }[a.cmd](t, a)


if __name__ == "__main__":
    main()
