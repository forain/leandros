//! `CONFIG_IWLWIFI=n`: the entry points other code calls. `scan_lines` still
//! serves /proc/net/wifi for whichever other Wi-Fi backend is built.
use alloc::string::String;
pub fn kexec_stop() -> Option<String> { None }
pub fn scan_lines(_out: &mut String) {
    #[cfg(feature = "wifi-sim")]
    if crate::wifi_sim::present() { return crate::wifi_sim::scan_lines(_out); }
    #[cfg(feature = "brcmf")]
    if crate::brcmf::netdev::present() { return crate::brcmf::netdev::MGR.scan_lines("brcmfmac", _out); }
}
