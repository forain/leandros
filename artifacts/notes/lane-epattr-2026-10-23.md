# lane/epattr 2026-10-23 - measurement tooling (L2.7, L3.7)

FIXED (tooling only, no behaviour change).

- `/proc/epollstat` (new, `sched::wakestat::EpStat`, `servers/vfs gen_epollstat`): one row per
  epoll instance that has waited: `tgid epfd handle parks useful useless overflow | fd:count:kind ...`.
  `useless` = sleeps after which the wait found nothing; the fds are the registrations the waking
  ring named in that pass (`-2:n:all` = wake carried no registration list, full re-probe).
  Kind is resolved at read time from the owner's fd table. Hot path: relaxed atomics in a static
  256-row table keyed by instance id; no locks, no allocation. Cumulative since boot, diff two reads.
  Attribution is per pass, so a wake naming several registrations credits each of them.
- `/proc/i915` gains `prime handle_to_fd N handle_to_fd_err N fd_to_handle N fd_to_handle_err N gem_close N`,
  counted at the exit of the DRM ioctl dispatch (backend-generic; file appears once any is non-zero).
- Test: `/bin/epolltest stat` (added to the regress suite): wait with a helper thread blipping a
  second eventfd; checks the row exists for our epoll fd, parks and wakes counted, and useless
  wakes pinned on the blip fd. Measured on aarch64/HVF: parks=48 useful=2 useless=38.
  Before the change the file does not exist (stat_row_present FAIL).
- Regress aarch64 green.
- Open: PRIME counters are not exercised by a guest test (needs an i915/virtio PRIME client);
  `/proc/<pid>/epollstat` not done, filter the tgid column instead.
