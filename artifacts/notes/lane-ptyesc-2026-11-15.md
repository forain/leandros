# lane/ptyesc 2026-11-15 - "pty drops a leading ESC in the CPR reply"

Not a kernel bug. Every VT 2..6 runs a login (userland/init) blocked reading the
same session pty vttest drives (VT_OPENQRY found none free, fell back to VT 6).
The login woke on the injected CPR reply and consumed byte 0 (ESC) before vttest
read the rest. Evidence: 86/200 replies started with '[' (n=5, no spins);
servers/tty/src/vt.rs reply (r[0]=0x1b) and pty.rs raw/read paths are lossless.

Fix: vttest `login_pause` (TIOCGSID + SIGSTOP the session leader, SIGCONT after)
around tabs_wide and the new `cpr_esc` case (200 CPRs, every reply must start
with ESC); `cpr` helper is strict again. Red 52-86/200 bad before, 0 after.
