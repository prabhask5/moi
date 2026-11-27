#!/usr/bin/env bash
# Everything CI runs, locally. Usage: scripts/ci.sh
set -euo pipefail
cd "$(dirname "$0")/.."
step() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
step "tests live in tests/"
scripts/check-tests-location.sh
step "no secrets in the repo"
scripts/check-secrets.sh
step "rustfmt"
cargo fmt --all -- --check
step "clippy"
cargo clippy --all-targets --all-features -- -D warnings
step "tests"
cargo test --all-features
step "docs links"
python3 scripts/check-links.py
if command -v cargo-deny >/dev/null; then step "cargo deny"; cargo deny check; else echo "(cargo-deny not installed — skipped locally, runs in CI)"; fi
step "budgets"
scripts/budgets.sh
printf '\n\033[1;32mall green\033[0m\n'
