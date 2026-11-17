"""Tests for reversible screensaver patching without GTK or root privileges."""
from pathlib import Path
from tempfile import TemporaryDirectory
import importlib.util
import unittest

script=Path(__file__).with_name("install.py")
spec=importlib.util.spec_from_file_location("penguin_cinnamon_install", script)
installer=importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallerTests(unittest.TestCase):
    def test_install_and_uninstall_round_trip(self):
        with TemporaryDirectory() as root:
            directory=Path(root)
            original=(
                "# mock Cinnamon MonitorView\n"
                "class MonitorView:\n"
                "    def set_next_wallpaper_image(self, image):\n"
                "        self.wallpaper_stack.transition_to_image(image)\n"
            )
            filename=directory/"monitorView.py"
            filename.write_text(original)
            installer.install(directory)
            patched=filename.read_text()
            self.assertIn("BEGIN PENGUIN LIVE BACKGROUND",patched)
            self.assertTrue((directory/"penguin_live.py").exists())
            self.assertTrue((directory/"monitorView.py.penguin-original").exists())
            installer.uninstall(directory)
            self.assertEqual(filename.read_text(),original)
            self.assertFalse((directory/"penguin_live.py").exists())

    def test_refuses_unknown_cinnamon_sources(self):
        with TemporaryDirectory() as root:
            directory=Path(root)
            filename=directory/"monitorView.py"
            filename.write_text("# changed upstream\n")
            with self.assertRaisesRegex(RuntimeError, "supported hook"):
                installer.install(directory)
            self.assertEqual(filename.read_text(),"# changed upstream\n")
            self.assertFalse((directory/"monitorView.py.penguin-original").exists())

    def test_refuses_clobbering_package_upgrade(self):
        with TemporaryDirectory() as root:
            directory=Path(root)
            filename=directory/"monitorView.py"
            filename.write_text("class MonitorView:\n"+installer.HOOK+"\n")
            installer.install(directory)
            filename.write_text("# package update replaced monitorView.py\n")
            with self.assertRaisesRegex(RuntimeError, "package update"):
                installer.uninstall(directory)
            self.assertEqual(filename.read_text(),"# package update replaced monitorView.py\n")


if __name__=="__main__":
    unittest.main()
