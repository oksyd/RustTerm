#!/usr/bin/env bash
# Reproducible, headless environment and terminal probe.
set -euo pipefail

usage() {
    printf '%s\n' 'Usage: tools/dev/probe.sh [--environment-only]' \
        'Without arguments: report the environment and run terminal module/PTY tests.' \
        'No graphical dependencies are installed and no application is launched.'
}

environment_only=false
case "${1:-}" in
    '') ;;
    --environment-only) environment_only=true ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
esac
if (( $# > 1 )); then
    usage >&2
    exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd -- "$repo_root"

printf '\n%s\n' '=== Environment (not a GUI acceptance result) ==='
date -u '+UTC: %Y-%m-%dT%H:%M:%SZ'
uname -srmo
if [[ -f /etc/os-release ]]; then
    # Print selected non-secret OS fields without evaluating this file as code.
    sed -n '/^PRETTY_NAME=/p; /^VERSION_ID=/p' /etc/os-release
fi
rustc --version
cargo --version
if [[ -f Cargo.lock ]]; then
    sha256sum Cargo.lock
else
    printf '%s\n' 'Cargo.lock: missing'
fi

for variable in DISPLAY WAYLAND_DISPLAY XDG_RUNTIME_DIR XDG_SESSION_TYPE; do
    if [[ -n "${!variable:-}" ]]; then
        printf '%s: set\n' "$variable"
    else
        printf '%s: unset\n' "$variable"
    fi
done
for tool in Xvfb xvfb-run weston cage Xorg vulkaninfo pkg-config cc cmake ninja bash; do
    if command -v "$tool" >/dev/null 2>&1; then
        printf '%s: available\n' "$tool"
    else
        printf '%s: unavailable\n' "$tool"
    fi
done
if command -v pkg-config >/dev/null 2>&1; then
    for package in x11 xkbcommon wayland-client fontconfig freetype2; do
        if version="$(pkg-config --modversion "$package" 2>/dev/null)"; then
            printf 'pkg-config %s: %s\n' "$package" "$version"
        else
            printf 'pkg-config %s: unavailable\n' "$package"
        fi
    done
fi
if [[ -d /dev/dri ]]; then
    printf '%s\n' '/dev/dri: present (device presence alone is not a rendering test)'
else
    printf '%s\n' '/dev/dri: absent'
fi
if [[ -f /usr/share/vulkan/icd.d/lvp_icd.json ]]; then
    printf '%s\n' 'Mesa software Vulkan ICD descriptor: present (execution not verified)'
fi

if "$environment_only"; then
    exit 0
fi

printf '\n%s\n' '=== Core and real PTY tests ==='
cargo test --locked --lib terminal:: -- --test-threads=1
cargo test --locked --test pty -- --test-threads=1
printf '\n%s\n' \
    'Headless tests passed. Ubuntu GUI, IME, DPI and performance acceptance remain separate.'
