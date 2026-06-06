#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

mkdir -p \
  "${PIXDR_ROOT}/patches/uhd" \
  "${PIXDR_ROOT}/patches/libusb" \
  "${PIXDR_ROOT}/patches/uhd-rs" \
  "${PIXDR_ROOT}/patches/boost-android"

rm -f "${PIXDR_ROOT}"/patches/*/*.patch

git -C "${PIXDR_UHD_SRC}" format-patch -1 HEAD --stdout \
  > "${PIXDR_ROOT}/patches/uhd/0001-pixdr-add-android-usb-fd-support-for-b200.patch"

git -C "${PIXDR_LIBUSB_SRC}" format-patch -1 HEAD --stdout \
  > "${PIXDR_ROOT}/patches/libusb/0001-pixdr-support-android-wrapped-usbfs-fd-diagnostics.patch"

git -C "${PIXDR_UHDRS_SRC}" format-patch -1 HEAD --stdout \
  > "${PIXDR_ROOT}/patches/uhd-rs/0001-pixdr-support-android-uhd-cross-build.patch"

git -C "${PIXDR_BOOST_ANDROID_SRC}" format-patch -1 HEAD --stdout \
  > "${PIXDR_ROOT}/patches/boost-android/0001-pixdr-make-android-boost-build-non-interactive.patch"

if grep -R "${HOME}\|${PIXDR_ROOT}" "${PIXDR_ROOT}/patches" >/dev/null 2>&1; then
  echo "WARNING: local absolute path found in patches; inspect before publishing:" >&2
  grep -R "${HOME}\|${PIXDR_ROOT}" "${PIXDR_ROOT}/patches" >&2 || true
fi

wc -l "${PIXDR_ROOT}"/patches/*/*.patch
