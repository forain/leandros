//! `CONFIG_IWLWIFI=n`: the entry points other code calls. `scan_lines` still
//! serves /proc/net/wifi for whichever other Wi-Fi backend is built.
use alloc::string::String;
pub fn kexec_stop() -> Option<String> { None }
pub fn scan_lines(_out: &mut String) {
    #[cfg(feature = "wifi-stack")]
    crate::wifi_class::scan_lines(_out);
}
