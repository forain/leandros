# lane/clickmove 2026-10-23 (L11.6 clicks ignored while the pointer moves) -- REFUTED in QEMU, narrowed to the overflow/resync path

## Compositor/libinput side: no loss (aarch64 HVF Mac, virgl, COSMIC logged in)
`run-leandros/clickmovetest.py`: greeter login, `WAYLAND_DEBUG=1 cosmic-term`, 50 QMP clicks per phase,
wl_pointer.button lines counted in the client log (cosmic-term binds 3 wl_pointers, so 3 lines per event):
- still (no motion):                       300 lines = 100 events = 50 down + 50 up
- batched ([abs,abs,btn down],[abs+50,btn up]): +300 = 50/50
- interleaved (4 motions, down, 4 motions, up, 4 motions, 8 ms apart): +300 = 50/50 (1800 motions)
- still again:                              +300 = 50/50
Together with lane/vtclock (evdev 100/100, 0 SYN_DROPPED) the virtio-tablet path loses nothing end to end.
cosmic-comp/libinput are not dropping button events that reach them.

## Remaining kernel suspect (physical mouse only, not yet proven)
USB HID is harvested at the 100 Hz tick, up to 8 reports per tick (drivers/usb/src/xhci/intr.rs, NTDS=8),
each one an ABS/SYN frame on event1; the client queue is 256 events and overflow discards it with SYN_DROPPED
(servers/evdev/src/lib.rs push). libevdev then resyncs button state with EVIOCGKEY, which evdev ANSWERS WITH
ZEROS (lib.rs `0x18 | 0x19 | 0x1b => zero_out`). So after any overflow a press inside the dropped window is
lost for good, and a held button is synthetically released (drag cut). On Linux EVIOCGKEY reports the real
state and libevdev synthesizes the missing transition. This matches "clicks ignored while moving" only if a
busy compositor lets the queue overflow under 1000 Hz motion: check on the laptop with EV_STATS (`drop`).
Fix regardless: track a per-device key/button bitmap in evdev push and return it for EVIOCGKEY.

## Side finding
The main checkout's f2fs-data0-aarch64.img (20:37) carried the AUGUST greetd (1085464 bytes = greetd.old-0808),
so greeter login failed AUTH_ERR there; a fresh build in the worktree has the new one and logs in on try 0.
