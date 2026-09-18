# brush (LeandrOS port)

Upstream: <https://github.com/reubeno/brush>, MIT. The bash-compatible shell
that is the login shell for every account (`/bin/brush`, hardlinked as
`/bin/sh`).

Pinned upstream commit `e46b4ae` (`build(deps): bump the github-actions group
with 9 updates (#1238)`, 2026-09). The LeandrOS deltas are the patches in
`patches/`, applied in name order on top of that commit; they are the
commits of the `leandros` branch of the sibling checkout, produced with
`git format-patch e46b4ae..leandros`.

## Where it is built from

Unlike the ports with a `build.sh`, brush is not fetched into a `.work`
tree: `scripts/build-all.sh` (`build_brush`) builds the **sibling checkout**
`../brush` (`~/code/brush` on the Mac, `leandros-siblings/brush` on the
Linux boxes) with the repo's pinned nightly for both musl targets, and
`scripts/mkfs-f2fs-populated.py` packs `../brush/target/<triple>/release/brush`.
That checkout must be upstream `e46b4ae` plus exactly these patches.
`sync.sh check` verifies that (tree clean, HEAD's diff against the pin equals
the patches); `sync.sh apply` resets a checkout to the pin and applies them.

brush also needs the sibling `../crossterm` (the 0.29.0 fork with the CPR
desync fix, 2 commits: unmodified import + fix); patch 0001 wires it in
through `[patch.crates-io]`. A missing `../crossterm` fails the build
outright.

## The LeandrOS deltas

`0001-LeandrOS-integration-…`: the `[patch.crates-io] crossterm` line, the
`kill` builtin fixes (default SIGTERM, numeric `-s N`, `kill -0` probes with
signal 0 — `dbus-run-session` relies on it) and the first version of the
background-job pid slot.

`0002-interp-yield-cooperatively-…`: `consume_budget()` at the top of every
loop iteration so N builtin-only background loops cannot occupy all N tokio
workers and freeze the shell.

`0003-jobs-continue-and-signal-whole-process-groups-…` (lane/brush,
2026-09-18): the three job-control bugs from TODO.md "Open work":

* `fg`/`bg`/`kill %n` sent their signal to one pid; a stopped pipeline has
  every member stopped, so `fg` continued the leader only, then waited on
  the still-stopped members for ever with the terminal handed to the job:
  the login shell hung. Signals now go to the job's process group
  (`killpg`) when it has one, per pid otherwise (job control off).
* A pipeline failing after a member took the terminal propagated the error
  without giving the terminal back (bash: `give_terminal_to(shell_pgrp)` on
  every path). `Pipeline::execute` and `fg` restore the shell's group on
  every path.
* `cmd &` printed `[1] <pid unknown>`: the parent now waits for the
  background task to decide its first command (first process spawned, a
  loop entered, or the task done) before announcing, so `[n] pid` and `$!`
  are real immediately. The slot also carries the pgid.

All three reproduce on Linux too: `cargo test -p brush-shell --test
brush-interactive-tests` gains `run_pipeline_suspend_and_fg` and
`run_in_background_reports_pid` (both fail on `e46b4ae`+0001+0002), and
`tests/cases/compat/background_jobs.yaml` gains `$!` cases compared against
bash. The guest-side regression is `scripts/shjobs.py` (serial console: ^Z,
`fg`, `bg`, `jobs`, `kill %1`, `cmd &`, pipelines whose members exit in
either order, spawn/redirect failures mid-pipeline, ^C — the shell must
still execute a typed command after every case).

## Known limits (upstream architecture, not regressions)

* A background job whose first command is a builtin, a function body that
  spawns nothing, or a loop has no pid: brush runs it as an in-process task
  (bash forks a subshell). `$!` is empty and `kill %n` fails for such jobs.
* `jobs` lists a job killed while stopped as `Stopped` until the next
  prompt reaps it (`Done`); `wait` returns immediately, so nothing is stuck.
* `wait <pid>` is not implemented (`wait` without arguments is).
