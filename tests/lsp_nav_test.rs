//! Навигация языкового сервера (T019, M007): `textDocument/documentSymbol` и
//! `textDocument/definition` проверяются эталонами, которые НЕ зависят от
//! сканера, на котором навигация построена:
//!
//! * диапазоны символов — сверкой с настоящим AST (`Stmt.span`, T017) и таблицей
//!   границ токенов лексера: 17 примеров из `examples/`, рукописные программы и
//!   случайные корректные программы;
//! * переход к определению — сверкой с настоящим ВЫПОЛНЕНИЕМ программы: в каждом
//!   объявлении лежит своё уникальное число, каждое использование имени
//!   записывает прочитанное значение в журнал (`note(номер_места, имя)`), и по
//!   числу в журнале видно, КАКОЕ объявление прочитала виртуальная машина.
//!   Ответ `definition` обязан указывать на то же объявление. Генератор
//!   параллельно ведёт собственную модель областей видимости; модель, в свою
//!   очередь, сверяется с исполнением, поэтому расхождение генератора с языком
//!   не останется незамеченным;
//! * устойчивость — испорченные программы, документы предельного размера.
//!
//! Ключевые слова в шаблонах пишутся `@LET`, `@IF`, … и подставляются словами
//! из SGA-букв (идентификаторы и числа — обычный ASCII, как в языке).

use sga::ast::{Stmt, StmtKind, TypeAnnotation};
use sga::lexer::token::{Token, TokenKind, TokenSpan};
use sga::lexer::Lexer;
use sga::lsp::json::Json;
use sga::lsp::nav::{self, Symbol, SymbolKind, MAX_SYMBOL_DEPTH};
use sga::lsp::scan::{Document, Pos};
use sga::lsp::{Server, MAX_DOCUMENT_BYTES};
use sga::parser::Parser;
use sga::runtime::Value;
use sga::sga_alphabet::encode_word;
use sga::{diagnostics, RunOptions};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ── общие помощники ────────────────────────────────────────────────────────

/// Подставляет SGA-слова: `@LET` -> слово `LET` из SGA-букв. Слово — максимальная
/// последовательность заглавных ASCII-букв после `@`.
fn sga(template: &str) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '@' {
            out.push(c);
            continue;
        }
        let mut word = String::new();
        while let Some(&next) = chars.peek() {
            if !next.is_ascii_uppercase() {
                break;
            }
            word.push(next);
            chars.next();
        }
        assert!(!word.is_empty(), "`@` без слова в шаблоне {template:?}");
        out.push_str(&encode_word(&word));
    }
    out
}

/// Детерминированный генератор псевдослучайных чисел (xorshift64*): одни и те
/// же зерна дают одни и те же программы, поэтому расхождение воспроизводимо.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        ((self.next() >> 33) as usize) % n
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

fn example_sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut found: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("каталог examples")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "sga" {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().to_string();
            Some((name, std::fs::read_to_string(&path).expect("пример")))
        })
        .collect();
    found.sort();
    found
}

fn assert_no_failures(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "расхождений: {}\n\n{}",
        failures.len(),
        failures
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// Строка исходника с номером `line` (с единицы) — для сообщений о расхождениях.
fn line_of(text: &str, line: usize) -> String {
    text.split('\n').nth(line - 1).unwrap_or("").to_string()
}

// ── 1. генератор программ: модель объявлений и использований ───────────────
//
// Программа начинается тремя служебными объявлениями:
//
//     VAR log = [];
//     FN note(MUT l, s, v) { push(l, [s, v]); RETURN v; }
//     FN tail(a, b) { RETURN b; }
//
// `note(журнал, N, выражение)` записывает в журнал пару «номер места,
// значение» и возвращает значение; `tail(a, b)` вычисляет `a` ради побочного
// эффекта и возвращает `b`. Журнал в тела именованных функций приходит
// первым параметром `MUT l`: во время выполнения такая функция видит только
// свои параметры и локальные имена (глобальные переменные ей недоступны, хотя
// `semantic` их разрешает — расхождение языка, TD-028), поэтому генератор
// читает внутри функции только их; замыкания захватывают всё видимое. Каждое объявление `d` хранит число `1000 + d`: переменная
// получает его через `tail(выражение, число)` (выражение с использованиями
// выполняется, значение отбрасывается), параметр — из аргументов вызова,
// переменная цикла — как начало диапазона `число..число+1`, привязка `MATCH` —
// как проверяемое значение, функция и замыкание — как результат вызова.
// Программа заканчивается оператором `log;`, его значение — журнал.

/// Число, которое хранит или возвращает объявление с номером `d`.
fn value_of(d: usize) -> i64 {
    1000 + d as i64
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Variable,
    Lambda,
    Record,
    Param,
    ForVar,
    MatchBind,
    Function,
    Struct,
    Field,
    Counter,
    Helper,
}

struct Decl {
    name: String,
    /// Начало токена имени (строка и колонка с единицы, колонка в символах).
    at: Pos,
    role: Role,
}

impl Decl {
    fn end(&self) -> Pos {
        (self.at.0, self.at.1 + self.name.chars().count())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SiteKind {
    Value,
    Call,
    Builtin,
    StructName,
    Key,
    AfterDot,
    Comment,
    Text,
}

/// Место в тексте, где написано имя, и то, куда оно по модели должно вести.
struct Site {
    name: String,
    at: Pos,
    kind: SiteKind,
    /// Номер объявления; `None` — определения нет (builtin, после точки,
    /// комментарий, строка).
    expected: Option<usize>,
    /// Значение попадает в журнал (`note`), поэтому его видит исполнение.
    observable: bool,
}

/// Позиция в выводимом тексте: строка и колонка (в символах) с единицы — как у
/// лексера.
struct Out {
    text: String,
    line: usize,
    col: usize,
}

impl Out {
    fn new() -> Out {
        Out {
            text: String::new(),
            line: 1,
            col: 1,
        }
    }

    fn put(&mut self, s: &str) {
        for c in s.chars() {
            self.text.push(c);
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
    }

    fn kw(&mut self, word: &str) {
        let glyphs = encode_word(word);
        self.put(&glyphs);
    }

    fn at(&self) -> Pos {
        (self.line, self.col)
    }
}

#[derive(Clone)]
enum Kind {
    Int {
        mutable: bool,
    },
    Lambda {
        params: Vec<usize>,
    },
    Record {
        structure: usize,
        field: usize,
    },
    /// Счётчик цикла `WHILE`: число не из таблицы, в журнал не читается.
    Counter,
    /// Служебное объявление (`log`): в случайных выражениях не используется.
    Hidden,
}

#[derive(Clone)]
struct Entry {
    name: String,
    decl: usize,
    kind: Kind,
}

struct Function {
    name: String,
    decl: usize,
    params: Vec<usize>,
}

struct StructModel {
    name: String,
    decl: usize,
    /// Имя поля и номер его объявления.
    fields: Vec<(String, usize)>,
}

struct Generated {
    text: String,
    decls: Vec<Decl>,
    sites: Vec<Site>,
}

struct Gen {
    rng: Rng,
    out: Out,
    decls: Vec<Decl>,
    sites: Vec<Site>,
    scopes: Vec<Vec<Entry>>,
    /// Индекс первой области текущей функции: присваивать можно только
    /// переменные этой функции.
    fn_base: usize,
    functions: Vec<Function>,
    structs: Vec<StructModel>,
    names: usize,
    note: usize,
    tail: usize,
    /// Имя и объявление журнала, видимого в этом месте: `log` на верхнем
    /// уровне, `l` — параметр именованной функции.
    log_name: String,
    log_decl: usize,
    /// Глобальные имена, которые видит `semantic` в теле именованной функции,
    /// а исполнение — нет: кандидаты на затенение локальным объявлением.
    shadowable: Vec<String>,
}

impl Gen {
    fn indent(&mut self, depth: usize) {
        self.out.put(&"    ".repeat(depth));
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.names += 1;
        format!("{prefix}{}", self.names)
    }

    fn reserve(&mut self, name: &str, role: Role) -> usize {
        self.decls.push(Decl {
            name: name.to_string(),
            at: (0, 0),
            role,
        });
        self.decls.len() - 1
    }

    /// Пишет имя объявления `d` и запоминает его место.
    fn write_decl(&mut self, d: usize) {
        self.decls[d].at = self.out.at();
        let name = self.decls[d].name.clone();
        self.out.put(&name);
    }

    fn declare(&mut self, name: &str, role: Role) -> usize {
        let d = self.reserve(name, role);
        self.write_decl(d);
        d
    }

    /// Пишет имя и записывает его как место использования.
    fn site(&mut self, name: &str, kind: SiteKind, expected: Option<usize>, observable: bool) {
        let at = self.out.at();
        self.out.put(name);
        self.sites.push(Site {
            name: name.to_string(),
            at,
            kind,
            expected,
            observable,
        });
    }

    /// Ближайшая видимая запись с этим именем.
    fn resolve(&self, name: &str) -> Option<Entry> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev())
            .find(|entry| entry.name == name)
            .cloned()
    }

    fn visible_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for entry in self.scopes.iter().flatten() {
            if !matches!(entry.kind, Kind::Hidden | Kind::Counter) && !names.contains(&entry.name) {
                names.push(entry.name.clone());
            }
        }
        names
    }

    fn declare_in_scope(&mut self, name: &str, decl: usize, kind: Kind) {
        self.scopes.last_mut().expect("область").push(Entry {
            name: name.to_string(),
            decl,
            kind,
        });
    }

    /// Имя для нового объявления: новое, имя из внешней области (затенение) или
    /// имя функции документа («двойник»: вызов выберет функцию, а не переменную).
    /// Имя, уже объявленное в ТЕКУЩЕЙ области, не берётся — это ошибка языка.
    fn pick_name(&mut self, prefix: &str) -> String {
        let current: Vec<String> = self
            .scopes
            .last()
            .map(|scope| scope.iter().map(|e| e.name.clone()).collect())
            .unwrap_or_default();
        if self.rng.chance(14) {
            let mut outer: Vec<String> = self.scopes[..self.scopes.len() - 1]
                .iter()
                .flatten()
                .filter(|e| !matches!(e.kind, Kind::Hidden))
                .map(|e| e.name.clone())
                .chain(self.shadowable.iter().cloned())
                .filter(|n| !current.contains(n))
                .collect();
            outer.dedup();
            if !outer.is_empty() {
                return outer[self.rng.below(outer.len())].clone();
            }
        }
        if self.rng.chance(8) && !self.functions.is_empty() {
            let name = self.functions[self.rng.below(self.functions.len())]
                .name
                .clone();
            if !current.contains(&name) {
                return name;
            }
        }
        self.fresh(prefix)
    }

    /// Имя видимого журнала как использование (`log` или `l`).
    fn log_arg(&mut self) {
        let name = self.log_name.clone();
        let decl = self.log_decl;
        self.site(&name, SiteKind::Value, Some(decl), false);
    }

    /// Пишет `note(журнал, N, ` и возвращает `N`: так будет называться
    /// СЛЕДУЮЩЕЕ записанное место (то, что читает журнал).
    fn note_open(&mut self) -> usize {
        let note = self.note;
        self.site("note", SiteKind::Call, Some(note), false);
        self.out.put("(");
        self.log_arg();
        self.out.put(", ");
        let id = self.sites.len();
        self.out.put(&id.to_string());
        self.out.put(", ");
        id
    }

    fn tail_open(&mut self) {
        let tail = self.tail;
        self.site("tail", SiteKind::Call, Some(tail), false);
        self.out.put("(");
    }

    /// Куда ведёт вызов `имя(`: объявление, номера его параметров и признак
    /// «функция документа» (ей первым аргументом идёт журнал).
    fn resolve_call(&self, name: &str) -> Option<(usize, Vec<usize>, bool)> {
        // порядок как в codegen: функция документа раньше переменной
        if let Some(f) = self.functions.iter().find(|f| f.name == name) {
            return Some((f.decl, f.params.clone(), true));
        }
        match self.resolve(name) {
            Some(Entry {
                decl,
                kind: Kind::Lambda { params },
                ..
            }) => Some((decl, params, false)),
            _ => None,
        }
    }

    /// `note(журнал, N, имя(аргументы))`: аргументы — числа параметров
    /// вызываемого, функции документа журнал передаётся первым.
    fn call(&mut self, name: &str) {
        let (target, params, is_function) = self.resolve_call(name).expect("вызываемое имя");
        self.note_open();
        self.site(name, SiteKind::Call, Some(target), true);
        self.out.put("(");
        let mut first = true;
        if is_function {
            self.log_arg();
            first = false;
        }
        for p in &params {
            if !first {
                self.out.put(", ");
            }
            first = false;
            self.out.put(&value_of(*p).to_string());
        }
        self.out.put("))");
    }

    fn use_entry(&mut self, entry: &Entry) {
        match &entry.kind {
            Kind::Int { .. } => {
                self.note_open();
                self.site(&entry.name, SiteKind::Value, Some(entry.decl), true);
                self.out.put(")");
            }
            Kind::Record { structure, field } => {
                let field_name = self.structs[*structure].fields[*field].0.clone();
                self.note_open();
                self.site(&entry.name, SiteKind::Value, Some(entry.decl), true);
                self.out.put(".");
                self.site(&field_name, SiteKind::AfterDot, None, false);
                self.out.put(")");
            }
            Kind::Lambda { .. } => self.call(&entry.name),
            Kind::Counter | Kind::Hidden => unreachable!("служебные записи не используются"),
        }
    }

    // ── выражения ──────────────────────────────────────────────────────────

    fn expr(&mut self, depth: usize) {
        let roll = self.rng.below(100);
        let names = self.visible_names();
        if depth >= 3 || roll < 16 {
            let n = self.rng.below(10).to_string();
            self.out.put(&n);
        } else if roll < 54 && !names.is_empty() {
            let name = names[self.rng.below(names.len())].clone();
            let entry = self.resolve(&name).expect("видимое имя");
            self.use_entry(&entry);
        } else if roll < 66 {
            let op = ["+", "-", "*"][self.rng.below(3)];
            self.out.put("(");
            self.expr(depth + 1);
            self.out.put(&format!(" {op} "));
            self.expr(depth + 1);
            self.out.put(")");
        } else if roll < 74 {
            self.site("len", SiteKind::Builtin, None, false);
            self.out.put("([");
            self.expr(depth + 1);
            self.out.put(", ");
            self.expr(depth + 1);
            self.out.put("])");
        } else if roll < 84 && !self.functions.is_empty() {
            let name = self.functions[self.rng.below(self.functions.len())]
                .name
                .clone();
            self.call(&name);
        } else if roll < 90 && !names.is_empty() {
            // строка с именами внутри: определения в ней нет
            self.tail_open();
            self.out.put("\"");
            for k in 0..1 + self.rng.below(2) {
                if k > 0 {
                    self.out.put(" ");
                }
                let name = names[self.rng.below(names.len())].clone();
                self.site(&name, SiteKind::Text, None, false);
            }
            self.out.put("\", 7)");
        } else {
            self.match_expr(depth);
        }
    }

    /// `MATCH tail(e, число) { 0 -> e, имя -> e }`: проверяемое значение —
    /// число привязки, первый пункт не сработает никогда.
    fn match_expr(&mut self, depth: usize) {
        let name = self.pick_name("b");
        let bind = self.reserve(&name, Role::MatchBind);
        self.out.kw("MATCH");
        self.out.put(" ");
        self.tail_open();
        self.expr(depth + 1);
        self.out.put(&format!(", {}) {{ 0 -> ", value_of(bind)));
        self.expr(depth + 1);
        self.out.put(", ");
        self.write_decl(bind);
        self.out.put(" -> ");
        self.scopes.push(vec![Entry {
            name,
            decl: bind,
            kind: Kind::Int { mutable: false },
        }]);
        self.expr(depth + 1);
        self.scopes.pop();
        self.out.put(" }");
    }

    fn cond(&mut self) {
        match self.rng.below(4) {
            0 => self.out.put("1 < 2"),
            1 => self.out.put("2 < 1"),
            2 => {
                self.expr(1);
                self.out.put(" > 0");
            }
            _ => {
                self.expr(1);
                self.out.put(" < 0");
            }
        }
    }

    /// Инициализатор: число объявления, при желании с вычислением выражения.
    fn init(&mut self, d: usize) {
        if self.rng.chance(45) {
            self.out.put(&value_of(d).to_string());
        } else {
            self.tail_open();
            self.expr(0);
            self.out.put(&format!(", {})", value_of(d)));
        }
    }

    // ── операторы ──────────────────────────────────────────────────────────

    fn body(&mut self, depth: usize) {
        for _ in 0..=self.rng.below(3) {
            self.stmt(depth, false);
        }
    }

    fn stmt(&mut self, depth: usize, top: bool) {
        let roll = self.rng.below(100);
        let deep = depth >= 3;
        if top && roll < 12 {
            self.named_function();
        } else if top && roll < 18 {
            self.struct_decl();
        } else if roll < 38 || deep && roll < 70 {
            self.variable(depth);
        } else if roll < 46 {
            self.effect(depth);
        } else if roll < 52 {
            self.assign(depth);
        } else if roll < 58 && !self.structs.is_empty() {
            self.record(depth);
        } else if roll < 66 && !deep {
            self.if_stmt(depth);
        } else if roll < 70 && !deep {
            self.for_stmt(depth);
        } else if roll < 73 && !deep {
            self.while_stmt(depth);
        } else if roll < 75 && !deep {
            self.bare_block(depth);
        } else if roll < 85 {
            self.lambda(depth);
        } else if roll < 90 {
            self.comment(depth);
        } else {
            self.effect(depth);
        }
    }

    fn effect(&mut self, depth: usize) {
        self.indent(depth);
        self.tail_open();
        self.expr(0);
        self.out.put(", 0);\n");
    }

    fn variable(&mut self, depth: usize) {
        let (word, mutable) = match self.rng.below(4) {
            0 => ("VAR", true),
            1 => ("CONST", false),
            _ => ("LET", false),
        };
        let name = self.pick_name("v");
        self.indent(depth);
        self.out.kw(word);
        self.out.put(" ");
        let d = self.declare(&name, Role::Variable);
        if self.rng.chance(25) {
            self.out.put(": int");
        }
        self.out.put(" = ");
        self.init(d);
        self.out.put(";\n");
        self.declare_in_scope(&name, d, Kind::Int { mutable });
    }

    fn assign(&mut self, depth: usize) {
        let targets: Vec<Entry> = self.scopes[self.fn_base..]
            .iter()
            .flatten()
            .filter(|e| matches!(e.kind, Kind::Int { mutable: true }))
            .filter(|e| self.resolve(&e.name).is_some_and(|r| r.decl == e.decl))
            .cloned()
            .collect();
        if targets.is_empty() {
            self.effect(depth);
            return;
        }
        let target = targets[self.rng.below(targets.len())].clone();
        self.indent(depth);
        self.site(&target.name, SiteKind::Value, Some(target.decl), false);
        self.out.put(" = ");
        self.tail_open();
        self.expr(0);
        self.out.put(&format!(", {});\n", value_of(target.decl)));
    }

    fn if_stmt(&mut self, depth: usize) {
        self.indent(depth);
        self.out.kw("IF");
        self.out.put(" ");
        self.cond();
        self.out.put(" {\n");
        self.scopes.push(Vec::new());
        self.body(depth + 1);
        self.scopes.pop();
        if self.rng.chance(50) {
            self.indent(depth);
            self.out.put("} ");
            self.out.kw("ELSE");
            self.out.put(" {\n");
            self.scopes.push(Vec::new());
            self.body(depth + 1);
            self.scopes.pop();
        }
        self.indent(depth);
        self.out.put("}\n");
    }

    fn for_stmt(&mut self, depth: usize) {
        let name = self.fresh("i");
        self.indent(depth);
        self.out.kw("FOR");
        self.out.put(" ");
        let d = self.declare(&name, Role::ForVar);
        self.out.put(" ");
        self.out.kw("IN");
        let from = value_of(d);
        self.out.put(&format!(" {from}..{} {{\n", from + 1));
        self.scopes.push(vec![Entry {
            name,
            decl: d,
            kind: Kind::Int { mutable: false },
        }]);
        self.body(depth + 1);
        self.scopes.pop();
        self.indent(depth);
        self.out.put("}\n");
    }

    fn while_stmt(&mut self, depth: usize) {
        let name = self.fresh("w");
        self.indent(depth);
        self.out.kw("VAR");
        self.out.put(" ");
        let d = self.declare(&name, Role::Counter);
        self.out.put(" = 0;\n");
        self.declare_in_scope(&name, d, Kind::Counter);
        self.indent(depth);
        self.out.kw("WHILE");
        self.out.put(" ");
        self.site(&name, SiteKind::Value, Some(d), false);
        self.out.put(" < 2 {\n");
        self.scopes.push(Vec::new());
        // приращение первым: BREAK и CONTINUE дальше не создают бесконечный цикл
        self.indent(depth + 1);
        self.site(&name, SiteKind::Value, Some(d), false);
        self.out.put(" = ");
        self.site(&name, SiteKind::Value, Some(d), false);
        self.out.put(" + 1;\n");
        self.body(depth + 1);
        self.scopes.pop();
        self.indent(depth);
        self.out.put("}\n");
    }

    fn bare_block(&mut self, depth: usize) {
        self.indent(depth);
        self.out.put("{\n");
        self.scopes.push(Vec::new());
        self.body(depth + 1);
        self.scopes.pop();
        self.indent(depth);
        self.out.put("}\n");
    }

    fn comment(&mut self, depth: usize) {
        self.indent(depth);
        self.out.put("// ");
        self.out.kw("LET");
        self.out.put(" ");
        let names = self.visible_names();
        if names.is_empty() {
            self.out.put("nothing\n");
            return;
        }
        for k in 0..1 + self.rng.below(3) {
            if k > 0 {
                self.out.put(" = ");
            }
            let name = names[self.rng.below(names.len())].clone();
            self.site(&name, SiteKind::Comment, None, false);
        }
        self.out.put(";\n");
    }

    /// Параметры: новые имена или затенение имён внешних областей; все
    /// различны в пределах списка. Пишет `(список)`; у функции документа
    /// первым идёт `MUT l` (журнал). Возвращает номера и имена обычных
    /// параметров и объявление журнала.
    fn params(&mut self, typed: bool, with_log: bool) -> (Vec<usize>, Vec<String>, Option<usize>) {
        let count = self.rng.below(3);
        let mut ids = Vec::new();
        let mut names: Vec<String> = Vec::new();
        self.out.put("(");
        let mut log = None;
        if with_log {
            self.out.kw("MUT");
            self.out.put(" ");
            log = Some(self.declare("l", Role::Param));
        }
        for k in 0..count {
            if k > 0 || with_log {
                self.out.put(", ");
            }
            let outer: Vec<String> = self
                .scopes
                .iter()
                .flatten()
                .filter(|e| !matches!(e.kind, Kind::Hidden))
                .map(|e| e.name.clone())
                .filter(|n| !names.contains(n))
                .collect();
            let name = if self.rng.chance(14) && !outer.is_empty() {
                outer[self.rng.below(outer.len())].clone()
            } else {
                self.fresh("p")
            };
            let d = self.declare(&name, Role::Param);
            if typed && self.rng.chance(30) {
                self.out.put(": int");
            }
            ids.push(d);
            names.push(name);
        }
        self.out.put(")");
        (ids, names, log)
    }

    /// Область тела замыкания: параметры поверх всего, что видно снаружи.
    fn enter_lambda(&mut self, ids: &[usize], names: &[String]) -> usize {
        let saved = self.fn_base;
        self.scopes.push(
            ids.iter()
                .zip(names)
                .map(|(&decl, name)| Entry {
                    name: name.clone(),
                    decl,
                    kind: Kind::Int { mutable: false },
                })
                .collect(),
        );
        self.fn_base = self.scopes.len() - 1;
        saved
    }

    fn lambda(&mut self, depth: usize) {
        let name = self.pick_name("f");
        self.indent(depth);
        self.out.kw("LET");
        self.out.put(" ");
        let d = self.declare(&name, Role::Lambda);
        self.out.put(" = ");
        self.out.kw("FN");
        let (ids, names, _) = self.params(false, false);
        self.out.put(" {\n");
        let saved = self.enter_lambda(&ids, &names);
        self.body(depth + 1);
        self.indent(depth + 1);
        self.out.kw("RETURN");
        self.out.put(&format!(" {};\n", value_of(d)));
        self.fn_base = saved;
        self.scopes.pop();
        self.indent(depth);
        self.out.put("};\n");
        self.declare_in_scope(&name, d, Kind::Lambda { params: ids });
        // вызов сразу: тело замыкания выполняется
        self.indent(depth);
        self.call(&name);
        self.out.put(";\n");
    }

    fn named_function(&mut self) {
        let name = self.fresh("fn");
        self.out.kw("FN");
        self.out.put(" ");
        let d = self.declare(&name, Role::Function);
        let (ids, names, log) = self.params(true, true);
        let log = log.expect("журнал");
        let typed_result = self.rng.chance(30);
        if typed_result {
            self.out.put(" -> int");
        }
        self.out.put(" {\n");
        // во время выполнения функция видит только свои параметры и локальные
        // имена; глобальные имена остаются лишь кандидатами на затенение
        let global_names: Vec<String> = self
            .scopes
            .iter()
            .flatten()
            .filter(|e| !matches!(e.kind, Kind::Hidden))
            .map(|e| e.name.clone())
            .collect();
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![Vec::new()]);
        let saved_base = std::mem::replace(&mut self.fn_base, 0);
        let saved_names = std::mem::replace(&mut self.shadowable, global_names);
        let saved_log = (
            std::mem::replace(&mut self.log_name, "l".to_string()),
            std::mem::replace(&mut self.log_decl, log),
        );
        self.declare_in_scope("l", log, Kind::Hidden);
        for (&decl, param) in ids.iter().zip(&names) {
            self.declare_in_scope(param, decl, Kind::Int { mutable: false });
        }
        self.body(1);
        self.indent(1);
        self.out.kw("RETURN");
        self.out.put(&format!(" {};\n", value_of(d)));
        self.scopes = saved_scopes;
        self.fn_base = saved_base;
        self.shadowable = saved_names;
        (self.log_name, self.log_decl) = saved_log;
        self.out.put("}\n");
        self.functions.push(Function {
            name: name.clone(),
            decl: d,
            params: ids,
        });
        self.call(&name);
        self.out.put(";\n");
    }

    fn struct_decl(&mut self) {
        let name = self.fresh("P");
        self.out.kw("STRUCT");
        self.out.put(" ");
        let d = self.declare(&name, Role::Struct);
        self.out.put(" { ");
        let mut pool: Vec<String> = ["x", "y", "z", "w"].iter().map(|s| s.to_string()).collect();
        // поле с именем видимой переменной: ключ литерала — поле, а не переменная
        pool.extend(self.visible_names());
        let mut fields: Vec<(String, usize)> = Vec::new();
        for k in 0..1 + self.rng.below(3) {
            let field = pool[self.rng.below(pool.len())].clone();
            if fields.iter().any(|(n, _)| *n == field) {
                continue;
            }
            if k > 0 {
                self.out.put(", ");
            }
            let fd = self.declare(&field, Role::Field);
            fields.push((field, fd));
        }
        self.out.put(" }\n");
        self.structs.push(StructModel {
            name,
            decl: d,
            fields,
        });
    }

    fn record(&mut self, depth: usize) {
        let structure = self.rng.below(self.structs.len());
        let name = self.pick_name("s");
        self.indent(depth);
        self.out.kw("LET");
        self.out.put(" ");
        let d = self.declare(&name, Role::Record);
        self.out.put(" = ");
        let (struct_name, struct_decl, fields) = {
            let s = &self.structs[structure];
            (s.name.clone(), s.decl, s.fields.clone())
        };
        self.site(&struct_name, SiteKind::StructName, Some(struct_decl), false);
        self.out.put(" { ");
        let carrier = self.rng.below(fields.len());
        let mut order: Vec<usize> = (0..fields.len()).collect();
        for k in (1..order.len()).rev() {
            let j = self.rng.below(k + 1);
            order.swap(k, j);
        }
        for (k, &field) in order.iter().enumerate() {
            if k > 0 {
                self.out.put(", ");
            }
            self.site(
                &fields[field].0,
                SiteKind::Key,
                Some(fields[field].1),
                false,
            );
            self.out.put(": ");
            if field == carrier {
                self.tail_open();
                self.expr(1);
                self.out.put(&format!(", {})", value_of(d)));
            } else {
                self.expr(1);
            }
        }
        self.out.put(" };\n");
        self.declare_in_scope(
            &name,
            d,
            Kind::Record {
                structure,
                field: carrier,
            },
        );
    }

    fn header(&mut self) {
        // VAR log = [];
        self.out.kw("VAR");
        self.out.put(" ");
        let log = self.declare("log", Role::Helper);
        self.out.put(" = [];\n");
        self.declare_in_scope("log", log, Kind::Hidden);
        // FN note(s, v) { push(log, [s, v]); RETURN v; }
        self.log_decl = log;
        self.out.kw("FN");
        self.out.put(" ");
        self.note = self.declare("note", Role::Helper);
        self.out.put("(");
        self.out.kw("MUT");
        self.out.put(" ");
        let l = self.declare("l", Role::Param);
        self.out.put(", ");
        let s = self.declare("s", Role::Param);
        self.out.put(", ");
        let v = self.declare("v", Role::Param);
        self.out.put(") { ");
        self.site("push", SiteKind::Builtin, None, false);
        self.out.put("(");
        self.site("l", SiteKind::Value, Some(l), false);
        self.out.put(", [");
        self.site("s", SiteKind::Value, Some(s), false);
        self.out.put(", ");
        self.site("v", SiteKind::Value, Some(v), false);
        self.out.put("]); ");
        self.out.kw("RETURN");
        self.out.put(" ");
        self.site("v", SiteKind::Value, Some(v), false);
        self.out.put("; }\n");
        // FN tail(a, b) { RETURN b; }
        self.out.kw("FN");
        self.out.put(" ");
        self.tail = self.declare("tail", Role::Helper);
        self.out.put("(");
        self.declare("a", Role::Param);
        self.out.put(", ");
        let b = self.declare("b", Role::Param);
        self.out.put(") { ");
        self.out.kw("RETURN");
        self.out.put(" ");
        self.site("b", SiteKind::Value, Some(b), false);
        self.out.put("; }\n");
    }
}

fn generate(seed: u64) -> Generated {
    let mut gen = Gen {
        rng: Rng::new(seed),
        out: Out::new(),
        decls: Vec::new(),
        sites: Vec::new(),
        scopes: vec![Vec::new()],
        fn_base: 0,
        functions: Vec::new(),
        structs: Vec::new(),
        names: 0,
        note: 0,
        tail: 0,
        log_name: "log".to_string(),
        log_decl: 0,
        shadowable: Vec::new(),
    };
    gen.header();
    for _ in 0..(7 + gen.rng.below(9)) {
        gen.stmt(0, true);
    }
    let log = gen.decls.iter().position(|d| d.name == "log").expect("log");
    gen.site("log", SiteKind::Value, Some(log), false);
    gen.out.put(";\n");
    Generated {
        text: gen.out.text,
        decls: gen.decls,
        sites: gen.sites,
    }
}

// ── 2. исполнение: что прочитала виртуальная машина ───────────────────────

/// Журнал выполнения: пары «номер места, прочитанное число».
fn execute(text: &str) -> Result<Vec<(usize, i64)>, String> {
    match sga::run_source_with_options(text, RunOptions::new()) {
        Ok(Value::Array(log)) => log
            .borrow()
            .iter()
            .map(|entry| match entry {
                Value::Array(pair) => match pair.borrow().as_slice() {
                    [Value::Int(site), Value::Int(value)] => Ok((*site as usize, *value)),
                    other => Err(format!("запись журнала {other:?}")),
                },
                other => Err(format!("запись журнала {other:?}")),
            })
            .collect(),
        other => Err(format!("программа вернула не журнал: {other:?}")),
    }
}

#[derive(Default, Debug)]
struct Stats {
    programs: usize,
    invalid: usize,
    declarations: usize,
    sites: usize,
    observed_sites: usize,
    observable_sites: usize,
    by_kind: BTreeMap<String, usize>,
}

fn describe(program: &Generated, site: usize) -> String {
    let s = &program.sites[site];
    format!(
        "место №{site} `{}` ({:?}) на {}:{}: {}",
        s.name,
        s.kind,
        s.at.0,
        s.at.1,
        line_of(&program.text, s.at.0).trim()
    )
}

fn location(doc: &Document, at: Pos) -> Option<(Pos, Pos)> {
    nav::definition(doc, at).map(|l| (l.start, l.end))
}

/// Сверяет `definition` с моделью и исполнением на одной программе.
fn check_definitions(
    label: &str,
    program: &Generated,
    stats: &mut Stats,
    failures: &mut Vec<String>,
) {
    let issues = diagnostics::analyze_source(&program.text);
    if !issues.is_empty() {
        stats.invalid += 1;
        return;
    }
    let log = match execute(&program.text) {
        Ok(log) => log,
        Err(error) => {
            failures.push(format!(
                "{label}: корректная по диагностикам программа не выполнилась: {error}"
            ));
            return;
        }
    };
    stats.programs += 1;
    let doc = Document::new(&program.text);

    // объявление ведёт само к себе — с любой позиции внутри имени
    for decl in &program.decls {
        let want = Some((decl.at, decl.end()));
        for offset in 0..decl.name.chars().count() {
            let at = (decl.at.0, decl.at.1 + offset);
            let got = location(&doc, at);
            if got != want {
                failures.push(format!(
                    "{label}: объявление `{}` ({:?}) на {}:{} с позиции +{offset}: ждали {want:?}, получили {got:?}\n{}",
                    decl.name,
                    decl.role,
                    decl.at.0,
                    decl.at.1,
                    line_of(&program.text, decl.at.0)
                ));
            }
        }
        stats.declarations += 1;
    }

    // что прочитало исполнение
    let mut seen: HashMap<usize, BTreeSet<i64>> = HashMap::new();
    for (site, value) in &log {
        seen.entry(*site).or_default().insert(*value);
    }

    for (index, site) in program.sites.iter().enumerate() {
        stats.sites += 1;
        let want = site.expected.map(|d| {
            let decl = &program.decls[d];
            (decl.at, decl.end())
        });
        let key = format!(
            "{:?}->{}",
            site.kind,
            site.expected.map_or("нет".to_string(), |d| format!(
                "{:?}",
                program.decls[d].role
            ))
        );
        *stats.by_kind.entry(key).or_default() += 1;

        // модель против исполнения: если место выполнялось, оно прочитало
        // число объявления, на которое указывает модель
        if site.observable {
            stats.observable_sites += 1;
        }
        if let Some(values) = seen.get(&index) {
            stats.observed_sites += 1;
            let expected = site.expected.map(value_of);
            let agreed = site.observable
                && values.len() == 1
                && expected.is_some_and(|e| values.contains(&e));
            if !agreed {
                failures.push(format!(
                    "{label}: ГЕНЕРАТОР или исполнение: {}: модель ждёт {expected:?}, исполнение прочитало {values:?}",
                    describe(program, index)
                ));
                continue;
            }
        }

        // сам ответ сервера: с первой буквы слова и из его середины
        let length = site.name.chars().count();
        for offset in 0..length {
            let at = (site.at.0, site.at.1 + offset);
            let got = location(&doc, at);
            if got != want {
                failures.push(format!(
                    "{label}: {} (+{offset}): ждали {want:?}, получили {got:?}",
                    describe(program, index)
                ));
                break;
            }
        }
    }
}

fn random_programs(seeds: std::ops::Range<u64>, minimum_valid: usize) {
    let mut stats = Stats::default();
    let mut failures = Vec::new();
    for seed in seeds {
        let program = generate(seed);
        check_definitions(
            &format!("зерно {seed}"),
            &program,
            &mut stats,
            &mut failures,
        );
    }
    eprintln!("{stats:#?}");
    assert_no_failures(&failures);
    assert!(
        stats.programs >= minimum_valid,
        "генератор даёт слишком мало корректных программ: {stats:?}"
    );
    assert!(
        stats.observed_sites * 100 >= stats.observable_sites * 55,
        "исполнение должно видеть большинство мест: {stats:?}"
    );
}

#[test]
fn definition_matches_the_real_execution_on_random_programs_a() {
    random_programs(1..161, 120);
}

#[test]
fn definition_matches_the_real_execution_on_random_programs_b() {
    random_programs(1_000..1_160, 120);
}

// ── 3. символы: диапазоны против настоящего AST ───────────────────────────

/// Токены, их границы и дерево настоящих лексера и парсера.
struct Parsed {
    tokens: Vec<Token>,
    spans: Vec<TokenSpan>,
    program: Vec<Stmt>,
}

fn parse(text: &str) -> Option<Parsed> {
    let (tokens, spans) = Lexer::new(text).tokenize_with_spans().ok()?;
    let mut parser = Parser::new(tokens.clone());
    let program = parser.parse_program().ok()?;
    Some(Parsed {
        tokens,
        spans,
        program,
    })
}

/// Что должен ответить `documentSymbol`, если судить по настоящему AST и
/// таблице границ токенов лексера.
#[derive(Debug, PartialEq)]
struct Expected {
    name: String,
    kind: SymbolKind,
    detail: Option<String>,
    range: (Pos, Pos),
    selection: (Pos, Pos),
    children: Vec<Expected>,
}

/// Как аннотация записана в исходнике (ASCII-имена пишутся строчными, тип
/// `nil` — SGA-словом `NIL`; сервер печатает аннотацию так, как её написали).
fn type_text(ty: &TypeAnnotation) -> String {
    match ty {
        TypeAnnotation::Nil => "NIL".to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn expected_symbols(parsed: &Parsed) -> Vec<Expected> {
    let bounds = |first: usize, last: usize| -> (Pos, Pos) {
        let a = &parsed.spans[first];
        let b = &parsed.spans[last];
        ((a.line, a.col), (b.end_line, b.end_col))
    };
    let mut out = Vec::new();
    for stmt in &parsed.program {
        assert_ne!(stmt.span, sga::ast::Span::NONE, "у оператора нет границ");
        let first = stmt.span.first as usize;
        let last = stmt.span.last as usize;
        match &stmt.kind {
            StmtKind::FnDecl {
                name,
                params,
                return_ty,
                ..
            } => {
                let mut detail = String::from("(");
                for (k, param) in params.iter().enumerate() {
                    if k > 0 {
                        detail.push_str(", ");
                    }
                    if param.mutable {
                        detail.push_str("MUT ");
                    }
                    detail.push_str(&param.name);
                    if let Some(ty) = &param.ty {
                        detail.push_str(": ");
                        detail.push_str(&type_text(ty));
                    }
                }
                detail.push(')');
                if let Some(ty) = return_ty {
                    detail.push_str(" -> ");
                    detail.push_str(&type_text(ty));
                }
                out.push(Expected {
                    name: name.clone(),
                    kind: SymbolKind::Function,
                    detail: Some(detail),
                    range: bounds(first, last),
                    selection: bounds(first + 1, first + 1),
                    children: Vec::new(),
                });
            }
            StmtKind::StructDecl { name, fields } => {
                // поля — идентификаторы между `{` (first + 2) и `}` (last)
                let mut children = Vec::new();
                for i in first + 3..last {
                    if let TokenKind::Ident(field) = &parsed.tokens[i].kind {
                        let at = bounds(i, i);
                        children.push(Expected {
                            name: field.clone(),
                            kind: SymbolKind::Field,
                            detail: None,
                            range: at,
                            selection: at,
                            children: Vec::new(),
                        });
                    }
                }
                let names: Vec<&String> = children.iter().map(|c| &c.name).collect();
                assert_eq!(
                    names,
                    fields.iter().collect::<Vec<_>>(),
                    "поля из токенов расходятся с AST у `{name}`"
                );
                out.push(Expected {
                    name: name.clone(),
                    kind: SymbolKind::Struct,
                    detail: None,
                    range: bounds(first, last),
                    selection: bounds(first + 1, first + 1),
                    children,
                });
            }
            StmtKind::VarDecl { name, ty, .. } => {
                let (mnemonic, kind) = match parsed.tokens[first].kind {
                    TokenKind::Let => ("LET", SymbolKind::Variable),
                    TokenKind::Var => ("VAR", SymbolKind::Variable),
                    TokenKind::Const => ("CONST", SymbolKind::Constant),
                    ref other => panic!("объявление переменной начинается с {other:?}"),
                };
                let detail = match ty {
                    Some(ty) => format!("{mnemonic}: {}", type_text(ty)),
                    None => mnemonic.to_string(),
                };
                out.push(Expected {
                    name: name.clone(),
                    kind,
                    detail: Some(detail),
                    range: bounds(first, last),
                    selection: bounds(first + 1, first + 1),
                    children: Vec::new(),
                });
            }
            _ => {}
        }
    }
    out
}

fn to_expected(symbol: &Symbol) -> Expected {
    Expected {
        name: symbol.name.clone(),
        kind: symbol.kind,
        detail: symbol.detail.clone(),
        range: (symbol.start, symbol.end),
        selection: (symbol.name_start, symbol.name_end),
        children: symbol.children.iter().map(to_expected).collect(),
    }
}

fn count_expected(list: &[Expected]) -> usize {
    list.iter().map(|e| 1 + count_expected(&e.children)).sum()
}

#[derive(Default, Debug)]
struct SymbolStats {
    programs: usize,
    unparsed: usize,
    symbols: usize,
}

/// Сверяет символы сервера с AST. `false` — программа не разобралась.
fn check_symbols(
    label: &str,
    text: &str,
    stats: &mut SymbolStats,
    failures: &mut Vec<String>,
) -> bool {
    let Some(parsed) = parse(text) else {
        stats.unparsed += 1;
        return false;
    };
    let expected = expected_symbols(&parsed);
    let doc = Document::new(text);
    let actual: Vec<Expected> = nav::document_symbols(&doc)
        .iter()
        .map(to_expected)
        .collect();
    stats.programs += 1;
    stats.symbols += count_expected(&expected);
    if actual != expected {
        let first = actual
            .iter()
            .zip(&expected)
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| actual.len().min(expected.len()));
        failures.push(format!(
            "{label}: символы расходятся с AST (сервер {}, AST {}), первое расхождение №{first}:\n  сервер: {:?}\n  AST:    {:?}",
            actual.len(),
            expected.len(),
            actual.get(first),
            expected.get(first)
        ));
    }
    true
}

#[test]
fn symbols_match_the_real_ast_on_the_examples() {
    let examples = example_sources();
    assert_eq!(examples.len(), 17, "в examples/ должно быть 17 примеров");
    let mut stats = SymbolStats::default();
    let mut failures = Vec::new();
    for (name, text) in &examples {
        assert!(
            check_symbols(name, text, &mut stats, &mut failures),
            "{name}: пример не разобрался"
        );
    }
    eprintln!("examples: {stats:?}");
    assert_no_failures(&failures);
    assert_eq!(stats.programs, 17);
    assert!(stats.symbols > 40, "проверка вырождена: {stats:?}");
}

/// Рукописные программы: каждая бьёт в одну особенность символов.
const SYMBOL_PROGRAMS: &[(&str, &str)] = &[
    ("пустая программа", ""),
    ("только комментарий", "// ничего\n"),
    (
        "виды переменных и аннотации типов",
        "@LET a = 1;\n@VAR b: int = 2;\n@CONST c: string = \"x\";\n@LET d: array = [1, 2];\n@VAR e: closure = @FN(x) { @RETURN x; };\n@LET f: any = 3;\n@LET g: float = 1.5;\n@LET h: bool = @TRUE;\n@LET n: @NIL = @NIL;\n",
    ),
    (
        "функции: параметры, MUT, типы, результат",
        "@FN f() { @RETURN 1; }\n@FN g(a: int, @MUT b, c: string) -> bool { @LET inner = 1; @IF a > 0 { @LET deeper = 2; } @RETURN @TRUE; }\n@FN h(@MUT v: array) -> int { @RETURN 0; }\n",
    ),
    (
        "структуры с полями",
        "@STRUCT One { a }\n@STRUCT Two { a, b }\n@STRUCT Many { a, b, c, d, e }\n",
    ),
    (
        "объявления на нескольких строках",
        "@STRUCT P {\n    x,\n    y\n}\n@FN add(a,\n        b) {\n    @RETURN a + b;\n}\n@LET long =\n    1 +\n    2;\n",
    ),
    (
        "переменные внутри блоков не символы",
        "{ @LET a = 1; }\n@IF @TRUE { @LET b = 2; }\n@FOR i @IN 0..2 { @LET c = i; }\n@WHILE @FALSE { @VAR d = 1; }\n@LET top = 0;\n",
    ),
    ("последний оператор без точки с запятой", "@LET a = 1;\n@LET b = 2"),
    (
        "структура, литерал и метод",
        "@STRUCT P { x, y }\n@LET p = P { x: 1, y: 2 };\n@FN P_sum(self) { @RETURN self.x + self.y; }\n",
    ),
    (
        "кириллица и эмодзи в строках до объявлений",
        "@LET s = \"привет 😀\"; @LET t = 1;\n// 😀 комментарий\n@FN f(x) { @RETURN x; }\n",
    ),
    (
        "IMPORT и объявления",
        "@IMPORT \"m.sga\";\n@LET a = 1;\n@FN f() { @RETURN a; }\n",
    ),
    (
        "MATCH и замыкания в инициализаторах",
        "@LET m = @MATCH 1 { 1 -> 2, n -> n };\n@LET c = @FN(a) { @RETURN @FN(b) { @RETURN a + b; }; };\n",
    ),
    (
        "переводы строк CRLF",
        "@LET a = 1;\r\n@FN f() {\r\n    @RETURN a;\r\n}\r\n@STRUCT S { x,\r\n y }\r\n",
    ),
    (
        "оператор выражения между объявлениями",
        "@LET a = 1;\nprint(a);\n@FN f() { @RETURN 1; }\nf();\n@LET b = f();\n",
    ),
];

#[test]
fn symbols_match_the_real_ast_on_handwritten_programs() {
    let mut stats = SymbolStats::default();
    let mut failures = Vec::new();
    for (title, template) in SYMBOL_PROGRAMS {
        assert!(
            check_symbols(title, &sga(template), &mut stats, &mut failures),
            "{title}: программа не разобралась"
        );
    }
    eprintln!("handwritten: {stats:?}");
    assert_no_failures(&failures);
    assert_eq!(stats.programs, SYMBOL_PROGRAMS.len());
    assert!(stats.symbols > 40, "{stats:?}");
}

#[test]
fn symbols_match_the_real_ast_on_random_programs() {
    let mut stats = SymbolStats::default();
    let mut failures = Vec::new();
    for seed in 1..=300 {
        let program = generate(seed);
        check_symbols(
            &format!("зерно {seed}"),
            &program.text,
            &mut stats,
            &mut failures,
        );
    }
    eprintln!("random: {stats:?}");
    assert_no_failures(&failures);
    assert!(stats.programs >= 280, "слишком мало разобранных: {stats:?}");
    assert!(stats.symbols > 1_500, "{stats:?}");
}

/// Позиция лексера (символы) -> позиция LSP (строка, единицы UTF-16) — независимо
/// от `lsp::position`.
fn utf16_position(text: &str, pos: Pos) -> (usize, usize) {
    let line = text.split('\n').nth(pos.0 - 1).unwrap_or("");
    let units: usize = line.chars().take(pos.1 - 1).map(char::len_utf16).sum();
    (pos.0 - 1, units)
}

fn kind_number(kind: SymbolKind) -> i64 {
    match kind {
        SymbolKind::Function => 12,
        SymbolKind::Struct => 23,
        SymbolKind::Field => 8,
        SymbolKind::Variable => 13,
        SymbolKind::Constant => 14,
    }
}

fn flatten_expected(list: &[Expected], text: &str, depth: usize, out: &mut Vec<String>) {
    for e in list {
        let span = |(a, b): (Pos, Pos)| {
            let (al, ac) = utf16_position(text, a);
            let (bl, bc) = utf16_position(text, b);
            format!("{al}:{ac}-{bl}:{bc}")
        };
        out.push(format!(
            "{depth}|{}|{}|{}|{}|{}",
            e.name,
            kind_number(e.kind),
            e.detail.clone().unwrap_or_default(),
            span(e.range),
            span(e.selection)
        ));
        flatten_expected(&e.children, text, depth + 1, out);
    }
}

fn flatten_json(list: &[Json], depth: usize, out: &mut Vec<String>) {
    for s in list {
        let pos = |v: &Json| {
            format!(
                "{}:{}",
                v.get("line").and_then(Json::as_i64).expect("line"),
                v.get("character")
                    .and_then(Json::as_i64)
                    .expect("character")
            )
        };
        let range = |key: &str| {
            let r = s.get(key).unwrap_or_else(|| panic!("нет {key}: {s}"));
            format!(
                "{}-{}",
                pos(r.get("start").expect("start")),
                pos(r.get("end").expect("end"))
            )
        };
        out.push(format!(
            "{depth}|{}|{}|{}|{}|{}",
            s.get("name").and_then(Json::as_str).expect("name"),
            s.get("kind").and_then(Json::as_i64).expect("kind"),
            s.get("detail").and_then(Json::as_str).unwrap_or(""),
            range("range"),
            range("selectionRange")
        ));
        if let Some(children) = s.get("children").and_then(Json::as_array) {
            flatten_json(children, depth + 1, out);
        }
    }
}

// ── 3а. разговор с сервером ────────────────────────────────────────────────

const URI: &str = "file:///nav.sga";

/// Сервер без анализа диагностик (проверяется только навигация) с открытым
/// документом.
struct Session {
    server: Server,
    next_id: i64,
}

impl Session {
    fn start(hierarchical: bool, text: &str) -> Session {
        let mut server = Server::with_analyzer(Box::new(|_| Vec::new()));
        let capabilities = if hierarchical {
            r#"{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}}"#
        } else {
            "{}"
        };
        let init = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"processId":null,"capabilities":{capabilities}}}}}"#
        );
        server.handle_message(init.as_bytes());
        server.handle_message(br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        let open = Json::object([
            ("jsonrpc", Json::string("2.0")),
            ("method", Json::string("textDocument/didOpen")),
            (
                "params",
                Json::object([(
                    "textDocument",
                    Json::object([
                        ("uri", Json::string(URI)),
                        ("version", Json::Int(1)),
                        ("text", Json::string(text)),
                    ]),
                )]),
            ),
        ])
        .to_string();
        server.handle_message(open.as_bytes());
        Session {
            server,
            next_id: 10,
        }
    }

    fn call(&mut self, method: &str, params: Json) -> Json {
        self.next_id += 1;
        let body = Json::object([
            ("jsonrpc", Json::string("2.0")),
            ("id", Json::Int(self.next_id)),
            ("method", Json::string(method)),
            ("params", params),
        ])
        .to_string();
        let out = self.server.handle_message(body.as_bytes());
        assert_eq!(out.len(), 1, "{method}: ответов {}", out.len());
        assert_eq!(out[0].get("id"), Some(&Json::Int(self.next_id)));
        assert!(out[0].get("error").is_none(), "{method}: {}", out[0]);
        out[0].get("result").cloned().expect("result")
    }

    fn symbols(&mut self) -> Json {
        self.call(
            "textDocument/documentSymbol",
            Json::object([("textDocument", Json::object([("uri", Json::string(URI))]))]),
        )
    }

    fn definition(&mut self, line: usize, character: usize) -> Json {
        self.call(
            "textDocument/definition",
            Json::object([
                ("textDocument", Json::object([("uri", Json::string(URI))])),
                (
                    "position",
                    Json::object([
                        ("line", Json::Int(line as i64)),
                        ("character", Json::Int(character as i64)),
                    ]),
                ),
            ]),
        )
    }
}

#[test]
fn symbol_positions_in_the_protocol_answer_are_utf16_and_match_the_ast() {
    // примеры содержат кириллицу и эмодзи в строках и комментариях: колонки в
    // ответе — единицы UTF-16, считаются здесь заново, независимо от сервера
    let mut checked = 0;
    let mut corpus: Vec<(String, String)> = example_sources();
    corpus.push((
        "эмодзи до объявления".to_string(),
        sga("@LET s = \"😀😀\"; @LET t = 1; @FN f(x) { @RETURN \"😀\"; }\n@STRUCT S { a, b }\n"),
    ));
    for (name, text) in &corpus {
        let parsed = parse(text).unwrap_or_else(|| panic!("{name}: документ не разобрался"));
        let mut expected = Vec::new();
        flatten_expected(&expected_symbols(&parsed), text, 0, &mut expected);
        let mut hierarchical = Session::start(true, text);
        let answer = hierarchical.symbols();
        let mut got = Vec::new();
        flatten_json(answer.as_array().expect("массив"), 0, &mut got);
        assert_eq!(got, expected, "{name}: иерархический ответ");
        checked += expected.len();

        // плоский ответ: те же символы в том же порядке обхода, с `containerName`
        let mut flat = Session::start(false, text);
        let list = flat.symbols();
        let list = list.as_array().expect("массив");
        assert_eq!(list.len(), expected.len(), "{name}: плоский ответ");
        for (entry, line) in list.iter().zip(&expected) {
            let name_part = line.split('|').nth(1).expect("имя");
            assert_eq!(entry.get("name").and_then(Json::as_str), Some(name_part));
            assert!(entry.get("location").is_some() && entry.get("children").is_none());
        }
    }
    assert!(checked > 40, "{checked}");
}

// ── 4. рукописные программы: метки, выполнение, диагностики ───────────────
//
// Метка ставится ПЕРЕД именем и вырезается из текста перед разбором:
//
//     ⟦D3⟧  — объявление №3 (имя, которое объявляется здесь);
//     ⟦U3⟧  — использование, которое обязано вести к объявлению №3;
//     ⟦U0⟧  — использование, у которого определения нет.
//
// Программа с `runs` выполняется по-настоящему: она сама складывает в массив `r`
// числа, прочитанные из объявлений, и последним оператором возвращает его.
// Итог выполнения — независимое от сканера свидетельство того, КАКОЕ
// объявление читает использование. `accepted` — вердикт анализатора
// диагностик (`semantic`), которому сканер обязан соответствовать: программа,
// где имя не разрешается, отклоняется, и определения у такого имени нет.

struct Case {
    title: &'static str,
    template: &'static str,
    /// Ожидаемый итог выполнения; `None` — программа только анализируется.
    runs: Option<&'static [i64]>,
    /// Принимает ли программу анализатор диагностик.
    accepted: bool,
}

struct Marked {
    text: String,
    declarations: BTreeMap<usize, Pos>,
    uses: Vec<(usize, Pos)>,
}

/// Вырезает метки и запоминает позиции (строка и колонка в символах, с единицы,
/// как у лексера) имён, перед которыми они стояли.
fn strip_marks(template: &str) -> Marked {
    let mut text = String::new();
    let mut declarations = BTreeMap::new();
    let mut uses = Vec::new();
    let (mut line, mut col) = (1usize, 1usize);
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c == '⟦' {
            let mut label = String::new();
            loop {
                match chars.next() {
                    Some('⟧') => break,
                    Some(next) => label.push(next),
                    None => panic!("метка не закрыта в {template:?}"),
                }
            }
            let kind = label.chars().next();
            let number: usize = label
                .get(1..)
                .and_then(|digits| digits.parse().ok())
                .unwrap_or_else(|| panic!("метка ⟦{label}⟧ в {template:?}"));
            match kind {
                Some('D') => {
                    assert!(number > 0, "объявления нумеруются с единицы: ⟦{label}⟧");
                    assert!(
                        declarations.insert(number, (line, col)).is_none(),
                        "метка ⟦{label}⟧ повторена в {template:?}"
                    );
                }
                Some('U') => uses.push((number, (line, col))),
                _ => panic!("метка ⟦{label}⟧ в {template:?}"),
            }
            continue;
        }
        text.push(c);
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    Marked {
        text,
        declarations,
        uses,
    }
}

/// Имя идентификатора, который лексер начинает ровно в `pos`.
fn identifier_at(doc: &Document, pos: Pos) -> Option<String> {
    doc.tokens
        .iter()
        .find(|token| token.start() == pos)
        .and_then(|token| match &token.kind {
            TokenKind::Ident(name) => Some(name.clone()),
            _ => None,
        })
}

/// Итог выполнения: массив целых.
fn execute_ints(text: &str) -> Result<Vec<i64>, String> {
    match sga::run_source_with_options(text, RunOptions::new()) {
        Ok(Value::Array(items)) => items
            .borrow()
            .iter()
            .map(|item| match item {
                Value::Int(n) => Ok(*n),
                other => Err(format!("в итоговом массиве не число: {other:?}")),
            })
            .collect(),
        other => Err(format!("итог — не массив: {other:?}")),
    }
}

#[derive(Default, Debug)]
struct CaseStats {
    cases: usize,
    executed: usize,
    rejected: usize,
    declarations: usize,
    uses: usize,
    null_uses: usize,
}

/// Ответ `definition` с каждой позиции внутри слова: с первой буквы и из
/// середины; `Some` — описание первого расхождения.
fn check_word(
    doc: &Document,
    text: &str,
    title: &str,
    what: &str,
    pos: Pos,
    length: usize,
    want: Option<(Pos, Pos)>,
) -> Option<String> {
    for offset in 0..length {
        let at = (pos.0, pos.1 + offset);
        let got = location(doc, at);
        if got != want {
            return Some(format!(
                "{title}: {what} на {}:{} (+{offset}): ждали {want:?}, получили {got:?}\n{}",
                pos.0,
                pos.1,
                line_of(text, pos.0)
            ));
        }
    }
    None
}

fn check_case(case: &Case, stats: &mut CaseStats, failures: &mut Vec<String>) {
    let title = case.title;
    let marked = strip_marks(&sga(case.template));
    let text = &marked.text;
    let doc = Document::new(text);
    stats.cases += 1;

    // вердикт анализатора диагностик
    let issues = diagnostics::analyze_source(text);
    if issues.is_empty() != case.accepted {
        failures.push(format!(
            "{title}: анализатор {} программу, ждали обратного",
            if issues.is_empty() {
                "принял"
            } else {
                "отклонил"
            }
        ));
    }
    if !case.accepted {
        stats.rejected += 1;
    }

    // настоящее выполнение
    if let Some(expected) = case.runs {
        assert!(case.accepted, "{title}: отклонённую программу не выполняют");
        match execute_ints(text) {
            Ok(got) if got == expected => stats.executed += 1,
            other => failures.push(format!(
                "{title}: выполнение вернуло {other:?}, ждали {expected:?}"
            )),
        }
    }

    let length_at = |pos: Pos| identifier_at(&doc, pos).map(|word| word.chars().count());

    for (&number, &pos) in &marked.declarations {
        let Some(length) = length_at(pos) else {
            failures.push(format!(
                "{title}: метка D{number} на {}:{} стоит не на идентификаторе",
                pos.0, pos.1
            ));
            continue;
        };
        stats.declarations += 1;
        failures.extend(check_word(
            &doc,
            text,
            title,
            &format!("объявление D{number}"),
            pos,
            length,
            Some((pos, (pos.0, pos.1 + length))),
        ));
    }
    for &(number, pos) in &marked.uses {
        let Some(length) = length_at(pos) else {
            failures.push(format!(
                "{title}: метка U{number} на {}:{} стоит не на идентификаторе",
                pos.0, pos.1
            ));
            continue;
        };
        stats.uses += 1;
        let want = if number == 0 {
            stats.null_uses += 1;
            None
        } else {
            let decl = *marked
                .declarations
                .get(&number)
                .unwrap_or_else(|| panic!("{title}: у метки U{number} нет метки D{number}"));
            let decl_length = length_at(decl).unwrap_or(0);
            Some((decl, (decl.0, decl.1 + decl_length)))
        };
        failures.extend(check_word(
            &doc,
            text,
            title,
            &format!("использование U{number}"),
            pos,
            length,
            want,
        ));
    }
}

const CASES: &[Case] = &[
    Case {
        title: "функция и замыкание с одним именем: вызов идёт к функции, она объявлена первой",
        template: "@VAR r = [];\n@FN ⟦D1⟧f() { @RETURN 1; }\n@LET ⟦D2⟧f = @FN() { @RETURN 2; };\npush(r, ⟦U1⟧f());\nr;\n",
        runs: Some(&[1]),
        accepted: true,
    },
    Case {
        title: "функция и замыкание с одним именем: вызов идёт к функции, она объявлена второй",
        template: "@VAR r = [];\n@LET ⟦D2⟧f = @FN() { @RETURN 2; };\n@FN ⟦D1⟧f() { @RETURN 1; }\npush(r, ⟦U1⟧f());\nr;\n",
        runs: Some(&[1]),
        accepted: true,
    },
    Case {
        title: "вызов переменной-замыкания",
        template: "@VAR r = [];\n@LET ⟦D2⟧g = @FN(a) { @RETURN a + 2; };\npush(r, ⟦U2⟧g(0));\nr;\n",
        runs: Some(&[2]),
        accepted: true,
    },
    Case {
        title: "имя функции без вызова — не значение: определения нет",
        template: "@VAR r = [];\n@FN f() { @RETURN 1; }\npush(r, ⟦U0⟧f);\nr;\n",
        runs: None,
        accepted: false,
    },
    Case {
        title: "builtin выигрывает у одноимённой функции документа",
        template: "@VAR r = [];\n@FN ⟦D1⟧len(a) { @RETURN 100; }\npush(r, ⟦U0⟧len([1, 2, 3]));\nr;\n",
        runs: Some(&[3]),
        accepted: true,
    },
    Case {
        title: "затенение: внешняя переменная до внутренней, внутренняя, снова внешняя",
        template: "@VAR r = [];\n@LET ⟦D1⟧x = 1;\n{\n    push(r, ⟦U1⟧x);\n    @LET ⟦D2⟧x = 2;\n    push(r, ⟦U2⟧x);\n}\npush(r, ⟦U1⟧x);\nr;\n",
        runs: Some(&[1, 2, 1]),
        accepted: true,
    },
    Case {
        title: "инициализатор не видит собственного объявления",
        template: "@VAR r = [];\n@LET ⟦D1⟧x = 1;\n{\n    @LET ⟦D2⟧x = ⟦U1⟧x + 1;\n    push(r, ⟦U2⟧x);\n}\nr;\n",
        runs: Some(&[2]),
        accepted: true,
    },
    Case {
        title: "параметр затеняет глобальную переменную",
        template: "@VAR r = [];\n@LET ⟦D1⟧v = 1;\n@FN f(⟦D2⟧v) { @RETURN ⟦U2⟧v; }\npush(r, f(2));\npush(r, ⟦U1⟧v);\nr;\n",
        runs: Some(&[2, 1]),
        accepted: true,
    },
    Case {
        title: "замыкание читает захваченную переменную и свой параметр",
        template: "@VAR r = [];\n@LET ⟦D1⟧a = 10;\n@LET c = @FN(⟦D2⟧b) { @RETURN ⟦U1⟧a + ⟦U2⟧b; };\npush(r, c(2));\nr;\n",
        runs: Some(&[12]),
        accepted: true,
    },
    Case {
        title: "вложенные замыкания видят все внешние имена",
        template: "@VAR r = [];\n@LET ⟦D1⟧a = 10;\n@LET ⟦D4⟧outer = @FN(⟦D2⟧b) { @RETURN @FN(⟦D3⟧c) { @RETURN ⟦U1⟧a + ⟦U2⟧b + ⟦U3⟧c; }; };\n@LET ⟦D5⟧inner = ⟦U4⟧outer(2);\npush(r, ⟦U5⟧inner(3));\nr;\n",
        runs: Some(&[15]),
        accepted: true,
    },
    Case {
        title: "переменная цикла FOR и привязка MATCH",
        template: "@VAR r = [];\n@FOR ⟦D1⟧i @IN 5..7 { push(r, ⟦U1⟧i); }\n@LET m = @MATCH 6 { 7 -> 0, ⟦D2⟧n -> ⟦U2⟧n };\npush(r, m);\nr;\n",
        runs: Some(&[5, 6, 6]),
        accepted: true,
    },
    Case {
        title: "ключ литерала — поле структуры, значение — переменная, после точки определения нет",
        template: "@VAR r = [];\n@STRUCT ⟦D1⟧P { ⟦D2⟧x, ⟦D3⟧y }\n@LET ⟦D4⟧x = 10;\n@LET p = ⟦U1⟧P { ⟦U2⟧x: ⟦U4⟧x + 2, ⟦U3⟧y: 0 };\npush(r, p.⟦U0⟧x);\nr;\n",
        runs: Some(&[12]),
        accepted: true,
    },
    Case {
        title: "функция видна до своего объявления",
        template: "@VAR r = [];\npush(r, ⟦U1⟧f());\n@FN ⟦D1⟧f() { @RETURN 1; }\nr;\n",
        runs: Some(&[1]),
        accepted: true,
    },
    Case {
        title: "рекурсия: вызов внутри функции ведёт к ней самой",
        template: "@VAR r = [];\n@FN ⟦D1⟧fact(⟦D2⟧n) { @IF ⟦U2⟧n < 2 { @RETURN 1; } @RETURN ⟦U2⟧n * ⟦U1⟧fact(⟦U2⟧n - 1); }\npush(r, ⟦U1⟧fact(4));\nr;\n",
        runs: Some(&[24]),
        accepted: true,
    },
    Case {
        title: "`{` после условия — блок, а не литерал структуры",
        template: "@VAR r = [];\n@LET ⟦D1⟧ok = @TRUE;\n@IF ⟦U1⟧ok { push(r, 1); }\nr;\n",
        runs: Some(&[1]),
        accepted: true,
    },
    Case {
        title: "образец `_` в MATCH ничего не объявляет",
        template: "@VAR r = [];\npush(r, @MATCH 5 { 1 -> 0, ⟦U0⟧_ -> 9 });\nr;\n",
        runs: Some(&[9]),
        accepted: true,
    },
    Case {
        title: "аннотации типов определения не имеют",
        template: "@VAR r = [];\n@FN f(⟦D1⟧a: ⟦U0⟧int) -> ⟦U0⟧int { @RETURN ⟦U1⟧a; }\n@LET ⟦D2⟧b: ⟦U0⟧int = f(3);\npush(r, ⟦U2⟧b);\nr;\n",
        runs: Some(&[3]),
        accepted: true,
    },
    Case {
        title: "TD-028: функция читает глобальную переменную — так решает semantic",
        template: "@LET ⟦D1⟧g = 1;\n@FN f() { @RETURN ⟦U1⟧g; }\n",
        runs: None,
        accepted: true,
    },
    Case {
        title: "TD-028, контроль: та же программа с опечаткой в имени отклоняется",
        template: "@LET g = 1;\n@FN f() { @RETURN ⟦U0⟧gg; }\n",
        runs: None,
        accepted: false,
    },
    Case {
        title: "TD-029: переменная FOR затеняет внешнюю только внутри цикла",
        template: "@LET ⟦D1⟧i = 5;\n@FOR ⟦D2⟧i @IN 0..3 { @LET inside = ⟦U2⟧i; }\n@LET after = ⟦U1⟧i;\n",
        runs: None,
        accepted: true,
    },
    Case {
        title: "использование раньше объявления и неизвестные имена",
        template: "@LET a = ⟦U0⟧b;\n@LET ⟦D1⟧b = 1;\n@LET c = ⟦U0⟧nope();\n@LET d = ⟦U0⟧nothing;\n",
        runs: None,
        accepted: false,
    },
    Case {
        title: "переменная блока не видна после блока",
        template: "@IF @TRUE { @LET ⟦D1⟧inner = 1; @LET u = ⟦U1⟧inner; }\n@LET after = ⟦U0⟧inner;\n",
        runs: None,
        accepted: false,
    },
    Case {
        title: "замыкание не видит само себя в своём инициализаторе",
        template: "@LET ⟦D1⟧f = @FN(n) { @RETURN ⟦U0⟧f(n); };\n@LET g = ⟦U1⟧f;\n",
        runs: None,
        accepted: false,
    },
];

#[test]
fn definition_follows_the_marks_and_the_programs_run_as_the_marks_say() {
    let mut stats = CaseStats::default();
    let mut failures = Vec::new();
    for case in CASES {
        check_case(case, &mut stats, &mut failures);
    }
    eprintln!("{stats:?}");
    assert_no_failures(&failures);
    assert_eq!(stats.cases, CASES.len());
    assert!(stats.executed >= 15, "{stats:?}");
    assert!(stats.uses >= 40 && stats.null_uses >= 8, "{stats:?}");
    assert!(stats.declarations >= 28, "{stats:?}");
}

/// Контроль самой проверки: неверная метка, неверный итог и неверный вердикт
/// анализатора не должны проходить незамеченными.
#[test]
fn the_mark_checker_notices_a_wrong_mark_a_wrong_result_and_a_wrong_verdict() {
    let wrong_mark = Case {
        title: "метка ведёт не туда",
        template: "@LET ⟦D1⟧a = 1;\n@LET ⟦D2⟧b = 2;\n@LET c = ⟦U2⟧a;\n",
        runs: None,
        accepted: true,
    };
    let wrong_null = Case {
        title: "метка «нет определения» у имени с определением",
        template: "@LET a = 1;\n@LET c = ⟦U0⟧a;\n",
        runs: None,
        accepted: true,
    };
    let wrong_result = Case {
        title: "итог выполнения не тот",
        template: "@VAR r = [];\npush(r, 1);\nr;\n",
        runs: Some(&[2]),
        accepted: true,
    };
    let wrong_verdict = Case {
        title: "вердикт анализатора не тот",
        template: "@LET a = ⟦U0⟧b;\n",
        runs: None,
        accepted: true,
    };
    let not_an_identifier = Case {
        title: "метка стоит не на идентификаторе",
        template: "@LET a = ⟦U0⟧1;\n",
        runs: None,
        accepted: true,
    };
    for case in [
        wrong_mark,
        wrong_null,
        wrong_result,
        wrong_verdict,
        not_an_identifier,
    ] {
        let mut stats = CaseStats::default();
        let mut failures = Vec::new();
        check_case(&case, &mut stats, &mut failures);
        assert!(
            !failures.is_empty(),
            "{}: проверка пропустила заведомо неверный случай",
            case.title
        );
    }
}

// ── 5. испорченный текст ──────────────────────────────────────────────────
//
// Пользователь печатает: документ почти всегда не парсится. Навигация не имеет
// права паниковать и обязана держать инварианты на любом тексте:
//
//   * символы устроены корректно (имя внутри диапазона, ребёнок внутри
//     родителя, порядок, глубина ≤ MAX_SYMBOL_DEPTH) и каждый символ стоит на
//     идентификаторе, а `definition` на его имени возвращает то же имя;
//   * `definition` указывает ровно на идентификатор документа, а определение
//     определения — оно само;
//   * два вызова на одном тексте дают одно и то же.

const DAMAGE_WORDS: &[&str] = &[
    "LET", "VAR", "CONST", "FN", "STRUCT", "IF", "ELSE", "FOR", "IN", "WHILE", "MATCH", "RETURN",
    "MUT", "IMPORT",
];

const DAMAGE_JUNK: &[&str] = &[
    "{", "}", "(", ")", "[", "]", "\"", "//", "/*", ";", ",", ".", "..", "->", ":", "=", "\r\n",
    "\n", "\t", " ", "😀", "я", "\0", "\u{feff}", "x", "y_1", "P", "0", "1.5", "_",
];

/// Портит текст: усекает, вырезает, вставляет мусор или слово, заменяет символ,
/// меняет или дублирует строки; 1–4 порчи подряд.
fn damage(text: &str, rng: &mut Rng) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    for _ in 0..(1 + rng.below(4)) {
        let n = chars.len();
        match rng.below(7) {
            0 if n > 0 => chars.truncate(rng.below(n)),
            1 if n > 0 => {
                let from = rng.below(n);
                let to = (from + 1 + rng.below(20)).min(n);
                chars.drain(from..to);
            }
            2 | 3 => {
                let at = rng.below(n + 1);
                let piece = if rng.chance(50) {
                    sga(&format!("@{}", DAMAGE_WORDS[rng.below(DAMAGE_WORDS.len())]))
                } else {
                    DAMAGE_JUNK[rng.below(DAMAGE_JUNK.len())].to_string()
                };
                let tail = chars.split_off(at);
                chars.extend(piece.chars());
                chars.extend(tail);
            }
            4 if n > 0 => {
                let at = rng.below(n);
                let piece = DAMAGE_JUNK[rng.below(DAMAGE_JUNK.len())];
                chars[at] = piece.chars().next().unwrap_or(' ');
            }
            5 | 6 => {
                let joined: String = chars.iter().collect();
                let mut lines: Vec<&str> = joined.split('\n').collect();
                let a = rng.below(lines.len());
                let b = rng.below(lines.len());
                if rng.chance(50) {
                    lines.swap(a, b);
                } else {
                    let copy = lines[a];
                    lines.insert(b, copy);
                }
                chars = lines.join("\n").chars().collect();
            }
            _ => {}
        }
    }
    chars.into_iter().collect()
}

fn flatten_symbols<'a>(list: &'a [Symbol], depth: usize, out: &mut Vec<(&'a Symbol, usize)>) {
    for symbol in list {
        out.push((symbol, depth));
        flatten_symbols(&symbol.children, depth + 1, out);
    }
}

/// Нарушения устройства дерева символов.
fn symbol_problems(list: &[Symbol], parent: Option<&Symbol>, depth: usize, out: &mut Vec<String>) {
    if depth >= MAX_SYMBOL_DEPTH {
        out.push(format!("глубина {depth} не меньше {MAX_SYMBOL_DEPTH}"));
        return;
    }
    let mut previous: Option<Pos> = None;
    for s in list {
        if !(s.start <= s.name_start && s.name_start < s.name_end && s.name_end <= s.end) {
            out.push(format!(
                "`{}`: имя {:?}-{:?} не внутри диапазона {:?}-{:?}",
                s.name, s.name_start, s.name_end, s.start, s.end
            ));
        }
        if let Some(p) = parent {
            if !(p.start <= s.start && s.end <= p.end) {
                out.push(format!(
                    "`{}` {:?}-{:?} не внутри родителя `{}` {:?}-{:?}",
                    s.name, s.start, s.end, p.name, p.start, p.end
                ));
            }
        }
        if previous.is_some_and(|before| s.start < before) {
            out.push(format!("`{}` нарушает порядок обхода", s.name));
        }
        previous = Some(s.start);
        symbol_problems(&s.children, Some(s), depth + 1, out);
    }
}

#[derive(Default, Debug)]
struct DamageStats {
    texts: usize,
    parsable: usize,
    symbols: usize,
    probes: usize,
    found: usize,
}

/// `probes` — сколько идентификаторов опросить (все символы опрашиваются всегда).
fn check_damaged(
    label: &str,
    text: &str,
    probes: usize,
    rng: &mut Rng,
    stats: &mut DamageStats,
    failures: &mut Vec<String>,
) {
    stats.texts += 1;
    if parse(text).is_some() {
        stats.parsable += 1;
    }
    let doc = Document::new(text);
    let symbols = nav::document_symbols(&doc);
    if nav::document_symbols(&Document::new(text)) != symbols {
        failures.push(format!("{label}: два вызова document_symbols различаются"));
    }
    let mut problems = Vec::new();
    symbol_problems(&symbols, None, 0, &mut problems);
    let mut flat = Vec::new();
    flatten_symbols(&symbols, 0, &mut flat);
    stats.symbols += flat.len();
    for (symbol, _) in &flat {
        let on_identifier = doc.tokens.iter().any(|t| {
            t.start() == symbol.name_start
                && t.end() == symbol.name_end
                && matches!(&t.kind, TokenKind::Ident(name) if *name == symbol.name)
        });
        if !on_identifier {
            problems.push(format!(
                "`{}` не стоит на своём идентификаторе",
                symbol.name
            ));
        }
        let own = Some((symbol.name_start, symbol.name_end));
        if location(&doc, symbol.name_start) != own {
            problems.push(format!(
                "definition на имени символа `{}` ({:?}) вернул {:?}",
                symbol.name,
                symbol.name_start,
                location(&doc, symbol.name_start)
            ));
        }
    }
    for problem in problems.iter().take(3) {
        failures.push(format!("{label}: {problem}"));
    }

    // опрос идентификаторов: все, если их немного, иначе случайная выборка
    let identifiers: Vec<usize> = doc
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(t.kind, TokenKind::Ident(_)))
        .map(|(i, _)| i)
        .collect();
    let mut chosen: Vec<usize> = if identifiers.len() <= probes {
        identifiers.clone()
    } else {
        (0..probes)
            .map(|_| identifiers[rng.below(identifiers.len())])
            .collect()
    };
    if let Some(&last) = identifiers.last() {
        chosen.push(last); // курсор в самом конце набранного текста
    }
    for i in chosen {
        let token = &doc.tokens[i];
        let width = token.end().1.saturating_sub(token.start().1).max(1);
        for at in [
            token.start(),
            (token.start().0, token.start().1 + width / 2),
        ] {
            stats.probes += 1;
            let got = location(&doc, at);
            if let Some((a, b)) = got {
                stats.found += 1;
                let on_identifier = doc.tokens.iter().any(|t| {
                    t.start() == a && t.end() == b && matches!(t.kind, TokenKind::Ident(_))
                });
                if !on_identifier {
                    failures.push(format!(
                        "{label}: definition с {at:?} указал на {a:?}-{b:?} — это не идентификатор"
                    ));
                } else if location(&doc, a) != got {
                    failures.push(format!(
                        "{label}: определение {a:?} (с {at:?}) не ведёт само к себе: {:?}\n{}",
                        location(&doc, a),
                        excerpt(text)
                    ));
                }
            }
            if location(&doc, at) != got {
                failures.push(format!(
                    "{label}: два вызова definition с {at:?} различаются"
                ));
            }
        }
    }

    // позиции вне текста и на краях
    let lines = text.split('\n').count();
    for at in [
        (0, 0),
        (0, 1),
        (1, 0),
        (lines + 5, 1),
        (1, 1_000_000),
        (usize::MAX / 2, usize::MAX / 2),
        (usize::MAX, usize::MAX),
    ] {
        if let Some(found) = location(&doc, at) {
            failures.push(format!(
                "{label}: позиция вне текста {at:?} нашла {found:?}"
            ));
        }
    }
}

/// Начало текста для сообщения о расхождении.
fn excerpt(text: &str) -> String {
    let head: String = text.chars().take(700).collect();
    format!("{head:?}")
}

fn damage_corpus() -> Vec<(String, String)> {
    let mut corpus = example_sources();
    for (title, template) in SYMBOL_PROGRAMS {
        corpus.push((title.to_string(), sga(template)));
    }
    for case in CASES {
        corpus.push((
            case.title.to_string(),
            strip_marks(&sga(case.template)).text,
        ));
    }
    for seed in 1..=40 {
        corpus.push((format!("зерно {seed}"), generate(seed).text));
    }
    corpus
}

/// Раундов порчи: один по умолчанию; переменная окружения `SGA_NAV_FUZZ=N`
/// добавляет N раундов с новыми зёрнами для разового глубокого прогона.
fn damage_rounds() -> u64 {
    1 + std::env::var("SGA_NAV_FUZZ")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

#[test]
fn damaged_programs_keep_the_navigation_invariants() {
    let corpus = damage_corpus();
    let rounds = damage_rounds();
    let mut stats = DamageStats::default();
    let mut failures = Vec::new();
    for round in 0..rounds {
        let mut rng = Rng::new(2026 + round);
        for (name, text) in &corpus {
            check_damaged(name, text, 40, &mut rng, &mut stats, &mut failures);
            for variant in 0..24 {
                let broken = damage(text, &mut rng);
                check_damaged(
                    &format!("{name} / раунд {round} / порча {variant}"),
                    &broken,
                    24,
                    &mut rng,
                    &mut stats,
                    &mut failures,
                );
            }
        }
    }
    eprintln!("{stats:?}");
    assert_no_failures(&failures);
    assert!(stats.texts >= 1_800 * rounds as usize, "{stats:?}");
    assert!(
        stats.parsable * 10 < stats.texts * 4,
        "порча должна ломать большинство текстов: {stats:?}"
    );
    assert!(stats.symbols > 5_000 && stats.found > 5_000, "{stats:?}");
}

#[test]
fn every_prefix_of_a_program_keeps_the_navigation_invariants() {
    // состояния редактора во время набора: все начала текста, по символу
    let mut texts: Vec<(String, String)> = Vec::new();
    for (title, template) in SYMBOL_PROGRAMS.iter().skip(2).take(5) {
        texts.push((title.to_string(), sga(template)));
    }
    for case in CASES.iter().skip(5).take(6) {
        texts.push((
            case.title.to_string(),
            strip_marks(&sga(case.template)).text,
        ));
    }
    for seed in [3, 4, 5] {
        texts.push((format!("зерно {seed}"), generate(seed).text));
    }
    let mut rng = Rng::new(11);
    let mut stats = DamageStats::default();
    let mut failures = Vec::new();
    for (name, text) in &texts {
        let chars: Vec<char> = text.chars().collect();
        let step = (chars.len() / 400).max(1);
        for cut in (0..=chars.len()).step_by(step) {
            let prefix: String = chars[..cut].iter().collect();
            check_damaged(
                &format!("{name} / начало из {cut} символов"),
                &prefix,
                6,
                &mut rng,
                &mut stats,
                &mut failures,
            );
        }
    }
    eprintln!("{stats:?}");
    assert_no_failures(&failures);
    assert!(stats.texts > 1_000 && stats.found > 1_000, "{stats:?}");
}

// ── 6. документы предельного размера ──────────────────────────────────────
//
// Документ до `MAX_DOCUMENT_BYTES` (1 МиБ) — форма, которую выбирает не
// программист, а ошибка или злой умысел: сотни тысяч вложенных скобок,
// десятки тысяч объявлений в одной строке, структура со ста тысячами полей.
// Запросы идут через настоящий `Server` в потоке со стеком 512 КиБ (меньше
// обычного 2 МиБ потока тестов и 8 МиБ стека главного потока): рекурсия по
// размеру документа переполнила бы его. Время каждого запроса ограничено —
// квадратичная работа на таком тексте заняла бы минуты (первая версия
// `documentSymbol` с переводом каждой позиции заново занимала 19 с на 16 тысячах
// функций). Бюджет — 5 с на запрос в отладочной сборке (AC-08 T019); измерено:
// самый тяжёлый запрос — около секунды.

const SMALL_STACK: usize = 512 * 1024;
const BUDGET: Duration = Duration::from_secs(5);

fn on_small_stack<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("nav-small-stack".to_string())
        .stack_size(SMALL_STACK)
        .spawn(work)
        .expect("поток")
        .join()
        .expect("работа на малом стеке завершилась паникой")
}

/// Число символов в токенах `text[..byte]` — колонка (с нуля) в строке.
fn column_of(text: &str, byte: usize) -> usize {
    let line_start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    text[line_start..byte].chars().count()
}

/// Строит текст, пока он помещается в предел, и возвращает его вместе с числом
/// кусков.
fn fill(limit: usize, head: &str, piece: impl Fn(usize) -> String, tail: &str) -> (String, usize) {
    let mut text = String::from(head);
    let mut count = 0;
    loop {
        let next = piece(count);
        if text.len() + next.len() + tail.len() > limit {
            break;
        }
        text.push_str(&next);
        count += 1;
    }
    text.push_str(tail);
    assert!(text.len() <= limit);
    (text, count)
}

/// Запрос `definition` (строка и символ с нуля) и ожидаемый ответ
/// `(строка, начало, конец)`; `None` — `null`.
type DefinitionCheck = ((usize, usize), Option<(usize, usize, usize)>);

/// Построитель формы документа по пределу размера.
type ShapeBuilder = fn(usize) -> Shape;

/// Что ждём от документа: сколько символов и куда должно вести определение.
struct Shape {
    label: &'static str,
    text: String,
    symbols: usize,
    definitions: Vec<DefinitionCheck>,
}

fn location_triple(answer: &Json) -> Option<(usize, usize, usize)> {
    if matches!(answer, Json::Null) {
        return None;
    }
    let range = answer.get("range").expect("range");
    let get = |side: &str, key: &str| {
        range
            .get(side)
            .and_then(|p| p.get(key))
            .and_then(Json::as_i64)
            .expect("координата") as usize
    };
    assert_eq!(get("start", "line"), get("end", "line"));
    Some((
        get("start", "line"),
        get("start", "character"),
        get("end", "character"),
    ))
}

/// Самая большая глубина дерева `DocumentSymbol` и общее число символов.
fn tree_size(list: &[Json]) -> (usize, usize) {
    let mut deepest = 0;
    let mut total = 0;
    let mut level: Vec<&Json> = list.iter().collect();
    let mut depth = 0;
    while !level.is_empty() {
        depth += 1;
        deepest = depth;
        total += level.len();
        level = level
            .iter()
            .filter_map(|s| s.get("children").and_then(Json::as_array))
            .flatten()
            .collect();
    }
    (deepest, total)
}

/// Прогоняет форму через сервер на малом стеке; возвращает время ответа
/// `documentSymbol` (иерархического) — для проверки масштабирования.
fn exercise(shape: Shape) -> Duration {
    on_small_stack(move || {
        let Shape {
            label,
            text,
            symbols,
            definitions,
        } = shape;
        assert!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "{label}: {} байт",
            text.len()
        );

        let started = Instant::now();
        let mut hierarchical = Session::start(true, &text);
        let opened = started.elapsed();
        let began = Instant::now();
        let answer = hierarchical.symbols();
        let symbols_time = began.elapsed();
        let (deepest, total) = tree_size(answer.as_array().expect("массив"));
        assert!(
            deepest <= MAX_SYMBOL_DEPTH,
            "{label}: глубина ответа {deepest} больше {MAX_SYMBOL_DEPTH}"
        );
        assert_eq!(total, symbols, "{label}: число символов (иерархия)");
        drop(answer);

        let mut flat = Session::start(false, &text);
        let flat_answer = flat.symbols();
        assert_eq!(
            flat_answer.as_array().expect("массив").len(),
            symbols,
            "{label}: число символов (плоский список)"
        );
        drop(flat_answer);

        let mut definition_time = Duration::ZERO;
        for ((line, character), want) in definitions {
            let began = Instant::now();
            let got = location_triple(&hierarchical.definition(line, character));
            definition_time = definition_time.max(began.elapsed());
            assert_eq!(got, want, "{label}: definition с {line}:{character}");
        }
        eprintln!(
            "{label}: {} байт, символов {symbols}; didOpen {opened:?}, documentSymbol {symbols_time:?}, самый долгий definition {definition_time:?}",
            text.len()
        );
        assert!(opened < BUDGET, "{label}: didOpen {opened:?}");
        assert!(
            symbols_time < BUDGET,
            "{label}: documentSymbol {symbols_time:?}"
        );
        assert!(
            definition_time < BUDGET,
            "{label}: definition {definition_time:?}"
        );
        symbols_time
    })
}

/// Функции по одной на строке; в конце — вызов средней.
fn many_functions(limit: usize) -> Shape {
    let (text, n) = fill(
        limit,
        "",
        |i| {
            sga(&format!(
                "@FN f{i}(a, @MUT b: int) -> int {{ @RETURN a; }}\n"
            ))
        },
        "",
    );
    let middle = n / 2;
    let text = format!("{text}f{middle}(1, 2);\n");
    let length = format!("f{middle}").len();
    Shape {
        label: "функции по одной на строке",
        text,
        symbols: n,
        definitions: vec![
            ((n, 1), Some((middle, 3, 3 + length))),
            ((middle, 4), Some((middle, 3, 3 + length))),
            ((0, 0), None),
        ],
    }
}

/// Те же функции, но все в одной строке (минифицированный файл).
fn one_line(limit: usize) -> Shape {
    let (text, n) = fill(
        limit,
        "",
        |i| sga(&format!("@FN f{i}(a) {{ @RETURN a; }} ")),
        "",
    );
    let middle = n / 2;
    let text = format!("{text}f{middle}(1);");
    let length = format!("f{middle}").len();
    // имя объявления стоит после `FN ` — слова из букв SGA и пробела
    let declared = text
        .find(&sga(&format!("@FN f{middle}(")))
        .expect("объявление")
        + sga("@FN ").len();
    let decl_col = column_of(&text, declared);
    let call_col = column_of(&text, text.rfind(&format!("f{middle}(")).expect("вызов"));
    Shape {
        label: "все функции в одной строке",
        text,
        symbols: n,
        definitions: vec![
            ((0, call_col + 1), Some((0, decl_col, decl_col + length))),
            ((0, decl_col), Some((0, decl_col, decl_col + length))),
        ],
    }
}

/// Структура с очень большим числом полей и литерал с ключом из середины.
fn many_fields(limit: usize) -> Shape {
    let tail = sga("}\n@LET s = S { f0: 1 };\n");
    let (text, n) = fill(limit, &sga("@STRUCT S { "), |i| format!("f{i}, "), &tail);
    // ключ литерала — первое поле `f0`; оно стоит сразу после `{ `
    let field_col = sga("@STRUCT S { ").chars().count();
    let key_col = column_of(&text, text.rfind("f0:").expect("ключ"));
    Shape {
        label: "структура с огромным числом полей",
        text,
        // структура, её поля и переменная `s`
        symbols: 1 + n + 1,
        definitions: vec![((1, key_col), Some((0, field_col, field_col + 2)))],
    }
}

/// Много переменных верхнего уровня и использование первой из них в конце.
fn many_variables(limit: usize) -> Shape {
    let (text, n) = fill(limit, "", |i| sga(&format!("@LET v{i} = {i};\n")), "");
    let text = format!("{text}v0;\n");
    Shape {
        label: "десятки тысяч переменных верхнего уровня",
        text,
        symbols: n,
        definitions: vec![((n, 0), Some((0, 4, 6)))],
    }
}

/// Лестница незакрытых функций: символы вкладываются, но ответ остаётся мелким.
fn unclosed_ladder(limit: usize) -> Shape {
    let (text, n) = fill(limit, "", |_| sga("@FN a() { "), "");
    Shape {
        label: "лестница незакрытых функций",
        text,
        symbols: n,
        definitions: vec![((0, 3), Some((0, 3, 4)))],
    }
}

/// Сотни тысяч открытых `{`, затем объявление и его использование.
fn deep_braces(limit: usize) -> Shape {
    let tail = sga(" @LET x = 1; x;");
    let n = limit - tail.len();
    let text = format!("{}{tail}", "{".repeat(n));
    let decl = column_of(&text, text.find(" x = ").expect("объявление") + 1);
    let used = column_of(&text, text.rfind("x;").expect("использование"));
    Shape {
        label: "сотни тысяч вложенных блоков",
        text,
        symbols: 0,
        definitions: vec![((0, used), Some((0, decl, decl + 1)))],
    }
}

/// Сотни тысяч открытых скобок перед использованием.
fn deep_parentheses(limit: usize) -> Shape {
    let head = sga("@LET x = 1;\n");
    let n = limit - head.len() - 1;
    let text = format!("{head}{}x", "(".repeat(n));
    Shape {
        label: "сотни тысяч вложенных скобок",
        text,
        symbols: 1,
        definitions: vec![((1, n), Some((0, 4, 5)))],
    }
}

#[test]
fn one_mebibyte_shapes_are_served_on_a_small_stack_within_the_budget() {
    let shapes = [
        many_functions(MAX_DOCUMENT_BYTES),
        one_line(MAX_DOCUMENT_BYTES),
        many_fields(MAX_DOCUMENT_BYTES),
        many_variables(MAX_DOCUMENT_BYTES),
        unclosed_ladder(MAX_DOCUMENT_BYTES),
        deep_braces(MAX_DOCUMENT_BYTES),
        deep_parentheses(MAX_DOCUMENT_BYTES),
    ];
    for shape in shapes {
        exercise(shape);
    }
}

/// Лучшее из нескольких времён ответа `documentSymbol` на тексте.
fn best_symbols_time(text: &str) -> Duration {
    let text = text.to_string();
    on_small_stack(move || {
        let mut session = Session::start(true, &text);
        (0..3)
            .map(|_| {
                let began = Instant::now();
                let answer = session.symbols();
                let elapsed = began.elapsed();
                assert!(answer.as_array().is_some_and(|list| !list.is_empty()));
                elapsed
            })
            .min()
            .expect("три замера")
    })
}

#[test]
fn document_symbol_time_grows_linearly_with_the_document() {
    // вчетверо больший документ: линейная работа — вчетверо дольше, квадратичная
    // (просмотр текста с начала на каждую позицию) — в шестнадцать раз
    let shapes: [(&str, ShapeBuilder); 3] = [
        ("функции по одной на строке", many_functions),
        ("все функции в одной строке", one_line),
        ("структура с огромным числом полей", many_fields),
    ];
    for (label, build) in shapes {
        let small = best_symbols_time(&build(MAX_DOCUMENT_BYTES / 4).text);
        let big = best_symbols_time(&build(MAX_DOCUMENT_BYTES).text);
        eprintln!("{label}: 256 КиБ — {small:?}, 1 МиБ — {big:?}");
        assert!(
            big.as_secs_f64() < 10.0 * small.as_secs_f64() + 0.05,
            "{label}: время растёт быстрее линейного: {small:?} -> {big:?}"
        );
    }
}
