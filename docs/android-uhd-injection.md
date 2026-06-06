# Android USB context injection into UHD

pixdr does not mutate exported UHD global variables to pass Android USB state.
Instead, the app injects a context object into the UHD fork.

## Boundary

- UHD declares `uhd::transport::android_usb_context` in
  `uhd/transport/android_usb_context.hpp`.
- UHD discovers an optional provider named `pixdr_uhd_android_usb_context` via a
  weak symbol and `dlsym(RTLD_DEFAULT, ...)` fallback.
- The provider returns `nullptr`/`fd < 0` when pixdr has no authorized Android
  USB device. In that case UHD uses its normal libusb enumeration path.

## App-side provider

- Rust owns the mutable state in `app/src/android_uhd_context.rs`.
- `cxx::bridge` exposes read-only Rust accessors to C++.
- `app/src/uhd_android_context.cc` implements a thin C++ adapter deriving from
  UHD's `android_usb_context`.
- Rust exports the final `pixdr_uhd_android_usb_context` symbol so it remains
  visible from the `cdylib`; the C++ factory itself can stay local.

## Injected state

- Android `UsbDeviceConnection` fd
- usbfs root path, usually `/dev/bus/usb`
- USB VID/PID
- FX3 firmware-loaded state

This keeps Android-specific state at the application edge and leaves UHD with a
small optional host-context interface rather than process-wide mutable globals.
