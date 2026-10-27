# lane piserialxfer — file push + kexec over the serial console (Pi 5, no network) — DELIVERED (QEMU); Pi untried

## What
- `userland/serrx/serrx.c` (static, 52 KB): `serrx <path>` receives framed lines
  `#<H|D|E|X><seq8><len4><crc32_8>:<base64>\n`, go-back-N cumulative ACK/NAK (`\r@A<seq>.`/`\r@N<seq>.`,
  CR-prefixed, no newline: no fb-console scroll), writes `<path>.part`, fsync, re-reads + sha256, rename, dir fsync.
  Raw tty while running, restored on every exit; 30 s idle timeout; `-V` probe, `-s <file>` sha256. In /bin via mkfs list.
- `scripts/pi-serial-push.py <local> <remote>` (`--port` dev or `unix:<sock>`, `--cmd`, `--kexec-exec`): refuses if
  `lsof` shows another process on the device (names it; never kills), TIOCEXCL, answers brush CPR, window 3 x 768 B
  (< 4 KiB backlog + 1 KiB ring), resend on NAK/3 s stall, progress + ETA, abort frame on any exception.
  Bootstraps a missing serrx by typing base64 into `head -c N | base64 -d` (~7 s), verified by sha256.
- `scripts/pi-serial-kexec.sh <kernel> <initrd> [extras]`: push both to /boot/pi/trial, info, `kexec -l --reuse-cmdline
  --append='<extras> sxk=<nonce>'`, `kexec -e`, answer login (LEANDROS_PI_LOGIN), check /proc/cmdline in the new kernel.
- `--baud`: NOT done — the console has no baud ioctl (tty TCSETS ignores speeds) or /proc knob; script rejects != 115200.

## Evidence
- host:serialxfertest.py (runtests `serialxfer`, aarch64 HVF): bootstrap + 64 KiB push 7.1 s 9.1 KiB/s; kernel-direct
  5.7 MB + initrd pushed (~420 s), 0 resends, sha OK; kexec'd kernel shows `serialxfer=1 sxk=...` — PASS.
- Local pty loopback with 30% frames corrupted/truncated: 300 KB, 200 frames resent, sha OK.
- Expected on the Pi at 115200: ~9 KiB/s payload => ~10.5 min per 5.7 MB kernel.

## Found
- Kernel: console read fast path returns EFAULT (-14) for a user buffer in never-touched .bss
  (validate_user_buf on an unfaulted page). Repro: static C `static char b[4096]; read(0,b,4096)` on the serial
  console => -1 EFAULT. serrx pre-touches its buffers. Open.
- Console stdin can be O_NONBLOCK after brush: serrx polls on EAGAIN.
