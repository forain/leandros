//! KVM_IRQFD bookkeeping (pure). An irqfd binds an eventfd (by slot) to a GSI. On arm64 the
//! default routing maps GSI n to SPI n (INTID n + 32). With `KVM_IRQFD_FLAG_RESAMPLE` the guest
//! EOI of the (level) interrupt de-asserts the line and signals `resample`.
use crate::abi::*;
use alloc::vec::Vec;

pub const EINVAL: i32 = 22;
pub const EEXIST: i32 = 17;
pub const ENOENT: i32 = 2;
pub const EBUSY: i32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Irqfd {
    /// Eventfd slot that triggers the interrupt.
    pub slot: usize,
    pub gsi: u32,
    /// Eventfd slot signalled after the guest EOI (level interrupts), if any.
    pub resample: Option<usize>,
}

#[derive(Default)]
pub struct IrqfdTable { list: Vec<Irqfd> }

pub fn deassign(a: &KvmIrqfd) -> bool { a.flags & KVM_IRQFD_FLAG_DEASSIGN != 0 }
pub fn wants_resample(a: &KvmIrqfd) -> bool { a.flags & KVM_IRQFD_FLAG_RESAMPLE != 0 }

/// Validate the flags of a KVM_IRQFD argument.
pub fn check(a: &KvmIrqfd) -> Result<(), i32> {
    if a.flags & !(KVM_IRQFD_FLAG_DEASSIGN | KVM_IRQFD_FLAG_RESAMPLE) != 0 { return Err(EINVAL); }
    Ok(())
}

/// The SPI INTID a GSI maps to under the default routing.
pub fn gsi_to_intid(gsi: u32) -> u32 { gsi + 32 }

impl IrqfdTable {
    pub fn new() -> Self { IrqfdTable { list: Vec::new() } }
    pub fn len(&self) -> usize { self.list.len() }
    pub fn is_empty(&self) -> bool { self.list.is_empty() }

    /// Register; one eventfd can feed only one GSI (Linux: EBUSY on a duplicate fd+gsi).
    pub fn assign(&mut self, e: Irqfd) -> Result<(), i32> {
        if self.list.iter().any(|x| x.slot == e.slot && x.gsi == e.gsi) { return Err(EBUSY); }
        self.list.push(e);
        Ok(())
    }

    /// Remove the registration of (`slot`, `gsi`); returns it so the caller can drop its references.
    pub fn deassign(&mut self, slot: usize, gsi: u32) -> Option<Irqfd> {
        let i = self.list.iter().position(|x| x.slot == slot && x.gsi == gsi)?;
        Some(self.list.remove(i))
    }

    /// GSIs raised by a write to eventfd `slot`.
    pub fn gsis_for(&self, slot: usize) -> Vec<u32> {
        self.list.iter().filter(|x| x.slot == slot).map(|x| x.gsi).collect()
    }

    /// Does any registration trigger on `slot`?
    pub fn has_slot(&self, slot: usize) -> bool { self.list.iter().any(|x| x.slot == slot) }

    /// Resample registrations (gsi, resample slot) for the guest EOI of INTID `intid`.
    pub fn resample_for(&self, intid: u32) -> Vec<(u32, usize)> {
        self.list.iter()
            .filter(|x| gsi_to_intid(x.gsi) == intid)
            .filter_map(|x| x.resample.map(|r| (x.gsi, r)))
            .collect()
    }

    /// Take every registration (VM teardown).
    pub fn take_all(&mut self) -> Vec<Irqfd> { core::mem::take(&mut self.list) }
}
