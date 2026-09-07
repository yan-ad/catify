import hashlib
import json
import os
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
                self.assertIn("cfy-v1.2.3-x86_64-unknown-linux-gnu/catify", archive.getnames())

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
                self.assertIn("cfy-v1.2.3-x86_64-pc-windows-msvc/catify.exe", archive.namelist())

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


    def test_shell_installer_falls_back_to_prerelease(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            version = "1.2.3-pre.0"
            target = "aarch64-apple-darwin"
            release = root / "releases" / f"v{version}"
            release.mkdir(parents=True)
            binary = root / "cfy"
            binary.write_text("#!/bin/sh\necho fixture\n")
            binary.chmod(0o755)

            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/package-release.py"),
                    "--binary", str(binary),
                    "--version", version,
                    "--target", target,
                    "--output", str(release),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/generate-checksums.py"),
                    str(release / f"cfy-v{version}-{target}.tar.gz"),
                    "--output", str(release / "SHA256SUMS"),
                ],
                check=True,
                capture_output=True,
                text=True,
            )

            tools = root / "tools"
            tools.mkdir()
            curl = tools / "curl"
            curl.write_text(
                "#!/bin/sh\n"
                "case \"$*\" in\n"
                "  *releases/latest*) exit 22 ;;\n"
                f"  *api.github.com*) printf '%s' '[{{\"tag_name\":\"v{version}\"}}]' ;;\n"
                "  *) exec /usr/bin/curl \"$@\" ;;\n"
                "esac\n"
            )
            curl.chmod(0o755)
            install = root / "install.sh"
            install.write_text((ROOT / "install.sh").read_text())
            install.chmod(0o755)
            destination = root / "bin"
            env = dict(os.environ)
            env.update({
                "PATH": f"{tools}:{env['PATH']}",
                "CFY_RELEASE_BASE_URL": f"file://{root / 'releases'}",
                "CFY_INSTALL_DIR": str(destination),
            })
            uname = tools / "uname"
            uname.write_text(
                "#!/bin/sh\n"
                "case \"$1\" in -s) echo Darwin ;; -m) echo arm64 ;; *) echo Darwin ;; esac\n"
            )
            uname.chmod(0o755)
            subprocess.run(["sh", str(install)], check=True, env=env, capture_output=True, text=True)
            self.assertTrue((destination / "cfy").is_file())

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
