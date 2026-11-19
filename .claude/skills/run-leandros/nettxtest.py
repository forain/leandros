"""nettxtest.py [aarch64|x86_64] [--keep] [--secs S] -- net daemon cost of a bulk TCP download.

Boots, logs in, waits for the DHCP lease, starts a host HTTP server that streams zeros, and runs
`/bin/netcheck --get` in the guest for S seconds (default 10). Around the transfer it samples
/proc/net/nic (`netd: passes`, `netd_why`) and the netd task's CPU time, and reports
passes/s, Mbit/s, passes per MB, timer-limited sleeps by length and daemon CPU %.
  bulk: daemon passes per MB moved < BOUND and immediate re-polls per MB < IMM_BOUND (a pass
        per ACK or per RX batch is fine; spinning on a full driver TX queue is not: 15 k/MB).
Options: --tx (guest -> host over ssh, scp pull's direction), --shape SPEC (netshape= knob on a copy
of the boot image, e.g. eth0:20000:20000:64:256:2 for a Wi-Fi-like 20 Mbit/s link); a shaped
download must reach RATE_SHARE of the link rate and drop <= DROP_SHARE of its frames in the RX queue). Exit 0 iff PASS.
Run id `<caller run id>-nettx`.
"""
import http.server
import os
import hostclean
import re
import socketserver
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
BOUND = 3000.0  # daemon passes per MB moved
IMM_BOUND = 100.0  # immediate (poll_delay 0) re-polls per MB moved
RATE_SHARE = 0.5  # shaped download: share of the link RX rate it must reach
DROP_SHARE = 0.01  # shaped download: largest share of frames the RX queue may drop


class Zeros(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        n = 1 << 30
        self.send_response(200)
        self.send_header("Content-Length", str(n))
        self.end_headers()
        buf = b"\0" * 65536
        try:
            while n > 0:
                self.wfile.write(buf)
                n -= len(buf)
        except OSError:
            pass

    def log_message(self, *a):
        pass


def main(argv):
    arch = "x86_64" if "x86_64" in argv else ("aarch64" if "aarch64" in argv else os.environ.get("LEANDROS_TEST_ARCH", "aarch64"))
    keep = "--keep" in argv
    secs = float(argv[argv.index("--secs") + 1]) if "--secs" in argv else 10.0
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    os.environ["LEANDROS_RUN_ID"] = f"{caller}-nettx" if caller else "nettx"
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    tx = "--tx" in argv
    shape = argv[argv.index("--shape") + 1] if "--shape" in argv else ""
    if shape:
        import shutil
        img = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(driver.REPO_ROOT, f"leandros-limine-{arch}.img")
        timg = os.path.join(driver.REPO_ROOT, f"nettx-shape-{arch}.img")
        if subprocess.run(["cp", "-c", img, timg], capture_output=True).returncode != 0:  # APFS clone; GNU cp has no -c
            subprocess.run(["cp", "--sparse=always", img, timg], check=True)
        subprocess.run([os.path.join(driver.REPO_ROOT, "scripts/set-image-cmdline.sh"), timg, f"netshape={shape}"],
                       check=True, capture_output=True)
        os.environ["LEANDROS_DISK_IMAGE"] = timg

    def sh(cmd, t=30):
        return driver.serial_run(cmd, timeout=t * scale, arch=arch).get("output", "")

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    socketserver.TCPServer.allow_reuse_address = True
    srv = socketserver.ThreadingTCPServer(("0.0.0.0", 0), Zeros)
    srv.daemon_threads = True
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    ok, detail = False, "no sample"
    try:
        stop()
        stt = subprocess.run([sys.executable, DRV, "start", arch], capture_output=True, text=True, timeout=900)
        if stt.returncode != 0:
            print("start failed: " + (stt.stdout + stt.stderr)[-600:], flush=True)
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            capture_output=True, text=True, timeout=1200)
        t0 = time.time()
        while time.time() - t0 < 120 * scale and "dhcp: bound" not in sh("cat /proc/net/nic"):
            time.sleep(2)
        time.sleep(5)
        pid = re.search(r"NETDPID /proc/(\d+)/", sh("for f in /proc/[0-9]*/comm; do read c < $f; [ \"$c\" = netd ] && echo NETDPID $f; done"))
        host = "10.0.2.2" if "10.0.2." in sh("cat /proc/net/nic") else "192.168.105.1"
        statf = f"/proc/{pid.group(1)}/stat" if pid else "/dev/null"
        probe = f"read u r < /proc/uptime; echo NI $u; cat /proc/net/nic; echo ST $(cat {statf})"
        if tx:
            # Guest -> host bulk (scp pull's direction): sshd streams zeros.
            import sshsmoke, tempfile
            tmp = tempfile.mkdtemp(prefix="nettx-")
            hostclean.track(tmp)
            h, p = sshsmoke.target()
            ssh = sshsmoke.Ssh(h, p, tmp)
            for _ in range(30):
                if ssh.run("leandro", "true", password="leandro", timeout=60)[0] == 0:
                    break
                time.sleep(2)
            nbytes = int(secs * 2.5e6)
            out = sh(probe)
            t1 = time.time()
            rc, so, _ = ssh.run("leandro", f"head -c {nbytes} /dev/zero", password="leandro", timeout=600)
            el = time.time() - t1
            out += "\n" + sh(probe)
            out += f"\nNETCHECK get: {'PASS' if rc == 0 and len(so) == nbytes else 'FAIL'} ssh {len(so) / 1048576.0:.1f} MB in {el:.2f} s = {len(so) * 8 / el / 1e6:.1f} Mbit/s\n"
        else:
            out = sh(f"{probe}; /bin/netcheck --get http://{host}:{port}/z --secs {secs}; {probe}", secs + 90)
        s = [(float(m.group(1)), int(m.group(2))) for m in re.finditer(r"^NI (\d+\.\d+)\n(?:.*\n)*?netd: passes (\d+) ", out, re.M)]
        why = re.findall(r"^netd_why: kicked (\d+) rearm (\d+) timer_sleeps (\d+) .*? early (\d+) active (\d+) immediate (\d+) tx_held (\d+) timer_expired (\d+) timer_hist (\d+)/(\d+)/(\d+)/(\d+)", out, re.M)
        st = [l.split() for l in re.findall(r"^ST (.*)$", out, re.M)]
        g = re.search(r"NETCHECK get: (\w+) .*?([\d.]+) MB in ([\d.]+) s = ([\d.]+) Mbit/s", out)
        print("\n".join(re.findall(r"^(?:netd_(?:why|kick): |shape: |tcp_recovery: |tcp_rx: |tcp_rxwin: |neighbor: |eth0|NETCHECK|  get\|).*$", out, re.M)[-16:]), flush=True)
        if lg.returncode == 0 and len(s) == 2 and g and g.group(1) == "PASS":
            dt = s[1][0] - s[0][0]
            mb, gsecs, mbit = float(g.group(2)), float(g.group(3)), float(g.group(4))
            passes = s[1][1] - s[0][1]
            ppmb = passes / mb if mb else float("inf")
            ok = ppmb < BOUND and len(why) == 2
            # Shaped download: the stack must sustain >= half the link's RX rate
            # (before SACK on active opens it got 0.6 of 20 Mbit/s: one hole
            # repaired per sender RTO after the slow-start overshoot).
            rate_ok = True
            if shape and not tx:
                f = shape.split(":")
                rx_kbit = int(f[2]) if len(f) > 2 and f[2].isdigit() else 0
                if rx_kbit:
                    rate_ok = mbit >= RATE_SHARE * rx_kbit / 1000.0
                    print(f"shaped download: {mbit:.1f} Mbit/s of {rx_kbit / 1000.0:.1f} "
                          f"(need >= {RATE_SHARE * rx_kbit / 1000.0:.1f}): {'PASS' if rate_ok else 'FAIL'}", flush=True)
            ok = ok and rate_ok
            # Shaped download: the receive window must not let the sender's slow
            # start overshoot the 256-frame netshape RX queue. With a fixed 2 MiB
            # window main dropped 4.4 % of the frames (694-716 of ~16 k, queue at
            # 256); receive-window autotuning drops none (lane rxwin).
            if shape and not tx:
                sh_rx = [(int(m.group(1)), int(m.group(2))) for m in
                         re.finditer(r"^shape: .*?; rx \d+ kbit/s passed (\d+) dropped (\d+)", out, re.M)]
                if len(sh_rx) == 2:
                    passed, dropped = sh_rx[1][0] - sh_rx[0][0], sh_rx[1][1] - sh_rx[0][1]
                    drop_ok = dropped <= DROP_SHARE * max(passed, 1)
                    print(f"shaped download: rx queue dropped {dropped} of {passed + dropped} frames "
                          f"(need <= {DROP_SHARE * 100:.1f} %): {'PASS' if drop_ok else 'FAIL'}", flush=True)
                    ok = ok and drop_ok
            if len(why) == 2:
                d = [int(b) - int(a) for a, b in zip(*why)]
                ok = ok and d[5] / max(mb, 1e-9) < IMM_BOUND
                print(f"delta kicked {d[0]} rearm {d[1]} timer_sleeps {d[2]} early {d[3]} active {d[4]} "
                      f"immediate {d[5]} tx_held {d[6]} timer_expired {d[7]} timer_hist(<100us/<1ms/<10ms/>=10ms) {d[8]}/{d[9]}/{d[10]}/{d[11]}", flush=True)
            cpu = ""
            if len(st) == 2 and len(st[0]) > 15:
                ticks = sum(int(st[1][i]) - int(st[0][i]) for i in (13, 14))
                cpu = f", netd cpu {ticks / dt:.1f}% (ticks/s at 100 Hz)"
            detail = (f"{mb:.1f} MB in {gsecs:.2f} s = {mbit:.1f} Mbit/s, daemon passes {passes / dt:.0f}/s, "
                      f"{ppmb:.0f} passes/MB (bound {BOUND:.0f}), immediate {d[5] / max(mb, 1e-9) if len(why) == 2 else -1:.0f}/MB (bound {IMM_BOUND:.0f}){cpu}")
        else:
            detail = f"login rc={lg.returncode} samples={s} get={g.group(0) if g else None} out={out[-300:]!r}"
        print(f"bulk: {'PASS' if ok else 'FAIL'} {detail}", flush=True)
    finally:
        srv.shutdown()
        if not keep:
            stop()
    print(f"nettxtest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(hostclean.run(main, sys.argv[1:]))
