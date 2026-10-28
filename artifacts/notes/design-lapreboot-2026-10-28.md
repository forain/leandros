# lapreboot: EliteBook 840 G5 reboot sometimes ends POWERED OFF (2026-10-28)

## Facts from the logs
- The failing boot is b219.txt (`[init] reboot requested by pid 440`). Its banner is
  **build b01ea05b3e-20261026T2004Z**. Per git, that build contains neither lane/lappower
  (997cae88) nor lane/lapsusp (fb3ffc02), and b219 has no `acpi: SCI_EN was clear` or
  `acpi: namespace` line. The firmware had SCI_EN clear (legacy/SMM mode) and nothing of ours
  touched PM1/GPE/EC.
  **=> lappower's SCI_EN, the EC driver, GPEs and lapsusp's pm.rs are ruled out for this failure.**
- The tail of b219 matches b218 (a reboot that worked, on a lappower build) line for line up to
  `reboot: Restarting system`: same plan `quiesce=1 type=cold method=auto hda_crst=0
  xhci_hcrst=1 usb_vbus=on busmaster_off=1`, HDA to D3, e1000e released, phase 1 in about
  3.2 ms. Nothing in the logged part of the quiesce differs.
- FADT on this laptop (b219 `[ACPI]` line): RESET_REG = SystemIO 0xCF9, value **0x06**.
  PM1a_CNT = 0x1804, so PM1a_EVT = 0x1800 and PM1_EN = 0x1802. Linux's default on this machine
  (reboot=acpi) is therefore one `outb(0xCF9, 0x06)`: a hard reset with **no** power cycle.

## The code path (x86_64)
`kernel/src/power.rs::kernel_restart`:
1. `shutdown_prepare` (filesystems, NVMe CC.SHN).
2. `quiesce_phase1` (HDA codec to D3 with EAPD off, e1000e, list of bus-master functions).
3. `reboot: Restarting system`.
4. `machine_prepare` (klog final flush, SYNCHRONIZE CACHE, `cli`).
5. `quiesce_phase2` (xHCI halt and HCRST, bus mastering off). This prints to serial only, so it
   is invisible on the laptop.
6. `arch_x86_64::power::restart(p, Auto, cold=true)`.

`RebootCfg::from_cmdline`: `cold = opt("reboot.type") != Some("warm")`, so **cold is the
default**. It was introduced in 28637c95 (lane/rebootq, 2026-10-16) as extra insurance against
the CX8200 beep. The codec quiesce is what actually fixed the beep.

`arch/x86_64/src/power.rs::restart`, as used with Auto:
- **cold (the current default):** `reset_cf9(true)`, which writes 0xCF9 = 0x02, waits 50 us,
  writes 0x0E (FULL_RST: the PCH holds SLP_S3#/S4#/S5# low for 3-5 s) and waits 500 ms.
  Then `reset_acpi` (0xCF9 = 0x06), then `reset_kbd` (0xFE x10), then triple fault.
- **warm:** ACPI, KBD, ACPI, KBD, CF9 0x06, triple. This is Linux's BOOT_ACPI chain minus EFI
  and BIOS.

Linux `native_machine_emergency_restart` with the default reboot=acpi runs ACPI RESET_REG, then
8042 x10, then ACPI again, then 8042 again, then EFI ResetSystem, then (CF9 only with reboot=pci
or a quirk), then triple. **Linux never writes 0x0E on this laptop.** We do it first, on every
reboot.

## Ranked causes
1. **(High) The cold 0xCF9 0x0E power cycle is turned into S5 by the platform.** With
   FULL_RST, the PCH drops SLP_S3#/S4#/S5# for 3-5 s. On the HP, the EC sequences the rails from
   those signals. Any of the following leaves the machine off:
   (a) An EC race or policy: S5 seen, then rails or the DSW/SUS well cut, for example on battery
   with the BIOS "Deep Sleep"/"Power off in S5" settings, before the PCH deasserts SLP_S*.
   (b) The reset escalates to a **global reset**. This happens if firmware set ETR3.CF9GR, or on
   a host-reset handshake timeout. The platform then follows AFTERG3_EN, which laptops set to
   "stay off".
   Evidence for this cause: it is the only step that differs from Linux, it is the first and
   effectively the only reset our chain issues, and it is a power-state transition. The symptom
   ("off, power button needed") is exactly an S5 landing. Intermittency fits an EC timing race or
   a race with the AC/battery state.
2. **(Medium-low) A hang after `Restarting system`, mistaken for power-off.** This would be in
   klog `flush_final`, `flush_all_disks` (USB stick SYNC CACHE), or the xHCI halt/HCRST loop.
   Those waits are "bounded", but nobody checks the bounds once interrupts are off. On the laptop
   it cannot be seen (serial only). The screen would stay lit with the last console text, and
   power-off would need a 4 s hold. **Ask the user:** was the screen dark and the power LED off,
   and did a short press start it?
3. **(Low, for this failure) Leftover PM1/GPE state** (PWRBTN_STS, enabled wake GPEs, SCI_EN=1
   from lappower). This is ruled out for b219: the build had no lappower, and the reset is not an
   SLPx transition. With 0x0E going through an S5-like window, a stale enabled wake or PWRBTN
   event could still change the outcome on lappower kernels. Clearing it costs nothing.
4. **(Low) The fall-through writes while the full reset is in progress.** If the CPU outlives the
   0x0E write by more than 500 ms, we write 0xCF9 = 0x06 into a PMC that is mid power cycle. That
   is undefined, and could be the path that escalates to a global reset.
5. **Ruled out:** the HDA/e1000e quiesce (identical in the good b218), and lapsusp pm.rs (not in
   the build).

## Fix (intended diff, no code written)
### arch/x86_64/src/power.rs
```diff
-pub enum Method { Auto, Acpi, Cf9, Kbd, Triple }
+pub enum Method { Auto, Acpi, Kbd, Cf9, Cf9Full, Triple }
+
+/// Quiet the ACPI event blocks before any reset: PM1_EN = 0, PM1_STS written all-ones
+/// (W1C: PWRBTN/SLPBTN/WAK/RTC/PRBTNOR), GPE0_EN = 0, GPE0_STS written all-ones.
+/// SCI_EN is left alone. Logs one line with the before values.
+unsafe fn quiet_acpi_events(p: &AcpiPower) {
+    if p.pm1a_evt != 0 { outw(p.pm1a_evt + 2, 0); outw(p.pm1a_evt, 0xFFFF); }
+    if p.pm1b_evt != 0 { outw(p.pm1b_evt + 2, 0); outw(p.pm1b_evt, 0xFFFF); }
+    let half = p.gpe0_len / 2;
+    for i in 0..half { outb(p.gpe0 + half + i, 0); outb(p.gpe0 + i, 0xFF); }
+}
 
 pub unsafe fn restart(p: Option<&AcpiPower>, method: Method, cold: bool) -> ! {
+    if let Some(p) = p { quiet_acpi_events(p); }
-    // cold: cf9(0x0E), acpi, kbd ; warm: acpi, kbd, acpi, kbd, cf9(0x06)
+    // Auto (both types): ACPI RESET_REG, 8042, ACPI, 8042, CF9 0x06, then CF9 0x0E only
+    // as the last resort before the triple fault. `cold` only selects Cf9Full when the
+    // user explicitly asks for it via reboot.method=cf9full.
+    //   chain = [first(method)] ++ [Acpi, Kbd, Acpi, Kbd, Cf9, Cf9Full] minus first; triple.
```
- `reset_cf9(full)`: keep the 0x02 then code sequence (Linux's). For full, wait 5 s, not
  500 ms, before falling through, so the chain never writes into a running power cycle.
- `boot::acpi::AcpiPower` gains `pm1a_evt`, `pm1b_evt`, `gpe0`, `gpe0_len` (FADT offsets 56,
  60, 80, 92, which lappower already parses).

### kernel/src/power.rs
- The default becomes **warm** (Linux's behaviour): `cold = opt("reboot.type") == Some("cold")`.
  Plan line: `type=warm method=auto`.
- New knob: `reboot.method=auto|acpi|kbd|cf9|cf9full|triple`. `cf9` means 0x06 and `cf9full`
  means 0x0E. Keep `reboot.type=cold` as an alias for `method=cf9full`.
- Print the chain into the persistent klog *before* `flush_final`, for example
  `[REBOOT] chain: acpi(0xCF9<-0x06) kbd acpi kbd cf9 cf9full triple`, so the next failure's log
  says what was about to run.
- **Boot-side breadcrumbs (the decisive evidence for cause 1a vs 1b vs 2).** Early on x86_64,
  log one line `[PMC] reset cause: ...` with:
  - PMC 00:1f.2 config GEN_PMCON_A (0xA0) and GEN_PMCON_B (0xA4) raw values, decoded as
    GBL_RST_STS, HOST_RST_STS, PWR_FLR, AFTERG3_EN;
  - PWRMBASE (BAR at 0x48) + 0x48 ETR3 (CF9GR bit 20, CF9LOCK bit 31);
  - PWRMBASE + 0x1924/0x1928 GBLRST_CAUSE0/1;
  - PM1_STS at 0x1800 (WAK_STS bit 15, PWRBTN_STS bit 8, PRBTNOR_STS bit 11).

  Log only; write nothing except W1C of what was already logged, and no W1C at all on the
  first cut.

  Bit positions are from the 100/200-series PCH datasheet vol 2 and coreboot
  `soc/intel/skylake/include/soc/pmc.h`. **Verify before decoding.**
  How to read the result:
  - a power-button wake with GBL_RST_STS=1 means cause 1b;
  - a wake with no global reset after a 0x0E means 1a;
  - PRBTNOR_STS=1 (a 4 s hold) with the screen having been lit means cause 2.
- `quiesce_phase2`: after `cli`, record each step's end into a static `REBOOT_STAGE` and, if
  cheap, into the i915 framebuffer as a colored square (the laptop's only "serial"). A hang
  then shows which step it is.

## Laptop test plan
Use one trial kernel with the fix and the breadcrumbs, `remote.py deploy-kernel`. Then:
1. **Baseline (keeps the old behaviour).** Run `--cmdline "<good> reboot.method=cf9full"`. Loop
   `remote.py reboot --timeout 240` N=30 times, both on AC and on battery (unplug).
   Per-boot counter: before each reboot, `echo $((n+1)) > /home/leandro/.rbcount && sync`;
   after it, read it back and `grep -c 'Kernel starting'` in `espctl klog --all`. Record the
   `[PMC] reset cause` line of every boot.
   Expect: the power-off reproduces at some rate, most likely on battery. The PMC line on the
   boot after the power-button press tells us whether it was a global reset or not.
2. **Fix.** Default chain (warm, ACPI first), with no cmdline knob: N=50 cycles, AC and battery.
   Pass criteria:
   - 0 power-offs;
   - every boot's PMC line shows no GBL_RST_STS and no PWR_FLR;
   - no HDA beep through POST and the next boot (the original reason for cold);
   - USB stick and e1000e/iwlwifi come up on every boot (warm keeps VBUS on; the xHCI HCRST
     already handles it).
3. **Knob A/B:** 10 cycles each of `reboot.method=kbd` and `reboot.method=cf9` (0x06), to prove
   the fallbacks work on their own.
4. Rerun on a lappower kernel (SCI_EN=1): 20 cycles. The `quiet_acpi_events` line should show
   which PM1/GPE bits were set at reset time.
5. If step 2 is clean and step 1 reproduces, cause 1 is confirmed. Promote, and keep `cf9full`
   only as an opt-in knob.

Recovery for every cycle: if `REBOOT FAILED`, ask the user for a short power press and a note
of whether the screen was lit or dark. Then collect `remote.py logs`.
