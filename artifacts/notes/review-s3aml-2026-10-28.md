# Review: merge cd2d2389 (lane/s3aml), 2026-10-28

Scope: `git diff cd2d2389^1 cd2d2389`, plus the laptop DSDT/SSDTs in scratchpad/lapdsdt (disassembled).

## Verdict
- **Safe to ship with one cheap fix (F1). Then do a plain-boot soak on the laptop before promoting it.**
  The trial 151aa042909f was booted only with dry-run knobs, it reset, and battery/display/Wi-Fi were never checked.
- **Do NOT block SMI_CMD (0xB2) at runtime.** The premise that "every space now reaches the host" is wrong
  for the risky spaces: SystemMemory, SystemIO, PCI_Config and EC already reached hardware before this merge
  (the old gate was `s <= SPACE_EC`, i.e. spaces 0..3). The merge only adds SystemCMOS reads (writes need a
  knob), plus counters for SMBus/GPIO/GSBus/PCC, which still read 0 and drop writes.
  HP firmware issues software SMIs from ordinary runtime methods:
  - `_STA` at DSDT:9641 calls `SSMI(0xEA3A)`
  - `VHIV`/`VGDD` hotkey and video methods call 0xEA74
  - `UWLS` calls 0xEA90 and 0xEA84
  - SSDT18 `XTBT` (Thunderbolt) writes `SSMP = 0xF7`
  Linux (ACPICA) and Windows allow these too: 0xB2 is not in ACPICA's protected-port list. Blocking them
  would change `_STA` results and break hotkeys/TBT. The 0xEA91 reset comes from *calling `_WAK(3)` without a
  real S3*, not from SMI reach in general.
- aarch64: runtime is untouched. `lappower::init` is `#[cfg(target_arch = "x86_64")]` (kernel/src/init.rs:339).
  On aarch64 the driver only compiles, against `pio` stubs. `drivers/acpi-aml-tests` is a detached workspace.

## What actually changes on a default x86 boot (no pm.* knobs)
1. **The FACS global lock is now taken around every `Field(..., Lock, ...)` access.** The HP DSDT has 127
   Lock fields, among them the EC fields that battery polling and `_Qxx` read, and `SSWP`/`SSMP` (the 0xB2 port).
   So every battery poll now does `lock cmpxchg` on FACS+16. That matches the spec and is correct, but it is
   the one new path that runs constantly, so F1 matters.
2. SystemCMOS reads are real. The HP has no SystemCMOS region; its CMOS access is SSDT18 `RDCM`, which uses
   SystemIO 0x70 and was already live. So this change does nothing on the laptop.
3. Accesses to the other spaces are only counted. The `Sleep()` cap stays 100 ms outside `SLEEP_WIN`, and
   `pause_poll` is only set by `s3_dryrun`. No behaviour change.

## Findings (ranked)

### F1 (Medium, fix before promote): if global-lock acquire times out, release then corrupts the firmware's ownership
`gl_acquire` gives up after 100k × `delay_us(1)` (≥100 ms, spent while holding the STATE spin mutex). On
timeout it increments `GL_FAILS`, but it leaves `GL_DEPTH` raised and the field access still goes ahead.
`gl_release` then does `fetch_and(!3)`, which clears the firmware's owned and pending bits. From then on SMM
and the OS can both believe they own the EC lock. A single timeout also makes every Lock-field access wait
another 100 ms with STATE held, which stalls battery reads, backlight and lapi2c.
Fix:
- Keep an `GL_OWNED: AtomicBool`.
- Make `Host::global_lock(acquire) -> bool`.
- On timeout, clear only *our* pending bit with a cmpxchg loop, leave `GL_OWNED=false`, and have
  `unit_read`/`unit_write` return an error (ACPICA returns AE_TIME, so the method fails). Do not access the
  field without the lock.
- In `gl_release`, touch FACS only when `GL_OWNED`.
- Cut the timeout to about 10 ms. SMM never holds the lock across an SMI return, so a long hold means the
  lock word is stale or wrong.
- Optional: after an acquire that had to go through pending, clear PM1_STS.GBL_STS (bit 5, W1C).

### F2 (Low-Med): CMOS index/data ports have no lock shared with the kernel RTC
`arch/x86_64/src/rtc.rs` (`set_alarm`, `clear_alarm`, `read_raw`) and the new `SPACE_CMOS` path both drive
0x70/0x71 with no common lock. The same goes for SystemIO 0x70 regions such as HP SSDT18 `RDCM`, which is
pre-existing. If AML changes the index between the RTC code's index write and its data write, `set_alarm`
writes the wrong CMOS byte. That can cause a CMOS checksum error at the next POST.
It is inert on the HP for SystemCMOS, but `RDCM` is live.
Fix: export a `CMOS_LOCK` (irq-safe) from `arch/x86_64/src/rtc.rs` and take it in:
- the KHost `SPACE_CMOS` read and write paths
- KHost `SPACE_IO` accesses that overlap 0x70..0x73
- the RTC functions

### F3 (Low): `SLEEP_WIN` is set before the STATE lock is taken
`run_group` stores `SLEEP_WIN=true`, then waits on STATE. If kacpid, backlight, lapi2c or nvidia `_DSM` holds
STATE at that moment, its `Sleep()` gets the 2 s cap and its writes are traced.
Fix: set and clear `SLEEP_WIN` (and reset `TRACE_LEFT`) inside the `with_ns_locked` closure.
Separately, a 2 s `Sleep()` inside the window busy-waits with STATE held. That is acceptable during a suspend,
but the s3core design should state it.

### F4 (Low): `pause_poll` only gates `power::poll()`
`with_ns` callers (backlight `_BCM`, lapi2c `_INI`/`_DSM`, nvidia) still evaluate AML between `_PTS` and the
SLP_EN write.
For s3core:
- freeze userspace and devices before `_PTS`
- take one "AML quiesced" flag that `with_ns` refuses with an error while it is set
- after resume, drain `SCI_EVT` (`_Qxx`) once the pause ends, before userspace is thawed

### F5 (Info): `Acquire(_GL)` is still a no-op mutex (aml.rs:273/1327)
It is inconsistent with the new Field Lock path. HP's `SSMI(..., Arg4)` passes Arg4=Zero everywhere, so
nothing depends on it today. Map `\_GL` Acquire/Release onto `gl_acquire`/`gl_release` later.

### F6 (Info): the dry-run knob
`pm.s3_dryrun=2` resets the HP every time. Keep it (it is diagnostic), but log a warning at boot when it is
set. `=1` (`_PTS` only) is safe. `_PTS` then calls `HPTS`, which may put the EC into sleep mode, and the run
ends with `_TTS(0)` but no `_WAK`. Confirm that fans and keyboard still work after `=1`. If they do not,
`=1` also needs the commit-forward policy described below.

## No change needed
- **Deadlock between GL, EC and kacpid:** none possible. The GL is a bounded spin taken only inside STATE and
  only around one leaf access. The EC driver's lock is taken inside it and never the other way round.
  `Field(Lock)` nesting through Index/Bank fields is reentrant via `GL_DEPTH`.
- **GL details:** the FADT offsets are right (X_FIRMWARE_CTRL at 132, FIRMWARE_CTRL at 36). The
  `LockRule = flags & 0x10` decode is right. The `GBL_RLS` read-modify-write on PM1a_CNT bit 2 is safe,
  because SLP_EN reads back as 0.

## s3core: what to do about SMI 0xEA91 in `_WAK`
What the firmware does:
- DSDT:26802 `_WAK`: `If (Arg0 == 3) { \_SB.SSMI(0xEA91, Arg0, 0, 0, 0) }`, issued after `HWAK` and
  `Notify(BAT0)`.
- `SSMI` → `GSWS` → a WordAcc write of `SSWP` (0xB2/0xB3), i.e. `outw(0xB2, 0xEA91)`.

This is HP's "resumed from S3" SMM notification. The handler behind it is not visible in AML. Treat
"`_WAK(3)` without a real S3 resets the machine" as a firmware contract.

Design:
1. **`_WAK(3)` runs only after a real S3.** That means the CPU came back through the FACS waking vector, or
   PM1_STS.WAK_STS is set. Use Linux's order:
   1. `leave_sleep_state_prep`: write the `\_S0_` SLP_TYP into PM1x_CNT, and clear WAK_STS and the fixed status bits
   2. restore the runtime GPE_EN
   3. `_SST(2)`
   4. `_WAK(3)`, allowing the SMI
   5. `_SST(1)`
   6. drain the EC queries
   7. `_TTS(0)`
2. **Commit forward once `_PTS(3)` has returned.** Linux's `acpi_pm_finish` can run `_WAK` after an abort,
   but on HP that resets the machine. So:
   - Do everything that can fail *before* `_PTS`: device suspend, AP offline, waking vector and trampoline,
     the RTC alarm, GPE setup.
   - After `_PTS`, only `wbinvd` and the SLP_TYP|SLP_EN write remain.
   - If one of our own steps fails after `_PTS`, do not abort. Arm the RTC for about 2 s and enter S3 anyway,
     so `_WAK` always follows a real wake. `_PTS`'s `HPTS` puts the EC into sleep mode, so skipping `_WAK`
     would leave fans and keyboard in that state. That is worse than a 2 s nap.
3. **Fall-through:** if WAK_STS is still clear 1 s after SLP_EN, the hardware ignored the sleep. Log it, then
   run the full resume path including `_WAK` (Linux behaviour). A firmware reset is an acceptable worst case
   for a path that should never happen.
4. **Change the lapsusp2 design**, because it contradicts the hardware:
   - :83: "`_WAK(3)` always runs once `_PTS` has run (error path)" → commit-forward.
   - H2 (pm_test=platform, `_PTS`/`_WAK` while awake) is unsafe on HP. Replace it with "H2′: `_PTS`, then a
     real S3 with a 2 s RTC wake, then `_WAK`".
   - :570's mitigation becomes commit-forward.
   - Add a knob `pm.s3_wak=0` for diagnostics only. It skips `_WAK` after a real resume.
5. **Before s3core relies on this, prove which write causes the reset (one laptop boot):**
   - Add `pm.aml_smi_drop=0xEA91`, which logs and drops that one SSWP write.
   - Before every 0xB2 write, print a marker to serial or the persistent log, synchronously, so the last line
     is reliable.
   - Run with `pm.s3_dryrun=2`.
   - If there is no reset, the SMI is confirmed. If it still resets, suspect the `_WAK` writes before it:
     Mem 0xfed159a0 (MCHBAR+0x59A0, the package power limit), EC 0xe3/0xf7, or `HWAK`. Re-trace those.
   - If a Linux live USB is available, `echo platform > /sys/power/pm_test; echo mem > /sys/power/state` on the
     same laptop tells us whether stock Linux also resets.
6. Optional, cheap: keep a ring of the last 16 SW-SMI values (value, ns, caller method) and log it in the
   `[S3]` summary and on panic. This is evidence only, not a gate.

## Gate before promoting cd2d2389 (or cd2d2389 + F1) as the laptop's good kernel
Boot the laptop plain, with no `pm.s3_*` knobs, and check:
- `[S3]`/aml logs show `global lock fails 0`
- battery % and AC plug/unplug events
- Fn brightness and hotkeys (these go `_Qxx` → SSMI)
- lid
- Wi-Fi and the desktop
- 30 min idle with no stalls, and EC timeouts unchanged from the previous good kernel

The usual QEMU x86_64 and aarch64 regress runs also apply.
