//! `nvrm.selftest=ce`: real GPU work through the in-kernel RM client, run by
//! the `[nvrm]` thread right after `rm_init_adapter` succeeded.
//!
//! The steps (each logs `[nvrm] +<ms> selftest <step>: ...` and ends with
//! `selftest <step> ok|FAILED|skipped`):
//!
//! | step | what |
//! |---|---|
//! | abi | the Rust parameter blocks/constants against NVIDIA's headers (C glue table) |
//! | client | NV01_ROOT; driver version; attached GPU ids -> device instance |
//! | device | NV01_DEVICE_0 + NV20_SUBDEVICE_0; class list |
//! | info | name string, FB (VRAM, BAR1, bus width), GR (GPCs/TPCs/SMs), engines + CE caps |
//! | vaspace | FERMI_VASPACE_A |
//! | memory | sysmem (push buffer, GPFIFO, USERD, semaphore, 2 x 1 MiB) + 1 MiB vidmem, mapped into the VA space |
//! | channel | KEPLER_CHANNEL_GROUP_A + AMPERE_CHANNEL_GPFIFO_A + AMPERE_DMA_COPY_B, VOLTA_USERMODE_A doorbell, work-submit token, GPFIFO_SCHEDULE |
//! | submit | first submission: a semaphore release the CPU polls |
//! | memset | CE fill of vidmem, read back with a CE copy to sysmem; sysmem -> vidmem -> sysmem copy round trip |
//! | intr | a semaphore release with a non-stall interrupt (logs whether the ISR saw it) |
//! | fb | colour bars on the GOP console, written by the CE into VRAM |
//! | loop | `nvrm.selftest_loops` (1000) fill + semaphore round trips, timed |
//! | teardown | every object freed in reverse order |
//!
//! Options: `nvrm.selftest_stop=<step>` (stop after it, then tear down),
//! `nvrm.selftest_skip=<step>[,<step>]`, `nvrm.selftest_hold=<s>` (how long
//! the bars stay up, 10), `nvrm.selftest_loops=<n>` (1000),
//! `nvrm.selftest_fbphys=<hex>` (GOP surface VRAM offset if RM cannot give
//! the console memory object; default 0, used only after a read-back check).
//!
//! Every wait is bounded (2 s per semaphore) and an abort point for the
//! bring-up watchdog; the stick log is flushed before every step that touches
//! the GPU.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

use crate::client::*;
use crate::params::*;
use crate::{nvlog, time};

pub const STEPS: &[&str] = &[
    "abi", "client", "device", "info", "vaspace", "memory", "channel", "submit",
    "memset", "intr", "fb", "loop", "teardown",
];
pub const STEP_LIST: &str = "abi client device info vaspace memory channel submit memset intr fb loop teardown";

const T_ABI: usize = 0;
const T_CLIENT: usize = 1;
const T_DEVICE: usize = 2;
const T_INFO: usize = 3;
const T_VASPACE: usize = 4;
const T_MEMORY: usize = 5;
const T_CHANNEL: usize = 6;
const T_SUBMIT: usize = 7;
const T_MEMSET: usize = 8;
const T_INTR: usize = 9;
const T_FB: usize = 10;
const T_LOOP: usize = 11;
const T_TEARDOWN: usize = 12;

/// Which self-test the command line asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    None,
    /// Device independent: ABI check + client/controls (runs without a GPU).
    Abi,
    /// Everything (needs rm_init_adapter OK).
    Ce,
}

pub(crate) fn mode_from_cmdline() -> Mode {
    match crate::opt_str("nvrm.selftest") {
        Some("ce") | Some("1") => Mode::Ce,
        Some("abi") => Mode::Abi,
        Some(other) => {
            nvlog!("[nvrm] unknown nvrm.selftest={} (ce | abi); ignoring\n", other);
            Mode::None
        }
        None => Mode::None,
    }
}

struct Cfg {
    stop_after: Option<usize>,
    skip: u32,
    hold_s: u64,
    loops: u32,
    fbphys: Option<u64>,
}

fn step_index(name: &str) -> Option<usize> {
    STEPS.iter().position(|s| *s == name)
}

fn cfg() -> Cfg {
    let stop_after = crate::opt_str("nvrm.selftest_stop").and_then(|s| {
        let i = step_index(s);
        if i.is_none() {
            nvlog!("[nvrm] unknown nvrm.selftest_stop={} (steps: {}); ignoring\n", s, STEP_LIST);
        }
        i
    });
    let mut skip = 0u32;
    if let Some(list) = crate::opt_str("nvrm.selftest_skip") {
        for s in list.split(',').filter(|s| !s.is_empty()) {
            match step_index(s) {
                Some(i) => skip |= 1 << i,
                None => nvlog!("[nvrm] unknown step '{}' in nvrm.selftest_skip (steps: {})\n", s, STEP_LIST),
            }
        }
    }
    let num = |k: &str| crate::opt_str(k).and_then(crate::parse_num);
    Cfg {
        stop_after,
        skip,
        hold_s: num("nvrm.selftest_hold").unwrap_or(10).min(600),
        loops: num("nvrm.selftest_loops").unwrap_or(1000).clamp(1, 1_000_000) as u32,
        fbphys: num("nvrm.selftest_fbphys"),
    }
}

/// What the self-test needs from the bring-up.
pub(crate) struct Env {
    /// GOP framebuffer (Limine): physical address, geometry.
    pub fb_phys: u64,
    pub fb_w: u32,
    pub fb_h: u32,
    pub fb_pitch: u32,
}

// ----------------------------------------------------------- the state --

const SEG_BYTES: usize = 1024;
const PB_BYTES: u64 = 128 * 1024;
const SEGS: usize = PB_BYTES as usize / SEG_BYTES;
const GP_ENTRIES: u32 = 256;
const BIG: u64 = 1 << 20;
const SEM_TIMEOUT_MS: u64 = 2000;
/// drivers::framebuffer scanout owner id for the colour-bar demo.
const SCANOUT_ID: u32 = 0x6e76_7273;

struct Buf {
    name: &'static str,
    loc: MemLocation,
    mem: Handle,
    virt: Handle,
    va: u64,
    cpu: *mut u8,
}

#[derive(Default)]
struct Outcome {
    passed: u32,
    failed: u32,
    skipped: u32,
}

struct St {
    cfg: Cfg,
    c: Option<Client>,
    devinst: u32,
    dev: Handle,
    sub: Handle,
    classes: Vec<u32>,
    vas: Handle,
    bufs: Vec<Buf>,
    ce_engine: u32,
    ce_class: u32,
    tsg: Handle,
    chan: Handle,
    ce: Handle,
    usermode: Handle,
    doorbell: *mut u8,
    token: u32,
    gp_put: u32,
    nsub: usize,
    payload: u32,
    /// Console (GOP) surface objects: memory, VA range, GPU VA.
    con_mem: Handle,
    con_virt: Handle,
    con_va: u64,
    scanout_claimed: bool,
    channel_ok: bool,
    submit_ok: bool,
    out: Outcome,
}

fn ms() -> u64 {
    crate::ms()
}

macro_rules! st {
    ($step:expr, $($t:tt)*) => {
        nvlog!("[nvrm] +{}ms selftest {}: {}\n", ms(), STEPS[$step], format_args!($($t)*))
    };
}

impl St {
    fn c(&self) -> &Client {
        self.c.as_ref().expect("client")
    }

    fn buf(&self, name: &str) -> &Buf {
        self.bufs.iter().find(|b| b.name == name).expect("buffer")
    }

    fn sem_ptr(&self) -> *mut u32 {
        self.buf("sem").cpu as *mut u32
    }

    fn sem_value(&self) -> u32 {
        unsafe { core::ptr::read_volatile(self.sem_ptr()) }
    }

    fn userd_u32(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile(self.buf("userd").cpu.add(off) as *const u32) }
    }

    /// Allocate, CPU-map (sysmem) and GPU-map (unless `gpu_map` is false)
    /// one buffer.
    fn new_buf(&mut self, name: &'static str, loc: MemLocation, size: u64, gpu_map: bool) -> Result<()> {
        let c = self.c.as_ref().expect("client");
        let attrs = MemAttrs { contiguous: loc == MemLocation::Sysmem && size <= 64 * 1024, ..Default::default() };
        let mem = c.alloc_memory(self.dev, self.sub, loc, size, attrs)?;
        let mut b = Buf { name, loc, mem, virt: 0, va: 0, cpu: core::ptr::null_mut() };
        let r = (|| -> Result<()> {
            if gpu_map {
                let (virt, va) = c.alloc_va(self.dev, self.vas, size, 0x1000, PageSize::Small)?;
                b.virt = virt;
                b.va = c.map_dma(self.dev, virt, mem, 0, size, MapFlags::default())?;
                if b.va != va {
                    nvlog!("[nvrm]   {}: mapped at {:#x}, range starts at {:#x}\n", name, b.va, va);
                }
            }
            if loc == MemLocation::Sysmem {
                b.cpu = c.map_cpu(self.dev, mem, 0, size, OS33_FLAGS_ACCESS_READ_WRITE)?;
            }
            Ok(())
        })();
        let line = alloc::format!("{:<6} {:?} {} KiB: memory {:#x}, GPU VA {:#x}{}", name, loc, size / 1024, mem, b.va,
                                  if b.cpu.is_null() { String::new() } else { alloc::format!(", CPU {:p}", b.cpu) });
        self.bufs.push(b);
        match &r {
            Ok(()) => st!(T_MEMORY, "{}", line),
            Err(e) => st!(T_MEMORY, "{} -> FAILED {:?}", line, e),
        }
        r
    }

    // ------------------------------------------------------ submission --

    /// Build one push segment (SET_OBJECT, the caller's methods, a semaphore
    /// release of the next payload), queue it on the GPFIFO and ring the
    /// doorbell. Returns the payload to wait for.
    fn submit(&mut self, intr: bool, build: impl FnOnce(&mut Push)) -> u32 {
        let seg = self.nsub % SEGS;
        self.nsub += 1;
        let pb = self.buf("pb");
        let words = unsafe {
            core::slice::from_raw_parts_mut(pb.cpu.add(seg * SEG_BYTES) as *mut u32, SEG_BYTES / 4)
        };
        let seg_va = pb.va + (seg * SEG_BYTES) as u64;
        let sem_va = self.buf("sem").va;
        let mut p = Push { w: words, n: 0, subch: NVA06F_SUBCHANNEL_COPY_ENGINE };
        p.n += unsafe { lnv_push_set_object(p.w.as_mut_ptr(), p.subch, self.ce_class) } as usize;
        build(&mut p);
        self.payload = self.payload.wrapping_add(1);
        let payload = self.payload;
        p.sem_release(sem_va, payload, intr);
        let bytes = (p.n * 4) as u32;
        let entry = unsafe { lnv_gp_entry(seg_va, bytes) };
        let gp = self.buf("gpfifo").cpu as *mut u64;
        unsafe { core::ptr::write_volatile(gp.add(self.gp_put as usize), entry) };
        self.gp_put = (self.gp_put + 1) % GP_ENTRIES;
        fence(Ordering::SeqCst);
        unsafe {
            core::ptr::write_volatile(self.buf("userd").cpu.add(USERD_GP_PUT) as *mut u32, self.gp_put);
        }
        fence(Ordering::SeqCst);
        unsafe {
            core::ptr::write_volatile(self.doorbell.add(NVC361_NOTIFY_CHANNEL_PENDING) as *mut u32, self.token);
        }
        payload
    }

    /// Poll the semaphore until it reaches `payload` (bounded, an abort point
    /// for the watchdog). Returns the wait in ns.
    fn wait(&self, step: usize, payload: u32, what: &str) -> core::result::Result<u64, ()> {
        let t0 = sched::monotonic_ns();
        let deadline = t0 + SEM_TIMEOUT_MS * 1_000_000;
        loop {
            let v = self.sem_value();
            if (v.wrapping_sub(payload) as i32) >= 0 {
                return Ok(sched::monotonic_ns() - t0);
            }
            if sched::monotonic_ns() > deadline {
                st!(step, "TIMEOUT waiting for {} (semaphore {:#x}, want {:#x}) after {} ms", what, v, payload, SEM_TIMEOUT_MS);
                self.dump_channel(step);
                return Err(());
            }
            time::relax("selftest semaphore poll");
            core::hint::spin_loop();
        }
    }

    fn dump_channel(&self, step: usize) {
        st!(step, "channel state: USERD GP_GET {} GP_PUT {} (ours {}), GET {:#x} PUT {:#x}; submissions {}; isr {}, bh {}; RM Xid lines so far {}",
            self.userd_u32(USERD_GP_GET), self.userd_u32(USERD_GP_PUT), self.gp_put,
            self.userd_u32(USERD_GET), self.userd_u32(USERD_PUT), self.nsub,
            crate::device::isr_count(), crate::device::bh_count(), crate::xid_lines());
        st!(step, "  GP_GET == GP_PUT: host fetched the entry (the CE never released the semaphore: method/address error -> see 'NVRM: Xid' above); GP_GET behind: the channel never ran (scheduling/doorbell/USERD)");
    }

    fn submit_wait(&mut self, step: usize, what: &str, intr: bool, build: impl FnOnce(&mut Push)) -> core::result::Result<u64, ()> {
        let p = self.submit(intr, build);
        self.wait(step, p, what)
    }
}

/// One push segment under construction.
struct Push<'a> {
    w: &'a mut [u32],
    n: usize,
    subch: u32,
}

extern "C" {
    fn lnv_push_set_object(p: *mut u32, subch: u32, cls: u32) -> u32;
    fn lnv_push_ce_memset(p: *mut u32, subch: u32, dst: u64, dst_mode: u32, pattern: u32, width: u32,
                          height: u32, pitch: u32, flush: u8) -> u32;
    fn lnv_push_ce_copy(p: *mut u32, subch: u32, dst: u64, dst_mode: u32, src: u64, src_mode: u32, bytes: u32,
                        flush: u8) -> u32;
    fn lnv_push_ce_sem_release(p: *mut u32, subch: u32, sem: u64, payload: u32, intr: u8) -> u32;
    fn lnv_gp_entry(pb: u64, bytes: u32) -> u64;
}

/// Encoders write at most this many dwords per call.
const MAX_OP_WORDS: usize = 32;

impl Push<'_> {
    fn room(&self) -> bool {
        self.n + MAX_OP_WORDS <= self.w.len()
    }

    /// 32-bit fill: `width` elements per line, `height` lines `pitch` apart.
    /// dst_mode 0 = GPU VA, 1 = physical vidmem.
    fn memset(&mut self, dst: u64, dst_mode: u32, pattern: u32, width: u32, height: u32, pitch: u32) {
        assert!(self.room());
        self.n += unsafe {
            lnv_push_ce_memset(self.w[self.n..].as_mut_ptr(), self.subch, dst, dst_mode, pattern, width, height, pitch, 1)
        } as usize;
    }

    fn copy(&mut self, dst: u64, dst_mode: u32, src: u64, src_mode: u32, bytes: u32) {
        assert!(self.room());
        self.n += unsafe {
            lnv_push_ce_copy(self.w[self.n..].as_mut_ptr(), self.subch, dst, dst_mode, src, src_mode, bytes, 1)
        } as usize;
    }

    fn sem_release(&mut self, sem: u64, payload: u32, intr: bool) {
        assert!(self.room());
        self.n += unsafe {
            lnv_push_ce_sem_release(self.w[self.n..].as_mut_ptr(), self.subch, sem, payload, intr as u8)
        } as usize;
    }
}

// ------------------------------------------------------------- runner --

/// Run the self-test. `env` is None for the device-independent `abi` mode.
/// Returns true when every step that ran passed.
pub(crate) fn run(mode: Mode, env: Option<&Env>) -> bool {
    let cfg = cfg();
    nvlog!("[nvrm] +{}ms ==== selftest {:?}: steps {} (stop after {}, skip mask {:#x}, hold {} s, loops {}) ====\n",
           ms(), mode, STEP_LIST, cfg.stop_after.map_or("-", |i| STEPS[i]), cfg.skip, cfg.hold_s, cfg.loops);
    let mut s = St {
        cfg, c: None, devinst: 0, dev: 0, sub: 0, classes: Vec::new(), vas: 0, bufs: Vec::new(),
        ce_engine: 0, ce_class: 0, tsg: 0, chan: 0, ce: 0, usermode: 0, doorbell: core::ptr::null_mut(),
        token: 0, gp_put: 0, nsub: 0, payload: 0, con_mem: 0, con_virt: 0, con_va: 0,
        scanout_claimed: false, channel_ok: false, submit_ok: false, out: Outcome::default(),
    };
    let last = if mode == Mode::Abi { T_CLIENT } else { T_LOOP };
    let mut stopped = false;
    for i in 0..=last {
        if stopped {
            break;
        }
        if s.cfg.skip & (1 << i) != 0 {
            st!(i, "skipped (nvrm.selftest_skip)");
            s.out.skipped += 1;
            continue;
        }
        crate::device::substep(STEPS[i]);
        if i >= T_SUBMIT && !s.channel_ok {
            st!(i, "skipped (no channel)");
            s.out.skipped += 1;
            continue;
        }
        if i > T_SUBMIT && !s.submit_ok {
            st!(i, "skipped (the first submission did not complete)");
            s.out.skipped += 1;
            continue;
        }
        if i >= T_SUBMIT || i == T_CHANNEL {
            crate::device::flush_log();
        }
        let t0 = sched::monotonic_ns();
        let ok = match i {
            T_ABI => step_abi(&mut s),
            T_CLIENT => step_client(&mut s, mode),
            T_DEVICE => step_device(&mut s),
            T_INFO => step_info(&mut s),
            T_VASPACE => step_vaspace(&mut s),
            T_MEMORY => step_memory(&mut s),
            T_CHANNEL => step_channel(&mut s),
            T_SUBMIT => step_submit(&mut s),
            T_MEMSET => step_memset(&mut s),
            T_INTR => step_intr(&mut s),
            T_FB => {
                let ok = step_fb(&mut s, env);
                // Give the screen back to the console (the VT layer repaints
                // its text) however the step ended.
                if s.scanout_claimed {
                    release_console();
                    s.scanout_claimed = false;
                }
                ok
            }
            T_LOOP => step_loop(&mut s),
            _ => true,
        };
        let el = (sched::monotonic_ns() - t0) / 1_000_000;
        nvlog!("[nvrm] +{}ms selftest {} {} ({} ms)\n", ms(), STEPS[i], if ok { "ok" } else { "FAILED" }, el);
        if ok {
            s.out.passed += 1;
        } else {
            s.out.failed += 1;
            // Later steps depend on earlier ones up to the channel; after
            // that each GPU step stands on its own (the channel may be dead,
            // which the next wait will show quickly).
            if i <= T_CHANNEL || i == T_SUBMIT {
                nvlog!("[nvrm] +{}ms selftest: {} failed; skipping to teardown\n", ms(), STEPS[i]);
                stopped = true;
            }
        }
        if s.cfg.stop_after == Some(i) {
            nvlog!("[nvrm] +{}ms selftest: stopping after {} (nvrm.selftest_stop)\n", ms(), STEPS[i]);
            stopped = true;
        }
    }
    crate::device::substep("teardown");
    let t0 = sched::monotonic_ns();
    let ok = step_teardown(&mut s);
    nvlog!("[nvrm] +{}ms selftest teardown {} ({} ms)\n", ms(), if ok { "ok" } else { "FAILED" },
           (sched::monotonic_ns() - t0) / 1_000_000);
    if ok { s.out.passed += 1 } else { s.out.failed += 1 }
    let pass = s.out.failed == 0;
    nvlog!("[nvrm] +{}ms ==== selftest {}: {} passed, {} failed, {} skipped; RM Xid lines {}, stubs hit {} ====\n",
           ms(), if pass { "PASSED" } else { "FAILED" }, s.out.passed, s.out.failed, s.out.skipped,
           crate::xid_lines(), crate::stub_hits());
    crate::device::flush_log();
    pass
}

// -------------------------------------------------------------- steps --

fn step_abi(_s: &mut St) -> bool {
    let (ok, bad) = abi_check();
    st!(T_ABI, "{} constants/sizes/offsets match NVIDIA's headers, {} mismatches", ok, bad);
    if bad != 0 {
        st!(T_ABI, "refusing to talk to RM with a mismatched parameter layout (fix drivers/nvidia/src/params.rs)");
    }
    // What RM's chipset code will find through os_get_acpi_rsdp_from_uefi
    // (PCIe config space of the upstream bridge). Informational.
    let seg = crate::platform::probe_mcfg_like_rm();
    st!(T_ABI, "ACPI MCFG as RM walks it: {} ECAM segment(s){}", seg,
        if seg == 0 { " -- RM will report 'PCIE config space is inaccessible'" } else { "" });
    bad == 0
}

fn step_client(s: &mut St, mode: Mode) -> bool {
    let c = match Client::new() {
        Ok(c) => c,
        Err(e) => {
            st!(T_CLIENT, "NV01_ROOT alloc FAILED: {:?}", e);
            return false;
        }
    };
    st!(T_CLIENT, "NV01_ROOT client handle {:#x}", c.handle());
    let mut v = zeroed::<Nv0000CtrlSystemGetBuildVersionV2Params>();
    match c.control(c.handle(), NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION_V2, &mut *v) {
        Ok(()) => st!(T_CLIENT, "RM build: driver '{}' version '{}' branch '{}' cl {}", cstr_bytes(&v.driverVersionBuffer),
                      cstr_bytes(&v.versionBuffer), cstr_bytes(&v.driverBranch), v.changelistNumber),
        Err(e) => st!(T_CLIENT, "GET_BUILD_VERSION_V2 FAILED: {:?}", e),
    }
    let mut ids = zeroed::<Nv0000CtrlGpuGetAttachedIdsParams>();
    let attached: Vec<u32> = match c.control(c.handle(), NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS, &mut *ids) {
        Ok(()) => ids.gpuIds.iter().copied().take_while(|&i| i != NV0000_CTRL_GPU_INVALID_ID && i != 0).collect(),
        Err(e) => {
            st!(T_CLIENT, "GET_ATTACHED_IDS FAILED: {:?}", e);
            s.c = Some(c);
            return false;
        }
    };
    st!(T_CLIENT, "attached GPU ids: {:x?}", attached);
    s.c = Some(c);
    let Some(&id) = attached.first() else {
        if mode == Mode::Abi {
            // No GPU (QEMU): show that a device allocation is refused cleanly.
            let mut p = Nv0080AllocParameters { deviceId: 0, hClientShare: s.c().handle(), ..Default::default() };
            let r = s.c().alloc(s.c().handle(), NV01_DEVICE_0, &mut p);
            st!(T_CLIENT, "no GPU attached; NV01_DEVICE_0 alloc -> {:?} (expected to fail)", r.err());
            return r.is_err();
        }
        st!(T_CLIENT, "no GPU attached to RM");
        return false;
    };
    let mut info = Nv0000CtrlGpuGetIdInfoV2Params { gpuId: id, ..Default::default() };
    match s.c().control(s.c().handle(), NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2, &mut info) {
        Ok(()) => {
            st!(T_CLIENT, "gpu {:#x}: device instance {}, subdevice {}, flags {:#x}, board {:#x}", id,
                info.deviceInstance, info.subDeviceInstance, info.gpuFlags, info.boardId);
            s.devinst = info.deviceInstance;
            true
        }
        Err(e) => {
            st!(T_CLIENT, "GET_ID_INFO_V2 FAILED: {:?}", e);
            false
        }
    }
}

fn step_device(s: &mut St) -> bool {
    let h = s.c().handle();
    let mut p = Nv0080AllocParameters { deviceId: s.devinst, hClientShare: h, ..Default::default() };
    s.dev = match s.c().alloc(h, NV01_DEVICE_0, &mut p) {
        Ok(d) => d,
        Err(e) => {
            st!(T_DEVICE, "NV01_DEVICE_0 FAILED: {:?}", e);
            return false;
        }
    };
    let mut p = Nv2080AllocParameters { subDeviceId: 0 };
    s.sub = match s.c().alloc(s.dev, NV20_SUBDEVICE_0, &mut p) {
        Ok(d) => d,
        Err(e) => {
            st!(T_DEVICE, "NV20_SUBDEVICE_0 FAILED: {:?}", e);
            return false;
        }
    };
    st!(T_DEVICE, "device {:#x} (instance {}), subdevice {:#x}", s.dev, s.devinst, s.sub);
    let mut cl = zeroed::<Nv0080CtrlGpuGetClasslistV2Params>();
    match s.c().control(s.dev, NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2, &mut *cl) {
        Ok(()) => {
            let n = (cl.numClasses as usize).min(cl.classList.len());
            s.classes = cl.classList[..n].to_vec();
            let mut line = String::new();
            for c in &s.classes {
                line.push_str(&alloc::format!(" {:x}", c));
            }
            st!(T_DEVICE, "{} classes:{}", n, line);
        }
        Err(e) => st!(T_DEVICE, "GET_CLASSLIST_V2 FAILED: {:?} (assuming the Ampere classes)", e),
    }
    let has = |c: u32| s.classes.is_empty() || s.classes.contains(&c);
    s.ce_class = if has(AMPERE_DMA_COPY_B) { AMPERE_DMA_COPY_B } else { AMPERE_DMA_COPY_A };
    for (c, n) in [(AMPERE_CHANNEL_GPFIFO_A, "AMPERE_CHANNEL_GPFIFO_A"), (s.ce_class, "copy engine class"),
                   (VOLTA_USERMODE_A, "VOLTA_USERMODE_A"), (KEPLER_CHANNEL_GROUP_A, "KEPLER_CHANNEL_GROUP_A"),
                   (FERMI_VASPACE_A, "FERMI_VASPACE_A")] {
        if !has(c) {
            st!(T_DEVICE, "class {:#x} ({}) NOT in the class list", c, n);
        }
    }
    st!(T_DEVICE, "using host class {:#x}, CE class {:#x}", AMPERE_CHANNEL_GPFIFO_A, s.ce_class);
    true
}

fn step_info(s: &mut St) -> bool {
    let c = s.c();
    let mut name = zeroed::<Nv2080CtrlGpuGetNameStringParams>();
    match c.control(s.sub, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &mut *name) {
        Ok(()) => st!(T_INFO, "name: {}", cstr_bytes(&name.gpuNameString[..NV2080_GPU_MAX_NAME_STRING_LENGTH])),
        Err(e) => st!(T_INFO, "GET_NAME_STRING FAILED: {:?}", e),
    }

    let fb_idx = [
        (NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE, "ram KiB"), (NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE, "total ram KiB"),
        (NV2080_CTRL_FB_INFO_INDEX_HEAP_SIZE, "heap KiB"), (NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE, "heap free KiB"),
        (NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE, "bar1 KiB"), (NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH, "bus width"),
        (NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE, "ram type"), (NV2080_CTRL_FB_INFO_INDEX_L2CACHE_SIZE, "L2 bytes"),
        (NV2080_CTRL_FB_INFO_INDEX_FBP_COUNT, "FBPs"), (NV2080_CTRL_FB_INFO_INDEX_LTC_COUNT, "LTCs"),
    ];
    let mut fb = zeroed::<Nv2080CtrlFbGetInfoV2Params>();
    fb.fbInfoListSize = fb_idx.len() as u32;
    for (i, (idx, _)) in fb_idx.iter().enumerate() {
        fb.fbInfoList[i].index = *idx;
    }
    match c.control(s.sub, NV2080_CTRL_CMD_FB_GET_INFO_V2, &mut *fb) {
        Ok(()) => {
            let mut line = String::new();
            for (i, (_, n)) in fb_idx.iter().enumerate() {
                line.push_str(&alloc::format!(" {}={}", n, fb.fbInfoList[i].data));
            }
            st!(T_INFO, "FB:{}", line);
            st!(T_INFO, "VRAM {} MiB (ram type {}: 0x11 GDDR6, 0x12 GDDR6X), BAR1 {} MiB",
                fb.fbInfoList[0].data / 1024, fb.fbInfoList[6].data, fb.fbInfoList[4].data / 1024);
        }
        Err(e) => st!(T_INFO, "FB_GET_INFO_V2 FAILED: {:?}", e),
    }

    let gr_idx = [
        (NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_GPCS, "max GPCs"), (NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_TPC_PER_GPC, "max TPC/GPC"),
        (NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_SM_PER_TPC, "SM/TPC"), (NV2080_CTRL_GR_INFO_INDEX_SHADER_PIPE_COUNT, "shader pipes"),
        (NV2080_CTRL_GR_INFO_INDEX_GPU_CORE_COUNT, "cores"), (NV2080_CTRL_GR_INFO_INDEX_SM_VERSION, "SM version"),
        (NV2080_CTRL_GR_INFO_INDEX_MAX_WARPS_PER_SM, "warps/SM"),
    ];
    let mut gr = zeroed::<Nv2080CtrlGrGetInfoV2Params>();
    gr.grInfoListSize = gr_idx.len() as u32;
    for (i, (idx, _)) in gr_idx.iter().enumerate() {
        gr.grInfoList[i].index = *idx;
    }
    let mut sm_per_tpc = 0;
    match c.control(s.sub, NV2080_CTRL_CMD_GR_GET_INFO_V2, &mut *gr) {
        Ok(()) => {
            let mut line = String::new();
            for (i, (_, n)) in gr_idx.iter().enumerate() {
                line.push_str(&alloc::format!(" {}={:#x}", n, gr.grInfoList[i].data));
            }
            sm_per_tpc = gr.grInfoList[2].data;
            st!(T_INFO, "GR:{}", line);
        }
        Err(e) => st!(T_INFO, "GR_GET_INFO_V2 FAILED: {:?}", e),
    }
    let mut gm = Nv2080CtrlGrGetGpcMaskParams::default();
    match c.control(s.sub, NV2080_CTRL_CMD_GR_GET_GPC_MASK, &mut gm) {
        Ok(()) => {
            let mut tpcs = 0;
            let mut line = String::new();
            for g in 0..32 {
                if gm.gpcMask & (1 << g) == 0 {
                    continue;
                }
                let mut tm = Nv2080CtrlGrGetTpcMaskParams { gpcId: g, ..Default::default() };
                match c.control(s.sub, NV2080_CTRL_CMD_GR_GET_TPC_MASK, &mut tm) {
                    Ok(()) => {
                        tpcs += tm.tpcMask.count_ones();
                        line.push_str(&alloc::format!(" gpc{}:{:#x}", g, tm.tpcMask));
                    }
                    Err(e) => line.push_str(&alloc::format!(" gpc{}:{:?}", g, e)),
                }
            }
            st!(T_INFO, "{} GPCs (mask {:#x}), {} TPCs, {} SMs; TPC masks:{}", gm.gpcMask.count_ones(), gm.gpcMask, tpcs,
                tpcs * sm_per_tpc, line);
        }
        Err(e) => st!(T_INFO, "GR_GET_GPC_MASK FAILED: {:?}", e),
    }

    // Engines and copy-engine capabilities: pick the CE for the channel.
    let mut eng = zeroed::<Nv2080CtrlGpuGetEnginesV2Params>();
    let mut chosen: Option<(u32, u8)> = None;
    let mut first_ce: Option<u32> = None;
    match c.control(s.sub, NV2080_CTRL_CMD_GPU_GET_ENGINES_V2, &mut *eng) {
        Ok(()) => {
            let n = (eng.engineCount as usize).min(eng.engineList.len());
            let mut line = String::new();
            for &e in &eng.engineList[..n] {
                line.push_str(&alloc::format!(" {:#x}", e));
            }
            st!(T_INFO, "{} engines (NV2080_ENGINE_TYPE):{}", n, line);
            for &e in &eng.engineList[..n] {
                let Some(ci) = copy_index(e) else { continue };
                first_ce.get_or_insert(e);
                let mut caps = Nv2080CtrlCeGetCapsV2Params { ceEngineType: e, ..Default::default() };
                match c.control(s.sub, NV2080_CTRL_CMD_CE_GET_CAPS_V2, &mut caps) {
                    Ok(()) => {
                        let b = caps.capsTbl[0];
                        st!(T_INFO, "  CE{} (engine {:#x}): caps {:02x} {:02x}{}{}{}{}", ci, e, b, caps.capsTbl[1],
                            if b & CE_CAPS_GRCE != 0 { " GRCE" } else { "" },
                            if b & CE_CAPS_SHARED != 0 { " shared" } else { "" },
                            if b & CE_CAPS_SYSMEM_READ != 0 { " sysmem-read" } else { "" },
                            if b & CE_CAPS_SYSMEM_WRITE != 0 { " sysmem-write" } else { "" });
                        if chosen.is_none() && b & CE_CAPS_GRCE == 0 {
                            chosen = Some((e, b));
                        }
                    }
                    Err(err) => st!(T_INFO, "  CE{} (engine {:#x}): CE_GET_CAPS_V2 FAILED: {:?}", ci, e, err),
                }
            }
        }
        Err(e) => st!(T_INFO, "GET_ENGINES_V2 FAILED: {:?}", e),
    }
    s.ce_engine = match (chosen, first_ce) {
        (Some((e, _)), _) => e,
        (None, Some(e)) => e,
        (None, None) => engine_copy(2),
    };
    st!(T_INFO, "copy engine for the self-test channel: CE{} (NV2080 engine type {:#x}{})",
        copy_index(s.ce_engine).unwrap_or(0), s.ce_engine,
        if chosen.is_some() { ", first async (non-GRCE) CE" } else if first_ce.is_some() { ", no async CE found: first CE" } else { ", engine list unavailable: guessed" });
    true
}

fn step_vaspace(s: &mut St) -> bool {
    let mut p = NvVaspaceAllocationParameters { index: NV_VASPACE_ALLOCATION_INDEX_GPU_NEW, ..Default::default() };
    match s.c().alloc(s.dev, FERMI_VASPACE_A, &mut p) {
        Ok(h) => {
            s.vas = h;
            st!(T_VASPACE, "FERMI_VASPACE_A {:#x}: base {:#x} size {:#x} big page {:#x}", h, p.vaBase, p.vaSize, p.bigPageSize);
            true
        }
        Err(e) => {
            st!(T_VASPACE, "FERMI_VASPACE_A FAILED: {:?}", e);
            false
        }
    }
}

/// Deterministic, address-dependent test data.
fn pattern_word(i: usize) -> u32 {
    (i as u32).wrapping_add(1).wrapping_mul(0x9e37_79b9) ^ 0x5a5a_0000
}

fn step_memory(s: &mut St) -> bool {
    let plan: [(&'static str, MemLocation, u64, bool); 7] = [
        ("pb", MemLocation::Sysmem, PB_BYTES, true),
        ("gpfifo", MemLocation::Sysmem, 4096, true),
        ("userd", MemLocation::Sysmem, 4096, false),
        ("sem", MemLocation::Sysmem, 4096, true),
        ("src", MemLocation::Sysmem, BIG, true),
        ("dst", MemLocation::Sysmem, BIG, true),
        ("vid", MemLocation::Vidmem, BIG, true),
    ];
    for (name, loc, size, gpu) in plan {
        if s.new_buf(name, loc, size, gpu).is_err() {
            return false;
        }
    }
    // CPU sanity on the sysmem side and the test data.
    let src = s.buf("src").cpu as *mut u32;
    for i in 0..(BIG as usize / 4) {
        unsafe { src.add(i).write_volatile(pattern_word(i)) };
    }
    let ok = (0..(BIG as usize / 4)).step_by(4093).all(|i| unsafe { src.add(i).read_volatile() } == pattern_word(i));
    unsafe { core::ptr::write_volatile(s.sem_ptr(), 0) };
    st!(T_MEMORY, "CPU access to the sysmem mappings {}; src filled with the test pattern",
        if ok { "ok" } else { "BROKEN (read back differs)" });
    ok
}

fn step_channel(s: &mut St) -> bool {
    let c = s.c();
    let mut tp = NvChannelGroupAllocationParameters { hVASpace: s.vas, engineType: s.ce_engine, ..Default::default() };
    let tsg = match c.alloc(s.dev, KEPLER_CHANNEL_GROUP_A, &mut tp) {
        Ok(h) => h,
        Err(e) => {
            st!(T_CHANNEL, "KEPLER_CHANNEL_GROUP_A (engine {:#x}) FAILED: {:?}", s.ce_engine, e);
            return false;
        }
    };
    s.tsg = tsg;
    st!(T_CHANNEL, "channel group {:#x} on engine {:#x}", tsg, s.ce_engine);

    let mut cp = zeroed::<NvChannelAllocParams>();
    cp.gpFifoOffset = s.buf("gpfifo").va;
    cp.gpFifoEntries = GP_ENTRIES;
    cp.hUserdMemory[0] = s.buf("userd").mem;
    cp.userdOffset[0] = 0;
    cp.engineType = s.ce_engine;
    let c = s.c();
    let chan = match c.alloc(tsg, AMPERE_CHANNEL_GPFIFO_A, &mut *cp) {
        Ok(h) => h,
        Err(e) => {
            st!(T_CHANNEL, "AMPERE_CHANNEL_GPFIFO_A FAILED: {:?} (GPFIFO VA {:#x}, {} entries, USERD {:#x})", e,
                cp.gpFifoOffset, GP_ENTRIES, cp.hUserdMemory[0]);
            return false;
        }
    };
    s.chan = chan;
    st!(T_CHANNEL, "channel {:#x}: GPFIFO VA {:#x} ({} entries), USERD = sysmem {:#x}, chid {}", chan, cp.gpFifoOffset,
        GP_ENTRIES, cp.hUserdMemory[0], cp.cid);

    let mut ce = Nvb0b5AllocationParameters { version: NVB0B5_ALLOCATION_PARAMETERS_VERSION_1, engineType: s.ce_engine };
    let c = s.c();
    s.ce = match c.alloc(chan, s.ce_class, &mut ce) {
        Ok(h) => h,
        Err(e) => {
            st!(T_CHANNEL, "copy engine object {:#x} FAILED: {:?}", s.ce_class, e);
            return false;
        }
    };
    st!(T_CHANNEL, "copy engine object {:#x} (class {:#x})", s.ce, s.ce_class);

    let c = s.c();
    s.usermode = match c.alloc_noparams(s.sub, VOLTA_USERMODE_A) {
        Ok(h) => h,
        Err(e) => {
            st!(T_CHANNEL, "VOLTA_USERMODE_A FAILED: {:?}", e);
            return false;
        }
    };
    let c = s.c();
    s.doorbell = match c.map_cpu(s.sub, s.usermode, 0, NVC361_NV_USERMODE__SIZE, OS33_FLAGS_ACCESS_WRITE_ONLY) {
        Ok(p) => p,
        Err(e) => {
            st!(T_CHANNEL, "mapping the USERMODE (doorbell) page FAILED: {:?}", e);
            return false;
        }
    };
    let mut tok = Nvc36fCtrlGetWorkSubmitTokenParams::default();
    if let Err(e) = s.c().control(chan, NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN, &mut tok) {
        st!(T_CHANNEL, "GET_WORK_SUBMIT_TOKEN FAILED: {:?}", e);
        return false;
    }
    s.token = tok.workSubmitToken;
    st!(T_CHANNEL, "doorbell: USERMODE {:#x} mapped at {:p} (+{:#x}), work-submit token {:#x}", s.usermode, s.doorbell,
        NVC361_NOTIFY_CHANNEL_PENDING, s.token);

    let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 1, bSkipSubmit: 0 };
    match s.c().control(chan, NVA06F_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch) {
        Ok(()) => st!(T_CHANNEL, "channel scheduled (NVA06F GPFIFO_SCHEDULE)"),
        Err(e) => {
            st!(T_CHANNEL, "NVA06F GPFIFO_SCHEDULE FAILED: {:?}; trying the channel group", e);
            let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 1, bSkipSubmit: 0 };
            if let Err(e) = s.c().control(tsg, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch) {
                st!(T_CHANNEL, "NVA06C GPFIFO_SCHEDULE FAILED too: {:?}", e);
                return false;
            }
            st!(T_CHANNEL, "channel group scheduled (NVA06C GPFIFO_SCHEDULE)");
        }
    }
    st!(T_CHANNEL, "USERD after scheduling: GP_GET {} GP_PUT {}", s.userd_u32(USERD_GP_GET), s.userd_u32(USERD_GP_PUT));
    s.channel_ok = true;
    true
}

fn step_submit(s: &mut St) -> bool {
    st!(T_SUBMIT, "first submission: SET_OBJECT {:#x} on subchannel {} + semaphore release -> sem VA {:#x}",
        s.ce_class, NVA06F_SUBCHANNEL_COPY_ENGINE, s.buf("sem").va);
    let seg = s.nsub % SEGS;
    let r = s.submit_wait(T_SUBMIT, "the first semaphore", false, |_| {});
    // Log the first push segment: the exact words the GPU was given.
    let pb = s.buf("pb").cpu as *const u32;
    let mut line = String::new();
    for i in 0..10 {
        line.push_str(&alloc::format!(" {:08x}", unsafe { pb.add(seg * SEG_BYTES / 4 + i).read_volatile() }));
    }
    st!(T_SUBMIT, "push words:{}; GPFIFO[0] = {:#018x}", line,
        unsafe { (s.buf("gpfifo").cpu as *const u64).read_volatile() });
    match r {
        Ok(ns) => {
            st!(T_SUBMIT, "semaphore = {:#x} after {} us: the channel runs, the CE executes methods and writes sysmem",
                s.sem_value(), ns / 1000);
            s.submit_ok = true;
            true
        }
        Err(()) => false,
    }
}

/// Compare `n` words at `p` with `want(i)`; log the first mismatches.
fn verify(step: usize, what: &str, p: *const u32, n: usize, want: impl Fn(usize) -> u32) -> bool {
    let mut bad = 0usize;
    for i in 0..n {
        let v = unsafe { p.add(i).read_volatile() };
        let w = want(i);
        if v != w {
            if bad < 4 {
                st!(step, "  {}: word {} (byte {:#x}) = {:#010x}, want {:#010x}", what, i, i * 4, v, w);
            }
            bad += 1;
        }
    }
    st!(step, "{}: {} of {} words correct{}", what, n - bad, n, if bad == 0 { "" } else { " -- MISMATCH" });
    bad == 0
}

fn step_memset(s: &mut St) -> bool {
    let vid = s.buf("vid").va;
    let dst = s.buf("dst").va;
    let src = s.buf("src").va;
    let words = (BIG / 4) as u32;
    let dstp = s.buf("dst").cpu as *mut u32;
    unsafe { core::ptr::write_bytes(dstp as *mut u8, 0, BIG as usize) };

    // (a) fill vidmem, copy it back to sysmem, check.
    let pat = 0xa5c3_3c5a;
    let t = s.submit_wait(T_MEMSET, "vidmem fill + read-back copy", false, |p| {
        p.memset(vid, 0, pat, words, 1, 0);
        p.copy(dst, 0, vid, 0, BIG as u32);
    });
    let Ok(ns) = t else { return false };
    st!(T_MEMSET, "CE filled 1 MiB of vidmem with {:#x} and copied it to sysmem in {} us", pat, ns / 1000);
    let a = verify(T_MEMSET, "vidmem fill (via CE read-back)", dstp, BIG as usize / 4, |_| pat);

    // (b) sysmem -> vidmem -> sysmem round trip of the address-dependent data.
    unsafe { core::ptr::write_bytes(dstp as *mut u8, 0, BIG as usize) };
    let t = s.submit_wait(T_MEMSET, "sysmem -> vidmem -> sysmem copies", false, |p| {
        p.copy(vid, 0, src, 0, BIG as u32);
        p.copy(dst, 0, vid, 0, BIG as u32);
    });
    let Ok(ns) = t else { return false };
    st!(T_MEMSET, "2 x 1 MiB copies in {} us ({} MB/s)", ns / 1000, (2 * BIG) * 1000 / ns.max(1));
    let b = verify(T_MEMSET, "round trip", dstp, BIG as usize / 4, pattern_word);
    a && b
}

fn step_intr(s: &mut St) -> bool {
    let isr0 = crate::device::isr_count();
    let bh0 = crate::device::bh_count();
    let r = s.submit_wait(T_INTR, "semaphore with non-stall interrupt", true, |_| {});
    if r.is_err() {
        return false;
    }
    let t0 = sched::monotonic_ns();
    while crate::device::isr_count() == isr0 && sched::monotonic_ns() - t0 < 200_000_000 {
        sched::kthread::msleep(1);
    }
    let (isr, bh) = (crate::device::isr_count() - isr0, crate::device::bh_count() - bh0);
    st!(T_INTR, "semaphore released; {} ISR(s), {} bottom half(s) since{}", isr, bh,
        if isr == 0 { " (no MSI arrived for the non-stall interrupt within 200 ms: not fatal, completion is polled)" } else { "" });
    true
}

const BAR_COLOURS: [(u32, &str); 8] = [
    (0x00ff_ffff, "white"), (0x00ff_ff00, "yellow"), (0x0000_ffff, "cyan"), (0x0000_ff00, "green"),
    (0x00ff_00ff, "magenta"), (0x00ff_0000, "red"), (0x0000_00ff, "blue"), (0x0080_8080, "grey"),
];

fn step_fb(s: &mut St, env: Option<&Env>) -> bool {
    let Some(env) = env else { return true };
    if env.fb_phys == 0 || env.fb_w == 0 || env.fb_h == 0 || env.fb_pitch == 0 {
        st!(T_FB, "no GOP framebuffer from the boot loader; nothing to draw on");
        return true;
    }
    let (w, h, pitch) = (env.fb_w, env.fb_h, env.fb_pitch);
    let surface = pitch as u64 * h as u64;
    st!(T_FB, "GOP framebuffer: phys {:#x}, {}x{}, pitch {}, {} KiB", env.fb_phys, w, h, pitch, surface / 1024);

    // 1. Where is the GOP surface in VRAM? Ask RM for its reserved console
    //    memory (the region it keeps mapped at BAR1 offset 0 when BAR1 goes
    //    virtual) and map it into our VA space.
    let (mut base, mut mode) = (0u64, 1u32);
    let mut have_target = false;
    match s.c().alloc_noparams(s.dev, NV01_MEMORY_FRAMEBUFFER_CONSOLE) {
        Ok(mem) => {
            s.con_mem = mem;
            let mut pa = Nv0041CtrlGetSurfacePhysAttrParams::default();
            match s.c().control(mem, NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR, &mut pa) {
                Ok(()) => st!(T_FB, "RM console memory {:#x}: VRAM offset {:#x} ({}), contiguous segment {:#x} bytes", mem,
                              pa.memOffset, if pa.memAperture == NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_VIDMEM { "vidmem" } else { "sysmem?" },
                              pa.contigSegmentSize),
                Err(e) => st!(T_FB, "RM console memory {:#x}: GET_SURFACE_PHYS_ATTR FAILED: {:?}", mem, e),
            }
            let len = (surface + 0xfff) & !0xfff;
            let mapped = match s.c().alloc_va(s.dev, s.vas, len, 0x10000, PageSize::Small) {
                Ok((virt, _)) => {
                    s.con_virt = virt;
                    s.c().map_dma(s.dev, virt, mem, 0, len, MapFlags::default())
                }
                Err(e) => Err(e),
            };
            match mapped {
                Ok(va) => {
                    s.con_va = va;
                    base = va;
                    mode = 0;
                    have_target = true;
                    st!(T_FB, "console memory mapped at GPU VA {:#x} ({} KiB)", va, len / 1024);
                }
                Err(e) => {
                    st!(T_FB, "mapping the console memory FAILED: {:?}; falling back to physical CE writes", e);
                    if pa.memAperture == NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_VIDMEM && pa.contigSegmentSize >= surface {
                        base = pa.memOffset;
                        have_target = true;
                    }
                }
            }
        }
        Err(e) => st!(T_FB, "NV01_MEMORY_FRAMEBUFFER_CONSOLE FAILED: {:?} (RM did not reserve the console)", e),
    }
    if !have_target {
        base = s.cfg.fbphys.unwrap_or(0);
        mode = 1;
        st!(T_FB, "using physical VRAM offset {:#x} (nvrm.selftest_fbphys; 0 = BAR1 offset of the GOP surface in pre-GSP physical mode)", base);
    }

    // 2. Prove the target is the scanned-out surface: the CPU writes a marker
    //    through BAR1 (where the console draws), the CE reads the same spot.
    drivers::framebuffer::drm_scanout_claim(SCANOUT_ID);
    s.scanout_claimed = true;
    let rows = 4u32;
    let probe = (rows * pitch) as usize;
    let Some(fbva) = mm::kva::ioremap(env.fb_phys, probe, mm::kva::CacheMode::Uncached) else {
        st!(T_FB, "ioremap of the GOP framebuffer FAILED");
        return false;
    };
    let fbw = fbva as *mut u32;
    let marker = |i: usize| 0x00a0_0000 | ((i as u32 * 0x0101) & 0xffff);
    let mark_words = (w as usize).min(256);
    for y in 0..rows as usize {
        for x in 0..mark_words {
            unsafe { fbw.add(y * pitch as usize / 4 + x).write_volatile(marker(y * 1000 + x)) };
        }
    }
    fence(Ordering::SeqCst);
    // A read through BAR1 pushes the posted writes out to VRAM before the
    // CE looks (UVM's dummy BAR1 read before a doorbell).
    let _ = unsafe { fbw.add((rows as usize - 1) * pitch as usize / 4 + mark_words - 1).read_volatile() };
    let dst = s.buf("dst");
    let (dst_va, dstp) = (dst.va, dst.cpu as *mut u32);
    unsafe { core::ptr::write_bytes(dstp as *mut u8, 0, probe) };
    let r = s.submit_wait(T_FB, "read-back of the first framebuffer rows", false, |p| {
        p.copy(dst_va, 0, base, mode, probe as u32);
    });
    let mut same = 0usize;
    for y in 0..rows as usize {
        for x in 0..mark_words {
            if unsafe { dstp.add(y * pitch as usize / 4 + x).read_volatile() } == marker(y * 1000 + x) {
                same += 1;
            }
        }
    }
    let total = rows as usize * mark_words;
    st!(T_FB, "CPU wrote a marker into the GOP surface through BAR1; the CE read {} of {} marker pixels back from {} {:#x}",
        same, total, if mode == 0 { "GPU VA" } else { "VRAM offset" }, base);
    mm::kva::iounmap(fbva);
    if r.is_err() {
        return false;
    }
    if same != total {
        st!(T_FB, "the CE target is NOT the scanout surface; not drawing (try nvrm.selftest_fbphys=<VRAM offset>)");
        return false;
    }
    if mode == 1 {
        st!(T_FB, "GOP surface confirmed at VRAM offset {:#x}", base);
    }

    // 3. Colour bars.
    let (x0, y0) = (w / 16, h / 4);
    let bw = (w - 2 * x0) / 8;
    let bh = h / 2;
    let draw = |s: &mut St| -> core::result::Result<u64, ()> {
        s.submit_wait(T_FB, "colour bars", false, |p| {
            for (i, (col, _)) in BAR_COLOURS.iter().enumerate() {
                let x = x0 + i as u32 * bw;
                let off = base + y0 as u64 * pitch as u64 + x as u64 * 4;
                p.memset(off, mode, *col, bw, bh, pitch);
            }
        })
    };
    let Ok(ns) = draw(s) else { return false };
    st!(T_FB, "8 colour bars ({}x{} px each, at x={} y={}) drawn by the CE in {} us: {}", bw, bh, x0, y0, ns / 1000,
        BAR_COLOURS.iter().map(|c| c.1).collect::<Vec<_>>().join(" "));
    // Check: the CE reads one line through the bars back; the CPU reads the
    // bar centres through BAR1 (8 uncached reads).
    let ymid = y0 + bh / 2;
    let line_off = ymid as u64 * pitch as u64;
    let r = s.submit_wait(T_FB, "bar line read-back", false, |p| {
        p.copy(dst_va, 0, base + line_off, mode, pitch);
    });
    let mut ce_ok = 0;
    let mut cpu_ok = 0;
    let cpu_line = mm::kva::ioremap(env.fb_phys + line_off, pitch as usize, mm::kva::CacheMode::Uncached);
    for (i, (col, _)) in BAR_COLOURS.iter().enumerate() {
        let x = (x0 + i as u32 * bw + bw / 2) as usize;
        if r.is_ok() && unsafe { dstp.add(x).read_volatile() } & 0x00ff_ffff == *col {
            ce_ok += 1;
        }
        if let Some(l) = cpu_line {
            if unsafe { (l as *const u32).add(x).read_volatile() } & 0x00ff_ffff == *col {
                cpu_ok += 1;
            }
        }
    }
    if let Some(l) = cpu_line {
        mm::kva::iounmap(l);
    }
    st!(T_FB, "bar centres correct: {}/8 via CE read-back, {}/8 via CPU reads of BAR1", ce_ok, cpu_ok);
    crate::device::flush_log();

    // 4. Hold them on screen (the console is paused), repainting every second.
    st!(T_FB, "holding the bars on screen for {} s (nvrm.selftest_hold); the console resumes afterwards", s.cfg.hold_s);
    let mut repaints = 0;
    let end = sched::monotonic_ns() + s.cfg.hold_s * 1_000_000_000;
    while sched::monotonic_ns() < end {
        crate::device::substep("fb");
        sched::kthread::msleep(1000);
        if draw(s).is_err() {
            break;
        }
        repaints += 1;
    }
    st!(T_FB, "{} repaints during the hold", repaints);
    ce_ok == 8
}

fn step_loop(s: &mut St) -> bool {
    let n = s.cfg.loops;
    let vid = s.buf("vid").va;
    let chunk: u32 = 64 * 1024;
    let (isr0, xid0, stubs0) = (crate::device::isr_count(), crate::xid_lines(), crate::stub_hits());
    let (mut min, mut max, mut sum) = (u64::MAX, 0u64, 0u64);
    let t0 = sched::monotonic_ns();
    for i in 0..n {
        let pat = 0x3c00_0000 | i;
        match s.submit_wait(T_LOOP, "loop iteration", false, |p| p.memset(vid, 0, pat, chunk / 4, 1, 0)) {
            Ok(ns) => {
                min = min.min(ns);
                max = max.max(ns);
                sum += ns;
            }
            Err(()) => {
                st!(T_LOOP, "iteration {} of {} did not complete", i, n);
                return false;
            }
        }
        if i % 100 == 99 {
            crate::device::substep("loop");
        }
    }
    let total = sched::monotonic_ns() - t0;
    st!(T_LOOP, "{} fill(64 KiB)+semaphore round trips in {} ms: {} per s; latency min {} us, avg {} us, max {} us",
        n, total / 1_000_000, n as u64 * 1_000_000_000 / total.max(1), min / 1000, sum / n as u64 / 1000, max / 1000);
    // The last fill must be in vidmem.
    let dst = s.buf("dst");
    let (dst_va, dstp) = (dst.va, dst.cpu as *const u32);
    let last = 0x3c00_0000 | (n - 1);
    if s.submit_wait(T_LOOP, "final read-back", false, |p| p.copy(dst_va, 0, vid, 0, chunk)).is_err() {
        return false;
    }
    let ok = verify(T_LOOP, "last fill", dstp, chunk as usize / 4, |_| last);
    let (isr, xid, stubs) = (crate::device::isr_count() - isr0, crate::xid_lines() - xid0, crate::stub_hits() - stubs0);
    st!(T_LOOP, "during the loop: {} ISRs, {} RM Xid lines, {} stub hits", isr, xid, stubs);
    ok && xid == 0
}

fn step_teardown(s: &mut St) -> bool {
    let mut bad = 0u32;
    let mut freed = 0u32;
    if s.scanout_claimed {
        drivers::framebuffer::drm_scanout_release(SCANOUT_ID);
        s.scanout_claimed = false;
    }
    let Some(c) = s.c.take() else {
        st!(T_TEARDOWN, "nothing allocated");
        return true;
    };
    let mut note = |what: &str, h: Handle, r: Result<()>| {
        match r {
            Ok(()) => freed += 1,
            Err(e) => {
                bad += 1;
                nvlog!("[nvrm] +{}ms selftest teardown: {} {:#x}: {:?}\n", ms(), what, h, e);
            }
        }
    };
    // Reverse order of allocation.
    if s.con_va != 0 {
        note("unmap console", s.con_mem, c.unmap_dma(s.dev, s.con_virt, s.con_mem, s.con_va));
    }
    if s.con_virt != 0 {
        note("free console VA", s.con_virt, c.free(s.dev, s.con_virt));
    }
    if s.con_mem != 0 {
        note("free console memory", s.con_mem, c.free(s.dev, s.con_mem));
    }
    if !s.doorbell.is_null() {
        note("unmap USERMODE", s.usermode, c.unmap_cpu(s.sub, s.usermode, s.doorbell));
    }
    if s.usermode != 0 {
        note("free USERMODE", s.usermode, c.free(s.sub, s.usermode));
    }
    if s.ce != 0 {
        note("free CE object", s.ce, c.free(s.chan, s.ce));
    }
    if s.chan != 0 {
        note("free channel", s.chan, c.free(s.tsg, s.chan));
    }
    if s.tsg != 0 {
        note("free channel group", s.tsg, c.free(s.dev, s.tsg));
    }
    while let Some(b) = s.bufs.pop() {
        if !b.cpu.is_null() {
            note(b.name, b.mem, c.unmap_cpu(s.dev, b.mem, b.cpu));
        }
        if b.va != 0 {
            note(b.name, b.mem, c.unmap_dma(s.dev, b.virt, b.mem, b.va));
        }
        if b.virt != 0 {
            note(b.name, b.virt, c.free(s.dev, b.virt));
        }
        let parent = if b.loc == MemLocation::Vidmem { s.sub } else { s.dev };
        note(b.name, b.mem, c.free(parent, b.mem));
    }
    if s.vas != 0 {
        note("free VA space", s.vas, c.free(s.dev, s.vas));
    }
    if s.sub != 0 {
        note("free subdevice", s.sub, c.free(s.dev, s.sub));
    }
    if s.dev != 0 {
        note("free device", s.dev, c.free(c.handle(), s.dev));
    }
    let h = c.handle();
    note("free client", h, c.destroy());
    st!(T_TEARDOWN, "{} RM calls ok, {} failed", freed, bad);
    bad == 0
}

/// Give the console back if the colour-bar demo still holds the scanout
/// (the watchdog parks the RM thread mid-step).
pub(crate) fn release_console() {
    drivers::framebuffer::drm_scanout_release(SCANOUT_ID);
}
