#!/usr/bin/env python3
"""Upload verified desktop builds to a draft for manual publication on GitHub."""
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

from artifacts import digest, verify_release, version_info


def gh(*args, check=True):
    result = subprocess.run(['gh', *args], text=True, capture_output=True)
    if check and result.returncode:
        raise RuntimeError('GitHub CLI failed: ' + result.stderr.strip())
    return result


def verify_tag(repo, tag, commit):
    actual = gh('api', f'repos/{repo}/commits/{tag}', '--jq', '.sha').stdout.strip()
    if actual != commit:
        raise ValueError('Release tag changed since the build; refusing to update the draft')


def release(repo, tag):
    result = gh('api', f'repos/{repo}/releases/tags/{tag}', check=False)
    if result.returncode:
        if '(HTTP 404)' in result.stderr: return None
        raise RuntimeError('Could not inspect GitHub Release: '+result.stderr.strip())
    return json.loads(result.stdout)


def verify_uploaded(assets, directory):
    expected = {file.name: file for file in directory.iterdir() if file.is_file()}
    if len(assets) != len(expected) or {asset['name'] for asset in assets} != expected.keys():
        raise ValueError('GitHub release assets do not match the verified artifact set')
    for asset in assets:
        file = expected[asset['name']]
        if asset['size'] != file.stat().st_size or asset.get('digest') != 'sha256:'+digest(file):
            raise ValueError(f'GitHub asset size/digest mismatch: {file.name}')


def main():
    tag, commit, repo = (os.environ[name] for name in ['RELEASE_TAG', 'RELEASE_COMMIT', 'GH_REPO'])
    info = version_info(tag)
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repo) or not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('Invalid repository/commit')
    directory = Path('dist')
    verify_release(directory, info['version'])
    verify_tag(repo, tag, commit)
    marker = f'<!-- rustterm-release-commit:{commit} -->'
    existing = release(repo, tag)
    if existing and (not existing['draft'] or marker not in (existing.get('body') or '')):
        raise ValueError('Refusing to overwrite a published release or a draft created elsewhere')
    if not existing:
        notes = f'''{marker}
Desktop binaries for RustTerm {info['version']}.

| Platform | Artifact | Start |
| --- | --- | --- |
| Ubuntu 24.04 / 26.04 x86_64 | `rustterm-{info['version']}-linux-x86_64.tar.gz` | Extract and run `./install.sh` (Python 3.11+) |
| Windows x86_64 (MSVC) | `rustterm-{info['version']}-windows-x86_64.zip` | Extract and run `rustterm.exe` |

Each archive includes its own top-level directory, licenses, README, and build metadata. `SHA256SUMS` covers all downloads; individual `.sha256` files support the Ubuntu in-app updater.

Windows packages are preview builds: compilation and package integrity are checked, while native runtime validation is pending. RDP audio, clipboard files, folder redirection, exclusive keyboard capture, ZMODEM, and SQL SSH tunnels currently require Linux. Windows portable updates are installed by downloading the next ZIP.

'''
        with tempfile.TemporaryDirectory(prefix='rustterm-release-notes-') as temporary:
            path = Path(temporary)/'notes.md'; path.write_text(notes,encoding='utf-8')
            gh('release', 'create', tag, '--repo', repo, '--verify-tag', '--target', commit,
               '--title', f'RustTerm {info["version"]}', '--draft', '--generate-notes', '--notes-file', str(path),
               '--prerelease='+info['prerelease'])
    assets = sorted(str(file) for file in directory.iterdir() if file.is_file())
    gh('release', 'upload', tag, *assets, '--repo', repo, '--clobber')
    for attempt in range(5):
        uploaded = release(repo, tag)
        if not uploaded or not uploaded['draft'] or marker not in (uploaded.get('body') or ''):
            raise ValueError('Release changed during upload; expected an owned draft')
        try:
            verify_uploaded(uploaded['assets'], directory)
            break
        except ValueError:
            if attempt == 4: raise
            time.sleep(3)
    verify_tag(repo, tag, commit)
    print(f'Draft ready: {repo} {tag}. Publish it manually on GitHub after review.')
    print(uploaded.get('html_url') or f'https://github.com/{repo}/releases')

if __name__ == '__main__':
    main()
