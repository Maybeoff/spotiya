import json
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(platform.libc_ver()[0] == "glibc" and shutil.which("cc"), "glibc C compiler required")
class NativeStdioTests(unittest.TestCase):
    def test_vendor_constructor_and_destructor_do_not_corrupt_json(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "vendor.c"
            binary = Path(directory) / "check"
            source.write_text(r'''
#include <stdio.h>
#include <unistd.h>
void spotiya_native_stderr(void);
__attribute__((constructor)) static void before(void) { printf("vendor constructor\n"); }
__attribute__((destructor)) static void after(void) { puts("vendor destructor"); }
int main(void) {
    spotiya_native_stderr();
    const char output[] = "{\"profile\":\"mini1\"}\n";
    if (write(STDOUT_FILENO, output, sizeof(output)-1) != sizeof(output)-1) return 1;
    printf("vendor runtime\n");
    return 0;
}
''')
            subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
                            str(ROOT / "controls/stdio.c"), str(source), "-o", str(binary)], check=True)
            result = subprocess.run([str(binary)], text=True, capture_output=True, check=True)
            self.assertEqual(json.loads(result.stdout), {"profile": "mini1"})
            self.assertIn("vendor constructor", result.stderr)
            self.assertIn("vendor runtime", result.stderr)
            self.assertIn("vendor destructor", result.stderr)


if __name__ == "__main__":
    unittest.main()
