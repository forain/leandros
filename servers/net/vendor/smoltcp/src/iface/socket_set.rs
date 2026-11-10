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
}

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
        SocketSet { sockets, set_gen }
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

    /// Get a socket from the set by its handle, as mutable.
    ///
    /// # Panics
    /// This function may panic if the handle does not belong to this socket set
    /// or the socket has the wrong type.
    pub fn get<T: AnySocket<'a>>(&self, handle: SocketHandle) -> &T {
        match self.sockets.get(self.live_index(handle).unwrap_or(usize::MAX)).and_then(|s| s.inner.as_ref()) {
            Some(item) => {
                T::downcast(&item.socket).expect("handle refers to a socket of a wrong type")
            }
            None => panic!("handle does not refer to a valid socket"),
        }
    }

    /// Get a mutable socket from the set by its handle, as mutable.
    ///
    /// # Panics
    /// This function may panic if the handle does not belong to this socket set
    /// or the socket has the wrong type.
    pub fn get_mut<T: AnySocket<'a>>(&mut self, handle: SocketHandle) -> &mut T {
        match self.live_index(handle).and_then(|i| self.sockets[i].inner.as_mut()) {
            Some(item) => T::downcast_mut(&mut item.socket)
                .expect("handle refers to a socket of a wrong type"),
            None => panic!("handle does not refer to a valid socket"),
        }
    }

    /// Remove a socket from the set, without changing its state.
    ///
    /// # Panics
    /// This function may panic if the handle does not belong to this socket set.
    pub fn remove(&mut self, handle: SocketHandle) -> Socket<'a> {
        net_trace!("[{}]: removing", handle.0);
        match self.live_index(handle).and_then(|i| self.sockets[i].inner.take()) {
            Some(item) => item.socket,
            None => panic!("handle does not refer to a valid socket"),
        }
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
