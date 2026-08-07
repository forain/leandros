# M5 session-ship prep progress

Started + finished 2026-07-22. Host-only, repo-read-only lane.

## Steps — ALL DONE
- [x] 0. Recon: m3-gl-stack, m4-input-ship, ports/dbus, cosmic-epoch layout
- [x] 1. cosmic-comp closure verify (both arches) — CLOSED, zero gaps, both arches
- [x] 2. Session bus staging (busd, session.conf, dbus-run-session) — staged, arch/type verified
- [x] 3. Font audit — scan dirs + zero-font behavior confirmed from source; Open Sans + Noto Sans Mono staged (not Fira Sans — verified, prior memory guess was wrong)
- [x] 4. ICED_BACKEND / renderer env var — confirmed ICED_BACKEND=tiny-skia from iced fork source; also found COSMIC_BACKEND=kms (cosmic-comp's own var)
- [x] 5. notes/m5-session-manifest.md written

## Result summary (see m5-session-manifest.md for full detail)
- cosmic-comp NEEDED=8, fully resolved against m3-gl-stack sysroot ∪ m4-input-ship, both arches, no host libs.
- busd staged /usr/libexec/busd (static ET_EXEC, correct arch both), session.conf -> /usr/share/dbus-1/session.conf, dbus-run-session -> /usr/bin/dbus-run-session.
- Fonts: /usr/share/fonts/ is the scan dir that matters (fontconfig-parser finds no config on LeandrOS -> falls back to load_no_fontconfig() -> /usr/share/fonts, /usr/local/share/fonts, ~/.fonts, ~/.local/share/fonts). Zero fonts = soft-fail (blank text, no panic), verified no panic/unwrap in the font-match path. Staged: Open Sans (Regular/Bold/Semibold) + Noto Sans Mono (Regular/Bold), SIL OFL 1.1, sourced from the already-cloned libcosmic checkout's own res/ dir (exact upstream files). ~1.6MB total.
- ICED_BACKEND=tiny-skia verified in iced/renderer/src/fallback.rs + iced/tiny_skia matcher. COSMIC_BACKEND=kms verified in cosmic-comp/src/backend/mod.rs (cosmic-comp's own backend selector, not iced's).

## Artifacts
- m5-session-ship/verify-closure.sh, m5-session-ship/{x86_64,aarch64}/usr/{libexec,bin,share/dbus-1}/, m5-session-ship/share/fonts/{open-sans,noto}/
- notes/m5-session-manifest.md (full writeup + mkfs snippet, not applied to repo)

No blockers. Nothing left pending for this lane.
