# lane/i915modtest (K3b) - i915 as a module under the fake-device suites - 2026-10-30

DELIVERED. `CONFIG_I915=m` passes the autoloadtest i915 case on both arches.

Fixes: kapi_page_alloc/free order cap 10 -> buddy MAX_ORDER (kernel/src/kmod/exports/pci.rs; the fake GSM is order 11);
modules/i915 init wires `set_tmpfs_census` (fixes churn_tmpfs_no_orphaned_slots / tmpfs_pool_no_orphans_after_hold).

| suite | arch | =m pass/fail | =y pass/fail |
|---|---|---|---|
| i915fake | x86_64 | 0 unexpected fails | 102/0 |
| i915crtc2 | x86_64 | 0 unexpected fails | 4/0 |
| i915fence | x86_64 | 0 unexpected fails | 103/0 |
| i915fake | aarch64 | 100/2 | 100/2 |
| i915crtc2 | aarch64 | 4/0 | 4/0 |
| i915fence | aarch64 | 101/2 | 101/2 |

The two aarch64 fails (ddb_plane_whole_pipe_0_860, ddb_cursor_top_32_860_892) also fail with the built-in `=y` build on
aarch64 (measured on this branch; see lane-lapdispC1-2026-10-27.md and lane-i915kenv-2026-10-30.md), so they are not a
module bug. autoloadtest allows exactly those two names on aarch64 and requires none elsewhere.

Alias check: default QEMU (virtio-gpu, no Intel) with I915=m, aarch64: i915 absent from /proc/modules, no i915 kmod log lines.
Box aarch64 full regress (pci.rs change touches the kernel): 61 suites, 0 bad, PASS.
