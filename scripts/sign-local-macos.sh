#!/usr/bin/env bash
# Re-sign JTerm.app on macOS with a persistent self-signed local Code Signing certificate
# stored in your login Keychain ("JTerm Local Signing").
#
# Why? macOS TCC (Screen Recording, Accessibility, Automation, Camera, Mic) pins ad-hoc
# grants to a build's exact CDHash, so upgrading an ad-hoc-signed app invalidates grants.
# Signing JTerm.app with a persistent local certificate binds TCC grants to the certificate
# + bundle ID (dev.moomou.jterm), so permissions persist seamlessly across JTerm upgrades!
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this helper runs on macOS (Darwin)" >&2
  exit 1
fi

APP_PATH="${1:-/Applications/JTerm.app}"
CERT_NAME="${JTERM_LOCAL_CERT_NAME:-JTerm Local Signing}"

if [[ ! -d "${APP_PATH}" ]]; then
  echo "error: app bundle not found at ${APP_PATH}" >&2
  echo "usage: $0 [/Applications/JTerm.app]" >&2
  exit 1
fi

if ! security find-identity -v -p codesigning | grep -Fq "\"${CERT_NAME}\""; then
  echo "==> Creating persistent local Code Signing certificate '${CERT_NAME}' in login Keychain..."
  TMP="$(mktemp -d /tmp/jterm-cert.XXXXXX)"
  trap 'rm -rf "${TMP}"' EXIT
  cat > "${TMP}/cert.cfg" << EOF
[ req ]
distinguished_name = dn
x509_extensions = v3_codesign
prompt = no

[ dn ]
CN = ${CERT_NAME}

[ v3_codesign ]
keyUsage = digitalSignature
extendedKeyUsage = codeSigning
basicConstraints = critical,CA:false
EOF
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -config "${TMP}/cert.cfg" \
    -keyout "${TMP}/key.pem" -out "${TMP}/cert.pem" 2>/dev/null
  openssl pkcs12 -export -legacy -out "${TMP}/ident.p12" \
    -inkey "${TMP}/key.pem" -in "${TMP}/cert.pem" \
    -passout pass:jterm-local 2>/dev/null
  security import "${TMP}/ident.p12" -k "${HOME}/Library/Keychains/login.keychain-db" \
    -P jterm-local -T /usr/bin/codesign
  echo "==> Marking '${CERT_NAME}' as trusted for Code Signing (macOS may prompt for your login password once)..."
  security add-trusted-cert -p codeSign -k "${HOME}/Library/Keychains/login.keychain-db" "${TMP}/cert.pem"
fi

echo "==> Signing ${APP_PATH} with '${CERT_NAME}'..."
/usr/bin/codesign --force --deep --sign "${CERT_NAME}" --identifier dev.moomou.jterm "${APP_PATH}"
/usr/bin/codesign --verify --deep --strict --verbose=2 "${APP_PATH}"
echo "DONE: ${APP_PATH} signed with persistent identity '${CERT_NAME}'."
echo "macOS TCC permissions (Screen Recording, Accessibility, etc.) will now persist across JTerm upgrades!"
