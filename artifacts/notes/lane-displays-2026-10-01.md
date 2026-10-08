# Lane displays (2026-10-02): PARTIAL, unverified

Branch lane/displays @ 59057ac (base f0c8994), pushed.

## Root causes (from source, not yet confirmed at runtime)
- Blank Displays page: cosmic-settings has no output-management client of its own. cosmic-randr-shell::list() execs `cosmic-randr list --kdl`, and every change execs `cosmic-randr ...`. cosmic-randr was not on the image, so the spawn failed and the page model stayed empty. Same on both arches.
- Driver: GETCONNECTOR listed 1 mode ("Native", no PREFERRED flag, refresh 59.99x from integer clock) and a fixed 320x200 mm, so there was no resolution to switch to.
- Start page: Settings reads state key active_page (~/.local/state/cosmic/com.system76.CosmicSettings/v1/active_page) and defaults to "desktop". The 41 CosmicSettings.*.desktop entries (incl. Displays, `Exec=cosmic-settings displays`) are committed for both arches. Hypotheses: a saved active_page, or launcher Enter racing pop-launcher's sort. Not tested.

## Changes
- drivers/src/drm_device_interface.rs: native mode first (PREFERRED|DRIVER), plus the standard sizes that fit inside it, each exactly 60.000 Hz; mm size at 100 DPI (QEMU EDID convention); GETCONNECTOR no longer holds the DRM device lock while it writes user memory.
- scripts/mkfs-f2fs-populated.py: stage /bin/cosmic-randr.
- Artifacts (not in git): cosmic-randr built unmodified at 6e8e795 (the rev cosmic-settings pins) to ~/code/leandros-artifacts/m6-session-bins/out/cosmic-randr-{aarch64,x86_64}, stripped, NEEDED libc only. Copied to the same path on the desktop.

## State when stopped
- Builds were still running: Mac aarch64 (worktree ~/code/leandros-displays) and desktop x86_64 (worktree /run/media/forain/samsung970pro512/leandros-siblings/leandros-displays, built with SKIP_FIREFOX/SKIP_PORTAL). Neither has finished, so nothing is booted and nothing is verified.
- Helper: scratchpad dsp.py (key/type/click/shot through driver.py).
- Next steps: boot --virgl on both arches; cat the active_page state file; screenshot the launcher before Enter; run `cosmic-randr list` from serial with XDG_RUNTIME_DIR=/run/user/1000; open the Displays page; try `cosmic-randr mode <out> 1024 768`, revert, scale 1.25 then revert; drmsmoke 78+; COSMIC smoke.
- Open risk: whether a mode switch presents correctly (present_blob_fb scans out at the fb size, which should follow; the legacy flip path uses crtc.mode).

## Update
- 59057ac: an atomic MODE_ID commit now decodes the mode blob (under BLOBS only; BLOBS and the device mutex are never nested) and sets crtc.mode. Before this, GETCRTC and the legacy flip path still used the boot mode after a switch. GPU blob framebuffers already scan out at the framebuffer's own size through present_blob_fb.
- Both builds started before 59057ac, so they lack this commit; an incremental rebuild is needed after they finish.

## Verification: x86_64 KVM --virgl at 59057ac (desktop, 1920x1080) — PASS
- active_page state file does not exist on x86_64 (no ~/.local/state/cosmic/com.system76.CosmicSettings/v1/ at all). Launcher query "displays" ranks Displays FIRST (Ctrl+1), so Enter opens Displays: aarch64 landing on Displays is consistent with Displays being the top hit of the typed query (not a saved state, not tested on aarch64 yet).
- `WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000 cosmic-randr list` (root): HDMI-A-1 enabled, 487x274 mm, scale 100%, 11 modes at 60.000 Hz, 1920x1080 current+preferred (also 1680x1050, 1600x900, 1440x900, 1366x768, 1280x1024, 1280x800, 1280x720, 1024x768, 800x600, 640x480). Note XDG_RUNTIME_DIR alone gives NoCompositor; WAYLAND_DISPLAY=wayland-1 is needed.
- Settings -> Displays screenshot: Resolution 1920x1080, Refresh 60 Hz, Scale 100%, orientation Standard.
- `cosmic-randr mode HDMI-A-1 1024 768`: screen really became 1024x768 (screenshot is 1024x768, settings window re-laid out, list shows current=1024x768); revert to 1920x1080 OK (one screenshot caught the dock mid-animation, clean afterwards).
- Scale: `cosmic-randr scale` does not exist; used `mode HDMI-A-1 1920 1080 --scale 1.25` -> list 125%, Settings shows 125%, larger UI in screenshot; `--scale 1` reverted to 100%.
- drmsmoke (live session): 64 PASS, failed=0, skipped=19 (the ATOMIC_* cases SKIP while cosmic-comp holds the display; the 78+ figure was not reached in this configuration). Serial: 0 SEGV, 0 panic, 0 [PF], 0 WDOG.
- aarch64: Mac build (pids 36184/38763) still running at 35 min; not verified yet.

## Verification: aarch64 (Mac HVF --virgl, 1280x800, 59057ac, image 18:28 newer than commit 17:28) — PASS
- Login as leandro through greetd; ~/.local/state/cosmic/com.system76.CosmicSettings/v1/ does not exist on aarch64 either (no saved active_page). Launcher "settings" ranks COSMIC Settings first (generic page); "displays" ranks Displays first, Enter opens the Displays page.
- `cosmic-randr list`: HDMI-A-1, 325x203 mm, 100%, 5 modes at 60.000 Hz (1280x800 current+preferred, 1280x720, 1024x768, 800x600, 640x480).
- Settings -> Displays: Resolution 1280x800, Refresh 60 Hz, Scale 100%, Standard (not blank).
- `mode HDMI-A-1 1024 768`: screenshot is really 1024x768, Settings shows 1024x768; revert to 1280x800 gives a 1280x800 screenshot. `mode ... 1280 800 --scale 1.25`: list and Settings 125%, larger UI; `--scale 1` back to 100%.
- drmsmoke no-gpu boot: 78 PASS, failed=0, skipped=5. Live virgl session: failed=0, skipped=19 (ATOMIC cases skip). Serial: 0 SEGV, 0 panic, 0 [PF], 0 WDOG.
- Seen during the live drmsmoke: "[VFS] fd table FULL at 512" and "EXECBUFFER refused: out-fence fd unavailable" for the drmsmoke process itself (its own fd exhaustion test); not a regression.
