#!/usr/bin/env bash
set -euo pipefail

SDK_DIR="${MACOS_SDK_DIR:-${HOME}/.cache/macos-sdk}"
SDK_VER="26.1"
SDK_URL="https://github.com/joseluisq/macosx-sdks/releases/download/${SDK_VER}/MacOSX${SDK_VER}.sdk.tar.xz"

rustup target add aarch64-apple-darwin x86_64-apple-darwin

mkdir -p "${SDK_DIR}"
if [[ ! -d "${SDK_DIR}/MacOSX${SDK_VER}.sdk" ]]; then
  echo "Downloading MacOSX${SDK_VER}.sdk..."
  curl -fsSL -o "${SDK_DIR}/MacOSX${SDK_VER}.sdk.tar.xz" "${SDK_URL}"
  tar -xf "${SDK_DIR}/MacOSX${SDK_VER}.sdk.tar.xz" -C "${SDK_DIR}/"
  rm -f "${SDK_DIR}/MacOSX${SDK_VER}.sdk.tar.xz"
fi

echo "macOS SDK ready at ${SDK_DIR}/MacOSX${SDK_VER}.sdk"
