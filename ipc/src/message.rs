//! Message format — fixed-size, zero-copy where possible.
//!
//! Small messages (≤ MESSAGE_INLINE_BYTES) are copied inline.
//! Larger payloads use shared memory pages passed by capability.

pub const MESSAGE_INLINE_BYTES: usize = 440;

/// A kernel IPC message.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Message {
    /// Identifies the requested operation (server-defined).
    pub tag: u64,
    /// Port to which the server should send its reply (set by sys_call; 0 = none).
    pub reply_port: u32,
    /// Inline payload for small messages.
    pub data: [u8; MESSAGE_INLINE_BYTES],
    /// Boolean-ish flag for capability presence (to ensure stable repr(C) layout).
    pub has_cap: u64,
    /// Optional shared-memory capability for large payloads.
    pub cap: u64,
}

impl Message {
    pub const fn empty() -> Self {
        Self { tag: 0, reply_port: 0, data: [0; MESSAGE_INLINE_BYTES], has_cap: 0, cap: 0 }
    }
}
