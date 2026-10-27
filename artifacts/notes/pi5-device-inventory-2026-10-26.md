# Pi 5 device inventory (BCM2712 D0 + RP1), 2026-10-26

Source: the firmware-applied D0 device tree (`$SP/hw/rpi5-d0.dts`), no Linux overlays applied.
"Enabled" = `status = "okay"` or no status property. KMS nodes (v3d, hvs, hdmi, dsi, csi, vec,
dpi), every I2C bus, the RP1 PWM/fan, I2S and pcie1 are `disabled` in this tree (Linux turns them
on with overlays such as `vc4-kms-v3d`); they are listed at the end for completeness.

Owners: pisdio/brcmf (Wi-Fi), pipcie (PCIe/RP1), pigem (Ethernet), pirp1 (RP1 clocks/GPIO/fan/LEDs/
power button), piusb (USB), pv3d-* (GPU), pisound-design (audio/Bluetooth/display), pimisc (this lane).

## BCM2712 side

| Node | What it is | Driver in tree | Owner |
|---|---|---|---|
| cpus (4x cortex-a76), l2/l3 cache | CPUs | arch/aarch64 (SMP via PSCI) | core |
| psci (`arm,psci-1.0`) | TF-A: CPU_ON, SYSTEM_RESET/OFF | arch/aarch64/src/smp.rs, power.rs | core (+pimisc PM fallback) |
| gic-400 @7fff9000, armv8 timer | interrupts, timer | arch/aarch64 | core |
| arm-pmu | perf counters | none | none (not needed) |
| timer@7c003000 `bcm2835-system-timer` | legacy 1 MHz timer | none (arch timer used) | none needed |
| uart10 @7d001000 (pl011) | serial console | arch serial | core (#101 RX loss open) |
| uarta @7d50c000 `bcm7271-uart` + `bluetooth` (`bcm43438-bt`) | Bluetooth HCI UART of the CYW43455 | none | pisound-design (design only) |
| sdio1 mmc@fff000 | root SD card | drivers/src/sdhci.rs, blkdev_sdhci.rs | core / pisdio |
| sdio2 mmc@1100000 + `wifi@1` (`bcm4329-fmac`), wl-on-reg | Wi-Fi SDIO | drivers/src/sdhci.rs + brcmf/ | pisdio / brcmf |
| spi10 @7d004000 + spidev10 | BCM2835 SPI (on-board peripheral link) | none | **uncovered** |
| rng@7d208000 `bcm2711-rng200` | hardware RNG | drivers/src/rpi_misc.rs | pimisc |
| watchdog@7d200000 `bcm2712-pm` | PM: power islands, watchdog, reset/halt | drivers/src/rpi_pm.rs (shared) | pimisc + pv3d-* |
| avs-monitor@7d542000 + `thermal` | SoC temperature | drivers/src/rpi_misc.rs, thermal.rs | pimisc |
| mailbox@7c013880, firmware, vcio | VideoCore property mailbox | drivers/src/rpi_mailbox.rs | core |
| firmware-clocks / firmware-reset / power | firmware clocks, resets, power domains | rpi_mailbox (v3d clock, pimisc ARM clock) | pv3d-*, pimisc |
| rpi_rtc `raspberrypi,rpi-rtc` | firmware RTC (battery on J5) | drivers/src/rpi_misc.rs | pimisc |
| nvmem_otp/cust/mac/priv `rpi-otp` | OTP: serial, MAC, customer/private keys | none | **uncovered** |
| fb `bcm2708-fb` | firmware framebuffer | rpi_mailbox::init_framebuffer | core |
| gpio@7d508500, gpio@7d517c00 (`brcmstb-gpio`), pinctrl@7d504100 / aon-pinctrl@7d510700, gpiomem | BCM2712 GPIO + pin mux (WL/BT_REG_ON, SD 1V8, LEDs) | partial (pisdio touches WL_ON) | **uncovered as a driver** |
| intc@7d503000, l2-intc @7d508380/@7d508400 | second-level interrupt controllers (uarta, GPIO, ...) | none | **uncovered** |
| reset-controller@1504318, pcie rescal | brcmstb resets | none / pipcie | pipcie |
| msi-controller@1000130000/131000 (`bcm2712-mip`) | PCIe MSI | none | pipcie |
| pcie2 @1000120000 (x4 to RP1) | RP1 link | drivers/src/rp1.rs (partial) | pipcie |
| pcie0 @1000100000 | external x1 FFC (NVMe/HAT) | none | **uncovered** |
| iommu@5100/5200/5280, iommuc@5b00 | BCM2712 IOMMUs (display/ISP/codec) | none | **uncovered** (with media) |
| dma@10000, dma@10600 `bcm2712-dma` | 40-bit SoC DMA | none | **uncovered** (low) |
| codec@800000 `bcm2712-hevc-dec` | HEVC decoder | none | **uncovered** (low) |
| pisp_be@880000 + syscon piarbctl | ISP back end (camera) | none | **uncovered** (low) |
| clock@7c700000 `brcm2711-dvp` | HDMI clocks/resets | none | pisound-design (display) |
| regulators, cam*_reg, sd-io-1v8-reg | fixed/GPIO regulators | none | pisdio (SD/Wi-Fi ones) |
| leds `gpio-leds`, pwr_button `gpio-keys` | ACT/PWR LED, power button | none | pirp1 |
| usb-nop-xceiv | USB PHY stub | n/a | piusb |

## RP1 side (behind pcie2)

| Node | What it is | Driver in tree | Owner |
|---|---|---|---|
| clocks@18000 `rp1-clocks` | RP1 PLLs/clocks | rp1.rs (partial) | pirp1 |
| gpio@d0000 `rp1-gpio`, gpiomem | 40-pin header GPIO | none | pirp1 |
| ethernet@100000 `rp1-gem` + phy1 | Gigabit Ethernet | macb.rs, macb_hw.rs | pigem |
| usb@200000, usb@300000 `snps,dwc3` | 2x xHCI | xhci (shared) | piusb |
| mailbox@8000, rp1_firmware, shmem@ff00, sram@400000 | RP1 M3 firmware mailbox (PIO) | none | **uncovered** (low) |
| pio@178000 `rp1-pio` | programmable I/O | none | **uncovered** (low) |
| dma@188000 `snps,axi-dma` | RP1 DMA (SPI/I2S/UART) | none | **uncovered** (with audio) |
| adc@c8000 `rp1-adc` | RP1 die temperature + 4 ADC inputs | none | **uncovered** (small; fits pirp1/thermal) |

## Disabled in this tree (need an overlay or our own enabling)
v3d, hvs, hdmi0/1, dsi/csi, vec, dpi (display: pv3d-*/pisound-design); all I2C buses; RP1 pwm1 +
cooling_fan (pirp1 drives it directly); RP1 I2S (pisound-design); pcie1; extra RP1 UART/SPI.

## Uncovered, worth a lane (priority order)
1. **pcie0 external x1 + NVMe** (`pinvme`): the NVMe HAT is the fast root disk; reuses the pipcie
   root-complex code and the existing NVMe driver.
2. **BCM2712 GPIO/pinctrl + l2 intc** (`pigpio`): brcmstb GPIO, aon pinctrl, the two l2-intc;
   prerequisite for Bluetooth (BT_REG_ON, uarta IRQ) and the SD 1.8 V regulator.
3. **Bluetooth HCI over uarta** (`pibt`): after pisound-design and pigpio; CYW43455 BT patchram.
4. **OTP / board identity** (`piotp`, small): serial and MAC via firmware tags (GET_BOARD_SERIAL,
   GET_BOARD_MAC) for pigem's MAC and /etc/machine-id; could be a pimisc follow-up.
5. **RP1 ADC temperature** (small): second sensor next to `thermal::soc_millicelsius()`; fold into pirp1.
6. Low: HEVC decoder + IOMMUs (media), PiSP (camera), RP1 PIO/DMA, spi10.
