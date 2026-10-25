# lane/inloadx86 — 2026-10-25 — FIXED (openbugs #69)

x86_64/TCG net-e2e `inloadtest` serial case: the 5000-line console spew did not
finish within 60 s. Real kernel inefficiency (a), not a test budget problem.

## Root cause
Console `write(2)` -> `serial_write_raw` -> per-byte UART + framebuffer-console
mirror -> one `fb_flush` per write, which blits the dirty rect from the shadow to
the boot framebuffer. After every scroll (every 8 lines) that rect is the whole
1920x1080x4 surface. The blit used `copy_nonoverlapping` -> the kernel's
byte-loop `memcpy` (kernel/src/mem.rs), and under TCG each store into the VGA BAR
is a separate access.

TSC profile (temporary counters, 500-line echo loop, x86_64/TCG):
- console write total 10.9 G cycles for 528 writes (~20 ms/write)
- blit 9.85 G (90%): 62 full-surface blits, ~160 ms each (~52 MB/s)
- glyph draw + shadow scroll 0.95 G; UART path itself cheap
- VT switched away (no fb mirror): 235-277 lines/s vs 40 lines/s on VT1.

## Fix
`drivers/src/framebuffer.rs`: `copy_px_wide` — the blit copies each row with
`cld; rep movsq` (+ one tail pixel) on x86_64; other arches unchanged.

## Numbers (x86_64/TCG, 500-line `echo` loop over serial, VT1 on screen)
| build | lines/s |
|---|---|
| main cc5ae528 | 39-44 (3 runs) |
| blit fix | 115 |
| (rejected) kernel-wide rep-string memcpy | 130 |

5000 lines: ~113-125 s before (> 30 s + 60 s window) -> ~43 s after.

inloadtest serial: before `FAIL 57/57 ... spew NOT finished` (integ16, cc5ae528);
after (blit fix 5db79d9a) `PASS 57/57 ... spew finished`.

## Gates (blit fix)
- x86_64/TCG `--suite net-e2e`: overall PASS (all 22 commands, incl. inloadtest serial 57/57, linkmontest, netflaptest)
- aarch64 `--suite net-e2e`: overall PASS (inloadtest serial PASS, inbound 274/274)
- e1000e x86_64 probe: `dhcp: bound`
- netflaptest `down` failed once at 1.60 s (fixed-kernel-wide variant run), passed 2/2 on rerun: timing flake.

## Rejected variant (new finding)
Replacing the kernel's `memcpy/memset/memmove` with `rep movsq/stosq` on x86_64
(f6a9a6a3, reverted a8a61763) made inloadtest pass but left the e1000e NIC
without a DHCP lease (linkmontest `setup: FAIL no e1000e lease`, 3/3; reverting
mem.rs alone restores `dhcp: bound`). Something in the e1000e/netd path depends
on byte-wise copy semantics or timing. Repro: build f6a9a6a3, boot
`LEANDROS_NIC=e1000e LEANDROS_NET=user` x86_64, `cat /proc/net/nic` has no
`dhcp: bound`. Open; also the generic byte-loop mem* stays slow on both arches.
