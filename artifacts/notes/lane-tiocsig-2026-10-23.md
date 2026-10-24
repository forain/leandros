# lane/tiocsig (L10.2) - FIXED

Pty master TIOCSIG took its argument through a pointer (and the kernel tty ioctl bounce table copied 4 bytes in from it), so Linux-style `ioctl(master, TIOCSIG, SIGINT)` returned EFAULT.

Fix: `kernel/src/syscall.rs` no longer bounces 0x40045436 (arg is a value); `servers/tty/src/pty.rs` TIOCSIG treats arg as an int signal number, EINVAL outside 1..=64, else signals the slave's foreground pgrp. relibc only defines the constant; no wrapper change needed.

Guard: ptytest `tiocsig` (16 -> 17 cases): child in slave foreground pgrp, EINVAL for 0 and 65, TIOCSIG SIGUSR1 delivered. Red on unfixed main (`fails=1 tiocsig`), green after fix; aarch64 full regress 45/45 ok.
