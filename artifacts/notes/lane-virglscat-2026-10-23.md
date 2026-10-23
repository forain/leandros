# lane/virglscat (L1.10) - FIXED 2026-10-23

Scattered virgl buffers can be mmapped (VIRTGPU_MAP / MAP_DUMB token = first frame phys, sys_mmap maps per run),
PRIME-exported (dmabuf VMO from pages) and ADDFB'd. Knob: cmdline `virtgpu.force_scatter=1` or GETPARAM 0x1000000B.

Evidence (aarch64, HVF, virgl):
- Red (mmap path reverted to b.phys): `drmsmoke --scatter` RC=2, TRANSFER_ROUNDTRIP (map) FAIL, SCATTER_MMAP FAIL.
- Green: `drmsmoke --scatter` 0 fails, scatter_creates=2, SCATTER_MMAP/ROUNDTRIP/SECOND_MMAP/PRIME_MMAP PASS, 0 [PAN].
- `drmsmoke --force-scatter` (full suite) + gpuprobe gl: PASS, 0 [PAN]. Normal drmsmoke + gpuprobe gl: PASS.
- Full regress aarch64: PASS.
Open: device VMAs hold no BO reference (munmap after GEM_CLOSE race, same as contiguous dumb mmaps).
