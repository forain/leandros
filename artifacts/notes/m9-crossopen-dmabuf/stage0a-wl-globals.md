# Stage 0a — does cosmic-comp advertise `zwp_linux_dmabuf_v1` on LeandrOS?

**Answer: NO. It is absent, and so are the other two globals behind the same gate.**
The measurement confirms the design doc's expectation (`crossopen_design.md` §6.1). The
cross-open dmabuf route (item 8) cannot reach a Wayland Vulkan client, at any amount of
kernel effort, in this configuration.

Run: aarch64 / HVF, Mac main tree at `c27557f`, fresh `f2fs-data0-aarch64.img`
(regenerated 19:13). cosmic-comp `dec1ee86` (`epoch-1.3.0`). 2026-08-06.
Harness: `~/code/leandros-artifacts/m9_stage0a_run.py` → `driver.py session`
(raw transcript, no prompt parsing — **not** `driver.py cmd`).
Raw serial: `stage0a-aarch64-r1-serial.txt`. Screenshots: `-t150.png`, `-t300.png`.

---

## 1. The instrument, and why it is not `leandros-applet`

The design doc suggested extending `leandros-applet`. **That would have produced a
convincing false negative.** `leandros-applet` calls `Connection::connect_to_env()`, and
cosmic-panel hands each applet an inherited `WAYLAND_SOCKET` fd — so the applet connects
to **cosmic-panel's embedded wayland server**, not to cosmic-comp. That server advertises
`wl_compositor` + `wl_shm` + `xdg_wm_base` and no dmabuf: exactly the shape of the "real
negative" this task was told to look for.

Instrument built instead: `wl-globals`, a new dependency-free binary
(`~/code/leandros-artifacts/m9-wlglobals/`, same musl/PIE recipe as the applet, staged to
`/bin/wl-globals`). It **ignores the environment entirely**, enumerates every
`wayland-*` socket in `$XDG_RUNTIME_DIR`, connects to each by explicit path via
`Connection::from_socket`, and prints every global unfiltered.

**Which server was dumped is not inferred.** cosmic-session's own log line settles it:

```
INFO cosmic_session: got environmental variables from cosmic-comp: [("WAYLAND_DISPLAY", "wayland-1")]
```

`wayland-1` is cosmic-comp's socket, per cosmic-comp's own handshake.

---

## 2. Result — the full global list

`/run/user/0/wayland-1`, **54 globals**, identical across three passes 30 s apart:

| # | interface | v | | # | interface | v |
|---|---|---|---|---|---|---|
| 1 | `cosmic_a11y_manager_v1` | 2 | | 28 | `xdg_activation_v1` | 1 |
| 2 | `cosmic_corner_radius_manager_v1` | 2 | | 29 | `xdg_wm_base` | 7 |
| 3 | `ext_background_effect_manager_v1` | 1 | | 30 | `zcosmic_keyboard_layout_manager_v1` | 1 |
| 4 | `ext_data_control_manager_v1` | 1 | | 31 | `zcosmic_output_manager_v1` | 3 |
| 5 | `ext_foreign_toplevel_image_capture_source_manager_v1` | 1 | | 32 | `zcosmic_overlap_notify_v1` | 1 |
| 6 | `ext_foreign_toplevel_list_v1` | 1 | | 33 | `zcosmic_toplevel_info_v1` | 3 |
| 7 | `ext_idle_notifier_v1` | 2 | | 34 | `zcosmic_toplevel_manager_v1` | 4 |
| 8 | `ext_image_copy_capture_manager_v1` | 1 | | 35 | `zcosmic_workspace_image_capture_source_manager_v1` | 1 |
| 9 | `ext_output_image_capture_source_manager_v1` | 1 | | 36 | `zcosmic_workspace_manager_v2` | 2 |
| 10 | `ext_session_lock_manager_v1` | 1 | | 37 | `zwlr_data_control_manager_v1` | 2 |
| 11 | `ext_workspace_manager_v1` | 1 | | 38 | `zwlr_layer_shell_v1` | 5 |
| 12 | `org_kde_kwin_server_decoration_manager` | 1 | | 39 | `zwlr_output_manager_v1` | 4 |
| 13 | `wl_compositor` | 5 | | 40 | `zwlr_output_power_manager_v1` | 1 |
| 14 | `wl_data_device_manager` | 3 | | 41 | `zwp_idle_inhibit_manager_v1` | 1 |
| 15 | `wl_fixes` | 1 | | 42 | `zwp_input_method_manager_v2` | 1 |
| 16 | `wl_output` | 4 | | 43 | `zwp_keyboard_shortcuts_inhibit_manager_v1` | 1 |
| 17 | `wl_seat` | 9 | | 44 | `zwp_pointer_constraints_v1` | 1 |
| 18 | `wl_shm` | 2 | | 45 | `zwp_pointer_gestures_v1` | 3 |
| 19 | `wl_subcompositor` | 1 | | 46 | `zwp_primary_selection_device_manager_v1` | 1 |
| 20 | `wp_alpha_modifier_v1` | 1 | | 47 | `zwp_relative_pointer_manager_v1` | 1 |
| 21 | `wp_cursor_shape_manager_v1` | 2 | | 48 | `zwp_tablet_manager_v2` | 1 |
| 22 | `wp_fractional_scale_manager_v1` | 1 | | 49 | `zwp_text_input_manager_v3` | 1 |
| 23 | `wp_pointer_warp_v1` | 1 | | 50 | `zwp_virtual_keyboard_manager_v1` | 1 |
| 24 | `wp_presentation` | 2 | | 51 | `zxdg_decoration_manager_v1` | 1 |
| 25 | `wp_security_context_manager_v1` | 1 | | 52 | `zxdg_exporter_v2` | 1 |
| 26 | `wp_single_pixel_buffer_manager_v1` | 1 | | 53 | `zxdg_importer_v2` | 1 |
| 27 | `wp_viewporter` | 1 | | 54 | `zxdg_output_manager_v1` | 3 |

(Machine-readable source: the 162 `[WLG] G` lines in `stage0a-aarch64-r1-serial.txt`;
this table is generated from them, not hand-transcribed.)

### Absent — all three globals behind the `!is_software` gate

| interface | gate | observed |
|---|---|---|
| `zwp_linux_dmabuf_v1` | `kms/device.rs:760` → `kms/socket.rs:57` | **absent** |
| `wl_drm` | `kms/device.rs:760` → `kms/socket.rs:59` | **absent** |
| `wp_drm_lease_device_v1` | `kms/device.rs:775` (same `!is_software`) | **absent** |

Also absent: `wp_linux_drm_syncobj_manager_v1` — expected, `COSMIC_DISABLE_SYNCOBJ=1`.

---

## 3. Proof the dump was not truncated, and the controls

**This is a real negative, not an empty run.** Five independent checks:

1. **Positive content.** 54 globals including `wl_compositor`, `wl_shm`, `xdg_wm_base`,
   `wl_seat`, `wl_output`, `zwlr_layer_shell_v1` — a full COSMIC registry, including
   interfaces never named in the dumper's source.

2. **Self-checking matcher.** One routine, one line, controls and subjects together:
   ```
   [WLG] MATCH sock=wayland-1 wl_compositor=1 wl_shm=1 wl_seat=1 xdg_wm_base=1 \
         zwlr_layer_shell_v1=1 wl_output=1 zwp_linux_dmabuf_v1=0 wl_drm=0
   ```
   A broken matcher would report the controls as 0 too. It reports them as 1.
   The `MATCH` line is independently checkable against the raw `G` lines — two
   representations of the same data in the same output.

3. **Exact line arithmetic — no shredding.** 186 `[WLG]` lines total. Structure predicts
   `4 (pre-control) + 1 BEGIN + 3 × (PASS+TRY+OPEN+54 G+COUNT+MATCH+PASSEND) + 1 END = 186`.
   Exact match. `G`=162=3×54, `TRY`=`OPEN`=3, `BEGIN`=`END`=2.
   This matters: **146 `[DRM-SRV] mmap` kernel trace lines were interleaved on the same
   console** (the hazard that previously shredded guest output). It did not shred this one.

4. **Pre-session control.** The same binary, run before the session started:
   `[WLG] PASS 1/1 sockets=0 []` → `PASSEND` → `END`. The dumper runs, its output reaches
   the serial intact, and it reports *nothing* when there is nothing — its output tracks
   reality rather than printing a canned list.

5. **Session liveness during the dump.** `-t150.png`: 1280×800, panel bar (27,27,27) over
   Orion Nebula wallpaper. `-t300.png`: the panel bar with `leandros-applet`'s **live clock
   at 00:03:51** still compositing above the console text. Zero `EL0 Fault`, zero `panic`.
   The compositor was up and rendering when it reported 54 globals.

### Control that FAILED (reported for completeness)

`echo LANER_CONSOLE_ALIVE_9A7C`, sent ~180 s after session launch, **never appeared**
(x0). Commands sent to the login shell after the session floods the console do not
execute or do not echo — the console is saturated by session output plus the `[DRM-SRV]`
traces. The 7th command (a second foreground measurement) therefore never ran.

**The primary measurement did not depend on it.** The dumper was *backgrounded* as the
third command, while the console was still responsive, and slept 100 s inside its own
process. Its `BEGIN pid=108` line and three complete passes prove it ran. Had the
measurement been designed as "type a command later", this run would have returned nothing.

---

## 4. Why `is_software` is true — three behavioural proofs

I did **not** directly observe the `EGL_MESA_device_software` string: cosmic-comp's own
stdout is not relayed into the serial log by cosmic-session (only `cosmic-panel`,
`launch_pad`, `cosmic-greeter`, `cosmic-notifications`, `busd` appear). What is available
is stronger than a log line, because it observes the branch itself:

1. **No `wayland-1-card0` socket exists.** `create_socket` (`kms/socket.rs:31`) binds a
   second, gpu-specific listener named `{socket}-{card0}` — and it is *only* through that
   socket that a client gets `advertised_drm_node = Some(render_node)`, which is what the
   dmabuf global's per-client filter requires (`socket.rs:49`, `state.rs:198-202`). The
   dumper globs `wayland-*`, which would have caught `wayland-1-card0`. Only `wayland-1`
   existed, on all three passes. **`create_socket` was never called.**
   This also eliminates the alternative explanation "the global exists but my client was
   filtered out": there was no socket through which any client could have passed the filter.

2. **`Failed to initialize hardware-acceleration` x0.** That warning is the `Err` arm of
   the same `match` (`device.rs:764-771`). Its absence means `create_socket` did not *fail*
   — the `(!is_software).then(...)` closure returned `None`, i.e. `is_software == true`.

3. **The third global agrees.** `wp_drm_lease_device_v1` (`device.rs:775`, independent
   call site, same gate) is also absent. Three globals and one socket, all consistent with
   exactly one boolean.

Supporting: the guest runs `-device virtio-gpu-pci` (no GL); the launcher sets
`GBM_ALWAYS_SOFTWARE=1`; the panel's own client-side stack reports
`GL Renderer: "softpipe"`, `GL Version: "OpenGL ES 3.1 Mesa 25.3.6"`, `EGL Version: (1, 5)`,
`PLATFORM_WAYLAND_KHR`; and the launcher must force `COSMIC_RENDER_DEVICE=226:0` precisely
because `determine_primary_gpu` filters software devices (`kms/mod.rs:272`).

**Scope of the claim.** This says cosmic-comp advertises no dmabuf *in this configuration*
— software EGL, which is forced here because the macOS host has no EGL, so
`virtio-gpu-gl,venus=on` is unusable and the guest has no hardware GL. It is not a claim
that cosmic-comp never advertises dmabuf. The finding would flip only if the guest gained
a non-software EGL device, which is the blocked M3/Venus-on-Linux lane, not item 8.

---

## 5. The alternative route is reachable — verified on the shipped binary

`MESA_VK_WSI_DEBUG=sw` (`crossopen_design.md` §6.2) needs Wayland WSI compiled into the
Venus ICD *and* the env var live in a release build. Both check out against the shipped
`venus-lane/stage-aarch64/usr/lib/libvulkan_virtio.so`:

* built `-Dplatforms=wayland -Dvulkan-drivers=virtio`
  (`venus-lane/build-venus-icd-alpine.sh:33,35`);
* `MESA_VK_WSI_DEBUG` present as a string in the binary, alongside the WSI debug flag
  table (`noshm`, `linear`, `dxgi`, `nowlts`) — the option is parsed at runtime;
* `wsi_wl` ×30, `wl_shm` ×4, `wl_shm_pool` ×2, `zwp_linux_dmabuf` ×8 — both WSI branches
  are compiled in.

No Mesa rebuild is required to try it. (Not yet run end-to-end — that is the next step,
not a finding.)

---

## 6. Verdict

**Take the `MESA_VK_WSI_DEBUG=sw` route. The dmabuf route is dead for M4.**

`zwp_linux_dmabuf_v1` is not advertised, and Mesa's WSI binds `wl_shm` *only* in the `sw`
case and `zwp_linux_dmabuf_v1` *only* in the non-`sw` case — mutually exclusive
(`wsi_common_wayland.c:1406-1421`). A non-`sw` Venus on this compositor returns
`VK_ERROR_SURFACE_LOST_KHR`. Item 8's Stages 3–5 are killed as an M4 unblocker.

Stages 1–2 remain due regardless: the exported-dmabuf use-after-free (§2.4) is a real
one-process unprivileged bug and is untouched by this finding.

## 7. Artifacts and tree state

* Instrument crate: `~/code/leandros-artifacts/m9-wlglobals/` (new).
* Harness: `~/code/leandros-artifacts/m9_stage0a_run.py` (new).
* **Uncommitted main-tree change:** `scripts/mkfs-f2fs-populated.py` gained ~8 lines
  staging `/bin/wl-globals`, conditional on the host binary existing (same pattern as
  `leandros-applet`). Nothing in the session depends on it. Revert or keep as a
  Stage-0 instrument, as preferred. x86_64 was not built or staged.
* Aside, not this lane's: `[DRM-SRV] mmap token=... -> writeback` traces are enabled and
  flood the console continuously during a session (146 lines in a ~7 min run). They are
  the known output-shredding hazard and are worth gating.
