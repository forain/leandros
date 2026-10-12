use crate::iwl_fw::*;

fn tlv(out: &mut Vec<u8>, t: u32, data: &[u8]) {
    out.extend_from_slice(&t.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 { out.push(0); }
}

fn sec(addr: u32, body: &[u8]) -> Vec<u8> {
    let mut v = addr.to_le_bytes().to_vec();
    v.extend_from_slice(body);
    v
}

fn header() -> Vec<u8> {
    let mut d = vec![0u8; HEADER_LEN];
    d[4..8].copy_from_slice(&MAGIC.to_le_bytes());
    d[8..16].copy_from_slice(b"test fw\0");
    d[72..76].copy_from_slice(&0x0000_2400u32.to_le_bytes());
    d
}

#[test]
fn parses_synthetic_dual_cpu_paged_image() {
    let mut d = header();
    tlv(&mut d, TLV_PHY_SKU, &0x0033_0018u32.to_le_bytes());
    tlv(&mut d, TLV_API_CHANGES_SET, &[0, 0, 0, 0, 0x00, 0x03, 0, 0x40]);
    tlv(&mut d, TLV_ENABLED_CAPABILITIES, &[2, 0, 0, 0, 0, 0x02, 0, 0]);
    tlv(&mut d, TLV_NUM_OF_CPU, &2u32.to_le_bytes());
    tlv(&mut d, TLV_PAGING, &(39u32 * 4096).to_le_bytes());
    tlv(&mut d, TLV_SEC_RT, &sec(0x404000, &[1, 2, 3]));
    tlv(&mut d, TLV_SEC_RT, &sec(0x800000, &[4; 8]));
    tlv(&mut d, TLV_SEC_RT, &sec(CPU1_CPU2_SEPARATOR, &[0; 4]));
    tlv(&mut d, TLV_SEC_RT, &sec(0x405000, &[5; 5]));
    tlv(&mut d, TLV_SEC_RT, &sec(PAGING_SEPARATOR, &[0; 4]));
    tlv(&mut d, TLV_SEC_RT, &sec(0x440000, &[6; 6]));
    tlv(&mut d, TLV_SEC_RT, &sec(0x1000000, &[7; 12]));
    tlv(&mut d, TLV_SEC_INIT, &sec(0x404000, &[9; 2]));
    tlv(&mut d, TLV_DEF_CALIB, &[1, 0, 0, 0, 0x11, 0, 0, 0, 0x22, 0, 0, 0]);
    tlv(&mut d, TLV_CMD_VERSIONS, &[0x18, 0, 10, 0, 0x03, 4, 1, 0]);
    tlv(&mut d, 999, &[1, 2, 3, 4, 5]);

    let fw = parse(&d).unwrap();
    assert_eq!(fw.human, "test fw");
    assert_eq!(fw.api_major(), 0x24);
    assert_eq!((fw.valid_tx_ant, fw.valid_rx_ant), (3, 3));
    assert!(fw.has_api(api::FRAGMENTED_SCAN) && fw.has_api(api::WIFI_MCC_UPDATE) && fw.has_api(api::STA_TYPE));
    assert!(!fw.has_api(api::NEW_VERSION));
    assert!(fw.has_capa(capa::LAR_SUPPORT_V3) && !fw.has_capa(capa::LAR_SUPPORT));
    assert_eq!(fw.num_cpus, 2);
    assert_eq!(fw.paging_mem_size, 39 * 4096);
    assert_eq!(paging_layout(fw.paging_mem_size as usize), (5, 7));
    assert_eq!(fw.default_calib[IMG_INIT], (0x11, 0x22));
    assert_eq!(fw.cmd_ver(0, 0x18), Some(10));
    assert_eq!(fw.cmd_ver(4, 3), Some(1));
    assert_eq!(fw.cmd_ver(0, 0x19), None);

    let rt = &fw.images[IMG_REGULAR];
    assert_eq!(rt.sections.len(), 7);
    let s0 = rt.sections[0];
    assert_eq!((s0.offset, s0.len), (0x404000, 3));
    assert_eq!(&d[s0.start..s0.start + s0.len], &[1, 2, 3]);
    let plan = rt.load_plan();
    assert_eq!(plan, LoadPlan { cpu1: vec![0, 1], cpu2: vec![3], paging: Some((5, 6)) });
    let init = fw.images[IMG_INIT].load_plan();
    assert_eq!(init, LoadPlan { cpu1: vec![0], cpu2: vec![], paging: None });
}

#[test]
fn rejects_bad_files() {
    assert!(parse(&[0u8; 10]).is_err());
    let mut d = header();
    d[4] = 0;
    assert!(parse(&d).is_err());
    let mut d = header();
    tlv(&mut d, TLV_SEC_INIT, &sec(0, &[1]));
    assert_eq!(parse(&d).unwrap_err(), "no runtime (SEC_RT) image");
    let mut d = header();
    d.extend_from_slice(&TLV_SEC_RT.to_le_bytes());
    d.extend_from_slice(&1000u32.to_le_bytes());
    d.extend_from_slice(&[0; 8]);
    assert!(parse(&d).is_err());
    let mut d = header();
    tlv(&mut d, TLV_PAGING, &4097u32.to_le_bytes());
    assert!(parse(&d).is_err());
}

/// The real linux-firmware file, when it has been fetched.
#[test]
fn parses_real_8265_firmware() {
    let path = std::env::var("IWL_FW").unwrap_or_else(|_| {
        format!("{}/.cache/leandros/iwlwifi/iwlwifi-8265-36.ucode", std::env::var("HOME").unwrap_or_default())
    });
    let d = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => { eprintln!("skipping: {} not present", path); return; }
    };
    let fw = parse(&d).unwrap();
    assert_eq!(fw.api_major(), 36);
    assert_eq!(fw.num_cpus, 2);
    assert_eq!(fw.paging_mem_size, 0x27000);
    assert!(fw.has_capa(capa::UMAC_SCAN) && fw.has_capa(capa::DQA_SUPPORT) && fw.has_capa(capa::LAR_SUPPORT));
    assert!(!fw.has_capa(capa::TLC_OFFLOAD) && !fw.has_capa(capa::SESSION_PROT_CMD));
    assert!(fw.has_api(api::STA_TYPE) && fw.has_api(api::NEW_VERSION) && fw.has_api(api::ADAPTIVE_DWELL));
    assert!(!fw.has_api(api::ADAPTIVE_DWELL_V2));
    assert_eq!((fw.valid_tx_ant, fw.valid_rx_ant), (3, 3));
    let rt = fw.images[IMG_REGULAR].load_plan();
    assert_eq!(rt.cpu1.len(), 4);
    assert_eq!(rt.cpu2.len(), 4);
    let (css, data) = rt.paging.unwrap();
    assert_eq!(fw.images[IMG_REGULAR].sections[data].len, 0x27000);
    assert!(fw.images[IMG_REGULAR].sections[css].len <= FW_PAGING_SIZE);
    let init = fw.images[IMG_INIT].load_plan();
    assert_eq!(init.cpu1.len(), 5);
    assert_eq!(init.cpu2.len(), 4);
    // Command versions the driver depends on.
    assert_eq!(fw.cmd_ver(0, 0x18), Some(10)); // ADD_STA
    assert_eq!(fw.cmd_ver(0, 0x6a), Some(1)); // PHY_CONFIGURATION_CMD
    assert_eq!(fw.cmd_ver(0, 0x0d), Some(7)); // SCAN_REQ_UMAC
}
