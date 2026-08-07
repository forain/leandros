//! neonprobe — does userspace FP/SIMD (q0-q31) survive a trap into the LeandrOS
//! aarch64 kernel?
//!
//! arch/aarch64/src/exception_asm.s saves a 288-byte frame (x0-x30, sp_el0, elr,
//! spsr, ttbr0) and no vector state, while the kernel is built "+neon,+fp-armv8"
//! (targets/aarch64-unknown-kernel.json). Whether that bites depends on which
//! trap you take and how much work the kernel does before returning:
//!
//!   SVC   — syscall. Measured clean.
//!   IRQ   — timer. Measured clean (the context switch saves q0-q31).
//!   FAULT — demand paging. This one memsets a page and copies up to 64 KiB out
//!           of f2fs per fault, which is exactly the code LLVM lowers through q
//!           registers. Untested until now.
//!
//! Each case loads a pattern into q0-q31, takes the trap inside the same asm
//! block, stores them back and diffs. A demand-paging fault happens on the first
//! touch of any page — including the first execution of a function — which is
//! why a 52 MB binary meets it constantly and a small one barely at all.

use std::collections::HashMap;

const PAGE: usize = 4096;

extern "C" {
    fn mmap(
        addr: *mut core::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        off: i64,
    ) -> *mut core::ffi::c_void;
}

#[cfg(target_arch = "aarch64")]
fn pattern() -> [u128; 32] {
    let mut p = [0u128; 32];
    for (i, v) in p.iter_mut().enumerate() {
        *v = 0x1111_1111_1111_1111_2222_2222_2222_2222u128 ^ ((i as u128) << 96 | (i as u128));
    }
    p
}

macro_rules! ldq {
    () => {
        concat!(
            "ldp q0,  q1,  [{i}, #0]\n",   "ldp q2,  q3,  [{i}, #32]\n",
            "ldp q4,  q5,  [{i}, #64]\n",  "ldp q6,  q7,  [{i}, #96]\n",
            "ldp q8,  q9,  [{i}, #128]\n", "ldp q10, q11, [{i}, #160]\n",
            "ldp q12, q13, [{i}, #192]\n", "ldp q14, q15, [{i}, #224]\n",
            "ldp q16, q17, [{i}, #256]\n", "ldp q18, q19, [{i}, #288]\n",
            "ldp q20, q21, [{i}, #320]\n", "ldp q22, q23, [{i}, #352]\n",
            "ldp q24, q25, [{i}, #384]\n", "ldp q26, q27, [{i}, #416]\n",
            "ldp q28, q29, [{i}, #448]\n", "ldp q30, q31, [{i}, #480]\n"
        )
    };
}
macro_rules! stq {
    () => {
        concat!(
            "stp q0,  q1,  [{o}, #0]\n",   "stp q2,  q3,  [{o}, #32]\n",
            "stp q4,  q5,  [{o}, #64]\n",  "stp q6,  q7,  [{o}, #96]\n",
            "stp q8,  q9,  [{o}, #128]\n", "stp q10, q11, [{o}, #160]\n",
            "stp q12, q13, [{o}, #192]\n", "stp q14, q15, [{o}, #224]\n",
            "stp q16, q17, [{o}, #256]\n", "stp q18, q19, [{o}, #288]\n",
            "stp q20, q21, [{o}, #320]\n", "stp q22, q23, [{o}, #352]\n",
            "stp q24, q25, [{o}, #384]\n", "stp q26, q27, [{o}, #416]\n",
            "stp q28, q29, [{o}, #448]\n", "stp q30, q31, [{o}, #480]\n"
        )
    };
}

/// Load q0-q31, take a demand-paging fault by touching `page`, store q0-q31.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn roundtrip_fault(inp: &[u128; 32], out: &mut [u128; 32], page: *mut u8) {
    unsafe {
        core::arch::asm!(
            ldq!(),
            "ldr x9, [{p}]",          // first touch of this page -> EL0 fault
            stq!(),
            i = in(reg) inp.as_ptr(),
            o = in(reg) out.as_mut_ptr(),
            p = in(reg) page,
            out("x9") _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _,
            out("v4") _, out("v5") _, out("v6") _, out("v7") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _,
            out("v28") _, out("v29") _, out("v30") _, out("v31") _,
            options(nostack)
        );
    }
}

/// Same, but the page is already resident — the control.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn roundtrip_resident(inp: &[u128; 32], out: &mut [u128; 32], page: *mut u8) {
    roundtrip_fault(inp, out, page)
}

#[cfg(target_arch = "aarch64")]
fn diff(inp: &[u128; 32], out: &[u128; 32]) -> Vec<usize> {
    (0..32).filter(|&i| inp[i] != out[i]).collect()
}

#[cfg(target_arch = "aarch64")]
fn neon_tests() {
    let inp = pattern();
    let mut out = [0u128; 32];

    // 64 MiB of untouched anonymous memory: one fresh page per iteration.
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 2;
    const MAP_ANON: i32 = 0x20;
    let len = 64 * 1024 * 1024;
    let base = unsafe {
        mmap(
            core::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        )
    } as *mut u8;
    if base as isize == -1 || base.is_null() {
        println!("FAULT: mmap failed, skipping");
        return;
    }

    let mut first_bad = None;
    let mut bad_rounds = 0usize;
    let rounds = 4000usize;
    for i in 0..rounds {
        let page = unsafe { base.add(i * PAGE) };
        roundtrip_fault(&inp, &mut out, page);
        let d = diff(&inp, &out);
        if !d.is_empty() {
            bad_rounds += 1;
            if first_bad.is_none() {
                first_bad = Some((i, d.clone(), out));
            }
        }
    }
    println!("FAULT(cold page): {bad_rounds}/{rounds} faults clobbered vector state");
    if let Some((i, d, got)) = first_bad {
        println!("  first at fault {i}: {} regs clobbered {:?}", d.len(), d);
        let r = d[0];
        println!("  q{r} expected {:#034x}", inp[r]);
        println!("  q{r} got      {:#034x}", got[r]);
    }

    // Control: same instruction sequence, page already resident -> no fault.
    let resident = unsafe { base.add(0) };
    let mut bad_resident = 0usize;
    for _ in 0..4000 {
        roundtrip_resident(&inp, &mut out, resident);
        if !diff(&inp, &out).is_empty() {
            bad_resident += 1;
        }
    }
    println!("FAULT(resident page, control): {bad_resident}/4000 clobbered");
}

#[cfg(not(target_arch = "aarch64"))]
fn neon_tests() {
    println!("NEON tests: aarch64 only");
}

/// The user-visible consequence: hash a key while faulting pages in, and watch
/// a HashMap lose track of an entry it definitely stored.
fn hash_stability_under_faults() {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 2;
    const MAP_ANON: i32 = 0x20;
    let len = 64 * 1024 * 1024;
    let base = unsafe {
        mmap(
            core::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        )
    } as *mut u8;

    let s = String::from("path=/run/user/0/bus");
    let k = &s[0..4];
    let rs = RandomState::new();
    let reference = rs.hash_one(k);

    let mut unstable = 0usize;
    let mut map_fails = 0usize;
    let rounds = 4000usize;
    for i in 0..rounds {
        // Fault in a fresh page immediately before hashing.
        if !base.is_null() && base as isize != -1 {
            unsafe {
                core::ptr::read_volatile(base.add(i * PAGE));
            }
        }
        if rs.hash_one(k) != reference {
            unstable += 1;
        }
        let mut m: HashMap<&str, &str, RandomState> = HashMap::with_hasher(rs.clone());
        m.insert(k, &s[5..]);
        if m.get(k).is_none() {
            map_fails += 1;
        }
    }
    println!("HASH: {unstable}/{rounds} hashes of the same key disagreed");
    println!("MAP:  {map_fails}/{rounds} insert-then-get lookups failed");
}

/// 60 MiB of .rodata: file-backed, demand-paged out of f2fs by the kernel's
/// exec-image path — the same VMA kind the applet's cold code lives in, and a
/// heavier fault than anonymous memory (gathered 64 KiB read + copy + cache
/// maintenance) rather than a single page memset.
static BLOB: &[u8] = include_bytes!("../pattern.bin");

#[cfg(target_arch = "aarch64")]
fn file_backed_fault_test() {
    let inp = pattern();
    let mut out = [0u128; 32];
    let pages = BLOB.len() / PAGE;

    let mut bad_rounds = 0usize;
    let mut first_bad: Option<(usize, Vec<usize>, [u128; 32])> = None;
    for i in 0..pages {
        let p = unsafe { BLOB.as_ptr().add(i * PAGE) as *mut u8 };
        roundtrip_fault(&inp, &mut out, p);
        let d = diff(&inp, &out);
        if !d.is_empty() {
            bad_rounds += 1;
            if first_bad.is_none() {
                first_bad = Some((i, d.clone(), out));
            }
        }
    }
    println!("FILEFAULT: {bad_rounds}/{pages} file-backed page touches clobbered vector state");
    if let Some((i, d, got)) = first_bad {
        println!("  first at page {i}: {} regs clobbered {:?}", d.len(), d);
        for &r in d.iter().take(4) {
            println!("  q{r} expected {:#034x}", inp[r]);
            println!("  q{r} got      {:#034x}", got[r]);
        }
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn file_backed_fault_test() {}

fn main() {
    neon_tests();
    file_backed_fault_test();
    hash_stability_under_faults();
    println!("DONE");
}
