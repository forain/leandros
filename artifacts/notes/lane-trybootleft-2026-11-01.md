# lane/trybootleft 2026-11-01

`/proc/tryboot` (vfs, state in `sched::tryboot_state`, written by kernel/src/tryboot.rs):
`state=off|armed|held|confirmed deadline_s=N left_s=N holds=N` (left_s 0 when off/confirmed; holds counts hold-on calls). Never prints the cmdline.
`espctl pi status` ends with `tryboot: <that line>` or `tryboot: off`; `pi_card.pi_status_cmd` (remote.py pi-status) prints the same.
Test (aarch64 QEMU, leandros.tryboot=1 tryboot.deadline=300): armed left 299 -> 293 after 6 s; tryboot-hold -> held holds=1; tryboot-commit -> confirmed. check-rpi5.sh passes.
