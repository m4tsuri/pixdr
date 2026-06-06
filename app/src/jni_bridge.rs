//! JNI bridge: injects JVM + UsbDevice + UsbDeviceConnection into UHD C++.

use jni::{JavaVM, objects::Global};
use log::info;
use std::sync::OnceLock;

static JNI_STATE: OnceLock<JniState> = OnceLock::new();

struct JniState {
    _jvm: JavaVM,
    _dev: Global<jni::objects::JObject<'static>>,
    _conn: Global<jni::objects::JObject<'static>>,
}

extern "C" {
    fn android_jni_init(
        jvm: *mut std::ffi::c_void,
        usb_device: jni::sys::jobject,
        usb_conn: jni::sys::jobject,
    );
}

pub(crate) fn store_jni_state(
    jvm: JavaVM,
    usb_device: Global<jni::objects::JObject<'static>>,
    usb_connection: Global<jni::objects::JObject<'static>>,
) {
    let vm_ptr = jvm.get_raw() as *mut std::ffi::c_void;
    let dev_raw = usb_device.as_raw();
    let conn_raw = usb_connection.as_raw();
    unsafe { android_jni_init(vm_ptr, dev_raw, conn_raw); }

    JNI_STATE.set(JniState { _jvm: jvm, _dev: usb_device, _conn: usb_connection }).ok();
    info!("JNI state injected into UHD C++ transport layer");
}
