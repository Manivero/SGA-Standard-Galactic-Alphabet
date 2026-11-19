//! Согласованность диагностик языкового сервера и ошибок `sga run` на
//! случайных программах (T017, M007).
//!
//! `diagnostics::analyze_source` (LSP) и `run_source` (CLI) приходят к позиции
//! ошибки разными путями: первая берёт диапазон из таблицы границ токенов,
//! построенной лексером при разборе, вторая лексит текст ПОВТОРНО на пути
//! ошибки и печатает `[строка:колонка]`. Тест порождает синтаксически
//! корректные программы со всеми видами конструкций языка, часть из которых
//! намеренно содержит ошибки имён, владения и типов, и требует, чтобы обе
//! стороны согласились: та же стадия, та же позиция начала, тот же текст.
//! Программы без ошибок анализа обязаны давать пустой список диагностик.
//!
//! Программы завершаются и не трогают окружение: без `WHILE`, без рекурсии
//! (функции вызывают только объявленные раньше), без ввода-вывода кроме
//! `PRINT`, который перехватывает тестовый харнесс.

use sga::diagnostics::{analyze_source, Code, Severity};
use sga::sga_alphabet::encode_word as kw;
use sga::{run_source, SgaError};

/// Детерминированный генератор (xorshift64): тест воспроизводим.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Генератор программ. Следит за областями видимости, чтобы большинство имён
/// были объявлены; необъявленное имя (`zz`), повторное объявление, присваивание
/// `LET`-переменной, неверная арность и несовместимые типы появляются с
/// заданной вероятностью.
struct Gen {
    rng: Rng,
    /// области видимости: (имя, изменяемое)
    scopes: Vec<Vec<(String, bool)>>,
    /// объявленные функции: (имя, число параметров)
    fns: Vec<(String, usize)>,
    /// объявленные структуры: (имя, поля)
    structs: Vec<(String, Vec<&'static str>)>,
    in_fn: bool,
    in_loop: bool,
    fresh: usize,
}

const FIELDS: [&str; 3] = ["x", "y", "z"];
const BINARY_OPS: [&str; 11] = ["+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">="];
const STRINGS: [&str; 3] = ["\"s\"", "\"юникод ✓\"", "\"😀 é\""];
const TYPES: [&str; 4] = ["int", "string", "bool", "float"];

impl Gen {
    fn new(seed: u64) -> Gen {
        Gen {
            rng: Rng(seed),
            scopes: vec![Vec::new()],
            fns: Vec::new(),
            structs: Vec::new(),
            in_fn: false,
            in_loop: false,
            fresh: 0,
        }
    }

    fn fresh_name(&mut self, prefix: &str) -> String {
        self.fresh += 1;
        format!("{prefix}{}", self.fresh)
    }

    fn declare(&mut self, name: &str, mutable: bool) {
        self.scopes
            .last_mut()
            .expect("область")
            .push((name.to_string(), mutable));
    }

    /// Имя переменной: объявленное (чаще) или необъявленное `zz`. `None`, если
    /// пока нечего выбирать.
    fn var(&mut self) -> Option<String> {
        let names: Vec<String> = self
            .scopes
            .iter()
            .flatten()
            .map(|(name, _)| name.clone())
            .collect();
        if names.is_empty() {
            return None;
        }
        if self.rng.chance(3) {
            return Some("zz".to_string());
        }
        Some(names[self.rng.below(names.len())].clone())
    }

    fn leaf(&mut self) -> String {
        match self.rng.below(12) {
            0..=2 => self.rng.below(100).to_string(),
            3 => format!("{}.5", self.rng.below(10)),
            4 => self.rng.pick(&STRINGS).to_string(),
            5 => kw("TRUE"),
            6 => kw("FALSE"),
            7 => kw("NIL"),
            _ => match self.var() {
                Some(name) => name,
                None => self.rng.below(100).to_string(),
            },
        }
    }

    fn expr(&mut self, depth: usize) -> String {
        if depth == 0 {
            return self.leaf();
        }
        match self.rng.below(100) {
            0..=29 => self.leaf(),
            30..=49 => {
                let left = self.expr(depth - 1);
                let right = self.expr(depth - 1);
                let op = if self.rng.chance(12) {
                    kw(if self.rng.chance(50) { "AND" } else { "OR" })
                } else {
                    self.rng.pick(&BINARY_OPS).to_string()
                };
                if !left.contains(' ') && !right.contains(' ') && self.rng.chance(40) {
                    // без скобок: левоассоциативная цепочка листьев
                    format!("{left} {op} {right}")
                } else {
                    format!("({left} {op} {right})")
                }
            }
            50..=55 => {
                let operand = self.expr(depth - 1);
                if self.rng.chance(60) {
                    format!("(-{operand})")
                } else {
                    format!("({} {operand})", kw("NOT"))
                }
            }
            56..=60 => {
                let items: Vec<String> = (0..self.rng.below(3))
                    .map(|_| self.expr(depth - 1))
                    .collect();
                format!("[{}]", items.join(", "))
            }
            61..=65 => {
                let target = self.expr(depth - 1);
                let index = self.expr(depth - 1);
                format!("({target})[{index}]")
            }
            66..=75 => self.builtin_call(depth),
            76..=80 => self.user_call(depth),
            81..=84 => {
                // замыкание: параметр виден только в его теле
                let param = self.fresh_name("q");
                self.scopes.push(vec![(param.clone(), true)]);
                let saved_fn = std::mem::replace(&mut self.in_fn, true);
                let body = self.expr(depth - 1);
                self.in_fn = saved_fn;
                self.scopes.pop();
                format!("{}({param}) {{ {} {body}; }}", kw("FN"), kw("RETURN"))
            }
            85..=89 => self.struct_literal(depth),
            90..=93 => {
                let object = self.expr(depth - 1);
                format!("({object}).{}", self.rng.pick(&FIELDS))
            }
            94..=96 => self.match_expr(depth),
            _ => {
                let inner = self.expr(depth - 1);
                format!("({inner})")
            }
        }
    }

    fn builtin_call(&mut self, depth: usize) -> String {
        let (name, arity) = match self.rng.below(4) {
            0 => ("len", 1),
            1 => ("to_string", 1),
            2 => ("to_int", 1),
            _ => ("push", 2),
        };
        // иногда неверная арность
        let count = if self.rng.chance(10) {
            self.rng.below(4)
        } else {
            arity
        };
        let args: Vec<String> = (0..count).map(|_| self.expr(depth - 1)).collect();
        format!("{name}({})", args.join(", "))
    }

    fn user_call(&mut self, depth: usize) -> String {
        if self.fns.is_empty() {
            return self.builtin_call(depth);
        }
        let (name, arity) = self.fns[self.rng.below(self.fns.len())].clone();
        let count = if self.rng.chance(10) {
            self.rng.below(4)
        } else {
            arity
        };
        let args: Vec<String> = (0..count).map(|_| self.expr(depth - 1)).collect();
        format!("{name}({})", args.join(", "))
    }

    fn struct_literal(&mut self, depth: usize) -> String {
        if self.structs.is_empty() {
            return self.leaf();
        }
        let (name, fields) = self.structs[self.rng.below(self.structs.len())].clone();
        let mut parts: Vec<String> = Vec::new();
        for field in fields {
            if self.rng.chance(80) {
                parts.push(format!("{field}: {}", self.expr(depth - 1)));
            }
        }
        if self.rng.chance(8) {
            parts.push(format!("w: {}", self.expr(depth - 1)));
        }
        if parts.is_empty() {
            parts.push(format!("x: {}", self.expr(depth - 1)));
        }
        format!("{name} {{ {} }}", parts.join(", "))
    }

    fn match_expr(&mut self, depth: usize) -> String {
        let scrutinee = self.expr(depth - 1);
        let mut arms: Vec<String> = Vec::new();
        for pattern in ["1", "2", "\"a\""].iter().take(1 + self.rng.below(3)) {
            arms.push(format!("{pattern} -> {}", self.leaf()));
        }
        // обычно завершается catch-all; иногда нарушает правило
        match self.rng.below(100) {
            0..=69 => arms.push(format!("_ -> {}", self.leaf())),
            70..=84 => arms.push(format!("m -> {}", self.leaf())),
            85..=92 => {}                                         // нет catch-all
            _ => arms.insert(0, format!("_ -> {}", self.leaf())), // не последний
        }
        format!("{} ({scrutinee}) {{ {} }}", kw("MATCH"), arms.join(", "))
    }

    /// Блок из 1–2 операторов в новой области видимости.
    fn block(&mut self, depth: usize) -> String {
        self.scopes.push(Vec::new());
        let count = 1 + self.rng.below(2);
        let body: Vec<String> = (0..count).map(|_| self.stmt(depth, false)).collect();
        self.scopes.pop();
        body.join(" ")
    }

    fn stmt(&mut self, depth: usize, top: bool) -> String {
        match self.rng.below(100) {
            0..=34 => self.declaration(),
            35..=46 => {
                let Some(name) = self.var() else {
                    return self.declaration();
                };
                let value = self.expr(2);
                format!("{name} = {value};")
            }
            47..=50 => {
                let Some(name) = self.var() else {
                    return self.declaration();
                };
                let index = self.expr(1);
                let value = self.expr(1);
                format!("{name}[{index}] = {value};")
            }
            51..=52 => {
                let Some(name) = self.var() else {
                    return self.declaration();
                };
                let value = self.expr(1);
                format!("{name}.{} = {value};", self.rng.pick(&FIELDS))
            }
            53..=62 => {
                let args: Vec<String> = (0..1 + self.rng.below(2)).map(|_| self.expr(2)).collect();
                format!("{}({});", kw("PRINT"), args.join(", "))
            }
            63..=70 if depth > 0 => {
                let condition = self.expr(1);
                let then_branch = self.block(depth - 1);
                if self.rng.chance(40) {
                    let else_branch = self.block(depth - 1);
                    format!(
                        "{} ({condition}) {{ {then_branch} }} {} {{ {else_branch} }}",
                        kw("IF"),
                        kw("ELSE")
                    )
                } else {
                    format!("{} ({condition}) {{ {then_branch} }}", kw("IF"))
                }
            }
            71..=76 if depth > 0 => {
                let variable = self.fresh_name("i");
                let high = if self.rng.chance(8) {
                    "\"a\"".to_string()
                } else {
                    (1 + self.rng.below(3)).to_string()
                };
                self.scopes.push(vec![(variable.clone(), true)]);
                let saved_loop = std::mem::replace(&mut self.in_loop, true);
                let body = self.block(depth - 1);
                self.in_loop = saved_loop;
                self.scopes.pop();
                format!(
                    "{} {variable} {} 0..{high} {{ {body} }}",
                    kw("FOR"),
                    kw("IN")
                )
            }
            77..=82 if top => self.function(),
            83..=85 if top => {
                let name = self.fresh_name("S");
                let count = 1 + self.rng.below(3);
                let fields: Vec<&'static str> = FIELDS[..count].to_vec();
                let text = format!("{} {name} {{ {} }}", kw("STRUCT"), fields.join(", "));
                self.structs.push((name, fields));
                text
            }
            86..=88 => {
                if self.in_fn || self.rng.chance(30) {
                    format!("{} {};", kw("RETURN"), self.expr(1))
                } else {
                    self.declaration()
                }
            }
            89..=91 => {
                if self.in_loop || self.rng.chance(30) {
                    format!(
                        "{};",
                        kw(if self.rng.chance(50) {
                            "BREAK"
                        } else {
                            "CONTINUE"
                        })
                    )
                } else {
                    self.declaration()
                }
            }
            _ => {
                let call = if self.rng.chance(50) {
                    self.builtin_call(2)
                } else {
                    self.user_call(2)
                };
                format!("{call};")
            }
        }
    }

    fn declaration(&mut self) -> String {
        let mutable = self.rng.chance(50);
        let keyword = kw(if mutable { "VAR" } else { "LET" });
        // иногда повторное объявление в той же области
        let reuse = self.rng.chance(6) && self.scopes.last().is_some_and(|scope| !scope.is_empty());
        let name = if reuse {
            let scope = self.scopes.last().expect("область");
            scope[self.rng.below(scope.len())].0.clone()
        } else {
            self.fresh_name("v")
        };
        let annotation = if self.rng.chance(20) {
            format!(": {}", self.rng.pick(&TYPES))
        } else {
            String::new()
        };
        // значение не видит объявляемого имени
        let value = self.expr(2);
        if !reuse {
            self.declare(&name, mutable);
        }
        format!("{keyword} {name}{annotation} = {value};")
    }

    fn function(&mut self) -> String {
        let name = self.fresh_name("f");
        let count = self.rng.below(3);
        let mut params = Vec::new();
        let mut declared = Vec::new();
        for _ in 0..count {
            let param = self.fresh_name("p");
            let mutable = self.rng.chance(30);
            params.push(format!(
                "{}{param}",
                if mutable {
                    format!("{} ", kw("MUT"))
                } else {
                    String::new()
                }
            ));
            declared.push((param, mutable));
        }
        self.scopes.push(declared);
        let saved = (
            std::mem::replace(&mut self.in_fn, true),
            std::mem::replace(&mut self.in_loop, false),
        );
        let mut body: Vec<String> = (0..1 + self.rng.below(2))
            .map(|_| self.stmt(1, false))
            .collect();
        if self.rng.chance(70) {
            body.push(format!("{} {};", kw("RETURN"), self.expr(1)));
        }
        (self.in_fn, self.in_loop) = saved;
        self.scopes.pop();
        // вызывать функцию можно только после её объявления (без рекурсии)
        self.fns.push((name.clone(), count));
        format!(
            "{} {name}({}) {{ {} }}",
            kw("FN"),
            params.join(", "),
            body.join(" ")
        )
    }

    fn program(&mut self) -> String {
        let count = 1 + self.rng.below(6);
        let statements: Vec<String> = (0..count).map(|_| self.stmt(2, true)).collect();
        statements.join("\n")
    }
}

/// Разбирает `[L:C] метка: текст` (формат ошибок лексера, парсера, semantic
/// и типов). Возвращает позицию начала и текст после метки.
fn split_cli_message(message: &str) -> Option<((usize, usize), &str)> {
    let rest = message.strip_prefix('[')?;
    let (position, rest) = rest.split_once("] ")?;
    let (line, col) = position.split_once(':')?;
    let (_label, text) = rest.split_once(": ")?;
    Some(((line.parse().ok()?, col.parse().ok()?), text))
}

#[derive(Default, Debug)]
struct Stats {
    clean: usize,
    semantic: usize,
    type_error: usize,
}

#[test]
fn random_programs_get_the_same_error_from_the_cli_and_the_language_server() {
    let mut stats = Stats::default();
    for seed in 1..=5000u64 {
        let mut generator = Gen::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let source = generator.program();
        let diagnostics = analyze_source(&source);
        assert!(
            diagnostics.len() <= 1,
            "программа {seed}: больше одной диагностики: {diagnostics:?}\n{source}"
        );
        match run_source(&source) {
            Err(
                error @ (SgaError::Lex(_)
                | SgaError::Parse(_)
                | SgaError::Semantic(_)
                | SgaError::Type(_)),
            ) => {
                let (expected_code, message) = match &error {
                    SgaError::Lex(m) => (Code::Lexer, m),
                    SgaError::Parse(m) => (Code::Parser, m),
                    SgaError::Semantic(m) => (Code::Semantic, m),
                    SgaError::Type(m) => (Code::Type, m),
                    _ => unreachable!(),
                };
                assert!(
                    !matches!(expected_code, Code::Lexer | Code::Parser),
                    "программа {seed}: генератор породил синтаксическую ошибку: {message}\n{source}"
                );
                let Some((position, text)) = split_cli_message(message) else {
                    panic!("программа {seed}: сообщение без позиции: {message}\n{source}")
                };
                assert_eq!(
                    diagnostics.len(),
                    1,
                    "программа {seed}: CLI сообщил об ошибке ({message}), а сервер — нет\n{source}"
                );
                let d = &diagnostics[0];
                assert_eq!(d.code, expected_code, "программа {seed}: {d:?}\n{source}");
                assert_eq!(d.severity, Severity::Error, "программа {seed}");
                assert_eq!(
                    (d.start_line, d.start_col),
                    position,
                    "программа {seed}: позиции CLI и сервера расходятся: {message} / {d:?}\n{source}"
                );
                assert_eq!(d.message, text, "программа {seed}\n{source}");
                match expected_code {
                    Code::Semantic => stats.semantic += 1,
                    _ => stats.type_error += 1,
                }
            }
            // успешный запуск или ошибка времени исполнения: анализ прошёл
            Ok(_) | Err(SgaError::Runtime(_)) | Err(SgaError::Import(_)) => {
                assert!(
                    diagnostics.is_empty(),
                    "программа {seed}: CLI анализ принял, а сервер сообщил {diagnostics:?}\n{source}"
                );
                stats.clean += 1;
            }
        }
    }
    // Генератор не вырожден: есть программы всех трёх исходов
    // (на 5000 программах: ≈ 1300 корректных, ≈ 2200 semantic, ≈ 1500 типов)
    assert!(stats.clean >= 600, "мало корректных программ: {stats:?}");
    assert!(stats.semantic >= 1000, "мало ошибок semantic: {stats:?}");
    assert!(stats.type_error >= 700, "мало ошибок типов: {stats:?}");
}
