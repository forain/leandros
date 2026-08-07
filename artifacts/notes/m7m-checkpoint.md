# M7m checkpoint
Main e07bc29. Mission: name W3 recursion site, produce comp-patch decision pkg, escalate.
W1 SOLVED (M7l, commit 20657c0). Sole blocker = W3 comp unbounded recursion, EL0 PID5 write-fault sp-base.
Fault handler: arch/aarch64/src/exception.rs:157 exc_el0_sync_handler, exit at :228.
UserFrame: sched context.rs:47, .x[31] (x[29]=fp,x[30]=lr), .sp_el0.
Main stack window: [USER_STACK_TOP-USER_STACK_SIZE, USER_STACK_TOP] = [0x7FFFFFFFF000-0x800000, 0x7FFFFFFFF000] eager-mapped 8MB.
cosmic-comp = static-linked Rust PIE base 0x200000 (smithay linked IN, not .so). addr2line vs cosmic-comp-aarch64.
PLAN: (1) add gated x29-walk bounded to main-stack window in exception.rs before exit. (2) one session run, symbolize. (3) check --no-xwayland arg parse in cosmic-epoch/cosmic-comp. (4) escalate pkg.
## Steps
- [ ] step0 orient

## FINDING item3 (--no-xwayland) — RESOLVED, launcher flag is CORRECT
cosmic-comp/src/lib.rs: --no-xwayland IS parsed correctly (line 128 RawArgs loop -> with_xwayland=false; help text line 262). NOT a wrong flag.
The "kiosk child" is cosmic-comp's OWN quirk: notify_ready (lib.rs:84) treats env::args().skip(1) i.e. argv[1] as a kiosk-child exec command. So `cosmic-comp --no-xwayland` uses argv[1] BOTH as the flag AND (bogusly) as kiosk child "--no-xwayland" -> spawn fails -> "Error running kiosk child" = HARMLESS post-W1-fix (was the trigger of the W1 close_all bug, now fixed).
W3 recursion is in client composite/focus/tiling/popup layout (native Wayland clients cosmic-bg/panel), independent of xwayland. => item3 does NOT change W3 trigger. Flag spelling correct; kiosk child benign. Only path to desktop = comp recursion patch (needs approval).

## W3 BACKTRACE CAPTURED (run bt0, aarch64 uefi-tcg) — DECISIVE recursion signature
- EL0 Fault PID=5 ESR=92000047 FAR=0x7FFFFF7FEFF0 EC=0x24 DFSC=07 WnR=1(write) ELR=0x1516B04
- [BT] fp=0x7FFFFF7FF050 lr=0x1516C3C ; ALL 64 frames ret=0x1516C3C (uniform self-recursion)
- Base=0x200000 CONFIRMED (loader syscall.rs:2943 ET_DYN->MAIN_DYN_BASE; interp 0x30000000, mmap 0x40000000+)
- asm exception_asm.s:173,186: elr arg = elr_el1 (TRUE PC, not LR). M6 §5 LR-bug REFUTED.
## PARADOX (unresolved, needs on-target insn dump):
- addr2line(0x1316B04)=GlesFrame::draw_solid ; addr2line(0x1316C3C)=draw_solid too
- BUT disasm file-vaddr 0x1316B00=`blr x8`, 0x1316B04=`ldr w0,[x26,#0x18f0]`(LOAD), 0x1316C38=`stur x13,[x29,#-8]`, 0x1316C3C=`stp x10,x11,[sp,#0x1d8]`
- A LOAD cannot raise WnR=1 write fault; 0x1516C3C landing mid-basic-block (stp, not after a bl) is not a valid return site
- => runtime bytes at 0x1516B04 must DIFFER from file 0x1316B04, OR base!=0x200000 for this addr. Contradicts loader code.
- brute-force base search (prologue a9ba7bfd + call@+0x134): 59 candidates, none canonical => underdetermined.
## NEXT: rebuilt kernel with fault-time insn dump (read live runtime words @ELR-4/ELR/ELR+4 and @x30-8/-4/0, bounded to main-image VA). Run m7m_btcap again -> the runtime insn word names truth.

## RUN bt1 (insn dump) — GROUND TRUTH runtime bytes at fault:
- @0x1516B00=D65F03C0 ret ; @0x1516B04=A9BA7BFD stp x29,x30,[sp,#-0x60]! (PROLOGUE, faulting write) ; @0x1516B08=F9000BFB str x27,[sp,#0x10]
- @0x1516C34=AA1703E2 mov x2,x23 ; @0x1516C38=D63F0100 blr x8 (INDIRECT recursive call) ; @0x1516C3C=B40005F5 cbz x21
- => runtime bytes DIFFER from cosmic-comp file 0x1316B04 (blr;ldr). Recursion is a self-loop via `blr x8` (virtual/fn-ptr dispatch). NOT the main cosmic-comp mapping at 0x200000.
- Byte-window search across all aarch64 .so + cosmic-comp: UNIQUE match only cosmic-comp file-off 0x11e6fe8 (vaddr 0x11f6fe8) -> but runtime at base 0x200000 = 0x13F6FE8 != 0x1516B04. Likely COINCIDENTAL (generic compiler dispatch-loop pattern); real module mis-identified.
## NEXT: rebuilt with sched::dump_user_vma -> prints faulting task's VMA map (start/end/prot/file_cap/file_off). The region containing 0x1516B04 gives module base+file offset -> definitive module ID -> symbolize.

## ★★★ W3 ROOT CAUSE — NOT cosmic-comp. It is BRUSH (the shell). ★★★
- VMA dump (run bt2): PID5 sole module = file-backed EXEC at base 0x1000000, .text foff 0x101000 flen 0x48FFBC. No ld-musl, no 0x200000 image => STATIC binary.
- IDENTIFIED: /Users/forain/code/brush/target/aarch64-unknown-linux-musl/release/brush — ET_EXEC, 6123952 B, base 0x1000000, entry 0x1111070. LOAD segments match VMA EXACTLY. Byte window at file-off 0x506B04 == captured runtime bytes EXACTLY.
- brush is STRIPPED (0 nm symbols) -> addr2line ??. Recursing fn = brush .text vaddr 0x1516B04 (ET_EXEC bias0, runtime=vaddr).
- The ENTIRE M6/M7 "cosmic-comp draw_solid recursion" was a MIS-SYMBOLIZATION: assumed PID5=cosmic-comp @ base 0x200000 (giving draw_solid). Wrong binary, wrong base. It's brush @ 0x1000000.
- PID6 IS cosmic-comp (@0x200000, ELR 0x13BFEB4 = rustybuzz find_language_feature) — separate instr-abort, secondary.
## RECURSION MECHANISM (disasm of brush 0x1516B04):
- Reads atomic global registry ptr @ brush .data 0x1606d90 (RwLock-guarded: two bl 0x158f570 = lock acquires).
- Linear-searches a table of 0xc0-byte entries by u32 key w24; matched entry x26.
- Loads fn ptr [x26,#0x20], filters (>=2 level check + byte[x26,#0xa8] bit2 enabled), then `blr x8` (0x1516c38) with (w24,x21,x23).
- That fn ptr re-enters 0x1516B04 => unbounded self-recursion. SHAPE = Rust `tracing` crate callsite/subscriber dispatch re-entrancy (subscriber logs from inside its own event handler).
## IMPLICATION: NO cosmic-comp patch needed. Fix = brush (tracing re-entrancy) OR launcher-script workaround. Escalation premise inverted.
## Probe running: bisect which launcher construct triggers brush recursion (trap/cmdsubst/arith/fd3/etc).

## M7m CLOSE-OUT (escalation, premise inverted)
- W3 ROOT: brush tracing-dispatch recursion @ brush .text 0x1516B04 (module base 0x1000000). NOT cosmic-comp. 3x reproduced, byte-exact.
- Trigger: full launcher job/exec paths (not isolated constructs/RUST_LOG). Next = BRUSH wave (debuginfo build names it; fix re-entrancy).
- Tree reverted CLEAN e07bc29 (exception.rs + sched/lib.rs restored). Diagnostic saved: notes/m7m-el0-backtrace-facility.diff. Clean aarch64 images rebuilt.
- Notes: m7-progress.md "M7m" section; MEMORY.md index updated; harnesses m7m_btcap.py/m7m_symbolize.sh/m7m_probe{,2,3}.py.
- NO commits (finding inverts mission; orchestrator redirects to brush). x86_64 not exercised (aarch64 was sufficient to root-cause; brush bug is arch-independent).
