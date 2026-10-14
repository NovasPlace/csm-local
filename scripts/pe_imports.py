#!/usr/bin/env python3
"""Inspect PE DLL imports without external packages or Windows SDK tools.

--require-standalone checks imports against Windows 10/11 system DLL names.
This detects declared DLL dependencies, not DLLs loaded dynamically at runtime,
and is not a substitute for testing the executable on a clean Windows machine.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import struct
import sys


class PEFormatError(ValueError):
    """An executable has unsupported or malformed PE metadata."""


# Conservative Windows 10/11 system DLL allowlist. New imports require review.
WINDOWS_SYSTEM_DLLS = {
    "advapi32.dll", "bcrypt.dll", "bcryptprimitives.dll", "cfgmgr32.dll",
    "comctl32.dll", "comdlg32.dll", "crypt32.dll", "dwmapi.dll", "gdi32.dll",
    "imm32.dll", "iphlpapi.dll", "kernel32.dll", "kernelbase.dll", "msvcrt.dll",
    "ncrypt.dll", "netapi32.dll", "normaliz.dll", "ntdll.dll", "ole32.dll",
    "oleaut32.dll", "powrprof.dll", "propsys.dll", "psapi.dll", "rpcrt4.dll",
    "secur32.dll", "setupapi.dll", "shell32.dll", "shlwapi.dll", "ucrtbase.dll",
    "user32.dll", "userenv.dll", "uxtheme.dll", "version.dll", "winhttp.dll",
    "wininet.dll", "winmm.dll", "wintrust.dll", "ws2_32.dll", "wtsapi32.dll",
}


def inspect_pe(path: Path | str) -> dict:
    """Return architecture and normal/delay DLL names from a PE file."""
    data = Path(path).read_bytes()

    def checked(start: int, size: int) -> bytes:
        if start < 0 or size < 0 or start + size > len(data):
            raise PEFormatError("PE field extends outside the file")
        return data[start:start + size]

    def u16(offset: int) -> int:
        return struct.unpack("<H", checked(offset, 2))[0]

    def u32(offset: int) -> int:
        return struct.unpack("<I", checked(offset, 4))[0]

    if checked(0, 2) != b"MZ":
        raise PEFormatError("Missing DOS executable signature")
    pe = u32(0x3C)
    if checked(pe, 4) != b"PE\0\0":
        raise PEFormatError("Missing PE signature")
    coff = pe + 4
    machine = u16(coff)
    section_count = u16(coff + 2)
    optional_size = u16(coff + 16)
    optional = coff + 20
    checked(optional, optional_size)
    magic = u16(optional)
    if magic == 0x20B:
        directory_relative = 112
        directory_count_relative = 108
        image_base = struct.unpack("<Q", checked(optional + 24, 8))[0]
    elif magic == 0x10B:
        directory_relative = 96
        directory_count_relative = 92
        image_base = u32(optional + 28)
    else:
        raise PEFormatError(f"Unsupported optional-header magic: 0x{magic:x}")
    if optional_size < directory_relative:
        raise PEFormatError("Truncated optional header")
    headers_size = u32(optional + 60)
    directory_count = u32(optional + directory_count_relative)
    if directory_relative + directory_count * 8 > optional_size:
        raise PEFormatError("Data directories extend beyond the optional header")
    sections = []
    for index in range(section_count):
        section = optional + optional_size + index * 40
        checked(section, 40)
        virtual_address = u32(section + 12)
        raw_size = u32(section + 16)
        raw_offset = u32(section + 20)
        sections.append((virtual_address, raw_size, raw_offset))

    def offset(rva: int, size: int = 1) -> int:
        if 0 <= rva and rva + size <= headers_size:
            checked(rva, size)
            return rva
        for start, raw_size, raw in sections:
            if start <= rva and rva + size <= start + raw_size:
                position = raw + rva - start
                checked(position, size)
                return position
        raise PEFormatError(f"Unmapped file-backed RVA: 0x{rva:x}")

    def dll_name(rva: int) -> str:
        position = offset(rva)
        # DLL names cannot reasonably exceed MAX_PATH here. Check the entire
        # string through RVA mapping so a malformed name cannot cross sections.
        content = bytearray()
        for index in range(1024):
            byte = data[offset(rva + index)]
            if byte == 0:
                break
            content.append(byte)
        else:
            raise PEFormatError("Unterminated DLL import name")
        try:
            name = content.decode("ascii")
        except UnicodeDecodeError as error:
            raise PEFormatError("Non-ASCII DLL import name") from error
        if not name or any(character in name for character in "\\/:"):
            raise PEFormatError("Empty or path-qualified DLL import name")
        return name

    def directory(index: int) -> tuple[int, int]:
        if index >= directory_count:
            return 0, 0
        return struct.unpack("<II", checked(optional + directory_relative + index * 8, 8))

    def import_names(index: int, descriptor_size: int, delayed: bool) -> list[str]:
        rva, size = directory(index)
        if not rva and not size:
            return []
        if not rva or size < descriptor_size:
            raise PEFormatError("Invalid DLL import directory")
        names = []
        for relative in range(0, size - descriptor_size + 1, descriptor_size):
            position = offset(rva + relative, descriptor_size)
            descriptor = checked(position, descriptor_size)
            if not any(descriptor):
                return names
            if delayed:
                attributes = u32(position)
                name_address = u32(position + 4)
                name_rva = name_address if attributes & 1 else name_address - image_base
            else:
                name_rva = u32(position + 12)
            names.append(dll_name(name_rva))
        raise PEFormatError("DLL import directory has no terminator")

    return {
        "machine": f"0x{machine:04x}",
        "architecture": {0x8664: "x64", 0x14C: "x86", 0xAA64: "arm64"}.get(machine, "unknown"),
        "format": "PE32+" if magic == 0x20B else "PE32",
        "subsystem": u16(optional + 68),
        "imports": import_names(1, 20, False),
        "delay_imports": import_names(13, 32, True),
    }


def standalone_issues(report: dict) -> list[str]:
    """Flag imports requiring a redistributable or further manual review."""
    issues = []
    for name in sorted(set(report["imports"] + report["delay_imports"]), key=str.lower):
        normalized = name.lower()
        if normalized.startswith(("vcruntime", "msvcp", "msvcr", "concrt", "vcomp")) and normalized != "msvcrt.dll":
            issues.append(f"{name}: Microsoft Visual C++ redistributable dependency")
        elif normalized not in WINDOWS_SYSTEM_DLLS and not normalized.startswith(("api-ms-win-", "ext-ms-win-")):
            issues.append(f"{name}: not in the reviewed Windows system DLL allowlist")
    return issues


def require_standalone(report: dict) -> None:
    issues = standalone_issues(report)
    if issues:
        raise ValueError("Standalone import check failed: " + "; ".join(issues))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--require-standalone", action="store_true")
    parser.add_argument("--json", action="store_true", help="Emit a path-free JSON report")
    args = parser.parse_args()
    try:
        report = inspect_pe(args.executable)
        issues = standalone_issues(report)
        if args.json:
            print(json.dumps({**report, "standalone_import_issues": issues}, indent=2))
        else:
            print(f"Architecture: {report['architecture']} ({report['machine']}, {report['format']})")
            print("Normal DLL imports: " + (", ".join(report["imports"]) or "none"))
            print("Delay DLL imports: " + (", ".join(report["delay_imports"]) or "none"))
        if args.require_standalone:
            require_standalone(report)
        return 0
    except (OSError, ValueError) as error:
        print(f"PE import check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
