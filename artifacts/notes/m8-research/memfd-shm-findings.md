# memfd + shared-mmap gaps — research wave (2026-07-30/31)

STATUS: IN PROGRESS — written incrementally. Sections marked SETTLED are final.

## SETTLED FACT 1 — the memfd + MAP_SHARED cross-process path is ALREADY coherent, and is already regression-tested

`userland/scmtest/src/main.rs:808` `test_shared_memfd_pixels()` executes **exactly**
the kernel path the applet uses:

- `raw_memfd_create()` (scmtest main.rs:190) → `sys_memfd_create`
  (kernel/src/syscall.rs:6847) → named `/tmp/memfd:<name>` tmpfs node +
  `vfs::mark_memfd` (servers/vfs/src/lib.rs:471), i.e. the TmpVmo is created at
  memfd_create time, before any ftruncate.
- `raw_ftruncate(mfd, 4096)` → `handle_ftruncate` (servers/vfs/src/lib.rs:4698),
  VMO branch at :4707 grows `vmo.pages`.
- `mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, mfd, 0)` → the K1 shared
  branch, kernel/src/syscall.rs:1659-1679 → `vfs::vmo_acquire_frames`
  (servers/vfs/src/lib.rs:537) → `AddressSpace::map_shared_frames`
  (mm/src/vmm.rs:399).
- fd handed to a **second process** over SCM_RIGHTS (`export_fd`
  servers/vfs/src/lib.rs:3398 / `import_fd` :3418 — the `VnodeKind` is copied
  verbatim, so the receiver's fd is `TmpFile { idx }` with the *same* idx).
- Child mmaps MAP_SHARED, verifies pattern A written by the parent BEFORE the
  child mapped, then **writes pattern B after both mappings exist**, and the
  parent re-reads pattern B through its **pre-existing** mapping
  (scmtest main.rs:865, :882-885).

That last step is precisely "a write performed after the peer has mapped is
visible to the peer". It passes (scmtest 25/25 both arches, notes/m7v-reg-*).

=> The Gap 2 working hypothesis ("the compositor's mapping resolves to DIFFERENT
physical frames once the inode is promoted to a TmpVmo; MAP_SHARED aliases only
for content written before the peer maps") is **FALSIFIED by an existing green
regression test that exercises the identical code path**.

Corollaries:
- There is no COW problem on this path: `map_shared_frames` sets `cow: false`
  and installs PTEs eagerly (mm/src/vmm.rs:444-463), so the write never even
  faults.
- There is no copy-at-mmap-time problem: the eager private-copy fallback
  (kernel/src/syscall.rs:1681-1760) is only reached when the MAP_SHARED+TmpFile
  branch above does not fire.
- There is no aarch64 cache-maintenance gap: both mappings are ordinary
  cacheable USER pages created by the same `map_page` with the same attributes;
  hardware keeps them coherent. The only cache maintenance in the fault path is
  for EXECUTE pages (mm/src/vmm.rs:692-702, :714-720) and is irrelevant here.
  If a dcache/PoU gap existed, scmtest's cross-process pattern-B check would
  fail on aarch64 — it does not.

(continued below)

## SETTLED FACT 2 — the applet's Wayland protocol usage is CORRECT (orchestrator, 2026-08-02)

Source: /Users/forain/code/leandros-artifacts/m7w-applet/src/main.rs

Per frame `draw()` (main.rs:176-205) does, in order:
- rewrite every pixel: `*self.pixels.add(i) = BG` for all WIDTH*HEIGHT, then
  `draw_text(self.pixels, &clock_text())` (:182-186)
- `pool.create_buffer(...)` — a FRESH wl_buffer every frame (:188-189)
- `surface.attach(Some(&buffer), 0, 0)` (:192)
- `surface.damage_buffer(0, 0, WIDTH, HEIGHT)` (:193)
- `surface.commit()` (:194)
- destroys the PREVIOUS buffer only after replacing it (:196-198)

=> attach/damage/commit are all present and correctly ordered, full-surface damage
is posted every frame, and buffer identity is fresh each frame. The "client forgot
to damage" hypothesis is FALSIFIED.

## SETTLED FACT 3 — the repaint IS driven, and the time source DOES advance

- Event loop (:354-421) polls with a TICK_MS=1000 timeout and repaints when the
  wall second changes: `if app.drawn && now != shown_sec { app.draw(&qh) }` (:416-420).
- `clock_text()` (:68-74) derives from `libc::time(NULL)`.
- `sys_time` is x86_64-ONLY (kernel/src/syscall.rs:2446, `#[cfg(not(target_arch =
  "aarch64"))]`) — BUT relibc's `time()` does NOT use it: it calls
  `Sys::clock_gettime(CLOCK_REALTIME, ...)` (relibc src/header/time/mod.rs:521-528).
- `sys_clock_gettime` (kernel/src/syscall.rs:2123-2134) ignores clkid and returns
  `ticks()/100` sec + nsec — it ADVANCES.

=> time() advances on aarch64, so the repaint gate fires once a second and draw()
re-runs. "Frozen because the timer never fires" is FALSIFIED.

## SETTLED FACT 4 — a read-only compositor mapping still takes the shared path

The K1 shared branch (kernel/src/syscall.rs:1659-1679) is gated ONLY on
`flags & MAP_SHARED != 0` && `kind == VnodeKind::TmpFile` — there is NO PROT_WRITE
requirement. So smithay mapping the pool read-only still aliases the same VMO
frames via `vmo_acquire_frames` -> `map_shared_frames`; it does NOT fall through to
the eager private-copy path at :1681+.

=> "compositor's PROT_READ mapping silently got a private copy" is FALSIFIED.

## WHERE THAT LEAVES GAP 2

Kernel coherence: proven working (FACT 1). Client protocol: proven correct (FACT 2).
Repaint actually happens: proven (FACT 3). Compositor mapping takes the aliasing
path: proven (FACT 4).

So the remaining candidates are all COMPOSITOR-SIDE or in the pool/fd hand-off:
1. smithay/cosmic-comp caching the imported SHM texture across frames despite a
   fresh wl_buffer + full damage (check smithay's shm import + renderer texture
   cache keying in ~/.cargo/git/checkouts/smithay-*/).
2. The compositor mapping the pool at a size/offset that does not track the
   client's, or mapping ONCE at pool-create and never seeing later stores because
   its mapping was established through a DIFFERENT path than scmtest's (e.g. the
   fd arrived via the Wayland socket's SCM_RIGHTS import — verify import_fd
   preserves TmpFile{idx} for the COMPOSITOR's fd table specifically).
3. Something in wl_shm_pool.create_buffer offset handling.

NEXT STEP: this now needs ON-TARGET EVIDENCE, not more code reading. Instrument
the kernel to log which mmap branch cosmic-comp takes for the applet's pool fd
(shared-alias vs private-copy) and the frame addresses on both sides. Three
plausible hypotheses have each been falsified by reading; stop reading and measure.
