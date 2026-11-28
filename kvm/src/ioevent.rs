//! KVM_IOEVENTFD matching (pure). `len == 0` registers an any-length match
//! (KVM_CAP_IOEVENTFD_ANY_LENGTH); a datamatch applies to the stored value.
use crate::abi::*;

pub const EINVAL: i32 = 22;
pub const EEXIST: i32 = 17;
pub const ENOENT: i32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoEvent {
    pub addr: u64,
    pub len: u32,
    pub datamatch: Option<u64>,
    /// Opaque handle of the signalled object (the kernel stores the eventfd slot).
    pub target: usize,
}

impl IoEvent {
    pub fn from_abi(a: &KvmIoeventfd, target: usize) -> Result<IoEvent, i32> {
        if a.flags & KVM_IOEVENTFD_FLAG_PIO != 0 { return Err(EINVAL); }
        if a.flags & !(KVM_IOEVENTFD_FLAG_DATAMATCH | KVM_IOEVENTFD_FLAG_DEASSIGN) != 0 { return Err(EINVAL); }
        if !matches!(a.len, 0 | 1 | 2 | 4 | 8) { return Err(EINVAL); }
        if a.addr.checked_add(a.len as u64).is_none() { return Err(EINVAL); }
        let datamatch = if a.flags & KVM_IOEVENTFD_FLAG_DATAMATCH != 0 { Some(a.datamatch) } else { None };
        Ok(IoEvent { addr: a.addr, len: a.len, datamatch, target })
    }

    /// Does an MMIO write of `len` bytes (`val` zero-extended) at `addr` hit this registration?
    pub fn matches(&self, addr: u64, len: u32, val: u64) -> bool {
        if self.addr != addr { return false; }
        if self.len != 0 && self.len != len { return false; }
        match self.datamatch { Some(d) => d == val, None => true }
    }
}
