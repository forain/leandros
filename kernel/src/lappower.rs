//! Laptop power plumbing (x86_64): ACPI power module init, its 200 ms poll
//! thread, the "ACPI Power Buttons" evdev node, and the hook table behind
//! `/sys/class/power_supply` and `/sys/class/backlight`.
//! Knobs: `acpi_power=0` skips everything, `acpi_power.fake=1` serves a fake
//! battery/AC/backlight (QEMU tests).

use vfs_server::sysclass::Ops;

static OPS: Ops = Ops {
    devices: |class, idx, out| drivers::power_sysfs::device(class, idx, out),
    attrs: |class, dev, idx, out| drivers::power_sysfs::attr_name(class, dev, idx, out),
    read: |class, dev, attr, out| drivers::power_sysfs::read(class, dev, attr, out),
    writable: |class, dev, attr| drivers::power_sysfs::writable(class, dev, attr),
    write: |class, dev, attr, data| drivers::power_sysfs::write(class, dev, attr, data),
};

fn sleep_until(deadline: u64) {
    while sched::monotonic_ns() < deadline {
        sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
        if sched::monotonic_ns() >= deadline { sched::block_on_poll_cancel(); }
        else { sched::block_on_poll_commit(); }
    }
}

/// Input-report counter for the HID-over-I2C probe (lane lapi2c): polls every 10 ms until its window ends.
fn i2c_task() -> ! {
    let mut next = sched::monotonic_ns();
    while drivers::i2c_hid::listen_once(sched::monotonic_ns()) {
        next += 10_000_000;
        sleep_until(next);
    }
    loop { next += 3_600_000_000_000; sleep_until(next); }
}

static S3_DRYRUN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

static S3_WAK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn task() -> ! {
    let mut registered = false;
    let mut tries = 0u32;
    let mut next = sched::monotonic_ns();
    let mut dryrun_at = if S3_DRYRUN.load(core::sync::atomic::Ordering::Relaxed) { Some(next + 8_000_000_000) } else { None };
    loop {
        if let Some(t) = dryrun_at {
            if sched::monotonic_ns() >= t { dryrun_at = None; drivers::acpi_power::s3_dryrun(S3_DRYRUN.load(core::sync::atomic::Ordering::Relaxed) && S3_WAK.load(core::sync::atomic::Ordering::Relaxed)); }
        }
        if !registered && tries < 50 {
            registered = drivers::power_sysfs::register_input();
            tries += 1;
        }
        drivers::acpi_power::poll();
        next += 200_000_000;
        sleep_until(next);
    }
}

/// Kernel init, task context, after the heap, PCI and evdev setup.
pub fn init(rsdp: u64, hhdm: u64, cmdline: &str) {
    let has = |k: &str| cmdline.split_ascii_whitespace().any(|t| t == k);
    if has("acpi_power=0") { return; }
    drivers::power_sysfs::set_fake(has("acpi_power.fake=1"));
    drivers::acpi_power::init(rsdp, hhdm);
    S3_DRYRUN.store(has("pm.s3_dryrun=1") || has("pm.s3_dryrun=2"), core::sync::atomic::Ordering::Relaxed);
    S3_WAK.store(has("pm.s3_dryrun=2"), core::sync::atomic::Ordering::Relaxed);
    vfs_server::sysclass::set_sysclass_ops(&OPS);
    vfs_server::sysclass::set_acpi_tables(|idx, out| {
        let (name, t) = drivers::acpi_power::raw_table(idx)?;
        let n = name.len().min(out.len());
        out[..n].copy_from_slice(&name.as_bytes()[..n]);
        Some((n, t))
    });
    if drivers::i2c_hid::init(cmdline) && sched::spawn(i2c_task, 0).map(|p| sched::set_comm(p, b"ki2chid")).is_none() {
        crate::serial_print_str("[I2C] could not start the HID probe thread\n");
    }
    if sched::spawn(task, 0).map(|p| sched::set_comm(p, b"kacpid")).is_none() {
        crate::serial_print_str("[ACPI] could not start the power poll thread\n");
    }
}
