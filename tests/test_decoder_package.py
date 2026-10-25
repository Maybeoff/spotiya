import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("decoder_package", Path(__file__).resolve().parents[1] / "decoder/package.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DecoderPackageTests(unittest.TestCase):
    def payload(self):
        binary = bytearray(52)
        binary[:6] = b"\x7fELF\x01\x01"
        binary[18:20] = b"\x28\x00"
        return {
            "bin/playplay-decoder": bytes(binary),
            "licenses/unicorn/COPYING": b"test license",
            "sources.json": b"{}",
            "revisions.json": MODULE.encoded({"engine_sources": MODULE.sources(), "unicorn": MODULE.UNICORN_REVISION, "binary_sha256": MODULE.sha(binary)}),
        }

    def test_repackaging_is_reproducible_and_updates_only_release_metadata(self):
        payload = self.payload()
        with tempfile.TemporaryDirectory() as directory:
            path = MODULE.package(payload, "0.9.3", Path(directory))
            first = path.read_bytes()
            MODULE.package(payload, "0.9.3", Path(directory))
            self.assertEqual(path.read_bytes(), first)
            unpacked = MODULE.read_payload(first)
            self.assertEqual(unpacked["bin/playplay-decoder"], payload["bin/playplay-decoder"])
            self.assertEqual(json.loads(unpacked["manifest.json"])["spotiya_version"], "0.9.3")
            pin = {"engine_sources": MODULE.sources(), "binary_sha256": MODULE.sha(payload["bin/playplay-decoder"])}
            MODULE.validate_pin(unpacked, pin, MODULE.sources())
            changed_sources = dict(MODULE.sources(), **{"decoder/decoder.c": "changed"})
            with self.assertRaisesRegex(ValueError, "sources changed"):
                MODULE.validate_pin(unpacked, pin, changed_sources)
            unpacked["bin/playplay-decoder"] += b"changed"
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                MODULE.validate_pin(unpacked, pin, MODULE.sources())

    def test_bootstrap_archive_rejects_links_traversal_and_duplicate_binaries(self):
        for name, kind, repeats in (
            (MODULE.PREFIX + "../../outside", tarfile.REGTYPE, 1),
            (MODULE.PREFIX + "bin/playplay-decoder", tarfile.SYMTYPE, 1),
            (MODULE.PREFIX + "bin/playplay-decoder", tarfile.REGTYPE, 2),
        ):
            with self.subTest(name=name, kind=kind):
                output = io.BytesIO()
                with tarfile.open(fileobj=output, mode="w:gz") as archive:
                    for _ in range(repeats):
                        item = tarfile.TarInfo(name)
                        item.type = kind
                        archive.addfile(item, io.BytesIO())
                with self.assertRaises(ValueError):
                    MODULE.read_payload(output.getvalue())


if __name__ == "__main__":
    unittest.main()
