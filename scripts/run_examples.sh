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
# Пример write_file пишет файл на диск (T015). Убираем его при ЛЮБОМ выходе из
# скрипта, в т.ч. при ошибке, чтобы запуск примеров не оставлял следов в
# examples/data/.
trap 'rm -f examples/data/written_by_example.txt' EXIT

for f in examples/*.sga; do
    echo "=== $f ==="
    # Запись файлов запрещена по умолчанию: флаг --allow-write получает ТОЛЬКО
    # пример, который пишет файлы. (${flags[@]+...} — чтобы пустой массив не
    # ронял `set -u` на старых bash.)
    flags=()
    case "$f" in
        examples/write_file.sga) flags=(--allow-write) ;;
    esac
    ./target/debug/sga run ${flags[@]+"${flags[@]}"} "$f" </dev/null
    echo
done
