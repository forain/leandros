# lane/pigpu 2026-10-28: V3D ladder green on the Pi 5

FIXED. `v3d.probe=1` on the Pi 5 now climbs the whole ladder.

Root cause of `fail pm-confirm mismatch val=0x0000706d`: the pm-confirm gate required
PM_GRAFX POWOK. The 2712 firmware leaves GRAFX at 0x706d (POWUP|ISPOW|ISFUNC; POWOK and
MRDONE clear) and V3D answers behind it. The gate now requires POWUP|ISPOW|ISFUNC.

Firmware power-domain step (SET_DOMAIN_STATE 0x00038030, as raspberrypi-power): implemented
(`power::fw_domain_on`, knob `v3d.pwr=fw`, off by default). The Pi 5 firmware answers
GET_DOMAIN_STATE (0x00030030, state 0 for domains 0..15) but rejects SET_DOMAIN_STATE with
buffer code BadCode(0x80000001). The domain number is 7 (dt-bindings), not 10. It is not needed.
`v3d.pwr=nogate` additionally skips the island-bit check.

Evidence (good f51a46029ffb -> trial, `v3d.probe=1` only):
    [v3d] ident ver=71 rev=10 cores=1 qpus=12 va=36 pa=36 clk=960MHz sms=idle
    [v3d] mmu pt=0x1da000000 flush ok; irq hub=282 ok core=281 ok mode=irq
    [v3d] cl queue up pool=4x256K mode=irq
    [v3d] selftest clear crc=0xe8f5217c ok frdone=1 t=14us
    [v3d] selftest tfu copy ok perm=4096/4096 crc=0x42bae001 tfuc=1 t=13us
    [v3d] armed: card0+renderD128 report v3d (auto, probe ok)
No SError. Host tests drivers/v3d-power-tests: 21/21.

Open: the probe is still opt-in (`v3d.probe=1`); next is pi-gpu-smoke.sh and the default-on decision.
