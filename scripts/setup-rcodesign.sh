#!/usr/bin/env bash
# Download and verify rcodesign (apple-codesign 0.29.0) on Linux x86_64/arm64
# for full macOS .app bundle code signing (Info.plist + _CodeSignature/CodeResources + Mach-O).
set -euo pipefail

VER="0.29.0"
ARCH="$(uname -m)"
case "${ARCH}" in
  x86_64)  TRIPLE="x86_64-unknown-linux-musl" ;;
  aarch64) TRIPLE="aarch64-unknown-linux-musl" ;;
  *) echo "error: unsupported Linux arch for rcodesign: ${ARCH}" >&2; exit 1 ;;
esac

DEST_DIR="${RCODESIGN_DIR:-${HOME}/.cache/macos-tools}"
BIN="${DEST_DIR}/rcodesign"
if [[ -x "${BIN}" ]]; then
  echo "rcodesign already installed at ${BIN}"
  exit 0
fi

mkdir -p "${DEST_DIR}"
TMP="$(mktemp -d /tmp/rcodesign-dl.XXXXXX)"
trap 'rm -rf "${TMP}"' EXIT

BASE_URL="https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F${VER}"
TAR="apple-codesign-${VER}-${TRIPLE}.tar.gz"

echo "==> Downloading ${TAR}..."
curl -fsSL "${BASE_URL}/${TAR}" -o "${TMP}/${TAR}"
curl -fsSL "${BASE_URL}/${TAR}.sha256" -o "${TMP}/${TAR}.sha256"

echo "==> Verifying SHA-256 checksum..."
EXPECTED="$(awk '{print $1}' "${TMP}/${TAR}.sha256")"
ACTUAL="$(sha256sum "${TMP}/${TAR}" | awk '{print $1}')"
if [[ "${EXPECTED}" != "${ACTUAL}" ]]; then
  echo "error: SHA-256 mismatch for ${TAR}: expected ${EXPECTED}, got ${ACTUAL}" >&2
  exit 1
fi

tar -xzf "${TMP}/${TAR}" -C "${TMP}"
install -m 0755 "${TMP}/apple-codesign-${VER}-${TRIPLE}/rcodesign" "${BIN}"
echo "Installed rcodesign ${VER} at ${BIN}"
"${BIN}" --version
