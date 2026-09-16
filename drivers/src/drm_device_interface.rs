//! DRM device interface for userspace applications
//!
//! This module provides the kernel-side interface that userspace applications
//! like DOOM can use to communicate with the DRM subsystem.

use ::core::slice;
use ::core::ptr;
use super::drm::*;
use super::drm_driver::*;
use super::{Driver, DriverError};

// ── Standard Linux DRM IOCTL Constants ───────────────────────────────────────

const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;
const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC01464A6;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC06864A2;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;
const DRM_IOCTL_VERSION: u32 = 0xC0406400;

// ── K4: Mesa/GBM buffer + Smithay/libdrm KMS surface ─────────────────────────
const DRM_IOCTL_GET_CAP: u32 = 0xC010640C;
const DRM_IOCTL_SET_CLIENT_CAP: u32 = 0x4010640D;
const DRM_IOCTL_SET_MASTER: u32 = 0x0000641E;
const DRM_IOCTL_DROP_MASTER: u32 = 0x0000641F;
const DRM_IOCTL_GET_MAGIC: u32 = 0x80046402;
const DRM_IOCTL_AUTH_MAGIC: u32 = 0x40046411;
const DRM_IOCTL_GEM_CLOSE: u32 = 0x40086409;
const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = 0xC00464B4;
const DRM_IOCTL_MODE_ADDFB2: u32 = 0xC06864B8;
const DRM_IOCTL_MODE_RMFB: u32 = 0xC00464AF;
const DRM_IOCTL_MODE_DIRTYFB: u32 = 0xC01864B1;
// _IOWR('d', 0xB9, drm_mode_obj_get_properties) — struct is 28 data bytes,
// padded to 32 by its u64 members, hence size 0x20 in the request code.
const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC02064B9;
const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC01064B5; // _IOWR('d',0xB5, drm_mode_get_plane_res=16)
const DRM_IOCTL_MODE_GETPLANE: u32 = 0xC02064B6;          // _IOWR('d',0xB6, drm_mode_get_plane=32)
const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xC04064AA;       // _IOWR('d',0xAA, drm_mode_get_property=64)
// Synthetic KMS object ids for the single primary plane exposed to compositors.
// crtc/connector/encoder are all id 1; the plane + its "type" property take
// distinct ids. crtc index 0 => possible_crtcs bit 0.
const DRM_PLANE_ID: u32 = 30;
const DRM_PLANE_TYPE_PROP_ID: u32 = 40;
const DRM_PLANE_TYPE_PRIMARY: u32 = 1; // drm PlaneType: Overlay=0, Primary=1, Cursor=2
const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = 0xC00C642D;
const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = 0xC00C642E;

// DRM capability ids (drm_get_cap.capability)
const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
const DRM_CAP_PRIME: u64 = 0x5;
// PRIME capability flags returned in drm_get_cap.value for DRM_CAP_PRIME.
const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
const DRM_PRIME_CAP_EXPORT: u64 = 0x2;
const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
const DRM_CAP_ASYNC_PAGE_FLIP: u64 = 0x7;
const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;
const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;

// drm_set_client_cap.capability
const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
const DRM_CLIENT_CAP_ATOMIC: u64 = 3;

// ── Atomic KMS ───────────────────────────────────────────────────────────────
const DRM_IOCTL_MODE_ATOMIC: u32 = 0xC03864BC;
const DRM_IOCTL_MODE_CREATEPROPBLOB: u32 = 0xC01064BD;
const DRM_IOCTL_MODE_DESTROYPROPBLOB: u32 = 0xC00464BE;
const DRM_IOCTL_MODE_GETPROPBLOB: u32 = 0xC01064AC;

const DRM_CAP_CURSOR_WIDTH: u64 = 0x8;
const DRM_CAP_CURSOR_HEIGHT: u64 = 0x9;

// ── Sync objects (DRM_IOCTL_SYNCOBJ_*) ───────────────────────────────────────
//
// WHY THESE EXIST AT ALL. `TODO.md:3251` recorded that syncobj was "not on the
// critical path — Mesa compiles the SIMULATE path unconditionally". That is
// true of the **virtgpu** gallium/Venus driver and false of **v3d**:
// `drm_v3d_submit_cl` carries `in_sync_bcl`, `in_sync_rcl` and `out_sync` as
// syncobj handles unconditionally and the v3d gallium/vulkan drivers have no
// simulate path. So this is a hard prerequisite for the Pi 5 GPU, and it is
// fully testable under QEMU with no hardware.
//
// REQUEST CODES ARE DERIVED, NOT GUESSED — this file's convention, and the two
// codes that were once guessed here cost real sessions. `_IOC(dir, type, nr,
// size) = dir<<30 | size<<16 | type<<8 | nr`, with `_IOWR` => dir 3 =>
// 0xC000_0000, DRM's type 'd' => 0x64 => 0x6400. Every syncobj ioctl is _IOWR.
// The `nr`s are drm.h's 0xBF..0xCF block. Sizes are the C struct sizes, all of
// which are 8-byte aligned so `sizeof` is just the sum of the fields:
//
//   nr    struct                        fields                          size
//   ────  ────────────────────────────  ──────────────────────────────  ────
//   0xBF  drm_syncobj_create            u32 handle, u32 flags            8  0x08
//   0xC0  drm_syncobj_destroy           u32 handle, u32 pad              8  0x08
//   0xC1  drm_syncobj_handle            u32 handle, u32 flags,
//         (HANDLE_TO_FD)                s32 fd,     u32 pad             16  0x10
//   0xC2  drm_syncobj_handle            (same struct)                   16  0x10
//   0xC3  drm_syncobj_wait              u64 handles, s64 timeout_nsec,
//                                       u32 count_handles, u32 flags,
//                                       u32 first_signaled, u32 pad     32  0x20
//   0xC4  drm_syncobj_array             u64 handles, u32 count, u32 pad 16  0x10
//   0xC5  drm_syncobj_array             (same struct)                   16  0x10
//   0xCA  drm_syncobj_timeline_wait     u64 handles, u64 points,
//                                       s64 timeout_nsec, u32 count,
//                                       u32 flags, u32 first, u32 pad   40  0x28
//   0xCB  drm_syncobj_timeline_array    u64 handles, u64 points,
//                                       u32 count_handles, u32 flags    24  0x18
//   0xCC  drm_syncobj_transfer          u32 src_handle, u32 dst_handle,
//                                       u64 src_point, u64 dst_point,
//                                       u32 flags, u32 pad              32  0x20
//   0xCD  drm_syncobj_timeline_array    (same struct)                   24  0x18
//   0xCF  drm_syncobj_eventfd           u32 handle, u32 flags, u64 point,
//                                       s32 fd, u32 pad                 24  0x18
//
// Cross-check against a code already proven in this file: ATOMIC is nr 0xBC on
// a 56-byte struct and reads 0xC038_64BC — 0xC0000000 | 0x38<<16 | 0x6400 |
// 0xBC. The arithmetic below is identical.
const DRM_IOCTL_SYNCOBJ_CREATE: u32 = 0xC00864BF;
const DRM_IOCTL_SYNCOBJ_DESTROY: u32 = 0xC00864C0;
const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: u32 = 0xC01064C1;
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: u32 = 0xC01064C2;
const DRM_IOCTL_SYNCOBJ_WAIT: u32 = 0xC02064C3;
const DRM_IOCTL_SYNCOBJ_RESET: u32 = 0xC01064C4;
const DRM_IOCTL_SYNCOBJ_SIGNAL: u32 = 0xC01064C5;
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u32 = 0xC02864CA;
const DRM_IOCTL_SYNCOBJ_QUERY: u32 = 0xC01864CB;
const DRM_IOCTL_SYNCOBJ_TRANSFER: u32 = 0xC02064CC;
const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: u32 = 0xC01864CD;
const DRM_IOCTL_SYNCOBJ_EVENTFD: u32 = 0xC01864CF;

/// True for every `DRM_IOCTL_SYNCOBJ_*` request code, implemented or not.
///
/// The DRM server uses this to pick the syncobj errno table: this seam has
/// historically collapsed every failure to -1, and syncobj is the first family
/// whose *errno values* are part of the contract Mesa reads (ETIME vs ENOENT vs
/// EINVAL decide whether a wait retries, a handle is re-created, or the driver
/// aborts). Derived from the encoded `nr` rather than a hand-kept list so a
/// code added above cannot be forgotten here.
pub fn is_syncobj_ioctl(cmd: u32) -> bool {
    if (cmd >> 8) & 0xFF != 0x64 { return false; }
    matches!(cmd & 0xFF, 0xBF | 0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC4 | 0xC5
                       | 0xCA | 0xCB | 0xCC | 0xCD | 0xCF)
}

// drm_syncobj_create.flags
const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;
// drm_syncobj_wait.flags
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 1 << 2;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE: u32 = 1 << 3;

// drm_get_cap.capability
const DRM_CAP_SYNCOBJ: u64 = 0x13;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

#[repr(C)]
#[derive(Clone, Copy)]
struct drm_syncobj_create { handle: u32, flags: u32 }

#[repr(C)]
#[derive(Clone, Copy)]
struct drm_syncobj_destroy { handle: u32, pad: u32 }

#[repr(C)]
#[derive(Clone, Copy)]
struct drm_syncobj_array { handles: u64, count_handles: u32, pad: u32 }

#[repr(C)]
#[derive(Clone, Copy)]
struct drm_syncobj_wait {
    handles: u64,
    /// **Absolute** CLOCK_MONOTONIC nanoseconds, not a duration — upstream's
    /// `drm_timeout_abs_to_jiffies`. <= 0, or already past, means "poll".
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

/// Synthetic plane ids. 30 is the pre-existing primary; 31 is the new cursor
/// plane. crtc/connector/encoder all keep id 1 (see the note above) — object
/// *types* are disambiguated in the atomic path by the property id, since our
/// property-id ranges are disjoint per object class.
const DRM_CURSOR_PLANE_ID: u32 = 31;
const DRM_PLANE_TYPE_CURSOR: u32 = 2;
const DRM_CRTC_ID: u32 = 1;
const DRM_CONNECTOR_ID: u32 = 1;

// Property ids. Ranges are deliberately disjoint per object class:
//   40..=51 plane, 60..=61 crtc, 70 connector.
const PROP_TYPE: u32 = 40; // == DRM_PLANE_TYPE_PROP_ID
const PROP_PLANE_CRTC_ID: u32 = 41;
const PROP_FB_ID: u32 = 42;
const PROP_SRC_X: u32 = 43;
const PROP_SRC_Y: u32 = 44;
const PROP_SRC_W: u32 = 45;
const PROP_SRC_H: u32 = 46;
const PROP_CRTC_X: u32 = 47;
const PROP_CRTC_Y: u32 = 48;
const PROP_CRTC_W: u32 = 49;
const PROP_CRTC_H: u32 = 50;
const PROP_FB_DAMAGE_CLIPS: u32 = 51;
const PROP_ACTIVE: u32 = 60;
const PROP_MODE_ID: u32 = 61;
const PROP_CONN_CRTC_ID: u32 = 70;

// drm_mode_object types (used as OBJECT-property value types).
const DRM_MODE_OBJECT_CRTC: u64 = 0xcccc_cccc;
const DRM_MODE_OBJECT_FB: u64 = 0xfbfb_fbfb;
const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
const DRM_MODE_OBJECT_CRTC_T: u32 = 0xcccc_cccc;
const DRM_MODE_OBJECT_CONNECTOR_T: u32 = 0xc0c0_c0c0;

// drm_mode_property flags.
const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
const DRM_MODE_PROP_OBJECT: u32 = 1 << 6; // DRM_MODE_PROP_TYPE(1)
const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6; // DRM_MODE_PROP_TYPE(2)

// drm_mode_atomic.flags
const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

/// How a property's value array must be reported. The compositor's drm-rs
/// indexes `values[0]`/`values[1]` **unchecked** for RANGE, SIGNED_RANGE and
/// OBJECT properties, so returning `count_values = 0` for any of them panics
/// cosmic-comp. Every entry below therefore carries a concrete value array.
#[derive(Clone, Copy, PartialEq)]
enum PropKind {
    /// count_values = 2, values = [min, max]
    Range(u64, u64),
    /// count_values = 2, values = [min as u64, max as u64]
    SignedRange(i64, i64),
    /// count_values = 1, values = [object type]
    Object(u64),
    /// count_values = 0 — no array access in drm-rs for blobs.
    Blob,
    /// count_values = 0, count_enum_blobs = 0. This is exactly what the legacy
    /// path already shipped for "type" and what smithay's plane_type() needs
    /// (it reads the raw property value, never the enum names).
    Enum,
}

struct PropDef {
    id: u32,
    name: &'static [u8], // NUL-terminated, <= 32 bytes
    kind: PropKind,
}

const fn prop(id: u32, name: &'static [u8], kind: PropKind) -> PropDef {
    PropDef { id, name, kind }
}

/// The complete property table. `flags` is derived from `kind`.
static PROPS: &[PropDef] = &[
    prop(PROP_TYPE, b"type\0", PropKind::Enum),
    prop(PROP_PLANE_CRTC_ID, b"CRTC_ID\0", PropKind::Object(DRM_MODE_OBJECT_CRTC)),
    prop(PROP_FB_ID, b"FB_ID\0", PropKind::Object(DRM_MODE_OBJECT_FB)),
    prop(PROP_SRC_X, b"SRC_X\0", PropKind::Range(0, u32::MAX as u64)),
    prop(PROP_SRC_Y, b"SRC_Y\0", PropKind::Range(0, u32::MAX as u64)),
    prop(PROP_SRC_W, b"SRC_W\0", PropKind::Range(0, u32::MAX as u64)),
    prop(PROP_SRC_H, b"SRC_H\0", PropKind::Range(0, u32::MAX as u64)),
    prop(PROP_CRTC_X, b"CRTC_X\0", PropKind::SignedRange(i32::MIN as i64, i32::MAX as i64)),
    prop(PROP_CRTC_Y, b"CRTC_Y\0", PropKind::SignedRange(i32::MIN as i64, i32::MAX as i64)),
    prop(PROP_CRTC_W, b"CRTC_W\0", PropKind::Range(0, u32::MAX as u64)),
    prop(PROP_CRTC_H, b"CRTC_H\0", PropKind::Range(0, u32::MAX as u64)),
    prop(PROP_FB_DAMAGE_CLIPS, b"FB_DAMAGE_CLIPS\0", PropKind::Blob),
    prop(PROP_ACTIVE, b"ACTIVE\0", PropKind::Range(0, 1)),
    prop(PROP_MODE_ID, b"MODE_ID\0", PropKind::Blob),
    prop(PROP_CONN_CRTC_ID, b"CRTC_ID\0", PropKind::Object(DRM_MODE_OBJECT_CRTC)),
];

fn prop_def(id: u32) -> Option<&'static PropDef> {
    PROPS.iter().find(|p| p.id == id)
}

fn prop_flags(kind: PropKind) -> u32 {
    match kind {
        PropKind::Range(..) => DRM_MODE_PROP_RANGE,
        PropKind::SignedRange(..) => DRM_MODE_PROP_SIGNED_RANGE,
        PropKind::Object(..) => DRM_MODE_PROP_OBJECT,
        PropKind::Blob => DRM_MODE_PROP_BLOB,
        PropKind::Enum => DRM_MODE_PROP_ENUM,
    }
}

/// The exact value array a property reports. Both GETPROPERTY passes call this,
/// which is what keeps the two-pass counts identical (drm-ffi does
/// `Vec::set_len` from the *second* call's count).
fn prop_values(kind: PropKind) -> [u64; 2] {
    match kind {
        PropKind::Range(min, max) => [min, max],
        PropKind::SignedRange(min, max) => [min as u64, max as u64],
        PropKind::Object(ty) => [ty, 0],
        PropKind::Blob | PropKind::Enum => [0, 0],
    }
}

fn prop_value_count(kind: PropKind) -> u32 {
    match kind {
        PropKind::Range(..) | PropKind::SignedRange(..) => 2,
        PropKind::Object(..) => 1,
        PropKind::Blob | PropKind::Enum => 0,
    }
}

/// Property ids exposed by each object, in report order, with their current
/// values. `obj_type` disambiguates crtc (1) from connector (1).
fn object_props(obj_id: u32, obj_type: u32) -> &'static [u32] {
    const PLANE_COMMON: &[u32] = &[
        PROP_TYPE,
        PROP_PLANE_CRTC_ID,
        PROP_FB_ID,
        PROP_SRC_X,
        PROP_SRC_Y,
        PROP_SRC_W,
        PROP_SRC_H,
        PROP_CRTC_X,
        PROP_CRTC_Y,
        PROP_CRTC_W,
        PROP_CRTC_H,
        PROP_FB_DAMAGE_CLIPS,
    ];
    // The cursor plane omits FB_DAMAGE_CLIPS: it is always uploaded whole.
    const CURSOR_PLANE: &[u32] = &[
        PROP_TYPE,
        PROP_PLANE_CRTC_ID,
        PROP_FB_ID,
        PROP_SRC_X,
        PROP_SRC_Y,
        PROP_SRC_W,
        PROP_SRC_H,
        PROP_CRTC_X,
        PROP_CRTC_Y,
        PROP_CRTC_W,
        PROP_CRTC_H,
    ];
    const CRTC: &[u32] = &[PROP_ACTIVE, PROP_MODE_ID];
    const CONNECTOR: &[u32] = &[PROP_CONN_CRTC_ID];

    match obj_type {
        DRM_MODE_OBJECT_PLANE => match obj_id {
            DRM_PLANE_ID => PLANE_COMMON,
            DRM_CURSOR_PLANE_ID => CURSOR_PLANE,
            _ => &[],
        },
        DRM_MODE_OBJECT_CRTC_T if obj_id == DRM_CRTC_ID => CRTC,
        DRM_MODE_OBJECT_CONNECTOR_T if obj_id == DRM_CONNECTOR_ID => CONNECTOR,
        // obj_type 0 (DRM_MODE_OBJECT_ANY) or an unrecognised type: fall back
        // to the plane ids, which are the only unambiguous ones.
        _ => match obj_id {
            DRM_PLANE_ID => PLANE_COMMON,
            DRM_CURSOR_PLANE_ID => CURSOR_PLANE,
            _ => &[],
        },
    }
}

// PAGE_FLIP flags / event types
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

// ── Virtio-GPU (virtgpu_drm.h) IOCTLs ────────────────────────────────────────
//
// Every driver-private DRM ioctl number is
//   DRM_IOWR(DRM_COMMAND_BASE + nr, struct) =
//   (3 << 30) | (size_of::<struct>() << 16) | ('d' << 8) | (0x40 + nr)
//
// The previous constants here were wrong twice over: they omitted the
// DRM_COMMAND_BASE (0x40) offset entirely — so EXECBUFFER was 0x…6402 where
// Linux says 0x…6442 — and several encoded stale struct sizes (GET_CAPS carried
// 0x08 for a 24-byte struct).  A userspace caller using the real
// virtgpu_drm.h numbers therefore matched no dispatch arm at all and fell
// through to Unsupported.  These are recomputed field-for-field against
// /usr/include/drm/virtgpu_drm.h.
const DRM_IOCTL_VIRTGPU_MAP: u32 = 0xC0106441;                  // drm_virtgpu_map, 16
const DRM_IOCTL_VIRTGPU_EXECBUFFER: u32 = 0xC0406442;           // drm_virtgpu_execbuffer, 64
const DRM_IOCTL_VIRTGPU_GETPARAM: u32 = 0xC0106443;             // drm_virtgpu_getparam, 16
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: u32 = 0xC0386444;      // 14 * u32 = 56
const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: u32 = 0xC0106445;        // 4 * u32 = 16
const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: u32 = 0xC02C6446;   // 11 * u32 = 44
const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: u32 = 0xC02C6447;     // 11 * u32 = 44
const DRM_IOCTL_VIRTGPU_WAIT: u32 = 0xC0086448;                 // drm_virtgpu_3d_wait, 8
const DRM_IOCTL_VIRTGPU_GET_CAPS: u32 = 0xC0186449;             // drm_virtgpu_get_caps, 24
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB: u32 = 0xC030644A; // 48
const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: u32 = 0xC010644B;         // 16

// virtgpu_drm.h GETPARAM ids.
const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
const VIRTGPU_PARAM_CROSS_DEVICE: u64 = 5;
const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs: u64 = 7;
const VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;

/// LeandrOS-PRIVATE debug param — NOT upstream ABI, and deliberately not in the
/// upstream numbering. Reads back the virtgpu 3D context id bound to the
/// *calling open* (0 = that open has no context yet).
///
/// Upstream virtgpu params are 1..=8 and Mesa only ever queries those, so a
/// value this far above the range cannot collide with a param upstream might
/// add later; a Mesa that has never heard of it simply never asks.
///
/// It exists because per-open context ownership is otherwise unobservable from
/// userspace: with one global context slot, a submission on fd A still returns
/// 0 while executing in fd B's context, so `ioctl(...) == 0` cannot tell a
/// correct kernel from a broken one. `userland/venustest` phase 2 asserts on
/// this value instead of on liveness.
const VIRTGPU_PARAM_LEANDROS_CTX_ID: u64 = 0x1000_0001;

/// LeandrOS-private GETPARAM (see the note above for why the numbering is safe):
/// how many host-visible window reservations are live right now.
///
/// Same justification as CTX_ID — it makes an otherwise invisible kernel
/// invariant assertable. Whether `RESOURCE_UNMAP_BLOB` + `hostvis_free` actually
/// return the window space a HOST3D blob took cannot be seen through any
/// upstream interface: a leaking kernel keeps returning success on every
/// create/map/close cycle and only fails once the window is exhausted, which is
/// tens of thousands of cycles away. `userland/venustest` reads this before and
/// after a burst of cycles and requires it to come back to where it started.
const VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS: u64 = 0x1000_0002;

/// LeandrOS-private GETPARAM: the host-visible window's length in MiB, 0 if the
/// device exposed no such window. MiB rather than bytes because GETPARAM writes
/// a 32-bit int through the user pointer (upstream's `copy_to_user(..., &value,
/// sizeof(int))`) and the window is routinely 4 GiB.
const VIRTGPU_PARAM_LEANDROS_HOSTVIS_MIB: u64 = 0x1000_0003;

/// LeandrOS-private GETPARAM: the low 32 bits of the fence id produced by the
/// most recent EXECBUFFER **on the calling open**, 0 if that open has never
/// submitted.
///
/// Same justification as CTX_ID, for the sibling process-global. While the
/// submitting fence lived in one `LAST_EXEC_FENCE` atomic, an open that had
/// never submitted anything still observed whichever open submitted last — and
/// that was invisible from userspace, because every WAIT returned 0 either way
/// (submission is synchronous, so every fence is already retired). Reading this
/// on two opens is what makes "the fence is per-open" assertable rather than
/// merely believed. Truncated to 32 bits because GETPARAM writes an `int`.
const VIRTGPU_PARAM_LEANDROS_LAST_FENCE: u64 = 0x1000_0004;

/// LeandrOS-private GETPARAM: how many blob **objects** are live right now —
/// not handles, not fds. Same justification as CTX_ID and HOSTVIS_SPANS: it
/// makes an otherwise invisible kernel invariant assertable.
///
/// Specifically, it is the only way userspace can tell "the exported fd kept
/// the buffer alive" from "the buffer was freed and the read happened to find
/// plausible bytes". A count of handles would answer neither question, because
/// the whole point of `BO LIFETIME` is that a handle and an object stop being
/// the same thing. `userland/venustest` phase 6 asserts on it across a
/// GEM_CLOSE-then-close(fd) sequence.
const VIRTGPU_PARAM_LEANDROS_BLOB_OBJS: u64 = 0x1000_0005;

/// `drm_virtgpu_context_set_param.param` values.
const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;
const VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK: u64 = 0x0003;
const VIRTGPU_CONTEXT_PARAM_DEBUG_NAME: u64 = 0x0004;

/// `drm_virtgpu_execbuffer.flags` (virtgpu_drm.h).
const VIRTGPU_EXECBUF_FENCE_FD_IN: u32 = 0x01;
const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x02;
const VIRTGPU_EXECBUF_RING_IDX: u32 = 0x04;
/// Every flag upstream defines. Anything outside this is a flag from a kernel
/// newer than the one this driver was written against.
const VIRTGPU_EXECBUF_FLAGS_KNOWN: u32 =
    VIRTGPU_EXECBUF_FENCE_FD_IN | VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX;

// ── 3D-path tracing ──────────────────────────────────────────────────────────
//
// Same pattern, and the same reason, as `pci::RENDER_DEBUG`: these sites sit on
// the per-frame EXECBUFFER/ioctl path, where a per-byte serial write per call is
// measurable. Flip to `true` locally to get the full per-call trace back — it is
// what made the M2 execbuffer work tractable, so it is kept rather than deleted.
//
// Note this covers only the HOT traces. Rare, genuinely-unexpected events
// (an unknown ioctl, a field we were asked to honour and did not) log
// unconditionally through `serial_debug`, deduplicated so they cannot flood.
pub const GPU3D_DEBUG: bool = false;

#[inline(always)]
fn gdbg(msg: &str) {
    if GPU3D_DEBUG { crate::pci::serial_debug(msg); }
}
#[inline(always)]
fn gdbg_hex(v: u32) {
    if GPU3D_DEBUG { crate::pci::serial_debug_hex(v); }
}
#[inline(always)]
fn gdbg_hex_64(v: u64) {
    if GPU3D_DEBUG { crate::pci::serial_debug_hex_64(v); }
}

// ── One-shot diagnostics ─────────────────────────────────────────────────────
//
// Both blind spots these close sit on paths a client can drive thousands of
// times a second (an unknown ioctl in a retry loop; EXECBUFFER once per frame),
// so an unconditional log would drown the serial console and change the timing
// of the very thing being diagnosed. Each distinct *shape* is therefore reported
// exactly once per boot: the first occurrence is never lost, and the millionth
// costs one compare.
//
// LOCK ORDER: both are leaf `spin::Mutex`es holding plain integers. Neither is
// ever taken while another lock is held, and no user memory is touched under
// them (the 82d0cc3 freeze class).
const MAX_NOTED: usize = 32;

struct NoteSet {
    seen: [u32; MAX_NOTED],
    n: usize,
}

impl NoteSet {
    const fn new() -> Self { Self { seen: [0; MAX_NOTED], n: 0 } }
    /// True the first time `key` is offered (and then never again). Returns
    /// true once the table is full, too — better a repeated line than a
    /// silently dropped one.
    fn first(&mut self, key: u32) -> bool {
        if self.seen[..self.n].contains(&key) { return false; }
        if self.n < MAX_NOTED { self.seen[self.n] = key; self.n += 1; }
        true
    }
}

/// ioctl numbers that reached the dispatch default arm.
static UNKNOWN_IOCTLS: Mutex<NoteSet> = Mutex::new(NoteSet::new());
/// EXECBUFFER requests carrying fields we do not act on, keyed by the shape of
/// the divergence rather than by the call.
static EXEC_DIVERGENCE: Mutex<NoteSet> = Mutex::new(NoteSet::new());


// ── Standard Linux DRM Structs ───────────────────────────────────────────────

#[repr(C)]
#[derive(Default)]
struct drm_mode_card_res {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_modeinfo {
    clock: u32,
    hdisplay: u16, hsync_start: u16, hsync_end: u16, htotal: u16, hskew: u16,
    vdisplay: u16, vsync_start: u16, vsync_end: u16, vtotal: u16, vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_connector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_encoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_crtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: drm_mode_modeinfo,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_create_dumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_map_dumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_fb_cmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_crtc_page_flip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_get_cap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_set_client_cap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_gem_close {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_mode_destroy_dumb {
    handle: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_auth {
    magic: u32,
}

// 104 bytes: repr(C) inserts 4 bytes of pad before `modifier` (u64 alignment).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_mode_fb_cmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_mode_fb_dirty_cmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

// DRM event blobs delivered by read() on the card fd.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct drm_event_vblank {
    ev_type: u32,
    length: u32,
    user_data: u64,
    tv_sec: u32,
    tv_usec: u32,
    sequence: u32,
    crtc_id: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_version {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: u64,
    date_len: usize,
    date: u64,
    desc_len: usize,
    desc: u64,
}

// ── virtgpu_drm.h structs, field-for-field ───────────────────────────────────
// Mesa's venus backend (`vn_renderer_virtgpu.c`) issues these via raw ioctl()
// with its own vendored copy of the header, so any field that is out of order
// or missing here is read as a different field's bytes and never diagnosed.

#[repr(C)]
struct drm_virtgpu_resource_create {
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
    bo_handle: u32,
    res_handle: u32,
    // These two were missing entirely, which is why the ioctl number encoded a
    // 48-byte struct where Linux says 56.
    size: u32,
    stride: u32,
}

#[repr(C)]
struct drm_virtgpu_execbuffer {
    // Upstream order is flags-then-size-then-command. The previous definition
    // led with `command`, so a caller filling the real struct had its `flags`
    // and `size` words read as the low and high halves of the command pointer.
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}

#[repr(C)]
struct drm_virtgpu_get_caps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
}

#[repr(C)]
struct drm_virtgpu_getparam {
    param: u64,
    value: u64,
}

#[repr(C)]
struct drm_virtgpu_map {
    offset: u64,
    handle: u32,
    pad: u32,
}

#[repr(C)]
struct drm_virtgpu_3d_wait {
    handle: u32,
    flags: u32,
}

/// `struct drm_virtgpu_resource_info` — four u32s, 16 bytes.
///
/// `bo_handle` is in; `res_handle`, `size` and `blob_mem` are out. Verbatim from
/// Mesa's vendored copy of the kernel uAPI
/// (`mesa/include/drm-uapi/virtgpu_drm.h`), which is the header the Venus ICD is
/// compiled against, and byte-identical to libdrm's and the kernel's own.
///
/// NOTE the ioctl number: DRM_IOWR over a 16-byte struct is 0xC010_6445. A
/// 0xC01C_6445 (0x1C = 28-byte) encoding is sometimes quoted for this ioctl and
/// is wrong — no revision of this struct has ever been 28 bytes. (0x1C is
/// DRM_IOCTL_MODE_ADDFB's payload size, a few lines above.)
#[repr(C)]
struct drm_virtgpu_resource_info {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}

/// The ioctl number embeds the struct's size, so a field added to the struct
/// without updating the constant (or vice versa) is a silent ABI break that
/// only shows up as "Mesa's ioctl matched no dispatch arm". Tie the two
/// together at compile time instead — see the DRM_IOWR formula above.
const _: () = assert!(
    DRM_IOCTL_VIRTGPU_RESOURCE_INFO
        == (3u32 << 30)
            | ((::core::mem::size_of::<drm_virtgpu_resource_info>() as u32) << 16)
            | (0x64u32 << 8)
            | (DRM_COMMAND_BASE + 0x05)
);
/// `DRM_COMMAND_BASE` from drm.h: driver-private ioctl nrs start at 0x40.
const DRM_COMMAND_BASE: u32 = 0x40;

#[repr(C)]
struct drm_virtgpu_resource_create_blob {
    blob_mem: u32,
    blob_flags: u32,
    bo_handle: u32,
    res_handle: u32,
    size: u64,
    pad: u32,
    cmd_size: u32,
    cmd: u64,
    blob_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct drm_virtgpu_context_set_param {
    param: u64,
    value: u64,
}

#[repr(C)]
struct drm_virtgpu_context_init {
    num_params: u32,
    pad: u32,
    ctx_set_params: u64,
}

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::vec;
use alloc::collections::VecDeque;
use ::core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

/// A dumb buffer's physical base and buddy allocation order, so DESTROY_DUMB /
/// GEM_CLOSE can return the exact pages to the allocator (freeing the wrong
/// order corrupts the buddy allocator).
#[derive(Clone, Copy)]
struct DumbBuf {
    phys: usize,
    order: usize,
    /// Fence of the most recent EXECBUFFER that named this BO in `bo_handles`
    /// (0 = never named). See "THE FENCE MODEL" on `BlobObj`.
    ///
    /// A dumb buffer is a 2D scanout target and no 3D client has any reason to
    /// name one in a submission — but `bo_handles` is a plain handle array and
    /// nothing stops it, so the field exists rather than leaving one BO kind
    /// silently unfenceable. That would make VIRTGPU_WAIT answer "nothing
    /// outstanding" for a buffer a submission had genuinely touched, which is
    /// exactly the class of wrong answer the per-BO fence exists to remove.
    last_fence: u64,
    /// Lifetime identity, from the same `NEXT_BO_OBJ` space blob objects use.
    /// It is what an exported dmabuf fd remembers, because a *gem handle* must
    /// not be what keeps a buffer alive: handles are retired by DESTROY_DUMB /
    /// GEM_CLOSE while the fd is still open, and (for blobs) they are per-open.
    /// See `BO LIFETIME` below.
    obj: u32,
    /// Live references. One for the gem handle itself while `handle_live`, plus
    /// one for **each exporting `TmpVmo` slot** (per slot, not per fd — dup,
    /// fork and SCM_RIGHTS copies of one dmabuf fd already share one slot).
    refs: u32,
    /// False once DESTROY_DUMB / GEM_CLOSE has retired the gem handle. The
    /// record then survives only to keep exporting fds valid, and every handle
    /// resolution path treats it as absent, so the handle number is as dead as
    /// it was before this refcount existed.
    handle_live: bool,
    /// Host virtio-gpu resource id for a BO created by
    /// `DRM_IOCTL_VIRTGPU_RESOURCE_CREATE` (a virgl 3D resource), or 0 for a
    /// plain dumb buffer whose host resource is derived elsewhere.
    ///
    /// virgl BOs live in this registry rather than one of their own precisely
    /// so that every existing consumer resolves them unchanged — PRIME export,
    /// MAP_DUMB, ADDFB2 and the refcounted BO lifetime all key off a gem handle
    /// here. What they could not derive is the *host* resource id, which
    /// TRANSFER_TO/FROM_HOST_3D need, so it is carried alongside.
    res_id: u32,
}

static DUMB_BUFFERS: Mutex<BTreeMap<u32, DumbBuf>> = Mutex::new(BTreeMap::new());

// ── BO LIFETIME ──────────────────────────────────────────────────────────────
//
// THE BUG THIS EXISTS TO CLOSE. `release_blob`/`free_dumb` used to call
// `mm::buddy::free(phys, order)` the moment the gem handle went away, and
// `vmo_free_slot` (servers/vfs) returns early for a borrowed VMO on the stated
// grounds that the DRM layer frees the block exactly once. Nothing anywhere
// made the DRM object outlive an exported dmabuf fd, so from ONE unprivileged
// process, with no cross-open work at all:
//
//     h  = RESOURCE_CREATE_BLOB(blob_mem = GUEST, size = N)
//     fd = PRIME_HANDLE_TO_FD(h)          // borrowed VMO aliases those frames
//     GEM_CLOSE(h)                        // buddy::free(phys, order)
//     read(fd, buf, N)                    // walks the FREED frames via the HHDM
//
// The read succeeded and returned whatever the buddy allocator had since handed
// those frames to — page tables, slab pages, another process's anonymous memory
// — and `mmap(fd, MAP_SHARED)` was the same hazard with writes. Pre-existing on
// the dumb path since PRIME export existed; widened to blobs by the export.
//
// THE RULE. A BO object is destroyed when its reference count reaches zero.
// References are held by:
//   1. each gem handle naming it (a `BlobHandle` in `BLOB_BUFFERS`, or a live
//      `DUMB_BUFFERS` entry), and
//   2. each **exporting `TmpVmo` slot** in the VFS.
// Granularity 2 is per slot and not per fd on purpose: `TMP_VMOS` is keyed by
// the data-owning slot, so dup/fork/SCM_RIGHTS copies of one dmabuf fd share
// one slot and that slot is destroyed exactly once, by `vmo_free_slot`. One ref
// per slot is therefore both sufficient and impossible to double-drop.
//
// THE OPPOSITE FAILURE — double release — is the class this project hit in
// `9be954f` (the `import_fd` EMFILE double-release): two `resource_unref`s for
// one resource and a double `mm::buddy::free` of an order-N block, which is
// allocator corruption rather than a leak. Two structural guards:
//   * every decrement is a test-and-remove under ONE acquisition of the object
//     map, so two racing droppers cannot both observe zero;
//   * the teardown body lives INSIDE `blob_unref`'s / `dumb_unref_by_obj`'s
//     zero arm, on the record those functions removed. There is no
//     `release_blob(record)` entry point any more, so there is nothing a caller
//     that "knows" the count could call.
// An unref of an object that is already gone logs `[DRM] bo refcount underflow`
// and returns without freeing anything.
//
// LOCK ORDER. `BLOB_BUFFERS` (handles) and `BLOB_OBJS` (objects) are separate
// leaf locks and are taken ONE AT A TIME, never nested, exactly as
// `BLOB_BUFFERS`/`DUMB_BUFFERS` already were. Resolving a handle therefore
// reads the handle map, drops it, then reads the object map; if the object
// vanished in between the answer is None, which is the correct "this handle
// names nothing" and is indistinguishable from the handle having been closed a
// microsecond earlier. `VIRTIO_GPU` is taken with NO BO map held, ever.

/// Lifetime identity for a BO of either kind. Never reused within a boot, so a
/// stale id resolves to nothing rather than to a different buffer.
static NEXT_BO_OBJ: AtomicU32 = AtomicU32::new(1);

/// A virtgpu blob buffer object created through DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB.
/// `phys`/`order` are the guest pages handed to the host as the blob's backing
/// (zero for host-side blob memory, which the guest never owns pages for);
/// `res_handle` is the resource id the host knows it by.
///
/// THE FENCE MODEL. Upstream attaches a submission's fence to every GEM object
/// the submission named in `drm_virtgpu_execbuffer.bo_handles`, and
/// `virtio_gpu_wait_ioctl` then waits on the fence of the ONE object its
/// `handle` names. That is why WAIT takes a BO handle at all. This driver
/// previously had no per-BO fence state, so `bo_handles` was discarded and WAIT
/// ignored its handle and consulted a single process-global `LAST_EXEC_FENCE` —
/// meaning a WAIT on open A's buffer was answered from open B's submission.
///
/// `last_fence` is that missing state. Note what it does and does not change
/// TODAY: submission is a synchronous busy-spin, so every fence this driver
/// hands out is already retired by the time EXECBUFFER returns, and both the
/// old and new code therefore answer every WAIT with success. The value is that
/// the *contract* is now the upstream one — an unknown handle is refused, a
/// handle is answered about itself, and no open can observe another's fence —
/// so when submission becomes asynchronous (the unwired ISR) the semantics are
/// already right rather than needing to be re-derived under a live client.
#[derive(Clone, Copy)]
struct BlobObj {
    phys: usize,
    order: usize,
    res_handle: u32,
    size: u64,
    /// Fence of the most recent EXECBUFFER that named this BO in `bo_handles`;
    /// 0 = never named in any submission, i.e. nothing is outstanding for it.
    /// On the OBJECT rather than the handle: upstream attaches the fence to the
    /// `virtio_gpu_object`, and two handles naming one buffer must not disagree
    /// about whether work against it has retired.
    last_fence: u64,
    /// `blob_mem` the blob was created with (VIRTIO_GPU_BLOB_MEM_*). Non-zero
    /// for every *virtgpu* entry: RESOURCE_CREATE_BLOB rejects blob_mem == 0.
    /// RESOURCE_INFO reports it, and Mesa's Venus backend refuses an imported BO
    /// whose blob_mem is not the one it allocates with.
    ///
    /// **0 for a v3d BO**, which is not a blob at all and has no virtio-gpu
    /// blob_mem to report. The two kinds can never be seen by one client — the
    /// backends are mutually exclusive for the life of the boot — so no virtgpu
    /// consumer of this field is reachable with a v3d object in hand; `v3d_va`
    /// is the field that says which kind this is.
    blob_mem: u32,
    /// Host-visible window bookkeeping, both zero until RESOURCE_MAP_BLOB has
    /// succeeded for this blob (and zero forever for a guest-backed one). Per
    /// RESOURCE and not per open: the placement is a property of the host
    /// resource, so a second handle on the same object sees the same token.
    ///   * `win_off`  — byte offset of the reservation inside the shared-memory
    ///                  window, the value handed to RESOURCE_MAP_BLOB and the key
    ///                  `hostvis_free` releases.
    ///   * `map_phys` — the guest-physical address that offset resolves to
    ///                  (`window.phys + win_off`), which IS the mmap token
    ///                  VIRTGPU_MAP reports. Non-zero is the "is mapped" flag:
    ///                  the window base is a PCI BAR address and can never be 0.
    win_off: u64,
    map_phys: u64,
    /// `map_info` the host answered RESOURCE_MAP_BLOB with (VIRTIO_GPU_MAP_CACHE_*).
    /// Load-bearing, not diagnostic: `blob_map_cache_type` reports it to
    /// `sys_mmap`, which maps the blob non-cached when the host asked for
    /// UNCACHED or WC. See the cacheability note on `virtgpu_handle_map`.
    map_info: u32,
    /// Live references: one per `BlobHandle` naming this object, plus one per
    /// exporting `TmpVmo` slot. See `BO LIFETIME`.
    refs: u32,
    /// GPU virtual address, for a BO created by `DRM_IOCTL_V3D_CREATE_BO`;
    /// **0 for every virtgpu blob**, which is also what makes this field the
    /// marker for "this object belongs to the v3d backend".
    ///
    /// Reusing `BLOB_OBJS` for v3d BOs rather than standing up a third registry
    /// is deliberate: GEM_CLOSE, the refcounted `BO LIFETIME` rules, per-open
    /// reachability, `bo_fence`/`bo_attach_fence`, PRIME export and the
    /// `bo_census` leak audit are all generic and all key off this map, and a
    /// parallel registry would have to re-derive every one of them (and could
    /// drift from them). What is NOT generic is the host round-trip in
    /// `blob_unref` — a v3d BO has no virtio-gpu resource behind it — so that
    /// teardown is gated on `res_handle != 0`, which is a true statement about
    /// any object with no host resource and not a v3d special case.
    ///
    /// A u64 rather than the u32 the UAPI carries, so the allocator's window
    /// arithmetic (`V3D_VA_TOP` is 0x1_0000_0000) does not have to be done in a
    /// type that cannot hold its own upper bound. Every value stored here fits
    /// in a u32 by construction.
    v3d_va: u64,
}

/// One gem handle naming a `BlobObj`. **The handle IS one reference.**
#[derive(Clone, Copy)]
struct BlobHandle {
    /// Key into `BLOB_OBJS`.
    obj: u32,
    /// The `open_id` that may use this handle — the per-open GEM handle table,
    /// flattened into an owner tag on a shared map rather than a map per open.
    ///
    /// Upstream gives each `drm_file` its own handle→object table, so handle 5
    /// on two opens is two different objects and one open simply cannot name
    /// the other's. Here the handle space stays global (handles are unique
    /// process-wide, allocated from `NEXT_BLOB_HANDLE`) and the isolation is
    /// enforced by comparing this field instead. That yields the property that
    /// matters — no open can map, describe, wait on, EXPORT over PRIME, or
    /// close a BO belonging to another — without renumbering a handle space
    /// that `dumb_buffer_phys_order` and the framebuffer path still resolve
    /// globally.
    ///
    /// 0 means "created without an open identity" (the legacy `Driver::handle`
    /// path, which passes open_id 0). Such a BO is unowned and reachable from
    /// anywhere, and an open_id-0 caller may reach anything — there is no
    /// identity to check in either direction. See `blob_lookup`.
    owner: u32,
    /// The 3D context THIS handle's open attached the resource to (0 = none).
    /// Per HANDLE rather than per object because attachment is per-open: the
    /// handle that goes away detaches its own context binding and only its own.
    /// A blob may well outlive the CONTEXT_INIT that was current when it was
    /// made, which is why the binding is remembered here at all.
    ctx: u32,
}

/// Objects. Keyed by `NEXT_BO_OBJ` id; nothing outside this module names one.
static BLOB_OBJS: Mutex<BTreeMap<u32, BlobObj>> = Mutex::new(BTreeMap::new());
/// Handles. Unchanged key space (`NEXT_BLOB_HANDLE`), new value.
static BLOB_BUFFERS: Mutex<BTreeMap<u32, BlobHandle>> = Mutex::new(BTreeMap::new());
/// GEM handles for blob BOs. Kept well above the dumb-buffer handle space so a
/// handle is unambiguously one or the other.
static NEXT_BLOB_HANDLE: AtomicU32 = AtomicU32::new(0x4000);

/// A handle joined to its object — what `blob_lookup` answers with, so every
/// consumer keeps reading one flat record and the split stays invisible to
/// them. A snapshot: holding one implies nothing about the BO still existing,
/// exactly as the old by-value `BlobBuf` copy did.
///
/// Carries only what a *consumer* of a handle needs. `owner` is deliberately
/// absent — `blob_lookup` has already applied `open_may_reach` and re-exposing
/// the tag would invite a second, divergent copy of that test. So is `ctx`: the
/// only thing that acts on a context binding is the teardown of the handle that
/// made it, and that reads `BlobHandle` directly.
#[derive(Clone, Copy)]
struct BlobView {
    obj: u32,
    phys: usize,
    res_handle: u32,
    size: u64,
    last_fence: u64,
    blob_mem: u32,
    map_phys: u64,
    /// See `BlobObj::v3d_va`. Non-zero means this is a v3d BO, which is what
    /// GET_BO_OFFSET answers with and what MMAP_BO refuses to confuse with a
    /// virtgpu blob's host-visible token.
    v3d_va: u64,
}

impl BlobView {
    fn join(obj: u32, o: BlobObj) -> Self {
        Self {
            obj,
            phys: o.phys,
            res_handle: o.res_handle,
            size: o.size,
            last_fence: o.last_fence,
            blob_mem: o.blob_mem,
            map_phys: o.map_phys,
            v3d_va: o.v3d_va,
        }
    }
}

// ── Host-visible blob window allocator ───────────────────────────────────────
//
// A VIRTIO_GPU_BLOB_MEM_HOST3D resource lives in HOST memory. The guest reaches
// it by asking the host, via RESOURCE_MAP_BLOB, to place it at a guest-chosen
// byte offset inside the device's shared-memory region (the
// VIRTIO_PCI_CAP_SHARED_MEMORY_CFG window, shmid = SHM_ID_HOST_VISIBLE), and
// then mapping the resulting guest-physical range. Choosing those offsets is
// entirely the guest's job — the host only refuses overlaps — so this is a real
// allocator, not a counter.
//
// POLICY: first-fit over the live spans, ascending, with 64 KiB granularity.
//   * First-fit over an ordered map (rather than a bump pointer) is what makes
//     the space actually recyclable: a Vulkan app that creates and destroys ~20
//     blobs must reuse the same low offsets, not walk the window.
//   * 64 KiB granularity keeps every reservation aligned for any host page size
//     we might meet (4 KiB on the x86-64 Linux box, 16 KiB on Apple silicon,
//     64 KiB on a large-page arm64 host) — QEMU adds the mapped resource as a
//     RAM-device subregion at this offset, and an unaligned subregion cannot be
//     handed to a hardware memory slot. It also bounds fragmentation.
// BOUNDS: total handed out is capped by the window the device advertised
//   (`window.len`, 4 GiB as configured today) and each request by
//   MAX_BLOB_BYTES (64 MiB); the number of live spans is capped by
//   MAX_HOSTVIS_SPANS so a runaway client cannot grow the map without limit.
//   Every span is released by `hostvis_free`, from exactly three places, which
//   between them cover every way one can stop being needed: `free_blob` (the
//   normal GEM_CLOSE / open-release teardown), and the two rollback paths in
//   `hostvis_map_blob` — a RESOURCE_MAP_BLOB the host refused, and a blob whose
//   record disappeared before the result could be stored.
//
// LOCK ORDER: HOSTVIS_SPANS is a leaf, like VIRTGPU_CTXS. It is never held
// across `VIRTIO_GPU.lock()`, across `BLOB_BUFFERS.lock()`, or across any
// access to user memory (the 82d0cc3 freeze class).
const HOSTVIS_GRAIN: u64 = 64 * 1024;
const MAX_HOSTVIS_SPANS: usize = 256;

/// Live reservations: offset → length, both multiples of `HOSTVIS_GRAIN`.
static HOSTVIS_SPANS: Mutex<BTreeMap<u64, u64>> = Mutex::new(BTreeMap::new());

/// Reserve `bytes` of window space in a `window_len`-byte window. Returns the
/// byte offset, or None if the window is full (or the span table is).
fn hostvis_alloc(bytes: u64, window_len: u64) -> Option<u64> {
    if bytes == 0 || window_len == 0 { return None; }
    let need = bytes.checked_add(HOSTVIS_GRAIN - 1)? & !(HOSTVIS_GRAIN - 1);
    let mut spans = HOSTVIS_SPANS.lock();
    if spans.len() >= MAX_HOSTVIS_SPANS { return None; }
    // First fit: walk the live spans in ascending order and take the first gap
    // that is big enough; `cursor` is the end of the last span considered.
    let mut cursor: u64 = 0;
    for (&off, &len) in spans.iter() {
        if off.saturating_sub(cursor) >= need { break; }
        cursor = cursor.max(off.checked_add(len)?);
    }
    if cursor.checked_add(need)? > window_len { return None; }
    spans.insert(cursor, need);
    Some(cursor)
}

/// Release a reservation previously returned by `hostvis_alloc`.
fn hostvis_free(off: u64) {
    HOSTVIS_SPANS.lock().remove(&off);
}
// ── Per-open 3D contexts ─────────────────────────────────────────────────────
//
// Linux keys the virtgpu 3D context off the open file (`struct drm_file`), so
// two processes each holding card0 open get independent contexts. This table is
// that keying: `open_id` is the VFS's opaque per-open cookie, delivered in ioctl
// slot 4. It replaces a single global context id, which two Vulkan clients
// would stomp on each other.
//
// LOCK ORDER: VIRTGPU_CTXS is a leaf. Never hold it across `VIRTIO_GPU.lock()`
// and never across a user-memory access — a demand fault taken under a spinlock
// is the 82d0cc3 all-vCPU freeze class.
const MAX_GPU_CTXS: usize = 16;

#[derive(Clone, Copy)]
struct GpuCtx {
    /// VFS open cookie; 0 marks a free slot.
    open_id: u32,
    /// Host context id from `ctx_create`.
    ctx_id: u32,
    /// VIRTGPU_CONTEXT_PARAM_CAPSET_ID the context was created with.
    capset: u32,
    /// VIRTGPU_CONTEXT_PARAM_NUM_RINGS the context was created with, clamped to
    /// at least 1. Upstream keeps this per `drm_file` for exactly one purpose:
    /// bounds-checking `drm_virtgpu_execbuffer.ring_idx`. A context that never
    /// set the param has one ring, ring 0.
    num_rings: u32,
    /// Fence id of the most recent EXECBUFFER on this open, 0 if it has never
    /// submitted. Replaces the process-global `LAST_EXEC_FENCE`.
    ///
    /// Deliberately NOT a fallback for VIRTGPU_WAIT. WAIT names a BO, and a BO
    /// that was never named in a submission has nothing outstanding — answering
    /// it from the open's last submission instead would re-create, one scope
    /// down, exactly the wrong-fence coupling that removing the global fixed.
    /// It exists so the per-open fence is observable from userspace (via
    /// `VIRTGPU_PARAM_LEANDROS_LAST_FENCE`, which is how venustest asserts the
    /// de-globalization) and so a submission naming no BOs is still accounted
    /// for somewhere rather than dropped.
    last_fence: u64,
}

impl GpuCtx {
    const fn empty() -> Self {
        Self { open_id: 0, ctx_id: 0, capset: 0, num_rings: 1, last_fence: 0 }
    }
}

static VIRTGPU_CTXS: Mutex<[GpuCtx; MAX_GPU_CTXS]> =
    Mutex::new([GpuCtx::empty(); MAX_GPU_CTXS]);

/// The whole binding for `open_id`, or None if it has no context. open_id 0
/// (an untracked caller — the legacy `Driver::handle` path) never has one.
fn ctx_lookup_entry(open_id: u32) -> Option<GpuCtx> {
    if open_id == 0 { return None; }
    let t = VIRTGPU_CTXS.lock();
    t.iter().find(|c| c.open_id == open_id).copied()
}

/// Host 3D context id bound to `open_id`, or 0 if it has none.
fn ctx_lookup(open_id: u32) -> u32 {
    ctx_lookup_entry(open_id).map(|c| c.ctx_id).unwrap_or(0)
}

/// `ctx_bind` failure. The value distinguishes the two cases:
///   * 0                — no slot left in VIRTGPU_CTXS (or `open_id` was 0,
///                        which can never own a context). Fatal for the ioctl.
///   * any other value  — a concurrent CONTEXT_INIT on the SAME open won the
///                        race; the value is the ctx_id it bound. The caller
///                        must destroy the context it just created and report
///                        success, because the open does now have a context.
const CTX_BIND_NO_SLOT: u32 = 0;

/// Return this open's context id, creating the **default** (virgl) context on
/// first use if it has none.
///
/// Upstream Linux never requires an explicit `CONTEXT_INIT`: `vfpriv` starts
/// with `context_created = false` and `virtio_gpu_create_context()` runs lazily
/// from the first ioctl that needs one, with capset 0 so the host picks its
/// default — virgl. Mesa's virgl winsys leans on that; it only issues
/// CONTEXT_INIT when it wants a *specific* capset. We required CONTEXT_INIT
/// unconditionally, which was invisible while Venus was the only consumer
/// (Venus always asks for capset 4 explicitly) and fails every classic virgl
/// client at its first EXECBUFFER.
///
/// Returns 0 only when no context could be established, which callers must
/// still treat as a refusal.
fn ctx_ensure(open_id: u32) -> u32 {
    if let Some(c) = ctx_lookup_entry(open_id) { return c.ctx_id; }
    if open_id == 0 { return 0; }

    // capset 0 = "host default". Deliberately not VIRTIO_GPU_CAPSET_VIRGL: the
    // host may prefer virgl2, and letting it choose is what upstream does.
    let ctx = {
        let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
        match guard.as_mut() {
            Some(gpu) => match gpu.ctx_create(0, "leandros-virgl") {
                Ok(c) => c,
                Err(_) => return 0,
            },
            None => return 0,
        }
    };
    match ctx_bind(open_id, ctx, 0, 1) {
        Ok(()) => ctx,
        // Lost a race: another thread on this open bound first. Its context is
        // as good as ours, so adopt it and drop the one we just made.
        Err(winner) if winner != CTX_BIND_NO_SLOT => {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            if let Some(gpu) = guard.as_mut() { gpu.ctx_destroy(ctx); }
            winner
        }
        Err(_) => {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            if let Some(gpu) = guard.as_mut() { gpu.ctx_destroy(ctx); }
            0
        }
    }
}

fn ctx_bind(open_id: u32, ctx_id: u32, capset: u32, num_rings: u32) -> Result<(), u32> {
    if open_id == 0 { return Err(CTX_BIND_NO_SLOT); }
    let mut t = VIRTGPU_CTXS.lock();
    if let Some(c) = t.iter().find(|c| c.open_id == open_id) {
        // Non-zero by construction: only a successful ctx_create gets bound.
        return Err(c.ctx_id);
    }
    match t.iter_mut().find(|c| c.open_id == 0) {
        Some(slot) => {
            *slot = GpuCtx {
                open_id,
                ctx_id,
                capset,
                num_rings: num_rings.max(1),
                last_fence: 0,
            };
            Ok(())
        }
        None => Err(CTX_BIND_NO_SLOT),
    }
}

/// The last fd on a card0 open closed: tear down that open's 3D context.
/// Called from the DRM server's VFS_CLOSE arm.
pub fn drm_release_open(open_id: u32) {
    if open_id == 0 { return; }
    // Master dies with the open that held it. Without this, a compositor that
    // exits — or crashes — while holding master leaves the node permanently
    // EBUSY for the next one, and nothing short of a reboot can present again.
    drm_master_clear(open_id);
    // Take the slot and DROP the guard before touching the device — see the
    // lock-order note above.
    let ctx = {
        let mut t = VIRTGPU_CTXS.lock();
        match t.iter_mut().find(|c| c.open_id == open_id) {
            Some(c) => { let id = c.ctx_id; *c = GpuCtx::empty(); id }
            None => 0,
        }
    };

    // Blobs this open created and never closed. A Vulkan client that exits (or
    // crashes) without GEM_CLOSE would otherwise hold its host resources — and,
    // for host-side blobs, its slice of the shared-memory window — until reboot.
    //
    // Keyed on `owner`, the open that created the BO. This used to be keyed on
    // the host context id instead, because that was the only per-open thing a
    // blob recorded — which meant a blob created BEFORE this open's
    // CONTEXT_INIT (ctx == 0) matched nothing and was leaked for the rest of the
    // boot, and an open that never called CONTEXT_INIT at all had its blobs
    // skipped wholesale. Owner-keying reaches both, and is the exact set
    // upstream frees when a `drm_file` is released.
    //
    // Note what this reclaims now that a BO is refcounted: the open's HANDLES,
    // and with them the references those handles held. An object one of them
    // named survives if — and only if — something else still holds a reference,
    // which today means an exported dmabuf fd. That is the point: a client that
    // sends a dmabuf over Wayland and then exits must not pull the buffer out
    // from under the compositor, and before this it did.
    //
    // Collect under the lock, free after dropping it: `free_blob` locks
    // BLOB_BUFFERS itself and then talks to the device.
    let orphans: Vec<u32> = {
        let map = BLOB_BUFFERS.lock();
        map.iter().filter(|(_, h)| h.owner == open_id).map(|(h, _)| *h).collect()
    };
    for h in orphans {
        DrmDeviceInterface::free_blob(h);
    }

    // Sync objects this open created and never destroyed. Same reason as the
    // blob sweep above and the same shape: upstream destroys a `drm_file`'s
    // whole syncobj idr in `drm_syncobj_release`, and without this a client
    // that exits mid-frame leaks one entry per fence it ever created — which,
    // for a v3d client, is one per submit.
    syncobj_release_open(open_id);

    // Nothing further to do for an open that never created a context — but its
    // blobs, above, still had to be reclaimed.
    if ctx == 0 { return; }

    let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
    if let Some(gpu) = guard.as_mut() {
        gpu.ctx_destroy(ctx);
    }
}

/// Record `fence` as the most recent submission on `open_id`. Silently does
/// nothing for an open with no context, which cannot have submitted anything.
fn ctx_record_fence(open_id: u32, fence: u64) {
    if open_id == 0 { return; }
    let mut t = VIRTGPU_CTXS.lock();
    if let Some(c) = t.iter_mut().find(|c| c.open_id == open_id) {
        c.last_fence = fence;
    }
}

// ── BO handle resolution, scoped to the calling open ─────────────────────────
//
// Every ioctl that consumes a BO handle — MAP, RESOURCE_INFO, WAIT, GEM_CLOSE,
// and EXECBUFFER's `bo_handles` — goes through these two helpers rather than
// indexing BLOB_BUFFERS / DUMB_BUFFERS directly, so the ownership rule is
// stated once and cannot drift between them.
//
// THE RULE (see `BlobHandle::owner`): a caller may reach a BO if it owns it, if
// the BO is unowned (owner 0), or if the caller itself has no identity
// (open_id 0 — the legacy `Driver::handle` path, which cannot be checked).
// Everything else is a miss, indistinguishable from a handle that was never
// allocated: refusing with "not yours" would leak the existence of another
// client's buffers, and upstream's per-`drm_file` tables make the two cases
// literally identical anyway.
//
// DUMB BUFFERS ARE NOT SCOPED, on purpose. A dumb handle is consumed by
// ADDFB/ADDFB2 and the framebuffer/console path, which have no open identity to
// carry, and `dumb_buffer_phys_order` resolves one globally for PRIME/dmabuf
// export (`prime_export_acquire` scopes only the blob half, which is the half a
// Vulkan client owns). Scoping them would break the compositor for no gain: the
// isolation gap that mattered is the Vulkan client's blob BOs, which are
// created and consumed on one fd by one process.
fn open_may_reach(caller: u32, owner: u32) -> bool {
    caller == 0 || owner == 0 || caller == owner
}

/// Resolve a blob BO handle for `open_id`, or None if it does not exist or
/// belongs to another open.
///
/// Two maps, taken ONE AT A TIME (see `BO LIFETIME`): the handle map answers
/// which object and whether this open may reach it, the object map answers what
/// the object is. A concurrent close between the two reads makes the object
/// lookup miss, which is reported as None — the same answer a handle closed one
/// instruction earlier gives.
fn blob_lookup(handle: u32, open_id: u32) -> Option<BlobView> {
    let h = *BLOB_BUFFERS.lock().get(&handle)?;
    if !open_may_reach(open_id, h.owner) { return None; }
    let o = *BLOB_OBJS.lock().get(&h.obj)?;
    Some(BlobView::join(h.obj, o))
}

/// Resolve a **live** dumb-buffer handle. A record whose `handle_live` is false
/// has had its gem handle retired by DESTROY_DUMB/GEM_CLOSE and survives only
/// to keep an exported dmabuf fd valid; it must resolve nowhere, so the handle
/// number is exactly as dead as it was before the refcount existed.
fn dumb_lookup(handle: u32) -> Option<DumbBuf> {
    DUMB_BUFFERS.lock().get(&handle).filter(|b| b.handle_live).copied()
}

/// The host virtio-gpu resource already bound to `handle`, if any.
///
/// ## Why this exists — a resource-id space that overlapped itself
///
/// Three framebuffer paths derived their host resource as `handle + 10`, with a
/// comment explaining it avoided the kernel console's id 1. That was true and
/// insufficient: `alloc_resource_id()` hands out 3D and blob resource ids
/// starting at **16** (`virtio_gpu.rs`), so gem handle 6 produced host resource
/// 16 — the very first id the 3D allocator returns. Two live buffers then name
/// one host resource.
///
/// It stayed invisible while the two spaces were never busy at once: dumb
/// buffers were the softpipe desktop and 3D resources were Venus, and nothing
/// ran both. Zink runs both. The symptom is a host-side refusal of a *later*
/// command on the resource — `RESOURCE_ATTACH_BACKING` answering
/// `VIRTIO_GPU_RESP_ERR_UNSPEC` — and because `VirtioGpu::submit` is a
/// synchronous busy-spin, the guest then wedges rather than reporting an error.
///
/// Both kinds now draw from the single `alloc_resource_id()` space, and a BO
/// created by `VIRTGPU_RESOURCE_CREATE` keeps the 3D resource it already owns
/// instead of having a 2D one created over the top of it.
fn fb_resource_id(handle: u32) -> Option<u32> {
    DUMB_BUFFERS.lock().get(&handle).map(|b| b.res_id).filter(|r| *r != 0)
}

/// Remember the host resource bound to `handle`, so a second ADDFB on the same
/// BO reuses it rather than allocating (and re-attaching) a second one.
fn fb_set_resource_id(handle: u32, res_id: u32) {
    if let Some(b) = DUMB_BUFFERS.lock().get_mut(&handle) { b.res_id = res_id; }
}

// ── Blob framebuffers ────────────────────────────────────────────────────────
//
// THE GAP THIS CLOSES. Both ADDFB paths resolved their handle through the
// dumb-buffer registry only, and presentation is a CPU copy of the
// framebuffer's guest pages into the console's resource 1
// (`perform_software_scaling` / `present_damaged`). A **blob** BO — the handle
// Zink's GBM path mints for every buffer it renders, via `vkGetMemoryFdKHR`
// on the render node and `PRIME_FD_TO_HANDLE` on card0 — fell through both:
// ADDFB2 built a 2D resource over it and tried to ATTACH_BACKING guest address
// 0 (the four `ctrl 0x106, error 0x1200` refusals in QEMU's stderr), and the
// present found `physical_addresses[0] == 0` and returned without touching a
// pixel. cosmic-comp under Zink/Venus therefore flipped at ~20 fps into a
// scanout that stayed black. Not a Zink problem: no path existed by which a
// buffer living in host memory could reach the display.
//
// THE MODEL, which is upstream's (`virtio_gpu_primary_plane_update`): a blob BO
// already owns a host resource, so the framebuffer records that resource and
// nothing is created or attached; a present points the scanout at the blob
// itself with SET_SCANOUT_BLOB and repaints with RESOURCE_FLUSH. QEMU exports
// the blob as a dmabuf to its display backend — zero copies, which is the
// whole point of rendering on the host GPU.
//
// A GUEST-backed blob (`phys != 0`) keeps the CPU path: it has guest pages, the
// existing copy works on them unchanged, and QEMU cannot export a guest-iov
// blob as a dmabuf anyway. `blob_res` is therefore set only when there are no
// guest pages, and `physical_addresses[0] == 0 && blob_res != 0` is the one
// test every present path makes.

/// `DRM_FORMAT_*` fourcc → `VIRTIO_GPU_FORMAT_*`, upstream's
/// `virtio_gpu_translate_format` for the formats this KMS offers. XRGB8888 is
/// the fallback because it is the only format the plane advertised before
/// blob framebuffers existed, so it is what every existing client sends.
fn virtio_format_for(fourcc: u32) -> u32 {
    use crate::virtio_gpu as vg;
    match fourcc {
        0x34325241 /* AR24 ARGB8888 */ => vg::VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
        0x34324258 /* XB24 XBGR8888 */ => vg::VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM,
        0x34324241 /* AB24 ABGR8888 */ => vg::VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM,
        _ /* XR24 XRGB8888 and anything else */ => vg::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
    }
}

/// Build the framebuffer for a blob BO, or None if `handle` names no blob this
/// open may reach (the caller then takes the dumb-buffer path unchanged).
///
/// `handles[0]` becomes the host resource id, as the 2D path does, so DIRTYFB
/// and every other consumer that reads it as a resource keep working. No lock
/// is held on return, and `VIRTIO_GPU` is never taken: there is nothing to
/// create.
fn blob_framebuffer(
    handle: u32,
    open_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    offset: u32,
    fourcc: u32,
    modifier: u64,
) -> Option<DrmFramebuffer> {
    let b = blob_lookup(handle, open_id)?;
    // A v3d BO is not a virtio-gpu resource and cannot be scanned out here.
    if b.v3d_va != 0 || b.res_handle == 0 { return None; }
    let mut fb = DrmFramebuffer::new(width, height, DrmFormat::Xrgb8888, handle, pitch);
    fb.offsets[0] = offset;
    fb.modifier = modifier;
    fb.handles[0] = b.res_handle;
    fb.physical_addresses[0] = b.phys as u64;
    fb.virtio_format = virtio_format_for(fourcc);
    if b.phys == 0 { fb.blob_res = b.res_handle; }
    if DRM_STATS {
        crate::pci::serial_debug("[DRM] blob framebuffer: handle=");
        crate::pci::serial_debug_hex(handle);
        crate::pci::serial_debug(" res=");
        crate::pci::serial_debug_hex(b.res_handle);
        crate::pci::serial_debug(" fourcc=");
        crate::pci::serial_debug_hex(fourcc);
        crate::pci::serial_debug(" pitch=");
        crate::pci::serial_debug_hex(pitch);
        crate::pci::serial_debug(" mod_lo=");
        crate::pci::serial_debug_hex(modifier as u32);
        crate::pci::serial_debug(" guest_phys=");
        crate::pci::serial_debug_hex(b.phys as u32);
        crate::pci::serial_debug("\n");
    }
    Some(fb)
}

/// Present `fb_id` if it is a host-memory blob framebuffer: SET_SCANOUT_BLOB
/// when the scanout is not already on this resource, then RESOURCE_FLUSH over
/// the damage (the whole surface when `rects` is None).
///
/// Returns None when the framebuffer is not a blob one, and the caller falls
/// through to the CPU-copy present exactly as before. Called with the DRM
/// device lock held; `VIRTIO_GPU` nests inside it in the same order
/// `atomic_commit` already uses.
///
/// Every successful present bumps `SCANOUT_WRITES`, because that counter is
/// how `handle_ioctl` learns a DRM client has taken the surface and silences
/// the console (`drm_scanout_claim`). Without it the console's next dirty
/// rectangle would `flush(1, ..)` and yank the scanout back to resource 1.
fn present_blob_fb(
    device: &mut DrmDevice,
    fb_id: DrmObjectId,
    rects: Option<&[(i32, i32, i32, i32)]>,
) -> Option<Result<(), DriverError>> {
    let (res, w, h, format, stride, offset) = {
        let fb = device.get_framebuffer(fb_id)?;
        if fb.blob_res == 0 || fb.physical_addresses[0] != 0 { return None; }
        (fb.blob_res, fb.width, fb.height, fb.virtio_format, fb.pitches[0], fb.offsets[0])
    };
    if w == 0 || h == 0 { return Some(Err(DriverError::InvalidParameter)); }

    // Damage union in framebuffer coordinates, clamped; an empty list after
    // clamping still owes the scanout switch (the buffer may be new) but no
    // flush.
    let (mut x0, mut y0, mut x1, mut y1) = (0u32, 0u32, w, h);
    if let Some(clips) = rects {
        let (mut ux0, mut uy0, mut ux1, mut uy1) = (w, h, 0u32, 0u32);
        for &(a, b, c, d) in clips {
            let sx0 = a.max(0) as u32;
            let sy0 = b.max(0) as u32;
            let sx1 = (c.max(0) as u32).min(w);
            let sy1 = (d.max(0) as u32).min(h);
            if sx0 >= sx1 || sy0 >= sy1 { continue; }
            if sx0 < ux0 { ux0 = sx0; }
            if sy0 < uy0 { uy0 = sy0; }
            if sx1 > ux1 { ux1 = sx1; }
            if sy1 > uy1 { uy1 = sy1; }
        }
        (x0, y0, x1, y1) = (ux0, uy0, ux1, uy1);
    }

    let r = {
        let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
        let gpu = match guard.as_mut() {
            Some(g) => g,
            None => return Some(Err(DriverError::NotFound)),
        };
        let mut switched = false;
        if gpu.current_scanout() != res {
            if !gpu.set_scanout_blob(res, w, h, format, stride, offset) {
                crate::pci::serial_debug("[DRM] SET_SCANOUT_BLOB refused res=");
                crate::pci::serial_debug_hex(res);
                crate::pci::serial_debug("\n");
                return Some(Err(DriverError::Io));
            }
            switched = true;
        }
        // A fresh scanout binding is displayed by the host on its own; a flush
        // is still what tells it which rectangle changed, so send one whenever
        // there is damage or the binding just moved.
        if x0 < x1 && y0 < y1 {
            if !gpu.resource_flush(res, x0, y0, x1 - x0, y1 - y0) {
                crate::pci::serial_debug("[DRM] blob RESOURCE_FLUSH refused res=");
                crate::pci::serial_debug_hex(res);
                crate::pci::serial_debug("\n");
                return Some(Err(DriverError::Io));
            }
        } else if switched {
            let _ = gpu.resource_flush(res, 0, 0, w, h);
        }
        Ok(())
    };

    crate::drm::device::SCANOUT_WRITES.fetch_add(1, Ordering::Relaxed);
    if let Some(plane) = device.planes.first_mut() {
        plane.fb_id = Some(fb_id);
    }
    Some(r)
}

/// Does `handle` name a BO this open may reach, of either kind? Upstream's
/// `drm_gem_object_lookup` miss, which EXECBUFFER answers -ENOENT to.
///
/// The maps are locked one at a time, never nested: they are leaves and
/// keeping them independent is what makes that true by construction.
fn bo_exists(handle: u32, open_id: u32) -> bool {
    if blob_lookup(handle, open_id).is_some() {
        return true;
    }
    dumb_lookup(handle).is_some()
}

/// The fence of the work most recently submitted against `handle`, or None if
/// the handle names no BO this open may reach. `Some(0)` is the meaningful
/// answer "this BO exists and nothing has ever been submitted against it".
fn bo_fence(handle: u32, open_id: u32) -> Option<u64> {
    if let Some(b) = blob_lookup(handle, open_id) {
        return Some(b.last_fence);
    }
    dumb_lookup(handle).map(|b| b.last_fence)
}

/// Attach `fence` to `handle`. False if the BO went away between validation and
/// submission (a concurrent GEM_CLOSE), which is benign — a closed BO is one
/// nothing can wait on.
fn bo_attach_fence(handle: u32, open_id: u32, fence: u64) -> bool {
    // Resolve handle → object under the handle map, then write the fence under
    // the object map. Never both at once (see `BO LIFETIME`). The fence lives on
    // the object because a submission fences the buffer, not the name for it.
    let obj = {
        let blobs = BLOB_BUFFERS.lock();
        match blobs.get(&handle) {
            Some(h) if open_may_reach(open_id, h.owner) => Some(h.obj),
            Some(_) => return false, // exists, but not this open's
            None => None,
        }
    };
    if let Some(obj) = obj {
        return match BLOB_OBJS.lock().get_mut(&obj) {
            Some(o) => { o.last_fence = fence; true }
            None => false,
        };
    }
    match DUMB_BUFFERS.lock().get_mut(&handle) {
        Some(b) if b.handle_live => { b.last_fence = fence; true }
        _ => false,
    }
}

// ── Reference counting (see `BO LIFETIME`) ───────────────────────────────────

/// Drop one reference on blob object `obj`, and detach `detach_ctx` from the
/// host resource on the way out — that is the dropping HANDLE's own context
/// binding, and 0 for a dmabuf-fd reference, which has no context.
///
/// Returns false if `obj` names no blob object, which lets the shared entry
/// point (`bo_release_exported`) try the dumb registry before deciding an id is
/// bogus. The decrement and the removal happen under ONE acquisition of
/// `BLOB_OBJS`, so two droppers racing cannot both observe zero and both tear
/// the resource down — a double `resource_unref` plus a double
/// `mm::buddy::free` of an order-N block is allocator corruption, which is the
/// `9be954f` class.
fn blob_unref(obj: u32, detach_ctx: u32) -> bool {
    let mut m = BLOB_OBJS.lock();
    let (res_handle, zero) = match m.get_mut(&obj) {
        Some(o) => {
            o.refs = o.refs.saturating_sub(1);
            (o.res_handle, o.refs == 0)
        }
        None => return false,
    };
    let dead = if zero { m.remove(&obj) } else { None };
    drop(m); // never hold a BO map across the device round-trip

    // Nothing to say to the device: an fd reference (`detach_ctx == 0`) going
    // away while other references remain is pure bookkeeping. Skipping the lock
    // matters because this is now the compositor's per-frame dmabuf-close path.
    //
    // `res_handle != 0` is the second reason to say nothing, and it is the one
    // a v3d BO takes: an object with no host virtio-gpu resource has nothing to
    // unmap, detach or unref, and issuing `resource_unref(0)` would name the
    // host's resource id 0. Written as a property of the object rather than as
    // "is this v3d", because it is true of any object with no host resource and
    // stays true if another kind ever appears. Every virtgpu blob has a
    // non-zero `res_handle` (`alloc_resource_id()` starts at 16), so this
    // changes nothing on that path.
    if res_handle != 0 && (detach_ctx != 0 || dead.is_some()) {
        let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
        if let Some(gpu) = guard.as_mut() {
            // UNMAP before UNREF: the host holds the window sub-region on behalf
            // of a live resource, and unreferencing it first leaves the
            // subregion attached to a resource that no longer exists. The
            // detach sits between them, exactly where it sat before the object
            // and the handle were separated.
            if let Some(o) = dead.as_ref() {
                if o.map_phys != 0 {
                    gpu.resource_unmap_blob(o.res_handle);
                }
            }
            if detach_ctx != 0 {
                gpu.ctx_detach_resource(detach_ctx, res_handle);
            }
            if dead.is_some() {
                gpu.resource_unref(res_handle);
            }
        }
    }

    if let Some(o) = dead {
        // Return the window space unconditionally once the record is gone —
        // including when UNMAP_BLOB failed or the device had vanished. The
        // record is what `hostvis_free` is reachable from, so holding the
        // reservation back would leak it for the rest of the boot with nothing
        // left able to release it. Reusing an offset the host (wrongly) still
        // believes in fails closed rather than corrupts: the host refuses to map
        // a second resource over a live sub-region, so the next
        // RESOURCE_MAP_BLOB at that offset is rejected and rolls itself back.
        if o.map_phys != 0 {
            hostvis_free(o.win_off);
        }
        // The GPU address space, on the same "the record is what it is
        // reachable from" principle as the window reservation above: once this
        // record is gone nothing else can return the span, so it is returned
        // here, unconditionally, from the one place the object dies.
        if o.v3d_va != 0 {
            v3d_va_free(o.v3d_va);
        }
        if o.phys != 0 {
            mm::buddy::free(o.phys, o.order);
        }
        let _ = o.size;
    }
    true
}

/// Drop one reference on the dumb BO carrying object id `obj`. Returns false if
/// no dumb record carries it. The scan is over a map that holds a handful of
/// entries (one per live scanout buffer), and it runs only on dmabuf-fd
/// release, never on a per-frame path.
fn dumb_unref_by_obj(obj: u32) -> bool {
    let mut m = DUMB_BUFFERS.lock();
    let handle = match m.iter().find(|(_, b)| b.obj == obj) {
        Some((h, _)) => *h,
        None => return false,
    };
    let zero = match m.get_mut(&handle) {
        Some(b) => {
            b.refs = b.refs.saturating_sub(1);
            b.refs == 0
        }
        None => return false,
    };
    let dead = if zero { m.remove(&handle) } else { None };
    drop(m);
    if let Some(b) = dead {
        mm::buddy::free(b.phys, b.order);
    }
    true
}

/// **The VFS release hook.** One exporting `TmpVmo` slot has gone away; drop the
/// reference it held on BO object `obj`.
///
/// Registered as a function pointer from `servers/drm`'s `init` rather than
/// called directly, because `vfs-server` does not depend on `drivers` and must
/// not start to. It is invoked with **no tmpfs lock held** — see the lock-order
/// note on `vfs_server::set_dmabuf_release`. A null registration (headless
/// build, no DRM device) is a no-op, which is correct: nothing can have
/// exported.
///
/// Reaching neither registry is the underflow signal: an id was released twice,
/// or an id was invented. It is logged rather than ignored because the guard
/// test asserts on the absence of this line.
pub fn bo_release_exported(obj: u32) {
    if obj == 0 {
        return;
    }
    if blob_unref(obj, 0) {
        return;
    }
    if dumb_unref_by_obj(obj) {
        return;
    }
    crate::pci::serial_debug("[DRM] bo refcount underflow obj=");
    crate::pci::serial_debug_hex(obj);
    crate::pci::serial_debug("\n");
}

/// How many blob objects are live right now. Backs
/// `VIRTGPU_PARAM_LEANDROS_BLOB_OBJS`, which is what makes the refcount
/// assertable from userspace at all.
fn blob_obj_count() -> u32 {
    BLOB_OBJS.lock().len() as u32
}

/// Cache type the host asked us to use for the host-visible blob whose mmap
/// token covers `phys` (VIRTIO_GPU_MAP_CACHE_*), or VIRTIO_GPU_MAP_CACHE_CACHED
/// when `phys` is not inside one.
///
/// `sys_mmap`'s DynamicDevice arm asks this AFTER `handle_ioctl_mmap` has
/// validated the token, so an address this device never handed out cannot reach
/// here; answering CACHED for one is the conservative reading anyway, and is
/// exactly right for the two token spaces that carry no `map_info` at all —
/// dumb buffers and guest-backed blobs, which are guest RAM and coherent by
/// construction.
///
/// Containment, not equality, deliberately: `handle_ioctl_mmap` accepts any
/// address inside a blob's reservation so that a partial map of a large blob
/// works, and the cache type has to follow the same rule or a partial map would
/// silently come out with the wrong attributes.
///
/// LOCKING: takes BLOB_BUFFERS and nothing else, touches no user memory, and is
/// called with no other lock held — the 82d0cc3 discipline.
pub fn blob_map_cache_type(phys: u64) -> u32 {
    if phys == 0 { return crate::virtio_gpu::VIRTIO_GPU_MAP_CACHE_CACHED; }
    // Over the OBJECTS, not the handles: `map_phys`/`size`/`map_info` describe
    // the host mapping, which belongs to the buffer rather than to any one gem
    // handle naming it. Iterating handles would visit a shared object once per
    // handle and, once import mints a second handle, could disagree with itself.
    let blobs = BLOB_OBJS.lock();
    for b in blobs.values() {
        if b.map_phys != 0 && phys >= b.map_phys && phys - b.map_phys < b.size {
            return b.map_info & crate::virtio_gpu::VIRTIO_GPU_MAP_CACHE_MASK;
        }
        // A v3d BO is guest RAM, so the paragraph above would call it CACHED —
        // and that is exactly the wrong answer. On real V3D the GPU is not
        // coherent with the CPU caches: upstream maps every v3d BO write-combine
        // (`v3d_bo.c` sets `pgprot_writecombine`), and Mesa relies on it, writing
        // command lists and vertex data through the mapping with no flush of its
        // own. Answering CACHED here would work under QEMU — where the "GPU"
        // is this stub and reads nothing — and then fail on the Pi in the way
        // that is hardest to see, as sporadically wrong geometry rather than as
        // an error. The stub therefore reports what the hardware needs, so the
        // mapping attributes are settled before the hardware arrives.
        //
        // Same containment test as above, and against `phys`/`size` rather than
        // `map_phys`: a v3d BO's mmap token IS its guest-physical base.
        if b.v3d_va != 0
            && b.phys != 0
            && phys >= (b.phys as u64)
            && phys - (b.phys as u64) < b.size
        {
            return crate::virtio_gpu::VIRTIO_GPU_MAP_CACHE_WC;
        }
    }
    crate::virtio_gpu::VIRTIO_GPU_MAP_CACHE_CACHED
}

// ── Property blobs ───────────────────────────────────────────────────────────
// Atomic modesetting passes modes (and damage clips) by blob id rather than by
// value. Blobs are opaque byte strings the client creates once and references
// from later commits, so they must outlive the ioctl that created them.
static BLOBS: Mutex<BTreeMap<u32, Vec<u8>>> = Mutex::new(BTreeMap::new());
static NEXT_BLOB_ID: AtomicU32 = AtomicU32::new(0x1000);

/// True once a client has taken DRM_CLIENT_CAP_ATOMIC. The legacy handlers stay
/// live either way; this only records which contract the client is using.
static ATOMIC_CLIENT: AtomicBool = AtomicBool::new(false);

/// Whether the compositor is driving us through the atomic path.
pub fn atomic_client() -> bool {
    ATOMIC_CLIENT.load(Ordering::Relaxed)
}

/// Framebuffer id whose pixels are currently loaded into the host cursor
/// resource. A commit naming this same id needs no upload.
static LAST_CURSOR_FB: AtomicU32 = AtomicU32::new(0);
static CURSOR_UPDATES: AtomicU64 = AtomicU64::new(0);
static CURSOR_MOVES: AtomicU64 = AtomicU64::new(0);
/// Atomic commits actually applied (TEST_ONLY validations are not counted).
static ATOMIC_COMMITS: AtomicU64 = AtomicU64::new(0);
/// TEST_ONLY validations. smithay issues these constantly while probing which
/// plane assignments are possible.
static ATOMIC_TESTS: AtomicU64 = AtomicU64::new(0);
/// Requests (test or real) that named the cursor plane at all. If this stays
/// zero the compositor never even tried the cursor plane, which is a very
/// different problem from the plane being tried and rejected.
static CURSOR_PLANE_SEEN: AtomicU64 = AtomicU64::new(0);

/// FB_DAMAGE_CLIPS accounting, all behind `DRM_STATS`.
///
/// These exist to settle a question no counter we had could answer. smithay
/// keeps a skipped plane *in* the atomic request (`build_planes` filters on
/// `!state.skip || state.config.is_some()`, and the skip branch clones the
/// previous config verbatim), so "smithay decided the primary has no damage"
/// and "smithay re-rendered the whole screen" arrive here as the same shaped
/// ioctl. What tells them apart is the clip list:
///
/// * `dmg_full` climbing at the commit rate, `dmg_px` ~ W*H per present ⇒ the
///   compositor is damaging the entire output every frame. The blocker is in
///   its own damage tracker (buffer age / element history), not in us.
/// * `dmg_rect` climbing with a small `dmg_px` ⇒ damage tracking works and this
///   change is doing its job.
/// * `dmg_skip` climbing ⇒ smithay is already skipping the primary plane and
///   the recorded premise ("smithay still flips the primary every cursor
///   frame") was an artefact of counting one flip per atomic commit.
static DAMAGE_FULL: AtomicU64 = AtomicU64::new(0);
static DAMAGE_RECT: AtomicU64 = AtomicU64::new(0);
static DAMAGE_SKIP: AtomicU64 = AtomicU64::new(0);
static DAMAGE_PX: AtomicU64 = AtomicU64::new(0);
static BLOBS_CREATED: AtomicU64 = AtomicU64::new(0);

/// Primary-plane state of the last commit we actually presented.
///
/// When smithay's damage tracker reports nothing to draw it sets
/// `plane_state.skip` and copies the *previous* frame's plane config over the
/// new one (`compositor/mod.rs:2320-2324`). The clip list lives behind an `Arc`
/// on its side, so the copy keeps the same blob id. An incoming commit whose
/// FB_ID **and** clip-blob id both match the last one we presented is therefore
/// that exact case, and there is nothing new in the framebuffer to copy.
///
/// Blob id 0 means "no clip list", which is not a fingerprint — never skip on
/// it.
static LAST_PRIMARY_FB: AtomicU32 = AtomicU32::new(0);
static LAST_PRIMARY_DAMAGE: AtomicU32 = AtomicU32::new(0);

/// Past this many clips the per-rect bookkeeping and the widening union cost
/// more than the full-surface copy they exist to avoid, so fall back to full.
const MAX_DAMAGE_RECTS: usize = 64;

// Committed crtc/connector state. Only used to decide whether an incoming
// atomic request needs ALLOW_MODESET.
static CRTC_ACTIVE: AtomicU32 = AtomicU32::new(0);
static CRTC_MODE_BLOB: AtomicU32 = AtomicU32::new(0);
static CONN_CRTC: AtomicU32 = AtomicU32::new(0);

/// One plane's worth of an atomic request. Every field is `None` when the
/// commit did not mention that property — "unchanged", not "zero".
#[derive(Default, Clone, Copy)]
struct AtomicPlaneReq {
    crtc_id: Option<u32>,
    fb_id: Option<u32>,
    src_x: Option<u32>,
    src_y: Option<u32>,
    src_w: Option<u32>,
    src_h: Option<u32>,
    crtc_x: Option<i32>,
    crtc_y: Option<i32>,
    crtc_w: Option<u32>,
    crtc_h: Option<u32>,
    damage_blob: Option<u32>,
}

/// Decode an FB_DAMAGE_CLIPS blob into half-open source rects.
///
/// The blob is an array of `struct drm_mode_rect { __s32 x1, y1, x2, y2; }` in
/// framebuffer coordinates. `None` means "treat the whole surface as damaged" —
/// the upstream meaning of an absent clip list and the only safe answer to a
/// blob we cannot use. It is deliberately never an error: FB_DAMAGE_CLIPS is a
/// hint, and rejecting the commit over one would stall the compositor.
///
/// Takes BLOBS and nothing else, touches no user memory, and must be called
/// with the DRM device lock NOT held — the two are never nested in either
/// direction.
fn damage_rects(blob_id: u32) -> Option<Vec<(i32, i32, i32, i32)>> {
    if blob_id == 0 { return None; }
    let bytes = {
        let blobs = BLOBS.lock();
        let b = blobs.get(&blob_id)?;
        if b.is_empty() || b.len() % 16 != 0 || b.len() > MAX_DAMAGE_RECTS * 16 {
            return None;
        }
        b.clone()
    };
    let mut out = Vec::with_capacity(bytes.len() / 16);
    for c in bytes.chunks_exact(16) {
        let rd = |o: usize| i32::from_ne_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]]);
        out.push((rd(0), rd(4), rd(8), rd(12)));
    }
    Some(out)
}

/// Total area of a clip list, saturating. Only used for the `DRM_STATS` line —
/// it is what says "the compositor damaged 3 000 pixels" versus "it damaged the
/// whole 1 024 000-pixel output and the clip list is decoration".
fn damage_area(rects: &[(i32, i32, i32, i32)]) -> u64 {
    let mut total = 0u64;
    for &(x1, y1, x2, y2) in rects {
        let w = (x2 as i64 - x1 as i64).max(0) as u64;
        let h = (y2 as i64 - y1 as i64).max(0) as u64;
        total = total.saturating_add(w.saturating_mul(h));
    }
    total
}

/// Resolve a dumb-buffer GEM handle to its physical base + buddy order, so the
/// syscall layer can build a PRIME/dmabuf fd whose backing frames ARE this
/// buffer's contiguous pages (`phys .. phys + (1<<order)*4096`). Returns None
/// for an unknown handle. Copy-out to user memory happens in the syscall layer,
/// never here (this only reads the kernel-side registry).
pub fn dumb_buffer_phys_order(handle: u32) -> Option<(usize, usize)> {
    dumb_lookup(handle).map(|b| (b.phys, b.order))
}

/// What backs a GEM handle for PRIME/dmabuf export.
///
/// `phys == 0` is a real and expected state, not an error: a pure
/// `BLOB_MEM_HOST3D` blob lives in HOST memory and the guest owns no pages for
/// it at all. It reaches that memory, if it ever does, through the virtio-gpu
/// shared-memory BAR window after RESOURCE_MAP_BLOB — deliberately never
/// through the direct map. An fd exported for such a BO is therefore a
/// shareable token, not a mapping; see `install_dmabuf_vmo`.
#[derive(Clone, Copy)]
pub struct PrimeExport {
    /// Physical base of the contiguous buddy block backing the BO, or 0 when
    /// there are no guest pages.
    pub phys: usize,
    /// Buddy order of that block. Meaningless when `phys == 0`.
    pub order: usize,
    /// Bytes the exported fd reports through fstat/lseek. Mesa's kms_swrast
    /// PRIME importer takes `lseek(fd, 0, SEEK_END)` as the buffer size and
    /// gives up entirely if it fails, so for a blob this is the resource's own
    /// size rather than the buddy allocation's power-of-two rounding.
    pub len: usize,
    /// **Lifetime identity of the BO, and the reference this call took on it.**
    /// The fd must remember THIS and never a gem handle: a gem handle is
    /// retired by DESTROY_DUMB/GEM_CLOSE while the fd is still open, and for a
    /// blob it is per-open, so it cannot be what keeps a buffer alive. Hand it
    /// to `vfs::install_dmabuf_vmo`; if the export fails after this point, hand
    /// it to `bo_release_exported` instead. Never 0 on a successful call.
    pub obj: u32,
}

/// Resolve a GEM handle for PRIME/dmabuf export, of EITHER BO kind, **and take
/// one reference on the object** for the fd that is about to be built.
///
/// The PRIME intercept used to call `dumb_buffer_phys_order` directly, so it
/// answered EINVAL for every Venus blob. That one gap gated `vkGetMemoryFdKHR`,
/// which Mesa's WSI calls for EVERY swapchain image on every DRM-image path
/// (`wsi_create_native_image_mem` -> `wsi_init_image_dmabuf_fd`) and once more
/// as a feature probe (`wsi_drm_check_dma_buf_sync_file_import_export`, on a
/// 4 KiB device-local allocation) — which is why offscreen rendering works
/// today while no WSI surface can be created at all.
///
/// THE REFERENCE IS TAKEN HERE, not in the VFS, because here is the only place
/// that holds the object map and can do it atomically with the resolution. The
/// caller owns it from the moment this returns `Some` and **must** either hand
/// it to `install_dmabuf_vmo` (which transfers it to the `TmpVmo` slot) or
/// release it with `bo_release_exported`. That is why the name says `acquire`:
/// the old `prime_export_backing` was a pure query and this is not.
///
/// SCOPING follows the two registries' existing rules rather than inventing a
/// third: a blob is reachable only by the open that created it (`blob_lookup` /
/// `open_may_reach`), and a dumb buffer stays deliberately global. `None` means
/// "no BO this open may reach", indistinguishable from a handle that was never
/// allocated — the same answer upstream's per-`drm_file` table gives, and it
/// also covers the object having been torn down between the handle read and the
/// object read.
///
/// Copy-out to user memory happens in the syscall layer, never here; this only
/// touches the kernel-side registries, and never holds two at once.
pub fn prime_export_acquire(handle: u32, open_id: u32) -> Option<PrimeExport> {
    let obj = {
        let map = BLOB_BUFFERS.lock();
        match map.get(&handle) {
            Some(h) if open_may_reach(open_id, h.owner) => Some(h.obj),
            // A blob handle this open may not reach is a refusal outright, not
            // a fall-through to the dumb registry: the two handle spaces are
            // disjoint, so falling through could only ever mis-resolve.
            Some(_) => return None,
            None => None,
        }
    };
    if let Some(obj) = obj {
        let mut objs = BLOB_OBJS.lock();
        let o = objs.get_mut(&obj)?;
        o.refs = o.refs.saturating_add(1);
        return Some(PrimeExport { phys: o.phys, order: o.order, len: o.size as usize, obj });
    }
    // Dumb exports keep reporting the buddy allocation, byte-for-byte what they
    // reported before this function existed: GBM/EGL fstat the exported fd and
    // the compositor has been running against that number since 36f62d0.
    let mut dumb = DUMB_BUFFERS.lock();
    let b = dumb.get_mut(&handle).filter(|b| b.handle_live)?;
    b.refs = b.refs.saturating_add(1);
    Some(PrimeExport {
        phys: b.phys,
        order: b.order,
        len: (1usize << b.order) * 4096,
        obj: b.obj,
    })
}

/// PRIME_FD_TO_HANDLE for a **blob** object: mint a gem handle that the
/// IMPORTING open may reach, or return the one it already holds on this object.
///
/// WHY. Handles are scoped to the open that created them (`BlobHandle::owner`,
/// `open_may_reach`). PRIME_FD_TO_HANDLE used to echo the *exporter's* handle
/// across the fd, so the very next thing an importer does with it —
/// `VIRTGPU_RESOURCE_INFO`, which Mesa's Venus backend issues in
/// `virtgpu_bo_create_from_dma_buf` — resolved to "not yours" and was refused
/// as an unknown handle. Under Zink that is the compositor importing a client's
/// `zwp_linux_dmabuf_v1` buffer: cosmic-comp answered cosmic-panel's
/// `create_immed` with a protocol error and the panel died on every start.
///
/// WHAT UPSTREAM DOES, which this mirrors:
///   * `drm_gem_prime_fd_to_handle` gives each `drm_file` its own handle on the
///     shared object, and returns the *existing* handle if that file already
///     imported (or created) the object — hence the dedup scan first, which also
///     makes a self-import (Mesa re-importing its own export) answer with the
///     original handle;
///   * `virtio_gpu_gem_object_open` then sends CTX_ATTACH_RESOURCE for the
///     importer's context, because the host renderer's per-context resource
///     table is what `vkAllocateMemory` with a resource import consults — a
///     Venus context can only import a resource it has been told about.
///
/// The new handle is one reference on the object (`BO LIFETIME`), released
/// by GEM_CLOSE like any other, and it records the importer's context so the
/// close detaches exactly what this attach attached.
///
/// Returns `None` if `obj` names no live blob object — a dumb-buffer export,
/// whose handles are deliberately global (see `open_may_reach`), or a buffer
/// torn down between the fd read and here — and the caller falls back to the
/// exporter's handle, which for a dumb buffer is the right answer.
pub fn prime_import_blob(obj: u32, open_id: u32) -> Option<u32> {
    if open_id == 0 { return None; } // no identity: every handle is reachable anyway
    {
        let map = BLOB_BUFFERS.lock();
        if let Some((h, _)) = map.iter().find(|(_, h)| h.obj == obj && h.owner == open_id) {
            return Some(*h);
        }
    }
    // Take the handle's reference under ONE acquisition of the object map.
    let res_handle = {
        let mut objs = BLOB_OBJS.lock();
        let o = objs.get_mut(&obj)?;
        o.refs = o.refs.saturating_add(1);
        o.res_handle
    };
    let ctx = ctx_lookup(open_id);
    if ctx != 0 && res_handle != 0 {
        let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
        if let Some(gpu) = guard.as_mut() {
            // Logged, not propagated — same contract as RESOURCE_CREATE_BLOB's
            // attach: upstream issues it for effect and returns 0 regardless.
            if !gpu.ctx_attach_resource(ctx, res_handle) {
                crate::pci::serial_debug("[DRM] PRIME import: CTX_ATTACH_RESOURCE refused ctx=");
                crate::pci::serial_debug_hex(ctx);
                crate::pci::serial_debug(" res=");
                crate::pci::serial_debug_hex(res_handle);
                crate::pci::serial_debug("\n");
            }
        }
    }
    let handle = NEXT_BLOB_HANDLE.fetch_add(1, Ordering::Relaxed);
    BLOB_BUFFERS.lock().insert(handle, BlobHandle { obj, owner: open_id, ctx });
    Some(handle)
}

// ── DRM sync objects ─────────────────────────────────────────────────────────
//
// A syncobj is a *named, per-drm_file container for a fence*. That indirection
// is the whole point: a client creates the handle, hands it to a submission as
// `out_sync`, and waits on it later — the fence inside is replaced each time
// the handle is submitted against. v3d's `drm_v3d_submit_cl` takes three of
// them (`in_sync_bcl`, `in_sync_rcl`, `out_sync`) and has no simulate path.
//
// THE SHORTCUT NOT TAKEN. The virtgpu out-fence eventfd in this tree is
// signalled *at creation* (TODO.md:3251). That is only correct because
// `VirtioGpu::submit` is a synchronous busy-spin, so every fence it hands out
// is already retired when the ioctl returns. V3D submission is asynchronous by
// construction, so a syncobj here carries a real fence id and a real,
// separately-published retirement, and nothing in this section declares a
// syncobj signalled merely because it was created or submitted against.
//
// THE FENCE MODEL, and how it stays honest with only two words of state:
//   * `fence == 0`               — NULL fence. Upstream's "no fence yet".
//   * `signaled == true`         — an explicitly signalled stub fence
//                                  (CREATE_SIGNALED / SIGNAL), or a retirement
//                                  already folded in.
//   * `fence != 0 && !signaled`  — a real pending fence, signalled iff it has
//                                  been retired.
// Retirement is published as a **lock-free watermark** (`SYNCOBJ_FENCE_DONE`),
// not by walking the table, so `syncobj_retire_fence` is callable straight from
// a GPU completion ISR: it is one `fetch_max` plus a try-wake, takes no lock at
// all, and therefore cannot be the 82d0cc3 shape (a lock ordered against
// RUN_QUEUE taken with IRQs off). The cost of a watermark is that it assumes
// fence ids retire in issue order, which holds for a single in-order job queue
// — v3d's bin and render queues each are one — and is why the retire entry
// point is documented as per-queue-monotonic rather than arbitrary.
//
// LOCK ORDER. `SYNCOBJS` is a leaf, exactly like `BLOB_OBJS`/`BLOB_BUFFERS`:
// nothing else is ever locked while it is held, no user memory is touched while
// it is held, and it is never held across a `VIRTIO_GPU` round trip or across
// a `yield`. The WAIT path in particular copies the caller's handle array into
// a kernel `Vec` *before* it locks anything and blocks with the lock dropped.

/// One DRM sync object. Binary only — see `syncobj_timeline_stub`.
#[derive(Clone, Copy)]
struct SyncObj {
    /// The `open_id` that may name this syncobj — upstream gives each
    /// `drm_file` its own syncobj idr, and this is the same owner-tag
    /// flattening `BlobHandle::owner` uses, tested through the same
    /// `open_may_reach` so there is one ownership rule in this file and not
    /// two that can drift.
    owner: u32,
    /// Explicitly signalled. RESET clears it, SIGNAL and CREATE_SIGNALED set
    /// it, and a retired `fence` folds into it lazily via `syncobj_is_signaled`
    /// rather than eagerly, so retirement costs nothing per syncobj.
    signaled: bool,
    /// The fence this syncobj currently holds; 0 is upstream's NULL fence, and
    /// is what makes a WAIT without `WAIT_FOR_SUBMIT` return EINVAL.
    fence: u64,
    /// Live references. One per handle. The second reference a
    /// `HANDLE_TO_FD`-exported sync file would hold does not exist yet (that
    /// ioctl is ENOSYS — there is no fd-minting seam inside this crate), so the
    /// count can only be 1 today. It is here so that adding fd export is a
    /// refcount change and not a lifetime redesign, which is the lesson
    /// `BO LIFETIME` above was paid for.
    refs: u32,
}

/// Syncobj handles. A namespace of its own, disjoint from GEM handles by
/// definition (upstream keeps a separate idr per `drm_file`), so this starts at
/// 1 and does not have to dodge `NEXT_BLOB_HANDLE`.
static SYNCOBJS: Mutex<BTreeMap<u32, SyncObj>> = Mutex::new(BTreeMap::new());
static NEXT_SYNCOBJ_HANDLE: AtomicU32 = AtomicU32::new(1);

/// Highest retired fence id. Lock-free so an ISR can publish into it. See
/// "THE FENCE MODEL" above for the in-order assumption this encodes.
static SYNCOBJ_FENCE_DONE: AtomicU64 = AtomicU64::new(0);

/// Tasks currently parked inside `syncobj_handle_wait`.
///
/// Load-bearing for idle CPU, not diagnostic: `SIGNAL` and fence retirement
/// wake the *global* poll wait-channel, and `idletest` guards the property that
/// an idle system issues no wakes at all. Gating every wake on this counter
/// means a syncobj that nobody is waiting on costs one relaxed load and wakes
/// nothing.
static SYNCOBJ_WAITERS: AtomicUsize = AtomicUsize::new(0);

/// `SYNCOBJ_FENCE_DONE` as of the last wake `drm_tick` issued for it. The
/// backstop for a retirement whose `try_wake_poll` lost the RUN_QUEUE race:
/// `try_wake_poll` is allowed to defer, and unlike the flip queue there is
/// nothing that would naturally retry, so the tick does.
static SYNCOBJ_WAKE_SEEN: AtomicU64 = AtomicU64::new(0);

/// The most a single WAIT/RESET/SIGNAL may name. Upstream has no explicit cap
/// (it fails at `kmalloc_array`); a bound is cheaper than trusting a `u32`
/// count from userspace to be sane, and real callers use single digits.
const SYNCOBJ_MAX_HANDLES: u32 = 4096;

/// Is this syncobj signalled *right now*? `done` is a hoisted snapshot of
/// `SYNCOBJ_FENCE_DONE` so one probe pass answers consistently for every
/// handle in the array rather than re-reading a moving watermark per entry.
fn syncobj_is_signaled(s: &SyncObj, done: u64) -> bool {
    s.signaled || (s.fence != 0 && s.fence <= done)
}

/// Wake syncobj waiters, if there are any. Task context.
fn syncobj_wake() {
    if SYNCOBJ_WAITERS.load(Ordering::Relaxed) != 0 {
        // No lock held here, by construction — every caller drops SYNCOBJS
        // first. `wake_poll` takes RUN_QUEUE.
        sched::wake_poll();
    }
}

/// Drop one reference on `handle`, removing it at zero. Takes the map the
/// caller already holds: this is called from teardown loops and from DESTROY,
/// and re-locking inside would be a nested acquisition of a leaf lock.
fn syncobj_put(map: &mut BTreeMap<u32, SyncObj>, handle: u32) {
    if let Some(s) = map.get_mut(&handle) {
        s.refs = s.refs.saturating_sub(1);
        if s.refs == 0 { map.remove(&handle); }
    }
}

/// Copy a user array of `count` `u32` syncobj handles into kernel memory.
///
/// **Done before any lock is taken, always.** This is the exact hazard
/// `82d0cc3` records: the array is ordinary user memory and may be paged out,
/// so touching it takes a demand-paging fault, and a demand-paging fault under
/// an IRQ-off spinlock re-enters the scheduler lock and freezes every vCPU with
/// no panic. A raw (unaligned) read rather than `read_user_buf` is deliberate
/// and matches every other arm in this file: the DRM server's ioctl path runs
/// **synchronously on the caller's thread in the caller's address space**, so a
/// fault here is a normal, serviceable fault, whereas `read_user_buf` walks the
/// page tables by hand and simply fails on a non-resident page.
fn syncobj_read_handles(ptr: u64, count: u32) -> Result<Vec<u32>, DriverError> {
    if ptr == 0 || count == 0 || count > SYNCOBJ_MAX_HANDLES {
        return Err(DriverError::InvalidParameter);
    }
    let mut v = vec![0u32; count as usize];
    for i in 0..count as usize {
        v[i] = unsafe { ptr::read_unaligned((ptr as *const u32).add(i)) };
    }
    Ok(v)
}

/// Absolute-nanosecond timeout to an absolute tick deadline.
///
/// `None` means "do not block at all" — upstream's `drm_timeout_abs_to_jiffies`
/// returns 0 for a zero/negative timeout *and* for one already in the past, and
/// a zero timeout is documented there as "make 0 timeout mean poll". Rounding
/// **up** to the next tick is what keeps a sub-tick timeout from degenerating
/// into a busy-poll loop that never sleeps; the same truncation-to-zero bug in
/// `nanosleep` (`fb398c7`) is on record in this tree.
///
/// The clock is `arch_monotonic_ns`, which is literally the same
/// `timer::monotonic_ns` that backs `sys_clock_gettime(CLOCK_MONOTONIC)` — the
/// clock userspace computed the absolute deadline from. If those two ever
/// diverge, every syncobj wait either times out instantly or hangs forever.
fn syncobj_deadline_ticks(timeout_nsec: i64) -> Option<u64> {
    if timeout_nsec <= 0 { return None; }
    let abs = timeout_nsec as u64;
    let now = unsafe { arch_monotonic_ns() };
    if abs <= now { return None; }
    // 100 Hz tick.
    let rel_ticks = (abs - now) / 10_000_000 + 1;
    Some(sched::ticks().checked_add(rel_ticks).unwrap_or(u64::MAX))
}

/// One evaluation of a whole WAIT predicate.
///
/// `Ok(Some(i))` — satisfied, `i` is the index of the first signalled handle
/// (upstream sets `first_signaled` to that index regardless of `WAIT_ALL`).
/// `Ok(None)`    — not yet; the caller may block.
/// `Err(..)`     — permanent refusal, exactly as upstream orders the checks:
///                 an unreachable handle is ENOENT (`drm_syncobj_array_find`),
///                 and a NULL fence without `WAIT_FOR_SUBMIT` is EINVAL.
///
/// Holds `SYNCOBJS` for the whole pass and touches no user memory inside it.
fn syncobj_probe(
    handles: &[u32],
    open_id: u32,
    wait_all: bool,
    for_submit: bool,
) -> Result<Option<u32>, DriverError> {
    let done = SYNCOBJ_FENCE_DONE.load(Ordering::Acquire);
    let map = SYNCOBJS.lock();
    let mut first: Option<u32> = None;
    let mut all = true;
    for (i, &h) in handles.iter().enumerate() {
        let s = match map.get(&h) {
            Some(s) if open_may_reach(open_id, s.owner) => s,
            // Unknown, or another open's. Indistinguishable on purpose — see
            // the note on `open_may_reach`.
            _ => return Err(DriverError::NotFound),
        };
        if syncobj_is_signaled(s, done) {
            if first.is_none() { first = Some(i as u32); }
        } else {
            if s.fence == 0 && !for_submit {
                // Upstream refuses the whole call rather than waiting forever
                // on a container that nothing has submitted into.
                return Err(DriverError::InvalidParameter);
            }
            all = false;
        }
    }
    if wait_all {
        Ok(if all { Some(first.unwrap_or(0)) } else { None })
    } else {
        Ok(first)
    }
}

/// The last fd on a card0/renderD128 open closed: destroy that open's
/// syncobjs. Called from `drm_release_open`, same shape as the blob sweep it
/// sits next to — collect the keys, then drop the references, all under one
/// acquisition of the leaf map and with no device round trip inside it.
fn syncobj_release_open(open_id: u32) {
    let mut map = SYNCOBJS.lock();
    let dead: Vec<u32> = map
        .iter()
        .filter(|(_, s)| s.owner == open_id)
        .map(|(h, _)| *h)
        .collect();
    for h in dead { syncobj_put(&mut map, h); }
}

// ── The V3D submit seam ──────────────────────────────────────────────────────
//
// These three are the entire interface a `drm_v3d_submit_cl` implementation
// needs from this section. They are `pub` and unused inside this crate today,
// which is the point: the syncobj contract is settled and testable *before* the
// GPU driver that consumes it exists, rather than being re-derived under a live
// client.

/// Read the fence a syncobj currently holds — v3d's `in_sync_bcl`/`in_sync_rcl`
/// dependency lookup. `Some(0)` means "this syncobj exists and holds no fence",
/// which is a satisfied dependency, not an error. `None` means the handle names
/// nothing this open may reach (answer ENOENT).
pub fn syncobj_fence(handle: u32, open_id: u32) -> Option<u64> {
    let map = SYNCOBJS.lock();
    let s = map.get(&handle)?;
    if !open_may_reach(open_id, s.owner) { return None; }
    if s.signaled { return Some(0); }
    Some(s.fence)
}

/// Install `fence` as a syncobj's pending fence — v3d's `out_sync` at submit
/// time. Clears any previous signalled state: the container now tracks *this*
/// submission and must not report the previous one's completion.
///
/// False if the handle names nothing this open may reach. Never declares the
/// fence retired, however fast the submission actually is.
pub fn syncobj_attach_fence(handle: u32, open_id: u32, fence: u64) -> bool {
    let mut map = SYNCOBJS.lock();
    match map.get_mut(&handle) {
        Some(s) if open_may_reach(open_id, s.owner) => {
            s.fence = fence;
            s.signaled = false;
            true
        }
        _ => false,
    }
}

/// Publish "every fence up to and including `fence` has retired".
///
/// **Callable from a GPU completion ISR**: one `fetch_max` and one non-blocking
/// wake, no lock of any kind, so it cannot take a lock ordered against
/// RUN_QUEUE with IRQs off. `fence` must come from a single in-order queue (see
/// "THE FENCE MODEL"); calling it with an out-of-order id would signal earlier
/// fences that have not actually completed.
///
/// The wake is best-effort — `try_wake_poll` defers under RUN_QUEUE contention
/// — and `drm_tick` retries any deferred wake within one tick.
pub fn syncobj_retire_fence(fence: u64) {
    if fence == 0 { return; }
    SYNCOBJ_FENCE_DONE.fetch_max(fence, Ordering::Release);
    if SYNCOBJ_WAITERS.load(Ordering::Relaxed) != 0 {
        sched::try_wake_poll();
    }
}

/// Tick backstop for a deferred `syncobj_retire_fence` wake. Two relaxed loads
/// and nothing else when no task is waiting, which is why it is safe to sit
/// ahead of `drm_tick`'s throttle return without touching idle CPU.
fn syncobj_tick() {
    if SYNCOBJ_WAITERS.load(Ordering::Relaxed) == 0 { return; }
    let done = SYNCOBJ_FENCE_DONE.load(Ordering::Relaxed);
    if SYNCOBJ_WAKE_SEEN.load(Ordering::Relaxed) != done {
        SYNCOBJ_WAKE_SEEN.store(done, Ordering::Relaxed);
        sched::try_wake_poll();
    }
}

// ── V3D (Broadcom VideoCore VI/VII 3D core) ──────────────────────────────────
//
// WHY THIS EXISTS. The Raspberry Pi 5's GPU is a `v3d` DRM device, and Mesa's
// `v3d` gallium driver talks to it through the UAPI below and nothing else.
// QEMU models no V3D on any machine type, so none of this can be exercised
// against a device — but roughly half of it is pure DECODE (request codes,
// struct layouts, parameter values, handle/VA bookkeeping, the Mesa loader
// handshake), and that half is exactly the half that is expensive to debug with
// a Pi on the desk. The stub backend below closes it under QEMU.
//
// WHAT IS REAL AND WHAT IS STUBBED, stated once so nothing here is mistaken for
// a driver:
//   REAL — GET_PARAM (fixed, self-consistent V3D 7.1 identity), CREATE_BO
//          (buddy-backed guest pages + a GPU VA), MMAP_BO, GET_BO_OFFSET,
//          WAIT_BO, GEM_CLOSE, and SUBMIT_CL's *decode*: BO-list validation,
//          in-sync resolution, fence allocation, out-sync attach, deferred
//          retirement.
//   STUBBED — there is no GPU. SUBMIT_CL executes nothing; the command lists
//          are not even read. SUBMIT_TFU / SUBMIT_CSD / PERFMON_* are ENOSYS.
//
// REQUEST CODES ARE DERIVED, NOT GUESSED — this file's convention (see the
// syncobj block above; the two codes that were ever guessed cost real
// sessions). `_IOC(dir, type, nr, size) = dir<<30 | size<<16 | type<<8 | nr`,
// DRM's type is 'd' = 0x64, `nr = DRM_COMMAND_BASE (0x40) + index`, _IOWR is
// dir 3 => 0xC000_0000 and _IOW is dir 1 => 0x4000_0000. Struct sizes are from
// Mesa's vendored `include/drm-uapi/v3d_drm.h` and were checked by compiling
// that header's declarations and printing `sizeof`, not by eye:
//
//  idx  nr    ioctl                struct / fields                       size  code
//  ───  ────  ───────────────────  ────────────────────────────────────  ────  ──────────
//  0x00 0x40  SUBMIT_CL            10*u32 (bcl/rcl/sync/qma/qms/qts),
//                                  u64 bo_handles, 4*u32, u64 exts        72   0xC0486440
//  0x01 0x41  WAIT_BO              u32 handle, u32 pad, u64 timeout_ns    16   0xC0106441
//  0x02 0x42  CREATE_BO            u32 size, flags, handle, offset        16   0xC0106442
//  0x03 0x43  MMAP_BO              u32 handle, u32 flags, u64 offset      16   0xC0106443
//  0x04 0x44  GET_PARAM            u32 param, u32 pad, u64 value          16   0xC0106444
//  0x05 0x45  GET_BO_OFFSET        u32 handle, u32 offset                  8   0xC0086445
//  0x06 0x46  SUBMIT_TFU (_IOW)    7*u32, u32 coef[4], u32 bo_handles[4],
//                                  3*u32, u64 exts, {u32 ioc, u32 pad}    88   0x40586446
//  0x07 0x47  SUBMIT_CSD (_IOW)    u32 cfg[7], u32 coef[4], (pad 4),
//                                  u64 bo_handles, 4*u32, u64 exts, 2*u32 88   0x40586447
//  0x08 0x48  PERFMON_CREATE       u32 id, u32 ncounters, u8 c[32]        40   0xC0286448
//  0x09 0x49  PERFMON_DESTROY      u32 id                                  4   0xC0046449
//  0x0a 0x4a  PERFMON_GET_VALUES   u32 id, u32 pad, u64 values_ptr        16   0xC010644A
//  0x0b 0x4b  SUBMIT_CPU (_IOW)    u64 bo_handles, 2*u32, u64 exts        24   0x4018644B
//  0x0c 0x4c  PERFMON_GET_COUNTER  u8 counter + name[64] + category[32]
//                                  + description[256] + reserved[7]      360   0xC168644C
//  0x0d 0x4d  PERFMON_SET_GLOBAL   u32 flags, u32 id            (_IOW)     8   0x4008644D
//
// Two of those sizes are the ones eyeballing gets wrong, so they are called
// out: SUBMIT_CSD has **four bytes of tail padding after `coef[4]`** to align
// the `u64 bo_handles` that follows (44 -> 48), and PERFMON_GET_COUNTER is a
// byte-aligned 360, not a rounded 368.
const DRM_IOCTL_V3D_SUBMIT_CL: u32 = 0xC0486440;
const DRM_IOCTL_V3D_WAIT_BO: u32 = 0xC0106441;
const DRM_IOCTL_V3D_CREATE_BO: u32 = 0xC0106442;
const DRM_IOCTL_V3D_MMAP_BO: u32 = 0xC0106443;
const DRM_IOCTL_V3D_GET_PARAM: u32 = 0xC0106444;
const DRM_IOCTL_V3D_GET_BO_OFFSET: u32 = 0xC0086445;
const DRM_IOCTL_V3D_SUBMIT_TFU: u32 = 0x40586446;
const DRM_IOCTL_V3D_SUBMIT_CSD: u32 = 0x40586447;
const DRM_IOCTL_V3D_PERFMON_CREATE: u32 = 0xC0286448;
const DRM_IOCTL_V3D_PERFMON_DESTROY: u32 = 0xC0046449;
const DRM_IOCTL_V3D_PERFMON_GET_VALUES: u32 = 0xC010644A;
const DRM_IOCTL_V3D_SUBMIT_CPU: u32 = 0x4018644B;
const DRM_IOCTL_V3D_PERFMON_GET_COUNTER: u32 = 0xC168644C;
const DRM_IOCTL_V3D_PERFMON_SET_GLOBAL: u32 = 0x4008644D;

/// True for every `DRM_IOCTL_V3D_*` request code. Derived from the encoded `nr`
/// (0x40..0x4D on DRM's type 'd') rather than from a hand-kept list, exactly as
/// `is_syncobj_ioctl` is, so a code added above cannot be forgotten here.
///
/// Used by the DRM server to pick the v3d errno table. `nr` alone is not enough
/// to identify a driver-private ioctl in general — `DRM_COMMAND_BASE + n` means
/// something different for every driver — but this device answers exactly one
/// driver ABI at a time, and the whole 0x40+ block is v3d's while the v3d
/// backend is active and virtgpu's otherwise. `virtgpu`'s codes are 0x40..0x4C
/// too, so the backend flag is what disambiguates, not the number.
pub fn is_v3d_ioctl(cmd: u32) -> bool {
    if !v3d_active() { return false; }
    if (cmd >> 8) & 0xFF != 0x64 { return false; }
    matches!(cmd & 0xFF, 0x40..=0x4D)
}

// `enum drm_v3d_param` — the ordinal IS the wire value, so these are written as
// the ordinals they are. 0x00..0x0C are the block this driver answers; 0x0D
// (MAX_PERF_COUNTERS), 0x0E (SUPPORTS_SUPER_PAGES), 0x0F (GLOBAL_RESET_COUNTER)
// and 0x10 (CONTEXT_RESET_COUNTER) were appended by later kernels and are
// answered too, because `v3d_get_device_info` reads two of them.
const V3D_PARAM_UIFCFG: u32 = 0;
const V3D_PARAM_HUB_IDENT1: u32 = 1;
const V3D_PARAM_HUB_IDENT2: u32 = 2;
const V3D_PARAM_HUB_IDENT3: u32 = 3;
const V3D_PARAM_CORE0_IDENT0: u32 = 4;
const V3D_PARAM_CORE0_IDENT1: u32 = 5;
const V3D_PARAM_CORE0_IDENT2: u32 = 6;
const V3D_PARAM_SUPPORTS_TFU: u32 = 7;
const V3D_PARAM_SUPPORTS_CSD: u32 = 8;
const V3D_PARAM_SUPPORTS_CACHE_FLUSH: u32 = 9;
const V3D_PARAM_SUPPORTS_PERFMON: u32 = 10;
const V3D_PARAM_SUPPORTS_MULTISYNC_EXT: u32 = 11;
const V3D_PARAM_SUPPORTS_CPU_QUEUE: u32 = 12;
const V3D_PARAM_MAX_PERF_COUNTERS: u32 = 13;
const V3D_PARAM_SUPPORTS_SUPER_PAGES: u32 = 14;
const V3D_PARAM_GLOBAL_RESET_COUNTER: u32 = 15;
const V3D_PARAM_CONTEXT_RESET_COUNTER: u32 = 16;

// THE IDENTITY. Mesa decodes these three registers and refuses any version it
// does not compile support for, so they are not decoration — they select the
// code path for every shader Mesa will ever emit against this device
// (`v3d_device_info.c:32`, `v3d_screen.c:796`):
//
//   ver         = ((IDENT0 >> 24) & 0xff) * 10 + (IDENT1 & 0xf)
//   vpm_size    = ((IDENT1 >> 28) & 0xf) * 8192
//   qpu_count   = ((IDENT1 >> 4) & 0xf) * ((IDENT1 >> 8) & 0xf)   // nslc * qups
//   rev         =  (HUB_IDENT3 >> 8)  & 0xff
//   compat_rev  =  (HUB_IDENT3 >> 16) & 0xff
//
// and `ver` must come out 42 or 71 — anything else is
// "V3D %d.%d not supported by this version of Mesa" and a NULL screen. The Pi 5
// is 7.1, so:
//
//   IDENT0 = 0x07443356 -> major 7. The low three bytes are the core's ASCII
//            tag, 'V' 0x56, '3' 0x33, 'D' 0x44, in ascending byte order exactly
//            as the hardware lays it out. Nothing in Mesa reads them; they are
//            written truthfully rather than zeroed so a serial dump of this
//            register looks like the register it claims to be.
//   IDENT1 = 0x20000421 -> minor 1 (=> ver 71), nslc 2, qups 4 => 8 QPUs,
//            VPM nibble 2 => 16384 bytes. 8 QPUs is the documented Pi 4/Pi 5
//            core width; VPM 16 KiB is the V3D 4.x/7.x figure. Both feed real
//            arithmetic in the compiler (`vir.c:2454` asserts vpm_size > 0 and
//            divides by a sector size; `v3d_program.c:542` sizes the spill BO
//            as qpu_count * 4 * ...), so a zero in either is a divide-by-zero
//            or a zero-sized allocation later, not a cosmetic wrong number.
//   HUB_IDENT3 = 0x00040600 -> rev 6, compat_rev 4. Not arbitrary: Mesa's
//            `v3d_device_info.h:105` branches on exactly `rev == 6 &&
//            compat_rev >= 4`, which is a real silicon point, and several
//            workarounds key on `ver == 71 && rev < 6`. Claiming rev 6 selects
//            the same paths a Pi 5 does, which is the whole point of a stub
//            that exists to de-risk the hardware phase.
const V3D_IDENT0: u64 = 0x0744_3356;
const V3D_IDENT1: u64 = 0x2000_0421;
const V3D_HUB_IDENT3: u64 = 0x0004_0600;
/// 1 core, hub revision 1, TVER 7. Read by Linux's debugfs and by nothing in
/// Mesa; answered rather than refused so a probe of the whole param block does
/// not have to special-case which ids exist.
const V3D_HUB_IDENT1: u64 = 0x0700_0101;

/// **Zero on purpose, and load-bearing.** `v3d_get_device_info` stores this in
/// `devinfo->max_perfcnt`, and `v3dx_counter.c:41` then reads the performance
/// counter *descriptions* one of two ways: through
/// `DRM_IOCTL_V3D_PERFMON_GET_COUNTER` when `max_perfcnt` is non-zero, and from
/// Mesa's own compiled-in table when it is zero. `v3d_perfcntrs_init` runs
/// during `v3d_screen_create` and a NULL return there is a hard `goto fail`, so
/// advertising a non-zero count while PERFMON_GET_COUNTER answers ENOSYS would
/// fail screen creation outright. Zero is also the truth: there are no counters.
const V3D_MAX_PERF_COUNTERS: u64 = 0;

// ── Backend selection ────────────────────────────────────────────────────────
//
// THE CONSTRAINT. On a QEMU/virtio build every byte of today's behaviour must
// be unchanged unless the v3d backend is explicitly asked for — the same node
// answers virtgpu (Venus, virgl, Zink) and its `DRM_IOCTL_VERSION` identity is
// what Mesa's loader picks a driver by, so a backend that guessed wrong would
// silently swap the whole userspace stack.
//
// THE MECHANISM: one atomic, default off, armed through
// `DRM_IOCTL_SET_CLIENT_CAP` with a private capability id.
//
// Why SET_CLIENT_CAP and not a new ioctl: it is an *existing* entry point that
// already exists to let a client declare which contract it wants (that is
// literally what DRM_CLIENT_CAP_ATOMIC does here), it is reachable from
// userspace with no new plumbing, its handler already ignores unknown ids so
// adding one cannot break an existing caller, and it needs no edit to
// `drivers/src/lib.rs` (owned by another lane). A cargo feature was rejected
// for the opposite reason: it cannot be toggled inside one boot, so the
// A/B evidence this lane exists to produce — virtgpu behaviour and v3d
// behaviour from the same image — would need two builds and two boots.
//
// Why nothing auto-detects: the honest auto-rule is "this is a Pi 5", and this
// crate has no board identity that is not owned by another lane right now.
// "There is no virtio-GPU" is a tempting proxy and a wrong one — it is also
// true of every headless QEMU run. When the real Pi 5 v3d driver lands it
// should call `v3d_set_enabled(true)` from its own probe; that is one line and
// it belongs to the code that actually found the hardware.
//
// SCOPE: device-global, not per-open, deliberately. `DRM_IOCTL_VERSION` has no
// open identity to key on at the seam that answers it for the render node
// (`servers/drm/src/lib.rs`), and the whole point of the flag is to change the
// identity Mesa reads *before* it has created anything. Arming it is a
// statement about the device, so it reads as one.
const DRM_CLIENT_CAP_LEANDROS_V3D: u64 = 0x1000_0003;

static V3D_ENABLED: AtomicBool = AtomicBool::new(false);

/// v3d request codes already reported by the first-of-each-kind trace in
/// `v3d_dispatch`. Its own set, separate from `UNKNOWN_IOCTLS`: that one is a
/// 32-entry cap shared with every unimplemented ioctl in the driver, and a
/// noisy boot could evict the v3d entries from it.
static V3D_FIRST_SEEN: Mutex<NoteSet> = Mutex::new(NoteSet::new());

/// Is the v3d backend the one answering for this device right now?
#[inline]
pub fn v3d_active() -> bool {
    V3D_ENABLED.load(Ordering::Relaxed)
}

/// Arm or disarm the v3d backend. Called from `SET_CLIENT_CAP`, and intended to
/// be called from a real Pi 5 GPU probe once one exists.
pub fn v3d_set_enabled(on: bool) {
    V3D_ENABLED.store(on, Ordering::Relaxed);
}

// ── GPU virtual address space ────────────────────────────────────────────────
//
// Every v3d BO has an address in the GPU's own 32-bit space, handed back by
// CREATE_BO (`offset`) and re-readable with GET_BO_OFFSET. It is NOT the CPU
// mmap token — MMAP_BO answers that separately — and on real hardware it is
// what the MMU page tables are keyed by. Mesa bakes it straight into command
// lists and shader uniforms, so it has to be stable for the life of the handle.
//
// Same shape as `hostvis_alloc` above (first-fit over a `BTreeMap<u64, u64>` of
// live spans in a bounded window) and for the same reason: a real allocator
// rather than a bump pointer, so a client that churns BOs reuses low addresses
// instead of walking off the end of the window.
//
// GRAIN 4 KiB — the V3D MMU's page size, so every BO starts on a page the MMU
// can map independently.
// WINDOW 0x1000 .. 0x1_0000_0000 — the full 32-bit space **minus page zero**.
// `drm_v3d_create_bo.offset` is a u32, so the top is exclusive and the largest
// address this can hand out is 0xFFFF_F000, which still fits. Leaving page 0
// out is what makes a null GPU pointer invalid, and it is also what upstream
// promises in the UAPI header: "This offset value will always be nonzero, since
// various HW units treat 0 specially."
//
// LOCK ORDER: a leaf, exactly like HOSTVIS_SPANS. Never held across a device
// round-trip, across another BO map, or across any access to user memory.
const V3D_VA_GRAIN: u64 = 4096;
const V3D_VA_BASE: u64 = 0x1000;
const V3D_VA_TOP: u64 = 0x1_0000_0000;
const MAX_V3D_VA_SPANS: usize = 1024;

static V3D_VA_SPANS: Mutex<BTreeMap<u64, u64>> = Mutex::new(BTreeMap::new());

/// Reserve `bytes` of GPU address space. Returns the base address, or None when
/// the window (or the span table) is full.
fn v3d_va_alloc(bytes: u64) -> Option<u64> {
    if bytes == 0 { return None; }
    let need = bytes.checked_add(V3D_VA_GRAIN - 1)? & !(V3D_VA_GRAIN - 1);
    let mut spans = V3D_VA_SPANS.lock();
    if spans.len() >= MAX_V3D_VA_SPANS { return None; }
    // First fit, ascending. `cursor` starts at V3D_VA_BASE rather than 0, which
    // is the only difference from `hostvis_alloc`: page 0 is never a candidate,
    // so it cannot be handed out even when the map is empty.
    let mut cursor: u64 = V3D_VA_BASE;
    for (&off, &len) in spans.iter() {
        if off.saturating_sub(cursor) >= need { break; }
        cursor = cursor.max(off.checked_add(len)?);
    }
    if cursor.checked_add(need)? > V3D_VA_TOP { return None; }
    spans.insert(cursor, need);
    Some(cursor)
}

/// Release a reservation previously returned by `v3d_va_alloc`.
fn v3d_va_free(va: u64) {
    if va == 0 { return; }
    V3D_VA_SPANS.lock().remove(&va);
}

// ── The stub submit queue ────────────────────────────────────────────────────
//
// THE THING THIS EXISTS NOT TO DO. `TODO.md:3251` records that virtgpu signals
// its EXECBUFFER out-fence at *creation* time. That is defensible there only
// because `VirtioGpu::submit` is a synchronous busy-spin, so the work really is
// finished before the ioctl returns. It is not defensible here and it is not
// what this does: a stub that retires its fence inside the submit ioctl exposes
// a fence that is never once observably outstanding, and every consumer built
// against it — Mesa's `v3d_fence`, WAIT_BO, SYNCOBJ_WAIT — gets tested against
// a shape that real hardware will not have. The asynchrony is most of what
// there is to get right, so the stub models it.
//
// HOW IT DEFERS. `drm_tick` is already registered as a 100 Hz scheduler tick
// hook (`servers/drm`'s `init`) and is already the thing that paces page-flip
// completions for exactly this reason. A submitted fence is queued here with a
// due tick of `now + 1` and retired by the tick hook, so it is outstanding for
// at least one full tick (~10 ms) and userspace can observe it as outstanding.
// The tick hook is the closest structural analogue this kernel has to the GPU
// completion interrupt that will replace it: it runs in interrupt-driven
// context, it is not the submitting thread, and it retires in issue order.
//
// IN-ORDER, WHICH THE WATERMARK REQUIRES. `syncobj_retire_fence` publishes a
// high-water mark, so fence ids must be per-queue monotonic and must retire in
// issue order or a later fence would signal earlier ones that have not
// finished. One queue, one counter, one FIFO, drained from the front only —
// and `V3D_FENCE_NEXT` is never reused within a boot, so a stale id resolves to
// "long since retired" rather than to somebody else's work.
//
// IDLE COST. `V3D_INFLIGHT_N` is the reason `drm_tick` can consult this with a
// single relaxed load when nothing is in flight. `idletest` asserts that an
// idle system issues no wakes at all, and taking a `Mutex` per tick to discover
// an empty queue would put a lock acquisition on the idle path for no reason.
/// How many 100 Hz ticks a stub submission stays outstanding.
///
/// TWO, not one, and the reason is a race rather than a preference. Retirement
/// fires when `sched::ticks() >= due`, so `due = now + 1` means "at the next
/// tick edge" — which can be 10 ms away or 10 µs away, depending on where in
/// the tick the submit landed. A test that submits and then immediately polls
/// the out-fence, expecting to find it outstanding, would fail whenever the
/// submit happened to land just before an edge: rare, timing-dependent, and
/// indistinguishable from a real regression. `now + 2` puts a full tick under
/// the floor, so the fence is outstanding for at least 10 ms however the submit
/// is aligned. Latency is not a property a stub has to optimise.
const V3D_STUB_LATENCY_TICKS: u64 = 2;

/// Next fence id. Monotonic, never reused, starts at 1 because 0 is
/// "no fence" everywhere in this file.
static V3D_FENCE_NEXT: AtomicU64 = AtomicU64::new(1);

/// (fence, due tick), oldest first. Drained only from the front.
static V3D_INFLIGHT: Mutex<VecDeque<(u64, u64)>> = Mutex::new(VecDeque::new());

/// `V3D_INFLIGHT.len()`, readable without the lock. See IDLE COST above.
static V3D_INFLIGHT_N: AtomicUsize = AtomicUsize::new(0);

/// Bound on outstanding stub submissions. A client that submits faster than the
/// tick retires would otherwise grow the queue without limit; refusing at the
/// cap is the same back-pressure a real ring gives.
const MAX_V3D_INFLIGHT: usize = 256;

/// Allocate a fence id and queue it for deferred retirement. None when too much
/// is already outstanding (answer EBUSY).
fn v3d_submit_fence() -> Option<u64> {
    let mut q = V3D_INFLIGHT.lock();
    if q.len() >= MAX_V3D_INFLIGHT { return None; }
    let fence = V3D_FENCE_NEXT.fetch_add(1, Ordering::Relaxed);
    q.push_back((fence, sched::ticks().wrapping_add(V3D_STUB_LATENCY_TICKS)));
    V3D_INFLIGHT_N.store(q.len(), Ordering::Relaxed);
    Some(fence)
}

/// Retire every stub submission whose deferral has elapsed. Called from
/// `drm_tick`; the stand-in for a GPU completion ISR.
///
/// Retires strictly from the front, so the watermark
/// `syncobj_retire_fence` publishes never skips an unfinished fence. The `>=`
/// comparison is on a monotonic tick counter, and the queue is short, so this
/// is a couple of compares per tick in the worst case.
fn v3d_tick() {
    if V3D_INFLIGHT_N.load(Ordering::Relaxed) == 0 { return; }
    let now = sched::ticks();
    let mut highest = 0u64;
    {
        // try_lock, not lock: this runs in tick context and a submitting thread
        // may hold the queue. Missing a tick costs 10 ms of extra latency on a
        // stub; spinning in a tick hook costs the machine.
        let mut q = match V3D_INFLIGHT.try_lock() { Some(g) => g, None => return };
        while let Some(&(fence, due)) = q.front() {
            if now < due { break; }
            q.pop_front();
            highest = fence;
        }
        V3D_INFLIGHT_N.store(q.len(), Ordering::Relaxed);
    }
    // Outside the lock: `syncobj_retire_fence` wakes pollers, and a wake taken
    // under a leaf lock held in tick context is the 82d0cc3 ordering hazard.
    // One call with the highest id retires everything below it — that is what a
    // watermark means — so a burst costs one wake, not one per fence.
    if highest != 0 { syncobj_retire_fence(highest); }
}

/// Has `fence` retired? Reads the same watermark `syncobj_is_signaled` does, so
/// WAIT_BO and SYNCOBJ_WAIT can never disagree about one fence.
fn v3d_fence_done(fence: u64) -> bool {
    fence == 0 || fence <= SYNCOBJ_FENCE_DONE.load(Ordering::Acquire)
}

// ── DRM page-flip event channel ──────────────────────────────────────────────
// PAGE_FLIP-with-event completions are NOT delivered instantly: doing so lets a
// compositor's render loop resubmit with zero delay and peg the CPU (there is no
// real vblank here). Instead they queue in PENDING_FLIPS and `drm_tick()` — a
// 100 Hz tick hook — promotes at most one per ~vblank window into READY_EVENTS,
// which read()/poll() on the card fd drain. This gives Smithay/kmscube a stable
// frame cadence and keeps idle CPU at zero (idletest guards it).
static PENDING_FLIPS: Mutex<VecDeque<[u8; 32]>> = Mutex::new(VecDeque::new());
static READY_EVENTS:  Mutex<VecDeque<[u8; 32]>> = Mutex::new(VecDeque::new());
static FLIP_SEQUENCE: AtomicU32 = AtomicU32::new(0);
static LAST_FLIP_DELIVER_TICK: AtomicU64 = AtomicU64::new(0);

/// Frame-pipeline instrumentation for cursor-latency triage: one line every
/// 2 s with page-flips submitted/delivered, DIRTYFB calls, and cumulative time
/// inside the flip path. Off by default — it writes to the UART straight from
/// the tick, bypassing CONSOLE_OUT_LOCK. Flip to `true` to re-measure.
///
/// What it established (aarch64, 1280x800, softpipe): under 60 pointer moves/s
/// the compositor submits **0.9 page flips/s**, every submitted flip is
/// delivered (so the 50 Hz `drm_tick` throttle below is nowhere near binding),
/// DIRTYFB is never used, and the kernel's own scale+flush costs only ~1.7 ms
/// per flip. The ~1 fps pointer is therefore the compositor recompositing the
/// whole screen in software, not anything in this path.
/// ON while the Zink/Venus input-stall investigation is open: the freeze that
/// follows a keystroke is a STALL with a catch-up burst after it, not a uniform
/// slowdown, and only the counters below can say which stage stops. Costs one
/// serial line every 200 ticks (2 s) plus a clock read per page flip and per
/// control-queue command. Set back to `false` once that is settled.
pub const DRM_STATS: bool = false;
static FLIPS_SUBMITTED: AtomicU64 = AtomicU64::new(0);
static DIRTYFB_CALLS: AtomicU64 = AtomicU64::new(0);
static DIRTYFB_CLIPS: AtomicU64 = AtomicU64::new(0);
/// Cumulative microseconds spent inside the page-flip path (software scale +
/// full-screen virtio-gpu transfer). Tells apart "our flush is the bottleneck"
/// from "the compositor's softpipe recomposite is".
static FLIP_US_TOTAL: AtomicU64 = AtomicU64::new(0);
static LAST_STAT_TICK: AtomicU64 = AtomicU64::new(0);
/// Advances each time an event becomes readable — epoll's edge emulation reads
/// this as the card fd's readiness sequence (see VFS handle_poll seq contract).
static DELIVERED_SEQ: AtomicU64 = AtomicU64::new(0);

/// Monotonic readiness sequence for the card fd (epoll edge emulation).
pub fn drm_event_seq() -> u64 {
    DELIVERED_SEQ.load(Ordering::Relaxed)
}

extern "C" {
    /// Monotonic nanoseconds since boot, sub-tick resolution, never decreasing —
    /// forwarded from the arch crate (`drivers` sits below it in the dependency
    /// graph and cannot call `timer::monotonic_ns` directly; same symbol
    /// `evdev-server` uses for input event timestamps).
    fn arch_monotonic_ns() -> u64;
}

/// Queue a FLIP_COMPLETE event for throttled delivery. Called from the PAGE_FLIP
/// ioctl (syscall context — a normal lock is fine; `drm_tick` uses try_lock).
fn queue_flip_event(crtc_id: u32, user_data: u64) {
    let seq = FLIP_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    // Sub-tick-resolution timestamp (interpolated inside the current 100 Hz
    // tick from the arch's free-running counter) rather than the raw tick
    // count — smithay reads this for presentation feedback, and a 10 ms-
    // granular clock reports an identical time for every flip in the same
    // tick.
    let now_ns = unsafe { arch_monotonic_ns() };
    let ev = drm_event_vblank {
        ev_type: DRM_EVENT_FLIP_COMPLETE,
        length: 32,
        user_data,
        tv_sec: (now_ns / 1_000_000_000) as u32,
        tv_usec: ((now_ns / 1_000) % 1_000_000) as u32,
        sequence: seq,
        crtc_id,
    };
    let mut blob = [0u8; 32];
    unsafe { ptr::copy_nonoverlapping(&ev as *const _ as *const u8, blob.as_mut_ptr(), 32); }
    PENDING_FLIPS.lock().push_back(blob);
}

/// Live-object census for the `[DRMSTAT]` line: `(dumb, dumb_retired, blob_objs,
/// blob_handles)`.
///
/// DERIVED, NOT MAINTAINED. The maps *are* the census, so the numbers come from
/// `.len()` (and one filtered count) rather than from increment/decrement sites.
/// A hand-kept counter can only drift from the maps it claims to describe; this
/// one cannot.
///
/// READING IT:
///   * `bo_bhnd` sustained above `bo_blob` — more gem handles than objects —
///     is a **handle leak**: handles are being minted and never retired.
///   * `bo_blob` above `bo_bhnd` is the healthy converse: objects outliving
///     their handles because an exported dmabuf fd still pins them.
///   * `bo_dumbret` climbing monotonically is the **retention leak**: dumb
///     records retired as handles but never dropped by their last exporting fd.
///   * `bo_dumb` must be bounded over a session. Unbounded growth there means
///     handles minted by an import path are outliving their release — the
///     defect `65fb20c` fixed, reappearing. Note the expected magnitude is
///     per *exported buffer*, not per composited frame: a 185 s census of a
///     live session recorded 5 dumb creates, 5 PRIME exports and 1 free, then
///     froze, because cosmic-comp exports at allocation and not per frame.
///
/// LOCKING — the whole reason this is a separate function. `drm_tick` runs in
/// **IRQ context at 100 Hz**. A blocking `.lock()` here deadlocks the instant
/// the tick interrupts a thread on the same CPU that already holds one of these
/// mutexes from an ioctl — the `RUN_QUEUE` freeze shape, which wedges every CPU
/// with no panic. So: `try_lock()` only, never `.lock()`, and specifically NOT
/// `blob_obj_count()`, which blocks and is for syscall context.
///
/// A field that could not be sampled reads `u64::MAX`, not 0, so a missed
/// sample is legible as *missed*. Zero would be indistinguishable from "every
/// object was freed", which is exactly the conclusion this instrument exists to
/// support or refute. Occasional `u64::MAX` under load is expected; a field
/// stuck there means the lock is never free and the sample is worthless.
fn bo_census() -> (u64, u64, u64, u64) {
    const MISSED: u64 = u64::MAX;
    let (dumb, dumb_retired) = match DUMB_BUFFERS.try_lock() {
        Some(m) => (
            m.len() as u64,
            m.values().filter(|b| !b.handle_live).count() as u64,
        ),
        None => (MISSED, MISSED),
    };
    let blob_objs = match BLOB_OBJS.try_lock() {
        Some(m) => m.len() as u64,
        None => MISSED,
    };
    let blob_handles = match BLOB_BUFFERS.try_lock() {
        Some(m) => m.len() as u64,
        None => MISSED,
    };
    (dumb, dumb_retired, blob_objs, blob_handles)
}

/// 100 Hz tick hook (IRQ context): promote at most one pending flip to readable,
/// throttled to ~50 Hz. MUST be non-blocking (try_lock only) and MUST NOT wake
/// pollers when nothing is delivered — otherwise idle CPU regresses. Registered
/// by the DRM server at init. Consistent lock order (PENDING then READY) + the
/// read/flip paths each touching only one of the two means no deadlock.
pub fn drm_tick() {
    let now = sched::ticks();
    if DRM_STATS {
        let ls = LAST_STAT_TICK.load(Ordering::Relaxed);
        if now.wrapping_sub(ls) >= 200 {
            LAST_STAT_TICK.store(now, Ordering::Relaxed);
            crate::pci::serial_debug("[DRMSTAT] t=");
            crate::pci::serial_debug_hex_64(now);
            crate::pci::serial_debug(" flips_sub=");
            crate::pci::serial_debug_hex_64(FLIPS_SUBMITTED.load(Ordering::Relaxed));
            crate::pci::serial_debug(" flips_del=");
            crate::pci::serial_debug_hex_64(DELIVERED_SEQ.load(Ordering::Relaxed));
            crate::pci::serial_debug(" dirtyfb=");
            crate::pci::serial_debug_hex_64(DIRTYFB_CALLS.load(Ordering::Relaxed));
            crate::pci::serial_debug(" clips=");
            crate::pci::serial_debug_hex_64(DIRTYFB_CLIPS.load(Ordering::Relaxed));
            crate::pci::serial_debug(" flip_us=");
            crate::pci::serial_debug_hex_64(FLIP_US_TOTAL.load(Ordering::Relaxed));
            // Primary-plane damage. `flips_sub` now counts only presents that
            // moved pixels, so `flips_sub < atomic` is the win this measures.
            crate::pci::serial_debug(" dmg_full=");
            crate::pci::serial_debug_hex_64(DAMAGE_FULL.load(Ordering::Relaxed));
            crate::pci::serial_debug(" dmg_rect=");
            crate::pci::serial_debug_hex_64(DAMAGE_RECT.load(Ordering::Relaxed));
            crate::pci::serial_debug(" dmg_skip=");
            crate::pci::serial_debug_hex_64(DAMAGE_SKIP.load(Ordering::Relaxed));
            crate::pci::serial_debug(" dmg_px=");
            crate::pci::serial_debug_hex_64(DAMAGE_PX.load(Ordering::Relaxed));
            crate::pci::serial_debug(" blobs=");
            crate::pci::serial_debug_hex_64(BLOBS_CREATED.load(Ordering::Relaxed));
            // Cursor-plane traffic. Once the atomic cursor plane is live,
            // pointer motion should show up here as `curs_mv` climbing while
            // `flips_sub` stays flat — that is the whole point of the lane.
            crate::pci::serial_debug(" curs_up=");
            crate::pci::serial_debug_hex_64(CURSOR_UPDATES.load(Ordering::Relaxed));
            crate::pci::serial_debug(" curs_mv=");
            crate::pci::serial_debug_hex_64(CURSOR_MOVES.load(Ordering::Relaxed));
            crate::pci::serial_debug(" atomic=");
            crate::pci::serial_debug_hex_64(ATOMIC_COMMITS.load(Ordering::Relaxed));
            crate::pci::serial_debug(" atest=");
            crate::pci::serial_debug_hex_64(ATOMIC_TESTS.load(Ordering::Relaxed));
            crate::pci::serial_debug(" cplane=");
            crate::pci::serial_debug_hex_64(CURSOR_PLANE_SEEN.load(Ordering::Relaxed));
            // Guest-side witness that injected input reached the kernel ring at
            // all. Without it, "pointer moves produced no cursor traffic" cannot
            // be told apart from "the moves never arrived" — QMP accepting
            // input-send-event only proves the host queued them.
            crate::pci::serial_debug(" evpush=");
            crate::pci::serial_debug_hex_64(evdev_server::events_pushed());
            // Live-object census, DERIVED from the maps (see `bo_census`). New
            // fields go at the END of the line, never in the middle: `c5abb8d`
            // inserted five `dmg_*` fields mid-line and every position-keyed
            // parser downstream silently reported zero for everything after
            // them. `0xffffffffffffffff` in any of these four means the sample
            // was skipped on lock contention, NOT that the map is empty.
            let (bo_dumb, bo_dumbret, bo_blob, bo_bhnd) = bo_census();
            crate::pci::serial_debug(" bo_dumb=");
            crate::pci::serial_debug_hex_64(bo_dumb);
            crate::pci::serial_debug(" bo_dumbret=");
            crate::pci::serial_debug_hex_64(bo_dumbret);
            crate::pci::serial_debug(" bo_blob=");
            crate::pci::serial_debug_hex_64(bo_blob);
            crate::pci::serial_debug(" bo_bhnd=");
            crate::pci::serial_debug_hex_64(bo_bhnd);
            // Poll wakes actually issued, against `evpush` above (wakes the
            // uncoalesced path would have issued). Their ratio measures
            // `evdev_server::WAKE_MODE`: equal under the baseline, ~1/2 on keys
            // and ~1/3 on motion under SYN-only, and bounded by the tick rate
            // under burst coalescing however fast input arrives. END of the
            // line, per the rule above — never inserted mid-line.
            crate::pci::serial_debug(" pollwake=");
            crate::pci::serial_debug_hex_64(evdev_server::poll_wakes());
            // Control-queue stall census. `submit` spins the vCPU for the whole
            // host round trip, so these say whether a freeze is the guest
            // waiting on the host rather than the guest being busy. Mean is
            // ctrlq_us/ctrlq_n; ctrlq_max is the outlier a mean hides.
            crate::pci::serial_debug(" ctrlq_n=");
            crate::pci::serial_debug_hex_64(
                crate::virtio_gpu::CTRLQ_CMDS.load(Ordering::Relaxed));
            crate::pci::serial_debug(" ctrlq_us=");
            crate::pci::serial_debug_hex_64(
                crate::virtio_gpu::CTRLQ_SPIN_US.load(Ordering::Relaxed));
            crate::pci::serial_debug(" ctrlq_max=");
            crate::pci::serial_debug_hex_64(
                crate::virtio_gpu::CTRLQ_SPIN_MAX_US.load(Ordering::Relaxed));
            crate::pci::serial_debug(" ctrlq_to=");
            crate::pci::serial_debug_hex_64(
                crate::virtio_gpu::CTRLQ_TIMEOUTS.load(Ordering::Relaxed));
            crate::pci::serial_debug("\n");
        }
    }
    // Deferred-wake backstop for syncobj fence retirement. Ahead of the flip
    // throttle's early return because it is not a flip and must not be gated on
    // one; free when nothing is waiting (one relaxed load), which is what keeps
    // idletest's zero-idle-wake property intact.
    syncobj_tick();

    // Deferred retirement of stub v3d submissions — the stand-in for the GPU
    // completion interrupt. Ahead of the flip throttle for the same reason
    // `syncobj_tick` is: it is not a flip and must not be paced by one. One
    // relaxed load when nothing is in flight, which is what keeps `idletest`'s
    // zero-idle-wake property intact on a build where v3d is never armed.
    v3d_tick();

    let last = LAST_FLIP_DELIVER_TICK.load(Ordering::Relaxed);
    // One tick (~10 ms) since the last delivery. The throttle exists so an idle
    // compositor cannot spin — one event per tick still guarantees that, and
    // idletest guards the property. Do NOT drain the whole queue here.
    if now.wrapping_sub(last) < 1 { return; }

    let mut pend = match PENDING_FLIPS.try_lock() { Some(g) => g, None => return };
    if pend.is_empty() { return; }
    let mut ready = match READY_EVENTS.try_lock() { Some(g) => g, None => return };
    if let Some(blob) = pend.pop_front() {
        ready.push_back(blob);
        drop(ready);
        drop(pend);
        LAST_FLIP_DELIVER_TICK.store(now, Ordering::Relaxed);
        DELIVERED_SEQ.fetch_add(1, Ordering::Relaxed);
        sched::try_wake_poll();
    }
}

/// Drain whole (32-byte) DRM events into `out`. Returns bytes written (0 = EAGAIN).
pub fn drm_read_events(out: &mut [u8]) -> usize {
    let mut ready = READY_EVENTS.lock();
    let mut written = 0;
    while out.len() - written >= 32 {
        match ready.pop_front() {
            Some(ev) => { out[written..written + 32].copy_from_slice(&ev); written += 32; }
            None => break,
        }
    }
    written
}

/// Poll readiness for the card fd: true when a DRM event is queued to read.
pub fn drm_has_events() -> bool {
    !READY_EVENTS.lock().is_empty()
}

// ── DRM master ───────────────────────────────────────────────────────────────
//
// Master is per *open* of card0 and it is what decides who may move pixels into
// the shared hardware surface. Before this, SET_MASTER and DROP_MASTER were a
// pair of unconditional `Ok(0)`s and nothing anywhere returned EACCES, so
// `fb_vt_scanout_revoke` — the VT layer taking the display back — was a
// suggestion: the client's very next present re-claimed the scanout and the
// console flickered straight back out. Switching away from a live compositor
// was a race the compositor won, and switching away from a *wedged* one, which
// is the case the whole VT item exists for, could not be done at all.
//
// **Which ioctls are gated is derived, not guessed.** Every path that takes the
// surface bumps `crate::drm::device::SCANOUT_WRITES` (drm/device.rs:370 and
// :442), and those two sites are reachable only from SETCRTC, PAGE_FLIP,
// ATOMIC, DIRTYFB and the three custom LeandrOS codes 0x1001/0x1004/0x1005.
// Gating exactly that set closes the re-claim hole completely — and, just as
// importantly, no virtgpu *render* ioctl can reach it, so a Vulkan client on
// either node is untouched by any of this.
//
// The refusal happens BEFORE the dispatch `match`, which is also where the
// present counter is sampled: an ioctl that is refused never runs, so it can
// never claim.
//
// ── VT scoping ───────────────────────────────────────────────────────────────
//
// A grant is remembered together with the VT that was on screen when it was
// made, and it is armed only while that VT is still on screen. The suspension
// is therefore **derived**: a VT switch writes nothing into the DRM layer and
// cannot race a present, because the moment `ACTIVE` moves, `master_ok` is
// already false for everyone whose VT just left. Switching back re-arms it by
// the same computation.
//
// **The auto-rearm is deliberate and load-bearing.** Master is suspended, never
// destroyed. A compositor that does not re-issue SET_MASTER on resume — and
// nothing obliges it to — would otherwise come back to a permanently dead
// display with no console underneath it either, because its VT is KD_GRAPHICS
// and the console stays gated off. "Cannot present while backgrounded" is the
// property we want; "cannot ever present again" is a wedge.
//
// We cannot ask which VT a card0 fd belongs to — there is no such association,
// on Linux either — so "the caller's VT" means "the VT that was active when this
// open was granted master". That is enough for the only question anyone asks:
// whether the holder is in the foreground.

/// The card0 open holding DRM master, or 0 for "nobody".
static MASTER_OPEN: AtomicU32 = AtomicU32::new(0);

/// `vt::active()` as it stood when [`MASTER_OPEN`] was granted. Meaningless
/// while `MASTER_OPEN` is 0.
static MASTER_VT: AtomicUsize = AtomicUsize::new(0);

/// Ioctls that may only be issued by the DRM master of the VT on screen.
///
/// A list, unlike the console-claim rule it sits next to, because a *refusal*
/// has to be decided before the ioctl runs and therefore cannot be derived from
/// what the ioctl did. The list is kept honest by the counter: anything added
/// later that presents will bump `SCANOUT_WRITES` from outside this set, which
/// is the same observable that caught the previous hardcoded list missing
/// DRM_IOCTL_MODE_ATOMIC.
fn requires_master(cmd: u32) -> bool {
    matches!(
        cmd,
        DRM_IOCTL_MODE_SETCRTC
            | DRM_IOCTL_MODE_PAGE_FLIP
            | DRM_IOCTL_MODE_ATOMIC
            | DRM_IOCTL_MODE_DIRTYFB
            | 0x1001 // set mode
            | 0x1004 // flip page
            | 0x1005 // set plane
    )
}

/// True when `open_id` holds master **and** the VT it holds it on is the one on
/// screen. Two relaxed-ish atomic loads and nothing else — this is on the path
/// of every present.
fn master_ok(open_id: u32) -> bool {
    open_id != 0
        && MASTER_OPEN.load(Ordering::SeqCst) == open_id
        && MASTER_VT.load(Ordering::SeqCst) == tty_server::vt::active()
}

/// Gate one master-only ioctl, granting master implicitly on first use.
///
/// Linux hands a client implicit master at `drm_open` when the device has none.
/// We have no VFS_OPEN arm in the DRM server at all — `servers/drm/src/lib.rs`
/// sees VFS_IOCTL/READ/POLL/CLOSE and nothing else — so the first master-gated
/// ioctl from an open is our equivalent of that moment. It is what keeps every
/// existing client working unchanged: `drmsmoke` and DOOM never issue
/// SET_MASTER, and a gate that required it would have refused both.
fn master_gate(open_id: u32) -> Result<(), DriverError> {
    // An open with no identity cannot be arbitrated in either direction: there
    // is nothing to record as the holder and nothing to compare a later ioctl
    // against. That is the legacy `Driver::handle` path, which behaves exactly
    // as it did before.
    if open_id == 0 { return Ok(()); }
    if master_ok(open_id) { return Ok(()); }
    match MASTER_OPEN.compare_exchange(0, open_id, Ordering::SeqCst, Ordering::SeqCst) {
        // The VT is stored after the exchange, so a loser of the race cannot
        // overwrite the winner's. The window in between is the granting open's
        // own, and the worst it can cost that open is one refused ioctl.
        Ok(_) => { MASTER_VT.store(tty_server::vt::active(), Ordering::SeqCst); Ok(()) }
        Err(_) => Err(DriverError::Access),
    }
}

/// `DRM_IOCTL_SET_MASTER`.
fn master_set(open_id: u32) -> Result<usize, DriverError> {
    if open_id == 0 { return Ok(0); }
    let cur = MASTER_OPEN.load(Ordering::SeqCst);
    if cur == open_id {
        // Already master. Answering 0 is required — a compositor re-asserts
        // master on every session resume and must not be told EBUSY by itself.
        // Backgrounded, the honest answer is EACCES: the grant is intact but
        // suspended, and the caller cannot present with it. Note this does NOT
        // move the grant to the current VT; re-arming is the switch back's job,
        // not a client's, or a background SET_MASTER would be a way to steal a
        // console.
        return if MASTER_VT.load(Ordering::SeqCst) == tty_server::vt::active() {
            Ok(0)
        } else {
            Err(DriverError::Access)
        };
    }
    if cur != 0 { return Err(DriverError::Busy); }
    match MASTER_OPEN.compare_exchange(0, open_id, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => { MASTER_VT.store(tty_server::vt::active(), Ordering::SeqCst); Ok(0) }
        Err(_) => Err(DriverError::Busy),
    }
}

/// `DRM_IOCTL_DROP_MASTER`.
fn master_drop(open_id: u32) -> Result<usize, DriverError> {
    if open_id == 0 { return Ok(0); }
    match MASTER_OPEN.compare_exchange(open_id, 0, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => Ok(0),
        Err(_) => Err(DriverError::NotMaster),
    }
}

/// Forget the master grant. `open_id` 0 clears it whoever holds it.
///
/// Two call sites, for two different reasons: a card0 open going away (its
/// master dies with it, or the node is unusable until reboot), and the panic
/// handler's `console_force_reclaim`, which takes the display back
/// unconditionally and must not leave a master able to take it away again.
pub fn drm_master_clear(open_id: u32) {
    if open_id == 0 {
        MASTER_OPEN.store(0, Ordering::SeqCst);
        return;
    }
    let _ = MASTER_OPEN.compare_exchange(open_id, 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// DRM device interface for userspace communication
pub struct DrmDeviceInterface {
    driver: DrmDriver,
    _device_path: &'static str,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

impl DrmDeviceInterface {
    /// Create new DRM device interface
    pub fn new() -> Self {
        Self {
            driver: DrmDriver::new(),
            _device_path: "/dev/dri/card0",
        }
    }

    /// Handle incoming IPC messages.
    ///
    /// `open_id` identifies which *open* of card0 this ioctl arrived on (the
    /// VFS's per-open cookie, ioctl slot 4). It is a parameter, not state on
    /// `self`, because port handlers run synchronously on the caller's thread:
    /// this method is re-entrant across clients and faults on user memory.
    /// 0 means "no open identity" — the 3D arms refuse it.
    pub fn handle_ioctl(&mut self, cmd: u32, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        crate::pci::rdebug("[DRM-IF] handle_ioctl cmd=");
        crate::pci::rdebug_hex(cmd);
        crate::pci::rdebug("\n");

        // MASTER GATE. Deliberately ahead of the present-counter sample and the
        // dispatch `match` below: a refused ioctl must not reach a handler, and
        // therefore cannot claim the scanout on its way out. Putting it after
        // the match — refusing the *result* — would let the present land first
        // and leave the console gated off by the very ioctl we rejected.
        if requires_master(cmd) {
            master_gate(open_id)?;
        }

        // The DRM device lock is a spin::Mutex. It must NOT be held across any
        // dereference of user memory: a demand-paging fault taken under a spinlock
        // is the 82d0cc3 all-vCPU freeze class (no panic, IF=0 on every vCPU).
        // We therefore lock PER ARM, only around device-state access. The new K4
        // arms strictly copy the user struct into a kernel local BEFORE locking
        // and write results back AFTER dropping the lock. The pre-existing arms
        // operate on small fixed ioctl structs that the caller filled on its own
        // always-resident stack immediately before the syscall.
        //
        // SCANOUT CLAIM: whichever open just moved pixels into the hardware
        // surface owns it, and the framebuffer console must stop painting over
        // it — the console draws into that same buffer, and its scroll memmoves
        // the whole thing (see `drm_scanout_claim` in
        // drivers/src/framebuffer.rs). Sampling the present counter across the
        // dispatch is what replaces the old hardcoded list of "console-killing"
        // ioctl numbers: that list named SETCRTC and PAGE_FLIP and two custom
        // codes, and missed DRM_IOCTL_MODE_ATOMIC entirely, so an atomic
        // compositor scanned out with the console still live underneath it.
        // Deriving the claim from the present itself covers every path,
        // including ones added later.
        //
        // Two card0 opens issuing ioctls on two vCPUs in the same instant can
        // misattribute a present to the wrong open. The cost is the console
        // returning one close early rather than late, and a KMS master is
        // single here — where it was not, this would need the open id threaded
        // down to the composite instead.
        let presents_before = crate::drm::device::SCANOUT_WRITES.load(Ordering::Relaxed);

        // V3D FIRST, AND ONLY WHEN ARMED. Two v3d request codes are bit-identical
        // to two virtgpu ones (see `v3d_dispatch`), so a flat `match` cannot hold
        // both and the active backend is the only thing that disambiguates them.
        // Disarmed — which is every QEMU/virtio build unless a client explicitly
        // asks — this is one relaxed load and the `match` below is reached
        // bit-identically.
        let v3d_res = if v3d_active() { self.v3d_dispatch(cmd, arg, open_id) } else { None };
        let res = match v3d_res { Some(r) => r, None => match cmd {
            // ── Mode setting ioctls (Custom LeandrOS / DOOM path) ──
            0x1001 => self.handle_set_mode(arg),
            0x1003 => self.handle_get_mode_safe(arg),
            0x1002 => { let d = get_drm_device(); let mut g = d.lock(); self.handle_create_framebuffer(&mut g, arg) },
            0x4600 => { let d = get_drm_device(); let mut g = d.lock(); self.handle_fbioget_vscreeninfo(&mut g, arg) },
            0x1004 => { let d = get_drm_device(); let mut g = d.lock(); self.handle_flip_page(&mut g, arg) },
            0x1005 => { let d = get_drm_device(); let mut g = d.lock(); self.handle_set_plane(&mut g, arg) },
            0x1006 => self.handle_get_capabilities(arg),
            0x1007 => { let d = get_drm_device(); let mut g = d.lock(); self.handle_ioctl_mmap(&mut g, arg) },

            // ── Standard Linux DRM IOCTLs (already wired) ──
            DRM_IOCTL_VERSION => self.std_handle_version(arg),
            DRM_IOCTL_MODE_GETRESOURCES => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_get_resources(&mut g, arg) },
            DRM_IOCTL_MODE_GETCONNECTOR => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_get_connector(&mut g, arg) },
            DRM_IOCTL_MODE_GETENCODER => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_get_encoder(&mut g, arg) },
            DRM_IOCTL_MODE_GETCRTC => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_get_crtc(&mut g, arg) },
            DRM_IOCTL_MODE_CREATE_DUMB => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_create_dumb(&mut g, arg) },
            DRM_IOCTL_MODE_MAP_DUMB => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_map_dumb(&mut g, arg) },
            DRM_IOCTL_MODE_ADDFB => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_addfb(&mut g, arg, open_id) },
            DRM_IOCTL_MODE_SETCRTC => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_set_crtc(&mut g, arg) },
            DRM_IOCTL_MODE_PAGE_FLIP => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_page_flip(&mut g, arg) },

            // ── Virtio-GPU 3D IOCTLs (lock VIRTIO_GPU, not the DRM device) ──
            DRM_IOCTL_VIRTGPU_RESOURCE_CREATE => self.virtgpu_handle_resource_create(arg),
            DRM_IOCTL_VIRTGPU_EXECBUFFER => self.virtgpu_handle_execbuffer(arg, open_id),
            DRM_IOCTL_VIRTGPU_GET_CAPS => self.virtgpu_handle_get_caps(arg),
            DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST => self.virtgpu_handle_transfer_to_host(arg),
            DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST => self.virtgpu_handle_transfer_from_host(arg),
            DRM_IOCTL_VIRTGPU_GETPARAM => self.virtgpu_handle_getparam(arg, open_id),
            DRM_IOCTL_VIRTGPU_CONTEXT_INIT => self.virtgpu_handle_context_init(arg, open_id),
            DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB => self.virtgpu_handle_resource_create_blob(arg, open_id),
            DRM_IOCTL_VIRTGPU_MAP => self.virtgpu_handle_map(arg, open_id),
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO => self.virtgpu_handle_resource_info(arg, open_id),
            DRM_IOCTL_VIRTGPU_WAIT => self.virtgpu_handle_wait(arg, open_id),

            // ── K4: Mesa/GBM buffer + Smithay/libdrm KMS surface ──
            DRM_IOCTL_GET_CAP => self.std_handle_get_cap(arg),
            DRM_IOCTL_SET_CLIENT_CAP => self.std_handle_set_client_cap(arg),
            DRM_IOCTL_SET_MASTER => master_set(open_id),
            DRM_IOCTL_DROP_MASTER => master_drop(open_id),
            DRM_IOCTL_GET_MAGIC => self.std_handle_get_magic(arg),
            DRM_IOCTL_AUTH_MAGIC => Ok(0),
            DRM_IOCTL_GEM_CLOSE => self.std_handle_gem_close(arg, open_id),
            DRM_IOCTL_MODE_DESTROY_DUMB => self.std_handle_destroy_dumb(arg, open_id),
            DRM_IOCTL_MODE_ADDFB2 => self.std_handle_addfb2(arg, open_id),
            DRM_IOCTL_MODE_RMFB => self.std_handle_rmfb(arg),
            DRM_IOCTL_MODE_DIRTYFB => self.std_handle_dirtyfb(arg),
            DRM_IOCTL_MODE_OBJ_GETPROPERTIES => self.std_handle_obj_get_properties(arg),
            DRM_IOCTL_MODE_GETPLANERESOURCES => self.std_handle_get_plane_resources(arg),
            DRM_IOCTL_MODE_GETPLANE => self.std_handle_get_plane(arg),
            DRM_IOCTL_MODE_GETPROPERTY => self.std_handle_get_property(arg),

            // ── Atomic KMS ──
            DRM_IOCTL_MODE_ATOMIC => self.std_handle_atomic(arg),
            DRM_IOCTL_MODE_CREATEPROPBLOB => self.std_handle_create_blob(arg),
            DRM_IOCTL_MODE_DESTROYPROPBLOB => self.std_handle_destroy_blob(arg),
            DRM_IOCTL_MODE_GETPROPBLOB => self.std_handle_get_blob(arg),

            // ── Sync objects ──
            // Binary syncobjs are implemented; the timeline family and sync-file
            // export answer ENOSYS explicitly (see `syncobj_timeline_stub`).
            // None of these is master-gated and none can present, so they are
            // absent from `requires_master` by construction.
            DRM_IOCTL_SYNCOBJ_CREATE => self.syncobj_handle_create(arg, open_id),
            DRM_IOCTL_SYNCOBJ_DESTROY => self.syncobj_handle_destroy(arg, open_id),
            DRM_IOCTL_SYNCOBJ_WAIT => self.syncobj_handle_wait(arg, open_id),
            DRM_IOCTL_SYNCOBJ_RESET => self.syncobj_handle_array(arg, open_id, false),
            DRM_IOCTL_SYNCOBJ_SIGNAL => self.syncobj_handle_array(arg, open_id, true),
            DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD
            | DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE
            | DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT
            | DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL
            | DRM_IOCTL_SYNCOBJ_QUERY
            | DRM_IOCTL_SYNCOBJ_TRANSFER
            | DRM_IOCTL_SYNCOBJ_EVENTFD => self.syncobj_timeline_stub(arg),
            // No PRIME (single node, render==scanout) — Mesa falls back to software.
            DRM_IOCTL_PRIME_HANDLE_TO_FD | DRM_IOCTL_PRIME_FD_TO_HANDLE => Err(DriverError::Unsupported),

            // An ioctl this driver does not implement. Historically this arm was
            // a silent `Err(Unsupported)`, which is indistinguishable — from the
            // outside and from the serial log — from an ioctl that ran and
            // failed. A missing arm then looks like a broken one, and costs a
            // session to find. Report the number so the gap names itself.
            //
            // `nr` and `size` are the identifying halves of the encoded request
            // (`_IOC(dir, type, nr, size)`): `nr` is what to look up in
            // `drm.h`/`virtgpu_drm.h`, and a matching `nr` with the wrong `size`
            // is the other failure this makes visible — a struct whose layout
            // drifted from the caller's.
            _ => {
                if UNKNOWN_IOCTLS.lock().first(cmd) {
                    crate::pci::serial_debug("[DRM] unimplemented ioctl cmd=");
                    crate::pci::serial_debug_hex(cmd);
                    crate::pci::serial_debug(" nr=");
                    crate::pci::serial_debug_hex(cmd & 0xFF);
                    crate::pci::serial_debug(" type=");
                    crate::pci::serial_debug_hex((cmd >> 8) & 0xFF);
                    crate::pci::serial_debug(" size=");
                    crate::pci::serial_debug_hex((cmd >> 16) & 0x3FFF);
                    crate::pci::serial_debug(" (reported once per boot)\n");
                }
                Err(DriverError::Unsupported)
            }
        }};

        // Re-check master on the way out, not just on the way in. The gate
        // above refuses ioctls that have not started; it cannot recall one that
        // is already inside a handler, and on a software-scaled 1920x1080
        // present that handler runs for tens of milliseconds under TCG. A VT
        // switch landing in that window used to leave the console repainted,
        // gated back OFF by the claim this in-flight present made on its way
        // out, and silent from then on — a black screen that answered every
        // keystroke and showed none of them. Measured on x86_64, where the
        // window is wide; the same race exists on aarch64 under HVF, ~30x
        // narrower.
        //
        // A present whose VT left while it ran is stale by definition, so it
        // does not get to silence the console. `open_id` 0 keeps the legacy
        // `Driver::handle` behaviour, which has no master to check.
        if crate::drm::device::SCANOUT_WRITES.load(Ordering::Relaxed) != presents_before
            && (open_id == 0 || master_ok(open_id))
        {
            crate::framebuffer::drm_scanout_claim(open_id);
        }

        crate::pci::rdebug("[DRM-IF] handle_ioctl finished, returning Result\n");
        res
    }

    /// Handle DRM_IOCTL_SET_MODE
    fn handle_set_mode(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, refresh] array
        let mode_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 3)
        };

        let width = mode_data[0];
        let height = mode_data[1];
        let refresh = mode_data[2];

        // Set display mode using our DRM subsystem
        match ModeSet::set_display_mode(width, height, refresh) {
            Ok(()) => Ok(0),
            Err(_) => Err(DriverError::Io),
        }
    }

    /// Handle DRM_IOCTL_GET_MODE safely by not holding the lock during userspace write
    fn handle_get_mode_safe(&mut self, arg: usize) -> Result<usize, DriverError> {
        crate::pci::rdebug("[DRM-IF] handle_get_mode_safe starting\n");
        
        if arg == 0 { return Err(DriverError::InvalidParameter); }

        let mut width = 0;
        let mut height = 0;
        let mut refresh = 60;
        let mut found = false;

        // 1. Get info from device (acquiring lock briefly)
        {
            crate::pci::rdebug("[DRM-IF] Locking DRM_DEVICE briefly...\n");
            let device = get_drm_device().lock();
            if let Some(crtc) = device.crtcs.first() {
                if let Some(mode) = &crtc.mode {
                    crate::pci::rdebug("[DRM-IF] Got mode from CRTC\n");
                    width = mode.hdisplay as u32;
                    height = mode.vdisplay as u32;
                    refresh = mode.vrefresh;
                    found = true;
                }
            }
            crate::pci::rdebug("[DRM-IF] Unlocked DRM_DEVICE\n");
        }

        if !found {
            crate::pci::rdebug("[DRM-IF] Falling back to VFS info\n");
            // Get mode from existing KMS framebuffer console
            extern "C" {
                fn vfs_get_framebuffer_info(info: &mut FramebufferInfo);
            }

            let mut fb_info = FramebufferInfo { width: 0, height: 0, pitch: 0 };
            unsafe { vfs_get_framebuffer_info(&mut fb_info); }

            if fb_info.width > 0 && fb_info.height > 0 {
                crate::pci::rdebug("[DRM-IF] Got mode from VFS: ");
                crate::pci::rdebug_hex(fb_info.width);
                crate::pci::rdebug("x");
                crate::pci::rdebug_hex(fb_info.height);
                crate::pci::rdebug("\n");

                width = fb_info.width;
                height = fb_info.height;
                refresh = 60;
            } else {
                crate::pci::rdebug("[DRM-IF] Final fallback to 640x480\n");
                width = 640;
                height = 480;
                refresh = 60;
            }
        }

        // 2. Write to userspace
        crate::pci::rdebug("[DRM-IF] Writing to userspace at ");
        crate::pci::rdebug_hex(arg as u32);
        crate::pci::rdebug("\n");

        unsafe {
            let ptr = arg as *mut u32;
            ptr.write_volatile(width);
            ptr.add(1).write_volatile(height);
            ptr.add(2).write_volatile(refresh);
        }

        crate::pci::rdebug("[DRM-IF] handle_get_mode_safe finished OK\n");
        Ok(0)
    }

    /// A card0 open closed: give the framebuffer console the scanout back, but
    /// only if this is the open that took it.
    ///
    /// It used to hand the console back on every card0 close, which meant a
    /// second, short-lived open of the node — a probe, a libseat handshake —
    /// wiped a live compositor's display, because reclaim clears the screen and
    /// prints a banner. `open_id` 0 means "no open identity" and releases
    /// unconditionally.
    pub fn release(&mut self, open_id: u32) {
        crate::framebuffer::drm_scanout_release(open_id);
    }

    /// Handle DRM_IOCTL_CREATE_FB
    fn handle_create_framebuffer(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, format, fb_id_out, buffer_ptr_out, mmap_offset_out]
        let fb_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 6)
        };

        let width = fb_data[0];
        let height = fb_data[1];
        let _format = fb_data[2];

        // Allocate dumb buffer
        let buffer = DrmDumbBuffer::create(width, height, 32)?;
        let mmap_offset = buffer.mmap_offset;

        // Create framebuffer object
        let mut fb = DrmFramebuffer::new(
            width,
            height,
            DrmFormat::Xrgb8888,
            buffer.handle,
            width * 4 // pitch
        );
        fb.physical_addresses[0] = mmap_offset as u64;

        let fb_id = fb.id().0;
        device.framebuffers.insert(fb.id(), fb);

        // If Virtio-GPU is present, bind a resource for this framebuffer.
        // Looked up BEFORE taking the device lock so the two are never nested.
        let existing_res = fb_resource_id(buffer.handle);
        let mut bound_res: Option<u32> = None;
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let res_id = match existing_res {
                // Already has a host resource (a VIRTGPU_RESOURCE_CREATE 3D BO,
                // or a second ADDFB on the same buffer) — reuse it.
                Some(r) => r,
                None => {
                    let r = gpu.alloc_resource_id();
                    gpu.create_resource_2d(r, width, height);
                    gpu.attach_backing(r, mmap_offset as u64, width * height * 4);
                    r
                }
            };

            // Also store the resource ID in the FB's handles for flip_page
            if let Some(fb_obj) = device.framebuffers.get_mut(&DrmObjectId(fb_id)) {
                fb_obj.handles[0] = res_id;
            }
            bound_res = Some(res_id);
        }
        if let Some(r) = bound_res { fb_set_resource_id(buffer.handle, r); }

        // Return results to userspace.
        // Slot [4] = 0 forces DOOM through its mmap() branch, which calls sys_mmap →
        // ioctl 0x1007 → map_device(virt, phys_addr, len) — giving DOOM a proper virtual
        // address that maps to the same physical page VirtIO reads via attach_backing.
        // Slot [5] carries the physical address used as the mmap offset (< 4 GiB assumed).
        fb_data[3] = fb_id;
        fb_data[4] = 0;                        // no direct buffer pointer — force mmap
        fb_data[5] = mmap_offset as u32;       // physical address as mmap offset


        Ok(0)
    }

    /// Handle DRM_IOCTL_FLIP_PAGE with hardware scaling
    fn handle_flip_page(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to [fb_id, flags, src_width, src_height] for scaling support
        let flip_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 4)
        };

        let fb_id = DrmObjectId(flip_data[0]);
        let _flags = flip_data[1];
        let src_width = if flip_data[2] != 0 { flip_data[2] } else { 320 };
        let src_height = if flip_data[3] != 0 { flip_data[3] } else { 200 };

        // A host-memory blob framebuffer is scanned out directly; there are no
        // guest pixels for the software-scaling path below to copy.
        if let Some(r) = present_blob_fb(device, fb_id, None) {
            return r.map(|_| 0);
        }

        crate::pci::rdebug("[DRM-IF] handle_flip_page fb_id=");
        crate::pci::rdebug_hex(fb_id.0);
        crate::pci::rdebug(" src=");
        crate::pci::rdebug_hex(src_width);
        crate::pci::rdebug("x");
        crate::pci::rdebug_hex(src_height);
        crate::pci::rdebug("\n");

        // Get first CRTC for page flip
        if let Some(crtc) = device.crtcs.first() {
            let crtc_id = crtc.id();

            // Get display dimensions
            let (display_width, display_height) = if let Some(mode) = &crtc.mode {
                (mode.hdisplay as u32, mode.vdisplay as u32)
            } else {
                // Fallback to VFS info if mode not initialized
                extern "C" {
                    fn vfs_get_framebuffer_info(info: &mut FramebufferInfo);
                }
                let mut info = FramebufferInfo { width: 0, height: 0, pitch: 0 };
                unsafe { vfs_get_framebuffer_info(&mut info); }
                (info.width, info.height)
            };

            crate::pci::rdebug("[DRM-IF] flip display=");
            crate::pci::rdebug_hex(display_width);
            crate::pci::rdebug("x");
            crate::pci::rdebug_hex(display_height);
            crate::pci::rdebug("\n");

            if display_width == 0 || display_height == 0 {
                crate::pci::rdebug("[DRM-IF] flip aborted: zero display dims\n");
                return Err(DriverError::NotFound);
            }

            // Set the new framebuffer on the primary plane with hardware scaling
            if let Some(plane) = device.planes.first() {
                let plane_id = plane.id();

                // Create atomic state for hardware-scaled page flip
                let mut atomic_state = AtomicModeSet::begin();

                // Use hardware scaling from source framebuffer to full display
                AtomicModeSet::set_plane(
                    &mut atomic_state,
                    plane_id,
                    Some(crtc_id),
                    Some(fb_id),
                    0, 0, display_width, display_height, // Dst
                    0, 0, src_width << 16, src_height << 16, // Src
                );

                // Commit the atomic state with hardware scaling
                // Pass device directly to avoid deadlock
                AtomicModeSet::commit(device, atomic_state, 0)?;
                Ok(0)
            } else {
                Err(DriverError::NotFound)
            }
        } else {
            Err(DriverError::NotFound)
        }
    }

    /// Handle DRM_IOCTL_SET_PLANE
    fn handle_set_plane(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to plane configuration data
        let plane_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 12)
        };

        let plane_id = DrmObjectId(plane_data[0]);
        let crtc_id = if plane_data[1] != 0 { Some(DrmObjectId(plane_data[1])) } else { None };
        let fb_id = if plane_data[2] != 0 { Some(DrmObjectId(plane_data[2])) } else { None };
        let crtc_x = plane_data[3] as i32;
        let crtc_y = plane_data[4] as i32;
        let crtc_w = plane_data[5];
        let crtc_h = plane_data[6];
        let src_x = plane_data[7];
        let src_y = plane_data[8];
        let src_w = plane_data[9];
        let src_h = plane_data[10];

        let mut atomic_state = AtomicModeSet::begin();

        AtomicModeSet::set_plane(
            &mut atomic_state,
            plane_id,
            crtc_id,
            fb_id,
            crtc_x,
            crtc_y,
            crtc_w,
            crtc_h,
            src_x,
            src_y,
            src_w,
            src_h,
        );

        AtomicModeSet::commit(device, atomic_state, 0)?;
        Ok(0)
    }

    /// Handle DRM_IOCTL_GET_CAPS
    fn handle_get_capabilities(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [capability, value_out]
        let caps_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 2)
        };

        let capability = caps_data[0];

        let value = match capability {
            0x1 => 1, // DRM_CAP_DUMB_BUFFER - supported
            0x2 => 1, // DRM_CAP_VBLANK - supported
            0x3 => 0, // DRM_CAP_PRIME - not supported
            0x7 => 1, // DRM_CAP_ASYNC_PAGE_FLIP - supported
            0x8 => 64, // DRM_CAP_CURSOR_WIDTH
            0x9 => 64, // DRM_CAP_CURSOR_HEIGHT
            _ => 0,
        };

        caps_data[1] = value;
        Ok(0)
    }

    // ── Standard Linux DRM IOCTL Handlers ─────────────────────────────────────

    fn std_handle_version(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let v = unsafe { &mut *(arg as *mut drm_version) };

        // ── v3d ──────────────────────────────────────────────────────────────
        // Ahead of everything below, and an early return, because when the v3d
        // backend is armed there is no virtio question left to ask: this node
        // answers the v3d ABI and nothing else.
        //
        // THE NAME MUST BE EXACTLY `v3d`. Mesa's pipe loader reads
        // `DRM_IOCTL_VERSION.name` (`pipe_loader_drm.c:276`) and plain
        // `strcmp`s it against each driver's descriptor (`:98`). There is no
        // fuzzy match and no diagnostic: an unrecognised name silently selects
        // the software backend, which is the identical failure the virgl lane
        // already paid for once with `leandros-drm`.
        //
        // Version 1.0.0 is upstream's v3d DRIVER_MAJOR/MINOR/PATCHLEVEL.
        //
        // ⚠ THE STRINGS BELOW CARRY NO TRAILING NUL, unlike the two arms after
        // this one, and that is a fix rather than an inconsistency. libdrm's
        // `drmGetVersion` is a two-pass protocol: pass one asks with null
        // pointers, `malloc`s `name_len + 1`, passes `name_len` BACK UNCHANGED
        // in pass two, and NUL-terminates the result itself. The sibling arms
        // report `"virtio_gpu\0".len()` — 11 for a 10-character name — so the
        // capacity guard is `name_len >= 11` and libdrm's honest reply of 11
        // satisfies it by luck: the count it was given is the count it hands
        // back. Reporting the true `strlen` while still guarding on
        // `len + 1` would NOT satisfy it, and the copy would silently not
        // happen, which is how this was found — `v3d` reported as 4 characters.
        //
        // Upstream Linux reports `strlen` and copies `min(user_len, strlen)`
        // bytes, leaving termination to the caller. That is what this arm does.
        // The sibling arms are left exactly as they were: their off-by-one is
        // self-consistent, Venus and the DRI loader both work against it today,
        // and changing it would change every existing client's answer for no
        // benefit to this lane.
        if v3d_active() {
            v.version_major = 1;
            v.version_minor = 0;
            v.version_patchlevel = 0;
            let name = "v3d";
            let date = "20180419";
            let desc = "Broadcom V3D graphics";
            if v.name != 0 && v.name_len >= name.len() {
                unsafe { ptr::copy_nonoverlapping(name.as_ptr(), v.name as *mut u8, name.len()); }
            }
            v.name_len = name.len();
            if v.date != 0 && v.date_len >= date.len() {
                unsafe { ptr::copy_nonoverlapping(date.as_ptr(), v.date as *mut u8, date.len()); }
            }
            v.date_len = date.len();
            if v.desc != 0 && v.desc_len >= desc.len() {
                unsafe { ptr::copy_nonoverlapping(desc.as_ptr(), v.desc as *mut u8, desc.len()); }
            }
            v.desc_len = desc.len();
            return Ok(0);
        }

        // The identity FOLLOWS the device we actually got — see the note below.
        let virgl = crate::virtio_gpu::virgl_negotiated();
        if virgl {
            // Upstream virtio_gpu's DRIVER_MAJOR/MINOR/PATCHLEVEL, matching the
            // render node (servers/drm/src/lib.rs `render_node_version`).
            v.version_major = 0;
            v.version_minor = 1;
            v.version_patchlevel = 0;
        } else {
            v.version_major = 1;
            v.version_minor = 6;
            v.version_patchlevel = 0;
        }

        // card0 used to report `leandros-drm` ON PURPOSE: Mesa's DRI loader
        // picks a driver by exactly this string (`loader_get_driver_for_fd`),
        // and an unrecognised name falls through to the software backend. That
        // was the right call while the shipped megadriver was softpipe-only —
        // claiming to be virtio_gpu would have sent the loader hunting for a
        // virgl driver that was not in the image.
        //
        // The megadriver now ships `virgl` alongside softpipe, so the fallback
        // this name bought is exactly what we no longer want: `leandros-drm`
        // made Mesa pick kms_swrast and report `renderer: "softpipe"` even
        // with a working virglrenderer on the host. Reporting the upstream
        // identity is also the truth — every ioctl behind this node is the
        // virtgpu ABI.
        //
        // ⚠ The tradeoff the old comment warned about is real: on the virgl
        // identity there is no silent software fallback on this fd. If virgl
        // cannot initialise, GBM/EGL fail outright rather than degrading. That
        // is why the name is CONDITIONAL on the host having actually negotiated
        // VIRTIO_GPU_F_VIRGL — on a plain `virtio-vga` (or `run-qemu.sh
        // --no-virgl`) we keep the old identity and the software path is
        // untouched, so the fallback is chosen by the device, not by luck.
        let (name, date, desc) = if virgl {
            ("virtio_gpu\0", "0\0", "virtio GPU\0")
        } else {
            ("leandros-drm\0", "20261201\0", "LeandrOS DRM driver\0")
        };

        if v.name != 0 && v.name_len >= name.len() {
            unsafe { ptr::copy_nonoverlapping(name.as_ptr(), v.name as *mut u8, name.len()); }
        }
        v.name_len = name.len();

        if v.date != 0 && v.date_len >= date.len() {
            unsafe { ptr::copy_nonoverlapping(date.as_ptr(), v.date as *mut u8, date.len()); }
        }
        v.date_len = date.len();

        if v.desc != 0 && v.desc_len >= desc.len() {
            unsafe { ptr::copy_nonoverlapping(desc.as_ptr(), v.desc as *mut u8, desc.len()); }
        }
        v.desc_len = desc.len();

        Ok(0)
    }

    fn std_handle_get_resources(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let res = unsafe { &mut *(arg as *mut drm_mode_card_res) };
        
        // We report 1 of each for a simple virtual device
        let crtc_ids = [1u32];
        let connector_ids = [1u32];
        let encoder_ids = [1u32];

        if res.crtc_id_ptr != 0 && res.count_crtcs >= 1 {
            unsafe { ptr::copy_nonoverlapping(crtc_ids.as_ptr(), res.crtc_id_ptr as *mut u32, 1); }
        }
        res.count_crtcs = 1;

        if res.connector_id_ptr != 0 && res.count_connectors >= 1 {
            unsafe { ptr::copy_nonoverlapping(connector_ids.as_ptr(), res.connector_id_ptr as *mut u32, 1); }
        }
        res.count_connectors = 1;

        if res.encoder_id_ptr != 0 && res.count_encoders >= 1 {
            unsafe { ptr::copy_nonoverlapping(encoder_ids.as_ptr(), res.encoder_id_ptr as *mut u32, 1); }
        }
        res.count_encoders = 1;

        res.min_width = 320;
        res.max_width = 4096;
        res.min_height = 200;
        res.max_height = 4096;

        Ok(0)
    }

    fn std_handle_get_connector(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let conn = unsafe { &mut *(arg as *mut drm_mode_get_connector) };
        
        conn.connector_id = 1;
        conn.connector_type = 11; // DRM_MODE_CONNECTOR_VIRTUAL
        conn.connector_type_id = 1;
        conn.connection = 1; // Connected
        conn.mm_width = 320;
        conn.mm_height = 200;

        if conn.encoders_ptr != 0 && conn.count_encoders >= 1 {
            let encoders = [1u32];
            unsafe { ptr::copy_nonoverlapping(encoders.as_ptr(), conn.encoders_ptr as *mut u32, 1); }
        }
        conn.count_encoders = 1;

        // Provide at least one mode
        if conn.modes_ptr != 0 && conn.count_modes >= 1 {
            extern "C" { fn vfs_get_framebuffer_info(info: &mut FramebufferInfo); }
            let mut info = FramebufferInfo { width: 0, height: 0, pitch: 0 };
            unsafe { vfs_get_framebuffer_info(&mut info); }
            let mut mode = drm_mode_modeinfo::default();
            mode.hdisplay = info.width as u16;
            mode.vdisplay = info.height as u16;
            mode.vrefresh = 60;
            // Populate non-zero blanking/timing. Consumers that derive the refresh
            // rate from the raw mode (smithay Output: refresh = clock*1e6/(htotal*
            // vtotal)) divide by htotal/vtotal, so leaving them 0 panics the
            // compositor. virtio-gpu scanout only uses hdisplay/vdisplay; the sync
            // fields are otherwise cosmetic. Approximate CVT blanking, with `clock`
            // (kHz) chosen so the derived refresh is exactly 60 Hz.
            let htotal = (info.width as u16).saturating_add(160);
            let vtotal = (info.height as u16).saturating_add(40);
            mode.hsync_start = (info.width as u16).saturating_add(48);
            mode.hsync_end   = (info.width as u16).saturating_add(80);
            mode.htotal      = htotal;
            mode.vsync_start = (info.height as u16).saturating_add(3);
            mode.vsync_end   = (info.height as u16).saturating_add(9);
            mode.vtotal      = vtotal;
            mode.clock = (htotal as u32 * vtotal as u32 * 60) / 1000;
            let name = b"Native\0";
            mode.name[..name.len()].copy_from_slice(name);
            
            unsafe { ptr::copy_nonoverlapping(&mode, conn.modes_ptr as *mut drm_mode_modeinfo, 1); }
        }
        conn.count_modes = 1;
        conn.encoder_id = 1;

        Ok(0)
    }

    fn std_handle_get_encoder(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let enc = unsafe { &mut *(arg as *mut drm_mode_get_encoder) };
        enc.encoder_id = 1;
        enc.encoder_type = 3; // DRM_MODE_ENCODER_VIRTUAL
        enc.crtc_id = 1;
        enc.possible_crtcs = 1;
        Ok(0)
    }

    fn std_handle_get_crtc(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let crtc_out = unsafe { &mut *(arg as *mut drm_mode_crtc) };
        let crtc_id = DrmObjectId(crtc_out.crtc_id);
        if let Some(crtc) = device.get_crtc(crtc_id) {
            // Find FB ID from planes associated with this CRTC
            crtc_out.fb_id = device.planes.iter()
                .find(|p| p.crtc_id == Some(crtc_id))
                .and_then(|p| p.fb_id)
                .map(|id| id.0)
                .unwrap_or(0);
            crtc_out.x = crtc.x as u32;
            crtc_out.y = crtc.y as u32;
            if let Some(mode) = &crtc.mode {
                crtc_out.mode.hdisplay = mode.hdisplay as u16;
                crtc_out.mode.vdisplay = mode.vdisplay as u16;
                crtc_out.mode.vrefresh = mode.vrefresh;
            }
            crtc_out.mode_valid = if crtc.mode.is_some() { 1 } else { 0 };
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn std_handle_create_dumb(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let create = unsafe { &mut *(arg as *mut drm_mode_create_dumb) };
        let buffer = DrmDumbBuffer::create(create.width, create.height, create.bpp)?;
        
        create.handle = buffer.handle;
        create.pitch = buffer.pitch;
        create.size = buffer.size as u64;

        Ok(0)
    }

    fn std_handle_map_dumb(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let map = unsafe { &mut *(arg as *mut drm_mode_map_dumb) };

        // Return the actual physical address associated with the dumb buffer handle
        match dumb_lookup(map.handle) {
            Some(b) => { map.offset = b.phys as u64; Ok(0) }
            None => Err(DriverError::NotFound),
        }
    }
    fn std_handle_addfb(&mut self, device: &mut DrmDevice, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let add = unsafe { &mut *(arg as *mut drm_mode_fb_cmd) };

        // Blob BO: see "Blob framebuffers". Legacy ADDFB carries no fourcc;
        // 32 bpp is XRGB8888, which is the only depth this path ever accepted.
        if let Some(fb) = blob_framebuffer(add.handle, open_id, add.width, add.height,
                                           add.pitch, 0, 0x34325258, 0) {
            let fb_id = fb.id().0;
            device.framebuffers.insert(fb.id(), fb);
            add.fb_id = fb_id;
            return Ok(0);
        }

        let mut fb = DrmFramebuffer::new(
            add.width,
            add.height,
            DrmFormat::Xrgb8888,
            add.handle,
            add.pitch
        );

        // Use the physical address associated with the dumb buffer handle. An
        // unknown handle is refused (see std_handle_addfb2), not given a
        // framebuffer over guest address 0.
        let phys_addr = dumb_lookup(add.handle).map(|b| b.phys).ok_or(DriverError::NotFound)?;
        fb.physical_addresses[0] = phys_addr as u64;

        // If Virtio-GPU is present, bind a resource for this framebuffer.
        // See fb_resource_id: `handle + 10` overlapped the id space
        // alloc_resource_id() hands 3D/blob resources out of, from 16 up.
        // Resolved before the device lock so DUMB_BUFFERS is never nested
        // inside VIRTIO_GPU.
        let existing_res = fb_resource_id(add.handle);
        let mut bound_res: Option<u32> = None;
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let res_id = match existing_res {
                // A BO that already owns a host resource — a
                // VIRTGPU_RESOURCE_CREATE 3D buffer, or a re-ADDFB — keeps it.
                // Creating a 2D resource over the top would either collide with
                // the 3D one or double-attach its backing.
                Some(r) => r,
                None => {
                    let r = gpu.alloc_resource_id();
                    gpu.create_resource_2d(r, add.width, add.height);
                    gpu.attach_backing(r, phys_addr as u64, add.width * add.height * 4);
                    r
                }
            };
            fb.handles[0] = res_id;
            bound_res = Some(res_id);
        }
        if let Some(r) = bound_res { fb_set_resource_id(add.handle, r); }

        let fb_id = fb.id().0;
        device.framebuffers.insert(fb.id(), fb);
        add.fb_id = fb_id;

        Ok(0)
    }
    fn std_handle_set_crtc(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let set = unsafe { &mut *(arg as *mut drm_mode_crtc) };
        let crtc_id = DrmObjectId(set.crtc_id);
        let fb_id_val = set.fb_id;
        let fb_id = Some(DrmObjectId(fb_id_val));
        let mode = Some(DrmModeInfo::new(set.mode.hdisplay, set.mode.vdisplay, set.mode.vrefresh));

        device.set_crtc(crtc_id, mode, set.x, set.y, &[], fb_id)?;

        // drmModeSetCrtc semantics require the given framebuffer to be presented
        // immediately. device.set_crtc only updates internal CRTC/plane state; it
        // does NOT push pixels to the virtio-gpu scanout (unlike handle_flip_page).
        // A compositor that mode-sets and then waits for the frame to appear (or
        // for the vblank of a follow-up page-flip) would otherwise never see its
        // first frame — the display stays on the stale kernel console. smithay's
        // legacy surface, for instance, does set_crtc(fb) then a page_flip(fb);
        // without presenting here the first frame is invisible until (and unless)
        // that flip lands. Mirror handle_flip_page's software-scale + gpu.flush so
        // the framebuffer is scanned out now. kmscube's continuous page-flips make
        // it immune to this gap; anvil/cosmic-comp are not.
        if fb_id_val != 0 {
            let mut src_w = set.mode.hdisplay as u32;
            let mut src_h = set.mode.vdisplay as u32;
            if let Some(fb) = device.get_framebuffer(DrmObjectId(fb_id_val)) {
                src_w = fb.width;
                src_h = fb.height;
            }
            let flip_args = [fb_id_val, 0u32, src_w, src_h];
            let _ = self.handle_flip_page(device, flip_args.as_ptr() as usize);
        }
        Ok(0)
    }

    fn std_handle_page_flip(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let flip = unsafe { &mut *(arg as *mut drm_mode_crtc_page_flip) };

        let mut src_w = 320;
        let mut src_h = 200;
        if let Some(fb) = device.get_framebuffer(DrmObjectId(flip.fb_id)) {
            src_w = fb.width;
            src_h = fb.height;
        }

        let flags = flip.flags;
        let user_data = flip.user_data;
        let crtc_id = flip.crtc_id;
        let flip_args = [flip.fb_id, flip.flags, src_w, src_h];
        let t0 = if DRM_STATS { crate::snd::monotonic_us() } else { 0 };
        let r = self.handle_flip_page(device, flip_args.as_ptr() as usize);
        if DRM_STATS {
            FLIP_US_TOTAL.fetch_add(crate::snd::monotonic_us().wrapping_sub(t0), Ordering::Relaxed);
        }
        // On success, if the client asked for a completion event, queue one for
        // throttled delivery (drm_tick). This is what lets a compositor schedule
        // the next frame.
        if r.is_ok() { FLIPS_SUBMITTED.fetch_add(1, Ordering::Relaxed); }
        if r.is_ok() && (flags & DRM_MODE_PAGE_FLIP_EVENT != 0) {
            queue_flip_event(crtc_id, user_data);
        }
        r
    }

    // ── K4 IOCTL handlers (copy-in-before-lock; see handle_ioctl note) ─────────

    /// Retire a dumb buffer's gem handle and drop the reference it held.
    ///
    /// The pages go back to the buddy allocator only once the LAST reference is
    /// gone, which for an exported buffer is when the dmabuf fd's tmpfs slot
    /// dies. Freeing them here unconditionally — what this used to do — is the
    /// pre-existing half of the use-after-free described under `BO LIFETIME`:
    /// `read()` on a still-open exported fd walked frames the allocator had
    /// already handed to someone else.
    ///
    /// Idempotent. GEM_CLOSE calls this for every handle it is given (the two
    /// handle spaces are disjoint but the call is unconditional), and a client
    /// may legitimately issue both DESTROY_DUMB and GEM_CLOSE; `handle_live`
    /// makes the handle's single reference droppable exactly once.
    fn free_dumb(handle: u32) {
        let mut map = DUMB_BUFFERS.lock();
        let zero = match map.get_mut(&handle) {
            Some(b) if !b.handle_live => return, // already retired
            Some(b) => {
                b.handle_live = false;
                b.refs = b.refs.saturating_sub(1);
                b.refs == 0
            }
            None => return,
        };
        let dead = if zero { map.remove(&handle) } else { None };
        drop(map);
        if let Some(b) = dead {
            mm::buddy::free(b.phys, b.order);
        }
    }

    /// DRM_IOCTL_GET_CAP — Smithay/Mesa best-effort capability probe.
    fn std_handle_get_cap(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let mut cap = unsafe { ptr::read_unaligned(arg as *const drm_get_cap) };
        cap.value = match cap.capability {
            DRM_CAP_DUMB_BUFFER => 1,
            DRM_CAP_TIMESTAMP_MONOTONIC => 1,
            DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
            DRM_CAP_ADDFB2_MODIFIERS => 0,
            // Mesa's softpipe (our only sw rasterizer) gates its dmabuf path on
            // drmGetCap(DRM_CAP_PRIME): with EXPORT clear, GBM's gbm_bo_create
            // falls back to create_dumb, which yields a gbm_bo whose ->image is
            // NULL. dri2_drm_image_get_buffers then hands that NULL back with the
            // BACK bit set, and dri2_allocate_textures NULL-derefs it. Reporting
            // EXPORT (matching every real DRM driver, which always returns
            // IMPORT|EXPORT) routes GBM through the proper DRIimage path where the
            // bo is backed by a real gallium resource GL can render into. Our dumb
            // buffers are KMS-scanout-capable; kmscube consumes the KMS handle,
            // not a PRIME fd, so PRIME_HANDLE_TO_FD need not be implemented here.
            DRM_CAP_PRIME => DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT,
            DRM_CAP_ASYNC_PAGE_FLIP => 0,
            // The host's cursor is fixed at 64x64 and silently drops anything
            // else, so advertise exactly that. Reporting 0 (the old `_ => 0`)
            // makes smithay skip the cursor plane entirely.
            DRM_CAP_CURSOR_WIDTH => 64,
            DRM_CAP_CURSOR_HEIGHT => 64,
            // Binary sync objects: CREATE/DESTROY/WAIT/RESET/SIGNAL are real.
            // v3d's Vulkan and gallium drivers use them unconditionally.
            DRM_CAP_SYNCOBJ => 1,
            // Timeline sync objects: NOT implemented, and this zero is the
            // load-bearing half of that decision, not a placeholder. It is the
            // flag Mesa's `vk_sync_timeline` reads to fall back to emulating
            // timelines on binary syncobjs, and the one v3dv reads into
            // `caps.multisync`. Reporting 1 here while TIMELINE_WAIT answers
            // ENOSYS would make clients take a path that cannot work.
            DRM_CAP_SYNCOBJ_TIMELINE => 0,
            // Unknown caps: value 0 + success. Smithay probes many optional caps
            // and treats an ioctl error differently from "cap == 0".
            _ => 0,
        };
        unsafe { ptr::write_unaligned(arg as *mut drm_get_cap, cap); }
        Ok(0)
    }

    /// DRM_IOCTL_SET_CLIENT_CAP — refuse ATOMIC so Smithay selects the legacy
    /// (non-atomic) KMS path we implement; accept UNIVERSAL_PLANES and others.
    fn std_handle_set_client_cap(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let cap = unsafe { ptr::read_unaligned(arg as *const drm_set_client_cap) };
        match cap.capability {
            DRM_CLIENT_CAP_ATOMIC => {
                ATOMIC_CLIENT.store(cap.value != 0, Ordering::Relaxed);
                Ok(0)
            }
            DRM_CLIENT_CAP_UNIVERSAL_PLANES => Ok(0),
            // Private, and the ONE way the v3d backend is armed. See "Backend
            // selection". Deliberately in the client-cap space rather than as a
            // new ioctl number: this entry point exists to let a client say
            // which contract it wants, unknown ids here have always been
            // accepted-and-ignored so nothing existing can be broken by adding
            // one, and no Mesa/libdrm caller sets a capability above 6.
            DRM_CLIENT_CAP_LEANDROS_V3D => {
                v3d_set_enabled(cap.value != 0);
                crate::pci::serial_debug(if cap.value != 0 {
                    "[DRM] v3d backend ARMED (card0 and renderD128 now report \"v3d\")\n"
                } else {
                    "[DRM] v3d backend disarmed\n"
                });
                Ok(0)
            }
            _ => Ok(0),
        }
    }

    /// DRM_IOCTL_MODE_OBJ_GETPROPERTIES — report zero properties on every object.
    ///
    /// No KMS object-property model exists yet. The only consumer on the legacy
    /// path is smithay's LegacyDrmDevice reset (set_connector_state), which
    /// enumerates a connector's properties solely to find "DPMS" and toggle it;
    /// an empty set makes that loop a no-op (leaving the connector in its current
    /// state) and lets LegacyDrmDevice init proceed. The caller passes its buffer
    /// capacity in count_props (offset 16); overwrite it with the number actually
    /// written (0). Runs synchronously in the caller's address space and takes no
    /// device lock, so a plain unaligned write is safe (82d0cc3 concerns only
    /// apply to user-memory access under a spinlock).
    fn std_handle_obj_get_properties(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // struct drm_mode_obj_get_properties { u64 props_ptr@0; u64 prop_values_ptr@8;
        //   u32 count_props@16; u32 obj_id@20; u32 obj_type@24; }
        // The single primary plane (obj_id 30) exposes exactly one property, "type"
        // = PRIMARY, which smithay's planes() requires (plane_type panics on absence).
        // Every other object (connectors etc.) reports zero properties — enough for
        // the legacy DPMS reset, which just enumerates and finds nothing to toggle.
        let obj_id   = unsafe { ptr::read_unaligned((arg + 20) as *const u32) };
        let obj_type = unsafe { ptr::read_unaligned((arg + 24) as *const u32) };
        let ids = object_props(obj_id, obj_type);

        let props_ptr = unsafe { ptr::read_unaligned(arg as *const u64) };
        let vals_ptr  = unsafe { ptr::read_unaligned((arg + 8) as *const u64) };
        let cap       = unsafe { ptr::read_unaligned((arg + 16) as *const u32) };
        if props_ptr != 0 && vals_ptr != 0 && cap as usize >= ids.len() {
            for (i, &pid) in ids.iter().enumerate() {
                unsafe {
                    ptr::write_unaligned((props_ptr as *mut u32).add(i), pid);
                    ptr::write_unaligned(
                        (vals_ptr as *mut u64).add(i),
                        Self::current_prop_value(obj_id, pid),
                    );
                }
            }
        }
        // The true count goes back on every pass — the caller sizes its arrays
        // from the first call and re-reads the count on the second.
        unsafe { ptr::write_unaligned((arg + 16) as *mut u32, ids.len() as u32); }
        Ok(0)
    }

    /// Current value of `prop_id` on `obj_id`. Only the values a compositor
    /// actually reads back matter here; the rest report 0, which is the correct
    /// "unset" state for an unconfigured plane.
    fn current_prop_value(obj_id: u32, prop_id: u32) -> u64 {
        match prop_id {
            PROP_TYPE => match obj_id {
                DRM_CURSOR_PLANE_ID => DRM_PLANE_TYPE_CURSOR as u64,
                _ => DRM_PLANE_TYPE_PRIMARY as u64,
            },
            _ => 0,
        }
    }

    /// DRM_IOCTL_MODE_GETPLANERESOURCES — expose a single (primary) plane.
    /// smithay's DrmCompositor needs at least one primary plane bound to the crtc
    /// to build a scanout surface; without it connector setup bails and nothing is
    /// ever composited. struct drm_mode_get_plane_res { u64 plane_id_ptr@0; u32 count_planes@8; }.
    fn std_handle_get_plane_resources(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        const PLANES: [u32; 2] = [DRM_PLANE_ID, DRM_CURSOR_PLANE_ID];
        let ptr_planes = unsafe { ptr::read_unaligned(arg as *const u64) };
        let cap = unsafe { ptr::read_unaligned((arg + 8) as *const u32) };
        if ptr_planes != 0 && cap as usize >= PLANES.len() {
            unsafe {
                ptr::copy_nonoverlapping(PLANES.as_ptr(), ptr_planes as *mut u32, PLANES.len());
            }
        }
        unsafe { ptr::write_unaligned((arg + 8) as *mut u32, PLANES.len() as u32); }
        Ok(0)
    }

    /// DRM_IOCTL_MODE_GETPLANE — describe the primary plane. It is usable on crtc
    /// index 0 (possible_crtcs bit 0) and advertises linear XRGB/ARGB8888.
    /// struct drm_mode_get_plane { u32 plane_id@0; crtc_id@4; fb_id@8; possible_crtcs@12;
    ///   gamma_size@16; count_format_types@20; u64 format_type_ptr@24; }.
    fn std_handle_get_plane(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        const XR24: u32 = 0x3432_5258;
        const AR24: u32 = 0x3432_5241;
        const PRIMARY_FORMATS: [u32; 2] = [XR24, AR24];
        // The cursor plane advertises AR24 only: the host composites it as an
        // overlay and needs the alpha channel. Offering XR24 invites smithay to
        // pick a format the host would render fully opaque.
        const CURSOR_FORMATS: [u32; 1] = [AR24];

        let plane_id = unsafe { ptr::read_unaligned(arg as *const u32) };
        let formats: &[u32] = match plane_id {
            DRM_CURSOR_PLANE_ID => &CURSOR_FORMATS,
            DRM_PLANE_ID => &PRIMARY_FORMATS,
            _ => return Err(DriverError::NotFound),
        };
        unsafe {
            ptr::write_unaligned((arg + 4) as *mut u32, 0);  // crtc_id: not currently bound
            ptr::write_unaligned((arg + 8) as *mut u32, 0);  // fb_id
            ptr::write_unaligned((arg + 12) as *mut u32, 1); // possible_crtcs: crtc index 0
            ptr::write_unaligned((arg + 16) as *mut u32, 0); // gamma_size
        }
        let cap = unsafe { ptr::read_unaligned((arg + 20) as *const u32) };
        let fmt_ptr = unsafe { ptr::read_unaligned((arg + 24) as *const u64) };
        if fmt_ptr != 0 && cap as usize >= formats.len() {
            unsafe { ptr::copy_nonoverlapping(formats.as_ptr(), fmt_ptr as *mut u32, formats.len()); }
        }
        unsafe { ptr::write_unaligned((arg + 20) as *mut u32, formats.len() as u32); }
        Ok(0)
    }

    /// DRM_IOCTL_MODE_GETPROPERTY — only the plane "type" property is defined.
    /// smithay's plane_type() reads just the property name; leaving the value/enum
    /// counts at 0 makes drm-ffi's get_property a single-pass call (no array fetch).
    /// struct drm_mode_get_property { u64 values_ptr@0; u64 enum_blob_ptr@8; u32 prop_id@16;
    ///   u32 flags@20; char name[32]@24; u32 count_values@56; u32 count_enum_blobs@60; }.
    fn std_handle_get_property(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let prop_id = unsafe { ptr::read_unaligned((arg + 16) as *const u32) };
        let def = match prop_def(prop_id) {
            Some(d) => d,
            None => return Err(DriverError::Unsupported),
        };

        let count = prop_value_count(def.kind);
        let values = prop_values(def.kind);
        let values_ptr = unsafe { ptr::read_unaligned(arg as *const u64) };
        let in_count = unsafe { ptr::read_unaligned((arg + 56) as *const u32) };

        unsafe {
            ptr::write_unaligned((arg + 20) as *mut u32, prop_flags(def.kind));
            // name[32] — zero the field first so a shorter name never inherits
            // the caller's stack bytes.
            ptr::write_bytes((arg + 24) as *mut u8, 0, 32);
            let n = def.name.len().min(32);
            ptr::copy_nonoverlapping(def.name.as_ptr(), (arg + 24) as *mut u8, n);
        }

        // Fill the value array only when the caller supplied one big enough.
        // drm-rs indexes values[0] (OBJECT) and values[0..2] (RANGE and
        // SIGNED_RANGE) without checking the count, so the count reported here
        // must always be the real one — and identical on both passes, because
        // drm-ffi does Vec::set_len from the *second* call's count.
        if values_ptr != 0 && in_count >= count && count > 0 {
            unsafe {
                ptr::copy_nonoverlapping(values.as_ptr(), values_ptr as *mut u64, count as usize);
            }
        }
        unsafe {
            ptr::write_unaligned((arg + 56) as *mut u32, count);
            ptr::write_unaligned((arg + 60) as *mut u32, 0); // count_enum_blobs
        }
        Ok(0)
    }

    // ── Property blobs ───────────────────────────────────────────────────────

    /// DRM_IOCTL_MODE_CREATEPROPBLOB.
    /// struct drm_mode_create_blob { u64 data@0; u32 length@8; u32 blob_id@12; }
    fn std_handle_create_blob(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let data = unsafe { ptr::read_unaligned(arg as *const u64) };
        let length = unsafe { ptr::read_unaligned((arg + 8) as *const u32) } as usize;
        if data == 0 || length == 0 || length > 64 * 1024 {
            return Err(DriverError::InvalidParameter);
        }
        // Copy the user array in BEFORE taking BLOBS: a demand-paging fault on
        // `data` with that lock held is the 82d0cc3 freeze class. This ordering
        // is the whole reason the copy is not folded into the insert below.
        let mut buf = vec![0u8; length];
        unsafe { ptr::copy_nonoverlapping(data as *const u8, buf.as_mut_ptr(), length); }

        let id = NEXT_BLOB_ID.fetch_add(1, Ordering::Relaxed);
        BLOBS.lock().insert(id, buf);
        if DRM_STATS { BLOBS_CREATED.fetch_add(1, Ordering::Relaxed); }
        unsafe { ptr::write_unaligned((arg + 12) as *mut u32, id); }
        Ok(0)
    }

    /// DRM_IOCTL_MODE_DESTROYPROPBLOB. struct { u32 blob_id@0; }
    fn std_handle_destroy_blob(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let id = unsafe { ptr::read_unaligned(arg as *const u32) };
        match BLOBS.lock().remove(&id) {
            Some(_) => Ok(0),
            None => Err(DriverError::NotFound),
        }
    }

    /// DRM_IOCTL_MODE_GETPROPBLOB — two-pass like GETPROPERTY.
    /// struct drm_mode_get_blob { u32 blob_id@0; u32 length@4; u64 data@8; }
    fn std_handle_get_blob(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let id = unsafe { ptr::read_unaligned(arg as *const u32) };
        let in_len = unsafe { ptr::read_unaligned((arg + 4) as *const u32) } as usize;
        let data = unsafe { ptr::read_unaligned((arg + 8) as *const u64) };

        // Copy out from under the lock: a demand-paging fault on `data` taken
        // with BLOBS held is the 82d0cc3 freeze class.
        let blob = match BLOBS.lock().get(&id) {
            Some(b) => b.clone(),
            None => return Err(DriverError::NotFound),
        };
        if data != 0 && in_len >= blob.len() {
            unsafe { ptr::copy_nonoverlapping(blob.as_ptr(), data as *mut u8, blob.len()); }
        }
        unsafe { ptr::write_unaligned((arg + 4) as *mut u32, blob.len() as u32); }
        Ok(0)
    }

    // ── Atomic modesetting ───────────────────────────────────────────────────

    /// DRM_IOCTL_MODE_ATOMIC.
    /// struct drm_mode_atomic { u32 flags@0; u32 count_objs@4; u64 objs_ptr@8;
    ///   u64 count_props_ptr@16; u64 props_ptr@24; u64 prop_values_ptr@32;
    ///   u64 reserved@40; u64 user_data@48; }
    ///
    /// `objs_ptr` carries bare object ids with no type tag. Our synthetic crtc,
    /// connector and encoder all have id 1, so the type is recovered from the
    /// property id instead — the property-id ranges are disjoint per object
    /// class exactly so this is unambiguous.
    fn std_handle_atomic(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }

        // Copy the entire request into kernel memory BEFORE taking any lock: a
        // demand-paging fault under the DRM spinlock is the 82d0cc3 freeze.
        let flags = unsafe { ptr::read_unaligned(arg as *const u32) };
        let count_objs = unsafe { ptr::read_unaligned((arg + 4) as *const u32) } as usize;
        let objs_ptr = unsafe { ptr::read_unaligned((arg + 8) as *const u64) };
        let counts_ptr = unsafe { ptr::read_unaligned((arg + 16) as *const u64) };
        let props_ptr = unsafe { ptr::read_unaligned((arg + 24) as *const u64) };
        let vals_ptr = unsafe { ptr::read_unaligned((arg + 32) as *const u64) };
        let user_data = unsafe { ptr::read_unaligned((arg + 48) as *const u64) };

        // An empty commit is legal and is a no-op.
        if count_objs == 0 { return Ok(0); }
        // We expose 4 objects; anything wildly larger is a malformed request.
        if count_objs > 64 || objs_ptr == 0 || counts_ptr == 0 {
            return Err(DriverError::InvalidParameter);
        }

        let mut objs = vec![0u32; count_objs];
        let mut counts = vec![0u32; count_objs];
        unsafe {
            ptr::copy_nonoverlapping(objs_ptr as *const u32, objs.as_mut_ptr(), count_objs);
            ptr::copy_nonoverlapping(counts_ptr as *const u32, counts.as_mut_ptr(), count_objs);
        }

        let mut total = 0usize;
        for &c in &counts {
            total = total.saturating_add(c as usize);
        }
        if total > 1024 { return Err(DriverError::InvalidParameter); }
        if total > 0 && (props_ptr == 0 || vals_ptr == 0) {
            return Err(DriverError::InvalidParameter);
        }
        let mut pids = vec![0u32; total];
        let mut pvals = vec![0u64; total];
        if total > 0 {
            unsafe {
                ptr::copy_nonoverlapping(props_ptr as *const u32, pids.as_mut_ptr(), total);
                ptr::copy_nonoverlapping(vals_ptr as *const u64, pvals.as_mut_ptr(), total);
            }
        }

        // ── Fold the flattened (obj, prop, value) triples into a request ──
        let mut primary = AtomicPlaneReq::default();
        let mut cursor = AtomicPlaneReq::default();
        let mut want_active: Option<u64> = None;
        let mut want_mode: Option<u64> = None;
        let mut want_conn_crtc: Option<u64> = None;

        let mut k = 0usize;
        for (i, &obj) in objs.iter().enumerate() {
            for _ in 0..counts[i] {
                let pid = pids[k];
                let val = pvals[k];
                k += 1;

                match pid {
                    // ── crtc ──
                    PROP_ACTIVE => {
                        if obj != DRM_CRTC_ID { return Err(DriverError::InvalidParameter); }
                        want_active = Some(val);
                    }
                    PROP_MODE_ID => {
                        if obj != DRM_CRTC_ID { return Err(DriverError::InvalidParameter); }
                        want_mode = Some(val);
                    }
                    // ── connector ──
                    PROP_CONN_CRTC_ID => {
                        if obj != DRM_CONNECTOR_ID { return Err(DriverError::InvalidParameter); }
                        want_conn_crtc = Some(val);
                    }
                    // ── planes ──
                    _ => {
                        let p = match obj {
                            DRM_PLANE_ID => &mut primary,
                            DRM_CURSOR_PLANE_ID => {
                                if DRM_STATS {
                                    CURSOR_PLANE_SEEN.fetch_add(1, Ordering::Relaxed);
                                }
                                &mut cursor
                            }
                            _ => return Err(DriverError::InvalidParameter),
                        };
                        match pid {
                            PROP_TYPE => {} // immutable; ignore writes
                            PROP_PLANE_CRTC_ID => p.crtc_id = Some(val as u32),
                            PROP_FB_ID => p.fb_id = Some(val as u32),
                            // SRC_* are 16.16 fixed point.
                            PROP_SRC_X => p.src_x = Some((val >> 16) as u32),
                            PROP_SRC_Y => p.src_y = Some((val >> 16) as u32),
                            PROP_SRC_W => p.src_w = Some((val >> 16) as u32),
                            PROP_SRC_H => p.src_h = Some((val >> 16) as u32),
                            PROP_CRTC_X => p.crtc_x = Some(val as i32),
                            PROP_CRTC_Y => p.crtc_y = Some(val as i32),
                            PROP_CRTC_W => p.crtc_w = Some(val as u32),
                            PROP_CRTC_H => p.crtc_h = Some(val as u32),
                            PROP_FB_DAMAGE_CLIPS => p.damage_blob = Some(val as u32),
                            _ => return Err(DriverError::InvalidParameter),
                        }
                    }
                }
            }
        }

        // ── Modeset gating ──
        // Without ALLOW_MODESET a request that *changes* ACTIVE, MODE_ID or the
        // connector's CRTC_ID must be rejected. That rejection is precisely how
        // smithay discovers it needs a modeset, so getting it wrong either
        // wedges startup or makes every frame a modeset.
        let allow_modeset = flags & DRM_MODE_ATOMIC_ALLOW_MODESET != 0;
        let test_only = flags & DRM_MODE_ATOMIC_TEST_ONLY != 0;
        let cur_active = CRTC_ACTIVE.load(Ordering::Relaxed) as u64;
        let cur_mode = CRTC_MODE_BLOB.load(Ordering::Relaxed) as u64;
        let cur_conn = CONN_CRTC.load(Ordering::Relaxed) as u64;
        let changes_modeset = want_active.map_or(false, |v| v != cur_active)
            || want_mode.map_or(false, |v| v != cur_mode)
            || want_conn_crtc.map_or(false, |v| v != cur_conn);
        if changes_modeset && !allow_modeset {
            return Err(DriverError::InvalidParameter);
        }

        // A MODE_ID blob must exist if one was named.
        if let Some(mode_blob) = want_mode {
            if mode_blob != 0 && !BLOBS.lock().contains_key(&(mode_blob as u32)) {
                return Err(DriverError::InvalidParameter);
            }
        }

        // TEST_ONLY: everything above is the validation. Never present.
        // smithay issues these constantly; a spurious failure here silently
        // disables the cursor plane rather than producing a visible error.
        if test_only {
            if DRM_STATS { ATOMIC_TESTS.fetch_add(1, Ordering::Relaxed); }
            return Ok(0);
        }

        if let Some(v) = want_active { CRTC_ACTIVE.store(v as u32, Ordering::Relaxed); }
        if let Some(v) = want_mode { CRTC_MODE_BLOB.store(v as u32, Ordering::Relaxed); }
        if let Some(v) = want_conn_crtc { CONN_CRTC.store(v as u32, Ordering::Relaxed); }

        // ── Present ──
        //
        // FB_DAMAGE_CLIPS bounds the pixel work. Resolve the blob to rects here,
        // BEFORE the DRM device lock is taken: BLOBS and that mutex must never
        // be nested, in either order.
        let mut presented = false;
        if let Some(fb_id) = primary.fb_id {
            if fb_id != 0 {
                let blob = primary.damage_blob.unwrap_or(0);
                // A modeset repaints unconditionally — a clip list describes the
                // surface as it was under the previous mode and means nothing
                // after it. Same for the first present of a framebuffer.
                let full_repaint = changes_modeset || want_mode.is_some();
                let unchanged = !full_repaint
                    && blob != 0
                    && fb_id == LAST_PRIMARY_FB.load(Ordering::Relaxed)
                    && blob == LAST_PRIMARY_DAMAGE.load(Ordering::Relaxed);
                let rects = if full_repaint { None } else { damage_rects(blob) };

                if unchanged {
                    // smithay re-sent the previous plane config byte for byte:
                    // its damage tracker found nothing to draw. Present nothing;
                    // the completion event below is still owed and still sent.
                    if DRM_STATS { DAMAGE_SKIP.fetch_add(1, Ordering::Relaxed); }
                } else {
                    let t0 = if DRM_STATS { crate::snd::monotonic_us() } else { 0 };
                    let r = {
                        let d = get_drm_device();
                        let mut g = d.lock();
                        match rects.as_deref() {
                            Some(clips) => {
                                if DRM_STATS {
                                    DAMAGE_RECT.fetch_add(1, Ordering::Relaxed);
                                    DAMAGE_PX.fetch_add(damage_area(clips), Ordering::Relaxed);
                                }
                                match present_blob_fb(&mut g, DrmObjectId(fb_id), Some(clips)) {
                                    Some(r) => r.map(|_| 0usize),
                                    None => g.present_damaged(DrmObjectId(fb_id), clips).map(|_| 0usize),
                                }
                            }
                            None => {
                                if DRM_STATS { DAMAGE_FULL.fetch_add(1, Ordering::Relaxed); }
                                let (mut src_w, mut src_h) = (320u32, 200u32);
                                if let Some(fb) = g.get_framebuffer(DrmObjectId(fb_id)) {
                                    src_w = fb.width;
                                    src_h = fb.height;
                                }
                                let flip_args = [fb_id, 0u32, src_w, src_h];
                                self.handle_flip_page(&mut g, flip_args.as_ptr() as usize)
                            }
                        }
                    };
                    if DRM_STATS {
                        FLIP_US_TOTAL
                            .fetch_add(crate::snd::monotonic_us().wrapping_sub(t0), Ordering::Relaxed);
                    }
                    r?;
                    // Counts presents that moved pixels, not atomic commits. It
                    // used to be the same number by construction, which is what
                    // made "smithay flips the primary every frame" unfalsifiable.
                    FLIPS_SUBMITTED.fetch_add(1, Ordering::Relaxed);
                    if DRM_STATS {
                        let now = crate::snd::monotonic_us();
                        mm::gap2::s("[FLIP] t_us="); mm::gap2::h(now as usize);
                        mm::gap2::kv(" pid=", sched::current_pid() as usize);
                        mm::gap2::kv(" inp_age_us=", now.wrapping_sub(evdev_server::last_push_us()) as usize);
                        mm::gap2::kv(" n=", FLIPS_SUBMITTED.load(Ordering::Relaxed) as usize);
                        mm::gap2::nl();
                    }
                    presented = true;
                }

                LAST_PRIMARY_FB.store(fb_id, Ordering::Relaxed);
                LAST_PRIMARY_DAMAGE.store(blob, Ordering::Relaxed);
            }
        }

        Self::commit_cursor_plane(&cursor);
        if DRM_STATS { ATOMIC_COMMITS.fetch_add(1, Ordering::Relaxed); }

        // A commit that only reconfigured the cursor plane still owes the
        // client its completion event, otherwise smithay's frame loop stalls.
        let _ = presented;
        if flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            queue_flip_event(DRM_CRTC_ID, user_data);
        }
        Ok(0)
    }

    /// Apply the cursor plane's share of an atomic commit to the virtio-gpu
    /// cursor queue.
    ///
    /// The whole point of the plane is that repositioning is free, so pixels
    /// move only when the commit actually names a different framebuffer.
    /// A commit that carries CRTC_X/CRTC_Y and nothing else — smithay's
    /// "repositioning cursor plane", by far the common case — issues a single
    /// MOVE_CURSOR and touches no pixel data at all.
    fn commit_cursor_plane(req: &AtomicPlaneReq) {
        // Unbinding the plane (either the crtc or the fb going to 0) hides it.
        let unbound = req.crtc_id == Some(0) || req.fb_id == Some(0);
        if unbound {
            LAST_CURSOR_FB.store(0, Ordering::Relaxed);
            crate::virtio_gpu::cursor_hide();
            return;
        }

        // Position: CRTC_X/CRTC_Y already carry the hotspot baked in (smithay
        // does not send a hotspot property), so the host hotspot stays (0, 0).
        // Negative coordinates are clamped — the host takes unsigned values.
        let x = req.crtc_x.unwrap_or(0).max(0) as u32;
        let y = req.crtc_y.unwrap_or(0).max(0) as u32;

        match req.fb_id {
            // A framebuffer we have not uploaded yet: copy its pixels into the
            // cursor resource and publish it.
            Some(fb_id) if fb_id != LAST_CURSOR_FB.load(Ordering::Relaxed) => {
                let (phys, w, h) = {
                    let dev = get_drm_device();
                    let g = dev.lock();
                    match g.get_framebuffer(DrmObjectId(fb_id)) {
                        Some(fb) => (fb.physical_addresses[0], fb.width, fb.height),
                        None => return,
                    }
                };
                // ADDFB2 falls back to phys 0 for buffers it cannot resolve
                // (the DRIimage path rather than a dumb buffer). Uploading from
                // address 0 would push garbage to the host, so refuse loudly
                // and leave the previous cursor in place.
                if phys == 0 {
                    crate::pci::serial_debug("[DRM] cursor fb has no physical backing\n");
                    return;
                }
                let bytes = (w as usize)
                    .saturating_mul(h as usize)
                    .saturating_mul(4)
                    .min((crate::virtio_gpu::CURSOR_W * crate::virtio_gpu::CURSOR_H * 4) as usize);
                let src = unsafe {
                    slice::from_raw_parts(mm::phys_to_virt(phys as usize) as *const u8, bytes)
                };
                if crate::virtio_gpu::cursor_update(src, 0, 0, x, y) {
                    LAST_CURSOR_FB.store(fb_id, Ordering::Relaxed);
                    if DRM_STATS { CURSOR_UPDATES.fetch_add(1, Ordering::Relaxed); }
                }
            }
            // Same framebuffer, or none named: position only. No pixel traffic.
            _ => {
                if req.crtc_x.is_some() || req.crtc_y.is_some() {
                    if crate::virtio_gpu::cursor_move(x, y) {
                        if DRM_STATS { CURSOR_MOVES.fetch_add(1, Ordering::Relaxed); }
                    }
                }
            }
        }
    }

    /// DRM_IOCTL_GET_MAGIC — single-seat stub: return a nonzero magic.
    fn std_handle_get_magic(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let mut a = unsafe { ptr::read_unaligned(arg as *const drm_auth) };
        a.magic = 1;
        unsafe { ptr::write_unaligned(arg as *mut drm_auth, a); }
        Ok(0)
    }

    /// DRM_IOCTL_GEM_CLOSE — free the handle's backing (Ok even if unknown).
    ///
    /// Open-scoped for blob BOs: a handle belonging to another open is left
    /// alone, and the call still reports success, because from this open's point
    /// of view the handle simply names nothing — which is the same answer it
    /// already gave for a handle that was never allocated. Dumb buffers are not
    /// scoped (see `open_may_reach`).
    fn std_handle_gem_close(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let c = unsafe { ptr::read_unaligned(arg as *const drm_gem_close) };
        Self::gem_handle_delete(c.handle, open_id);
        Ok(0)
    }

    /// Retire a blob gem handle unconditionally and drop the reference it held.
    /// The object — host resource, window reservation, guest pages — survives
    /// until the last reference goes, which may be an exported dmabuf fd's.
    ///
    /// The removal is a statement of its own so the `BLOB_BUFFERS` guard is
    /// DEAD before `blob_unref` takes `VIRTIO_GPU` and busy-spins on a device
    /// round-trip. Written as `if let Some(..) = LOCK.remove(..)` it was not:
    /// the temporary guard in an `if let` scrutinee lives to the end of the
    /// whole `if let`, so the old body ran the entire teardown with the handle
    /// map held. `free_blob_owned` was already written this way for exactly
    /// that reason; this is the same shape.
    fn free_blob(handle: u32) {
        let taken = BLOB_BUFFERS.lock().remove(&handle);
        if let Some(h) = taken {
            blob_unref(h.obj, h.ctx);
        }
    }

    /// GEM_CLOSE's entry point: free `handle` only if `open_id` may reach it.
    /// The ownership test and the removal happen under one acquisition of
    /// BLOB_BUFFERS, so two opens racing to close the same handle cannot both
    /// pass the test and both drop the object's reference.
    fn free_blob_owned(handle: u32, open_id: u32) {
        let taken = {
            let mut map = BLOB_BUFFERS.lock();
            let may = match map.get(&handle) {
                Some(h) => open_may_reach(open_id, h.owner),
                None => false,
            };
            if may { map.remove(&handle) } else { None }
        };
        if let Some(h) = taken {
            blob_unref(h.obj, h.ctx);
        }
    }

    /// The one handle-retirement path. Upstream, GEM_CLOSE and MODE_DESTROY_DUMB
    /// are literally the same operation (both land in drm_gem_handle_delete, and
    /// DESTROY_DUMB does NOT check that the handle names a dumb buffer). They are
    /// the same operation here too, so the two ioctls cannot drift apart again.
    ///
    /// Exactly one reference is dropped, from whichever registry minted the
    /// handle: `free_dumb` is idempotent on an already-retired dumb handle, and
    /// `free_blob_owned` tests ownership and removes under a single
    /// `BLOB_BUFFERS` acquisition, so two opens racing here cannot both unref.
    /// The two handle spaces are disjoint, so calling both is not a double drop.
    fn gem_handle_delete(handle: u32, open_id: u32) {
        Self::free_dumb(handle);
        Self::free_blob_owned(handle, open_id);
    }

    /// DRM_IOCTL_MODE_DESTROY_DUMB — retire the handle's gem handle.
    ///
    /// Not dumb-only, despite the name. Mesa's kms-dri winsys releases *every*
    /// handle it owns this way — the ones it minted with CREATE_DUMB and the
    /// ones it minted with drmPrimeFDToHandle alike — and `GEM_CLOSE` appears
    /// nowhere in that file (`kms_dri_sw_winsys.c:295`, return value discarded).
    /// Routing through `gem_handle_delete` matches upstream, where DESTROY_DUMB
    /// does not check that the handle names a dumb buffer either.
    ///
    /// An unknown handle is `Ok(0)`, not `-ENOENT`, for three reasons.
    /// `servers/drm/src/lib.rs:237` collapses every `DriverError` to
    /// `err_reply(-1)`, which the VFS reports as EPERM, so `-ENOENT` is not even
    /// expressible without first plumbing real errnos through the port protocol.
    /// Mesa discards the return value at the call site above, so the distinction
    /// would reach no caller. And an error here would contradict the idempotence
    /// invariant landed in `49399f9` (see `free_dumb`), under which issuing both
    /// DESTROY_DUMB and GEM_CLOSE on one handle is legitimate and drops one
    /// reference in total — the second call must succeed and do nothing.
    fn std_handle_destroy_dumb(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let d = unsafe { ptr::read_unaligned(arg as *const drm_mode_destroy_dumb) };
        Self::gem_handle_delete(d.handle, open_id);
        Ok(0)
    }

    /// DRM_IOCTL_MODE_ADDFB2 — LINEAR only, plane 0. Same internal path as ADDFB.
    fn std_handle_addfb2(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let mut cmd2 = unsafe { ptr::read_unaligned(arg as *const drm_mode_fb_cmd2) };

        let handle = cmd2.handles[0];
        let width = cmd2.width;
        let height = cmd2.height;
        let pitch = if cmd2.pitches[0] != 0 { cmd2.pitches[0] } else { width * 4 };

        // A blob BO keeps the host resource it already has; see "Blob
        // framebuffers". Resolved before VIRTIO_GPU is taken, like the dumb
        // lookup below, so the BO maps never nest inside it.
        const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;
        let modifier = if cmd2.flags & DRM_MODE_FB_MODIFIERS != 0 { cmd2.modifier[0] } else { 0 };
        if let Some(fb) = blob_framebuffer(handle, open_id, width, height, pitch,
                                           cmd2.offsets[0], cmd2.pixel_format, modifier) {
            let fb_id = fb.id().0;
            {
                let dev = get_drm_device();
                let mut g = dev.lock();
                g.framebuffers.insert(fb.id(), fb);
            }
            cmd2.fb_id = fb_id;
            unsafe { ptr::write_unaligned(arg as *mut drm_mode_fb_cmd2, cmd2); }
            return Ok(0);
        }

        // A handle naming no BO this open may reach is upstream's
        // `drm_gem_object_lookup` miss, -ENOENT. It used to fall through to
        // `phys_addr = 0`, which built a 2D resource over guest address 0 and
        // asked the host to attach it — the `ctrl 0x106, error 0x1200`
        // refusals every Zink session used to log four of.
        let phys_addr = dumb_lookup(handle).map(|b| b.phys).ok_or(DriverError::NotFound)?;

        let mut fb = DrmFramebuffer::new(width, height, DrmFormat::Xrgb8888, handle, pitch);
        fb.physical_addresses[0] = phys_addr as u64;

        // Bind a virtio-gpu resource so SETCRTC/PAGE_FLIP/DIRTYFB can transfer the
        // CPU-rendered pixels to the host. (This locks VIRTIO_GPU, not the DRM
        // device — no user memory is touched here.)
        // See fb_resource_id for why this is no longer `handle + 10`.
        let existing_res = fb_resource_id(handle);
        let mut bound_res: Option<u32> = None;
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let res_id = match existing_res {
                Some(r) => r,
                None => {
                    let r = gpu.alloc_resource_id();
                    gpu.create_resource_2d(r, width, height);
                    gpu.attach_backing(r, phys_addr as u64, width * height * 4);
                    r
                }
            };
            fb.handles[0] = res_id;
            bound_res = Some(res_id);
        }
        if let Some(r) = bound_res { fb_set_resource_id(handle, r); }

        let fb_id = fb.id().0;
        {
            let dev = get_drm_device();
            let mut g = dev.lock();
            g.framebuffers.insert(fb.id(), fb);
        }

        cmd2.fb_id = fb_id;
        unsafe { ptr::write_unaligned(arg as *mut drm_mode_fb_cmd2, cmd2); }
        Ok(0)
    }

    /// DRM_IOCTL_MODE_RMFB — remove a framebuffer (arg is a bare u32 fb_id).
    fn std_handle_rmfb(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let fb_id = unsafe { ptr::read_unaligned(arg as *const u32) };
        let dev = get_drm_device();
        let mut g = dev.lock();
        let _ = g.remove_framebuffer(DrmObjectId(fb_id));
        Ok(0)
    }

    /// DRM_IOCTL_MODE_DIRTYFB — flush a CPU-rendered fb to the host display.
    fn std_handle_dirtyfb(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let cmd = unsafe { ptr::read_unaligned(arg as *const drm_mode_fb_dirty_cmd) };
        if DRM_STATS {
            DIRTYFB_CALLS.fetch_add(1, Ordering::Relaxed);
            DIRTYFB_CLIPS.fetch_add(cmd.num_clips as u64, Ordering::Relaxed);
        }
        let flush_args = {
            let dev = get_drm_device();
            let g = dev.lock();
            g.get_framebuffer(DrmObjectId(cmd.fb_id))
                .map(|fb| (fb.handles[0], fb.width, fb.height, fb.blob_res != 0 && fb.physical_addresses[0] == 0))
        };
        if let Some((res_id, w, h, host_blob)) = flush_args {
            if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
                if host_blob {
                    // Nothing to transfer: the host already holds the pixels.
                    gpu.resource_flush(res_id, 0, 0, w, h);
                } else {
                    gpu.flush(res_id, 0, 0, w, h);
                }
            }
        }
        Ok(0)
    }

    /// Handle FBIOGET_VSCREENINFO (0x4600)
    fn handle_fbioget_vscreeninfo(&self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }

        let (width, height, _pitch) = if let Some((_, w, h, p)) = crate::framebuffer::get_hardware_fb_info() {
            (w as u32, h as u32, p as u32)
        } else {
            (1280, 800, 1280 * 4)
        };

        let data = unsafe { slice::from_raw_parts_mut(arg as *mut u32, 8) };
        data[0] = width;
        data[1] = height;
        data[2] = width; // xres_virtual
        data[3] = height; // yres_virtual
        data[4] = 0; // xoffset
        data[5] = 0; // yoffset
        data[6] = 32; // bits_per_pixel
        data[7] = 0; // grayscale

        Ok(0)
    }

    /// Handle DRM_IOCTL_MMAP - returns physical address of framebuffer
    fn handle_ioctl_mmap(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg contains the requested physical address/offset
        let requested_phys = arg as u64;


        if requested_phys == 0 {
            // Default: return the hardware framebuffer
            extern "C" {
                fn vfs_get_framebuffer_base() -> u64;
            }

            let fb_base = unsafe { vfs_get_framebuffer_base() };
            if fb_base == 0 {
                return Err(DriverError::NotFound);
            }
            Ok(fb_base as usize)
        } else {
            // The mmap token was passed as the offset to mmap(). Under this
            // driver's scheme (see `virtgpu_handle_map`) the token IS a
            // guest-physical address, so the answer is the token itself — but
            // only for a token this device actually handed out, otherwise a
            // caller could map arbitrary physical memory through this device.
            //
            // Three token spaces resolve here:
            //   1. dumb buffers      — buddy base;
            //   2. guest-backed blobs — buddy base;
            //   3. host-visible blobs — `window.phys + win_off`, which is NOT
            //      guest RAM at all but a range of the virtio-gpu shared-memory
            //      BAR. Accepted anywhere inside the blob's own reservation so a
            //      partial map of a large blob works; `map_phys` is non-zero only
            //      after RESOURCE_MAP_BLOB succeeded, so an unmapped blob's
            //      window space is never reachable.
            //
            // The two scans are separate statements so only one map is ever
            // locked at a time (`||` in one expression keeps both temporaries
            // alive to the end of the statement). A dumb record whose gem
            // handle has been retired is skipped: it survives only to keep an
            // exported fd's frames alive, and that fd maps through its own
            // tmpfs VMO, never through this device token.
            let known_dumb = DUMB_BUFFERS
                .lock()
                .values()
                .any(|b| b.handle_live && b.phys == requested_phys as usize);
            let known_blob = || {
                BLOB_OBJS.lock().values().any(|b| {
                    (b.phys != 0 && b.phys == requested_phys as usize)
                        || (b.map_phys != 0
                            && requested_phys >= b.map_phys
                            && requested_phys - b.map_phys < b.size)
                })
            };
            if !known_dumb && !known_blob() {
                return Err(DriverError::InvalidParameter);
            }
            Ok(requested_phys as usize)
        }
    }

    /// Handle read operations (for events)
    pub fn handle_read(&mut self, _buffer: &mut [u8]) -> Result<usize, DriverError> {
        // For now, return no events
        // In a full implementation, this would return DRM events like vsync
        Ok(0)
    }

    /// Handle write operations (for framebuffer data)
    pub fn handle_write(&mut self, buffer: &[u8]) -> Result<usize, DriverError> {
        let device = get_drm_device();
        let mut device_lock = device.lock();

        // Prefer the plane's current fb, but fall back to the first available framebuffer.
        // The plane's fb_id is None until the first atomic commit (flip), so we need the
        // fallback so that write()-based rendering works before the first flip call.
        let fb_id = device_lock.planes.first().and_then(|p| p.fb_id)
            .or_else(|| device_lock.framebuffers.keys().next().copied());

        if let Some(fb_id) = fb_id {
            let (src_phys, fb_w, fb_h, fb_size) = {
                let fb = device_lock.get_framebuffer(fb_id).ok_or(DriverError::NotFound)?;
                (fb.physical_addresses[0], fb.width, fb.height, fb.size())
            };
            if src_phys != 0 {
                let src_virt = mm::phys_to_virt(src_phys as usize) as *mut u8;
                let count = buffer.len().min(fb_size as usize);
                unsafe {
                    ptr::copy_nonoverlapping(buffer.as_ptr(), src_virt, count);
                }
                let flip_data = [fb_id.raw(), 0, fb_w, fb_h];
                self.handle_flip_page(&mut device_lock, &flip_data as *const _ as usize)?;
                return Ok(count);
            }
        }

        Err(DriverError::Unsupported)
    }

    /// Handle mmap operations for framebuffer access
    pub fn handle_mmap(&mut self, offset: usize, size: usize) -> Result<*mut u8, DriverError> {
        if offset != 0 {
            // Map the requested physical address (likely a dumb buffer)
            // Note: in a production driver we'd check if this physical address 
            // belongs to a buffer we allocated.
            let buffer_ptr = mm::phys_to_virt(offset) as *mut u8;
            return Ok(buffer_ptr);
        }

        // Get the real hardware framebuffer base address from VFS
        extern "C" {
            fn vfs_get_framebuffer_base() -> u64;
        }

        let fb_phys = unsafe { vfs_get_framebuffer_base() };
        if fb_phys == 0 {
            return Err(DriverError::NotFound);
        }

        // Convert physical address to virtual address for userspace access
        let buffer_ptr = mm::phys_to_virt(fb_phys as usize) as *mut u8;

        // Validate the requested mapping size
        if size > 0x10000000 { // Limit to 256MB max for safety
            return Err(DriverError::Unsupported);
        }

        Ok(buffer_ptr)
    }

    // ── Virtio-GPU IOCTL Handlers ───────────────────────────────────────────

    fn virtgpu_handle_resource_create(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // Copy the request out of user memory before taking the device lock.
        let req = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_resource_create) };

        // ── Guest backing store ──────────────────────────────────────────────
        // The old code created a host resource and stopped. That left a BO with
        // no guest pages and no entry in any registry, so:
        //   * PRIME_HANDLE_TO_FD could not resolve the handle at all, which is
        //     what smithay reported as `AsDmabufError(InvalidFD(InvalidFdError))`
        //     for every scanout format, ending in "Backend initialized without
        //     output"; and
        //   * TRANSFER_TO_HOST_3D had nothing to transfer *from*.
        //
        // Mesa supplies `size` (and `stride`); fall back to a tight 4-byte
        // packing when it asks us to choose, which is what the 2D scanout
        // formats virgl hands to KMS all are.
        let stride = if req.stride != 0 { req.stride } else { req.width.saturating_mul(4) };
        let size = if req.size != 0 {
            req.size as usize
        } else {
            (stride as usize).saturating_mul(req.height.max(1) as usize)
        };
        if size == 0 { return Err(DriverError::InvalidParameter); }

        let pages = size.div_ceil(4096);
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        let phys = mm::buddy::alloc(order).ok_or(DriverError::Io)?;

        let res_handle = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = match guard.as_mut() {
                Some(g) => g,
                None => { mm::buddy::free(phys, order); return Err(DriverError::NotFound); }
            };
            // Allocate a real resource id rather than the fixed 1 the stub used,
            // and pass the caller's geometry through instead of an empty command
            // body (which, with the old opcode, was not even RESOURCE_CREATE_3D).
            let rid = gpu.alloc_resource_id();
            if !gpu.create_resource_3d_full(
                rid, req.target, req.format, req.bind,
                req.width, req.height, req.depth.max(1), req.array_size.max(1),
                req.last_level, req.nr_samples, req.flags,
            ) {
                mm::buddy::free(phys, order);
                return Err(DriverError::Io);
            }
            // Without ATTACH_BACKING the host resource has no guest memory bound
            // and every transfer is a no-op the host silently accepts.
            if !gpu.attach_backing(rid, phys as u64, size as u32) {
                mm::buddy::free(phys, order);
                return Err(DriverError::Io);
            }
            rid
        };

        // Register in the dumb registry so PRIME export / MAP / ADDFB2 and the
        // refcounted BO lifetime resolve this handle exactly like any other BO.
        // The gem handle and the host resource id are DIFFERENT namespaces; the
        // stub returned the resource id as both, which would have collided with
        // dumb handles the moment anything else allocated one.
        let handle = DrmDumbBuffer::next_handle();
        DUMB_BUFFERS.lock().insert(handle, DumbBuf {
            phys,
            order,
            last_fence: 0,
            obj: NEXT_BO_OBJ.fetch_add(1, Ordering::Relaxed),
            refs: 1,
            handle_live: true,
            res_id: res_handle,
        });

        // bo_handle @40, res_handle @44, size @48, stride @52.
        unsafe {
            (arg as *mut u8).add(40).cast::<u32>().write_volatile(handle);
            (arg as *mut u8).add(44).cast::<u32>().write_volatile(res_handle);
            (arg as *mut u8).add(48).cast::<u32>().write_volatile(size as u32);
            (arg as *mut u8).add(52).cast::<u32>().write_volatile(stride);
        }
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_EXECBUFFER — forward the real command stream.
    ///
    /// The previous implementation bound `_exec` and never touched it, then
    /// submitted `&[]`: every command stream userspace ever produced was thrown
    /// away while the ioctl reported success.
    ///
    /// WHAT OF THE REQUEST IS HONOURED, AND WHAT IS NOT.
    ///
    /// Honoured: `command`/`size` (the stream itself) and, since this change,
    /// `ring_idx` when the caller sets VIRTGPU_EXECBUF_RING_IDX — it travels in
    /// `hdr.ring_idx` with VIRTIO_GPU_FLAG_INFO_RING_IDX, the upstream encoding,
    /// so the host creates the completion fence on the ring Mesa asked for
    /// instead of always on ring 0.
    ///
    /// Also honoured, since the per-BO fence landed: `bo_handles` /
    /// `num_bo_handles`. Upstream resolves the array to GEM objects BEFORE
    /// touching the device (a handle that names nothing fails the whole ioctl
    /// with -ENOENT) and attaches the submission's fence to each object, which
    /// is what makes a later WAIT on one of those BOs report on THIS submission.
    /// Both halves are done here now — see "THE FENCE MODEL" on `BlobObj` for
    /// what that does and does not change while submission is synchronous.
    ///
    /// Also honoured, since the SIMULATE_SYNCOBJ fix: a **fence-only** request,
    /// i.e. `command == 0 && size == 0`. See the block comment on the check
    /// below for why that is a legitimate request and not a malformed one.
    ///
    /// `fence_fd` with FENCE_FD_OUT is honoured too, but NOT here — the out-fence
    /// fd is minted by `kernel/src/syscall.rs::sys_ioctl`, which is the only
    /// layer that has the caller's fd table (same split as PRIME_HANDLE_TO_FD).
    /// This function is deliberately unaware of it; all it has to guarantee is
    /// that a fence-only request is *not* refused.
    ///
    /// NOT honoured, and reported once per distinct shape rather than dropped in
    /// silence:
    ///   * `fence_fd` with FENCE_FD_IN — sync_file *import* needs the fd to be
    ///     resolved back to a fence object and waited on before submission. The
    ///     driver layer has no channel to read the caller's fd table, and unlike
    ///     the OUT direction there is no signalled-by-construction shortcut.
    ///   * `in_syncobjs` / `out_syncobjs` / `syncobj_stride` — same.
    ///   * any flag outside VIRTGPU_EXECBUF_FLAGS_KNOWN. Upstream answers EINVAL
    ///     for those; we deliberately do not, because refusing a flag from a
    ///     newer uAPI would turn a client that works today into one that fails
    ///     outright. It is logged instead.
    fn virtgpu_handle_execbuffer(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // Read the request out of user memory BEFORE any device lock is taken.
        let exec = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_execbuffer) };
        // A FENCE-ONLY submission: no command stream, just "give me a fence for
        // everything submitted so far". Mesa's venus backend issues exactly this
        // once per process from `sim_syncobj_create` (vn_renderer_virtgpu.c:145),
        // with `flags = RING_IDX | FENCE_FD_OUT` and every other field zero, and
        // treats a failure as "syncobj simulation unavailable" — which disables
        // every `vn_renderer_sync`, and with it the ring-teardown submit in
        // `vn_ring_destroy`. Refusing it is therefore not a safe conservative
        // choice; it silently leaks a host-side ring per Venus instance.
        //
        // It is also exactly answerable here: `submit` busy-spins on the used
        // ring (virtio_gpu.rs), so every earlier submission on this open is
        // already retired by the time any ioctl returns. A fence over an empty
        // stream is a no-op whose result is "already signalled", which is the
        // truth rather than an approximation.
        //
        // Only BOTH-zero is a fence-only request. `size` without `command`, or
        // `command` without `size`, stays malformed and stays refused.
        let fence_only = exec.command == 0 && exec.size == 0;
        if !fence_only && (exec.command == 0 || exec.size == 0) {
            return Err(DriverError::InvalidParameter);
        }
        const MAX_CMD_BYTES: usize = 4 << 20;
        let size = exec.size as usize;
        if size > MAX_CMD_BYTES { return Err(DriverError::InvalidParameter); }
        // Upstream bounds `num_bo_handles` only by what `kvmalloc_array` will
        // serve. A tighter bound belongs here because the value is caller-
        // controlled and decides a kernel allocation: 1024 is orders of
        // magnitude above what Mesa's Venus backend submits (a handful of BOs
        // per command stream) and far below anything that could pressure the
        // allocator.
        const MAX_BO_HANDLES: u32 = 1024;

        let ctx = ctx_ensure(open_id);
        if ctx == 0 {
            crate::pci::serial_debug("[DRM] EXECBUFFER: no context and none could be created\n");
            return Err(DriverError::InvalidParameter);
        }
        let entry = ctx_lookup_entry(open_id);

        gdbg("[EXECDBG] ctx=");
        gdbg_hex(ctx);
        gdbg(" size=");
        gdbg_hex(exec.size);
        gdbg(" flags=");
        gdbg_hex(exec.flags);
        gdbg(" ring=");
        gdbg_hex(exec.ring_idx);
        gdbg(" nbo=");
        gdbg_hex(exec.num_bo_handles);
        gdbg(" cmd=");
        gdbg_hex_64(exec.command);
        gdbg("\n");

        // ── Ring selection (implemented) ────────────────────────────────────
        // Upstream reads `ring_idx` only when the caller opted in with the
        // flag, and rejects an index at or past the ring count the context was
        // created with. Without the flag the submission is unringed, which is
        // what every caller before this change effectively got.
        let ring_idx: Option<u8> = if exec.flags & VIRTGPU_EXECBUF_RING_IDX != 0 {
            let num_rings = entry.map(|c| c.num_rings).unwrap_or(1);
            if exec.ring_idx >= num_rings {
                crate::pci::serial_debug("[DRM] EXECBUFFER ring_idx=");
                crate::pci::serial_debug_hex(exec.ring_idx);
                crate::pci::serial_debug(" >= context num_rings=");
                crate::pci::serial_debug_hex(num_rings);
                crate::pci::serial_debug("\n");
                return Err(DriverError::InvalidParameter);
            }
            Some(exec.ring_idx as u8)
        } else {
            None
        };

        // ── bo_handles (implemented) ────────────────────────────────────────
        // Copied out of user memory here, with every other user read, and
        // BEFORE any lock is taken. `num_bo_handles == 0` with a non-null
        // pointer is not an error — upstream gates entirely on the count.
        let bo_handles: Vec<u32> = if exec.num_bo_handles != 0 {
            if exec.bo_handles == 0 || exec.num_bo_handles > MAX_BO_HANDLES {
                crate::pci::serial_debug("[DRM] EXECBUFFER: bad bo_handles array, n=");
                crate::pci::serial_debug_hex(exec.num_bo_handles);
                crate::pci::serial_debug("\n");
                return Err(DriverError::InvalidParameter);
            }
            let n = exec.num_bo_handles as usize;
            let mut v = vec![0u32; n];
            unsafe {
                ::core::ptr::copy_nonoverlapping(exec.bo_handles as *const u32, v.as_mut_ptr(), n);
            }
            v
        } else {
            Vec::new()
        };

        // Resolve the whole array before the device is touched, exactly as
        // `virtio_gpu_array_from_handles` does: a submission that names a BO
        // that does not exist is rejected outright rather than executed with
        // the bad handle quietly skipped. Nothing has been submitted at this
        // point, so returning here leaves no half-done work behind.
        for &h in bo_handles.iter() {
            if !bo_exists(h, open_id) {
                crate::pci::serial_debug("[DRM] EXECBUFFER: unknown bo_handle=");
                crate::pci::serial_debug_hex(h);
                crate::pci::serial_debug("\n");
                return Err(DriverError::NotFound);
            }
        }

        // ── Divergence report (logged only) ─────────────────────────────────
        // Keyed by the shape of what was asked for, not by the call, so a
        // per-frame submission logs its first frame and nothing after. The key
        // packs: which unhandled fields were non-zero, and which flag bits were
        // set, so two different divergences never collapse into one report.
        let unknown_flags = exec.flags & !VIRTGPU_EXECBUF_FLAGS_KNOWN;
        // FENCE_FD_OUT is no longer in this list: `sys_ioctl` mints a signalled
        // eventfd and writes it into `fence_fd` on the way back out, so claiming
        // it is unhonoured would be a false report. FENCE_FD_IN is still ignored,
        // and so is a non-zero incoming `fence_fd` that nothing asked us to
        // consume — both mean the caller expected an in-fence wait we do not do.
        let ignored_fence_fd = exec.flags & VIRTGPU_EXECBUF_FENCE_FD_IN != 0
            || (exec.flags & VIRTGPU_EXECBUF_FENCE_FD_OUT == 0 && exec.fence_fd != 0);
        let ignored_syncobj = exec.num_in_syncobjs != 0 || exec.num_out_syncobjs != 0
            || exec.in_syncobjs != 0 || exec.out_syncobjs != 0;
        if ignored_fence_fd || ignored_syncobj || unknown_flags != 0 {
            let key = (exec.flags & 0xFFFF)
                | (ignored_fence_fd as u32) << 17
                | (ignored_syncobj as u32) << 18;
            if EXEC_DIVERGENCE.lock().first(key) {
                crate::pci::serial_debug("[DRM] EXECBUFFER: fields asked for but NOT honoured —");
                if ignored_fence_fd {
                    crate::pci::serial_debug(" fence_fd");
                }
                if ignored_syncobj {
                    crate::pci::serial_debug(" syncobjs(in=");
                    crate::pci::serial_debug_hex(exec.num_in_syncobjs);
                    crate::pci::serial_debug(" out=");
                    crate::pci::serial_debug_hex(exec.num_out_syncobjs);
                    crate::pci::serial_debug(")");
                }
                if unknown_flags != 0 {
                    crate::pci::serial_debug(" unknown_flags=");
                    crate::pci::serial_debug_hex(unknown_flags);
                }
                crate::pci::serial_debug(" flags=");
                crate::pci::serial_debug_hex(exec.flags);
                crate::pci::serial_debug(" (reported once per shape)\n");
            }
        }

        // A fence-only request submits nothing. Returning here — rather than
        // handing `submit_3d` an empty slice, which it refuses — is the point:
        // there is no stream to execute and no new fence to mint, because every
        // earlier submission on this open has already retired. Deliberately NOT
        // touching `ctx_record_fence`: recording a fence id that was never sent
        // to the host would make a later WAIT report on a submission that does
        // not exist. `bo_handles` was still validated above, so a fence-only
        // request naming a bogus BO is still refused, exactly as a real one is.
        if fence_only {
            return Ok(0);
        }

        // Copy the stream into kernel memory while no spinlock is held: touching
        // a user page can demand-fault, and faulting under the device spinlock is
        // the 82d0cc3 all-vCPU freeze class.
        let mut cmds = vec![0u8; size];
        unsafe {
            ::core::ptr::copy_nonoverlapping(exec.command as *const u8, cmds.as_mut_ptr(), size);
        }

        let fence = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_mut().ok_or(DriverError::NotFound)?;
            gpu.submit_3d(ctx, &cmds, ring_idx).map_err(|_| DriverError::Io)?
        };

        // Attach the fence to every BO the submission named, and record it as
        // this open's most recent — the two replacements for the single global
        // `LAST_EXEC_FENCE`. A BO that vanished since validation (concurrent
        // GEM_CLOSE) is skipped: it is gone, so nothing can wait on it.
        for &h in bo_handles.iter() {
            let _ = bo_attach_fence(h, open_id, fence);
        }
        ctx_record_fence(open_id, fence);
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_GET_CAPS — copy the host's capset blob back to
    /// `caps.addr`. The previous implementation issued a (wrongly numbered)
    /// GET_CAPSET and discarded the response entirely.
    fn virtgpu_handle_get_caps(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let caps = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_get_caps) };
        if caps.addr == 0 || caps.size == 0 {
            return Err(DriverError::InvalidParameter);
        }
        const MAX_CAPS_BYTES: usize = 1 << 20;
        let want = (caps.size as usize).min(MAX_CAPS_BYTES);

        // Fetch into a kernel buffer under the device lock …
        let blob = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_mut().ok_or(DriverError::NotFound)?;

            // Like Linux: resolve the capset id against the host's table first.
            // A capset the host does not expose is EINVAL, not a buffer of
            // zeros — which is the difference between "Venus is present" and
            // "the ioctl returned 0".
            let (max_ver, max_size) = match gpu.find_capset(caps.cap_set_id) {
                Some(v) => v,
                None => {
                    crate::pci::serial_debug("[DRM] GET_CAPS: host exposes no capset id ");
                    crate::pci::serial_debug_hex(caps.cap_set_id);
                    crate::pci::serial_debug("\n");
                    return Err(DriverError::InvalidParameter);
                }
            };
            crate::pci::serial_debug("[DRM] GET_CAPS capset=");
            crate::pci::serial_debug_hex(caps.cap_set_id);
            crate::pci::serial_debug(" host max_ver=");
            crate::pci::serial_debug_hex(max_ver);
            crate::pci::serial_debug(" max_size=");
            crate::pci::serial_debug_hex(max_size);
            crate::pci::serial_debug("\n");
            if max_size == 0 {
                return Err(DriverError::InvalidParameter);
            }
            let n = want.min(max_size as usize);
            gpu.get_capset(caps.cap_set_id, caps.cap_set_ver, n)
                .map_err(|_| DriverError::Io)?
        };
        // … and copy it out only after the lock is released.
        let n = blob.len().min(want);
        unsafe {
            ::core::ptr::copy_nonoverlapping(blob.as_ptr(), caps.addr as *mut u8, n);
        }
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_GETPARAM. Mesa's venus backend probes these before it
    /// will do anything else and refuses to proceed if they read back wrong, so
    /// each answer is derived from what was actually negotiated rather than
    /// hardcoded.
    ///
    /// `open_id` is threaded in the same way EXECBUFFER / CONTEXT_INIT /
    /// RESOURCE_CREATE_BLOB already take it, and for the same reason: one param
    /// (`VIRTGPU_PARAM_LEANDROS_CTX_ID`) answers about the calling open rather
    /// than about the device.
    fn virtgpu_handle_getparam(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let req = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_getparam) };

        // Answered before any device lock is taken. It is pure per-open
        // bookkeeping (no device round-trip), and taking VIRTGPU_CTXS
        // underneath VIRTIO_GPU would invert the lock order documented on
        // VIRTGPU_CTXS. It also stays answerable when the GPU is absent.
        if req.param == VIRTGPU_PARAM_LEANDROS_CTX_ID {
            let ctx = ctx_lookup(open_id) as u64;
            if req.value == 0 { return Err(DriverError::InvalidParameter); }
            unsafe { (req.value as *mut u32).write_volatile(ctx as u32) };
            return Ok(0);
        }
        // Same treatment, same reason as CTX_ID: per-open bookkeeping behind a
        // leaf lock, read into a local before the user pointer is touched.
        if req.param == VIRTGPU_PARAM_LEANDROS_LAST_FENCE {
            let f = ctx_lookup_entry(open_id).map(|c| c.last_fence).unwrap_or(0);
            if req.value == 0 { return Err(DriverError::InvalidParameter); }
            unsafe { (req.value as *mut u32).write_volatile(f as u32) };
            return Ok(0);
        }
        // Same treatment, same reason: a leaf lock, no device round-trip, and
        // the count is read out of the guard into a local BEFORE the user
        // pointer is touched (never write user memory under a spinlock).
        if req.param == VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS {
            let n = HOSTVIS_SPANS.lock().len() as u32;
            if req.value == 0 { return Err(DriverError::InvalidParameter); }
            unsafe { (req.value as *mut u32).write_volatile(n) };
            return Ok(0);
        }
        // Likewise: BLOB_OBJS is a leaf, the count is copied out of the guard
        // into a local, and the user pointer is written with no lock held.
        if req.param == VIRTGPU_PARAM_LEANDROS_BLOB_OBJS {
            let n = blob_obj_count();
            if req.value == 0 { return Err(DriverError::InvalidParameter); }
            unsafe { (req.value as *mut u32).write_volatile(n) };
            return Ok(0);
        }

        let value: u64 = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_mut().ok_or(DriverError::NotFound)?;
            use crate::virtio_gpu as vg;
            match req.param {
                VIRTGPU_PARAM_3D_FEATURES => gpu.has_feature(vg::VIRTIO_GPU_F_VIRGL) as u64,
                // We implement GET_CAPSET_INFO as a distinct command with a real
                // index→id mapping, which is exactly what this "fix" denotes.
                VIRTGPU_PARAM_CAPSET_QUERY_FIX => 1,
                VIRTGPU_PARAM_RESOURCE_BLOB => {
                    gpu.has_feature(vg::VIRTIO_GPU_F_RESOURCE_BLOB) as u64
                }
                // Host-visible blob memory needs both the blob feature and an
                // actual shared-memory BAR window to map it into.
                VIRTGPU_PARAM_HOST_VISIBLE => {
                    (gpu.has_feature(vg::VIRTIO_GPU_F_RESOURCE_BLOB)
                        && gpu.shared_mem_region().is_some()) as u64
                }
                // No PRIME / cross-device sharing on this node.
                VIRTGPU_PARAM_CROSS_DEVICE => 0,
                VIRTGPU_PARAM_CONTEXT_INIT => {
                    gpu.has_feature(vg::VIRTIO_GPU_F_CONTEXT_INIT) as u64
                }
                VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs => {
                    let n = gpu.num_capsets().min(16);
                    let mut mask = 0u64;
                    for i in 0..n {
                        if let Ok((id, _ver, _sz)) = gpu.get_capset_info(i) {
                            if id < 64 { mask |= 1u64 << id; }
                        }
                    }
                    mask
                }
                // CTX_CREATE carries a debug_name and we populate it.
                VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME => 1,
                // LeandrOS-private: host-visible window geometry, in MiB.
                VIRTGPU_PARAM_LEANDROS_HOSTVIS_MIB => {
                    gpu.shared_mem_region().map(|r| r.len >> 20).unwrap_or(0)
                }
                _ => return Err(DriverError::InvalidParameter),
            }
        };

        // `value` is a USER POINTER (u64_to_user_ptr), not an out-field.
        // Upstream virtio_gpu_getparam_ioctl does
        //     copy_to_user(u64_to_user_ptr(param->value), &value, sizeof(int))
        // i.e. it writes a 32-bit int THROUGH the pointer. Mesa's Venus ICD
        // reads back that pointee, not the struct field, so writing the value
        // in place made every param read as 0 for Mesa.
        if req.value == 0 { return Err(DriverError::InvalidParameter); }
        unsafe { (req.value as *mut u32).write_volatile(value as u32) };
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_CONTEXT_INIT — create the 3D context whose type is
    /// selected by VIRTGPU_CONTEXT_PARAM_CAPSET_ID (4 = Venus).
    fn virtgpu_handle_context_init(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let init = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_context_init) };
        let n = init.num_params as usize;
        if init.ctx_set_params == 0 || n == 0 || n > 8 {
            return Err(DriverError::InvalidParameter);
        }

        // Copy the whole param array before locking anything.
        let mut params = [drm_virtgpu_context_set_param { param: 0, value: 0 }; 8];
        for i in 0..n {
            params[i] = unsafe {
                ::core::ptr::read_volatile(
                    (init.ctx_set_params as *const drm_virtgpu_context_set_param).add(i),
                )
            };
        }

        let mut capset_id: u32 = 0;
        // Upstream's cap. `num_rings` only ever gates `execbuffer.ring_idx`, so
        // recording it costs nothing and turns a ring index we would otherwise
        // forward blind into a bounds-checked one.
        const MAX_RINGS: u32 = 64;
        let mut num_rings: u32 = 1;
        for p in params[..n].iter() {
            match p.param {
                VIRTGPU_CONTEXT_PARAM_CAPSET_ID => capset_id = p.value as u32,
                VIRTGPU_CONTEXT_PARAM_NUM_RINGS => {
                    if p.value > MAX_RINGS as u64 { return Err(DriverError::InvalidParameter); }
                    num_rings = (p.value as u32).max(1);
                }
                // Accepted and ignored: nothing here polls rings, and the debug
                // name is already supplied by `ctx_create`.
                VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK
                | VIRTGPU_CONTEXT_PARAM_DEBUG_NAME => {}
                _ => return Err(DriverError::InvalidParameter),
            }
        }
        if capset_id == 0 {
            return Err(DriverError::InvalidParameter);
        }

        // Second CONTEXT_INIT on the SAME open. Linux answers EEXIST here.
        //
        // TODO(errno): we answer Ok(0) instead, and that is a deliberate
        // deviation — servers/drm/src/lib.rs collapses every Err from this
        // function to err_reply(-1) (EPERM), so EEXIST is simply not
        // expressible without plumbing real errnos through
        // Result<usize, DriverError>. Reporting success for a re-init with the
        // capset the open already has is the harmless reading; a re-init asking
        // for a *different* capset is genuinely wrong, so that still fails.
        if let Some(existing) = ctx_lookup_entry(open_id) {
            return if existing.capset == capset_id {
                Ok(0)
            } else {
                Err(DriverError::InvalidParameter)
            };
        }

        let ctx = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_mut().ok_or(DriverError::NotFound)?;
            gpu.ctx_create(capset_id, "leandros-venus")
                .map_err(|_| DriverError::Io)?
        };
        if let Err(winner) = ctx_bind(open_id, ctx, capset_id, num_rings) {
            // Nothing else may reach this context, so drop it either way.
            {
                let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
                if let Some(gpu) = guard.as_mut() { gpu.ctx_destroy(ctx); }
            }
            if winner != CTX_BIND_NO_SLOT {
                // A concurrent init on this same open got there first; the open
                // has a context, which is all the caller asked for.
                return Ok(0);
            }
            crate::pci::serial_debug("[DRM] CONTEXT_INIT: no free per-open context slot\n");
            return Err(DriverError::Io);
        }
        crate::pci::serial_debug("[DRM] virtgpu context created, capset=");
        crate::pci::serial_debug_hex(capset_id);
        crate::pci::serial_debug(" ctx_id=");
        crate::pci::serial_debug_hex(ctx);
        crate::pci::serial_debug("\n");
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB.
    fn virtgpu_handle_resource_create_blob(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let req =
            unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_resource_create_blob) };
        const MAX_BLOB_BYTES: u64 = 64 << 20;
        if req.size == 0 || req.size > MAX_BLOB_BYTES || req.blob_mem == 0 {
            return Err(DriverError::InvalidParameter);
        }

        use crate::virtio_gpu as vg;
        // GUEST and HOST3D_GUEST blobs are backed by guest pages we allocate and
        // hand over as mem entries; a pure HOST3D blob lives host-side and the
        // guest owns no pages for it.
        let guest_backed = req.blob_mem == vg::VIRTIO_GPU_BLOB_MEM_GUEST
            || req.blob_mem == vg::VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST;

        let size = req.size as usize;
        let order = vg::order_for_bytes(size);
        let phys = if guest_backed {
            let p = mm::buddy::alloc(order).ok_or(DriverError::Io)?;
            unsafe { ::core::ptr::write_bytes(mm::phys_to_virt(p) as *mut u8, 0, (1usize << order) * 4096) };
            p
        } else {
            0
        };

        let ctx = ctx_lookup(open_id);
        let backing = if guest_backed { Some((phys as u64, size as u32)) } else { None };

        let res_handle = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = match guard.as_mut() {
                Some(g) => g,
                None => {
                    if phys != 0 { mm::buddy::free(phys, order); }
                    return Err(DriverError::NotFound);
                }
            };
            let rid = gpu.alloc_resource_id();
            match gpu.resource_create_blob(
                ctx, rid, req.blob_mem, req.blob_flags, req.blob_id, req.size, backing,
            ) {
                Ok(()) => {
                    // Upstream parity. `virtio_gpu_gem_object_open()` sends
                    // CTX_ATTACH_RESOURCE for every GEM object opened on a 3D
                    // `drm_file`, which is how the host learns that this
                    // resource belongs to this context; without it the host's
                    // per-context resource table stays empty and a renderer is
                    // entitled to reject any command stream referring to the
                    // resource. `free_blob` has always sent the matching
                    // CTX_DETACH_RESOURCE, so until now the pair was
                    // half-written: a detach for an attach that never happened.
                    //
                    // Failure is logged, not propagated — exactly as upstream,
                    // where the attach is issued for effect and
                    // `virtio_gpu_gem_object_open` returns 0 regardless. A host
                    // that refuses the attach still gave us a valid resource,
                    // and turning that into a failed ioctl would break blob
                    // creation on hosts where it works today.
                    if ctx != 0 && !gpu.ctx_attach_resource(ctx, rid) {
                        crate::pci::serial_debug("[DRM] CTX_ATTACH_RESOURCE refused ctx=");
                        crate::pci::serial_debug_hex(ctx);
                        crate::pci::serial_debug(" res=");
                        crate::pci::serial_debug_hex(rid);
                        crate::pci::serial_debug("\n");
                    }
                    rid
                }
                Err(()) => {
                    drop(guard);
                    if phys != 0 { mm::buddy::free(phys, order); }
                    return Err(DriverError::Io);
                }
            }
        };

        // The object, then the handle that names it. The handle IS the object's
        // one initial reference (`BO LIFETIME`); an exported dmabuf fd later
        // takes a second, which is what makes the buffer outlive GEM_CLOSE.
        let obj = NEXT_BO_OBJ.fetch_add(1, Ordering::Relaxed);
        BLOB_OBJS.lock().insert(
            obj,
            BlobObj {
                phys,
                order,
                res_handle,
                size: req.size,
                blob_mem: req.blob_mem,
                // Nothing has been submitted against a brand-new BO.
                last_fence: 0,
                // Host-visible mapping is established lazily, by VIRTGPU_MAP.
                win_off: 0,
                map_phys: 0,
                map_info: 0,
                refs: 1,
                // A virtgpu blob has no GPU virtual address: the host owns the
                // GPU-side mapping. Zero here is also what keeps it out of
                // every v3d path (see `BlobObj::v3d_va`).
                v3d_va: 0,
            },
        );
        let handle = NEXT_BLOB_HANDLE.fetch_add(1, Ordering::Relaxed);
        BLOB_BUFFERS.lock().insert(
            handle,
            BlobHandle {
                obj,
                // The creating open owns it, whether or not it has a context.
                owner: open_id,
                ctx,
            },
        );

        // Write back only bo_handle (offset 8) and res_handle (offset 12) rather
        // than the whole struct, so nothing the caller set is clobbered.
        unsafe {
            (arg as *mut u8).add(8).cast::<u32>().write_volatile(handle);
            (arg as *mut u8).add(12).cast::<u32>().write_volatile(res_handle);
        }
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_MAP — turn a BO handle into the mmap offset for it.
    ///
    /// MMAP TOKEN SCHEME. This driver's device-fd convention is that the offset
    /// passed to `mmap()` IS the guest-physical address of the memory to map;
    /// `handle_ioctl_mmap` validates it against the buffers it handed out and the
    /// kernel then maps that physical range (see kernel/src/syscall.rs, the
    /// DynamicDevice arm of `sys_mmap`). Both blob kinds produce a token under
    /// that one scheme, so userspace never has to know which kind it holds:
    ///   * guest-backed (BLOB_MEM_GUEST / HOST3D_GUEST, and dumb buffers) —
    ///     the token is the buddy allocation's physical base;
    ///   * host-side (BLOB_MEM_HOST3D) — the token is
    ///     `shmem_window.phys + win_off`, the guest-physical address the host
    ///     places the resource at inside the shared-memory BAR window. Nothing
    ///     backs it in guest RAM; the BAR does.
    ///
    /// The host-side path is the one Mesa's Venus ICD needs: it allocates its
    /// command ring as a HOST3D + USE_MAPPABLE blob and maps it here, and every
    /// `vkCreateInstance` fails with VK_ERROR_OUT_OF_HOST_MEMORY if this refuses.
    ///
    /// The mapping is established lazily and exactly once per blob: the window
    /// reservation and the RESOURCE_MAP_BLOB round-trip happen on the first MAP,
    /// and a repeat MAP of the same handle re-reports the same token rather than
    /// asking the host to map an already-mapped resource (which it refuses).
    ///
    /// CACHEABILITY: the token still carries no cache type, but the blob record
    /// does. `sys_mmap`'s DynamicDevice arm asks `blob_map_cache_type` about the
    /// token it just validated and maps the range non-cached when the host
    /// answered VIRTIO_GPU_MAP_CACHE_UNCACHED or _WC. Only those two change
    /// anything: _CACHED — which is what the host reports for the Venus command
    /// ring — and the two guest-RAM token spaces stay write-back exactly as
    /// they were.
    ///
    /// This is not a nicety. A blob the host maps write-combining is host memory
    /// we have no cache coherence with, so a write-back guest mapping may serve
    /// a stale cache line indefinitely. That is what stalled `vkrender`'s
    /// `s0_submit` under x86_64/KVM while both TCG runs passed: Mesa's
    /// fence-feedback slot lands in the first HOST_COHERENT memory type, which
    /// is not HOST_CACHED, so the host asks for map_info = 0x03 (WC) — and
    /// `vn_GetFenceStatus` polls it with a plain load that never touches the
    /// ring. TCG hides it on both arches because TCG models no guest cache.
    ///
    /// LOCKING: user memory is read before any lock is taken and written after
    /// every lock is dropped, and no two of BLOB_BUFFERS / HOSTVIS_SPANS /
    /// VIRTIO_GPU are ever held at the same time — the 82d0cc3 discipline.
    fn virtgpu_handle_map(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // `struct drm_virtgpu_map { u64 offset; u32 handle; u32 pad; }`.
        let handle = unsafe { (arg as *const u8).add(8).cast::<u32>().read_volatile() };

        // Copy the blob record out; hold nothing. Scoped to the calling open:
        // another client's blob reads as a handle that does not exist, so this
        // cannot be used to obtain an mmap token for it.
        let blob = blob_lookup(handle, open_id);

        let token: u64 = match blob {
            // ── Host-side blob memory ────────────────────────────────────────
            Some(b) if b.blob_mem == crate::virtio_gpu::VIRTIO_GPU_BLOB_MEM_HOST3D => {
                if b.map_phys != 0 {
                    b.map_phys // already mapped — idempotent
                } else {
                    self.hostvis_map_blob(b)?
                }
            }
            // ── Guest-backed blob ────────────────────────────────────────────
            Some(b) if b.phys != 0 => b.phys as u64,
            Some(_) => {
                // A guest-backed blob with no pages cannot happen (creation
                // allocates or fails), so this is a HOST3D_GUEST/GUEST record
                // whose backing went missing. Refuse rather than hand out 0.
                crate::pci::serial_debug("[DRM] VIRTGPU_MAP: blob has no backing\n");
                return Err(DriverError::Unsupported);
            }
            None => {
                let phys = dumb_lookup(handle)
                    .map(|b| b.phys)
                    .ok_or(DriverError::InvalidParameter)?;
                if phys == 0 { return Err(DriverError::Unsupported); }
                phys as u64
            }
        };

        // `offset` is the first u64 of drm_virtgpu_map. Written with no lock held.
        unsafe { (arg as *mut u64).write_volatile(token) };
        Ok(0)
    }

    /// First MAP of a HOST3D blob: reserve window space, ask the host to place
    /// the resource there, and record the resulting token. Returns the token.
    ///
    /// Split out of `virtgpu_handle_map` so the lock discipline is visible in one
    /// place: HOSTVIS_SPANS is taken and released inside `hostvis_alloc`,
    /// VIRTIO_GPU is taken and released on its own, and BLOB_OBJS is taken last
    /// to record the result. Never two at once, never any across the device
    /// round-trip's busy-spin.
    fn hostvis_map_blob(&mut self, b: BlobView) -> Result<u64, DriverError> {
        // The window the device advertised at probe time. Deliberately NOT mapped
        // anywhere yet — it is gigabytes wide; only the sub-range this blob lands
        // in ever becomes a mapping, and only in the calling process.
        let window = {
            let guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            match guard.as_ref().and_then(|g| g.shared_mem_region()) {
                Some(r) if r.phys != 0 && r.len != 0 => r,
                _ => {
                    crate::pci::serial_debug(
                        "[DRM] VIRTGPU_MAP: host-visible blob but no shared-memory window\n",
                    );
                    return Err(DriverError::Unsupported);
                }
            }
        };

        let off = match hostvis_alloc(b.size, window.len) {
            Some(o) => o,
            None => {
                crate::pci::serial_debug("[DRM] VIRTGPU_MAP: host-visible window full, size=");
                crate::pci::serial_debug_hex(b.size as u32);
                crate::pci::serial_debug("\n");
                return Err(DriverError::Io);
            }
        };

        let map_info = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            match guard.as_mut() {
                Some(gpu) => gpu.resource_map_blob(b.res_handle, off),
                None => Err(()),
            }
        };
        let map_info = match map_info {
            Ok(mi) => mi,
            Err(()) => {
                // The host kept nothing, so neither do we.
                hostvis_free(off);
                crate::pci::serial_debug("[DRM] VIRTGPU_MAP: RESOURCE_MAP_BLOB refused res=");
                crate::pci::serial_debug_hex(b.res_handle);
                crate::pci::serial_debug("\n");
                return Err(DriverError::Io);
            }
        };

        let token = window.phys + off;

        // Record it on the OBJECT, not on the handle: the placement is a
        // property of the host resource, and a second handle on the same object
        // must see the same token rather than ask the host to map an
        // already-mapped resource (which it refuses).
        //
        // Keying the rollback on the object is also what keeps it correct now
        // that a BO can have more than one handle. If the check were still
        // "did the HANDLE vanish", a concurrent GEM_CLOSE of one handle while
        // another still held the object would undo a map the surviving handle
        // is entitled to — and, worse, `hostvis_free` a span the object's own
        // teardown would later free again. The rollback fires only when the
        // OBJECT is gone, in which case nothing else can be holding the span
        // and `blob_unref` could not have seen a reservation that did not exist
        // yet.
        let recorded = {
            let mut map = BLOB_OBJS.lock();
            match map.get_mut(&b.obj) {
                Some(e) => { e.win_off = off; e.map_phys = token; e.map_info = map_info; true }
                None => false,
            }
        };
        if !recorded {
            {
                let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
                if let Some(gpu) = guard.as_mut() { gpu.resource_unmap_blob(b.res_handle); }
            }
            hostvis_free(off);
            return Err(DriverError::InvalidParameter);
        }

        crate::pci::serial_debug("[DRM] host-visible blob mapped: res=");
        crate::pci::serial_debug_hex(b.res_handle);
        crate::pci::serial_debug(" win_off=");
        crate::pci::serial_debug_hex(off as u32);
        crate::pci::serial_debug(" phys_hi=");
        crate::pci::serial_debug_hex((token >> 32) as u32);
        crate::pci::serial_debug(" phys_lo=");
        crate::pci::serial_debug_hex(token as u32);
        crate::pci::serial_debug(" map_info=");
        crate::pci::serial_debug_hex(map_info);
        crate::pci::serial_debug("\n");
        if map_info & crate::virtio_gpu::VIRTIO_GPU_MAP_CACHE_MASK
            > crate::virtio_gpu::VIRTIO_GPU_MAP_CACHE_CACHED
        {
            // WC (3) or UNCACHED (2). Recorded above and honoured by sys_mmap
            // through `blob_map_cache_type`; still logged, because a non-cached
            // mapping is a throughput cliff worth seeing in a trace — no longer
            // because it is a divergence.
            crate::pci::serial_debug(
                "[DRM] non-cached blob mapping honoured (uncached)\n",
            );
        }
        Ok(token)
    }

    /// DRM_IOCTL_VIRTGPU_RESOURCE_INFO — describe the resource behind a BO
    /// handle. This is the last virtgpu ioctl Mesa's Venus ICD needs from us.
    ///
    /// Mirrors upstream `virtio_gpu_resource_info_ioctl`: look the GEM object up
    /// by `bo_handle`, then report
    ///   * `res_handle` — the host's resource id,
    ///   * `size`       — the GEM object's size, i.e. the PAGE-ALIGNED creation
    ///                    size (upstream reads `qobj->base.base.size`, and
    ///                    `virtio_gpu_object_create` rounds the request up to a
    ///                    page). Deliberately NOT the buddy allocation, which is
    ///                    rounded further to a power of two: Mesa takes this
    ///                    value as the mmap size for the blob, so over-reporting
    ///                    would hand it a window past the resource.
    ///   * `blob_mem`   — the blob memory type, which upstream leaves at the
    ///                    caller's zero for a non-blob resource.
    /// `bo_handle` is an input and is left exactly as the caller set it.
    ///
    /// Mesa's two callers (`virtgpu_bo_create_from_dma_buf`,
    /// `virtgpu_bo_create_from_device_memory` in vn_renderer_virtgpu.c) both
    /// require `blob_mem` to equal the type they allocate with and `size` to be
    /// at least the size they asked for, so all three outputs are load-bearing —
    /// returning 0 here would fail the import rather than degrade it.
    ///
    /// Open-scoped, like the other handle-consuming ioctls. The note that used
    /// to sit here said this was deliberately NOT scoped because MAP and
    /// GEM_CLOSE were not either, and that scoping only the query would be
    /// incoherent — a handle another open could still map and close, but not
    /// describe. That reasoning was right, and the fix was to scope all three
    /// together rather than to leave all three global: `BlobHandle::owner` now
    /// carries the ownership that upstream's per-`drm_file` GEM table carries.
    /// It costs Mesa nothing: it creates and queries on the same fd.
    ///
    /// Only blob BOs are known here. A dumb-buffer handle has no `res_handle`
    /// recorded (DumbBuf carries no host resource id), and Venus never creates
    /// dumb buffers, so an unknown handle is refused with NotFound — upstream's
    /// -ENOENT for a handle lookup miss.
    fn virtgpu_handle_resource_info(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // Read the input BEFORE taking any lock, and write the outputs back
        // AFTER dropping it: a demand fault taken under a spinlock is the
        // 82d0cc3 all-vCPU freeze class. No device round-trip is needed — this
        // is pure guest-side bookkeeping — so VIRTIO_GPU is never locked at all.
        let req = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_resource_info) };

        let blob = match blob_lookup(req.bo_handle, open_id) {
            Some(b) => b,
            None => {
                crate::pci::serial_debug("[DRM] RESOURCE_INFO: unknown bo_handle=");
                crate::pci::serial_debug_hex(req.bo_handle);
                crate::pci::serial_debug("\n");
                return Err(DriverError::NotFound);
            }
        };

        // Page-align, as the GEM object size is. The cast cannot truncate:
        // RESOURCE_CREATE_BLOB caps `size` at MAX_BLOB_BYTES (64 MiB).
        let size = ((blob.size + 0xFFF) & !0xFFFu64) as u32;

        // Write the three output fields individually (offsets 4/8/12), leaving
        // the caller's `bo_handle` at offset 0 untouched — same discipline as
        // RESOURCE_CREATE_BLOB's write-back.
        unsafe {
            (arg as *mut u8).add(4).cast::<u32>().write_volatile(blob.res_handle);
            (arg as *mut u8).add(8).cast::<u32>().write_volatile(size);
            (arg as *mut u8).add(12).cast::<u32>().write_volatile(blob.blob_mem);
        }
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_WAIT — block until the work submitted against the BO
    /// named by `handle` has retired.
    ///
    /// `handle` is a BO handle and is now used as one. Upstream
    /// (`virtio_gpu_wait_ioctl`) looks the GEM object up and waits on ITS fence,
    /// answering -ENOENT for a handle that names nothing. This used to ignore
    /// the handle entirely and consult the process-global `LAST_EXEC_FENCE`, so
    /// a wait on one open's buffer was answered from another open's submission.
    ///
    /// Three outcomes, all of them meaning something distinct:
    ///   * handle names no BO this open may reach → refused (upstream -ENOENT);
    ///   * BO exists, `last_fence == 0` → it was never named in any submission,
    ///     so nothing is outstanding for it: success, with no device round-trip;
    ///   * BO has a fence → ask the device whether it retired.
    /// Submission is a synchronous busy-spin, so the third case is always
    /// already retired today; it is asked rather than assumed so that the answer
    /// stays correct when that stops being true.
    fn virtgpu_handle_wait(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let w = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_3d_wait) };
        if w.handle == 0 { return Err(DriverError::InvalidParameter); }

        let fence = match bo_fence(w.handle, open_id) {
            Some(f) => f,
            None => {
                crate::pci::serial_debug("[DRM] VIRTGPU_WAIT: unknown bo_handle=");
                crate::pci::serial_debug_hex(w.handle);
                crate::pci::serial_debug("\n");
                return Err(DriverError::NotFound);
            }
        };
        if fence == 0 {
            return Ok(0);
        }
        let retired = {
            let guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_ref().ok_or(DriverError::NotFound)?;
            gpu.fence_retired(fence)
        };
        if retired { Ok(0) } else { Err(DriverError::Io) }
    }

    // ── DRM_IOCTL_SYNCOBJ_* ──────────────────────────────────────────────────
    //
    // ERRNO IS THE CONTRACT HERE. These arms return `DriverError` like every
    // other arm, but the DRM server maps them through a *syncobj-specific*
    // table (`servers/drm/src/lib.rs`) because Mesa branches on the value:
    //
    //   InvalidParameter -> EINVAL (22)   bad flags/count, NULL fence w/o
    //                                     WAIT_FOR_SUBMIT, DESTROY of nothing
    //   NotFound         -> ENOENT (2)    a handle no such open may reach
    //   Busy             -> ETIME  (62)   WAIT ran out of time
    //   Io               -> EINTR  (4)    WAIT interrupted by a signal
    //   Unsupported      -> ENOSYS (38)   timeline / fd-export, not implemented
    //
    // `Busy`/`Io` are puns: `drivers/src/lib.rs` has no Timeout or Interrupted
    // variant and is owned elsewhere. They are unambiguous because the mapping
    // is keyed on the ioctl being a syncobj one, and no syncobj arm returns
    // either for any other reason.

    /// DRM_IOCTL_SYNCOBJ_CREATE. `flags` may only be CREATE_SIGNALED.
    fn syncobj_handle_create(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let mut c = unsafe { ptr::read_unaligned(arg as *const drm_syncobj_create) };
        if c.flags & !DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
            return Err(DriverError::InvalidParameter);
        }
        let handle = NEXT_SYNCOBJ_HANDLE.fetch_add(1, Ordering::Relaxed);
        // Never reused within a boot, so a stale handle resolves to nothing
        // rather than to somebody else's syncobj — the same property
        // `NEXT_BO_OBJ` buys for BOs. 2^32 creates is not reachable.
        if handle == 0 { return Err(DriverError::Io); }
        {
            let mut map = SYNCOBJS.lock();
            map.insert(handle, SyncObj {
                owner: open_id,
                signaled: c.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0,
                fence: 0,
                refs: 1,
            });
        }
        // Write-back with the map dropped: user memory, never under a lock.
        c.handle = handle;
        unsafe { ptr::write_unaligned(arg as *mut drm_syncobj_create, c); }
        Ok(0)
    }

    /// DRM_IOCTL_SYNCOBJ_DESTROY. Upstream answers **EINVAL**, not ENOENT, for
    /// a handle that is not there (`drm_syncobj_destroy_ioctl` fails the
    /// `idr_remove`), which is why this one arm differs from RESET/SIGNAL/WAIT.
    fn syncobj_handle_destroy(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let d = unsafe { ptr::read_unaligned(arg as *const drm_syncobj_destroy) };
        if d.pad != 0 { return Err(DriverError::InvalidParameter); }
        let mut map = SYNCOBJS.lock();
        match map.get(&d.handle) {
            Some(s) if open_may_reach(open_id, s.owner) => {}
            _ => return Err(DriverError::InvalidParameter),
        }
        syncobj_put(&mut map, d.handle);
        Ok(0)
    }

    /// DRM_IOCTL_SYNCOBJ_RESET / _SIGNAL — both take `drm_syncobj_array`.
    ///
    /// All-or-nothing, like upstream's `drm_syncobj_array_find`: every handle
    /// is validated before any of them is mutated, so a bad handle late in the
    /// array cannot leave the earlier ones half-signalled.
    fn syncobj_handle_array(
        &mut self,
        arg: usize,
        open_id: u32,
        signal: bool,
    ) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let a = unsafe { ptr::read_unaligned(arg as *const drm_syncobj_array) };
        if a.pad != 0 { return Err(DriverError::InvalidParameter); }
        // Copied out of user memory BEFORE the lock — see `syncobj_read_handles`.
        let handles = syncobj_read_handles(a.handles, a.count_handles)?;
        {
            let mut map = SYNCOBJS.lock();
            for &h in &handles {
                match map.get(&h) {
                    Some(s) if open_may_reach(open_id, s.owner) => {}
                    _ => return Err(DriverError::NotFound),
                }
            }
            for &h in &handles {
                if let Some(s) = map.get_mut(&h) {
                    if signal {
                        // A stub already-signalled fence, upstream's
                        // `drm_syncobj_replace_fence(syncobj, NULL)` + signal.
                        s.signaled = true;
                    } else {
                        // RESET installs the NULL fence. Both halves must go:
                        // leaving `fence` behind would let a since-retired id
                        // re-signal the container the client just cleared.
                        s.signaled = false;
                        s.fence = 0;
                    }
                }
            }
        }
        if signal { syncobj_wake(); }
        Ok(0)
    }

    /// DRM_IOCTL_SYNCOBJ_WAIT.
    ///
    /// HOW IT BLOCKS. The three-phase `block_on_poll_*` protocol, verbatim from
    /// `sys_epoll_wait` — prepare (publish Blocked-on-poll while still
    /// running), re-probe, then commit or cancel. The re-probe is what closes
    /// the check-then-sleep lost wake: a `SIGNAL` landing after the first probe
    /// either finds this task already Blocked (and `wake_poll` flips it Ready)
    /// or shows up in the re-probe. There is no busy-spin and no polling
    /// timer — a zero/past timeout returns without ever entering the loop body,
    /// and a real timeout sleeps to its deadline tick.
    ///
    /// NO LOCK IS HELD ACROSS THE BLOCK. `syncobj_probe` takes and drops
    /// `SYNCOBJS` inside itself; the handle array was copied out of user memory
    /// before the first probe; the only thing live across `yield` is a kernel
    /// `Vec`. That is the `82d0cc3` rule applied to a path that sleeps, where
    /// breaking it would not merely fault under a spinlock but hold one for the
    /// entire duration of a GPU job.
    fn syncobj_handle_wait(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let mut w = unsafe { ptr::read_unaligned(arg as *const drm_syncobj_wait) };
        const KNOWN_FLAGS: u32 = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
            | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
            | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE
            | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE;
        if w.flags & !KNOWN_FLAGS != 0 { return Err(DriverError::InvalidParameter); }
        if w.pad != 0 { return Err(DriverError::InvalidParameter); }
        let handles = syncobj_read_handles(w.handles, w.count_handles)?;
        let wait_all = w.flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
        // WAIT_AVAILABLE is deliberately NOT folded in here. On a timeline it
        // asks to wait for a *point* to become available rather than signalled;
        // on a binary syncobj it degenerates, and treating it as
        // WAIT_FOR_SUBMIT would silently turn an EINVAL that upstream returns
        // into an unbounded wait. Accepted as a known flag, otherwise ignored.
        let for_submit = w.flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0;
        // WAIT_DEADLINE only asks the driver to *boost* toward the deadline.
        // We have no scheduler to boost, so honouring it is a no-op.
        let deadline = syncobj_deadline_ticks(w.timeout_nsec);

        SYNCOBJ_WAITERS.fetch_add(1, Ordering::Relaxed);
        let outcome = loop {
            match syncobj_probe(&handles, open_id, wait_all, for_submit) {
                Err(e) => break Err(e),
                Ok(Some(first)) => break Ok(first),
                Ok(None) => {}
            }
            // A zero or already-expired timeout is a pure poll. This is also
            // the only exit for a wait that genuinely runs out of time.
            let dl = match deadline {
                None => break Err(DriverError::Busy), // ETIME
                Some(d) => d,
            };
            if sched::ticks() >= dl { break Err(DriverError::Busy); }
            // Upstream sleeps TASK_INTERRUPTIBLE and answers -ERESTARTSYS.
            // Returning EINTR is the honest equivalent at this seam; the
            // deadline is absolute, so a caller that restarts the ioctl waits
            // for exactly the time it had left.
            if sched::has_deliverable_signal() { break Err(DriverError::Io); }

            sched::block_on_poll_prepare_until(dl);
            let ready = !matches!(
                syncobj_probe(&handles, open_id, wait_all, for_submit),
                Ok(None)
            );
            if ready || sched::ticks() >= dl || sched::has_deliverable_signal() {
                sched::block_on_poll_cancel();
                continue;
            }
            sched::block_on_poll_commit();
        };
        SYNCOBJ_WAITERS.fetch_sub(1, Ordering::Relaxed);

        let first = outcome?;
        // Write-back only on success, and only with every lock dropped.
        w.first_signaled = first;
        unsafe { ptr::write_unaligned(arg as *mut drm_syncobj_wait, w); }
        Ok(0)
    }

    /// Timeline syncobjs (TIMELINE_WAIT / TIMELINE_SIGNAL / QUERY / TRANSFER)
    /// and sync-file export (HANDLE_TO_FD / FD_TO_HANDLE) — **ENOSYS, on
    /// purpose**, and reported as such rather than as a generic failure.
    ///
    /// TIMELINE: a correct timeline needs a *chain* of per-point fences, not a
    /// scalar payload — a point may be submitted-but-unsignalled, and
    /// `WAIT_AVAILABLE` exists precisely to distinguish that state. A scalar
    /// approximation would pass a smoke test and deadlock Mesa the first time
    /// it waited on a point that had been signalled out of order. It is also
    /// unnecessary: `DRM_CAP_SYNCOBJ_TIMELINE` reports 0, which is the flag
    /// Mesa's `vk_sync_timeline` reads to select its binary-syncobj emulation,
    /// and v3d's `caps.multisync` reads to stay off. v3d needs binary.
    ///
    /// HANDLE_TO_FD / FD_TO_HANDLE: minting an fd is a VFS/syscall-layer
    /// operation and there is no seam for it inside this crate — note that the
    /// PRIME arms in the dispatch below are `Unsupported` for the same reason
    /// and are intercepted a layer up. Native fence sync is not needed for the
    /// first v3d bring-up.
    fn syncobj_timeline_stub(&mut self, _arg: usize) -> Result<usize, DriverError> {
        Err(DriverError::Unsupported)
    }

    // ── V3D ioctl handlers ───────────────────────────────────────────────────

    /// Route a `DRM_IOCTL_V3D_*` request, or `None` if this is not one.
    ///
    /// A SEPARATE DISPATCH, AHEAD OF THE MAIN `match cmd`, AND NOT AN ARM OF IT.
    /// That is forced, not stylistic: **two v3d request codes are bit-identical
    /// to two virtgpu ones**, because both drivers number their private ioctls
    /// from `DRM_COMMAND_BASE` and two of the structs happen to be the same
    /// size —
    ///
    ///     0xC0106441 = V3D_WAIT_BO (16 B)  = VIRTGPU_MAP      (16 B)
    ///     0xC0106443 = V3D_MMAP_BO (16 B)  = VIRTGPU_GETPARAM (16 B)
    ///
    /// A flat `match` cannot hold both; the number alone does not identify the
    /// operation, and only the active backend does. Routing v3d first, and only
    /// when armed, gives exactly the required property: with the backend
    /// disarmed this function is one relaxed load and every existing arm is
    /// reached bit-identically, and with it armed the whole `DRM_COMMAND_BASE`
    /// block is v3d's, which is what "this device answers one driver ABI"
    /// means.
    ///
    /// `None` for a code in the block that is not a v3d code — a size that does
    /// not match, say — so it falls through to the main match's unknown-ioctl
    /// reporter, which prints `nr` and `size` and is precisely the diagnostic
    /// for a struct whose layout drifted from the caller's.
    fn v3d_dispatch(
        &mut self,
        cmd: u32,
        arg: usize,
        open_id: u32,
    ) -> Option<Result<usize, DriverError>> {
        // FIRST OF EACH KIND, ONCE PER BOOT. Not a debug hack — it is the only
        // way to tell "Mesa never opened this device" from "Mesa got as far as
        // CREATE_BO and then died in its own code", and those two look
        // identical from userspace when the client crashes with no output. One
        // `NoteSet` compare after the first call of each code, so a client in a
        // per-frame submit loop pays nothing. Its own note set rather than
        // `UNKNOWN_IOCTLS`, which is capped at 32 entries and is about a
        // different question.
        //
        // AFTER the `match`, not before it. Written the other way round it fired
        // for every ioctl that merely *reached* this function — VERSION,
        // GETRESOURCES, GETCONNECTOR — because the `_ => return None` arm had
        // not run yet, and a trace that reports non-v3d ioctls as v3d ones is
        // worse than no trace.
        let out = match cmd {
            DRM_IOCTL_V3D_GET_PARAM => self.v3d_handle_get_param(arg),
            DRM_IOCTL_V3D_CREATE_BO => self.v3d_handle_create_bo(arg, open_id),
            DRM_IOCTL_V3D_MMAP_BO => self.v3d_handle_mmap_bo(arg, open_id),
            DRM_IOCTL_V3D_GET_BO_OFFSET => self.v3d_handle_get_bo_offset(arg, open_id),
            DRM_IOCTL_V3D_WAIT_BO => self.v3d_handle_wait_bo(arg, open_id),
            DRM_IOCTL_V3D_SUBMIT_CL => self.v3d_handle_submit_cl(arg, open_id),
            DRM_IOCTL_V3D_SUBMIT_TFU
            | DRM_IOCTL_V3D_SUBMIT_CSD
            | DRM_IOCTL_V3D_SUBMIT_CPU
            | DRM_IOCTL_V3D_PERFMON_CREATE
            | DRM_IOCTL_V3D_PERFMON_DESTROY
            | DRM_IOCTL_V3D_PERFMON_GET_VALUES
            | DRM_IOCTL_V3D_PERFMON_GET_COUNTER
            | DRM_IOCTL_V3D_PERFMON_SET_GLOBAL => Self::v3d_enosys(cmd),
            _ => return None,
        };
        // GET_PARAM is keyed on the PARAM rather than the request code, because
        // every one of Mesa's device-info reads shares one code: without the
        // param id the trace cannot distinguish "Mesa read IDENT0 and died"
        // from "Mesa read all nine and died", which is the whole question when a
        // client crashes silently during screen creation. Read back out of the
        // caller's struct, which the handler has already validated.
        let key = if cmd == DRM_IOCTL_V3D_GET_PARAM && arg != 0 {
            0x8000_0000 | unsafe { ptr::read_unaligned(arg as *const u32) }
        } else {
            cmd
        };
        if V3D_FIRST_SEEN.lock().first(key) {
            crate::pci::serial_debug("[V3D] first ");
            if cmd == DRM_IOCTL_V3D_GET_PARAM {
                crate::pci::serial_debug("GET_PARAM param=");
                crate::pci::serial_debug_hex(key & 0x7FFF_FFFF);
            } else {
                crate::pci::serial_debug("ioctl nr=");
                crate::pci::serial_debug_hex(cmd & 0xFF);
            }
            crate::pci::serial_debug(if out.is_ok() { " -> ok\n" } else { " -> err\n" });
        }
        Some(out)
    }

    /// The v3d ioctls this backend deliberately does not implement.
    ///
    /// **ENOSYS, explicitly, not a generic failure.** Each one is a whole
    /// engine, and a stub for any of them would be a lie a caller could not
    /// detect:
    ///   * SUBMIT_TFU — the texture formatting unit. `DRM_V3D_PARAM_SUPPORTS_TFU`
    ///     reports 1 because that is what a Pi 5 reports and it is what
    ///     `v3d_screen.c:298` reads to advertise `PIPE_CAP_GENERATE_MIPMAP`.
    ///     The mismatch is intentional and is the one place this backend is
    ///     knowingly inconsistent: screen creation and every path that does not
    ///     generate a mipmap work, and a path that does gets a hard ENOSYS at
    ///     the exact ioctl instead of silently wrong texture data. Reporting 0
    ///     instead would change which Mesa code path runs and therefore reduce
    ///     what this stub covers, which is the opposite of its purpose.
    ///   * SUBMIT_CSD — compute. `SUPPORTS_CSD` reports 0, so Mesa never issues
    ///     one; the arm exists so that if it ever does, it is told why.
    ///   * SUBMIT_CPU — the CPU-job queue. `SUPPORTS_CPU_QUEUE` reports 0.
    ///   * PERFMON_* — `SUPPORTS_PERFMON` reports 0 and `MAX_PERF_COUNTERS`
    ///     reports 0, which together keep Mesa on its compiled-in counter table
    ///     and off this family entirely (see `V3D_MAX_PERF_COUNTERS`).
    ///
    /// Logged once per code per boot, through the same `UNKNOWN_IOCTLS` note set
    /// the unimplemented-ioctl reporter uses, so a client that retries in a loop
    /// cannot flood the console.
    fn v3d_enosys(cmd: u32) -> Result<usize, DriverError> {
        if UNKNOWN_IOCTLS.lock().first(cmd) {
            crate::pci::serial_debug("[DRM] v3d ioctl not implemented (ENOSYS) cmd=");
            crate::pci::serial_debug_hex(cmd);
            crate::pci::serial_debug(" nr=");
            crate::pci::serial_debug_hex(cmd & 0xFF);
            crate::pci::serial_debug(" (reported once per boot)\n");
        }
        Err(DriverError::Unsupported)
    }

    /// DRM_IOCTL_V3D_GET_PARAM — `struct drm_v3d_get_param { u32 param; u32 pad;
    /// u64 value; }`.
    ///
    /// **This is the gate on the whole driver.** `v3d_get_device_info` fails
    /// screen creation outright if either CORE0_IDENT0 or CORE0_IDENT1 errors
    /// (`v3d_device_info.c:52,58`), and `v3d_screen_create` then `goto fail`s
    /// (`v3d_screen.c:796`). See the IDENTITY block for what the values decode
    /// to and why each one is the number it is.
    ///
    /// An unknown param is EINVAL, as upstream's `v3d_get_param_ioctl` answers,
    /// and NOT a zero value: `v3d_has_feature` reads the return code, so
    /// answering 0 successfully for an id we have never heard of would claim we
    /// understood the question.
    fn v3d_handle_get_param(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let param = unsafe { ptr::read_unaligned(arg as *const u32) };
        let pad = unsafe { ptr::read_unaligned((arg as *const u32).add(1)) };
        if pad != 0 { return Err(DriverError::InvalidParameter); }

        let value: u64 = match param {
            V3D_PARAM_CORE0_IDENT0 => V3D_IDENT0,
            V3D_PARAM_CORE0_IDENT1 => V3D_IDENT1,
            // Per-core config (TMU/slice geometry) that Mesa never reads.
            V3D_PARAM_CORE0_IDENT2 => 0,
            V3D_PARAM_HUB_IDENT1 => V3D_HUB_IDENT1,
            // Hub MMU/AXI config. Not read by the gallium driver.
            V3D_PARAM_HUB_IDENT2 => 0,
            V3D_PARAM_HUB_IDENT3 => V3D_HUB_IDENT3,
            // UIF (Uniform InterleaveD Format) memory configuration. Mesa's UIF
            // layout code uses its own compiled-in constants
            // (`V3D_UIFCFG_BANKS`/`_PAGE_SIZE`/`_XOR_VALUE` in v3d_screen.h) and
            // never reads this register, so it is answered rather than refused
            // only so a probe of the whole block does not have to know that.
            V3D_PARAM_UIFCFG => 0,

            // Feature bits. Each one selects a Mesa code path; see `v3d_enosys`
            // for why TFU is 1 while SUBMIT_TFU is not implemented.
            V3D_PARAM_SUPPORTS_TFU => 1,
            V3D_PARAM_SUPPORTS_CSD => 0,
            // Costs nothing here (there are no caches to flush) and keeps Mesa
            // on the same submit path a Pi 5 takes: it sets
            // DRM_V3D_SUBMIT_CL_FLUSH_CACHE in `flags`, which SUBMIT_CL below
            // accepts as a known flag.
            V3D_PARAM_SUPPORTS_CACHE_FLUSH => 1,
            V3D_PARAM_SUPPORTS_PERFMON => 0,
            // **0 on purpose.** Advertising it turns every submit into an
            // extension chain (`drm_v3d_multi_sync` hung off `extensions`)
            // instead of the flat `in_sync_bcl`/`in_sync_rcl`/`out_sync` fields,
            // which is strictly more decode surface for no capability we have.
            V3D_PARAM_SUPPORTS_MULTISYNC_EXT => 0,
            V3D_PARAM_SUPPORTS_CPU_QUEUE => 0,
            V3D_PARAM_SUPPORTS_SUPER_PAGES => 0,
            V3D_PARAM_MAX_PERF_COUNTERS => V3D_MAX_PERF_COUNTERS,
            // GPU reset accounting. Answered (rather than refused) with 0: the
            // ioctl succeeding is what sets `devinfo->has_reset_counter`, and 0
            // is the truth — this device has never been reset, because it has
            // never run anything.
            V3D_PARAM_GLOBAL_RESET_COUNTER => 0,
            V3D_PARAM_CONTEXT_RESET_COUNTER => 0,

            _ => return Err(DriverError::InvalidParameter),
        };

        // `value` is at offset 8. Written with no lock held.
        unsafe { ((arg as *mut u8).add(8) as *mut u64).write_unaligned(value) };
        Ok(0)
    }

    /// DRM_IOCTL_V3D_CREATE_BO — `struct drm_v3d_create_bo { u32 size; u32
    /// flags; u32 handle; u32 offset; }`.
    ///
    /// Allocates physically contiguous guest pages and a GPU virtual address,
    /// and returns the gem handle plus that address. Contiguous because there is
    /// no V3D MMU behind this stub to scatter-gather with; when the real driver
    /// lands it will program the MMU instead and this becomes a
    /// non-contiguous allocation with the same interface.
    ///
    /// **NON-ROUND SIZES ARE THE NORMAL CASE, not an edge case.** Mesa
    /// over-allocates every resource it creates: `v3d_resource.c:113-116` adds
    /// `V3D_TFU_READAHEAD_SIZE` (64 B) to every non-`PIPE_BUFFER` resource and 4 B
    /// to every `PIPE_BUFFER`, precisely so the TFU's and `ldunifa`'s read-ahead
    /// cannot run off the end of the last page. So a 4096-byte texture arrives
    /// here as 4160 and MUST become a two-page allocation. Rounding down, or
    /// rejecting, would fail the very allocation Mesa makes most often.
    fn v3d_handle_create_bo(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let size = unsafe { ptr::read_unaligned(arg as *const u32) };
        let flags = unsafe { ptr::read_unaligned((arg as *const u32).add(1)) };
        // Upstream: "There are currently no values for the flags argument", and
        // `v3d_create_bo_ioctl` refuses a non-zero one. Refusing keeps a flag
        // added later from being silently ignored.
        if flags != 0 { return Err(DriverError::InvalidParameter); }
        if size == 0 { return Err(DriverError::InvalidParameter); }

        let pages = ((size as usize) + 4095) / 4096;
        // Buddy order convention, as at `kms.rs:117`: ceil_log2 of the page
        // count. Freeing with a different order corrupts the allocator, so the
        // order is stored on the object and never recomputed at teardown.
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        let alloc_bytes = ((1usize << order) * 4096) as u64;

        let phys = mm::buddy::alloc(order).ok_or(DriverError::Io)?;
        // Zeroed, like every other BO this driver hands out. A GPU BO whose
        // contents are whatever the buddy allocator last had there is an
        // information leak across processes, and the one class of bug that
        // looks like a rendering artefact rather than like a security hole.
        unsafe { ptr::write_bytes(mm::phys_to_virt(phys) as *mut u8, 0, alloc_bytes as usize); }

        let va = match v3d_va_alloc(alloc_bytes) {
            Some(v) => v,
            None => {
                mm::buddy::free(phys, order);
                return Err(DriverError::Io);
            }
        };

        // Object then handle, exactly as RESOURCE_CREATE_BLOB does it: the
        // handle IS the object's one initial reference (`BO LIFETIME`).
        let obj = NEXT_BO_OBJ.fetch_add(1, Ordering::Relaxed);
        BLOB_OBJS.lock().insert(
            obj,
            BlobObj {
                phys,
                order,
                // No host virtio-gpu resource exists behind a v3d BO. This is
                // what keeps `blob_unref` from talking to a device about it.
                res_handle: 0,
                // The ALLOCATED size, not the requested one: `handle_ioctl_mmap`
                // and `blob_map_cache_type` both test containment against it,
                // and a caller that mmaps its whole BO must be inside.
                size: alloc_bytes,
                blob_mem: 0,
                last_fence: 0,
                win_off: 0,
                map_phys: 0,
                map_info: 0,
                refs: 1,
                v3d_va: va,
            },
        );
        let handle = NEXT_BLOB_HANDLE.fetch_add(1, Ordering::Relaxed);
        BLOB_BUFFERS.lock().insert(handle, BlobHandle { obj, owner: open_id, ctx: 0 });

        // handle at offset 8, offset at offset 12. Written after every lock is
        // dropped.
        unsafe {
            (arg as *mut u8).add(8).cast::<u32>().write_unaligned(handle);
            (arg as *mut u8).add(12).cast::<u32>().write_unaligned(va as u32);
        }
        Ok(0)
    }

    /// DRM_IOCTL_V3D_MMAP_BO — `struct drm_v3d_mmap_bo { u32 handle; u32 flags;
    /// u64 offset; }`.
    ///
    /// Despite the name this performs no mapping; it turns a gem handle into the
    /// offset to pass to `mmap()` on the drm fd. That is this driver's existing
    /// token scheme unchanged — the offset IS the guest-physical base, validated
    /// on the way back in by `handle_ioctl_mmap` (0x1007) so a caller cannot map
    /// physical memory this device never handed out.
    ///
    /// The **cacheability** is the part that is not free. `blob_map_cache_type`
    /// answers write-combine for a v3d BO, which propagates through
    /// `servers/drm`'s `MMAP_HINT_UNCACHED` into `sys_mmap`. See the note there
    /// for why CACHED would pass under QEMU and fail on the Pi.
    fn v3d_handle_mmap_bo(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let handle = unsafe { ptr::read_unaligned(arg as *const u32) };
        let flags = unsafe { ptr::read_unaligned((arg as *const u32).add(1)) };
        if flags != 0 { return Err(DriverError::InvalidParameter); }

        let b = blob_lookup(handle, open_id).ok_or(DriverError::NotFound)?;
        // Scoped to v3d BOs even though the registry is shared: a virtgpu blob
        // reached through a v3d ioctl would be a caller confusing two ABIs, and
        // its token is not derived the same way.
        if b.v3d_va == 0 || b.phys == 0 { return Err(DriverError::NotFound); }

        unsafe { ((arg as *mut u8).add(8) as *mut u64).write_unaligned(b.phys as u64) };
        Ok(0)
    }

    /// DRM_IOCTL_V3D_GET_BO_OFFSET — `struct drm_v3d_get_bo_offset { u32 handle;
    /// u32 offset; }`.
    ///
    /// The GPU address CREATE_BO already returned. It exists for the handle that
    /// arrived some other way (an import), and Mesa calls it on the BO-cache
    /// path, so it must answer the same value for the life of the handle — which
    /// it does, because the address lives on the object and is released only
    /// when the object dies.
    fn v3d_handle_get_bo_offset(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let handle = unsafe { ptr::read_unaligned(arg as *const u32) };
        let b = blob_lookup(handle, open_id).ok_or(DriverError::NotFound)?;
        if b.v3d_va == 0 { return Err(DriverError::NotFound); }
        unsafe { (arg as *mut u8).add(4).cast::<u32>().write_unaligned(b.v3d_va as u32) };
        Ok(0)
    }

    /// DRM_IOCTL_V3D_WAIT_BO — `struct drm_v3d_wait_bo { u32 handle; u32 pad;
    /// u64 timeout_ns; }`.
    ///
    /// Waits for the last submission that named this BO. Built on the existing
    /// per-BO `bo_fence` state and the same `SYNCOBJ_FENCE_DONE` watermark
    /// SYNCOBJ_WAIT reads, so the two can never disagree about one fence.
    ///
    /// `timeout_ns` is a **RELATIVE** duration here, unlike `drm_syncobj_wait`'s
    /// absolute `timeout_nsec` — upstream v3d runs it through
    /// `nsecs_to_jiffies_timeout`, and it decrements the field by the elapsed
    /// time on the way out so an interrupted caller can re-issue the ioctl and
    /// wait only the remainder. Both behaviours are reproduced; getting the
    /// absolute/relative distinction wrong is a wait that either returns
    /// instantly forever or never returns.
    ///
    /// Timeout is ETIME (`DriverError::Busy`, punned as the syncobj family
    /// already does — `drivers/src/lib.rs` has no `Timeout` variant and is owned
    /// by another lane; the pun is unambiguous because `drm_errno` consults the
    /// v3d table only for v3d ioctls). An unknown handle is EINVAL, matching
    /// upstream's `drm_gem_dma_resv_wait`, not ENOENT.
    fn v3d_handle_wait_bo(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let handle = unsafe { ptr::read_unaligned(arg as *const u32) };
        let pad = unsafe { ptr::read_unaligned((arg as *const u32).add(1)) };
        if pad != 0 { return Err(DriverError::InvalidParameter); }
        let timeout_ns = unsafe { ((arg as *const u8).add(8) as *const u64).read_unaligned() };

        // Resolve BEFORE parking, and refuse an unknown handle even when the
        // timeout is zero: a poll of a handle that does not exist is still a
        // bad handle.
        let fence = bo_fence(handle, open_id).ok_or(DriverError::InvalidParameter)?;

        let start_ns = unsafe { arch_monotonic_ns() };
        // Relative -> absolute, then reuse the one deadline converter so the
        // rounding-up rule (never truncate a sub-tick timeout to a busy-poll —
        // the `fb398c7` class) is stated in exactly one place.
        let deadline = if timeout_ns == 0 {
            None
        } else {
            syncobj_deadline_ticks(start_ns.saturating_add(timeout_ns).min(i64::MAX as u64) as i64)
        };

        // Registered in the SYNCOBJ waiter count on purpose: that counter is
        // what `syncobj_retire_fence` consults before issuing a wake, and this
        // is a task parked on exactly the fence it retires. A separate counter
        // would mean a retirement that woke syncobj waiters and left BO waiters
        // asleep until the next tick.
        SYNCOBJ_WAITERS.fetch_add(1, Ordering::Relaxed);
        let outcome = loop {
            if v3d_fence_done(fence) { break Ok(()); }
            let dl = match deadline {
                None => break Err(DriverError::Busy), // ETIME — pure poll
                Some(d) => d,
            };
            if sched::ticks() >= dl { break Err(DriverError::Busy); }
            // Upstream sleeps interruptibly and answers -ERESTARTSYS; EINTR is
            // the honest equivalent at this seam, and the deadline written back
            // below is what makes a restart wait only the remainder.
            if sched::has_deliverable_signal() { break Err(DriverError::Io); }

            sched::block_on_poll_prepare_until(dl);
            if v3d_fence_done(fence) || sched::ticks() >= dl || sched::has_deliverable_signal() {
                sched::block_on_poll_cancel();
                continue;
            }
            sched::block_on_poll_commit();
        };
        SYNCOBJ_WAITERS.fetch_sub(1, Ordering::Relaxed);

        // Decrement the caller's timeout by however long this actually took.
        // Upstream does this on EVERY exit including the error ones, which is
        // why it is here rather than inside the success arm.
        let elapsed = unsafe { arch_monotonic_ns() }.saturating_sub(start_ns);
        let remaining = timeout_ns.saturating_sub(elapsed);
        unsafe { ((arg as *mut u8).add(8) as *mut u64).write_unaligned(remaining) };

        outcome.map(|_| 0usize)
    }

    /// DRM_IOCTL_V3D_SUBMIT_CL — **decode only**.
    ///
    /// Nothing is executed and the command lists are not even read: there is no
    /// V3D core under QEMU and pretending otherwise would be the whole value of
    /// this exercise thrown away. What IS real is everything around the
    /// execution — and that is the half that is expensive to get wrong later:
    ///
    ///   1. every field is validated, in upstream's order, with upstream's errno;
    ///   2. every handle in `bo_handles` must name a BO this open can reach;
    ///   3. `in_sync_bcl` / `in_sync_rcl` resolve through `syncobj_fence`, so a
    ///      stale syncobj handle is ENOENT here rather than a hang later;
    ///   4. a monotonic fence id is allocated and queued for **deferred**
    ///      retirement (see "The stub submit queue"), so the fence is genuinely
    ///      outstanding when this ioctl returns;
    ///   5. `out_sync` gets that fence via `syncobj_attach_fence`, and every BO
    ///      named gets it via `bo_attach_fence`, so WAIT_BO and SYNCOBJ_WAIT
    ///      both see the same one submission.
    ///
    /// DEPENDENCIES ARE SATISFIED BY CONSTRUCTION, not by ignoring them. There
    /// is one queue, ids are monotonic, and retirement is a watermark drained
    /// strictly from the front — so any fence this submission could name as a
    /// dependency is numerically smaller than the one it is about to get, and
    /// therefore retires first. A second queue (TFU, CSD) would break that and
    /// is exactly why neither exists yet.
    fn v3d_handle_submit_cl(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let u32_at = |i: usize| -> u32 { unsafe { ptr::read_unaligned((arg as *const u32).add(i)) } };
        let u64_at = |b: usize| -> u64 { unsafe { ((arg as *const u8).add(b) as *const u64).read_unaligned() } };

        let bcl_start = u32_at(0);
        let bcl_end = u32_at(1);
        let rcl_start = u32_at(2);
        let rcl_end = u32_at(3);
        let in_sync_bcl = u32_at(4);
        let in_sync_rcl = u32_at(5);
        let out_sync = u32_at(6);
        let bo_handles = u64_at(40);
        let bo_handle_count = u32_at(12); // byte offset 48
        let flags = u32_at(13); // byte offset 52
        let perfmon_id = u32_at(14); // byte offset 56
        let pad = u32_at(15); // byte offset 60
        let extensions = u64_at(64);

        if pad != 0 { return Err(DriverError::InvalidParameter); }

        const V3D_SUBMIT_CL_FLUSH_CACHE: u32 = 0x01;
        const V3D_SUBMIT_EXTENSION: u32 = 0x02;
        if flags & !(V3D_SUBMIT_CL_FLUSH_CACHE | V3D_SUBMIT_EXTENSION) != 0 {
            return Err(DriverError::InvalidParameter);
        }
        // We advertise `SUPPORTS_MULTISYNC_EXT = 0`, so no client has any reason
        // to hang an extension chain off this submit and there is no decoder for
        // one. Refusing is what turns "we quietly ignored your wait/signal
        // dependencies" — which would show up as a race, days later — into an
        // error at the call site. Both the flag and a stray pointer are refused,
        // because upstream only walks the chain when the flag is set and a
        // non-zero pointer without it is a caller that thinks otherwise.
        if flags & V3D_SUBMIT_EXTENSION != 0 || extensions != 0 {
            return Err(DriverError::InvalidParameter);
        }
        // `SUPPORTS_PERFMON` is 0, so there are no perfmon ids to find.
        // Upstream answers ENOENT for an id that does not resolve; every id
        // fails to resolve here.
        if perfmon_id != 0 { return Err(DriverError::NotFound); }
        // Upstream validates the CL extents (`v3d_job_init` / the CL bounds
        // checks): an end before its start is a command list of negative length.
        // A zero-length BCL is legal — an RCL-only blit submits exactly that.
        if bcl_end < bcl_start || rcl_end < rcl_start {
            return Err(DriverError::InvalidParameter);
        }

        // USER MEMORY FIRST, LOCKS AFTER — the 82d0cc3 rule. `syncobj_read_handles`
        // is the shared copier and states it; a BO handle array is the same
        // shape as a syncobj handle array and gets the same cap.
        let handles = if bo_handle_count == 0 {
            Vec::new()
        } else {
            syncobj_read_handles(bo_handles, bo_handle_count)?
        };
        // Upstream's `v3d_lookup_bos` fails the whole submit on the first handle
        // that does not resolve, before any of them is fenced. Validate the
        // entire list before attaching anything, so a rejected submit leaves no
        // BO carrying a fence for work that will never run.
        for &h in handles.iter() {
            if !bo_exists(h, open_id) { return Err(DriverError::NotFound); }
        }

        // Dependencies. `Some(0)` is a real answer — the syncobj exists and holds
        // no fence, which is a satisfied dependency — and `None` is "this handle
        // names nothing you can reach", which is ENOENT.
        for h in [in_sync_bcl, in_sync_rcl] {
            if h != 0 && syncobj_fence(h, open_id).is_none() {
                return Err(DriverError::NotFound);
            }
        }
        // Resolve the out-sync BEFORE allocating a fence, so a bad handle costs
        // no fence id and queues no phantom retirement. It is re-resolved by the
        // attach below; the window between is a concurrent DESTROY on another
        // thread, which upstream races the same way and which costs only a fence
        // that retires with nothing listening.
        if out_sync != 0 && syncobj_fence(out_sync, open_id).is_none() {
            return Err(DriverError::NotFound);
        }

        // Everything is valid: take a fence id and queue the deferred retirement.
        let fence = v3d_submit_fence().ok_or(DriverError::Busy)?;

        if out_sync != 0 && !syncobj_attach_fence(out_sync, open_id, fence) {
            return Err(DriverError::NotFound);
        }
        // Upstream attaches the job's fence to every BO the job named, which is
        // what makes WAIT_BO answer about the right work. A handle closed
        // between validation and here returns false, which is benign — a closed
        // BO is one nothing can wait on.
        for &h in handles.iter() {
            bo_attach_fence(h, open_id, fence);
        }

        gdbg("[V3D] SUBMIT_CL fence=");
        gdbg_hex_64(fence);
        gdbg(" bos=");
        gdbg_hex(bo_handle_count);
        gdbg("\n");
        Ok(0)
    }

    fn virtgpu_handle_transfer_to_host(&mut self, _arg: usize) -> Result<usize, DriverError> {
        crate::pci::rdebug("[DRM] Virtio-GPU Transfer To Host\n");
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let _res = gpu.send_command(crate::virtio_gpu::VirtioGpuCmd::TransferToHost3d, &[]);
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn virtgpu_handle_transfer_from_host(&mut self, _arg: usize) -> Result<usize, DriverError> {
        crate::pci::rdebug("[DRM] Virtio-GPU Transfer From Host\n");
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let _res = gpu.send_command(crate::virtio_gpu::VirtioGpuCmd::TransferFromHost3d, &[]);
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }
}

impl Driver for DrmDeviceInterface {
    fn probe(&mut self) -> Result<(), DriverError> {
        self.driver.probe()
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        // Parse DRM ioctl from message
        let cmd = msg.tag as u32;
        let arg = if msg.data.len() >= 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&msg.data[0..8]);
            usize::from_le_bytes(bytes)
        } else {
            0
        };

        // open_id 0: this legacy path is a raw port message with no VFS fd
        // behind it, so it carries no open identity. The 3D arms (CONTEXT_INIT,
        // EXECBUFFER, RESOURCE_CREATE_BLOB) therefore fail here by design.
        // Nothing routes real traffic through it — /dev/dri/card0 goes via
        // VFS_IOCTL in servers/drm.
        match self.handle_ioctl(cmd, arg, 0) {
            Ok(result) => {
                let mut response = ipc::Message::empty();
                response.tag = 0; // Success
                let result_bytes = result.to_le_bytes();
                response.data[0..8].copy_from_slice(&result_bytes);
                response
            },
            Err(_) => {
                let mut response = ipc::Message::empty();
                response.tag = 1; // Error
                response
            },
        }
    }
}

/// DRM-specific dumb buffer structure
#[derive(Debug, Clone)]
pub struct DrmDumbBuffer {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub pitch: u32,
    pub size: u32,
    pub handle: u32,
    pub mmap_offset: usize,
}

impl DrmDumbBuffer {
    /// Create a dumb buffer for simple framebuffer access
    pub fn create(width: u32, height: u32, bpp: u32) -> Result<Self, DriverError> {
        let pitch = width * ((bpp + 7) / 8);
        let size = pitch * height;

        // Calculate pages and buddy order
        let pages = (size as usize + 4095) / 4096;
        let order = pages.next_power_of_two().trailing_zeros() as usize;

        crate::pci::rdebug("[DRM-IF] Creating dumb buffer ");
        crate::pci::rdebug_hex(width);
        crate::pci::rdebug("x");
        crate::pci::rdebug_hex(height);
        crate::pci::rdebug(" (order ");
        crate::pci::rdebug_hex(order as u32);
        crate::pci::rdebug(")\n");

        // Allocate physical memory for the framebuffer
        // We use buddy_alloc to get contiguous physical memory
        let phys_addr = mm::buddy::alloc(order).ok_or(DriverError::Io)? as u64;

        crate::pci::rdebug("[DRM-IF] Allocated at ");
        crate::pci::rdebug_hex_64(phys_addr);
        crate::pci::rdebug("\n");

        // Zero the newly allocated buffer
        let virt_addr = mm::phys_to_virt(phys_addr as usize) as *mut u8;
        unsafe {
            ptr::write_bytes(virt_addr, 0, size as usize);
        }

        let handle = Self::next_handle();
        // `refs: 1` is the gem handle itself; an exported dmabuf fd takes a
        // second (`BO LIFETIME`). `obj` is the lifetime identity the fd
        // remembers — never `handle`, which DESTROY_DUMB retires while the fd
        // is still open.
        DUMB_BUFFERS.lock().insert(
            handle,
            DumbBuf {
                phys: phys_addr as usize,
                order,
                last_fence: 0,
                obj: NEXT_BO_OBJ.fetch_add(1, Ordering::Relaxed),
                refs: 1,
                handle_live: true,
                // A dumb buffer's host resource is allocated lazily at ADDFB
                // time (see fb_resource_id), not here.
                res_id: 0,
            },
        );
        
        // mmap_offset for userspace will be the physical address
        // The syscall handler will use this to map the device memory
        let mmap_offset = phys_addr as usize;

        Ok(DrmDumbBuffer {
            width,
            height,
            bpp,
            pitch,
            size,
            handle,
            mmap_offset,
        })
    }

    /// Get next available handle
    fn next_handle() -> u32 {
        static mut NEXT_HANDLE: u32 = 1;
        unsafe {
            let handle = NEXT_HANDLE;
            NEXT_HANDLE += 1;
            handle
        }
    }
}

