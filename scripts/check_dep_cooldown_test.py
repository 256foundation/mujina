"""Unit tests for check-dep-cooldown. Run with python3 -m unittest."""

import importlib.machinery
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

SCRIPT = Path(__file__).with_name("check-dep-cooldown")
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
NOW = datetime(2026, 9, 1, tzinfo=timezone.utc)


def load_script():
    loader = importlib.machinery.SourceFileLoader("check_dep_cooldown", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


check_dep_cooldown = load_script()


def lock_text(*packages):
    """A Cargo.lock holding (name, version, source) entries; source None is a path."""
    text = "version = 4\n\n"
    for name, version, source in packages:
        text += f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
        if source:
            text += f'source = "{source}"\n'
        text += "\n"
    return text


def days_ago(days):
    return (NOW - timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ")


class CheckLockAgeTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.previous_cwd = os.getcwd()
        self.addCleanup(os.chdir, self.previous_cwd)
        os.chdir(self.dir.name)
        self.cargo_home = Path(self.dir.name, "cargo-home")
        os.environ["CARGO_HOME"] = str(self.cargo_home)
        self.addCleanup(os.environ.pop, "CARGO_HOME")
        Path(".cargo").mkdir()
        Path(".cargo/config.toml").write_text(
            '[registry]\nglobal-min-publish-age = "7 days"\n'
        )

    def cache(self, name, **pubtimes):
        """Write a sparse index cache entry: version=pubtime, or None for no field."""
        path = self.cargo_home / "registry/index/index.crates.io-test/.cache"
        path = path / check_dep_cooldown.index_path(name)
        body = b"\x03\x02\x00\x00\x00" + b"etag\0"
        for version, pubtime in pubtimes.items():
            entry = {"vers": version}
            if pubtime:
                entry["pubtime"] = pubtime
            body += version.encode() + b"\0" + json.dumps(entry).encode() + b"\0"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body)

    def commit_base(self, text):
        Path("Cargo.lock").write_text(text)
        # Ignore the user's git config so signing and hooks stay out.
        env = dict(
            os.environ,
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_AUTHOR_NAME="t",
            GIT_AUTHOR_EMAIL="t@t",
            GIT_COMMITTER_NAME="t",
            GIT_COMMITTER_EMAIL="t@t",
        )
        for args in (
            ["init", "-q"],
            ["add", "Cargo.lock"],
            ["commit", "-q", "-m", "base"],
        ):
            subprocess.run(["git", *args], check=True, env=env)

    def test_young_version_added_since_base_fails(self):
        self.commit_base(lock_text(("old", "1.0.0", CRATES_IO)))
        Path("Cargo.lock").write_text(
            lock_text(("old", "1.0.0", CRATES_IO), ("fresh", "2.0.0", CRATES_IO))
        )
        self.cache("fresh", **{"2.0.0": days_ago(6)})
        added, problems = check_dep_cooldown.check("HEAD", NOW)
        self.assertEqual(added, [("fresh", "2.0.0")])
        self.assertEqual(len(problems), 1)
        self.assertIn("fresh 2.0.0", problems[0])
        self.assertIn("6 days old, cooldown is 7 days", problems[0])

    def test_aged_version_passes(self):
        self.commit_base(lock_text())
        Path("Cargo.lock").write_text(lock_text(("aged", "1.0.0", CRATES_IO)))
        self.cache("aged", **{"1.0.0": days_ago(7)})
        added, problems = check_dep_cooldown.check("HEAD", NOW)
        self.assertEqual(added, [("aged", "1.0.0")])
        self.assertEqual(problems, [])

    def test_version_already_in_base_is_not_checked(self):
        text = lock_text(("fresh", "2.0.0", CRATES_IO))
        self.commit_base(text)
        Path("Cargo.lock").write_text(text)
        added, problems = check_dep_cooldown.check("HEAD", NOW)
        self.assertEqual((added, problems), ([], []))

    def test_git_and_path_sources_are_ignored(self):
        self.commit_base(lock_text())
        Path("Cargo.lock").write_text(
            lock_text(
                ("member", "0.1.0", None),
                ("pinned", "0.2.0", "git+https://example.org/pinned#abc"),
            )
        )
        added, problems = check_dep_cooldown.check("HEAD", NOW)
        self.assertEqual((added, problems), ([], []))

    def test_missing_cache_entry_fails_with_fetch_hint(self):
        Path("Cargo.lock").write_text(lock_text(("unseen", "1.0.0", CRATES_IO)))
        added, problems = check_dep_cooldown.check(None, NOW)
        self.assertEqual(len(problems), 1)
        self.assertIn("unseen 1.0.0: not in the local index cache", problems[0])
        self.assertIn("cargo fetch --locked", problems[0])

    def test_entry_without_pubtime_fails(self):
        Path("Cargo.lock").write_text(lock_text(("undated", "1.0.0", CRATES_IO)))
        self.cache("undated", **{"1.0.0": None})
        added, problems = check_dep_cooldown.check(None, NOW)
        self.assertIn("not in the local index cache", problems[0])

    def test_whole_lock_is_checked_without_a_base(self):
        Path("Cargo.lock").write_text(
            lock_text(("fresh", "2.0.0", CRATES_IO), ("aged", "1.0.0", CRATES_IO))
        )
        self.cache("fresh", **{"2.0.0": days_ago(1)})
        self.cache("aged", **{"1.0.0": days_ago(30)})
        added, problems = check_dep_cooldown.check(None, NOW)
        self.assertEqual(added, [("aged", "1.0.0"), ("fresh", "2.0.0")])
        self.assertEqual(len(problems), 1)
        self.assertIn("fresh 2.0.0", problems[0])

    def test_unknown_base_ref_is_an_error(self):
        Path("Cargo.lock").write_text(lock_text())
        subprocess.run(["git", "init", "-q"], check=True)
        with self.assertRaises(SystemExit):
            check_dep_cooldown.check("nosuchref", NOW)

    def test_cooldown_accepts_rfc_durations(self):
        for text, expected in (
            ("7 days", timedelta(days=7)),
            ("1 day", timedelta(days=1)),
            ("12 hours", timedelta(hours=12)),
            ("90 minutes", timedelta(minutes=90)),
        ):
            Path(".cargo/config.toml").write_text(
                f'[registry]\nglobal-min-publish-age = "{text}"\n'
            )
            self.assertEqual(check_dep_cooldown.cooldown(), expected, text)

    def test_cooldown_rejects_other_forms(self):
        Path(".cargo/config.toml").write_text(
            '[registry]\nglobal-min-publish-age = "P7D"\n'
        )
        with self.assertRaises(SystemExit):
            check_dep_cooldown.cooldown()

    def test_index_path_follows_cargo_layout(self):
        self.assertEqual(check_dep_cooldown.index_path("a"), "1/a")
        self.assertEqual(check_dep_cooldown.index_path("h2"), "2/h2")
        self.assertEqual(check_dep_cooldown.index_path("log"), "3/l/log")
        self.assertEqual(check_dep_cooldown.index_path("serde"), "se/rd/serde")
        self.assertEqual(check_dep_cooldown.index_path("Inflector"), "in/fl/inflector")


if __name__ == "__main__":
    unittest.main()
