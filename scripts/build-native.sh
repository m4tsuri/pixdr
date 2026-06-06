#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

pixdr_require_cmd cmake make wget tar >/dev/null

NCORES="${PIXDR_JOBS:-$(nproc)}"
HOST_TAG="linux-x86_64"
TOOLCHAIN="${PIXDR_NDK}/toolchains/llvm/prebuilt/${HOST_TAG}"

if [[ ! -d "${PIXDR_NDK}" ]]; then
  echo "ERROR: NDK not found: ${PIXDR_NDK}" >&2
  echo "Set PIXDR_NDK=/path/to/android-ndk-r27d or place it under build/native/android-ndk-r27d." >&2
  exit 1
fi

for d in "${PIXDR_UHD_SRC}" "${PIXDR_LIBUSB_SRC}" "${PIXDR_UHDRS_SRC}" "${PIXDR_BOOST_ANDROID_SRC}"; do
  [[ -d "$d" ]] || { echo "ERROR: missing external repo: $d" >&2; exit 1; }
done

export CC="${TOOLCHAIN}/bin/aarch64-linux-android${PIXDR_ANDROID_API}-clang"
export CXX="${TOOLCHAIN}/bin/aarch64-linux-android${PIXDR_ANDROID_API}-clang++"
export AR="${TOOLCHAIN}/bin/llvm-ar"
export RANLIB="${TOOLCHAIN}/bin/llvm-ranlib"
export STRIP="${TOOLCHAIN}/bin/llvm-strip"
export LD="${TOOLCHAIN}/bin/ld.lld"

mkdir -p "${PIXDR_PREFIX}/lib" "${PIXDR_PREFIX}/include" "${PIXDR_BUILD_DIR}/sources"

cat <<EOF
==========================================
Building native dependencies for pixdr
NDK:        ${PIXDR_NDK}
API:        ${PIXDR_ANDROID_API}
Prefix:     ${PIXDR_PREFIX}
Jobs:       ${NCORES}
UHD src:    ${PIXDR_UHD_SRC}
libusb src: ${PIXDR_LIBUSB_SRC}
==========================================
EOF

# -----------------------------------------------------------------------------
# Boost 1.87.0
# -----------------------------------------------------------------------------
echo "\n=== STEP 1: Boost 1.87.0 ==="
BOOST_DIR="${PIXDR_BUILD_DIR}/sources/boost/down/1.87.0"
BOOST_TAR="${PIXDR_BUILD_DIR}/sources/boost_1_87_0.tar.gz"

if [[ ! -f "${BOOST_DIR}/bootstrap.sh" ]]; then
  mkdir -p "$(dirname "${BOOST_DIR}")"
  if [[ ! -f "${BOOST_TAR}" ]]; then
    wget -q --show-progress \
      https://archives.boost.io/release/1.87.0/source/boost_1_87_0.tar.gz \
      -O "${BOOST_TAR}"
  fi
  tar xzf "${BOOST_TAR}" -C "$(dirname "${BOOST_DIR}")"
  rm -rf "${BOOST_DIR}"
  mv "$(dirname "${BOOST_DIR}")/boost_1_87_0" "${BOOST_DIR}"
fi

(
  cd "${PIXDR_BOOST_ANDROID_SRC}"
  cat > do.sh <<EOF
#!/usr/bin/env bash
set -euo pipefail
export BOOST_DIR="${BOOST_DIR}"
export NDK_DIR="${PIXDR_NDK}"
export ABI_NAMES="${PIXDR_ANDROID_ABI}"
export LINKAGES="static"
./__build.sh
EOF
  chmod +x do.sh
  ./do.sh
)

BOOST_BUILD="${PIXDR_BOOST_ANDROID_SRC}/build/install"
[[ -d "${BOOST_BUILD}/include" ]] || { echo "ERROR: Boost build failed" >&2; exit 1; }
cp -R "${BOOST_BUILD}/include/boost" "${PIXDR_PREFIX}/include/"
cp "${BOOST_BUILD}/libs/${PIXDR_ANDROID_ABI}"/*.a "${PIXDR_PREFIX}/lib/"

# -----------------------------------------------------------------------------
# libusb
# -----------------------------------------------------------------------------
echo "\n=== STEP 2: libusb ==="
if [[ ! -f "${PIXDR_LIBUSB_SRC}/configure" ]]; then
  (cd "${PIXDR_LIBUSB_SRC}" && ./autogen.sh)
fi

LIBUSB_BUILD="${PIXDR_BUILD_DIR}/libusb-android"
rm -rf "${LIBUSB_BUILD}"
mkdir -p "${LIBUSB_BUILD}"
(
  cd "${LIBUSB_BUILD}"
  SYSROOT="${TOOLCHAIN}/sysroot"
  "${PIXDR_LIBUSB_SRC}/configure" \
    --host=aarch64-linux-android \
    --prefix="${PIXDR_PREFIX}" \
    --enable-static \
    --disable-shared \
    --disable-udev \
    CC="${CC} --sysroot=${SYSROOT}" \
    CXX="${CXX} --sysroot=${SYSROOT}" \
    CFLAGS="-fPIC" \
    CXXFLAGS="-fPIC"
  make -j "${NCORES}"
  make install
)

# -----------------------------------------------------------------------------
# UHD
# -----------------------------------------------------------------------------
echo "\n=== STEP 3: UHD 4.10 B200/B210 only ==="
UHD_BUILD="${PIXDR_BUILD_DIR}/uhd-android"
rm -rf "${UHD_BUILD}"
mkdir -p "${UHD_BUILD}"
(
  cd "${UHD_BUILD}"
  cmake "${PIXDR_UHD_SRC}/host" \
    -DCMAKE_TOOLCHAIN_FILE="${PIXDR_NDK}/build/cmake/android.toolchain.cmake" \
    -DANDROID_ABI="${PIXDR_ANDROID_ABI}" \
    -DANDROID_PLATFORM="android-${PIXDR_ANDROID_API}" \
    -DANDROID_STL=c++_shared \
    -DCMAKE_INSTALL_PREFIX="${PIXDR_PREFIX}" \
    -DCMAKE_FIND_ROOT_PATH="${PIXDR_PREFIX}" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBOOST_ROOT="${PIXDR_PREFIX}" \
    -DBoost_USE_STATIC_LIBS=ON \
    -DBoost_USE_DEBUG_LIBS=OFF \
    -DBoost_COMPILER=-clang \
    -DBoost_ARCHITECTURE=-a64 \
    -DLIBUSB_INCLUDE_DIRS="${PIXDR_PREFIX}/include/libusb-1.0" \
    -DLIBUSB_LIBRARIES="${PIXDR_PREFIX}/lib/libusb-1.0.a" \
    -DCMAKE_SHARED_LINKER_FLAGS="-llog" \
    -DENABLE_STATIC_LIBS=OFF \
    -DENABLE_EXAMPLES=OFF \
    -DENABLE_TESTS=OFF \
    -DENABLE_UTILS=OFF \
    -DENABLE_PYTHON_API=OFF \
    -DENABLE_MANUAL=OFF \
    -DENABLE_DOXYGEN=OFF \
    -DENABLE_MAN_PAGES=OFF \
    -DENABLE_OCTOCLOCK=OFF \
    -DENABLE_E300=OFF \
    -DENABLE_E320=OFF \
    -DENABLE_N300=OFF \
    -DENABLE_N320=OFF \
    -DENABLE_X300=OFF \
    -DENABLE_USRP2=OFF \
    -DENABLE_N230=OFF \
    -DENABLE_MPMD=OFF \
    -DENABLE_B100=OFF \
    -DENABLE_USRP1=OFF \
    -DENABLE_X400=OFF \
    -DENABLE_DPDK=OFF \
    -DENABLE_SIM=OFF \
    -DUHD_BOOST_REQUIRED=FALSE
  make -j "${NCORES}"
  make install
)

cat <<EOF

==========================================
Native build complete
Prefix: ${PIXDR_PREFIX}
$(ls -lh "${PIXDR_PREFIX}/lib/libuhd.so" 2>/dev/null || true)
$(ls -lh "${PIXDR_PREFIX}/lib/libusb-1.0.a" 2>/dev/null || true)
==========================================
EOF
