//! Минимальный JSON-слой языкового сервера (T016, M007).
//!
//! Нужен для JSON-RPC: `null`, `id` как целое ИЛИ строка, порядок ключей и
//! различие целых/дробных чисел обязаны сохраняться. `crate::json` (T008,
//! `std/json` для программ SGA) для этого не годится — он отображает JSON в
//! `runtime::Value` (объект превращается в массив пар) и имеет свои пределы
//! под программы SGA; внешних зависимостей в проекте нет, поэтому у LSP свой
//! небольшой слой (дублирование осознанное, см. TD-017).
//!
//! Разбор соответствует RFC 8259 и строже привычного: ведущие нули, `+1`,
//! `.5`, `1.`, голые управляющие символы в строках, одиночные суррогаты и
//! число, не представимое конечным `f64`, отклоняются. Вложенность
//! ограничена (`MAX_DEPTH`) — разбор нельзя обрушить длинной цепочкой `[`.

use std::fmt;

/// Максимальная вложенность массивов/объектов. Глубже — ошибка разбора.
pub const MAX_DEPTH: usize = 128;

/// JSON-значение. Объект — упорядоченный список пар (порядок ключей из
/// входа сохраняется, дубли не склеиваются).
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Число без дробной части и экспоненты, поместившееся в `i64`.
    Int(i64),
    /// Остальные числа; всегда конечное (`NaN`/`±inf` не создаются разбором).
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Ошибка разбора: описание и смещение в байтах от начала входа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (байт {})", self.message, self.offset)
    }
}

impl std::error::Error for JsonError {}

impl Json {
    /// Значение по ключу в объекте. При дубликатах ключа возвращает
    /// ПОСЛЕДНЕЕ вхождение (как `JSON.parse`). Не объект — `None`.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(pairs) => pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Объект из пар «ключ — значение» (порядок сохраняется).
    pub fn object<const N: usize>(pairs: [(&str, Json); N]) -> Json {
        Json::Object(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    pub fn string(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Float(x) => write_float(*x, out),
            Json::Str(s) => write_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Сериализация в компактный JSON (без пробелов).
impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.write(&mut out);
        f.write_str(&out)
    }
}

/// `NaN` и `±inf` в JSON непредставимы — пишутся как `null`. Конечное число
/// всегда содержит `.` (иначе `1.0` вернулось бы из разбора как `Int(1)`).
fn write_float(x: f64, out: &mut String) {
    if !x.is_finite() {
        out.push_str("null");
        return;
    }
    // `Display` для f64 никогда не использует экспоненциальную запись и
    // выдаёт кратчайшее представление, которое читается обратно точно.
    let text = x.to_string();
    out.push_str(&text);
    if !text.contains('.') {
        out.push_str(".0");
    }
}

/// Экранируются `"`, `\` и управляющие символы (< 0x20); всё остальное
/// (кириллица, символы вне BMP, `/`, DEL) пишется как есть.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Разбирает ровно одно JSON-значение; кроме пробельных символов после него
/// ничего быть не может.
pub fn parse(text: &str) -> Result<Json, JsonError> {
    let mut parser = Parser { src: text, pos: 0 };
    parser.skip_ws();
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != text.len() {
        return parser.err("лишние данные после JSON-значения");
    }
    Ok(value)
}

struct Parser<'a> {
    src: &'a str,
    /// Смещение в байтах; всегда на границе символа, когда им пользуются
    /// для среза (двигается только по ASCII-байтам структуры или внутри
    /// строки до `"`/`\`).
    pos: usize,
}

impl Parser<'_> {
    fn err<T>(&self, message: &str) -> Result<T, JsonError> {
        Err(JsonError {
            message: message.to_string(),
            offset: self.pos,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, JsonError> {
        match self.peek() {
            None => self.err("неожиданный конец ввода"),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_object(depth),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => self.err("неожиданный символ"),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, JsonError> {
        // `pos` стоит на ASCII-байте (`n`/`t`/`f`) — срез безопасен.
        if self.src[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            self.err("неизвестный литерал")
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth >= MAX_DEPTH {
            return self.err("превышена максимальная вложенность JSON");
        }
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return self.err("ожидалась ',' или ']'"),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth >= MAX_DEPTH {
            return self.err("превышена максимальная вложенность JSON");
        }
        self.pos += 1; // {
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return self.err("ожидалась строка-ключ");
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return self.err("ожидался ':' после ключа");
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.parse_value(depth + 1)?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(pairs));
                }
                _ => return self.err("ожидалась ',' или '}'"),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.skip_digits(),
            _ => return self.err("ожидалась цифра"),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("ожидалась цифра после '.'");
            }
            self.skip_digits();
            is_float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("ожидалась цифра в экспоненте");
            }
            self.skip_digits();
            is_float = true;
        }
        let text = &self.src[start..self.pos];
        if !is_float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Json::Int(n));
            }
        }
        match text.parse::<f64>() {
            Ok(x) if x.is_finite() => Ok(Json::Float(x)),
            _ => Err(JsonError {
                message: "число не представимо конечным f64".to_string(),
                offset: start,
            }),
        }
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    /// Разбирает строку; `pos` стоит на открывающей `"`.
    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.pos += 1;
        let mut out = String::new();
        let mut run_start = self.pos;
        loop {
            match self.peek() {
                None => return self.err("незакрытая строка"),
                Some(b'"') => {
                    out.push_str(&self.src[run_start..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.src[run_start..self.pos]);
                    self.pos += 1;
                    out.push(self.parse_escape()?);
                    run_start = self.pos;
                }
                Some(c) if c < 0x20 => {
                    return self.err("управляющий символ в строке должен быть экранирован")
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Разбирает escape-последовательность; `pos` стоит сразу после `\`.
    fn parse_escape(&mut self) -> Result<char, JsonError> {
        let Some(c) = self.peek() else {
            return self.err("незакрытая escape-последовательность");
        };
        self.pos += 1;
        Ok(match c {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{08}',
            b'f' => '\u{0C}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.parse_unicode_escape(),
            _ => {
                self.pos -= 1;
                return self.err("недопустимая escape-последовательность");
            }
        })
    }

    /// `\uXXXX` с уже прочитанным `\u`; суррогатная пара — один символ.
    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let first = self.parse_hex4()?;
        let code = match first {
            0xD800..=0xDBFF => {
                if !self.src[self.pos..].starts_with("\\u") {
                    return self.err("одиночный старший суррогат");
                }
                self.pos += 2;
                let second = self.parse_hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return self.err("за старшим суррогатом должен следовать младший");
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return self.err("одиночный младший суррогат"),
            other => other,
        };
        match char::from_u32(code) {
            Some(c) => Ok(c),
            None => self.err("недопустимая кодовая точка"),
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let bytes = self.src.as_bytes();
        let mut value = 0u32;
        for i in 0..4 {
            let digit = bytes
                .get(self.pos + i)
                .and_then(|b| (*b as char).to_digit(16));
            match digit {
                Some(d) => value = value * 16 + d,
                None => {
                    self.pos += i;
                    return self.err("ожидались 4 шестнадцатеричные цифры после \\u");
                }
            }
        }
        self.pos += 4;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Json {
        parse(text).unwrap_or_else(|e| panic!("{text:?} должен разбираться: {e}"))
    }

    fn bad(text: &str) -> JsonError {
        match parse(text) {
            Ok(v) => panic!("{text:?} должен отклоняться, получено {v:?}"),
            Err(e) => e,
        }
    }

    // ── разбор: допустимое ───────────────────────────────────────────

    #[test]
    fn literals_and_surrounding_whitespace() {
        assert_eq!(ok("null"), Json::Null);
        assert_eq!(ok("true"), Json::Bool(true));
        assert_eq!(ok("false"), Json::Bool(false));
        assert_eq!(ok(" \t\r\n null \n"), Json::Null);
    }

    #[test]
    fn integers_including_i64_bounds() {
        assert_eq!(ok("0"), Json::Int(0));
        assert_eq!(ok("-0"), Json::Int(0));
        assert_eq!(ok("42"), Json::Int(42));
        assert_eq!(ok("-17"), Json::Int(-17));
        assert_eq!(ok("9223372036854775807"), Json::Int(i64::MAX));
        assert_eq!(ok("-9223372036854775808"), Json::Int(i64::MIN));
    }

    #[test]
    fn integers_outside_i64_become_floats() {
        assert_eq!(
            ok("9223372036854775808"),
            Json::Float(9223372036854775808.0)
        );
        assert_eq!(
            ok("-9223372036854775809"),
            Json::Float(-9223372036854775809.0)
        );
        assert_eq!(ok("100000000000000000000000"), Json::Float(1e23));
    }

    #[test]
    fn fractions_and_exponents_are_floats() {
        assert_eq!(ok("1.5"), Json::Float(1.5));
        assert_eq!(ok("-0.25"), Json::Float(-0.25));
        assert_eq!(ok("0.0"), Json::Float(0.0));
        assert_eq!(ok("1e3"), Json::Float(1000.0));
        assert_eq!(ok("1E+3"), Json::Float(1000.0));
        assert_eq!(ok("2.5e-3"), Json::Float(0.0025));
        assert_eq!(ok("1e0"), Json::Float(1.0));
    }

    #[test]
    fn strings_with_all_escapes_and_raw_utf8() {
        assert_eq!(ok(r#""""#), Json::string(""));
        assert_eq!(ok(r#""abc""#), Json::string("abc"));
        assert_eq!(
            ok(r#""\" \\ \/ \b \f \n \r \t""#),
            Json::string("\" \\ / \u{08} \u{0C} \n \r \t")
        );
        assert_eq!(ok(r#""\u0041\u00e9\u0000""#), Json::string("Aé\0"));
        assert_eq!(ok(r#""\u00E9""#), Json::string("é"));
        assert_eq!(
            ok("\"привет 😀 \u{EB40}\""),
            Json::string("привет 😀 \u{EB40}")
        );
    }

    #[test]
    fn surrogate_pair_becomes_one_astral_character() {
        assert_eq!(ok(r#""\ud83d\ude00""#), Json::string("😀"));
        assert_eq!(ok(r#""\uD83D\uDE00x""#), Json::string("😀x"));
        assert_eq!(ok(r#""\udbff\udfff""#), Json::string("\u{10FFFF}"));
    }

    #[test]
    fn nested_containers_preserve_order() {
        let v = ok(r#" {"b": [1, {"x": null}, []], "a": {}, "c": [true, "s"]} "#);
        let Json::Object(pairs) = &v else {
            panic!("ожидался объект")
        };
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["b", "a", "c"], "порядок ключей должен сохраняться");
        assert_eq!(
            v.get("b"),
            Some(&Json::Array(vec![
                Json::Int(1),
                Json::object([("x", Json::Null)]),
                Json::Array(vec![]),
            ]))
        );
        assert_eq!(v.get("a"), Some(&Json::Object(vec![])));
    }

    #[test]
    fn duplicate_keys_are_kept_and_get_returns_the_last() {
        let v = ok(r#"{"k": 1, "k": 2, "other": 0, "k": 3}"#);
        let Json::Object(pairs) = &v else {
            panic!("ожидался объект")
        };
        assert_eq!(pairs.len(), 4, "дубли не склеиваются");
        assert_eq!(v.get("k"), Some(&Json::Int(3)));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn accessors() {
        let v = ok(r#"{"s": "x", "n": 7, "a": [1], "z": null, "f": 1.5}"#);
        assert_eq!(v.get("s").and_then(Json::as_str), Some("x"));
        assert_eq!(v.get("n").and_then(Json::as_i64), Some(7));
        assert_eq!(
            v.get("f").and_then(Json::as_i64),
            None,
            "дробное — не целое"
        );
        assert_eq!(v.get("s").and_then(Json::as_i64), None);
        assert_eq!(
            v.get("a").and_then(Json::as_array),
            Some(&[Json::Int(1)][..])
        );
        assert!(v.get("z").is_some_and(Json::is_null));
        assert!(!Json::Int(0).is_null());
        assert_eq!(Json::Int(1).get("x"), None, "get на не-объекте");
    }

    // ── разбор: отклоняемое ──────────────────────────────────────────

    #[test]
    fn empty_and_truncated_input_is_rejected() {
        for text in [
            "", "   ", "\n", "nul", "tru", "fals", "nulll", "[", "[1", "[1,", "{", "{\"a\"",
            "{\"a\":", "{\"a\":1",
        ] {
            bad(text);
        }
    }

    #[test]
    fn trailing_garbage_is_rejected_with_its_offset() {
        assert_eq!(bad("1 2").offset, 2);
        assert_eq!(bad("{} x").offset, 3);
        for text in ["[]]", "null null", "truefalse", "1,", "{}{}"] {
            bad(text);
        }
    }

    #[test]
    fn malformed_containers_are_rejected() {
        for text in [
            "[1,]",
            "[,1]",
            "[1 2]",
            "[1,,2]",
            "{\"a\":1,}",
            "{,}",
            "{\"a\"}",
            "{\"a\":}",
            "{1:2}",
            "{'a':1}",
            "{\"a\" 1}",
            "{\"a\":1 \"b\":2}",
            "[}",
            "{]",
        ] {
            bad(text);
        }
    }

    #[test]
    fn malformed_numbers_are_rejected() {
        for text in [
            "01",
            "-01",
            "+1",
            ".5",
            "1.",
            "1.e5",
            "1e",
            "1e+",
            "1E-",
            "-",
            "--1",
            "0x10",
            "Infinity",
            "-Infinity",
            "NaN",
            "1_000",
            "1,5",
            "٣",
        ] {
            bad(text);
        }
    }

    #[test]
    fn missing_digits_after_a_point_or_in_an_exponent_are_reported_as_such() {
        // Причина — в сообщении: проверка «число конечно» дала бы для этих
        // входов невнятное «не представимо конечным f64».
        for text in ["1.", "1.e5", "[1.]", "2.x"] {
            let e = bad(text);
            assert!(e.message.contains("после '.'"), "{text:?}: {e}");
        }
        for text in ["1e", "1E", "1e+", "1E-", "2.5e", "[1e]", "1ex"] {
            let e = bad(text);
            assert!(e.message.contains("в экспоненте"), "{text:?}: {e}");
        }
    }

    #[test]
    fn numbers_that_are_not_finite_f64_are_rejected() {
        let e = bad("1e999");
        assert!(e.message.contains("f64"), "{e}");
        bad("-1e999");
        bad(&format!("1{}", "0".repeat(400)));
    }

    #[test]
    fn malformed_strings_are_rejected() {
        for text in [
            "\"abc",              // не закрыта
            "\"a\nb\"",           // голый перевод строки
            "\"a\tb\"",           // голая табуляция
            "\"a\u{1}b\"",        // голый управляющий символ
            "\"\\x\"",            // неизвестный escape
            "\"\\",               // обрыв после обратной косой
            "\"\\u12\"",          // короткая \u
            "\"\\u12G4\"",        // не-hex цифра
            "\"\\u12",            // обрыв внутри \u
            "\"\\ud83d\"",        // одиночный старший суррогат
            "\"\\ud83dx\"",       // старший суррогат без пары
            "\"\\ude00\"",        // одиночный младший суррогат
            "\"\\ud83d\\u0041\"", // старший + не младший
            "\"\\ud83d\\ud83d\"", // два старших
            "'single'",
        ] {
            bad(text);
        }
    }

    #[test]
    fn non_json_whitespace_and_comments_are_rejected() {
        for text in ["\u{FEFF}1", "\u{00A0}1", "// c\n1", "/* c */ 1", "1 // c"] {
            bad(text);
        }
    }

    #[test]
    fn nesting_up_to_the_limit_is_accepted_and_deeper_is_rejected() {
        let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        ok(&nested(MAX_DEPTH));
        let e = bad(&nested(MAX_DEPTH + 1));
        assert!(e.message.contains("вложенность"), "{e}");

        let objects = |n: usize| format!("{}1{}", "{\"a\":".repeat(n), "}".repeat(n));
        ok(&objects(MAX_DEPTH));
        bad(&objects(MAX_DEPTH + 1));
    }

    #[test]
    fn absurd_nesting_is_rejected_without_overflowing_the_stack() {
        // 2 МиБ стека — как у потока тестового харнесса
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                bad(&"[".repeat(1_000_000));
                bad(&"{\"a\":".repeat(1_000_000));
                bad(&format!("{}{}", "[".repeat(300_000), "]".repeat(300_000)));
            })
            .expect("поток")
            .join()
            .expect("разбор не должен падать");
    }

    // ── сериализация ─────────────────────────────────────────────────

    #[test]
    fn scalars_serialize() {
        assert_eq!(Json::Null.to_string(), "null");
        assert_eq!(Json::Bool(true).to_string(), "true");
        assert_eq!(Json::Bool(false).to_string(), "false");
        assert_eq!(Json::Int(-5).to_string(), "-5");
        assert_eq!(Json::Int(i64::MIN).to_string(), "-9223372036854775808");
        assert_eq!(Json::Float(1.5).to_string(), "1.5");
    }

    #[test]
    fn floats_always_keep_a_fractional_part_and_non_finite_become_null() {
        assert_eq!(Json::Float(1.0).to_string(), "1.0");
        assert_eq!(Json::Float(-0.0).to_string(), "-0.0");
        assert_eq!(Json::Float(1e21).to_string(), "1000000000000000000000.0");
        assert_eq!(Json::Float(1e-7).to_string(), "0.0000001");
        assert_eq!(Json::Float(f64::NAN).to_string(), "null");
        assert_eq!(Json::Float(f64::INFINITY).to_string(), "null");
        assert_eq!(Json::Float(f64::NEG_INFINITY).to_string(), "null");
    }

    #[test]
    fn strings_escape_quote_backslash_and_control_characters_only() {
        assert_eq!(
            Json::string("a\"b\\c\nd\re\tf\u{08}g\u{0C}h").to_string(),
            r#""a\"b\\c\nd\re\tf\bg\fh""#
        );
        assert_eq!(
            Json::string("\u{01}\u{1f}").to_string(),
            r#""\u0001\u001f""#
        );
        // остальное — как есть: кириллица, символ вне BMP, SGA-буква, `/`, DEL
        assert_eq!(
            Json::string("привет 😀 \u{EB40} / \u{7f}").to_string(),
            "\"привет 😀 \u{EB40} / \u{7f}\""
        );
    }

    #[test]
    fn containers_serialize_compactly_in_order() {
        let v = Json::object([
            (
                "z",
                Json::Array(vec![Json::Int(1), Json::Null, Json::Array(vec![])]),
            ),
            ("a", Json::Object(vec![])),
            ("m", Json::object([("k", Json::string("v"))])),
        ]);
        assert_eq!(v.to_string(), r#"{"z":[1,null,[]],"a":{},"m":{"k":"v"}}"#);
        // ключи тоже экранируются
        assert_eq!(
            Json::object([("a\"b", Json::Null)]).to_string(),
            r#"{"a\"b":null}"#
        );
    }

    // ── круговое преобразование ──────────────────────────────────────

    #[test]
    fn parse_serialize_parse_is_stable_for_tricky_inputs() {
        for text in [
            "null",
            "0",
            "-0",
            "1.0",
            "-0.0",
            "0.1",
            "1e-7",
            "1e21",
            "1.7976931348623157e308",
            "5e-324",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775809",
            r#""\u0000\u001f\"\\\/ \ud83d\ude00 привет""#,
            r#"{"k":1,"k":2,"nested":{"a":[1,2.5,"x",null,true,false,{}]}}"#,
            "[[[[[[[[[[]]]]]]]]]]",
        ] {
            let first = ok(text);
            let again = ok(&first.to_string());
            assert_eq!(first, again, "круговое преобразование для {text:?}");
        }
    }

    /// Случайное дерево (xorshift64, детерминированно): сериализация и
    /// обратный разбор обязаны возвращать то же значение.
    fn random_json(state: &mut u64, depth: usize) -> Json {
        fn next(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }
        let kinds = if depth >= 4 { 6 } else { 8 };
        match next(state) % kinds {
            0 => Json::Null,
            1 => Json::Bool(next(state) & 1 == 0),
            2 => Json::Int(next(state) as i64),
            3 => {
                let x = f64::from_bits(next(state));
                Json::Float(if x.is_finite() { x } else { 0.5 })
            }
            4 => Json::Float((next(state) % 2000) as f64 / 8.0 - 100.0),
            5 => {
                let alphabet = [
                    'a', 'Z', '"', '\\', '\n', '\u{1}', 'я', '😀', '\u{EB40}', '/', ' ',
                ];
                let len = next(state) % 8;
                Json::Str(
                    (0..len)
                        .map(|_| alphabet[(next(state) % alphabet.len() as u64) as usize])
                        .collect(),
                )
            }
            6 => Json::Array(
                (0..next(state) % 4)
                    .map(|_| random_json(state, depth + 1))
                    .collect(),
            ),
            _ => Json::Object(
                (0..next(state) % 4)
                    .map(|i| {
                        (
                            format!("k{}{}", i, next(state) % 3),
                            random_json(state, depth + 1),
                        )
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn random_values_survive_a_serialize_parse_round_trip() {
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        for _ in 0..3000 {
            let value = random_json(&mut state, 0);
            let text = value.to_string();
            let back = parse(&text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert_eq!(back, value, "через текст {text:?}");
        }
    }
}
