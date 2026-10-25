import importlib.util
import os
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("check_addresses", Path(__file__).resolve().parents[1] / "decoder/check_addresses.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
APK = os.environ.get("SPOTIYA_SPOTIFY_APK")


class DecoderAddressTests(unittest.TestCase):
    def test_every_library_address_in_decoder_c_is_named(self):
        self.assertEqual(MODULE.check_table(), [])

    @unittest.skipUnless(APK, "set SPOTIYA_SPOTIFY_APK to the pinned Spotify APK")
    def test_names_match_the_pinned_library_and_mistakes_are_caught(self):
        import json
        layout = json.loads((MODULE.ROOT / "layout.json").read_text())
        raw = MODULE.read_library(Path(APK), layout)
        self.assertEqual(MODULE.check_library(raw), [])
        plt, text = dict(MODULE.PLT), dict(MODULE.TEXT)
        try:
            MODULE.PLT[0x18B7160], MODULE.PLT[0x18B7170] = "memmove", "memcpy"
            MODULE.TEXT[0x143F8FE] = ("shifted entry point", "4157")
            problems = MODULE.check_library(raw)
        finally:
            MODULE.PLT.clear(); MODULE.PLT.update(plt)
            MODULE.TEXT.clear(); MODULE.TEXT.update(text)
        self.assertEqual(len(problems), 3, problems)


if __name__ == "__main__":
    unittest.main()
