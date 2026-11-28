//! kvmtest -- /dev/kvm API, memslot, vCPU and exit-path test (kvm-plan section 5).
//!
//!   /bin/kvmtest [phase...|all]
//!
//! Phases: open caps memslot regs reglist mmio psci wfi sysoff immexit sigkick smp
//!         vgicv2 sigstress unmap forkvm
//! Output: `KVMTEST <phase> PASS|FAIL|SKIP detail`, then
//! `KVMTEST summary pass=N fail=N skip=N`. The exit status is the FAIL count.
//! Without /dev/kvm (x86_64, hosts without EL2) it prints `SKIP no /dev/kvm`
//! and exits 0. Guest code: blob.rs (hand-assembled A64, MMU off, GPA 0x4000_0000).

#![no_std]
#![no_main]
#![feature(c_variadic)]
#![allow(non_camel_case_types, dead_code, static_mut_refs)]

#[path = "../../muslrt/rt.rs"]
mod rt;
mod blob;

use core::ffi::c_void;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};

type c_int = i32;

extern "C" {
    fn open(path: *const u8, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn ioctl(fd: c_int, req: usize, ...) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn write(fd: c_int, buf: *const u8, n: usize) -> isize;
    fn _exit(status: c_int) -> !;
    fn usleep(us: u32) -> c_int;
    fn alarm(s: u32) -> u32;
    fn signal(sig: c_int, handler: usize) -> usize;
    fn clock_gettime(clk: c_int, ts: *mut [i64; 2]) -> c_int;
    fn pthread_self() -> usize;
    fn pthread_kill(t: usize, sig: c_int) -> c_int;
    fn pthread_create(t: *mut usize, attr: *const c_void, f: extern "C" fn(*mut c_void) -> *mut c_void, arg: *mut c_void) -> c_int;
    fn pthread_join(t: usize, ret: *mut *mut c_void) -> c_int;
    fn __errno_location() -> *mut c_int;
    fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
    fn fork() -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, opts: c_int) -> c_int;
}

#[panic_handler]
fn panic(_i: &core::panic::PanicInfo) -> ! { unsafe { _exit(134) } }

// ---- KVM UAPI (aarch64) ----------------------------------------------------------------
const KVM_GET_API_VERSION: usize = 0xAE00;
const KVM_CREATE_VM: usize = 0xAE01;
const KVM_CHECK_EXTENSION: usize = 0xAE03;
const KVM_GET_VCPU_MMAP_SIZE: usize = 0xAE04;
const KVM_SET_USER_MEMORY_REGION: usize = 0x4020_AE46;
const KVM_CREATE_VCPU: usize = 0xAE41;
const KVM_ARM_PREFERRED_TARGET: usize = 0x8020_AEAF;
const KVM_ARM_VCPU_INIT: usize = 0x4020_AEAE;
const KVM_GET_ONE_REG: usize = 0x4010_AEAB;
const KVM_SET_ONE_REG: usize = 0x4010_AEAC;
const KVM_GET_REG_LIST: usize = 0xC008_AEB0;
const KVM_RUN: usize = 0xAE80;
const KVM_CREATE_DEVICE: usize = 0xC00C_AEE0;
const KVM_CREATE_DEVICE_TEST: u32 = 1;
const KVM_DEV_TYPE_ARM_VGIC_V2: u32 = 5;
const KVM_DEV_TYPE_ARM_VGIC_V3: u32 = 7;
const ENODEV: c_int = 19;

const EXIT_MMIO: u32 = 6;
const EXIT_INTR: u32 = 10;
const EXIT_SYSTEM_EVENT: u32 = 24;
const SYSTEM_EVENT_SHUTDOWN: u32 = 1;

// kvm_run offsets
const R_IMMEDIATE_EXIT: usize = 1;
const R_EXIT_REASON: usize = 8;
const R_MMIO_ADDR: usize = 32;
const R_MMIO_DATA: usize = 40;
const R_MMIO_LEN: usize = 48;
const R_MMIO_IS_WRITE: usize = 52;
const R_SE_TYPE: usize = 32;
const R_DEVICE_IRQ_LEVEL: usize = 304;

// register ids: KVM_REG_ARM64 | size | class | index
const REG_U32: u64 = 0x6020_0000_0000_0000;
const REG_U64: u64 = 0x6030_0000_0000_0000;
const REG_U128: u64 = 0x6040_0000_0000_0000;
const CORE: u64 = 0x0010 << 16;
const fn core_x(n: u64) -> u64 { REG_U64 | CORE | (n * 2) }
const REG_SP: u64 = REG_U64 | CORE | 0x3e;
const REG_PC: u64 = REG_U64 | CORE | 0x40;
const REG_PSTATE: u64 = REG_U64 | CORE | 0x42;
const REG_V0: u64 = REG_U128 | CORE | 0x54;
const REG_FPSR: u64 = REG_U32 | CORE | 0xd4;
const REG_FPCR: u64 = REG_U32 | CORE | 0xd5;
const REG_PSCI_VERSION: u64 = REG_U64 | (0x0014 << 16);
// timer count: (3,3,14,3,2) -- the UAPI swaps CNT and CVAL; volatile, so not compared
const REG_TIMER_CNT: u64 = REG_U64 | (0x0013 << 16) | (3 << 14) | (3 << 11) | (14 << 7) | (3 << 3) | 2;

const RAM_GPA: u64 = 0x4000_0000;
const RAM_SIZE: usize = 0x10000;
const MMIO_GPA: u64 = 0x0900_0000;
const PSTATE_EL1H: u64 = 0x3c5;
const EINTR: c_int = 4;
const E2BIG: c_int = 7;
const EINVAL: c_int = 22;
const SIGUSR1: c_int = 10;
const SIGALRM: c_int = 14;
const PHASE_TIMEOUT_S: u32 = 120; // TCG-in-TCG is slow

// ---- output ------------------------------------------------------------------------------
static mut PASS: u32 = 0;
static mut FAIL: u32 = 0;
static mut SKIP: u32 = 0;
static mut CUR: &[u8] = b"?";

struct Line { b: [u8; 240], n: usize }
impl Line {
    fn new() -> Line { Line { b: [0; 240], n: 0 } }
    fn s(&mut self, t: &[u8]) -> &mut Line {
        for &c in t { if self.n < 238 { self.b[self.n] = c; self.n += 1; } }
        self
    }
    fn d(&mut self, mut v: u64) -> &mut Line {
        let mut t = [0u8; 20]; let mut i = 20;
        loop { i -= 1; t[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
        self.s(&t[i..])
    }
    fn x(&mut self, v: u64) -> &mut Line {
        let mut t = [0u8; 18]; t[0] = b'0'; t[1] = b'x';
        for i in 0..16 { t[2 + i] = b"0123456789abcdef"[((v >> (60 - 4 * i)) & 15) as usize]; }
        self.s(&t)
    }
    fn flush(&mut self) { self.s(b"\n"); unsafe { write(1, self.b.as_ptr(), self.n); } }
}

unsafe fn pass(name: &[u8], detail: &[u8]) -> bool {
    PASS += 1;
    Line::new().s(b"KVMTEST ").s(name).s(b" PASS ").s(detail).flush();
    true
}
unsafe fn fail(name: &[u8], what: &[u8], v: u64) -> bool {
    FAIL += 1;
    Line::new().s(b"KVMTEST ").s(name).s(b" FAIL ").s(what).s(b"=").d(v).s(b" (").x(v).s(b")").flush();
    false
}
unsafe fn skip(name: &[u8], why: &[u8]) -> bool {
    SKIP += 1;
    Line::new().s(b"KVMTEST ").s(name).s(b" SKIP ").s(why).flush();
    true
}

extern "C" fn on_alarm(_s: c_int) {
    unsafe {
        Line::new().s(b"KVMTEST ").s(CUR).s(b" FAIL timeout (guest or kernel hung)").flush();
        _exit(3);
    }
}
extern "C" fn on_usr1(_s: c_int) {}

unsafe fn errno() -> c_int { *__errno_location() }

// ---- raw accessors -----------------------------------------------------------------------
unsafe fn rd32(run: *mut u8, off: usize) -> u32 { (run.add(off) as *const u32).read_volatile() }
unsafe fn rd64(run: *mut u8, off: usize) -> u64 { (run.add(off) as *const u64).read_volatile() }
unsafe fn wr64(run: *mut u8, off: usize, v: u64) { (run.add(off) as *mut u64).write_volatile(v) }
unsafe fn rd8(run: *mut u8, off: usize) -> u8 { run.add(off).read_volatile() }
unsafe fn wr8(run: *mut u8, off: usize, v: u8) { run.add(off).write_volatile(v) }

#[repr(C)]
struct OneReg { id: u64, addr: u64 }
#[repr(C)]
struct MemRegion { slot: u32, flags: u32, gpa: u64, size: u64, hva: u64 }
#[repr(C)]
struct VcpuInit { target: u32, features: [u32; 7] }

unsafe fn get_one(vcpu: c_int, id: u64, buf: *mut u8) -> c_int {
    let r = OneReg { id, addr: buf as u64 };
    ioctl(vcpu, KVM_GET_ONE_REG, &r as *const OneReg)
}
unsafe fn set_one(vcpu: c_int, id: u64, buf: *const u8) -> c_int {
    let r = OneReg { id, addr: buf as u64 };
    ioctl(vcpu, KVM_SET_ONE_REG, &r as *const OneReg)
}
unsafe fn get_u64(vcpu: c_int, id: u64) -> Option<u64> {
    let mut v = 0u64;
    if get_one(vcpu, id, &mut v as *mut u64 as *mut u8) == 0 { Some(v) } else { None }
}
unsafe fn set_u64(vcpu: c_int, id: u64, v: u64) -> bool {
    set_one(vcpu, id, &v as *const u64 as *const u8) == 0
}
unsafe fn now_ns() -> u64 {
    let mut ts = [0i64; 2];
    clock_gettime(1, &mut ts);
    ts[0] as u64 * 1_000_000_000 + ts[1] as u64
}

// ---- VM construction -----------------------------------------------------------------------
struct Ctx { kvm: c_int, vm: c_int, vcpu: c_int, run: *mut u8, ram: *mut u8, run_len: usize }

unsafe fn open_kvm() -> c_int { open(b"/dev/kvm\0".as_ptr(), 2 /* O_RDWR */) }

unsafe fn vcpu_create(vm: c_int, id: usize, run_len: usize, power_off: bool) -> Option<(c_int, *mut u8)> {
    let vcpu = ioctl(vm, KVM_CREATE_VCPU, id);
    if vcpu < 0 { return None; }
    let mut init = VcpuInit { target: 0, features: [0; 7] };
    if ioctl(vm, KVM_ARM_PREFERRED_TARGET, &mut init as *mut VcpuInit) != 0 { return None; }
    init.features[0] |= 1 << 2; // KVM_ARM_VCPU_PSCI_0_2
    if power_off { init.features[0] |= 1 << 0; } // KVM_ARM_VCPU_POWER_OFF
    if ioctl(vcpu, KVM_ARM_VCPU_INIT, &init as *const VcpuInit) != 0 { return None; }
    let run = mmap(null_mut(), run_len, 3, 1 /* MAP_SHARED */, vcpu, 0);
    if run as isize == -1 { return None; }
    Some((vcpu, run as *mut u8))
}

/// VM with 64 KiB of RAM at RAM_GPA (slot 0) holding `code`, vCPU 0 at pc=RAM_GPA, PSTATE=EL1h.
unsafe fn setup(name: &[u8], kvm: c_int, code: &[u32]) -> Option<Ctx> {
    let vm = ioctl(kvm, KVM_CREATE_VM, 0usize);
    if vm < 0 { fail(name, b"CREATE_VM errno", errno() as u64); return None; }
    let ram = mmap(null_mut(), RAM_SIZE, 3, 0x22 /* MAP_PRIVATE|MAP_ANONYMOUS */, -1, 0);
    if ram as isize == -1 { fail(name, b"mmap ram errno", errno() as u64); return None; }
    let ram = ram as *mut u8;
    for (i, w) in code.iter().enumerate() { (ram as *mut u32).add(i).write_volatile(*w); }
    let rg = MemRegion { slot: 0, flags: 0, gpa: RAM_GPA, size: RAM_SIZE as u64, hva: ram as u64 };
    if ioctl(vm, KVM_SET_USER_MEMORY_REGION, &rg as *const MemRegion) != 0 {
        fail(name, b"SET_USER_MEMORY_REGION errno", errno() as u64); return None;
    }
    let run_len = ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0usize);
    if run_len < 4096 { fail(name, b"GET_VCPU_MMAP_SIZE", run_len as u64); return None; }
    let Some((vcpu, run)) = vcpu_create(vm, 0, run_len as usize, false) else {
        fail(name, b"vcpu create/init/mmap errno", errno() as u64); return None;
    };
    if !set_u64(vcpu, REG_PC, RAM_GPA) || !set_u64(vcpu, REG_PSTATE, PSTATE_EL1H) {
        fail(name, b"set pc/pstate errno", errno() as u64); return None;
    }
    Some(Ctx { kvm, vm, vcpu, run, ram, run_len: run_len as usize })
}

/// KVM_RUN; returns (rc, errno).
unsafe fn vrun(vcpu: c_int) -> (c_int, c_int) {
    let rc = ioctl(vcpu, KVM_RUN, 0usize);
    (rc, if rc < 0 { errno() } else { 0 })
}

/// Run until an exit; checks it is an MMIO exit at `addr`. Returns the written value (write) or 0.
unsafe fn expect_mmio(name: &[u8], c: &Ctx, step: &[u8], addr: u64, write_: bool, len: u32) -> Option<u64> {
    let (rc, e) = vrun(c.vcpu);
    let r = rd32(c.run, R_EXIT_REASON);
    let mut l = Line::new();
    if rc != 0 { l.s(step).s(b": KVM_RUN rc=").d(rc as u64).s(b" errno=").d(e as u64).s(b" exit_reason=").d(r as u64);
        FAIL += 1; Line::new().s(b"KVMTEST ").s(name).s(b" FAIL ").s(&l.b[..l.n]).flush(); return None; }
    if r != EXIT_MMIO { fail(name, b"exit_reason (want 6=MMIO)", r as u64); return None; }
    if rd64(c.run, R_MMIO_ADDR) != addr { fail(name, b"mmio phys_addr", rd64(c.run, R_MMIO_ADDR)); return None; }
    if rd32(c.run, R_MMIO_LEN) != len { fail(name, b"mmio len", rd32(c.run, R_MMIO_LEN) as u64); return None; }
    if (rd8(c.run, R_MMIO_IS_WRITE) != 0) != write_ { fail(name, b"mmio is_write", rd8(c.run, R_MMIO_IS_WRITE) as u64); return None; }
    let mask = if len >= 8 { !0u64 } else { (1u64 << (len * 8)) - 1 };
    Some(if write_ { rd64(c.run, R_MMIO_DATA) & mask } else { 0 })
}

unsafe fn want(name: &[u8], what: &[u8], got: u64, expect: u64) -> bool {
    if got == expect { true } else { fail(name, what, got); false }
}

// ---- phases ----------------------------------------------------------------------------------
unsafe fn ph_open() -> bool {
    let n = b"open";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open /dev/kvm errno", errno() as u64); }
    let v = ioctl(kvm, KVM_GET_API_VERSION, 0usize);
    if !want(n, b"KVM_GET_API_VERSION", v as u64, 12) { return false; }
    let sz = ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0usize);
    if !want(n, b"KVM_GET_VCPU_MMAP_SIZE", sz as u64, 4096) { return false; }
    // IPA size: bits 7:0 of the arg; 0 = default (40); below 32 is invalid
    let bad = ioctl(kvm, KVM_CREATE_VM, 31usize);
    if bad >= 0 || errno() != EINVAL { return fail(n, b"CREATE_VM(ipa=31) must be EINVAL, rc", bad as u64); }
    let vm = ioctl(kvm, KVM_CREATE_VM, 0usize);
    if vm < 0 { return fail(n, b"CREATE_VM(0) errno", errno() as u64); }
    // KVM_CHECK_EXTENSION works on the VM fd too
    if ioctl(vm, KVM_CHECK_EXTENSION, 3usize) <= 0 { return fail(n, b"vm-fd CHECK_EXTENSION(USER_MEMORY)", 0); }
    close(vm); close(kvm);
    pass(n, b"api=12 mmap_size=4096 create_vm ok")
}

unsafe fn ph_caps() -> bool {
    let n = b"caps";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    // [R] = QEMU hard-requires it; the rest are advertised by the plan (section 2)
    let nonzero: [(usize, &[u8]); 17] = [
        (3, b"USER_MEMORY"), (21, b"DESTROY_MEMORY_REGION_WORKS"), (30, b"JOIN_MEMORY_REGIONS_WORKS"),
        (40, b"INTERNAL_ERROR_DATA"), (36, b"IOEVENTFD"), (122, b"IOEVENTFD_ANY_LENGTH"), (89, b"DEVICE_CTRL"),
        (165, b"ARM_VM_IPA_SIZE"), (9, b"NR_VCPUS"), (66, b"MAX_VCPUS"), (10, b"NR_MEMSLOTS"),
        (136, b"IMMEDIATE_EXIT"), (70, b"ONE_REG"), (87, b"ARM_PSCI"), (102, b"ARM_PSCI_0_2"), (144, b"ARM_USER_IRQ"), (81, b"READONLY_MEM"),
    ];
    let mut ok = true;
    for (id, nm) in nonzero.iter() {
        let v = ioctl(kvm, KVM_CHECK_EXTENSION, *id);
        if v <= 0 { Line::new().s(b"KVMTEST caps FAIL ").s(nm).s(b" (").d(*id as u64).s(b") = ").d(v as u64).flush(); FAIL += 1; ok = false; }
    }
    let ipa = ioctl(kvm, KVM_CHECK_EXTENSION, 165usize);
    if !(32..=48).contains(&ipa) { ok = fail(n, b"ARM_VM_IPA_SIZE out of range", ipa as u64) && ok; }
    let slots = ioctl(kvm, KVM_CHECK_EXTENSION, 10usize);
    if slots != 32 { ok = fail(n, b"NR_MEMSLOTS (plan: 32)", slots as u64) && ok; }
    // reported as 0 on purpose: no sync-mmu (RAM discard stays off), no irq routing, no SVE/PMU/MTE/...
    let zero: [(usize, &[u8]); 8] = [
        (16, b"SYNC_MMU"), (25, b"IRQ_ROUTING"), (15, b"COALESCED_MMIO"), (126, b"ARM_PMU_V3"),
        (170, b"ARM_SVE"), (205, b"ARM_MTE"), (231, b"USER_MEMORY2"), (234, b"GUEST_MEMFD"),
    ];
    for (id, nm) in zero.iter() {
        let v = ioctl(kvm, KVM_CHECK_EXTENSION, *id);
        if v != 0 { Line::new().s(b"KVMTEST caps FAIL ").s(nm).s(b" must be 0, got ").d(v as u64).flush(); FAIL += 1; ok = false; }
    }
    if ioctl(kvm, KVM_CHECK_EXTENSION, 9999usize) != 0 { ok = fail(n, b"unknown cap 9999 must be 0", 0) && ok; }
    // informational: IRQCHIP(0) and IRQFD(32) appear with the in-kernel vGIC
    let (ic, irqfd) = (ioctl(kvm, KVM_CHECK_EXTENSION, 0usize), ioctl(kvm, KVM_CHECK_EXTENSION, 32usize));
    close(kvm);
    if ok { Line::new().s(b"KVMTEST caps PASS ipa_bits=").d(ipa as u64).s(b" irqchip=").d(ic as u64).s(b" irqfd=").d(irqfd as u64).flush(); PASS += 1; }
    ok
}

unsafe fn ph_memslot() -> bool {
    let n = b"memslot";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::MEMSLOT) else { return false; };
    // slot validation: bad slot id, unaligned gpa, unaligned hva
    let bad_slot = MemRegion { slot: 32, flags: 0, gpa: 0x8000_0000, size: 0x1000, hva: c.ram as u64 };
    if ioctl(c.vm, KVM_SET_USER_MEMORY_REGION, &bad_slot as *const MemRegion) >= 0 { return fail(n, b"slot 32 accepted", 32); }
    let bad_gpa = MemRegion { slot: 1, flags: 0, gpa: 0x8000_0800, size: 0x1000, hva: c.ram as u64 };
    if ioctl(c.vm, KVM_SET_USER_MEMORY_REGION, &bad_gpa as *const MemRegion) >= 0 { return fail(n, b"unaligned gpa accepted", 0); }
    let bad_hva = MemRegion { slot: 1, flags: 0, gpa: 0x8000_0000, size: 0x1000, hva: c.ram as u64 + 0x800 };
    if ioctl(c.vm, KVM_SET_USER_MEMORY_REGION, &bad_hva as *const MemRegion) >= 0 { return fail(n, b"unaligned hva accepted", 0); }
    // a second, valid slot and its deletion (size 0)
    let s1 = MemRegion { slot: 1, flags: 0, gpa: 0x8000_0000, size: 0x1000, hva: c.ram as u64 + 0x2000 };
    if ioctl(c.vm, KVM_SET_USER_MEMORY_REGION, &s1 as *const MemRegion) != 0 { return fail(n, b"slot 1 create errno", errno() as u64); }
    let s1d = MemRegion { slot: 1, flags: 0, gpa: 0x8000_0000, size: 0, hva: 0 };
    if ioctl(c.vm, KVM_SET_USER_MEMORY_REGION, &s1d as *const MemRegion) != 0 { return fail(n, b"slot 1 delete errno", errno() as u64); }
    // the guest stores to RAM; the host reads it back through the HVA
    let (rc, e) = vrun(c.vcpu);
    if rc != 0 { return fail(n, b"KVM_RUN errno", e as u64); }
    if !want(n, b"exit_reason (want 24=SYSTEM_EVENT)", rd32(c.run, R_EXIT_REASON) as u64, EXIT_SYSTEM_EVENT as u64) { return false; }
    let v = (c.ram.add(0x1000) as *const u32).read_volatile();
    if !want(n, b"guest store visible at hva+0x1000", v as u64, 0xbabe_cafe) { return false; }
    // TODO(open): munmap of the RAM while the VM lives, then re-enter: the next guest access must exit or
    // fault, never read a stale frame. Needs a second blob run; left for kvm-dev.
    pass(n, b"slot checks ok, guest store 0xbabecafe seen through hva")
}

unsafe fn ph_regs() -> bool {
    let n = b"regs";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SPIN) else { return false; };
    let pat = 0x1122_3344_5566_7788u64;
    for id in [core_x(0), core_x(5), core_x(30), REG_SP] {
        if !set_u64(c.vcpu, id, pat ^ id) { return fail(n, b"set x/sp errno", errno() as u64); }
        match get_u64(c.vcpu, id) { Some(v) if v == pat ^ id => {} Some(v) => return fail(n, b"x/sp readback", v), None => return fail(n, b"get x/sp errno", errno() as u64) }
    }
    if get_u64(c.vcpu, REG_PC) != Some(RAM_GPA) { return fail(n, b"pc readback", get_u64(c.vcpu, REG_PC).unwrap_or(0)); }
    if get_u64(c.vcpu, REG_PSTATE) != Some(PSTATE_EL1H) { return fail(n, b"pstate readback", get_u64(c.vcpu, REG_PSTATE).unwrap_or(0)); }
    if !set_u64(c.vcpu, REG_PC, RAM_GPA + 4) || get_u64(c.vcpu, REG_PC) != Some(RAM_GPA + 4) { return fail(n, b"pc set/get", 0); }
    // 128-bit vector register
    let mut v = [0u8; 16]; let mut w = [0u8; 16];
    for i in 0..16 { v[i] = (0xa0 + i) as u8; }
    if set_one(c.vcpu, REG_V0, v.as_ptr()) != 0 { return fail(n, b"set v0 errno", errno() as u64); }
    if get_one(c.vcpu, REG_V0, w.as_mut_ptr()) != 0 || v != w { return fail(n, b"v0 readback", w[0] as u64); }
    // 32-bit fpsr / fpcr (only the low 32 bits of the buffer are defined)
    for id in [REG_FPSR, REG_FPCR] {
        let mut a = 0x0800_0000u32; let mut b = 0u32;
        if id == REG_FPSR { a = 0x0800_0010; }
        if set_one(c.vcpu, id, &a as *const u32 as *const u8) != 0 { return fail(n, b"set fpsr/fpcr errno", errno() as u64); }
        if get_one(c.vcpu, id, &mut b as *mut u32 as *mut u8) != 0 || a != b { return fail(n, b"fpsr/fpcr readback", b as u64); }
    }
    // firmware pseudo-register: PSCI 1.0
    if get_u64(c.vcpu, REG_PSCI_VERSION) != Some(0x10000) { return fail(n, b"PSCI_VERSION fw reg", get_u64(c.vcpu, REG_PSCI_VERSION).unwrap_or(0)); }
    // an id nobody implements must fail
    if get_u64(c.vcpu, REG_U64 | CORE | 0x9999).is_some() { return fail(n, b"bogus reg id accepted", 0); }
    pass(n, b"x0/x5/x30/sp/pc/pstate/v0/fpsr/fpcr/PSCI_VERSION round trip")
}

static mut REGBUF: [u64; 520] = [0; 520];
unsafe fn ph_reglist() -> bool {
    let n = b"reglist";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SPIN) else { return false; };
    // too small: -E2BIG and the count in .n
    REGBUF[0] = 0;
    let rc = ioctl(c.vcpu, KVM_GET_REG_LIST, REGBUF.as_mut_ptr());
    if rc >= 0 || errno() != E2BIG { return fail(n, b"REG_LIST(n=0) must be E2BIG, rc", rc as u64); }
    let count = REGBUF[0] as usize;
    if count < 20 || count > 512 { return fail(n, b"reg count (want 20..=512)", count as u64); }
    if ioctl(c.vcpu, KVM_GET_REG_LIST, REGBUF.as_mut_ptr()) != 0 { return fail(n, b"REG_LIST errno", errno() as u64); }
    let mut have_fw = false;
    let mut a = [0u64; 32]; let mut b = [0u64; 32]; // 256 bytes each
    for i in 0..count {
        let id = REGBUF[1 + i];
        if id == REG_PSCI_VERSION { have_fw = true; }
        let bytes = 1usize << ((id >> 52) & 0xf);
        if bytes > 256 { return fail(n, b"reg size > 256, id", id); }
        if get_one(c.vcpu, id, a.as_mut_ptr() as *mut u8) != 0 { return fail(n, b"GET_ONE_REG errno, id", id); }
        if set_one(c.vcpu, id, a.as_ptr() as *const u8) != 0 { return fail(n, b"SET_ONE_REG errno, id", id); }
        if id == REG_TIMER_CNT { continue; } // volatile
        if get_one(c.vcpu, id, b.as_mut_ptr() as *mut u8) != 0 { return fail(n, b"re-GET errno, id", id); }
        let (pa, pb) = (a.as_ptr() as *const u8, b.as_ptr() as *const u8);
        for k in 0..bytes { if *pa.add(k) != *pb.add(k) { return fail(n, b"round trip differs, id", id); } }
    }
    if !have_fw { return fail(n, b"PSCI_VERSION missing from list", 0); }
    Line::new().s(b"KVMTEST reglist PASS regs=").d(count as u64).flush(); PASS += 1;
    true
}

unsafe fn ph_mmio() -> bool {
    let n = b"mmio";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::MMIO) else { return false; };
    let Some(v) = expect_mmio(n, &c, b"write", MMIO_GPA, true, 4) else { return false; };
    if !want(n, b"write data (want 0x42)", v, 0x42) { return false; }
    if expect_mmio(n, &c, b"read", MMIO_GPA + 4, false, 4).is_none() { return false; }
    wr64(c.run, R_MMIO_DATA, 0x1234); // read completion: lands in x3 of the guest
    let Some(v) = expect_mmio(n, &c, b"echo", MMIO_GPA + 8, true, 4) else { return false; };
    if !want(n, b"read completion not delivered to x3 (got)", v, 0x1234) { return false; }
    pass(n, b"write 0x42, read -> 0x1234 completed into x3")
}

unsafe fn ph_psci() -> bool {
    let n = b"psci";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::PSCI) else { return false; };
    let Some(v) = expect_mmio(n, &c, b"version", MMIO_GPA + 8, true, 4) else { return false; };
    if !want(n, b"PSCI_VERSION (want 0x10000)", v, 0x10000) { return false; }
    let Some(v) = expect_mmio(n, &c, b"unknown", MMIO_GPA + 12, true, 4) else { return false; };
    if !want(n, b"unknown PSCI call (want 0xffffffff NOT_SUPPORTED)", v, 0xffff_ffff) { return false; }
    pass(n, b"PSCI_VERSION=1.0 in-kernel over HVC, unknown call -1")
}

unsafe fn ph_wfi() -> bool {
    let n = b"wfi";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::WFI) else { return false; };
    let t0 = now_ns();
    let mut irq_seen = false;
    for _ in 0..1000 {
        let (rc, e) = vrun(c.vcpu);
        let r = rd32(c.run, R_EXIT_REASON);
        if (rc < 0 && e == EINTR) || (rc == 0 && r == EXIT_INTR) {
            // kernel-irqchip=off: the vtimer line goes to userspace in device_irq_level bit 0
            if rd64(c.run, R_DEVICE_IRQ_LEVEL) & 1 != 0 { irq_seen = true; }
            continue;
        }
        if rc != 0 { return fail(n, b"KVM_RUN errno", e as u64); }
        if r != EXIT_MMIO { return fail(n, b"exit_reason (want 6=MMIO)", r as u64); }
        if rd64(c.run, R_MMIO_ADDR) != MMIO_GPA + 12 || rd8(c.run, R_MMIO_IS_WRITE) == 0 { return fail(n, b"mmio addr", rd64(c.run, R_MMIO_ADDR)); }
        if !want(n, b"post-WFI marker (want 0x57)", rd64(c.run, R_MMIO_DATA) & 0xffff_ffff, 0x57) { return false; }
        if !irq_seen { return fail(n, b"no EXIT_INTR with device_irq_level bit0 (vtimer)", 0); }
        let ms = (now_ns() - t0) / 1_000_000;
        Line::new().s(b"KVMTEST wfi PASS vtimer level seen, woke after ").d(ms).s(b" ms").flush(); PASS += 1;
        return true;
    }
    fail(n, b"guest never woke from WFI after 1000 runs", 0)
}

unsafe fn ph_sysoff() -> bool {
    let n = b"sysoff";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SYSOFF) else { return false; };
    let (rc, e) = vrun(c.vcpu);
    if rc != 0 { return fail(n, b"KVM_RUN errno", e as u64); }
    if !want(n, b"exit_reason (want 24=SYSTEM_EVENT)", rd32(c.run, R_EXIT_REASON) as u64, EXIT_SYSTEM_EVENT as u64) { return false; }
    if !want(n, b"system_event type (want 1=SHUTDOWN)", rd32(c.run, R_SE_TYPE) as u64, SYSTEM_EVENT_SHUTDOWN as u64) { return false; }
    pass(n, b"PSCI SYSTEM_OFF -> SYSTEM_EVENT SHUTDOWN")
}

unsafe fn ph_immexit() -> bool {
    let n = b"immexit";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SPIN) else { return false; };
    wr8(c.run, R_IMMEDIATE_EXIT, 1);
    let (rc, e) = vrun(c.vcpu);
    if rc != -1 || e != EINTR { return fail(n, b"KVM_RUN with immediate_exit must be -1/EINTR, errno", e as u64); }
    if !want(n, b"exit_reason (want 10=INTR)", rd32(c.run, R_EXIT_REASON) as u64, EXIT_INTR as u64) { return false; }
    if get_u64(c.vcpu, REG_PC) != Some(RAM_GPA) { return fail(n, b"guest ran: pc moved", get_u64(c.vcpu, REG_PC).unwrap_or(0)); }
    pass(n, b"immediate_exit=1 returned EINTR without entering the guest")
}

static mut KICK_TARGET: usize = 0;
static KICK_AT: AtomicU64 = AtomicU64::new(0);
extern "C" fn kicker(_a: *mut c_void) -> *mut c_void {
    unsafe {
        usleep(20_000);
        KICK_AT.store(now_ns(), SeqCst);
        pthread_kill(KICK_TARGET, SIGUSR1);
    }
    null_mut()
}
const KICK_LIMIT_MS: u64 = 50;

unsafe fn ph_sigkick() -> bool {
    let n = b"sigkick";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SPIN) else { return false; };
    signal(SIGUSR1, on_usr1 as *const () as usize);
    KICK_TARGET = pthread_self();
    let mut th = 0usize;
    if pthread_create(&mut th, null(), kicker, null_mut()) != 0 { return fail(n, b"pthread_create", 0); }
    let (rc, e) = vrun(c.vcpu); // the guest spins on `b .`
    let back = now_ns();
    pthread_join(th, null_mut());
    if !((rc < 0 && e == EINTR) || (rc == 0 && rd32(c.run, R_EXIT_REASON) == EXIT_INTR)) {
        return fail(n, b"KVM_RUN must return EINTR after pthread_kill, rc", rc as u64);
    }
    let lat_ms = back.saturating_sub(KICK_AT.load(SeqCst)) / 1_000_000;
    if lat_ms > KICK_LIMIT_MS { return fail(n, b"kick latency ms (limit 50)", lat_ms); }
    Line::new().s(b"KVMTEST sigkick PASS SIGUSR1 kick returned EINTR in ").d(lat_ms).s(b" ms").flush(); PASS += 1;
    true
}

static V1_FLAG: AtomicU64 = AtomicU64::new(0);
static V1_ADDR: AtomicU64 = AtomicU64::new(0);
static V1_DATA: AtomicU64 = AtomicU64::new(0);
static V1_ERR: AtomicU64 = AtomicU64::new(0);
static mut V1_VCPU: c_int = -1;
static mut V1_RUN: *mut u8 = null_mut();
extern "C" fn vcpu1_thread(_a: *mut c_void) -> *mut c_void {
    unsafe {
        // blocks inside KVM_RUN while the vCPU is powered off; PSCI CPU_ON from vCPU 0 starts it
        let (rc, e) = vrun(V1_VCPU);
        if rc != 0 { V1_ERR.store(100 + e as u64, SeqCst); }
        else if rd32(V1_RUN, R_EXIT_REASON) != EXIT_MMIO { V1_ERR.store(200 + rd32(V1_RUN, R_EXIT_REASON) as u64, SeqCst); }
        else {
            V1_ADDR.store(rd64(V1_RUN, R_MMIO_ADDR), SeqCst);
            V1_DATA.store(rd64(V1_RUN, R_MMIO_DATA) & 0xffff_ffff, SeqCst);
        }
        V1_FLAG.store(1, SeqCst);
    }
    null_mut()
}

unsafe fn ph_smp() -> bool {
    let n = b"smp";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SMP0) else { return false; };
    for (i, w) in blob::SMP1.iter().enumerate() { (c.ram as *mut u32).add(64 + i).write_volatile(*w); } // entry RAM+0x100
    let Some((v1, r1)) = vcpu_create(c.vm, 1, c.run_len, true) else { return fail(n, b"create vcpu1 (POWER_OFF) errno", errno() as u64); };
    V1_VCPU = v1; V1_RUN = r1;
    let mut th = 0usize;
    if pthread_create(&mut th, null(), vcpu1_thread, null_mut()) != 0 { return fail(n, b"pthread_create", 0); }
    let Some(res) = expect_mmio(n, &c, b"cpu_on result", MMIO_GPA + 12, true, 4) else { return false; };
    if !want(n, b"PSCI CPU_ON result (want 0=SUCCESS)", res, 0) { return false; }
    pthread_join(th, null_mut()); // vcpu1 exits on its first MMIO write
    if V1_ERR.load(SeqCst) != 0 { return fail(n, b"vcpu1 KVM_RUN (100+errno / 200+exit_reason)", V1_ERR.load(SeqCst)); }
    if !want(n, b"vcpu1 mmio addr", V1_ADDR.load(SeqCst), MMIO_GPA + 16) { return false; }
    if !want(n, b"vcpu1 context id (want 0x77 in x0)", V1_DATA.load(SeqCst), 0x77) { return false; }
    pass(n, b"vcpu1 POWER_OFF -> PSCI CPU_ON -> ran at RAM+0x100 with x0=ctx")
}

// ---- no-v2-vGIC claim + plan step 6 stress ------------------------------------------------------
#[repr(C)]
struct CreateDev { ty: u32, fd: u32, flags: u32 }

unsafe fn create_dev(vm: c_int, ty: u32, flags: u32) -> (c_int, c_int) {
    let mut cd = CreateDev { ty, fd: 0, flags };
    let rc = ioctl(vm, KVM_CREATE_DEVICE, &mut cd as *mut CreateDev);
    (rc, if rc < 0 { errno() } else { 0 })
}

/// The host GIC decides which vGIC exists: v3 host -> only the v3 device (v2 refused, ENODEV);
/// v2 host -> v3 refused (ENODEV) and v2 refused (ENODEV) while GICH_VA is not set.
unsafe fn ph_vgicv2() -> bool {
    let n = b"vgicv2";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let vm = ioctl(kvm, KVM_CREATE_VM, 0usize);
    if vm < 0 { return fail(n, b"CREATE_VM errno", errno() as u64); }
    let (r3, e3) = create_dev(vm, KVM_DEV_TYPE_ARM_VGIC_V3, KVM_CREATE_DEVICE_TEST);
    let (r2, e2) = create_dev(vm, KVM_DEV_TYPE_ARM_VGIC_V2, KVM_CREATE_DEVICE_TEST);
    let (rb, eb) = create_dev(vm, 99, 0);
    if rb >= 0 || eb != ENODEV { return fail(n, b"unknown device type must be ENODEV, errno", eb as u64); }
    if r3 == 0 {
        // GICv3 host
        if r2 >= 0 || e2 != ENODEV { return fail(n, b"v3 host: v2 vGIC must be ENODEV, rc", r2 as u64); }
        return pass(n, b"GICv3 host: v3 vGIC offered, v2 refused with ENODEV");
    }
    if e3 != ENODEV { return fail(n, b"v2 host: v3 vGIC must be ENODEV, errno", e3 as u64); }
    // real (non-TEST) creates must fail the same way, and leave the VM without an irqchip
    let (r3b, e3b) = create_dev(vm, KVM_DEV_TYPE_ARM_VGIC_V3, 0);
    if r3b >= 0 || e3b != ENODEV { return fail(n, b"v2 host: v3 create must be ENODEV, errno", e3b as u64); }
    if r2 >= 0 { // GICH_VA present: a v2 vGIC is legitimately offered
        close(vm); close(kvm);
        return pass(n, b"GICv2 host with GICH_VA: v2 vGIC offered, v3 refused with ENODEV");
    }
    if e2 != ENODEV { return fail(n, b"v2 host, no GICH_VA: v2 vGIC must be ENODEV, errno", e2 as u64); }
    let (r2b, e2b) = create_dev(vm, KVM_DEV_TYPE_ARM_VGIC_V2, 0);
    if r2b >= 0 || e2b != ENODEV { return fail(n, b"v2 host, no GICH_VA: v2 create must be ENODEV, errno", e2b as u64); }
    pass(n, b"GICv2 host, no GICH_VA: v2 and v3 vGIC both refused with ENODEV")
}

static STOP: AtomicU64 = AtomicU64::new(0);
extern "C" fn spammer(_a: *mut c_void) -> *mut c_void {
    unsafe { while STOP.load(SeqCst) == 0 { pthread_kill(KICK_TARGET, SIGUSR1); usleep(300); } }
    null_mut()
}

unsafe fn ph_sigstress() -> bool {
    let n = b"sigstress";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::SPIN) else { return false; };
    signal(SIGUSR1, on_usr1 as *const () as usize);
    KICK_TARGET = pthread_self();
    let probe0 = open_kvm(); close(probe0); // lowest free fd before the loop
    STOP.store(0, SeqCst);
    let mut th = 0usize;
    if pthread_create(&mut th, null(), spammer, null_mut()) != 0 { return fail(n, b"pthread_create", 0); }
    let (mut by_sig, mut by_imm) = (0u32, 0u32);
    let mut i = 0u32;
    while i < 1000 {
        if i & 1 == 1 {
            wr8(c.run, R_IMMEDIATE_EXIT, 1);
            let (rc, e) = vrun(c.vcpu);
            wr8(c.run, R_IMMEDIATE_EXIT, 0);
            if rc != -1 || e != EINTR { STOP.store(1, SeqCst); pthread_join(th, null_mut()); return fail(n, b"immediate_exit KVM_RUN errno", e as u64); }
            by_imm += 1;
        } else {
            let (rc, e) = vrun(c.vcpu);
            if !((rc < 0 && e == EINTR) || (rc == 0 && rd32(c.run, R_EXIT_REASON) == EXIT_INTR)) {
                STOP.store(1, SeqCst); pthread_join(th, null_mut());
                return fail(n, b"signalled KVM_RUN must be EINTR, errno", e as u64);
            }
            by_sig += 1;
        }
        i += 1;
    }
    STOP.store(1, SeqCst);
    pthread_join(th, null_mut());
    if get_u64(c.vcpu, REG_PC) != Some(RAM_GPA) { return fail(n, b"spin guest pc moved", get_u64(c.vcpu, REG_PC).unwrap_or(0)); }
    let probe1 = open_kvm(); close(probe1);
    if probe0 != probe1 { return fail(n, b"fd leak: lowest free fd changed to", probe1 as u64); }
    Line::new().s(b"KVMTEST sigstress PASS 1000 KVM_RUN returns (").d(by_sig as u64).s(b" signal, ").d(by_imm as u64).s(b" immediate_exit), no hang, no fd leak").flush(); PASS += 1;
    true
}

static RUN_VCPU: AtomicU64 = AtomicU64::new(0);
static RUN_RC: AtomicU64 = AtomicU64::new(0xdead);
static RUN_ERRNO: AtomicU64 = AtomicU64::new(0);
static RUN_EXIT: AtomicU64 = AtomicU64::new(0);
static RUN_DONE: AtomicU64 = AtomicU64::new(0);
static RUN_PTR: AtomicU64 = AtomicU64::new(0);
extern "C" fn run_thread(_a: *mut c_void) -> *mut c_void {
    unsafe {
        let (rc, e) = vrun(RUN_VCPU.load(SeqCst) as c_int);
        RUN_RC.store(rc as i64 as u64, SeqCst);
        RUN_ERRNO.store(e as u64, SeqCst);
        RUN_EXIT.store(rd32(RUN_PTR.load(SeqCst) as *mut u8, R_EXIT_REASON) as u64, SeqCst);
        RUN_DONE.store(1, SeqCst);
    }
    null_mut()
}

/// Starts the COUNTER guest in a thread; returns the thread id once the counter moves.
unsafe fn start_counter(n: &[u8], c: &Ctx) -> Option<usize> {
    for (i, w) in blob::COUNTER.iter().enumerate() { (c.ram as *mut u32).add(i).write_volatile(*w); }
    RUN_VCPU.store(c.vcpu as u64, SeqCst); RUN_PTR.store(c.run as u64, SeqCst);
    RUN_DONE.store(0, SeqCst); RUN_RC.store(0xdead, SeqCst);
    let mut th = 0usize;
    if pthread_create(&mut th, null(), run_thread, null_mut()) != 0 { fail(n, b"pthread_create", 0); return None; }
    let ctr = c.ram.add(0x1000) as *const u32;
    let mut t = 0;
    while ctr.read_volatile() == 0 && t < 2000 { usleep(5_000); t += 1; }
    if ctr.read_volatile() == 0 { fail(n, b"counter guest never ran", 0); return None; }
    Some(th)
}

/// Waits up to `ms` for the run thread to finish.
unsafe fn wait_run(ms: u32) -> bool {
    let mut t = 0;
    while RUN_DONE.load(SeqCst) == 0 && t < ms { usleep(1000); t += 1; }
    RUN_DONE.load(SeqCst) != 0
}

unsafe fn ph_unmap_one(n: &[u8], protect: bool) -> bool {
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::COUNTER) else { return false; };
    let Some(th) = start_counter(n, &c) else { return false; };
    signal(SIGUSR1, on_usr1 as *const () as usize);
    usleep(20_000);
    // another thread (here: main) pulls the guest RAM out from under the running vCPU
    let r = if protect { mprotect(c.ram as *mut c_void, RAM_SIZE, 0) } else { munmap(c.ram as *mut c_void, RAM_SIZE) };
    if r != 0 { return fail(n, b"munmap/mprotect errno", errno() as u64); }
    // the guest's next access must be refused (stage-2 torn down): KVM_RUN must come back by itself
    let self_exit = wait_run(3000);
    if !self_exit {
        pthread_kill(th, SIGUSR1); // unblock so the test can report instead of hanging
        wait_run(3000);
        return fail(n, b"guest kept running on unmapped/protected RAM (stage-2 not invalidated), kick needed", 1);
    }
    pthread_join(th, null_mut());
    let rc = RUN_RC.load(SeqCst) as i64; let e = RUN_ERRNO.load(SeqCst); let ex = RUN_EXIT.load(SeqCst);
    let ok = (rc < 0 && e != EINTR as u64) || (rc == 0 && ex != EXIT_INTR as u64);
    if !ok { return fail(n, b"KVM_RUN after RAM removal: rc/errno/exit unexpected, errno", e); }
    Line::new().s(b"KVMTEST ").s(n).s(b" PASS KVM_RUN returned by itself: rc=").d(rc as u64).s(b" errno=").d(e).s(b" exit_reason=").d(ex).flush(); PASS += 1;
    if protect { mprotect(c.ram as *mut c_void, RAM_SIZE, 3); }
    true
}
unsafe fn ph_unmap() -> bool {
    let a = ph_unmap_one(b"unmap", false);
    CUR = b"mprotect";
    let b = ph_unmap_one(b"mprotect", true);
    a && b
}

/// fork() while a vCPU runs: the child must not drive the VM or see guest stores; the parent's VM lives on.
unsafe fn ph_forkvm() -> bool {
    let n = b"forkvm";
    let kvm = open_kvm();
    if kvm < 0 { return fail(n, b"open errno", errno() as u64); }
    let Some(c) = setup(n, kvm, &blob::COUNTER) else { return false; };
    let Some(th) = start_counter(n, &c) else { return false; };
    signal(SIGUSR1, on_usr1 as *const () as usize);
    let ctr = c.ram.add(0x1000) as *const u32;
    let pid = fork();
    if pid < 0 { return fail(n, b"fork errno", errno() as u64); }
    if pid == 0 {
        // child: no vCPU thread, no usable guest mapping
        let mut bad = 0;
        let (rc, _) = vrun(c.vcpu);
        if rc >= 0 { bad |= 1; }
        let nv = ioctl(c.vm, KVM_CREATE_VCPU, 5usize);
        if nv >= 0 { bad |= 2; }
        let a = ctr.read_volatile();
        usleep(100_000);
        let b = ctr.read_volatile();
        if a != b { bad |= 4; } // the child's RAM copy must be frozen: guest stores go to the parent's pages
        _exit(bad);
    }
    let mut st: c_int = 0;
    if waitpid(pid, &mut st, 0) != pid { return fail(n, b"waitpid errno", errno() as u64); }
    let code = (st >> 8) & 0xff;
    if st & 0x7f != 0 { return fail(n, b"child killed by signal", (st & 0x7f) as u64); }
    if code & 1 != 0 { return fail(n, b"child KVM_RUN on the inherited vcpu fd succeeded", 1); }
    if code & 2 != 0 { return fail(n, b"child KVM_CREATE_VCPU on the inherited vm fd succeeded", 2); }
    if code & 4 != 0 { return fail(n, b"child sees guest stores (stale stage-2 shares COW page)", 4); }
    // parent's VM keeps working: the counter still advances and KVM_RUN is still interruptible
    let a = ctr.read_volatile();
    usleep(100_000);
    let b = ctr.read_volatile();
    if a == b { return fail(n, b"parent guest stalled after fork, counter", a as u64); }
    pthread_kill(th, SIGUSR1);
    if !wait_run(3000) { return fail(n, b"parent KVM_RUN not interruptible after fork", 0); }
    pthread_join(th, null_mut());
    if RUN_RC.load(SeqCst) as i64 != -1 || RUN_ERRNO.load(SeqCst) != EINTR as u64 { return fail(n, b"parent KVM_RUN after kick, errno", RUN_ERRNO.load(SeqCst)); }
    pass(n, b"child could not run/see the VM; parent guest kept running and stayed interruptible")
}

// ---- driver -----------------------------------------------------------------------------------
const PHASES: [(&[u8], unsafe fn() -> bool); 16] = [
    (b"open", ph_open), (b"caps", ph_caps), (b"memslot", ph_memslot), (b"regs", ph_regs), (b"reglist", ph_reglist),
    (b"mmio", ph_mmio), (b"psci", ph_psci), (b"wfi", ph_wfi), (b"sysoff", ph_sysoff),
    (b"immexit", ph_immexit), (b"sigkick", ph_sigkick), (b"smp", ph_smp),
    (b"vgicv2", ph_vgicv2), (b"sigstress", ph_sigstress), (b"unmap", ph_unmap), (b"forkvm", ph_forkvm),
];

unsafe fn cstr_eq(p: *const u8, s: &[u8]) -> bool {
    for (i, &c) in s.iter().enumerate() { if *p.add(i) != c { return false; } }
    *p.add(s.len()) == 0
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let summary = |code: i32| -> i32 {
        Line::new().s(b"KVMTEST summary pass=").d(PASS as u64).s(b" fail=").d(FAIL as u64).s(b" skip=").d(SKIP as u64).flush();
        code
    };
    let probe = open_kvm();
    if cfg!(not(target_arch = "aarch64")) || probe < 0 {
        skip(b"all", if probe < 0 { b"no /dev/kvm" } else { b"not aarch64" });
        return summary(0);
    }
    close(probe);
    signal(SIGALRM, on_alarm as *const () as usize);
    let all = argc < 2 || cstr_eq(*argv.add(1), b"all");
    let mut ran = false;
    for (name, f) in PHASES.iter() {
        let selected = all || (1..argc as usize).any(|i| cstr_eq(*argv.add(i), name));
        if !selected { continue; }
        ran = true;
        CUR = name;
        alarm(PHASE_TIMEOUT_S);
        f();
        alarm(0);
    }
    if !ran { fail(b"args", b"no known phase selected", 0); }
    summary(if FAIL == 0 { 0 } else { 1 })
}
