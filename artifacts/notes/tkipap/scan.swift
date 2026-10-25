import CoreWLAN
let i = CWWiFiClient.shared().interface()!
let nets = try! i.scanForNetworks(withSSID: nil)
for n in nets { if let c = n.wlanChannel, c.channelNumber == 40 || n.supportsSecurity(.wpaPersonalMixed) {
  print(c.channelNumber, c.channelWidth.rawValue, n.rssiValue, n.ssid ?? "?", n.supportsSecurity(.wpaPersonalMixed) ? "MIXED" : "-") } }
