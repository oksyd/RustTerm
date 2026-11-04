#!/usr/bin/env bash
# Build an Ubuntu .deb and a user-installable archive from the same executable.
set -euo pipefail

usage() {
    printf '%s\n' \
        'Usage: tools/release/package-linux.sh (--release | --binary PATH) [--output DIRECTORY]' \
        'Produces an Ubuntu .deb and a .tar.gz for user-prefix installation and updates.' \
        '--release explicitly builds rustterm with cargo --release --locked and packages it.' \
        '--binary packages an existing binary with a -dev suffix for installer verification.'
}

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
output="$repo_root/dist"
binary=''
build_release=false
while (( $# )); do
    case "$1" in
        --output|--binary)
            if (( $# < 2 )); then usage >&2; exit 2; fi
            if [[ "$1" == --output ]]; then output="$2"; else binary="$2"; fi
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --release) build_release=true; shift ;;
        *) usage >&2; exit 2 ;;
    esac
done
if [[ -z "$binary" ]] && ! "$build_release"; then
    usage
    exit 0
fi
if [[ -n "$binary" ]] && "$build_release"; then
    printf '%s\n' 'Choose either --release or --binary, not both.' >&2
    exit 2
fi

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
    printf '%s\n' 'This package target is Linux x86_64.' >&2
    exit 1
fi
for tool in dpkg-deb dpkg-shlibdeps; do
    if ! command -v "$tool" >/dev/null; then
        printf '%s\n' 'Debian packaging requires dpkg-dev and xz-utils.' >&2
        exit 1
    fi
done
if [[ -n "$binary" ]]; then binary="$(realpath -- "$binary")"; fi
mkdir -p -- "$output"
output="$(cd -- "$output" && pwd)"
cd -- "$repo_root"
version="$(python3 - <<'PY'
from tools.release.artifacts import package_info, version_info
version = package_info()['version']
print(version_info(version)['version'])
PY
)"
variant=release
if [[ -z "$binary" ]]; then
    python3 tools/release/artifacts.py build
    binary="${CARGO_TARGET_DIR:-$repo_root/target}/release/rustterm"
else
    variant=dev
    version="$version-dev"
fi
if [[ ! -f "$binary" || ! -x "$binary" ]]; then
    printf '%s\n' 'RustTerm executable is missing or not executable.' >&2
    exit 1
fi
python3 tools/release/artifacts.py binary-privacy "$binary"

stage="$(mktemp -d)"
artifact=''
trap 'rm -rf -- "$stage"; if [[ -n "$artifact" ]]; then rm -rf -- "$artifact"; fi' EXIT
artifact="$(mktemp -d "$output/.rustterm-package.XXXXXX")"
name="rustterm-$version-linux-x86_64"
root="$stage/$name"
mkdir -p "$root/bin" "$root/share/applications" "$root/share/icons/hicolor/scalable/apps" "$root/share/doc/rustterm"
install -m 755 "$binary" "$root/bin/rustterm"
install -m 644 assets/io.rustterm.RustTerm.desktop "$root/share/applications/"
install -m 644 assets/io.rustterm.RustTerm.svg "$root/share/icons/hicolor/scalable/apps/"
install -m 644 LICENSE README.md "$root/share/doc/rustterm/"
install -m 644 assets/licenses/alacritty_terminal-LICENSE-APACHE "$root/share/doc/rustterm/alacritty_terminal-LICENSE-APACHE"
install -m 755 src/update/installer/install.sh "$root/install.sh"
install -m 644 src/update/installer/install.py "$root/install.py"
python3 - "$root" "$variant" "$version" <<'PY'
import hashlib, json, pathlib, sys
root, variant, version = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
with (root/'bin/rustterm').open('rb') as binary:
    binary_hash = hashlib.file_digest(binary, 'sha256').hexdigest()
manifest = {'platform': 'linux-x86_64', 'variant': variant, 'version': version,
            'binary_sha256': binary_hash,
            'lock_sha256': hashlib.sha256(pathlib.Path('Cargo.lock').read_bytes()).hexdigest()}
(root/'build-info.json').write_text(json.dumps(manifest, indent=2) + '\n')
PY
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
    -czf "$artifact/$name.tar.gz" -C "$stage" "$name"
(cd -- "$artifact" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
python3 - "$root" "$artifact" "$version" "$variant" <<'PY'
from pathlib import Path
import sys
from tools.release.artifacts import package_deb, verify_deb
root, output, version, variant = sys.argv[1:]
package_deb(root, output)
verify_deb(Path(output) / f'rustterm-{version}-linux-x86_64.deb', version, variant)
PY
# Publish only complete artifacts. A failed build/hash leaves existing packages intact.
for extension in deb deb.sha256 tar.gz tar.gz.sha256; do
    mv -f -- "$artifact/$name.$extension" "$output/$name.$extension"
done
printf 'Package: %s\nPackage: %s\n' "$output/$name.deb" "$output/$name.tar.gz"
