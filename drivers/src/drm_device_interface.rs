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

/// `drm_virtgpu_context_set_param.param` values.
const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;
const VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK: u64 = 0x0003;
const VIRTGPU_CONTEXT_PARAM_DEBUG_NAME: u64 = 0x0004;


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
use ::core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

/// A dumb buffer's physical base and buddy allocation order, so DESTROY_DUMB /
/// GEM_CLOSE can return the exact pages to the allocator (freeing the wrong
/// order corrupts the buddy allocator).
#[derive(Clone, Copy)]
struct DumbBuf {
    phys: usize,
    order: usize,
}

static DUMB_BUFFERS: Mutex<BTreeMap<u32, DumbBuf>> = Mutex::new(BTreeMap::new());

/// A virtgpu blob buffer object created through DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB.
/// `phys`/`order` are the guest pages handed to the host as the blob's backing
/// (zero for host-side blob memory, which the guest never owns pages for);
/// `res_handle` is the resource id the host knows it by.
#[derive(Clone, Copy)]
struct BlobBuf {
    phys: usize,
    order: usize,
    res_handle: u32,
    size: u64,
    /// `blob_mem` the blob was created with (VIRTIO_GPU_BLOB_MEM_*). Never 0:
    /// RESOURCE_CREATE_BLOB rejects blob_mem == 0, so every entry in this map is
    /// a real blob. RESOURCE_INFO reports it, and Mesa's Venus backend refuses
    /// an imported BO whose blob_mem is not the one it allocates with.
    blob_mem: u32,
    /// The 3D context this blob was attached to at creation (0 = none). Kept
    /// per-blob because the context is now per-open: freeing the blob must
    /// detach it from *its* context, not from whichever one happens to be
    /// current.
    ctx: u32,
    /// Host-visible window bookkeeping, both zero until RESOURCE_MAP_BLOB has
    /// succeeded for this blob (and zero forever for a guest-backed one):
    ///   * `win_off`  — byte offset of the reservation inside the shared-memory
    ///                  window, the value handed to RESOURCE_MAP_BLOB and the key
    ///                  `hostvis_free` releases.
    ///   * `map_phys` — the guest-physical address that offset resolves to
    ///                  (`window.phys + win_off`), which IS the mmap token
    ///                  VIRTGPU_MAP reports. Non-zero is the "is mapped" flag:
    ///                  the window base is a PCI BAR address and can never be 0.
    win_off: u64,
    map_phys: u64,
    /// `map_info` the host answered RESOURCE_MAP_BLOB with (VIRTIO_GPU_MAP_CACHE_*),
    /// recorded for diagnostics — see the cacheability note on `virtgpu_handle_map`.
    map_info: u32,
}

static BLOB_BUFFERS: Mutex<BTreeMap<u32, BlobBuf>> = Mutex::new(BTreeMap::new());
/// GEM handles for blob BOs. Kept well above the dumb-buffer handle space so a
/// handle is unambiguously one or the other.
static NEXT_BLOB_HANDLE: AtomicU32 = AtomicU32::new(0x4000);

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
}

impl GpuCtx {
    const fn empty() -> Self { Self { open_id: 0, ctx_id: 0, capset: 0 } }
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

fn ctx_bind(open_id: u32, ctx_id: u32, capset: u32) -> Result<(), u32> {
    if open_id == 0 { return Err(CTX_BIND_NO_SLOT); }
    let mut t = VIRTGPU_CTXS.lock();
    if let Some(c) = t.iter().find(|c| c.open_id == open_id) {
        // Non-zero by construction: only a successful ctx_create gets bound.
        return Err(c.ctx_id);
    }
    match t.iter_mut().find(|c| c.open_id == 0) {
        Some(slot) => { *slot = GpuCtx { open_id, ctx_id, capset }; Ok(()) }
        None => Err(CTX_BIND_NO_SLOT),
    }
}

/// The last fd on a card0 open closed: tear down that open's 3D context.
/// Called from the DRM server's VFS_CLOSE arm.
pub fn drm_release_open(open_id: u32) {
    if open_id == 0 { return; }
    // Take the slot and DROP the guard before touching the device — see the
    // lock-order note above.
    let ctx = {
        let mut t = VIRTGPU_CTXS.lock();
        match t.iter_mut().find(|c| c.open_id == open_id) {
            Some(c) => { let id = c.ctx_id; *c = GpuCtx::empty(); id }
            None => 0,
        }
    };
    if ctx == 0 { return; }

    // Blobs this open created and never closed. A Vulkan client that exits
    // (or crashes) without GEM_CLOSE would otherwise hold its host resources —
    // and, for host-side blobs, its slice of the shared-memory window — until
    // reboot. Contexts are per-open and a context id is never reused while live,
    // so `ctx` identifies this open's blobs exactly; blobs created with no
    // context (ctx == 0) belong to no open and are left alone.
    //
    // Collect under the lock, free after dropping it: `free_blob` locks
    // BLOB_BUFFERS itself and then talks to the device.
    let orphans: Vec<u32> = {
        let map = BLOB_BUFFERS.lock();
        map.iter().filter(|(_, b)| b.ctx == ctx).map(|(h, _)| *h).collect()
    };
    for h in orphans {
        DrmDeviceInterface::free_blob(h);
    }

    let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
    if let Some(gpu) = guard.as_mut() {
        gpu.ctx_destroy(ctx);
    }
}

/// Fence id produced by the most recent EXECBUFFER, for VIRTGPU_WAIT.
static LAST_EXEC_FENCE: AtomicU64 = AtomicU64::new(0);

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

/// Resolve a dumb-buffer GEM handle to its physical base + buddy order, so the
/// syscall layer can build a PRIME/dmabuf fd whose backing frames ARE this
/// buffer's contiguous pages (`phys .. phys + (1<<order)*4096`). Returns None
/// for an unknown handle. Copy-out to user memory happens in the syscall layer,
/// never here (this only reads the kernel-side registry).
pub fn dumb_buffer_phys_order(handle: u32) -> Option<(usize, usize)> {
    DUMB_BUFFERS.lock().get(&handle).map(|b| (b.phys, b.order))
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

/// Queue a FLIP_COMPLETE event for throttled delivery. Called from the PAGE_FLIP
/// ioctl (syscall context — a normal lock is fine; `drm_tick` uses try_lock).
fn queue_flip_event(crtc_id: u32, user_data: u64) {
    let seq = FLIP_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    let now = sched::ticks(); // 100 Hz monotonic
    let ev = drm_event_vblank {
        ev_type: DRM_EVENT_FLIP_COMPLETE,
        length: 32,
        user_data,
        tv_sec: (now / 100) as u32,
        tv_usec: ((now % 100) * 10_000) as u32,
        sequence: seq,
        crtc_id,
    };
    let mut blob = [0u8; 32];
    unsafe { ptr::copy_nonoverlapping(&ev as *const _ as *const u8, blob.as_mut_ptr(), 32); }
    PENDING_FLIPS.lock().push_back(blob);
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
            crate::pci::serial_debug("\n");
        }
    }
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

        // If this is a mode-setting or flip call, disable the kernel console
        if cmd == 0x1001 || cmd == 0x1004 || cmd == 0xC06864A2 || cmd == 0xC01864B0 {
            crate::pci::rdebug("[DRM-IF] Disabling console\n");
            crate::framebuffer::set_console_disabled(true);
        }

        // The DRM device lock is a spin::Mutex. It must NOT be held across any
        // dereference of user memory: a demand-paging fault taken under a spinlock
        // is the 82d0cc3 all-vCPU freeze class (no panic, IF=0 on every vCPU).
        // We therefore lock PER ARM, only around device-state access. The new K4
        // arms strictly copy the user struct into a kernel local BEFORE locking
        // and write results back AFTER dropping the lock. The pre-existing arms
        // operate on small fixed ioctl structs that the caller filled on its own
        // always-resident stack immediately before the syscall.
        let res = match cmd {
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
            DRM_IOCTL_MODE_ADDFB => { let d = get_drm_device(); let mut g = d.lock(); self.std_handle_addfb(&mut g, arg) },
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
            DRM_IOCTL_VIRTGPU_MAP => self.virtgpu_handle_map(arg),
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO => self.virtgpu_handle_resource_info(arg),
            DRM_IOCTL_VIRTGPU_WAIT => self.virtgpu_handle_wait(arg),

            // ── K4: Mesa/GBM buffer + Smithay/libdrm KMS surface ──
            DRM_IOCTL_GET_CAP => self.std_handle_get_cap(arg),
            DRM_IOCTL_SET_CLIENT_CAP => self.std_handle_set_client_cap(arg),
            // Root single-seat: master is not gated (SETCRTC/PAGE_FLIP never check
            // it), so accept the transitions unconditionally.
            DRM_IOCTL_SET_MASTER | DRM_IOCTL_DROP_MASTER => Ok(0),
            DRM_IOCTL_GET_MAGIC => self.std_handle_get_magic(arg),
            DRM_IOCTL_AUTH_MAGIC => Ok(0),
            DRM_IOCTL_GEM_CLOSE => self.std_handle_gem_close(arg),
            DRM_IOCTL_MODE_DESTROY_DUMB => self.std_handle_destroy_dumb(arg),
            DRM_IOCTL_MODE_ADDFB2 => self.std_handle_addfb2(arg),
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
            // No PRIME (single node, render==scanout) — Mesa falls back to software.
            DRM_IOCTL_PRIME_HANDLE_TO_FD | DRM_IOCTL_PRIME_FD_TO_HANDLE => Err(DriverError::Unsupported),

            _ => Err(DriverError::Unsupported),
        };

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

    /// Release DRM resources and re-enable kernel console
    pub fn release(&mut self) {
        crate::framebuffer::set_console_disabled(false);
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

        // If Virtio-GPU is present, create a resource for this framebuffer
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            // Use handle + 10 as resource ID to avoid conflict with kernel console (1)
            let res_id = buffer.handle + 10;
            gpu.create_resource_2d(res_id, width, height);
            gpu.attach_backing(res_id, mmap_offset as u64, width * height * 4);
            
            // Also store the resource ID in the FB's handles for flip_page
            if let Some(fb_obj) = device.framebuffers.get_mut(&DrmObjectId(fb_id)) {
                fb_obj.handles[0] = res_id;
            }
        }

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
        v.version_major = 1;
        v.version_minor = 6;
        v.version_patchlevel = 0;

        let name = "leandros-drm\0";
        let date = "20261201\0";
        let desc = "LeandrOS DRM driver\0";

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
        let buffers = DUMB_BUFFERS.lock();
        if let Some(b) = buffers.get(&map.handle) {
            map.offset = b.phys as u64;
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }
    fn std_handle_addfb(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let add = unsafe { &mut *(arg as *mut drm_mode_fb_cmd) };

        let mut fb = DrmFramebuffer::new(
            add.width,
            add.height,
            DrmFormat::Xrgb8888,
            add.handle,
            add.pitch
        );

        // Use the physical address associated with the dumb buffer handle
        let phys_addr = DUMB_BUFFERS.lock().get(&add.handle).map(|b| b.phys).unwrap_or(0);
        fb.physical_addresses[0] = phys_addr as u64;

        // If Virtio-GPU is present, create a resource for this framebuffer
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            // Use handle + 10 as resource ID to avoid conflict with kernel console (1)
            let res_id = add.handle + 10;
            gpu.create_resource_2d(res_id, add.width, add.height);
            gpu.attach_backing(res_id, phys_addr as u64, add.width * add.height * 4);
            fb.handles[0] = res_id;
        }

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

    /// Free a dumb buffer's pages back to the buddy allocator and forget it.
    fn free_dumb(handle: u32) {
        if let Some(b) = DUMB_BUFFERS.lock().remove(&handle) {
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
        let mut buf = vec![0u8; length];
        unsafe { ptr::copy_nonoverlapping(data as *const u8, buf.as_mut_ptr(), length); }

        let id = NEXT_BLOB_ID.fetch_add(1, Ordering::Relaxed);
        BLOBS.lock().insert(id, buf);
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
        let mut presented = false;
        if let Some(fb_id) = primary.fb_id {
            if fb_id != 0 {
                let t0 = if DRM_STATS { crate::snd::monotonic_us() } else { 0 };
                let r = {
                    let d = get_drm_device();
                    let mut g = d.lock();
                    let (mut src_w, mut src_h) = (320u32, 200u32);
                    if let Some(fb) = g.get_framebuffer(DrmObjectId(fb_id)) {
                        src_w = fb.width;
                        src_h = fb.height;
                    }
                    let flip_args = [fb_id, 0u32, src_w, src_h];
                    self.handle_flip_page(&mut g, flip_args.as_ptr() as usize)
                };
                if DRM_STATS {
                    FLIP_US_TOTAL
                        .fetch_add(crate::snd::monotonic_us().wrapping_sub(t0), Ordering::Relaxed);
                }
                r?;
                FLIPS_SUBMITTED.fetch_add(1, Ordering::Relaxed);
                presented = true;
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
    fn std_handle_gem_close(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let c = unsafe { ptr::read_unaligned(arg as *const drm_gem_close) };
        Self::free_dumb(c.handle);
        Self::free_blob(c.handle);
        Ok(0)
    }

    /// Release a blob BO: retract any host-visible mapping, detach it from the
    /// 3D context, drop the host-side resource, and return its guest pages.
    /// Without this each RESOURCE_CREATE_BLOB leaks a buddy allocation, a host
    /// resource id and — for a host-side blob — a slice of the shared-memory
    /// window, for the lifetime of the boot.
    fn free_blob(handle: u32) {
        let b = match BLOB_BUFFERS.lock().remove(&handle) {
            Some(b) => b,
            None => return,
        };
        // The context this blob was actually attached to. GEM_CLOSE carries no
        // open identity (it is a plain handle op, and the handle space is
        // global), so the binding has to be remembered on the blob itself.
        let ctx = b.ctx;
        {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            if let Some(gpu) = guard.as_mut() {
                // UNMAP before UNREF: the host holds the window sub-region on
                // behalf of a live resource, and unreferencing it first leaves
                // the subregion attached to a resource that no longer exists.
                if b.map_phys != 0 {
                    gpu.resource_unmap_blob(b.res_handle);
                }
                if ctx != 0 {
                    gpu.ctx_detach_resource(ctx, b.res_handle);
                }
                gpu.resource_unref(b.res_handle);
            }
        }
        // Return the window space unconditionally once the record is gone —
        // including when UNMAP_BLOB failed or the device had vanished. The
        // record is what `hostvis_free` is reachable from, so holding the
        // reservation back would leak it for the rest of the boot with nothing
        // left able to release it. Reusing an offset the host (wrongly) still
        // believes in fails closed rather than corrupts: the host refuses to map
        // a second resource over a live sub-region, so the next
        // RESOURCE_MAP_BLOB at that offset is rejected and rolls itself back.
        if b.map_phys != 0 {
            hostvis_free(b.win_off);
        }
        if b.phys != 0 {
            mm::buddy::free(b.phys, b.order);
        }
        let _ = b.size;
    }

    /// DRM_IOCTL_MODE_DESTROY_DUMB — free the dumb buffer.
    fn std_handle_destroy_dumb(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let d = unsafe { ptr::read_unaligned(arg as *const drm_mode_destroy_dumb) };
        Self::free_dumb(d.handle);
        Ok(0)
    }

    /// DRM_IOCTL_MODE_ADDFB2 — LINEAR only, plane 0. Same internal path as ADDFB.
    fn std_handle_addfb2(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let mut cmd2 = unsafe { ptr::read_unaligned(arg as *const drm_mode_fb_cmd2) };

        let handle = cmd2.handles[0];
        let width = cmd2.width;
        let height = cmd2.height;
        let pitch = if cmd2.pitches[0] != 0 { cmd2.pitches[0] } else { width * 4 };
        let phys_addr = DUMB_BUFFERS.lock().get(&handle).map(|b| b.phys).unwrap_or(0);

        let mut fb = DrmFramebuffer::new(width, height, DrmFormat::Xrgb8888, handle, pitch);
        fb.physical_addresses[0] = phys_addr as u64;

        // Bind a virtio-gpu resource so SETCRTC/PAGE_FLIP/DIRTYFB can transfer the
        // CPU-rendered pixels to the host. (This locks VIRTIO_GPU, not the DRM
        // device — no user memory is touched here.)
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let res_id = handle + 10;
            gpu.create_resource_2d(res_id, width, height);
            gpu.attach_backing(res_id, phys_addr as u64, width * height * 4);
            fb.handles[0] = res_id;
        }

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
            g.get_framebuffer(DrmObjectId(cmd.fb_id)).map(|fb| (fb.handles[0], fb.width, fb.height))
        };
        if let Some((res_id, w, h)) = flush_args {
            if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
                gpu.flush(res_id, 0, 0, w, h);
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
            let known = DUMB_BUFFERS.lock().values().any(|b| b.phys == requested_phys as usize)
                || BLOB_BUFFERS.lock().values().any(|b| {
                    (b.phys != 0 && b.phys == requested_phys as usize)
                        || (b.map_phys != 0
                            && requested_phys >= b.map_phys
                            && requested_phys - b.map_phys < b.size)
                });
            if !known {
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

        let res_handle = {
            let mut guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_mut().ok_or(DriverError::NotFound)?;
            // Allocate a real resource id rather than the fixed 1 the stub used,
            // and pass the caller's geometry through instead of an empty command
            // body (which, with the old opcode, was not even RESOURCE_CREATE_3D).
            let rid = gpu.alloc_resource_id();
            if !gpu.create_resource_3d(rid, req.width, req.height, req.format) {
                return Err(DriverError::Io);
            }
            rid
        };

        // bo_handle @40, res_handle @44 in drm_virtgpu_resource_create.
        unsafe {
            (arg as *mut u8).add(40).cast::<u32>().write_volatile(res_handle);
            (arg as *mut u8).add(44).cast::<u32>().write_volatile(res_handle);
        }
        Ok(0)
    }

    /// DRM_IOCTL_VIRTGPU_EXECBUFFER — forward the real command stream.
    ///
    /// The previous implementation bound `_exec` and never touched it, then
    /// submitted `&[]`: every command stream userspace ever produced was thrown
    /// away while the ioctl reported success.
    fn virtgpu_handle_execbuffer(&mut self, arg: usize, open_id: u32) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // Read the request out of user memory BEFORE any device lock is taken.
        let exec = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_execbuffer) };
        if exec.command == 0 || exec.size == 0 {
            return Err(DriverError::InvalidParameter);
        }
        const MAX_CMD_BYTES: usize = 4 << 20;
        let size = exec.size as usize;
        if size > MAX_CMD_BYTES { return Err(DriverError::InvalidParameter); }

        let ctx = ctx_lookup(open_id);
        if ctx == 0 {
            crate::pci::serial_debug("[DRM] EXECBUFFER before CONTEXT_INIT\n");
            return Err(DriverError::InvalidParameter);
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
            gpu.submit_3d(ctx, &cmds).map_err(|_| DriverError::Io)?
        };
        LAST_EXEC_FENCE.store(fence, Ordering::Relaxed);
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
        // Same treatment, same reason: a leaf lock, no device round-trip, and
        // the count is read out of the guard into a local BEFORE the user
        // pointer is touched (never write user memory under a spinlock).
        if req.param == VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS {
            let n = HOSTVIS_SPANS.lock().len() as u32;
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
        for p in params[..n].iter() {
            match p.param {
                VIRTGPU_CONTEXT_PARAM_CAPSET_ID => capset_id = p.value as u32,
                // Single ring only; accept and ignore the ring params rather
                // than failing a request that is satisfiable.
                VIRTGPU_CONTEXT_PARAM_NUM_RINGS
                | VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK
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
        if let Err(winner) = ctx_bind(open_id, ctx, capset_id) {
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
                Ok(()) => rid,
                Err(()) => {
                    drop(guard);
                    if phys != 0 { mm::buddy::free(phys, order); }
                    return Err(DriverError::Io);
                }
            }
        };

        let handle = NEXT_BLOB_HANDLE.fetch_add(1, Ordering::Relaxed);
        BLOB_BUFFERS.lock().insert(
            handle,
            BlobBuf {
                phys,
                order,
                res_handle,
                size: req.size,
                blob_mem: req.blob_mem,
                ctx,
                // Host-visible mapping is established lazily, by VIRTGPU_MAP.
                win_off: 0,
                map_phys: 0,
                map_info: 0,
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
    /// CACHEABILITY: the token carries no cache type, so the resulting user
    /// mapping gets the address space's normal cacheable attributes. That is
    /// correct for the Venus ring, which the host reports as
    /// VIRTIO_GPU_MAP_CACHE_CACHED; a host asking for WC or UNCACHED is honoured
    /// only in the log, and the divergence is called out there rather than
    /// silently ignored (`handle_ioctl_mmap` has no channel to pass a cache type
    /// back to `sys_mmap`).
    ///
    /// LOCKING: user memory is read before any lock is taken and written after
    /// every lock is dropped, and no two of BLOB_BUFFERS / HOSTVIS_SPANS /
    /// VIRTIO_GPU are ever held at the same time — the 82d0cc3 discipline.
    fn virtgpu_handle_map(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // `struct drm_virtgpu_map { u64 offset; u32 handle; u32 pad; }`.
        let handle = unsafe { (arg as *const u8).add(8).cast::<u32>().read_volatile() };

        // Copy the blob record out; hold nothing.
        let blob = BLOB_BUFFERS.lock().get(&handle).copied();

        let token: u64 = match blob {
            // ── Host-side blob memory ────────────────────────────────────────
            Some(b) if b.blob_mem == crate::virtio_gpu::VIRTIO_GPU_BLOB_MEM_HOST3D => {
                if b.map_phys != 0 {
                    b.map_phys // already mapped — idempotent
                } else {
                    self.hostvis_map_blob(handle, b)?
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
                let phys = DUMB_BUFFERS
                    .lock()
                    .get(&handle)
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
    /// VIRTIO_GPU is taken and released on its own, and BLOB_BUFFERS is taken
    /// last to record the result. Never two at once, never any across the device
    /// round-trip's busy-spin.
    fn hostvis_map_blob(&mut self, handle: u32, b: BlobBuf) -> Result<u64, DriverError> {
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

        // Record it. If the handle vanished (a concurrent GEM_CLOSE on another
        // thread), undo the map instead of leaking the window space — free_blob
        // could not have seen a reservation that did not exist yet.
        let recorded = {
            let mut map = BLOB_BUFFERS.lock();
            match map.get_mut(&handle) {
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
            // WC (3) or UNCACHED (2). The mapping below will still be cacheable;
            // say so rather than let a coherency surprise look like a Vulkan bug.
            crate::pci::serial_debug(
                "[DRM] WARNING: host asked for non-cached blob mapping; mapping cacheable anyway\n",
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
    /// NOT open-scoped, and it takes no `open_id`. Upstream resolves the handle
    /// through the per-`drm_file` GEM table, but this driver's BO handle space is
    /// process-global (`NEXT_BLOB_HANDLE` / `BLOB_BUFFERS`), and so are the two
    /// ioctls that already consume a handle — VIRTGPU_MAP and GEM_CLOSE. Scoping
    /// only the query would be strictly incoherent (a handle another open could
    /// still map and close, but not describe) and would amount to inventing half
    /// a new isolation model; per-open handle tables are a whole separate change.
    /// It costs Mesa nothing either way: it creates and queries on the same fd.
    ///
    /// Only blob BOs are known here. A dumb-buffer handle has no `res_handle`
    /// recorded (DumbBuf carries only phys/order), and Venus never creates dumb
    /// buffers, so an unknown handle is refused with NotFound — upstream's
    /// -ENOENT for a handle lookup miss.
    fn virtgpu_handle_resource_info(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        // Read the input BEFORE taking any lock, and write the outputs back
        // AFTER dropping it: a demand fault taken under a spinlock is the
        // 82d0cc3 all-vCPU freeze class. No device round-trip is needed — this
        // is pure guest-side bookkeeping — so VIRTIO_GPU is never locked at all.
        let req = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_resource_info) };

        let blob = match BLOB_BUFFERS.lock().get(&req.bo_handle) {
            Some(b) => *b,
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

    /// DRM_IOCTL_VIRTGPU_WAIT — block until the work fenced by the most recent
    /// EXECBUFFER has retired. Submission is synchronous, so by the time
    /// EXECBUFFER returned the fence was already signalled; this reports that
    /// truthfully instead of unconditionally succeeding.
    fn virtgpu_handle_wait(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let w = unsafe { ::core::ptr::read_volatile(arg as *const drm_virtgpu_3d_wait) };
        if w.handle == 0 { return Err(DriverError::InvalidParameter); }

        let fence = LAST_EXEC_FENCE.load(Ordering::Relaxed);
        if fence == 0 {
            // Nothing was ever submitted, so nothing is outstanding.
            return Ok(0);
        }
        let retired = {
            let guard = crate::virtio_gpu::VIRTIO_GPU.lock();
            let gpu = guard.as_ref().ok_or(DriverError::NotFound)?;
            gpu.fence_retired(fence)
        };
        if retired { Ok(0) } else { Err(DriverError::Io) }
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
        DUMB_BUFFERS.lock().insert(handle, DumbBuf { phys: phys_addr as usize, order });
        
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

