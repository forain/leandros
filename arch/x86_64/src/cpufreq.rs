//! CPU performance-state setup (Intel HWP / EIST) and frequency reporting.
//!
//! Until this module the kernel never touched a P-state MSR, so a CPU ran at
//! whatever the firmware left it at: on the HP EliteBook 840 G5 (i5-8350U,
//! base 1.7 GHz, turbo 3.6 GHz) that measured as ~0.7-1.2 GHz. Per logical
//! CPU, on the BSP and on every AP's init path:
//!
//!   * **HWP** (CPUID.06H:EAX[7]): `IA32_PM_ENABLE` = 1, then
//!     `IA32_HWP_REQUEST` = {min = lowest, max = highest, desired = 0
//!     (autonomous), EPP}. EPP defaults to 0x80 (balanced), `cpu.perf=1`
//!     gives 0x40, `cpu.epp=<0..255>` sets it outright. On a part without the
//!     EPP field (CPUID.06H:EAX[10] clear) the bias goes to
//!     `IA32_ENERGY_PERF_BIAS` instead (0..15). Package-level requests
//!     (EAX[11]) are not used: the per-thread request leaves bit 42 clear, so
//!     `IA32_HWP_REQUEST_PKG` never governs and is left alone.
//!   * **EIST** otherwise (CPUID.01H:ECX[7]): `IA32_MISC_ENABLE` bit 16 on,
//!     and `IA32_PERF_CTL` = the 1-core turbo ratio (`MSR_TURBO_RATIO_LIMIT`
//!     low byte; `MSR_PLATFORM_INFO`[15:8] without turbo).
//!   * In both, a firmware "turbo disable" (`MISC_ENABLE` bit 38) is cleared
//!     when the bit takes the write (`cpu.turbo=0` keeps it set).
//!
//! Every MSR access goes through [`rdmsr_safe`]/[`wrmsr_safe`]: a #GP on a
//! CPU or hypervisor that does not implement the MSR resumes at a landing pad
//! (see [`msr_fixup`], called by the #GP path in `idt::fault_common`) instead
//! of halting the boot. Knobs: `cpu.pstate=0` touches nothing, `cpu.hwp=0`
//! skips HWP and uses the EIST path.
//!
//! Reporting: one `[cpufreq]` boot line per CPU, and `/proc/cpufreq` (built by
//! the VFS server through [`arch_cpufreq_report`]): per CPU the effective
//! frequency over the last ~1 s window from APERF/MPERF deltas × the TSC
//! frequency, the C0 residency (MPERF/TSC) and the `IA32_PERF_STATUS` ratio.
//! Each CPU samples its own counters from its timer tick ([`tick`]) at most
//! once a second; a CPU idle in a tickless sleep keeps an older sample, whose
//! age the file shows.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use super::cpus::{self, MAX_CPUS};

const IA32_MPERF: u32 = 0xE7;
const IA32_APERF: u32 = 0xE8;
const MSR_PLATFORM_INFO: u32 = 0xCE;
const IA32_PERF_STATUS: u32 = 0x198;
const IA32_PERF_CTL: u32 = 0x199;
const IA32_MISC_ENABLE: u32 = 0x1A0;
const MSR_TURBO_RATIO_LIMIT: u32 = 0x1AD;
const IA32_ENERGY_PERF_BIAS: u32 = 0x1B0;
const IA32_PM_ENABLE: u32 = 0x770;
const IA32_HWP_CAPABILITIES: u32 = 0x771;
const IA32_HWP_REQUEST: u32 = 0x774;

const MISC_EIST: u64 = 1 << 16;
const MISC_TURBO_DISABLE: u64 = 1 << 38;

// ── fault-tolerant MSR access ────────────────────────────────────────────────

core::arch::global_asm!(
    ".pushsection .text.msrsafe, \"ax\"",
    ".global __rdmsr_safe",
    ".global __wrmsr_safe",
    // edi = msr, rsi = *mut u64  ->  eax = 0 ok, 1 the rdmsr faulted.
    "__rdmsr_safe:",
    "    mov ecx, edi",
    "__rdmsr_safe_insn:",
    "    rdmsr",
    "    shl rdx, 32",
    "    or rax, rdx",
    "    mov [rsi], rax",
    "    xor eax, eax",
    "    ret",
    "__rdmsr_safe_fault:",
    "    mov eax, 1",
    "    ret",
    // edi = msr, rsi = value  ->  eax = 0 ok, 1 the wrmsr faulted.
    "__wrmsr_safe:",
    "    mov ecx, edi",
    "    mov eax, esi",
    "    mov rdx, rsi",
    "    shr rdx, 32",
    "__wrmsr_safe_insn:",
    "    wrmsr",
    "    xor eax, eax",
    "    ret",
    "__wrmsr_safe_fault:",
    "    mov eax, 1",
    "    ret",
    ".popsection",
);

extern "C" {
    fn __rdmsr_safe(msr: u32, out: *mut u64) -> u32;
    fn __wrmsr_safe(msr: u32, val: u64) -> u32;
    static __rdmsr_safe_insn: u8;
    static __rdmsr_safe_fault: u8;
    static __wrmsr_safe_insn: u8;
    static __wrmsr_safe_fault: u8;
}

/// Landing address for a kernel-mode #GP at `rip`, when `rip` is one of the
/// two fault-tolerant MSR instructions.
pub fn msr_fixup(rip: u64) -> Option<u64> {
    if rip == core::ptr::addr_of!(__rdmsr_safe_insn) as u64 {
        return Some(core::ptr::addr_of!(__rdmsr_safe_fault) as u64);
    }
    if rip == core::ptr::addr_of!(__wrmsr_safe_insn) as u64 {
        return Some(core::ptr::addr_of!(__wrmsr_safe_fault) as u64);
    }
    None
}

/// `rdmsr` that returns `None` instead of faulting on an unimplemented MSR.
/// Needs the IDT loaded on this CPU.
pub fn rdmsr_safe(msr: u32) -> Option<u64> {
    let mut v = 0u64;
    if unsafe { __rdmsr_safe(msr, &mut v) } == 0 { Some(v) } else { None }
}

/// `wrmsr` that returns false instead of faulting on a rejected write.
pub fn wrmsr_safe(msr: u32, val: u64) -> bool {
    unsafe { __wrmsr_safe(msr, val) == 0 }
}

// ── knobs ────────────────────────────────────────────────────────────────────

static PSTATE: AtomicBool = AtomicBool::new(true);
static USE_HWP: AtomicBool = AtomicBool::new(true);
static TURBO: AtomicBool = AtomicBool::new(true);
static EPP: AtomicU32 = AtomicU32::new(0x80);

/// Command-line knobs, from the kernel before `arch_x86_64::init`:
/// `cpu.pstate=0`, `cpu.hwp=0`, `cpu.turbo=0`, `cpu.perf=1`, `cpu.epp=N`
/// (`cpu.epp` wins over `cpu.perf`).
pub fn configure(pstate: Option<u64>, hwp: Option<u64>, turbo: Option<u64>,
                 perf: Option<u64>, epp: Option<u64>) {
    if pstate == Some(0) { PSTATE.store(false, Ordering::Relaxed); }
    if hwp == Some(0) { USE_HWP.store(false, Ordering::Relaxed); }
    if turbo == Some(0) { TURBO.store(false, Ordering::Relaxed); }
    if perf.unwrap_or(0) != 0 { EPP.store(0x40, Ordering::Relaxed); }
    if let Some(e) = epp { EPP.store(e.min(255) as u32, Ordering::Relaxed); }
}

// ── per-CPU state ────────────────────────────────────────────────────────────

const MODE_UNSET: u8 = 0;
const MODE_NONE: u8 = 1;
const MODE_HWP: u8 = 2;
const MODE_EIST: u8 = 3;
const MODE_OFF: u8 = 4;

struct CpuState {
    mode: AtomicU8,
    /// HWP: the raw `IA32_HWP_CAPABILITIES`. EIST: the same layout synthesised
    /// from PLATFORM_INFO / TURBO_RATIO_LIMIT (highest, guaranteed=base,
    /// efficient=lowest=min ratio).
    caps: AtomicU64,
    /// HWP: the `IA32_HWP_REQUEST` written; EIST: the `IA32_PERF_CTL` written.
    req: AtomicU64,
    // Sampling: written only by the owning CPU from its tick; `seq` odd while
    // a write is in progress.
    seq: AtomicU32,
    prev: [AtomicU64; 3],
    cur: [AtomicU64; 3],
    status: AtomicU64,
}

const Z: AtomicU64 = AtomicU64::new(0);
static STATE: [CpuState; MAX_CPUS] = [const { CpuState {
    mode: AtomicU8::new(MODE_UNSET),
    caps: AtomicU64::new(0),
    req: AtomicU64::new(0),
    seq: AtomicU32::new(0),
    prev: [Z; 3],
    cur: [Z; 3],
    status: AtomicU64::new(0),
} }; MAX_CPUS];

/// APERF/MPERF exist (CPUID.06H:ECX[0]) and sampling is allowed.
static SAMPLING: AtomicBool = AtomicBool::new(false);
/// PERF_STATUS readable (EIST or HWP was found).
static HAVE_STATUS: AtomicBool = AtomicBool::new(false);

fn cpuid(leaf: u32) -> core::arch::x86_64::CpuidResult {
    core::arch::x86_64::__cpuid_count(leaf, 0)
}

#[inline]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Bounded line writer over a stack buffer, for the boot line.
struct Line { buf: [u8; 256], n: usize }
impl Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        let k = b.len().min(self.buf.len() - self.n);
        self.buf[self.n..self.n + k].copy_from_slice(&b[..k]);
        self.n += k;
        Ok(())
    }
}

/// Clear a firmware turbo-disable, if allowed and if the bit takes the write.
/// Returns a short word for the boot line.
fn turbo_enable() -> &'static str {
    let Some(misc) = rdmsr_safe(IA32_MISC_ENABLE) else { return "?" };
    if misc & MISC_TURBO_DISABLE == 0 { return "on"; }
    if !TURBO.load(Ordering::Relaxed) { return "off(kept)"; }
    if wrmsr_safe(IA32_MISC_ENABLE, misc & !MISC_TURBO_DISABLE)
        && rdmsr_safe(IA32_MISC_ENABLE).map_or(false, |m| m & MISC_TURBO_DISABLE == 0) {
        "on(enabled)"
    } else {
        "off(locked)"
    }
}

/// Program this CPU's performance state and log one line. Called once per
/// logical CPU after its IDT is loaded (BSP: `arch_x86_64::init`; APs:
/// `smp::sched_ap_entry`).
pub fn init_cpu(cpu: usize) {
    let cpu = cpu.min(MAX_CPUS - 1);
    let st = &STATE[cpu];
    let mut l = Line { buf: [0; 256], n: 0 };
    let _ = write!(l, "[cpufreq] cpu{} ", cpu);

    let max_leaf = cpuid(0).eax;
    let l1 = cpuid(1);
    let l6 = if max_leaf >= 6 { cpuid(6) } else { core::arch::x86_64::CpuidResult { eax: 0, ebx: 0, ecx: 0, edx: 0 } };
    let has_hwp = l6.eax & (1 << 7) != 0;
    let has_epp = l6.eax & (1 << 10) != 0;
    let has_pkg = l6.eax & (1 << 11) != 0;
    let has_apm = l6.ecx & 1 != 0;
    let has_epb = l6.ecx & (1 << 3) != 0;
    let has_eist = l1.ecx & (1 << 7) != 0;
    let hv = l1.ecx & (1 << 31) != 0;

    if !PSTATE.load(Ordering::Relaxed) {
        st.mode.store(MODE_OFF, Ordering::Relaxed);
        let _ = write!(l, "off (cpu.pstate=0): P-state MSRs untouched");
    } else if has_hwp && USE_HWP.load(Ordering::Relaxed) {
        let turbo = turbo_enable();
        let pm_ok = rdmsr_safe(IA32_PM_ENABLE).map_or(false, |v| v & 1 != 0)
            || (wrmsr_safe(IA32_PM_ENABLE, 1)
                && rdmsr_safe(IA32_PM_ENABLE).map_or(false, |v| v & 1 != 0));
        match (pm_ok, rdmsr_safe(IA32_HWP_CAPABILITIES)) {
            (true, Some(caps)) => {
                let highest = caps & 0xFF;
                let lowest = (caps >> 24) & 0xFF;
                let epp = EPP.load(Ordering::Relaxed) as u64;
                let req = lowest | highest << 8 | (if has_epp { epp } else { 0 }) << 24;
                let wrote = wrmsr_safe(IA32_HWP_REQUEST, req);
                let mut bias = "";
                if !has_epp && has_epb {
                    if let Some(v) = rdmsr_safe(IA32_ENERGY_PERF_BIAS) {
                        let _ = wrmsr_safe(IA32_ENERGY_PERF_BIAS, (v & !0xF) | (epp * 15 / 255));
                        bias = " (EPP via ENERGY_PERF_BIAS)";
                    }
                }
                st.caps.store(caps, Ordering::Relaxed);
                st.req.store(rdmsr_safe(IA32_HWP_REQUEST).unwrap_or(req), Ordering::Relaxed);
                st.mode.store(MODE_HWP, Ordering::Relaxed);
                HAVE_STATUS.store(true, Ordering::Relaxed);
                let _ = write!(l, "HWP lowest={} efficient={} guaranteed={} highest={} -> req=0x{:x}{} epp={}{}{} turbo={}",
                    lowest, (caps >> 16) & 0xFF, (caps >> 8) & 0xFF, highest,
                    req, if wrote { "" } else { " (WRITE FAILED)" },
                    epp, if has_epp { "" } else { "(no EPP field)" }, bias, turbo);
                if has_pkg { let _ = write!(l, " pkg-req-capable"); }
            }
            (pm, c) => {
                st.mode.store(MODE_NONE, Ordering::Relaxed);
                let _ = write!(l, "none: HWP in CPUID but {}",
                    if !pm { "IA32_PM_ENABLE would not set" } else if c.is_none() { "HWP_CAPABILITIES unreadable" } else { "?" });
            }
        }
    } else if has_eist {
        let turbo = turbo_enable();
        let misc_ok = match rdmsr_safe(IA32_MISC_ENABLE) {
            Some(m) if m & MISC_EIST != 0 => true,
            Some(m) => wrmsr_safe(IA32_MISC_ENABLE, m | MISC_EIST),
            None => false,
        };
        let plat = rdmsr_safe(MSR_PLATFORM_INFO).unwrap_or(0);
        let base = (plat >> 8) & 0xFF;
        let min = (plat >> 40) & 0xFF;
        let turbo_on = rdmsr_safe(IA32_MISC_ENABLE).map_or(false, |m| m & MISC_TURBO_DISABLE == 0)
            && cpuid(6).eax & (1 << 1) != 0;
        let t1 = if turbo_on { rdmsr_safe(MSR_TURBO_RATIO_LIMIT).map_or(0, |v| v & 0xFF) } else { 0 };
        let target = if t1 >= base && t1 != 0 { t1 } else { base };
        if target == 0 {
            st.mode.store(MODE_NONE, Ordering::Relaxed);
            let _ = write!(l, "none: EIST in CPUID but no ratio (PLATFORM_INFO unreadable)");
        } else {
            let ctl = target << 8;
            let wrote = wrmsr_safe(IA32_PERF_CTL, ctl);
            st.caps.store(target | base << 8 | min << 16 | min << 24, Ordering::Relaxed);
            st.req.store(ctl, Ordering::Relaxed);
            st.mode.store(MODE_EIST, Ordering::Relaxed);
            HAVE_STATUS.store(true, Ordering::Relaxed);
            let status = rdmsr_safe(IA32_PERF_STATUS).map_or(0, |v| (v >> 8) & 0xFF);
            let _ = write!(l, "EIST{} min={} base={} max={} -> PERF_CTL=0x{:x}{} status={} turbo={}{}",
                if misc_ok { "" } else { " (MISC_ENABLE.EIST not set)" },
                min, base, target, ctl, if wrote { "" } else { " (WRITE FAILED)" }, status, turbo,
                if has_hwp { " (HWP available, cpu.hwp=0)" } else { "" });
        }
    } else {
        st.mode.store(MODE_NONE, Ordering::Relaxed);
        let _ = write!(l, "none: no HWP/EIST in CPUID{}", if hv { " (hypervisor)" } else { "" });
    }

    if has_apm && PSTATE.load(Ordering::Relaxed) {
        // Prime the window so the first /proc read after ~1 s has a delta.
        if let (Some(a), Some(m)) = (rdmsr_safe(IA32_APERF), rdmsr_safe(IA32_MPERF)) {
            let t = rdtsc();
            for (slot, v) in st.cur.iter().zip([t, a, m]) { slot.store(v, Ordering::Relaxed); }
            SAMPLING.store(true, Ordering::Relaxed);
        }
    }
    let _ = write!(l, " aperfmperf={}\n", if has_apm { "yes" } else { "no" });
    if let Ok(s) = core::str::from_utf8(&l.buf[..l.n]) { cpus::log(s); }
}

/// Timer-tick hook: refresh this CPU's APERF/MPERF window at most once per
/// second. IRQ context; a few `rdmsr`s once a second.
pub fn tick(cpu: usize) {
    if !SAMPLING.load(Ordering::Relaxed) || cpu >= MAX_CPUS { return; }
    let st = &STATE[cpu];
    let last = st.cur[0].load(Ordering::Relaxed);
    if last == 0 { return; } // this CPU was never primed (no APERF here)
    let khz = super::timer::tsc_khz();
    let now = rdtsc();
    if khz == 0 || now.wrapping_sub(last) < khz * 1000 { return; }
    let (Some(a), Some(m)) = (rdmsr_safe(IA32_APERF), rdmsr_safe(IA32_MPERF)) else { return };
    let status = if HAVE_STATUS.load(Ordering::Relaxed) { rdmsr_safe(IA32_PERF_STATUS).unwrap_or(0) } else { 0 };
    st.seq.fetch_add(1, Ordering::AcqRel);
    for i in 0..3 { st.prev[i].store(st.cur[i].load(Ordering::Relaxed), Ordering::Relaxed); }
    for (slot, v) in st.cur.iter().zip([now, a, m]) { slot.store(v, Ordering::Relaxed); }
    st.status.store(status, Ordering::Relaxed);
    st.seq.fetch_add(1, Ordering::AcqRel);
}

struct Buf<'a> { b: &'a mut [u8], n: usize }
impl Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let s = s.as_bytes();
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s[..k]);
        self.n += k;
        Ok(())
    }
}

/// `/proc/cpufreq` body into `buf`; returns the length written.
pub fn report(buf: &mut [u8]) -> usize {
    let mut w = Buf { b: buf, n: 0 };
    let khz = super::timer::tsc_khz();
    let now = rdtsc();
    let _ = writeln!(w, "tsc_mhz {}  epp {}  sampling {}",
        khz / 1000, EPP.load(Ordering::Relaxed),
        if SAMPLING.load(Ordering::Relaxed) { "aperf/mperf ~1s window" } else { "unavailable" });
    let _ = writeln!(w, "cpu mode lowest efficient guaranteed highest request      eff_mhz busy% status_mhz age_ms");
    let n = super::smp::active_cpu_count().clamp(1, MAX_CPUS);
    for cpu in 0..n {
        let st = &STATE[cpu];
        let mode = match st.mode.load(Ordering::Relaxed) {
            MODE_HWP => "hwp", MODE_EIST => "eist", MODE_OFF => "off", MODE_NONE => "none", _ => "-",
        };
        let caps = st.caps.load(Ordering::Relaxed);
        let _ = write!(w, "{:<3} {:<4} {:>6} {:>9} {:>10} {:>7} 0x{:<10x}",
            cpu, mode, (caps >> 24) & 0xFF, (caps >> 16) & 0xFF, (caps >> 8) & 0xFF, caps & 0xFF,
            st.req.load(Ordering::Relaxed));
        // Consistent snapshot of the two samples.
        let mut snap = None;
        for _ in 0..8 {
            let s0 = st.seq.load(Ordering::Acquire);
            if s0 & 1 != 0 { core::hint::spin_loop(); continue; }
            let p: [u64; 3] = core::array::from_fn(|i| st.prev[i].load(Ordering::Relaxed));
            let c: [u64; 3] = core::array::from_fn(|i| st.cur[i].load(Ordering::Relaxed));
            let s = st.status.load(Ordering::Relaxed);
            if st.seq.load(Ordering::Acquire) == s0 { snap = Some((p, c, s)); break; }
        }
        match snap {
            Some((p, c, status)) if p[0] != 0 && c[0] > p[0] && khz != 0 => {
                let dt = c[0] - p[0];
                let da = c[1].wrapping_sub(p[1]);
                let dm = c[2].wrapping_sub(p[2]);
                let eff = if dm != 0 { (da as u128 * khz as u128 / dm as u128 / 1000) as u64 } else { 0 };
                let busy = (dm as u128 * 100 / dt as u128).min(100) as u64;
                let age = now.wrapping_sub(c[0]) / khz.max(1);
                let _ = writeln!(w, " {:>7} {:>5} {:>10} {:>6}", eff, busy, ((status >> 8) & 0xFF) * 100, age);
            }
            _ => { let _ = writeln!(w, " {:>7} {:>5} {:>10} {:>6}", "-", "-", "-", "-"); }
        }
    }
    w.n
}

/// `/proc/cpufreq` for the VFS server (below this crate in the graph).
#[no_mangle]
pub extern "C" fn arch_cpufreq_report(buf: *mut u8, cap: usize) -> usize {
    if buf.is_null() || cap == 0 { return 0; }
    report(unsafe { core::slice::from_raw_parts_mut(buf, cap) })
}
