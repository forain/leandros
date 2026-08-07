#!/usr/bin/env python3
"""Minimal QMP client for injecting input into the LeandrOS QEMU (virtio-tablet
absolute pointer + virtio-keyboard). The driver's monitor socket is HMP, which
cannot inject an absolute pointer position; QMP input-send-event can. Requires
QEMU started with `-qmp unix:/tmp/leandros-qmp.sock,server,nowait`.

Usage:
  qmp.py move <x> <y>      # abs axes in [0..32767]
  qmp.py click             # left button down+up
  qmp.py key <qcode>...    # press+release each QEMU keycode (e.g. a b c ret)
"""
import socket, json, sys, time

SOCK = "/tmp/leandros-qmp.sock"


def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    for _ in range(40):
        try:
            s.connect(SOCK); break
        except OSError:
            time.sleep(0.15)
    else:
        sys.exit("QMP: cannot connect " + SOCK)
    s.settimeout(3.0)
    _recv(s)                                    # greeting
    _cmd(s, {"execute": "qmp_capabilities"})
    return s


def _recv(s):
    buf = b""
    while True:
        try:
            c = s.recv(4096)
        except socket.timeout:
            break
        if not c:
            break
        buf += c
        if b"\n" in c:
            break
    return buf.decode(errors="replace")


def _cmd(s, obj):
    s.sendall((json.dumps(obj) + "\r\n").encode())
    return _recv(s)


def send_events(s, events):
    print(_cmd(s, {"execute": "input-send-event", "arguments": {"events": events}}).strip())


def main():
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    s = connect()
    if a[0] == "move":
        x, y = int(a[1]), int(a[2])
        send_events(s, [
            {"type": "abs", "data": {"axis": "x", "value": x}},
            {"type": "abs", "data": {"axis": "y", "value": y}},
        ])
    elif a[0] == "click":
        send_events(s, [{"type": "btn", "data": {"button": "left", "down": True}}])
        time.sleep(0.05)
        send_events(s, [{"type": "btn", "data": {"button": "left", "down": False}}])
    elif a[0] == "key":
        for qc in a[1:]:
            send_events(s, [{"type": "key", "data": {"key": {"type": "qcode", "data": qc}, "down": True}}])
            time.sleep(0.03)
            send_events(s, [{"type": "key", "data": {"key": {"type": "qcode", "data": qc}, "down": False}}])
            time.sleep(0.03)
    else:
        sys.exit(__doc__)
    s.close()


if __name__ == "__main__":
    main()
