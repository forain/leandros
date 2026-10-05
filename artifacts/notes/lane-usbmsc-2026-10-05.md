# lane/usbmsc 2026-10-05: USB Mass Storage BOT + SCSI layer

Hardware independent, `no_std`, allocation free. Nothing in the kernel calls
it yet; it is compiled into the `usb` crate (`pub mod msc`).

## Files (drivers/usb/src/msc/)

- `transport.rs`: `UsbErr` and the `BulkTransport` trait. Identical to the
  copy the xHCI lane implements; integration should dedupe (keep one, re-export).
- `bot.rs`: `Bot<T>`: CBW/CSW framing, incrementing tag, Get Max LUN (stall or
  any failure gives 0), `reset_recovery()` (0xFF, value 0, index = interface,
  then clear halt IN and OUT), `execute(lun, cdb, Data) -> Completion`.
  Recovery follows BOT 1.0: data stall clears that endpoint then reads the CSW;
  CSW stall clears IN and retries once; CBW/other transport errors, bad
  signature, tag mismatch, bad status and phase error all do reset recovery
  (Disconnected skips it).
- `scsi.rs`: `ScsiDisk<T>` (below).
- `mod.rs`: re-exports `ScsiDisk`, `MscErr`, `BulkTransport`, `UsbErr`.

## API

```rust
ScsiDisk::new(transport: T, delay_ms: fn(u32)) -> ScsiDisk<T>
fn init(&mut self) -> Result<(), MscErr>          // probes LUNs 0..=max_lun, first ready wins
fn sector_size(&self) -> u32                       // 512 or 4096
fn capacity_sectors(&self) -> u64
fn read_only(&self) -> bool                        // MODE SENSE(6) WP bit; failure => false
fn lun(&self) -> u8
fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), MscErr>
fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> Result<(), MscErr>
fn flush(&mut self) -> Result<(), MscErr>          // SYNCHRONIZE CACHE(10), ILLEGAL REQUEST ignored
fn transport_mut(&mut self) -> &mut T
enum MscErr { Transport(UsbErr), Protocol, CheckCondition{key,asc,ascq}, ShortTransfer,
              NoDevice, UnsupportedSectorSize(u32), NotReady, InvalidArg, WriteProtected }
```

Buffers must be a multiple of the sector size and within capacity
(`InvalidArg` otherwise). Commands are split into chunks of at most 64 KiB.
READ/WRITE(10) are used unless `lba + count` exceeds 0xFFFFFFFF, then (16).
READ CAPACITY(16) is issued when (10) returns 0xFFFFFFFF.

Error ladder per command: 3 attempts (REQUEST SENSE after CHECK CONDITION),
reset recovery, 3 more attempts, then the last error. ILLEGAL REQUEST, DATA
PROTECT and Disconnected are not retried. A data stage shorter than requested
(including nonzero CSW residue) counts as a failure and is retried.
`init` polls TEST UNIT READY every 250 ms through `delay_ms`, retrying on UNIT
ATTENTION and NOT READY/04, and gives up after 10 s (`NotReady`).

Integration notes:
- The delay callback must really sleep; `init` can block up to ~10 s per LUN.
- Per-transfer timeout is `Bot::timeout_ms` (5000; 30000 during flush). The
  `Bot` is private to `ScsiDisk`; add an accessor if the xHCI lane needs to tune it.
- Not handled: UAS, multi-LUN simultaneous use (disk binds one LUN), hotplug
  media change after init (UNIT ATTENTION surfaces as a retried error).
- Stack use is small (CBW 31 B, sense 18 B, capacity 32 B).

## Tests

Host-only crate `drivers/usb/msc-tests` (own `[workspace]`, includes the module
by `#[path]`). The repo `.cargo/config` forces the kernel target, so pass the
host triple:

    cargo test --manifest-path drivers/usb/msc-tests/Cargo.toml --target aarch64-apple-darwin
    (x86_64 Linux host: --target x86_64-unknown-linux-gnu)

24 tests, all passing. Mock BOT device (in-memory sparse disk, 512 and 4096
sectors, 2^32+ sector virtual disk) with fault injection: data-phase stall (IN
and OUT), CSW stall, bad CSW signature, tag mismatch, phase error, UNIT
ATTENTION + NOT READY on first TURs, never-ready (10 s give-up), short read
with residue (both at Bot and ScsiDisk level), CHECK CONDITION then success
(READ, REQUEST SENSE, READ), Get Max LUN stall, first-ready-LUN selection.
Also: capacity(10)/(16), READ/WRITE(16) across 2^32, chunking at 64 KiB (5
commands for 300 KiB), write-protect, 2048-byte sectors rejected, bad args.

## Kernel verification

`./scripts/build-all.sh` (release, both arches) succeeds. Headless
`run-qemu.sh {x86_64,aarch64} --no-gpu -display none` (TCG on this Mac) boots
to init and greetd on both arches with no panic; the only notice is the
expected no-GPU banner from `--no-gpu`.
