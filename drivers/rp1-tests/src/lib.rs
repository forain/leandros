extern crate alloc;
#[path = "../../src/rp1_hw.rs"]
pub mod rp1_hw;
#[path = "../../src/brcmstb_hw.rs"]
pub mod brcmstb_hw;

#[cfg(test)]
mod tests {
    use super::rp1_hw::*;

    /// pll_sys as the DTB assigns it: VCO 1 GHz, pll_sys 200 MHz, sec 125 MHz.
    fn sys_regs() -> PllRegs {
        PllRegs { cs: PLL_CS_LOCK | 1, pwr: 0, fbdiv_int: 20, fbdiv_frac: 0,
                  prim: 0x5_1000, sec: 0x800 }
    }

    #[test]
    fn pll_rates_from_registers() {
        let r = pll_rates(&sys_regs(), XOSC_HZ);
        assert_eq!(r, PllRates { core: 1_000_000_000, pri: 200_000_000, pri_ph: 100_000_000, sec: 125_000_000, locked: true, powered: true });
        // Audio core 1.536 GHz = 50 MHz * 30.72: fractional feedback divider.
        let a = PllRegs { fbdiv_int: 30, fbdiv_frac: (0.72f64 * (1u64 << 24) as f64).round() as u32, ..sys_regs() };
        assert!((pll_core_hz(&a, XOSC_HZ) as i64 - 1_536_000_000).abs() < 4);
        // Powered down, sec held in reset, no post-divider: all zero.
        assert_eq!(pll_rates(&PllRegs { pwr: PLL_PWR_PD, ..sys_regs() }, XOSC_HZ).core, 0);
        assert_eq!(pll_rates(&PllRegs { sec: PLL_SEC_RST | (8 << 8), ..sys_regs() }, XOSC_HZ).sec, 0);
        assert_eq!(pll_rates(&PllRegs { prim: 0, ..sys_regs() }, XOSC_HZ).pri, 0);
        assert_eq!(Pll::Sys.off() + CLOCKS_BASE, 0x2_0000);
        assert_eq!(Pll::Video.off() + CLOCKS_BASE + PLL_SEC, 0x2_8014);
    }

    #[test]
    fn clock_divider_math() {
        assert_eq!(clk_div_16_16(1, 0), 1 << 16);
        assert_eq!(clk_div_16_16(0, 0), 1 << 32); // DIV_INT 0 = 2^16
        assert_eq!(clk_rate(200_000_000, 4, 0), 50_000_000);
        assert_eq!(clk_rate(200_000_000, 2, 0x8000_0000), 80_000_000); // 2.5
        assert_eq!(clk_choose_div(125_000_000, 125_000_000, false, 0xff), Some((1, 0)));
        assert_eq!(clk_choose_div(125_000_000, 25_000_000, false, 0xff), Some((5, 0)));
        assert_eq!(clk_choose_div(200_000_000, 80_000_000, true, 0xffff), Some((2, 0x8000_0000)));
        assert_eq!(clk_choose_div(200_000_000, 80_000_000, false, 0xff), Some((3, 0))); // 2.5 rounds up
        assert_eq!(clk_choose_div(50_000_000, 100_000_000, false, 0xff), None);
        assert_eq!(clk_choose_div(1_000_000_000, 1_000_000, false, 0xff), None); // > 8-bit max
        assert_eq!((CLK_ETH.div_int(), CLK_ETH.sel(), CLK_PWM1.div_frac(), CLK_ETH_TSU.ctrl), (0x68, 0x70, 0x8c, 0x134));
    }

    #[test]
    fn clock_plans_match_dtb_assigned_rates() {
        let r = pll_rates(&sys_regs(), XOSC_HZ);
        let rate_of = |p: Parent| match p { Parent::Xosc => XOSC_HZ, Parent::PllSys => r.pri, Parent::PllSysSec => r.sec, _ => 0 };
        assert_eq!(clk_plan(&CLK_ETH, &rate_of), Some((0, 1, 0))); // pll_sys_sec / 1
        assert_eq!(clk_plan(&CLK_ETH_TSU, &rate_of), Some((0, 1, 0))); // xosc / 1
        assert_eq!(clk_plan(&CLK_PWM1, &rate_of), Some((2, 1, 0))); // aux 2 = xosc
        // No pll_sys_sec: clk_eth falls back to pll_sys / ... 200 MHz cannot make 125 → None.
        let no_sec = |p: Parent| if p == Parent::PllSysSec { 0 } else { rate_of(p) };
        assert_eq!(clk_plan(&CLK_ETH, &no_sec), None);
        assert_eq!(clk_ctrl_enable(0x3e0 | 3, 2), (2 << 5) | 3 | CLK_CTRL_ENABLE);
        assert_eq!(clk_parent(&CLK_ETH, 1 << 5, 0), Parent::PllSys);
        assert_eq!(clk_parent(&CLK_PWM1, 2 << 5, 1), Parent::Xosc);
        assert_eq!(clk_parent(&CLK_SYS, 0, 1 << 2), Parent::PllSys); // SEL one-hot
        assert_eq!(clk_parent(&CLK_SYS, 0, 0), Parent::Xosc); // SEL 0: CTRL.SRC
    }

    #[test]
    fn gpio_registers() {
        let r = gpio_regs(32).unwrap(); // ETH_RST_N: bank 1 pin 4
        assert_eq!(r, GpioRegs { status: 0xd_4020, ctrl: 0xd_4024, pad: 0xf_4014, rio: 0xe_4000, bit: 1 << 4 });
        let r = gpio_regs(45).unwrap(); // FAN_PWM: bank 2 pin 11
        assert_eq!((r.ctrl, r.pad, r.rio, r.bit), (0xd_805c, 0xf_8030, 0xe_8000, 1 << 11));
        assert_eq!(gpio_regs(0).unwrap().ctrl, 0xd_0004);
        assert!(gpio_regs(53).is_some() && gpio_regs(54).is_none());
        // Same layout pipcie's ad-hoc PHY reset used, for every pin.
        for g in 0..54 {
            let r = gpio_regs(g).unwrap();
            assert_eq!(super::brcmstb_hw::rp1_gpio_regs(g), Some((r.ctrl, r.rio, r.pad, r.bit)), "gpio {g}");
        }
        assert_eq!(gpio_ctrl_fsel(0xffff_ffff, FSEL_GPIO), 0xffff_0fe5);
        assert_eq!(gpio_pad_active(PAD_OD | PAD_PULL_MASK), PAD_IE | PAD_PULL_MASK);
        assert_eq!(gpio_pad_pull(0xff, Pull::Down), 0xf3 | 0x4);
        assert_eq!(gpio_pad_pull(0, Pull::Up), 0x8);
    }

    #[test]
    fn fan_pwm_values() {
        assert_eq!((pwm_chan_ctrl(3), pwm_range(3), pwm_duty(3)), (0x44, 0x48, 0x50));
        // 50 MHz → 20 ns per tick; 41566 ns period ≈ 24.06 kHz.
        let (range, _) = pwm_ticks(50_000_000, FAN_PERIOD_NS, 0);
        assert_eq!(range, 2078);
        let duty: Vec<u32> = FAN_LEVELS.iter().map(|&l| pwm_ticks(50_000_000, FAN_PERIOD_NS, fan_duty_ns(FAN_PERIOD_NS, l)).1).collect();
        assert_eq!(duty, vec![0, 611, 1019, 1426, 2038]);
        assert_eq!(fan_duty_ns(FAN_PERIOD_NS, 255), FAN_PERIOD_NS - 1);
        assert_eq!(PWM_CHANNEL_DEFAULT | PWM_POLARITY, 0x109);
    }

    #[test]
    fn fan_curve_with_hysteresis() {
        assert_eq!(fan_state(40_000, 0), 0);
        assert_eq!(fan_state(50_000, 0), 1);
        assert_eq!(fan_state(61_000, 1), 2);
        assert_eq!(fan_state(80_000, 0), 4);
        assert_eq!(fan_state(57_000, 2), 2); // within 5 °C of the 60 °C trip: hold
        assert_eq!(fan_state(54_000, 2), 1); // below 55: drop one
        assert_eq!(fan_state(30_000, 4), 0);
    }

    #[test]
    fn board_gpio_bits_and_debounce() {
        assert_eq!(gio_reg(PWR_KEY_GIO, GIO_DATA), (0x04, 1 << 20));
        assert_eq!(gio_reg(ACT_LED_AON_GPIO, GIO_IODIR), (0x08, 1 << 9));
        assert_eq!(gio_reg(33, GIO_DATA), (0x24, 1 << 1));
        let mut d = Debounce::default();
        assert_eq!(d.feed(true, 50, 50), None);
        assert_eq!(d.feed(false, 50, 50), None); // first low sample: candidate only
        assert_eq!(d.feed(true, 50, 50), None); // bounce
        assert_eq!(d.feed(false, 50, 50), None);
        assert_eq!(d.feed(false, 50, 50), Some(true)); // 50 ms stable low = pressed
        assert_eq!(d.feed(false, 50, 50), None);
        assert_eq!(d.feed(true, 50, 50), None);
        assert_eq!(d.feed(true, 50, 50), Some(false));
    }
}
