#!/usr/bin/env python3
"""Build and verify desktop release assets."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import shlex
import struct
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[2]
PLATFORMS = {"linux-x86_64": "tar.gz", "windows-x86_64": "zip"}
LINUX_FILES = {
    'bin/rustterm', 'share/applications/io.rustterm.RustTerm.desktop',
    'share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg',
    'share/doc/rustterm/README.md', 'share/doc/rustterm/LICENSE',
    'share/doc/rustterm/alacritty_terminal-LICENSE-APACHE',
}
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z")
USER_DIRECTORY = re.compile(rb'/(?:home|Users)/[^/\x00\r\n]+/|[a-z]:[/\\]users[/\\][^/\\\x00\r\n]+[/\\]', re.I)
# Hosted GitHub checkouts repeat the repository name. A generic drive:/a/
# prefix also matches upstream MSVC CRT source paths, which are not host data.
HOSTED_WORKSPACE = re.compile(rb'[a-z]:[/\\]a[/\\]([^/\\\s\x00]+)[/\\]\1(?:[/\\]|$)', re.I)
PATH_PATTERNS = [('user directory', USER_DIRECTORY), ('GitHub workspace', HOSTED_WORKSPACE)]
WIDE_PATH_PATTERNS = [(kind, re.compile(pattern.pattern.decode('ascii'), re.I)) for kind, pattern in PATH_PATTERNS]


def package_info():
    return tomllib.loads((ROOT / 'Cargo.toml').read_text(encoding='utf-8'))['package']


def digest(path):
    with Path(path).open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def build_paths():
    """Host paths to replace in compiler diagnostics and distributed binaries."""
    home = Path.home()
    paths = [
        (home, '/build-home'),
        (Path(os.environ.get('CARGO_HOME', home / '.cargo')), '/cargo'),
        (Path(os.environ.get('RUSTUP_HOME', home / '.rustup')), '/rust'),
        (ROOT, '/rustterm'),
    ]
    if directory := os.environ.get('CARGO_TARGET_DIR'):
        paths.append((ROOT / directory, '/target'))
    if workspace := os.environ.get('GITHUB_WORKSPACE'):
        # Compiler tests use a temporary source root but may still run in the
        # checkout. Account for both locations, including junction spellings.
        if Path(workspace).resolve() != ROOT.resolve():
            paths.append((Path(workspace), '/workspace'))
    return sorted({(str(spelling), replacement) for path, replacement in paths
                   for spelling in (path.absolute(), path.resolve())}, key=lambda item: len(item[0]))


def release_environment(target=None):
    environment = dict(os.environ)
    # Cargo's encoded form also handles checkout paths containing spaces.
    encoded = environment.get('CARGO_ENCODED_RUSTFLAGS')
    flags = encoded.split('\x1f') if encoded else environment.get('RUSTFLAGS', '').split()
    for path, replacement in build_paths():
        for spelling in sorted({path, path.replace('\\', '/')}):
            flags.append(f'--remap-path-prefix={spelling}={replacement}')
    flags.extend(['-C', 'debuginfo=0', '-C', 'strip=symbols'])
    if target and target.endswith('-msvc'):
        flags.extend(['-C', 'target-feature=+crt-static', '-C', 'link-arg=/PDBALTPATH:rustterm.pdb'])
    else:
        # Native codecs can emit __FILE__ independently of rustc.
        native_flags = shlex.join([f'-ffile-prefix-map={path}={replacement}' for path, replacement in build_paths()])
        environment['CC_SHELL_ESCAPED_FLAGS'] = '1'
        for key in ('CFLAGS', 'CXXFLAGS'):
            environment[key] = environment.get(key, '') + ' ' + native_flags
    environment.pop('RUSTFLAGS', None)
    environment['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(flags)
    environment['CARGO_PROFILE_RELEASE_DEBUG'] = '0'
    environment['CARGO_PROFILE_RELEASE_STRIP'] = 'symbols'
    return environment


def build_release(target=None):
    command = ['cargo', 'build', '--release', '--locked', '--bin', 'rustterm']
    if target:
        command.extend(['--target', target])
    subprocess.run(command, cwd=ROOT, env=release_environment(target), check=True)


def verify_binary_privacy(source):
    """Reject build-host paths, including wide-character PE debug records."""
    needles = set()
    for path, _ in build_paths():
        # A filesystem root is not an identifying path.
        if path in {'/', '\\'} or re.fullmatch(r'[a-z]:[/\\]', path, re.I):
            continue
        for spelling in {path, path.replace('\\', '/')}:
            needles.update(spelling.lower().encode(encoding) for encoding in ('utf-8', 'utf-16-le'))
    overlap = max(1024, max(map(len, needles), default=0))
    tail = b''
    position = 0
    while chunk := source.read(1024 * 1024):
        data = tail + chunk
        lower = data.lower()
        def reject(kind, offset):
            # Give useful CI diagnostics without printing the identifying path.
            location = position - len(tail) + offset
            raise ValueError(f'Executable contains a build-host path ({kind}, byte 0x{location:x}); rebuild with the release packaging tools')

        for needle in needles:
            if (offset := lower.find(needle)) >= 0:
                reject('configured build directory', offset)
        # The draft job may run on a different host than the build job.
        for kind, pattern in PATH_PATTERNS:
            if match := pattern.search(data):
                reject(kind, match.start())
        # Decode both possible UTF-16LE alignments, preserving NUL separators
        # and non-ASCII user names. Stripping NULs joins unrelated strings.
        for alignment in (0, 1):
            end = len(data) - (len(data) - alignment) % 2
            wide = data[alignment:end].decode('utf-16-le', errors='replace')
            for kind, pattern in WIDE_PATH_PATTERNS:
                if match := pattern.search(wide):
                    offset = alignment + len(wide[:match.start()].encode('utf-16-le'))
                    reject(kind + ', UTF-16LE', offset)
        tail = data[-overlap:]
        position += len(chunk)


def version_info(tag, manifest=None):
    package = manifest or package_info()
    version = tag
    match = VERSION.fullmatch(version)
    if not match or version != package['version']:
        raise ValueError('Release tag must exactly match Cargo.toml package.version, without a v prefix')
    if match[4] and any(part.isdigit() and len(part) > 1 and part.startswith('0') for part in match[4].split('.')):
        raise ValueError('Numeric prerelease identifiers cannot have leading zeros')
    rust = package['rust-version']
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', rust):
        raise ValueError('rust-version must specify a complete toolchain version')
    return {'version': version, 'tag': tag, 'rust': rust, 'prerelease': str(bool(match[4])).lower()}


def metadata(tag):
    result = version_info(tag)
    ignored = subprocess.check_output(['git', 'ls-files', '--cached', '--ignored', '--exclude-standard'], cwd=ROOT)
    if ignored.strip():
        raise ValueError('Ignored local files are still tracked by Git; remove them from the index before releasing (keep local copies)')
    commit = subprocess.check_output(['git', 'rev-parse', '--verify', 'HEAD'], cwd=ROOT, text=True).strip()
    tagged = subprocess.check_output(['git', 'rev-parse', '--verify', f'refs/tags/{tag}^{{commit}}'], cwd=ROOT, text=True).strip()
    if tagged != commit:
        raise ValueError('The checked-out commit does not match the release tag')
    result['commit'] = commit
    if output := os.environ.get('GITHUB_OUTPUT'):
        with open(output, 'a', encoding='utf-8') as target:
            target.writelines(f'{key}={value}\n' for key, value in result.items())
    print(json.dumps(result))


def verify_executable(binary, version):
    result = subprocess.run([str(Path(binary).resolve()), '--version'], capture_output=True, text=True, timeout=30, check=True)
    if result.stdout.strip() != f'RustTerm {version}':
        raise ValueError('Executable --version does not match Cargo.toml')


def package_windows(options):
    if platform.system() != 'Windows' or platform.machine().lower() not in {'amd64', 'x86_64'}:
        raise ValueError('Build Windows packages on an x86_64 Windows host')
    version = package_info()['version']
    target = 'x86_64-pc-windows-msvc'
    variant = 'release' if options.release else 'dev'
    if options.release:
        build_release(target)
        binary = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')) / target / 'release/rustterm.exe'
    else:
        binary = Path(options.binary).resolve()
    with binary.open('rb') as source:
        if source.read(2) != b'MZ': raise ValueError('Expected a Windows executable')
        source.seek(0x3c)
        source.seek(struct.unpack('<I', source.read(4))[0])
        if source.read(6) != b'PE\0\0\x64\x86': raise ValueError('Expected an x86_64 PE executable')
        source.seek(0)
        verify_binary_privacy(source)
    if variant == 'dev': version += '-dev'
    name = f'rustterm-{version}-windows-x86_64'
    output = Path(options.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.rustterm-package-', dir=output) as temporary:
        staging = Path(temporary)
        root = staging / name
        root.mkdir()
        shutil.copyfile(binary, root / 'rustterm.exe')
        for name_in_repo, name_in_archive in [
            ('LICENSE', 'LICENSE'), ('README.md', 'README.md'),
            ('assets/licenses/alacritty_terminal-LICENSE-APACHE', 'alacritty_terminal-LICENSE-APACHE'),
        ]:
            shutil.copyfile(ROOT / name_in_repo, root / name_in_archive)
        manifest = {'version': version, 'platform': 'windows-x86_64', 'target': target,
                    'variant': variant, 'binary_sha256': digest(binary), 'lock_sha256': digest(ROOT / 'Cargo.lock')}
        (root / 'build-info.json').write_text(json.dumps(manifest, indent=2)+'\n', encoding='utf-8')
        archive = staging / f'{name}.zip'
        with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as package:
            for file in sorted(root.iterdir()):
                info = zipfile.ZipInfo(f'{name}/{file.name}', date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o100644 << 16
                with file.open('rb') as source, package.open(info, 'w', force_zip64=True) as target_file:
                    shutil.copyfileobj(source, target_file)
        checksum = staging / f'{archive.name}.sha256'
        checksum.write_text(f'{digest(archive)}  {archive.name}\n', encoding='ascii')
        os.replace(archive, output / archive.name)
        os.replace(checksum, output / checksum.name)
    print(f'Package: {output / (name+".zip")}')


def verify_checksum(archive):
    sidecar = archive.with_name(archive.name + '.sha256')
    if sidecar.read_text(encoding='ascii').split() != [digest(archive), archive.name]:
        raise ValueError(f'Invalid checksum: {archive.name}')


def debian_version(version):
    if not VERSION.fullmatch(version): raise ValueError('Invalid version')
    # Debian's ~ sorts a prerelease before the corresponding stable version.
    return version.replace('-', '~')


def package_deb(source, output):
    """Package the same Linux payload for dpkg, without a user-prefix installer."""
    source, output = Path(source).resolve(), Path(output).resolve()
    manifest = json.loads((source / 'build-info.json').read_text(encoding='utf-8'))
    version = manifest['version']
    archive = output / f'rustterm-{version}-linux-x86_64.deb'
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='rustterm-deb-') as temporary:
        work = Path(temporary)
        root = work / 'debian/rustterm'
        control = root / 'DEBIAN'
        control.mkdir(parents=True)
        for relative in sorted(LINUX_FILES):
            destination = root / 'usr' / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / relative, destination)
            destination.chmod(0o755 if relative == 'bin/rustterm' else 0o644)
        # Keep the system package distinct even when the application runs as root.
        manifest['installation'] = 'deb'
        (root / 'usr/share/doc/rustterm/build-info.json').write_text(
            json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        shutil.copyfile(ROOT / 'LICENSE', root / 'usr/share/doc/rustterm/copyright')
        (work / 'debian/control').write_text(
            'Source: rustterm\n\nPackage: rustterm\nArchitecture: amd64\n', encoding='utf-8')
        dependencies = subprocess.check_output(
            ['dpkg-shlibdeps', '-O', '-e' + str(root / 'usr/bin/rustterm')],
            cwd=work, text=True).strip()
        if not dependencies.startswith('shlibs:Depends=') or '\n' in dependencies:
            raise ValueError('Could not determine Debian runtime dependencies')
        # dlopen libraries are invisible to dpkg-shlibdeps. Fontconfig also
        # supplies the system font configuration read by the terminal renderer.
        dependencies = dependencies.removeprefix('shlibs:Depends=') + (
            ', fontconfig, libvulkan1, libwayland-client0, libwayland-cursor0, libpulse0, ca-certificates')
        files = sorted(path for path in root.rglob('*') if path.is_file())
        installed_size = sum((path.stat().st_size + 1023) // 1024 for path in files)
        (control / 'control').write_text(
            f'Package: rustterm\nVersion: {debian_version(version)}\nArchitecture: amd64\n'
            'Section: utils\nPriority: optional\nMaintainer: RustTerm contributors\n'
            'Homepage: https://github.com/oksyd/RustTerm\n'
            'Bugs: https://github.com/oksyd/RustTerm/issues\n'
            f'Installed-Size: {installed_size}\nDepends: {dependencies}\n'
            'Suggests: lrzsz, gnome-keyring\n'
            'Description: Native terminal, remote desktop, and database workspace\n'
            ' RustTerm provides local and SSH terminals, SFTP, RDP, and database clients.\n',
            encoding='utf-8')
        (control / 'md5sums').write_text(''.join(
            f'{hashlib.md5(path.read_bytes(), usedforsecurity=False).hexdigest()}  {path.relative_to(root).as_posix()}\n'
            for path in files), encoding='utf-8')
        for path in [root, *root.rglob('*')]:
            if path.is_dir(): path.chmod(0o755)
            elif path != root / 'usr/bin/rustterm': path.chmod(0o644)
        environment = dict(os.environ)
        environment.setdefault('SOURCE_DATE_EPOCH', '0')
        staged = work / archive.name
        subprocess.run(['dpkg-deb', '--root-owner-group', '-Zxz', '--build', str(root), str(staged)],
                       check=True, env=environment)
        shutil.copyfile(staged, archive)
    archive.with_name(archive.name + '.sha256').write_text(
        f'{digest(archive)}  {archive.name}\n', encoding='ascii')


def verify_deb(archive, version, variant='release'):
    verify_checksum(archive)
    with tempfile.TemporaryFile() as data:
        subprocess.run(['dpkg-deb', '--ctrl-tarfile', str(archive)], stdout=data, check=True)
        data.seek(0)
        with tarfile.open(fileobj=data, mode='r:') as package:
            seen = set()
            for entry in package:
                relative = PurePosixPath(entry.name).as_posix()
                if entry.isdir() and relative == '.': continue
                if not entry.isfile() or relative not in {'control', 'md5sums'} or relative in seen:
                    raise ValueError('Unexpected Debian control member')
                seen.add(relative)
            if seen != {'control', 'md5sums'}: raise ValueError('Incomplete Debian control data')
    fields = subprocess.check_output(
        ['dpkg-deb', '--field', str(archive), 'Package', 'Version', 'Architecture', 'Depends'], text=True)
    control = dict(line.split(': ', 1) for line in fields.strip().splitlines())
    if any(control.get(k) != v for k, v in {
        'Package': 'rustterm', 'Version': debian_version(version), 'Architecture': 'amd64',
    }.items()) or not control.get('Depends'):
        raise ValueError('Debian package metadata mismatch')
    expected = {'usr/' + path for path in LINUX_FILES} | {
        'usr/share/doc/rustterm/build-info.json', 'usr/share/doc/rustterm/copyright',
    }
    directories = {parent.as_posix() for path in expected for parent in PurePosixPath(path).parents}
    with tempfile.TemporaryFile() as data:
        subprocess.run(['dpkg-deb', '--fsys-tarfile', str(archive)], stdout=data, check=True)
        data.seek(0)
        with tarfile.open(fileobj=data, mode='r:') as package:
            seen = {}
            for entry in package:
                path = PurePosixPath(entry.name)
                relative = path.as_posix()
                if path.is_absolute() or '..' in path.parts or '\\' in entry.name:
                    raise ValueError('Unsafe Debian package path')
                if entry.uid != 0 or entry.gid != 0:
                    raise ValueError('Debian package files must be owned by root')
                if entry.isdir() and relative in directories:
                    if entry.mode != 0o755: raise ValueError('Invalid Debian directory permissions')
                    continue
                if not entry.isfile() or relative in seen or relative not in expected:
                    raise ValueError('Unexpected/duplicate Debian package member')
                if entry.mode != (0o755 if relative == 'usr/bin/rustterm' else 0o644):
                    raise ValueError('Invalid Debian file permissions')
                seen[relative] = entry
            if set(seen) != expected: raise ValueError('Incomplete Debian package')
            with package.extractfile(seen['usr/share/doc/rustterm/build-info.json']) as source:
                manifest = json.load(source)
            if any(manifest.get(k) != v for k, v in {
                'version': version, 'platform': 'linux-x86_64', 'variant': variant, 'installation': 'deb',
            }.items()):
                raise ValueError('Debian package manifest mismatch')
            with package.extractfile(seen['usr/bin/rustterm']) as source:
                if hashlib.file_digest(source, 'sha256').hexdigest() != manifest.get('binary_sha256'):
                    raise ValueError('Executable checksum mismatch')
            with package.extractfile(seen['usr/bin/rustterm']) as source:
                verify_binary_privacy(source)
    return manifest


def verify_archive(archive, version, platform_name):
    name = f'rustterm-{version}-{platform_name}'
    verify_checksum(archive)
    windows = platform_name.startswith('windows-')
    expected = ({'rustterm.exe', 'build-info.json', 'README.md', 'LICENSE', 'alacritty_terminal-LICENSE-APACHE'}
                if windows else LINUX_FILES | {'build-info.json', 'install.py', 'install.sh'})
    with (zipfile.ZipFile(archive) if windows else tarfile.open(archive, 'r:gz')) as package:
        entries = package.infolist() if windows else package.getmembers()
        seen = set()
        for entry in entries:
            member = entry.filename if windows else entry.name
            path = PurePosixPath(member)
            if path.is_absolute() or '..' in path.parts or '\\' in member or not path.parts or path.parts[0] != name:
                raise ValueError('Unsafe release archive path')
            if (entry.is_dir() if windows else entry.isdir()): continue
            if (windows and (entry.external_attr >> 16) & 0o170000 not in {0, 0o100000}) or (not windows and not entry.isfile()):
                raise ValueError('Links/special files are forbidden in release archives')
            relative = path.relative_to(name).as_posix()
            if relative in seen or relative not in expected:
                raise ValueError(f'Unexpected/duplicate release member: {relative}')
            seen.add(relative)
        if seen != expected: raise ValueError('Incomplete release archive')
        def read(member):
            return package.open(f'{name}/{member}') if windows else package.extractfile(f'{name}/{member}')
        with read('build-info.json') as source: manifest = json.load(source)
        if any(manifest.get(k) != v for k,v in {'version': version,'platform':platform_name,'variant':'release'}.items()):
            raise ValueError('Release manifest version/platform/variant mismatch')
        with read('rustterm.exe' if windows else 'bin/rustterm') as source:
            if hashlib.file_digest(source, 'sha256').hexdigest() != manifest.get('binary_sha256'):
                raise ValueError('Executable checksum mismatch')
        with read('rustterm.exe' if windows else 'bin/rustterm') as source:
            verify_binary_privacy(source)
    return manifest


def verify_release(directory, version):
    if not VERSION.fullmatch(version): raise ValueError('Invalid version')
    directory = Path(directory)
    expected = set()
    manifests = {}
    for platform_name, extension in PLATFORMS.items():
        archive = directory / f'rustterm-{version}-{platform_name}.{extension}'
        manifests[platform_name] = verify_archive(archive, version, platform_name)
        expected.update({archive.name, archive.name+'.sha256'})
    deb = directory / f'rustterm-{version}-linux-x86_64.deb'
    manifest = verify_deb(deb, version)
    if manifest['binary_sha256'] != manifests['linux-x86_64']['binary_sha256']:
        raise ValueError('Linux packages contain different executables')
    expected.update({deb.name, deb.name + '.sha256'})
    actual = {path.name for path in directory.iterdir() if path.is_file()} - {'SHA256SUMS'}
    if actual != expected: raise ValueError('Release contains missing or unexpected assets')
    (directory/'SHA256SUMS').write_text(''.join(f'{digest(directory/name)}  {name}\n' for name in sorted(expected)), encoding='ascii')
    print('Verified both platforms and generated SHA256SUMS')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    command = commands.add_parser('metadata'); command.add_argument('--tag', required=True)
    command = commands.add_parser('build'); command.add_argument('--target')
    command = commands.add_parser('package-windows')
    modes = command.add_mutually_exclusive_group(required=True)
    modes.add_argument('--release', action='store_true'); modes.add_argument('--binary')
    command.add_argument('--output', default='dist')
    command = commands.add_parser('verify'); command.add_argument('--directory', default='dist'); command.add_argument('--version', required=True)
    command = commands.add_parser('binary-version'); command.add_argument('binary')
    command = commands.add_parser('binary-privacy'); command.add_argument('binary')
    args = parser.parse_args()
    if args.command == 'metadata': metadata(args.tag)
    elif args.command == 'build': build_release(args.target)
    elif args.command == 'package-windows': package_windows(args)
    elif args.command == 'verify': verify_release(args.directory, args.version)
    elif args.command == 'binary-version': verify_executable(args.binary, package_info()['version'])
    elif args.command == 'binary-privacy':
        with open(args.binary, 'rb') as source:
            verify_binary_privacy(source)

if __name__ == '__main__':
    main()
