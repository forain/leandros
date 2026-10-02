# Console keymaps

Binary keymaps for `/bin/loadkmap` (userland/loadkmap), installed to
`/usr/share/keymaps/<name>.bmap` by `scripts/mkfs-f2fs-populated.py`.
Selected at boot by `/etc/vconsole.conf`:

    KEYMAP=de-latin1

Format: kbd's `loadkeys -b` ("bkeymap": magic, 256 present-flags, then 128
native-endian u16 keysyms per present keymap) — the same file busybox
`loadkmap` reads. Generated on a Linux host with kbd 2.10.0:

    LC_ALL=en_US.UTF-8 loadkeys -b -q <name> > <name>.bmap

for `us`, `uk`, `de-latin1`, `de-latin1-nodeadkeys`, `fr-latin1`, `es`,
`br-abnt2`. `de.bmap` is a copy of `de-latin1.bmap`: kbd's own `de` map is the
7-bit DIN 66003 layout (the umlaut keys type `[ ] \ @`, meant for a German
national font), which is not what anyone writing `KEYMAP=de` on a UTF-8
system wants. This alias is a deliberate deviation from kbd.

The kernel's built-in map (`servers/tty/src/defkeymap.rs`) is Linux's default
`defkeymap.map`, generated with `gen-defkeymap.py` from the output of
`loadkeys --mktable defkeymap.map`.
