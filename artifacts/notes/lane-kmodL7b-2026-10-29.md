# lane kmodL7b (2026-10-29): uvc as a loadable module

DELIVERED. Branch lane/kmodL7b. `CONFIG_USB_VIDEO_CLASS` is tristate (default y); =m builds modules/uvc from the unchanged drivers/usb/src/uvc.rs.

- Seam: uvc.rs takes its USB needs from `crate::uvcenv`. Kernel build (drivers/usb/src/uvcenv.rs) re-exports the xhci types; modules/uvc/src/lib.rs has its own uvcenv over the kapi (UsbFunction = handle + cfg, IsochPipe = pipe handle). IsochEpDesc/scan_isoch moved to the dependency-free drivers/usb/src/isocdesc.rs, shared by #[path].
- kapi (ksymtab 36 -> 44): `kapi_usb_ctrl(handle, setup*, buf, len, timeout_ms)`, `kapi_usb_set_alt`, `kapi_usb_cfg` (configuration descriptor), `kapi_usb_isoch_open(handle, iface, alt, ep, fifo, ntds, framed)`, `_submit` (poll: 0 ok / 1 busy / 2 gone, IRQ-safe try-lock), `_reap` (framed packet), `_close`, `kapi_v4l2_unregister`. Plan names "submit/reap" map to the IsochPipe poll/read_packet model; no callback (the module's cameras are pumped by the kernel v4l2 tick). `kapi_usb_class_register` gained a `detach` argument (no earlier user).
- Handle table replaces `exports::CLAIMED`: FUNCS (handle, owning driver, Arc<UsbFunction>), PIPES; monotonically increasing handles, never reused; no lock held across xhci/module calls. Detach: class::detach -> per-driver trampoline -> module `detach(handle)` -> handle dropped, pipes still open on it closed.
- Alias autoload at enumerate: `usb::class::wants_or_load` (xhci enumerator) calls the hook `kmod::autoload::usb_hook` for each interface nobody wants; it loads the first matching `usb` alias module, whose init registers the class driver, and `wants` then succeeds so the device is configured and offered. Installed before `usb_hcd::init`, so boot-time devices work. Policy (kmod=0, blacklist) via load_by_name.
- `usb.alias_selftest=1` runs that hook on a synthetic video-streaming interface (QEMU has no UVC device; the fake camera bypasses USB enumeration, so alias autoload cannot trigger from it). `uvc.fake=1` is registered from the module's init.
- Gating without changing =y features: `mod-usb-video-class` kernel feature is emitted by to-cargo.py only for =m (generic rule "mod-<sym>"), cfg-ing the built-in uvc out. Features md5 of all four defconfigs identical to main.
- ktest kmodkapi: 44 exports, bad-handle checks for every new call.
- scripts/kmod/autoloadtest.sh [arch] uvc.

## Evidence (uvc=m)
- aarch64 and x86_64: `modules.load=uvc uvc.fake=1`: camtest --probe PASS, camtest 8 PASS (8/8), /proc/modules `uvc 2097152 2 - Live`. `usb.alias_selftest=1`: `[kmod] usb alias 1234:5678 0e0200 -> uvc`, wants Some(14). With modules.blacklist=uvc: Blacklisted, wants None. ktest=kmodkapi PASS both.
- Sizes: uvc.kmod 70,320 B aarch64 / 63,312 B x86_64 (2 MiB slot). Kernel =m vs =y: aarch64 7,008,384 vs 7,048,984 (-40,600); x86_64 6,874,520 vs 6,913,984 (-39,464).
- aarch64 default (=y) regress PASS.
## Not tested
Real UVC hardware (real ctrl/isoch through the kapi, hot-unplug detach); the orchestrator's laptop test covers it.
## Found
aarch64 =m builds on current main fail `bidcheck`: `kernel-direct has no GNU build-id` (kernel itself ok). Not touched (kmodinitrd's); I bypassed it locally (uncommitted) to run the aarch64 gates.
