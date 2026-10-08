# Lane seriallogin — 2026-10-01

Branch `lane/seriallogin`, from origin/main `aff49e1`, with origin/main `6a500e1` (cmdhang) merged in. Head `e280b45`, pushed. Worktrees are removed on both the Mac and the desktop.

## Outcome: FIXED (ours, but not the premise)
The premise was that typed bytes vanish while virtio-gpu initialises. **That premise is REFUTED: no input byte was ever lost.** Across all runs, every boot had a working shell: the password was accepted and a probe command `echo SL$((6*7))OK` returned `SL42OK` on the first try. Typeahead also survives: a command typed right behind the password, before brush is up, ran on 8 of 8 x86 boots.

The real cause is **console OUTPUT splicing**. Kernel diagnostic lines (`[FORK]`, `[MMAP-BIG]`, `[GPU]`, `[DRM]`, `[EVDEV]`) are built piece by piece through `arch_serial_putc` and `print_number`, and none of those pieces took `CONSOLE_OUT_LOCK`. Whole-string prints did take it, but gave up after a 100k-spin budget and printed anyway.

User console writes hold the lock for milliseconds, because every byte costs one MMIO exit plus a framebuffer flush. So kernel lines from another vCPU were interleaved character by character into `Password: ` and `brush-0.5# `. For example, `[FOPRasKs]wo rtdg: id=4` is "Password: " woven into "[FORK] tgid=4". This happens during the greetd → cosmic-comp → virtio-gpu burst. The harness never saw the prompt, timed out, and typed into the wrong prompt.

TCSAFLUSH is not involved: the console's TCSETSF does not flush anything (`servers/tty termios_rw`).

## Fixes
1. **Kernel (`kernel/src/main.rs`, `init.rs`, `syscall.rs`): line-atomic kernel diagnostics.**
   - Each CPU stages its diagnostic bytes into one line.
   - A finished line goes into a FIFO outbox of 8 KiB.
   - The outbox is written only by whoever holds `CONSOLE_OUT_LOCK`, taken with `try_lock`. Interrupt context never spins on the lock.
   - `console_write_user` drains the outbox after it releases the lock.
   - Staging starts at `sched::run`. A panic drains the outbox and turns staging off.
   - The first version, which only staged lines and still used a spin budget, did not help: aarch64 went to 13/30 boots with a splice.
2. **Harness (`driver.py cmd_login`).**
   - The post-password markers `"> "`/`"# "` matched kernel lines such as `[GPU] MSI-X armed: control queue -> `. `login` then returned before brush was up, and the first `cmd` came back empty: 6 of 30 x86 boots.
   - `login` now waits for a real shell prompt (`\S[#$] `), with complete and half-printed kernel lines stripped first.

## Evidence (harness `slloop.py`, one persistent timestamped capture per boot; splice = kernel tag followed by ESC/prompt text on the same line)
| run | x86_64 KVM desktop `--virgl` | aarch64 HVF Mac `--virgl` (gles31 QEMU) |
|---|---|---|
| base (main + trace) | 30/30 shells; prompt spliced 7/30 (4 Password, 3 brush); any splice 22/30 boots | 59/60 shells seen, 1/30 prompt spliced; any splice 13/30 and 7/30 |
| final, capture loop | 30/30 ok, prompt seen 30/30, **0/30 boots with a splice** | 30/30 ok, **0/30 splice** |
| final, real `driver.py login` + `cmd` | **30/30**, login 0 s | **30/30**, login 0 s |

- Bytes in vs bytes received, from a temporary trace (dropped from the tree): `uart_rx` equalled the bytes sent; `tapdrop` 0; backlog 0.
- cmdhang overlap: before the merge, with my driver fix in, 3 of 30 x86 `driver login`+`cmd` runs lost the command's echo and output. That is consistent with the old 5 ms putc dropping output under host back-pressure, which is cmdhang's fix. After the merge it was 30/30.
- Regressions:
  - ptytest 15/15 on both arches.
  - polltest: 0 FAIL on both arches (x86 p50 36 µs).
  - COSMIC session (virglflake `vfloop.py`): x86 3/3 and aarch64 3/3. Each had 13 session pids, settings 5/5, no deaths, serial integrity 3/3, `login_tries` 0.

## Shared files
- `kernel/src/main.rs` and `syscall.rs` (`console_write_user`) were also touched by cmdhang; the merge was clean.
- `.claude/skills/run-leandros/driver.py`.

## Open
- A staged kernel line with no trailing `\n` waits until its CPU prints a newline or the stage fills. All current diagnostic lines end in `\n`.
- Kernel lines still use the non-patient putc, so they can be dropped under host back-pressure. That is by contract.
- Raw logs and harness: desktop `leandros-siblings/sl-out/`, `slloop.py`, `splice2.py`; Mac session scratchpad.
