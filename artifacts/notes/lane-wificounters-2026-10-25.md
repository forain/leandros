# lane/wificounters 2026-10-25 (openbugs L12 #48)

(a) FIXED. New shared `wifi_mgr::gk` (drivers/src/wifi_mgr.rs): GTK cipher, key index, install count,
last install age, and TKIP RX counters (fw-decrypted, host-decrypted, Michael failures, replay drops,
undecryptable). Printed as a `# Group key (since boot)` block (`gtk:` / `gtk_rx:` lines) at the end of the
MGR scan table, so /proc/net/wifi shows it for iwlwifi (mgr=1) and wifi_sim.
Hooks in iwlwifi.rs: both ADD_STA_KEY GTK installs, `rx_tkip` (fw/sw/MIC/undecrypted), `count_reject` (replay;
note replay_drop counts all data-frame replays, not only group).
wifi_sim: four_way keeps the supplicant; `sim_group_rekey(key_id)` sends a real EAPOL group message 1
(AES-wrapped GTK KDE) through the supplicant. New selftest checks: gtk-rekey-counted (installs +1, key idx
1->2), gtk-proc-line, gtk-rx-counters-move (the RX counters there are bumped synthetically; the real
decrypt paths are host-test covered, not exercised by the sim).
Before-change evidence: with the install hook disabled, gtk-rekey-counted and gtk-proc-line FAIL
(installs 1->1, key 1->1). After: wifisimtest PASS 44/44; aarch64 regress overall PASS.
Not deployed to the laptop.

(b) FINDING: no grep ships anywhere. The rootfs gets uutils coreutils (feat_os_unix_musl; uutils has no
grep), brush, bottom; nothing in build-all.sh or the f2fs manifest builds a grep/rg/busybox, so there is
nothing built-but-uninstalled to add. Options (not done): add ripgrep or a tiny grep as a sibling build,
or a brush `grep` function over awk if awk exists. Needs a user decision.
