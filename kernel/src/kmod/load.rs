//! Module loader. Segments are written through the HHDM alias of freshly
//! allocated frames, then each page is mapped once with its FINAL permission:
//! a page is never writable and executable, and never changes permission.

use alloc::vec::Vec;
use kmod_abi::{KmodCtx, KmodId, KMOD_ABI_VERSION};
use kmod_elf::{ElfErr, Expect};
use mm::kmodwin::{self, Perm};
use super::registry::{self, State};

const PAGE: usize = 4096;

/// The module whose `init` is running now (0 = none); `kapi_log` tags lines with it.
pub static CUR_OWNER: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source { Initrd, Syscall, Ktest }

#[derive(Clone, Copy, Debug)]
pub enum KmodErr { Disabled, Blacklisted, NotFound, Exists, Elf(ElfErr), SlotBusy, NoMem, Init(i32) }

impl KmodErr {
    pub fn errno(&self) -> isize {
        match self {
            KmodErr::Disabled | KmodErr::Blacklisted => 1,
            KmodErr::NotFound => 2,
            KmodErr::Exists => 17,
            KmodErr::Elf(_) => 8,
            KmodErr::SlotBusy => 16,
            KmodErr::NoMem => 12,
            KmodErr::Init(e) => *e as isize,
        }
    }
}

#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = 62;
#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = 183;

fn log(s: &str) { drivers::pci::serial_debug(s); }

fn conv(p: kmod_elf::Perm) -> Perm {
    match p { kmod_elf::Perm::R => Perm::R, kmod_elf::Perm::RX => Perm::RX, kmod_elf::Perm::RW => Perm::RW }
}

/// Data-cache clean + instruction-cache invalidate over `len` bytes at `va`, inner shareable.
#[cfg(target_arch = "aarch64")]
unsafe fn sync_icache(va: usize, len: usize) {
    let ctr: u64;
    core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack));
    let line = 4usize << ((ctr >> 16) & 0xf);
    let mut a = va & !(line - 1);
    while a < va + len {
        core::arch::asm!("dc cvau, {}", in(reg) a, options(nostack));
        a += line;
    }
    core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack));
}

/// Make freshly written code visible to instruction fetch on every CPU.
fn publish_code(pages: &[(usize, usize, Perm)]) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        for &(_, phys, perm) in pages {
            if perm == Perm::RX { sync_icache(mm::phys_to_virt(phys), PAGE); }
        }
        // Broadcast: kick every other online CPU so it takes an exception
        // (a context-synchronising event) before it can run the new code.
        let me = sched::cpu_id();
        for c in 0..sched::active_cpu_count() {
            if c != me { sched::trigger_preempt(c); }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = pages;
}

fn free_all(pages: &[(usize, usize, Perm)]) { for &(_, p, _) in pages { mm::buddy::free(p, 0); } }

pub fn load_image(img: &[u8], src: Source) -> Result<KmodId, KmodErr> {
    if super::disabled() { return Err(KmodErr::Disabled); }
    let ex = Expect { machine: MACHINE, build_id: super::buildid::build_id(),
                      window_base: mm::kmodwin::BASE as u64, window_size: mm::kmodwin::SIZE as u64 };
    let plan = match kmod_elf::validate(img, &ex) {
        Ok(p) => p,
        Err(e) => { log(&alloc::format!("[kmod] {:?} image refused: {:?}\n", src, e)); return Err(KmodErr::Elf(e)); }
    };
    let hdr = plan.hdr;
    if super::autoload::blacklisted(super::name_str(&hdr.name)) {
        log(&alloc::format!("[kmod] {} Blacklisted\n", super::name_str(&hdr.name)));
        return Err(KmodErr::Blacklisted);
    }
    if registry::name_exists(&hdr.name) {
        log("[kmod] refused: name already loaded\n");
        return Err(KmodErr::Exists);
    }
    let (base, size) = (hdr.slot_base as usize, hdr.slot_size as usize);
    if !kmodwin::reserve(base, size) {
        log("[kmod] refused: slot busy\n");
        return Err(KmodErr::SlotBusy);
    }
    // 1. allocate + write through the HHDM alias.
    let mut pages: Vec<(usize, usize, Perm)> = Vec::new();
    for s in plan.segs.iter().take(plan.nsegs).flatten() {
        let npages = (s.memsz as usize).div_ceil(PAGE);
        for i in 0..npages {
            let phys = match mm::buddy::alloc(0) {
                Some(p) => p,
                None => { free_all(&pages); kmodwin::release(base, size); return Err(KmodErr::NoMem); }
            };
            let dst = mm::phys_to_virt(phys) as *mut u8;
            unsafe {
                core::ptr::write_bytes(dst, 0, PAGE);
                let po = i * PAGE;
                if po < s.filesz as usize {
                    let n = (s.filesz as usize - po).min(PAGE);
                    let o = s.off as usize + po;
                    core::ptr::copy_nonoverlapping(img.as_ptr().add(o), dst, n);
                }
            }
            pages.push((s.vaddr as usize + i * PAGE, phys, conv(s.perm)));
        }
    }
    // 2. map once with the final permission.
    for (n, &(va, phys, perm)) in pages.iter().enumerate() {
        if !unsafe { kmodwin::map(va, phys, perm) } {
            for &(v, _, _) in &pages[..n] { unsafe { kmodwin::unmap(v, 1); } }
            free_all(&pages);
            kmodwin::release(base, size);
            return Err(KmodErr::SlotBusy);
        }
    }
    publish_code(&pages);
    // 3. register and run init on this thread.
    let id = match registry::add(hdr.name, base, size) {
        Some(i) => i,
        None => {
            for &(v, _, _) in &pages { unsafe { kmodwin::unmap(v, 1); } }
            free_all(&pages);
            kmodwin::release(base, size);
            return Err(KmodErr::Exists);
        }
    };
    let cl = crate::boot_cmdline().unwrap_or("");
    let ctx = KmodCtx {
        ctx_size: core::mem::size_of::<KmodCtx>() as u32, abi_version: KMOD_ABI_VERSION, owner: id,
        _pad: 0, _pad2: 0, cmdline: cl.as_ptr(), cmdline_len: cl.len(), params: core::ptr::null(), params_len: 0,
    };
    let init: extern "C" fn(*const KmodCtx) -> i32 = unsafe { core::mem::transmute(hdr.init as usize) };
    CUR_OWNER.store(id.0, core::sync::atomic::Ordering::SeqCst);
    let rc = init(&ctx);
    CUR_OWNER.store(0, core::sync::atomic::Ordering::SeqCst);
    let nm = super::name_str(&hdr.name);
    if rc >= 0 {
        registry::set_state(id, State::Live);
        log(&alloc::format!("[kmod] {} loaded id {} slot {:#x} {} pages\n", nm, id.0, base, pages.len()));
        return Ok(id);
    }
    if registry::release_all(id) {
        // Callbacks that can be taken back were unregistered; nothing can reference the slot: unmap, free, release, forget, so a retry works.
        for &(v, _, _) in &pages { unsafe { kmodwin::unmap(v, 1); } }
        free_all(&pages);
        kmodwin::release(base, size);
        registry::remove(id);
        log(&alloc::format!("[kmod] {} init failed {}, slot freed\n", nm, rc));
    } else {
        registry::set_state(id, State::Failed);
        log(&alloc::format!("[kmod] {} init failed {}, {} pages leaked (registrations live)\n", nm, rc, pages.len()));
    }
    Err(KmodErr::Init(rc))
}
