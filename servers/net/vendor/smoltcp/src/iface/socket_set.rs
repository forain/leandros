use core::fmt;
use managed::ManagedSlice;

use super::socket_meta::Meta;
use crate::socket::{AnySocket, Socket};

/// Opaque struct with space for storing one socket.
///
/// This is public so you can use it to allocate space for storing
/// sockets when creating an Interface.
#[derive(Debug, Default)]
pub struct SocketStorage<'a> {
    inner: Option<Item<'a>>,
    /// Bumped on every add into this slot, so a handle to a removed socket
    /// never names whatever reuses the slot.
    gen: u16,
}

impl<'a> SocketStorage<'a> {
    pub const EMPTY: Self = Self { inner: None, gen: 0 };
}

/// An item of a socket set.
#[derive(Debug)]
pub(crate) struct Item<'a> {
    pub(crate) meta: Meta,
    pub(crate) socket: Socket<'a>,
}

/// A handle, identifying a socket in an Interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SocketHandle(usize);

impl fmt::Display for SocketHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "#{}", self.0 & IDX_MASK)
    }
}

/// An extensible set of sockets.
///
/// The lifetime `'a` is used when storing a `Socket<'a>`.  If you're using
/// owned buffers for your sockets (passed in as `Vec`s) you can use
/// `SocketSet<'static>`.
#[derive(Debug)]
pub struct SocketSet<'a> {
    sockets: ManagedSlice<'a, SocketStorage<'a>>,
    /// Unique per set (process-wide counter): a handle minted by a freed
    /// set never validates against a new set that reuses its owner's id.
    set_gen: u16,
    /// Inert stand-ins handed out for a stale handle (see `get_mut`).
    tomb: alloc::vec::Vec<Socket<'a>>,
}

/// How many times a stale handle was resolved to a stand-in (diagnostics).
pub static STALE_HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

static SET_GEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

// Handle layout: set_gen << 48 | slot_gen << 32 | index.
const IDX_MASK: usize = 0xFFFF_FFFF;


impl<'a> SocketSet<'a> {
    /// Create a socket set using the provided storage.
    pub fn new<SocketsT>(sockets: SocketsT) -> SocketSet<'a>
    where
        SocketsT: Into<ManagedSlice<'a, SocketStorage<'a>>>,
    {
        let sockets = sockets.into();
        let set_gen = SET_GEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) as u16;
        SocketSet { sockets, set_gen, tomb: Self::make_tombs() }
    }

    /// Slot index of `handle` if it names a live socket of this set.
    fn live_index(&self, handle: SocketHandle) -> Option<usize> {
        let idx = handle.0 & IDX_MASK;
        let slot = self.sockets.get(idx)?;
        let ok = slot.inner.is_some()
            && (handle.0 >> 48) as u16 == self.set_gen
            && ((handle.0 >> 32) & 0xFFFF) as u16 == slot.gen;
        ok.then_some(idx)
    }

    /// True if `handle` names a live socket of this set.
    pub fn contains(&self, handle: SocketHandle) -> bool {
        self.live_index(handle).is_some()
    }

    /// Like `get`, but None for a stale handle or a wrong type.
    pub fn try_get<T: AnySocket<'a>>(&self, handle: SocketHandle) -> Option<&T> {
        let i = self.live_index(handle)?;
        T::downcast(&self.sockets[i].inner.as_ref()?.socket)
    }

    /// Like `get_mut`, but None for a stale handle or a wrong type.
    pub fn try_get_mut<T: AnySocket<'a>>(&mut self, handle: SocketHandle) -> Option<&mut T> {
        let i = self.live_index(handle)?;
        T::downcast_mut(&mut self.sockets[i].inner.as_mut()?.socket)
    }

    /// Like `remove`, but None for a stale handle.
    pub fn try_remove(&mut self, handle: SocketHandle) -> Option<Socket<'a>> {
        let i = self.live_index(handle)?;
        self.sockets[i].inner.take().map(|it| it.socket)
    }

    /// Add a socket to the set, and return its handle.
    ///
    /// # Panics
    /// This function panics if the storage is fixed-size (not a `Vec`) and is full.
    pub fn add<T: AnySocket<'a>>(&mut self, socket: T) -> SocketHandle {
        fn put<'a>(set_gen: u16, index: usize, slot: &mut SocketStorage<'a>, socket: Socket<'a>) -> SocketHandle {
            net_trace!("[{}]: adding", index);
            slot.gen = slot.gen.wrapping_add(1);
            let handle = SocketHandle(((set_gen as usize) << 48) | ((slot.gen as usize) << 32) | index);
            let mut meta = Meta::default();
            meta.handle = handle;
            slot.inner = Some(Item { meta, socket });
            handle
        }
        let set_gen = self.set_gen;

        let socket = socket.upcast();

        for (index, slot) in self.sockets.iter_mut().enumerate() {
            if slot.inner.is_none() {
                return put(set_gen, index, slot, socket);
            }
        }

        match &mut self.sockets {
            ManagedSlice::Borrowed(_) => panic!("adding a socket to a full SocketSet"),
            #[cfg(feature = "alloc")]
            ManagedSlice::Owned(sockets) => {
                sockets.push(SocketStorage { inner: None, gen: 0 });
                let index = sockets.len() - 1;
                put(set_gen, index, &mut sockets[index], socket)
            }
        }
    }

    /// Fresh inert sockets, one per variant: closed, unbound, zero-capacity
    /// buffers. Handed out for a stale handle so a racing teardown (stack
    /// reaped between a caller's check and use) yields the errors of a dead
    /// link instead of a panic.
    fn make_tombs() -> alloc::vec::Vec<Socket<'a>> {
        use crate::storage::{PacketBuffer, RingBuffer};
        use alloc::vec::Vec;
        let mut v: Vec<Socket<'a>> = Vec::new();
        #[cfg(feature = "socket-tcp")]
        v.push(Socket::Tcp(crate::socket::tcp::Socket::new(RingBuffer::new(Vec::new()), RingBuffer::new(Vec::new()))));
        #[cfg(feature = "socket-udp")]
        v.push(Socket::Udp(crate::socket::udp::Socket::new(PacketBuffer::new(Vec::new(), Vec::new()), PacketBuffer::new(Vec::new(), Vec::new()))));
        #[cfg(feature = "socket-icmp")]
        v.push(Socket::Icmp(crate::socket::icmp::Socket::new(PacketBuffer::new(Vec::new(), Vec::new()), PacketBuffer::new(Vec::new(), Vec::new()))));
        #[cfg(feature = "socket-raw")]
        v.push(Socket::Raw(crate::socket::raw::Socket::new(crate::wire::IpVersion::Ipv4, crate::wire::IpProtocol::Unknown(0), PacketBuffer::new(Vec::new(), Vec::new()), PacketBuffer::new(Vec::new(), Vec::new()))));
        #[cfg(feature = "socket-dhcpv4")]
        v.push(Socket::Dhcpv4(crate::socket::dhcpv4::Socket::new()));
        v
    }

    fn tombstone<T: AnySocket<'a>>(&mut self) -> &mut T {
        core::sync::atomic::AtomicU32::fetch_add(&STALE_HITS, 1, core::sync::atomic::Ordering::Relaxed);
        let fresh = Self::make_tombs();
        // Reset every stand-in so state written through a previous stale
        // access never leaks into the next one.
        self.tomb = fresh;
        let i = self.tomb.iter().position(|t| T::downcast(t).is_some()).expect("tombstone type");
        T::downcast_mut(&mut self.tomb[i]).expect("tombstone type")
    }

    /// Get a socket from the set by its handle.
    ///
    /// A stale handle (socket removed, slot reused, set gone) never panics:
    /// it resolves to an inert stand-in, so every operation fails like one on
    /// a dead link. Panics only on a live socket of the wrong type.
    pub fn get<T: AnySocket<'a>>(&self, handle: SocketHandle) -> &T {
        match self.sockets.get(self.live_index(handle).unwrap_or(usize::MAX)).and_then(|s| s.inner.as_ref()) {
            Some(item) => {
                T::downcast(&item.socket).expect("handle refers to a socket of a wrong type")
            }
            None => {
                core::sync::atomic::AtomicU32::fetch_add(&STALE_HITS, 1, core::sync::atomic::Ordering::Relaxed);
                let t = self.tomb.iter().find_map(|t| T::downcast(t));
                t.expect("tombstone type")
            }
        }
    }

    /// Get a mutable socket from the set by its handle; a stale handle
    /// resolves to an inert stand-in (see `get`).
    pub fn get_mut<T: AnySocket<'a>>(&mut self, handle: SocketHandle) -> &mut T {
        match self.live_index(handle) {
            Some(i) => T::downcast_mut(&mut self.sockets[i].inner.as_mut().expect("live").socket)
                .expect("handle refers to a socket of a wrong type"),
            None => self.tombstone::<T>(),
        }
    }

    /// Remove a socket from the set, without changing its state. A stale
    /// handle is a no-op (returns None).
    pub fn remove(&mut self, handle: SocketHandle) -> Option<Socket<'a>> {
        net_trace!("[{}]: removing", handle.0);
        self.try_remove(handle)
    }

    /// Get an iterator to the inner sockets.
    pub fn iter(&self) -> impl Iterator<Item = (SocketHandle, &Socket<'a>)> {
        self.items().map(|i| (i.meta.handle, &i.socket))
    }

    /// Get a mutable iterator to the inner sockets.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (SocketHandle, &mut Socket<'a>)> {
        self.items_mut().map(|i| (i.meta.handle, &mut i.socket))
    }

    /// Iterate every socket in this set.
    pub(crate) fn items(&self) -> impl Iterator<Item = &Item<'a>> + '_ {
        self.sockets.iter().filter_map(|x| x.inner.as_ref())
    }

    /// Iterate every socket in this set.
    pub(crate) fn items_mut(&mut self) -> impl Iterator<Item = &mut Item<'a>> + '_ {
        self.sockets.iter_mut().filter_map(|x| x.inner.as_mut())
    }
}
