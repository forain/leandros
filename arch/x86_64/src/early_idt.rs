//! A minimal IDT for the first moments of boot (lane fbmm, 2026-10-16).
//!
//! Between kernel entry and `idt::init` (boot stage 3) the CPU still uses
//! whatever IDTR the firmware/bootloader left. Limine leaves it "undefined";
//! on UEFI it is the firmware's own table, whose handlers live in boot-services
//! memory the memory manager is busy handing out -- and that Limine's page
//! tables need not even map. A fault there (an unmapped HHDM access, a bad
//! pointer in the memory-map walk) therefore ends in a silent firmware
//! dead-loop or a triple-fault reset: on the HP EliteBook with Fast Boot OFF,
//! one white square and nothing else.
//!
//! This table catches vectors 0-31 and paints the vector, error code, RIP,
//! CR2 and the last boot stage onto the early framebuffer text area
//! (`boot::earlyfb`), then halts. NMIs are reported and resumed (an NMI is
//! not our fault and the firmware may route one through for its own reasons).
//! No IST and no TSS: a fault with a broken stack still triple-faults -- but
//! that is no worse than before. `idt::init` replaces the table at stage 3.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, Ordering};

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry { lo: u16, sel: u16, ist: u8, attr: u8, mid: u16, hi: u32, _r: u32 }

#[repr(C, align(16))]
struct Table([Entry; 32]);

static mut TABLE: Table = Table([Entry { lo: 0, sel: 0, ist: 0, attr: 0, mid: 0, hi: 0, _r: 0 }; 32]);

/// Saved by `early_exc_common`, lowest address first.
#[repr(C)]
pub struct Frame {
    pub r15: u64, pub r14: u64, pub r13: u64, pub r12: u64,
    pub r11: u64, pub r10: u64, pub r9: u64,  pub r8: u64,
    pub rbp: u64, pub rdi: u64, pub rsi: u64, pub rdx: u64,
    pub rcx: u64, pub rbx: u64, pub rax: u64,
    pub vector: u64, pub error: u64,
    pub rip: u64, pub cs: u64, pub rflags: u64, pub rsp: u64, pub ss: u64,
}

extern "C" { static early_exc_stubs: [u64; 32]; }

// Vectors that push an error code: 8, 10-14, 17, 21, 29, 30.
macro_rules! stubs {
    ($($n:literal $ec:literal),* $(,)?) => {
        global_asm!(
            ".section .text",
            $(
                concat!(".balign 16\nearly_exc_stub_", $n, ":"),
                concat!(".if ", $ec, " == 0\n push 0\n.endif"),
                concat!("push ", $n),
                "jmp early_exc_common",
            )*
            ".section .rodata",
            ".balign 8",
            ".globl early_exc_stubs",
            "early_exc_stubs:",
            $( concat!(".quad early_exc_stub_", $n), )*
            ".section .text",
        );
    };
}
stubs!(0 0, 1 0, 2 0, 3 0, 4 0, 5 0, 6 0, 7 0, 8 1, 9 0, 10 1, 11 1, 12 1, 13 1, 14 1, 15 0,
       16 0, 17 1, 18 0, 19 0, 20 0, 21 1, 22 0, 23 0, 24 0, 25 0, 26 0, 27 0, 28 0, 29 1, 30 1, 31 0);

global_asm!(r#"
.section .text
.balign 16
early_exc_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, rsp
    mov rbx, rsp
    and rsp, -16
    call {handler}
    mov rsp, rbx
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    iretq
"#, handler = sym early_exc_handler);

static NMIS: AtomicU32 = AtomicU32::new(0);

const NAMES: [&str; 32] = [
    "#DE divide", "#DB debug", "NMI", "#BP breakpoint", "#OF overflow", "#BR bound",
    "#UD invalid opcode", "#NM no FPU", "#DF double fault", "coproc", "#TS bad TSS",
    "#NP not present", "#SS stack fault", "#GP general protection", "#PF page fault", "15",
    "#MF x87", "#AC alignment", "#MC machine check", "#XM SIMD", "#VE virt", "#CP control",
    "22", "23", "24", "25", "26", "27", "#HV", "#VC", "#SX security", "31",
];

fn hex(v: u64) { boot::earlyfb::put_hex(v, 1); }

extern "C" fn early_exc_handler(f: &mut Frame) {
    let v = (f.vector & 31) as usize;
    let cr2: u64;
    let cr3: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    }
    if v == 2 {
        let n = NMIS.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 && boot::earlyfb::text_on() {
            boot::earlyfb::set_color(boot::earlyfb::YELLOW);
            boot::earlyfb::line_break();
            boot::earlyfb::puts("[early] NMI #"); boot::earlyfb::put_dec(n as u64);
            boot::earlyfb::puts(" rip "); hex(f.rip);
            boot::earlyfb::puts(" stage "); boot::earlyfb::put_dec(boot::earlyfb::last_stage() as u64);
            boot::earlyfb::puts(" (resumed)\n");
            boot::earlyfb::set_color(boot::earlyfb::GREY);
        }
        return;
    }
    // Serial first (cheap, and the only output if the screen is not armed).
    let s = |t: &str| for b in t.bytes() { unsafe { crate::putc(b); } };
    let sh = |x: u64| {
        const H: &[u8; 16] = b"0123456789abcdef";
        unsafe { crate::putc(b'0'); crate::putc(b'x'); }
        let mut started = false;
        for i in (0..16).rev() {
            let d = ((x >> (i * 4)) & 0xF) as usize;
            if d != 0 || started || i == 0 { unsafe { crate::putc(H[d]); } started = true; }
        }
    };
    s("\n[early] CPU EXCEPTION "); s(NAMES[v]); s(" err="); sh(f.error);
    s(" rip="); sh(f.rip); s(" cr2="); sh(cr2); s(" rsp="); sh(f.rsp); s("\n");
    if boot::earlyfb::fatal_begin(0x00FF_00FF) {
        use boot::earlyfb::{puts, put_dec};
        puts("CPU EXCEPTION before the console: vector "); put_dec(v as u64);
        puts(" "); puts(NAMES[v]); puts("\n");
        boot::earlyfb::set_color(boot::earlyfb::WHITE);
        puts("err "); hex(f.error); puts("  rip "); hex(f.rip); puts("  cs "); hex(f.cs);
        puts("  rflags "); hex(f.rflags); puts("\n");
        puts("cr2 "); hex(cr2); puts("  cr3 "); hex(cr3); puts("  rsp "); hex(f.rsp);
        puts("  rbp "); hex(f.rbp); puts("\n");
        puts("rax "); hex(f.rax); puts(" rbx "); hex(f.rbx); puts(" rcx "); hex(f.rcx);
        puts(" rdx "); hex(f.rdx); puts("\n");
        puts("rsi "); hex(f.rsi); puts(" rdi "); hex(f.rdi); puts(" r8 "); hex(f.r8);
        puts(" r9 "); hex(f.r9); puts("\n");
        if v == 14 {
            let w = walk(cr2);
            puts("walk cr2 ("); put_dec(w.levels as u64); puts(" levels):");
            for d in 0..w.depth { puts(" "); hex(w.e[d]); }
            if w.bad_table { puts(" [table ptr outside mapped RAM]"); }
            if w.page != 0 { puts(" -> mapped, page "); hex(w.page); }
            puts("\n");
        }
        puts("last boot stage "); put_dec(boot::earlyfb::last_stage() as u64);
        puts(" - photograph this screen. Halted.\n");
    }
    boot::earlyfb::halt_forever();
}

// ── Bootloader page-table walk ───────────────────────────────────────────────
//
// On the HP with Fast Boot OFF the first write into the top of RAM through
// the HHDM page-faulted (not-present) although Limine 11.4.1's
// `build_pagemap` maps every non-reserved memory-map entry. A walk of the
// live tables answers "is this address mapped, and if not, at which level is
// the entry missing / what does it hold" -- for the fault report and for
// excluding unmapped usable RAM before mm writes into it.

static HHDM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TABLE_OK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The HHDM offset the walker reads tables through, and a predicate telling
/// which physical pages are safe to read through it (a table pointer from a
/// corrupted entry must not take a second fault).
pub fn set_walk_context(hhdm: u64, table_ok: fn(u64) -> bool) {
    HHDM.store(hhdm, Ordering::Relaxed);
    TABLE_OK.store(table_ok as usize, Ordering::Relaxed);
}

#[derive(Clone, Copy, Default)]
pub struct Walk {
    /// Entries read, top level first (5 with LA57, else 4 used).
    pub e: [u64; 5],
    pub levels: usize,
    /// How many entries were read.
    pub depth: usize,
    /// Mapped: size of the leaf page (4 KiB / 2 MiB / 1 GiB); 0 = not mapped.
    pub page: u64,
    /// A table pointer failed the safety predicate (corrupt or odd tables).
    pub bad_table: bool,
}

impl Walk {
    /// Bytes covered by the entry at `depth - 1` (where the walk stopped).
    pub fn span(&self) -> u64 {
        let lvl_from_bottom = self.levels - self.depth; // 0 = PT
        1u64 << (12 + 9 * lvl_from_bottom as u64)
    }
}

pub fn walk(va: u64) -> Walk {
    let cr3: u64;
    let cr4: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    }
    let levels = if cr4 & (1 << 12) != 0 { 5 } else { 4 };
    let hhdm = HHDM.load(Ordering::Relaxed);
    let ok = TABLE_OK.load(Ordering::Relaxed);
    let mut w = Walk { levels, ..Walk::default() };
    let mut table = cr3 & 0x000F_FFFF_FFFF_F000;
    for d in 0..levels {
        if ok == 0 || hhdm == 0 || !(unsafe { core::mem::transmute::<usize, fn(u64) -> bool>(ok) })(table) {
            w.bad_table = true;
            return w;
        }
        let shift = 12 + 9 * (levels - 1 - d) as u64;
        let idx = (va >> shift) & 511;
        let e = unsafe { core::ptr::read_volatile((hhdm + table + idx * 8) as *const u64) };
        w.e[d] = e;
        w.depth = d + 1;
        if e & 1 == 0 { return w; }
        let from_bottom = levels - 1 - d;
        if from_bottom == 0 || ((from_bottom == 1 || from_bottom == 2) && e & 0x80 != 0) {
            w.page = 1u64 << shift;
            return w;
        }
        table = e & 0x000F_FFFF_FFFF_F000;
    }
    w
}

/// What `fill_hhdm` did.
#[derive(Clone, Copy, Default)]
pub struct FillStats {
    pub gib: u64,
    pub mib2: u64,
    pub kib4: u64,
    pub tables: u64,
    /// Bytes that could not be mapped (table pool exhausted).
    pub failed: u64,
}

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const PS: u64 = 1 << 7;

/// Map every not-yet-mapped page of `ranges` (physical, 4 KiB aligned,
/// sorted) into the HHDM of the LIVE tables, in place: present entries are
/// never touched (Limine's attributes, the framebuffer's WC, the kernel
/// image stay exactly as they are), only empty slots are filled -- 1 GiB
/// leaves where aligned and `gib_ok`, else 2 MiB, else 4 KiB; leaf flags
/// P|RW (write-back, like Limine's HHDM), table flags P|RW|US (like
/// Limine's). New table pages come from `alloc` (physical, must already be
/// HHDM-mapped); they are zeroed here.
pub unsafe fn fill_hhdm(ranges: &[(u64, u64)], gib_ok: bool, alloc: &mut dyn FnMut() -> Option<u64>) -> FillStats {
    let mut st = FillStats::default();
    let hhdm = HHDM.load(Ordering::Relaxed);
    let cr3: u64;
    let cr4: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    let levels = if cr4 & (1 << 12) != 0 { 5usize } else { 4 };
    let root = cr3 & ADDR_MASK;
    let ent = |table: u64, va: u64, shift: u64| (hhdm + table + ((va >> shift) & 511) * 8) as *mut u64;

    for &(s, e) in ranges {
        let mut a = s;
        while a < e {
            let va = hhdm + a;
            // Walk down; at each level either find a present leaf (already
            // mapped: skip its whole extent), an empty slot to fill with a
            // leaf of this level's size, or a table to descend into.
            let mut table = root;
            let mut advanced = false;
            for d in 0..levels {
                let from_bottom = levels - 1 - d; // 0 = PT
                let shift = 12 + 9 * from_bottom as u64;
                let size = 1u64 << shift;
                let p = ent(table, va, shift);
                let v = core::ptr::read_volatile(p);
                if v & 1 != 0 {
                    if from_bottom == 0 || ((from_bottom == 1 || from_bottom == 2) && v & PS != 0) {
                        a = (a & !(size - 1)) + size; // mapped already
                        advanced = true;
                        break;
                    }
                    table = v & ADDR_MASK;
                    continue;
                }
                // Empty slot. A leaf here if this level can hold one and the
                // whole leaf lies inside the range.
                let leaf_ok = match from_bottom {
                    0 => true,
                    1 => a % size == 0 && e - a >= size,
                    2 => gib_ok && a % size == 0 && e - a >= size,
                    _ => false,
                };
                if leaf_ok {
                    core::ptr::write_volatile(p, a | 0x3 | if from_bottom > 0 { PS } else { 0 });
                    match from_bottom { 0 => st.kib4 += 1, 1 => st.mib2 += 1, _ => st.gib += 1 }
                    a += size;
                    advanced = true;
                    break;
                }
                // Need a new table below this slot.
                let t = match alloc() { Some(t) => t, None => {
                    st.failed += e - a;
                    a = e;
                    advanced = true;
                    break;
                } };
                core::ptr::write_bytes((hhdm + t) as *mut u8, 0, 4096);
                core::ptr::write_volatile(p, t | 0x7);
                st.tables += 1;
                table = t;
            }
            if !advanced { a += 4096; }
        }
    }
    // Flush everything (no PCID / global HHDM entries to worry about here).
    core::arch::asm!("mov {0}, cr3", "mov cr3, {0}", inout(reg) cr3 => _, options(nostack));
    st
}

/// CPUID.80000001h:EDX[26] -- 1 GiB pages.
pub fn gib_pages_supported() -> bool {
    let r = unsafe { core::arch::x86_64::__cpuid(0x8000_0001) };
    r.edx & (1 << 26) != 0
}

/// Test hook (`earlyfb.test=unmap4g`): clear the PDPT-level entry (1 GiB of
/// HHDM, leaf or table) covering `va` -- the HP's exact shape (PDPTE 0).
pub unsafe fn unmap_pdpte(va: u64) -> bool {
    let cr3: u64;
    let cr4: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    let levels = if cr4 & (1 << 12) != 0 { 5usize } else { 4 };
    let hhdm = HHDM.load(Ordering::Relaxed);
    let mut table = cr3 & ADDR_MASK;
    for d in 0..levels - 2 {
        let shift = 12 + 9 * (levels - 1 - d) as u64;
        let p = (hhdm + table + ((va >> shift) & 511) * 8) as *mut u64;
        let v = core::ptr::read_volatile(p);
        if v & 1 == 0 { return false; }
        if d == levels - 3 {
            core::ptr::write_volatile(p, 0);
            core::arch::asm!("mov {0}, cr3", "mov cr3, {0}", inout(reg) cr3 => _, options(nostack));
            return true;
        }
        table = v & ADDR_MASK;
    }
    false
}

/// Test hook (`earlyfb.test=unmap`): clear the leaf entry mapping `va` in the
/// live tables, reproducing the HP's unmapped HHDM top in QEMU. Returns the
/// size of the page that was unmapped (0 = it was not mapped).
pub unsafe fn unmap_leaf(va: u64) -> u64 {
    let w = walk(va);
    if w.page == 0 { return 0; }
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let hhdm = HHDM.load(Ordering::Relaxed);
    let mut table = cr3 & 0x000F_FFFF_FFFF_F000;
    for d in 0..w.depth {
        let shift = 12 + 9 * (w.levels - 1 - d) as u64;
        let p = (hhdm + table + ((va >> shift) & 511) * 8) as *mut u64;
        if d + 1 == w.depth {
            core::ptr::write_volatile(p, 0);
            core::arch::asm!("mov {0}, cr3", "mov cr3, {0}", inout(reg) cr3 => _, options(nostack));
            return w.page;
        }
        table = core::ptr::read_volatile(p) & 0x000F_FFFF_FFFF_F000;
    }
    0
}

/// Load the early IDT (BSP, Limine path, before memory-manager init).
pub fn install() {
    let cs: u16;
    unsafe {
        core::arch::asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack));
        let stubs = &*core::ptr::addr_of!(early_exc_stubs);
        let t = &mut *core::ptr::addr_of_mut!(TABLE);
        for (i, e) in t.0.iter_mut().enumerate() {
            let h = stubs[i];
            *e = Entry { lo: h as u16, sel: cs, ist: 0, attr: 0x8E, mid: (h >> 16) as u16, hi: (h >> 32) as u32, _r: 0 };
        }
        #[repr(C, packed)]
        struct Ptr { limit: u16, base: u64 }
        let p = Ptr { limit: (core::mem::size_of::<Table>() - 1) as u16, base: core::ptr::addr_of!(TABLE) as u64 };
        core::arch::asm!("lidt [{}]", in(reg) &p, options(readonly, nostack));
    }
}
