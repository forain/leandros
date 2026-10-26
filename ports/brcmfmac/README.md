# brcmfmac firmware (Raspberry Pi 5, CYW43455)

`fetch.sh` downloads three files from RPi-Distro/firmware-nonfree at the pinned
commit `c91cd2804cf7463aab913e7247c176049f16bbd6` (branch bookworm), checks
sha256 and caches them in `~/.cache/leandros/brcmfmac` (`$BRCMFMAC_CACHE`).
`scripts/mkfs-f2fs-populated.py` (aarch64) stages them on the root filesystem:

| Path on the root fs | Bytes | sha256 | Source (debian/config/brcm80211/) |
|---|---|---|---|
| `/lib/firmware/brcm/cyw43455-sdio.bin` | 609309 | d608f866582519c0a28d86db43040f4f1b98dd1d153e72e9752586546b4a36c3 | `cypress/cyfmac43455-sdio-standard.bin` (what Debian's `cyfmac43455-sdio.bin` links to) |
| `/lib/firmware/brcm/cyw43455-sdio.clm_blob` | 2676 | 9823842cae9fb9a5dd1e5fb31f595516ec7deee341354bef30bb3026eee29cc1 | `cypress/cyfmac43455-sdio.clm_blob` |
| `/lib/firmware/brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt` | 2074 | ca709be81a78bdb6932936374f39943acbd7af07fae6151011127599a3ce9e3d | `brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt`, a git symlink to `brcmfmac43455-sdio.txt`; the real file is stored under the 5-model-b name |

Driver API: the files are on the real root, so they exist only after init's
`pivot_root`. Use `drivers::firmware::request_firmware("/lib/firmware/brcm/<name>", max_len)`
from task context (a kernel thread) and wait for `drivers::firmware::root_ready()`;
before that every call returns `FwError::NoRoot`. `Firmware::read_at` copies out,
`chunks()` iterates, `read_firmware_chunked` streams without keeping the file (the
.bin is 609 KB; stream it to the chip's RAM). The nvram text carries a fixed
`macaddr=b8:27:eb:74:f2:6c`; the driver should replace it with the OTP/real MAC.

## Licence

`cyw43455-sdio.bin` and `.clm_blob` are Cypress/Infineon binaries under the
Cypress "binary-redist" EULA, which allows reproducing and distributing the
binary form for use with Cypress ICs; the text is in `LICENCE.cypress` (copied
from the `binary-redist-Cypress` stanza of firmware-nonfree's `debian/copyright`).
The Pi nvram text is GPL-2 (Linaro / Raspberry Pi). The blobs are never committed;
the fetch script downloads them.

## Card prep

    sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode direct \
        --wifi-psk <path>/leandro2026.psk --authorized-keys ~/.ssh/leandros_remote.pub <device>

The SSID is the psk file's basename without `.psk`; the first line is the
passphrase. It is written to a 0600 temp file, staged as `/etc/leandros/wifi.conf`
(mode 0600, root; `ssid=` / `psk=` lines, the same file iwlwifi reads on the
laptop via `$LEANDROS_WIFI_CONF`), and never echoed. `--authorized-keys` stages
`/root/.ssh/authorized_keys` (0600). sshd (`/usr/sbin/sshd`, started by init on
port 22, `PermitRootLogin prohibit-password`) is in the aarch64 image like on x86_64.
