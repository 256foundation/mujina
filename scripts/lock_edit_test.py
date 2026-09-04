"""Unit tests for lock-edit. Run with python3 -m unittest."""

import importlib.machinery
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("lock-edit")
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def load_script():
    loader = importlib.machinery.SourceFileLoader("lock_edit", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


lock_edit = load_script()


def entry(name, version, source=CRATES_IO, checksum="abc", deps=()):
    text = f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
    if source:
        text += f'source = "{source}"\nchecksum = "{checksum}"\n'
    if deps:
        text += "dependencies = [\n" + "".join(f' "{d}",\n' for d in deps) + "]\n"
    return text + "\n"


class LockEditTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.addCleanup(os.chdir, os.getcwd())
        os.chdir(self.dir.name)

    def write(self, *entries):
        text = "version = 4\n\n" + "".join(entries)
        Path("Cargo.lock").write_text(text)
        return text

    def test_drop_removes_only_that_entry(self):
        before = self.write(
            entry("a", "1.0.0", deps=("b",)), entry("b", "2.0.0"), entry("c", "3.0.0")
        )
        lock_edit.main(["lock-edit", "drop", "b"])
        self.assertEqual(
            Path("Cargo.lock").read_text(), before.replace(entry("b", "2.0.0"), "")
        )

    def test_drop_needs_a_version_when_two_are_locked(self):
        self.write(entry("dup", "1.0.0"), entry("dup", "2.0.0"))
        with self.assertRaises(SystemExit) as raised:
            lock_edit.main(["lock-edit", "drop", "dup"])
        self.assertIn("dup@VERSION", str(raised.exception))
        lock_edit.main(["lock-edit", "drop", "dup@1.0.0"])
        text = Path("Cargo.lock").read_text()
        self.assertNotIn('version = "1.0.0"', text)
        self.assertIn('version = "2.0.0"', text)

    def test_set_rewrites_version_and_drops_checksum(self):
        self.write(entry("h2", "0.4.15", checksum="old"), entry("z", "1.0.0"))
        lock_edit.main(["lock-edit", "set", "h2", "0.4.16"])
        text = Path("Cargo.lock").read_text()
        self.assertIn('name = "h2"\nversion = "0.4.16"\n', text)
        self.assertNotIn("old", text)
        self.assertIn(entry("z", "1.0.0"), text)

    def test_missing_crate_is_an_error(self):
        self.write(entry("a", "1.0.0"))
        with self.assertRaises(SystemExit) as raised:
            lock_edit.main(["lock-edit", "drop", "nope"])
        self.assertIn("not in Cargo.lock", str(raised.exception))

    def test_workspace_member_is_refused(self):
        self.write(entry("member", "0.1.0", source=None))
        with self.assertRaises(SystemExit) as raised:
            lock_edit.main(["lock-edit", "set", "member", "0.2.0"])
        self.assertIn("workspace member", str(raised.exception))

    def test_usage_on_bad_arguments(self):
        with self.assertRaises(SystemExit):
            lock_edit.main(["lock-edit", "drop"])


if __name__ == "__main__":
    unittest.main()
