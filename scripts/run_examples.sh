#!/usr/bin/env bash
# Запускает все примеры из examples/ на debug-сборке VM.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -x target/debug/sga ]; then
    echo "сборка sga..."
    cargo build
fi

for f in examples/*.sga; do
    echo "=== $f ==="
    ./target/debug/sga run "$f"
    echo
done
