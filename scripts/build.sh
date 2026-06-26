#!/usr/bin/env bash
# Полная сборка: компилятор/VM + инструмент translit + тесты.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== cargo build (sga) =="
cargo build --release

echo "== cargo build (sga-translit) =="
cargo build --release --manifest-path tools/translit/Cargo.toml

echo "== cargo test =="
cargo test

echo "Готово. Бинарники:"
echo "  target/release/sga"
echo "  tools/translit/target/release/sga-translit"
