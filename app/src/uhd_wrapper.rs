//! B210 initialization via UHD with Android USB fd+path support.

use anyhow::{Result, bail};
use log::info;

use crate::android_uhd_context;

pub fn set_android_usb_ids(vid: u16, pid: u16) {
    android_uhd_context::set_ids(vid, pid);
}

pub fn set_android_usb_firmware_loaded(loaded: bool) {
    android_uhd_context::set_firmware_loaded(loaded);
}

pub fn set_android_usb_path(path: &str) {
    // GrHardwareService passes the usbfs root (/dev/bus/usb), not the full
    // device node (/dev/bus/usb/001/002). Keep the full node for logging but
    // pass the root to UHD-style args.
    android_uhd_context::set_usbfs_path(usbfs_root_from_device_name(path));
}

fn usbfs_root_from_device_name(path: &str) -> String {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() >= 4 && parts[0] == "dev" && parts[1] == "bus" && parts[2] == "usb" {
        "/dev/bus/usb".to_string()
    } else {
        path.to_string()
    }
}

pub fn init_b210_with_fd(fd: i32) -> Result<uhd::Usrp> {
    let usbfs_path = android_uhd_context::usbfs_path();
    let args = format!("type=b200,fd={fd},usbfs_path={usbfs_path}");
    info!("Opening B210 with UHD args: {args}");
    android_uhd_context::set_fd(fd);
    unsafe {
        std::env::set_var("UHD_IMAGES_DIR", "/data/local/tmp/uhd-images");
    }

    if !android_uhd_context::firmware_loaded() {
        // GrHardwareService behavior: device::find() is the firmware-load stage.
        // If FX3 firmware is not loaded, UHD's B200 finder loads usrp_b200_fw.hex
        // and the device will re-enumerate. In that case open() may not happen in
        // the same pass; the app loop will pick up the new fd after re-enumeration.
        match uhd::Usrp::find(&args) {
            Ok(addrs) => {
                info!("UHD find returned {} address(es): {:?}", addrs.len(), addrs);
                if addrs.is_empty() {
                    android_uhd_context::clear_fd();
                    bail!("UHD find completed with no openable device yet; waiting for USB re-enumeration");
                }
            }
            Err(e) => {
                if let Some(msg) = uhd::last_error_message() {
                    info!("UHD find error detail: {msg}");
                }
                android_uhd_context::clear_fd();
                bail!("UHD find failed: {e}");
            }
        }
    } else {
        info!("FX3 firmware already responds to GET_COMPAT; skipping UHD find firmware-load stage");
    }

    match uhd::Usrp::open(&args) {
        Ok(usrp) => {
            let name = usrp.get_motherboard_name(0)?;
            let rx = usrp.get_num_rx_channels()?;
            let tx = usrp.get_num_tx_channels()?;
            info!("B210 OPENED: {name} (RX={rx}, TX={tx})");
            Ok(usrp)
        }
        Err(e) => {
            if let Some(msg) = uhd::last_error_message() {
                info!("UHD error: {msg}");
            }
            bail!("Failed: {e}")
        }
    }
}
