#!/usr/bin/env bash
# Сборка veldb под Raspberry Pi (aarch64, 64-битная Raspberry Pi OS / Ubuntu).
#
# Нужен кросс-компилятор C: sqlparser тянет psm/stacker (защита парсера от
# переполнения стека на вложенных скобках), а он собирается через cc.
#
# Вариант 1 — Docker + cross (ничего не ставится в систему):
#     cargo install cross --git https://github.com/cross-rs/cross
#     cross build --release --target aarch64-unknown-linux-gnu
#
# Вариант 2 — тулчейн локально (macOS, нужны Command Line Tools):
#     brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu
#     rustup target add aarch64-unknown-linux-gnu
#     а дальше этот скрипт.
#
# Вариант 3 — собрать прямо на Pi: `cargo build --release`. Дольше (10-20 мин
# на Pi 4), но без единой подвижной части.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=aarch64-unknown-linux-gnu
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc

if ! command -v aarch64-linux-gnu-gcc >/dev/null; then
  echo "нет aarch64-linux-gnu-gcc — см. варианты в шапке скрипта" >&2
  exit 1
fi

rustup target add "$TARGET"
cargo build --release --target "$TARGET"
ls -lh "target/$TARGET/release/veldb" "target/$TARGET/release/veldbctl"
