"""Portable packaging checks: python3 -m unittest discover -s tests -p 'test_*.py'."""
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]


class MacOSPackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="scriptmcp packaging ")
        self.addCleanup(self.temp.cleanup)
        self.stage = Path(self.temp.name) / "stage"
        self.stage.mkdir()
        self.binary = self.stage / "scriptmcp"
        self.binary.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
        self.binary.chmod(0o755)
        self.app = self.stage / "ScriptMCP.app"
        subprocess.run(
            ["bash", str(ROOT / "tools/package-macos-app.sh"),
             str(self.binary), str(self.app), "0.2.1"], check=True,
        )

    def test_bundle_metadata_and_launcher(self):
        with (self.app / "Contents/Info.plist").open("rb") as file:
            info = plistlib.load(file)
        self.assertEqual(info["CFBundlePackageType"], "APPL")
        self.assertEqual(info["CFBundleShortVersionString"], "0.2.1")
        launcher = self.app / "Contents/MacOS" / info["CFBundleExecutable"]
        self.assertTrue(os.access(launcher, os.X_OK))
        self.assertTrue(os.access(launcher.parent / "scriptmcp", os.X_OK))
        # No terminal stdin, just like Finder. Spaces in paths/args must survive.
        output = subprocess.check_output(
            [str(launcher), "--console", "--scripts", "/some script folder"],
            stdin=subprocess.DEVNULL, text=True,
        )
        self.assertEqual(output.splitlines(),
                         ["ui", "--console", "--scripts", "/some script folder"])

    def check_archive(self, use_zip):
        # Exercise the actual release script's ZIP function without compiling.
        script = (ROOT / "tools/package-release.sh").read_text()
        function = script.split("write_zip() {", 1)[1].split("\npackage_target()", 1)[0]
        command = "write_zip() {" + function
        if use_zip:
            command += '\nhas_cmd() { command -v "$1" >/dev/null 2>&1; }\n'
        else:
            command += '\nhas_cmd() { return 1; }\n'
        command += '\nwrite_zip "$1" "$2"\n'
        dest = Path(self.temp.name) / "release.zip"
        subprocess.run(["bash", "-eu", "-c", command, "test",
                        str(self.stage), str(dest)], check=True)
        with zipfile.ZipFile(dest) as archive:
            for name in ["scriptmcp", "ScriptMCP.app/Contents/MacOS/scriptmcp",
                         "ScriptMCP.app/Contents/MacOS/ScriptMCP"]:
                entry = archive.getinfo(name)
                self.assertTrue((entry.external_attr >> 16) & 0o111, name)
            plistlib.loads(archive.read("ScriptMCP.app/Contents/Info.plist"))

    def test_python_zip_fallback(self):
        self.check_archive(use_zip=False)

    @unittest.skipUnless(shutil.which("zip"), "zip is not installed")
    def test_system_zip(self):
        self.check_archive(use_zip=True)


if __name__ == "__main__":
    unittest.main()
