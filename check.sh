#!/usr/bin/env bash
# Полная проверка перед коммитом: форматирование, линтер, тесты.
set -euo pipefail
cd "$(dirname "$0")"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
