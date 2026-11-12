set shell := ["bash", "-euo", "pipefail", "-c"]

patch:
  cargo release patch --no-publish --execute

publish:
  cargo publish

[parallel]
ci: _ci-format _ci-rust _ci-python

ci-full: ci
  cargo clippy --manifest-path crates/rustrdp/Cargo.toml --target-dir target --all-targets --all-features -- -D warnings
  cargo test --manifest-path crates/rustrdp/Cargo.toml --target-dir target --all-features
  cargo clippy --manifest-path crates/rustrdp/Cargo.toml --target-dir target --all-targets --no-default-features -- -D warnings
  cargo test --manifest-path crates/rustrdp/Cargo.toml --target-dir target --no-default-features
  cargo doc --locked --no-deps
  cargo doc --manifest-path crates/rustrdp/Cargo.toml --target-dir target --no-deps --all-features

_ci-format:
  cargo fmt --all --check
  cargo fmt --manifest-path crates/rustrdp/Cargo.toml --check

_ci-rust:
  cargo clippy --locked --all-targets -p rustterm -p rustrdp -- -D warnings
  # Zsh PTY tests are optional locally; Actions installs and requires Zsh explicitly.
  cargo nextest run --locked --all-targets -p rustterm -p rustrdp --status-level slow
  cargo test --locked --doc -p rustterm -p rustrdp

_ci-python:
  python3 -m unittest discover -s tools/release -p test_release.py
  python3 -m unittest discover -s src/update/installer -p test_install.py
