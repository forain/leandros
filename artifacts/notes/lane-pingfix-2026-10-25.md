# lane/pingfix — 2026-10-25 (openbugs L12 #45)

## Verdict: ROOT-CAUSED (stale /bin/ping on the stick), debug-print FIXED

### ping 100% loss on the laptop
- Not a kernel/net bug. On the laptop (kernel `#1 97ffd47688`), `nettest -g 172.16.0.2`
  passes icmp_raw/icmp_dgram 4/4 and `/tmp/ping-new` (current x86_64 build, md5 caac72c2…)
  gets 2/2 replies from the Mac, while `/bin/ping` (md5 9dc45685…, 239456 B) times out
  against the same target in the same session.
- Cause: b1b100db (2026-10-20, L9.5) made SOCK_RAW ICMP reads carry the IPv4 header
  (raw(7)) and taught ping to skip it in the same commit. Kernel deploys swap only the
  kernel + initrd; the stick's rootfs still holds a pre-b1b100db ping, which reads
  byte 0 (0x45) as the ICMP type and never sees an echo reply.
- QEMU (both arches, current build): ping to the gateway / 1.1.1.1 / 127.0.0.1 all reply.
  (10.0.2.2 does not answer here: this Mac's QEMU net is socket_vmnet, gateway 192.168.105.1.)
- Not deployed: tkiprekey held the laptop. Next step: `remote.py deploy-files
  userland/target/x86_64-unknown-leandros/release/ping:/bin/ping:755` (or refresh the
  stick rootfs). Other relibc-linked rootfs binaries on the stick are probably stale too.

### ld_so debug lines
- relibc 642ec469 left unconditional `ld_so::init tp=` and `PH type=` writes to fd 2 in
  `src/ld_so/mod.rs`; every relibc-linked static binary printed them. Removed in relibc
  master 1b5f01e3 (not pushed).

### Guard
- nettest `ping_tool`: runs `/bin/ping -c 2 -W 2 <gateway>`, needs 2 replies, exit 0
  and empty stderr. Red on the unfixed relibc (`stderr not empty: "ld_so::init tp=0x0…"`),
  green after. aarch64 regress PASS (tag pfreg).
