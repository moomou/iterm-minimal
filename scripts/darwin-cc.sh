#!/usr/bin/env bash
set -euo pipefail

ZIG="${ZIG_BIN:-${HOME}/.local/opt/zig/zig}"
SDK="${MACOS_SDK_PATH:-${HOME}/.cache/macos-sdk/MacOSX26.1.sdk}"
MIN_MACOS="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
TARGET_ARCH="${DARWIN_CC_ARCH:-aarch64}"

if [[ ! -x "${ZIG}" ]]; then
  echo "error: zig not found at ${ZIG}" >&2
  exit 1
fi

exec "${ZIG}" cc \
  -target "${TARGET_ARCH}-macos.${MIN_MACOS}" \
  -isysroot "${SDK}" \
  -F"${SDK}/System/Library/Frameworks" \
  "$@"
