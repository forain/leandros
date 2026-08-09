# Item 9b: the tiling applet never ran, and minimize was never broken (2026-08-09)

TODO item 9b recorded *"tiling and minimize draw zero pixels — both start, stay up
(no exits, no restarts) and take their privileged socket, and neither paints."*

**The premise was wrong twice over.** Tiling never reached `execve` at all, and
minimize's blankness is upstream-correct behaviour. Two independent LeandrOS defects
were hiding behind one sentence.

## The discriminator that broke it open

Both applets print an unconditional `tracing::info!("Starting … applet with version")`
as the first statement of `main`. In the pre-fix capture:

| | `Starting:` attempts | own stdout lines |
|---|---|---|
| `cosmic-applet-tiling` | 1 | **0** |
| `cosmic-applet-minimize` | 1 | 66 |
| `cosmic-panel-button` | 5 | 129 |

Tiling printed nothing *and* logged no exit. `ProcessManager::start()` had returned
`Err`, swallowed by cosmic-panel's `if let Ok(key)` (`cosmic-panel-bin/src/main.rs:225`).
A spawn that fails inside `pre_exec` — after `fork`, before `execve` — produces no
stdout, no exit line, and no error. It is indistinguishable from a live, blank client.

## Defect 1 — `handle_fork_dup` dropped `UnixPendingAccept` across fork

`servers/net/src/lib.rs`. The fd-inheritance copy enumerated only `UnixConnected` and
`Unbound`, with `_ => {}` silently dropping everything else.

The applet's privileged socket is an unaccepted `connect()`. cosmic-panel binds an
abstract listener, marshals it to cosmic-comp over
`wp_security_context_v1.create_listener`, and connects to it *itself* — all
synchronously, before the Wayland request is even flushed
(`xdg_shell_wrapper/client/handlers/wp_security_context.rs:53-72`; note the
`connect_addr` at :64 happens **after** the marshal). launch-pad then forks with that
connector still in `UnixPendingAccept`. With the state dropped, the child's `pre_exec`
`fcntl(F_GETFD)` (launch-pad `src/util.rs:6`) returned EBADF, `pre_exec` failed, and
the child `_exit`ed before `execve`.

**Why tiling died and minimize lived.** Tiling is spawn #1, forked while the panel is
still inside the same calloop callback — before `create_listener` is flushed, so
cosmic-comp cannot have accepted. Minimize is spawn #5, forked ~200 ms later after four
expensive TCG fork+execs, by which time the fd was already `UnixConnected`, which the
existing arm did copy. Same code, same flag, opposite outcomes, decided purely by spawn
order. That asymmetry is the whole diagnosis.

Fixed by copying `UnixPendingAccept` (it is end A of a real `UnixConn` and refcounts
through `refs_a`), plus refcounting it in `handle_close` and `handle_close_all`, both of
which force-freed the connection. Those two were masked — bug 1 guaranteed a single
holder — and would have become live the instant fork inheritance worked, since the panel
drops its fd copy right after spawn.

Multi-holder is **routine**, not hypothetical. From an x86_64 session:

    [NET] fork 25->60 pending fd=263 conn=14 COPIED
    [NET] fork 25->61 pending fd=263 conn=14 COPIED

The same connector inherited by two children — `refs_a == 3` on one unaccepted
connection.

## Defect 2 — `MAX_OPEN_FILES = 32` is a global pool, and it starved the icon load

With tiling finally running, it reached `SCTK setup complete`, sized a 40x32 surface,
and then failed:

    cosmic_freedesktop_icons::theme: unable to read icon theme directory
        why=Os { code: 24, ... } dir="/usr/share/icons"

`servers/f2fs/src/lib.rs:434`'s `MAX_OPEN_FILES` is **per-mount and system-wide**, not
per-process. Everything lives on the one f2fs root mount, and `handle_open` allocates a
slot for directories too, so a `read_dir` walk holds slots for the whole iteration.

Proof it is global rather than per-process: **10 distinct processes hit errno 24 inside
a 3-second window** at session startup (panel, three panel buttons, tiling, minimize,
app-library, launcher, osd, notifications) and then it stopped permanently. A
per-process limit cannot produce a synchronised burst across ten unrelated processes
that then clears.

**Why it was permanent for tiling specifically.** `freedesktop-icons` holds themes in
`pub static THEMES: LazyLock<…>` (`src/theme/mod.rs:16`) and `continue`s past a
`read_dir` error (`:171-176`). A `LazyLock` initialises once and never retries, so a
single transient EMFILE at the instant tiling's `THEMES` initialised left that process
with an empty theme map for its entire lifetime. `cosmic-app-library` hit errno 24 too —
it simply wasn't holding the lazy init at that moment. Same crate, same tree, different
microsecond.

This retires item 9b's *"icon not staged — lookup in that directory is proven by a
rendering sibling"* elimination. The sibling really does render; the inference was still
wrong, because it treated a **race** as a **property**.

Fixed: `MAX_OPEN_FILES` 32 -> 256 (~386 KB, and note the array lands in `.data`, not
`.bss`, since it sits behind a `Mutex`), plus a real slot leak closed at
`servers/vfs/src/lib.rs:3253-3262` (a successful mount-open whose fd install fails
orphaned the f2fs slot forever — a death spiral, since a process at its own fd limit
permanently burned global slots), plus a permanent one-shot `report_open_files_full()`
so this exhaustion class names itself instead of surfacing as an opaque errno.

## Minimize was never broken

`cosmic-applet-minimize/src/lib.rs:145-149` calls `iced::window::minimize(main_window,
true)` unconditionally in `init()`, un-hiding on the first toplevel (`:212`) and
re-hiding when the list empties (`:220-226`). With zero toplevels, `view()` yields an
empty `Shrink` row inside `autosize().limits(min_width(1.).min_height(1.))` — a 1x1
transparent surface. The `72x64` in the log is the winit `WindowAttributes` struct at
window-creation time, and it carries `visible: false`.

Two negative controls: the only `panicked` line in the log is the pre-existing
`cosmic_session::comp::parse_and_handle_ipc` at `comp.rs:37`, and `Wayland handler
thread died` (printed unconditionally at `wayland_subscription.rs:36` when the handler
returns) appears **zero** times — so the handler thread is still inside its calloop.

Caveat for whoever revisits: an empty toplevel list and a wedged handler look identical
here. The only test that separates them is opening a real toplevel and checking that
minimize un-hides.

## Result

| | before | after |
|---|---|---|
| tiling reaches `execve` | no — 0 stdout lines | yes — 74 lines, both arches |
| `code: 24` in a session | 76 occurrences, 10 processes | 0 |
| tiling icon, panel right wing | 0 bright px | 176 bright px, both arches |

The icon renders the `…Tiling.Off` glyph (two overlapping windows), matching
`window.rs:245-251`'s unconditional `icon_button(if self.autotiled { ON } else { OFF })`.

`[NET] close-pending pid=38 fd=305 conn=70 refs=1` — the `handle_close` refcount path
firing in a live session, decrementing rather than destroying. It did not fire in the
two earlier sessions; in a normal session the panel's connector is already accepted by
the time launch-pad drops its fd, so the close lands in the `UnixConnected` arm. The
pending arm is the failure-path branch, which is why `scmtest`'s
`fork_inherits_pending_connector` constructs it deliberately.

## Instrument failures in this lane, for the ledger

1. **Session output redirected to a file inside the guest.** Serial is the capture
   channel; the log ended 155 lines in, at the shell prompt.
2. **A bare `sleep` during the drain.** QEMU's Unix serial chardev serves one client and
   **discards** guest output when nobody is attached (ledger entry 4 — hit by writing
   the exact pattern it warns about). `driver.py session` holds the socket open and
   pumps; that is what it is for.
3. **Both failed silently and in the same direction.** Every counter read 0 — including
   `errno 24 = 0` and `EBADF = 0`, which look like a pass. Absence-based pass criteria
   are trivially satisfied by an instrument measuring nothing. Pair them with a
   **presence** counter from the same capture (`Starting:` spawns, minimize lines, log
   size); those caught both failures.
4. **A 240 s per-command timeout on TCG x86_64** produced `scmtest 0/0`, `sigtest 0/0`,
   `drmsmoke 0/0` and a truncated `vfstest 16/3`. Zeros with zero failures are
   truncation, not reds.
5. The v2 screenshot survived failure 2 because it travels over the **QMP monitor**, a
   separate channel. Independent capture paths are worth having; a single one is a
   silent single point of failure.
