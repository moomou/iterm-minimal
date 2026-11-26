#!/usr/bin/env bash
set -euo pipefail

# Native macOS host: strip `-flavor darwin` and duplicate `-arch`, then invoke Apple's `/usr/bin/ld` directly!
if [[ "$(uname -s)" == "Darwin" ]]; then
  args=()
  has_arch=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -flavor)
        shift 2
        ;;
      -arch)
        if [[ "$has_arch" -eq 0 ]]; then
          has_arch=1
          args+=("$1" "$2")
        fi
        shift 2
        ;;
      *)
        args+=("$1")
        shift
        ;;
    esac
  done
  exec /usr/bin/ld "${args[@]}"
fi

# Linux cross-compilation host: locate rustup's bundled ld64.lld (LLVM Mach-O linker)
SYSROOT="$(rustc --print sysroot)"
HOST="$(rustc -vV | awk '/^host:/ { print $2 }')"
LD64="${LD64_LLD:-${SYSROOT}/lib/rustlib/${HOST}/bin/gcc-ld/ld64.lld}"

if [[ ! -x "${LD64}" ]]; then
  echo "error: ld64.lld not found at ${LD64}" >&2
  exit 1
fi

SDK="${MACOS_SDK_PATH:-${HOME}/.cache/macos-sdk/MacOSX26.1.sdk}"
if [[ ! -d "${SDK}" ]]; then
  echo "error: macOS SDK not found at ${SDK}. Set MACOS_SDK_PATH or run scripts/setup-macos-sdk.sh" >&2
  exit 1
fi

MIN_MACOS="${MACOSX_DEPLOYMENT_TARGET:-13.0}"

# Detect if caller already specified -platform_version or -syslibroot
has_syslibroot=0
has_platform_version=0
has_adhoc=0
args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -flavor)
      shift 2
      ;;
    -syslibroot)
      has_syslibroot=1
      args+=("$1" "$2")
      shift 2
      ;;
    -platform_version)
      # Override rustc's fallback 11.0.0 11.0.0 with our deployment target & SDK version
      has_platform_version=1
      args+=("-platform_version" "macos" "${MIN_MACOS}.0" "26.1")
      shift 4
      ;;
    -adhoc_codesign|-no_adhoc_codesign)
      has_adhoc=1
      args+=("$1")
      shift
      ;;
    *)
      args+=("$1")
      shift
      ;;
  esac
done

extra=()
if [[ "${has_syslibroot}" -eq 0 ]]; then
  extra+=("-syslibroot" "${SDK}")
fi
if [[ "${has_platform_version}" -eq 0 ]]; then
  extra+=("-platform_version" "macos" "${MIN_MACOS}.0" "26.1")
fi
if [[ "${has_adhoc}" -eq 0 ]]; then
  extra+=("-adhoc_codesign")
fi

# Always ensure standard library/framework search paths inside SDK
extra+=(
  "-L${SDK}/usr/lib"
  "-F${SDK}/System/Library/Frameworks"
)

exec "${LD64}" "${extra[@]}" "${args[@]}"
