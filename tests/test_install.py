from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT=Path(__file__).resolve().parents[1]


class InstallTests(unittest.TestCase):
    def test_staged_install_and_update(self):
        with tempfile.TemporaryDirectory(prefix="snipchord-install-") as temporary:
            prefix=Path(temporary)/"prefix with spaces"
            for _ in range(2):
                subprocess.run([sys.executable,str(ROOT/"tools/install.py"),"--prefix",str(prefix)],check=True,capture_output=True)
            launcher=prefix/"bin/snipchord"
            self.assertTrue(launcher.stat().st_mode & 0o111)
            version=subprocess.check_output([str(launcher),"--version"],text=True)
            self.assertEqual(version.strip(),"SnipChord 0.1.0")
            icon=prefix/"share/icons/hicolor/scalable/apps/snipchord.svg"
            self.assertEqual(icon.read_bytes(),(ROOT/"assets/snipchord.svg").read_bytes())
            subprocess.run(["desktop-file-validate",str(prefix/"share/applications/io.github.shane.snipchord.desktop")],check=True)

    def test_refuses_unmanaged_bundle(self):
        with tempfile.TemporaryDirectory(prefix="snipchord-install-") as temporary:
            prefix=Path(temporary)
            (prefix/"share/snipchord").mkdir(parents=True)
            result=subprocess.run([sys.executable,str(ROOT/"tools/install.py"),"--prefix",str(prefix)],capture_output=True)
            self.assertNotEqual(result.returncode,0)
            self.assertFalse((prefix/"share/snipchord/.snipchord-managed").exists())
