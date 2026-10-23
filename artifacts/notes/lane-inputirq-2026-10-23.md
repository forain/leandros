# lane/inputirq 2026-10-23 (L11.6 parts, L11.7 part)

FIXED (a): virtio-input is interrupt-driven. `virtio_keyboard::arm_irq` arms MSI-X on x86_64
(vector per device) and the shared INTx SPI via drivers::irq on aarch64, same as virtio-net
(netirq). The IRQ handler drains all input queues (try_lock). The 100 Hz tick poll drops to
1 Hz once any device is armed (safety net). Counters in /proc/vtstat:
`virtio_input_irqs N tick_poll_found M irq_armed 1`.
Evidence (aarch64, evherdtest, 62 QMP tablet frames): virtio_input_irqs 0 -> 62,
tick_poll_found 0 (no event ever needed the safety poll), evherd PASS, regress all green
except pstest (known tty failure, other lane).
x86_64: `cargo check -p drivers --target x86_64-unknown-none --release` clean; MSI-X path
not run in QEMU this lane.

ALREADY DONE (b): evdev::push_event already stamps events with arch_monotonic_ns()/1000 (the
CLOCK_MONOTONIC source) at receive time, now at IRQ time. Gap left open: EVIOCSCLOCKID is
stored but not applied (CLOCK_REALTIME/BOOTTIME requests still get monotonic stamps).

FIXED (c): vt_handoff_stats (stale_presents / vt_restores) exported in /proc/vtstat via a
stats hook (`tty_server::vt::register_stats_hook`) registered by the input driver.
