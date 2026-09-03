import hashlib
import json
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]


class ReleaseToolsTest(unittest.TestCase):
    def test_package_release_builds_unix_and_windows_archives(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            binary = root / "cfy"
            binary.write_text("fixture")
            output = root / "dist"

            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/package-release.py"),
                    "--binary",
                    str(binary),
                    "--version",
                    "1.2.3",
                    "--target",
                    "x86_64-unknown-linux-gnu",
                    "--output",
                    str(output),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            unix_archive = output / "cfy-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
            with tarfile.open(unix_archive, "r:gz") as archive:
                self.assertIn("cfy-v1.2.3-x86_64-unknown-linux-gnu/cfy", archive.getnames())

            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/package-release.py"),
                    "--binary",
                    str(binary),
                    "--version",
                    "1.2.3",
                    "--target",
                    "x86_64-pc-windows-msvc",
                    "--output",
                    str(output),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            windows_archive = output / "cfy-v1.2.3-x86_64-pc-windows-msvc.zip"
            with zipfile.ZipFile(windows_archive) as archive:
                self.assertIn("cfy-v1.2.3-x86_64-pc-windows-msvc/cfy.exe", archive.namelist())

    def test_checksum_generator_sorts_and_hashes_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            first = root / "b.zip"
            second = root / "a.tar.gz"
            first.write_bytes(b"b")
            second.write_bytes(b"a")
            sums = root / "SHA256SUMS"
            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/generate-checksums.py"),
                    str(first),
                    str(second),
                    "--output",
                    str(sums),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                sums.read_text().splitlines(),
                [
                    f"{hashlib.sha256(b'a').hexdigest()}  a.tar.gz",
                    f"{hashlib.sha256(b'b').hexdigest()}  b.zip",
                ],
            )

    def test_release_version_matches_workspace_and_npm(self):
        package_version = json.loads((ROOT / "package.json").read_text())["version"]
        result = subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts/check-release-version.py"),
                "--tag",
                f"v{package_version}",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.stdout.strip(), package_version)


if __name__ == "__main__":
    unittest.main()
