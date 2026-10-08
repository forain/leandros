//! In-kernel RM client API.
//!
//! The same entry point Linux's in-kernel RM clients use
//! (`rm_kernel_rmapi_op`, the RMAPI nvidia-modeset gets through
//! `nvidia_get_rm_ops`): privilege level KERNEL, parameter blocks in kernel
//! memory, RM taking its API/GPU locks inside. The C glue
//! (ports/openrm/glue/lnv_rmapi.c) fills the NVOSxx argument blocks; this
//! module gives them a typed Rust face. It is what the nouveau-UAPI layer in
//! the DRM server will sit on, and what `selftest` uses.
//!
//! Call from task context (a kernel thread or a syscall): RM sleeps on its
//! locks and in GSP RPC waits. Never from an ISR or under a spinlock.
//!
//! ```ignore
//! let c = Client::new()?;                                  // NV01_ROOT
//! let dev = c.alloc(c.handle(), NV01_DEVICE_0, &mut Nv0080AllocParameters { .. })?;
//! let sub = c.alloc(dev, NV20_SUBDEVICE_0, &mut Nv2080AllocParameters::default())?;
//! let mut name = zeroed::<Nv2080CtrlGpuGetNameStringParams>();
//! c.control(sub, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &mut *name)?;
//! let vas = c.alloc(dev, FERMI_VASPACE_A, &mut NvVaspaceAllocationParameters::default())?;
//! let buf = c.alloc_memory(dev, sub, MemLocation::Vidmem, 1 << 20, MemAttrs::default())?;
//! let (virt, va) = c.alloc_va(dev, vas, 1 << 20, 0x1000, PageSize::Small)?;
//! c.map_dma(dev, virt, buf, 0, 1 << 20, MapFlags::default())?;      // GPU VA = va
//! drop(c);                                                  // frees everything
//! ```

use core::ffi::{c_char, c_void, CStr};
use core::fmt;

use crate::nvlog;
use crate::params::*;

extern "C" {
    fn lnv_rm_alloc(h_client: Handle, h_parent: Handle, ph_object: *mut Handle, h_class: u32,
                    params: *mut c_void, params_size: u32) -> u32;
    fn lnv_rm_free(h_client: Handle, h_parent: Handle, h_object: Handle) -> u32;
    fn lnv_rm_control(h_client: Handle, h_object: Handle, cmd: u32, params: *mut c_void, params_size: u32) -> u32;
    fn lnv_rm_map_memory(h_client: Handle, h_device: Handle, h_memory: Handle, offset: u64, length: u64,
                         pp_address: *mut *mut c_void, flags: u32) -> u32;
    fn lnv_rm_unmap_memory(h_client: Handle, h_device: Handle, h_memory: Handle, p_address: *mut c_void,
                           flags: u32) -> u32;
    fn lnv_rm_map_memory_dma(h_client: Handle, h_device: Handle, h_dma: Handle, h_memory: Handle, offset: u64,
                             length: u64, flags: u32, p_dma_offset: *mut u64) -> u32;
    fn lnv_rm_unmap_memory_dma(h_client: Handle, h_device: Handle, h_dma: Handle, h_memory: Handle, flags: u32,
                               dma_offset: u64) -> u32;
    fn lnv_rm_dup_object(h_client: Handle, h_parent: Handle, ph_object: *mut Handle, h_client_src: Handle,
                         h_object_src: Handle, flags: u32) -> u32;
    fn lnv_status_str(status: u32) -> *const c_char;
    fn lnv_abi_count() -> u32;
    fn lnv_abi_entry(i: u32, value: *mut u64) -> *const c_char;
}

// ------------------------------------------------------------- status --

/// A failed RM call: the NV_STATUS RM returned.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RmError(pub u32);

impl RmError {
    /// RM's own name for the status ("NV_ERR_INVALID_ARGUMENT", ...).
    pub fn name(self) -> &'static str {
        let p = unsafe { lnv_status_str(self.0) };
        if p.is_null() {
            return "?";
        }
        unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("?")
    }
}

impl fmt::Debug for RmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x} {}", self.0, self.name())
    }
}

impl fmt::Display for RmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

pub type Result<T> = core::result::Result<T, RmError>;

fn check(st: u32) -> Result<()> {
    if st == crate::NV_OK { Ok(()) } else { Err(RmError(st)) }
}

/// `NV_ERR_GENERIC`, for failures detected on our side (bad argument sizes).
pub const NV_ERR_GENERIC: u32 = 0xffff;

// ------------------------------------------------------------ ABI check --

/// Compare every constant, size and offset in [`params::ABI`] with the C
/// glue's table (built from NVIDIA's headers). Returns (matched, mismatched);
/// each mismatch or missing name is logged. Device independent.
pub fn abi_check() -> (usize, usize) {
    let n = unsafe { lnv_abi_count() };
    let mut c_side = alloc::vec::Vec::with_capacity(n as usize);
    for i in 0..n {
        let mut v = 0u64;
        let p = unsafe { lnv_abi_entry(i, &mut v) };
        if p.is_null() {
            break;
        }
        c_side.push((unsafe { CStr::from_ptr(p) }.to_bytes(), v));
    }
    let (mut ok, mut bad) = (0, 0);
    for &(name, ours) in ABI {
        match c_side.iter().find(|(n, _)| *n == name.as_bytes()) {
            Some(&(_, theirs)) if theirs == ours => ok += 1,
            Some(&(_, theirs)) => {
                bad += 1;
                nvlog!("[nvrm] abi: MISMATCH {}: Rust {:#x}, C headers {:#x}\n", name, ours, theirs);
            }
            None => {
                bad += 1;
                nvlog!("[nvrm] abi: {} missing from the C table\n", name);
            }
        }
    }
    let c_only = c_side.iter().filter(|(n, _)| !ABI.iter().any(|(r, _)| r.as_bytes() == *n)).count();
    nvlog!("[nvrm] abi: C table {} entries, Rust {}; {} C-only (not used from Rust)\n", c_side.len(), ABI.len(), c_only);
    (ok, bad)
}

// ------------------------------------------------------------- client --

/// Where a physical allocation lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemLocation {
    /// Video memory (NV01_MEMORY_LOCAL_USER, parent = subdevice).
    Vidmem,
    /// System memory, CPU-cached and GPU-snooped (NV01_MEMORY_SYSTEM,
    /// parent = device). Pages come from `nv_alloc_pages`.
    Sysmem,
}

/// GPU page size for an allocation / a VA range / a mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageSize {
    /// RM's choice (big pages where possible).
    Default,
    /// 4 KiB.
    Small,
    /// 64 KiB (the "big" page on Ampere).
    Big,
}

impl PageSize {
    fn attr(self) -> u32 {
        match self {
            PageSize::Default => OS32_ATTR_PAGE_SIZE_DEFAULT,
            PageSize::Small => OS32_ATTR_PAGE_SIZE_4KB,
            PageSize::Big => OS32_ATTR_PAGE_SIZE_BIG,
        }
    }
}

/// Physical allocation attributes.
#[derive(Clone, Copy, Debug)]
pub struct MemAttrs {
    pub contiguous: bool,
    pub page_size: PageSize,
    /// NVOS32_ALLOC_FLAGS_* to add.
    pub extra_flags: u32,
}

impl Default for MemAttrs {
    fn default() -> Self {
        MemAttrs { contiguous: false, page_size: PageSize::Small, extra_flags: 0 }
    }
}

/// NV04_MAP_MEMORY_DMA flags.
#[derive(Clone, Copy, Debug)]
pub struct MapFlags(pub u32);

impl Default for MapFlags {
    /// Snooped (sysmem; ignored for vidmem), 4 KiB PTEs.
    fn default() -> Self {
        MapFlags(OS46_FLAGS_CACHE_SNOOP_ENABLE | OS46_FLAGS_PAGE_SIZE_4KB)
    }
}

/// One in-kernel RM client (an NV01_ROOT object). Dropping it frees the
/// client and, with it, every object allocated under it.
pub struct Client {
    h: Handle,
}

impl Client {
    /// Allocate a new client; RM picks the handle.
    pub fn new() -> Result<Client> {
        let mut h_client: Handle = 0;
        let mut h_obj: Handle = 0;
        check(unsafe {
            lnv_rm_alloc(0, 0, &mut h_obj, NV01_ROOT, &mut h_client as *mut Handle as *mut c_void,
                         core::mem::size_of::<Handle>() as u32)
        })?;
        let h = if h_obj != 0 { h_obj } else { h_client };
        Ok(Client { h })
    }

    /// The client handle (NV01_ROOT; parent of devices).
    pub fn handle(&self) -> Handle {
        self.h
    }

    /// NV04_ALLOC of `class` under `parent` with a typed parameter block.
    pub fn alloc<P>(&self, parent: Handle, class: u32, params: &mut P) -> Result<Handle> {
        self.alloc_raw(parent, class, params as *mut P as *mut c_void, core::mem::size_of::<P>() as u32)
    }

    /// NV04_ALLOC of a class that takes no parameters.
    pub fn alloc_noparams(&self, parent: Handle, class: u32) -> Result<Handle> {
        self.alloc_raw(parent, class, core::ptr::null_mut(), 0)
    }

    /// NV04_ALLOC with an untyped parameter block (`size` bytes at `params`;
    /// the UAPI layer passes user blocks through here after copying them in).
    pub fn alloc_raw(&self, parent: Handle, class: u32, params: *mut c_void, size: u32) -> Result<Handle> {
        let mut h: Handle = 0;
        check(unsafe { lnv_rm_alloc(self.h, parent, &mut h, class, params, size) })?;
        Ok(h)
    }

    /// NV04_ALLOC with a caller-chosen handle (as nouveau/NVK clients pick
    /// theirs).
    pub fn alloc_with_handle(&self, parent: Handle, handle: Handle, class: u32, params: *mut c_void, size: u32) -> Result<Handle> {
        let mut h = handle;
        check(unsafe { lnv_rm_alloc(self.h, parent, &mut h, class, params, size) })?;
        Ok(h)
    }

    /// NV04_CONTROL `cmd` on `object` with a typed parameter block.
    pub fn control<P>(&self, object: Handle, cmd: u32, params: &mut P) -> Result<()> {
        self.control_raw(object, cmd, params as *mut P as *mut c_void, core::mem::size_of::<P>() as u32)
    }

    pub fn control_raw(&self, object: Handle, cmd: u32, params: *mut c_void, size: u32) -> Result<()> {
        check(unsafe { lnv_rm_control(self.h, object, cmd, params, size) })
    }

    /// NV01_FREE of `object` (and everything under it).
    pub fn free(&self, parent: Handle, object: Handle) -> Result<()> {
        check(unsafe { lnv_rm_free(self.h, parent, object) })
    }

    /// NV04_DUP_OBJECT: duplicate `src_client`'s `src_object` under `parent`.
    pub fn dup(&self, parent: Handle, src_client: Handle, src_object: Handle) -> Result<Handle> {
        let mut h: Handle = 0;
        check(unsafe { lnv_rm_dup_object(self.h, parent, &mut h, src_client, src_object, 0) })?;
        Ok(h)
    }

    /// NV04_MAP_MEMORY: map `length` bytes at `offset` of `memory` into
    /// kernel virtual memory. `device` is the device or subdevice handle the
    /// memory belongs to. Sysmem: a WB kernel mapping of its pages; vidmem: a
    /// BAR1 window (reads are slow: uncached PCIe); USERMODE: its BAR0 page.
    pub fn map_cpu(&self, device: Handle, memory: Handle, offset: u64, length: u64, flags: u32) -> Result<*mut u8> {
        let mut p: *mut c_void = core::ptr::null_mut();
        check(unsafe { lnv_rm_map_memory(self.h, device, memory, offset, length, &mut p, flags) })?;
        if p.is_null() {
            return Err(RmError(NV_ERR_GENERIC));
        }
        Ok(p as *mut u8)
    }

    pub fn unmap_cpu(&self, device: Handle, memory: Handle, ptr: *mut u8) -> Result<()> {
        check(unsafe { lnv_rm_unmap_memory(self.h, device, memory, ptr as *mut c_void, 0) })
    }

    /// NV04_MAP_MEMORY_DMA: map `memory` (`offset`, `length`) into the GPU VA
    /// range `virt` (an NV50_MEMORY_VIRTUAL object from [`Client::alloc_va`]),
    /// at offset `va_offset` inside that range. Returns the GPU VA.
    pub fn map_dma_at(&self, device: Handle, virt: Handle, memory: Handle, offset: u64, length: u64,
                      flags: MapFlags, va_offset: u64) -> Result<u64> {
        let mut dma = va_offset;
        check(unsafe { lnv_rm_map_memory_dma(self.h, device, virt, memory, offset, length, flags.0, &mut dma) })?;
        Ok(dma)
    }

    /// [`Client::map_dma_at`] at the start of the range.
    pub fn map_dma(&self, device: Handle, virt: Handle, memory: Handle, offset: u64, length: u64,
                   flags: MapFlags) -> Result<u64> {
        self.map_dma_at(device, virt, memory, offset, length, flags, 0)
    }

    pub fn unmap_dma(&self, device: Handle, virt: Handle, memory: Handle, gpu_va: u64) -> Result<()> {
        check(unsafe { lnv_rm_unmap_memory_dma(self.h, device, virt, memory, 0, gpu_va) })
    }

    /// Physical memory: vidmem (NV01_MEMORY_LOCAL_USER under `subdevice`) or
    /// snooped sysmem (NV01_MEMORY_SYSTEM under `device`), as UVM's
    /// nvGpuOpsAllocPhysical does. Returns the memory handle.
    pub fn alloc_memory(&self, device: Handle, subdevice: Handle, loc: MemLocation, size: u64,
                        attrs: MemAttrs) -> Result<Handle> {
        let mut p = NvMemoryAllocationParams {
            owner: HEAP_OWNER_RM_KERNEL_CLIENT,
            r#type: NVOS32_TYPE_IMAGE,
            flags: attrs.extra_flags,
            size,
            ..Default::default()
        };
        p.attr = attrs.page_size.attr()
            | if attrs.contiguous { OS32_ATTR_PHYSICALITY_CONTIGUOUS } else { OS32_ATTR_PHYSICALITY_NONCONTIGUOUS };
        let (parent, class) = match loc {
            MemLocation::Vidmem => {
                p.attr |= OS32_ATTR_LOCATION_VIDMEM | OS32_ATTR_COHERENCY_UNCACHED;
                (subdevice, NV01_MEMORY_LOCAL_USER)
            }
            MemLocation::Sysmem => {
                p.attr |= OS32_ATTR_LOCATION_PCI | OS32_ATTR_COHERENCY_CACHED;
                (device, NV01_MEMORY_SYSTEM)
            }
        };
        self.alloc(parent, class, &mut p)
    }

    /// Reserve `size` bytes of GPU VA in `vaspace` (an NV50_MEMORY_VIRTUAL
    /// object under `device`, in the kernel-privileged part of the space, as
    /// UVM's nvGpuOpsAllocVirtual). Returns (handle, GPU VA).
    pub fn alloc_va(&self, device: Handle, vaspace: Handle, size: u64, align: u64, page: PageSize) -> Result<(Handle, u64)> {
        let mut p = NvMemoryAllocationParams {
            owner: HEAP_OWNER_RM_KERNEL_CLIENT,
            r#type: NVOS32_TYPE_IMAGE,
            flags: NVOS32_ALLOC_FLAGS_VIRTUAL | NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE
                | NVOS32_ALLOC_FLAGS_ALLOCATE_KERNEL_PRIVILEGED,
            attr: page.attr(),
            size,
            alignment: align.max(0x1000),
            hVASpace: vaspace,
            ..Default::default()
        };
        let h = self.alloc(device, NV50_MEMORY_VIRTUAL, &mut p)?;
        Ok((h, p.offset))
    }

    /// A CPU pointer into vidmem through BAR1 (NV04_MAP_MEMORY on a vidmem
    /// object). For occasional small accesses; bulk transfers belong on a
    /// copy engine.
    pub fn bar1_map(&self, subdevice: Handle, memory: Handle, offset: u64, length: u64) -> Result<*mut u8> {
        self.map_cpu(subdevice, memory, offset, length, OS33_FLAGS_ACCESS_READ_WRITE)
    }
}

impl Client {
    /// Free the client (and everything under it), reporting the status.
    pub fn destroy(self) -> Result<()> {
        let h = self.h;
        core::mem::forget(self);
        check(unsafe { lnv_rm_free(h, 0, h) })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let st = unsafe { lnv_rm_free(self.h, 0, self.h) };
        if st != crate::NV_OK {
            nvlog!("[nvrm] client {:#x}: free failed: {:?}\n", self.h, RmError(st));
        }
    }
}

/// NUL-terminated byte buffer from RM as &str.
pub fn cstr_bytes(b: &[u8]) -> &str {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).unwrap_or("<non-utf8>")
}
