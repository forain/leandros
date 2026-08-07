# M8 research: DRM cursor plane lane — findings

Research wave, 2026-07-30. Repo-read-only; no builds, no QEMU runs.
Sources read: smithay `efeb597` (the rev cosmic-comp pins), cosmic-comp, drm-rs 0.14.1 /
drm-ffi 0.9.1 / drm-sys 0.8.1, QEMU v11.0.2 upstream sources, and the LeandrOS tree.

---

## VERDICT

**Build the real cursor plane — but the gate is not the cursor plane, it is ATOMIC KMS.**
smithay's legacy DRM backend never programs a hardware cursor: `DrmCompositor` unconditionally
executes `if surface.is_legacy() { planes.cursor.clear(); planes.overlay.clear(); }`
(`compositor/mod.rs:1164-1168` and `:1348-1352`), and the only `set_cursor` call anywhere in
smithay's DRM backend is a *null clear* during device reset (`device/legacy.rs:105-107`). No
kernel-side change — not a cursor plane in GETPLANERESOURCES, not `DRM_CAP_CURSOR_WIDTH`, not a
`MODE_CURSOR2` ioctl — can make the current `SMITHAY_USE_LEGACY=1` session use it. So the lane is:
accept `DRM_CLIENT_CAP_ATOMIC`, implement the atomic-KMS ioctl surface smithay actually exercises,
expose a second plane typed `Cursor`, and translate cursor-plane commits into virtio-gpu
`UPDATE_CURSOR`/`MOVE_CURSOR`. The host side is *not* a blocker: QEMU's cocoa backend implements
`dpy_cursor_define` and renders the cursor as a separate `CALayer`, so the win is real end to end
(Q3). The in-kernel software composite (Q2) is **not** a substitute — cosmic-comp hardcodes
`CursorMode::All` on every KMS path and has no config to hide its software cursor, so a
kernel-drawn cursor produces *two* cursors and leaves the ~1 fps full-screen recomposite exactly
where it is. Q2's genuinely valuable half is the damage-limited flush, and that only becomes
possible once atomic gives us `FB_DAMAGE_CLIPS`. Recommended sequencing: a cheap Stage 0 that
proves the virtio-gpu cursor queue + cocoa path works before committing to the ~1000-line atomic
build, then atomic, then cursor plane, then damage clips.

---

## Q1 — THE GATE: does smithay's LEGACY DRM path ever program a cursor plane?

### Answer: NO. Definitively, and by explicit design.

**Evidence 1 — the only `set_cursor` in smithay's entire DRM backend is a null clear.**
A grep for `set_cursor|SetCursor|move_cursor|MoveCursor` across
`~/.cargo/git/checkouts/smithay-312425d48e59d8c8/efeb597/src/backend/drm/` returns exactly one hit:

`src/backend/drm/device/legacy.rs:98-112` (`LegacyDrmDevice::reset_state`):

```rust
for crtc in res_handles.crtcs() {
    #[allow(deprecated)]
    let _ = self
        .fd
        .set_cursor(*crtc, Option::<&drm::control::dumbbuffer::DumbBuffer>::None);
    // null commit (necessary to trigger removal on the kernel side with the legacy api.)
    self.fd.set_crtc(*crtc, None, (0, 0), &[], None)
```

That is `drmModeSetCursor(crtc, 0, 0, 0)` — a *teardown*, issued once at device init, whose error is
discarded (`let _ =`). There is no `drmModeSetCursor2` and no `drmModeMoveCursor` anywhere.

**Evidence 2 — `DrmCompositor` deletes every cursor and overlay plane when the surface is legacy.**
`src/backend/drm/compositor/mod.rs:1164-1168` (in `DrmCompositor::new`) and the identical block at
`:1348-1352` (in `new_with_gbm`/the second constructor):

```rust
// We do not support direct scan-out on legacy
if surface.is_legacy() {
    planes.cursor.clear();
    planes.overlay.clear();
}
```

`is_legacy()` is `matches!(&*self.internal, DrmSurfaceInternal::Legacy(_))`
(`src/backend/drm/surface/mod.rs:201-203`).

**Evidence 3 — it is documented as a deliberate limitation.**
`src/backend/drm/compositor/mod.rs:14-15`:

```
//! Note: While the [`DrmCompositor`] also works on *legacy* drm the use of overlay and cursor planes is disabled in that case.
//! Direct scan-out will only work with an atomic [`DrmSurface`].
```

**Evidence 4 — the legacy/atomic fork is decided exactly where we thought.**
`src/backend/drm/device/mod.rs:236-258`:

```rust
if force_legacy {
    info!("SMITHAY_USE_LEGACY is set. Forcing LegacyDrmDevice.");
};
Ok(
    if !force_legacy && fd.set_client_capability(ClientCapability::Atomic, true).is_ok() {
        DrmDeviceInternal::Atomic(AtomicDrmDevice::new(fd, active, disable_connectors)?)
    } else {
        info!("Falling back to LegacyDrmDevice");
        DrmDeviceInternal::Legacy(LegacyDrmDevice::new(fd, active, disable_connectors)?)
    },
)
```

We currently fail this *twice over*: the launcher sets `SMITHAY_USE_LEGACY=1`, and
`std_handle_set_client_cap` returns `Err(DriverError::Unsupported)` for
`DRM_CLIENT_CAP_ATOMIC` (`drivers/src/drm_device_interface.rs:1167-1175`).

**Corollary — the cursor is guaranteed to land on the primary plane today.**
cosmic-comp hardcodes `CursorMode::All` on every KMS render path
(`cosmic-comp/src/backend/kms/device.rs:944`, `kms/surface/mod.rs:1066`,
`kms/mod.rs:979`, `:1090`, `:1135`); `cursor_elements` then always calls `cursor::draw_cursor`
(`src/backend/render/mod.rs:511-512`). There is **no** config knob to suppress it. That is why every
1-pixel pointer move triggers a full recomposite.

### What it takes instead: ATOMIC KMS

Once `is_legacy()` is false, the cursor path lights up automatically, because cosmic-comp already
tags its cursor element `Kind::Cursor` (`src/backend/render/cursor.rs:217`, `:409`) — the exact
predicate smithay's `try_assign_cursor_plane` requires
(`compositor/mod.rs:3047-3050`: "skipping element … element kind not cursor").

And the payoff is confirmed in the same file: when the cursor is on the cursor plane, the primary
plane sees no damage and the GL render is skipped entirely —
`compositor/mod.rs:2318` `trace!("skipping primary plane, no damage");`. A pointer-only frame
becomes a single atomic commit that touches four plane properties. Zero softpipe work, zero pixel
traffic.

#### Conditions the driver must satisfy

1. **`DRM_IOCTL_SET_CLIENT_CAP` must accept `DRM_CLIENT_CAP_ATOMIC` (3)** and
   `DRM_CLIENT_CAP_UNIVERSAL_PLANES` (2) — the latter already returns `Ok(0)`.
   Note `planes()` gates on universal planes: `mod.rs:212-215`
   `cursor: if has_universal_planes { cursor } else { Vec::new() }`.
2. **`GETPLANERESOURCES` must report ≥2 planes**, one with property `type` = 1 (Primary) and one
   with `type` = 2 (Cursor). `plane_type()` (`mod.rs:218+`) reads the enum name/value; `planes()`
   sorts them into `primary`/`cursor`/`overlay` (`mod.rs:198-210`). Both must have
   `possible_crtcs` bit 0 set (`mod.rs:186` `resources.filter_crtcs(filter).contains(crtc)`).
3. **`DRM_CAP_CURSOR_WIDTH` (0x8) / `DRM_CAP_CURSOR_HEIGHT` (0x9) must return 64.**
   Today the real GET_CAP path falls into `_ => 0` (`drm_device_interface.rs:1157-1160`), so
   `DrmDevice::cursor_size()` (`device/mod.rs:291`) is 0x0 and
   `try_assign_cursor_plane` rejects every element as "too big"
   (`compositor/mod.rs:3058-3062`). (There *is* a `0x8 => 64, 0x9 => 64` at
   `drm_device_interface.rs:848-849`, but it is on the dead custom ioctl `0x1006` that no Linux
   client ever calls.) 64 is also mandated by QEMU — see Q3.
4. **A gbm allocator must be available** — `cursor_state` is `gbm.map(|gbm| …)`
   (`compositor/mod.rs:1220`, `:1402`; doc at `:1116` "`None` will disable the cursor plane").
   cosmic-comp already passes one (`GbmDrmOutputManager::new` at
   `cosmic-comp/src/backend/kms/device.rs:797-803`, `GbmDevice::new(fd)` at `:714`). With
   `DRM_CAP_ADDFB2_MODIFIERS = 0` and our swrast Mesa, `gbm_bo_create` for the 64x64 cursor
   resolves to `MODE_CREATE_DUMB` + `ADDFB2` — both already implemented
   (`drm_device_interface.rs:1007`, `:1300`) — and `bo.map_mut` (used by
   `copy_element_to_cursor_bo`, `compositor/mod.rs:4197-4240`) resolves to `MODE_MAP_DUMB` + mmap,
   also already implemented (`:1019`, `:1390`).
5. **Property enumeration must be complete and correctly typed** — see the hard hazard below.

#### The ioctls smithay will call, in order

Device open / `AtomicDrmDevice::new` (`device/atomic.rs:92+`):

| # | ioctl | code | notes |
|---|---|---|---|
| 1 | `SET_CLIENT_CAP` (ATOMIC) | `0x4010640D` | must now return 0 |
| 2 | `MODE_GETRESOURCES` | `0xC04064A0` | already implemented |
| 3 | `MODE_GETPLANERESOURCES` | `0xC01064B5` | must now report 2 planes |
| 4 | `MODE_OBJ_GETPROPERTIES` × every connector, crtc, plane | `0xC02064B9` | two-pass |
| 5 | `MODE_GETPROPERTY` × every property id returned above | `0xC04064AA` | two-pass |
| 6 | `MODE_GETCRTC` | `0xC06864A1` | already implemented |
| 7 | `MODE_CREATEPROPBLOB` (mode blob) | `0xC01064BD` | **new** |
| 8 | `MODE_ATOMIC` (TEST_ONLY \| ALLOW_MODESET) | `0xC03864BC` | **new** |
| 9 | `MODE_ATOMIC` (ALLOW_MODESET, blocking) | `0xC03864BC` | the modeset |
| 10 | `MODE_ATOMIC` (PAGE_FLIP_EVENT \| NONBLOCK) | `0xC03864BC` | every frame |
| 11 | `MODE_DESTROYPROPBLOB` | `0xC00464BE` | **new** |
| 12 | `MODE_GETPROPBLOB` | `0xC01064AC` | **new**; only needed if we advertise blob props (`IN_FORMATS`, `SIZE_HINTS`) — we should not, then this is never called |

Commit flag values (`drm-sys-0.8.1/src/bindings.rs:188-197`, `drm-0.14.1/src/control/mod.rs:1521-1533`):
`PAGE_FLIP_EVENT = 1`, `PAGE_FLIP_ASYNC = 2`, `ATOMIC_TEST_ONLY = 256 (0x100)`,
`ATOMIC_NONBLOCK = 512 (0x200)`, `ATOMIC_ALLOW_MODESET = 1024 (0x400)`.
smithay's use sites: `surface/atomic.rs:370,429,487,541,667,678,734-738,805` (all TEST_ONLY),
`:827-840` (modeset commit — note `NONBLOCK` is *commented out* at `:838`, so modeset commits are
blocking), `:898-902` (the per-frame flip: `PAGE_FLIP_EVENT | NONBLOCK`), `:943`, `:985`.

#### Struct layouts (from `drm-sys-0.8.1/src/bindings.rs`)

```c
struct drm_mode_atomic {            // :1301  — size 56 (0x38) → _IOWR('d',0xBC,56) = 0xC03864BC
    __u32 flags;                    // +0
    __u32 count_objs;               // +4
    __u64 objs_ptr;                 // +8    -> u32[count_objs]  object ids
    __u64 count_props_ptr;          // +16   -> u32[count_objs]  props-per-object
    __u64 props_ptr;                // +24   -> u32[sum]         property ids, flattened
    __u64 prop_values_ptr;          // +32   -> u64[sum]         values, flattened
    __u64 reserved;                 // +40
    __u64 user_data;                // +48   -> echoed in the FLIP_COMPLETE event
};

struct drm_mode_obj_get_properties {// :1069 — size 28→32 → 0xC02064B9 (already implemented)
    __u64 props_ptr;                // +0
    __u64 prop_values_ptr;          // +8
    __u32 count_props;              // +16
    __u32 obj_id;                   // +20
    __u32 obj_type;                 // +24
};

struct drm_mode_get_property {      // :1051 — size 64 → 0xC04064AA (already implemented)
    __u64 values_ptr;               // +0
    __u64 enum_blob_ptr;            // +8
    __u32 prop_id;                  // +16
    __u32 flags;                    // +20
    char  name[32];                 // +24
    __u32 count_values;             // +56
    __u32 count_enum_blobs;         // +60
};

struct drm_mode_property_enum {     // :1045 — 40 bytes
    __u64 value;                    // +0
    char  name[32];                 // +8
};

struct drm_mode_create_blob {       // :1331 — size 16 → _IOWR('d',0xBD,16) = 0xC01064BD
    __u64 data;                     // +0
    __u32 length;                   // +8
    __u32 blob_id;                  // +12  (out)
};

struct drm_mode_destroy_blob {      // :1338 — size 4  → _IOWR('d',0xBE,4)  = 0xC00464BE
    __u32 blob_id;
};

struct drm_mode_get_blob {          // :1086 — size 16 → _IOWR('d',0xAC,16) = 0xC01064AC
    __u32 blob_id;                  // +0
    __u32 length;                   // +4
    __u64 data;                     // +8
};

struct drm_mode_cursor2 {           // :1143 — size 36 → _IOWR('d',0xBB,36) = 0xC02464BB
    __u32 flags; __u32 crtc_id;     //        (NOT needed — smithay never calls it)
    __s32 x; __s32 y;
    __u32 width; __u32 height;
    __u32 handle;
    __s32 hot_x; __s32 hot_y;
};
```

`_IOWR` encoding used throughout: `0xC0000000 | (size << 16) | (0x64 << 8) | nr`.

#### Object type magic values (uapi `drm_mode.h`) — needed for `obj_type` and OBJECT props

`CRTC = 0xcccccccc`, `CONNECTOR = 0xc0c0c0c0`, `ENCODER = 0xe0e0e0e0`, `MODE = 0xdededede`,
`PROPERTY = 0xb0b0b0b0`, `FB = 0xfbfbfbfb`, `BLOB = 0xbbbbbbbb`, `PLANE = 0xeeeeeeee`,
`ANY = 0`.

#### Properties smithay reads or writes (exhaustive, from `surface/atomic.rs` + `device/atomic.rs` + `mod.rs`)

| object | property | type | required? | evidence |
|---|---|---|---|---|
| connector | `CRTC_ID` | OBJECT(CRTC) | **YES** — hard error | `device/atomic.rs:196`, `surface/atomic.rs:105`, `:1176`, `:1373` |
| crtc | `ACTIVE` | RANGE 0..1 (→Boolean) | **YES** | `device/atomic.rs:219`, `surface/atomic.rs:129`, `:1194`, `:1401` |
| crtc | `MODE_ID` | BLOB | **YES** | `device/atomic.rs:216`, `surface/atomic.rs:1196`, `:1396` |
| crtc | `VRR_ENABLED` | RANGE 0..1 | optional — guarded by `.is_ok()` | `surface/atomic.rs:130,568,630,1198` |
| plane | `type` | ENUM | **YES** — `plane_type()` errors without it | `mod.rs:218+`, already implemented |
| plane | `CRTC_ID` | OBJECT(CRTC) | **YES** | `device/atomic.rs:203`, `surface/atomic.rs:1226,1307,1442` |
| plane | `FB_ID` | OBJECT(FB) | **YES** | `device/atomic.rs:208`, `surface/atomic.rs:1227,1308,1449` |
| plane | `SRC_X/Y/W/H` | RANGE (16.16 fixed) | **YES** | `surface/atomic.rs:1230-1244`, `:1310-1313`, `:1455+` |
| plane | `CRTC_X/Y` | SIGNED_RANGE | **YES** | `surface/atomic.rs:1246-1247`, `:1315-1316` |
| plane | `CRTC_W/H` | RANGE | **YES** | `surface/atomic.rs:1248-1249`, `:1317-1318` |
| plane | `rotation` | BITMASK | optional — guarded | `surface/atomic.rs:1251-1264`, `:1320-1324` |
| plane | `alpha` | RANGE 0..0xffff | optional — guarded | `surface/atomic.rs:1266-1277`, `:1326-1327` |
| plane | `FB_DAMAGE_CLIPS` | BLOB | optional — guarded; **high value, see Q2** | `surface/atomic.rs:1278-1284`, `:1329-1330` |
| plane | `IN_FENCE_FD` | SIGNED_RANGE | optional — guarded; **do not advertise** (Nvidia-style breakage, and we have no fences) | `surface/atomic.rs:1285-1296`, `:1332-1333` |
| plane | `zpos` | RANGE or SIGNED_RANGE | optional — `plane_zpos` returns `Ok(None)` if absent | `mod.rs:247-270` |
| plane | `SIZE_HINTS` | BLOB | optional — falls back to `cursor_size` | `mod.rs:447-470`, used at `compositor/mod.rs:3119-3131` |
| plane | `IN_FORMATS` | BLOB | **only queried if `DRM_CAP_ADDFB2_MODIFIERS == 1`** — we return 0, so never | `mod.rs:300-320` |

Everything not marked **YES** can be omitted; smithay guards each with a
`prop_handle(...).is_ok()` check and degrades cleanly.

#### ⚠ HARD HAZARD — drm-rs indexes `values[0]`/`values[1]` with no bounds check

`drm-0.14.1/src/control/mod.rs:469-510`:

```rust
if flags.contains(ModePropFlags::RANGE) {
    let min = values[0];
    let max = values[1];
    ...
} else if flags.contains(ModePropFlags::SIGNED_RANGE) {
    let min = values[0];  let max = values[1];
} else if flags.contains(ModePropFlags::ENUM) {
    ... // empty vecs are fine here
} else if flags.contains(ModePropFlags::OBJECT) {
    match values[0] as u32 {
        DRM_MODE_OBJECT_CRTC => ValueType::CRTC,
        DRM_MODE_OBJECT_FB   => ValueType::Framebuffer, ...
    }
}
```

If `GETPROPERTY` returns `count_values = 0` for a RANGE / SIGNED_RANGE / OBJECT property,
**cosmic-comp panics with index-out-of-bounds**. The current handler writes
`count_values = 0` unconditionally (`drm_device_interface.rs:1265`), which is only safe because
the sole property is an ENUM. Every new property must return:

* RANGE / SIGNED_RANGE → `count_values = 2`, `values = [min, max]`
* OBJECT → `count_values = 1`, `values = [DRM_MODE_OBJECT_*]`
* ENUM → `count_values = count_enum_blobs = N`, with the `drm_mode_property_enum[]` array
  (safe to keep at 0 for `type`, as today — but a real enum list is cheap and more honest)
* BLOB / BITMASK → 0 is fine

Property flag bits (uapi): `PENDING 1<<0`, `RANGE 1<<1`, `IMMUTABLE 1<<2`, `ENUM 1<<3`,
`BLOB 1<<4`, `BITMASK 1<<5`, `OBJECT 1<<6` (extended), `SIGNED_RANGE 2<<6` (extended),
`ATOMIC 1<<31`. `EXTENDED_TYPE` mask = `0x0000ffc0`.

#### ⚠ HARD HAZARD — the two-pass ioctl protocol

`drm-ffi-0.9.1/src/mode.rs:549-583` calls `GETPROPERTY` **twice**: first with
`values_ptr = 0`/`count_values = 0` to learn the counts, then with allocated buffers. It then does
`map_set!(values, prop.count_values)` — a `Vec::set_len` on a Vec reserved with the *first* call's
count. **The second call must return the identical count**, or the length is set past the
allocation. Same shape for `get_properties` (OBJ_GETPROPERTIES) and `get_planes`.
The existing handlers already follow this pattern (write true count; fill array only when the
pointer is non-null and the caller's capacity is sufficient) — keep it.

#### Cost estimate

~900–1300 lines in `drivers/src/drm_device_interface.rs`, dominated by a static property table and
the `MODE_ATOMIC` parse/validate/apply. There is an unused in-kernel object/property/atomic model
in `drivers/src/drm/{core,properties,modes,device}.rs` (`DrmProperty`, `DrmAtomicState`,
`AtomicModeSet`, `DrmDevice::atomic_commit`) — it is *not* wired to the ioctl layer and its ids are
`DrmObjectId::new()`-allocated rather than the synthetic fixed ids smithay already talks to.
**Recommendation: extend the hand-rolled synthetic-id table in `drm_device_interface.rs`** (ids 1,
30, 40 today) rather than adopting `drm/properties.rs`; determinism matters more than reuse here,
and the existing hand-rolled handlers already encode the two-pass contract correctly.

---

## Q2 — the in-kernel composite fallback

### Honest assessment first

The brief proposed: save-under + alpha-blend the cursor into resource 1 and flush only
`old_rect ∪ new_rect` (~32 KB) instead of 4.1 MB. **This does not fix the ~1 fps**, for two reasons:

1. **The 4.1 MB flush is not the bottleneck.** DRM_STATS measured the whole kernel flip path
   (full-screen `perform_software_scaling` memcpy *plus* full-screen TRANSFER_TO_HOST_2D +
   RESOURCE_FLUSH) at **1.7 ms/flip** against a **1100 ms** frame period. Shrinking it to 32 KB
   recovers at most 0.15 % of the frame time. The other ~1098 ms is cosmic-comp's softpipe
   recomposite in userspace.
2. **It produces two cursors.** cosmic-comp uses `CursorMode::All` on every KMS path
   (`kms/device.rs:944`, `kms/surface/mod.rs:1066`, `kms/mod.rs:979/1090/1135`) and there is no
   config to disable the software cursor. A kernel-drawn cursor would ride on top of the
   compositor's own cursor, which keeps moving at 1 fps and keeps forcing the recomposite.

So a standalone in-kernel composite is a **negative-value change** as an end state. It is only
worth building as **Stage 0: a throwaway host-path proof**, and even then the *virtio-gpu cursor
queue* version is strictly better than blending into resource 1 (zero pixel traffic, host-side
CALayer, and it is the same code Stage 2 needs anyway).

### What IS worth building from Q2: the damage-limited flush

The full-screen flush is here — `drivers/src/drm/device.rs:301-330` (`DrmDevice::atomic_commit`):

```rust
for plane_state in state.plane_states {
    if self.fb_integration {
        match self.perform_software_scaling(&plane_state) { ... }        // device.rs:304
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            let (_, hw_width, hw_height, _) = crate::framebuffer::get_hardware_fb_info()...;
            if gpu.flush(1, 0, 0, hw_width, hw_height) { ... }            // device.rs:319  ← full screen
        }
    }
    ...
}
```

`perform_software_scaling` is `drivers/src/drm/device.rs:348-385`; when source and destination
resolutions match (always, at 1280x800) it is a single
`ptr::copy_nonoverlapping(src_ptr, dst_ptr, src_w * src_h)` — 4.1 MB.

**The infrastructure for a partial flush already exists and is already used elsewhere.**
`VirtioGpuDevice::flush` (`drivers/src/virtio_gpu.rs:591-635`) takes a rect and computes the
correct byte offset:

```rust
let offset = (y as u64 * self.scanout_w as u64 + x as u64) * 4;
```

and `drivers/src/framebuffer.rs:728-741` (`fb_flush`) already calls
`gpu.flush(1, x, y, w, h)` with the console's dirty rect. Only the DRM flip path ignores damage.

**Where the damage comes from:** nowhere, today. `DIRTYFB` is never called (measured: 0), and
smithay legacy has no way to report damage. Under atomic, `FB_DAMAGE_CLIPS`
(`surface/atomic.rs:1278-1284`) is a per-plane blob of `struct drm_mode_rect { __s32 x1, y1, x2, y2; }`
that smithay populates whenever we advertise the property. That is the clean source.

### Concrete spec (Stage 3, after atomic lands)

1. **Advertise `FB_DAMAGE_CLIPS`** as a BLOB property on the primary plane (and only the primary).
   `count_values = 0`, flags `BLOB | ATOMIC`.
2. **Implement `MODE_CREATEPROPBLOB` / `MODE_DESTROYPROPBLOB`** with a
   `static BLOBS: Mutex<BTreeMap<u32, Vec<u8>>>` and a monotonically increasing blob id (start at
   0x1000 to stay clear of the synthetic object ids). `MODE_ATOMIC` resolves the property value as
   a blob id and looks the rects up here.
3. **Thread the rect list into `DrmPlaneState`** (`drivers/src/drm/core.rs`): add
   `damage: Option<Vec<(i32,i32,i32,i32)>>`.
4. **In `atomic_commit` (`drm/device.rs:301`)**: replace the unconditional full-screen copy+flush
   with, per damage rect, a row-loop `copy_nonoverlapping` of `w` pixels for `h` rows, followed by
   one `gpu.flush(1, x, y, w, h)` per rect (or one flush over the union, to keep the virtio command
   count down — see the cost note below). Fall back to full-screen when `damage` is `None`, which
   is what a modeset commit will pass.
5. **Union vs. per-rect:** each `flush()` costs **two** virtio commands, and `send_command_raw`
   (`virtio_gpu.rs:465-521`) does two `mm::buddy::alloc(0)` + two frees *and* a bounded spin-wait
   per command. Cap at, say, 4 rects; beyond that, flush the bounding union. This keeps the
   worst case at 8 virtio commands per frame.

### If Stage 0 (throwaway kernel cursor) is built anyway — save-under design

Only relevant if you want a pre-atomic smoke test *without* the cursor queue. Not recommended
(the cursor-queue version is less code and is reusable), but for completeness:

* **State**: `static CURSOR: Mutex<CursorState>` in `drivers/src/framebuffer.rs` holding
  `{ x: i32, y: i32, visible: bool, img: [u32; 64*64], saved: [u32; 64*64], saved_valid: bool }`.
  ~32 KB of static, acceptable.
* **Hook point**: immediately after `perform_software_scaling` and before `gpu.flush`, in
  `drm/device.rs:304-319`. Restore save-under at the old rect, capture save-under at the new rect,
  alpha-blend `img` over resource 1 at the new rect.
* **Blend**: source-over on XRGB8888, `dst = src.a*src + (1-src.a)*dst` per channel, 8-bit fixed
  point.
* **Flush rect**: bounding box of `old_rect ∪ new_rect`, clipped to `(0,0,hw_w,hw_h)`.
* **Ordering hazard**: `perform_software_scaling` overwrites the whole surface every flip, which
  destroys the save-under. So save-under must be captured *after* the scale, not before — i.e. the
  save-under is only meaningful for cursor-only updates that skip the scale. Since today every
  flip re-scales the full screen, the save-under buys nothing. **This is the concrete reason the
  in-kernel composite is architecturally wrong here.**

---

## Q3 — host-side reality check: QEMU cocoa

### Answer: YES, cocoa implements it, and it is a genuine overlay — no blocker on macOS.

QEMU **11.0.2** (`/opt/homebrew/bin/qemu-system-aarch64`, Homebrew Cellar `11.0.2`). Verified
against upstream `raw.githubusercontent.com/qemu/qemu/v11.0.2/` (Homebrew ships a binary bottle
only). `-display help` on this host lists `none, curses, cocoa, dbus` — cocoa is the only relevant
backend here.

**`ui/cocoa.m:88-95`** — the file's only `DisplayChangeListenerOps`:

```c
static const DisplayChangeListenerOps dcl_ops = {
    .dpy_name          = "cocoa",
    .dpy_gfx_update = cocoa_update,
    .dpy_gfx_switch = cocoa_switch,
    .dpy_refresh = cocoa_refresh,
    .dpy_mouse_set = cocoa_mouse_set,          // :93
    .dpy_cursor_define = cocoa_cursor_define,  // :94
};
```

It is a real hardware-cursor implementation, not a framebuffer blit:

* `ui/cocoa.m:319,381-385` — a dedicated `CALayer *cursorLayer` is allocated and
  `addSublayer:`'d onto the view's layer (anchor point (0,1)).
* `ui/cocoa.m:2085-2090` `cocoa_cursor_define` → `ui/cocoa.m:468-513` builds a `CGImage` from
  `cursor->data` (32bpp, `kCGBitmapByteOrder32Little | kCGImageAlphaFirst`) and sets it as the
  layer contents.
* `ui/cocoa.m:2078-2083` `cocoa_mouse_set` → `ui/cocoa.m:450-466` sets `[cursorLayer setPosition:]`
  and `setHidden:!mouseOn` inside a `CATransaction` with implicit animation disabled.

So cursor motion costs **zero** guest-framebuffer recomposition on the host too. The win is not
merely relocated.

**gtk and sdl2 also implement both** (relevant to a future Linux box):
`ui/gtk.c` — `dcl_ops` (:555, mouse_set :561, cursor_define :562), `dcl_gl_area_ops` (:611,
:617-618), `dcl_egl_ops` (:643, :649-650) → `gd_mouse_set` (:447) / `gd_cursor_define` (:469).
`ui/sdl2.c` — `dcl_2d_ops` (:799, :805-806) and `dcl_gl_ops` (:810, :816-817) →
`sdl_mouse_warp` / `sdl_mouse_define`.

### No capability gating anywhere

There is no `dpy_cursor_define_supported()` in QEMU. `hw/display/virtio-gpu.c:78-114`
`update_cursor()` calls `dpy_cursor_define(s->con, s->current_cursor)` (:106) and
`dpy_mouse_set(s->con, cursor->pos.x, cursor->pos.y, cursor->resource_id)` (:113)
unconditionally. `hw/display/virtio-gpu-base.c` has no cursor-capability logic; it just registers
the cursor vq (:225/:228). If a UI lacked the callback, `ui/console.c:961-978` would store the
cursor and silently skip the listener — no warning, no fallback. Not our situation.

### Two host constraints to design to

1. **The cursor is hard-capped at exactly 64x64.** `hw/display/virtio-gpu.c:97-98` —
   `s->current_cursor = cursor_alloc(64, 64)`; `virtio_gpu_update_cursor_data()` (:45-76)
   **silently returns without copying** if the resource's pixman image is not exactly 64x64
   (:66-69), or if a blob is smaller than `64*64*4` (:60-63). So the guest must advertise
   `DRM_CAP_CURSOR_WIDTH/HEIGHT = 64` and always upload exactly 64x64 BGRA — otherwise a stale or
   blank cursor with no error returned.
2. **Visibility is keyed off `resource_id`.** `virtio-gpu.c:113` passes `cursor->resource_id` as
   the `on` boolean. `UPDATE_CURSOR` with `resource_id == 0` hides the cursor; `MOVE_CURSOR`
   carries the resource id from the last update, so a move with `resource_id == 0` also hides it.
   Be deliberate about this in the driver.

### ⚠ VERIFICATION-METHODOLOGY CONSEQUENCE

`ui/console.c:52-54,961-978` stores the cursor **on the console object** (`con->cursor`,
`con->cursor_x/y/on`), entirely separate from the `DisplaySurface`. `screendump` dumps the
surface. **A hardware cursor will therefore NOT appear in QMP `screendump` screenshots.** Every
pixel-verification harness used since M7w (`m7w_run.py` etc.) checks the guest surface — after this
lane lands, "no cursor in the screenshot" becomes the *expected* result, not a regression. Plan a
different check: `qom-get`/`query-*` will not expose it either, so use serial-side counters of
`UPDATE_CURSOR`/`MOVE_CURSOR` commands plus a live-window observation, and keep one pre-change
screenshot as the "software cursor" baseline.

---

## Q4 — secondary wins

### Q4a — `drm_tick` 50 Hz throttle + one-flip-per-tick

**Verified against current code.** `drivers/src/drm_device_interface.rs:414-449`:

```rust
pub fn drm_tick() {
    let now = sched::ticks();
    if DRM_STATS { ... }                                            // :416-434
    let last = LAST_FLIP_DELIVER_TICK.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < 2 { return; }                       // :436  ← 50 Hz gate

    let mut pend = match PENDING_FLIPS.try_lock() { Some(g) => g, None => return };
    if pend.is_empty() { return; }
    let mut ready = match READY_EVENTS.try_lock() { Some(g) => g, None => return };
    if let Some(blob) = pend.pop_front() {                          // :441  ← at most ONE
        ready.push_back(blob);
        drop(ready); drop(pend);
        LAST_FLIP_DELIVER_TICK.store(now, Ordering::Relaxed);
        DELIVERED_SEQ.fetch_add(1, Ordering::Relaxed);
        sched::try_wake_poll();
    }
}
```

Registered at `servers/drm/src/lib.rs:146` via `sched::register_tick_hook`; hooks run from
`sched::timer_tick_irq()` (`sched/src/lib.rs:1181-1196`), BSP only, 100 Hz
(`arch/aarch64/src/timer.rs:12,85`; `arch/x86_64/src/timer.rs:20,186`).

**Claimed fix (`< 1` + drain the queue) is correct but currently worthless, and carries real risk.**

* Worthless *now*: at 0.9 flips/s the queue depth is ≤1 and the gate is never the limiter — this is
  exactly what "flips delivered == submitted" already told us.
* Risky: the throttle exists for a documented reason. `drm_device_interface.rs:349-355`:
  "PAGE_FLIP-with-event completions are NOT delivered instantly: doing so lets a compositor's
  render loop resubmit with zero delay and peg the CPU (there is no real vblank here) … keeps idle
  CPU at zero (**idletest guards it**)." Draining the queue with `< 1` turns the 100 Hz tick into
  the only pacer; combined with a `NONBLOCK` atomic commit that returns immediately, a
  cursor-only frame loop could free-run at whatever rate the compositor can submit.
* **Recommendation: change `< 2` to `< 1` (100 Hz cadence, matching the tick) and keep the
  one-event-per-tick promotion.** That halves worst-case flip latency from 20 ms to 10 ms, keeps a
  hard 100 Hz ceiling on compositor wakeups, and cannot regress idletest. Draining the whole queue
  should only be revisited if a measurement shows `PENDING_FLIPS` depth > 1, which it currently
  never is. Do this *after* atomic lands, and re-run idletest both arches.

### Q4b — virtio-input is polled from the 100 Hz tick

**Confirmed.** `drivers/src/virtio_keyboard.rs:1` — "(PCI transport, polling mode)".
`poll_events()` (`:383-387`) is called from `arch/aarch64/src/timer.rs:72` and
`arch/x86_64/src/timer.rs:173`, BSP only, 100 Hz. Ring size: `virtio_keyboard.rs:246`
`let size = max_size.min(32);` — 32 descriptors, all pre-published at `:270-282`, recycled inside
`poll()` (`:304-354`). Note `:283` `(*avail).flags = 0; // Request interrupts (though we will poll)`.

**No virtio device in the tree uses interrupts.** virtio-gpu parses the ISR cap into `_isr_cfg`
(`virtio_gpu.rs:288,335`) and never uses it; virtio-net and virtio-blk headers both say "polling
mode". There is no `register_irq`/`request_irq` API in the tree at all. The only wired interrupts
are: aarch64 GIC PPI 27 (vtimer), SGI 1 (resched), SPI 33 (PL011) — `arch/aarch64/src/gic.rs:97-126`,
dispatched by an if-chain at `arch/aarch64/src/exception.rs:55-83`; x86_64 IDT vectors 32 (APIC
timer), 33 (PS/2 via the single `ioapic::set_irq(1, 0, 33)` at `arch/x86_64/src/lib.rs:82`), 0x40
(resched IPI), 0xFD (TLB shootdown). **No PCI GSI is ever routed on either arch.**

**Assessment: not worth it, and out of scope for this lane.** Making virtio-input interrupt-driven
means building PCI interrupt routing from scratch (MSI-X capability parsing, vector allocation,
GIC SPI or IOAPIC GSI programming, an ISR dispatch table) — a foundational subsystem, not a tuning
change, and it would touch `arch/*/src/{gic,exception,idt,lib}.rs`, `drivers/src/pci.rs`, and every
virtio driver. Against that: the ring holds 32 descriptors and QEMU's virtio-tablet emits 3 events
per motion (ABS_X, ABS_Y, SYN), so ~10 motions fit per 10 ms tick = a 1000 motion/s ceiling. At the
60 moves/s the measurement used, the ring is 6 % full. **The 100 Hz poll adds a bounded ≤10 ms of
pointer latency and is not a throughput limiter.** Revisit only if, after the cursor plane lands,
the pointer feels laggy in a way that measures as sampling and not as commit latency.

### Q4c — evdev timestamps quantized to 10 ms

**Confirmed.** `servers/evdev/src/lib.rs:395-410`:

```rust
let now_ticks = sched::ticks();                       // :398
let ev = input_event {
    time: timeval {
        tv_sec: (now_ticks / 100) as i64,             // :401
        tv_usec: ((now_ticks % 100) * 10000) as i64,  // :402
    },
    type_, code, value,
};
```

Every event drained in one tick carries an identical `timeval`; `tv_usec` is always a multiple of
10 000.

**Does any client care? YES — libinput does, and it is staged.**
`scripts/mkfs-f2fs-populated.py:421` stages `libinput.so.10`, and `:974` packs the libinput quirks
tree, so cosmic-comp is running smithay's `LibinputInputBackend`. libinput's pointer-acceleration
filters compute velocity from `Δdistance / Δtime` across a tracker window. With `Δtime == 0`
for events inside the same tick, the velocity estimate degenerates: libinput either discards the
sample or produces a zero/garbage velocity, which makes the acceleration factor jump around.

**Today this is invisible** — at 0.9 fps nobody can perceive acceleration behaviour. **After the
cursor plane lands it will become the next visible artifact** (a pointer that accelerates
erratically or feels "sticky then jumpy"). Two cheap mitigations, in preference order:

1. **Sub-tick timestamps.** `crate::snd::monotonic_us()` already exists and is already used for
   microsecond timing in this exact subsystem (`drm_device_interface.rs:1112,1115`). Use it in
   `evdev::push_event` instead of `sched::ticks()`: `tv_sec = us / 1_000_000`,
   `tv_usec = us % 1_000_000`. ~4 lines, no new infrastructure. **Do this.**
2. If (1) is not viable, synthesise a monotonic +1 µs offset per event within a tick — ugly, and
   it lies about ordering intervals. Prefer (1).

Note the same 100 Hz-derived scheme is used for DRM flip-event timestamps
(`drm_device_interface.rs:394,398-400`); smithay reads those for presentation feedback. Converting
both to `monotonic_us()` is the same change and is worth doing together, but flip timestamps are
lower risk to leave alone.

---

## ORDERED IMPLEMENTATION PLAN

Each stage is independently landable and independently verifiable. Stages 0 and 4c can be done in
parallel with 1 by a second agent; stages 1→2→3 are strictly sequential.

### Stage 0 — virtio-gpu cursor queue, proven end to end (≈250 lines, low risk)

Purpose: de-risk the host path and the cursorq before investing in atomic. Gate behind a
`pub const CURSOR_DEBUG: bool = false;` flag, same pattern as `DRM_STATS`.

**File: `drivers/src/virtio_gpu.rs`**

1. Extend `enum VirtioGpuCmd` (`:161-175`) with `UpdateCursor = 0x0300, MoveCursor = 0x0301`.
2. Add the two uapi structs (they are identical in size — 56 bytes — and share a header):

   ```rust
   #[repr(C, packed)]
   struct VirtioGpuCursorPos {          // 16 bytes
       scanout_id: u32, x: u32, y: u32, padding: u32,
   }
   #[repr(C, packed)]
   struct VirtioGpuUpdateCursor {       // 24 + 16 + 16 = 56 bytes
       hdr: VirtioGpuCtrlHdr,           // type = UpdateCursor | MoveCursor
       pos: VirtioGpuCursorPos,
       resource_id: u32,
       hot_x: u32, hot_y: u32,
       padding: u32,
   }
   ```

   The cursor queue takes **no response descriptor** in the QEMU implementation — it consumes the
   command and completes the chain. Send it as a single read-only descriptor.
3. **Parameterise the queue index.** `send_command_raw` hardcodes `self.queues[0]` (`:466`);
   `send_command` likewise (`:703`). Add `fn send_cursor_command(&mut self, data: &[u8])` that uses
   `self.queues[1]` and does **not** wait for a control response (just submit + notify, and reap
   used entries lazily on the next call to avoid descriptor exhaustion).
4. **Set up queue 1**: at `:394`, add `self.queues[1] = self.setup_queue(1);`. Guard for `None`
   (older QEMU always has 2 queues for virtio-gpu, but be defensive).
5. **Create the cursor resource once**: `create_resource_2d(2, 64, 64)` +
   `attach_backing(2, phys, 64*64*4)` against a buddy-allocated page pair (16 KB → order 2).
   Resource id 2 (1 is the scanout). Note `attach_backing` (`:553-573`) emits exactly one mem
   entry, so the backing must be physically contiguous — order-2 buddy alloc gives that.
6. `pub fn cursor_update(&mut self, hot_x: u32, hot_y: u32, x: u32, y: u32)` — writes 64x64 BGRA
   into the resource-2 backing, issues `TRANSFER_TO_HOST_2D` for resource 2 **on the control
   queue** (transfer is a control-queue command; only UPDATE/MOVE_CURSOR go on the cursorq), then
   `UPDATE_CURSOR` with `resource_id = 2`.
7. `pub fn cursor_move(&mut self, x: u32, y: u32)` — `MOVE_CURSOR`, `resource_id = 2` (must be
   nonzero or QEMU hides it — see Q3).
8. `pub fn cursor_hide(&mut self)` — `UPDATE_CURSOR` with `resource_id = 0`.

**Verification**: with `CURSOR_DEBUG = true`, drive `cursor_move` from the pointer position the
kernel already sees in `virtio_keyboard::poll` / `evdev::push_event` and observe a smooth cursor in
the live cocoa window. Confirm it does **not** appear in a `screendump` (this is the expected
result and validates the Q3 methodology note). Then set the flag back to `false` — do not ship a
second cursor.

### Stage 1 — ATOMIC KMS (≈900–1300 lines, the bulk of the lane)

**File: `drivers/src/drm_device_interface.rs`** (extend the existing synthetic-id table; do not
adopt `drivers/src/drm/properties.rs`).

1. **New ioctl constants**:

   ```rust
   const DRM_IOCTL_MODE_ATOMIC: u32          = 0xC03864BC;
   const DRM_IOCTL_MODE_CREATEPROPBLOB: u32  = 0xC01064BD;
   const DRM_IOCTL_MODE_DESTROYPROPBLOB: u32 = 0xC00464BE;
   const DRM_IOCTL_MODE_GETPROPBLOB: u32     = 0xC01064AC;
   ```

2. **New synthetic object ids** (keep the existing style: crtc/connector/encoder = 1,
   primary plane = 30, `type` prop = 40):

   ```
   plane:  primary = 30, cursor = 31
   props:  40 type            (exists)
           41 CRTC_ID   (plane, OBJECT->CRTC)
           42 FB_ID     (plane, OBJECT->FB)
           43 SRC_X   44 SRC_Y   45 SRC_W   46 SRC_H     (RANGE 0..u32::MAX as 16.16)
           47 CRTC_X  48 CRTC_Y                          (SIGNED_RANGE i32::MIN..i32::MAX)
           49 CRTC_W  50 CRTC_H                          (RANGE 0..u32::MAX)
           51 FB_DAMAGE_CLIPS (plane, BLOB)   <- primary only; used in Stage 3
           60 ACTIVE   (crtc, RANGE 0..1)
           61 MODE_ID  (crtc, BLOB)
           70 CRTC_ID  (connector, OBJECT->CRTC)
   ```

   Do **not** advertise `IN_FENCE_FD`, `IN_FORMATS`, `SIZE_HINTS`, `zpos`, `rotation`, `alpha`,
   or `VRR_ENABLED` — smithay guards every one of them and degrades cleanly.

3. **`std_handle_set_client_cap`** (`:1167`): change `DRM_CLIENT_CAP_ATOMIC => Err(...)` to
   `Ok(0)`, and record it in a `static ATOMIC_CLIENT: AtomicBool` (useful for keeping the legacy
   path alive; see the rollback note).

4. **`std_handle_get_cap`** (`:1137`): add
   `DRM_CAP_CURSOR_WIDTH (0x8) => 64, DRM_CAP_CURSOR_HEIGHT (0x9) => 64`.

5. **`std_handle_get_plane_resources`** (`:1218`): report 2 planes, `[30, 31]`, `count_planes = 2`.

6. **`std_handle_get_plane`** (`:1233`): dispatch on the `plane_id` at offset +0. Both planes get
   `possible_crtcs = 1`. Primary keeps `[XR24, AR24]`; the cursor plane should advertise
   **`AR24` only** (`0x34325241`) — QEMU needs alpha, and offering XR24 invites smithay to pick a
   format the host will render opaque.

7. **`std_handle_obj_get_properties`** (`:1188`): replace the single-plane special case with a
   table lookup keyed on `(obj_id, obj_type)`. Return the id/value pairs listed above. Preserve
   the two-pass contract: always write the true count to +16; fill the arrays only when both
   pointers are non-null and the incoming capacity is sufficient.

8. **`std_handle_get_property`** (`:1256`): replace the single-property special case with a table.
   **Per the Q1 hazard, every RANGE/SIGNED_RANGE property must return `count_values = 2` with
   `values = [min, max]`, and every OBJECT property `count_values = 1` with
   `values = [DRM_MODE_OBJECT_CRTC | DRM_MODE_OBJECT_FB]`.** Keep the two-pass discipline:
   the count returned on pass 2 must equal the count returned on pass 1.

9. **Blob store**: `static BLOBS: Mutex<BTreeMap<u32, Vec<u8>>>`, ids from a counter starting at
   0x1000. Implement `CREATEPROPBLOB` (copy `length` bytes from `data`, assign id, write back
   `blob_id`), `DESTROYPROPBLOB` (remove), `GETPROPBLOB` (two-pass: write `length`; copy when
   `data != 0`).

10. **`std_handle_atomic`** — the new core. Parse `drm_mode_atomic`, walk
    `objs_ptr[i]` / `count_props_ptr[i]` / flattened `props_ptr` + `prop_values_ptr`, and build a
    `DrmAtomicState`-shaped request. Rules:
    * Reject unknown object ids or unknown property ids with `EINVAL`.
    * `flags & 0x100` (TEST_ONLY) → validate only, return `Ok(0)`, **do not present**. smithay
      issues these constantly (`surface/atomic.rs:370,429,487,541,667,678,734,805`); an incorrect
      failure here silently disables the cursor plane
      (`compositor/mod.rs:3448` `info!("failed to test cursor {:?} state")`).
    * `flags & 0x400` (ALLOW_MODESET) → permit `MODE_ID`/`ACTIVE`/connector `CRTC_ID` changes.
      Without it, reject a request that changes them (that is the semantics smithay relies on to
      detect when a modeset is needed).
    * Otherwise: apply. Primary-plane `FB_ID` change → the existing scale+flush path
      (`drm/device.rs:301-330`). Cursor-plane props → Stage 2.
    * `flags & 0x1` (PAGE_FLIP_EVENT) → `queue_flip_event(crtc_id, atomic.user_data)`
      (`drm_device_interface.rs:392`). The existing event blob and `drm_tick` delivery are
      unchanged — this is the one piece that already works.
    * `flags & 0x200` (NONBLOCK) → we are synchronous anyway; ignore.

11. **Session change**: remove `SMITHAY_USE_LEGACY=1` from the launcher.

**Rollback safety (important)**: keep the legacy handlers intact and keep the launcher's
`SMITHAY_USE_LEGACY=1` behind an easily flipped switch, so a bad atomic commit path can be reverted
to the known-good M7y desktop in one line while debugging. Land Stage 1 and prove the *existing*
full-screen desktop still renders on the atomic path **before** adding the cursor plane in Stage 2.

### Stage 2 — cursor plane → virtio-gpu cursorq (≈200 lines)

In `std_handle_atomic`, when plane 31's properties change:

* `FB_ID` set to a new framebuffer → read the 64x64 pixels from that fb's backing
  (`DrmFramebuffer::physical_addresses[0]` via `mm::phys_to_virt`; ADDFB2 already records it at
  `drm_device_interface.rs:1313`), copy into the resource-2 backing, `TRANSFER_TO_HOST_2D` on the
  control queue, then `UPDATE_CURSOR` on the cursorq.
* Only `CRTC_X`/`CRTC_Y` changed (the common case — smithay's "repositioning cursor plane",
  `compositor/mod.rs:3202-3210`) → `MOVE_CURSOR` only. No pixel traffic at all.
* `FB_ID` set to 0 / `CRTC_ID` set to 0 → `UPDATE_CURSOR` with `resource_id = 0` to hide.
* Hotspot: smithay bakes the hotspot into the element position rather than sending a hotspot
  property, so pass `hot_x = hot_y = 0` and let `CRTC_X/Y` carry it.

**Watch for**: the cursor buffer arrives as a gbm BO. With `DRM_CAP_ADDFB2_MODIFIERS = 0` and
swrast Mesa this is a dumb buffer, so `DUMB_BUFFERS` (`drm_device_interface.rs:338`) already knows
its physical address. If Mesa ever routes it through the DRIimage path instead, the ADDFB2 handler
falls back to `phys_addr = 0` (`:1310`) and the cursor would upload garbage — add an explicit
check and a serial warning rather than silently uploading from address 0.

### Stage 3 — `FB_DAMAGE_CLIPS` → damage-limited scale + flush (≈150 lines)

As specced in Q2. Advertise property 51 on plane 30 only; resolve the blob id to
`[drm_mode_rect]`; thread into `DrmPlaneState`; per-rect row-copy in `perform_software_scaling`
and per-rect (or unioned, capped at 4) `gpu.flush(1, x, y, w, h)`. Low absolute value (~1.7 ms/flip
today) but it is the correct shape and it compounds once frames get cheap.

### Stage 4 — latency polish (≈50 lines total, do last, re-run idletest)

* **4c first** (it is the one that will actually be visible): `servers/evdev/src/lib.rs:398-402` →
  use `crate::snd::monotonic_us()` (or whatever the equivalent accessor is from that crate) instead
  of `sched::ticks()`, so libinput sees true microsecond deltas.
* **4a**: `drm_device_interface.rs:436` `< 2` → `< 1`. Keep the one-event-per-tick promotion.
  Re-run idletest on both arches — the throttle exists specifically to keep idle CPU at zero and
  idletest guards it.
* **4b**: do not do. Interrupt-driven virtio-input means building PCI interrupt routing from
  scratch across both arches; the 100 Hz poll costs a bounded ≤10 ms and the 32-descriptor ring is
  6 % full at the measured event rate.

### Verification notes that apply to every stage

* Both arches, release builds only, per `CLAUDE.md`.
* `screendump`-based pixel verification **will stop showing the cursor** once Stage 2 lands
  (Q3). Add serial counters for `UPDATE_CURSOR`/`MOVE_CURSOR` behind the `DRM_STATS` flag and use
  those as the machine-checkable signal; keep one pre-Stage-2 screenshot as the software-cursor
  baseline.
* Re-enable `DRM_STATS` (`drm_device_interface.rs:372`) after Stage 2 to confirm the headline
  number moved: page flips should stay low *and* pointer motion should no longer generate them at
  all.
