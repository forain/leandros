# lane espdur / espdur2 — 2026-10-25 — true one-shot trial kernel (openbugs #80)

Status: FIXED in QEMU, both arches (results below). Laptop verification is still open; the plan is at the end.

## Problem
The one-shot trial depended on the TRIAL kernel's own userland (`espctl boot` from init) to set the Limine
default back to good. A trial that hangs before userland, or whose stick I/O is dead (laptop d02c, 2026-10-25),
stays the default and boots again after every power cycle. A userland retry (WIP 7efbd862, dropped) only helps
when userland runs.

## Options considered
- (a) UEFI BootNext, or (b) BLI `LoaderEntryOneShot`. Limine 11.4.1 honours LoaderEntryOneShot and deletes it.
  Both need the OS to call SetVariable at runtime, and our kernel has no EFI runtime services: no runtime
  mapping and no calling-convention shim on either arch. That would be a big, risky port, and firmware quirks
  hit it on real HP UEFI. Rejected for now.
- (c) CHOSEN: let Limine consume the one-shot itself. Limine (common/menu.c) always writes the
  `LimineLastBootedEntry` NVRAM variable (NV|BS|RT) during boot services, just before it boots an entry. With
  `remember_last_entry: yes`, an entry path that resolves in the config beats `default_entry` (only
  `LoaderEntryOneShot` ranks higher). Entry paths are names. A duplicate name gets a `#N` suffix, so one config
  cannot alias the trial onto good. A SECOND Limine instance can, because it has its own config: Limine's UEFI
  config search starts in its own EFI directory.
- (d) An init-level retry is not needed as defence in depth any more. `espctl boot` still disarms, in whichever
  kernel next reaches userland.

## Design (userland/espctl/src/conf.rs, main.rs `write_slots`)
Armed config (chain form), with a nonce `<n>` (6 hex digits) that is fresh on every armed write:
```
remember_last_entry: yes
default_entry: 1
/LeandrOS once <tid> ~<n>      protocol: efi, path: boot():/EFI/LT<n>/BOOT{X64,AA64}.EFI
/LeandrOS trial <tid> ~<n>     (manual pick)
/LeandrOS good <gid> ~<n>
/LeandrOS factory ~<n>
```
`/EFI/LT<n>/` holds a copy of `/EFI/BOOT/BOOT*.EFI` and a `limine.conf` with `timeout: 0` and one entry. That
entry is the trial kernel, NAMED `LeandrOS good <gid> ~<n>` (or `factory ~<n>` when there is no good slot).
Both files are written and read back (SHA-256 and exact text) before the main config is committed.

Boot 1: the remembered path predates the nonce and matches nothing, so `default_entry` picks the chain. Limine B
records "good ~n" and loads the trial. Boot 2+: the remembered path resolves to good, whatever the trial did.
`espctl boot` (in the trial, or in the good kernel after a hang) writes the plain form: no
`remember_last_entry`, so a manual menu pick never sticks. It also removes stale `EFI/LT*` directories. The
unarmed config is byte-identical to before.

Residual window: A writes "once ~n" and then chainloads B. If B fails before its own write (a corrupt B
binary or config), boot 2 takes the chain again. B's files are verified on write, and that window contains
only firmware and Limine code, none of ours.

## Evidence
- Pre-fix (lane espdur, 780ae21e): espresettest `cut` (= today's `fault`) FAIL; the next boot was trial.
- 597aeb7a, aarch64 (HVF), espresettest: setup PASS, fault PASS (next boot: good), hang PASS (the kernel hung
  with `leandros.testhang=1`; after system_reset the next boot was good; disarm noted).
- x86_64 (OVMF, TCG) espresettest: setup/fault/hang all PASS. aarch64 regress: overall PASS, 0 boots failed.

## Test hooks
- `leandros.testhang=1` (kernel/src/init.rs): the kernel spins before userland.
- `espctl.fault_boot=1`: the first reset by `espctl boot` fails.
- `.claude/skills/run-leandros/espresettest.py <arch>`: about 10 min per arch.

## Laptop verification plan (deploy lane; hold `lock.sh laptop`)
1. Deploy a current-main kernel with remote.py deploy-kernel, without `--reboot`. Check the arm:
   `espctl cat limine.conf` must show `remember_last_entry: yes` and a `LeandrOS once … ~<n>` entry, and
   `espctl ls /EFI` must show `LT<n>`.
2. Reboot. The trial must come up (`uname -v`, slot=trial). The status must show the plain form again (no
   `~`). Reboot once more: the good kernel must come up.
3. Hang case: deploy a trial with `--append leandros.testhang=1`, reboot, wait until the screen stops at the
   testhang line, then hold power to cut. Power on: the good kernel must come up without touching the menu,
   and the console/klog must show `espctl boot` reporting "did not reach userland".
4. If step 3 boots the trial again, HP NVRAM writes from Limine are not persisting. Check from the Limine
   menu editor or the firmware, and report it. The plain-form fallback (userland reset) still works as before.
5. Fast Boot ON skips removable media entirely. This design adds no firmware Boot#### entry, so the stick
   boot path is unchanged.
