# cosmic-comp COSMIC_SESSION_SOCK recursion (ELR=0x1516B04) — host-only analysis

Date: 2026-07-23. Host-only lane. Binary analysed:
`m3-gl-stack/out/cosmic-comp-aarch64` sha256 `320bfe17…` (33,610,496 B).
Confirmed = the packed binary (byte-for-byte present in `f2fs-data0-aarch64.img`;
mkfs log packs 33,610,496 B; f2fs is NOT compressing).
smithay source: git rev `efeb597` (from `cosmic-comp/Cargo.lock`), checkout at
`~/.cargo/git/checkouts/smithay-312425d48e59d8c8/efeb597`.

## TL;DR (read this first)

1. The reported fault registers are **internally inconsistent with the binary**, so
   `0x1516B04` **cannot be trusted-symbolized from the ELR alone.** The team's working
   assumption ("find the recursive function at 0x1516B04 = draw_solid") is a **dead end** —
   proven below by two independent hardware checks.
2. The recursion itself is **real** (sp driven to the exact stack base at 256 KB *and* 8 MB;
   FAR always = sp−0x60). The recursive function is a **thin, fp/lr-only 0x60-byte-frame
   function** (prologue `stp x29,x30,[sp,#-0x60]!`) — a forwarder/tree-walker, not a big
   renderer function.
3. It is a **userspace bug**, gated by COSMIC_SESSION_SOCK only because that env var is what
   makes cosmic-session hand out `WAYLAND_DISPLAY` → clients connect → the compositor gets
   real windows/surfaces to lay out and composite. The kernel syscall suspects
   (fcntl/SIGPIPE/fd-numbering) are **ruled out** (§4).
4. **The one action that resolves this in a single run: add an EL0 fault-time x29-chain
   backtrace to the kernel fault handler, and log each exec's load base.** Prologues are
   frame-pointer-based (`add x29, sp, #…`), so the chain is walkable. §6.

## 1. Load base and the naive mapping (and why it is WRONG)

- Loader: `kernel/src/syscall.rs:218 MAIN_DYN_BASE = 0x0020_0000`; `:2902 bias = ET_DYN ?
  MAIN_DYN_BASE : 0`. cosmic-comp is `ET_DYN` **with PT_INTERP** (`/lib/ld-musl-aarch64.so.1`)
  → dynamically-linked PIE, mapped by the kernel at bias `0x200000`. init (PID 2) enters at
  `0x2109AC`, consistent with a 0x200000 image base. So base = **0x200000**.
- Naive: `0x1516B04 − 0x200000 = 0x1316B04`. addr2line/nm put that inside
  `<GlesFrame>::draw_solid` (inherent method, symbol `0x1316854`), at **+0x2B0**.

**This mapping fails two independent consistency checks against the fault dump**
(`ESR=0x92000047`, `FAR=0x7FFFFF7FEFA0`, `sp=0x7FFFFF7FF000`; EC=0x24 data abort,
DFSC=0x07 translation-fault L3, **WnR=1 = WRITE**, FAR = sp−0x60):

- **(a) Write fault on a load instruction.** The bytes at file-vaddr 0x1316B04 are
  `b958f340 = ldr w0,[x26,#0x18f0]` — a register **load**. A load cannot raise a **write**
  abort. (Bytes verified in both my copy and the packed image.)
- **(b) An earlier store through the same base register would fault first.** `x26` is loaded
  once (`0x13169f8 ldr x26,[x22,#0x48]`) and is **stored through** on the only path to +0x2B0:
  `0x1316a18 str xzr,[x26,#0x138]` and `0x1316adc str x8,[x26,#0x138]`. For FAR=sp−0x60 the
  +0x2B0 load needs `x26 = sp−0x1950` (below the stack, unmapped) — but then those two earlier
  stores (to `x26+0x138`, also below sp) would have aborted at 0x1316a18/0x1316adc, not at
  0x1316b04. They didn't. So `x26` was valid there ⇒ the +0x2B0 load could not abort at sp−0x60.

Conclusion: **at base 0x200000 the fault is provably not draw_solid+0x2B0.** Either the runtime
load base is not what the current kernel constant says, or the fault handler is reporting a
value that is not the true `ELR_EL1` (see §5). The coincidental landing in `draw_solid` is
misleading; do not act on it.

## 2. What the recursion actually looks like (this part is solid)

- `FAR = sp − 0x60` every time; `sp` lands on the **exact** stack base for **both** the 256 KB
  and the 8 MB kernels (m6-progress steps 6–7). sp marched all the way down ⇒ genuine
  **unbounded recursion** (or an >8 MB frame, excluded — no single Rust frame is 8 MB).
- The overflowing access is a prologue push writing x29 at sp−0x60 ⇒ the recursive function's
  first instruction is `stp x29, x30, [sp, #-0x60]!` (`a9ba7bfd`), i.e. a **0x60-byte frame
  that saves only fp+lr** — a *thin wrapper or a tree/recursive-descent walker*, NOT a heavy
  renderer routine (draw_solid's own prologue is `str d8,[sp,#-0x70]!` — different, so
  draw_solid is not it).

### Structural candidates that match the profile AND the sock-gating
Design-recursive functions in cosmic-comp/smithay whose prologue is exactly
`stp x29,x30,[sp,#-0x60]!` and that only run once real client windows/popups exist
(so: fine "direct", crash "under session"). These are the shortlist for the backtrace to
confirm — a **cyclic data structure fed by client surfaces** would make any of them recurse
without bound:

| file-vaddr | function | cycle that would blow it |
|---|---|---|
| 0xc5be9c | `cosmic_comp::shell::focus::raise_with_children` | a window that is its own transient-parent / A↔B transient cycle |
| 0x1345dbc | `smithay::desktop::wayland::popup::PopupManager…PopupNode::try_insert` | a popup whose parent chain forms a cycle |
| 0xc06ab4 | `cosmic_comp::shell::layout::tiling` id_tree `Tree::<Data>` walk | a tiling node that is its own ancestor |
| 0x9d6dd4 | `cosmic_comp::shell::grabs::menu MenuAlignment::rectangles_for_alignment` | self-referential menu geometry |

(None of these sits at 0x1516B04 under base 0x200000 — consistent with §1's finding that the
base/ELR reporting is off. Implied bases if the ELR really were one of their prologues are all
non-round: 0x8BAC68 / 0x1D0D48 / 0x910050 / 0xB3FD30 — inconclusive without the real base.)

The alternative to a cyclic tree is a **dynamic-dispatch forwarding loop**: cosmic-comp's KMS
path stacks `MultiFrame → GlowFrame → GlesFrame`, each forwarding `draw_solid` to the inner
layer. Source (smithay efeb597) shows the forwarding is written against the **concrete** inner
type (`self.frame.as_mut().unwrap().draw_solid(…)`), which resolves to the inherent method —
correct, non-recursive in this build. So a pure method-resolution loop is *not* structurally
present; a **cyclic input graph** is the more likely driver.

## 3. Why COSMIC_SESSION_SOCK is the gate (not the cause)

`session::setup_socket()` (once, early, `lib.rs:145`) and `session::run_socket()` (once, in
`notify_ready`, `lib.rs:79`) are **not recursive**. What the sock does: comp writes
`WAYLAND_DISPLAY` back to cosmic-session over the inherited socketpair; cosmic-session then
launches cosmic-bg / cosmic-panel / … as Wayland clients. Those clients map surfaces/popups →
the compositor now runs its **focus/raise/tiling/popup layout + composite** paths on real
input. "comp direct" and "comp under a bus w/o sock" never get clients, so those recursive
paths are never entered. The env var selects the code path; it does not itself recurse. This
matches m6-progress step 8 exactly.

## 4. LeandrOS-vs-Linux delta — kernel suspects ruled out

- **fcntl F_GETFD/F_SETFD on the socketpair fd** (`set_cloexec`): runs once in `setup_socket`,
  non-recursive, and *before* the event loop. Not in any recursive path.
- **SIGPIPE / broken launch-pad pipe**: writes to the session sock happen once in `run_socket`;
  tracing/`fmt` writers swallow `io::Error` (no re-log-on-write-error loop). Not a stack
  recursion source.
- **fd numbering ≥ 0x100 breaking select()/FD_SET(<1024)**: calloop uses **epoll** (`Generic`
  source), never `select`/`FD_SET`, and 0x100 < 1024 anyway. No path here.

None of these produces in-process render/shell recursion. **This is not a kernel
syscall-semantics bug.** The only *kernel-adjacent* userspace possibility is the dynamic linker
(`ld-musl`/relibc `ld.so`) mis-resolving a GL/EGL symbol so a `gl.*` call re-enters comp — but
the fault site (a `ldr` right after the `blr x8` GL dispatch at draw_solid+0x2ac returned
normally) does not support that; the blr returned. Keep it only as a tertiary hypothesis for
the backtrace to rule in/out.

The genuine LeandrOS delta is simply: **LeandrOS is the first environment where comp actually
composites live client windows via the software (llvmpipe) path**, so a latent cyclic-graph /
recursive-walk bug that a normal GPU desktop also has (but perhaps never trips) is now hit —
or a LeandrOS-specific ordering during surface/popup setup builds a cyclic tree.

## 5. Also check: is the fault report itself trustworthy?

Because §1 proves ELR/FAR/WnR are mutually inconsistent with the binary at the implied address,
a real possibility is a **fault-handler reporting bug**: printing `x30`/LR (a return address)
as "ELR", or a stale ELR/FAR from a prior fault, or an off-by-one. Note `0x1516B04` is exactly
the instruction *after* draw_solid's `blr x8` at 0x1516B00 — i.e. it is precisely draw_solid's
**return address (LR)** for that GL-dispatch call. If the handler prints LR-as-ELR, then the
true fault is in the callee of that blr and everyone has been symbolizing a return address.
Worth a 5-minute audit of the aarch64 EL0 sync-abort path (which of `ELR_EL1`/`x30`/`FAR_EL1`
it latches and in what order).

## 6. Fix, ranked

1. **(DO THIS FIRST — instrumentation, unblocks everything) Kernel: dump an EL0 user
   backtrace + load base at fault.** Prologues are `stp x29,x30,[sp,#-0x60]!` +
   `add x29, sp, #0x10`, so walk the fp chain: start `fp = x29`; repeat
   `ret = *(fp+8); next = *(fp); print ret; fp = next` for ~40 frames (bounds-check each read
   to the user stack VMA). Also print the recursive function's own PC (true `ELR_EL1`) and the
   process's **actual load base** (log `MAIN_DYN_BASE`/bias at `load_and_spawn_elf`). One m6
   session run then names the function directly:
   `llvm-addr2line -f -C -e cosmic-comp-aarch64 <ret − loadbase>` for each frame.
   This also settles §1/§5 (real base vs. handler bug) for free.
2. **Keep the 8 MB `USER_STACK_SIZE` bump** (correct robustness regardless; 256 KB was
   pathologically small). It does not fix the recursion — expected.
3. **Comp-side fix (spec once the backtrace names the function):** add a cycle guard /
   depth-limit at the identified walker. If it is `raise_with_children` or `PopupNode::
   try_insert` / tiling tree — guard against re-visiting an already-seen node (visited-set or
   ancestor check) and log+break the cycle. This is a small, local patch; exact call site TBD
   from the backtrace. Requires orchestrator approval + a comp rebuild.
4. **Tertiary (only if the backtrace shows a `gl.*`/dlsym stub in the cycle):** loader symbol
   resolution — audit `ld-musl`/relibc dlsym vs. `eglGetProcAddress` on LeandrOS.

There is **no principled kernel *semantics* fix** — the kernel cannot cure a userspace infinite
recursion; it can only diagnose it (item 1) and survive longer (item 2).

## 7. Cheap on-target verification probes (for the tree wave)

- **P1 (definitive): the §6.1 backtrace.** Re-run the m6 s0 session with the fp-walk +
  base-log kernel; capture the frames; symbolize offline. Expect the same 1–2 function names
  repeating — that IS the recursion.
- **P2 (base sanity, ~5 min): log the exec load base.** Print bias at `load_and_spawn_elf`;
  confirm cosmic-comp really is at 0x200000. If it is, §1's paradox means the handler
  mis-reports (audit per §5). If it isn't, subtract the real base and re-symbolize — that alone
  may name the function.
- **P3 (isolate cyclic-tree vs. GL): run comp under the session but `--no-xwayland` and with
  only cosmic-bg (no panel/launcher).** If it still recurses with just a background client, the
  cycle is in the wallpaper/output layout path; if only panel/popup clients trip it, it is the
  popup/menu/focus tree — narrows the guard site before the backtrace even lands.

## Symbolization recipe (for whoever gets the real base B)
```
llvm-addr2line -f -i -C -e m3-gl-stack/out/cosmic-comp-aarch64  $((RUNTIME_ADDR - B))
# B = 0x200000 per the current kernel constant, but VERIFY at runtime (P2) before trusting it.
```
