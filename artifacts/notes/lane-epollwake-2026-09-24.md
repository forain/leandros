# lane/epollwake — "brush never prints its prompt" (2026-09-25)

Branch `lane/epollwake` @ `f71bb55` (base origin/integ-wave-0924 `e790571`). Mac aarch64/HVF + x86_64/TCG, `--no-gpu`.

## Result: REFUTED as a kernel bug; harness fix
Repro (`/tmp/epollwake/hang.py`: loop `drmsmoke; pthreadtest` via driver `_serial_send`, 15 s timeout, raw serial
slice + ^T dump on timeout) on base e790571 aarch64/HVF: 21 "hangs" in 23 iterations. In **21/21** the raw serial
stream after `RC=0` contains the full repainted prompt (`ESC[?2004h ... brush-0.5# ... ESC7 ESC8 ESC[?25h`),
immediately followed by a `[TLBSTAT] t=... \n` line that the BSP timer tick writes straight to the UART
(cowtlb instrumentation; printed every 10 s when the period had an exec, i.e. right after a test command).
`_at_prompt` requires the prompt at the very END of the stream → timeout. ^T dump: brush main thread in
epoll_pwait dl=inf on crossterm's epoll = reedline's blocking key read AFTER painting (CPR wait would have a finite
deadline). Nothing wakes "on next input" because nothing was stuck. Same signature in cowtlb's dumps
(6/24 on ungated TLBSTAT, ~1/12 once gated).

## Fix
`.claude/skills/run-leandros/driver.py`: `_at_prompt` drops whole trailing `[TAG] ...\n` kernel lines from the raw
tail (before `_strip_ansi`, which eats "[T") and then matches the prompt. No kernel change.

## Evidence (after)
- aarch64/HVF: 60 iterations, 0 hangs, 48 TLBSTAT lines glued after a prompt tolerated.
- x86_64/TCG: 20 iterations, 0 hangs (2 glued).
- epolltest 11/11, wakepolltest 45/45, polltest (p50 193 µs, p99 2.1 ms, early 0), sigtest, pthreadtest: RC=0 both arches.

## Open / suggestion
Any harness with its own "prompt at tail" check has the same false positive while `[TLBSTAT]` stays on by default;
consider gating it behind a flag. Shared files touched: none (driver only).
