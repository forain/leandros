# lane/harness — 2026-10-02

Branch `lane/harness`, worktree `.claude/worktrees/harness`, base `08ac17c`. Not merged, not pushed.

## Commits
- `332f6fc` run-leandros: serial_run with a sentinel-framed exit status
- `ef98cb3` run-leandros: move runtests/ffsession/magcount into the skill
- `b1452ca` drm: per-object GETPARAMs for dumb/virgl BO lifetime
- `e03028b` drmsmoke: FB lifetime cases follow their own object; serialise v3d block
- ffsession: census processes in two execs; Firefox ready by count
- this note

## 1. Harness
Why `RC=?` happened (all three seen in old results.txt):
- Kernel lines ([FORK], [MMAP-BIG], [GPU]...) reach the UART unsynchronised with tty output, and
  some are written in pieces (`serial_debug("[DRM] fb "); serial_debug_hex(..)`), so they land
  inside the `RC=` line.
- The read ended on a prompt-shaped tail: scmtest prints `... -> ` and pauses; `\n\S*[#$>] \Z`
  matched `\n-> `, the read stopped mid-test, the next command was typed into the running
  test, and the RC that came back belonged to another command (old aarch64 results: scmtest
  truncated, its RC attributed to vfstest).
- A command typed before brush was back at its prompt lost its head.

Now (`driver.serial_run`, `driver.py run "<cmd>" [timeout] [arch]`):
- the command runs as `<cmd>; __r=$?; echo $__r >/tmp/.lrc-N; echo; echo "<<LRC:N:"$__r">>"`
  with a fresh nonce. The echoed line reads `<<LRC:N:"$__r">>`, so only shell output matches;
- the read ends only on that sentinel (searched plain and with whole kernel lines cut out) or
  on the timeout;
- if the shell is back at its prompt but the sentinel is unreadable, the status comes from
  `/tmp/.lrc-N` via a second sentinel-framed command (`status=rc-file`); still running at the
  deadline -> ^C, `status=timeout`;
- typed only after the echo is seen intact (whitespace-insensitive, kernel lines removed),
  else ^C and retype;
- `accel_kind()`, `wait_scale()` (3 on TCG, 1 on HVF/KVM, `LEANDROS_WAIT_SCALE`). `login`
  exits 2 when no shell prompt appears.

Tools now live in `.claude/skills/run-leandros/` (old `artifacts/notes/lane-firefox-tools/*.py`
paths are exec shims), documented in SKILL.md:
- `runtests.py <arch> <tag> [--suite regress|drm] [--virgl] [--repeat N] [--timeout S] [cmd...]`:
  one line per command, `summary.json`, exit status. `regress` = 13 suites + vfstest.
- `ffsession.py`: waits on processes (cosmic-greeter, cosmic-panel, cosmic-term, >= 3
  firefox processes) polled over one held serial connection. Process census = one
  `cat /proc/[0-9]*/cmdline | tr`; the old per-pid `readlink` loop took > 90 s with Firefox up.
  Firefox's children come from its fork server and keep its argv (no `-contentproc`), hence the
  count. `--*-timeout`, `--scale`, `steps.json`.

## 2. drmsmoke FB_SWEPT_ON_CLOSE flake
Root cause: a test assumption, not a kernel race. The failing run printed
before/mid/after/swept = 79/81/80/1: the sweep ran (swept +1, count fell by one at close), but
one foreign object appeared between the `before` and `mid` reads. The case compared the
device-wide `VIRTGPU_PARAM_LEANDROS_DUMB_OBJS`. The greeter's compositor is DRM master on a
virgl boot, and its virgl 3D resources live in the same registry. drm_release_open sweeps
synchronously inside close().
Not reproduced on the unmodified tree in 120 runs on aarch64/HVF (20 in one boot, 20 fresh boots,
80 across greeter restarts). An idle greeter holds dumb_objs at 99 for 180 s at 1 Hz, so the
competing allocation is transient (start-up / redraw), which fits 1 failure in 4.
Fix: GETPARAMs `LEANDROS_DUMB_OBJ_OF` (handle -> object id) and `LEANDROS_DUMB_OBJ_REFS`
(object -> refs, 0 = destroyed), in/out via `value`. The FB cases now assert on their own
object: 2 refs with handle + fb, 1 after DESTROY_DUMB, 0 after RMFB/close; the swept counter
must advance by >= 1.
Also: two drmsmoke at once failed V3D_* cases (one's disarm of the device-global v3d backend
broke the other's block). The v3d block now takes an exclusive flock on
`/tmp/.drmsmoke-v3d.lock`, released by the kernel if the holder dies. Verified: a background
30x drmsmoke loop beside a foreground 20x: 21/21 failed=0, FB_* all PASS, no V3D failure.
Note: arming still changes the device identity for every other client while the block runs.
That is inherent to the design; just don't start Mesa clients during a drmsmoke run.

## Verification (final tree, Mac; build-all.sh OK, 48 GB free)
| | aarch64/HVF | x86_64/TCG |
|---|---|---|
| regress (13 + vfstest), 3 fresh boots | 42/42 RC=0 | 42/42 RC=0 |
| drmsmoke --virgl, 3 fresh boots | 3/3 RC=0, failed=0, FB_SWEPT PASS | 3/3 RC=0, failed=0, FB_SWEPT PASS |
| RC=? / non-`ok` status | 0 / 0 (all 45 `ok`) | 0 / 0 (all 45 `ok`) |
| ffsession virgl (desktop + Firefox file://) | greeter 0.7 s, desktop 0.8 s, term 0.7 s, Firefox 10.7 s; magenta 0 | greeter 1.4 s, desktop 15.2 s, term 1.3 s, Firefox 44.1 s; magenta 0 |
Firefox stayed up for the whole observation on both arches (no EXIT line).
