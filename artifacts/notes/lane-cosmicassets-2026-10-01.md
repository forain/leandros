# Lane cosmicassets (2026-10-01): FIXED

Branch `lane/cosmicassets` @ `a529190` (base d7d3297), pushed. Image-content only, no COSMIC source patches.

## Changes
1. `scripts/mkfs-f2fs-populated.py`: stage upstream `cosmic-icons/extra/scalable/apps/application-default.svg` also as hicolor `scalable/apps/application-default-icon.svg` (the name `CosmicAppletTime.desktop` asks for; no upstream theme ships it under that name, so it is an alias of the upstream file).
2. `artifacts/m6-session-data/shared/usr/share/applications/`: 30 per-page `com.system76.CosmicSettings.*.desktop` files copied verbatim from `cosmic-epoch/cosmic-settings/resources/applications` (NoDisplay=true entries; the base CosmicSettings.desktop was already staged and is identical). Their 30 Icon= names (preferences-*) all exist in the already-staged cosmic-icons trees.

## Evidence (both arches; x86_64 was Mac TCG with virgl, the desktop was unreachable: "Network is unreachable", Tailscale timed out)
- Build: mkfs's Icon= cross-check prints no WARNING (all staged .desktop Icon= names resolve, including application-default-icon).
- Super + "appearance": Appearance entry listed first with its icon (also Wallpaper, Applications, Workspaces, Startup, Sound, Panel, Dock, Default applications, each with icons); Enter opens Settings on Desktop > Appearance. aarch64 (HVF) and x86_64 (TCG): screenshots in scratchpad ca/a64, ca/x64.
- "display" and "wallpaper" searches also run; shots in the same dirs.
- Session log (/var/log/greetd.log, 330 KB per arch): 0 lines mentioning default-icon, 0 icon load failures. exc/PF/SEGV/panic/WDOG all 0 on both.

## Sweep (task 3): remaining non-asset warnings, not fixable by staging
org.freedesktop.portal.Desktop / UPower / login1 / locale1 / PolicyKit not on the bus; `/home/leandro/Desktop` missing (cosmic-files-applet); "Failed to activate another instance" (no single-instance bus). No missing icon or .desktop warnings found.

## Open
- Desktop KVM x86_64 not verified (unreachable); the same image content is arch-independent.
