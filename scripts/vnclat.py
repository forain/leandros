#!/usr/bin/env python3
"""Input-to-pixels latency probes over a persistent VNC connection.

Works on every x86_64/aarch64 GPU path the run-leandros driver starts (Venus
and virgl both attach a loopback VNC listener to the GL console; screendump
cannot photograph either). The VNC server pushes only changed rectangles, so
"the first frame that differs" is timed to the update, not to a poll period.

Needs numpy (pillow for --save). Keys go through the driver's QMP socket, so
run it with the same LEANDROS_RUN_ID/LEANDROS_VNC_PORT as the running guest.

  vnclat.py launcher N     Super tap -> launcher visible; type 't' -> first
                           echo and results settled; Esc -> closed
  vnclat.py overview N     Super+W -> overview visible; Esc -> closed
                           (falls back to Super+W to close when Esc fails)
  vnclat.py shot FILE.png  save the current frame

Environment: DRIVER_DIR (run-leandros skill dir), LEANDROS_VNC_PORT (5909),
OUT (directory for per-step PNGs; optional).
"""
import os, socket, struct, sys, time
import numpy as np

sys.path.insert(0, os.environ.get("DRIVER_DIR", os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", ".claude", "skills", "run-leandros")))
import driver  # noqa: E402

PORT = int(os.environ.get("LEANDROS_VNC_PORT", "5909"))
OUT = os.environ.get("OUT")
DIFF = 40          # per-channel difference that counts as a changed pixel


class Vnc:
    def __init__(self):
        self.so = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        self._rd(12)
        self.so.sendall(b"RFB 003.008\n")
        n = self._rd(1)[0]
        self._rd(n)
        self.so.sendall(b"\x01")          # security: none
        self._rd(4)
        self.so.sendall(b"\x01")          # shared
        self.w, self.h = struct.unpack(">HH", self._rd(4))
        self._rd(16)
        self._rd(struct.unpack(">I", self._rd(4))[0])
        # 32 bpp little-endian xRGB, raw encoding only
        self.so.sendall(struct.pack(">Bxxx", 0) + struct.pack(
            ">BBBBHHHBBBxxx", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
        self.so.sendall(struct.pack(">BxHi", 2, 1, 0))
        self.fb = np.zeros((self.h, self.w, 4), dtype=np.uint8)
        self.last_update = 0.0
        self._req(0)
        self.pump(30)

    def _rd(self, n):
        b = bytearray()
        while len(b) < n:
            c = self.so.recv(n - len(b))
            if not c:
                raise IOError("vnc eof")
            b += c
        return bytes(b)

    def _req(self, inc):
        self.so.sendall(struct.pack(">BBHHHH", 3, inc, 0, 0, self.w, self.h))

    def pump(self, timeout):
        """Handle one server message; True if it was a framebuffer update."""
        self.so.settimeout(timeout)
        try:
            mt = self._rd(1)[0]
        except socket.timeout:
            return False
        finally:
            self.so.settimeout(30)
        if mt == 0:
            self._rd(1)
            nr = struct.unpack(">H", self._rd(2))[0]
            for _ in range(nr):
                x, y, rw, rh, enc = struct.unpack(">HHHHi", self._rd(12))
                if enc != 0:
                    raise IOError(f"encoding {enc}")
                data = self._rd(rw * rh * 4)
                self.fb[y:y + rh, x:x + rw] = np.frombuffer(data, np.uint8).reshape(rh, rw, 4)
            self.last_update = time.time()
            self._req(1)
            return True
        if mt == 2:
            return False
        if mt == 3:
            self._rd(3)
            self._rd(struct.unpack(">I", self._rd(4))[0])
            return False
        raise IOError(f"vnc msg {mt}")

    def frame(self):
        return self.fb[:, :, :3].copy()

    def diff(self, base):
        """Fraction of (2x-subsampled) pixels differing from `base`."""
        a = self.fb[::2, ::2, :3].astype(np.int16)
        b = base[::2, ::2].astype(np.int16)
        return float((np.abs(a - b).max(axis=2) > DIFF).mean())

    def quiet(self, dur, limit=15.0):
        """Pump until no update for `dur` s (at most `limit` s). Returns the
        time of the last update seen."""
        t0 = time.time()
        last = t0
        while time.time() - last < dur and time.time() - t0 < limit:
            if self.pump(0.1):
                last = time.time()
        return last

    def wait(self, pred, timeout):
        """Pump until pred() holds; seconds waited, or None on timeout."""
        t0 = time.time()
        if pred():
            return 0.0
        while time.time() - t0 < timeout:
            if self.pump(0.2) and pred():
                return self.last_update - t0
        return None

    def save(self, path):
        from PIL import Image
        Image.fromarray(self.frame()[:, :, ::-1]).save(path)   # BGR -> RGB


QMP = None


def _ev(q, down):
    return {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": q}}}


def keys(*qs, hold=0.06):
    """Press qs in order, hold, release in reverse (a chord, or one key)."""
    global QMP
    if QMP is None:
        QMP = driver._qmp_open()
    driver._qmp_command(QMP, "input-send-event", {"events": [_ev(q, True) for q in qs]})
    time.sleep(hold)
    driver._qmp_command(QMP, "input-send-event", {"events": [_ev(q, False) for q in reversed(qs)]})


def snap(v, name):
    if OUT:
        os.makedirs(OUT, exist_ok=True)
        v.save(os.path.join(OUT, name + ".png"))


def fmt(x):
    return "TIMEOUT" if x is None else f"{x:.3f}"


def summary(name, xs):
    ok = sorted(x for x in xs if x is not None)
    if not ok:
        print(f"{name}: n=0/{len(xs)}")
        return
    print(f"{name}: n={len(ok)}/{len(xs)} min={ok[0]:.3f} p50={ok[len(ok)//2]:.3f} max={ok[-1]:.3f}")


def launcher(v, n):
    show, echo, settle, close = [], [], [], []
    for i in range(n):
        v.quiet(1.5)
        base = v.frame()
        keys("meta_l")
        t_show = v.wait(lambda: v.diff(base) > 0.01, 30)
        v.quiet(0.8)
        snap(v, f"launcher{i}-open")
        opened = v.frame()
        t0 = time.time()
        keys("t")
        t_echo = v.wait(lambda: v.diff(opened) > 0.0005, 30)
        last = v.quiet(1.5)
        t_settle = (last - t0) if t_echo is not None else None
        snap(v, f"launcher{i}-typed")
        keys("esc")
        t_close = v.wait(lambda: v.diff(base) < 0.003, 10)
        snap(v, f"launcher{i}-closed")
        print(f"launcher {i}: show={fmt(t_show)} echo={fmt(t_echo)} settle={fmt(t_settle)} "
              f"esc_close={fmt(t_close)}", flush=True)
        show.append(t_show); echo.append(t_echo); settle.append(t_settle); close.append(t_close)
        if t_close is None:           # leave it closed for the next round
            keys("esc"); v.quiet(1.0)
    summary("launcher_show", show)
    summary("type_echo", echo)
    summary("type_settle", settle)
    summary("launcher_esc_close", close)


def overview(v, n):
    show, close, fallback = [], [], []
    for i in range(n):
        v.quiet(1.5)
        base = v.frame()
        keys("meta_l", "w")
        t_show = v.wait(lambda: v.diff(base) > 0.01, 30)
        v.quiet(1.0)
        snap(v, f"overview{i}-open")
        keys("esc")
        t_close = v.wait(lambda: v.diff(base) < 0.002, 8)
        snap(v, f"overview{i}-after-esc")
        t_fb = None
        if t_close is None:
            keys("meta_l", "w")
            t_fb = v.wait(lambda: v.diff(base) < 0.002, 15)
        print(f"overview {i}: show={fmt(t_show)} esc_close={fmt(t_close)}"
              + ("" if t_close is not None else f" superw_close={fmt(t_fb)}"), flush=True)
        show.append(t_show); close.append(t_close); fallback.append(t_fb)
    summary("overview_show", show)
    summary("overview_esc_close", close)


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    v = Vnc()
    if sys.argv[1] == "shot":
        v.quiet(0.5, 3)
        v.save(sys.argv[2])
        print(v.w, v.h)
    elif sys.argv[1] == "launcher":
        launcher(v, int(sys.argv[2]))
    elif sys.argv[1] == "overview":
        overview(v, int(sys.argv[2]))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
