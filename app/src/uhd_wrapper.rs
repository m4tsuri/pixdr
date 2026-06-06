//! B210 initialization via UHD with Android USB fd+path support.

use anyhow::{Result, bail};
use log::info;
use std::ffi::CString;

extern "C" {
    static mut g_android_usb_fd: isize;
    static mut g_android_usb_path: *const std::ffi::c_char;
    static mut g_android_usb_vid: u16;
    static mut g_android_usb_pid: u16;
    static mut g_android_usb_fw_loaded: bool;
}

// Store the device path string so it lives long enough
static mut PATH_BUF: Option<CString> = None;
static mut PATH_STR: Option<String> = None;
static mut FW_ALREADY_LOADED: bool = false;

pub fn set_android_usb_ids(vid: u16, pid: u16) {
    unsafe {
        g_android_usb_vid = vid;
        g_android_usb_pid = pid;
    }
}

pub fn set_android_usb_firmware_loaded(loaded: bool) {
    unsafe {
        FW_ALREADY_LOADED = loaded;
        g_android_usb_fw_loaded = loaded;
    }
}

pub fn set_android_usb_path(path: &str) {
    // GrHardwareService passes the usbfs root (/dev/bus/usb), not the full
    // device node (/dev/bus/usb/001/002). Keep the full node for logging but
    // pass the root to UHD-style args.
    let usbfs_root = usbfs_root_from_device_name(path);
    let cs = CString::new(usbfs_root.clone()).unwrap();
    unsafe {
        g_android_usb_path = cs.as_ptr();
        PATH_BUF = Some(cs);
        PATH_STR = Some(usbfs_root);
    }
}

fn usbfs_root_from_device_name(path: &str) -> String {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() >= 4 && parts[0] == "dev" && parts[1] == "bus" && parts[2] == "usb" {
        "/dev/bus/usb".to_string()
    } else {
        path.to_string()
    }
}

fn android_usb_path() -> String {
    unsafe { PATH_STR.clone().unwrap_or_else(|| "/dev/bus/usb".to_string()) }
}

pub fn init_b210_with_fd(fd: i32) -> Result<uhd::Usrp> {
    let usbfs_path = android_usb_path();
    let args = format!("type=b200,fd={fd},usbfs_path={usbfs_path}");
    info!("Opening B210 with UHD args: {args}");
    unsafe {
        g_android_usb_fd = fd as isize;
        std::env::set_var("UHD_IMAGES_DIR", "/data/local/tmp/uhd-images");
    }

    let fw_already_loaded = unsafe { FW_ALREADY_LOADED };
    if !fw_already_loaded {
        // GrHardwareService behavior: device::find() is the firmware-load stage.
        // If FX3 firmware is not loaded, UHD's B200 finder loads usrp_b200_fw.hex
        // and the device will re-enumerate. In that case open() may not happen in
        // the same pass; the app loop will pick up the new fd after re-enumeration.
        match uhd::Usrp::find(&args) {
            Ok(addrs) => {
                info!("UHD find returned {} address(es): {:?}", addrs.len(), addrs);
                if addrs.is_empty() {
                    unsafe { g_android_usb_fd = -1; }
                    bail!("UHD find completed with no openable device yet; waiting for USB re-enumeration");
                }
            }
            Err(e) => {
                if let Some(msg) = uhd::last_error_message() {
                    info!("UHD find error detail: {msg}");
                }
                unsafe { g_android_usb_fd = -1; }
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
