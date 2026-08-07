# dbus-run-session silent exit-1 under brush — root cause + fix

Host-only static + empirical analysis. No QEMU, no git writes.
Date: 2026-07-23. Analyst lane: dbus-brush (read-only).

Oracle: host brush binary `~/code/brush/target/release/brush`
(brush 0.4.0, git e46b4ae-**modified**, built 2026-07-21 09:59 — **includes**
the uncommitted working-tree patches: jobs.rs/interp.rs source is 2026-07-20
22:37, older than the binary, so the AsyncPidSlot patch IS compiled in).
This binary therefore faithfully represents the on-target brush behavior for
job-control / `$!`.

---

## TL;DR

Root cause is **not** the fd redirection and **not** any parse/exec abort.
It is a single line:

```
"$BUSD_BIN" ... --ready-fd 3 3>"$READY_FILE" &
BUSD_PID=$!          # <-- returns "" under brush
```

Under brush, `$!` read **synchronously on the statement right after `&`**
evaluates to the **empty string**. `BUSD_PID` is therefore empty, and the
poll loop's liveness guard

```
if ! kill -0 "$BUSD_PID" 2>/dev/null; then
    echo "dbus-run-session: busd exited before signaling readiness" >&2
    exit 1
```

runs `kill -0 ""`, which fails on the **first** iteration → prints
"busd exited before signaling readiness" → **exit 1, fast, child never
runs**. This matches the M5 symptom exactly ("fast exit 1, child never
runs", stderr not captured so it looked silent).

Reproduced 1:1 on the host (see Evidence). `/bin/sh` on the same script
populates `$!` and the script succeeds.

---

## Constructs used by the staged script + brush support verdict

Staged file: `~/code/leandros-artifacts/m5-session-ship/<arch>/usr/bin/dbus-run-session`

| Construct | Line | brush support | Verdict |
|---|---|---|---|
| `set -u` | 61 | yes | OK |
| `${VAR:-default}` param expansion | 63-72 | yes | OK |
| `$$` (pid) | 67 | yes | OK |
| functions `usage()`,`cleanup()` | 74,99 | yes | OK |
| `[ ... ]` / `test` (`-ge`,`=`,`!`,`-x`,`-s`,`-lt`) | many | yes (builtin) | OK |
| `shift` | 81 | yes | OK |
| `rm -f` | 93 | yes | OK |
| **numbered-fd redirect `3>"$FILE"`** | 96 | **yes** | **OK (see below)** |
| background `&` | 96 | yes | OK |
| **`$!` (last-bg pid)** | 97 | **BROKEN when read synchronously** | **ROOT CAUSE** |
| `trap ... EXIT INT TERM` | 104 | yes | OK |
| `kill` / `kill -0` | 100,111 | yes | OK (but fed empty pid) |
| `wait` | 101 | yes | OK |
| arithmetic `$((i+1))` | 115 | yes | OK |
| `export` | 124,126 | yes | OK |
| `"$@"` / `$?` / `exit` | 128-131 | yes | OK |

Suspects from the brief, each explicitly cleared:

- **(a) arbitrary-fd redirection `3>file`** — **SUPPORTED.** brush implements
  the full fd table: `interp.rs:1742-1753` opens the file and does
  `params.open_files.set_fd(fd_num, opened_file)` with
  `fd_num = specified_fd_num.unwrap_or(default)`. Empirically: foreground
  `sh -c 'echo HELLO >&3' 3>./r2` produces `r2` containing `HELLO`, and the
  child sees fd 3 open (`wrote-fd3-ok`). **Refuted as cause.**
- **(b) `--ready-fd` semantics / busd bad-fd** — busd DOES get fd 3 (the
  redirect works), so this is moot. busd is fine.
- **(c) parse/exec abort (`$(( ))`, `local`, `printf %q`)** — none of those
  are used; the script parses and runs. No abort. **Refuted.**
- **(d) LeandrOS gotchas** — `/dev/null` not used by this script; `test -s`
  on tmpfs proven working (S5); no `sleep` used (confirmed — bounded
  iteration count, not wall clock). **All clear.**

---

## The actual defect (brush source path)

brush runs a backgrounded external command as a **Tokio task on a worker
thread**, so the child's real PID is unknown at the moment `&` returns. The
uncommitted working-tree patch added an "async pid slot" to backfill it:

- `interp.rs:298-317` `spawn_async_ao_list_in_task`: creates
  `pid_slot = Arc<Mutex<Option<Pid>>>`, `tokio::spawn`s the job, attaches the
  slot to the `Job`.
- `interp.rs:541-550`: when the task actually spawns the external process it
  publishes `child.pid()` into that slot.
- `expansion.rs:1912-1918`: `$!` → `LastBackgroundProcessId` →
  `jobs().current_job().representative_pid()` → reads the slot
  (`jobs.rs:464` `representative_pid()`, patched to fall back to `pid_slot`).

The gap: the foreground shell executes the **next statement**
(`BUSD_PID=$!`) **before yielding** to the spawned task, so the slot is still
`None` and `$!` expands to `""` (`expansion.rs:1918`
`Expansion::from(String::new())`).

The slot DOES fill shortly after — on the multi-thread runtime
(`brush-shell/src/entry.rs:177` `new_multi_thread` on unix) the worker thread
fills it concurrently — but the script has already captured the empty value.

---

## Evidence (host brush, `sh` shim = brush to emulate LeandrOS)

```
# $! is empty for ANY backgrounded external cmd, redirect or not:
sleep 3 &            ; echo $!   ->  []        (empty)
sleep 3 3>./x &      ; echo $!   ->  []        (empty)
sh -c "sleep 3" &    ; echo $!   ->  []        (empty)
# reference /bin/sh:  ->  [39687]  (populated)

# It is a RACE, not a hard-unimpl: forcing the executor to poll fixes it:
sleep 3 &  /bin/echo warmup >/dev/null ; echo $!   ->  [40191]  (populated)
sleep 3 &  jobs >/dev/null ; echo $!               ->  []       (builtin: no yield)

# End-to-end reproduction of the real script logic (stub busd writes fd3):
brush :   DEBUG BUSD_PID=[]  -> "busd exited before signaling readiness" -> exit 1
/bin/sh:  DEBUG BUSD_PID=[40744] -> "CHILD WOULD RUN NOW" -> exit 0
```

Confidence: **very high.** Deterministic 1:1 reproduction on the same brush
binary that ships on-target; clean source-level explanation; the exit path
and message match the M5 report.

---

## Fix

Do not depend on a synchronous `$!`. **Primary fix (validated, shipped as
`~/code/leandros-artifacts/m5-session-ship/dbus-run-session.proposed`):**
launch busd through a tiny wrapper that records busd's REAL pid into a
pidfile before `exec`, and read it with the `read` builtin:

```sh
BUSD_BIN="$BUSD_BIN" SESSION_CONF="$SESSION_CONF" \
BUS_ADDR="unix:path=$BUS_SOCKET" PID_FILE="$PID_FILE" \
    sh -c 'echo $$ > "$PID_FILE"; exec "$BUSD_BIN" --config "$SESSION_CONF" \
           --address "$BUS_ADDR" --ready-fd 3' \
    3>"$READY_FILE" &
BUSD_PID=""
...
while ...; do
    [ -s "$READY_FILE" ] && break
    if [ -z "$BUSD_PID" ] && [ -s "$PID_FILE" ]; then
        read BUSD_PID < "$PID_FILE" || BUSD_PID=""      # builtin, no yield needed
    fi
    if [ -n "$BUSD_PID" ] && ! kill -0 "$BUSD_PID" 2>/dev/null; then
        echo "...busd exited before signaling readiness" >&2; exit 1
    fi
    i=$((i+1))
done
```

`$$` inside the wrapper is the sh pid; `exec` preserves the pid, so the value
is busd's actual pid. fd 3 is inherited across `exec`, so busd's `--ready-fd
3` still lands in `READY_FILE`. This is **deterministic regardless of Tokio
scheduling** and keeps the original's fast-fail-on-busd-death behavior.

Why script-side, not a brush patch: the brush behavior (async `$!`) is
arguably WAI for a Tokio-backed shell and patching job-control timing is
high-risk for a bus-launcher dependency. The script fix is contained,
verified, and portable (also runs under busybox ash / real /bin/sh).

### Host validation of the exact proposed file (under brush, `sh`=brush shim)

```
FINAL A happy path : child sees addr + real DBUS_SESSION_BUS_PID, exit 42 propagates  ✓
FINAL B busd dies   : "busd exited before signaling readiness", exit 1 (fast)          ✓
FINAL C no args     : usage, exit 64                                                    ✓
cleanup             : pidfile + ready file + socket removed on EXIT trap               ✓
```

### One dependency M6 MUST confirm

The wrapper calls `sh -c`. The script already requires `/bin/sh` (its own
shebang), but `sh` as a bareword needs PATH resolution. Confirm `sh` resolves
on-target inside the cosmic session's PATH (it will if PATH contains the dir
holding brush-as-/bin/sh). If it does not, use the **fallback** below, which
needs no extra binary.

### Fallback fix (no `sh` wrapper) — in-loop `$!` poll

Also validated on host. Keeps fast-fail; removes the `sh` dependency. Relies
on the worker thread filling the slot while the loop's per-iteration `stat`
(`test -s`) / `kill` syscalls yield to the kernel scheduler (populated at
iter ~9, before readiness at ~60, across 3 runs):

```sh
"$BUSD_BIN" ... --ready-fd 3 3>"$READY_FILE" &
BUSD_PID=""
while ...; do
    [ -s "$READY_FILE" ] && break
    [ -z "$BUSD_PID" ] && BUSD_PID=$!     # becomes non-empty once busd spawns
    if [ -n "$BUSD_PID" ] && ! kill -0 "$BUSD_PID" 2>/dev/null; then
        echo "...busd exited before signaling readiness" >&2; exit 1
    fi
    i=$((i+1))
done
```

Least-preferred variant: capture `$!` **after** the readiness loop only
(minimal diff) — but then a busd crash before readiness is not fast-failed;
it spins to `MAX_POLL_ITERS`. Not recommended given the 20M default.

---

## What the M6 on-target wave must validate

1. `sh` resolves on PATH inside the cosmic session (else switch to fallback).
2. Run the proposed `dbus-run-session -- <cmd>` on-target, both arches:
   busd binds ("Listening on UNIX socket"), `DBUS_SESSION_BUS_ADDRESS` is
   exported, the child runs, and the child's exit status propagates.
3. Confirm `read BUSD_PID < "$PID_FILE"` yields busd's real pid (kill on
   EXIT actually reaps busd; no orphaned busd after the session ends).
4. Negative path: a deliberately-broken busd config → exit 1 with the
   "busd exited" message, not a 20M-iteration hang.
5. Only after that: wire into start-cosmic (the original M5 caller) and
   confirm cosmic-session comes up.
```
