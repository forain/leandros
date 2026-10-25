# lane/esphang 2026-10-25: ESP hang / power-cut test (espdur step 3-4)

Result: PASSED (no code change).

- Trial 4c48b5088c9b (main 1ed7426a, `leandros.testhang=1`) deployed without --reboot; limine.conf showed
  `/LeandrOS once 4c48b5088c9b ~3F6EBB` and `/EFI/LT3F6EBB`.
- `remote.py reboot`, kernel hung at the testhang line; the user cut power and pressed it again, Limine menu untouched.
- Next boot: good 17c93ea8cfbd (`#1 laptopsync5`, up 64 s), no trial re-boot, so HP NVRAM writes from the
  Limine `once` entry were consumed/persisted correctly.
- klog: `espctl: the armed trial kernel did not reach userland (consumed by the bootloader, or good picked
  in the menu): disarmed, next boot: good`.
- `espctl status`: default=good armed=0 trial=-; limine.conf back to good + factory only (leftovers gone).
