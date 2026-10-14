#!/usr/bin/env bash
# Запускает все примеры из examples/ на debug-сборке VM.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -x target/debug/sga ]; then
    echo "сборка sga..."
    cargo build
fi

# Примеры запускаются с пустым stdin (</dev/null): пример std/io читает
# стандартный ввод (examples/stdin.sga) и в терминале ждал бы ввода с
# клавиатуры. Со своим вводом: printf '...' | ./target/debug/sga run examples/stdin.sga
for f in examples/*.sga; do
    echo "=== $f ==="
    ./target/debug/sga run "$f" </dev/null
    echo
done
