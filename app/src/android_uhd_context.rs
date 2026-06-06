//! Android USB context injected into UHD through a C++ weak provider.

use std::{ffi::c_void, sync::{Mutex, OnceLock}};

#[derive(Clone, Debug)]
struct AndroidUsbContextState {
    fd: isize,
    usbfs_path: String,
    vid: u16,
    pid: u16,
    firmware_loaded: bool,
}

impl Default for AndroidUsbContextState {
    fn default() -> Self {
        Self {
            fd: -1,
            usbfs_path: "/dev/bus/usb".to_string(),
            vid: 0,
            pid: 0,
            firmware_loaded: false,
        }
    }
}

static ANDROID_USB_CONTEXT: OnceLock<Mutex<AndroidUsbContextState>> = OnceLock::new();

unsafe extern "C" {
    fn pixdr_make_uhd_android_usb_context() -> *const c_void;
}

#[unsafe(no_mangle)]
pub extern "C" fn pixdr_uhd_android_usb_context() -> *const c_void {
    unsafe { pixdr_make_uhd_android_usb_context() }
}

fn state() -> &'static Mutex<AndroidUsbContextState> {
    ANDROID_USB_CONTEXT.get_or_init(|| Mutex::new(AndroidUsbContextState::default()))
}

pub fn set_ids(vid: u16, pid: u16) {
    let mut s = state().lock().unwrap();
    s.vid = vid;
    s.pid = pid;
}

pub fn set_firmware_loaded(loaded: bool) {
    state().lock().unwrap().firmware_loaded = loaded;
}

pub fn set_usbfs_path(path: String) {
    state().lock().unwrap().usbfs_path = path;
}

pub fn set_fd(fd: i32) {
    // Force the app-side C++ provider object into libpixdr.so. Without this
    // reference, the linker may discard the object because libuhd discovers it
    // dynamically via weak symbol/dlsym rather than through a direct relocation.
    let _ = pixdr_uhd_android_usb_context();
    state().lock().unwrap().fd = fd as isize;
}

pub fn clear_fd() {
    state().lock().unwrap().fd = -1;
}

pub fn firmware_loaded() -> bool {
    state().lock().unwrap().firmware_loaded
}

pub fn usbfs_path() -> String {
    state().lock().unwrap().usbfs_path.clone()
}

#[cxx::bridge(namespace = "pixdr")]
mod ffi {
    extern "Rust" {
        fn android_usb_fd() -> isize;
        fn android_usbfs_path() -> String;
        fn android_usb_vid() -> u16;
        fn android_usb_pid() -> u16;
        fn android_usb_firmware_loaded() -> bool;
    }
}

fn android_usb_fd() -> isize {
    state().lock().unwrap().fd
}

fn android_usbfs_path() -> String {
    state().lock().unwrap().usbfs_path.clone()
}

fn android_usb_vid() -> u16 {
    state().lock().unwrap().vid
}

fn android_usb_pid() -> u16 {
    state().lock().unwrap().pid
}

fn android_usb_firmware_loaded() -> bool {
    state().lock().unwrap().firmware_loaded
}
