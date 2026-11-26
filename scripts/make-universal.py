#!/usr/bin/env python3
"""Combine arm64 and x86_64 Mach-O binaries into a macOS Universal (FAT) binary."""
import struct
import sys

FAT_MAGIC = 0xCAFEBABE
MH_MAGIC_64 = 0xFEEDFACF
PAGE_ALIGN_EXP = 14  # 16384-byte alignment (Apple Silicon page size)
PAGE_ALIGN = 1 << PAGE_ALIGN_EXP


def read_macho_header(path: str):
    with open(path, "rb") as f:
        data = f.read()
    magic, cputype, cpusubtype = struct.unpack_from("<III", data, 0)
    if magic != MH_MAGIC_64:
        raise ValueError(f"{path} is not a 64-bit Mach-O binary (magic={magic:#x})")
    return cputype, cpusubtype, data


def align_up(n: int, align: int) -> int:
    return (n + align - 1) & ~(align - 1)


def main() -> int:
    if len(sys.argv) < 4:
        print("usage: make-universal.py <output> <slice1> <slice2> [...]", file=sys.stderr)
        return 2

    out_path = sys.argv[1]
    slices = [read_macho_header(p) for p in sys.argv[2:]]

    header_size = 8 + 20 * len(slices)
    cur_offset = align_up(header_size, PAGE_ALIGN)

    fat_entries = []
    for cputype, cpusubtype, data in slices:
        fat_entries.append((cputype, cpusubtype, cur_offset, len(data), PAGE_ALIGN_EXP, data))
        cur_offset = align_up(cur_offset + len(data), PAGE_ALIGN)

    with open(out_path, "wb") as out:
        out.write(struct.pack(">II", FAT_MAGIC, len(fat_entries)))
        for cputype, cpusubtype, offset, size, align_exp, _ in fat_entries:
            out.write(struct.pack(">IIIII", cputype, cpusubtype, offset, size, align_exp))
        for _, _, offset, _, _, data in fat_entries:
            pad = offset - out.tell()
            if pad > 0:
                out.write(b"\x00" * pad)
            out.write(data)

        total_size = out.tell()

    print(f"Created universal Mach-O binary: {out_path} ({total_size:,} bytes, {len(slices)} slices)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
