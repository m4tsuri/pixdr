#![cfg(target_os = "android")]

use anyhow::Context;
use jni::{
    jni_sig, jni_str,
    objects::{JObject, JValue},
    sys::jobject,
    JavaVM,
};

pub fn open_file_picker(
    vm_ptr: *mut std::ffi::c_void,
    activity_ptr: *mut std::ffi::c_void,
    request_code: i32,
    mime_type: &str,
) -> anyhow::Result<()> {
    let vm = unsafe { JavaVM::from_raw(vm_ptr.cast::<jni::sys::JavaVM>()) };
    vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let activity = unsafe { JObject::from_raw(env, activity_ptr as jobject) };
        let mime_type = env.new_string(mime_type)?;
        env.call_method(
            &activity,
            jni_str!("openFilePicker"),
            jni_sig!("(ILjava/lang/String;)V"),
            &[JValue::Int(request_code), JValue::Object(&mime_type)],
        )?;
        Ok(())
    })
    .context("calling PixdrActivity.openFilePicker failed")
}
