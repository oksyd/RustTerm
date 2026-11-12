set shell := ["bash", "-euo", "pipefail", "-c"]

patch:
  cargo release patch --no-publish --execute

publish:
  cargo publish

ci:
  @if ! "${RUSTTERM_TEST_ZSH:-zsh}" --version >/dev/null 2>&1; then echo "PTY tests require Zsh. Install zsh or set RUSTTERM_TEST_ZSH to its executable." >&2; exit 1; fi
  cargo fmt --all --check
  cargo fmt --manifest-path crates/rustrdp/Cargo.toml --check
  # RustTerm is a binary application with no feature matrix or crates.io release.
  cargo check --locked --all-targets
  cargo clippy --locked --all-targets -- -D warnings
  RUSTTERM_TEST_ZSH="${RUSTTERM_TEST_ZSH:-zsh}" cargo nextest run --locked --all-targets
  cargo test --locked --doc
  cargo doc --locked --no-deps
  # rustrdp is a library: retain its public API documentation and feature checks.
  cargo clippy --manifest-path crates/rustrdp/Cargo.toml --target-dir target --all-targets --all-features -- -D warnings
  cargo clippy --manifest-path crates/rustrdp/Cargo.toml --target-dir target --all-targets --no-default-features -- -D warnings
  cargo test --manifest-path crates/rustrdp/Cargo.toml --target-dir target --all-features
  cargo test --manifest-path crates/rustrdp/Cargo.toml --target-dir target --no-default-features
  cargo doc --manifest-path crates/rustrdp/Cargo.toml --target-dir target --no-deps --all-features
  python3 -m unittest discover -s tools/release -p test_release.py
  python3 -m unittest discover -s src/update/installer -p test_install.py
