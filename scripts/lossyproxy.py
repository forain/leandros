#!/usr/bin/env python3
"""lossyproxy.py - TCP proxy that randomly kills or stalls connections.

Test tool for scripts/remote.py push robustness (lane pushsafe). Each
connection survives a random number of bytes (exponential, mean --mean-bytes);
then, with probability --stall, it freezes (stops forwarding, keeps the socket
open: a silent Wi-Fi black hole) else it is reset.

  lossyproxy.py --listen 2800 --to 127.0.0.1:2763 --mean-bytes 6000000 \\
      --stall 0.5 --state /tmp/leandros-lossy-ssh   # then LEANDROS_REMOTE=qemu:lossy
"""
import argparse, random, socket, struct, threading, time


def pump(a, b, budget, stall, ev):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            if budget[0] is not None and budget[0] <= len(d):
                d = d[:max(budget[0], 0)]
                if d:
                    b.sendall(d)
                if stall:
                    time.sleep(3600)  # black hole: hold the connection open
                else:
                    a.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                break
            if budget[0] is not None:
                budget[0] -= len(d)
            b.sendall(d)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try:
                s.close()
            except OSError:
                pass


def serve(c, a):
    try:
        u = socket.create_connection((a.th, a.tp), timeout=5)
    except OSError:
        c.close()
        return
    live = random.random() > a.healthy
    budget = [int(random.expovariate(1 / a.mean_bytes))] if live else [None]
    stall = random.random() < a.stall
    # budget is shared: whichever direction exhausts it first kills the connection
    threading.Thread(target=pump, args=(c, u, budget, stall, None), daemon=True).start()
    threading.Thread(target=pump, args=(u, c, budget, stall, None), daemon=True).start()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, default=2800)
    ap.add_argument("--to", default="127.0.0.1:2763")
    ap.add_argument("--mean-bytes", type=float, default=6e6)
    ap.add_argument("--stall", type=float, default=0.5)
    ap.add_argument("--healthy", type=float, default=0.0, help="fraction of connections left alone")
    ap.add_argument("--state", help="write '127.0.0.1 <port>' here (driver ssh-state format)")
    ap.add_argument("--seed", type=int)
    a = ap.parse_args()
    random.seed(a.seed)
    a.th, tp = a.to.rsplit(":", 1)
    a.tp = int(tp)
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", a.listen))
    s.listen(64)
    if a.state:
        open(a.state, "w").write(f"127.0.0.1 {a.listen}\n")
    while True:
        c, _ = s.accept()
        threading.Thread(target=serve, args=(c, a), daemon=True).start()


if __name__ == "__main__":
    main()
