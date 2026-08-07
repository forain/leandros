# comp-recursion analysis — progress / checkpoint

Status: DONE (host-only static analysis). Deliverable: `comp-recursion-analysis.md`.

## What was established (high confidence)
- Binary analysed = the packed binary: `m3-gl-stack/out/cosmic-comp-aarch64` sha256 320bfe17…,
  33,610,496 B, byte-for-byte present in `f2fs-data0-aarch64.img` (f2fs not compressing).
  (m6-session-bins/out copy was moved away by the tree wave; only the m3 copy remains.)
- cosmic-comp is a **dynamic PIE** (PT_INTERP=/lib/ld-musl-aarch64.so.1), kernel bias
  MAIN_DYN_BASE=0x200000 (syscall.rs:218/2902).
- Fault dump decode: ESR=0x92000047 → EC=0x24 data abort, DFSC=0x07 transl-fault L3,
  **WnR=1 WRITE**, FAR=sp−0x60, sp=8 MB stack base. Genuine unbounded recursion (sp→base at
  both 256 KB and 8 MB). Recursive fn prologue = `stp x29,x30,[sp,#-0x60]!` (thin fp/lr frame).

## KEY correction to the team's assumption
- Naive `0x1516B04−0x200000=0x1316B04` → `GlesFrame::draw_solid+0x2B0` is **DISPROVEN** by two
  independent checks: (a) bytes there are `ldr w0,[x26,#0x18f0]` — a LOAD, can't raise a WRITE
  abort; (b) earlier stores through the same x26 (+0x138) on the only path would fault first.
  ⇒ ELR-based symbolization is UNRELIABLE. Do not chase draw_solid.
- Likely: either the runtime base ≠ what's assumed, OR the EL0 fault handler prints LR-as-ELR /
  stale values (0x1516B04 is exactly draw_solid's return addr after its `blr x8` GL dispatch).

## Ruled out
- Kernel syscall suspects: fcntl set_cloexec (once, early), SIGPIPE/pipe (writers swallow errs),
  fd≥256 vs FD_SET (calloop uses epoll, not select). Not a kernel-semantics bug.
- smithay draw_solid trait/inherent + Multi/Glow/Gles forwarding is written against concrete
  inner types → resolves to inherent → NOT a structural method-resolution recursion in this build.

## Leading hypothesis
- Userspace unbounded recursion in a **thin 0x60-frame walker fed a cyclic graph by real client
  windows/popups** (only reached once COSMIC_SESSION_SOCK → session launches clients).
  Shortlist (prologue matches, sock-gated): shell::focus::raise_with_children;
  desktop::wayland::popup PopupNode::try_insert; tiling id_tree Tree walk;
  grabs::menu MenuAlignment::rectangles_for_alignment.

## Handoff — next actions (for tree wave / orchestrator)
1. Add EL0 fault-time **x29-chain backtrace + load-base log** to the kernel (recipe in analysis
   §6.1). One m6 s0 run + offline addr2line names the function definitively.
2. Verify comp's real load base (log bias at load_and_spawn_elf) — settles base-vs-handler-bug.
3. Keep the 8 MB stack. Comp-side cycle-guard patch to be spec'd once backtrace names the site.
4. Optional narrowing probe: run session with only cosmic-bg (no panel) / --no-xwayland (P3).

No repo writes, no QEMU, no git — host-only lane respected.
