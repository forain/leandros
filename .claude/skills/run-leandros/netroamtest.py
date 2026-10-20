#!/usr/bin/env python3
"""netroamtest.py [aarch64|x86_64] [--keep] [--only tcp|auto|sec] -- Wi-Fi roaming
between two networks on different subnets, in its own boot (lane netroam).

The simulated Wi-Fi gets two data paths (LEANDROS_NIC2=wifisim on SLIRP
10.0.3.0/24, LEANDROS_NIC3=wifisim on 10.0.4.0/24) and these APs
(wifisim.aps on a throwaway copy of the boot image; root image cloned too):

  NetA     WPA2-PSK  -50 dBm  path 1 (10.0.3.x)  also the legacy network
                                                (wifi.ssid= on the cmdline)
  NetB     WPA2-PSK  -40 dBm  path 2 (10.0.4.x)  stronger than NetA
  Flaky    WPA2-PSK  -35 dBm  path 1             deauths (reason 6) in every handshake
  TkipNet  WPA/WPA2 mixed, TKIP group cipher     not joinable
  ShaNet   PSK-SHA256 only                       not joinable
  MfpNet   PSK with MFP required                 not joinable
  FtNet    FT-PSK only                           not joinable

sshd listens on 0.0.0.0:22 from boot; a UDP echo server
(`netcheck --udp-echo 7777`) is started before any churn. Host forwards:
eth0 ssh (driver), path 1 ssh + udp, path 2 ssh + udp.

Steps (`netroam_<step>: PASS|FAIL detail`):
  legacy_auto   the legacy NetA autoconnects with priority 0
  a_listen      sshd banner and UDP echo through wifi0 on NetA (10.0.3.15)
  b_listen      user connect NetB: banner + UDP echo through wifi0 (10.0.4.15)
  a2_listen     back to NetA: banner + UDP echo again (address churn twice)
  churn         four more A/B switches, banner after each
  eth0_listen   the eth0 banner still answers after all of it
  stall         sshd stopped (SIGSTOP) while one client connects, sends and
                gives up and a second connects: the second is queued, not
                refused; after SIGCONT it gets its banner and so does a new
                one (on main this left wifi0's port 22 refusing for minutes:
                the one smoltcp listener socket sat in CLOSE-WAIT unreported)
  stay          saving profiles does not move a working connection
  fail_return   a failed user activation (Flaky) returns to the last network
                used (NetA), not to the stronger NetB
  noflap        Flaky at priority 10: one attempt, then NetA, and no further
                Flaky attempt while connected (60 s)
  backoff       three disconnects in a row: back on NetA each time, Flaky
                tried at most once (its BSS backs off 15/60/120/300 s)
  rsn_log       each rejected BSS logs its RSN details once
  join_col      /proc/net/wifi: NetA JOIN yes; the four variants JOIN no
  sec_msgs      connecting to each variant fails unsupported-security with a
                message naming the cause
Exit 0 iff all PASS.
"""
import os
import imgclone
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")

APS = ",".join([
    "NetA:psk:neta-pass-123:-50:6",
    "NetB:psk:netb-pass-123:-40:36::2",
    "Flaky:psk:flaky-pass-123:-35:1::d",
    "TkipNet:psk:tkip-pass-123:-60:11::t",
    "ShaNet:psk:sha-pass-1234:-61:40::s",
    "MfpNet:psk:mfp-pass-1234:-62:44::m",
    "FtNet:psk:ft-pass-12345:-63:48::x",
])
CMDLINE = f"wifisim.aps={APS} wifi.ssid=NetA wifi.psk=neta-pass-123"


def free_port(kind=socket.SOCK_STREAM):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def running_arch():
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    pid = driver._qemu_pid()
    if pid is None:
        return None
    r = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True)
    for a in ("x86_64", "aarch64"):
        if f"qemu-system-{a}" in r.stdout:
            return a
    return None


def banner(port, timeout):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as so:
            so.settimeout(timeout)
            return so.recv(64).decode(errors="replace").strip()
    except OSError as e:
        return f"error {e}"


def udp_echo(port, timeout, payload=b"netroam-ping"):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(1.0)
    t0 = time.time()
    try:
        while time.time() - t0 < timeout:
            try:
                s.sendto(payload, ("127.0.0.1", port))
                d, _ = s.recvfrom(256)
                if d == payload:
                    return True
            except OSError:
                pass
        return False
    finally:
        s.close()


def main(argv):
    arch, keep, only = None, False, None
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
        elif a == "--only":
            only = argv[i + 1]
            i += 1
        else:
            print(__doc__)
            return 2
        i += 1
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    detected = running_arch()
    arch = arch or detected or os.environ.get("LEANDROS_TEST_ARCH") or "aarch64"
    if detected is not None:
        print(f"netroamtest: stopping the running VM (run id '{caller}')", flush=True)
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    tmp = tempfile.mkdtemp(prefix="netroam-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if subprocess.run(["cp", "-c", src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, CMDLINE], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"netroamtest: could not set the command line: {r.stdout}{r.stderr}")
        return 1

    p1, p2 = free_port(), free_port()
    u1, u2 = free_port(socket.SOCK_DGRAM), free_port(socket.SOCK_DGRAM)
    run_id = f"{caller}-netroam" if caller else "netroam"
    env = dict(os.environ)
    env.update({
        "LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root, "LEANDROS_NET": "user",
        "LEANDROS_NIC2": "wifisim", "LEANDROS_NIC2_SSH_PORT": str(p1), "LEANDROS_NIC2_HOSTFWD": f"udp:127.0.0.1:{u1}-:7777",
        "LEANDROS_NIC3": "wifisim", "LEANDROS_NIC3_SSH_PORT": str(p2), "LEANDROS_NIC3_HOSTFWD": f"udp:127.0.0.1:{u2}-:7777",
    })
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (scoped to the netroam run id)
    scale = driver.wait_scale(arch, "uefi")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = ok
        print(f"netroam_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(c, t=60):
        res = driver.serial_run(c, timeout=t * scale, arch=arch)
        return res.get("rc"), res.get("output", "") or ""

    def log_since(mark):
        try:
            with open(driver.SERIAL_FULL_LOG, "rb") as f:
                f.seek(mark)
                return f.read().decode(errors="replace")
        except FileNotFoundError:
            return ""

    def log_mark():
        try:
            return os.path.getsize(driver.SERIAL_FULL_LOG)
        except OSError:
            return 0

    def wifi_addr():
        _, out = sh("cat /proc/net/nic", 30)
        sec = out.split("[wifi0]", 1)
        if len(sec) < 2:
            return None
        m = re.search(r"^address: (10\.0\.\d+\.\d+)/\d+", sec[1].split("\n[", 1)[0], re.M)
        return m.group(1) if m else None

    def wait_addr(prefix, secs):
        t0 = time.time()
        while time.time() - t0 < secs * scale:
            a = wifi_addr()
            if a and a.startswith(prefix):
                return a
            time.sleep(1)
        return None

    def wait_log(mark, rx, secs):
        t0 = time.time()
        while time.time() - t0 < secs * scale:
            m = re.search(rx, log_since(mark))
            if m:
                return m
            time.sleep(1)
        return None

    uuids = {}

    def profiles():
        _, out = sh("netctl profiles", 30)
        for m in re.finditer(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\s+\"([^\"]+)\"", out):
            uuids[m.group(2)] = m.group(1)
        return out

    def connect(ssid, prefix, secs=60):
        """User-initiated activation of a profile; wait for its lease."""
        profiles()
        rc, out = sh(f"netctl connect --uuid {uuids[ssid]}", 60)
        return wait_addr(prefix, secs) if prefix else (rc, out)

    def listen_ok(port, uport, tag):
        b = banner(port, 10 * scale)
        u = udp_echo(uport, 10 * scale) if udp else None
        ok = b.startswith("SSH-") and (u is None or u)
        return ok, f"{tag}: banner '{b}'" + ("" if u is None else f", udp echo {'ok' if u else 'NONE'}")

    udp = False
    try:
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        t0 = time.time()
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"netroamtest: boot failed: {r.stdout[-400:]}{r.stderr[-400:]}")
            return 1
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            print("netroamtest: login failed")
            return 1
        print(f"netroamtest: booted + logged in after {time.time() - t0:.0f} s (arch {arch}, scale {scale}); "
              f"wifi path 1 ssh {p1} udp {u1}, path 2 ssh {p2} udp {u2}", flush=True)
        # The UDP server starts before any address churn.
        rc, out = sh("/bin/netcheck --help 2>&1 | head -40", 30)
        udp = "--udp-echo" in out
        if udp:
            sh("( /bin/netcheck --udp-echo 7777 1800 > /tmp/udpecho.log 2>&1 & )", 20)
        else:
            print("netroamtest: netcheck has no --udp-echo; UDP checks skipped", flush=True)

        full = log_since(0)
        m = re.search(r"autoconnect: 'NetA' \(priority (-?\d+)\)", full) or wait_log(0, r"autoconnect: 'NetA' \(priority (-?\d+)\)", 60)
        a = wait_addr("10.0.3.", 60)
        verdict("legacy_auto", m is not None and m.group(1) == "0" and a is not None,
                f"{m.group(0) if m else 'no legacy autoconnect'}; wifi0 {a}")

        if only in (None, "tcp"):
            ok, d = listen_ok(p1, u1, "NetA")
            verdict("a_listen", ok, d)
            sh("netctl save NetB netb-pass-123", 60)
            b = connect("NetB", "10.0.4.", 60)
            ok, d = listen_ok(p2, u2, f"NetB {b}")
            verdict("b_listen", b is not None and ok, d)
            a = connect("NetA", "10.0.3.", 60)
            ok, d = listen_ok(p1, u1, f"NetA {a}")
            verdict("a2_listen", a is not None and ok, d)
            bad = []
            for n in range(4):
                ssid, prefix, port, up = (("NetB", "10.0.4.", p2, u2), ("NetA", "10.0.3.", p1, u1))[n % 2]
                ad = connect(ssid, prefix, 60)
                ok, d = listen_ok(port, up, f"{ssid} {ad}")
                if ad is None or not ok:
                    bad.append(d)
            verdict("churn", not bad, "; ".join(bad) or "4 switches, banner after each")
            st = open(driver.SSH_STATE_FILE).read().split()
            b0 = banner(int(st[1]), 10 * scale) if len(st) == 2 else "no eth0 forward"
            verdict("eth0_listen", b0.startswith("SSH-"), f"eth0 '{b0}'")

            # A stalled server must not wedge its listener.
            _, out = sh('for p in /proc/[0-9]*; do c=$(head -c 14 $p/cmdline); [ "$c" = "/usr/sbin/sshd" ] && echo "SSHD ${p#/proc/}"; done', 30)
            m = re.search(r"SSHD (\d+)", out)
            if m is None:
                verdict("stall", False, f"no sshd pid: {out.strip()[-200:]}")
            else:
                pid = m.group(1)
                sh(f"kill -STOP {pid}", 20)
                try:
                    c1 = socket.create_connection(("127.0.0.1", p1), timeout=10)
                    c1.sendall(b"SSH-2.0-netroam\r\n")
                    c1.shutdown(socket.SHUT_WR)
                    time.sleep(1)
                    c1.close()
                except OSError as e:
                    print(f"netroamtest: stall client 1: {e}", flush=True)
                c2 = socket.create_connection(("127.0.0.1", p1), timeout=10)
                c2.settimeout(3 * scale)
                try:
                    early = c2.recv(64)        # b'' = the guest refused it
                    queued = False
                except socket.timeout:
                    early, queued = None, True
                sh(f"kill -CONT {pid}", 20)
                c2.settimeout(15 * scale)
                try:
                    late = c2.recv(64).decode(errors="replace").strip() if queued else ""
                except OSError as e:
                    late = f"error {e}"
                c2.close()
                time.sleep(2)
                b3 = banner(p1, 10 * scale)
                verdict("stall", queued and late.startswith("SSH-") and b3.startswith("SSH-"),
                        f"while stopped: {'queued' if queued else 'refused ' + repr(early)}; after SIGCONT: '{late}'; new: '{b3}'")

        if only in (None, "auto"):
            if "NetB" not in uuids:
                sh("netctl save NetB netb-pass-123", 60)
            mark = log_mark()
            sh("netctl save Flaky flaky-pass-123", 60)
            time.sleep(5 * scale)
            moved = re.search(r"(connecting to|autoconnect: )'[^']+'", log_since(mark))
            a = wifi_addr()
            verdict("stay", moved is None and a is not None and a.startswith("10.0.3."),
                    f"wifi0 {a}" + (f"; moved: {moved.group(0)}" if moved else ""))
            # A failed user activation: back to the last network used (NetA).
            mark = log_mark()
            profiles()
            sh(f"netctl connect --uuid {uuids['Flaky']}", 60)
            m = wait_log(mark, r"autoconnect: '([^']+)'", 60)
            a = wait_addr("10.0.3.", 60)
            verdict("fail_return", m is not None and m.group(1) == "NetA" and a is not None,
                    f"{m.group(0) if m else 'no autoconnect'}; wifi0 {a}")
            # Flaky at a higher priority: autoconnect tries it once, lands
            # on NetA and stays there (no autoconnect while connected).
            time.sleep(16)  # Flaky's first back-off (15 s) from fail_return
            mark = log_mark()
            sh("netctl save Flaky flaky-pass-123 --prio 10", 60)
            sh("netctl disconnect wifi0 --keep-auto", 60)
            time.sleep(5)
            a = wait_addr("10.0.3.", 90)
            time.sleep(60)
            text = log_since(mark)
            tries = len(re.findall(r"connecting to 'Flaky'", text))
            verdict("noflap", a is not None and tries == 1 and (wifi_addr() or "").startswith("10.0.3."),
                    f"Flaky attempts {tries}; wifi0 {a} -> {wifi_addr()}")
            # Three disconnects in a row: every time back on NetA, and Flaky
            # tried at most once in all (its BSS backs off 15/60/120/300 s).
            mark = log_mark()
            ends = []
            for _ in range(3):
                m0 = log_mark()
                sh("netctl disconnect wifi0 --keep-auto", 60)
                m = wait_log(m0, r"connected to '([^']+)' in", 60)
                ends.append(m.group(1) if m else None)
                wait_addr("10.0.3.", 30)
            tries = len(re.findall(r"connecting to 'Flaky'", log_since(mark)))
            verdict("backoff", ends == ["NetA"] * 3 and tries <= 1, f"reconnected to {ends}; Flaky attempts {tries}")

        if only in (None, "sec"):
            full = log_since(0)
            rows = {s: re.findall(rf"not joinable: '{s}'[^\n]*", full) for s in ("TkipNet", "ShaNet", "MfpNet", "FtNet")}
            want = {"TkipNet": "group TKIP", "ShaNet": "PSK-SHA256", "MfpNet": "MFPR 1", "FtNet": "FT-PSK"}
            bad = [f"{s}: {r[:2]}" for s, r in rows.items() if len(r) != 1 or want[s] not in r[0]]
            verdict("rsn_log", not bad, "; ".join(bad) or "one RSN line per rejected BSS")
            _, wt = sh("cat /proc/net/wifi", 30)
            joins = {m.group(2): m.group(1) for m in re.finditer(r"^\S+\s+\d+\s+-?\d+\s+\S.*?\s(yes|no)\s+\d+s (\S+)\s*$", wt, re.M)}
            ok = joins.get("NetA") == "yes" and all(joins.get(s) == "no" for s in ("TkipNet", "ShaNet", "MfpNet", "FtNet"))
            verdict("join_col", ok, f"{joins}")
            causes = {"TkipNet": "TKIP group cipher", "ShaNet": "PSK-SHA256", "MfpNet": "management frame protection",
                      "FtNet": "FT-PSK"}
            bad = []
            for s, why in causes.items():
                mark = log_mark()
                sh(f"netctl connect {s} {'x' * 10}", 60)
                m = wait_log(mark, rf"connection to '{s}' failed: (\S+) \(([^\n]*)\)", 30)
                if m is None or m.group(1) != "unsupported-security" or why not in m.group(2):
                    bad.append(f"{s}: {m.group(0) if m else 'no failure line'}")
            verdict("sec_msgs", not bad, "; ".join(bad) or "four precise unsupported-security messages")
    finally:
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/netroam-{arch}-serial.log")
        except Exception:  # noqa: BLE001
            pass
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"netroamtest: kept: LEANDROS_RUN_ID={run_id} LEANDROS_DISK_IMAGE={img} LEANDROS_ROOT_IMAGE={root}")
    ok = bool(results) and all(results.values())
    print(f"netroamtest: {'PASS' if ok else 'FAIL'} {sum(results.values())}/{len(results)} ({arch}) "
          + " ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in results.items()), flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
