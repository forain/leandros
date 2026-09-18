# lane/brush — 2026-09-18 — the three brush job-control bugs

TODO.md "Open work" items: (1) brush wedges the login shell if a pipeline
wait errors before it restores the foreground pgrp; (2) `fg` of a stopped
pipeline re-reports Stopped; (3) `cmd &` shows `<pid unknown>`. All three are
**brush bugs**; the kernel's job control (lane/jobctl `503b11e`) behaved per
POSIX in every case examined. Fixed in the brush sibling (branch `leandros`,
commit `3423c0e` on the desktop checkout) and pinned here as
`ports/brush/patches/0003-*`. Both defects (1)/(2) and (3) reproduce on
plain Linux with brush's own pty tests, so they are upstream bugs, not
LeandrOS-specific.

## Root causes

**(2) — and the actual wedge behind (1).** `Job::move_to_foreground` /
`move_to_background` called `continue_process(pgid)` which is
`kill(pgid, SIGCONT)`: one pid, the group leader. ^Z stops the whole
foreground group, so `fg` of `sleep 5 | cat` continued `sleep` only; `cat`
stayed `Stopped` (Ctrl-T dump: `pid=59 … Stopped … /bin/cat`), `sleep`
finished, and `fg` then sat in `job.wait()` on `cat` for ever with the
terminal handed to the job — the login shell was gone. `Job::kill` (`kill
%n`) had the same single-pid defect. bash uses `killpg`. Now: signals go to
the job's process group (`killpg`) when it has one — taken from the spawned
children's `pgid`, not assumed to equal the first pid — and to each known
pid when it does not (job control off, members share the shell's group).
The terminal is handed over before SIGCONT so a resumed member does not take
SIGTTIN/SIGTTOU again.

**(1).** `Pipeline::execute` did `spawn…?` then `wait…?` and only the Ok
path of the wait called `move_self_to_foreground()`. Any error after a member
had taken the terminal (a later member failing to spawn, an expansion error,
a failed wait) left the tty's foreground group as a dead group, so the
shell's next console read is a background read: SIGTTIN, or EIO for an
orphaned login shell (exactly the rule lane/jobctl implemented). Now every
path of `Pipeline::execute` and of the `fg` builtin restores the shell's
group, like bash's `give_terminal_to(shell_pgrp)`.
**Refuted premise:** no pipeline wait actually errors on LeandrOS — every
error-path scenario below passes with the wait code unchanged; the wedge
that was observed was (2). The restore-on-every-path change is bash parity
and defence.

**(3).** `spawn_async_ao_list_in_task` formatted `[n] pid` right after
`tokio::spawn`, before the task had spawned anything, so the pid slot was
empty (`$!` read a moment later was correct — the slot filled in
afterwards). The slot is now a resolvable cell (`Mutex` + `Notify`); the
parent awaits its resolution — first external process spawned (pid + pgid),
a loop entered (`for`/`while`/`until`/arithmetic `for`: "no process"), or
the task finished — and only then announces (capped at 2 s so a blocking builtin as the
first command — `wait &` — cannot hold the parent; `$!`/`jobs` read the
slot lazily afterwards). A command substitution or nested command in the
first command's expansion runs through the same `params` and resolves it.
`fg` announces the job only after the hand-over + SIGCONT (as before), so a
^Z typed right after the announcement reaches the job, not the shell —
brush's own `run_suspend_and_fg` pty test catches the other order.

## What changed

Sibling `../brush` (desktop `leandros-siblings/brush`), branch `leandros`:
- `39d00b3` the previously UNCOMMITTED integration patches (crossterm fork
  wiring, `kill` builtin, pid slot v1) — now committed;
- `f02a328` the Mac-only loop-yield commit (`a8006e7` there) — replicated so
  both machines converge (**the Mac's `~/code/brush` still needs
  `ports/brush/sync.sh apply`**: its `a8006e7` bundles the pid-slot hunk and
  its working tree carries the other patches uncommitted);
- `3423c0e` the fix: `brush-core/src/jobs.rs`, `interp.rs`,
  `sys/unix/signal.rs` (+stubs), `brush-builtins/src/fg.rs`, plus tests.

This repo:
- `ports/brush/` — README (pin `e46b4ae` + 3 patches), `patches/`,
  `sync.sh check|apply|export` (check passes on the desktop sibling).
- `scripts/shjobs.py` — serial-console job-control regression (36 checks).
- TODO.md: the three items closed.

## Evidence

x86_64 / **KVM** (desktop): `scripts/shjobs.py` **36/36** ×3 (fresh boot via
getty login as root, repeat in-session, and as `leandro`); before the fix
**12/36** with the shell wedged in `fg` (`cat` Stopped, brush parked) —
every later check failed as a consequence.
aarch64 / **TCG** (desktop): `scripts/shjobs.py --timeout-scale 3` **36/36**
(fresh boot, getty login as root).
Linux host (desktop): `cargo test -p brush-shell --test
brush-interactive-tests` 6/6 (new `run_pipeline_suspend_and_fg`,
`run_in_background_reports_pid` — both FAIL on the pre-fix tree, verified by
stashing); compat suite vs bash 2157 ran, **0 failed** (380 known-to-fail,
pre-existing), incl. 5 new `$!`/background cases; clippy clean.

Scenarios (all must leave the shell taking typed commands): `sleep 2 &`
→ `[1]+ <pid>`, `$!` equal, `jobs -p`, `wait`; `sleep 1 | cat &`;
`sleep 5 | cat` ^Z → `jobs` → `fg` runs to completion (4.3 s), no Stopped
re-report, `jobs` empty; ^Z → `bg` → Done; `sleep 2 | true` (last member
exits first), `true | sleep 1`, `yes | head -n 2`; `sleep 1 | /nonexistent`,
`sleep 1 | cat < /nonexistent`, `sleep 1 | cat ${x!y}`; ^C of `sleep 10 |
cat`; `sleep 20 | sleep 20` ^Z → `kill -9 %1; wait` returns in 0.4 s.

## Harness notes (x86_64 serial)

`shjobs.py` keeps a reader thread on the serial socket at all times and
syncs each command on its own echoed newline before waiting for the prompt:
QEMU's serial back end drops guest output while the client is not reading
(the kernel's `putc` gives up on a back-pressured UART), and reedline
repaints the prompt while a line is typed, so a naive "wait for prompt"
returns before the command ran and the next keystrokes then overflow the
16-byte RX FIFO.

## Still open (upstream brush architecture, documented in ports/brush/README)

- A background job whose first command is a builtin/loop/function that
  spawns nothing has no pid (`$!` empty, `kill %n` fails): brush runs it as
  an in-process task, bash forks.
- `jobs` shows a job killed while stopped as `Stopped` until the next prompt
  reaps it; `wait <pid>` unimplemented.
