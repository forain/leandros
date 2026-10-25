# lane/laptopsync5 - 2026-10-25: laptop sync of main aea35350 (espdur2 one-shot, DOOM/MAME pw-cat)

## Verdict: DELIVERED (kernel promoted; hang/power-cut step left for the user)
- Built x86_64 release from main aea35350 (tag `laptopsync5`), trial 17c93ea8cfbd, `uname -v` = `#1 laptopsync5` verified before measuring and before promoting.
  Promoted: good = 17c93ea8cfbd (sha256 b467c641...), default=good, armed=0; good cmdline still has root=PARTUUID and iwlwifi.mgr=1, no netwatch, no f2fs.rf (promote stripped the guard knob the trial carried).
- Cleanup done: /root/dm, snd.sh, snd3.sh, gv.sh, gv removed. Old trial d02c1ae9fabb discarded.
- Userland: /usr/sbin/espctl (new one-shot), /bin/doom (pw-cat fallback, markers present), /bin/vttest, /bin/mame (../mame 8dd069d818d, `pw-cat` strings present, sha256 d6e8663c... verified on the laptop).
- Armed check (plan step 1): limine.conf had `remember_last_entry: yes`, `/LeandrOS once 17c93ea8cfbd ~06A536`, trial/good/factory with the same ~nonce, /EFI/LT06A536 present. After the trial reached init and after promote the config is the plain form (no `~`).

## Measurements (desktop session logged in by script, DOOM within ~10 min of login)
- DOOM in a COSMIC window: fps 34.5-35.2 (cap 35), pw-cat stream, audio stats music_peak 12598-13017 steady (22938 first line), playing=1, dry=0 after the startup line.
- MAME captcomm in a window: wl fps 32-34, offered 59.6/s.
- DOOM on VT2 (vttest chvt 2, root, XDG_RUNTIME_DIR=/run/user/1000, no WAYLAND_DISPLAY): DRM double buffering active, "sound: PipeWire stream via pw-cat", music_peak 12903 playing=1 dry=0.
  This is a change from laptopsync4 (EACCES, display owned by cosmic-comp): /proc/vtstat now shows master_by_vt vt1=4 with set_eacces=0.
- MAME on VT2: "display 1920x1080 via DRM", "DRM SET_MODE failed (errno 1)" still logged, "leandros_sound: PipeWire stream via pw-cat", `Average speed: 98.52% (29 seconds)`. Back on VT1 at the end (chvt1=0, active 1).
- First attempt had WAYLAND_DISPLAY exported by the script, so the VT runs tried the compositor (errno 2); a harness mistake, redone.

## Gotchas
- Link was terrible (40% loss, sftp "Connection closed" in seconds). Retrying `push` of a chunk dir managed ~1 chunk per 5 min and the retry wrapper then cat'ed PARTIAL chunks over /bin/mame (9.6 MB); never concatenate without a size/sha check.
- What worked: resumable raw ssh stream (`tail -c +N+1 file | ssh 'cat >> f'`, N = remote size), 100-250 KB/s. A dropped session left a zombie remote `cat` that corrupted the bytes near the reconnect (equal size, wrong sha); fix = truncate to a verified prefix (`head -c`), and only resume when the remote size is stable over 8 s. Final sha matched.

## Open
- espdur plan steps 3-4 (testhang + power cut) need the user: see $SP/hw/ESP_HANG_TEST.md.
