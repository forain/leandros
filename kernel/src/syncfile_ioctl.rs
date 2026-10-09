//! Sync-file and dma-buf fence ioctls (a child module of `syscall`, kept in
//! its own file). These ioctls create or consume *fds*, so like PRIME they
//! are answered in the syscall layer, which owns the caller's fd table; the
//! fences themselves live in `drivers::sync_file` (see the notes there for
//! the model and for the gate, `drivers::sync_file::enabled`).
//!
//!   DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD  + EXPORT_SYNC_FILE   syncobj -> new sync file
//!   DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE  + IMPORT_SYNC_FILE   sync file -> syncobj payload
//!   DMA_BUF_IOCTL_EXPORT_SYNC_FILE  (dmabuf fd)          implicit fences -> sync file
//!   DMA_BUF_IOCTL_IMPORT_SYNC_FILE  (dmabuf fd)          sync file -> implicit fence
//!   DMA_BUF_IOCTL_SYNC / SET_NAME   (dmabuf fd)          accepted (coherent memory)
//!   SYNC_IOC_MERGE / FILE_INFO / SET_DEADLINE (sync file fd)
//!
//! Everything else — the syncobj fd ioctls without those flags (opaque fds),
//! and every ioctl while the gate is closed — returns `None` and takes the
//! old path, which answers exactly as before (ENOSYS from the DRM server,
//! ENOTTY for a tmpfs/eventfd fd).

use super::*;
use drivers::syncobj_core::Fence;
use drivers::sync_file;

const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: usize = 0xC01064C1;
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: usize = 0xC01064C2;
/// The same two with Linux 6.x's 24-byte `drm_syncobj_handle` (a trailing
/// `u64 point`), which a newer libdrm sends.
const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD_24: usize = 0xC01864C1;
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE_24: usize = 0xC01864C2;
const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1 << 0;
const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1 << 0;

const DMA_BUF_IOCTL_SYNC: usize = 0x4008_6200;
const DMA_BUF_SET_NAME_A: usize = 0x4004_6201;
const DMA_BUF_SET_NAME_B: usize = 0x4008_6201;
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: usize = 0xC008_6202;
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: usize = 0x4008_6203;
const DMA_BUF_SYNC_VALID: u64 = 0x7; // READ | WRITE | END

const SYNC_IOC_MERGE: usize = 0xC030_3E03;
const SYNC_IOC_FILE_INFO: usize = 0xC038_3E04;
const SYNC_IOC_SET_DEADLINE: usize = 0x4010_3E05;

const EINVAL: isize = -22;
const EFAULT: isize = -14;

/// Answer the ioctl if it is one of ours; `None` = not ours, carry on.
pub(super) fn intercept(pid: u32, fd: usize, cmd: usize, arg: usize) -> Option<isize> {
    match cmd {
        DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD | DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD_24 => syncobj_export(pid, fd, arg),
        DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE | DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE_24 => syncobj_import(pid, fd, arg),
        DMA_BUF_IOCTL_SYNC | DMA_BUF_SET_NAME_A | DMA_BUF_SET_NAME_B
        | DMA_BUF_IOCTL_EXPORT_SYNC_FILE | DMA_BUF_IOCTL_IMPORT_SYNC_FILE => dmabuf_ioctl(pid, fd, cmd, arg),
        SYNC_IOC_MERGE | SYNC_IOC_FILE_INFO | SYNC_IOC_SET_DEADLINE => sync_file_ioctl(pid, fd, cmd, arg),
        _ => None,
    }
}

fn open_id_of(pid: u32, fd: usize) -> Option<u32> {
    match vfs::vfs_get_node_kind(pid, fd) {
        Some(vfs::VnodeKind::DynamicDevice { open_id, .. }) => Some(open_id),
        _ => None,
    }
}

fn read_u32(addr: usize) -> u32 { unsafe { (addr as *const u32).read_unaligned() } }
fn read_i32(addr: usize) -> i32 { unsafe { (addr as *const i32).read_unaligned() } }
fn write_i32(addr: usize, v: i32) { unsafe { (addr as *mut i32).write_unaligned(v) } }
fn write_u32(addr: usize, v: u32) { unsafe { (addr as *mut u32).write_unaligned(v) } }

/// The fence a sync-file fd holds; `None` if `fd` is not a sync file.
fn fence_of_fd(pid: u32, fd: i32) -> Option<Fence> {
    if fd < 0 { return None; }
    vfs::eventfd_sync_token(pid, fd as usize).map(sync_file::fence_of)
}

/// Mint a sync-file fd holding `fence` (O_CLOEXEC, as every Linux producer
/// of one does). Returns the fd or a negative errno.
fn mint(pid: u32, fence: Fence) -> isize {
    static SIGNALLER_INSTALLED: AtomicBool = AtomicBool::new(false);
    if !SIGNALLER_INSTALLED.swap(true, Ordering::AcqRel) {
        fn signal(slot: u64) -> bool { vfs::eventfd_slot_signal_unref(slot as usize) }
        sync_file::set_signaller(signal);
    }
    const EFD_CLOEXEC: u64 = 0x8_0000;
    let efd_msg = make_vfs_msg(vfs::VFS_EVENTFD, &[0u64, EFD_CLOEXEC]);
    let efd = vfs_reply_val(&vfs::handle(&efd_msg, pid));
    if efd < 0 { return efd; }
    let token = sync_file::new_token();
    vfs::eventfd_note(pid, efd as usize, vfs::EVFD_ORIGIN_SYNC_FILE, token, [0; 16]);
    if let Some(vfs::VnodeKind::EventFd { slot }) = vfs::vfs_get_node_kind(pid, efd as usize) {
        if sync_file::is_signaled(&fence) {
            vfs::eventfd_slot_signal(slot);
        } else {
            // The signaller drops this reference, so a close racing the
            // retirement cannot hand the slot to a stranger first.
            vfs::eventfd_slot_ref(slot);
            if !sync_file::arm(token, fence, slot as u64) {
                vfs::eventfd_slot_signal(slot);
                vfs::eventfd_slot_unref(slot);
            }
        }
    }
    efd
}

// struct drm_syncobj_handle { u32 handle@0; u32 flags@4; s32 fd@8; u32 pad@12; [u64 point@16] }

fn syncobj_export(pid: u32, fd: usize, arg: usize) -> Option<isize> {
    if !sync_file::enabled() { return None; }
    if arg == 0 || !validate_user_buf(arg, 16) { return Some(EFAULT); }
    let handle = read_u32(arg);
    let flags = read_u32(arg + 4);
    if flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE == 0 {
        return None; // an opaque syncobj fd: still ENOSYS
    }
    if flags != DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE { return Some(EINVAL); }
    let open_id = open_id_of(pid, fd)?;
    let fence = match drivers::drm_device_interface::syncobj_export_fence(handle, open_id) {
        Ok(f) => f,
        Err(e) => return Some(-(e as isize)),
    };
    let sfd = mint(pid, fence);
    if sfd < 0 { return Some(sfd); }
    write_i32(arg + 8, sfd as i32);
    Some(0)
}

fn syncobj_import(pid: u32, fd: usize, arg: usize) -> Option<isize> {
    if !sync_file::enabled() { return None; }
    if arg == 0 || !validate_user_buf(arg, 16) { return Some(EFAULT); }
    let handle = read_u32(arg);
    let flags = read_u32(arg + 4);
    if flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE == 0 {
        return None; // an opaque syncobj fd: still ENOSYS
    }
    if flags != DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE { return Some(EINVAL); }
    let open_id = open_id_of(pid, fd)?;
    let fence = match fence_of_fd(pid, read_i32(arg + 8)) {
        Some(f) => f,
        None => return Some(EINVAL), // not a sync file (upstream: sync_file_get_fence fails)
    };
    match drivers::drm_device_interface::syncobj_import_fence(handle, open_id, fence) {
        Ok(()) => Some(0),
        Err(e) => Some(-(e as isize)),
    }
}

// struct dma_buf_export_sync_file { u32 flags@0; s32 fd@4; }

fn dmabuf_ioctl(pid: u32, fd: usize, cmd: usize, arg: usize) -> Option<isize> {
    if !sync_file::enabled() { return None; }
    let obj = vfs::dmabuf_obj_of(sched::tgid_of(pid), fd)?;
    match cmd {
        DMA_BUF_IOCTL_SYNC => {
            if arg == 0 || !validate_user_buf(arg, 8) { return Some(EFAULT); }
            let flags = unsafe { (arg as *const u64).read_unaligned() };
            // Coherent memory (cached sysmem, snooped): nothing to flush.
            Some(if flags & !DMA_BUF_SYNC_VALID != 0 { EINVAL } else { 0 })
        }
        DMA_BUF_SET_NAME_A | DMA_BUF_SET_NAME_B => Some(0),
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE => {
            if arg == 0 || !validate_user_buf(arg, 8) { return Some(EFAULT); }
            let flags = read_u32(arg);
            if !drivers::sync_file_core::resv_flags_valid(flags) { return Some(EINVAL); }
            let fence = sync_file::resv_export(obj, flags);
            let sfd = mint(pid, fence);
            if sfd < 0 { return Some(sfd); }
            write_i32(arg + 4, sfd as i32);
            Some(0)
        }
        DMA_BUF_IOCTL_IMPORT_SYNC_FILE => {
            if arg == 0 || !validate_user_buf(arg, 8) { return Some(EFAULT); }
            let flags = read_u32(arg);
            if !drivers::sync_file_core::resv_flags_valid(flags) { return Some(EINVAL); }
            let fence = match fence_of_fd(pid, read_i32(arg + 4)) {
                Some(f) => f,
                None => return Some(EINVAL),
            };
            sync_file::resv_import(obj, &fence, flags);
            Some(0)
        }
        _ => None,
    }
}

// struct sync_merge_data { char name[32]; s32 fd2@32; s32 fence@36; u32 flags@40; u32 pad@44; }
// struct sync_file_info  { char name[32]; s32 status@32; u32 flags@36; u32 num_fences@40;
//                          u32 pad@44; u64 sync_fence_info@48; }
// struct sync_fence_info { char obj_name[32]; char driver_name[32]; s32 status@64;
//                          u32 flags@68; u64 timestamp_ns@72; }   (80 bytes)

fn sync_file_ioctl(pid: u32, fd: usize, cmd: usize, arg: usize) -> Option<isize> {
    let token = vfs::eventfd_sync_token(pid, fd)?;
    let fence = sync_file::fence_of(token);
    match cmd {
        SYNC_IOC_SET_DEADLINE => Some(0),
        SYNC_IOC_MERGE => {
            if arg == 0 || !validate_user_buf(arg, 48) { return Some(EFAULT); }
            if read_u32(arg + 40) != 0 || read_u32(arg + 44) != 0 { return Some(EINVAL); }
            let other = match fence_of_fd(pid, read_i32(arg + 32)) {
                Some(f) => f,
                None => return Some(EINVAL),
            };
            let sfd = mint(pid, sync_file::merge(&fence, &other));
            if sfd < 0 { return Some(sfd); }
            write_i32(arg + 36, sfd as i32);
            Some(0)
        }
        SYNC_IOC_FILE_INFO => {
            if arg == 0 || !validate_user_buf(arg, 56) { return Some(EFAULT); }
            if read_u32(arg + 36) != 0 || read_u32(arg + 44) != 0 { return Some(EINVAL); }
            let signaled = sync_file::is_signaled(&fence);
            let ids: alloc::vec::Vec<u64> = match &fence {
                Fence::Signaled => alloc::vec![0],
                Fence::One(f) => alloc::vec![*f],
                Fence::All(v) => v.clone(),
            };
            let n = ids.len() as u32;
            let name = b"leandros-sync-file";
            unsafe {
                core::ptr::write_bytes(arg as *mut u8, 0, 32);
                core::ptr::copy_nonoverlapping(name.as_ptr(), arg as *mut u8, name.len());
            }
            write_i32(arg + 32, signaled as i32);
            let want = read_u32(arg + 40);
            if want == 0 {
                write_u32(arg + 40, n);
                return Some(0);
            }
            if want < n { return Some(EINVAL); }
            let p = unsafe { (arg as *const u64).add(6).read_unaligned() } as usize;
            if p == 0 || !validate_user_buf(p, 80 * n as usize) { return Some(EFAULT); }
            for (i, &f) in ids.iter().enumerate() {
                let e = p + 80 * i;
                unsafe { core::ptr::write_bytes(e as *mut u8, 0, 80); }
                let drv = b"leandros-drm";
                unsafe { core::ptr::copy_nonoverlapping(drv.as_ptr(), (e + 32) as *mut u8, drv.len()); }
                let done = f == 0 || sync_file::is_signaled(&Fence::One(f));
                write_i32(e + 64, done as i32);
            }
            write_u32(arg + 40, n);
            Some(0)
        }
        _ => None,
    }
}
