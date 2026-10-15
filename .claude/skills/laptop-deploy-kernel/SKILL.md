---
name: laptop-deploy-kernel
description: Put a newly built LeandrOS kernel (and initrd) on the real laptop's USB stick over SSH and boot it once, safely - a bad kernel cannot brick the stick because the previous good kernel and the flashed factory kernel stay as Limine entries. Use for every kernel change that must be tested on the laptop/PC hardware, for kernel command-line knob A/B tests, and to promote or roll back a kernel.
---

Prerequisites: the `laptop-remote-debug` skill (LEANDROS_REMOTE set, key login
works). Release builds only.

## Round trip

```sh
./scripts/build-all.sh --arch x86_64                 # release; LEANDROS_KERNEL_TAG=<label> to name it
python3 scripts/remote.py deploy-kernel --reboot     # upload, verify, arm, reboot, check, fetch logs
python3 scripts/remote.py promote                    # only after the new kernel proved itself
```

`deploy-kernel` takes `target/final-<arch>/kernel` + `initrd-<arch>.cpio`
(override `--kernel/--initrd`; arch = the target's `uname -m`). It uploads them
sha256-checked, and `espctl install` copies them to the ESP as
`/leandros/k/<id>/{kernel.elf,initrd.gz}`, reads them back from the medium, and
only then rewrites limine.conf with the new `LeandrOS trial <id>` entry as the
default. With `--reboot` it waits for sshd and prints `NEW KERNEL RUNNING: trial
<id>` plus `uname -v` (`#1 <build tag>`), or says it is not. `--promote` promotes
right away when the check passed.

Kernel arguments: `--cmdline 'i915.kmsdump=3 klog.flush=2'` replaces the extra
arguments of the new entry (default: the good entry's). Same kernel, other knobs:
deploy the same build again with another `--cmdline`. Change a slot in place:
`remote.py espctl cmdline good <args...>`.

## The boot entries (Limine on the stick's ESP)

| entry | what | when it boots |
|---|---|---|
| `LeandrOS trial <id>` | last deployed kernel | once: `espctl boot` in init sets the default back to the fallback as soon as the trial reaches init |
| `LeandrOS good <id>` | last promoted kernel | default otherwise |
| `LeandrOS factory` | the flashed `/kernel.elf` | fallback when there is no good one; never modified |

- Each entry's cmdline carries `leandros.slot=<slot> leandros.kid=<id>`;
  `espctl status` / `remote.py status` say what is running.
- The menu shows 3 s when there is more than one entry (`espctl timeout N`).
- limine.conf is replaced through a shadow copy (`/limine/limine.conf`, which
  Limine finds first), so a power cut at any point leaves a complete config.
  The flashed config is kept as `/leandros/factory.conf`; `espctl reset`
  restores it and removes all slots.

Other commands (`remote.py espctl <cmd>` runs any): `status`, `arm` (boot the
trial once more), `disarm`, `discard` (drop the trial), `default
<trial|good|factory>`, `promote [--force]`, `ls/cat/get/put/rm` on the ESP.

## If the new kernel does not come back

See "When a boot fails" in `laptop-remote-debug`: trial reached init -> power
cycle boots the fallback; died earlier -> the user picks `LeandrOS good`
(or `factory`) in the Limine menu. Then `remote.py logs` and `remote.py discard`.
Never `promote` a kernel that did not boot.
