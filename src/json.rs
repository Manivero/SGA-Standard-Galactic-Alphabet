//! Ручной JSON parser/serializer для `std/json` (T008, M003). См. Feature
//! Contract в PROJECT_STATUS.md.
//!
//! БЕЗ ВНЕШНИХ ЗАВИСИМОСТЕЙ (сознательно): `Cargo.toml` этого проекта не
//! имеет ни одной внешней зависимости (см. INITIAL AUDIT,
//! IMPLEMENTATION_LOG.md) — lexer/parser/VM всего языка написаны с нуля,
//! добавление `serde_json` было бы первой внешней зависимостью за всю
//! историю проекта и нарушило бы установившуюся архитектуру. JSON —
//! синтаксически простой, ограниченный формат (полная грамматика
//! умещается в один экран) — ручная реализация полностью пропорциональна.
//!
//! Соответствие типов (`Value` <-> JSON) — см. Feature Contract:
//! `Nil<->null`, `Bool<->true/false`, `Int`/`Float<->число` (наличие
//! `.`/`e`/`E` решает, во что парсить), `Str<->строка`,
//! `Array<->массив` (рекурсивно), `Struct` (только stringify) `->`
//! JSON-объект из РЕАЛЬНЫХ полей (отсортированных по имени — `fields`
//! хранится в `HashMap`, порядок итерации недетерминирован между
//! запусками, сортировка даёт воспроизводимый вывод), JSON-объект
//! (только parse) `->` `Array` из 2-элементных `Array` `[ключ, значение]`
//! — у SGA v0.1 нет generic map/dict-типа (см. docs/ROADMAP.md), это
//! ближайший честный эквивалент на существующих типах. `Closure` не
//! сериализуется (ошибка).

use crate::runtime::Value;
use std::cell::RefCell;
use std::rc::Rc;

// ============================== stringify ==============================

/// Сериализует `Value` в JSON-строку. `Err` — только для `Closure`
/// (нет осмысленного JSON-представления) или `Float` с NaN/Infinity
/// (не представимы в стандартном JSON).
pub fn stringify(value: &Value) -> Result<String, String> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), String> {
    match value {
        Value::Nil => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                return Err(
                    "json_stringify: NaN/Infinity не представимы в стандартном JSON".to_string(),
                );
            }
            // Rust печатает целые float без ".0" в некоторых случаях? Нет:
            // f64::to_string() всегда даёт хотя бы одну цифру после точки
            // для нецелых, но для 2.0 даёт "2" — добавим ".0" явно, чтобы
            // при парсинге обратно тип восстанавливался как Float, а не Int.
            let s = f.to_string();
            if s.contains('.') || s.contains('e') || s.contains('E') {
                out.push_str(&s);
            } else {
                out.push_str(&s);
                out.push_str(".0");
            }
        }
        Value::Str(s) => write_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.borrow().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Struct { fields, .. } => {
            out.push('{');
            let map = fields.borrow();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort(); // детерминированный вывод — см. doc-комментарий модуля
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_value(&map[*key], out)?;
            }
            out.push('}');
        }
        Value::Closure(_) => {
            return Err("json_stringify: нельзя сериализовать closure/функцию в JSON".to_string());
        }
    }
    Ok(())
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ================================ parse =================================

/// Парсит JSON-строку в `Value`. `Err(сообщение)` включает позицию
/// (символьный индекс) для диагностики — используется как сообщение
/// `RuntimeError` на стороне VM.
pub fn parse(src: &str) -> Result<Value, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut p = JsonParser {
        chars: &chars,
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!(
            "json_parse: лишние данные после корневого значения на позиции {}",
            p.pos
        ));
    }
    Ok(v)
}

struct JsonParser<'a> {
    chars: &'a [char],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<(), String> {
        match self.advance() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(format!(
                "json_parse: ожидался '{}' на позиции {}, получено '{}'",
                expected,
                self.pos - 1,
                c
            )),
            None => Err(format!(
                "json_parse: неожиданный конец строки, ожидался '{}'",
                expected
            )),
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('n') => self.parse_literal("null", Value::Nil),
            Some('t') => self.parse_literal("true", Value::Bool(true)),
            Some('f') => self.parse_literal("false", Value::Bool(false)),
            Some('"') => self.parse_string().map(Value::Str),
            Some('[') => self.parse_array(),
            Some('{') => self.parse_object(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!(
                "json_parse: неожиданный символ '{}' на позиции {}",
                c, self.pos
            )),
            None => Err("json_parse: неожиданный конец строки (ожидалось значение)".to_string()),
        }
    }

    fn parse_literal(&mut self, lit: &str, value: Value) -> Result<Value, String> {
        for expected in lit.chars() {
            self.expect_char(expected)?;
        }
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect_char('"')?;
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err("json_parse: незакрытая строка".to_string()),
                Some('"') => return Ok(s),
                Some('\\') => match self.advance() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some('b') => s.push('\u{0008}'),
                    Some('f') => s.push('\u{000C}'),
                    Some('u') => {
                        let cp = self.parse_hex4()?;
                        // Суррогатные пары (символы вне BMP, \uD800-\uDFFF)
                        // в v0.1 не собираются — редкий случай (эмодзи и
                        // т.п. в \u-escape), честно отклоняется отдельной
                        // ошибкой, а не тихо портится.
                        if (0xD800..=0xDFFF).contains(&cp) {
                            return Err(
                                "json_parse: суррогатные пары \\uXXXX (символы вне BMP) не поддерживаются в v0.1"
                                    .to_string(),
                            );
                        }
                        match char::from_u32(cp) {
                            Some(c) => s.push(c),
                            None => return Err(format!("json_parse: невалидный \\u{:04x}", cp)),
                        }
                    }
                    Some(other) => {
                        return Err(format!("json_parse: неизвестный escape '\\{}'", other))
                    }
                    None => return Err("json_parse: незакрытый escape в конце строки".to_string()),
                },
                Some(c) => s.push(c),
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut cp = 0u32;
        for _ in 0..4 {
            let c = self
                .advance()
                .ok_or_else(|| "json_parse: незавершённый \\u-escape".to_string())?;
            let digit = c
                .to_digit(16)
                .ok_or_else(|| format!("json_parse: невалидная hex-цифра '{}' в \\u-escape", c))?;
            cp = cp * 16 + digit;
        }
        Ok(cp)
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        let mut is_float = false;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        if self.peek() == Some('0') {
            self.pos += 1;
        } else if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        } else {
            return Err(format!(
                "json_parse: ожидалась цифра на позиции {}",
                self.pos
            ));
        }
        if self.peek() == Some('.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!(
                    "json_parse: ожидалась цифра после '.' на позиции {}",
                    self.pos
                ));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!(
                    "json_parse: ожидалась цифра в экспоненте на позиции {}",
                    self.pos
                ));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        if is_float {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|e| format!("json_parse: невалидное число '{}': {}", text, e))
        } else {
            match text.parse::<i64>() {
                Ok(n) => Ok(Value::Int(n)),
                // Целое, но не помещается в i64 (например, гигантский ID) —
                // не ошибка, а честный fallback на Float, симметрично
                // stringify (который тоже разрешает Float без дробной части
                // на выходе быть распарсенным как Int, если влезает).
                Err(_) => text
                    .parse::<f64>()
                    .map(Value::Float)
                    .map_err(|e| format!("json_parse: невалидное число '{}': {}", text, e)),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect_char('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Value::Array(Rc::new(RefCell::new(items))));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.skip_ws();
                    // JSON запрещает висячую запятую перед ']' — явная
                    // проверка вместо того, чтобы поймать это как менее
                    // понятную ошибку "ожидалось значение" на месте ']'.
                    if self.peek() == Some(']') {
                        return Err(format!(
                            "json_parse: висячая запятая перед ']' на позиции {}",
                            self.pos
                        ));
                    }
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                Some(c) => {
                    return Err(format!(
                        "json_parse: ожидался ',' или ']' на позиции {}, получено '{}'",
                        self.pos, c
                    ))
                }
                None => return Err("json_parse: незакрытый массив".to_string()),
            }
        }
        Ok(Value::Array(Rc::new(RefCell::new(items))))
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect_char('{')?;
        // JSON-объект -> Array из 2-элементных Array [ключ, значение] —
        // см. doc-комментарий модуля (у SGA v0.1 нет generic map-типа).
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Array(Rc::new(RefCell::new(pairs))));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect_char(':')?;
            let value = self.parse_value()?;
            let pair = Value::Array(Rc::new(RefCell::new(vec![Value::Str(key), value])));
            pairs.push(pair);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some('}') {
                        return Err(format!(
                            "json_parse: висячая запятая перед '}}' на позиции {}",
                            self.pos
                        ));
                    }
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                Some(c) => {
                    return Err(format!(
                        "json_parse: ожидался ',' или '}}' на позиции {}, получено '{}'",
                        self.pos, c
                    ))
                }
                None => return Err("json_parse: незакрытый объект".to_string()),
            }
        }
        Ok(Value::Array(Rc::new(RefCell::new(pairs))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(v: Value) -> Value {
        parse(&stringify(&v).unwrap()).unwrap()
    }

    #[test]
    fn test_roundtrip_scalars() {
        assert_eq!(rt(Value::Nil), Value::Nil);
        assert_eq!(rt(Value::Bool(true)), Value::Bool(true));
        assert_eq!(rt(Value::Bool(false)), Value::Bool(false));
        assert_eq!(rt(Value::Int(42)), Value::Int(42));
        assert_eq!(rt(Value::Int(-7)), Value::Int(-7));
        assert_eq!(rt(Value::Float(3.25)), Value::Float(3.25));
        assert_eq!(rt(Value::Str("hello".into())), Value::Str("hello".into()));
    }

    #[test]
    fn test_roundtrip_array_nested() {
        let inner = Value::Array(Rc::new(RefCell::new(vec![Value::Int(1), Value::Int(2)])));
        let outer = Value::Array(Rc::new(RefCell::new(vec![
            inner,
            Value::Nil,
            Value::Bool(true),
        ])));
        assert_eq!(rt(outer.clone()), outer);
    }

    #[test]
    fn test_parse_object_becomes_array_of_pairs() {
        let v = parse(r#"{"a": 1, "b": 2}"#).unwrap();
        match v {
            Value::Array(items) => {
                let items = items.borrow();
                assert_eq!(items.len(), 2);
            }
            other => panic!(
                "ожидался Array (представление объекта), получено {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_parse_invalid_json_is_err() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"a": }"#).is_err());
        assert!(parse("[1, 2,]").is_err());
        assert!(parse("").is_err());
    }
}
