//! Таблица Standard Galactic Alphabet (SGA) — алфавит зачарований Minecraft,
//! изначально созданный Tom Hall для серии Commander Keen (1990).
//!
//! Кодпоинты — это НЕ самодельная схема: используется официально
//! зарегистрированный блок Under-ConScript Unicode Registry (UCSUR)
//! "Standard Galactic", U+EB40..U+EB5F:
//! <https://www.kreativekorp.com/ucsur/charts/sga.html>
//! (proposal автор Rebecca Bettencourt, 2019-09-14).
//!
//! Это важно: используя зарегистрированный диапазон, а не произвольные
//! PUA-кодпоинты, SGA Programming Language совместим с уже существующим
//! шрифтом **Fairfax HD** (Kreative Software, лицензия SIL OFL-1.1,
//! <https://github.com/kreativekorp/open-relay>), который реально
//! отображает эти кодпоинты как иероглифы зачарования — см.
//! `vscode-extension/fonts/` и `docs/SGA_ALPHABET.md`.
//!
//! ВАЖНО: SGA-кодпоинты используются ТОЛЬКО для ключевых слов языка.
//! Идентификаторы, числа, строки и операторы — обычный ASCII/UTF-8.
//! Это осознанное архитектурное решение (см. docs/LANGUAGE_SPEC.md, §2).

pub const SGA_BASE: u32 = 0xEB40;

/// Возвращает SGA-кодпоинт для латинской буквы A-Z (case-insensitive).
pub fn letter_to_sga(c: char) -> Option<char> {
    let c = c.to_ascii_uppercase();
    if c.is_ascii_uppercase() {
        let offset = (c as u32) - ('A' as u32);
        char::from_u32(SGA_BASE + offset)
    } else {
        None
    }
}

/// Обратное преобразование: SGA-кодпоинт -> латинская буква.
pub fn sga_to_letter(c: char) -> Option<char> {
    let cp = c as u32;
    if (SGA_BASE..SGA_BASE + 26).contains(&cp) {
        Some((b'A' + (cp - SGA_BASE) as u8) as char)
    } else {
        None
    }
}

/// Проверка, что символ принадлежит диапазону SGA-букв.
pub fn is_sga_letter(c: char) -> bool {
    sga_to_letter(c).is_some()
}

/// Кодирует обычную ASCII-строку (мнемонику ключевого слова) в строку
/// SGA-кодпоинтов. Используется тулом `tools/translit` и при генерации
/// примеров — НЕ используется во время лексического анализа.
pub fn encode_word(word: &str) -> String {
    word.chars().filter_map(letter_to_sga).collect()
}

/// Декодирует строку SGA-кодпоинтов обратно в ASCII-мнемонику.
/// Используется лексером для распознавания ключевых слов.
pub fn decode_word(word: &str) -> String {
    word.chars().filter_map(sga_to_letter).collect()
}
