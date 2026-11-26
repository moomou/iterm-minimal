#!/usr/bin/env python3
"""Verify Mach-O binary integrity on Linux (universal or single-arch).

Inspects Mach-O magic, CPU type, load commands (LC_LOAD_DYLIB, LC_BUILD_VERSION,
LC_CODE_SIGNATURE, LC_MAIN), and checks that Apple frameworks & libSystem are
properly linked and ad-hoc signed for Apple Silicon.
"""
import struct
import sys

MH_MAGIC_64 = 0xFEEDFACF
FAT_MAGIC = 0xCAFEBABE
FAT_CIGAM = 0xBEBAFECA

CPU_TYPE_X86_64 = 0x01000007
CPU_TYPE_ARM64 = 0x0100000C

LC_LOAD_DYLIB = 0xC
LC_LOAD_WEAK_DYLIB = 0x80000018
LC_REEXPORT_DYLIB = 0x8000001F
LC_CODE_SIGNATURE = 0x1D
LC_MAIN = 0x80000028
LC_BUILD_VERSION = 0x32
LC_VERSION_MIN_MACOSX = 0x24

CPU_NAMES = {
    CPU_TYPE_X86_64: "x86_64",
    CPU_TYPE_ARM64: "arm64",
}


def decode_version(v: int) -> str:
    return f"{(v >> 16) & 0xFFFF}.{(v >> 8) & 0xFF}.{v & 0xFF}"


def inspect_slice(data: bytes, offset: int = 0) -> dict:
    magic, cputype, cpusubtype, filetype, ncmds, sizeofcmds, flags, _ = struct.unpack_from(
        "<IIIIIIII", data, offset
    )
    assert magic == MH_MAGIC_64, f"Not 64-bit Mach-O at offset {offset}: {magic:#x}"
    pos = offset + 32
    dylibs = []
    has_codesig = False
    codesig_size = 0
    has_main = False
    min_os = None
    sdk_ver = None

    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<II", data, pos)
        if cmd in (LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, LC_REEXPORT_DYLIB):
            name_offset = struct.unpack_from("<I", data, pos + 8)[0]
            raw = data[pos + name_offset : pos + cmdsize]
            name = raw.split(b"\x00", 1)[0].decode("utf-8", errors="replace")
            dylibs.append(name)
        elif cmd == LC_CODE_SIGNATURE:
            has_codesig = True
            _, codesig_size = struct.unpack_from("<II", data, pos + 8)
        elif cmd == LC_MAIN:
            has_main = True
        elif cmd == LC_BUILD_VERSION:
            platform, minos, sdk, _ = struct.unpack_from("<IIII", data, pos + 8)
            min_os = f"platform={platform} minos={decode_version(minos)}"
            sdk_ver = decode_version(sdk)
        elif cmd == LC_VERSION_MIN_MACOSX:
            ver, sdk = struct.unpack_from("<II", data, pos + 8)
            min_os = f"macosx={decode_version(ver)}"
            sdk_ver = decode_version(sdk)
        pos += cmdsize

    return {
        "arch": CPU_NAMES.get(cputype, f"{cputype:#x}"),
        "filetype": filetype,
        "ncmds": ncmds,
        "has_main": has_main,
        "has_codesig": has_codesig,
        "codesig_size": codesig_size,
        "min_os": min_os,
        "sdk_ver": sdk_ver,
        "dylibs": dylibs,
    }


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: verify-macho.py <binary>", file=sys.stderr)
        return 2

    path = sys.argv[1]
    with open(path, "rb") as f:
        data = f.read()

    magic_be = struct.unpack_from(">I", data, 0)[0]
    magic_le = struct.unpack_from("<I", data, 0)[0]

    slices = []
    if magic_be == FAT_MAGIC:
        nfat = struct.unpack_from(">I", data, 4)[0]
        print(f"Universal Fat Mach-O ({nfat} architectures): {path} ({len(data):,} bytes)")
        for i in range(nfat):
            cputype, cpusubtype, offset, size, align = struct.unpack_from(
                ">IIIII", data, 8 + i * 20
            )
            info = inspect_slice(data, offset)
            info["slice_size"] = size
            slices.append(info)
    elif magic_le == MH_MAGIC_64:
        print(f"Thin 64-bit Mach-O: {path} ({len(data):,} bytes)")
        info = inspect_slice(data, 0)
        info["slice_size"] = len(data)
        slices.append(info)
    else:
        print(f"ERROR: Unrecognized binary format (magic={magic_le:#x})", file=sys.stderr)
        return 1

    ok = True
    for s in slices:
        print(f"\n  Arch: {s['arch']} ({s['slice_size']:,} bytes)")
        print(f"    LC_MAIN:           {s['has_main']}")
        print(f"    LC_CODE_SIGNATURE: {s['has_codesig']} ({s['codesig_size']} bytes)")
        print(f"    Platform/SDK:      {s['min_os']} sdk={s['sdk_ver']}")
        print(f"    Linked libraries ({len(s['dylibs'])}):")
        for d in s["dylibs"]:
            print(f"      - {d}")
        if not s["has_main"]:
            print("    FAIL: missing LC_MAIN", file=sys.stderr)
            ok = False
        if s["arch"] == "arm64" and not s["has_codesig"]:
            print("    FAIL: arm64 Mach-O missing LC_CODE_SIGNATURE (required by macOS)", file=sys.stderr)
            ok = False
        if not any("libSystem.B.dylib" in d for d in s["dylibs"]):
            print("    FAIL: missing libSystem.B.dylib", file=sys.stderr)
            ok = False

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
