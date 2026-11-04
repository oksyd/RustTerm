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
    # /releases/tags/{tag} only resolves published releases. The authenticated
    # list includes drafts; paginate so retries also find older pending drafts.
    pages = json.loads(gh('api', f'repos/{repo}/releases?per_page=100', '--paginate', '--slurp').stdout)
    matches = [item for page in pages for item in page if item['tag_name'] == tag]
    if len(matches) > 1:
        raise ValueError('Multiple releases match this tag; refusing to choose a draft')
    return matches[0] if matches else None


def release_by_id(repo, release_id):
    result = gh('api', f'repos/{repo}/releases/{release_id}', check=False)
    if result.returncode:
        if '(HTTP 404)' in result.stderr: return None
        raise RuntimeError('Could not inspect GitHub Release: '+result.stderr.strip())
    return json.loads(result.stdout)


def owned_draft_id(value, tag, marker):
    if not value or value.get('draft') is not True or marker not in (value.get('body') or ''):
        raise ValueError('Refusing to overwrite a published release or a draft created elsewhere')
    if value.get('tag_name') != tag:
        raise ValueError('Release tag changed; refusing to update the draft')
    release_id = value.get('id')
    if type(release_id) is not int or release_id <= 0:
        raise ValueError('GitHub returned an invalid release ID')
    return release_id


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
    if not existing:
        notes = f'''{marker}
Desktop binaries for RustTerm {info['version']}.

| Platform | Artifact | Start |
| --- | --- | --- |
| Ubuntu 24.04 / 26.04 x86_64 (recommended) | `rustterm-{info['version']}-linux-x86_64.deb` | `sudo apt install ./rustterm-{info['version']}-linux-x86_64.deb` |
| Linux x86_64, user installation | `rustterm-{info['version']}-linux-x86_64.tar.gz` | Extract and run `./install.sh` (Python 3.11+) |
| Windows x86_64 (MSVC) | `rustterm-{info['version']}-windows-x86_64.zip` | Extract and run `rustterm.exe` |

The Ubuntu `.deb` includes the application menu entry and installs runtime dependencies through APT. In-app updates download the new `.deb` and request system authorization for APT to install it; uninstall with `sudo apt remove rustterm`. The `.tar.gz` supports installation without sudo and in-app updates. Both Linux packages contain the same executable.

Packages include licenses, README, and build metadata. `SHA256SUMS` covers all downloads; each package also has an individual `.sha256` file.

Windows packages are preview builds: compilation and package integrity are checked, while native runtime validation is pending. RDP audio, clipboard files, folder redirection, exclusive keyboard capture, ZMODEM, and SQL SSH tunnels currently require Linux. Windows portable updates are installed by downloading the next ZIP.

'''
        with tempfile.TemporaryDirectory(prefix='rustterm-release-notes-') as temporary:
            path = Path(temporary)/'notes.md'; path.write_text(notes,encoding='utf-8')
            gh('release', 'create', tag, '--repo', repo, '--verify-tag', '--target', commit,
               '--title', f'RustTerm {info["version"]}', '--draft', '--generate-notes', '--notes-file', str(path),
               '--prerelease='+info['prerelease'])
        for attempt in range(5):
            existing = release(repo, tag)
            if existing: break
            if attempt == 4:
                raise ValueError('Created draft is not visible through the release list; retry the workflow')
            time.sleep(3)
    release_id = owned_draft_id(existing, tag, marker)
    assets = sorted(str(file) for file in directory.iterdir() if file.is_file())
    gh('release', 'upload', tag, *assets, '--repo', repo, '--clobber')
    for attempt in range(5):
        uploaded = release_by_id(repo, release_id)
        if uploaded is not None:
            if owned_draft_id(uploaded, tag, marker) != release_id:
                raise ValueError('Release ID changed during upload; expected the original draft')
            try:
                verify_uploaded(uploaded['assets'], directory)
                break
            except ValueError:
                if attempt == 4: raise
        elif attempt == 4:
            raise ValueError('Uploaded draft is no longer visible by its release ID; retry the workflow')
        time.sleep(3)
    verify_tag(repo, tag, commit)
    print(f'Draft ready: {repo} {tag}. Publish it manually on GitHub after review.')
    print(uploaded.get('html_url') or f'https://github.com/{repo}/releases')

if __name__ == '__main__':
    main()
