# lane/writevatomic 2026-11-10 - FIXED

Root cause (from tapsynack): sys_writev issued one write per iovec, so pasta's 3-iovec tap frame (eth/IP/TCP) arrived as three runt frames.

Fix: writev/readv are one operation.
- tun/tap: tundev::writev gathers all iovecs into ONE frame; tundev::readv scatters ONE frame.
- sockets: writev/readv go through NET_SENDMSG/NET_RECVMSG with msghdr_ptr == 0 and (iov_ptr, iovcnt) in args 3/4 (no name, no control), so dgram/seqpacket/inet-dgram records are one message and readv never consumes a second record. Netlink answers -38 and keeps the per-iovec loop. A blocking stream socket that took only part of the vector continues with the remainder.
- pipes: total <= PIPE_BUF is gathered into a kernel buffer and handed to the ring as one write (all or nothing, EAGAIN when it does not fit).
- Regular files / other devices: unchanged (append lock across the vector).
- SIOCSIFMTU cap 65000 -> 65535 (cherry-pick of tapsynack d693faa8).

Not changed: other DynamicDevices (uinput/evdev) and pipe writev > PIPE_BUF stay per-iovec.

Tests: userland/tuntest vec_cases (22 checks). On the unfixed kernel 13 failed (tap readv, dgram/seqpacket writev+readv, pipe atomic); with the fix 22/22.
