"""Offline packaging and publication checks; never access GitHub or run an app."""
import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import artifacts

spec = importlib.util.spec_from_file_location('draft_release', Path(__file__).with_name('draft.py'))
draft = importlib.util.module_from_spec(spec)
spec.loader.exec_module(draft)

VERSION = '1.2.3'
COMMIT = 'a' * 40


def fixture(directory, platform, *, additions=None, manifest_changes=None):
    windows = platform.startswith('windows')
    name = f'rustterm-{VERSION}-{platform}'
    binary = b'test executable bytes'
    manifest = {'version': VERSION, 'platform': platform, 'variant': 'release',
                'binary_sha256': hashlib.sha256(binary).hexdigest()}
    manifest.update(manifest_changes or {})
    files = {
        'build-info.json': json.dumps(manifest).encode(),
        ('rustterm.exe' if windows else 'bin/rustterm'): binary,
    }
    if windows:
        files.update({path: b'fixture' for path in ['README.md', 'LICENSE', 'alacritty_terminal-LICENSE-APACHE']})
    else:
        files.update({path: b'fixture' for path in [
            'install.sh', 'install.py', 'share/applications/io.rustterm.RustTerm.desktop',
            'share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg',
            'share/doc/rustterm/README.md', 'share/doc/rustterm/LICENSE',
            'share/doc/rustterm/alacritty_terminal-LICENSE-APACHE',
        ]})
    files.update(additions or {})
    archive = directory / (name + ('.zip' if windows else '.tar.gz'))
    if windows:
        with zipfile.ZipFile(archive, 'w') as package:
            for path, data in files.items():
                package.writestr(f'{name}/{path}', data)
    else:
        with tarfile.open(archive, 'w:gz') as package:
            for path, data in files.items():
                entry = tarfile.TarInfo(f'{name}/{path}')
                entry.size = len(data)
                package.addfile(entry, io.BytesIO(data))
    checksum(archive)
    return archive


def checksum(archive):
    archive.with_name(archive.name + '.sha256').write_text(f'{artifacts.digest(archive)}  {archive.name}\n', encoding='ascii')


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)

    def pair(self):
        for platform in artifacts.PLATFORMS:
            fixture(self.directory, platform)

    def test_tag_must_match_manifest_and_full_toolchain(self):
        package = {'version': VERSION, 'rust-version': '1.99.1'}
        self.assertEqual(artifacts.version_info(VERSION, package)['prerelease'], 'false')
        for tag in ['v1.2.3', '1.2.4', '01.2.3', '1.2.3\n']:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                artifacts.version_info(tag, package)
        with self.assertRaises(ValueError):
            artifacts.version_info(VERSION, dict(package, **{'rust-version': '1.99'}))

    def test_ignored_files_cannot_be_published_from_git(self):
        with patch.object(artifacts, 'version_info', return_value={'version': VERSION}), \
             patch.object(artifacts.subprocess, 'check_output', return_value=b'ignored-local-file\n') as git:
            with self.assertRaisesRegex(ValueError, 'still tracked'):
                artifacts.metadata(VERSION)
            git.assert_called_once()

    def test_release_flags_handle_spaces_and_strip_build_paths(self):
        checkout = self.directory / 'checkout with spaces'
        checkout.mkdir()
        with patch.object(artifacts, 'ROOT', checkout), patch.dict(os.environ, {'CARGO_ENCODED_RUSTFLAGS': '-C\x1fopt-level=2'}):
            environment = artifacts.release_environment('x86_64-pc-windows-msvc')
            flags = environment['CARGO_ENCODED_RUSTFLAGS'].split('\x1f')
            self.assertIn(f'--remap-path-prefix={checkout.resolve()}=/rustterm', flags)
            self.assertIn('opt-level=2', flags)
            self.assertIn('strip=symbols', flags)
            self.assertIn('link-arg=/PDBALTPATH:rustterm.pdb', flags)
            self.assertNotIn('RUSTFLAGS', environment)

    def test_embedded_build_paths_are_rejected_in_utf8_and_utf16(self):
        for encoding in ['utf-8', 'utf-16-le']:
            with self.subTest(encoding=encoding), self.assertRaisesRegex(ValueError, 'build-host path'):
                artifacts.verify_binary_privacy(io.BytesIO(str(artifacts.ROOT).encode(encoding)))
        artifacts.verify_binary_privacy(io.BytesIO(b'/rustterm/src/main.rs\0/cargo/registry/src/library.rs'))

    def test_path_split_across_scan_chunks_is_rejected(self):
        path = str(artifacts.ROOT).encode()
        data = b'\0' * (1024 * 1024 - len(path) // 2) + path
        with self.assertRaisesRegex(ValueError, 'build-host path'):
            artifacts.verify_binary_privacy(io.BytesIO(data))

    @unittest.skipUnless(shutil.which('rustc'), 'Rust compiler is unavailable')
    def test_compiler_removes_source_path_from_file_macro(self):
        checkout = self.directory / 'checkout with spaces'
        checkout.mkdir()
        source = checkout / 'main.rs'
        source.write_text('fn main() { std::hint::black_box(file!()); }', encoding='utf-8')
        binary = checkout / ('sample.exe' if os.name == 'nt' else 'sample')
        target = 'x86_64-pc-windows-msvc' if os.name == 'nt' else None
        with patch.object(artifacts, 'ROOT', checkout):
            flags = artifacts.release_environment(target)['CARGO_ENCODED_RUSTFLAGS'].split('\x1f')
            subprocess.run(['rustc', str(source), '-o', str(binary), *flags], check=True, capture_output=True)
            with binary.open('rb') as built:
                artifacts.verify_binary_privacy(built)
        self.assertIn(b'/rustterm/main.rs', binary.read_bytes())

    def test_prerelease_and_build_metadata(self):
        for version, expected in [('1.2.3-rc.1', 'true'), ('1.2.3+build.2', 'false')]:
            package = {'version': version, 'rust-version': '1.99.1'}
            self.assertEqual(artifacts.version_info(version, package)['prerelease'], expected)
        with self.assertRaises(ValueError):
            artifacts.version_info('1.2.3-01', {'version': '1.2.3-01', 'rust-version': '1.99.1'})

    def test_complete_release_generates_checksums(self):
        self.pair()
        artifacts.verify_release(self.directory, VERSION)
        lines = (self.directory / 'SHA256SUMS').read_text().splitlines()
        self.assertEqual(len(lines), 4)
        for line in lines:
            expected, name = line.split()
            self.assertEqual(expected, artifacts.digest(self.directory / name))

    def test_missing_platform_cannot_publish(self):
        fixture(self.directory, 'linux-x86_64')
        with self.assertRaises(FileNotFoundError):
            artifacts.verify_release(self.directory, VERSION)
        self.assertFalse((self.directory / 'SHA256SUMS').exists())

    def test_corrupt_archive_is_rejected(self):
        archive = fixture(self.directory, 'windows-x86_64')
        with archive.open('ab') as file:
            file.write(b'corruption')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            artifacts.verify_archive(archive, VERSION, 'windows-x86_64')

    def test_wrong_binary_or_manifest_is_rejected(self):
        for platform in artifacts.PLATFORMS:
            for changes in [{'version': '1.2.4'}, {'variant': 'dev'}, {'platform': 'other'}, {'binary_sha256': '0'*64}]:
                with self.subTest(platform=platform, changes=changes), self.assertRaises(ValueError):
                    archive = fixture(self.directory, platform, manifest_changes=changes)
                    artifacts.verify_archive(archive, VERSION, platform)

    def test_unexpected_files_and_unsafe_paths_are_rejected(self):
        for platform in artifacts.PLATFORMS:
            for path in ['extra/note.txt', '../outside', 'a\\b']:
                with self.subTest(platform=platform, path=path), self.assertRaises(ValueError):
                    archive = fixture(self.directory, platform, additions={path: b'private'})
                    artifacts.verify_archive(archive, VERSION, platform)

    def test_unexpected_release_asset_is_rejected(self):
        self.pair()
        (self.directory / 'private.txt').write_text('private')
        with self.assertRaises(ValueError):
            artifacts.verify_release(self.directory, VERSION)

    def test_windows_packager_builds_portable_zip_without_running_executable(self):
        # A PE header is sufficient: packaging must not gate publication on GUI runtime checks.
        binary = self.directory / 'rustterm.exe'
        header = bytearray(128)
        header[:2] = b'MZ'
        struct.pack_into('<I', header, 0x3c, 64)
        header[64:70] = b'PE\0\0\x64\x86'
        binary.write_bytes(header)
        options = argparse.Namespace(release=True, binary=None, output=self.directory / 'dist')
        with patch.object(artifacts.platform, 'system', return_value='Windows'), \
             patch.object(artifacts.platform, 'machine', return_value='AMD64'), \
             patch.dict(os.environ, {'CARGO_TARGET_DIR': str(self.directory / 'target')}), \
             patch.object(artifacts.subprocess, 'run') as build:
            destination = self.directory / 'target/x86_64-pc-windows-msvc/release/rustterm.exe'
            destination.parent.mkdir(parents=True)
            destination.write_bytes(binary.read_bytes())
            artifacts.package_windows(options)
            build.assert_called_once()
            self.assertIn('--locked', build.call_args.args[0])
            self.assertIn('+crt-static', build.call_args.kwargs['env']['CARGO_ENCODED_RUSTFLAGS'])
        version = artifacts.package_info()['version']
        archive = options.output / f'rustterm-{version}-windows-x86_64.zip'
        artifacts.verify_archive(archive, version, 'windows-x86_64')

    def test_uploaded_digest_and_asset_set_are_verified(self):
        self.pair()
        artifacts.verify_release(self.directory, VERSION)
        assets = [{'name': p.name, 'size': p.stat().st_size, 'digest': 'sha256:'+artifacts.digest(p)}
                  for p in self.directory.iterdir()]
        draft.verify_uploaded(assets, self.directory)
        with self.assertRaises(ValueError):
            draft.verify_uploaded(assets[:-1], self.directory)
        assets[0]['digest'] = 'sha256:' + '0'*64
        with self.assertRaises(ValueError):
            draft.verify_uploaded(assets, self.directory)

    def test_tag_moved_after_build_is_rejected(self):
        with patch.object(draft, 'gh', return_value=subprocess.CompletedProcess([], 0, 'b'*40)):
            with self.assertRaisesRegex(ValueError, 'tag changed'):
                draft.verify_tag('owner/repo', VERSION, COMMIT)

    def test_published_or_foreign_draft_is_never_modified(self):
        environment = {'RELEASE_TAG': VERSION, 'RELEASE_COMMIT': COMMIT, 'GH_REPO': 'owner/repo'}
        for existing in [{'draft': False, 'body': ''}, {'draft': True, 'body': 'someone else'}]:
            with self.subTest(existing=existing), patch.dict(os.environ, environment), \
                 patch.object(draft, 'version_info', return_value={'version': VERSION}), \
                 patch.object(draft, 'verify_release'), patch.object(draft, 'verify_tag'), \
                 patch.object(draft, 'release', return_value=existing), patch.object(draft, 'gh') as gh:
                with self.assertRaisesRegex(ValueError, 'Refusing to overwrite'):
                    draft.main()
                gh.assert_not_called()

    def test_successful_upload_stays_a_draft_for_manual_publication(self):
        directory = self.directory / 'dist'
        directory.mkdir()
        for platform in artifacts.PLATFORMS:
            fixture(directory, platform)
        artifacts.verify_release(directory, VERSION)
        owned = {
            'draft': True,
            'body': f'<!-- rustterm-release-commit:{COMMIT} -->',
            'assets': [{'name': file.name, 'size': file.stat().st_size, 'digest': 'sha256:'+artifacts.digest(file)}
                       for file in directory.iterdir()],
        }
        environment = {'RELEASE_TAG': VERSION, 'RELEASE_COMMIT': COMMIT, 'GH_REPO': 'owner/repo'}
        info = {'version': VERSION, 'prerelease': 'false'}

        def github(*args, **kwargs):
            # Publishing, editing a release, or changing Latest is never allowed.
            self.assertIn(args[:2], [('release', 'create'), ('release', 'upload')])
            self.assertEqual(args[2], VERSION)
            if args[1] == 'create':
                self.assertIn('--draft', args)
                self.assertIn('--prerelease=false', args)
            return subprocess.CompletedProcess([], 0, '')

        for resume in [False, True]:
            with self.subTest(resume=resume), contextlib.chdir(self.directory), \
                 patch.dict(os.environ, environment), \
                 patch.object(draft, 'version_info', return_value=info), \
                 patch.object(draft, 'verify_tag') as verify_tag, \
                 patch.object(draft, 'release', side_effect=[owned if resume else None, owned]), \
                 patch.object(draft, 'gh', side_effect=github) as gh:
                draft.main()
                self.assertEqual([call.args[1] for call in gh.call_args_list],
                                 ['upload'] if resume else ['create', 'upload'])
                self.assertEqual(verify_tag.call_count, 2)


if __name__ == '__main__':
    unittest.main()
