//! What `sys_mmap` does with a device token once the device has answered the
//! mmap ioctl (L1.14 / L11.9 class). No kernel dependency, so
//! drivers/uapi-tests includes this file verbatim and checks it on the host.
//!
//! A DRM mmap takes two steps: the mmap ioctl (ownership, sees the open but
//! not the length) and the page pin (sees the length, holds the frames). A
//! buffer freed by GEM_CLOSE on another thread between the two made the pin
//! answer "not a guest-RAM buffer", and the token -- a physical address of
//! frames now back in the allocator -- used to be mapped with the plain,
//! unreferenced device mapping, of any length: a use-after-free mapping of
//! whatever the allocator handed those frames to next. The rule here: on a
//! DRM node, a token that neither pinned nor names a host-visible window span
//! may only be mapped if it is not RAM (framebuffer, BAR apertures).

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

/// `drm_node`: the fd is a DRM node (major 226). `ram`: some byte of the
/// range is allocator-managed RAM (`mm::buddy::is_ram`).
pub fn decide(drm_node: bool, pin: Pin, win: Window, ram: bool) -> Decision {
    match (pin, win) {
        (Pin::Refused, _) | (_, Window::Refused) => Decision::Refuse,
        (Pin::Pinned, _) => Decision::MapPinned,
        (Pin::NotGuestRam, Window::Held) => Decision::MapDevice,
        // Never an unreferenced mapping of RAM through a DRM node: the
        // buffer the ioctl vouched for is gone (or never was guest RAM we
        // track), and its frames may already belong to someone else.
        (Pin::NotGuestRam, Window::NotWindow) if drm_node && ram => Decision::Refuse,
        (Pin::NotGuestRam, Window::NotWindow) => Decision::MapDevice,
    }
}
