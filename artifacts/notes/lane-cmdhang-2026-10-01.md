# Lane cmdhang (2026-10-01): FIXED

Branch `lane/cmdhang` @ `1b70a64` (base 59cd980). Commits: bc66bd8, 2ecb6c7, 187c38d, 1b70a64.

## 1. `driver.py cmd "pthreadtest; echo RC=$?"` hang: two bugs, one per arch
- **aarch64 (harness bug).** `_serial_send` arms the prompt check when the echoed command shows up, but it did NOT test the prompt on the chunk that armed it. On HVF the echo tail, all of pthreadtest's output and the next prompt sometimes arrive in ONE recv (measured: 4 chunks instead of the usual 22-36), and then no further chunk ever comes, so the read ran to its full timeout. The guest was correct the whole time: the raw tail ends in a proper prompt and `_at_prompt()` returns True on it. Before the fix 12/20 runs hung (30 s timeout); after the fix 20/20 return in about 0.36 s. Fix: test the prompt on the arming chunk too, on the bytes after the echo only.
- **x86_64/KVM (guest bug: lost serial TX).** 1 of 20 runs (2/40 in total) lost about 600 contiguous bytes: the rest of the echo, the [FORK]/ld.so lines and the first PASS line. QEMU's 16550 writes one byte per chardev write, and a Linux AF_UNIX socket takes only **278** one-byte writes before EAGAIN (macOS: 8192). So any longer burst stalls THRE until the host reader gets scheduled. Past 5 ms, `putc` latched TX_WEDGED and dropped everything until the reader caught up. Fix: user console writes (`serial_write_raw` -> new `putc_patient`, both arches) wait up to 200 ms, with their own latch. IRQ-context output keeps the 5/10 ms bound. After the fix 40/40 runs pass, max 1.25 s.

## 2. COSMIC Settings missing from the launcher and app library
Only the binary was staged; there was no `com.system76.CosmicSettings.desktop`. Fix: commit upstream's file to `artifacts/m6-session-data/shared/usr/share/applications/`, and stage the hicolor icons from `m6-session-bins/src/cosmic-settings/resources/icons`, falling back to cosmic-epoch. The fallback order matters: the desktop's cosmic-epoch subset has no cosmic-settings, and its first image showed a placeholder gear.

## Evidence
| | aarch64 Mac HVF (virgl gles31) | x86_64 desktop KVM (virgl) |
|---|---|---|
| pthreadtest via driver | 20/20, max 411 ms (was 8/20) | 40/40, max 1251 ms (was 38/40) |
| ptytest | 15 PASS RC=0 | 15 PASS RC=0 (x2) |
| jobtest | PASS RC=0 | PASS RC=0 |
| Super + "settings" | COSMIC Settings listed with icon, Enter opens window | same |
| Super+A app library | Settings tile (real icon) + Terminal; search+Enter opens Settings | same |
| session counts | exc/PF/SEGV/panic/WDOG all 0 | all 0 |

## Open
- `com.system76.CosmicAppletTime.desktop` wants `application-default-icon` (warning predates this lane).
- The per-page Settings entries (NoDisplay) are not staged.
- IRQ-context kernel diagnostics can still be dropped under back-pressure (by design).
