import hashlib
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("airplay_fetch", Path(__file__).resolve().parents[1] / "airplay/fetch.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AirplayFetchTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name)
        self.root = self.base / "build"
        self.recipe = self.base / "recipe"
        self.mirror = self.base / "mirror"
        for path in (self.root, self.recipe, self.mirror):
            path.mkdir()
        projects = MODULE.PROJECTS
        MODULE.PROJECTS = {}
        self.addCleanup(setattr, MODULE, "PROJECTS", projects)

    def publish(self, version, checksum=None):
        archive = self.mirror / f"lib-{version}.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            data = version.encode()
            info = tarfile.TarInfo(f"lib-{version}/VERSION")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
        sha = checksum or hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.recipe / "sources.json").write_text(json.dumps([{"name": "lib", "url": archive.as_uri(), "sha256": sha}]))

    def test_version_bump_replaces_the_extracted_source(self):
        self.publish("1.0")
        MODULE.prepare(self.root, self.recipe)
        (self.root / "lib/build-output").write_text("stale")
        self.publish("2.0")
        MODULE.prepare(self.root, self.recipe)
        self.assertEqual((self.root / "lib/VERSION").read_text(), "2.0")
        self.assertFalse((self.root / "lib/build-output").exists())

    def test_unchanged_source_is_left_in_place(self):
        self.publish("1.0")
        MODULE.prepare(self.root, self.recipe)
        (self.root / "lib/build-output").write_text("kept")
        MODULE.prepare(self.root, self.recipe)
        self.assertTrue((self.root / "lib/build-output").exists())

    def test_tree_without_marker_is_extracted_again(self):
        (self.root / "lib").mkdir()
        (self.root / "lib/VERSION").write_text("partial")
        self.publish("1.0")
        MODULE.prepare(self.root, self.recipe)
        self.assertEqual((self.root / "lib/VERSION").read_text(), "1.0")

    def test_extraction_invalidates_the_build_stamp(self):
        self.publish("1.0")
        MODULE.prepare(self.root, self.recipe)
        (self.root / "logs").mkdir()
        (self.root / "logs/lib.done").write_text("signature")
        (self.root / "logs/other.done").write_text("signature")
        MODULE.prepare(self.root, self.recipe)
        self.assertTrue((self.root / "logs/lib.done").exists())
        (self.root / "lib" / MODULE.MARKER).unlink()
        MODULE.prepare(self.root, self.recipe)
        self.assertFalse((self.root / "logs/lib.done").exists())
        self.assertTrue((self.root / "logs/other.done").exists())

    def test_checksum_mismatch_keeps_no_archive_or_source(self):
        self.publish("1.0", "0" * 64)
        with self.assertRaisesRegex(RuntimeError, "checksum mismatch: lib"):
            MODULE.prepare(self.root, self.recipe)
        self.assertEqual(list((self.root / "downloads").iterdir()), [])
        self.assertFalse((self.root / "lib").exists())

    def test_corrupt_cached_archive_is_downloaded_again(self):
        self.publish("1.0")
        (self.root / "downloads").mkdir()
        (self.root / "downloads/lib-1.0.tar.gz").write_bytes(b"truncated")
        MODULE.prepare(self.root, self.recipe)
        self.assertEqual((self.root / "lib/VERSION").read_text(), "1.0")


if __name__ == "__main__":
    unittest.main()
