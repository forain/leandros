# lane/nicmcast 2026-11-11 - FIXED (brcmf); other drivers already pass-all

Audit: macb sets HRB/HRT = all ones with NCFGR_MTI (macb.rs:297); e1000e RCTL_MPE; r8169 ACCEPT_MULTICAST; iwlwifi MCAST_FILTER_CMD pass-all after association (iwl.mcast=0 disables). Only brcmf (Pi 5 Wi-Fi) set nothing, so the CYW firmware dropped group-addressed frames.

Fix: brcmf `init` sets iovar `allmulti`=1 after UP (cfg.rs). Host test init_sequence asserts it (red before, green after; brcmf-tests 84/84).

Evidence (Pi 5, kexec trial 0d4e1d2a5d, Mac sends 30 UDP datagrams to 224.0.0.251:5353 from en0 onto the Wi-Fi LAN, wifi0 /proc/net/dev rx packets delta): before +5 (idle noise), after +37. eth0 (macb) already received them.

Open: no per-group list (pass-all; smoltcp filters by joined group). IPv6 33:33 frames now reach the stack on every driver, but smoltcp v6 group acceptance is the IPv6 lane's concern. Laptop (e1000e/iwlwifi) not re-tested: code unchanged.
