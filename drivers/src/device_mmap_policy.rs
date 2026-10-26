//! What `sys_mmap` does with a device token once the device has answered the
//! mmap ioctl (L1.14 / L11.9 class, openbugs #94/#95). No kernel dependency,
//! so drivers/uapi-tests includes this file verbatim and checks it on the
//! host.
//!
//! A device mmap takes two steps: the mmap ioctl (ownership, sees the open
//! but not the length) and the page pin (sees the length, holds the frames).
//! A DRM buffer freed by GEM_CLOSE on another thread between the two made the
//! pin answer "not a guest-RAM buffer", and the token -- a physical address
//! of frames now back in the allocator -- used to be mapped with the plain,
//! unreferenced device mapping, of any length: a use-after-free mapping of
//! whatever the allocator handed those frames to next.
//!
//! The rule is device-agnostic (openbugs #95): whatever server answered the
//! ioctl, a token that neither pinned nor names a host-visible window span
//! may only be mapped if it is not RAM (MMIO, BAR apertures, firmware
//! framebuffer). An unreferenced mapping of allocator RAM outlives whatever
//! buffer the server meant (device close, stream stop, realloc), so a server
//! that wants to hand out RAM must register a page pin that references every
//! frame for the mapping's lifetime.

/// The guest-RAM page pin (`mmap_pin_frames`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// None: the token names no guest-RAM buffer (now).
    NotGuestRam,
    /// Some(empty): a guest-RAM buffer, but the request is refused.
    Refused,
    /// Some(frames): every frame is pinned for the VMA.
    Pinned,
}

/// The host-visible window pin (`mmap_pin_window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    NotWindow,
    Refused,
    Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// EINVAL.
    Refuse,
    /// `map_shared_frames` over the pinned frames.
    MapPinned,
    /// `map_device` of the physical range (no page references).
    MapDevice,
}

/// `ram`: some byte of the range is allocator-managed RAM
/// (`mm::buddy::is_ram`).
pub fn decide(pin: Pin, win: Window, ram: bool) -> Decision {
    match (pin, win) {
        (Pin::Refused, _) | (_, Window::Refused) => Decision::Refuse,
        (Pin::Pinned, _) => Decision::MapPinned,
        (Pin::NotGuestRam, Window::Held) => Decision::MapDevice,
        // Never an unreferenced mapping of RAM through any device: the
        // buffer the ioctl vouched for is gone (or never was guest RAM with
        // a pin), and its frames may already belong to someone else.
        (Pin::NotGuestRam, Window::NotWindow) if ram => Decision::Refuse,
        (Pin::NotGuestRam, Window::NotWindow) => Decision::MapDevice,
    }
}
