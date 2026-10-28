//! PID-1 init task — first process after the kernel bootstraps.
//!
//! Sets up the in-kernel servers (VFS, net, TTY), probes hardware drivers,
//! then extracts `bin/init` from the initrd and spawns it as the first user
//! process — everything after that (getty, login, the shell) is userland.

use crate::serial_print_str;
use crate::serial_print_hex;
use mm::paging::PageFlags;

/// Gate the `[CPIO]` initrd-lookup debug prints. `find_in_initrd` runs on every
/// execve that misses the VFS/RamFS (the initrd only holds `bin/init`), so a
/// process-spawn-heavy workload floods the serial console with per-file compare
/// lines. Off by default; flip to `true` when debugging initrd extraction.
const DBG_CPIO: bool = false;

extern "C" {
    fn arch_alloc_page_table_root() -> usize;
}

/// Per-task teardown run by the scheduler when a task exits.
fn task_exit_cleanup(pid: u32) {
    ipc::port::release_by_owner(pid);
    evdev_server::release_pid(pid);
}

/// The main entry point for the kernel's init task.
pub fn init_task_main(boot_info: &boot::BootInfo) {
    serial_print_str("[INIT] Kernel init task starting\n");

    // Registers the task-exit hook that releases a dying process's IPC reply
    // ports (ipc::port::release_by_owner). Without this, every process that
    // ever calls into a mounted filesystem (which lazily allocates its own
    // reply port on first use) leaks that port forever — the port table's
    // real backing storage is only 64 buckets, so the 64th distinct process
    // to touch a mounted fs permanently exhausts it for the rest of the
    // uptime. See servers/vfs's call_port: a failed port::create() there is
    // treated as silent success (an empty, all-zero reply), so callers like
    // sys_execve's ELF-size lookup see a phantom empty file instead of an
    // error.
    // Held spin::Mutex accounting for might_sleep / preemption (sched::spinheld).
    sched::spinheld::init();
    ipc::init();
    // ipc::init registered port::release_by_owner; the hook is a single slot,
    // so replace it with one that also retires the exiting task's pid-keyed
    // evdev queues (they have no VFS_CLOSE of their own).
    sched::register_task_exit_hook(task_exit_cleanup);

    // Wire the mm crate's file-backed-VMA hooks to the kernel's exec-file
    // registry (demand-paged exec reads ELF pages in from page faults).
    crate::syscall::init_exec_file_backing();

    // Make every path out of a task — including signal-initiated death, which
    // bypasses the EXIT syscall entirely — release its fds, pipes, sockets and
    // TTY state. See syscall::init_exit_teardown.
    crate::syscall::init_exit_teardown();

    // ── In-Kernel Servers ──────────────────────────────────────────────────
    if let Some(vfs_port) = vfs_server::init(0) {
        crate::syscall::set_vfs_server_port(vfs_port);
        serial_print_str("[INIT] VFS server port: ");
        crate::print_number(vfs_port);
        serial_print_str("\n");
    }
    if let Some(p) = evdev_server::init(0) {
        serial_print_str("[INIT] evdev server port: ");
        crate::print_number(p);
        serial_print_str("\n");
        // Opt-in: serial-console bytes also typed on a "serial-kbd" evdev
        // node (openbugs L12 #67; off by default, the serial line is a tty).
        if crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "evdev.serialkbd=1")) {
            let n = evdev_server::set_serial_kbd(true);
            serial_print_str("[INIT] evdev.serialkbd=1: serial-kbd is event");
            crate::print_number(n as _);
            serial_print_str("\n");
        }
    }
    drivers::virtio_keyboard::init();

    // Initialize DRM server and check for success
    if let Some(p) = drm_server::init(0) {
        serial_print_str("[INIT] DRM server port: ");
        crate::print_number(p);
        serial_print_str("\n");
    } else {
        serial_print_str("[INIT] ERROR: DRM server initialization failed\n");
    }

    // Initialize PipeWire server
    match pipewire_server::init(crate::boot_cmdline().unwrap_or("")) {
        Ok(p) => {
            serial_print_str("[INIT] PipeWire server port: ");
            crate::print_number(p);
            serial_print_str("\n");
            crate::syscall::set_audio_server_port(p);
        },
        Err(_) => {
            serial_print_str("[INIT] ERROR: PipeWire server initialization failed\n");
        }
    }

    // K2 poll-deadline tick hook: wakes finite-timeout poll/select/epoll_wait
    // waiters and timerfd sleepers on time (audio keeps its own hook slot).
    sched::register_tick_hook(crate::syscall::poll_deadline_tick);
    // …and the same service from each CPU's one-shot deadline interrupt, so a
    // timed wait wakes at its deadline rather than at the next tick edge.
    sched::register_deadline_hook(crate::syscall::poll_deadline_service);
    sched::register_clock_ns(crate::syscall::clock_ns_for_sched);
    // Address-space teardown passes a reschedule point every few hundred
    // pages, so a multi-GiB exit does not hold its CPU with IRQs masked.
    mm::vmm::register_teardown_resched(sched::kthread::cond_resched);
    // CPU-clock timers (ITIMER_VIRTUAL/PROF, CPU-time POSIX timers) fire on
    // their group's returns to user space.
    sched::register_cpu_timer_hook(tty_server::service_cpu_timers);
    sched::register_timer_dequeue_hook(tty_server::timer_dequeued);

    // ── Block Devices & Filesystems ──────────────────────────────────────────
    // SD-card boards: `sdhci.multi=1` / `sdhci.multitest=1` before the probe.
    #[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
    drivers::sdhci::configure(crate::boot_cmdline().unwrap_or(""));
    drivers::blkdev::init();
    // Raspberry Pi 5: ACT LED on SD I/O and the power button (KEY_POWER);
    // BCM2712 GPIO only, no PCIe. `pi5.board=0` disables it.
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    if !crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "pi5.board=0")) {
        drivers::pi5_board::init();
    }
    // Raspberry Pi 5: PCIe root complex for RP1 (Ethernet, USB). Opt-in until
    // hardware-verified; bounded (<= ~0.4 s) and inert on failure.
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    if crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "pcie.rpi5=1")) {
        drivers::rp1::GEM_IRQ.store(crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "rp1.irq=1")),
            core::sync::atomic::Ordering::Relaxed);
        drivers::brcmstb_pcie::configure(crate::boot_cmdline().unwrap_or(""));
        drivers::brcmstb_pcie::init();
        if drivers::brcmstb_pcie::is_up() && drivers::brcmstb_pcie::stage(16) {
            drivers::rp1::probe();
        }
        drivers::brcmstb_pcie::stage_done();
        drivers::pci::serial_debug("[pcie] state=");
        drivers::pci::serial_debug(drivers::brcmstb_pcie::state());
        drivers::pci::serial_debug("\n");
    }
    // Raspberry Pi 5: power the CYW43455 and bring sdio2 up as an SDIO host.
    // One `[sdio]` line either way; inert on failure, bounded to ~2 s.
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    drivers::brcmf::sdio_host::boot_probe();
    // Publish the disks to the VFS, which owns /dev/vd*, /dev/loopN and the
    // synthesized /sys/class/block tree. `vfs-server` deliberately does not
    // depend on `drivers` (the VFS sits below the device layer), so the
    // backend crosses the boundary as this vtable — the same shape the DRM
    // dmabuf hook uses. Registered here because this crate is the one place
    // that already depends on both.
    if let Some(n) = crate::boot_cmdline().and_then(|c| c.split_ascii_whitespace()
        .find_map(|t| t.strip_prefix("sched.check=")?.parse::<u64>().ok())) {
        sched::set_percpu_check_ticks(n);
    }
    if let Some(c) = crate::boot_cmdline() {
        serial_print_str("[BLK] kernel cmdline: ");
        vfs_server::cmdline_redact::redact(c, serial_print_str); // never echo wifi.psk= etc.
        serial_print_str("\n");
        // Test hook (espresettest, openbugs #80): a kernel that never reaches
        // userland -- it hangs before init runs, so this boot cannot change
        // anything on the boot medium.
        if c.split_ascii_whitespace().any(|t| t == "leandros.testhang=1") {
            serial_print_str("[BOOT] leandros.testhang=1: hanging before userland\n");
            loop {
                core::hint::spin_loop();
            }
        }
    }
    drivers::cmdline_secrets::stashed_keys(|k| {
        serial_print_str("[BLK] kernel cmdline: secret knob ");
        serial_print_str(k);
        serial_print_str("= lifted off the command line (deprecated; use a root-only file)\n");
    });
    // The disk Limine loaded us from: the /dev/root tie-break when several
    // removable f2fs volumes qualify. Logged so a wrong pick can be explained.
    {
        let mut txt = [0u8; 36];
        let (kind, n) = match crate::boot_disk() {
            Some(drivers::blkdev::BootDisk::Gpt(g)) =>
                ("GPT disk GUID ", drivers::partscan::PartUuid::Gpt(g).format(&mut txt)),
            Some(drivers::blkdev::BootDisk::Mbr(s)) =>
                ("MBR disk signature ", drivers::partscan::PartUuid::Mbr { disk_sig: s, pno: 0 }.format(&mut txt) - 3),
            None => ("unknown", 0),
        };
        serial_print_str("[BLK] boot disk: ");
        serial_print_str(kind);
        serial_print_str(core::str::from_utf8(&txt[..n]).unwrap_or("?"));
        serial_print_str("\n");
    }
    static DISK_OPS: vfs_server::block::DiskOps = vfs_server::block::DiskOps {
        disk_count: || drivers::blkdev::device_count(),
        // Names, parents and sizes straight from the registry (no I/O): the
        // VFS mirrors every disk and kernel-scanned partition, including ones
        // a USB disk registers long after boot.
        describe: |i| drivers::blkdev::describe(i).map(|d| vfs_server::block::DiskInfo {
            name: d.name,
            nlen: d.nlen,
            parent: d.parent,
            pno: d.pno,
            start: d.start_bytes,
            bytes: d.bytes,
            lbs: d.sector_size,
            removable: d.removable,
            scsi: d.class == "sd",
            nvme: d.class == "nvme",
            ro: d.read_only,
            ordinal: d.ordinal,
            gone: d.gone,
        }),
        removed_gen: || drivers::blkdev::removed_generation(),
        read_blocks: |i, blk, buf| drivers::blkdev::read_blocks(i, blk, buf),
        write_block: |i, blk, buf| match <&[u8; 4096]>::try_from(buf) {
            Ok(b) => drivers::blkdev::write_block(i, blk, b),
            Err(_) => false,
        },
        flush: |i| drivers::blkdev::flush(i),
    };
    vfs_server::block::init(&DISK_OPS);

    // ── USB ───────────────────────────────────────────────────────────────────
    usb::xhci::isoch::set_cmdline(crate::boot_cmdline().unwrap_or(""));
    usb::uvc::set_cmdline(crate::boot_cmdline().unwrap_or(""));
    drivers::usb_hcd::init();
    // The xHCI BIOS handoff above ends the firmware's USB legacy (PS/2)
    // emulation: an i8042 that was only ever that emulation is gone now.
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::keyboard::recheck("after the xHCI BIOS handoff");
    // USB sticks become sdX block devices (and their partitions sdXN) before
    // userspace init runs, so its /dev/root lookup finds a USB root on the
    // first try; init still retries for 30 s for anything slower.
    drivers::usb_storage::set_cmdline(crate::boot_cmdline().unwrap_or(""));
    drivers::usb_storage::init();
    // Read-only NVIDIA GPU probe (no-op unless an NVIDIA display function
    // exists). Runs before klog::init so its lines sit in the RAM ring and are
    // written out by the registration flush. Options: `nvprobe.*` on the
    // Limine cmdline.
    drivers::nvidia_probe::set_cmdline(crate::boot_cmdline().unwrap_or(""));
    // PS/2 mouse / touchpad on the i8042 AUX port (needs the command line
    // for its psmouse.* knobs, and the xHCI handoff done).
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::psmouse::init();
    vfs_server::set_kernel_cmdline(crate::boot_cmdline().unwrap_or("")); // /proc/cmdline
    vfs_server::set_proc_pcipm(drivers::pci::pm_report); // /proc/pcipm
    vfs_server::set_proc_klog(|| crate::klog::tail(256 << 10)); // /proc/klog (dmesg; boards with no ESP)
    {
        // Wedge diagnostics: on by default on the Pi 5 (wedge=0 turns them
        // off), opt-in (wedge=1) elsewhere.
        let cl = crate::boot_cmdline().unwrap_or("");
        let off = cl.split_ascii_whitespace().any(|t| t == "wedge=0");
        let on = cl.split_ascii_whitespace().any(|t| t == "wedge=1");
        sched::wedge::set_enabled(on || (cfg!(feature = "rpi5") && !off));
        sched::wedge::armed_banner(sched::active_cpu_count());
    }
    if crate::boot_cmdline().unwrap_or("").split_ascii_whitespace().any(|t| t == "wdog.selftest=1") {
        sched::wdog_selftest_arm();
    }
    drivers::nvidia_probe::run(
        boot_info.rsdp_addr, boot_info.hhdm_offset,
        boot_info.framebuffer_base, boot_info.framebuffer_width,
        boot_info.framebuffer_height, boot_info.framebuffer_pitch_bytes() as u32,
    );
    // NVMe SSDs (read-only unless nvme.rw=1): registered before klog::init so
    // the bring-up diagnostics reach the stick's log partition.
    drivers::nvme::init(crate::boot_cmdline().unwrap_or(""));
    drivers::i915::run(&drivers::i915::BootEnv { fb_phys: boot_info.framebuffer_base, fb_width: boot_info.framebuffer_width, fb_height: boot_info.framebuffer_height, fb_pitch: boot_info.framebuffer_pitch_bytes() as u32 }); // Intel Gen9 GPU probe (+ `i915.*` bring-up)
    #[cfg(target_arch = "aarch64")] arch_aarch64::exception::SERROR_ERRREC.store(crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "serror.errrec=1")), core::sync::atomic::Ordering::Relaxed);
    // V3D probe (opt-in v3d.probe=1), bracketed by SError checkpoints: one before, so an
    // abort left by an earlier probe is not pinned on V3D, and the probe's own after
    // each hardware phase. Any SError is logged and survived (arch exception.rs).
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))] {
        if let Some(syn) = arch_aarch64::exception::serror_checkpoint() {
            drivers::pci::serial_debug(&alloc::format!("[SERROR] pending before the v3d probe syn=0x{:016x} (an earlier probe)\n", syn));
        }
        drivers::v3d::probe_boot(crate::boot_cmdline().unwrap_or(""), boot_info.hhdm_offset, arch_aarch64::exception::serror_checkpoint);
        // RNG, firmware RTC, PM watchdog, thermal, ARM clock: one line each (lane pimisc).
        drivers::rpi_misc::boot(crate::boot_cmdline().unwrap_or(""), boot_info.hhdm_offset);
        // Serial reset sequence on uart10 (lane piwdt): 5x Ctrl-] in 2 s or BREAK 'b'.
        if let Some(h) = drivers::rpi_misc::serial_reset_hook() {
            arch_aarch64::uart::RX_BYTE_HOOK.store(h as usize, core::sync::atomic::Ordering::Release);
        }
        // BCM2712 GPIO/pinctrl/l2-intc (one [gpio] line) and OTP serial + MACs (one [otp] line), lane pigio.
        drivers::bcm2712_gpio::boot(arch_aarch64::exception::serror_checkpoint);
        drivers::bcm2712_otp::boot();
    }
    // HDMI0/1 read-only probe (hdmi.probe=0 disables): no display-register writes,
    // SError checkpoint after each block read.
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))] {
        if let Some(syn) = arch_aarch64::exception::serror_checkpoint() {
            drivers::pci::serial_debug(&alloc::format!("[SERROR] pending before the hdmi probe syn=0x{:016x} (an earlier probe)\n", syn));
        }
        drivers::hdmi_probe::probe_boot(crate::boot_cmdline().unwrap_or(""), boot_info.hhdm_offset, arch_aarch64::exception::serror_checkpoint);
    }
    // dma40 engine self-test (64 KiB mem-to-mem copy, one [dma40] line), checkpointed like v3d.
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))] {
        if let Some(syn) = arch_aarch64::exception::serror_checkpoint() {
            drivers::pci::serial_debug(&alloc::format!("[SERROR] pending before the dma40 selftest syn=0x{:016x} (an earlier probe)\n", syn));
        }
        drivers::bcm2712_dma40::boot(crate::boot_cmdline().unwrap_or(""), arch_aarch64::exception::serror_checkpoint);
    }
    // HDMI audio binds late: PipeWire's first init ran before the HDMI probe, so retry once now.
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))] {
        drivers::hdmi_audio::set_serror_check(arch_aarch64::exception::serror_checkpoint);
        if !pipewire_server::is_initialized() {
            if let Ok(p) = pipewire_server::init(crate::boot_cmdline().unwrap_or("")) {
                serial_print_str("[INIT] PipeWire server port (HDMI audio): ");
                crate::print_number(p);
                serial_print_str("\n");
                crate::syscall::set_audio_server_port(p);
            }
        }
    }
     #[cfg(all(target_arch = "aarch64", feature = "rpi5"))] { drivers::brcmf_boot::start(crate::boot_cmdline().unwrap_or("")); drivers::bt::boot::start(crate::boot_cmdline().unwrap_or(""), arch_aarch64::exception::serror_checkpoint, |port| vfs_server::register_device_notify_open("/dev/hci0", port, 0, vfs_server::HCI_MAJOR, vfs_server::HCI_MINOR)); } // Pi 5 V3D power-up + ident (one [v3d] line); CYW43455 firmware bring-up on a kthread ([brcmf] lines)
    // Persist the console log on the boot disk's log partition, if it has one.
    crate::klog::init(); crate::iostat::init(); crate::hungtask::init(); // + the 5 s [usbms]/[fslock] summary task
    #[cfg(all(feature = "nvidia", target_arch = "x86_64"))] nvidia_rm::init(&nvidia_rm::BootEnv { cmdline: crate::boot_cmdline().unwrap_or(""), fb_phys: boot_info.framebuffer_base, fb_width: boot_info.framebuffer_width, fb_height: boot_info.framebuffer_height, fb_pitch: boot_info.framebuffer_pitch_bytes() as u32, log_flush: crate::klog::flush_now, rsdp_phys: boot_info.rsdp_addr }); // OpenRM; one line so the default kernel's panic line numbers (and bytes) stay unchanged
    // ── Network Stack ────────────────────────────────────────────────────────
    drivers::netdev::init(crate::boot_cmdline().unwrap_or(""));
    net_server::init();
    sched::spawn(net_server::net_daemon, 0).map(|p| sched::set_comm(p, b"netd"));
    crate::netwatch::init();
    net_server::netctl::init(crate::boot_cmdline().unwrap_or("")); // /dev/netctl
    // Block devices are initialized, but f2fs disk mounting is deferred to userspace init.

    // ── Userspace Init ───────────────────────────────────────────────────────
    // We attempt to load the 'init' server from the initrd.
    serial_print_str("[INIT] Loading userspace init ELF binary from initrd\n");
    
    let mut actual_initrd_base = boot_info.initrd_base as usize;
    let mut actual_initrd_size = boot_info.initrd_size as usize;

    if actual_initrd_base == 0 {
        serial_print_str("[INIT] No initrd in boot info, trying memory scan...\n");
        if let Some((base, size)) = scan_memory_for_initrd() {
            actual_initrd_base = base;
            actual_initrd_size = size;

            // Persist the scanned location back into the global BootInfo so
            // later consumers — notably sys_execve, which reads it through
            // BOOT_INFO_PTR — can also find the initrd (e.g. for /bin/shell).
            let bi_ptr = crate::BOOT_INFO_PTR.load(core::sync::atomic::Ordering::SeqCst);
            if bi_ptr != 0 {
                unsafe {
                    let bi = &mut *(bi_ptr as *mut boot::BootInfo);
                    bi.initrd_base = base as u64;
                    bi.initrd_size = size as u64;
                }
            }
        }
    }

    if actual_initrd_base != 0 {
        serial_print_str("[INIT] Found initrd at physical ");
        serial_print_hex(actual_initrd_base);
        serial_print_str(" size ");
        serial_print_hex(actual_initrd_size);
        serial_print_str("\n");

        // Create a temporary BootInfo for extraction
        let tmp_info = boot::BootInfo {
            memory_map:          boot_info.memory_map,
            memory_map_len:      boot_info.memory_map_len,
            framebuffer_base:    boot_info.framebuffer_base,
            framebuffer_width:   boot_info.framebuffer_width,
            framebuffer_height:  boot_info.framebuffer_height,
            framebuffer_pitch:   boot_info.framebuffer_pitch,
            framebuffer_size:    boot_info.framebuffer_size,
            rsdp_addr:           boot_info.rsdp_addr,
            uart_base:           boot_info.uart_base,
            pci_ecam_base:       boot_info.pci_ecam_base,
            // Use the values resolved above (which may come from the memory
            // scan), not the raw boot_info fields — on direct boot the latter
            // are zero and the CPIO parser would see no initrd.
            initrd_base:         actual_initrd_base as u64,
            initrd_size:         actual_initrd_size as u64,
            hhdm_offset:         boot_info.hhdm_offset,
            cmdline_ptr:         boot_info.cmdline_ptr,
            cmdline_len:         boot_info.cmdline_len,
            efi_system_table:    boot_info.efi_system_table,
        };


        if let Some(init_elf) = extract_binary_from_initrd("bin/init", &tmp_info) {
            serial_print_str("[INIT] Successfully extracted init binary from initrd\n");
            
            // Register initrd with VFS so it can find files later (like doom1.wad)
            vfs_server::set_initrd(actual_initrd_base, actual_initrd_size);

            // Debug framebuffer and HHDM before registering with VFS
            serial_print_str("[INIT] Framebuffer debug info:\n");
            serial_print_str("[INIT]   Physical base: ");
            crate::serial_print_hex(boot_info.framebuffer_base as usize);
            serial_print_str("\n[INIT]   Resolution: ");
            crate::print_number(boot_info.framebuffer_width);
            serial_print_str("x");
            crate::print_number(boot_info.framebuffer_height);
            serial_print_str("\n[INIT]   Pitch: ");
            crate::print_number(boot_info.framebuffer_pitch);

            // Test virtual address conversion
            let fb_virt = mm::phys_to_virt(boot_info.framebuffer_base as usize);
            serial_print_str("\n[INIT]   Virtual address: ");
            crate::serial_print_hex(fb_virt);

            // Check if virtual address is in valid kernel space
            if fb_virt >= 0xFFFF_0000_0000_0000 {
                serial_print_str("\n[INIT]   Virtual address is in kernel space - OK\n");
            } else {
                serial_print_str("\n[INIT]   WARNING: Virtual address is NOT in kernel space!\n");
            }

            // Registering framebuffer with VFS is now centralized in main.rs
            // to ensure correct pitch heuristics are applied.

            // Debug: Log framebuffer console resolution
            serial_print_str("[INIT] Framebuffer console resolution: ");
            crate::print_number(boot_info.framebuffer_width);
            serial_print_str("x");
            crate::print_number(boot_info.framebuffer_height);
            serial_print_str(" pitch=");
            crate::print_number(boot_info.framebuffer_pitch);
            serial_print_str("\n");

            // Load and spawn the ELF
            let pid = load_and_spawn_elf(init_elf);
            serial_print_str("[INIT] Userspace init spawned with PID: ");
            crate::print_number(pid);
            serial_print_str("\n");
        } else {
            serial_print_str("[INIT] Error: Could not find bin/init in initrd\n");
            
            // Fallback: try "init" (no bin prefix)
             if let Some(init_elf) = extract_binary_from_initrd("init", &tmp_info) {
                serial_print_str("[INIT] Successfully extracted 'init' (fallback) from initrd\n");
                let pid = load_and_spawn_elf(init_elf);
                serial_print_str("[INIT] Userspace init spawned with PID: ");
                crate::print_number(pid);
                serial_print_str("\n");
             }
        }
    } else {
        serial_print_str("[INIT] Error: No initrd found!\n");
    }

    // Deferred-work worker and the request_firmware backend (lane kfacil),
    // then the boot self-tests if `ktest=` is on the command line.
    if !sched::workqueue::init_system() {
        serial_print_str("[INIT] could not start the system workqueue\n");
    }
    crate::firmware::init();
    sched::register_dump_hook(sched::workqueue::dump);
    crate::ktest::maybe_start();
    #[cfg(all(feature = "nvidia", target_arch = "x86_64"))] nvidia_rm::start_thread(); // [nvrm] thread, after init took its pid; one line, see above
    serial_print_str("[INIT] Starting scheduler loop...\n");
    crate::console_staging_enable();
    sched::run();
}

/// Walk a CPIO (newc) archive at `base_virt` and return its total length up to
/// and including the `TRAILER!!!` sentinel. Returns 0 if there is no valid CPIO
/// archive at that address. `base_virt` must be a directly-readable address
/// (identity- or HHDM-mapped).
pub fn cpio_image_size(base_virt: usize) -> usize {
    let p = base_virt as *const u8;
    let rd = |o: usize| unsafe { core::ptr::read_volatile(p.add(o)) };
    let parse_hex = |start: usize| -> usize {
        let mut v = 0usize;
        for i in 0..8 {
            let c = rd(start + i);
            let d = match c {
                b'0'..=b'9' => (c - b'0') as usize,
                b'a'..=b'f' => (c - b'a' + 10) as usize,
                b'A'..=b'F' => (c - b'A' + 10) as usize,
                _ => return v,
            };
            v = (v << 4) | d;
        }
        v
    };

    let mut offset = 0usize;
    loop {
        // Validate the newc magic "070701" at the current entry.
        for (i, b) in b"070701".iter().enumerate() {
            if rd(offset + i) != *b { return offset; }
        }
        let namesize = parse_hex(offset + 94);
        let filesize = parse_hex(offset + 54);
        let name_start = offset + 110;

        // The archive ends at the TRAILER!!! entry (which carries no data).
        let trailer = b"TRAILER!!!";
        if namesize >= trailer.len() + 1
            && trailer.iter().enumerate().all(|(i, b)| rd(name_start + i) == *b)
        {
            return (name_start + namesize + 3) & !3;
        }

        let file_start = (name_start + namesize + 3) & !3;
        offset = (file_start + filesize + 3) & !3;
        // Sanity bound: never walk past 1.5 GiB of archive.
        if offset > 0x6000_0000 { return offset; }
    }
}

/// Locate a file in the CPIO initrd and return its data.
pub fn extract_binary_from_initrd(name: &str, boot_info: &boot::BootInfo) -> Option<&'static [u8]> {
    let base = boot_info.initrd_base;
    let size = boot_info.initrd_size;

    if base == 0 || size == 0 { 
        serial_print_str("[CPIO] No initrd available\n");
        return None; 
    }

    let initrd_virt = mm::phys_to_virt(base as usize) as *const u8;
    let initrd_slice = unsafe { core::slice::from_raw_parts(initrd_virt, size as usize) };

    // Make sure the initrd is valid before trying to parse it
    if initrd_slice.len() < 110 {
        serial_print_str("[CPIO] Initrd too small to contain CPIO header\n");
        return None;
    }

    // Diagnostic
    if DBG_CPIO {
    serial_print_str("[CPIO] First 16 bytes of initrd: ");
    for i in 0..16 {
        if i < initrd_slice.len() {
            crate::serial_print_hex(initrd_slice[i] as usize);
            serial_print_str(" ");
        }
    }
    serial_print_str("\n");
    }

    // ── Simple CPIO (newc) parser ───────────────────────────────────────────
    let mut offset = 0;
    let target_name = name.trim_start_matches('/').trim_start_matches("./");
    if DBG_CPIO {
    serial_print_str("[CPIO] Looking for file: ");
    serial_print_str(target_name);
    serial_print_str("\n");
    }

    while offset + 110 <= initrd_slice.len() {
        // Check for valid CPIO header
        if initrd_slice[offset] != b'0' || initrd_slice[offset+1] != b'7' || 
           initrd_slice[offset+2] != b'0' || initrd_slice[offset+3] != b'7' || 
           initrd_slice[offset+4] != b'0' || initrd_slice[offset+5] != b'1' {
            // Check for GZIP magic 1f 8b
            if initrd_slice[offset] == 0x1f && initrd_slice[offset+1] == 0x8b {
                serial_print_str("[CPIO] Found GZIP initrd - extraction NOT SUPPORTED\n");
            } else if offset == 0 {
                serial_print_str("[CPIO] Invalid magic at offset 0: ");
                serial_print_str(unsafe { core::str::from_utf8_unchecked(&initrd_slice[offset..offset+6]) });
                serial_print_str("\n");
            }
            break;
        }

        let namesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+94..offset+102]).unwrap_or("0"), 16).unwrap_or(0);
        let filesize = usize::from_str_radix(core::str::from_utf8(&initrd_slice[offset+54..offset+62]).unwrap_or("0"), 16).unwrap_or(0);

        if namesize == 0 { 
            serial_print_str("[CPIO] Zero namesize, breaking\n");
            break; 
        }

        let name_start = offset + 110;
        if name_start + namesize > initrd_slice.len() { 
            serial_print_str("[CPIO] Name start + namesize exceeds slice length\n");
            break; 
        }
        
        let file_name = core::str::from_utf8(&initrd_slice[name_start..name_start + namesize - 1]).unwrap_or("");
        let current_entry_name = file_name.trim_start_matches('/').trim_start_matches("./");
        
        // Debug: Print the file name being compared
        if DBG_CPIO {
        serial_print_str("[CPIO] Comparing with file: ");
        serial_print_str(current_entry_name);
        serial_print_str("\n");
        }
        
        // Align to 4 bytes
        let file_start = (name_start + namesize + 3) & !3;
        
        if current_entry_name == target_name {
            if file_start + filesize > initrd_slice.len() { 
                serial_print_str("[CPIO] File data out of bounds\n");
                return None; 
            }
            serial_print_str("[CPIO] Found file in initrd\n");
            return Some(unsafe { core::slice::from_raw_parts(initrd_virt.add(file_start), filesize) });
        }

        offset = (file_start + filesize + 3) & !3;
    }

    // Silent: a miss here is the normal case for every exec PATH probe once
    // the F2FS root is mounted (binaries live in the VFS, not the initrd).
    None
}

/// Helper to load an ELF binary and create a task for it.
fn load_and_spawn_elf(elf_data: &[u8]) -> u32 {
    let root = unsafe { arch_alloc_page_table_root() };
    let mut as_ = mm::vmm::AddressSpace::new(root);
    // The boot init binary is a static ET_EXEC (relibc); bias 0 keeps the
    // historical literal-vaddr placement.
    let elf_info = elf::load(elf_data, &mut as_, 0).expect("failed to load ELF");
    let entry = elf_info.entry;
    
    // ── Map userspace stack ─────────────────────────────────────────────────
    // 1 MiB stack ending at 0x0000_1000_0000 (256MB)
    let stack_top = 0x1000_0000usize;
    let stack_size = 0x100000usize;
    let stack_base = stack_top - stack_size;
    let user_sp = stack_top - 64; // Well within mapping and 16-byte aligned
    
    // Demand-paged like an exec'd stack; only the top page is populated.
    let ok = as_.map_lazy(
        stack_base,
        stack_size,
        PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE,
        false,
    );
    if !ok { panic!("failed to map userspace stack"); }
    as_.stack_top = stack_top;
    as_.prefault_range(stack_top - 4096, 4096);

    // ── Initialize userspace stack with zeros ───────────────────────────────
    let zero = [0u8; 64];
    if !as_.write_user_buf(user_sp, &zero) { panic!("failed to initialize user stack"); }

    let pid = sched::spawn_user_with_address_space(entry, user_sp, as_).expect("failed to spawn init");
    // The first (and only) boot-loaded ELF is init: the process every orphan
    // is reparented to.
    if sched::init_pid() == 0 { sched::set_init_pid(pid); }
    
    serial_print_str("[INIT] load_and_spawn_elf: entry=");
    serial_print_hex(entry);
    serial_print_str(" sp=");
    serial_print_hex(user_sp);
    serial_print_str("\n");

    pid
}

/// Scan a range of physical memory for a CPIO signature.
fn scan_memory_for_initrd() -> Option<(usize, usize)> {
    serial_print_str("[INIT-SCAN] Searching for initrd magic (070701)...\n");

    let start: usize;
    let end: usize;

    #[cfg(target_arch = "x86_64")]
    {
        start = 0x01000000;
        end   = 0x20000000;
    }

    #[cfg(target_arch = "aarch64")]
    {
        start = 0x40000000;
        end   = 0xC0000000; // Search full 2GB RAM (-m 2G); QEMU places the
                            // initrd high, above the low 1GB window.
    }

    serial_print_str("[INIT-SCAN] Searching from ");
    serial_print_hex(start);
    serial_print_str(" to ");
    serial_print_hex(end);
    serial_print_str("...\n");

    let mut ptr = start;
    while ptr + 6 < end {
        let v_ptr = mm::phys_to_virt(ptr) as *const u8;
        unsafe {
            if core::ptr::read_volatile(v_ptr) == b'0' &&
               core::ptr::read_volatile(v_ptr.add(1)) == b'7' &&
               core::ptr::read_volatile(v_ptr.add(2)) == b'0' &&
               core::ptr::read_volatile(v_ptr.add(3)) == b'7' &&
               core::ptr::read_volatile(v_ptr.add(4)) == b'0' &&
               core::ptr::read_volatile(v_ptr.add(5)) == b'1' {
                
                serial_print_str("[INIT-SCAN] Found CPIO magic at physical ");
                serial_print_hex(ptr);
                serial_print_str("\n");
                
                return Some((ptr, 0x2000000)); // Default to 32MB max
            }
        }
        ptr += 4096;
    }
    serial_print_str("[INIT-SCAN] No initrd signature found.\n");
    None
}
