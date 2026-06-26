# SGA Language — VS Code Extension (v0.1.0)

## Реализовано

- ✅ Подсветка синтаксиса (`syntaxes/sga.tmLanguage.json`) — TextMate-грамматика
  распознаёт реальные SGA Unicode-кодпоинты официального блока UCSUR
  "Standard Galactic" (U+EB40..U+EB5F) как ключевые слова (`LET`/`VAR`/
  `CONST`/`FN`/`IF`/`ELSE`/`WHILE`/`FOR`/`IN`/`BREAK`/`CONTINUE`/`RETURN`/
  `AND`/`OR`/`NOT`/`TRUE`/`FALSE`/`NIL`/`STRUCT`/`PRINT`), строки, числа,
  комментарии `//`, операторы.
- ✅ Сниппеты (`snippets/sga.code-snippets`) — вставляют код с настоящими
  SGA-кодпоинтами (let/var/fn/if/ifelse/while/for/print/return).
- ✅ `language-configuration.json` — автозакрытие скобок/кавычек, комментарии.
- ✅ **Реальный шрифт с нарисованными рунами** (`fonts/FairfaxHD.ttf`,
  лицензия SIL OFL-1.1, см. `fonts/NOTICE.md` для атрибуции) — после
  установки в систему руны зачарования отображаются по-настоящему, а не
  как "тофу"-прямоугольники. `package.json` автоматически выставляет этот
  шрифт для `.sga`-файлов через `configurationDefaults`.

## Как увидеть настоящие руны (один разовый шаг)

VS Code не подключает шрифты из расширений автоматически в текстовый
редактор (в отличие от веб-страниц) — нужно установить TTF в систему один раз:

```bash
# Linux
mkdir -p ~/.local/share/fonts
cp fonts/FairfaxHD.ttf ~/.local/share/fonts/
fc-cache -f
```

На Windows/macOS — просто дважды кликните `fonts/FairfaxHD.ttf` и нажмите
"Установить". После этого откройте любой `.sga`-файл — шрифт подключится
автоматически (настройка уже прописана в `package.json`).

## НЕ реализовано в v0.1

- ❌ Автодополнение (completion provider)
- ❌ Hover-подсказки
- ❌ Go to definition
- ❌ Linting / diagnostics
- ❌ Formatter (есть только `sga fmt`, печатающий AST в основном CLI, не интегрированный с VS Code)

Все перечисленные пункты требуют language server (LSP) — отдельного
процесса с анализом проекта в реальном времени. Это отдельный
инженерный трек, см. `docs/ROADMAP.md` в корне репозитория.

## Происхождение шрифта и алфавита

Standard Galactic Alphabet — конструированная письменность Tom Hall
(Commander Keen, 1990), которую Minecraft переиспользовал в 2011 как
шрифт зачарований. Кодпоинты в этом расширении — официально
зарегистрированный блок UCSUR (Under-ConScript Unicode Registry), а не
произвольная самодельная схема. Шрифт `FairfaxHD.ttf` — независимая
работа Kreative Software под OFL-1.1, **не извлечена из файлов игры
Minecraft**. Подробности и ссылки — `fonts/NOTICE.md` и
`../docs/SGA_ALPHABET.md`.

## Установка (для разработки)

```bash
cd vscode-extension
# скопировать в ~/.vscode/extensions/ или открыть как папку расширения
# в VS Code: Run Extension (F5) из этой директории при наличии vsce/yo code
```

Публикация в Marketplace и упаковка `.vsix` не выполнялись в рамках v0.1.
