# Remote machines and trial kernels

The real test machines — the HP EliteBook 840 G5 laptop and the PC, both booted from a USB stick — are driven over SSH instead of being re-flashed for every change. [`scripts/remote.py`](../../scripts/remote.py) runs on the development Mac with its OpenSSH client against the guest's russh `sshd` ([../userland/programs.md](../userland/programs.md#sshd)), as root and by key, and uses `/usr/sbin/espctl` in the guest to manage the boot partition. A kernel is tried as a one-shot *trial* boot entry, so a bad kernel cannot brick the stick. The same tool works against a QEMU VM.

## Reaching the machine

```sh
export LEANDROS_REMOTE=<address>[:port]     # or user@host; `qemu` / `qemu:<run-id>` for a VM
python3 scripts/remote.py discover          # scan the local /24 for the russh banner
python3 scripts/remote.py status            # kernel, uptime, cmdline, boot entries
```

- The key is `~/.ssh/leandros_remote` (`LEANDROS_REMOTE_KEY` overrides; `remote.py keygen` creates it). `write-usb.sh` copies its public half to the ESP as `/leandros/authorized_keys`, and `espctl boot` imports it into `/root/.ssh` on every boot. Root logs in by key only. `remote.py setup` prints the one-time steps when no key is on the stick.
- Host keys are kept in `~/.ssh/leandros_known_hosts`; a re-flashed stick has a new one (`remote.py forget-host`).
- The machine gets its address by DHCP, on Ethernet or on Wi-Fi with the saved profiles ([../servers/netctl.md](../servers/netctl.md)). The console line `DHCP configured, address: X (ifname)` is printed once per interface.
- `LEANDROS_REMOTE=qemu` reads the SLIRP port forward that the QEMU driver recorded for `LEANDROS_RUN_ID`. Use `LEANDROS_NET=user` for it: under `socket_vmnet` the Mac cannot reach the guest. A stick image built with `scripts/write-usb.sh <arch> --image <img>` and booted with `LEANDROS_USB_STICK=<img>` has the laptop's ESP, log partition and boot entries.

## Commands

| Command | What it does |
|---|---|
| `run <cmd…>`, `shell` | run a command (exit status passed through), or an interactive shell |
| `push <local> <remote>`, `pull <remote> <local>` | copy a file or directory, sha256-verified; `push` writes a temporary name and renames it, and skips identical files |
| `deploy-files LOCAL:REMOTE[:MODE] …` or `--manifest F` | upload many files. A running binary keeps its old inode, so restart the process to use the new one |
| `deploy-kernel [--kernel F] [--initrd F] [--cmdline ARGS] [--netwatch S] [--no-arm] [--reboot [--promote]]` | install the release kernel and initrd as the trial entry ([below](#trial-kernels)) |
| `promote`, `discard` | make the trial the good entry, or drop it |
| `espctl <args…>` | run `espctl` in the guest: `status`, `arm`, `disarm`, `default <trial\|good\|factory>`, `cmdline <slot> <args…>`, `klog [--all\|--list\|--boot N]`, `ls`/`cat`/`get`/`put`/`rm` on the ESP |
| `logs [--out DIR]` | fetch the console log of this boot and of the four kept boots, status, partitions, `limine.conf`, `/var/log` and `/tmp/*.log` into `artifacts/remote-logs/<time>-<host>/` |
| `reboot [--no-wait] [--timeout S] [--no-logs]`, `wait` | sync and reboot, wait for sshd, report the boot time and fetch the logs |

Kernel and initrd always go through the boot entries, never through `push`.

**Logs are redacted.** The Wi-Fi passphrase can ride on the kernel command line (`wifi.psk=`, `iwlwifi.psk=`), which appears in the console log and `/proc/cmdline`. `remote.py logs` replaces its value with `<redacted>` in everything it saves. Logs fetched any other way (an `espctl klog` piped by hand, `read-usb-log.sh`) are not redacted; never paste them anywhere or commit them unchecked.

## Trial kernels

The stick's ESP holds up to three Limine entries:

| Entry | What | When it boots |
|---|---|---|
| `LeandrOS trial <id>` | the last deployed kernel | once: `espctl boot` in init sets the default back to the fallback as soon as the trial reaches init |
| `LeandrOS good <id>` | the last promoted kernel | the default otherwise |
| `LeandrOS factory` | the flashed `/kernel.elf` | the fallback when there is no good entry; never modified |

```sh
./scripts/build-all.sh --arch x86_64                 # release; LEANDROS_KERNEL_TAG=<label> names the build
python3 scripts/remote.py deploy-kernel --reboot     # upload, verify, arm, reboot, check, fetch logs
python3 scripts/remote.py promote                    # only after the new kernel proved itself
```

`deploy-kernel` uploads `target/final-<arch>/kernel` and the initrd with sha256 checks; `espctl install` copies them to `/leandros/k/<id>/` on the ESP, reads them back from the medium, and only then rewrites `limine.conf` (through a shadow copy, so a power cut leaves a complete config) with the trial entry as the default. With `--reboot` it waits for sshd and prints `NEW KERNEL RUNNING: trial <id>` and `uname -v`, or says that the trial is not running. Each entry's command line carries `leandros.slot=` and `leandros.kid=`, so `status` always says what booted.

`--cmdline 'ARGS'` sets the trial entry's extra kernel arguments (default: the good entry's). Deploying the same build again with another `--cmdline` is how a command-line knob is A/B tested.

### The network watchdog: `netwatch=`

A laptop reachable only over Wi-Fi is lost when a trial kernel breaks the network but otherwise boots, because the trial reached init and nobody can reach it to reboot. `deploy-kernel --netwatch S` appends `netwatch=S` to the **trial entry's** command line only. The kernel then starts a `netwatch` thread ([`kernel/src/netwatch.rs`](../../kernel/src/netwatch.rs)) that checks once a second whether any interface has an IPv4 address. If none has one `S` seconds after boot, it prints `[netwatch] no network address after S s — rebooting to the fallback entry` and reboots through the normal `reboot(2)` restart path (filesystems synced, the console log flushed to the log partition, devices quiesced). Since the trial has already disarmed itself, the machine comes back on the good entry, and the line is in the previous boot's log (`remote.py logs`).

- `netwatch.loss=S` does the same after an address had been seen once, when every interface has been without one for `S` seconds in a row. The net daemon drops an address after a 4 s carrier grace, so this also catches a lost link.
- Both are off by default. Choose `S` generously (60–120 s on Wi-Fi): a slow DHCP exchange looks the same as a broken network.
- **`promote` removes `netwatch=` from the good entry.** Promoting copies the trial's command line, and a good kernel must never reboot itself for a network outage; `remote.py promote` (and `deploy-kernel --promote`) strips the knob afterwards with `espctl cmdline good …`.
- `netwatchtest.py <arch>` in the run-leandros skill checks it in QEMU: a boot with no NIC (`LEANDROS_NIC=none`) must print the line and reset; a boot with a NIC must stay up.

### When a trial does not come back

`deploy-kernel --reboot` and `reboot` wait 300 s by default and then say what to do:

1. The trial reached init: it already set the default back to the fallback, so a power cycle boots the good kernel. With `netwatch=` this happens by itself when the network is what broke.
2. The trial died before init (or Limine refused it): a power cycle boots it again. Pick `LeandrOS good <id>` (or `factory`) in the Limine menu, which shows for 3 s whenever there is more than one entry. That boot's `espctl boot` sees that the trial was skipped and disarms it.
3. Once the machine is up: `remote.py logs` (the failed boot is in `klog-all.txt` if it got far enough to log), then `remote.py discard`. Never promote a kernel that did not boot.
4. Last resort: re-flash the stick (`scripts/write-usb.sh x86_64 /dev/diskN`, which keeps `/home`), or read its log partition on the Mac with `scripts/read-usb-log.sh /dev/rdiskN --all`.

For a kernel-only A/B, build only the kernel and reuse the initrd already on the ESP (`remote.py espctl get /leandros/k/<good>/initrd.gz <file>`, then `remote.py pull`). A full `build-all.sh` in a fresh worktree needs about 6 GB of disk.

## Starting the desktop remotely

From a root SSH login, a COSMIC session can be started without touching the machine by speaking greetd's IPC protocol to the running greeter's greetd ([../desktop/login.md](../desktop/login.md)). The socket is in the greeter account's runtime directory (`/run/user/990/greetd-<pid>.sock`, the value of `GREETD_SOCK`). Each message is a native-endian `u32` length followed by JSON:

1. `{"type":"create_session","username":"leandro"}` — answered with an `auth_message`;
2. `{"type":"post_auth_message_response","response":"<password>"}` — answered with `success`;
3. `{"type":"start_session","cmd":["/bin/sh","/bin/start-cosmic-leandros"],"env":["XDG_SESSION_TYPE=wayland","XDG_CURRENT_DESKTOP=COSMIC","XDG_SESSION_DESKTOP=COSMIC"]}`.

greetd then ends the greeter and starts the session as the user, exactly as a login at the keyboard does. Send `{"type":"cancel_session"}` first if a previous attempt left a session half-created. Pass the password to the client at run time (an environment variable or a prompt); never write it into a script, a log or a commit.

Once the session is up, programs can be started inside it from the root shell with `su` or `sudo` ([../getting-started/logging-in.md](../getting-started/logging-in.md#switching-user-su-and-sudo)), for example Firefox as `leandro` with `WAYLAND_DISPLAY=wayland-1`.

## Measuring on the laptop

- **The panel state matters; check it and set it.** After about 15 minutes without input, cosmic-idle blanks the screen (zwlr output power Off). Since lane blankplay (ports/cosmic-comp 0002/0003, ports/cosmic-idle 0001), clients keep getting frame callbacks at about 1/s while the output is off, so a Firefox started over SSH loads and plays YouTube with sound, and its screensaver inhibitor no longer turns the panel back on. Firefox then treats its window as hidden: video decode (RDD) drops and only audio and the network keep going, so compare CPU numbers only under the same screen state. `/bin/blanktest --status` (in the session: `XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1`) prints the outputs' power mode, and `--off` and `--on` switch it remotely. The i915 `flips` counter in `/proc/i915` and the `flips N/s` console lines show whether the panel was being updated. Before those patches, a client started with the panel blank waited forever for its first frame callback: ~0 % CPU and a few KiB on the radio, which looked exactly like a hang (lane-ffhang).
- **What "playing" looks like:** the RDD process above 5 %, `pipewire-pulse` and `leandros-snd-sink` at 3–7 % each, and Wi-Fi receive traffic of megabytes per minute (`/proc/net/dev`).
- **Where the CPU goes:** `/proc/<pid>/stat` now splits user and in-syscall time, and `/proc/sysstat`, `/proc/rqhold`, `/proc/wakestat` and `/proc/lockstat` show the kernel side ([debugging.md](debugging.md#procfs-counters)). Diff two reads over a fixed window. `ps -e -o pid,comm,args` and `ps -T` name the processes and threads.
- **Wi-Fi state:** `cat /proc/net/nic` gives the `wifi0` association (`ht:`), Block Ack (`rx_ba:`), the AP's rates and retries (`rx_quality:`), our TX rate control (`tx_rate:`) and TCP loss recovery (`tcp_recovery:`); see [../hardware/drivers.md](../hardware/drivers.md#wi-fi-qos-ht-and-block-ack).

## See also

- [Workflow](workflow.md) — machines, lanes, the QEMU driver.
- [Debugging](debugging.md) — reading console logs and kernel counters.
- [../hardware/pc.md](../hardware/pc.md) — the USB stick, its log partition, real-PC drivers.
- [../servers/netctl.md](../servers/netctl.md) — Wi-Fi profiles and credentials.
