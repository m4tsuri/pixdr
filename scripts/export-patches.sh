#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

mkdir -p \
  "${PIXDR_ROOT}/patches/uhd" \
  "${PIXDR_ROOT}/patches/libusb" \
  "${PIXDR_ROOT}/patches/uhd-rs" \
  "${PIXDR_ROOT}/patches/boost-android"

rm -f "${PIXDR_ROOT}"/patches/*/*.patch

export_series() {
  local repo="$1"
  local base="$2"
  local out_dir="$3"

  if git -C "${repo}" rev-parse --verify "${base}" >/dev/null 2>&1; then
    git -C "${repo}" format-patch -o "${out_dir}" "${base}..HEAD" >/dev/null
  else
    echo "WARNING: base '${base}' not found for ${repo}; exporting HEAD only" >&2
    git -C "${repo}" format-patch -1 HEAD -o "${out_dir}" >/dev/null
  fi
}

export_series "${PIXDR_UHD_SRC}" "${PIXDR_UHD_PATCH_BASE:-v4.10.0.0}" \
  "${PIXDR_ROOT}/patches/uhd"
export_series "${PIXDR_LIBUSB_SRC}" "${PIXDR_LIBUSB_PATCH_BASE:-v1.0.27}" \
  "${PIXDR_ROOT}/patches/libusb"
export_series "${PIXDR_UHDRS_SRC}" "${PIXDR_UHDRS_PATCH_BASE:-origin/master}" \
  "${PIXDR_ROOT}/patches/uhd-rs"
export_series "${PIXDR_BOOST_ANDROID_SRC}" "${PIXDR_BOOST_ANDROID_PATCH_BASE:-HEAD^}" \
  "${PIXDR_ROOT}/patches/boost-android"

if grep -R "${HOME}\|${PIXDR_ROOT}" "${PIXDR_ROOT}/patches" >/dev/null 2>&1; then
  echo "WARNING: local absolute path found in patches; inspect before publishing:" >&2
  grep -R "${HOME}\|${PIXDR_ROOT}" "${PIXDR_ROOT}/patches" >&2 || true
fi

wc -l "${PIXDR_ROOT}"/patches/*/*.patch
