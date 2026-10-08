//! The many small answers: platform and system info, process identity,
//! capabilities, file-private/events (no device files yet), ACPI, I2C,
//! debug/crash hooks and the NVLink libc wrappers. Values follow
//! nvidia-haiku's os-haiku.cpp unless a comment says otherwise.

use core::ffi::{c_char, c_void, CStr};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{nvlog, NV_ERR_INVALID_ARGUMENT, NV_ERR_NOT_SUPPORTED, NV_ERR_OBJECT_NOT_FOUND, NV_ERR_OPERATING_SYSTEM, NV_OK};

// ------------------------------------------------------------- boot fb --

static FB_PHYS: AtomicU64 = AtomicU64::new(0);
static FB_W: AtomicU32 = AtomicU32::new(0);
static FB_H: AtomicU32 = AtomicU32::new(0);
static FB_PITCH: AtomicU32 = AtomicU32::new(0);

pub(crate) fn set_boot_fb(phys: u64, w: u32, h: u32, pitch: u32) {
    FB_PHYS.store(phys, Ordering::Relaxed);
    FB_W.store(w, Ordering::Relaxed);
    FB_H.store(h, Ordering::Relaxed);
    FB_PITCH.store(pitch, Ordering::Relaxed);
}

static RSDP: AtomicU64 = AtomicU64::new(0);

pub(crate) fn set_rsdp(phys: u64) {
    RSDP.store(phys, Ordering::Relaxed);
}

pub(crate) fn boot_fb() -> (u64, u32, u32, u32) {
    (FB_PHYS.load(Ordering::Relaxed), FB_W.load(Ordering::Relaxed),
     FB_H.load(Ordering::Relaxed), FB_PITCH.load(Ordering::Relaxed))
}

/// The GOP framebuffer, if it lies inside one of this GPU's BARs (Linux
/// `NV_IS_CONSOLE_MAPPED`). RM then reserves the console's VRAM and, when the
/// framebuffer starts at BAR1's base, keeps a BAR1 mapping for it once BAR1
/// goes virtual (`bPreserveBar1ConsoleEnabled`).
#[no_mangle]
pub unsafe extern "C" fn nv_get_screen_info(
    nv: *mut c_void, phys: *mut u64, w: *mut u32, h: *mut u32, depth: *mut u32, pitch: *mut u32, size: *mut u64,
) {
    cb!("nv_get_screen_info");
    *phys = 0;
    *w = 0;
    *h = 0;
    *depth = 0;
    *pitch = 0;
    *size = 0;
    let (fb, fw, fh, fp) = boot_fb();
    if fb == 0 || nv.is_null() {
        return;
    }
    let mut inside = false;
    for i in 0..3 {
        let (mut base, mut len) = (0u64, 0u64);
        crate::glue::lnv_state_bar(nv as _, i, &mut base, &mut len);
        if len != 0 && fb >= base && fb < base + len {
            inside = true;
        }
    }
    nvlog!("[nvrm] +{}ms nv_get_screen_info: GOP fb {:#x} {}x{} pitch {} -> {}\n", crate::ms(), fb, fw, fh, fp,
           if inside { "on this GPU (reported)" } else { "not in this GPU's BARs (none reported)" });
    if inside {
        *phys = fb;
        *w = fw;
        *h = fh;
        *depth = 32;
        *pitch = fp;
        *size = fp as u64 * fh as u64;
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_disable_console_access() {
    cb!("os_disable_console_access");
    nvlog!("[nvrm] +{}ms os_disable_console_access (RM is about to touch the console's display state)\n", crate::ms());
}

#[no_mangle]
pub unsafe extern "C" fn os_enable_console_access() {
    cb!("os_enable_console_access");
    nvlog!("[nvrm] +{}ms os_enable_console_access\n", crate::ms());
}

// ------------------------------------------------------- platform / info --

#[no_mangle]
pub unsafe extern "C" fn os_get_cpu_count() -> u32 {
    cb!("os_get_cpu_count");
    sched::active_cpu_count().max(1) as u32
}

#[no_mangle]
pub unsafe extern "C" fn os_get_cpu_number() -> u32 {
    cb!("os_get_cpu_number");
    sched::cpu_id() as u32
}

/// Limine boots LeandrOS through UEFI on the target PC.
#[no_mangle]
pub unsafe extern "C" fn os_is_efi_enabled() -> u8 {
    cb!("os_is_efi_enabled");
    1
}

/// The ACPI RSDP from the UEFI configuration table (Limine hands it over),
/// as Linux returns `efi.acpi20`. RM's chipset code (clStorePcieConfig-
/// SpaceBaseFromMcfg) walks RSDP -> XSDT -> MCFG with os_map_kernel_space
/// and maps PCIe extended config space of the upstream bridge itself; without
/// it RM logs "clCheckUpstreamLtrSupport_IMPL: PCIE config space is
/// inaccessible!" and asserts in clPcieReadWord. The interface is 32-bit:
/// an RSDP above 4 GiB cannot be reported (UEFI keeps it low in practice).
/// `nvrm.nomcfg=1` withholds it.
#[no_mangle]
pub unsafe extern "C" fn os_get_acpi_rsdp_from_uefi(rsdp: *mut u32) -> u32 {
    cb!("os_get_acpi_rsdp_from_uefi");
    if rsdp.is_null() {
        return crate::NV_ERR_INVALID_ARGUMENT;
    }
    *rsdp = 0;
    let phys = RSDP.load(Ordering::Relaxed);
    if phys == 0 || phys > u32::MAX as u64 {
        nvlog!("[nvrm] +{}ms os_get_acpi_rsdp_from_uefi: {} (RM cannot reach the upstream bridge's PCIe config space)\n",
               crate::ms(), if phys == 0 { "no RSDP (or nvrm.nomcfg=1)" } else { "RSDP above 4 GiB" });
        return NV_ERR_OPERATING_SYSTEM;
    }
    *rsdp = phys as u32;
    nvlog!("[nvrm] +{}ms os_get_acpi_rsdp_from_uefi: RSDP at {:#x}\n", crate::ms(), phys);
    NV_OK
}

/// Walk RSDP -> XSDT/RSDT -> MCFG the way RM's chipset code does
/// (GetRsdtXsdtTablesAddr + clStorePcieConfigSpaceBaseFromMcfg: cached
/// os_map_kernel_space mappings, signature and checksum checks) and log what
/// RM will find. Device independent; the `abi` self-test step runs it.
/// Returns the number of MCFG allocations (ECAM segments) found.
pub(crate) fn probe_mcfg_like_rm() -> usize {
    use crate::mem::{os_map_kernel_space, os_unmap_kernel_space, NV_MEMORY_CACHED};
    let mut rsdp32 = 0u32;
    if unsafe { os_get_acpi_rsdp_from_uefi(&mut rsdp32) } != NV_OK {
        return 0;
    }
    unsafe fn map(phys: u64, len: u64) -> Option<*const u8> {
        let p = os_map_kernel_space(phys, len, NV_MEMORY_CACHED) as *const u8;
        if p.is_null() { None } else { Some(p) }
    }
    unsafe fn sum(p: *const u8, n: usize) -> u8 {
        (0..n).fold(0u8, |a, i| a.wrapping_add(p.add(i).read_volatile()))
    }
    unsafe fn rd32(p: *const u8) -> u32 { (p as *const u32).read_unaligned() }
    unsafe fn rd64(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }
    unsafe {
        let Some(r) = map(rsdp32 as u64, 0x24) else { return 0 };
        let sig_ok = core::slice::from_raw_parts(r, 8) == b"RSD PTR ";
        let rev = r.add(15).read();
        let (xsdt, rsdt) = (rd64(r.add(24)), rd32(r.add(16)));
        let ck = sum(r, 20);
        os_unmap_kernel_space(r as *mut c_void, 0x24);
        nvlog!("[nvrm] acpi: RSDP {:#x}: signature {}, checksum {}, revision {}, XSDT {:#x}, RSDT {:#x}\n", rsdp32,
               if sig_ok { "ok" } else { "BAD" }, if ck == 0 { "ok" } else { "BAD" }, rev, xsdt, rsdt);
        if !sig_ok || ck != 0 {
            return 0;
        }
        let (root, esz) = if rev > 1 && xsdt != 0 { (xsdt, 8u64) } else { (rsdt as u64, 4u64) };
        let Some(h) = map(root, 36) else { return 0 };
        let len = rd32(h.add(4)) as u64;
        os_unmap_kernel_space(h as *mut c_void, 36);
        let Some(t) = map(root, len) else { return 0 };
        let mut mcfg = 0u64;
        for i in 0..(len.saturating_sub(36) / esz) {
            let e = t.add(36 + (i * esz) as usize);
            let a = if esz == 8 { rd64(e) } else { rd32(e) as u64 };
            let Some(th) = map(a, 8) else { continue };
            let sig = rd32(th);
            os_unmap_kernel_space(th as *mut c_void, 8);
            if sig == u32::from_le_bytes(*b"MCFG") {
                mcfg = a;
                break;
            }
        }
        os_unmap_kernel_space(t as *mut c_void, len);
        if mcfg == 0 {
            nvlog!("[nvrm] acpi: no MCFG table under {:#x}\n", root);
            return 0;
        }
        let Some(mh) = map(mcfg, 8) else { return 0 };
        let mlen = rd32(mh.add(4)) as u64;
        os_unmap_kernel_space(mh as *mut c_void, 8);
        let Some(m) = map(mcfg, mlen) else { return 0 };
        let ck = sum(m, mlen as usize);
        let n = (mlen.saturating_sub(44) / 16) as usize;
        for i in 0..n {
            let e = m.add(44 + i * 16);
            nvlog!("[nvrm] acpi: MCFG {:#x} (checksum {}): ECAM base {:#x} segment {} buses {:#x}-{:#x}\n", mcfg,
                   if ck == 0 { "ok" } else { "BAD" }, rd64(e), (e.add(8) as *const u16).read_unaligned(),
                   e.add(10).read(), e.add(11).read());
        }
        os_unmap_kernel_space(m as *mut c_void, mlen);
        if ck == 0 { n } else { 0 }
    }
}

/// Fabric-manager capability (NVSwitch / IMEX systems): a single GeForce
/// has no fabric, so there is nothing to grant.
#[no_mangle]
pub unsafe extern "C" fn nv_acquire_fabric_mgmt_cap(fd: i32, duped_fd: *mut i32) -> u32 {
    cb!("nv_acquire_fabric_mgmt_cap");
    if !duped_fd.is_null() {
        *duped_fd = -1;
    }
    nvlog!("[nvrm] +{}ms nv_acquire_fabric_mgmt_cap(fd {}): no fabric manager on this system\n", crate::ms(), fd);
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn os_is_nvswitch_present() -> u8 {
    cb!("os_is_nvswitch_present");
    0
}

#[no_mangle]
pub unsafe extern "C" fn os_is_vgx_hyper() -> u8 {
    cb!("os_is_vgx_hyper");
    0
}

/// nvidia-haiku answers false (RM then avoids PAT-dependent WC paths).
#[no_mangle]
pub unsafe extern "C" fn os_pat_supported() -> u8 {
    cb!("os_pat_supported");
    0
}

#[no_mangle]
pub unsafe extern "C" fn os_is_grid_supported() -> u8 {
    cb!("os_is_grid_supported");
    0
}

#[no_mangle]
pub unsafe extern "C" fn os_get_grid_csp_support() -> u32 {
    cb!("os_get_grid_csp_support");
    0
}

/// First GPU = minor 0, as /dev/nvidia0 on Linux.
#[no_mangle]
pub unsafe extern "C" fn nv_get_dev_minor(_nv: *mut c_void) -> u32 {
    cb!("nv_get_dev_minor");
    0
}

#[no_mangle]
pub unsafe extern "C" fn nv_is_gpu_accessible(_nv: *mut c_void) -> u8 {
    cb!("nv_is_gpu_accessible");
    1
}

/// "OS does not know": RM derives primary-VGA status from config space.
#[no_mangle]
pub unsafe extern "C" fn nv_set_primary_vga_status(_nv: *mut c_void) -> u32 {
    cb!("nv_set_primary_vga_status");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn os_device_vm_present() -> u32 {
    cb!("os_device_vm_present");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_get_syncpoint_aperture(_id: u32, _phys: *mut u64, _limit: *mut u64, _off: *mut u32) -> u32 {
    cb!("nv_get_syncpoint_aperture");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_audio_dynamic_power(_nv: *mut c_void) {
    cb!("nv_audio_dynamic_power");
}

#[no_mangle]
pub unsafe extern "C" fn nv_is_chassis_notebook() -> u8 {
    cb!("nv_is_chassis_notebook");
    0
}

#[no_mangle]
pub unsafe extern "C" fn os_get_random_bytes(buf: *mut u8, len: u16) -> u32 {
    cb!("os_get_random_bytes");
    if buf.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    sched::random::fill(core::slice::from_raw_parts_mut(buf, len as usize));
    NV_OK
}

// --------------------------------------------------------- debug / crash --

#[no_mangle]
pub unsafe extern "C" fn os_add_record_for_crashLog(_buf: *mut c_void, _size: u32) {
    cb!("os_add_record_for_crashLog");
}

#[no_mangle]
pub unsafe extern "C" fn os_delete_record_for_crashLog(_buf: *mut c_void) {
    cb!("os_delete_record_for_crashLog");
}

#[no_mangle]
pub unsafe extern "C" fn os_dump_stack() {
    cb!("os_dump_stack");
    nvlog!("[nvrm] +{}ms os_dump_stack requested (pid {}, {} context)\n", crate::ms(), sched::current_pid(), crate::trace::context());
    crate::trace::dump_last();
}

// ------------------------------------------------------ process identity --

#[no_mangle]
pub unsafe extern "C" fn os_get_current_process() -> u32 {
    cb!("os_get_current_process");
    if sched::irqctx::in_irq() { 0 } else { sched::current_tgid() as u32 }
}

#[no_mangle]
pub unsafe extern "C" fn os_get_current_process_name(buf: *mut c_char, len: u32) {
    cb!("os_get_current_process_name");
    if buf.is_null() || len == 0 {
        return;
    }
    let out = core::slice::from_raw_parts_mut(buf as *mut u8, len as usize);
    let mut n = 0;
    if !sched::irqctx::in_irq() {
        let mut tmp = [0u8; 64];
        if let Some(k) = sched::exe_path(sched::current_tgid(), &mut tmp) {
            let k = k.min(tmp.len());
            let name = tmp[..k].rsplit(|&c| c == b'/').next().unwrap_or(&tmp[..k]);
            n = name.len().min(out.len() - 1);
            out[..n].copy_from_slice(&name[..n]);
        }
    }
    out[n] = 0;
}

#[no_mangle]
pub unsafe extern "C" fn os_get_current_thread(tid: *mut u64) -> u32 {
    cb!("os_get_current_thread");
    *tid = sched::current_pid() as u64;
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_get_euid(tok: *mut u32) -> u32 {
    cb!("os_get_euid");
    *tok = if sched::irqctx::in_irq() { 0 } else { sched::current_euid() };
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_is_administrator() -> u8 {
    cb!("os_is_administrator");
    (sched::irqctx::in_irq() || sched::current_euid() == 0) as u8
}

/// RS_ACCESS_NICE (1) / RS_ACCESS_PERFMON (3) need admin; others refused.
#[no_mangle]
pub unsafe extern "C" fn os_check_access(right: u16) -> u8 {
    cb!("os_check_access");
    match right {
        1 | 3 => os_is_administrator(),
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_is_init_ns() -> u8 {
    cb!("os_is_init_ns");
    0
}

/// A non-NULL cookie (nvidia-haiku: malloc(1)); pid namespaces do not exist.
#[no_mangle]
pub unsafe extern "C" fn os_get_pid_info() -> *mut c_void {
    cb!("os_get_pid_info");
    alloc::boxed::Box::into_raw(alloc::boxed::Box::new(0u64)) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_put_pid_info(p: *mut c_void) {
    cb!("os_put_pid_info");
    if !p.is_null() {
        drop(alloc::boxed::Box::from_raw(p as *mut u64));
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_find_ns_pid(info: *mut c_void, ns_pid: *mut u32) -> u32 {
    cb!("os_find_ns_pid");
    if info.is_null() || ns_pid.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    *ns_pid = sched::current_tgid() as u32;
    if *ns_pid == 0 { NV_ERR_OBJECT_NOT_FOUND } else { NV_OK }
}

// -------------------------------------------------------------- caps ----
// No /dev/nvidia-caps: entries are opaque non-NULL tokens (nvidia-haiku).

#[no_mangle]
pub unsafe extern "C" fn os_nv_cap_create_dir_entry(_parent: *mut c_void, _name: *const c_char, _mode: i32) -> *mut c_void {
    cb!("os_nv_cap_create_dir_entry");
    alloc::boxed::Box::into_raw(alloc::boxed::Box::new(0u64)) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_nv_cap_create_file_entry(_parent: *mut c_void, _name: *const c_char, _mode: i32) -> *mut c_void {
    cb!("os_nv_cap_create_file_entry");
    alloc::boxed::Box::into_raw(alloc::boxed::Box::new(0u64)) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_nv_cap_destroy_entry(cap: *mut c_void) {
    cb!("os_nv_cap_destroy_entry");
    if !cap.is_null() {
        drop(alloc::boxed::Box::from_raw(cap as *mut u64));
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_nv_cap_validate_and_dup_fd(_cap: *const c_void, _fd: i32) -> i32 {
    cb!("os_nv_cap_validate_and_dup_fd");
    -1
}

#[no_mangle]
pub unsafe extern "C" fn os_nv_cap_close_fd(_fd: i32) {
    cb!("os_nv_cap_close_fd");
}

// ------------------------------------------- file-private / events -------
// There are no NVIDIA device files yet (rm_ioctl comes with the DRM lane).

#[no_mangle]
pub unsafe extern "C" fn nv_get_file_private(_fd: i32, _ctl: u8, _os_priv: *mut *mut c_void) -> *mut c_void {
    cb!("nv_get_file_private");
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn nv_put_file_private(_os_priv: *mut c_void) {
    cb!("nv_put_file_private");
}

#[no_mangle]
pub unsafe extern "C" fn nv_post_event(_event: *mut c_void, _h: u32, _index: u32, _info32: u32, _info16: u16, _valid: u8) {
    cb!("nv_post_event");
}

// ---------------------------------------------------------------- ACPI --
// No ACPI interpreter: every method is NOT_SUPPORTED (as nvidia-haiku).

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_methods_init(present: *mut u32) {
    cb!("nv_acpi_methods_init");
    if !present.is_null() {
        *present = 0;
    }
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_methods_uninit() {
    cb!("nv_acpi_methods_uninit");
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_dsm_method(
    _nv: *mut c_void, _guid: *mut u8, _rev: u32, _nvpcf: u8, _sub: u32, _in: *mut c_void, _insz: u16,
    _status: *mut u32, _out: *mut c_void, _size: *mut u16,
) -> u32 {
    cb!("nv_acpi_dsm_method");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_ddc_method(_nv: *mut c_void, _edid: *mut c_void, _size: *mut u32, _multi: u8) -> u32 {
    cb!("nv_acpi_ddc_method");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_rom_method(_nv: *mut c_void, _in: *mut u32, _out: *mut u32) -> u32 {
    cb!("nv_acpi_rom_method");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_dod_method(_nv: *mut c_void, _out: *mut u32, _size: *mut u32) -> u32 {
    cb!("nv_acpi_dod_method");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_get_powersource(_ac: *mut u32) -> u32 {
    cb!("nv_acpi_get_powersource");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_mux_method(_nv: *mut c_void, _inout: *mut u32, _id: u32, _name: *const c_char) -> u32 {
    cb!("nv_acpi_mux_method");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_is_battery_present() -> u8 {
    cb!("nv_acpi_is_battery_present");
    0
}

// ----------------------------------------------------------------- I2C --

#[no_mangle]
pub unsafe extern "C" fn nv_i2c_add_adapter(_nv: *mut c_void, _port: u32) -> *mut c_void {
    cb!("nv_i2c_add_adapter");
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn nv_i2c_del_adapter(_nv: *mut c_void, _data: *mut c_void) {
    cb!("nv_i2c_del_adapter");
}

// ------------------------------------------------- NVLink libc wrappers --

#[no_mangle]
pub unsafe extern "C" fn nvlink_malloc(size: u64) -> *mut c_void {
    cb!("nvlink_malloc");
    let mut p = core::ptr::null_mut();
    if crate::basic::os_alloc_mem(&mut p, size) != NV_OK {
        return core::ptr::null_mut();
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_free(p: *mut c_void) {
    cb!("nvlink_free");
    crate::basic::os_free_mem(p);
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_memset(d: *mut c_void, v: i32, n: u64) -> *mut c_void {
    cb!("nvlink_memset");
    core::ptr::write_bytes(d as *mut u8, v as u8, n as usize);
    d
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_memcpy(d: *mut c_void, s: *const c_void, n: u64) -> *mut c_void {
    cb!("nvlink_memcpy");
    core::ptr::copy(s as *const u8, d as *mut u8, n as usize);
    d
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_strcpy(d: *mut c_char, s: *const c_char) -> *mut c_char {
    cb!("nvlink_strcpy");
    let n = CStr::from_ptr(s).to_bytes_with_nul().len();
    core::ptr::copy_nonoverlapping(s, d, n);
    d
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_strcmp(a: *const c_char, b: *const c_char) -> i32 {
    cb!("nvlink_strcmp");
    match CStr::from_ptr(a).to_bytes().cmp(CStr::from_ptr(b).to_bytes()) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_strlen(s: *const c_char) -> u64 {
    cb!("nvlink_strlen");
    CStr::from_ptr(s).to_bytes().len() as u64
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_assert(cond: i32) {
    cb!("nvlink_assert");
    if cond == 0 {
        nvlog!("[nvrm] nvlink_assert failed\n");
    }
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_sleep(ms: u32) {
    cb!("nvlink_sleep");
    crate::time::os_delay(ms);
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_get_platform_time() -> u64 {
    cb!("nvlink_get_platform_time");
    sched::monotonic_ns()
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_allocLock() -> *mut c_void {
    cb!("nvlink_allocLock");
    let mut m = core::ptr::null_mut();
    if crate::sync::os_alloc_mutex(&mut m) != NV_OK {
        return core::ptr::null_mut();
    }
    m
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_acquireLock(m: *mut c_void) {
    cb!("nvlink_acquireLock");
    crate::sync::os_acquire_mutex(m);
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_releaseLock(m: *mut c_void) {
    cb!("nvlink_releaseLock");
    crate::sync::os_release_mutex(m);
}

#[no_mangle]
pub unsafe extern "C" fn nvlink_freeLock(m: *mut c_void) {
    cb!("nvlink_freeLock");
    crate::sync::os_free_mutex(m);
}
