//! The kernel's view of the USB stack for `uvc.rs`: plain re-exports of the xhci types. The
//! loadable module (modules/uvc) has its own `uvcenv` with the same names over the kapi.

pub use crate::class::Match;
pub use crate::isocdesc::{scan_isoch, IsochEpDesc};
pub use crate::platform::{self, Deadline};
pub use crate::transport::UsbErr;
pub use crate::usb_log;
pub use crate::xhci::isoch::{IsochPipe, Poll, StreamFormat, UsbFunction};

/// The device on (`hc`, `slot`) left.
pub fn class_detach(hc: &crate::xhci::Hc, slot: u8, _ctx: usize) {
    crate::uvc::detach_where(|f| core::ptr::eq(&*f.hc, hc) && f.slot == slot);
}
