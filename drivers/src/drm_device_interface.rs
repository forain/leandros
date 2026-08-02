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

// Virtio-GPU specific IOCTLs
// const DRM_IOCTL_VIRTGPU_MAP: u32 = 0xC0106401;
const DRM_IOCTL_VIRTGPU_EXECBUFFER: u32 = 0x40286402;
// const DRM_IOCTL_VIRTGPU_GETPARAM: u32 = 0xC0106403;
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: u32 = 0xC0286404;
// const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: u32 = 0xC0186405;
const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: u32 = 0xC0186406;
const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: u32 = 0xC0186407;
// const DRM_IOCTL_VIRTGPU_WAIT: u32 = 0x40086408;
const DRM_IOCTL_VIRTGPU_GET_CAPS: u32 = 0xC0086409;
// const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB: u32 = 0xC050640a;


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
    handle: u32,
    res_id: u32,
}

#[repr(C)]
struct drm_virtgpu_execbuffer {
    command: u64,
    size: u32,
    flags: u32,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    pad: u32,
}

#[repr(C)]
struct drm_virtgpu_get_caps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
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
            crate::pci::serial_debug("\n");
        }
    }
    let last = LAST_FLIP_DELIVER_TICK.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < 2 { return; } // < 20 ms since last delivery

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

    /// Handle incoming IPC messages
    pub fn handle_ioctl(&mut self, cmd: u32, arg: usize) -> Result<usize, DriverError> {
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
            DRM_IOCTL_VIRTGPU_EXECBUFFER => self.virtgpu_handle_execbuffer(arg),
            DRM_IOCTL_VIRTGPU_GET_CAPS => self.virtgpu_handle_get_caps(arg),
            DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST => self.virtgpu_handle_transfer_to_host(arg),
            DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST => self.virtgpu_handle_transfer_from_host(arg),

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
                            DRM_CURSOR_PLANE_ID => &mut cursor,
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
        if test_only { return Ok(0); }

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
        Ok(0)
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
            // The physical address was passed as the offset to mmap(). Only echo
            // it back if it names a dumb buffer we actually allocated — otherwise
            // a caller could map arbitrary physical memory through this device.
            let known = DUMB_BUFFERS.lock().values().any(|b| b.phys == requested_phys as usize);
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
        let create = unsafe { &mut *(arg as *mut drm_virtgpu_resource_create) };
        
        
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            // Send ResourceCreate3d command to Virtio-GPU
            let _res = gpu.send_command(crate::virtio_gpu::VirtioGpuCmd::ResourceCreate3d, &[]);
            create.handle = 1; // Simplified handle management
            create.res_id = 1;
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn virtgpu_handle_execbuffer(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let _exec = unsafe { &mut *(arg as *mut drm_virtgpu_execbuffer) };
        
        crate::pci::rdebug("[DRM] Virtio-GPU ExecBuffer\n");
        
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            // Send Submit3d command to Virtio-GPU
            let _res = gpu.send_command(crate::virtio_gpu::VirtioGpuCmd::Submit3d, &[]);
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn virtgpu_handle_get_caps(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let _caps = unsafe { &mut *(arg as *mut drm_virtgpu_get_caps) };
        
        crate::pci::rdebug("[DRM] Virtio-GPU Get Caps\n");
        
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let _res = gpu.send_command(crate::virtio_gpu::VirtioGpuCmd::GetCapset, &[]);
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
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

        match self.handle_ioctl(cmd, arg) {
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

