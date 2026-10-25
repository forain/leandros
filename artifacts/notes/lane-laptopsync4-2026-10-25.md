# lane/laptopsync4 - 2026-10-25: laptop kernel + DOOM/MAME/sshd sync, hardware checks

## Verdict: PARTIAL (kernel promoted, DOOM/MAME window checks pass; VT2 check FAILS on the laptop)

### Kernel
- Built x86_64 release from main 1ae110ec (tag `laptopsync4`), trial id c9a6c46eab81, `uname -v` = `#1 laptopsync4` checked before measuring and promoting.
  Promoted: good = c9a6c46eab81 (sha256 4ac062ea...), default=good, armed=0; good cmdline still has root=PARTUUID and iwlwifi.mgr=1, no netwatch.
- Link was poor (wifi0 -75 dBm, ch 1, 24% rx retries, RTT 0.2-5 s). sftp of the kernel failed twice (Connection closed), third attempt passed.
  Mame (397 MB) as one deploy-files push failed after ~30 min; pushed as 48 x 8 MB chunks (`push` of a dir resumes by skipping identical files), 2102 s, reassembled with cat, sha256 matches.

### Userland deployed
/bin/doom (doomsnd music/SFX; `music_peak=` marker present), /bin/vttest, /usr/sbin/sshd (old kept as sshd.old), /bin/mame (cf644ba32b0 wayland, 831757542d7 kms, 22fedcad5cc keyboard).

### Measurements (login by scripted greetd, ~13 min after login for DOOM)
- DOOM in a COSMIC window ("drawing in a Wayland window"): fps 33.9-35.0 (cap 35), music_peak 12192-13284 steady (22938 in the first line), playing=1,
  audio dry=0 in 7 of 8 stat lines; the first line (startup sf2 load, max_dt=1062ms) has dry=2. No steady-state underruns.
- MAME `captcomm -str 30` in a window: "Average speed: 100.00% (29 seconds)", offered 59.6/s. Presented wl fps was 1.0 (cb_ms ~994): the panel had
  blanked (about 55 min after login), so frame callbacks came at 1/s (blankplay behaviour); speed is unaffected.
- VT2 (gv.sh, /bin/doom and /bin/mame from root on VT2 while the desktop session runs): FAIL on i915.
  DOOM: "the display is owned by another program (DRM master, EACCES)"; MAME: "DRM SET_MODE failed (errno 1)", then "FLIP_PAGE refused (errno 13), display not ours"
  repeating. /proc/vtstat: vt1 kd=graphics gfx_owner=472 (cosmic-comp) stays DRM master while vt2 is active; active VT bounced back to 1 by itself
  during the DOOM and MAME phases (switches 8 total); desktop is on VT1 at the end (active 1).

### Closes
- #55 (MAME laptop check): closed (windowed MAME on the laptop, 100% speed).
- #56 (VT2 laptop check): NOT closed - fails on hardware (see above).

### Gotcha
gv.sh's kill loop matches any process whose cmdline contains "doom" or "mame" and kills the SSH command shell running it; start it detached
(`nohup sh /root/gv.sh &`) from a command string that does not contain those words.
