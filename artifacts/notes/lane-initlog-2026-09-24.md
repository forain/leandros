# lane initlog — "init stops producing console output" (integ-wave-0924 @ ee7c029, aarch64/HVF)

Outcome: REFUTED as an init/kernel bug. It is a capture artifact of the driver.

- driver.py's serial is `-chardev socket,server=on,wait=off`; the serial log only records bytes while a
  driver command is connected. With no client, QEMU stops draining the PL011, TXFF stays set, and the
  kernel's TX-wedge latch (arch/aarch64/src/uart.rs putc) drops bytes (UART_TX_DROPPED).
- The no-GPU banner is printed a few seconds AFTER the `login:` prompt (greeter-real forks gpuprobe
  after the getty is up), i.e. exactly in the gap between `driver.py start` returning and the next
  `login`/`cmd` connecting.
- Evidence (same ee7c029 tree, aarch64/HVF, no GPU):
  * continuous socket reader attached right after `start`: banner present (2/2).
  * connect 25 s after boot: no banner on serial, but /proc/4 is gone (init reaped greetd) and a
    framebuffer screendump shows the full banner under the login prompt
    (artifacts/notes/lane-initlog-banner-fb.png) — init's write(1) went through the console path
    (the VT mirror got every byte), only the UART side was dropped.
  * `driver.py login root root; driver.py cmd exit`: "session ended, restarting login" present 3/3,
    both in cmd output and in the serial log.
- epollwake f71bb55 / x86_64 KVM showing the banner = timing luck, not the tree.
- No commit on lane/initlog (nothing to fix in init/kernel).
- How to verify the banner headlessly: attach a reader right after start, or screendump, or keep one
  driver session connected. A durable fix would be a persistent serial pump in driver.py.
- Side note: kernel `[FORK]` lines render as bare digit runs on the framebuffer console
  (print_number path), cosmetic.
