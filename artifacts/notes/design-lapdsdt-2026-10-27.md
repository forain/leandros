# Design / handoff: lane lapdsdt (laptop `_BST`/`_PSR` read 0), 2026-10-27

Status: **HANDOFF**. The process changed mid-lane: fast-worker writes the code. Step 1 is coded and committed on
`lane/lapdsdt` as `06b01cc8` (WIP, not merged, worktree `../leandros-lapdsdt`). It has not been built or tested yet. A
build was started; see "Build state" at the end. **The root cause of the zeros is NOT established yet**, because that
needs the real DSDT. This note gives the plan, the hypotheses ranked against the current code, and the instrumentation
that decides between them.

## 0. Facts (lane-lappower note + code)
- On the laptop, the namespace loads 5494 nodes with 0 errors. `_BIX` is correct (cycle count 464, design voltage, and
  the strings), so EC reads worked at least while `_BIX` was evaluated, or `_BIX` comes from somewhere other than the EC.
- `_BST` = {state ?, rate 0, remaining 0, voltage 9.14 V}. `_PSR` = 0. The firmware booted with SCI_EN clear.
- `drivers/src/acpi_power/power.rs` ~:147 and ~:182: an EC field read does `ec::read(addr).unwrap_or(0)`. **A failed EC
  transaction silently becomes 0**, and `ec.rs::wait()` turns the EC off for good after 32 timeouts (`PORTS.store(0)`).
  After that, every EC field reads 0 and nothing is logged per access.
- `ec.rs::read()` does not drain a stale OBF before it sends RD_EC (0x80). If OBF is already set (a leftover byte from a
  `_Qxx` query or from the firmware), `wait(OBF)` returns at once and returns the *stale* byte. Each later read is then
  shifted by one. Linux's EC driver flushes OBF and serializes all EC traffic under one lock.
- `aml.rs:1306`: Acquire always succeeds. Sleep and Stall need checking: if they are no-ops, the HP "SMBus-over-EC"
  battery protocol fails. That protocol writes SMPR/SMAD/SMCM, then polls SMST in a `While` loop with `Sleep`, then
  reads SMD0. The poll times out at once, and the method returns zeros.
- `_REG(3,1)` IS already called (power.rs:298-308), but only for parents of EC OperationRegions. HP often also
  checks a `ECOK`/`ECRG`/`ECON` Name that `_REG` sets. That should be fine, unless `_REG` itself fails partway.

## 1. Raw tables export (DONE in 06b01cc8, needs build + test)
Files:
- `servers/vfs/src/sysclass.rs` (end of file): `TableFn`, `set_acpi_tables()`, `is_tables_dir()`, `acpi_table()`,
  `is_tables_path()`, `tables_dirent()`, and `SC_TABLES = SC_BASE|0xF<<8|0xFF`. `dirent()` routes `SC_TABLES` first.
- `servers/vfs/src/lib.rs`:
  - The intercept list next to `sysclass::is_path(path)` (~:4812) gains `|| sysclass::is_tables_path(path)`.
  - Open (~:7488) gains two branches before `sysclass::is_path`. The tables dir becomes `VnodeKind::SysBlock{dev:
    SC_TABLES, level: LVL_SC_CLASS}`. A table file is root-only (EACCES otherwise) and becomes a TmpFile filled with
    `tmp_fill_kernel(.., t)`, using the full table (not clamped to TMP_BUF_SIZE) and the fake path
    `/tmp/.acpitbl_<idx>`.
- `drivers/src/acpi_power/power.rs`: `static RAW`, `raw_add()` (names FACP, ECDT, DSDT, SSDT1..n, the Linux style),
  and `pub fn raw_table(idx)`. It is fed from `init()` for the FADT, the ECDT and every table in `for_each_aml_table`.
  `mod.rs` re-exports `raw_table`.
- `kernel/src/lappower.rs::init`: `vfs_server::sysclass::set_acpi_tables(|idx,out| ...)` after `set_sysclass_ops`.
- `scripts/mkfs-f2fs-populated.py` (~:995) stages `/sys/class/power_supply`, `/sys/class/backlight` and
  `/sys/firmware/acpi/tables` as on-disk dirs, which fixes "`ls /sys/class` lacks power_supply/backlight". The overlay
  is consulted before the on-disk dir, so a populated class still shows its devices. NOTE: the laptop root is NOT
  re-imaged by deploy-kernel. On the laptop, prove the listing fix by checking a freshly built root image in QEMU
  (`ls /sys/class` in x86_64 QEMU with `acpi_power.fake=1`). Optionally also add a vfs getdents injection for
  `/sys/class`, if the orchestrator wants it on the existing stick without a reflash.
- Check: `stat`/`ls -l` of a table may report size 0 (open-time snapshot). `cat`/`dd`/`scp` read to EOF, which is
  enough for the pull.

## 2. Pull + disassemble (Mac; iasl is installed at /opt/homebrew/bin/iasl)
```sh
# under $SP/lock.sh laptop, laptop on a trial built from lane/lapdsdt (merge main first, INTEL=1)
for t in $(ssh root@172.16.149.179 ls /sys/firmware/acpi/tables); do
  ssh root@172.16.149.179 cat /sys/firmware/acpi/tables/$t > $SP/acpi/$t.dat; done
cd $SP/acpi && iasl -e SSDT*.dat -d DSDT.dat   # -> DSDT.dsl
```
(Use the remote.py pull/run helpers from the laptop-remote-debug skill if plain ssh as root is not set up.) Check that
DSDT.dat starts with "DSDT" and that the length at offset 4 matches the file size.

## 3. Read in DSDT.dsl
Grep for: `Device (BAT0)` -> `_BST`, `Device (AC*)` -> `_PSR`, `OperationRegion (.*EmbeddedControl`, `_REG`,
`ECOK|ECRG|ECON|ECAV`, `SMPR|SMST|SMAD|SMCM|SMD0` (SMBus over EC), `Sleep|Stall` in those paths, `\\_SB.*SMI|0xB2|SMIC`
(SMI mailbox), `OperationRegion (GNVS, SystemMemory` (NVS fields the SMI handler fills), and `Acquire` timeouts.
Classify the path as (a) plain EC fields, (b) SMBus over EC, (c) SMI + GNVS, or (d) cached Names updated by `_Qxx`.

## 4. Instrumentation (fast-worker adds this first; it decides between the hypotheses)
- `aml.rs` gains a host-callback trace. Knob `acpi_power.trace=1`, or always on in amltest. Each EmbeddedControl access
  logs `ec rd off=0x.. w=.. -> 0x..` / `ec wr off=0x.. <- 0x..`, plus `ec TIMEOUT` with the running count. SystemIO
  0xB2 writes log `smi <- 0x..`. Log only during an explicit `_BST`/`_PSR` eval, not during the 1 s polls (they spam).
- `power.rs`, after init and once after the first `_BST`, prints
  `acpi: ec timeouts=<n> present=<0|1> status=0x..`.
- amltest (`$SP/amltest`): add a mock Host with a 256-byte EC RAM array (seeded from a hex file, so a real EC dump
  can be replayed), SystemIO 0xB2 logging, and GNVS memory. Then run `\_SB...BAT0._BST` and `AC._PSR` against the real
  DSDT.dat+SSDTs. If the zeros reproduce with a non-zero EC RAM, the bug is in the interpreter. If they don't, it is
  hardware timing or the EC.
- Add a debug dump: `/sys/kernel/debug`-style, or log 256 bytes of EC RAM via `ec::read` 0..255 once at init (root
  knob `acpi_power.ecdump=1`). The amltest replay then uses real values.

## 5. Ranked hypotheses and the fix for each
1. **EC transactions fail or are mis-framed** (most likely given the silent `unwrap_or(0)` and the 32-timeout kill).
   Fix in `ec.rs`:
   - `read`/`write`/`query`: before the command, if `inb(cmd)&OBF != 0`, drain with `inb(data)` (bounded, ≤16 times).
   - Raise the per-wait timeout from 5 ms to 50 ms. Replace the permanent disable with "log + back off 1 s" and reset
     TIMEOUTS on success.
   - Serialize all EC traffic (the kacpid `_Qxx` path and the AML field path) under one `spin::Mutex<()>` in ec.rs.
   - Field access in power.rs (~:147/182) logs failures instead of folding them to 0. Optionally use burst mode
     (0x82 BE_EC / 0x83 BD_EC) for multi-byte fields, as Linux does for >1 byte.
2. **Sleep/Stall are no-ops**, which breaks the SMBus-over-EC polling. Check aml.rs ops 0x5B22 (Sleep) and 0x5B21
   (Stall). Make them real delays: `Host::sleep_ms` (busy-wait with `i915::delay_us` is fine at init, and kacpid can
   block). Also check that `While` with a decrementing timeout counter keeps its local state.
3. **SMI path (port 0xB2 + GNVS)**: the SystemIO write must reach the real port. Check that the Host's SystemIO write
   for width 8 at 0xB2 does `outb` (not a no-op or wrong width), and that GNVS SystemMemory is mapped uncached and
   read *after* the SMI. If HP needs `\_SB.WMID`/`HWMC` mailboxes, emulate them as plain memory.
4. **ECOK-style flag not set**: `_REG` runs before `\_SB.PCI0._INI`. If `_INI` resets the flag, or the flag lives in a
   scope whose `_REG` we skip (an EC region declared under a different device than the one with `_REG`), call `_REG`
   on every device that has an EC region *and* on the `PNP0C09` device itself, after PCI0._INI as well.
5. **Field offset/width bug** (BankField or IndexField selector writes for battery banks such as `BSEL`/`BNDX`): the
   trace in section 4 shows the selector write followed by reads. Compare against the iasl listing offsets.

## 6. Tests
- Host: `cd $SP/amltest && cargo run --release -- <DSDT.dat> [SSDT*.dat] --ec ec.hex --eval \\_SB.PCI0.LPCB.BAT0._BST`
  shows non-zero remaining/rate with the real EC dump. Add the real-DSDT case to amltest (a fixture in the scratchpad
  only). Before storing anything, zero the serial strings: the `_BIX` serial "36821 2020/05/14" if it is a literal,
  and any `SerialNumber`/UUID-like strings in the DSDT. Show it failing on the unfixed interpreter first.
- QEMU x86_64: `acpi_power.fake=1`. Then `ls /sys/class` lists power_supply and backlight (new root image), and
  `ls /sys/firmware/acpi/tables` lists DSDT, FACP, … The DSDT read as root has the right length; as non-root it gives
  EACCES. vfstest green. aarch64 vfstest green (module inert, no tables registered).
- Laptop (lock.sh laptop, trial from current main, INTEL=1, do NOT promote):
  - `cat /sys/class/power_supply/BAT0/{capacity,status,energy_now|charge_now,power_now|current_now}` and
    `AC/online`. Expected: capacity 1–100 and plausible; status Charging, Full or Not charging on AC; online=1 while
    plugged in; now/rate non-zero, unless Full (rate may legitimately be 0 when Full; then unplug-free proof = remaining
    ≈ full).
  - Expected log lines: `acpi: ec timeouts=0 present=1`, and with trace=1 a `_BST` trace with non-zero `ec rd` values.
  - Boot `good` back before releasing the lock.

## Build state
`06b01cc8` was compiled with `INTEL=1 LEANDROS_KERNEL_TAG=lapdsdt1 ./scripts/build-all.sh --arch x86_64` in the
background. The result is not confirmed in this lane, so the fast-worker must rebuild and fix any compile errors. The
likely spots are the `alloc::format!` in vfs lib.rs, and `tmp_free_slot` returning Option inside a fn that returns a
reply (handled with let-else).
