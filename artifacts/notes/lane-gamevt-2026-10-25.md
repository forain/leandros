# Lane gamevt (L12 #56): DOOM/MAME black on the VT2 text console — PARTIAL (QEMU fixed + guarded; laptop unverified)

## Root causes
1. FIXED (kernel, 8531ec2c): `handle_create_framebuffer` refused dumb buffers above 4 GiB (stale since the
   FAKE32 token, 6f15aeb8). MAME's 1280x800 FB lands high on QEMU aarch64 -> "could not create DRM framebuffer" -> console stays.
2. REFUTED: "MAME holds no master / screen does not come back after a VT round trip". vtstat showed implicit=1 then
   cleared=2: the guard's Esc key QUIT MAME. With no key, MAME holds VT2 (holder=2 vt=2) and comes back (0.959).
3. FIXED (DOOM): when card0 answers GET_MODE but CREATE_FB fails, DOOM now exits with
   "[SDL] DRM CREATE_FB failed (errno N); not falling back to /dev/fb0" instead of a silent fb0 fallback (KMS drivers
   such as i915 scan out their CRTC fb, not fb0). fb0 stays only when there is no KMS display. Decision: fail loudly.
4. MAME OSD (../mame 831757542d7, local, not pushed; files leandros_window.cpp + leandros_render.cpp KMS hunks only):
   SET_MODE after GET_MODE (exit with message on EACCES), FLIP_PAGE result checked; a refused flip does not swap buffers.
   Note: SET_MODE returns errno 1 (EPERM) on QEMU virtio-gpu; harmless (warning), not investigated.

## Observables / guard
- /proc/vtstat: new `flip presents=N last_fb=F last_open=O` (legacy FLIP_PAGE successes). `vttest chvt <vt>`.
- doomsession.py `--assert-vt` (+ `DOOMSESSION_VT_KEY`, "" for MAME): game drew on text VT >0.1, back after round trip >0.1,
  desktop differs from game >0.1, desktop-back vs pre-game desktop <0.5.
- aarch64 QEMU, virgl: DOOM PASS {1.0, 1.0, 0.9998, 0.0366}, presents 1492. MAME PASS {0.1334, 0.9594, 1.0, 0.0365},
  presents 207->251, log "FLIP_PAGE refused (errno 13)" on VT1 then "display regained".
  RED with the CREATE_FB fix reverted (= main's kernel): MAME FAIL {0.0014, 0.0014, ...}, "could not create DRM framebuffer".

## Open
- Laptop not run: remote.py needs LEANDROS_REMOTE=172.16.149.179 (my hold ran with it unset; nothing was deployed).
  Recipe: $SP/handoff/gamevt.md. Text-VT getty login not scripted (games run as root from ssh on the active VT2).
