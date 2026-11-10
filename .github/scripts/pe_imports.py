#!/usr/bin/env python3
"""List PE import DLLs. Exit 1 if a VC++ redistributable or OpenH264 DLL is required."""

from __future__ import annotations

import struct
import sys
from pathlib import Path

FORBIDDEN_PREFIXES = (
    "vcruntime",
    "msvcp",
    "concrt",
    "vcomp",
    "ucrtbase",
    "openh264",
    "api-ms-win-crt",
)


def _u16(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 2], "little")


def _u32(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset : offset + 4], "little")


def _sections(data: bytes, section_off: int, count: int) -> list[tuple[int, int, int, int]]:
    sections = []
    for index in range(count):
        start = section_off + index * 40
        virtual_size = _u32(data, start + 8)
        virtual_address = _u32(data, start + 12)
        raw_size = _u32(data, start + 16)
        raw_ptr = _u32(data, start + 20)
        sections.append((virtual_address, virtual_size, raw_ptr, raw_size))
    return sections


def _rva_to_offset(sections: list[tuple[int, int, int, int]], rva: int) -> int:
    for virtual_address, virtual_size, raw_ptr, raw_size in sections:
        span = max(virtual_size, raw_size, 1)
        if virtual_address <= rva < virtual_address + span:
            return raw_ptr + (rva - virtual_address)
    raise SystemExit(f"RVA 0x{rva:x} is not inside a section")


def _cstring(data: bytes, offset: int) -> str:
    end = data.index(b"\0", offset)
    return data[offset:end].decode("ascii")


def import_dlls(path: Path) -> list[str]:
    data = path.read_bytes()
    if data[:2] != b"MZ":
        raise SystemExit(f"{path} is not a PE executable")
    pe_offset = _u32(data, 0x3C)
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise SystemExit(f"{path} has a bad PE signature")
    coff = pe_offset + 4
    section_count = _u16(data, coff + 2)
    optional_size = _u16(data, coff + 16)
    optional = coff + 20
    magic = _u16(data, optional)
    if magic == 0x20B:
        directory_offset = optional + 112
        directory_count = _u32(data, optional + 108)
    elif magic == 0x10B:
        directory_offset = optional + 96
        directory_count = _u32(data, optional + 92)
    else:
        raise SystemExit(f"unknown PE magic {magic:#x}")
    sections = _sections(data, optional + optional_size, section_count)
    names: list[str] = []

    def directory(index: int) -> tuple[int, int]:
        if index >= directory_count:
            return 0, 0
        slot = directory_offset + index * 8
        return _u32(data, slot), _u32(data, slot + 4)

    import_rva, _import_size = directory(1)
    if import_rva:
        offset = _rva_to_offset(sections, import_rva)
        while _u32(data, offset) != 0 or _u32(data, offset + 12) != 0:
            name_rva = _u32(data, offset + 12)
            names.append(_cstring(data, _rva_to_offset(sections, name_rva)))
            offset += 20

    delay_rva, _delay_size = directory(13)
    if delay_rva:
        offset = _rva_to_offset(sections, delay_rva)
        while _u32(data, offset + 4) != 0:
            name_rva = _u32(data, offset + 4)
            names.append(_cstring(data, _rva_to_offset(sections, name_rva)))
            offset += 32
    return names


def forbidden(names: list[str]) -> list[str]:
    bad = []
    for name in names:
        lowered = name.lower()
        if any(lowered.startswith(prefix) for prefix in FORBIDDEN_PREFIXES):
            bad.append(name)
    return bad


def _self_test() -> None:
    # PE32+ with one import (KERNEL32.dll) and one delay-load (MFPLAT.dll).
    section_va = 0x1000
    section_raw = 0x200
    name_kernel = b"KERNEL32.dll\0"
    name_mf = b"MFPLAT.dll\0"
    # import descriptor (20) + null descriptor (20) + delay descriptor (32) + null (32)
    blob = bytearray(20 + 20 + 32 + 32)
    kernel_rva = section_va + len(blob)
    blob += name_kernel
    mf_rva = section_va + len(blob)
    blob += name_mf
    struct.pack_into("<IIIII", blob, 0, 0, 0, 0, kernel_rva, 0)
    struct.pack_into("<IIIIIIII", blob, 40, 0, mf_rva, 0, 0, 0, 0, 0, 0)

    pe = bytearray(section_raw + len(blob))
    pe[0:2] = b"MZ"
    pe_offset = 0x80
    struct.pack_into("<I", pe, 0x3C, pe_offset)
    pe[pe_offset : pe_offset + 4] = b"PE\0\0"
    coff = pe_offset + 4
    optional_size = 240
    struct.pack_into("<HHIIIHH", pe, coff, 0x8664, 1, 0, 0, 0, optional_size, 0x22)
    optional = coff + 20
    struct.pack_into("<H", pe, optional, 0x20B)
    struct.pack_into("<I", pe, optional + 108, 16)
    struct.pack_into("<II", pe, optional + 112 + 8, section_va, 40)
    struct.pack_into("<II", pe, optional + 112 + 13 * 8, section_va + 40, 32)
    section = optional + optional_size
    pe[section : section + 8] = b".rdata\0\0"
    struct.pack_into("<IIII", pe, section + 8, len(blob), section_va, len(blob), section_raw)
    pe[section_raw : section_raw + len(blob)] = blob

    path = Path(__file__).with_suffix(".selftest.exe")
    path.write_bytes(pe)
    try:
        found = import_dlls(path)
        if found != ["KERNEL32.dll", "MFPLAT.dll"]:
            raise SystemExit(f"self-test parsed {found}")
        if forbidden(found):
            raise SystemExit("self-test flagged system DLLs")
        if not forbidden(["VCRUNTIME140.dll", "OpenH264.dll", "api-ms-win-crt-runtime-l1-1-0.dll"]):
            raise SystemExit("self-test missed a redistributable")
    finally:
        path.unlink(missing_ok=True)
    print("pe_imports self-test ok")


def main() -> None:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        _self_test()
        return
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} <exe> | --self-test")
    path = Path(sys.argv[1])
    names = import_dlls(path)
    print(f"imports of {path.name}:")
    for name in names:
        print(f"  {name}")
    bad = forbidden(names)
    if bad:
        print("this binary still needs a runtime that must be bundled or statically linked:", ", ".join(bad), file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
