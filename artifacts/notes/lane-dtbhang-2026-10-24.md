# lane/dtbhang, 2026-10-24 (openbugs L8.11)

## Verdict: REFUTED as a kernel bug. It was a harness run-id collision. Harness guard added, scan hardened.

### Evidence
- integ9 left exactly one "hung" serial log: `/tmp/ffsession/tests-i9-a64reg/serial-full.log`, 18 lines. It ends with
  `[early] dtb ram scan: found at 0x40000000 (unmapped pages skipped: 0)` and then `[early] dt`, which is
  the start of the *next* line (`[early] dtb reserved: none`). So the scan had already finished: it hit
  the first page it read (QEMU virt puts the DTB at the start of RAM). The output stops in the middle
  of a string.
- `summary.json` for that same run says `"boot": "ok"`. `driver.py start` and `login root` both
  succeeded, which means a VM reached `login:`. After that, every command failed with
  `[Errno 32] Broken pipe` and then `cannot connect to serial socket`.
- `serial-full.log` is QEMU's `logfile=`, and every `driver.py start` truncates it. A log that stops
  at early boot right after a successful login therefore belongs to a *second* QEMU, started with
  the same `LEANDROS_RUN_ID`. That `start` killed the VM we had logged into (hence EPIPE), and our
  log was copied mid-boot. integ9 ran `usbunplugtest.py` (which runs `driver.py stop` and then
  `start` for each phase) at the same time as regress. Its hid phase reported "FAIL: boot" in both
  runs: the same collision, seen from the other side.
- Scan time measured on 20 boots (new log field): 376–464 us. 20/20 boots reached login (HVF,
  `tests-dtbhang-loop`).

### Changes
- `.claude/skills/run-leandros/driver.py`: new `claim_run_id(who)` takes a non-blocking flock on
  `/tmp/leandros-<RUN_ID>-harness.lock` that lasts for the whole harness run. A second harness with
  the same id exits straight away with
  `LEANDROS_RUN_ID='x' is already in use by another test harness (pid N runtests.py <tag>)`.
  Both `runtests.py` and `usbunplugtest.py` call it. Ad-hoc `driver.py` calls are not affected.
  Verified: a second runtests started during the loop was refused with that message.
- `boot/src/device_tree.rs`: `fdt_header_sane()` means a magic match alone is no longer accepted.
  It checks that totalsize is between 40 B and 2 MiB and fits in the scan window, that
  last_comp_version <= 17, and that the struct, strings and memreserve offsets lie inside the blob.
  `scan_for_dtb` uses it. The scan was already bounded to 32 MiB, and `AT S1E1R` was already
  followed by an ISB.
- `kernel/src/main.rs` `early_dtb_ram_scan()`: logs the elapsed time from CNTVCT, e.g.
  `(unmapped pages skipped: 0x0, 385 us)`. pcibridgetest matches only the prefix.

### Tests
- 20-boot loop PASS 20/20; aarch64 regress all PASS except wifisimtest "boot failed" during a concurrent x86 build, solo rerun PASS; pcibridge PASS; x86_64 release build OK.

### Open
- None for L8.11. Concurrent harness runs need distinct `LEANDROS_RUN_ID`s, and they now get a loud
  error instead of a phantom "boot hang".
