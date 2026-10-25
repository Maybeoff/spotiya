#!/usr/bin/env python3
"""Package an explicitly rebuilt decoder, or the checksum-pinned release binary."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import tarfile
import tempfile
import tomllib
import urllib.request
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
ASSET = "spotiya-playplay-armv7.tar.gz"
PREFIX = "spotiya-playplay/playplay/"
ENGINE_FILES = ("decoder/decoder.c", "decoder/prepare.py", "decoder/layout.json", "scripts/build-playplay.sh")
UNICORN_REVISION = "8028ec436f2d9376525352dd38ed9ed6b9f6be10"
LIMIT = 16 * 1024 * 1024


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sources() -> dict[str, str]:
    return {name: sha((ROOT / name).read_bytes()) for name in ENGINE_FILES}


def encoded(value: dict) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def arm_binary(data: bytes) -> None:
    if len(data) < 52 or data[:6] != b"\x7fELF\x01\x01" or data[18:20] != b"\x28\x00":
        raise ValueError("expected an ARMv7 little-endian ELF32 decoder")


def local_payload(binary: Path, unicorn: Path) -> dict[str, bytes]:
    import subprocess

    revision = subprocess.check_output(["git", "-C", str(unicorn), "rev-parse", "HEAD"], text=True).strip()
    dirty = subprocess.check_output(["git", "-C", str(unicorn), "status", "--porcelain", "--untracked-files=no"], text=True)
    if revision != UNICORN_REVISION or dirty:
        raise ValueError("expected the clean pinned Unicorn checkout")
    data = binary.read_bytes()
    arm_binary(data)
    layout = json.loads((ROOT / "decoder/layout.json").read_text())
    payload = {"bin/playplay-decoder": data, "licenses/spotiya/LICENSE": (ROOT / "LICENSE").read_bytes()}
    for name in ("COPYING", "COPYING_GLIB", "COPYING.LGPL2", "qemu/COPYING", "qemu/COPYING.LIB", "qemu/LICENSE"):
        payload["licenses/unicorn/" + name.replace("qemu/", "QEMU_")] = (unicorn / name).read_bytes()
    payload["sources.json"] = encoded({
        "spotiya": "https://github.com/klockky/spotiya",
        "unicorn": "https://github.com/unicorn-engine/unicorn/tree/" + revision,
        "spotify_android_version": "9.1.92.3058",
        "spotify_library_sha256": layout["source_sha256"],
        "notice": "This experimental helper embeds selected native code and tables from the Spotify Android application and executes them using Unicorn. These fragments are not covered by Spotiya's MIT license. The complete APK, account data and credentials are not included. See decoder/README.md for rebuilding from the matching APK.",
    })
    payload["revisions.json"] = encoded({"engine_sources": sources(), "unicorn": revision, "binary_sha256": sha(data)})
    return payload


def read_payload(data: bytes) -> dict[str, bytes]:
    if len(data) > LIMIT:
        raise ValueError("decoder archive exceeds 16 MiB")
    result = {}
    total = 0
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for index, member in enumerate(archive):
            if index >= 256:
                raise ValueError("too many archive entries")
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or not member.name.startswith(PREFIX):
                raise ValueError("unsafe decoder archive path")
            if member.isdir():
                continue
            name = member.name[len(PREFIX):]
            allowed = name in ("bin/playplay-decoder", "manifest.json", "SHA256SUMS", "sources.json", "revisions.json") or (name.startswith("licenses/") and len(PurePosixPath(name).parts) == 3)
            total += member.size
            if not member.isfile() or not allowed or name in result or total > 32 * 1024 * 1024:
                raise ValueError("unexpected decoder archive entry")
            result[name] = archive.extractfile(member).read()
    arm_binary(result["bin/playplay-decoder"])
    return result


def validate_pin(payload: dict[str, bytes], pin: dict, engine: dict[str, str]) -> None:
    revisions = json.loads(payload["revisions.json"])
    if pin["engine_sources"] != engine or revisions["engine_sources"] != engine:
        raise ValueError("decoder sources changed; rebuild the binary and update decoder/prebuilt.json")
    if sha(payload["bin/playplay-decoder"]) != pin["binary_sha256"] or revisions["binary_sha256"] != pin["binary_sha256"]:
        raise ValueError("pinned decoder binary checksum mismatch")
    if revisions["unicorn"] != UNICORN_REVISION:
        raise ValueError("unexpected Unicorn revision")
    if not any(name.startswith("licenses/unicorn/") for name in payload) or "sources.json" not in payload:
        raise ValueError("decoder package is missing notices")


def fetch_payload(pin: dict) -> dict[str, bytes]:
    url = pin["url"]
    if not url.startswith("https://github.com/klockky/spotiya/releases/download/"):
        raise ValueError("expected a pinned Spotiya release URL")
    cache = Path(os.environ.get("SPOTIYA_CACHE", Path.home() / ".cache/spotiya")) / "downloads"
    cache.mkdir(parents=True, exist_ok=True)
    path = cache / (pin["archive_sha256"] + ".tar.gz")
    if not path.is_file():
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read(LIMIT + 1)
        if len(data) > LIMIT or sha(data) != pin["archive_sha256"]:
            raise ValueError("downloaded decoder archive checksum mismatch")
        with tempfile.NamedTemporaryFile(dir=cache, delete=False) as output:
            temporary = Path(output.name)
            output.write(data)
        temporary.replace(path)
    data = path.read_bytes()
    if sha(data) != pin["archive_sha256"]:
        raise ValueError("cached decoder archive checksum mismatch")
    payload = read_payload(data)
    validate_pin(payload, pin, sources())
    return payload


def package(payload: dict[str, bytes], version: str, destination: Path) -> Path:
    payload = dict(payload)
    arm_binary(payload["bin/playplay-decoder"])
    payload["manifest.json"] = encoded({"format": 1, "target": "armv7-unknown-linux-gnueabihf", "spotiya_version": version, "files": {"bin/playplay-decoder": sha(payload["bin/playplay-decoder"])}})
    payload.pop("SHA256SUMS", None)
    payload["SHA256SUMS"] = "".join(f"{sha(data)}  {name}\n" for name, data in sorted(payload.items())).encode()
    destination.mkdir(parents=True, exist_ok=True)
    path = destination / ASSET
    with path.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0, compresslevel=9) as zipped, tarfile.open(fileobj=zipped, mode="w") as archive:
        for name, data in sorted(payload.items()):
            info = tarfile.TarInfo(PREFIX + name)
            info.size = len(data)
            info.mode = 0o755 if name.startswith("bin/") else 0o644
            archive.addfile(info, io.BytesIO(data))
    (destination / "PLAYPLAY_SHA256SUMS").write_text(f"{sha(path.read_bytes())}  {ASSET}\n")
    return path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="explicitly package a freshly rebuilt ARM decoder")
    parser.add_argument("--unicorn-source", type=Path, help="clean pinned source checkout for license notices")
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    if bool(args.binary) != bool(args.unicorn_source):
        parser.error("--binary and --unicorn-source must be supplied together")
    payload = local_payload(args.binary, args.unicorn_source) if args.binary else fetch_payload(json.loads((ROOT / "decoder/prebuilt.json").read_text()))
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    path = package(payload, version, args.output)
    print(f"{path.name}: {path.stat().st_size} bytes; decoder: {len(payload['bin/playplay-decoder'])} bytes")


if __name__ == "__main__":
    main()
