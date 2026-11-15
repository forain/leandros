# lane/nvmeebs 2026-11-14 (EC2 design lane L2)

FIXED (QEMU): Amazon NVMe quirks in drivers/src/nvme.rs.
- Amazon ids (1d0f:0061/8061/cd00-cd02) or `nvme.quirks=amzn`: DMA buffers checked < 2^48, I/O timeout 300 s
  (`nvme.io_timeout=<s>` override), warning at 30 s, reset only at cap or CSTS.CFS.
- Identify serial/model and Amazon device name (bytes 3072..3103) kept per controller; VFS exposes
  `/dev/disk/by-id/nvme-<model>_<serial>[_<nsid>][-partN]` (open/stat/mount) and sysfs `device/serial`, `device/ebs_device`.
- Amazon controller holding the booted root (boot disk or resolved root) is writable without `nvme.rw=1`
  (`nvme.rw=0` opts out); every other controller keeps the read-only default (laptop unchanged).
- Test: `runtests.py <arch> <tag> nvmeebs` (host:nvmeebstest.py). Red on main: root mount refused (nvme0n1 read-only). Green aarch64.
Open: real Nitro run (G1); no directory listing of /dev/disk/by-id; QEMU model string is fixed ("QEMU NVMe Ctrl").
