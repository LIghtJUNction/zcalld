#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=target
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --release --workspace --locked
python3 scripts/integration.py
python3 scripts/release.py check
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_release.py
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck scripts/check.sh scripts/package.sh packaging/install.sh
fi
