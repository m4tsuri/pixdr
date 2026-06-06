#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

pixdr_require_cmd adb >/dev/null

# Keep logs useful for app debugging; avoid noisy wgpu shader dumps and system UI spam.
adb logcat "$@" | grep --line-buffered -E \
  'pixdr|RustStdoutStderr|B200|B210|UHD|USB|GET_COMPAT|Loading firmware|Loading FPGA|Operating over USB|Register loopback|SDR stream|RX FFT|FATAL|panic|libusb|UsbHostManager' \
  | grep --line-buffered -v -E 'naga::|wgpu_hal::gles::device: Naga generated shader|android_activity::activity_impl::glue: (Start|Resume|Pause|Stop|SaveInstanceState|WindowFocusChanged)'
