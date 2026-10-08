# Framebuffer console

The kernel's framebuffer console is the machine's primary console: a real VT emulator drawn into the same surface the display scans out, with serial as a secondary, approximate mirror. It draws into a write-back shadow and only ever writes the framebuffer, and it yields the scanout to whichever DRM client is presenting.

## Overview

- Code: [`drivers/src/framebuffer.rs`](../../drivers/src/framebuffer.rs) (surface, VT emulator, shadow, scanout arbitration), [`drivers/src/vector_font.rs`](../../drivers/src/vector_font.rs) (glyph data), the console write path in `kernel/src/main.rs`, and the VT layer in [`servers/tty/src/vt.rs`](../../servers/tty/src/vt.rs).
- `BOOT_FB`/`KERNEL_FB` point at the RAM buffer backing virtio-gpu resource 1; every console byte is drawn there and flushed to the host as a dirty rectangle.
- Six virtual terminals each keep a text plane; the console draws the plane of the VT on screen. VT semantics, PTYs and switching are described in [TTY and VT](../servers/tty-and-vt.md).

## How it works

### Glyphs

The console renders an embedded **8×16 bitmap font** derived from Fira Code (`get_fira_code_char`), with a smaller fallback bitmap for anything outside it. Each cell is cleared before its glyph is drawn, because a line editor repaints the same cells on every keystroke and glyph drawing only lights set bits. At 1920×1080 that gives a 240×67 cell grid.

`vector_font.rs` also contains a TrueType/OpenType outline parser, a rasteriser and a 256-entry glyph cache, but `init_vector_font` currently disables it ("debug boot crash") and the image carries no real TrueType font — only a synthetic header. Earlier descriptions of the console as rendering sub-pixel-positioned Fira Code outlines describe that dormant path, not the shipped one.

### A real VT emulator

The console is a terminal, not a print sink:

- It answers cursor-position reports (`ESC[6n`) and device-status/attribute queries from its own cursor, so a full-screen line editor (brush via reedline/crossterm) works on it.
- `TIOCGWINSZ` reports the true cell grid through the `kernel_console_winsize` callback (the tty server deliberately does not depend on the drivers crate) instead of a hardcoded 80×24.
- Multi-byte ANSI escape sequences are delivered to a reader in one `read` rather than split.
- **The framebuffer is the authoritative console** and wins geometry ties against the serial port. Two terminals of different geometry cannot both be correct off one byte stream, so the cursor-position query is intercepted in the kernel's console-write path and answered from the framebuffer; it never reaches the UART, where an attached host terminal of a different size would otherwise answer for it. When the two sinks disagree, the framebuffer is made correct and serial is accepted as approximate.
- Synthetic console input (such as a CPR reply) is visible to `poll`/`epoll` readiness, not just to `read`. Crossterm blocks in epoll for the reply; an invisible reply left it waiting until reedline's 2 s timeout dropped brush out of interactive mode, and the stale reply then leaked to the next reader (the shell tried to execute `bin/[13;1Recho`).

### Atomic writes

Console writes are atomic (`b3659fa`). They were not always: a kernel trace interleaved mid-sequence with the shell's `ESC[38;5;` prompt colour once spliced into a literal `ESC[3i` — ANSI *media copy* — which opened a print dialog on the host terminal. The CSI final-byte alphabet is now restricted to what the console implements, and kernel log lines print whole through a per-CPU outbox, so they no longer interleave into a `Password:` prompt either.

### Never read the surface you draw on

Scrolling used to `copy` the framebuffer onto itself. The framebuffer must be uncached for the scanout to see it, and uncached *loads* cannot burst — no line fill, no prefetch, one transaction per access — so every newline pulled 8.29 MB back out of an uncached mapping. `ls -l /bin` took about 40 s on x86_64/KVM at 1920×1080. Two changes fixed it:

1. **Scroll several rows at once** (`c0b13ef`). A scroll costs the same whether it advances one text row or eight, so `SCROLL_ROWS = 8`: 1.579 s per line → 0.190 s, 300 lines in 62.7 s. The console jumps eight rows and can leave up to eight blank rows below the cursor — fbcon's trade when it cannot pan.
2. **A write-back shadow** (`27cf782`). All drawing goes to a cached shadow allocated from the buddy allocator, and `fb_flush` blits only the dirty rectangle out. The framebuffer is read exactly once, when the shadow is seeded, and thereafter only written, sequentially. `ls -l /bin`: 40 s → 0.70 s, 57×, with the scroll granularity unchanged. If the shadow allocation fails, drawing falls back to the direct path.

The measurements that led there are instructive: scaling `SCROLL_ROWS` from 8 to 32 cut 40 s to ~6.9 s (cost tracked scroll count almost exactly), while making the mapping write-combining alone moved it only to 38.7 s, because that accelerated only the copy's write half. A scroll still dirties the whole screen and costs one full-surface blit and transfer, which is why multi-row scrolling stays.

Two supporting corrections came out of the same work:

- Both architectures had programmed a burst-capable uncached memory type and then used it only for the userspace `/dev/fb` mmap — aarch64 mapped the console Device-nGnRnE, which may not merge even two adjacent pixel stores. Both now map the console **write-combining** (Normal non-cacheable on aarch64).
- `IA32_PAT` is per logical processor, and an AP leaves INIT/SIPI holding the reset PAT, so the console was write-combining on the BSP and write-through on every AP — one set of physical lines under two memory types. The BSP now publishes its PAT and each AP adopts it verbatim as the first statement of `sched_ap_entry` (`5ce8af2`). See [x86_64](../kernel/arch-x86_64.md).

### Sharing the scanout with DRM clients

The console and the scanout are the same surface, and they arbitrate for it. A program that merely *printed* during a live desktop session used to destroy the display: one `\n` past the last text row scrolls the whole surface up, and a compositor repaints only what it damaged, so everything static scrolled off and was never redrawn — 334,503 distinct colours collapsed to 177 and 79% of the screen went black.

The rule now (`edad115`):

- **A DRM present claims the scanout** for the card0 open that made it (`drm_scanout_claim`), and the console stops drawing until that open gives it back. The predicate is "has a DRM client composited into this surface and not yet released it", claimed from the present path itself. It replaced a hardcoded list of ioctl numbers that never included `DRM_IOCTL_MODE_ATOMIC` — the one path a real compositor drives — so an atomic compositor never turned the console off at all.
- **Ownership is per open**, not a global flag. Any card0 close used to hand the console back, and the reclaim cleared the screen, so a short-lived second open of the node wiped a live compositor's display.
- **Reclaim repaints** the saved text plane of the VT now on screen rather than clearing the surface and printing a "[Console Resumed]" banner.
- **A VT switch** revokes the scanout claim (`fb_vt_scanout_revoke`) so a text VT can take the display back; the DRM master rules then stop the background client from re-claiming it (see [drm-kms.md](drm-kms.md#drm-master-one-per-vt)).
- **A kernel panic** takes the scanout back unconditionally with two atomic stores and no locks — the panicking thread may already hold the framebuffer lock — and brings VT 1 forward, so a panic during a live session is visible.

### `/dev/fb0`

Writes to `/dev/fb0` are copied into the console shadow and transferred to the host, but only while the console surface is on screen (a text VT with no DRM scanout owner); under a compositor they do nothing, as on Linux (`5cf984f`). Before this fbdev pixels never reached the host and the next console blit overwrote them.

## Design decisions

| Decision | Why |
|---|---|
| Framebuffer is the authoritative console; serial is approximate | One byte stream cannot satisfy two geometries; the screen is what the user sees |
| Write-back shadow plus write-combining mapping | Uncached reads were the entire cost; WC only helps writes |
| Scanout claimed by presents, owned per open | Covers every present path, including ones added later, without listing ioctls |
| Panic reclaim is lock-free | A panic handler that blocks prints nothing |

## Limitations and open work

- The vector-font path is dormant; only the bitmap font ships.
- Kernel log lines go to VT 1's plane only (as with `console=tty1`).
- On return to VT 1 from a compositor, the console text flashes until the compositor's first present, because the libseat shim never sets `KD_GRAPHICS` on that VT.
- A scroll is still a full-surface blit and transfer.

## Code and tests

- [`drivers/src/framebuffer.rs`](../../drivers/src/framebuffer.rs), [`drivers/src/vector_font.rs`](../../drivers/src/vector_font.rs), [`servers/tty/src/vt.rs`](../../servers/tty/src/vt.rs).
- [`userland/drmsmoke`](../../userland/drmsmoke/src/main.rs) guards the console/scanout handoff an atomic commit must cause; `--hold` leaves a checkable frame on the scanout.
- [`userland/vttest`](../../userland/vttest) — VT planes, switching, CPR/DSR answers.

## See also

- [TTY and VT](../servers/tty-and-vt.md) — line discipline, VTs, PTYs.
- [drm-kms.md](drm-kms.md) — DRM master and the present paths that claim the scanout.
- [Debugging](../development/debugging.md) — serial capture and console pitfalls.
- [Input](../input.md) — how the UART and keyboard feed the console.
