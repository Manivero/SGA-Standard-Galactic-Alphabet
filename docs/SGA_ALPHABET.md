# SGA Alphabet — таблица соответствия

## Происхождение (проверено, не предположение)

Standard Galactic Alphabet — это не изобретение Mojang/Minecraft. Это
конструированная письменность (constructed script), созданная **Tom
Hall** для серии игр **Commander Keen** (1990, id Software/Apogee) —
простая подстановочная замена 26 латинских букв на символы. **Mojang**
переиспользовал этот же алфавит в 2011 году как шрифт для текста
зачарований в Minecraft (внутреннее имя в коде игры — `alt`); сам набор
форм букв при этом не принадлежит Mojang — это более старая, независимая
работа Tom Hall.

Источники: KeenWiki (shikadi.net), Minecraft Wiki, Wikipedia "Standard
Galactic Alphabet", Wikidata Q18435048.

## Кодировка: официальный реестр UCSUR, не самодельная схема

Алфавит официально закодирован в Unicode Private Use Area под блоком
**Standard Galactic**, диапазон **U+EB40–U+EB5F**, зарегистрированным
**Under-ConScript Unicode Registry (UCSUR)** — реестром, координирующим
присвоение PUA-диапазонов конструированным письменностям:

- Proposal: Rebecca Bettencourt, 2019-09-14
- Источник: <https://www.kreativekorp.com/ucsur/charts/sga.html>

```
SGA_BASE = U+EB40
sga(letter) = SGA_BASE + (letter - 'A')   для A..Z
EB5A, EB5B, EB5C зарезервированы и НЕ используются (по спецификации UCSUR)
EB5D / EB5E — кавычки SGA, EB5F — точка (не используются в SGA v0.1 — пунктуация языка обычная ASCII)
```

| Латиница | Codepoint | Латиница | Codepoint | Латиница | Codepoint |
|----------|-----------|----------|-----------|----------|-----------|
| A | U+EB40 | J | U+EB49 | S | U+EB52 |
| B | U+EB41 | K | U+EB4A | T | U+EB53 |
| C | U+EB42 | L | U+EB4B | U | U+EB54 |
| D | U+EB43 | M | U+EB4C | V | U+EB55 |
| E | U+EB44 | N | U+EB4D | W | U+EB56 |
| F | U+EB45 | O | U+EB4E | X | U+EB57 |
| G | U+EB46 | P | U+EB4F | Y | U+EB58 |
| H | U+EB47 | Q | U+EB50 | Z | U+EB59 |
| I | U+EB48 | R | U+EB51 |   |        |

Реализация — `src/sga_alphabet.rs` (`letter_to_sga`, `sga_to_letter`,
`encode_word`, `decode_word`), `SGA_BASE = 0xEB40`.

Почему важно использовать именно зарегистрированный диапазон, а не
произвольные PUA-кодпоинты: это делает `.sga`-файлы совместимыми с уже
существующими шрифтами сообщества конланг-энтузиастов, а не только с
шрифтом, написанным специально для этого репозитория.

## Где это применяется

**Только ключевые слова** языка состоят из SGA-кодпоинтов. Идентификаторы
переменных/функций, числовые и строковые литералы, операторы и пунктуация
— обычный ASCII/UTF-8.

## Таблица ключевых слов

| Мнемоника  | SGA-кодпоинты (hex)                                    | Значение |
|------------|---------------------------------------------------------|----------|
| `LET`      | EB4B EB44 EB53                                          | immutable-объявление |
| `VAR`      | EB55 EB40 EB51                                          | mutable-объявление |
| `CONST`    | EB42 EB4E EB4D EB52 EB53                                | константа (immutable) |
| `FN`       | EB45 EB4D                                               | объявление функции |
| `RETURN`   | EB51 EB44 EB53 EB54 EB51 EB4D                           | возврат значения |
| `IF`       | EB48 EB45                                               | условие |
| `ELSE`     | EB44 EB4B EB52 EB44                                     | альтернативная ветка |
| `WHILE`    | EB56 EB47 EB48 EB4B EB44                                | цикл по условию |
| `FOR`      | EB45 EB4E EB51                                          | цикл по диапазону |
| `IN`       | EB48 EB4D                                               | часть `for x in a..b` |
| `TRUE`     | EB53 EB51 EB54 EB44                                     | литерал true |
| `FALSE`    | EB45 EB40 EB4B EB52 EB44                                | литерал false |
| `STRUCT`   | EB52 EB53 EB51 EB54 EB42 EB53                           | (зарезервировано, см. ROADMAP) |
| `PRINT`    | EB4F EB51 EB48 EB4D EB53                                | встроенный вывод |
| `BREAK`    | EB41 EB51 EB44 EB40 EB4A                                | выход из цикла |
| `CONTINUE` | EB42 EB4E EB4D EB53 EB48 EB4D EB54 EB44                 | переход к след. итерации |
| `AND`      | EB40 EB4D EB43                                          | логическое И |
| `OR`       | EB4E EB51                                               | логическое ИЛИ |
| `NOT`      | EB4D EB4E EB53                                          | логическое НЕ |
| `NIL`      | EB4D EB48 EB4B                                          | отсутствие значения |
| `MUT`      | EB4C EB54 EB53                                          | mutable-заимствование параметра функции (Ownership/Borrowing) |

## Настоящий шрифт — теперь подключён

`vscode-extension/fonts/FairfaxHD.ttf` — рабочий шрифт, в котором эти
кодпоинты отрисованы настоящими рунами зачарования (а не "тофу"-
прямоугольниками). Это не наша работа — это шрифт **Fairfax HD** от
**Kreative Software** (автор UCSUR-предложения для SGA), лицензия
**SIL OFL-1.1**, источник
<https://github.com/kreativekorp/open-relay>. Полная атрибуция —
`vscode-extension/fonts/NOTICE.md`. Покрытие глифов U+EB40–U+EB59
проверено программно (`ttf-parser`) — все 26 латинских букв отрисованы.

Инструкция по установке шрифта в систему — `vscode-extension/README.md`.

## Инструмент для написания кода

Поскольку набирать кодпоинты руками неудобно, используйте
`tools/translit`: пишите код с ASCII-мнемониками (`let`, `fn`, `if`, ...)
в файле `*.sga.src`, инструмент заменит только эти слова на настоящие
SGA-кодпоинты, не трогая идентификаторы, строки и числа.
