#!/usr/bin/env bash
# Shared build environment for pixdr.
# Source this file from other scripts; do not execute directly.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  echo "env.sh is meant to be sourced, not executed" >&2
  exit 1
fi

PIXDR_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIXDR_APP_DIR="${PIXDR_ROOT}/app"
PIXDR_EXTERNAL="${PIXDR_ROOT}/external"
PIXDR_BUILD_DIR="${PIXDR_ROOT}/build/native"
PIXDR_UHD_SRC="${PIXDR_EXTERNAL}/uhd"
PIXDR_LIBUSB_SRC="${PIXDR_EXTERNAL}/libusb"
PIXDR_UHDRS_SRC="${PIXDR_EXTERNAL}/uhd-rs"
PIXDR_BOOST_ANDROID_SRC="${PIXDR_EXTERNAL}/boost-android"

# Android target. Keep API 29: NativeActivity + USB host works and matches the
# currently built UHD/libusb artifacts.
PIXDR_ANDROID_API="${PIXDR_ANDROID_API:-29}"
PIXDR_ANDROID_ABI="${PIXDR_ANDROID_ABI:-arm64-v8a}"
PIXDR_RUST_TARGET="${PIXDR_RUST_TARGET:-aarch64-linux-android}"

# Local toolchain layout produced by scripts/build-native.sh.
PIXDR_NDK="${PIXDR_NDK:-${PIXDR_BUILD_DIR}/android-ndk-r27d}"
PIXDR_PREFIX="${PIXDR_PREFIX:-${PIXDR_BUILD_DIR}/toolchain/${PIXDR_ANDROID_ABI}}"

export ANDROID_HOME="${ANDROID_HOME:-${HOME}/Android/Sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${PIXDR_NDK}}"

export UHD_INCLUDE_DIR="${UHD_INCLUDE_DIR:-${PIXDR_PREFIX}/include}"
export UHD_LIB_DIR="${UHD_LIB_DIR:-${PIXDR_PREFIX}/lib}"
export BOOST_INCLUDE_DIR="${BOOST_INCLUDE_DIR:-${PIXDR_PREFIX}/include}"
export BOOST_LIB_DIR="${BOOST_LIB_DIR:-${PIXDR_PREFIX}/lib}"
export NDK_SYSROOT="${NDK_SYSROOT:-${PIXDR_NDK}/toolchains/llvm/prebuilt/linux-x86_64/sysroot}"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="${CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER:-${PIXDR_NDK}/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${PIXDR_ANDROID_API}-clang}"

PIXDR_APK="${PIXDR_APK:-${PIXDR_APP_DIR}/pixdr-debug.apk}"
PIXDR_PACKAGE="${PIXDR_PACKAGE:-org.pixdr.app}"
PIXDR_ACTIVITY="${PIXDR_ACTIVITY:-android.app.NativeActivity}"

pixdr_require_cmd() {
  local missing=0
  for cmd in "$@"; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
      echo "ERROR: required command not found: $cmd" >&2
      missing=1
    fi
  done
  [[ "$missing" == 0 ]]
}

pixdr_android_build_tools() {
  local dir
  dir=$(ls -d "${ANDROID_HOME}/build-tools/"35*/ 2>/dev/null | sort -V | tail -1 || true)
  if [[ -z "$dir" ]]; then
    dir=$(ls -d "${ANDROID_HOME}/build-tools/"*/ 2>/dev/null | sort -V | tail -1 || true)
  fi
  if [[ -z "$dir" ]]; then
    echo "ERROR: Android SDK build-tools not found under ${ANDROID_HOME}/build-tools" >&2
    echo "Install e.g.: sdkmanager 'platforms;android-${PIXDR_ANDROID_API}' 'build-tools;35.0.0'" >&2
    return 1
  fi
  printf '%s\n' "${dir%/}"
}

pixdr_print_env() {
  cat <<EOF
pixdr build environment
  root:        ${PIXDR_ROOT}
  app:         ${PIXDR_APP_DIR}
  external:    ${PIXDR_EXTERNAL}
  build dir:   ${PIXDR_BUILD_DIR}
  Android SDK: ${ANDROID_HOME}
  NDK:         ${PIXDR_NDK}
  prefix:      ${PIXDR_PREFIX}
  target:      ${PIXDR_RUST_TARGET}
  API:         ${PIXDR_ANDROID_API}
  APK:         ${PIXDR_APK}
EOF
}
