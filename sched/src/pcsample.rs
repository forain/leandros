//! Timer-tick PC sampler: where every CPU is, 100 times a second.
//!
//! WHY IT EXISTS. "cosmic-comp's render thread is Running in 65 % of Ctrl-T
//! samples" says a thread is busy, not whether the time is ours (kernel) or
//! its own (userspace), nor where. Host-side RIP sampling needs the QEMU
//! monitor (`info registers`), which hangs QEMU under HVF and cannot name the
//! process behind a user PC. This samples from inside the guest instead: each
//! CPU's local timer interrupt records the interrupted PC, whether it was user
//! or kernel mode, the pid/tgid on that CPU and the syscall being serviced.
//!
//! Output: Ctrl-T (see `dump_tasks`) requests a drain; the BSP's next ticks
//! print the ring oldest-first, `DRAIN_PER_TICK` lines per tick, as
//! `[PCS] cpu tgid pid sc mode pc` (hex; `sc` fff = not in a syscall, mode
//! u/k/i = user / kernel / idle). Sampling pauses while a drain is running and
//! the ring is emptied by it, so each Ctrl-T reports the window since the last.
//! Symbolize user PCs with the PIE bias (0x200000) / the VMA dump; kernel PCs
//! against the kernel ELF.
//!
//! Compile-time gated (`ENABLED`), like `DRM_STATS`: off, both hooks are a
//! constant-false branch.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

pub const ENABLED: bool = false;

const RING: usize = 32768;
const DRAIN_PER_TICK: usize = 48;

static PC: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
/// `cpu:4 | user:1 | idle:1 | sc:12 | pid:20 | tgid:20` (low to high as listed
/// from bit 0: tgid 0..20, pid 20..40, sc 40..52, idle 52, user 53, cpu 54..58).
static META: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static DRAINING: AtomicBool = AtomicBool::new(false);
static DRAIN_POS: AtomicUsize = AtomicUsize::new(0);
static DRAIN_END: AtomicUsize = AtomicUsize::new(0);

/// Ask for the ring to be printed (Ctrl-T).
pub fn request_drain() {
    if !ENABLED || DRAINING.load(Relaxed) { return; }
    let head = HEAD.load(Relaxed);
    let start = head.saturating_sub(RING);
    DRAIN_POS.store(start, Relaxed);
    DRAIN_END.store(head, Relaxed);
    DRAINING.store(true, Relaxed);
}

/// Timer-IRQ hook. `pc` is the interrupted PC, `user` whether it was EL0/ring 3.
#[inline]
pub fn sample(pc: u64, user: bool) {
    if !ENABLED { return; }
    let cpu = unsafe { crate::cpu_id() }.min(crate::MAX_CPUS - 1);
    if DRAINING.load(Relaxed) {
        if cpu == 0 { drain_some(); }
        return;
    }
    let pid = crate::pid_on_cpu(cpu);
    let tgid = if pid == 0 { 0 } else {
        let t = crate::CURRENT_TGID[cpu].load(Relaxed);
        if t == 0 { pid } else { t }
    };
    let sc = crate::syscall_on_cpu(cpu);
    let sc = if sc == crate::NO_SYSCALL { 0xfff } else { (sc as u64) & 0xfff };
    let idle = pid == 0 && !user;
    let meta = (tgid as u64 & 0xfffff)
        | ((pid as u64 & 0xfffff) << 20)
        | (sc << 40)
        | ((idle as u64) << 52)
        | ((user as u64) << 53)
        | ((cpu as u64 & 0xf) << 54);
    let i = HEAD.fetch_add(1, Relaxed);
    PC[i % RING].store(pc, Relaxed);
    META[i % RING].store(meta, Relaxed);
}

fn drain_some() {
    extern "C" { fn arch_serial_putc_dump(c: u8); }
    fn s(x: &str) { for &b in x.as_bytes() { unsafe { arch_serial_putc_dump(b); } } }
    fn h(n: u64) {
        let d = b"0123456789abcdef";
        let mut started = false;
        for i in (0..16).rev() {
            let v = ((n >> (i * 4)) & 0xf) as usize;
            if v != 0 || started || i == 0 { started = true; unsafe { arch_serial_putc_dump(d[v]); } }
        }
    }
    let end = DRAIN_END.load(Relaxed);
    let mut pos = DRAIN_POS.load(Relaxed);
    let stop = (pos + DRAIN_PER_TICK).min(end);
    if pos == end.saturating_sub(RING) {
        s("[PCS] begin n="); h((end - pos) as u64); s("\n");
    }
    while pos < stop {
        let m = META[pos % RING].load(Relaxed);
        let pc = PC[pos % RING].load(Relaxed);
        s("[PCS] "); h((m >> 54) & 0xf);
        s(" "); h(m & 0xfffff);
        s(" "); h((m >> 20) & 0xfffff);
        s(" "); h((m >> 40) & 0xfff);
        s(if m & (1 << 53) != 0 { " u " } else if m & (1 << 52) != 0 { " i " } else { " k " });
        h(pc); s("\n");
        pos += 1;
    }
    DRAIN_POS.store(pos, Relaxed);
    if pos >= end {
        s("[PCS] end\n");
        HEAD.store(0, Relaxed);
        DRAINING.store(false, Relaxed);
    }
}
