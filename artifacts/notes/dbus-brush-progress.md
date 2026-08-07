# dbus-brush lane — progress / checkpoint

Lane: HOST-ONLY, READ-ONLY. No QEMU, no git writes. Analyze why
`dbus-run-session` fails silently (exit 1) under brush on LeandrOS.
Date: 2026-07-23.

## STATUS: DONE. Root cause found, fix proposed + host-validated.

## Deliverables written
- `notes/dbus-brush-rootcause.md` — full root cause + fix + M6 validation list.
- `m5-session-ship/dbus-run-session.proposed` — NEW file (staged originals
  untouched). Pidfile-wrapper fix. Validated end-to-end under host brush.
- `notes/dbus-brush-progress.md` — this checkpoint.

## Root cause (one line)
`BUSD_PID=$!` read synchronously right after `busd ... &` returns the EMPTY
string under brush (backgrounded external cmd's pid is published
asynchronously via a Tokio "async pid slot"; foreground doesn't yield before
the next statement). Empty pid → `kill -0 ""` fails on loop iter 1 →
"busd exited before signaling readiness" → fast exit 1, child never runs.
Matches M5 symptom exactly. Reproduced 1:1 on host brush;
/bin/sh populates `$!` and works.

## Cleared (NOT the cause)
- fd redirect `3>file`: SUPPORTED (interp.rs:1742-1753); busd gets fd 3.
- parse/exec abort: none.
- /dev/null, test -s tmpfs, no-sleep: all fine.

## Key source refs (brush working tree = on-target)
- expansion.rs:1912-1918  `$!` -> current_job().representative_pid() -> "" if None
- interp.rs:298-317 / 541-550  async pid slot spawn + publish (uncommitted patch)
- interp.rs:1742-1753  numbered-fd redirect (works)
- entry.rs:177  new_multi_thread runtime (worker thread fills slot concurrently)

## Oracle used
`~/code/brush/target/release/brush` (0.4.0 e46b4ae-modified, built 07-21
09:59 > patched src 07-20 22:37 -> patch IS compiled in). `sh` shim -> brush
to emulate LeandrOS. Test scratch in /tmp/dbustest (host tmp, not protected).

## Fix
Primary (in .proposed): launch busd via `sh -c 'echo $$ >pidfile; exec busd
... --ready-fd 3' 3>readyfile &`; read pid with `read` builtin. Deterministic,
keeps fast-fail. ONE caveat for M6: `sh` must resolve on PATH on-target.
Fallback (no sh dep): poll `$!` INSIDE the loop (slot fills by iter ~9).
Both host-validated. Confidence: very high.

## Constraints honored
Did not touch leandros tree, ~/code/brush (read-only), or staged ship sets.
Only wrote under leandros-artifacts/notes + the new .proposed file. No build
of brush (used existing binary). No QEMU. No git.
