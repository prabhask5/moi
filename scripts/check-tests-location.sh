#!/usr/bin/env bash
# Integration tests must live in tests/ (CLAUDE.md §4). Inline #[cfg(test)] unit tests for pure functions are fine.
set -euo pipefail
cd "$(dirname "$0")/.."
stray=$(git ls-files 2>/dev/null | grep -E '(^|/)(test_[^/]*|[^/]*_test)\.rs$' | grep -v '^tests/' || true)
if [[ -n $stray ]]; then echo "Test files outside tests/ are not run consistently. Move them into tests/:"; echo "$stray"; exit 1; fi
echo "ok"
