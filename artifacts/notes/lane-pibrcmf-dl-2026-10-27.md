# lane pibrcmf-dl (2026-10-27): CYW43455 firmware download read-back mismatch

Status: FIXED in host tests (most likely cause); hardware unverified. Runtime knobs let the
next Pi boot bisect the bus over serial with no new kernel if the fix is not enough.

## Symptom (Pi 5 boot4, also on every `brcmfctl retry`)
    [brcmf] fail fw RAM read-back mismatch stage=code t=40ms
    [brcmf] diag ... cr4 ioctl=00000021 rst=00000000 ... ramtop=f5909ba5 fw=not-started

## Root cause (most likely)
`fw::verify` read back two 256-byte samples: the head, and the tail at `len - 256`.
`cyw43455-sdio.bin` is 609309 bytes, so the tail sample started at offset 609053
(addr & 3 == 1). With SBSDIO_SB_ACCESS_2_4B_FLAG the SDIO core turns F1 CMD53s into
32-bit backplane accesses, so addr[1:0] is dropped and the read returns the enclosing
words: the data is shifted by one byte and the compare fails, although the write itself was
fine. brcmfmac never issues unaligned RAM transfers. Evidence:
- t=40 ms covers the whole 609 KB write at 4-bit 50 MHz (about 25 ms of wire time). Every
  CMD53 got a clean R5 and no CRC error, so the bus carried the data; a broken data path
  would have shown CRC/timeout errors, not a silent compare failure.
- ramtop differing per attempt is expected: nvram is written after the code stage, so the
  top of RAM is still uninitialised SRAM. It is not evidence of misdirected reads.
- The mock now models word-granular 2_4B accesses (`word_granular`, default on). With the
  old tail offset it fails exactly like the Pi: `stage=code`, off=609053, got = exp shifted
  by one byte, nbad=256.

## Fix
- `Backplane::ram_write`/`ram_read`: the body moves as whole words; an unaligned head or a
  partial tail word is a read-modify-write (or a word read) of the enclosing word. This also
  removes the 29-byte trailing byte-mode CMD53 of the image tail (1 byte past a word).
- `verify` samples start word-aligned; `verify=2` (the new default) reads back the whole
  image (about 25 ms more) and records the first difference.
- Failure is self-describing: `[brcmf] fail fw RAM read-back mismatch stage=code off=0x..
  addr=0x.. exp=<8 bytes> got=<8 bytes> nbad=N checked=N t=..ms`.

## Runtime knobs (no new kernel needed; each retry starts from the defaults)
- `retry sdclk=25000|...` (kHz, capped at 25 MHz without HS), `hs=0`, `blk=0` (byte-mode
  CMD53 only), `blksz=64|512` (F1 block size), `chunk=N` (max bytes per CMD53),
  `verify=0|1|2`, `win4b=0|1` (bulk RAM transfers without the 4-byte flag).
- `peek addr=0x...`: 16 bytes as four 4-byte register reads AND as one bulk read; a
  difference between the two points at the bulk path. `poke addr=0x... val=0x...`: one
  32-bit write plus read-back. Both need a parked chip (after a failure past attach).
- A non-default bus tune prints `[sdio] tune sdclk=.. hs=.. blk=.. blksz=.. chunk=..`.

## Tests
- brcmf-tests 56/56 (+5): `odd_length_image_verifies_on_a_word_granular_core` (shown failing
  with the old tail offset + byte-exact unaligned read), `unaligned_ram_access_is_word_safe`
  (shown failing with the head alignment removed), `verify_mismatch_is_self_describing`,
  `win4b_off_uses_byte_access_for_ram_only`, `dl_knobs_parse`.
- sdio-tests 19/19 (+2): `capped_split_never_exceeds_cap`, `tune_knobs_reach_the_bus`.
- check-rpi5.sh rpi5 + raspi4b clean. aarch64 regress: see the merge report.

## Next Pi boot should show
    [brcmf] fw 609309 nvram ...@0x... rstvec 0xb83ef198 sr 1 F2 ready t=...ms
or, if the bus really corrupts data, a `fail fw ... off=.. exp=.. got=..` line to bisect with
the knobs above (order: verify=2 already default -> sdclk=25000 -> hs=0 -> blk=0 -> chunk=64
-> win4b=0; peek addr=0x198000 compares the word and bulk paths).

## Open
- The word-granular model of the 2_4B flag is from brcmfmac's behaviour (it never issues
  unaligned RAM transfers), not from a datasheet. The Pi's next failure line settles it.
- Pre-existing warning: `parked = None` never read in `brcmf_boot::run`.
