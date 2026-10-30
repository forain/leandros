# lane/cosapplets2 2026-10-30
- audio applet: built + staged (+ .desktop). No libpulse: it uses cosmic-settings-audio-client (varlink to settings-daemon, PipeWire).
- minimize/tiling: already built and staged by the m6 route; unchanged.
- workspaces: system default plugins_wings staged (upstream list + CosmicAppletWorkspaces).
- aarch64 QEMU (btsession --keep, TCG): panel logged "Starting:" for audio, tiling, workspaces, minimize; all 4 processes alive >15 s; no panic in greetd.log.
- Not verified: the audio applet's displayed sink volume (no screenshot taken); settings-daemon reports default sink leandros_output.
- Regress suite not run (no kernel/userspace code change; image staging only).
