//! Перевод позиций лексера в позиции LSP (T016, M007).
//!
//! Лексер считает строки с единицы (по `\n`), а колонку — с единицы и в
//! СИМВОЛАХ Unicode. LSP считает строки с нуля, а `character` — с нуля и в
//! ЕДИНИЦАХ UTF-16 (если клиент не договорился о другой кодировке; сервер
//! её не согласует, см. docs/LSP.md). Различие существенно: буквы SGA
//! (U+EB40..U+EB59, BMP) — одна единица UTF-16, а символ вне BMP (например,
//! эмодзи в строковой константе) — две, и без пересчёта подчёркивание после
//! такого символа съезжало бы.

/// Позиция в терминах LSP: обе координаты с нуля.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

fn clamp_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Переводит позицию `(line, col)` лексера (1-based, колонка в символах) в
/// позицию LSP в тексте `text`.
///
/// Строки — по `\n`, как в лексере; `\r` перед `\n` остаётся в строке
/// (клиент отсекает `character` за концом строки сам). Позиции за пределами
/// текста прижимаются к его концу: колонка за концом строки — к концу этой
/// строки, строка за концом текста — к концу последней строки.
pub fn to_lsp(text: &str, line: usize, col: usize) -> Position {
    let wanted = line.saturating_sub(1);
    let mut index = 0;
    let mut content = "";
    let mut found = false;
    for (i, l) in text.split('\n').enumerate() {
        index = i;
        content = l;
        if i == wanted {
            found = true;
            break;
        }
    }
    let character = if found {
        content
            .chars()
            .take(col.saturating_sub(1))
            .map(char::len_utf16)
            .sum()
    } else {
        utf16_len(content)
    };
    Position {
        line: clamp_u32(index),
        character: clamp_u32(character),
    }
}

/// Таблица строк документа для массового перевода позиций (T019).
///
/// [`to_lsp`] просматривает текст с начала при каждом вызове: для одной
/// позиции (подсказка, переход к определению) этого достаточно, а
/// `textDocument/documentSymbol` на документе в 1 МиБ переводит десятки тысяч
/// позиций, и квадратичная работа заняла бы минуты. Таблица строится за один
/// проход по тексту, после чего каждый перевод — O(1). Ответы совпадают с
/// [`to_lsp`] во всех случаях (дифференциальный тест ниже).
#[derive(Debug, Clone)]
pub struct LineTable {
    lines: Vec<LineInfo>,
}

#[derive(Debug, Clone)]
struct LineInfo {
    /// Число символов в строке (без `\n`).
    chars: usize,
    /// Единицы UTF-16 в первых `k` символах — `units[k]`; только у строк с
    /// символами вне BMP (у остальных единиц ровно столько же, сколько символов).
    units: Option<Box<[u32]>>,
}

impl LineInfo {
    /// Единицы UTF-16 в первых `chars` символах строки; `chars` за концом строки
    /// прижимается к её концу.
    fn units_before(&self, chars: usize) -> usize {
        let k = chars.min(self.chars);
        match &self.units {
            Some(units) => units[k] as usize,
            None => k,
        }
    }
}

impl LineTable {
    pub fn new(text: &str) -> LineTable {
        let lines = text
            .split('\n')
            .map(|line| {
                let mut chars = 0usize;
                let mut astral = false;
                for c in line.chars() {
                    chars += 1;
                    astral |= c.len_utf16() == 2;
                }
                let units = astral.then(|| {
                    let mut sums = Vec::with_capacity(chars + 1);
                    let mut total = 0u32;
                    sums.push(0);
                    for c in line.chars() {
                        // как у `clamp_u32` в `to_lsp`: сверх `u32::MAX` не считаем
                        total = total.saturating_add(c.len_utf16() as u32);
                        sums.push(total);
                    }
                    sums.into_boxed_slice()
                });
                LineInfo { chars, units }
            })
            .collect();
        LineTable { lines }
    }

    /// То же, что [`to_lsp`] для текста, по которому построена таблица.
    pub fn to_lsp(&self, line: usize, col: usize) -> Position {
        let wanted = line.saturating_sub(1);
        match self.lines.get(wanted) {
            Some(info) => Position {
                line: clamp_u32(wanted),
                character: clamp_u32(info.units_before(col.saturating_sub(1))),
            },
            None => {
                // `split` даёт хотя бы одну строку, даже у пустого текста
                let last = self.lines.len().saturating_sub(1);
                Position {
                    line: clamp_u32(last),
                    character: clamp_u32(self.lines[last].units_before(usize::MAX)),
                }
            }
        }
    }
}

/// Обратное преобразование (T018, M007): позиция LSP — строка с нуля,
/// `character` с нуля в единицах UTF-16 — в позицию лексера: `(строка, колонка)`
/// с единицы, колонка в СИМВОЛАХ. Нужно запросам, которые клиент задаёт по
/// позиции курсора (`textDocument/hover`, `textDocument/completion`).
///
/// Прижимает так же, как [`to_lsp`]: `character` за концом строки — к концу
/// этой строки, строка за концом текста — к концу последней строки.
/// `character` внутри суррогатной пары (посреди символа вне BMP) округляется
/// ВНИЗ — до начала этого символа: курсор не может стоять посреди символа, а
/// округление вверх сдвинуло бы его за символ, которого он не касался.
pub fn from_lsp(text: &str, line: u32, character: u32) -> (usize, usize) {
    let wanted = usize::try_from(line).unwrap_or(usize::MAX);
    let mut index = 0;
    let mut content = "";
    let mut found = false;
    for (i, l) in text.split('\n').enumerate() {
        index = i;
        content = l;
        if i == wanted {
            found = true;
            break;
        }
    }
    let character = if found {
        usize::try_from(character).unwrap_or(usize::MAX)
    } else {
        usize::MAX
    };
    let mut units = 0usize;
    let mut chars = 0usize;
    for c in content.chars() {
        let width = c.len_utf16();
        if units.saturating_add(width) > character {
            break;
        }
        units += width;
        chars += 1;
    }
    (index + 1, chars + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, line: usize, col: usize) -> (u32, u32) {
        let p = to_lsp(text, line, col);
        (p.line, p.character)
    }

    fn back(text: &str, line: u32, character: u32) -> (usize, usize) {
        from_lsp(text, line, character)
    }

    #[test]
    fn from_lsp_shifts_ascii_positions_to_one_based() {
        assert_eq!(back("abc def", 0, 0), (1, 1));
        assert_eq!(back("abc def", 0, 4), (1, 5));
        assert_eq!(back("ab\ncd\nef", 2, 1), (3, 2));
        assert_eq!(back("ab\ncd\nef", 1, 0), (2, 1));
    }

    #[test]
    fn from_lsp_counts_sga_letters_as_one_unit() {
        let text = "\u{EB40}\u{EB41}\u{EB42} x";
        assert_eq!(back(text, 0, 3), (1, 4));
        assert_eq!(back(text, 0, 4), (1, 5));
    }

    #[test]
    fn from_lsp_counts_astral_characters_as_two_units() {
        let text = "😀x";
        assert_eq!(back(text, 0, 0), (1, 1));
        assert_eq!(back(text, 0, 2), (1, 2), "x стоит после эмодзи");
        assert_eq!(back(text, 0, 3), (1, 3), "конец строки");
        let text = "я😀😀я";
        assert_eq!(back(text, 0, 1 + 2 + 2), (1, 4));
        assert_eq!(back(text, 0, 1 + 2 + 2 + 1), (1, 5));
    }

    #[test]
    fn from_lsp_rounds_down_inside_a_surrogate_pair() {
        // character 1 — между двумя единицами одного эмодзи
        assert_eq!(back("😀x", 0, 1), (1, 1));
        assert_eq!(back("я😀😀я", 0, 2), (1, 2), "внутри первого эмодзи");
        assert_eq!(back("я😀😀я", 0, 4), (1, 3), "внутри второго эмодзи");
    }

    #[test]
    fn from_lsp_clamps_a_column_past_the_end_of_line() {
        assert_eq!(back("ab\ncd", 0, 99), (1, 3));
        assert_eq!(back("😀", 0, 99), (1, 2));
        assert_eq!(back("ab\ncd", 0, u32::MAX), (1, 3));
    }

    #[test]
    fn from_lsp_clamps_a_line_past_the_end_of_text() {
        assert_eq!(back("ab\ncd", 99, 0), (2, 3));
        assert_eq!(back("😀", 5, 5), (1, 2));
        assert_eq!(back("", 0, 0), (1, 1));
        assert_eq!(back("", 5, 5), (1, 1));
        assert_eq!(
            back("ab\ncd\n", u32::MAX, u32::MAX),
            (3, 1),
            "пустая последняя строка"
        );
    }

    #[test]
    fn from_lsp_keeps_carriage_return_inside_the_line() {
        let text = "ab\r\ncd";
        assert_eq!(back(text, 1, 1), (2, 2));
        assert_eq!(back(text, 0, 2), (1, 3), "перед \\r");
        assert_eq!(back(text, 0, 3), (1, 4), "после \\r");
        assert_eq!(back(text, 0, 50), (1, 4), "дальше — конец строки");
    }

    #[test]
    fn from_lsp_is_the_inverse_of_to_lsp_for_every_position_in_the_text() {
        let corpus = [
            "",
            "a",
            "abc def",
            "ab\ncd\nef",
            "ab\ncd\n",
            "\n\n",
            "\u{EB40}\u{EB41}\u{EB42} x\n\u{EB4B}\u{EB44}\u{EB53}",
            "😀x\nя😀😀я\r\n\t\tz",
            "tab\there\r\nline",
        ];
        for text in corpus {
            let lines: Vec<&str> = text.split('\n').collect();
            for (i, l) in lines.iter().enumerate() {
                let width = l.chars().count();
                for col in 1..=width + 1 {
                    let p = to_lsp(text, i + 1, col);
                    assert_eq!(
                        from_lsp(text, p.line, p.character),
                        (i + 1, col),
                        "text {text:?}, position {}:{col}",
                        i + 1
                    );
                }
            }
        }
    }

    #[test]
    fn from_lsp_never_panics_on_extreme_arguments() {
        for text in ["", "a", "😀\nb", "\n"] {
            for line in [0, 1, 2, u32::MAX] {
                for character in [0, 1, 2, 3, u32::MAX] {
                    let (l, c) = from_lsp(text, line, character);
                    assert!(l >= 1 && c >= 1);
                }
            }
        }
    }

    #[test]
    fn ascii_positions_are_shifted_to_zero_based() {
        assert_eq!(at("abc def", 1, 1), (0, 0));
        assert_eq!(at("abc def", 1, 5), (0, 4));
        assert_eq!(at("ab\ncd\nef", 3, 2), (2, 1));
        assert_eq!(at("ab\ncd\nef", 2, 1), (1, 0));
    }

    #[test]
    fn sga_letters_are_one_utf16_unit_each() {
        // U+EB40..U+EB59 лежат в BMP: символов столько же, сколько единиц
        let text = "\u{EB40}\u{EB41}\u{EB42} x";
        assert_eq!(at(text, 1, 4), (0, 3));
        assert_eq!(at(text, 1, 5), (0, 4));
    }

    #[test]
    fn astral_character_before_the_position_counts_as_two_units() {
        // `😀` — один символ лексера (колонка), но две единицы UTF-16
        let text = "😀x";
        assert_eq!(at(text, 1, 1), (0, 0));
        assert_eq!(at(text, 1, 2), (0, 2), "x стоит после эмодзи: 2 единицы");
        assert_eq!(at(text, 1, 3), (0, 3), "конец строки: 2 + 1");
        // два эмодзи подряд и не-BMP посреди строки с кириллицей
        let text = "я😀😀я";
        assert_eq!(at(text, 1, 5), (0, 1 + 2 + 2 + 1));
        assert_eq!(at(text, 1, 4), (0, 1 + 2 + 2));
    }

    #[test]
    fn astral_character_on_an_earlier_line_does_not_shift_later_lines() {
        let text = "😀😀\nabc";
        assert_eq!(at(text, 2, 3), (1, 2));
    }

    #[test]
    fn tab_is_one_unit() {
        assert_eq!(at("\t\tx", 1, 3), (0, 2));
    }

    #[test]
    fn crlf_documents_keep_carriage_return_inside_the_line() {
        let text = "ab\r\ncd";
        assert_eq!(at(text, 2, 2), (1, 1));
        assert_eq!(at(text, 1, 3), (0, 2), "перед \\r");
        assert_eq!(
            at(text, 1, 4),
            (0, 3),
            "после \\r: клиент обрежет до конца строки"
        );
    }

    #[test]
    fn last_line_without_trailing_newline() {
        let text = "ab\ncd";
        assert_eq!(at(text, 2, 3), (1, 2), "сразу после последнего символа");
    }

    #[test]
    fn position_after_the_final_newline_is_an_empty_last_line() {
        // `Eof` после завершающего перевода строки: строка 3, колонка 1
        assert_eq!(at("ab\ncd\n", 3, 1), (2, 0));
        assert_eq!(at("\n", 2, 1), (1, 0));
    }

    #[test]
    fn column_past_the_end_of_line_is_clamped_to_it() {
        assert_eq!(at("ab\ncd", 1, 99), (0, 2));
        assert_eq!(at("😀", 1, 99), (0, 2));
    }

    #[test]
    fn line_past_the_end_of_text_is_clamped_to_its_end() {
        assert_eq!(at("ab\ncd", 99, 1), (1, 2));
        assert_eq!(at("😀", 5, 5), (0, 2));
        assert_eq!(at("", 1, 1), (0, 0));
        assert_eq!(at("", 5, 5), (0, 0));
    }

    #[test]
    fn zero_arguments_do_not_panic() {
        assert_eq!(at("abc", 0, 0), (0, 0));
        assert_eq!(at("abc", 1, 0), (0, 0));
    }

    #[test]
    fn the_line_table_agrees_with_to_lsp_on_every_small_text_and_position() {
        // все тексты до пяти символов из алфавита, где есть ASCII, кириллица,
        // буква SGA, символ вне BMP, `\n` и `\r`
        let alphabet = ['a', 'я', '\u{EB40}', '😀', '\n', '\r'];
        let mut texts = vec![String::new()];
        let mut layer = vec![String::new()];
        for _ in 0..5 {
            let mut next = Vec::new();
            for base in &layer {
                for c in alphabet {
                    let mut text = base.clone();
                    text.push(c);
                    next.push(text);
                }
            }
            texts.extend(next.iter().cloned());
            layer = next;
        }
        assert_eq!(texts.len(), 1 + 6 + 36 + 216 + 1296 + 7776);
        let coordinates = [0, 1, 2, 3, 4, 5, 6, 99, usize::MAX];
        for text in &texts {
            let table = LineTable::new(text);
            for line in coordinates {
                for col in coordinates {
                    assert_eq!(
                        table.to_lsp(line, col),
                        to_lsp(text, line, col),
                        "text {text:?}, position {line}:{col}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_line_table_handles_a_long_line_with_astral_characters_in_constant_time() {
        // 100 000 эмодзи в одной строке: перевод каждой позиции — по таблице
        let text = "😀".repeat(100_000);
        let table = LineTable::new(&text);
        assert_eq!(
            table.to_lsp(1, 1),
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            table.to_lsp(1, 50_001),
            Position {
                line: 0,
                character: 100_000
            }
        );
        assert_eq!(
            table.to_lsp(1, 100_001),
            Position {
                line: 0,
                character: 200_000
            }
        );
        assert_eq!(
            table.to_lsp(7, 7),
            Position {
                line: 0,
                character: 200_000
            },
            "строка за концом текста — конец последней строки"
        );
    }
}
