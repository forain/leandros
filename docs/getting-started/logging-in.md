# Logging in

A default boot offers a graphical login on the display, a `login:` prompt on the serial console, and text logins on virtual terminals 2–6. All of them authenticate the same accounts against `/etc/shadow`.

## Accounts

Two accounts are provisioned by the image builder, usable from every login:

| User | Password | uid | Home | Shell |
|---|---|---|---|---|
| `root` | `root` | 0 | `/root` | `/bin/brush` |
| `leandro` | `leandro` | 1000 | `/home/leandro` | `/bin/brush` |

Passwords are stored as SHA-256 crypt entries (`$sha256$<salt>$<hex>`). Non-root sessions are fully supported, including a complete COSMIC desktop as `leandro`. Some regression suites exercise root-only behaviour and must be run as root; see [testing](../development/testing.md). Account and credential handling is described in [security](../servers/security.md).

## The graphical login (default)

PID 1 ([`userland/init`](../../userland/init/src/main.rs)) supervises the graphical login chain: `greetd` drives `cosmic-comp` in kiosk mode with `cosmic-greeter` as its client on VT 1. The greeter runs unprivileged, as the system account `cosmic-greeter` (uid 990). A correct password starts the COSMIC session for that user; a wrong one leaves the greeter up. When the session ends, init restarts the greeter.

Things worth knowing:

- **No GPU, no greeter.** COSMIC renders on the host GPU only. If the VM has no GL-capable virtio-gpu (for example `--no-gpu`, or Homebrew's QEMU on a Mac), the chain exits with status 78 and init prints, once:

  ```
  ## NO GPU RENDERER: the graphical login (COSMIC) was NOT started.
  ```

  The serial and VT logins are unaffected. See [Running](running.md#gpu-paths) for how to get a GPU path; software rendering is opt-in with `touch /etc/leandros/allow-software-render`.
- **Logs.** The chain's stdout and stderr — `cosmic-comp`, `cosmic-greeter`, `cosmic-session` and every applet — go to `/var/log/greetd.log`, rotated at 8 MiB to `/var/log/greetd.log.1`. They do not go to the console on purpose: the framebuffer console is what the compositor draws over, and a panel spinning on a dead Wayland connection once wrote over a gigabyte of log before the guest died.
- **Respawn back-off.** A chain that dies quickly is restarted after 3, 6, 12, 24, then 30 s, and given up on after 20 consecutive short runs; one that ran for at least a minute is restarted promptly. When the chain dies, init also kills its orphaned clients, which would otherwise spin on a broken Wayland socket at a full CPU each.
- **Memory guard.** When available memory stays below a floor (the larger of 96 MiB and a tenth of RAM), init kills the largest process on the graphical side, as `systemd-oomd` would. Firefox therefore needs the default 4 GiB of guest RAM.
- **Runtime directories.** The VFS seeds `/run/user/0` at boot so the greeter and root sessions have a runtime directory from the first syscall; each login session gets its own `/run/user/<uid>` (mode 0700) created the same way. Console logins set `XDG_RUNTIME_DIR` too.

### Testing the greeter alone

`sh /bin/greeter-fake` runs the same greeter against upstream's `fakegreet` protocol harness, with no daemon, no PAM and no session worker in the way — the smaller thing to reach for when the greeter itself is what is under test.

### Why it is arranged this way

The greeter's role comes from its *account*, not its file name; greetd must run with `vt = "none"`; its control socket cannot live in `/run` (F2FS has no socket nodes); there is no PAM stack, so `ports/greetd/pam-leandros` supplies the libpam ABI; and `cosmic-comp` itself still runs as root. These constraints, and the reasons for each, are in [the desktop login design](../desktop/login.md).

## Opting out: text login only

Touch the marker and reboot to boot to the text logins only — init checks for it before starting the greetd chain:

```sh
touch /etc/leandros/text-login     # persistent: it lives on the root F2FS volume
rm /etc/leandros/text-login        # back to the graphical login
```

To stop a running greeter without rebooting (for example so a test can take DRM master):

```sh
touch /etc/leandros/text-login
kill $(cat /run/greetd-init.pid)
```

With the marker present init does not restart it. Remember to remove the marker afterwards; the image is persistent.

## The serial console

The terminal `run-qemu.sh` was started from is the guest's serial console. Init runs a getty loop on it independently of the greeter, respawning `login` every time a session exits. It is what the test harnesses talk to, and the recovery path when the graphical stack is broken — the same arrangement a Linux machine has with a display manager on the seat and `agetty` on `ttyS0`.

Typing **Ctrl-T** on the serial console (not the framebuffer) prints a task dump — every task's state, CPU, blocked port or futex, user registers, plus allocator and socket census lines — even with no shell in the foreground. See [debugging](../development/debugging.md#ctrl-t-task-dump).

## Virtual terminals

Six virtual terminals exist. VT 1 carries the graphical login and the COSMIC session; VTs 2–6 each run a text login (`/dev/tty2`…`/dev/tty6`, `TERM=linux`), respawned when the session ends. **Ctrl+Alt+F1…F6** switches between them from anywhere, including from inside COSMIC. A text login that cannot start is retried a few times and then left alone rather than becoming a fork storm. See [tty and VT](../servers/tty-and-vt.md).

## Shutting down

`poweroff`, `reboot` and `halt` (one binary, chosen by its name) ask init for an orderly shutdown: every process gets SIGTERM, then SIGKILL, the file systems are synced and remounted read-only, then `reboot(2)`. The COSMIC power menu's Shut Down and Restart go through the same path. Under `run-qemu.sh` a power-off ends QEMU and a reboot restarts the VM through the firmware; the next boot finds both volumes cleanly unmounted.

## See also

- [Desktop login design](../desktop/login.md) — greetd, `cosmic-greeter`, `greeter-launch`, `pam-leandros`.
- [Desktop session](../desktop/session.md) — what starts after login.
- [Security](../servers/security.md) — accounts, `/etc/shadow`, privilege drops.
- [Programs](../userland/programs.md) — `init`, `login`, `greeter-launch`.
