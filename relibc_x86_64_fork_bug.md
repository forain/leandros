# relibc `fork()` child-return corruption on x86_64

**Status:** FIXED 2026-07-03. Root cause: stale writable TLB entries in the
*parent* after `clone_as` downgraded its live PTEs to read-only for CoW.
Fix: TLB flush at the end of `mm::cow::clone_as` (`mm/src/cow.rs`).
forktest went 0/10 → 10/10 on x86_64; all suites green on both arches.

**Last investigated:** 2026-07-03.

---

## 0. Root cause and fix (resolution)

`clone_as` downgrades the parent's own page-table entries to read-only
(`map_page(src_root, …, downgraded)`) so that either side's next write takes
a CoW fault. But `arch_map_page` performs **no TLB invalidation** on either
arch — the aarch64 version even documents that its `dsb/isb` covers
invalid→valid transitions only, and x86_64's `map_4k` just writes the PTE.
That is fine for the *child* (its fresh CR3/TTBR0 load flushes on first
schedule), but the **parent returns from the fork syscall with stale
writable TLB entries for its own stack still cached**.

The failure sequence (answers §8's open questions):

1. Parent's `fork()` returns and, before any context switch, the caller
   invokes the `waitpid` wrapper. Those pushes/locals write to the same user
   stack region where the child's copy of relibc `fork()`'s frame (saved
   `rbx/rbp/r14/r15` and the return address) lives.
2. The write goes through the stale writable TLB entry — **no fault, no CoW
   copy** — mutating the still-shared physical page.
3. The child is scheduled, resumes at the `cmp` after `syscall` with a
   perfectly correct `rax = 0` (as the `[FKDBG]` print showed), takes the
   `pid == 0` path, `xor eax,eax` — and then the epilogue **pops corrupted
   callee-saved registers and `ret`s through the overwritten return-address
   slot**, landing after the parent's `call waitpid` site instead of at the
   `if (pid == 0)` test. The child skips the branch and runs the parent's
   code path → the fork cascade. The corruption was never `rax`; it was the
   user-stack `ret`.

Why each prior observation fits:

- **Delay in `fork_current` fixes it / child-first passes:** if the child
  runs first, its first stack *write* faults (fresh TLB), CoW splits the
  page, and the parent's stale-TLB writes then land on a frame the child no
  longer maps. Also, any CR3 reload (every context switch) clears the
  parent's stale entries — so the hazard window is exactly "parent user-mode
  writes between fork-return and the first switch."
- **Preemption off → deterministic fail:** cooperative order is always
  parent-first through `waitpid`, i.e. always inside the hazard window.
- **Raw `clone(SIGCHLD)` immune:** the inline syscall consumes `rax` in a
  register with no user-stack pops or `ret` inside the window.
- **aarch64 green:** same latent hazard, but `arch_set_page_table` does
  `tlbi vmalle1` on every switch and the scheduling order there doesn't hit
  the window with a corrupting write. The fix flushes on aarch64 too
  (`tlbi vmalle1is` via `arch_tlb_shootdown_all`), closing it properly.

**The fix** (`mm/src/cow.rs`): call `tlb_shootdown_all()` at the end of
`clone_as`, while the parent's root is still the active one, so the parent's
first post-fork write takes the CoW fault it must. On x86_64 this is a CR3
reload (user PTEs are non-global); on aarch64 an inner-shareable `tlbi`.

**Verification (2026-07-03):** x86_64 forktest 10/10 (was 0/10); memtest,
polltest, sigtest, pthreadtest, timertest all green; full suite likewise
green on aarch64.

The sections below are the original investigation record, kept for history.

---

## 1. One-paragraph summary

On x86_64, calling relibc's `fork()` (the userspace wrapper in
`userland/relibc/src/header/unistd/mod.rs`) returns a **corrupted, nonzero
value to the child process** most of the time. Because the child sees a
nonzero return, it takes the *parent* branch of `if (pid == 0)`, so a
fork-then-continue program spawns a runaway duplicate that re-forks in every
subsequent step (an exponential process cascade). The **kernel clone/fork path
is correct** — a raw `clone(SIGCHLD)` syscall gives the child `0` reliably, and
the kernel demonstrably writes `rax = 0` into the child's saved frame. The
corruption is in the userspace path *after* the syscall returns, and it is
**timing-sensitive**: perturbing the schedule (e.g. an extra serial print in
the kernel fork path) makes it pass. It has not been root-caused.

The same code path is **correct on aarch64**: `userland/forktest` is 8/8 green
there.

---

## 2. Symptoms

Running `forktest` (or any relibc-linked program that does
`fork()` then branches on the return value) on x86_64:

```
fork_return_and_waitpid: FAIL
child_malloc_after_fork: FAIL
pthread_atfork_hooks_run: FAIL
--- forktest done ---
pthread_atfork_hooks_run: FAIL
--- forktest done ---
child_malloc_after_fork: FAIL
pthread_atfork_hooks_run: FAIL
--- forktest done ---
...
```

The tell-tale sign is **multiple `--- forktest done ---` lines** (8 per run =
2³, one doubling per `fork()` across the 3 tests). Each forked child, having
received a nonzero return, believes it is the parent, continues past its
intended `_exit`, reaches the next test, and forks again.

Pass rate measured on x86_64: **0/10** for the committed forktest; occasionally
1/10 passes purely on timing.

---

## 3. What is confirmed CORRECT (rule-outs)

These were each verified empirically, so the bug is **not** here:

1. **The kernel clone/fork delivers `rax = 0` to the child.**
   - `memtest` (which forks via `leandros_libc`'s raw `clone(SIGCHLD)`,
     `syscall1`) passes reliably on x86_64, including
     `map_shared_fork_visibility`, which *requires* the child to actually run
     its `if (pid==0)` branch and write to shared memory.
   - A raw inline-asm `clone(SIGCHLD)` (5-arg form, identical args to relibc)
     dropped into forktest gives the child `0` reliably.

2. **The kernel writes the correct child frame.** A debug print added to
   `fork_current` (`sched/src/clone.rs`, x86_64 branch, right after
   `(*child_frame_ptr).rax = 0`) showed, for the relibc-fork child:
   ```
   [FKDBG] child rax=0000000000000000 rip=000000000021574A rsp=00007FFFFFFFEDD0
   ```
   `rax = 0`, and `rip = 0x21574A` is **the instruction immediately after the
   `syscall`** in relibc's compiled `fork()` (see §5), not the syscall itself —
   so the child is not re-executing `clone`.

3. **relibc's `fork()` source logic is correct.** With the `atfork` hook lists
   empty (they are — `enable_alloc_after_fork` is never called; see §6), the
   wrapper reduces to `Sys::fork().or_minus_one_errno()` plus empty loops.
   `e_raw(0)` → `Ok(0)` → `0`.

4. **The disassembly returns 0 for the observed child state.** A child
   resuming at `fork+0x6a` with `rax = 0` provably falls through to the
   `return 0` path (see §5).

5. **Not SMP.** QEMU is launched with no `-smp`, so it is single-CPU. Cross-CPU
   races are ruled out.

6. **Not the kernel `wait4` bug.** A separate, real kernel bug (`sys_wait`
   returning `0` instead of the pid and writing a raw exit code instead of the
   `(code & 0xff) << 8` status) was found and fixed in the same session
   (commit "kernel: return reaped pid and POSIX-encode wait status from
   wait4"). That fix is independent; the fork corruption reproduces regardless.

7. **aarch64 is fine.** Identical relibc source, identical `forktest`; 8/8
   green. So the bug is specific to the x86_64 codegen/ABI/runtime, not the
   relibc fork logic itself.

---

## 4. How to reproduce

### 4.1 Build

```sh
cd /Users/forain/code/leandros

# relibc for x86_64 (produces librelibc.a) — usually already built:
#   userland/relibc/target/x86_64-unknown-leandros/release/librelibc.a

# forktest (links librelibc.a directly, like pthreadtest/sigtest/polltest):
RUSTFLAGS="-C link-arg=--entry=_start -C link-arg=-static -C linker=rust-lld -C relocation-model=static" \
cargo +nightly build --manifest-path userland/Cargo.toml -p forktest \
  --target targets/x86_64-unknown-leandros.json \
  -Z build-std=core,alloc -Zjson-target-spec --release
```

Then rebuild the initrd + disk image so `/bin/forktest` is present (either
`./scripts/build-all.sh --arch x86_64`, or the incremental initrd+image repack
in §7).

### 4.2 Run

```sh
python3 .claude/skills/run-leandros/driver.py start x86_64
python3 .claude/skills/run-leandros/driver.py cmd "forktest"
```

**Expected (buggy) output:** several `--- forktest done ---` lines and many
`: FAIL` lines (the cascade). A correct run prints each test once with `PASS`
and exactly one `--- forktest done ---`.

For a clean pass-rate measurement, loop the `cmd "forktest"` invocation ~10×
and count runs where `--- forktest done ---` appears exactly once with zero
`: FAIL`.

### 4.3 Minimal repro (no test harness)

Any relibc-linked binary:

```rust
let r = fork();            // relibc fork()
if r == 0 {
    // CHILD: on x86_64 this branch is usually NOT taken (bug)
    write(1, b"child\n".as_ptr(), 6);
    _exit(0);
}
// PARENT (and, buggily, the child too):
let mut st = 0;
waitpid(r, &mut st, 0);
```

Contrast with `memtest`'s raw `leandros_libc::fork()` in the same conditions,
which correctly takes the `pid == 0` branch in the child.

---

## 5. Disassembly of relibc `fork()` (x86_64)

From `userland/target/x86_64-unknown-leandros/release/forktest`
(`fork` at `0x2156e0`; `objdump` via the rustup llvm-objdump):

```
2156e0 <fork>:
  2156e0: push  rbp / mov rbp,rsp / push r15 / push r14 / push rbx / push rax
  2156ea: mov   rax, fs:0x0          ; TLS self-ptr — prepare-hooks loop reads
  ...      (loop over fork_hooks[0]; empty → skipped)
  215734: mov   eax, 0x38            ; nr = 56 (clone)
  215739: mov   edi, 0x11            ; SIGCHLD
  21573e: xor   esi, esi             ; 0
  215740: xor   edx, edx             ; 0
  215742: xor   r10d, r10d           ; 0
  215745: xor   r8d, r8d             ; 0
  215748: syscall
  21574a: cmp   rax, -0xfff          ; <-- child resumes HERE with rax=0
  215750: jbe   0x21576d             ;     (0 <= -0xfff unsigned) → success path
  ...
  21576d: (pid == 0 branch: child-hooks loop, empty) → xor eax,eax → return 0
  215???: epilogue: add rsp,8 / pop rbx / pop r14 / pop r15 / pop rbp / ret
```

The kernel-observed child `rip = 0x21574A` is exactly `215748 + 2` = the `cmp`
right after `syscall`. With `rax = 0`, the child takes `jbe 0x21576d` →
child-hooks path → `xor eax, eax` → returns `0`.

**Conclusion:** statically, a child resuming at `0x21574a` with `rax = 0`
returns `0`. Empirically it returns nonzero. So either `rax` is not actually
`0` when the child *consumes* it (something clobbers it between resume and use)
or the child does not actually execute this straight-line path — neither of
which has been explained.

---

## 6. Notes on the atfork/TLS red herring

The original hazard note blamed "thread-local atfork hooks via `%fs`/`tpidr`."
That is not the cause:

- `fork_hooks` (`userland/relibc/src/header/pthread/mod.rs:90`) is a
  `static mut [LinkedList<...>; 3]`, initially all empty.
- The only thing that populates it, `enable_alloc_after_fork`
  (`platform/allocator/sys.rs`), is **never called** anywhere in the tree
  (`grep -rn enable_alloc_after_fork` finds only the definition). So the
  allocator's lock is *not* registered as an atfork handler.
- With empty lists, the wrapper's three `for … in &fork_hooks[n]` loops are
  no-ops; the child executes no hook code.
- `forktest`'s own `pthread_atfork` test registers handlers, but it runs
  *after* the first fork test, and even there the handlers only touch
  process-local statics.

The compiled prologue *does* touch `%fs:0x0` to read `fork_hooks[0]` (the
loop's list head), but the list is empty so the loop is skipped; and the
child-branch `%fs` read at `0x9a` is immediately followed by `xor eax,eax`, so
it cannot affect the return value.

---

## 7. What was tried (and the result)

### 7.1 Instrumentation (diagnostic, all reverted)

- **Ground-truth forktest** driving control flow off `getpid()` and off
  `fork()==0`, with parent/child comms first over a **pipe**, then over a
  **`MAP_SHARED` page**. Both show the cascade; the shared-memory version is
  what's committed (no blocking I/O, memtest-proven).
- **Single-atomic-line serial prints** (one `write()` per process) to defeat
  char-level interleaving — this is how the "child sees nonzero" conclusion was
  confirmed; naive multi-`write` debug output interleaves and lies.
- **Kernel `[FKDBG]` print** of the child frame `rax`/`rip`/`rsp` in
  `fork_current` — confirmed the frame is correct (`rax=0`, `rip`=after-syscall).
  **Side effect: adding this print makes forktest PASS**, because the extra
  serial latency in the parent's fork syscall shifts the schedule. This is the
  strongest evidence that the bug is a timing/ordering issue, not a static
  logic error.
- **Raw `clone(SIGCHLD)` vs relibc `fork()` A/B probe** in the same binary,
  each recording the child's observed return via a shared-memory slot indexed
  by a `lock xadd` counter (so the value is captured without trusting it for
  control flow). Raw clone child → `0`; relibc fork child → corrupted.

### 7.2 Attempted fixes

- **Disable timer preemption** (`preempt_check` → early `return`). Result:
  forktest failed **deterministically** (every run). This *disproved* the
  "preemption clobbers the child's registers" hypothesis — disabling the
  suspected cause made it worse, not better. (It also revealed that some relibc
  spin/yield paths rely on preemption to make progress.) Reverted.

- **Full-frame-saving x86_64 timer IRQ entry.** Hypothesis: aarch64's IRQ
  vector spills a complete `UserFrame` in assembly before any Rust runs, so a
  `preempt_check()` → `cpu_switch_to` (which only preserves callee-saved
  registers) is transparent; x86_64 used a plain `extern "x86-interrupt"`
  `timer_irq`, which only preserves the registers the handler body touches, so
  a task switch mid-handler could lose the interrupted task's caller-saved
  registers (e.g. the child's `rax`). Implemented a `timer_irq_entry` asm stub
  that pushes a full `UserFrame` (matching `syscall_entry`'s layout),
  conditionally `swapgs`, calls a `timer_irq_handler(frame)`, restores the full
  frame, and `iretq`s; preemption restricted to user-mode ticks. Result: boot
  fine, memtest still green, but **forktest unchanged (still 0/10)**. Combined
  with the "disabling preemption makes it worse" result, this confirms
  preemption is not the corruptor. **Reverted** (kept the tree focused on the
  verified wait4 fix).

### 7.3 Incremental repack helper used during investigation

To avoid the slow full `build-all.sh` (doom/MAME), a script rebuilt only the
initrd + disk image from already-built kernel/relibc:

```sh
# copies userland/target/<arch>-unknown-leandros/release/forktest into the
# -none release dir, rebuilds initrd-<arch>.cpio (cpio -H newc) and
# leandros-limine-<arch>.img (mkfs.fat + mmd/mcopy + sgdisk), reusing
# target/final-<arch>/kernel and .limine-cache/limine-11.4.1-binary.
```

(See the session's `$CLAUDE_JOB_DIR/tmp/repack.sh`; mirrors the `create_initrd`
and `create_disk_image` functions in `scripts/build-all.sh`.)

---

## 8. Open questions / leads for next time

1. **Is `rax` actually `0` at the moment the child *consumes* it?** The frame
   has `rax=0` and the resume `rip` is correct, but no one has observed the
   child's `rax` in userspace at `0x21574a` directly. A single-stepping /
   hardware-watchpoint approach, or a kernel that logs the child's first
   userspace register state, would settle whether the corruption is
   pre-resume (frame/iretq) or post-resume (something async).

2. **Why does a parent-side delay fix it?** The `[FKDBG]` print sits in
   `fork_current` between building `child_ctx` and enqueuing the child. The
   delay shifts *when the child is first scheduled relative to the parent's
   continuation*. What shared state is order-dependent there? Candidates:
   copy-on-write of the **user stack** page (parent's post-fork stack writes vs
   the child's first stack reads), or the child's kernel-stack frame vs the
   parent still touching it. memtest exercises CoW isolation and passes, but
   with a much shorter post-fork code path than relibc's `fork()` tail.

3. **Why deterministic-fail with preemption OFF but flaky with it ON?** Under
   pure cooperative scheduling the child runs only when the parent yields (in
   `wait_pid`); that ordering apparently always hits the bug. Preemption
   sometimes interleaves them differently and occasionally "wins." Mapping the
   exact cooperative interleaving that corrupts the child would likely pinpoint
   the shared state in (2).

4. **`iretq` / `swapgs` / `fs_base` restore on the child's first entry.** The
   child's first return to userspace goes through `fork_ret_to_user`
   (`arch/x86_64/src/syscall.rs`), which pops the full `UserFrame` and `iretq`s
   after `swapgs`. `cpu_switch_to` restores the child's `fs_base` via
   `wrmsr`. Worth re-auditing this exact sequence for an ordering/`swapgs`
   subtlety that only bites when the child's post-syscall code (relibc's, which
   reads `%fs`) runs before something settles — even though the child-branch
   `%fs` read shouldn't affect the return value per the disassembly.

5. **Compare the two syscalls' surrounding codegen.** `leandros_libc`'s working
   `fork` is a tiny function that returns `rax` immediately; relibc's `fork()`
   is a larger function with a stack frame, a `%fs` prologue access, and the
   `e_raw`/`or_minus_one_errno` tail. The difference in *how long the child
   runs before consuming `rax`* — and what it touches (stack, `%fs`) — is the
   most likely axis. A deliberately-lengthened raw-clone wrapper (raw syscall
   followed by the same amount of stack/`%fs` churn as relibc's tail) would
   test whether "length of the post-syscall window" alone reproduces it.

---

## 9. Related

- `userland/forktest/src/main.rs` — the regression suite (green aarch64, red
  x86_64 as a deterministic repro).
- Kernel wait4 fix (independent, committed): `kernel/src/syscall.rs`
  (`sys_wait`, `encode_wait_status`), `sched/src/lib.rs` (`wait_pid`).
- `userland/memtest/src/main.rs` — raw-fork reference that works on x86_64.
- Memory: `project_waitpid_and_x86_fork`, `project_poll_epoll_phase9`
  (hazard re-diagnosed), `project_thread_management_rlct_clone`.
