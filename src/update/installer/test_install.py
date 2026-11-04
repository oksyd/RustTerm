"""Small filesystem-only installer regressions; never launch RustTerm or build a binary."""

import hashlib
import contextlib
import io
import importlib.util
import json
from pathlib import Path
import os
import tempfile
import unittest
from unittest import mock
import uuid


spec = importlib.util.spec_from_file_location("rustterm_installer", Path(__file__).with_name("install.py"))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.package = root / "package"
        self.prefix = root / 'user install % "$`'
        for source, _ in installer.FILES.values():
            path = self.package / source
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        desktop = self.package / "share/applications/io.rustterm.RustTerm.desktop"
        desktop.write_text("[Desktop Entry]\nType=Application\nName=RustTerm\nExec=rustterm\n")
        self.set_binary(b"old binary fixture")

    def set_binary(self, data):
        (self.package / "bin/rustterm").write_bytes(data)
        (self.package / "build-info.json").write_text(json.dumps({"binary_sha256": hashlib.sha256(data).hexdigest()}))

    def snapshot(self):
        return {relative: (self.prefix / relative).read_bytes() for relative in installer.FILES}

    def test_install_and_uninstall_preserve_user_configuration(self):
        installer.install(self.package, self.prefix)
        config = self.prefix / "config.toml"
        config.write_text("user data")
        self.assertEqual((self.prefix / "bin/rustterm").stat().st_mode & 0o777, 0o755)
        desktop = (self.prefix / "share/applications/io.rustterm.RustTerm.desktop").read_text()
        self.assertIn("%%", desktop)
        self.assertNotIn("Exec=rustterm\n", desktop)
        installer.install(self.package, self.prefix, uninstall=True)
        self.assertEqual(config.read_text(), "user data")
        self.assertTrue(all(not (self.prefix / relative).exists() for relative in installer.FILES))

    def test_group_writable_user_directories_are_preserved_through_install_update_and_remove(self):
        directories = [self.prefix, *(self.prefix / name for name in [
            'bin', 'share', 'share/icons', 'share/icons/hicolor',
            'share/icons/hicolor/scalable', 'share/icons/hicolor/scalable/apps',
        ])]
        for path in directories:
            path.mkdir(parents=True, exist_ok=True)
            path.chmod(0o775)
        unrelated = self.prefix / 'bin/other-program'
        unrelated.write_bytes(b'untouched')
        installer.install(self.package, self.prefix)
        self.set_binary(b'new version')
        installer.install(self.package, self.prefix)
        self.assertEqual((self.prefix / 'bin/rustterm').read_bytes(), b'new version')
        installer.install(self.package, self.prefix, uninstall=True)
        for path in directories:
            self.assertEqual(path.stat().st_mode & 0o777, 0o775)
        self.assertEqual(unrelated.read_bytes(), b'untouched')

    def test_world_writable_foreign_owned_and_symlink_directories_are_rejected(self):
        self.prefix.mkdir()
        self.prefix.chmod(0o777)
        with self.assertRaisesRegex(ValueError, 'writable by everyone'):
            installer.install(self.package, self.prefix)
        self.prefix.chmod(0o775)
        with mock.patch.object(installer.os, 'geteuid', return_value=os.geteuid() + 1):
            with self.assertRaisesRegex(ValueError, 'owned by the current user'):
                installer.install(self.package, self.prefix)
        outside = self.prefix.parent / 'outside'
        outside.mkdir()
        (self.prefix / 'bin').symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'not a symlink'):
            installer.install(self.package, self.prefix)
        self.assertEqual(list(outside.iterdir()), [])

    def test_recovery_hint_only_appears_for_a_pending_transaction(self):
        self.prefix.mkdir()
        for pending in [False, True]:
            if pending:
                (self.prefix / installer.JOURNAL).write_text('pending transaction')
            error = io.StringIO()
            with mock.patch.object(installer.sys, 'argv', ['install.py', '--prefix', str(self.prefix)]), \
                 mock.patch.object(installer, 'install', side_effect=ValueError('directory error')), \
                 contextlib.redirect_stderr(error):
                self.assertEqual(installer.main(), 1)
            self.assertIn('directory error', error.getvalue())
            self.assertEqual('transaction remains' in error.getvalue(), pending)

    def test_mid_commit_failure_restores_every_old_file(self):
        installer.install(self.package, self.prefix)
        previous = self.snapshot()
        self.set_binary(b"new binary fixture")
        replace = os.replace
        failed = False
        def injected(source, destination):
            nonlocal failed
            if not failed and str(source).endswith(".new") and str(destination).endswith(".desktop"):
                failed = True
                raise OSError("injected disk error")
            return replace(source, destination)
        with mock.patch.object(installer.os, "replace", side_effect=injected):
            with self.assertRaises(OSError):
                installer.install(self.package, self.prefix)
        self.assertTrue(failed)
        self.assertEqual(self.snapshot(), previous)
        self.assertFalse((self.prefix / installer.JOURNAL).exists())

    def test_corrupt_package_fails_before_any_installed_file_changes(self):
        installer.install(self.package, self.prefix)
        previous = self.snapshot()
        (self.package / "bin/rustterm").write_bytes(b"corrupt")
        with self.assertRaises(ValueError):
            installer.install(self.package, self.prefix)
        self.assertEqual(self.snapshot(), previous)

    def test_stale_updater_cannot_overwrite_a_newer_installation(self):
        installer.install(self.package, self.prefix)
        marker = self.prefix / "share/doc/rustterm/build-info.json"
        expected = hashlib.sha256(marker.read_bytes()).hexdigest()
        self.set_binary(b"newer release")
        installer.install(self.package, self.prefix)
        newer = self.snapshot()
        self.set_binary(b"stale downloaded release")
        with self.assertRaisesRegex(ValueError, "installed version changed"):
            installer.install(self.package, self.prefix, expected_manifest_sha256=expected)
        self.assertEqual(self.snapshot(), newer)

    def test_symlink_destination_never_redirects_writes(self):
        installer.install(self.package, self.prefix)
        target = self.prefix / "share/doc/rustterm/LICENSE"
        target.unlink()
        outside = self.prefix.parent / "outside"
        outside.write_bytes(b"untouched")
        target.symlink_to(outside)
        with self.assertRaises(ValueError):
            installer.install(self.package, self.prefix)
        self.assertEqual(outside.read_bytes(), b"untouched")
        self.assertEqual((self.prefix / "bin/rustterm").read_bytes(), b"old binary fixture")

    def test_interrupted_prepared_transaction_is_recovered(self):
        installer.install(self.package, self.prefix)
        item = {"relative": "bin/rustterm", "existed": True}
        identifier = str(uuid.uuid4())
        target, staged, backup = installer.transaction_paths(self.prefix, item, identifier)
        os.link(target, backup)
        staged.write_bytes(b"interrupted replacement")
        installer.write_journal(self.prefix, {"id": identifier, "state": "prepared", "files": [item]})
        os.replace(staged, target)
        installer.recover(self.prefix)
        self.assertEqual(target.read_bytes(), b"old binary fixture")
        self.assertFalse(backup.exists())

    def test_upgrade_does_not_require_or_remove_legacy_patch_notice(self):
        installer.install(self.package, self.prefix)
        relative = "share/doc/rustterm/THIRD_PARTY_PATCHES.md"
        legacy = self.prefix / relative
        legacy.write_text("Historical patch notice")
        self.assertFalse((self.package / relative).exists())
        self.set_binary(b"new upstream binary fixture")
        installer.install(self.package, self.prefix)
        self.assertEqual((self.prefix / "bin/rustterm").read_bytes(), b"new upstream binary fixture")
        self.assertEqual(legacy.read_text(), "Historical patch notice")
        self.assertTrue((self.prefix / "share/doc/rustterm/alacritty_terminal-LICENSE-APACHE").is_file())
        installer.install(self.package, self.prefix, uninstall=True)
        self.assertEqual(legacy.read_text(), "Historical patch notice")

    def test_previous_installer_transaction_recovers_retired_patch_notice(self):
        installer.install(self.package, self.prefix)
        previous = self.snapshot()
        relative = "share/doc/rustterm/THIRD_PARTY_PATCHES.md"
        legacy = self.prefix / relative
        legacy.write_bytes(b"previous patch notice")
        identifier = str(uuid.uuid4())
        items = []
        for name in [*installer.FILES, relative]:
            item = {"relative": name, "existed": True}
            target, staged, backup = installer.transaction_paths(self.prefix, item, identifier)
            os.link(target, backup)
            staged.write_bytes(b"interrupted replacement")
            items.append(item)
        installer.write_journal(self.prefix, {"id": identifier, "state": "prepared", "files": items})
        for item in items:
            target, staged, _ = installer.transaction_paths(self.prefix, item, identifier)
            os.replace(staged, target)
        installer.recover(self.prefix)
        self.assertEqual(self.snapshot(), previous)
        self.assertEqual(legacy.read_bytes(), b"previous patch notice")
        self.assertFalse((self.prefix / installer.JOURNAL).exists())


if __name__ == "__main__":
    unittest.main()
