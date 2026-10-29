# lane/bkeys 2026-10-29

FIXED: HP EliteBook 840 G5 brightness keys in the x86 PS/2 set-1 driver
(arch/x86_64/src/keyboard.rs, E0 table): E0 12 -> KEY_BRIGHTNESSDOWN (224),
E0 17 -> KEY_BRIGHTNESSUP (225). Break codes E0 92 / E0 97 follow from the
make|0x80 handling. The `kbd: unmapped sc` log is kept; E0 01 stays unmapped.

Capabilities: the evdev keyboard node already advertises KEY codes 1..255
(servers/evdev/src/lib.rs, DEVKIND_KEYBOARD EV_KEY), so 224/225 are covered.

Gates: x86_64 and aarch64 release builds OK; x86_64 box regress (see merge).
Open: needs a real-laptop press to confirm the compositor reacts.
