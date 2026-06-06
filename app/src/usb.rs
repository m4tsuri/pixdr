//! USB for B210 via JNI from NativeActivity.
//!
//! This module intentionally stays Java/Kotlin-free, but still uses the
//! standard Android UsbManager permission flow via JNI.

use jni::{
    jni_sig, jni_str, Env,
    objects::{Global, JObject, JObjectArray, JString, JValue},
    sys::jobject,
    JavaVM,
};
use log::{error, info, warn};
use std::os::raw::{c_int, c_ulong};
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex, OnceLock};

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

const USBDEVFS_RESET: c_ulong = 0x5514;

static USB_CONNECTION: OnceLock<Mutex<Option<Global<JObject<'static>>>>> = OnceLock::new();
static USB_PERMISSION_REQUESTED: AtomicBool = AtomicBool::new(false);
static FX3_RESET_REQUESTED: AtomicBool = AtomicBool::new(false);

const VID_ETTUS: i32 = 0x2500;
const PID_B200_B210: i32 = 0x0020;
const VID_CYPRESS: i32 = 0x04b4;
const PID_FX3_BOOTLOADER: i32 = 0x00f3;

const ACTION_USB_PERMISSION: &str = "org.pixdr.app.USB_PERMISSION";
const FLAG_MUTABLE: i32 = 0x0200_0000;
const FLAG_UPDATE_CURRENT: i32 = 0x0800_0000;

pub fn close_current_usb_connection(vm_ptr: *mut std::ffi::c_void) {
    let Some(m) = USB_CONNECTION.get() else { return; };
    let mut guard = m.lock().expect("USB_CONNECTION poisoned");
    let Some(conn) = guard.take() else { return; };
    let vm = unsafe { JavaVM::from_raw(vm_ptr.cast::<jni::sys::JavaVM>()) };
    let _ = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let _ = env.call_method(conn.as_obj(), jni_str!("close"), jni_sig!("()V"), &[])?;
        Ok(())
    });
}

pub fn open_b210_usb(
    vm_ptr: *mut std::ffi::c_void,
    activity_ptr: *mut std::ffi::c_void,
) -> Option<i32> {
    let vm = unsafe { JavaVM::from_raw(vm_ptr.cast::<jni::sys::JavaVM>()) };
    let res = vm.attach_current_thread(|env| -> jni::errors::Result<Option<i32>> {
        let activity = unsafe { JObject::from_raw(env, activity_ptr as jobject) };
        let usb_key = env.new_string("usb")?;
        let usb_mgr = env
            .call_method(
                &activity,
                jni_str!("getSystemService"),
                jni_sig!((java.lang.String) -> java.lang.Object),
                &[JValue::Object(&usb_key)],
            )?
            .l()?;

        let device_map = env
            .call_method(&usb_mgr, jni_str!("getDeviceList"), jni_sig!(() -> java.util.HashMap), &[])?
            .l()?;
        let values = env
            .call_method(&device_map, jni_str!("values"), jni_sig!(() -> java.util.Collection), &[])?
            .l()?;
        let arr = env
            .call_method(&values, jni_str!("toArray"), jni_sig!("()[Ljava/lang/Object;"), &[])?
            .l()?;
        let obj_array = unsafe { JObjectArray::<JObject>::from_raw(env, arr.as_raw()) };

        info!("Scanning {} USB devices...", obj_array.len(env)?);
        for i in 0..obj_array.len(env)? {
            let dev = obj_array.get_element(env, i)?;
            let vid = env
                .call_method(&dev, jni_str!("getVendorId"), jni_sig!("()I"), &[])?
                .i()?;
            let pid = env
                .call_method(&dev, jni_str!("getProductId"), jni_sig!("()I"), &[])?
                .i()?;
            if !is_b210_related(vid, pid) {
                continue;
            }

            let name_obj = env
                .call_method(&dev, jni_str!("getDeviceName"), jni_sig!("()Ljava/lang/String;"), &[])?
                .l()?;
            let name = {
                let jstr = JString::cast_local(env, name_obj)?;
                let s: String = env.get_string(&jstr)?.into();
                s
            };
            info!("  B210-related USB device vid=0x{vid:04x} pid=0x{pid:04x} path={name}");

            let has_permission = env
                .call_method(
                    &usb_mgr,
                    jni_str!("hasPermission"),
                    jni_sig!((android.hardware.usb.UsbDevice) -> boolean),
                    &[JValue::Object(&dev)],
                )?
                .z()?;
            if !has_permission {
                if !USB_PERMISSION_REQUESTED.swap(true, Ordering::SeqCst) {
                    warn!("No USB permission yet; requesting Android UsbManager permission dialog");
                    request_usb_permission(env, &activity, &usb_mgr, &dev)?;
                } else {
                    warn!("Waiting for Android USB permission grant");
                }
                return Ok(None);
            }
            USB_PERMISSION_REQUESTED.store(false, Ordering::SeqCst);

            let conn = env
                .call_method(
                    &usb_mgr,
                    jni_str!("openDevice"),
                    jni_sig!((android.hardware.usb.UsbDevice) -> android.hardware.usb.UsbDeviceConnection),
                    &[JValue::Object(&dev)],
                )?
                .l()?;
            if conn.is_null() {
                error!("UsbManager.openDevice() returned null despite permission");
                return Ok(None);
            }

            // Match GrHardwareService: Do not claim the interface in Java.
            // The fd is handed to UHD/libusb, which performs interface claims.
            let fd = env
                .call_method(&conn, jni_str!("getFileDescriptor"), jni_sig!("()I"), &[])?
                .i()?;
            info!("  USB fd={fd}");

            // Keep UsbDeviceConnection alive. The fd belongs to this Java object.
            let global_conn = env.new_global_ref(&conn)?;
            *USB_CONNECTION
                .get_or_init(|| Mutex::new(None))
                .lock()
                .expect("USB_CONNECTION poisoned") = Some(global_conn);

            if vid == VID_CYPRESS && pid == PID_FX3_BOOTLOADER {
                info!("  FX3 bootloader detected; loading FX3 firmware first");
                if load_fx3_firmware(env, &conn)? {
                    info!("  FX3 firmware load submitted; waiting for USB re-enumeration");
                }
                return Ok(None);
            }

            let fw_loaded = log_vendor_get_compat_probe(env, &conn)?;
            if !fw_loaded {
                // Match GrHardwareService: pass the fd into UHD and let
                // uhd::device::find() perform the FX3 firmware-load stage.
                warn!("  GET_COMPAT failed; handing fd to UHD firmware-load/find path");
            }
            FX3_RESET_REQUESTED.store(false, Ordering::SeqCst);

            crate::uhd_wrapper::set_android_usb_ids(vid as u16, pid as u16);
            crate::uhd_wrapper::set_android_usb_firmware_loaded(fw_loaded);
            crate::uhd_wrapper::set_android_usb_path(&name);
            return Ok(Some(fd));
        }
        Ok(None)
    });

    match res {
        Ok(Some(fd)) => Some(fd),
        Ok(None) => None,
        Err(e) => {
            error!("USB JNI error: {e:?}");
            None
        }
    }
}

fn is_b210_related(vid: i32, pid: i32) -> bool {
    (vid == VID_ETTUS && pid == PID_B200_B210)
        || (vid == VID_CYPRESS && pid == PID_FX3_BOOTLOADER)
}

fn log_vendor_get_compat_probe<'local>(
    env: &mut Env<'local>,
    conn: &JObject<'local>,
) -> jni::errors::Result<bool> {
    let buf = env.new_byte_array(4)?;
    let ret = control_transfer_in(env, conn, 0xC0, 0x15, 0, 0, &buf, 2, 1000)?;
    if ret == 2 {
        let mut bytes = [0i8; 4];
        env.get_byte_array_region(&buf, 0, &mut bytes)?;
        info!(
            "  Java UsbDeviceConnection GET_COMPAT ret=2 bytes={:02x} {:02x}",
            bytes[0] as u8,
            bytes[1] as u8
        );
        Ok(true)
    } else {
        warn!("  Java UsbDeviceConnection GET_COMPAT ret={ret}");
        Ok(false)
    }
}

fn close_usb_connection<'local>(
    env: &mut Env<'local>,
    conn: &JObject<'local>,
) -> jni::errors::Result<()> {
    let _ = env.call_method(conn, jni_str!("close"), jni_sig!("()V"), &[])?;
    Ok(())
}

fn request_fx3_bootloader_reset<'local>(
    env: &mut Env<'local>,
    conn: &JObject<'local>,
) -> jni::errors::Result<bool> {
    let zeros = [0u8; 4];
    let ret = control_transfer_out(env, conn, 0x40, 0x99, 0, 0, &zeros, 1000)?;
    if ret == zeros.len() as i32 {
        info!("  FX3 bootloader reset request accepted");
        Ok(true)
    } else {
        warn!("  FX3 bootloader reset request failed ret={ret}");
        Ok(false)
    }
}

fn load_fx3_firmware<'local>(
    env: &mut Env<'local>,
    conn: &JObject<'local>,
) -> jni::errors::Result<bool> {
    let path = "/data/local/tmp/uhd-images/usrp_b200_fw.hex";
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            error!("  Cannot read FX3 firmware {path}: {e}");
            return Ok(false);
        }
    };

    let mut base: u32 = 0;
    let mut records = 0usize;
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let bytes = match parse_ihex_line(line) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("  Bad Intel HEX line {}: {e}", line_no + 1);
                return Ok(false);
            }
        };
        if bytes.len() < 5 {
            error!("  Bad Intel HEX line {}: too short", line_no + 1);
            return Ok(false);
        }
        let len = bytes[0] as usize;
        if bytes.len() < 5 + len {
            error!("  Bad Intel HEX line {}: length mismatch", line_no + 1);
            return Ok(false);
        }
        let addr = ((bytes[1] as u16) << 8) | bytes[2] as u16;
        let rectype = bytes[3];
        let data = &bytes[4..4 + len];
        match rectype {
            0x00 => {
                let full_addr = base + addr as u32;
                let ret = control_transfer_out(
                    env,
                    conn,
                    0x40,
                    0xA0,
                    (full_addr & 0xffff) as i32,
                    ((full_addr >> 16) & 0xffff) as i32,
                    data,
                    1000,
                )?;
                if ret != len as i32 {
                    error!("  FX3 firmware write failed at 0x{full_addr:08x}: ret={ret}, len={len}");
                    return Ok(false);
                }
                records += 1;
            }
            0x01 => break,
            0x02 => {
                if data.len() != 2 {
                    error!("  Bad extended segment address at line {}", line_no + 1);
                    return Ok(false);
                }
                base = ((((data[0] as u16) << 8) | data[1] as u16) as u32) << 4;
            }
            0x04 => {
                if data.len() != 2 {
                    error!("  Bad extended linear address at line {}", line_no + 1);
                    return Ok(false);
                }
                base = ((((data[0] as u16) << 8) | data[1] as u16) as u32) << 16;
            }
            0x05 => {
                // Match UHD ihex_reader: Start Linear Address tells FX3 CPU
                // to jump to the loaded firmware entry point by issuing the
                // same 0xA0 request with zero-length payload.
                if data.len() != 4 {
                    error!("  Bad start linear address at line {}", line_no + 1);
                    return Ok(false);
                }
                let full_addr = ((data[0] as u32) << 24)
                    | ((data[1] as u32) << 16)
                    | ((data[2] as u32) << 8)
                    | data[3] as u32;
                let ret = control_transfer_out(
                    env,
                    conn,
                    0x40,
                    0xA0,
                    (full_addr & 0xffff) as i32,
                    ((full_addr >> 16) & 0xffff) as i32,
                    &[],
                    1000,
                )?;
                if ret != 0 {
                    error!("  FX3 firmware jump failed at 0x{full_addr:08x}: ret={ret}");
                    return Ok(false);
                }
                info!("  FX3 firmware jump address: 0x{full_addr:08x}");
            }
            _ => {}
        }
    }

    info!("  FX3 firmware records written: {records}");
    std::thread::sleep(std::time::Duration::from_millis(1000));
    Ok(records > 0)
}

fn control_transfer_in<'local>(
    env: &mut Env<'local>,
    conn: &JObject<'local>,
    request_type: i32,
    request: i32,
    value: i32,
    index: i32,
    buf: &jni::objects::JByteArray<'local>,
    len: i32,
    timeout_ms: i32,
) -> jni::errors::Result<i32> {
    env.call_method(
        conn,
        jni_str!("controlTransfer"),
        jni_sig!("(IIII[BII)I"),
        &[
            JValue::Int(request_type),
            JValue::Int(request),
            JValue::Int(value),
            JValue::Int(index),
            JValue::Object(buf),
            JValue::Int(len),
            JValue::Int(timeout_ms),
        ],
    )?
    .i()
}

fn control_transfer_out<'local>(
    env: &mut Env<'local>,
    conn: &JObject<'local>,
    request_type: i32,
    request: i32,
    value: i32,
    index: i32,
    data: &[u8],
    timeout_ms: i32,
) -> jni::errors::Result<i32> {
    let buf = env.new_byte_array(data.len())?;
    let signed: Vec<i8> = data.iter().map(|b| *b as i8).collect();
    env.set_byte_array_region(&buf, 0, &signed)?;
    env.call_method(
        conn,
        jni_str!("controlTransfer"),
        jni_sig!("(IIII[BII)I"),
        &[
            JValue::Int(request_type),
            JValue::Int(request),
            JValue::Int(value),
            JValue::Int(index),
            JValue::Object(&buf),
            JValue::Int(data.len() as i32),
            JValue::Int(timeout_ms),
        ],
    )?
    .i()
}

fn parse_ihex_line(line: &str) -> Result<Vec<u8>, String> {
    if !line.starts_with(':') {
        return Err("missing ':'".to_string());
    }
    let hex = &line[1..];
    if hex.len() % 2 != 0 {
        return Err("odd number of hex digits".to_string());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let b = u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|e| format!("invalid hex byte: {e}"))?;
        out.push(b);
    }
    let checksum = out.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    if checksum != 0 {
        return Err("checksum mismatch".to_string());
    }
    Ok(out)
}

fn request_usb_permission<'local>(
    env: &mut Env<'local>,
    activity: &JObject<'local>,
    usb_mgr: &JObject<'local>,
    dev: &JObject<'local>,
) -> jni::errors::Result<()> {
    let action = env.new_string(ACTION_USB_PERMISSION)?;
    let intent = env.new_object(
        jni_str!("android/content/Intent"),
        jni_sig!((java.lang.String) -> void),
        &[JValue::Object(&action)],
    )?;

    // Restrict the PendingIntent to this package.
    let pkg = env
        .call_method(activity, jni_str!("getPackageName"), jni_sig!("()Ljava/lang/String;"), &[])?
        .l()?;
    let _ = env.call_method(
        &intent,
        jni_str!("setPackage"),
        jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&pkg)],
    )?;

    let flags = FLAG_MUTABLE | FLAG_UPDATE_CURRENT;
    let pending = env
        .call_static_method(
            jni_str!("android/app/PendingIntent"),
            jni_str!("getBroadcast"),
            jni_sig!("(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;"),
            &[
                JValue::Object(activity),
                JValue::Int(0),
                JValue::Object(&intent),
                JValue::Int(flags),
            ],
        )?
        .l()?;

    env.call_method(
        usb_mgr,
        jni_str!("requestPermission"),
        jni_sig!("(Landroid/hardware/usb/UsbDevice;Landroid/app/PendingIntent;)V"),
        &[JValue::Object(dev), JValue::Object(&pending)],
    )?;
    Ok(())
}
