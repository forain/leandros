# lane/podtrace 2026-11-09 - podman userland in the image + failure trace

PARTIAL (packaging DELIVERED; podman run blocked by kernel mntns/idmap, documented).

- ports/alpine/world + locks: podman, crun, conmon, netavark, aardvark-dns, passt, fuse-overlayfs, slirp4netns, shadow-subids (provides shadow-uidmap), containers-common (+36 pkgs on aarch64; 468 MiB Alpine staged). scriptallow: podman/fuse-overlayfs/slirp4netns/pinentry post-install (hints/modprobe/symlink only).
- Image: /etc/subuid|subgid on disk (apk's empty ones dropped before apk-shadow), /etc/containers/storage.conf = vfs (ports/podman), /var/tmp 1777 (was missing), newuidmap/newgidmap 4755 shim (Alpine uses file caps, which the image lacks), unix_chkpwd root:42 tolerated in alpinepkgs. LEANDROS_IMAGE_ARCHIVE=<tar> embeds a local OCI archive at /<name> (tracing aid).
- Go runtime OK: podman --version / info work as root. First failures: unshare(CLONE_NEWNS) EINVAL (sys_unshare, kernel/src/syscall.rs:8396) in layer apply; rootless "cannot clone: Invalid argument". Full ordered trace: orch/designs/podtrace-findings.md.
- Evidence: aarch64 default regress on Mac: overall PASS, 0 boots failed. Box regress not run (box /tmp tmpfs 95% full, ENOSPC).
