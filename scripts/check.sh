#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
CARGO_TARGET_DIR="$(cargo metadata --no-deps --format-version 1 --locked --offline | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
export CARGO_TARGET_DIR
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --release --workspace --locked
cargo build --release --locked -p zcalld-plugin-routing --example recovery-fixture
python3 scripts/test-recovery.py
python3 scripts/test-service-install.py --binary "$CARGO_TARGET_DIR/release/zcalld" --installer "$CARGO_TARGET_DIR/release/zcalld-installer"
python3 scripts/integration.py "$CARGO_TARGET_DIR/release/zcalld"
python3 scripts/test-transport.py --binary "$CARGO_TARGET_DIR/release/zcalld"
python3 scripts/test-generation-cancellation.py --binary "$CARGO_TARGET_DIR/release/zcalld"
ZCALLD_TEST_BINARY="$CARGO_TARGET_DIR/release/zcalld" python3 scripts/test-control.py
ZCALLD_TEST_BINARY="$CARGO_TARGET_DIR/release/zcalld" python3 scripts/test-keys.py
ZCALLD_TEST_BINARY="$CARGO_TARGET_DIR/release/zcalld" python3 scripts/test-library.py
python3 scripts/test-installer.py --binary "$CARGO_TARGET_DIR/release/zcalld" --installer "$CARGO_TARGET_DIR/release/zcalld-installer"
python3 scripts/test-cutover.py
python3 scripts/release.py check
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p test_release.py
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck scripts/check.sh scripts/package.sh packaging/install.sh
fi
