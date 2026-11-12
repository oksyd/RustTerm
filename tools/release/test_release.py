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


def deb_fixture(directory):
    if not shutil.which('dpkg-deb'):
        raise unittest.SkipTest('Debian package integration checks run on Ubuntu')
    name = f'rustterm-{VERSION}-linux-x86_64'
    with tempfile.TemporaryDirectory() as temporary:
        source = Path(temporary)
        with tarfile.open(directory / (name + '.tar.gz'), 'r:gz') as package:
            for relative in artifacts.LINUX_FILES | {'build-info.json'}:
                path = source / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                with package.extractfile(name + '/' + relative) as data:
                    path.write_bytes(data.read())
        # Fixture executables are deliberately not runnable. The actual Linux
        # packaging smoke test exercises dpkg-shlibdeps against the built ELF.
        with patch.object(artifacts.subprocess, 'check_output', return_value='shlibs:Depends=libc6 (>= 2.39)\n'):
            artifacts.package_deb(source, directory)
    return directory / (name + '.deb')


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)

    def pair(self):
        for platform in artifacts.PLATFORMS:
            fixture(self.directory, platform)
        deb_fixture(self.directory)

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

    def test_msvc_upstream_paths_are_not_mistaken_for_our_checkout(self):
        upstream = r'D:\a\_work\1\s\src\vctools\crt\vcstartup\src\startup\exe_common.inl'
        for path in (upstream, upstream.replace('\\', '/')):
            for encoding in ('utf-8', 'utf-16-le'):
                with self.subTest(path=path, encoding=encoding):
                    artifacts.verify_binary_privacy(io.BytesIO(path.encode(encoding)))

    def test_foreign_windows_user_and_checkout_paths_are_rejected(self):
        for path in (r'C:\Users\Example User\source.rs', r'C:\Users\测试\source.rs',
                     r'D:\a\ExampleProject\ExampleProject\source.rs'):
            for spelling in (path, path.replace('\\', '/')):
                for encoding in ('utf-8', 'utf-16-le'):
                    for prefix in (b'', b'\x7f'):
                        with self.subTest(path=spelling, encoding=encoding, prefix=prefix), \
                             self.assertRaisesRegex(ValueError, 'build-host path'):
                            artifacts.verify_binary_privacy(io.BytesIO(prefix + spelling.encode(encoding)))

    def test_unrelated_binary_strings_are_not_joined_into_paths(self):
        for data in (b'C:\\Us\x00ers\\example\\file.rs',
                     'C:\\Us\x00ers\\example\\file.rs'.encode('utf-16-le')):
            artifacts.verify_binary_privacy(io.BytesIO(data))

    def test_configured_windows_paths_are_always_rejected(self):
        # A known build root must win even when it resembles a supplier path.
        root = r'D:\a\_work\1\s'
        with patch.object(artifacts, 'build_paths', return_value=[(root, '/rustterm')]):
            for encoding in ('utf-8', 'utf-16-le'):
                with self.subTest(encoding=encoding), self.assertRaisesRegex(ValueError, 'configured build directory') as error:
                    artifacts.verify_binary_privacy(io.BytesIO((root + r'\source.rs').encode(encoding)))
                self.assertNotIn(root, str(error.exception))

    def test_utf16_windows_path_crossing_scan_boundary_is_rejected(self):
        path = r'D:\a\ExampleProject\ExampleProject\source.rs'.encode('utf-16-le')
        data = b'\x00' * (1024 * 1024 - 13) + path
        with self.assertRaisesRegex(ValueError, 'GitHub workspace, UTF-16LE'):
            artifacts.verify_binary_privacy(io.BytesIO(data))

    def test_compiler_mapping_retains_actual_github_workspace(self):
        workspace = self.directory / 'workspace'
        checkout = self.directory / 'test source'
        with patch.object(artifacts, 'ROOT', checkout), patch.dict(os.environ, {'GITHUB_WORKSPACE': str(workspace)}):
            paths = artifacts.build_paths()
            self.assertIn((str(workspace.resolve()), '/workspace'), paths)
            self.assertIn((str(checkout.resolve()), '/rustterm'), paths)

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
        self.assertIn(b'/rustterm/main.rs', binary.read_bytes().replace(b'\\', b'/'))

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
        self.assertEqual(len(lines), 6)
        for line in lines:
            expected, name = line.split()
            self.assertEqual(expected, artifacts.digest(self.directory / name))

    def test_missing_platform_cannot_publish(self):
        fixture(self.directory, 'linux-x86_64')
        with self.assertRaises(FileNotFoundError):
            artifacts.verify_release(self.directory, VERSION)
        self.assertFalse((self.directory / 'SHA256SUMS').exists())

    def test_missing_debian_package_cannot_publish(self):
        for platform in artifacts.PLATFORMS:
            fixture(self.directory, platform)
        with self.assertRaises(FileNotFoundError):
            artifacts.verify_release(self.directory, VERSION)
        self.assertFalse((self.directory / 'SHA256SUMS').exists())

    def test_debian_prerelease_sorts_before_stable(self):
        self.assertEqual(artifacts.debian_version('1.2.3-rc.1+build.2'), '1.2.3~rc.1+build.2')
        if shutil.which('dpkg'):
            subprocess.run(['dpkg', '--compare-versions', artifacts.debian_version('1.2.3-rc.1'), 'lt', '1.2.3'], check=True)

    def test_debian_package_metadata_and_layout(self):
        fixture(self.directory, 'linux-x86_64')
        archive = deb_fixture(self.directory)
        manifest = artifacts.verify_deb(archive, VERSION)
        self.assertEqual(manifest['installation'], 'deb')
        dependencies = subprocess.check_output(['dpkg-deb', '-f', str(archive), 'Depends'], text=True)
        for dependency in ['libc6 (>= 2.39)', 'libvulkan1', 'libwayland-client0', 'libpulse0', 'fontconfig', 'pkexec', 'zenity']:
            self.assertIn(dependency, dependencies)
        listing = subprocess.check_output(['dpkg-deb', '-c', str(archive)], text=True)
        self.assertIn('./usr/bin/rustterm', listing)
        self.assertIn('./usr/share/applications/io.rustterm.RustTerm.desktop', listing)
        self.assertNotIn('install.sh', listing)

    def test_debian_package_rejects_wrong_metadata_private_files_and_scripts(self):
        fixture(self.directory, 'linux-x86_64')
        archive = deb_fixture(self.directory)
        for change in ['architecture', 'manifest', 'private', 'postinst', 'permissions', 'binary']:
            with self.subTest(change=change), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / 'root'
                subprocess.run(['dpkg-deb', '--raw-extract', str(archive), str(root)], check=True)
                if change == 'architecture':
                    path = root / 'DEBIAN/control'
                    path.write_text(path.read_text().replace('Architecture: amd64', 'Architecture: arm64'))
                elif change == 'manifest':
                    path = root / 'usr/share/doc/rustterm/build-info.json'
                    manifest = json.loads(path.read_text()); manifest['variant'] = 'dev'
                    path.write_text(json.dumps(manifest))
                elif change == 'private':
                    (root / 'usr/share/doc/rustterm/private.txt').write_text('private')
                elif change == 'postinst':
                    path = root / 'DEBIAN/postinst'
                    path.write_text('#!/bin/sh\nexit 0\n'); path.chmod(0o755)
                elif change == 'permissions':
                    (root / 'usr/bin/rustterm').chmod(0o644)
                else:
                    (root / 'usr/bin/rustterm').write_bytes(b'wrong binary')
                changed = Path(temporary) / archive.name
                subprocess.run(['dpkg-deb', '--root-owner-group', '--build', str(root), str(changed)],
                               check=True, stdout=subprocess.DEVNULL)
                checksum(changed)
                with self.assertRaises(ValueError):
                    artifacts.verify_deb(changed, VERSION)

    def test_linux_packages_must_contain_the_same_executable(self):
        self.pair()
        # Replace just the tarball with another internally consistent executable.
        binary = b'another build'
        fixture(self.directory, 'linux-x86_64', additions={'bin/rustterm': binary},
                manifest_changes={'binary_sha256': hashlib.sha256(binary).hexdigest()})
        with self.assertRaisesRegex(ValueError, 'different executables'):
            artifacts.verify_release(self.directory, VERSION)

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

    def test_draft_lookup_uses_paginated_release_list_not_published_tag_endpoint(self):
        owned = {'id': 42, 'tag_name': VERSION, 'draft': True, 'body': 'draft notes'}

        def github(*args, **kwargs):
            if '/releases/tags/' in args[1]:
                return subprocess.CompletedProcess([], 1, '', 'gh: Not Found (HTTP 404)')
            self.assertEqual(args, ('api', 'repos/owner/repo/releases?per_page=100', '--paginate', '--slurp'))
            return subprocess.CompletedProcess([], 0, json.dumps([
                [{'id': 41, 'tag_name': '1.2.4', 'draft': False}], [owned],
            ]))

        with patch.object(draft, 'gh', side_effect=github):
            self.assertEqual(draft.release('owner/repo', VERSION), owned)

    def test_draft_lookup_rejects_ambiguous_matches_and_api_errors(self):
        duplicate = {'id': 42, 'tag_name': VERSION, 'draft': True}
        with patch.object(draft, 'gh', return_value=subprocess.CompletedProcess([], 0, json.dumps([[duplicate], [duplicate]]))):
            with self.assertRaisesRegex(ValueError, 'Multiple releases'):
                draft.release('owner/repo', VERSION)
        with patch.object(draft, 'gh', side_effect=RuntimeError('GitHub CLI failed: HTTP 403')):
            with self.assertRaisesRegex(RuntimeError, 'HTTP 403'):
                draft.release('owner/repo', VERSION)

    def test_draft_id_lookup_does_not_treat_permission_errors_as_missing(self):
        for status in [403, 404, 500]:
            with self.subTest(status=status), patch.object(draft, 'gh', return_value=
                    subprocess.CompletedProcess([], 1, '', f'gh: API error (HTTP {status})')) as github:
                if status == 404:
                    self.assertIsNone(draft.release_by_id('owner/repo', 42))
                else:
                    with self.assertRaises(RuntimeError):
                        draft.release_by_id('owner/repo', 42)
                github.assert_called_once_with('api', 'repos/owner/repo/releases/42', check=False)

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
        deb_fixture(directory)
        artifacts.verify_release(directory, VERSION)
        owned = {
            'id': 42,
            'tag_name': VERSION,
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
                 patch.object(draft, 'release_by_id', return_value=owned) as by_id, \
                 patch.object(draft, 'gh', side_effect=github) as gh:
                draft.main()
                self.assertEqual([call.args[1] for call in gh.call_args_list],
                                 ['upload'] if resume else ['create', 'upload'])
                self.assertEqual(verify_tag.call_count, 2)
                by_id.assert_called_once_with('owner/repo', 42)

    def test_create_and_resume_drafts_when_tag_endpoint_cannot_see_them(self):
        directory = self.directory / 'dist'
        directory.mkdir()
        (directory / 'asset.zip').write_bytes(b'fixture')
        assets = [{'name': 'asset.zip', 'size': 7, 'digest': 'sha256:' + artifacts.digest(directory / 'asset.zip')}]
        environment = {'RELEASE_TAG': VERSION, 'RELEASE_COMMIT': COMMIT, 'GH_REPO': 'owner/repo'}
        marker = f'<!-- rustterm-release-commit:{COMMIT} -->'
        owned = {'id': 42, 'tag_name': VERSION, 'draft': True, 'body': marker, 'assets': assets}

        for resume in [False, True]:
            created = resume
            list_reads, id_reads, creates, uploads = 0, 0, 0, 0

            def github(*args, **kwargs):
                nonlocal created, list_reads, id_reads, creates, uploads
                result = lambda value: subprocess.CompletedProcess([], 0, json.dumps(value))
                if args[:2] == ('api', 'repos/owner/repo/releases?per_page=100'):
                    list_reads += 1
                    # A new draft takes one list refresh to become visible.
                    return result([[owned] if created and (resume or list_reads > 2) else []])
                if args[:2] == ('api', 'repos/owner/repo/releases/42'):
                    id_reads += 1
                    if id_reads == 1:
                        return subprocess.CompletedProcess([], 1, '', 'gh: Not Found (HTTP 404)')
                    return result(dict(owned, assets=[] if id_reads == 2 else assets))
                if args[:2] == ('release', 'create'):
                    self.assertIn('--draft', args)
                    self.assertIn(marker, Path(args[args.index('--notes-file') + 1]).read_text())
                    created = True
                    creates += 1
                    return result(None)
                if args[:2] == ('release', 'upload'):
                    self.assertTrue(created)
                    uploads += 1
                    return result(None)
                self.fail(f'Unexpected GitHub operation: {args}')

            with self.subTest(resume=resume), contextlib.chdir(self.directory), \
                 patch.dict(os.environ, environment), patch.object(draft, 'verify_release'), \
                 patch.object(draft, 'version_info', return_value={'version': VERSION, 'prerelease': 'false'}), \
                 patch.object(draft, 'verify_tag'), patch.object(draft.time, 'sleep') as sleep, \
                 patch.object(draft, 'gh', side_effect=github):
                draft.main()
                self.assertEqual(creates, 0 if resume else 1)
                self.assertEqual(uploads, 1)
                self.assertEqual(id_reads, 3)
                self.assertEqual(sleep.call_count, 2 if resume else 3)

    def test_changed_draft_is_rejected_after_upload_without_retrying_mutations(self):
        (self.directory / 'dist').mkdir()
        owned = {'id': 42, 'tag_name': VERSION, 'draft': True,
                 'body': f'<!-- rustterm-release-commit:{COMMIT} -->', 'assets': []}
        environment = {'RELEASE_TAG': VERSION, 'RELEASE_COMMIT': COMMIT, 'GH_REPO': 'owner/repo'}
        for change in [{'draft': False}, {'body': 'other build'}, {'tag_name': '1.2.4'}, {'id': 43}]:
            with self.subTest(change=change), contextlib.chdir(self.directory), \
                 patch.dict(os.environ, environment), patch.object(draft, 'verify_release'), \
                 patch.object(draft, 'version_info', return_value={'version': VERSION}), \
                 patch.object(draft, 'verify_tag'), patch.object(draft, 'release', return_value=owned), \
                 patch.object(draft, 'release_by_id', return_value=dict(owned, **change)), \
                 patch.object(draft.time, 'sleep') as sleep, patch.object(draft, 'gh') as github:
                with self.assertRaises(ValueError):
                    draft.main()
                self.assertEqual([call.args[:2] for call in github.call_args_list], [('release', 'upload')])
                sleep.assert_not_called()

    def test_invisible_new_draft_is_not_recreated_or_uploaded(self):
        environment = {'RELEASE_TAG': VERSION, 'RELEASE_COMMIT': COMMIT, 'GH_REPO': 'owner/repo'}
        with patch.dict(os.environ, environment), patch.object(draft, 'verify_release'), \
             patch.object(draft, 'version_info', return_value={'version': VERSION, 'prerelease': 'false'}), \
             patch.object(draft, 'verify_tag'), patch.object(draft, 'release', return_value=None) as lookup, \
             patch.object(draft.time, 'sleep') as sleep, patch.object(draft, 'gh') as github:
            with self.assertRaisesRegex(ValueError, 'Created draft is not visible'):
                draft.main()
            self.assertEqual([call.args[:2] for call in github.call_args_list], [('release', 'create')])
            self.assertEqual(lookup.call_count, 6)
            self.assertEqual(sleep.call_count, 4)


if __name__ == '__main__':
    unittest.main()
