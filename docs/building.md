# Building pixdr

## Supported script entrypoints

Use the scripts in the repository root:

| Script | Purpose |
| --- | --- |
| `scripts/env.sh` | Shared environment variables for all scripts |
| `scripts/build-native.sh` | Slow from-scratch Boost/libusb/UHD Android build |
| `scripts/build-rust.sh` | Build `libpixdr.so` with `cargo ndk` |
| `scripts/package-apk.sh` | Package/sign `app/pixdr-debug.apk` |
| `scripts/build-apk.sh` | Build Rust + package APK |
| `scripts/install-run.sh` | Install APK, clear logcat, start NativeActivity |
| `scripts/logcat.sh` | Filtered pixdr/UHD/USB logs |
| `scripts/rebuild-all.sh` | Fast native rebuild + APK build/install/run |

## External dependencies

`external/` contains forked dependency repos. In a public release these should be git submodules:

- `external/uhd` -> `m4tsuri/uhd`, branch `pixdr-android-uhd-4.10`
- `external/libusb` -> `m4tsuri/libusb`, branch `pixdr-android`
- `external/uhd-rs` -> `m4tsuri/uhd-rust`, branch `pixdr-android`
- `external/boost-android` -> `m4tsuri/Boost-for-Android`, branch `pixdr-android`

Generated/local-heavy files go under `build/` and are ignored by `.gitignore`:

- `build/native/android-ndk-*`
- `build/native/toolchain/`
- `build/native/libusb-android/`
- `build/native/uhd-android/`
- `build/native/sources/`

`patches/` contains `git format-patch` exports for review/auditing.

## Build order

```bash
scripts/build-native.sh   # slow; only needed initially or after native dependency changes
scripts/fetch-uhd-images.sh
scripts/build-apk.sh
scripts/install-run.sh
scripts/logcat.sh
```

Fast rebuild after native source edits:

```bash
scripts/rebuild-all.sh
```

App-only rebuild:

```bash
scripts/build-apk.sh && scripts/install-run.sh
```

## Firmware/FPGA images

Fetch runtime images with:

```bash
scripts/fetch-uhd-images.sh
```

The package script copies these generated files from `app/assets/` when present:

- `usrp_b200_fw.hex`
- `usrp_b200_fpga.bin`
- `usrp_b210_fpga.bin`

The generated image files are ignored by git until redistribution licensing is decided.

The APK also still sets `UHD_IMAGES_DIR=/data/local/tmp/uhd-images` in the app code for the current UHD flow. During development, keep the same images available there as well if needed:

```bash
adb shell mkdir -p /data/local/tmp/uhd-images
adb push app/assets/usrp_b200_fw.hex /data/local/tmp/uhd-images/
adb push app/assets/usrp_b210_fpga.bin /data/local/tmp/uhd-images/
```

## Troubleshooting

- `libuhd.so not found`: run `scripts/build-native.sh`, or set `PIXDR_PREFIX` to an existing Android UHD prefix.
- Android build-tools missing: install `build-tools;35.0.0` and `platforms;android-29` with `sdkmanager`.
- B210 firmware loads but does not re-enumerate: use a powered USB-C hub/dock.
- `B210 opened; RX stream failed`: UHD opened the device, but RX streamer creation/start failed. Restart app or replug while this Android USB streaming path is still being hardened.
