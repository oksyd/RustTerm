#!/usr/bin/env bash
# Stage all package files before changing the installation; rollback is handled by Python.
set -euo pipefail
package_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$package_root/install.py" "$@"
