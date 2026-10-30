//! Core kapi v1: the symbols a module may import. Each is `#[no_mangle] extern "C"`
//! and listed in `.ksymtab` (kept by the linker scripts, bounded by
//! `__ksymtab_start/_end`). Subsystem exports are appended by their lanes.

use core::alloc::Layout;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize, Ordering};
use kmod_abi::{sys, KSym, KmodCtx, KmodId, RegKind, KMOD_ABI_VERSION};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::AtomicU64;
use kmod_abi::{BUS_USB, KCameraOps, KIrqHandler, KNetifOps, KSndOps, KPciDev, KPmFn, KTickFn, KUsbDetachFn, KUsbIf, KUsbProbeFn,
               KmodDevId, KMAP_MMIO, KMAP_WC, KMAP_WRITABLE, KNET_WIFI};

fn log(s: &str) { drivers::pci::serial_debug(s); }

unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 { &[] } else { core::slice::from_raw_parts(p, n) }
}


mod common;
mod core_;
mod pci;
mod irq;
mod net;
mod snd;
mod pm;
mod v4l2;
mod usbfn;
mod hci;
mod blk;
mod input;
mod wifi;
mod drm;

pub use pci::*;
pub use irq::*;
pub use net::*;
pub use snd::*;
pub use pm::*;
pub use blk::*;
pub use input::*;
pub use wifi::*;
pub use drm::*;
pub use v4l2::*;
pub use usbfn::*;
pub use hci::*;
pub use core_::*;
pub use common::*;

// Compile-time signature check against kmod_abi::sys.
const _: sys::AbiVersionFn = kapi_abi_version;
const _: sys::LogFn = kapi_log;
const _: sys::PanicFn = kapi_panic;
const _: sys::AllocFn = kapi_alloc;
const _: sys::DeallocFn = kapi_dealloc;
const _: sys::ReallocFn = kapi_realloc;
const _: sys::CmdlineFn = kapi_cmdline;
const _: sys::MonotonicFn = kapi_monotonic_ns;
const _: sys::SleepFn = kapi_msleep;
const _: sys::SleepFn = kapi_udelay;
const _: sys::SpinHeldFn = kapi_spin_held;
const _: sys::SpinHeldCountFn = kapi_spin_held_count;
const _: sys::KthreadSpawnFn = kapi_kthread_spawn;
const _: sys::KtestPingFn = kapi_ktest_ping;
const _: sys::PciFindFn = kapi_pci_find;
const _: sys::PciCfgReadFn = kapi_pci_cfg_read;
const _: sys::PciCfgWriteFn = kapi_pci_cfg_write;
const _: sys::MapDeviceFn = kapi_map_device;
const _: sys::PhysToVirtFn = kapi_phys_to_virt;
const _: sys::PageAllocFn = kapi_page_alloc;
const _: sys::PageFreeFn = kapi_page_free;
const _: sys::HwrngRegisterFn = kapi_hwrng_register;
const _: sys::PciEnableFn = kapi_pci_enable;
const _: sys::PciFindCapFn = kapi_pci_find_cap;
const _: sys::IrqRequestMsiFn = kapi_irq_request_msi;
const _: sys::IrqRequestLineFn = kapi_irq_request_line;
const _: sys::WaitNewFn = kapi_wait_new;
const _: sys::WaitTimeoutFn = kapi_wait_timeout;
const _: sys::WaitNotifyFn = kapi_wait_notify;
const _: sys::NetdevRegisterFn = kapi_netdev_register;
const _: sys::NetdevKickFn = kapi_netdev_kick;
const _: sys::SndRegisterFn = kapi_snd_register;
const _: sys::NetdevKickFn = kapi_netdev_kick_from_irq;
const _: sys::PmRegisterFn = kapi_pm_register;
const _: sys::PmRegister2Fn = kapi_pm_register2;
const _: sys::VirtToPhysFn = kapi_virt_to_phys;
const _: sys::CanSleepFn = kapi_can_sleep;
const _: sys::SleepUntilFn = kapi_sleep_until;
const _: sys::YieldFn = kapi_yield;
const _: sys::CanSleepFn = kapi_irqs_masked;
const _: kmod_abi::blk::sys_blk::BlkRegisterFn = kapi_blk_register;
const _: kmod_abi::blk::sys_blk::BlkRemoveFn = kapi_blk_remove;
const _: kmod_abi::input::sys_input::InputRegisterFn = kapi_input_register;
const _: kmod_abi::input::sys_input::InputEventFn = kapi_input_event;
const _: kmod_abi::input::sys_input::InputSetCapsFn = kapi_input_set_caps;
const _: kmod_abi::blk::sys_blk::BoolFn = kapi_preempt_disabled;
const _: kmod_abi::blk::sys_blk::VoidFn = kapi_preempt_enable_no_resched;
const _: sys::CurrentPidFn = kapi_current_pid;
const _: sys::IoremapFn = kapi_ioremap;
const _: sys::IounmapFn = kapi_iounmap;
const _: sys::DmaSyncFn = kapi_dma_sync;
const _: sys::CanSleepFn = kapi_dma_coherent;
const _: sys::MutexNewFn = kapi_mutex_new;
const _: sys::WaitNotifyFn = kapi_mutex_lock;
const _: sys::WaitNotifyFn = kapi_mutex_unlock;
const _: sys::MutexTryFn = kapi_mutex_trylock;
const _: sys::SemNewFn = kapi_sem_new;
const _: sys::WaitTimeoutFn = kapi_sem_down_timeout;
const _: sys::WaitNotifyFn = kapi_sem_up;
const _: sys::HrtimerNewFn = kapi_hrtimer_new;
const _: sys::HrtimerArmFn = kapi_hrtimer_arm;
const _: sys::WaitNotifyFn = kapi_hrtimer_cancel;
const _: sys::NetdevKickFn = kapi_preempt_disable;
const _: sys::NetdevKickFn = kapi_preempt_enable;
const _: sys::RandomFillFn = kapi_random_fill;
const _: sys::CopyFromUserFn = kapi_copy_from_user;
const _: sys::CopyToUserFn = kapi_copy_to_user;
const _: sys::PciScanFn = kapi_pci_scan;
const _: sys::IrqRequestMsixFn = kapi_irq_request_msix;
const _: sys::CanSleepFn = kapi_fw_root_ready;
const _: sys::FwRequestFn = kapi_fw_request;
const _: sys::WaitNotifyFn = kapi_fw_release;
const _: sys::SecretPeekFn = kapi_secret_peek;
const _: sys::ExtEventStatsFn = kapi_ext_event_stats;
const _: sys::ProcPrimeFn = kapi_proc_prime;
const _: sys::V4l2RegisterFn = kapi_v4l2_register;
const _: sys::TickRegisterFn = kapi_tick_register;
const _: sys::UsbClassRegisterFn = kapi_usb_class_register;
const _: sys::UsbCtrlFn = kapi_usb_ctrl;
const _: sys::UsbSetAltFn = kapi_usb_set_alt;
const _: sys::UsbCfgFn = kapi_usb_cfg;
const _: sys::UsbIsochOpenFn = kapi_usb_isoch_open;
const _: sys::UsbIsochSubmitFn = kapi_usb_isoch_submit;
const _: sys::UsbIsochReapFn = kapi_usb_isoch_reap;
const _: sys::UsbIsochCloseFn = kapi_usb_isoch_close;
const _: sys::UsbIntrOpenFn = kapi_usb_intr_open;
const _: sys::UsbIntrPollFn = kapi_usb_intr_poll;
const _: sys::UsbIntrOutFn = kapi_usb_intr_out;
const _: sys::UsbIntrCloseFn = kapi_usb_intr_close;
const _: sys::UsbBulkOpenFn = kapi_usb_bulk_open;
const _: sys::UsbBulkFn = kapi_usb_bulk;
const _: sys::UsbBulkCloseFn = kapi_usb_bulk_close;
const _: sys::HciRegisterFn = kapi_hci_register;
const _: sys::HciRxFn = kapi_hci_rx;
const _: sys::HciTxPopFn = kapi_hci_tx_pop;
const _: sys::HciCtlFn = kapi_hci_ctl;
const _: sys::HciUnregisterFn = kapi_hci_unregister;
const _: sys::UsbBtOpenFn = kapi_usb_bt_open;
const _: sys::UsbBtPollFn = kapi_usb_bt_poll;
const _: sys::UsbBtSendFn = kapi_usb_bt_send;
const _: sys::UsbBtCloseFn = kapi_usb_bt_close;
const _: kmod_abi::input::sys_input::InputUnregisterFn = kapi_input_unregister;
const _: kmod_abi::input::sys_input::InputEventConsoleFn = kapi_input_event_console;
const _: kmod_abi::input::sys_input::InputLedsFn = kapi_input_leds;
const _: kmod_abi::input::sys_input::InputRepeatFn = kapi_input_repeat;
const _: kmod_abi::input::sys_input::RelPointerReportFn = kapi_rel_pointer_report;
const _: kmod_abi::input::sys_input::RelPointerAdvertiseFn = kapi_rel_pointer_advertise;
const _: sys::V4l2UnregisterFn = kapi_v4l2_unregister;
const _: () = {
    assert!(mm::kmodwin::BASE as u64 == kmod_abi::layout::BASE);
    assert!(mm::kmodwin::SIZE as u64 == kmod_abi::layout::SIZE);
};

macro_rules! sym { ($n:literal, $f:ident) => { KSym { name: concat!($n, "\0").as_ptr(), addr: $f as *const () } }; }

pub const NEXPORTS: usize = 118;

#[used]
#[link_section = ".ksymtab"]
static KSYMTAB: [KSym; NEXPORTS] = [
    sym!("kapi_abi_version", kapi_abi_version),
    sym!("kapi_log", kapi_log),
    sym!("kapi_panic", kapi_panic),
    sym!("kapi_alloc", kapi_alloc),
    sym!("kapi_dealloc", kapi_dealloc),
    sym!("kapi_realloc", kapi_realloc),
    sym!("kapi_cmdline", kapi_cmdline),
    sym!("kapi_monotonic_ns", kapi_monotonic_ns),
    sym!("kapi_msleep", kapi_msleep),
    sym!("kapi_udelay", kapi_udelay),
    sym!("kapi_spin_held", kapi_spin_held),
    sym!("kapi_spin_held_count", kapi_spin_held_count),
    sym!("kapi_kthread_spawn", kapi_kthread_spawn),
    sym!("kapi_ktest_ping", kapi_ktest_ping),
    sym!("kapi_pci_find", kapi_pci_find),
    sym!("kapi_pci_cfg_read", kapi_pci_cfg_read),
    sym!("kapi_pci_cfg_write", kapi_pci_cfg_write),
    sym!("kapi_map_device", kapi_map_device),
    sym!("kapi_phys_to_virt", kapi_phys_to_virt),
    sym!("kapi_page_alloc", kapi_page_alloc),
    sym!("kapi_page_free", kapi_page_free),
    sym!("kapi_hwrng_register", kapi_hwrng_register),
    sym!("kapi_pci_enable", kapi_pci_enable),
    sym!("kapi_pci_find_cap", kapi_pci_find_cap),
    sym!("kapi_irq_request_msi", kapi_irq_request_msi),
    sym!("kapi_irq_request_line", kapi_irq_request_line),
    sym!("kapi_wait_new", kapi_wait_new),
    sym!("kapi_wait_timeout", kapi_wait_timeout),
    sym!("kapi_wait_notify", kapi_wait_notify),
    sym!("kapi_netdev_register", kapi_netdev_register),
    sym!("kapi_wifi_register", kapi_wifi_register),
    sym!("kapi_drm_register", kapi_drm_register),
    sym!("kapi_drm_offset_alloc", kapi_drm_offset_alloc),
    sym!("kapi_drm_offset_release", kapi_drm_offset_release),
    sym!("kapi_fence_wait", kapi_fence_wait),
    sym!("kapi_syncobj_install", kapi_syncobj_install),
    sym!("kapi_syncobj_exists", kapi_syncobj_exists),
    sym!("kapi_syncobj_in_fences", kapi_syncobj_in_fences),
    sym!("kapi_fence_irq_hint", kapi_fence_irq_hint),
    sym!("kapi_kms_event_complete", kapi_kms_event_complete),
    sym!("kapi_netdev_kick", kapi_netdev_kick),
    sym!("kapi_snd_register", kapi_snd_register),
    sym!("kapi_netdev_kick_from_irq", kapi_netdev_kick_from_irq),
    sym!("kapi_pm_register", kapi_pm_register),
    sym!("kapi_pm_register2", kapi_pm_register2),
    sym!("kapi_virt_to_phys", kapi_virt_to_phys),
    sym!("kapi_can_sleep", kapi_can_sleep),
    sym!("kapi_sleep_until", kapi_sleep_until),
    sym!("kapi_yield", kapi_yield),
    sym!("kapi_irqs_masked", kapi_irqs_masked),
    sym!("kapi_current_pid", kapi_current_pid),
    sym!("kapi_ioremap", kapi_ioremap),
    sym!("kapi_iounmap", kapi_iounmap),
    sym!("kapi_dma_sync", kapi_dma_sync),
    sym!("kapi_dma_coherent", kapi_dma_coherent),
    sym!("kapi_mutex_new", kapi_mutex_new),
    sym!("kapi_mutex_lock", kapi_mutex_lock),
    sym!("kapi_mutex_unlock", kapi_mutex_unlock),
    sym!("kapi_mutex_trylock", kapi_mutex_trylock),
    sym!("kapi_sem_new", kapi_sem_new),
    sym!("kapi_sem_down_timeout", kapi_sem_down_timeout),
    sym!("kapi_sem_up", kapi_sem_up),
    sym!("kapi_hrtimer_new", kapi_hrtimer_new),
    sym!("kapi_hrtimer_arm", kapi_hrtimer_arm),
    sym!("kapi_hrtimer_cancel", kapi_hrtimer_cancel),
    sym!("kapi_preempt_disable", kapi_preempt_disable),
    sym!("kapi_preempt_enable", kapi_preempt_enable),
    sym!("kapi_random_fill", kapi_random_fill),
    sym!("kapi_copy_from_user", kapi_copy_from_user),
    sym!("kapi_copy_to_user", kapi_copy_to_user),
    sym!("kapi_pci_scan", kapi_pci_scan),
    sym!("kapi_irq_request_msix", kapi_irq_request_msix),
    sym!("kapi_fw_root_ready", kapi_fw_root_ready),
    sym!("kapi_fw_request", kapi_fw_request),
    sym!("kapi_fw_release", kapi_fw_release),
    sym!("kapi_secret_peek", kapi_secret_peek),
    sym!("kapi_ext_event_stats", kapi_ext_event_stats),
    sym!("kapi_proc_prime", kapi_proc_prime),
    sym!("kapi_v4l2_register", kapi_v4l2_register),
    sym!("kapi_tick_register", kapi_tick_register),
    sym!("kapi_usb_class_register", kapi_usb_class_register),
    sym!("kapi_usb_ctrl", kapi_usb_ctrl),
    sym!("kapi_usb_set_alt", kapi_usb_set_alt),
    sym!("kapi_usb_cfg", kapi_usb_cfg),
    sym!("kapi_usb_isoch_open", kapi_usb_isoch_open),
    sym!("kapi_usb_isoch_submit", kapi_usb_isoch_submit),
    sym!("kapi_usb_isoch_reap", kapi_usb_isoch_reap),
    sym!("kapi_usb_isoch_close", kapi_usb_isoch_close),
    sym!("kapi_v4l2_unregister", kapi_v4l2_unregister),
    sym!("kapi_blk_register", kapi_blk_register),
    sym!("kapi_blk_remove", kapi_blk_remove),
    sym!("kapi_input_register", kapi_input_register),
    sym!("kapi_input_event", kapi_input_event),
    sym!("kapi_input_set_caps", kapi_input_set_caps),
    sym!("kapi_usb_intr_open", kapi_usb_intr_open),
    sym!("kapi_usb_intr_poll", kapi_usb_intr_poll),
    sym!("kapi_usb_intr_out", kapi_usb_intr_out),
    sym!("kapi_usb_intr_close", kapi_usb_intr_close),
    sym!("kapi_usb_bulk_open", kapi_usb_bulk_open),
    sym!("kapi_usb_bulk", kapi_usb_bulk),
    sym!("kapi_usb_bulk_close", kapi_usb_bulk_close),
    sym!("kapi_hci_register", kapi_hci_register),
    sym!("kapi_hci_rx", kapi_hci_rx),
    sym!("kapi_hci_tx_pop", kapi_hci_tx_pop),
    sym!("kapi_hci_ctl", kapi_hci_ctl),
    sym!("kapi_hci_unregister", kapi_hci_unregister),
    sym!("kapi_usb_bt_open", kapi_usb_bt_open),
    sym!("kapi_usb_bt_poll", kapi_usb_bt_poll),
    sym!("kapi_usb_bt_send", kapi_usb_bt_send),
    sym!("kapi_usb_bt_close", kapi_usb_bt_close),
    sym!("kapi_input_unregister", kapi_input_unregister),
    sym!("kapi_input_event_console", kapi_input_event_console),
    sym!("kapi_input_leds", kapi_input_leds),
    sym!("kapi_input_repeat", kapi_input_repeat),
    sym!("kapi_rel_pointer_report", kapi_rel_pointer_report),
    sym!("kapi_rel_pointer_advertise", kapi_rel_pointer_advertise),
    sym!("kapi_preempt_disabled", kapi_preempt_disabled),
    sym!("kapi_preempt_enable_no_resched", kapi_preempt_enable_no_resched),
];

extern "C" {
    static __ksymtab_start: KSym;
    static __ksymtab_end: KSym;
}

/// The linked export table, as the linker script delimits it.
pub fn ksymtab() -> &'static [KSym] {
    unsafe {
        let s = core::ptr::addr_of!(__ksymtab_start);
        let e = core::ptr::addr_of!(__ksymtab_end);
        core::slice::from_raw_parts(s, (e as usize - s as usize) / core::mem::size_of::<KSym>())
    }
}

/// Address of an export, found by walking the table.
pub fn lookup(name: &str) -> Option<usize> {
    for k in ksymtab() {
        let mut n = 0;
        while unsafe { *k.name.add(n) } != 0 { n += 1; }
        if unsafe { core::slice::from_raw_parts(k.name, n) } == name.as_bytes() { return Some(k.addr as usize); }
    }
    None
}
