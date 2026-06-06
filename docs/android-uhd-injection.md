# Android USB context injection into UHD

pixdr does not mutate exported UHD global variables and does not patch libusb for
Android fd compatibility. Android-specific USB behavior lives in the UHD fork as
an optional native usbfs transport.

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

## UHD-native Android usbfs transport

When an Android context is present, B200 discovery/open uses
`host/lib/transport/android_usbfs.cpp` instead of pretending the Android fd is a
normal libusb device.

Implemented operations:

- control transfers via `USBDEVFS_CONTROL`
- bulk transfers via `USBDEVFS_BULK`
- interface claim/release via `USBDEVFS_CLAIMINTERFACE` and
  `USBDEVFS_RELEASEINTERFACE`
- endpoint clear via `USBDEVFS_CLEAR_HALT`
- device reset via `USBDEVFS_RESET`

The regular libusb transport remains available for desktop/traditional USB
paths. libusb is now an upstream dependency, not an Android compatibility layer.

## Future performance work

The first native backend uses blocking `USBDEVFS_BULK` calls to satisfy UHD's
`zero_copy_if` contract. If sustained sample rate becomes the bottleneck, replace
the bulk implementation with an async URB queue using `USBDEVFS_SUBMITURB`,
`USBDEVFS_REAPURB`, and `USBDEVFS_DISCARDURB`; this should be an internal change
to `android_usbfs.cpp` without changing the app/UHD context boundary.
