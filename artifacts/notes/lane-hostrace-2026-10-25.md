# lane/hostrace (openbugs #75) - FIXED

Root cause (both symptoms): runtests booted its own VM (run id X) before host: scripts; inloadtest/linkmontest
start their own VM under X-<name>, so two QEMUs opened the same disk image (write lock) and inloadtest's serial
connect failed. In net-e2e earlier host tests had already stopped the runtests VM, hiding it.
Fixes: runtests skips its own boot when every command is a self-booting host script (script source contains "start")
and stops its VM before a self-booting script; driver cmd_stop now waits for QEMU exit (term, then kill) instead of sleeping 1+2 s.
Evidence (x86_64): before, `host:inloadtest.py host:linkmontest.py` inloadtest dies in 19 s with "cannot connect to serial socket" (x3);
after, it runs its full 175 s (x2). Remaining linkmontest/inload serial failures on x86 TCG are test-content, not harness.
