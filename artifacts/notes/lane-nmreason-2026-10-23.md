# lane/nmreason 2026-10-23 (L11.14) FIXED

nmtest_real "AP that refuses association" reported `connection failed: unknown`.
Root cause (serial DBG trace): the failed user connect (FAILED reason 5, then DISCONNECTED)
was followed within milliseconds by wifi_mgr autoconnect of another profile (SimHome);
its CONNECTING cleared the shim's fail hold/last_fail, so Device.StateReason read by nmrs
after the AC died was 0 (nmrs prints "unknown"). Not the shim and not netctl: a timing race
exposed by the faster net paths (netirq/netmmio).
Fix: wifi_mgr pauses autoconnect 2 s after a failed non-auto connect (USER_FAIL_AUTO_DELAY_MS),
like NM's retry delay, so the failure stays on the device while clients read it.
Evidence: net-e2e nmtest_real pass=18 fail=1 before, pass=19 fail=0 after
("supplicant configuration failed after 2220 ms").
