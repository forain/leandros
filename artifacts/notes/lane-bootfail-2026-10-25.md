# lane/bootfail 2026-10-25 — FIXED (openbugs L12 #68)

## Symptom
`runtests.py <arch> <tag>` reported `boot failed (attempt 0)` on every boot (twcrash 3/3,
bssidbar, integ15 x86_64), although serial-full.log reached `login: `.

## Cause (regression from lane/vncport ac7a59ce)
`cmd_start` calls `_release_vnc_pick()` after the serial socket appears and before it
connects its serial client. That function waits up to 5 s for QEMU to bind the picked
VNC port. Without `--venus`/`--virgl` there is no `-vnc` listener, so nothing ever binds
it and the wait always runs the full 5 s. The serial chardev is `server=on,wait=off`
and drops output while no client is attached; HVF/TCG guests print `login: ` in under
5 s, so the driver connected after the prompt had gone by (its serial.log began at the
`[ckpt] 5000 ms` stats line, ~1 KB) and then waited 120 s for a prompt that never came
again -> exit 1 -> "boot failed".

## Fix
- `_release_vnc_pick(wait_bind)`: wait for the bind only when the QEMU command line has
  `-vnc`.
- `cmd_start`: if the live serial read times out, accept a `login: ` already present in
  QEMU's own logfile (SERIAL_FULL_LOG), so a late-attaching client can never miss the
  prompt again.

## Evidence
- Unfixed, aarch64 `/bin/true`: boots failed 1/1, serial.log 1059 B with 0 `login: `,
  serial-full.log has 1.
- Fixed: aarch64 regress PASS 2/2 (boots failed 0/1 each, live serial.log saw `login: `,
  so the primary fix works, not only the fallback); x86_64 TCG boot+login PASS 1/1.
- Orchestrator suspicion (periodic `[pcache]`/`[ckpt]` lines breaking prompt match) REFUTED:
  no change to those prints in 3 days; `login: ` is a substring match and survives
  interleaving; the stats lines were simply the first output after the 5 s-late attach.

lane/serialkbd (unmerged) touches only kernel/evdev/doom/evtest2, not the harness: unrelated.
