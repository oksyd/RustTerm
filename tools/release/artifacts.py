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
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z")


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
    return sorted({(str(path.resolve()), replacement) for path, replacement in paths}, key=lambda item: len(item[0]))


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
        if len(Path(path).parts) < 2:
            continue
        for spelling in {path, path.replace('\\', '/')}:
            needles.update(spelling.lower().encode(encoding) for encoding in ('utf-8', 'utf-16-le'))
    # The draft job may run on a different host than the build job.
    identifying_path = re.compile(rb'/(?:home|Users)/[^/\s\x00]+/|[a-z]:[/\\](?:users|a)[/\\][^/\\\s\x00]+[/\\]', re.I)
    overlap = max(1024, max(map(len, needles), default=0))
    tail = b''
    while chunk := source.read(1024 * 1024):
        data = tail + chunk
        lower = data.lower()
        if any(needle in lower for needle in needles) or identifying_path.search(data) or identifying_path.search(data.replace(b'\x00', b'')):
            raise ValueError('Executable contains a build-host path; rebuild with the release packaging tools')
        tail = data[-overlap:]


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


def verify_archive(archive, version, platform_name):
    name = f'rustterm-{version}-{platform_name}'
    sidecar = archive.with_name(archive.name + '.sha256')
    if sidecar.read_text(encoding='ascii').split() != [digest(archive), archive.name]:
        raise ValueError(f'Invalid checksum: {archive.name}')
    windows = platform_name.startswith('windows-')
    expected = {'rustterm.exe', 'build-info.json', 'README.md', 'LICENSE', 'alacritty_terminal-LICENSE-APACHE'} if windows else {
        'bin/rustterm', 'build-info.json', 'install.py', 'install.sh',
        'share/applications/io.rustterm.RustTerm.desktop', 'share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg',
        'share/doc/rustterm/README.md', 'share/doc/rustterm/LICENSE', 'share/doc/rustterm/alacritty_terminal-LICENSE-APACHE',
    }
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


def verify_release(directory, version):
    if not VERSION.fullmatch(version): raise ValueError('Invalid version')
    directory = Path(directory)
    expected = set()
    for platform_name, extension in PLATFORMS.items():
        archive = directory / f'rustterm-{version}-{platform_name}.{extension}'
        verify_archive(archive, version, platform_name)
        expected.update({archive.name, archive.name+'.sha256'})
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
