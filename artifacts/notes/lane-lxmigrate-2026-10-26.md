# lane/lxmigrate (stages 2+3, 2026-10-26): laptop root and /home on lx -- FIXED (openbugs #46)

State now (laptop, USB stick):
- good = 936fa75df020 (kernel from main 86a2bd4c, build 86a2bd4ca2-20261026T0338Z), good cmdline root=PARTUUID=a20ccfac-02.
- / = sda2, written from the lx root image (f2fs-data0-x86_64.img of main 86a2bd4c, sha f5084bd9..., 4.19 GB,
  dd at 747 kB/s in 5601 s, readback sha equal). /home = sda4, fresh `mkfs-f2fs-home.py head` image
  (--size 43719327744, readback sha equal), mounted by LABEL=leandros-home.
- Factory entry a3f226cc has no lx reader: it can no longer mount root. Do not boot it.

Evidence (after the final reboot): klog `[F2FS] Linux node format (lxnode)` x2 (/ and /home); `df` / = sda2, /home = sda4;
write + sync + reboot + readback ok on both; wifi0 default route; ssh ok; scripts/desktop-login.sh rc 0,
"session up after ~6 s (cosmic-comp + cosmic-panel as uid 1000)"; gpuprobe `Mesa Intel(R) UHD Graphics 620 (KBL GT2)`, GLES 3.2.
Secrets (wifi.conf, network profiles, ssh host key, authorized_keys) copied guest-side, sha-compared, never left the laptop.
/etc/leandros/text-login removed.

Found on the way:
- NEW: every f2fs mount reports st_dev 1, so `cp -a` from one f2fs mount to another fails with "are the same file" when the
  inode numbers coincide (same image origin). Repro: mount a second f2fs volume cloned from root,
  `stat -c '%d %i' /etc/x /mnt/new/etc/x` prints identical pairs. Worked around with a content copy (s1c.sh).
- The root image was built without INTEL=1, so it shipped the shared Mesa ship-set (no iris/ANV) and greetd refused COSMIC.
  Fixed on the stick by overlaying the 4 differing files of gpu-stage-x86_64-intel (libgallium, libvulkan_nouveau,
  libvulkan_intel, intel_icd.x86_64.json) via `remote.py deploy-files`. Any future laptop root image needs INTEL=1.
- Guest helper readback counts must divide the image size (home head is 102785024 B, not a 4 MiB multiple).

Helpers: scripts/f2fslx/migrate/ (README lists them).
