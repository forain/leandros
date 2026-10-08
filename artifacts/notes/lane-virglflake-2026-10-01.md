# Lane virglflake — 2026-10-01

Branch `lane/virglflake` from origin/main `3add9f5`, head `22c8e14` (pushed). One commit.
Harness (session scratchpad `vf/`): `vfloop.py <n> <tag>` (finalverify shape: greeter login, [Super+T + `ls -la /bin`], Super+A/Esc + Super+W/Esc x3, cosmic-settings open + Super+Q x5; per boot: session pid set before/after, settings-opened count, host virglrenderer error lines in QEMU stderr, greetd.log error scan, 3x 200-byte serial echo probe), `serburst.py <len> <reps>` (serial burst while the shell is busy in `sleep 2`).

## Outcome
- **Desktop virgl flake (Illegal surface / dock vanishes / settings exit): NOT REPRODUCED on current main — 0 in 30 sessions.** Likely gone with the AF_UNIX 208 KiB and weak_window fixes; no guest-side virgl id-lifetime bug found.
- **Serial input loss: FIXED** (root-caused, ours).

## Measurements (x86_64 KVM, desktop, `--virgl`, GALLIUM_THREAD=0 auto)
| run | image | shape | sessions OK | host `Illegal`/`dispatch` lines | session pids died | settings opened |
|---|---|---|---|---|---|---|
| base | main 3add9f5 | no terminal, 1 overview round | 10/10 | 0 | 0 | 50/50 |
| base2 | main | + Super+T/typing, 3 overview rounds | 9/10 (1 = harness root login failed, session not exercised) | 0 | 0 | 45/45 |
| fix | lane | same as base2 | 9/10 (same harness login failure) | 0 | 0 | 45/45 |
| a64fix | lane, Mac aarch64 HVF gles31 virgl | base shape | 5/5 | 0 | 0 | 25/25 |
| fixvenus | lane, desktop Venus | base2 shape | 2/2 | 0 | 0 | 10/10 |

## Serial input loss — root cause and fix
- The UART drain (`arch/*/timer.rs`, aarch64 `uart_irq`) pushed every byte into the console tap's evdev queue: 256 slots, 2 per byte (KEY+SYN) while the console owns the keyboard. A full evdev queue is handled Linux-evdev style (`EvClient::push`): the **whole queue is discarded** and SYN_DROPPED queued. For a byte stream that silently loses everything pending: any burst >~128 bytes arriving while the reader is busy.
- Fix: `evdev_server::serial_rx_drain` — keeps the console tap below 8 free slots, parks the excess in an ordered 4 KiB backlog refilled first on each drain, and stops reading the UART only when the backlog is full (QEMU then backpressures; 16550/PL011 accept only FIFO space). ISIG bytes and Ctrl-T still act on read. aarch64 masks PL011 RX/RT interrupts while throttled (level-triggered INTID 33) and the cpu0 tick unmasks (`uart::set_rx_irq`, MMIO only on change).
- `serburst` (shell in `sleep 2`, then `echo X<len>Y`): x86_64 KVM **main 0/3 @600 B, 0/2 @3000 B; lane 3/3 @600, 2/2 @3000, 1/1 @6000 (throttle path)**. aarch64 HVF lane 3/3 @600, 2/2 @3000, 1/1 @6000 (no aarch64 base run; same code path as x86 before the fix).

## Regressions (lane image)
- x86_64 KVM `--no-gpu`: drmsmoke failed=0; polltest 0 FAIL (p50 35 us, max 76 us, wake_latency PASS).
- aarch64 HVF `--no-gpu`: drmsmoke failed=0; polltest all PASS.

## virgl id-lifetime review (no bug found that explains the flake)
- 3D resource ids are monotonic (`alloc_resource_id`), never reused. Ctx ids are recycled lowest-first, but only after CTX_DESTROY is answered (ctrlq is in order).
- RESOURCE_CREATE attaches to the creator's ctx (ctx_ensure first). PRIME import of a virgl 3D BO makes a per-open alias holding a ref on the primary, and RESOURCE_INFO attaches it to the importer's ctx (Mesa always calls it on a cache miss). Host unref happens only at refcount zero. No CTX_DETACH on alias close, which is harmless (host unref detaches from every ctx).
- EXECBUFFER rejects the whole submission on an unknown bo_handle, which would produce exactly this class of host error later. It logs `[DRM] EXECBUFFER: unknown bo_handle=`. That line appeared 0 times in all serial logs. greetd.log showed 0 "expect bad rendering".

## Open items
1. **Root serial login fails in about 1 of 10 boots, on both main and the lane.** The harness retries up to 4 times: main base2 needed 1 retry on 3 boots, and one boot never logged in; the lane fix run needed 4 retries over 3 boots, and one boot never logged in. The password line ("root\r") vanishes exactly while virtio-gpu initializes (`[GPU] MSI-X armed` interleaves). The next line is then taken as the password, and login falls out of step. It is not queue overflow, since the lane rate is unchanged. Something consumes or drops console bytes during DRM-server init, or login/brush startup does (see driver.py's "first 8-byte chunk dropped" note). Not root-caused.
2. **KMS framebuffers hold no BO reference and are never swept on open close** (`std_handle_addfb2` / `remove_framebuffer`; nothing in `drm_release_open`). A BO whose last handle and fd go away unrefs its host resource and frees its pages while a framebuffer, and possibly the scanout, still names them. This explains hostgpufault's `SET_SCANOUT error 0x1203 (invalid resource id)`, and the 2D present path can read freed pages. The fix needs a fb→BO ref plus a per-open fb sweep. Not done here.
3. If the virgl flake returns, the harness now records host error lines, pid churn and serial integrity per boot: rerun `vfloop.py`.

Worktrees removed: desktop `leandros-siblings/leandros-virglflake`, `leandros-vffix`; Mac `~/code/leandros-virglflake`. Desktop raw logs kept in `leandros-siblings/vf-out/` (plus `vfloop.py`, `serburst.py`, `vf-queue.sh`).
