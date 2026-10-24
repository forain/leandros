# lane/pwvanish2 (L11.4 second half) - REFUTED

Original symptom (lane-polish1002): on aarch64, while root plays audio on the serial console during the desktop login, the launcher's backgrounded `sh -c 'exec pipewire'` vanished (4/7 runs then).

Retest on main b7549612 (aarch64, --virgl, release build): `AV_PRECLAIM=5 AV_STEPS=probe audio-session-test.py`, i.e. root runs `aplay tone-440-10s.wav` 5x in a row in the background on serial before the greeter login, then the leandro login via the greeter.

Result: 7 of 7 runs, pipewire AND wireplumber present in /proc after login, pipewire.log non-empty (`leandros-snd-sink: connecting -> paused`), `wpctl status` answers. Vanished: 0/7 (was 4/7). Previous lane pwvanish: plain `exec pipewire` as root 7/7 fine.
The sink waits on EBUSY while root's aplay holds the device (by design, see lane-polish1002 section 4), so the device exclusivity is not what killed it. The old failure has not reproduced since the greetd/launcher fixes (lane-greetdfix-2026-10-23); no kernel change made.
