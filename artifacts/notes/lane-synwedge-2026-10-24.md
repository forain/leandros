# Lane synwedge (2026-10-24): hostfwd connects hang while Firefox plays YouTube

**FIXED (harness, not the guest).** Found by lane ffsw: on SLIRP (hostfwd 127.0.0.1:P -> guest :22),
after Firefox was relaunched on YouTube from an ffsession `--hostpost`, every new host connect to P
hung. Hypothesis "smoltcp stops answering inbound SYNs under load" is **REFUTED**.

## Root cause
`sample` of the wedged QEMU (r1, Firefox on YouTube, hang at probe 12 = ~25 s after launch):
- `CPU 3/HVF`: `pl011_write -> qemu_chr_write -> qemu_chr_write_buffer -> g_usleep` (2538/2561
  samples), i.e. the socket chardev's write-all retry loop on EAGAIN, **with the BQL held**.
- `qemu_main`: `main_loop_wait -> qemu_mutex_lock` (BQL), and CPUs 0-2 also on the BQL.
So the main loop stopped: SLIRP never accept()ed (host SYN_SENT / no banner, 156 bytes unread in an
existing hostfwd connection), the monitor and serial input stalled too. The client that stopped
reading was ffsession itself: it holds a `driver._Serial` connection (`SER`) for the whole session
and does not pump it during `subprocess.run(--hostpost)`. Once the guest printed more than the
unix-socket buffer on the console, the vCPU spun forever holding the BQL. Same hazard for every
`_Serial` holder that sleeps or runs a subprocess (doomsession, nmsession, kparkstest, ...).

## Fix
- `driver._Serial` drains the socket in a reader thread for as long as the connection is held
  (logs to SERIAL_LOG, answers CPR probes); `pump()` waits on a condition, `send()` shares the
  non-blocking socket under a tx lock. Callers are unchanged.
- `scripts/build-all.sh` copies `~/.ssh/leandros_remote.pub` (or `$LEANDROS_REMOTE_KEY.pub`) into the
  ESP as `::/leandros/authorized_keys`, so remote.py / ssh as root works on QEMU without mcopy by hand.

## Guard: `host:inloadtest.py` (in net-e2e; alias `inload`)
Own boot on SLIRP; guest bulk load via `netcheck --get --par 4`, `--put`, a `--via` connect loop
against a host server at 10.0.2.2 (~930 Mbit/s moved), host banner probes on the hostfwd port,
in-guest `netcheck --via 127.0.0.1 22` x20, then a held unpumped `_Serial` while the guest prints
~400 KB on the console.
- unfixed driver: inbound 93/93, local 20/20, **serial 0/6 FAIL**
- fixed: inbound 93/93 (max 0.04 s), local 20/20, serial 59/59 PASS
- Firefox YouTube repro via ffsession `--hostpost` (120 probes, 2 s apart): unfixed wedged at
  probe 12; fixed 120/120.

## Left open
- A `--hostpost` script still cannot use `driver.py cmd` while ffsession holds the serial
  connection (QEMU's socket chardev serves one client); use ssh (remote.py) from hostposts.
- The warm-SW-cache question of lane ffsw (L11.11) is untouched; QEMU ssh now stays up for it.
