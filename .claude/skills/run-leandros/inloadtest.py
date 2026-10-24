"""inloadtest.py [aarch64|x86_64] [--keep] -- inbound TCP connects while the guest moves bulk traffic.

Boots on SLIRP (LEANDROS_NET=user, hostfwd 127.0.0.1:<port> -> guest :22), logs in, starts
heavy guest traffic against a host server at 10.0.2.2 (parallel bulk downloads, uploads and a
loop of short connections), then opens new host connections to the forwarded sshd port in a
loop and requires each to get the SSH banner. From inside the guest it also connects to
127.0.0.1:22 (splits slirp from the guest stack).
  inbound: every host connect gets "SSH-" within 10 s (a lost one = an unanswered SYN).
  local:   every in-guest connect to 127.0.0.1:22 gets the banner.
  serial:  while a held driver._Serial is not pumped (ffsession's --hostpost) and the guest
           prints ~400 KB on the console, host connects still get the banner. Before the
           reader thread, QEMU's chardev write spun with the BQL held once the socket buffer
           filled, freezing SLIRP: every new hostfwd connect hung (lane synwedge).
Exit 0 iff PASS.  Run id `<caller run id>-inload`.
"""
import os
import re
import socket
import socketserver
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
WINDOW = int(os.environ.get("INLOAD_WINDOW", "60"))
STREAMS = int(os.environ.get("INLOAD_STREAMS", "4"))
CHUNK = b"\0" * 65536
MOVED = [0]  # bytes the host server sent + received (load check)


class Bulk(socketserver.BaseRequestHandler):
    """An HTTP GET -> 200 with an endless body of zeros; anything else (netcheck --put, --via)
    is swallowed until EOF."""
    def handle(self):
        s = self.request
        s.settimeout(120)
        try:
            head = s.recv(4096)
            if head.startswith(b"GET"):
                s.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 1099511627776\r\nConnection: close\r\n\r\n")
                while True:
                    s.sendall(CHUNK)
                    MOVED[0] += len(CHUNK)
            while head:
                head = s.recv(65536)
                MOVED[0] += len(head)
        except OSError:
            pass


class Srv(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


def banner(host, port, t=10.0):
    t0 = time.time()
    try:
        with socket.create_connection((host, port), timeout=t) as c:
            c.settimeout(max(0.5, t - (time.time() - t0)))
            buf = b""
            while b"\n" not in buf:
                d = c.recv(256)
                if not d:
                    break
                buf += d
            return buf.startswith(b"SSH-"), time.time() - t0
    except OSError:
        return False, time.time() - t0


def main(argv):
    arch = "x86_64" if "x86_64" in argv else ("aarch64" if "aarch64" in argv else os.environ.get("LEANDROS_TEST_ARCH", "aarch64"))
    keep = "--keep" in argv
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    os.environ["LEANDROS_RUN_ID"] = f"{caller}-inload" if caller else "inload"
    os.environ["LEANDROS_NET"] = "user"
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")

    def sh(cmd, t=30):
        return driver.serial_run(cmd, timeout=t * scale, arch=arch).get("output", "")

    def stop():
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    srv = Srv(("0.0.0.0", 0), Bulk)
    hport = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    ok = False
    try:
        stop()
        subprocess.run([sys.executable, DRV, "start", arch], capture_output=True, text=True, timeout=900)
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                            capture_output=True, text=True, timeout=1200)
        tgt = subprocess.run([sys.executable, DRV, "ssh-target"], capture_output=True, text=True, timeout=30).stdout.split()
        host, port = tgt[0], int(tgt[1])
        t0 = time.time()
        while time.time() - t0 < 120 * scale and "dhcp: bound" not in sh("cat /proc/net/nic"):
            time.sleep(2)
        t0 = time.time()
        while time.time() - t0 < 120 * scale and not banner(host, port, 5)[0]:
            time.sleep(2)
        base, _ = banner(host, port)
        print(f"baseline: {'ok' if base else 'NO BANNER'} (login rc={lg.returncode}, target {host}:{port})", flush=True)
        # Load: bulk downloads, uploads and a short-connection loop, all to the host via the gateway.
        url = f"http://10.0.2.2:{hport}/bulk"
        load = [f"(while :; do netcheck --get {url} --par {STREAMS} --secs 600 > /dev/null; done) &",
                f"(while :; do netcheck --put 10.0.2.2 {hport} 100000 --secs 600 > /dev/null; done) &",
                f"(while :; do netcheck --via 10.0.2.2 {hport} > /dev/null; done) &"]
        sh(" ".join(load) + " echo LOADUP", 30)
        time.sleep(5)
        fails, lat, n = [], [], 0
        m0 = MOVED[0]
        t0 = time.time()
        while time.time() - t0 < WINDOW:
            good, dt = banner(host, port)
            n += 1
            lat.append(dt)
            if not good:
                fails.append(round(time.time() - t0, 1))
            time.sleep(0.2)
        mb = (MOVED[0] - m0) / 1e6
        print(f"load: {mb:.0f} MB moved through the host server in {WINDOW} s ({mb * 8 / WINDOW:.0f} Mbit/s)", flush=True)
        nic = sh("cat /proc/net/nic; cat /proc/net/tcp | head -n 40", 30)
        loc = sh("i=0; g=0; while [ $i -lt 20 ]; do netcheck --via 127.0.0.1 22 > /tmp/via.out 2>&1 && g=$((g+1)); i=$((i+1)); done; echo LOCAL $g/20", 300)
        m = re.search(r"LOCAL (\d+)/20", loc)
        lok = bool(m) and m.group(1) == "20"
        iok = base and n > 0 and not fails
        print(f"inbound: {'PASS' if iok else 'FAIL'} {n - len(fails)}/{n} host connects got the banner, "
              f"max {max(lat or [0]):.2f} s; failures at {fails[:10]}", flush=True)
        print(f"local: {'PASS' if lok else 'FAIL'} {m.group(0) if m else loc[-200:]!r}", flush=True)
        if not (iok and lok):
            print("--- /proc/net/nic + tcp at the end ---\n" + nic[-3000:], flush=True)
        ok = iok and lok
        # serial: a held, unpumped serial connection under heavy console output.
        sh("kill %1 %2 %3 2>/dev/null; pkill netcheck; echo STOPPED", 30)
        ser = driver._Serial()
        try:
            ser.send(b"\r")
            time.sleep(1)
            ser.send(b"i=0; while [ $i -lt 5000 ]; do echo 0123456789012345678901234567890123456789012345678901234567890123456789012345; i=$((i+1)); done; echo SPEWDONE\r")
            sfails, sn = [], 0
            t0 = time.time()
            while time.time() - t0 < 30:
                good, _ = banner(host, port, 5)
                sn += 1
                if not good:
                    sfails.append(round(time.time() - t0, 1))
                time.sleep(0.5)
            done = ser.pump(60, until=lambda b: b"SPEWDONE\r\n" in b or b"SPEWDONE\n" in b)
        finally:
            ser.close()
        sok = not sfails and done
        print(f"serial: {'PASS' if sok else 'FAIL'} {sn - len(sfails)}/{sn} host connects got the banner while a held "
              f"serial connection was not pumped; spew {'finished' if done else 'NOT finished'}; failures at {sfails[:10]}", flush=True)
        ok = ok and sok
    finally:
        srv.shutdown()
        if not keep:
            stop()
    print(f"inloadtest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
