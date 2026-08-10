# Item 14 piece 3 — the libseat shim measured under a real VT switch

Measured 2026-08-10 on the Linux box, x86_64/KVM, tree at `6146a15` (the first build
that contains the DRM master work `f6ebb8b`/`959710d`). Two full COSMIC sessions, each
driven `Ctrl+Alt+F2` -> wait -> `Ctrl+Alt+F1` via `driver.py chord`.

**No instrumentation was written.** The shim already carries `LEANDROS_INPUT_TRACE` +
`LEANDROS_INPUT_TRACE_DIR`; enabling them was sufficient, so nothing had to be edited
and nothing had to be reverted. The `SEAT_TRACE_BUDGET` of 200 lines that the source
warns about is not a hazard in practice: smithay calls `libseat_dispatch()` only when
`get_fd()` is readable, not once per event-loop iteration. A whole session plus two
switches produced **13 lines**.

## Headline: the shim is a CORRECT producer, and it is DISABLED in production

Both statements are measured, and they are about different things.

### Production configuration: zero events, ever

Launched exactly as the desktop is launched (`sh /bin/start-cosmic-leandros`):

    [SEATSHIM] pid=19 owned_vt_from_ctty: ttyname_r failed rc=25
    [SEATSHIM] pid=19 owned_vt: unknown (no XDG_VTNR, no VT controlling terminal)
    [SEATSHIM] pid=19 vt_probe: VT ownership unknown -- no VT support, always-active fallback
    [SEATSHIM] pid=19 open_seat ... fd=22 vt_fd=-1 own_vtnr=-1 active=1
    [SEATSHIM] pid=19 get_fd seat=... -> 22 (vt_fd=-1 conn_fd=22)

`get_fd()` hands back the inert eventfd, so nothing can ever wake `dispatch()`. After
a complete F2 -> F1 round trip the trace file was **byte-identical** — no `dispatch`,
no `disable_seat`, no `enable_seat`. `98b4a52` fixed the dead fd, but the shim never
reaches the code path that would use the live one.

**The break is `owned_vt()`, and it is one step earlier than expected.** The prediction
was that `ttyname_r` would succeed, return `/dev/console`, and fail the
`strncmp(buf, "/dev/tty", 8)` prefix test. It does not get that far: `ttyname_r`
returns **rc=25 (ENOTTY)**. musl's `ttyname_r` calls `isatty()` first, and `isatty()`
fails on the fd that `open("/dev/tty")` yields. Confirmed live from the guest shell:
`readlink /proc/self/fd/0` -> `/dev/console`.

Why there is nothing to find: `init` does `setsid(); ioctl(0, TIOCSCTTY, 0)` on the
kernel console fast path, which has no fd-table entry and records no per-session ctty
(`servers/tty/src/lib.rs:206` only sets `CONSOLE_FG_PGID`). No process in the chain
ever opens `/dev/ttyN`, and nothing sets `XDG_VTNR`. VT ownership is genuinely
unknowable, and the shim's conservative fallback is doing exactly what it was written
to do.

**This matters for the fix:** repairing only the *name* would not be enough —
`isatty()` on the `/dev/tty` proxy must succeed too. The cheap fix is the other
branch: export `XDG_VTNR` in the session launch path.

### With `XDG_VTNR=1`: correct events, right order, right time

Same image, same session, one env var added. Full trace across the round trip:

    owned_vt: 1 (from XDG_VTNR)
    vt_probe -> vt_fd=24 own_vtnr=1 v_active=1 active=1 (kernel VT support detected)
    open_seat ... fd=22 vt_fd=24 own_vtnr=1 active=1
    get_fd  -> 24 (vt_fd=24 conn_fd=22)
    open_device /dev/input/event0 -> 27 ; /dev/input/event1 -> 28 ; /dev/dri/card0 -> 29
    -- Ctrl+Alt+F2 --
    dispatch -> disable_seat (VT 1 lost foreground to 2)
    disable_seat
    close_device id=27 rc=0
    close_device id=28 rc=0
    -- Ctrl+Alt+F1 --
    dispatch -> enable_seat (VT 1 became foreground)
    open_device /dev/input/event0 -> 28 ; /dev/input/event1 -> 35

`disable_seat` then `enable_seat`, **exactly once each, in that order**, each on the
correct edge. `/dev/tty0` really does become poll-readable on a switch, and smithay
really does register it and call `dispatch()`.

**smithay's `is_active()` demonstrably observed both.** The proof is not that the
callback returned — it is what cosmic-comp did next: it **closed both evdev fds on
deactivate and reopened them on activate**. That is the libseat consumer contract
being honoured, driven by our event.

It did **not** close `card0` (id=29). The DRM fd is kept across the switch, which is
why no `SET_MASTER` is needed on the way back — the auto-rearm in `959710d` is what
makes that safe.

## Refinement to item 14's "the input half is not done"

Still true of the *kernel*: `/dev/input/*` is not gated or revoked on switch. But the
practical symptom the item describes — "a backgrounded client still sees the keyboard
and mouse" — does not survive contact with a working seat event: a libseat consumer
releases the devices voluntarily. The kernel-side gate is defence against clients that
do not, not the only route to the behaviour.

## Did cosmic-comp get EACCES, and did it recover?

**Yes, and it recovered — but the evidence is behavioural, not a logged errno.**

In the production run the shim never said "deactivate", so `is_active()` stayed true
and cosmic-comp kept presenting into a VT it no longer owned. The screen went **black
on VT2 and stayed black for the full 20 s** (`item14-e1-vt2-black.png` — a uniform
frame; the 351-byte PNG is itself the measurement). The panel clock ticks once a
second, so there was continuous damage and therefore continuous presents throughout.
Before `f6ebb8b` that same situation returned the client's frame within one flip; here
it never returned until `Ctrl+Alt+F1`. The only refusal the gate emits is
`DriverError::Access` -> **-13/EACCES** (`servers/drm/src/lib.rs:242`), so cosmic-comp
was refused with EACCES repeatedly for ~20 s.

It did not tear the device down. The desktop returned intact on F1 with its clock
advanced 00:01:58 -> 00:03:25 (87 s), no re-initialisation, no `SET_MASTER`
(`item14-e1-desktop-back.png`). **So smithay treats EACCES as recoverable**, which is
what item 14 recorded as asserted-not-read. This is a behavioural confirmation of the
consequence, NOT a reading of smithay's error mapping — the distinction the item
itself insists on. smithay remains unread.

**What is missing is a count.** The kernel logs nothing on refusal, and cosmic-comp's
own log could not supply it: `cosmic-session` panicked early (`parse_and_handle_ipc`
-> `unwrap_failed`, backtrace in the session log) and it is the process that relays
cosmic-comp's stderr, so the log stops at 00:01:24 — before the switch. Putting the
errno itself on the record needs either a rate-limited print in `master_gate`'s
`Err(_)` arm (`drivers/src/drm_device_interface.rs:2046` — note the `drivers` crate
has no print facility at all today, which is why this was not just done) or a session
run in which cosmic-session survives.

The `XDG_VTNR=1` run is the control: with `disable_seat` delivered, cosmic-comp stops
presenting and the same window produces no fight over the scanout at all.

## QMP chord injection (item 18) worked on live QEMU, first try

`driver.py chord ctrl alt f2` / `f1` — four injections across two sessions, four
switches, no misses and no stuck modifiers (the desktop kept taking input after every
return). `fe89e8c` is exercised; item 18 piece 1 can drop "syntax-checked only, never
run against a live QEMU".

## Reproducing

    ./scripts/build-all.sh --arch x86_64
    driver.py start x86_64 ; driver.py login root root
    driver.py cmd "export XDG_VTNR=1; export LEANDROS_INPUT_TRACE=1; \
                   export LEANDROS_INPUT_TRACE_DIR=/tmp; \
                   sh /bin/start-cosmic-leandros > /tmp/cosmic.log 2>&1 &"
    driver.py chord ctrl alt f2   # ... wait ...   driver.py chord ctrl alt f1
    driver.py cmd "cat /tmp/seatshim.*.log"

Drop the `XDG_VTNR=1` to reproduce the production no-op. There is no `grep` in the
guest; use `tail`/`cat`.

## Landmine hit on the way

A second QEMU already held the image write lock (a human-driven `-serial mon:stdio`
run on the box). The session was run from a *shadow root* — copies of the three images
plus `.claude/skills/run-leandros/` — because `driver.py` derives `REPO_ROOT` from its
own location. Cheap trick, and it does not disturb whoever owns the real tree.
