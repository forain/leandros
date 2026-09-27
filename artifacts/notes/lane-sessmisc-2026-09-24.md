# Lane sessmisc — 2026-09-24

Machine: linux laptop 172.16.149.179 (x86_64/KVM, also x86_64/TCG for the WDOG repro);
worktree `~/Projects/leandros-sessmisc`, branch `lane/sessmisc` (base `fc5a6bc`).
aarch64 check on the Mac (HVF), worktree `~/code/leandros-sessmisc`.

## Step 0 — laptop housekeeping
leandros-{drmsmoke,jobctl,misc,perms,tsccal}: all clean, all HEADs contained in origin/main →
removed; `git worktree prune`, `git fetch --prune`. Main checkout (`p1/signals`) untouched.
Local branches of those lanes left in place (not deleted).

## 1. Super+T in a serial-started session — FIXED (image staging, not kernel/input)
Root cause: `mkfs-f2fs-populated.py` sourced `/usr/share/cosmic` from a hardcoded
`~/code/cosmic-epoch`, which exists only on the Mac (desktop has `~/Projects/cosmic-epoch`,
laptop had none). Every lookup is `isdir`/`isfile`-gated, so Linux-built images shipped **no
/usr/share/cosmic at all** → empty keybinding table (`Shortcuts::default()`), `NoConfigDirectory`
in the shortcuts log. Input was fine: `[EVCLI]` showed the session comp's queue receiving the
chord (deliv 0→6). The misc lane's "one try" ran on the laptop, hence the symptom.
Fix `9a2526e`: resolve `$LEANDROS_COSMIC_EPOCH` → `../cosmic-epoch` beside the checkout → beside
the main checkout of a worktree → `~/code/cosmic-epoch`; **SystemExit** if the Shortcuts
`defaults`/`system_actions` keys were not staged. Build prints `COSMIC system defaults: 263 file(s)`.
Laptop: synced a data-only subset (263 files) of the Mac's cosmic-epoch (epoch-1.3.0) to
`~/Projects/cosmic-epoch` (README.subset there). Desktop already has `~/Projects/cosmic-epoch`
(found via the main-checkout sibling rule).
Evidence x86_64/KVM: Super+T → `/bin/cosmic-term` process within ≤14 s, window + brush prompt
on screen (2 boots).
Caveat (by design, not a bug): with the greeter running (default boot), a second compositor
started from serial never gets DRM master — it cannot present anything. Test serial sessions
with `touch /etc/leandros/text-login; kill $(cat /run/greetd-init.pid)`.

## 2. `[WDOG] … cosmic-comp mmap ~2 s` — FIXED (kernel)
Characterized with a new `[MMAP-SLOW]` report: MAP_PRIVATE file mmaps are copied eagerly
**inside the syscall, with IRQs masked**. The big ones are each Rust binary mapping its own
executable for backtrace symbolization (RUST_BACKTRACE=1 + anyhow errors): cosmic-comp 33 MiB,
cosmic-greeter-login 36 MiB; plus ld-musl mapping libgallium (23 MiB + 12 + 9 MiB).
KVM: 0.75–1.5 s each. x86_64/TCG: 2.3 s (libgallium) and 3.7 s (exe) — both produced
`[WDOG] cpu2 took no timer tick for ~2 s … /bin/cosmic-comp last syscall 9` (2 lines by t=25 s).
Fix `a573652`: copy in 512 KiB chunks with `irq_window()` between them (nothing held across a
chunk). After: x86_64/TCG 0 WDOG over ~400 s (greeter startup, same maps still 1–7.8 s total),
KVM 0 WDOG. Total cost unchanged — the real cure is lazy file-backed private mappings (or
`RUST_LIB_BACKTRACE=0` in the session/greeter env to stop the self-exe maps: ~1.1–1.5 s saved per
COSMIC process start on KVM, several s on TCG) — not done here.
Also seen on TCG only: many tiny mmaps at 100–700 ms in `map_ms`/`fixup_ms` (anon 2-page
map_lazy 250 ms) → address-space/RUN_QUEUE lock contention, runqlock lane territory.
Matches polltimer's report (x86_64/TCG poll spikes up to 3.97 s during greeter startup).

## 3. Greeter keystroke lag, x86_64/KVM (clock fixed) — NOT refuted, still severe
Probe: QMP key 'a', poll screendump of the password field every 0.5 s until it changes.
- base fc5a6bc: 3 keys 26.2 / 36.0 / 41.7 s
- stats build: 5 keys p50 39.6 s (25.2–43.7); 15 page flips in 214 s (0.07 flips/s)
- final a573652: 9 keys 2.6, 6.2, 12.4, 15.0, 15.0, 29.8, 32.4, 33.5, 43.2 s (p50 15 s)
Keys are never lost (all dots arrive). While "idle" the greeter's cosmic-comp render thread is
Running in userspace in 65 % of Ctrl-T samples (mmap/munmap churn; 9.6k mmaps), and
cosmic-greeter-login is Running in a `stat()` loop in 40 %. Next: find what the greeter stats
in a loop, and count page faults on the comp's per-frame anon buffers.
⚠ A 12 ms screendump poll loop starves QEMU's main loop and inflates the lag — poll ≥0.5 s.

## aarch64 (Mac HVF, `~/code/leandros-sessmisc` @ a573652, full aarch64 build)
Build prints `COSMIC system defaults: 263 file(s) from /Users/forain/code/cosmic-epoch`.
Greeter boot: same slow maps (`[MMAP-SLOW]` comp exe 32 MiB 1.0 s, greeter 37 MiB 1.66 s),
0 `[WDOG]`. Serial-started session (text-login): desktop up, Super+T → cosmic-term window with a
brush prompt within 40 s. Greeter lag not measured on aarch64.

## Files
`scripts/mkfs-f2fs-populated.py`, `kernel/src/syscall.rs` (sys_mmap only). No other lane's files.
