# Review: lane lapbt (merge a04012d9) — 2026-10-27

Scope: `git diff a04012d9^1 a04012d9` (btusb + btintel, Intel 8265, `/dev/hci0` on x86_64).
Verdict: **merge is OK to keep**; no blocker in the lane itself. One P0 found next to it
(rpi5 kernel does not build on main, not caused by lapbt), plus P1/P2 follow-ups below.

## 1. Pi 5 UART Bluetooth path — not broken by lapbt

- cfg gating in `drivers/src/bt/hcidev.rs` is right: only `pump()` and its imports
  (`Transport`, `H4Parser`, `pump_once`, `format`) are gated to `aarch64 + rpi5`; the port,
  VFS handler, queues and the new `push_rx/pop_tx/set_dead/notify` build everywhere.
  `bt/boot.rs` (UART) stays rpi5-only and still calls `hcidev::create_port()` + `pump()`.
- **Compile check** (scratch worktree at a04012d9, `cargo check -p kernel --features rpi5`,
  aarch64 kernel target, release): the `drivers` crate (all of `bt/`) compiles clean
  (one harmless "unnecessary unsafe" warning at hcidev.rs:77). The **kernel crate fails**:
  `kernel/src/pm.rs:65-66` calls `drivers::blkdev::pm_freeze/pm_thaw`, which the rpi5/raspi4b
  `blkdev` (`blkdev_sdhci.rs`) does not have. Introduced by **df3b582b (lapsusp)**, already on
  main before lapbt. **P0: the Pi 5 kernel cannot be built from main** — fix: add no-op
  `pm_freeze(_)->Ok(String::new())` / `pm_thaw()` to `drivers/src/blkdev_sdhci.rs`, or cfg-gate
  the two hooks in pm.rs. Worktree removed.
- **Double registration risk (P2):** `drivers::bt::usb::start` is called unconditionally in
  `kernel/src/init.rs:229` (all arches, including rpi5). On a Pi with a USB BT dongle (RP1 xHCI
  enumerates E0/01/01 the same way) both transports would `create_port()`, both would call
  `register_device_notify_open("/dev/hci0")`, and two pumps would drain the single static
  `QUEUES` (each stealing the other's TX). Today's Pi has no dongle, so nothing breaks, but
  guard it: a `static CLAIMED: AtomicBool` in hcidev.rs, `pub fn claim() -> bool` (first
  transport wins, the loser logs `[bt] /dev/hci0 already owned by <uart|usb>` and parks), called
  by both `boot.rs` and `usb.rs` before `create_port()`. Alternatively gate `usb::start` with
  `#[cfg(not(feature = "rpi5"))]`, but the claim is the general fix (also covers a laptop
  with a second dongle: `take()` only ever hands out one function, good).

## 2. xHCI: can a failing BT endpoint take down the root controller?

- IN pipes (interrupt events, bulk ACL): good. `BtUsb::poll` recovers with
  `recover_ep_inner` (no `die()`), bounded by MAX_RECOVERIES, then reports `Gone` and the
  pump marks `/dev/hci0` dead. Laptop log shows exactly this path working (cc 4 on bulk IN
  during Intel Reset, recovered).
- **OUT side is not covered (P1):** `send_cmd` -> `control()` and `send_bulk` -> `bulk()` ->
  `wait_td`: on timeout or disconnect `wait_td` calls **`recover_ep`**, and `control()` calls
  `recover_ep` on any other error too; `recover_ep` calls `x.die("endpoint recovery failed")`
  if recovery fails, halting the controller the root stick is on. That is the shared policy
  (DMA safety) and was fine for storage/HID, but BT now drives ~2400 bulk OUT secure-sends and
  an Intel Reset per boot and the device demonstrably misbehaves around the reset. Suggested
  fix (own small lane, `xhcisafe`): a slot-scoped failure mode — if recovery of a non-root
  slot's endpoint fails, issue Disable Slot (the xHC stops all DMA for that slot) and mark the
  slot gone, instead of halting the controller; `die()` only when Disable Slot itself fails.
- `MAX_RECOVERIES = 32` is a lifetime counter: after 32 transient errors over days of uptime BT
  dies permanently. Reset `recoveries` to 0 after a harvest with a successful completion.
- `InPipe::harvest` `break`s on the first failed completion and then `events.clear()`s,
  dropping successful completions behind it in the same batch (data loss, and the
  reassembler is then misaligned, see 3).

## 3. Reassembly bounds (`usb_reasm.rs`)

- Bounded: an event is at most 2+255 bytes; ACL lengths > 4096 drop the buffer (`resyncs`);
  the buffer never grows past one packet + one USB chunk. No panics: all indexing is behind
  length checks. Good.
- Not robust to a lost chunk: after an endpoint reset (or the harvest drop above) the
  reassembler keeps half a packet and parses garbage as the next header; ACL only resyncs by
  luck (len > 4096), events never. Fix: `BtPoll::Data` carries a `reset: (bool, bool)` flag
  set when an IN pipe was recovered; the pump/`Link` then `reset()`s that reassembler.

## 4. Firmware download error paths (`btintel.rs`)

- Missing firmware, bad CSS header, truncated `.sfi`, reject status, no ff/06, no ff/02 boot
  event, still-bootloader after reset: all return a described `Error`, the thread logs
  `fail stage=intel ...` and parks; `/dev/hci0` never appears. Host-tested. Good.
- **No reset-to-bootloader on failure (P2).** Linux `btintel_download_fw` calls
  `btintel_reset_to_bootloader()` (0xfc01 `{01,01,01,00, 0}` + 150 ms) after a failed download
  so the controller is clean for a retry/kexec. Here a half-loaded bootloader stays that way
  until a power cycle. Add that reset on any error after the first secure-send, then retry
  `setup` once.
- `send_result` is checked one fragment late (ff/06 arrives after the Command Complete) and is
  never cleared: a stale `(0,0)` from mid-download would skip the final "wait for ff/06".
  Set `send_result = None` before sending the last fragment.
- A DDC write failure aborts the whole bring-up although the controller is operational:
  downgrade to a warning (log + continue to `/dev/hci0`).

## 5. Runtime cost (P1 for the laptop)

The pump never blocks: `yield` x64, then `msleep(1)` until 2000 idle rounds, then
`msleep(4)` forever = **~250 wake-ups/s with no Bluetooth traffic**, even with `/dev/hci0`
closed — against the laptop idle work (getty < 4/s). Cheap fix: if `OWNER == 0` (expose
`hcidev::is_open()`) sleep 100 ms; when open and idle, 20 ms. Proper fix later: wake the pump
from the xHCI event ring (`wake_poll_key` on BT slot completions).

## 6. Regress

Only pstest was re-run after df1a2bc3. The x86 log (`lapbt-x86reg.txt`) also shows
`/proc/1/stat starttime is stable` FAIL ("None None"), `ps -u root`, `ps aux lists pid 1`
— all the same exited-kthread symptom (pid 1's /proc entry unreadable). Required before
the next push: full regress x86_64 + aarch64 on the box on main (`box/regress.sh`), and
the rpi5 kernel build once the P0 above is fixed. The kthread bug itself: lane `kexit`
in design-lapbt2-2026-10-27.md.

## Smaller notes
- `kthread::spawn` returns before `set_exe_path("[btusb]")`: see `kexit` (race).
- `usb.rs` `start()` doc says enumeration is synchronous at boot; a BT dongle hot-plugged
  later is never claimed (fine for the built-in 8265; note it).
- SCO TX is silently accepted and dropped (`sco_drops`); btd should not see success for
  SCO — return EINVAL from hcidev write for type 0x03 when the transport lacks SCO.
