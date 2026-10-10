# lane/nvwait: greetd must wait for a booting NVIDIA card (2026-10-09)

## Bug
Real PC, RTX 3070, `nvrm.start=1 nvrm.vktest=0` (nvrm-boot9.log). init starts
greetd at ~+0.8 s; the greeter's `/bin/gpu-env` runs `gpuprobe caps` once. The
OpenRM bring-up is still in GSP boot then (firmware loaded +3.2 s, KMS ready
+7.3 s, nouveau UAPI ready +7.8 s), card0 is not yet `nouveau`, gpu-env finds no
renderer, greeter-real exits 78 and init prints the NO GPU RENDERER banner and
never retries.

boot6 vs boot9: boot6 (`nvrm.vktest=1`, slow USB root, pre-fsio) greetd also
started at ~+0.8 s, but the USB-stick F2FS reads were so slow (GSP firmware
alone took 4.2 s) that the greeter shell and gpu-env only ran after KMS was
ready (+9.4 s); cosmic-comp's first WDOG lines come well after. lane/fsio sped
the root up (boot9: root ready +0.85 s, firmware 3.2 s), which exposed the race.
The ordering in boot6 was luck, not design.

## Fix
- `/proc/nvrm_state`: `absent | probing | ready | failed`
  - state lives in `sched::nvrm_state` (sched is the crate both drivers/nvidia
    and the VFS already depend on); VFS serves the node (servers/vfs).
  - PROBING is set in drivers/nvidia `init` when `nvrm.start=1` and a GPU exists;
    READY at the end of `uapi::init` (after the KMS seam registered); FAILED in
    `device::finish` if still probing (rm_init failure, adapter failure, UAPI
    or `nvrm.uapi=0`, no start) and when the watchdog finds the RM thread
    unresponsive. No NVIDIA / no `nvrm.start` stays `absent`.
  - `nvrm.fakestate=<secs>:<ready|failed>` on the kernel command line fakes
    "probing until <secs> of uptime, then ready/failed" (QEMU rehearsal).
- gpu-env: on `probing`, polls every 250 ms (cap 90 s) then continues to the
  normal gpuprobe; logs `gpu-env: waiting for NVIDIA card (probing)... <state>
  after X.Y s`. Absent = one read, no delay. `LEANDROS_NVRM_STATE_FILE` points
  it at a fake file.
- init: greetd exit 78 while the state is probing: wait (cap 90 s), and if ready
  restart the graphical login once; otherwise the usual banner.

## Tests (this Mac)
- build-all.sh release, both arches; regress suite via runtests.py both arches:
  all RC=0 (aarch64 HVF, x86_64 TCG).
- aarch64 and x86_64 `--virgl` boots: COSMIC session starts, gpu-env log has no
  waiting line, `/proc/nvrm_state` = absent.
- wait path with LEANDROS_NVRM_STATE_FILE: probing -> ready after 3.1 s then
  normal renderer selection; probing -> failed after 2.1 s then the NO GPU
  banner (LEANDROS_RENDERER=zink to force refusal); state `a` (absent) takes
  0.03 s total.
- x86_64 kernel compiled and linked with `--features nvidia` (libnvrm.a via
  ports/openrm/build.sh) into a scratch target dir; default build untouched.
- NOT exercised: init's 78+probing retry and `nvrm.fakestate` (the Limine
  cmdline is baked into the image; needs a limine.conf edit) and the real PC.
