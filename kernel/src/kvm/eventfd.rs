//! KVM_IOEVENTFD: MMIO writes that signal an eventfd instead of exiting to QEMU.
use super::*;
use ::kvm::ioevent::IoEvent;

impl Vm {
    /// KVM_IOEVENTFD on the VM fd (`pid` = caller; the fd is resolved in its table).
    pub(super) fn ioeventfd(&self, pid: u32, a: &KvmIoeventfd) -> isize {
        let dereg = a.flags & KVM_IOEVENTFD_FLAG_DEASSIGN != 0;
        if a.fd < 0 { return -9; }
        let slot = match vfs::vfs_get_node_kind(pid, a.fd as usize) {
            Some(vfs::VnodeKind::EventFd { slot }) => slot,
            Some(_) => return -22,
            None => return -9,
        };
        let ev = match IoEvent::from_abi(a, slot) { Ok(e) => e, Err(e) => return -(e as isize) };
        let mut list = self.ioevents.lock();
        if dereg {
            match list.iter().position(|x| *x == ev) {
                Some(i) => { list.remove(i); drop(list); vfs::eventfd_slot_unref(slot); 0 }
                None => -2,
            }
        } else {
            if list.iter().any(|x| x.addr == ev.addr && x.target == slot && x.datamatch == ev.datamatch && (x.len == ev.len)) { return -17; }
            vfs::eventfd_slot_ref(slot);
            list.push(ev);
            0
        }
    }

    /// For kvm-run: an MMIO write of `len` bytes (`val` zero-extended) at `ipa`.
    /// True when a registered eventfd took it (no exit to userspace).
    pub fn try_ioeventfd(&self, ipa: u64, len: u32, val: u64) -> bool {
        let mut hit = false;
        for e in self.ioevents.lock().iter().filter(|e| e.matches(ipa, len, val)) {
            vfs::eventfd_slot_signal(e.target);
            hit = true;
        }
        hit
    }

    pub(super) fn drop_ioevents(&self) {
        let list = core::mem::take(&mut *self.ioevents.lock());
        for e in list { vfs::eventfd_slot_unref(e.target); }
    }
}
