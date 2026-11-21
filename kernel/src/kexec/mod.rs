//! `kexec_file_load`: stage a `kernel-direct` image for a later kexec.
//!
//! Load phase (lane kexecload) and the jump (`reboot(KEXEC)`, lane
//! kexecexec: [`exec_prepare`] + [`exec_jump`]; x86_64 in lane kexecx86).
//! Design: `artifacts/notes/kexec/`.
//!
//! The image is read from the file descriptors into buddy pages, described
//! by a Linux-style indirection list (see `stage`). Nothing is copied to its
//! destination until the jump.

mod elf;
#[cfg(target_arch = "aarch64")]
mod exec_aarch64;
#[cfg(target_arch = "x86_64")]
mod exec_x86_64;
mod info;
pub mod devreset;
pub mod prevlog;
mod stage;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use stage::{Alloc, List};

pub const EPERM: isize = -1;
pub const EIO: isize = -5;
pub const ENOEXEC: isize = -8;
pub const EBADF: isize = -9;
pub const ENOMEM: isize = -12;
pub const EFAULT: isize = -14;
pub const EBUSY: isize = -16;
pub const EINVAL: isize = -22;
pub const EADDRNOTAVAIL: isize = -99;

pub const KEXEC_FILE_UNLOAD: usize = 1;
pub const KEXEC_FILE_NO_INITRAMFS: usize = 4;

#[cfg(target_arch = "x86_64")]
const INITRD_PHYS: u64 = 0x1000_0000;
// Where the next kernel looks for its initrd: the Pi 5 kernel probes
// 0x1000_0000 (the firmware's `initramfs ... 0x10000000`, main.rs) and would
// otherwise find the OLD initrd still sitting there.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
const INITRD_PHYS: u64 = 0x1000_0000;
#[cfg(all(target_arch = "aarch64", not(feature = "rpi5")))]
const INITRD_PHYS: u64 = 0x4800_0000;

#[cfg(target_arch = "x86_64")]
const MB2_BOOTLOADER_MAGIC: u64 = 0x36d7_6289;

/// Includes the NUL, as `kexec_file_load` passes it (Linux COMMAND_LINE_SIZE).
const CMDLINE_MAX: usize = boot::COMMAND_LINE_SIZE;

/// A staged image.
struct Image {
    entry: u64,
    bootinfo: u64,
    head: u64,
    regs: [u64; 2],
    footprint: Vec<(u64, u64)>,
    /// The final-log buffer handed to the next kernel (`prevlog`).
    prevlog: u64,
    /// Every buddy block we own (freed on unload).
    blocks: Vec<(usize, usize)>,
}

static STATE: spin::Mutex<Option<Image>> = spin::Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);
/// `kexec.test=acpiresvd` on the cmdline being loaded: hand the next kernel its
/// ACPI-table ranges as raw type 8 (firmware Reserved), simulating firmware that
/// keeps tables there (x86_64 fail-first test, kexecacpitest.py).
static ACPI_RESVD: AtomicBool = AtomicBool::new(false);

/// What the jump needs.
#[derive(Clone)]
pub struct Loaded {
    /// Physical entry point of the new kernel.
    pub entry: u64,
    /// Physical address of the MBI (x86_64) or FDT (aarch64).
    pub bootinfo: u64,
    /// Physical address of the first indirection page.
    pub head: u64,
    /// Registers to set: x86_64 `[EAX, EBX]` = `[0x36d76289, MBI]`;
    /// aarch64 `[x0, 0]` = `[FDT, 0]`.
    pub regs: [u64; 2],
    /// Every physical range the copy writes, plus the boot info (cache clean).
    pub footprint: Vec<(u64, u64)>,
    /// Physical address of the final-log buffer (`prevlog`), 0 = none.
    pub prevlog: u64,
}

/// The staged image, if any.
pub fn loaded() -> Option<Loaded> {
    STATE.lock().as_ref().map(|i| Loaded {
        entry: i.entry,
        bootinfo: i.bootinfo,
        head: i.head,
        regs: i.regs,
        footprint: i.footprint.clone(),
        prevlog: i.prevlog,
    })
}

fn free_image(img: Image) {
    for (a, o) in img.blocks { mm::buddy::free(a, o); }
}

fn hex(v: u64) -> alloc::string::String { alloc::format!("{:#x}", v) }

pub fn sys_kexec_file_load(kernel_fd: usize, initrd_fd: usize, cmdline_len: usize,
                           cmdline_ptr: usize, flags: usize) -> isize {
    if !sched::caps::host_capable(sched::caps::CAP_SYS_BOOT) { return EPERM; }
    let flags = flags & 0xffff_ffff;
    if flags & !(KEXEC_FILE_UNLOAD | KEXEC_FILE_NO_INITRAMFS) != 0 { return EINVAL; }

    if flags & KEXEC_FILE_UNLOAD != 0 {
        if let Some(img) = STATE.lock().take() { free_image(img); }
        crate::serial_print_str("[KEXEC] unloaded\n");
        return 0;
    }

    // Linux passes `int` fds; compare the low 32 bits sign-extended.
    let kfd = kernel_fd as u32 as i32;
    if kfd < 0 { return EBADF; }
    let initrd = if flags & KEXEC_FILE_NO_INITRAMFS != 0 { None } else {
        let f = initrd_fd as u32 as i32;
        if f < 0 { return EBADF; }
        Some(f as usize)
    };

    let mut cmdline: Vec<u8> = Vec::new();
    if cmdline_len > 0 {
        if cmdline_len > CMDLINE_MAX { return EINVAL; }
        cmdline.resize(cmdline_len, 0);
        if unsafe { sched::uaccess::copy_from_user(cmdline.as_mut_ptr(), cmdline_ptr, cmdline_len) } != 0 {
            return EFAULT;
        }
        // Linux requires the string to be NUL terminated.
        if cmdline[cmdline_len - 1] != 0 { return EINVAL; }
        cmdline.pop();
    }

    if BUSY.swap(true, Ordering::Acquire) { return EBUSY; }
    let r = do_load(kfd as usize, initrd, &cmdline);
    BUSY.store(false, Ordering::Release);
    match r {
        Ok(img) => {
            let old = STATE.lock().replace(img);
            if let Some(o) = old { free_image(o); }
            0
        }
        Err(e) => e,
    }
}

fn do_load(kfd: usize, initrd_fd: Option<usize>, cmdline: &[u8]) -> Result<Image, isize> {
    let bi_ptr = crate::BOOT_INFO_PTR.load(Ordering::SeqCst);
    if bi_ptr == 0 { return Err(EINVAL); }
    let bi = unsafe { &*(bi_ptr as *const boot::BootInfo) };

    ACPI_RESVD.store(
        core::str::from_utf8(cmdline).map_or(false, |c| c.split_ascii_whitespace().any(|t| t.trim_end_matches('\0') == "kexec.test=acpiresvd")),
        Ordering::SeqCst);
    let parsed = elf::parse(kfd)?;
    let map = info::raw_map(bi);
    let ram = info::loadable(&map);
    #[cfg(target_arch = "x86_64")]
    let ram = info::with_own_image(bi, ram);

    // Destination ranges (page rounded): segments incl. BSS, then the initrd.
    // Segments are staged in ascending paddr order; byte ranges must not
    // overlap, but neighbours may share a page, so their page ranges merge.
    let mut segs = parsed.segs.clone();
    segs.sort_by_key(|s| s.paddr);
    for w in segs.windows(2) {
        if w[0].paddr + w[0].memsz > w[1].paddr { return Err(EINVAL); }
    }
    let mut footprint: Vec<(u64, u64)> = Vec::new();
    for s in &segs {
        #[cfg(target_arch = "x86_64")]
        if s.paddr + s.memsz > (2u64 << 30) { return Err(EINVAL); } // direct HHDM is 0-2 GiB
        let (st, en) = (s.paddr & !0xfff, (s.paddr + s.memsz + 0xfff) & !0xfff);
        match footprint.last_mut() {
            Some(l) if st <= l.1 => { if en > l.1 { l.1 = en; } }
            _ => footprint.push((st, en)),
        }
    }
    let nseg = footprint.len();
    let initrd_len = match initrd_fd {
        Some(fd) => {
            let sz = crate::syscall::sys_lseek_pub(fd, 0, 2);
            if sz < 0 { return Err(sz); }
            let r = crate::syscall::sys_lseek_pub(fd, 0, 0);
            if r < 0 { return Err(r); }
            sz as u64
        }
        None => 0,
    };
    if initrd_len != 0 {
        let (st, en) = (INITRD_PHYS, (INITRD_PHYS + initrd_len + 0xfff) & !0xfff);
        if footprint.iter().any(|&(a, b)| st < b && a < en) { return Err(EINVAL); }
        footprint.push((st, en));
    }
    // Every destination page must be RAM the old kernel can give up.
    for &(s, e) in &footprint {
        if !info::within(&ram, s, e) {
            crate::serial_print_str(&alloc::format!(
                "[KEXEC] destination {}-{} is not usable RAM\n", hex(s), hex(e)));
            return Err(EADDRNOTAVAIL);
        }
    }

    // ...and none may land on a firmware-reserved range (BL31 on the Pi 5).
    #[cfg(target_arch = "aarch64")]
    {
        let g = crate::DTB_RESERVED.lock();
        for &(s, e) in &footprint {
            if let Some(&(rs, re)) = g.0[..g.1].iter().find(|&&(rs, re)| s < re && rs < e) {
                crate::serial_print_str(&alloc::format!(
                    "[KEXEC] destination {}-{} overlaps DTB reserved {}-{}\n", hex(s), hex(e), hex(rs), hex(re)));
                return Err(EADDRNOTAVAIL);
            }
        }
    }
    let mut forbidden = footprint.clone();
    #[cfg(target_arch = "x86_64")]
    forbidden.push((0, 0x10_0000));
    let mut a = Alloc::new(forbidden);

    let res = stage_all(&mut a, &segs, parsed.entry, kfd, initrd_fd, initrd_len, &footprint, nseg, cmdline, bi, &map);
    match res {
        Ok(img) => {
            a.release_held();
            Ok(img)
        }
        Err(e) => {
            a.release_all();
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_all(a: &mut Alloc, segs: &[elf::Seg], entry: u64, kfd: usize, initrd_fd: Option<usize>,
             initrd_len: u64, footprint: &[(u64, u64)], nseg: usize, cmdline: &[u8],
             bi: &boot::BootInfo, map: &[info::Raw]) -> Result<Image, isize> {
    let mut list = List::new(a)?;
    let mut st = stage::Stager::new();
    for s in segs {
        st.file(a, &mut list, kfd, s.offset, s.filesz, s.paddr)?;
    }
    st.finish(a, &mut list)?;
    if let Some(fd) = initrd_fd {
        if initrd_len != 0 {
            st.file(a, &mut list, fd, 0, initrd_len, INITRD_PHYS)?;
            st.finish(a, &mut list)?;
        }
    }
    list.done(a)?;

    // The old kernel's final log (prevlog.rs): a block outside every
    // destination, in RAM the next kernel maps early (below 4 GiB on x86_64;
    // aarch64 like the FDT block).
    #[cfg(target_arch = "x86_64")]
    let prevlog = a.block(prevlog::ORDER, Some(4 << 30))? as u64;
    #[cfg(target_arch = "aarch64")]
    let prevlog = a.block(prevlog::ORDER, Some(8 << 30))? as u64;

    let inp = info::Inputs {
        prevlog: (prevlog, prevlog::SIZE as u64),
        cmdline,
        initrd: if initrd_len != 0 { Some((INITRD_PHYS, initrd_len)) } else { None },
        footprint,
    };
    let bootinfo = info::build(bi, map, &inp, a)?;

    let mut fp: Vec<(u64, u64)> = footprint.to_vec();
    #[cfg(target_arch = "aarch64")]
    fp.push((bootinfo, bootinfo + (4096u64 << 2)));

    #[cfg(target_arch = "x86_64")]
    let regs = [MB2_BOOTLOADER_MAGIC, bootinfo];
    #[cfg(target_arch = "aarch64")]
    let regs = [bootinfo, 0];

    let pages: usize = a.owned.iter().map(|&(_, o)| 1usize << o).sum();
    let blocks = core::mem::take(&mut a.owned);
    crate::serial_print_str(&alloc::format!(
        "[KEXEC] loaded: {} segments{}, entry={} bootinfo={} head={} pages={} (src={} ind={}) cmdline={}B\n",
        nseg,
        if initrd_len != 0 { alloc::format!(" + initrd {}B at {}", initrd_len, hex(INITRD_PHYS)) }
        else { alloc::string::String::new() },
        hex(entry), hex(bootinfo), hex(list.head), pages, list.sources, list.ind_pages,
        cmdline.len()));
    for (i, &(s, e)) in footprint.iter().enumerate() {
        crate::serial_print_str(&alloc::format!(
            "[KEXEC]   dest {}: {}-{}\n", if i < nseg { "seg" } else { "initrd" }, hex(s), hex(e)));
    }
    crate::serial_print_str(&alloc::format!("[KEXEC]   final-log buffer: {}-{}\n",
        hex(prevlog), hex(prevlog + prevlog::SIZE as u64)));
    Ok(Image { entry, bootinfo, head: list.head, regs, footprint: fp, prevlog, blocks })
}

#[allow(dead_code)]
pub const EOPNOTSUPP: isize = -95;

/// First half of `reboot(KEXEC)`, task context, before anything is shut
/// down: EINVAL when no image is loaded, else build what the jump needs.
/// On success the image is pinned (no further load/unload).
pub fn exec_prepare() -> Result<(), isize> {
    if BUSY.swap(true, Ordering::Acquire) { return Err(EBUSY); }
    let r = (|| {
        let img = loaded().ok_or(EINVAL)?;
        // From here every kernel console line is kept for the next kernel.
        prevlog::start(img.prevlog);
        let bi_ptr = crate::BOOT_INFO_PTR.load(Ordering::SeqCst);
        if bi_ptr == 0 { return Err(EINVAL); }
        let bi = unsafe { &*(bi_ptr as *const boot::BootInfo) };
        let map = info::raw_map(bi);
        #[cfg(target_arch = "aarch64")]
        { exec_aarch64::prepare(&img, &map) }
        #[cfg(target_arch = "x86_64")]
        { exec_x86_64::prepare(&img, &map) }
    })();
    if r.is_err() { prevlog::cancel(); BUSY.store(false, Ordering::Release); }
    r
}

/// The framebuffer the staged MBI hands over (x86_64), recomputed the way
/// `info::build` decided it, for the device-reset plan.
pub(crate) fn acpi_resvd() -> bool { ACPI_RESVD.load(Ordering::SeqCst) }

pub fn handover_fb() -> Option<u64> {
    let bi_ptr = crate::BOOT_INFO_PTR.load(Ordering::SeqCst);
    if bi_ptr == 0 { return None; }
    let bi = unsafe { &*(bi_ptr as *const boot::BootInfo) };
    let map = info::raw_map(bi);
    let top = map.iter().filter(|r| r.raw_type == 0).map(|r| r.base + r.length).max().unwrap_or(0);
    devreset::handover_fb(bi, top)
}

/// Second half: interrupts masked, devices quiesced. Never returns.
#[cfg(target_arch = "aarch64")]
pub fn exec_jump() -> ! { exec_aarch64::stop_and_jump() }
#[cfg(target_arch = "x86_64")]
pub fn exec_jump() -> ! { exec_x86_64::stop_and_jump() }

/// aarch64: the previous LeandrOS kernel left its 16 KiB FDT block (FDT plus
/// AP park page) out of `/memory`, so this kernel's map has a hole there, and
/// every kexec of this kernel would copy the hole on, one more per generation.
/// Once the APs have left the park page, move the FDT to the heap, put the
/// block back into the map as Available and give it to the allocator.
/// No-op for a firmware DTB (no kexec AP mailbox).
#[cfg(target_arch = "aarch64")]
pub fn return_boot_fdt() {
    let mb = boot::device_tree::kexec_ap_mailbox();
    if mb == 0 { return; }
    // Test knob: keep the hole, to show the regression guard failing.
    if crate::cmdline_knob("kexec.keep_fdt_hole") == Some("1") { return; }
    let blk = mb & !0x3fff;
    let end = blk + 0x4000;
    let bi = unsafe { &mut *core::ptr::addr_of_mut!(crate::BOOT_INFO) };
    // Already RAM in this map (e.g. a non-LeandrOS-built FDT)? Nothing to do.
    if bi.memory_regions().iter().any(|r| r.kind == boot::MemoryType::Available
        && r.base < end && blk < r.base + r.length) {
        return;
    }
    // Keep a copy of the FDT: drivers read the recorded DTB at probe time.
    if let Some(d) = unsafe { boot::device_tree::recorded_dtb() } {
        let mut v: Vec<u64> = alloc::vec![0u64; (d.len() + 7) / 8];
        unsafe { core::ptr::copy_nonoverlapping(d.as_ptr(), v.as_mut_ptr() as *mut u8, d.len()); }
        boot::device_tree::set_recorded_dtb(v.as_ptr() as usize);
        core::mem::forget(v);
    }
    let reported = boot::sanitize_stats().map(|(_, r)| r).unwrap_or(0);
    let add = [boot::MemoryRegion { base: blk, length: 0x4000, kind: boot::MemoryType::Available }];
    unsafe { boot::sanitize_boot_info(bi, reported, &add) };
    let n = mm::buddy::add_free_range(blk as usize, end as usize);
    crate::serial_print_str(&alloc::format!(
        "[KEXEC] boot FDT block {:#x}-{:#x} returned to the map ({} pages)\n", blk, end, n));
}
