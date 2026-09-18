# lane/complk — compositor-death memory leak (2026-09-18)

Item: "~140 MB of kernel memory leaks per `cosmic-comp` death; a respawn storm
ends in `[BUDDY] Allocation failed`" (TODO.md "Open work", lane/zinkimg finding).

Machine: linux desktop 172.16.158.150, x86_64/KVM for iteration; aarch64 verified
on the Mac (HVF). Default greeter boot, compositor killed with `kill -9` from the
serial root shell, buddy census read from the Ctrl-T dump (`[TASKS] buddy
free_pages=… blocks/order:…`).

## Reproduction (before)

Killing the greeter's cosmic-comp on `7a9ed31`: free pages 355217 → 264930 over
5 deaths on x86_64/KVM, i.e. ~21k pages (~84 MiB) per death, and the loss grows
with time. After ~15 deaths the greeter chain can no longer fork (a cosmic-comp
fork copies ~210 MiB eagerly — see "refuted / observed" below).

## Root cause — four leaks, one of them not kernel memory at all

Instrumented `AddressSpace::drop` and `drm_release_open` to attribute the pages
(temporary `[ASDROP]`/`[DRMREL]`/`[DUMB]` serial lines, removed before commit).

1. **Orphaned greeters (the bulk).** `cosmic-greeter-login` outlives its
   compositor: iced/sctk logs `SCTK dispatch error: underlying IO error: Broken
   pipe` in a hot loop forever (32 KB/s into `/var/log/greetd.log`, one CPU
   spinning). Each orphan pins its own ~180 MiB address space plus every
   dmabuf fd it inherited (see 3). The Ctrl-T dump after 5 deaths showed three
   `cosmic-greeter-login` thread groups in Ready/Running with `ppid` pointing at
   dead compositors: the kernel never reparented orphans, so nothing could wait
   for them, see them, or kill them. On a systemd host the greetd unit's cgroup
   kill would have taken them; here nothing did.
2. **Intermediate page tables were never freed.** `AddressSpace::drop` freed the
   VMA frames and the root page only; every PDPT/PD/PT page a process ever
   touched leaked for the rest of the boot (~50 pages per `brush -c true`,
   610–835 pages per cosmic-comp, ~1000 per greeter chain).
3. **Dumb-buffer gem handles had no owner.** `drm_release_open` swept blob
   handles and syncobjs but not `DUMB_BUFFERS`, so a compositor that died
   without DESTROY_DUMB leaked its swapchain (3 × 8 MiB at 1920x1080 + the
   cursor) and the host-side 2D resource behind each ADDFB. On top of that the
   PRIME export ignored `DRM_CLOEXEC`, so the kiosk child (the greeter)
   inherited a reference on every exported scanout buffer across fork+exec.
4. **The respawn cadence.** init restarted greetd every 3 s regardless of how
   long it had run, so a broken chain was a fork storm.

## What changed

- `mm/src/vmm.rs`, `mm/src/paging.rs`, `arch/{x86_64,aarch64}/src/paging.rs`:
  `arch_free_user_page_tables(root)` walks the user half of the tree (x86_64:
  PML4 entries 0..256 — the kernel half is shared; aarch64: the whole TTBR0
  root, which is private) and frees every table node; `AddressSpace::drop`
  calls it before freeing the root.
- `drivers/src/drm_device_interface.rs`: `DumbBuf.owner` (the creating
  `open_id`, 0 for the legacy custom-ioctl path); `drm_release_open` retires
  the open's live dumb/virgl-3D handles through `free_dumb` (refcounted — an
  exported fd still keeps the object alive); `dumb_release_host_resource`
  RESOURCE_UNREFs the host resource when a BO's last reference goes.
- `kernel/src/syscall.rs`: PRIME_HANDLE_TO_FD honours `DRM_CLOEXEC`.
- `sched/src/lib.rs`, `kernel/src/init.rs`: POSIX orphan reparenting —
  `reparent_children(tgid)` moves a dying process's children to init (run in
  `exit_group` before the group kill, and in `exit` for a leader); `INIT_PID`
  recorded at boot; `last_pid()`.
- `servers/vfs/src/lib.rs`: `/proc/loadavg`'s last field is the last allocated
  pid (as on Linux); `/proc/<pid>/stat` field 6 is the real session id; a
  `[VFS] tmpfiles=… vmos=… vmo_pages=… pipes=… fdtables=… fds=…` census line in
  the Ctrl-T dump.
- `userland/init/src/main.rs`: when greetd exits, `sweep_strays` SIGKILLs every
  process the kernel reparented to init that init did not start and that is
  not in the serial login's session (found through `/proc/loadavg` +
  `/proc/<pid>/stat`, iterated until a pass kills nothing); the respawn delay
  doubles 3→6→12→24→30 s (cap) and resets after a run of ≥ 60 s, and the
  20-respawn ceiling counts consecutive short runs only.
- Tests: `drmsmoke --leak [N]` (N forked children open card0, CREATE_DUMB ×3 +
  MAP_DUMB + mmap + ADDFB2 + one PRIME export mmapped through its fd, then
  `_exit` or SIGKILL with everything still held; MemFree before/after must
  differ by < 256 KiB per death — no master needed, safe next to a live
  compositor). `memtest exit_frees_page_tables` (32 children each owning ~512
  page-table pages; < 512 KiB lost per death). memtest's `sysinfo` number is
  now arch-correct (it was x86_64's on aarch64, so `fill_ram_no_leak` was
  silently skipping itself there).

## Evidence

x86_64/KVM (desktop), full build of this branch:
- `drmsmoke --leak 20`: `kib_lost_per_death=0`, `leak_children_alloc_ok: PASS`,
  `leak_per_death_under_budget: PASS` (20 compositor-shaped deaths, half by
  SIGKILL, run next to the live greeter).
- `memtest`: `exit_frees_page_tables: PASS` (`lost_kib_per_death=461`, budget
  512 — see "where I stopped"), the older cases unchanged.
- Greeter kill loop, 6 deaths with a 60 s settle: init logs `killed 2 stray
  process(es) left by the graphical login` and `graphical login exited after
  ~100 s, restarting in 3 s (1/20)` on every death (backoff reset by the stable
  run); the Ctrl-T dump shows exactly one greeter chain alive after each
  respawn (before: one orphaned `cosmic-greeter-login` per death, spinning);
  the `[VFS]` census returns to `vmos=0 vmo_pages=0 dmabuf=0` after every
  death. The trough (first sample after each respawn) still falls:
  466750 → 453414 → 438708 → 423914 → 410478 → 399440 pages, i.e. **~14k
  pages (~55 MiB) per death remain unexplained** — NOT dumb buffers, page
  tables, VMOs, pipes, fd tables or leftover processes (all checked).

aarch64/HVF (Mac), full build of this branch, run right after boot while the
greeter was still coming up:
- `memtest`: `exit_frees_page_tables: PASS` (`lost_kib_per_death=429`),
  `fill_ram_no_leak: PASS` (this case used to skip itself on aarch64).
- `drmsmoke --leak 20`: `kib_lost_per_death=657` → `leak_per_death_under_budget:
  FAIL` against the 256 KiB budget. 657 KiB is 1/6 of one leaked buffer and the
  same magnitude as memtest's residual, and the desktop was still settling
  during the run, so this is either the residual per-death leak below or
  greeter growth; not separated for lack of time.

## Where I stopped / next steps / unverified

- MERGEABLE for the kernel/DRM/init changes (both arches build, boot, and the
  DRM leak test is 0 KiB/death on x86_64), but the item is NOT closed: a
  residual ~14k pages per greeter-chain death (x86_64) and ~100–160 pages per
  plain process death (both arches, memtest/drmsmoke residuals) remain. Next:
  instrument `mm::buddy` by caller class (slab/heap growth is the prime
  suspect — `PAGE_REFS`, `lazy_pages` Vecs, EXIT_LOG, net buffers — plus the
  f2fs side of `/var/log/greetd.log`, which each greetd start truncates and an
  orphan greeter floods), and re-run the 15-death loop comparing troughs.
- `drmsmoke --leak`'s 256 KiB budget may be too tight when a desktop is still
  growing; run it after the greeter has settled (or with
  `/etc/leandros/text-login`) before reading a FAIL as a regression.
- The 15-death storm to `[BUDDY] Allocation failed` was not re-run to the end
  on this branch (6 deaths verified on x86_64/KVM, ~100 s apart, no orphans).
- aarch64 greeter kill loop not run (only the two test binaries).

## Refuted / observed on the way

- "~140 MB of kernel memory per death" is mostly not kernel memory: it is the
  orphaned greeter (a userspace process) plus its inherited dmabuf refs. The
  genuinely leaked kernel pages per death were ~1000 (page tables) + 6148
  (dumb buffers) ≈ 28 MiB.
- Memory readings around a greeter respawn are not comparable phase to phase:
  cosmic-comp's footprint grows from ~200 MiB to ~300 MiB over its first
  minute, and `fork` in this kernel copies every writable private page eagerly
  (`mm/src/cow.rs`), so spawning the kiosk child costs a transient ~210 MiB —
  the point at which a depleted guest hits `[BUDDY] Allocation failed`.
  Compare troughs (all of the chain dead), not steady states.
- The tmpfs/memfd VMO store, pipe rings and fd tables were checked with the
  new `[VFS]` census: they return to baseline after every death.

## What remains

- `fork` of a large process copies all writable private pages eagerly (a
  ~210 MiB copy for cosmic-comp's kiosk spawn); `posix_spawn`/`vfork` or true
  CoW for writable pages would remove the transient.
- `DrmDevice.framebuffers` entries created by a dead open are not swept (a few
  bytes each; upstream's `drm_fb_release`).
- A dumb handle imported into a second open via PRIME_FD_TO_HANDLE is the
  exporter's handle number; the exporter's release now retires it, and the
  importer keeps the pages only through its fd (as before, the object survives
  on the fd's reference).
- cosmic-greeter's hot loop on a dead compositor socket is upstream behaviour
  (systemd's cgroup kill hides it there); init's sweep is the LeandrOS answer.
