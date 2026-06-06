#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

# Commit and push external fork branches. This is useful after editing external/*.
# Requires GitHub CLI authentication.

pixdr_require_cmd git gh >/dev/null

USER="${PIXDR_GITHUB_USER:-$(gh api user -q .login)}"

prepare_push() {
  local name="$1" path="$2" upstream="$3" fork_repo="$4" branch="$5" msg="$6" addspec="$7"
  echo "\n=== ${name} ==="
  cd "${path}"
  git switch -C "${branch}"
  git add ${addspec}
  if git diff --cached --quiet; then
    echo "No staged changes for ${name}"
  else
    git commit -m "${msg}"
  fi
  gh repo fork "${upstream}" --clone=false || true
  local fork_url="https://github.com/${USER}/${fork_repo}.git"
  if git remote get-url fork >/dev/null 2>&1; then
    git remote set-url fork "${fork_url}"
  else
    git remote add fork "${fork_url}"
  fi
  git push -u fork "${branch}"
}

prepare_push \
  "UHD" "${PIXDR_UHD_SRC}" "EttusResearch/uhd" "uhd" \
  "${PIXDR_UHD_BRANCH:-pixdr-android-uhd-4.10}" \
  "pixdr: add Android USB fd support for B200" \
  "host/include/uhd/transport/usb_device_handle.hpp host/lib/transport/libusb1_base.cpp host/lib/transport/libusb1_base.hpp host/lib/transport/libusb1_control.cpp host/lib/usrp/b200/b200_iface.cpp host/lib/usrp/b200/b200_impl.cpp host/lib/utils/pathslib.cpp host/lib/utils/platform.cpp"

prepare_push \
  "libusb" "${PIXDR_LIBUSB_SRC}" "libusb/libusb" "libusb" \
  "${PIXDR_LIBUSB_BRANCH:-pixdr-android}" \
  "pixdr: support Android wrapped usbfs fd diagnostics" \
  "libusb/core.c libusb/os/linux_usbfs.c"

prepare_push \
  "uhd-rs" "${PIXDR_UHDRS_SRC}" "samcrow/uhd-rust" "uhd-rust" \
  "${PIXDR_UHDRS_BRANCH:-pixdr-android}" \
  "pixdr: support Android UHD cross build" \
  "uhd-sys/build.rs"

prepare_push \
  "Boost-for-Android" "${PIXDR_BOOST_ANDROID_SRC}" "dec1/Boost-for-Android" "Boost-for-Android" \
  "${PIXDR_BOOST_ANDROID_BRANCH:-pixdr-android}" \
  "pixdr: make Android Boost build non-interactive" \
  "__build.sh"
