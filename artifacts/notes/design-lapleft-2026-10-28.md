# design lapleft 2026-10-28: what is left on the laptop (greeter, webcam, S3 follow-ons, display, HDMI audio, touchpad)

Machine: HP EliteBook 840 G5. good = 2d3e00b630f2 (main 046ddea2). Laptop rules: $SP/LAPTOP_DRIVERS.md + LANE_PROTOCOL.md
(one lock hold per deploy→measure→restore, INTEL=1 release, `--append` knobs only, never bare `espctl cmdline`, never factory,
never promote: the orchestrator promotes). Lanes test aarch64 + x86_64 QEMU (laptop-only drivers), then the laptop.
Every lane: Q0 inert (both arches boot without knobs, regress unchanged), short note `artifacts/notes/lane-<lane>-2026-10-28.md`.

Lanes that need NO user: **lapgreet, lapcam3, lapdispB, lapdispC2a, lapptp4, s3cpus (after s3core)**, and the QEMU/inert
halves of s3xhci/s3nvme/s3i915/s3iwlhda/s3policy. User-dependent: S3 H4+ (someone at the laptop), every [MONITOR] step,
HDMI audio listening, touchpad finger test.

---------------------------------------------------------------------------------------------------
## 1. lapgreet: the first greetd login after boot sometimes fails, the retry works

### Evidence gathered (read-only, 2026-10-28)
- `/var/log/greetd.log` (3.1 MB) and `greetd.log.1` (8.4 MB) each hold ONE boot, and neither contains a user login:
  only `profile: session starting, uid=0 ... /run/user/990` (the greeter session). init keeps only one older
  generation (userland/init/src/main.rs:456 `DM_LOG`, :458 `DM_LOG_OLD`), so every failed-login boot so far has been
  rotated away. **The cause cannot be proven from the existing logs. This lane is diagnose-first.**
- 99 % of the log volume is `ERROR cosmic_greeter::time: No zbus system connection.` (369×) plus anyhow backtraces
  (290× 20 lines). That noise is what pushes the useful lines out.
- greetd itself logs nothing about auth or session results, and its worker discards the session exit status
  (ports/greetd/data/profile:171-177 says so). pam-leandros (ports/greetd/pam-leandros/src/lib.rs) logs nothing.
- The greeter runs `cosmic-comp (root) → greeter-launch → sh dbus-run-session → busd + cosmic-greeter-login`
  (userland/greeter-launch/src/main.rs:51-54, :443-506). The user session is
  `start-cosmic-leandros → dbus-run-session → cosmic-session → cosmic-comp` (artifacts/m6-session-data/start-cosmic-leandros:231/233,
  `set -eu` at the top). greetd.conf: `vt="none"`, `switch=false`, `source_profile=true`.

### Ranked hypotheses (each with the log line that confirms or kills it)
- **H-A: DRM master hand-over race.** greetd starts the user session as soon as the greeter *process* returns, but the
  greeter compositor's card0 open file (or a dup inherited by `busd`/`dbus-run-session`, which outlive cosmic-comp by
  a moment) still holds master. The user cosmic-comp's SET_MASTER gets EBUSY (the code at
  drivers/src/drm_device_interface.rs:5854 documents exactly this EBUSY shape), cosmic-comp exits, cosmic-session exits,
  greetd restarts the greeter. A retry seconds later works because the holder is gone. Fits "first try fails, retry works".
  Confirm: new kernel line `drm: SET_MASTER EBUSY open_id N pid P (holder open_id M pid Q comm C)` at the failed login.
- **H-B: user session prerequisites not ready on a fast first login.** /home is the persistent `lx` volume (lane homevol),
  `/run/user/1000`, or pipewire/dbus-run-session start-up; `set -eu` in start-cosmic makes any failing step exit.
  Confirm: start-cosmic stderr shows the failing command, worker exit status non-zero within <3 s.
- **H-C: greeter-side race.** cosmic-greeter loads sessions/users asynchronously; `failed to load user data from daemon`
  is logged at every boot. A submit before the session list is loaded can call `start_session` with an empty/wrong
  command. Confirm: greetd IPC trace shows `start_session cmd=[]` or an error response to the greeter.
- **H-D: auth path.** pam_authenticate first call fails (cold /etc/shadow read, conversation ordering). Least likely
  (lane greetdfix fixed the only known one). Confirm: pam line `auth user=leandro rc=7`.

### Design (instrumentation; no greetd/PAM config change, no password/shadow access)
Owned files: userland/init/src/main.rs (log rotation only), ports/greetd/patches/0004-trace-leandros.patch (new),
ports/greetd/pam-leandros/src/lib.rs (one log line), artifacts/m6-session-data/start-cosmic-leandros (trace lines),
ports/greetd/data/greeter-env (RUST_LOG filter only), drivers/src/drm_device_interface.rs (one log line near
`master_gate`/SET_MASTER handling, :5920-5995). Rebuild greetd with ports/greetd/build.sh (the `_check_greetd_fresh`
guard in scripts/mkfs-f2fs-populated.py must stay green).
1. init: keep 4 generations (`greetd.log.1..3`), rotate at boot only, and cap each at 4 MB by truncating from the
   front at rotation. Write a `=== boot <uname tag> <mono s> ===` header line first.
2. greeter-env: `RUST_LOG=info,cosmic_greeter::time=off` for the greeter only (kills 369 lines/boot). Not a greetd config change.
3. pam-leandros `pam_authenticate`: one stderr line `pam-leandros: auth user=<name> rc=<n> in <ms> ms` (never the
   secret, never the hash).
4. greetd patch 0004 (logging only): on each IPC request log `greetd: ipc <type>` (+ `cmd=<argv0>` for start_session,
   never the response text of a secret prompt); in `session/worker.rs` after `waitpid`, log
   `greetd: session uid=<u> exited status=<s> after <ms> ms`; log when the greeter exits and when the pending session is started
   (`greetd: greeter exited after start_session; starting session in <ms> ms`).
5. start-cosmic-leandros: `echo "start-cosmic: <mono> step <name>" >&2` at HOME check, dbus-run-session, cosmic-session exec;
   on the non-exec path print `$?`.
6. Kernel: the SET_MASTER/EBUSY line from H-A, rate-limited 8/boot; also `drm: master released open_id N (last close, pid P)`.

### Fix (chosen after the trace; pre-designed so the lane can finish in one go)
- H-A: Linux semantics are master-per-open-file released at last close. Make sure the greeter's card0 fd is not
  inherited: check that cosmic-comp's card0 open has O_CLOEXEC on our side (VFS open flag honoured on exec) and that
  `dbus-run-session`/`busd` close inherited fds; if the holder is a zombie window (fd table freed late at exit), release
  master in the exit path before the parent's wait returns. Do NOT add a "steal master" rule.
- H-B: start-cosmic waits up to 5 s (100 ms steps) for `$HOME` to be a directory and `/run/user/$uid` to exist,
  then continues (logs the wait). Script change only.
- H-C: patch cosmic-greeter to keep Submit disabled until the session list is loaded (or default to cosmic.desktop
  `Exec=/bin/sh /bin/start-cosmic-leandros` when the list is empty).
- H-D: retry pam_authenticate once internally on EIO/ENOENT reading /etc/shadow (not on a wrong password).

### Test plan
- QEMU x86_64 `--gpu virgl` (greeter path): boot, wait for `GREETER-LAUNCH: dropped to uid 990`, run
  `/bin/greetd-login` as leandro immediately (0 s), then log out and repeat at +0 s ten times; repeat with a 1 s
  delay. Look for `greetd: session uid=1000 exited` within 5 s.
- Laptop (lock held): 4 trial boots. On each: wait for the greeter line in greetd.log, then over SSH run greetd-login as
  leandro at once (lanes are authorized for leandro/leandro; creds from $SP/laptop-login.env at use time, never printed).
  Pull all greetd.log generations + klog. Restore good.
- Pass: 4/4 first logins reach `cosmic-comp` running as uid 1000 for >30 s; trace lines present; aarch64 regress PASS.

### Expected log lines
```
=== boot 046ddea2 12.3 ===
pam-leandros: auth user=leandro rc=0 in 41 ms
greetd: ipc start_session cmd=/bin/sh
greetd: greeter exited after start_session; starting session in 180 ms
drm: master released open_id 7 (last close, pid 214)           (H-A healthy order)
start-cosmic: 13.9 step home ok / dbus-run-session / exec cosmic-session
```
Failure shape to catch: `drm: SET_MASTER EBUSY ...` or `greetd: session uid=1000 exited status=1 after 900 ms`.

### User steps
None required. Optional: when the user next sees a failed first login, note the clock time; the 4 kept generations
then contain it.

---------------------------------------------------------------------------------------------------
## 2. lapcam3: webcam ~10 fps in room light

### Facts
- drivers/usb/src/uvc.rs:95 `parse` reads only the VC header (:107-108) and the VS formats; **no Camera Terminal or
  Processing Unit is parsed, so no CT control is ever touched**: the device keeps its defaults. HP's IR/RGB modules
  default to AE mode = aperture priority (8) with **CT_AE_PRIORITY = 1 (frame rate may vary)**: in room light the
  exposure grows to ~100 ms → 10 fps. lapcam2 measured 30/30 frames, 100 ms spacing, no drops → the device, not the
  transport, is slow. That matches AE priority, not isoch bandwidth.
- :166 `negotiate`: bmHint=1, interval = the frame descriptor's default interval (:267 `fr.interval`); the committed
  interval is logged at :250 (`[UVC] slot .. interval ..`). Verify it reads 333333. Bandwidth: :248 picks the smallest
  alt with bytes ≥ dwMaxPayloadTransferSize; also logged.
- kernel/src/v4l2.rs:403 G/S_PARM exists; check that S_PARM's timeperframe actually re-negotiates (else it is a no-op).
- The current laptop dmesg holds no `[UVC]` lines (ring wrapped or not logged this boot); the lane must capture them.

### Design
Owned files: drivers/usb/src/uvc.rs, kernel/src/v4l2.rs (controls + S_PARM), userland/camtest/camtest.c.
1. uvc.rs parse: in the VC interface (`cur_sub == 0x01`, `0x24` descriptors), record
   - INPUT_TERMINAL (d[2]==0x02) with wTerminalType 0x0201 (camera): `ct_id = d[3]`, `ct_ctrls = LE(d[15..15+d[14]])`
   - PROCESSING_UNIT (d[2]==0x05): `pu_id = d[3]`, `pu_ctrls`
   into `VideoInfo { ct_id, ct_ctrls, pu_id, pu_ctrls }`.
2. Control helpers: `ctrl_get(f, v, unit, sel, req, buf)` / `ctrl_set` with bmRequestType 0xA1/0x21, bRequest
   GET_CUR 0x81 / GET_MIN 0x82 / GET_MAX 0x83 / GET_RES 0x84 / GET_INFO 0x86 / GET_DEF 0x87 / SET_CUR 0x01,
   wValue = sel<<8, wIndex = unit<<8 | vc_iface. Selectors: CT_AE_MODE 0x02 (1 B bitmap 1 manual, 2 auto, 4 shutter,
   8 aperture), CT_AE_PRIORITY 0x03 (1 B), CT_EXPOSURE_TIME_ABSOLUTE 0x04 (4 B, 100 µs units). bmControls bits D1/D2/D3.
   Check GET_INFO bit1 (SET supported) before SET_CUR; never fail the stream on a control error (log once).
3. Stream::start (:238), after `negotiate` and before `IsochPipe::open_ex`: apply knobs
   - `uvc.ae_prio=0|1|dev` (default **0** = constant frame rate, what Windows camera apps and WebRTC ask for)
   - `uvc.exposure=auto|manual:<100us units>` (default auto: AE mode left as device default)
   then GET_CUR read-back and log.
4. V4L2 (kernel/src/v4l2.rs): QUERYCTRL/G_CTRL/S_CTRL for V4L2_CID_EXPOSURE_AUTO (0x009a0901),
   EXPOSURE_ABSOLUTE (0x009a0902), EXPOSURE_AUTO_PRIORITY (0x009a0903) mapped onto 2.; S_PARM picks the nearest
   discrete interval and re-negotiates when not streaming.
5. camtest: `--fps` prints mean/max frame spacing from `ts_ns`; `--ae-prio N`, `--exposure N` via S_CTRL.

### Verification that does not depend on light
- T1 (control path): `camtest --exposure 1000 --ae-prio 1 90` (manual 100 ms) → 9–11 fps; then
  `camtest --exposure 1000 --ae-prio 0 90` → camera must either clamp exposure to ≤33 ms and give 29–31 fps or
  report the conflict; then `--exposure 50 --ae-prio 0` (5 ms) → **29–31 fps, spacing 33.3 ± 2 ms**. Manual exposure makes
  the result independent of the room.
- T2 (default): plain `camtest --fps 90` with default knobs → read-back line shows `ae_prio 0`, ≥ 28 fps. Also with
  the lens covered by a finger (darkest case) → still ≥ 28 fps (that is the actual user bug).
- T3: `uvc.fake=1` QEMU x86_64 + aarch64: controls absent → logs `no camera terminal`, stream PASS unchanged.
- Laptop: one lock hold, trial with no extra knobs; camtest pushed separately (deploy-kernel swaps only kernel+initrd).

### Expected log lines
```
[UVC] slot 5: camera terminal 1 controls 0x00000a0e, processing unit 2 controls 0x0000177f
[UVC] slot 5: format 1 frame 1 (640x480) interval 333333 max_frame 614400 max_payload 3072 -> alt 6 (3072 B/interval)
[UVC] slot 5: ae_mode 8 (aperture) ae_prio 1 -> 0, exposure_abs 156 (min 3 max 2047)
camtest: 90/90 frames, 29.97 fps, spacing mean 33.4 ms max 35.0 ms
```
### User steps
None. Optional: confirm the picture in Firefox/Cheese looks right (darker but smooth in dim light is expected with ae_prio 0).

---------------------------------------------------------------------------------------------------
## 3. S3 follow-ons after s3core (from design-lapsusp2 §2/§8, amended by review-s3aml-2026-10-28)

### Constraints that every S3 lane inherits (from the review and s3aml2)
- R1 `_WAK(3)` runs ONLY after a real S3 (resumed through the FACS vector or PM1_STS.WAK_STS set): SMI 0xEA91 resets the
  laptop otherwise (proved by s3aml2: dropping it via `pm.aml_smi_drop=0xEA91` stopped the reset).
- R2 Commit forward after `_PTS(3)`: nothing may fail after `_PTS`; a late failure arms the RTC +2 s and enters S3 anyway.
  So every device lane's **suspend** hook runs before `_PTS` and must not fail late; device resume failures are logged
  and never re-enter AML.
- R3 `pm_test` levels `platform|processors|core` never run `_PTS/_WAK` on this laptop (they would call _WAK while awake).
  The old H2 is replaced by **H2′ = `_PTS` → real S3 with RTC +2 s → `_WAK`** (s3core). Lanes below never use H2.
  If s3core did not implement the auto-skip, run those levels with `pm.s3_noaml=1`.
- R4 Freeze userspace and devices before `_PTS`; `with_ns` is refused while `pause_poll(true)` (s3aml2 F4);
  after resume drain SCI_EVT (`_Qxx`) before thaw.
- R5 Log on the boot after any reset: `[S3]` summary + SW-SMI ring (review item 6) if s3core added it.
- R6 Laptop conditions for real S3: AC, lid open, no external display, no USB except the stick, **a person present**
  (power-button recovery). Kernel-driven `pm.s3_selftest=20 pm.s3_selftest_after=reboot`; evidence = stick klog.
- R7 Every lane: `cfg(x86_64)` or inert without `pm.s3=1`; aarch64 regress + x86 Q0 unchanged.

### Order
s3core (running) → **s3cpus** → (**s3xhci** ∥ **s3nvme**) → **H4 gate (user present)** → **s3i915** (after lapdispB/C2a
merge, both touch i915/kms.rs + vblank.rs) → **s3iwlhda** (+LPSS i2c) → **s3policy** → H6 (user).
s3pci is DONE (drivers/src/pci_pm.rs, pm.rs DEVS last entry "pci").

### 3.1 s3cpus
Owns: arch/x86_64/src/pm_ap.rs (new: park/unpark asm, `AP_CTX`, `PARKED`), arch/x86_64/src/smp.rs (resume entry,
trampoline param reuse :149-156, :193-283, `sched_ap_entry` body :416-438 factored into `ap_percpu_init()`),
sched/src/lib.rs (ONE hook before idle `sti; hlt; cli` at ~:7596; ⚠ coordinate with the Pi lane that owns sched/epoll:
keep the hunk to 3 lines), kernel/src/pm.rs (freezer flag + AP park step), the return-to-user path (freezer check).
Design: lapsusp2 §1.4 verbatim. Additions: the park request is sent only after the freezer settled; unpark on timeout
(2 s) is the ONLY failure path and it is before `_PTS` (R2). TSC_ADJUST sync on resume; `ACTIVE_CPUS` untouched.
Remove s3core's "refuses unless 1 CPU online" (keep `nosmp` knob).
Tests: Q1 x86_64 TCG `-smp 4 pm.s3=1`, `pm_test=processors` ×20 (no AML per R3) and `pm_test=core` ×20; then QEMU real S3
(Q2) with 4 CPUs ×10 `--usb-stick` if s3core already does Q2. aarch64 regress. Laptop: `pm.s3=1` +
`echo processors > /sys/power/pm_test; echo mem > /sys/power/state` ×5 over SSH (no AML, no power loss = safe unattended).
Expected: `[S3] aps: 7 parked in 1 ms, INIT sent` / `[S3] aps: 7 resumed (tsc skew max N cycles)` / `[S3] freeze: N user threads frozen`.
User: none.

### 3.2 s3xhci (root stick: the critical one)
Owns: drivers/usb/src/xhci/mod.rs (:175 `Xhci::init` split into `hw_init()` reused by reinit; :254-261 scratchpad kept;
:276 DCBAAP, :277 CRCR, :281-283 ERST), drivers/usb/src/xhci/pm.rs (new: save/restore, CSS/CRS, persist),
drivers/src/usb_hcd.rs:674/:699 (pm hooks), usb_storage + usb_hid `reset_resume`, kernel/src/pm.rs DEVS line.
Not uvc.rs (lapcam3): the camera is simply removed+re-added on mismatch / re-enumerated.
Design: lapsusp2 §2 xHCI verbatim (restore path, reinit path with USB persist by descriptors + serial + capacity, UNIT
ATTENTION 06/29/00 retry ≤5, READ CAPACITY equal before the block gate opens; mismatch on the root → halt, no writes).
`pm.s3_xhci=auto|restore|reinit`. SPT-LP PME quirk (8086:9d2f) is phase 2.
Tests: Q2 `--usb-stick pm.s3=1` real S3 ×20 with `suspendtest --io-during` (Q3), both `restore` and `reinit`; disconnect
the stick in the QEMU monitor during S3 → expect the halt line, no f2fs write. aarch64 regress.
Laptop: **H4 (user present, R6)**: `pm.s3=1 pm.s3_selftest=20 pm.s3_beep=1 pm.s3_led=1 pm.s3_i915=skip pm.s3_flush_each=1`.
Expected: `[S3] resumed: wake PM1_STS 0x8400 (WAK RTC)`, `[PM] resume pci/xhci(reinit: 1 dev persisted, root LUN same
capacity)`, `[S3] klog flushed after resume (block online)`, `[S3] aml: _SST(2); _WAK(3) -> (0, 0)`, reboot after 15 s.
User: be at the laptop, watch power LED blink → beep ~20 s → Caps LED → reboot; if it hangs >2 min, hold power 4 s.

### 3.3 s3nvme (NVMe + e1000e)
Owns: drivers/src/nvme.rs (:1184 `init` → per-controller `bring_up(&Ctrl)`; :1238 `shutdown_all` reused), drivers/src/e1000e.rs
(:1694 init split, :2288/:2305 hooks, ULP disable / LANPHYPC toggle), DEVS lines.
Design: lapsusp2 §2 NVMe + e1000e verbatim (HMB Set Features 0x0D same descriptors, APST, same zeroed queue memory).
Tests: QEMU x86_64 with `-device nvme` real S3 ×20 + read-verify of a pattern file on the NVMe mount; e1000e: QEMU
`-device e1000e`, ping across S3, DHCP lease kept. Laptop: rides H4/H5 (needs user present): H5 from SSH over the
wired I219 (`nohup sh -c 'echo +20 > /sys/class/rtc/rtc0/wakealarm; echo mem > /sys/power/state; suspendtest --disk-after --net-after' &`).
Expected: `[PM] resume nvme: ctrl 0 RDY in N ms, HMB 0x.. re-enabled, 4 I/O queues`, `e1000e: ULP disabled, link up 1000 Mb/s`.
User: present for H5; plug Ethernet if Wi-Fi resume (s3iwlhda) is not in yet.

### 3.4 s3i915
Owns: drivers/src/i915/pm.rs (new), drivers/src/i915/mod.rs:286-287 hooks, small resume entry points in wa.rs, exec.rs
(engines RING_MODE/HWS_PGA/execlists), rps.rs, gtt (rewrite every GGTT PTE), kms.rs (resume through the existing modeset
path with saved state: cdclk → DPLL0 → eDP PPS T1-T3 → link train → pipe/planes → backlight). Starts after lapdispB +
lapdispC2a merge (they own vblank.rs/outputs.rs/power.rs/kms.rs DDB): it must also save/restore pipe B off-state and the
new power refcounts. `pm.s3_i915=skip|resume` (default resume once proven; skip during H4).
Tests: QEMU `i915.fake=2 i915.kms=1` exercises the save/restore register paths (fake MMIO) ×20; virgl desktop regress.
Laptop H6a (user present): drop `pm.s3_i915=skip`, selftest S3 with `pm.s3_selftest_after=stay`; evidence without
screenshots: `PIPE_FRMCOUNT_A` advancing, cosmic-comp flips completing, `kms: underruns 0`, iris `i915gltest` after resume PASS.
Expected: `[PM] resume i915: wa 31 regs, ggtt 262144 PTEs, rcs0/bcs0 alive, RC6 on, eDP PPS on in 210 ms, link 2x2.7 ok, pipe A on`.
User: present; confirm the panel lights after resume.

### 3.5 s3iwlhda (+ LPSS I2C)
Owns: drivers/src/iwlwifi.rs (:1297 init path reused as `fw_reload()`), drivers/src/hda.rs (controller CRST/CORB/RIRB on the
same memory + codec verb cache replay incl. lapaudio3's saved PIN_CTL/jack state), drivers/src/dw_i2c.rs + i2c_hid.rs
resume (LPSS priv 0x204 reset deassert, SCL timings, HID RESET + SET_POWER ON; on failure the existing ps2 fallback).
Design: lapsusp2 §2 iwlwifi/HDA verbatim; i915 resumes before hda (display codec needs the power well).
Tests: QEMU `--snd hda` S3 ×10 + `sndtest tone`; aarch64 regress. Laptop H5 over Wi-Fi (user present): SSH reconnects
≤15 s after wake, `sndtest record 3` PASS, `touchpad=i2c` boot → pad works after resume.
Expected: `iwlwifi: resume: ALIVE, NVM ok, assoc in 3.1 s`, `[HDA] resume: CORB/RIRB ok, 42 verbs replayed`, `touchpad: i2c resumed (CPD0)`.

### 3.6 s3policy
Owns: ports/sysbus/src/login1.rs (:327 `suspend`, CanSuspend, PrepareForSleep, delay inhibitors, lid evdev), userland/init/src/main.rs
(:118-188 initctl `suspend`, same authorization as poweroff), `/etc/leandros/logind.conf` (new, all `ignore`), suspendtest.
Design: lapsusp2 §4 verbatim; `pm.s3_require_rtc` stays ON (refuses with `suspend: bring-up mode, needs an RTC alarm`)
until H6 passes; 2 s post-resume lid debounce + `_LID` re-read; cosmic-comp's handle-lid-switch inhibitor honoured.
Tests: QEMU: `busctl call ... CanSuspend` → `na` without pm.s3, `yes` with; Suspend with an RTC alarm → PrepareForSleep
true/false; fake lid via the ACPI evdev test hook → no suspend while `ignore`. Laptop (user present): COSMIC menu → Suspend
(with `pm.s3_require_rtc=0` for that boot only), wake with the power button; then logind.conf `HandleLidSwitch=suspend`
set by the user, lid close → suspend, open → resume.
User: decides when `pm.s3_require_rtc=0` and the lid policy become defaults.

---------------------------------------------------------------------------------------------------
## 4. Display: what needs no monitor

| Step (design-lapdisp2) | No monitor? | How it is verified without one |
|---|---|---|
| B 1-4 HPD IRQ, ISR, kthread, cache-only info | YES | SDEIMR/SDEIER readback logged; HPD irq count stays 0 unplugged; 2 s SDEISR backstop sees nothing; idle wakes ±1/s vs good |
| B 5-7 uevent record, libudev shim, drmsmoke --hotplug, fake-HPD ioctl 0x100B | YES | QEMU x86_64 + aarch64 + laptop: fake HPD → record → cosmic-comp rescan in its log; input hotplug still works |
| B tests 3-4 (real plug, cosmic scan) | NO | [MONITOR] |
| C2 prerequisites: power.rs refcount (PW2, DDI_x_IO) | YES | laptop: get/put → state bits readback, refcount balance logged |
| C2 DDB/WM proportional split | YES | host unit tests vs Linux values; **laptop: `i915.ddb_test=1` shrinks pipe A to [0,446) for 10 s and restores, eDP underruns must stay 0** (the main C2 risk, testable on the panel alone) |
| C2 WRPLL calc, HDMI buf-trans table | YES | host unit tests vs Linux for 148500/74250/108000/65000/25175 kHz |
| C2 DPLL1 program + lock | YES | laptop `i915.pipeb_test=pll`: program DPLL1 for 148.5 MHz, wait lock, log lock time, disable (no DDI mapping) |
| C2 full pipe-B sequence | code YES, lighting NO | `i915.pipeb_test=dryrun` logs the exact register writes; QEMU fake compiles/skips; real light-up is [MONITOR] |
| C2 KmsPipe2 wiring (C1 hooks) to real pipe B | code YES | QEMU i915crtc2 suite still PASS (stub); real is [MONITOR] |
| C3 M/N, DP buf-trans, link-training state machine | YES | host tests against a simulated DPCD sink (CR fail → fallback rate/lanes; EQ fail; TPS3) |
| C3 AUX training on a sink, C4 everything, A check (3) | NO | [MONITOR] |

### 4.1 Lane lapdispB (no monitor)
Owns: drivers/src/i915/vblank.rs (SDEIMR/SDEIER in init; ISR :475-482 sets `outputs::HPD_PENDING`; IRQ_IDLE_STREAK :487),
drivers/src/i915/outputs.rs (`HPD_PENDING`, `reprobe(port)`, `info()` cache-only; first probe at init :156 kept; kthread
`i915-hpd`), servers/evdev/src/lib.rs (~:2192/:2238 `hp_drm_change`), ports/input-stack/shims/libudev/libudev.c (:1165, :1207,
:220; subsystem filtering), userland/drmsmoke (`--hotplug`), drivers/src/drm_device_interface.rs (root-only ioctl 0x100B).
Design: design-lapdisp2 Lane B steps 1-7 verbatim. Knobs `i915.hpd=0`, `i915.hpd_debounce_ms=100`. Kthread polls only while
extout is on, ≤ 0.5 wake/s (2 s backstop), event-driven otherwise.
Tests: QEMU x86_64 `i915.fake=2 i915.kms=1 i915.extout=1`: `drmsmoke --hotplug &` + fake HPD → record in <1 s; evtest2
hotplug unchanged; `LIBUDEV_TRACE=1` shows `action=change syspath=/sys/class/drm/card0`. aarch64 regress (virtio: no records).
Laptop (no monitor): `i915.extout=1` trial: `outputs: HPD irq enabled (SDEIMR .. SDEIER ..), kthread i915-hpd`, 0 HPD irqs
in 10 min, idle wake census ±1/s vs good; fake HPD over SSH → cosmic-comp log shows a connector rescan, desktop unaffected;
`drmsmoke --list` ×100 < 1 ms each.
User: none ([MONITOR] tests 3-4 later, bundled with C2).

### 4.2 Lane lapdispC2a (no monitor; after or parallel to B, disjoint files except kms.rs)
Owns: drivers/src/i915/power.rs (new; `outputs::with_pw2` switched to it), drivers/src/i915/pipeb.rs (new, sequence behind
`i915.pipeb_test=pll|dryrun|hdmi1|hdmi2`, the last two only light with a connected monitor), drivers/src/i915/wrpll.rs +
buf_trans.rs (new, pure, host-tested; tables COPIED from Linux skl_ddi_calculate_wrpll / _skl_trans_hdmi / _kbl_u_trans_dp),
drivers/src/i915/kms.rs (:96 `skl_ddb_plan`, :113 `ddb_setup` ranges; :122 refusal replaced by the proportional split;
WM :~703 per pipe), cursor.rs:260 (pipe parameter), new `i915.ddb_test=1`.
Design: design-lapdisp2 C2 "Prerequisites" + steps 1-4 + the DDB transition rule (shrink A, wait A vblank, only then B).
Cross-check every offset against hw.rs:400-442 first.
Tests: host `cargo test` for wrpll/buf_trans/ddb; QEMU fake: compiled, `pipeb_test` skipped, i915crtc2 + drm PASS; aarch64 regress.
Laptop (no monitor): `i915.pipeb_test=pll` → `pipeb: DPLL1 148500 kHz dco 8910000 locked in N us, disabled`;
`i915.ddb_test=1` → `kms: ddb test A [0,446) 10 s underruns 0, restored [0,892)`; `i915.pipeb_test=dryrun` → full register
plan logged, nothing written beyond DPLL; power refcount balanced (`power: PW2 refs 0 at end`).
User: none. Then C2b = actual light-up [MONITOR], C3 (training state machine can be host-tested earlier as C3a), C4.

### 4.3 Steps that strictly need the user's monitor
lapdispA check (3) + default flip; B tests 3-4; C2 colour bars at 1080p/720p/4K, unplug during test; C3 every AUX/link-
training step (incl. which jack is DDI-B vs DDI-C, USB-C alt-mode); C4 all (extend, mirror, 5× replug, YouTube 5 min);
HDMI audio listening (§5). Bundle them into ONE user session: HDMI monitor + (optional) USB-C DP adapter, ~45 min.

---------------------------------------------------------------------------------------------------
## 5. HDMI audio (after C2b lights HDMI): minimal plan, lane lapaudiohdmi
Owns: drivers/src/i915/audio.rs (new), pipeb.rs (mode HDMI instead of DVI + AVI infoframe), drivers/src/hda.rs (display
codec playback), drivers/src/snd.rs (second sink device), ports/pipewire (second sink node).
1. i915 (Linux intel_audio.c hsw_audio_codec_enable, gen9): TRANS_DDI_FUNC_CTL_B mode HDMI (0<<24) instead of DVI;
   AVI infoframe via VIDEO_DIP_CTL(trans B) + AVI data regs; audio needs PW2 (power.rs get). Program
   HSW_AUD_PIN_ELD_CP_VLD 0x650C0 (clear ELD_VALID for transcoder B, set AUDIO_OUTPUT_ENABLE), write the ELD built from the
   EDID CEA block into HSW_AUD_EDID_DATA(pipe) after resetting the address in HSW_AUD_DIP_ELD_CTRL(pipe), set ELD_VALID;
   HSW_AUD_CFG(pipe): pixel-clock index for the mode, N/CTS automatic. All offsets copied from Linux i915_reg/intel_audio_regs.h
   and checked against the PRM.
2. HDA display codec (8086:280b, already probed with `no_fallback`): the Intel vendor verb to enable all pins/converters
   (Linux `intel_haswell_enable_all_pins`), pin for port B (NID 0x05; port C 0x06), read ELD via GET_HDMI_ELDD after the
   unsolicited presence/ELD-valid event, converter stream/channel + format (48 kHz S16 2ch), pin ctl OUT_EN, audio infoframe
   via DIP index/data/xmit verbs, channel count = 2.
3. snd.rs: a second playback device (`/dev/snd/pcmC0D3p`-equivalent the leandros pipewire module can open) only while ELD valid;
   pipewire node `leandros_hdmi` (Audio/Sink); hot-unplug → ENODEV → node exits (same pattern as leandros-snd-source).
Tests: no monitor: pins enumerate, "no ELD" path, register dry-run; QEMU unchanged. [MONITOR+speakers]: `sndtest tone --dev hdmi`
audible 5 s, then pavucontrol/cosmic sound picks the HDMI sink, YouTube audio on the TV.
Expected: `i915 audio: pipe B HDMI ELD 0x.. bytes, ELD_VALID, cfg 0x..`, `[HDA] display codec: pin 05 ELD 'DELL U2415' 2ch 48k, stream 5`.

---------------------------------------------------------------------------------------------------
## 6. Touchpad default `touchpad=auto`

Today: i2c_hid.rs:336-337 parses `auto` and logs `not enabled yet, using ps2`; default `ps2`. Active polling is a
constant 8 ms (:27 `POLL_ACTIVE_NS`) = 125 wakes/s with no GpioInt wait (review-lapptp2 item 5 not done).

### Lane lapptp4 (no user; default stays ps2)
Owns drivers/src/i2c_hid.rs, drivers/src/dw_i2c.rs, drivers/src/i2c_acpi.rs.
1. `auto` = try i2c when i2c_acpi finds a PNP0C50 device with a PTP collection, otherwise ps2 (same code path as `i2c`
   with the existing fallback).
2. Idle cost: wire the `_CRS` GpioInt (or, if the GPIO controller is not driven yet, adaptive polling: 8 ms while frames flow,
   back off to 100 ms after 2 s of empty reads, back to 8 ms on the first report). Gate: idle wake census with i2c
   ≤ ps2 + 2/s.
3. `touchpad: auto -> i2c (CPD0)` / `touchpad: auto -> ps2 (<reason>)` log line.
Tests: QEMU both arches (no device → `auto -> ps2 (no PNP0C50)`); laptop `touchpad=auto touchpad.debug=1` (no finger needed):
active, errs 0, idle wakes measured.

### What flips the default to `auto`
All of:
1. The user's finger test (lane-lapptp2 USER steps 1-7) passes on `touchpad=i2c` (or `auto`), klog has
   `touchpad: PTP reports confirmed`, errs 0, and no double motion (`ps2 pad packets dropped` rises, or the PS/2 side is
   silent).
2. lapptp4's idle cost gate holds (≤ ps2 + 2/s).
3. S3: either s3iwlhda's LPSS/i2c_hid resume is merged, or S3 is still opt-in (`pm.s3` off by default). Before
   `pm.s3` becomes default, resume must restore the pad or fall back to ps2.
4. Two clean laptop boots with `auto` (desktop login, typing + pad + trackstick). Then the orchestrator changes
   `unwrap_or("ps2")` → `unwrap_or("auto")` at i2c_hid.rs:336 in a one-line lane and promotes after the usual soak.
User: the finger test (≈5 min) on a `touchpad=auto touchpad.debug=1` trial.
