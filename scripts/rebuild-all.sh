#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "${SCRIPT_DIR}/env.sh"

pixdr_require_cmd make cargo cargo-ndk adb >/dev/null

LIBUSB_BUILD="${PIXDR_BUILD_DIR}/libusb-android"
UHD_BUILD="${PIXDR_BUILD_DIR}/uhd-android"

if [[ ! -d "${LIBUSB_BUILD}" || ! -d "${UHD_BUILD}" ]]; then
  echo "Native build directories not found. Running full native build first."
  "${SCRIPT_DIR}/build-native.sh"
else
  echo "=== 1. Rebuild libusb ==="
  make -C "${LIBUSB_BUILD}" -j"$(nproc)"
  make -C "${LIBUSB_BUILD}" install

  echo "=== 2. Rebuild UHD ==="
  make -C "${UHD_BUILD}" -j"$(nproc)"
  make -C "${UHD_BUILD}" install
fi

echo "=== 3. Build APK ==="
"${SCRIPT_DIR}/build-apk.sh"

echo "=== 4. Install and run ==="
"${SCRIPT_DIR}/install-run.sh"
