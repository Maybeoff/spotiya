#!/usr/bin/env python3
"""Name and verify the library addresses that decoder.c hooks or calls.

decoder.c and layout.json are pinned by checksum (see prebuilt.json), so the addresses
stay as literals there. This table is their documentation. Without arguments the script
checks that the table covers every address in decoder.c; given the pinned APK or library,
it also checks each address against the ELF itself.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import struct
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent

# PLT stubs that decoder.c answers itself instead of running library code.
PLT = {
    0x18B5530: "__cxa_begin_catch",
    0x18B5550: "std::string::~basic_string()",
    0x18B5560: "std::string::__init(const char*, size_t)",
    0x18B5570: "__cxa_allocate_exception",
    0x18B55A0: "__cxa_throw",
    0x18B55C0: "operator new(size_t)",
    0x18B55D0: "strlen",
    0x18B55E0: "operator delete(void*)",
    0x18B5600: "std::string::basic_string(const std::string&)",
    0x18B56A0: "__cxa_thread_atexit",
    0x18B57C0: "std::mutex::lock()",
    0x18B5C60: "__cxa_end_catch",
    0x18B57E0: "std::random_device::operator()()",
    0x18B57F0: "std::random_device::~random_device()",
    0x18B5800: "std::random_device::random_device(const std::string&)",
    0x18B6230: "calloc",
    0x18B7160: "memcpy",
    0x18B7170: "memmove",
    0x18B8210: "__emutls_get_address",
    0x18B8310: "__cxa_thread_atexit_impl",
}
# Mangled names as the dynamic symbol table spells them.
MANGLED = {
    "std::string::~basic_string()": "_ZNSt6__ndk112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEED2Ev",
    "std::string::__init(const char*, size_t)": "_ZNSt6__ndk112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEE6__initEPKcm",
    "operator new(size_t)": "_Znwm",
    "operator delete(void*)": "_ZdlPv",
    "std::string::basic_string(const std::string&)": "_ZNSt6__ndk112basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEC2ERKS5_",
    "std::mutex::lock()": "_ZNSt6__ndk15mutex4lockEv",
    "std::random_device::operator()()": "_ZNSt6__ndk113random_deviceclEv",
    "std::random_device::~random_device()": "_ZNSt6__ndk113random_deviceD1Ev",
    "std::random_device::random_device(const std::string&)": "_ZNSt6__ndk113random_deviceC2ERKNS_12basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEE",
}
# Addresses inside the stripped .text section, checked by their first instruction bytes.
TEXT = {
    0xAFC5A8: ("helper skipped by the decoder", "807f0800"),
    0xBE9085: ("start of the traced initialisation run", "4889442428"),
    0xBE9BD3: ("end of the traced initialisation run", "488b05"),
    0x143F7BE: ("decoder setup entry point", "554157"),
    0x143F8FE: ("decoder block entry point", "554157"),
}
# Hook ranges in decoder.c that are bounds rather than code addresses.
BOUNDS = {0xBE9BD2, 0x18B5500, 0x18B845F, 0x18B8460}


def addresses_in_source() -> set[int]:
    text = (ROOT / "decoder.c").read_text()
    # Library code lies in .text and .plt (0xa26a10..0x18b8460); smaller values are sizes and
    # the emulator's own mappings start at 0x20000000.
    return {int(v, 16) for v in re.findall(r"0x([0-9a-fA-F]{6,8})\b", text) if 0xA00000 <= int(v, 16) < 0x2000000}


def check_table() -> list[str]:
    known = set(PLT) | set(TEXT) | BOUNDS
    return [f"decoder.c uses 0x{a:x}, which check_addresses.py does not name" for a in sorted(addresses_in_source() - known)]


def read_library(source: Path, layout: dict) -> bytes:
    if source.suffix.lower() == ".apk":
        with zipfile.ZipFile(source) as archive:
            raw = archive.read(layout["library_entry"])
    else:
        raw = source.read_bytes()
    if hashlib.sha256(raw).hexdigest() != layout["source_sha256"]:
        raise ValueError("not the pinned Spotify library; see decoder/README.md")
    return raw


def sections(raw: bytes) -> dict[str, tuple[int, int, int, int, int]]:
    shoff, = struct.unpack_from("<Q", raw, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", raw, 0x3A)
    headers = [struct.unpack_from("<IIQQQQIIQQ", raw, shoff + i * shentsize) for i in range(shnum)]
    names = headers[shstrndx][4]

    def name(offset: int) -> str:
        return raw[names + offset:raw.index(b"\0", names + offset)].decode()

    # name -> (address, file offset, size, link, entry size)
    return {name(h[0]): (h[3], h[4], h[5], h[6], h[9]) for h in headers}


def check_library(raw: bytes) -> list[str]:
    table = sections(raw)
    plt_addr, plt_off, plt_size, _, _ = table[".plt"]
    _, rela_off, rela_size, _, _ = table[".rela.plt"]
    _, sym_off, _, _, _ = table[".dynsym"]
    _, str_off, _, _, _ = table[".dynstr"]
    text_addr, text_off, text_size, _, _ = table[".text"]
    got_to_symbol = {}
    for offset in range(rela_off, rela_off + rela_size, 24):
        r_offset, r_info, _ = struct.unpack_from("<QQq", raw, offset)
        st_name, = struct.unpack_from("<I", raw, sym_off + (r_info >> 32) * 24)
        got_to_symbol[r_offset] = raw[str_off + st_name:raw.index(b"\0", str_off + st_name)].decode()
    problems = []
    for address, name in sorted(PLT.items()):
        if not plt_addr <= address < plt_addr + plt_size:
            problems.append(f"0x{address:x} is outside .plt")
            continue
        stub = plt_off + address - plt_addr
        if raw[stub:stub + 2] != b"\xff\x25":
            problems.append(f"0x{address:x} is not a PLT stub")
            continue
        disp, = struct.unpack_from("<i", raw, stub + 2)
        symbol = got_to_symbol.get(address + 6 + disp)
        if symbol != MANGLED.get(name, name):
            problems.append(f"0x{address:x} is {symbol}, expected {name}")
    for address, (purpose, prefix) in sorted(TEXT.items()):
        start = text_off + address - text_addr
        if not text_addr <= address < text_addr + text_size or raw[start:start + len(prefix) // 2].hex() != prefix:
            problems.append(f"0x{address:x} ({purpose}) does not hold the expected code")
    return problems


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("source", nargs="?", type=Path, help="pinned Spotify APK or extracted x86_64 library")
    args = parser.parse_args()
    problems = check_table()
    if args.source:
        layout = json.loads((ROOT / "layout.json").read_text())
        try:
            problems += check_library(read_library(args.source, layout))
        except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
            parser.exit(1, f"decoder: {error}\n")
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        sys.exit(1)
    print(f"{len(PLT) + len(TEXT)} decoder addresses named" + (" and verified against the library" if args.source else ""))


if __name__ == "__main__":
    main()
