//! `/dev/hci0` on the Pi 5 (docs/servers/bluetooth.md section A): the raw HCI
//! node btd opens. A kernel-handler port answers the VFS from the shared
//! `HciQueues`; the bt thread becomes the pump once the vendor init is done.
//! The node is registered (by the kernel, through `on_ready`) only after that.
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use alloc::format;
use alloc::vec;
use ipc::Message;
use spin::Mutex;

use super::bringup::Transport;
use super::h4::H4Parser;
use super::hcidev_core::{self as core_, pump_once, HciQueues, QueueLock};

// VFS request tags (servers/vfs: drivers does not depend on vfs_server).
const VFS_OPEN: u64 = 0x10;
const VFS_READ: u64 = 0x11;
const VFS_WRITE: u64 = 0x12;
const VFS_CLOSE: u64 = 0x13;
const VFS_POLL: u64 = 0x2F;

const EFAULT: i32 = -14;
const EBUSY: i32 = -16;
const ENOSYS: i32 = -38;

static QUEUES: Mutex<HciQueues> = Mutex::new(HciQueues::new());
/// open_id of the one open (exclusive), 0 = closed.
static OWNER: AtomicU32 = AtomicU32::new(0);
/// Bumped on every readiness change (the poll reply's sequence word).
static SEQ: AtomicU64 = AtomicU64::new(0);

struct KLock;
impl QueueLock for KLock {
    fn with<R>(&self, f: impl FnOnce(&mut HciQueues) -> R) -> R {
        let fl = sched::ksync::irq_save();
        let r = f(&mut QUEUES.lock());
        sched::ksync::irq_restore(fl);
        r
    }
}

/// The poll key of `/dev/hci0`: what the pump wakes and the POLL reply names.
pub fn poll_key() -> u64 { sched::poll_key(sched::poll_class::BT, 0) }

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off + 8].try_into().unwrap_or([0u8; 8]))
}

fn reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

fn handle(msg: &Message, _caller: u32, _target: u32) -> Message {
    match msg.tag {
        VFS_OPEN => {
            let id = match arg(msg, 4) as u32 { 0 => u32::MAX, v => v };
            if OWNER.compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return reply(EBUSY as i64);
            }
            KLock.with(|q| q.open());
            reply(0)
        }
        VFS_READ => {
            let (ptr, count) = (arg(msg, 1) as usize, arg(msg, 2) as usize);
            let pkt = match KLock.with(|q| q.read(count)) { Ok(p) => p, Err(e) => return reply(e as i64) };
            let ok = sched::with_current_address_space_mut(|a| unsafe { a.write_user_buf(ptr, &pkt) }).unwrap_or(false);
            if !ok { return reply(EFAULT as i64); }
            reply(pkt.len() as i64)
        }
        VFS_WRITE => {
            let (ptr, count) = (arg(msg, 1) as usize, arg(msg, 2) as usize);
            if count == 0 || count > core_::MAX_PKT { return reply(core_::EINVAL as i64); }
            let mut buf = vec![0u8; count];
            if unsafe { sched::uaccess::copy_from_user(buf.as_mut_ptr(), ptr, count) } != 0 {
                return reply(EFAULT as i64);
            }
            match KLock.with(|q| q.write(&buf)) { Ok(n) => reply(n as i64), Err(e) => reply(e as i64) }
        }
        VFS_POLL => {
            let revents = KLock.with(|q| q.poll_mask());
            let key = poll_key();
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&revents.to_le_bytes());
            m.data[8..16].copy_from_slice(&SEQ.load(Ordering::Acquire).to_le_bytes());
            m.data[24..32].copy_from_slice(&sched::poll_key_bit(key).to_le_bytes());
            m.data[32] = 1;
            sched::poll_reply_put_keys(&mut m.data, &[key]);
            m
        }
        VFS_CLOSE => {
            let id = match arg(msg, 1) as u32 { 0 => u32::MAX, v => v };
            if OWNER.load(Ordering::Acquire) == id {
                KLock.with(|q| q.close());
                OWNER.store(0, Ordering::Release);
            }
            reply(0)
        }
        _ => reply(ENOSYS as i64),
    }
}

/// Creates the kernel-handler port for `/dev/hci0` (before it is registered).
pub fn create_port() -> Option<u32> {
    let port = ipc::port::create(0)?;
    if !ipc::port::register_kernel_handler(port, handle) { return None; }
    Some(port)
}

/// The bt thread after a successful bring-up: moves packets between the
/// queues and the controller forever. A failed send marks the node dead
/// (POLLHUP|POLLERR, EIO) and the thread idles.
pub fn pump<T: Transport>(t: &mut T) -> ! {
    let mut parser = H4Parser::new();
    let (mut drops, mut last_log_us, mut logged_drops) = (0u64, 0u64, 0u64);
    loop {
        let out = pump_once(&KLock, t, &mut parser, 1000, 4096);
        if out.changed() {
            SEQ.fetch_add(1, Ordering::AcqRel);
            sched::wake_poll_key(poll_key());
        }
        drops += out.dropped as u64;
        let now = sched::monotonic_ns() / 1_000;
        if drops != logged_drops && now - last_log_us >= 1_000_000 {
            crate::bt::boot::log(&format!("rx drop n={}", drops));
            logged_drops = drops;
            last_log_us = now;
        }
        if out.died {
            crate::bt::boot::log("controller stopped taking bytes: /dev/hci0 dead");
            loop { sched::kthread::msleep(1000); }
        }
    }
}
