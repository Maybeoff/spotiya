#!/usr/bin/env python3
"""Generate local decoder resources from one pinned, user-supplied APK or ELF."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import struct
import zipfile
from pathlib import Path

from elftools.elf.elffile import ELFFile

ROOT = Path(__file__).resolve().parent
PAGE_SIZE = 4096


def read_library(source: Path, layout: dict) -> bytes:
    if source.suffix.lower() == ".apk":
        with zipfile.ZipFile(source) as archive:
            member = archive.getinfo(layout["library_entry"])
            if member.file_size != layout["library_size"]:
                raise ValueError("unsupported APK library size")
            raw = archive.read(member)
    else:
        if source.stat().st_size != layout["library_size"]:
            raise ValueError("unsupported native library size")
        raw = source.read_bytes()
    if hashlib.sha256(raw).hexdigest() != layout["source_sha256"]:
        raise ValueError("unsupported APK library checksum; see decoder/README.md")
    return raw


def static_image(raw: bytes, layout: dict) -> bytes:
    limit = layout["module_limit"]
    image = bytearray(limit)
    elf = ELFFile(io.BytesIO(raw))
    for segment in elf.iter_segments():
        if segment["p_type"] == "PT_LOAD":
            begin, size = segment["p_vaddr"], segment["p_filesz"]
            if begin < 0 or begin + size > limit:
                raise ValueError("ELF segment exceeds the image bound")
            image[begin:begin + size] = segment.data()
    for relocation in elf.get_section_by_name(".rela.dyn").iter_relocations():
        if relocation["r_info_type"] == 8:
            offset = relocation["r_offset"]
            if not 0 <= offset <= limit - 8:
                raise ValueError("ELF relocation exceeds the image bound")
            struct.pack_into("<Q", image, offset, relocation["r_addend"])
    payload = b"".join(image[page:page + PAGE_SIZE] for page in layout["pages"])
    if hashlib.sha256(payload).hexdigest() != layout["image_sha256"]:
        raise ValueError("static image checksum mismatch")
    return payload


def write_header(output: Path, raw: bytes, layout: dict, payload: bytes) -> None:
    sites = layout["exception_sites"]
    catches, spans, calls = [], [], []
    for site in sites:
        spans.append((len(catches), len(site["catches"])))
        catches.extend(site["catches"])
    for pc in layout["main_calls"]:
        for index, site in enumerate(sites):
            if site["start"] <= pc < site["end"] and site["landing"]:
                calls.append((pc, index))
                break
    token_offset = layout["build_token_offset"]
    token = raw[token_offset:token_offset + 16]
    constant = bytes.fromhex(layout["decoder_constant_hex"])
    if len(token) != 16 or len(constant) != 16:
        raise ValueError("invalid build parameter size")
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w") as out:
        out.write("/* Generated locally from the pinned ELF. Do not commit. */\n")
        out.write("#include <stdint.h>\n")
        out.write("struct catch_entry { uint64_t type_address; uint32_t selector; };\n")
        out.write("struct catch_site { uint64_t landing; uint32_t begin, count; };\n")
        out.write("struct tracked_call { uint64_t pc; uint32_t site; };\n")
        out.write("static const uint32_t image_pages[] = {" + ",".join(hex(p) for p in layout["pages"]) + "};\n")
        out.write("static const struct catch_entry catch_entries[] = {\n")
        for catch in catches:
            out.write("{%s,%d},\n" % (hex(catch["indirect_type_address"]), catch["selector"]))
        out.write("};\nstatic const struct catch_site catch_sites[] = {\n")
        for site, (begin, count) in zip(sites, spans):
            out.write("{%s,%d,%d},\n" % (hex(site["landing"]), begin, count))
        out.write("};\nstatic const struct tracked_call tracked_calls[] = {\n")
        for pc, site in calls:
            out.write("{%s,%d},\n" % (hex(pc), site))
        out.write("};\nstatic const uint8_t decoder_constant[16] = {" + ",".join(hex(b) for b in constant) + "};\n")
        out.write("static const uint8_t playplay_build_token[16] = {" + ",".join(hex(b) for b in token) + "};\n")
        out.write("static const uint8_t initial_image[] = {\n")
        for offset in range(0, len(payload), 32):
            out.write(",".join(str(b) for b in payload[offset:offset + 32]) + ",\n")
        out.write('};\n_Static_assert(sizeof(initial_image) == sizeof(image_pages)/sizeof(image_pages[0])*4096, "image shape");\n')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="matching Spotify APK or extracted x86_64 library")
    parser.add_argument("output", type=Path, help="generated resources.h outside the source tree")
    args = parser.parse_args()
    layout = json.loads((ROOT / "layout.json").read_text())
    try:
        raw = read_library(args.source, layout)
        payload = static_image(raw, layout)
        write_header(args.output, raw, layout, payload)
    except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        parser.exit(1, f"decoder: {error}\n")
    print(f"Prepared {len(payload)} static bytes from the pinned library")


if __name__ == "__main__":
    main()
