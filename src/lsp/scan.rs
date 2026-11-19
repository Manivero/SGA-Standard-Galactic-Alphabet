//! Сканер документа для подсказок (T018, M007): терпимая токенизация,
//! лексическое состояние в позиции курсора, префикс слова, глобальные
//! объявления (функции, структуры) и области видимости имён.
//!
//! Движок работает на ТОКЕНАХ лексера, а не на AST: документ, который сейчас
//! набирают (`PRINT(le`), почти всегда синтаксически неверен, а у парсера нет
//! восстановления после ошибок (TD-020) — дерева он не даст. Поэтому здесь
//! небольшой собственный разбор поверх токенов, который ПОВТОРЯЕТ правила
//! `semantic::Analyzer` о видимости имён (проверяется дифференциальным тестом
//! `tests/lsp_assist_test.rs`: на валидных программах решение «имя видно в
//! этом месте» совпадает с решением настоящего анализатора).
//!
//! Позиции — позиции ЛЕКСЕРА: `(строка, колонка)` с единицы, колонка в
//! символах; кортежи сравниваются лексикографически, как позиции в тексте.
//! Перевод из протокола — `position::from_lsp`.
//!
//! Ничего здесь не рекурсивно по размеру документа: разбор — один проход по
//! токенам со стеком областей в куче.

use super::vocabulary;
use crate::lexer::token::{TokenKind, TokenSpan};
use crate::lexer::Lexer;
use crate::sga_alphabet::{decode_word, is_sga_letter};
use std::borrow::Cow;
use std::collections::HashSet;

/// Позиция в тексте: `(строка, колонка)` лексера, обе с единицы.
pub type Pos = (usize, usize);

/// Сколько раз после первой токенизации текст лексируется заново, заменяя
/// очередную лексическую ошибку пробелами. Больше — «текстовый режим».
pub const MAX_RELEX: usize = 16;

/// Токен с границами.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenInfo {
    pub kind: TokenKind,
    pub span: TokenSpan,
}

impl TokenInfo {
    /// Начало токена (включительно).
    pub fn start(&self) -> Pos {
        (self.span.line, self.span.col)
    }

    /// Конец токена (исключительно).
    pub fn end(&self) -> Pos {
        (self.span.end_line, self.span.end_col)
    }

    /// Слово: ключевое слово или идентификатор.
    pub fn is_word(&self) -> bool {
        matches!(self.kind, TokenKind::Ident(_)) || keyword_mnemonic(&self.kind).is_some()
    }
}

/// ASCII-мнемоника ключевого слова по виду токена; `None` — не ключевое слово.
pub fn keyword_mnemonic(kind: &TokenKind) -> Option<&'static str> {
    use crate::lexer::token::{keyword_from_mnemonic, KEYWORD_MNEMONICS};
    KEYWORD_MNEMONICS
        .iter()
        .copied()
        .find(|m| keyword_from_mnemonic(m).as_ref() == Some(kind))
}

/// Результат терпимой токенизации.
#[derive(Debug, Clone, Default)]
pub struct Tokenized {
    /// Токены без `Eof`.
    pub tokens: Vec<TokenInfo>,
    /// `true`: лексических ошибок больше, чем текст готов терпеть
    /// (`MAX_RELEX`), токенов нет — доступно только то, что можно решить по
    /// самому тексту.
    pub text_mode: bool,
}

/// Токенизация, терпимая к лексическим ошибкам: место ошибки заменяется
/// пробелами (длина и переводы строк сохраняются, поэтому позиции остальных
/// токенов не сдвигаются), и текст лексируется заново.
///
/// Лексер при ошибке возвращает ТОЛЬКО ошибку — без токенов до неё; без этой
/// обработки незакрытая строка или посторонний символ в любом месте файла
/// лишили бы подсказок весь документ.
pub fn tokenize(text: &str) -> Tokenized {
    let mut current: Cow<str> = Cow::Borrowed(text);
    for _ in 0..=MAX_RELEX {
        match Lexer::new(&current).tokenize_with_spans() {
            Ok((tokens, spans)) => {
                let mut items: Vec<TokenInfo> = tokens
                    .into_iter()
                    .zip(spans)
                    .map(|(token, span)| TokenInfo {
                        kind: token.kind,
                        span,
                    })
                    .collect();
                if matches!(items.last().map(|t| &t.kind), Some(TokenKind::Eof)) {
                    items.pop();
                }
                return Tokenized {
                    tokens: items,
                    text_mode: false,
                };
            }
            Err(error) => {
                current = Cow::Owned(blank_range(
                    &current,
                    (error.line, error.col),
                    (error.end_line, error.end_col),
                ));
            }
        }
    }
    Tokenized {
        tokens: Vec::new(),
        text_mode: true,
    }
}

/// Заменяет пробелами все символы текста в `[start, end)`, кроме переводов
/// строки. Пустой диапазон расширяется до одного символа, чтобы повторная
/// токенизация гарантированно продвигалась вперёд.
fn blank_range(text: &str, start: Pos, end: Pos) -> String {
    let end = if end <= start {
        (start.0, start.1 + 1)
    } else {
        end
    };
    let mut out = String::with_capacity(text.len());
    let (mut line, mut col) = (1usize, 1usize);
    for c in text.chars() {
        let inside = (line, col) >= start && (line, col) < end;
        out.push(if inside && c != '\n' { ' ' } else { c });
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    out
}

/// Лексическое состояние в позиции (между символами).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lex {
    /// Обычный код.
    Code,
    /// Внутри строковой константы (в том числе незакрытой).
    Str,
    /// Внутри комментария `//` (до конца строки).
    Comment,
}

/// Лексическое состояние перед позицией `pos`, вычисленное с начала текста
/// по тем же правилам, что у лексера: строка `"…"` с экранированием `\` может
/// занимать несколько строк, комментарий `//` идёт до конца строки; `//`
/// внутри строки и кавычка внутри комментария ничего не значат. Не зависит от
/// лексических ошибок в других местах текста (в отличие от токенов).
pub fn lexical_state(text: &str, pos: Pos) -> Lex {
    let (mut line, mut col) = (1usize, 1usize);
    let mut state = Lex::Code;
    let mut escaped = false;
    let mut slash = false;
    for c in text.chars() {
        if (line, col) >= pos {
            break;
        }
        match state {
            Lex::Code => {
                if c == '"' {
                    state = Lex::Str;
                    slash = false;
                } else if c == '/' {
                    if slash {
                        state = Lex::Comment;
                        slash = false;
                    } else {
                        slash = true;
                    }
                } else {
                    slash = false;
                }
            }
            Lex::Str => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    state = Lex::Code;
                }
            }
            Lex::Comment => {
                if c == '\n' {
                    state = Lex::Code;
                }
            }
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    state
}

/// Вид префикса — слова, которое набирают у курсора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixKind {
    /// Слева от курсора нет символов слова.
    Empty,
    /// ASCII-слово: буквы, цифры и `_`, начинается с буквы или `_`.
    Ascii,
    /// Слово из букв SGA.
    Glyph,
    /// Слева — ASCII-слово, начинающееся с цифры (`12ab`, `3`): не слово.
    Digit,
}

/// Слово, прилегающее к курсору слева.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    pub kind: PrefixKind,
    pub text: String,
    /// Позиция первого символа префикса (при пустом — позиция курсора).
    pub start: Pos,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Префикс у курсора: максимальный прилегающий слева ASCII-идентификатор или
/// слово из букв SGA (по тексту строки, а не по токенам: токена может не быть,
/// пока слово не дописано).
pub fn prefix_at(text: &str, pos: Pos) -> Prefix {
    let line = text.split('\n').nth(pos.0.saturating_sub(1)).unwrap_or("");
    let before: Vec<char> = line.chars().take(pos.1.saturating_sub(1)).collect();
    let empty = Prefix {
        kind: PrefixKind::Empty,
        text: String::new(),
        start: pos,
    };
    let Some(&last) = before.last() else {
        return empty;
    };
    let mut i = before.len();
    let kind = if is_sga_letter(last) {
        while i > 0 && is_sga_letter(before[i - 1]) {
            i -= 1;
        }
        PrefixKind::Glyph
    } else if is_word_char(last) {
        while i > 0 && is_word_char(before[i - 1]) {
            i -= 1;
        }
        if before[i].is_ascii_digit() {
            PrefixKind::Digit
        } else {
            PrefixKind::Ascii
        }
    } else {
        return empty;
    };
    Prefix {
        kind,
        text: before[i..].iter().collect(),
        start: (pos.0, i + 1),
    }
}

/// Параметр функции, объявленной в документе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamInfo {
    pub name: String,
    pub mutable: bool,
    /// Аннотация типа как написана (`int`, `NIL`).
    pub ty: Option<String>,
    /// Индекс токена имени параметра (T019: переход к определению).
    pub at: usize,
}

/// Функция, объявленная в документе (`FN имя(…)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnInfo {
    pub name: String,
    pub params: Vec<ParamInfo>,
    pub ret: Option<String>,
    /// Строка имени (с единицы).
    pub line: usize,
    /// Индекс токена имени (T019); `FN` стоит на единицу левее.
    pub name_at: usize,
    /// Индекс `{` тела, если заголовок закончен.
    pub open: Option<usize>,
    /// Индекс последнего токена заголовка (при законченном — самой `{`).
    pub last: usize,
}

impl FnInfo {
    /// `имя([MUT ]параметр[: тип], …)[ -> тип]`.
    pub fn signature(&self) -> String {
        format!("{}{}", self.name, self.signature_tail())
    }

    /// Сигнатура без имени: `([MUT ]параметр[: тип], …)[ -> тип]`.
    pub fn signature_tail(&self) -> String {
        let params: Vec<String> = self
            .params
            .iter()
            .map(|p| {
                format!(
                    "{}{}{}",
                    if p.mutable { "MUT " } else { "" },
                    p.name,
                    p.ty.as_ref().map(|t| format!(": {t}")).unwrap_or_default()
                )
            })
            .collect();
        let ret = self
            .ret
            .as_ref()
            .map(|t| format!(" -> {t}"))
            .unwrap_or_default();
        format!("({}){}", params.join(", "), ret)
    }
}

/// Структура, объявленная в документе (`STRUCT Имя { поля }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructInfo {
    pub name: String,
    pub fields: Vec<String>,
    pub line: usize,
    /// Индекс токена имени (T019); `STRUCT` стоит на единицу левее, `{` — правее.
    pub name_at: usize,
    /// Индексы токенов полей — параллельно `fields`.
    pub fields_at: Vec<usize>,
}

/// Функции и структуры документа: видны во всём файле, в том числе выше
/// места объявления (как у `semantic::Analyzer`, который собирает их первым
/// проходом). При повторном объявлении имени первое побеждает.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Globals {
    pub functions: Vec<FnInfo>,
    pub structs: Vec<StructInfo>,
}

impl Globals {
    pub fn function(&self, name: &str) -> Option<&FnInfo> {
        self.functions.iter().find(|f| f.name == name)
    }

    pub fn structure(&self, name: &str) -> Option<&StructInfo> {
        self.structs.iter().find(|s| s.name == name)
    }
}

/// Чем закончился разбор заголовка функции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderEnd {
    /// Заголовок полный; значение — индекс `{` тела.
    Complete(usize),
    /// Токены кончились внутри заголовка (курсор в заголовке).
    Incomplete,
    /// Встретился токен, не подходящий заголовку; значение — его индекс.
    Malformed(usize),
}

#[derive(Debug)]
struct Header {
    name: Option<String>,
    params: Vec<ParamInfo>,
    ret: Option<String>,
    /// Встретилась ли `(` после (необязательного) имени.
    saw_paren: bool,
    end: HeaderEnd,
}

fn type_name(kind: &TokenKind) -> Option<String> {
    match kind {
        TokenKind::Ident(name) => Some(name.clone()),
        TokenKind::Nil => Some("NIL".to_string()),
        _ => None,
    }
}

/// Терпимый разбор заголовка `FN [имя] ( [MUT] имя [: тип], … ) [-> тип] {`
/// по токенам `toks` (индекс `fn_at` — токен `FN`). Не паникует на любом
/// наборе токенов; всегда возвращает осмысленно то, что успел разобрать.
fn parse_header(toks: &[TokenInfo], fn_at: usize) -> Header {
    let mut h = Header {
        name: None,
        params: Vec::new(),
        ret: None,
        saw_paren: false,
        end: HeaderEnd::Incomplete,
    };
    let mut j = fn_at + 1;
    if let Some(TokenKind::Ident(name)) = toks.get(j).map(|t| &t.kind) {
        h.name = Some(name.clone());
        j += 1;
    }
    match toks.get(j).map(|t| &t.kind) {
        None => return h,
        Some(TokenKind::LParen) => {
            h.saw_paren = true;
            j += 1;
        }
        Some(_) => {
            h.end = HeaderEnd::Malformed(j);
            return h;
        }
    }
    // параметры
    loop {
        let Some(tok) = toks.get(j) else { return h };
        match &tok.kind {
            TokenKind::RParen => {
                j += 1;
                break;
            }
            TokenKind::Comma => {
                j += 1;
                continue;
            }
            _ => {}
        }
        let mut mutable = false;
        if matches!(tok.kind, TokenKind::Mut) {
            mutable = true;
            j += 1;
        }
        let Some(tok) = toks.get(j) else { return h };
        let TokenKind::Ident(name) = &tok.kind else {
            h.end = HeaderEnd::Malformed(j);
            return h;
        };
        let mut param = ParamInfo {
            name: name.clone(),
            mutable,
            ty: None,
            at: j,
        };
        j += 1;
        if matches!(toks.get(j).map(|t| &t.kind), Some(TokenKind::Colon)) {
            j += 1;
            let Some(tok) = toks.get(j) else {
                h.params.push(param);
                return h;
            };
            let Some(ty) = type_name(&tok.kind) else {
                h.params.push(param);
                h.end = HeaderEnd::Malformed(j);
                return h;
            };
            param.ty = Some(ty);
            j += 1;
        }
        h.params.push(param);
        // за параметром — `,` или `)`
        match toks.get(j).map(|t| &t.kind) {
            None => return h,
            Some(TokenKind::Comma | TokenKind::RParen) => {}
            Some(_) => {
                h.end = HeaderEnd::Malformed(j);
                return h;
            }
        }
    }
    // `-> тип`
    if matches!(toks.get(j).map(|t| &t.kind), Some(TokenKind::Arrow)) {
        j += 1;
        let Some(tok) = toks.get(j) else { return h };
        let Some(ty) = type_name(&tok.kind) else {
            h.end = HeaderEnd::Malformed(j);
            return h;
        };
        h.ret = Some(ty);
        j += 1;
    }
    match toks.get(j).map(|t| &t.kind) {
        None => h,
        Some(TokenKind::LBrace) => {
            h.end = HeaderEnd::Complete(j);
            h
        }
        Some(_) => {
            h.end = HeaderEnd::Malformed(j);
            h
        }
    }
}

/// Все объявления функций и структур документа в порядке появления, повторные
/// имена — тоже (T019: структура файла перечисляет каждое объявление).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declarations {
    pub functions: Vec<FnInfo>,
    pub structs: Vec<StructInfo>,
}

/// Функции и структуры по всем токенам документа. Именованный `FN` в
/// корректной программе бывает только на верхнем уровне, поэтому образец
/// `FN имя (` ищется на любой глубине: так объявления, лежащие ниже
/// незакрытой скобки, не пропадают из подсказок.
pub fn collect_declarations(toks: &[TokenInfo]) -> Declarations {
    let mut decls = Declarations::default();
    for (i, tok) in toks.iter().enumerate() {
        match &tok.kind {
            TokenKind::Fn => {
                let h = parse_header(toks, i);
                let (Some(name), true) = (h.name, h.saw_paren) else {
                    continue;
                };
                let (open, last) = match h.end {
                    HeaderEnd::Complete(open) => (Some(open), open),
                    HeaderEnd::Malformed(end) => (None, end.saturating_sub(1)),
                    HeaderEnd::Incomplete => (None, toks.len() - 1),
                };
                decls.functions.push(FnInfo {
                    name,
                    params: h.params,
                    ret: h.ret,
                    line: toks[i + 1].span.line,
                    name_at: i + 1,
                    open,
                    last,
                });
            }
            TokenKind::Struct => {
                let (Some(name_tok), Some(open)) = (toks.get(i + 1), toks.get(i + 2)) else {
                    continue;
                };
                let TokenKind::Ident(name) = &name_tok.kind else {
                    continue;
                };
                if !matches!(open.kind, TokenKind::LBrace) {
                    continue;
                }
                let mut fields = Vec::new();
                let mut fields_at = Vec::new();
                for (offset, t) in toks[i + 3..].iter().enumerate() {
                    match &t.kind {
                        TokenKind::Ident(field) => {
                            fields.push(field.clone());
                            fields_at.push(i + 3 + offset);
                        }
                        TokenKind::Comma => {}
                        _ => break,
                    }
                }
                decls.structs.push(StructInfo {
                    name: name.clone(),
                    fields,
                    line: name_tok.span.line,
                    name_at: i + 1,
                    fields_at,
                });
            }
            _ => {}
        }
    }
    decls
}

/// Функции и структуры документа для подсказок: имя — один раз, при повторном
/// объявлении побеждает первое.
pub fn collect_globals(toks: &[TokenInfo]) -> Globals {
    let all = collect_declarations(toks);
    // уже объявленные имена: просмотр списка на каждое объявление был бы
    // квадратичным на файле из десятков тысяч функций
    let mut function_names: HashSet<String> = HashSet::new();
    let mut struct_names: HashSet<String> = HashSet::new();
    Globals {
        functions: all
            .functions
            .into_iter()
            .filter(|f| function_names.insert(f.name.clone()))
            .collect(),
        structs: all
            .structs
            .into_iter()
            .filter(|s| struct_names.insert(s.name.clone()))
            .collect(),
    }
}

/// Вид имени в области видимости.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Let,
    Var,
    Const,
    Param,
    ForVar,
    MatchBind,
}

/// Имя, видимое в позиции курсора.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarInfo {
    pub name: String,
    pub kind: VarKind,
    /// Индекс токена имени в объявлении (T019: переход к определению).
    pub at: usize,
}

/// Почему в этой позиции подсказок нет (по структуре кода у курсора).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// После одиночной `.`: поле или метод — типов нет, подсказывать нечего.
    AfterDot,
    /// Имя вводимого объявления (после `LET`/`VAR`/`CONST`/`STRUCT`/`FOR`/
    /// `FN`/`MUT`).
    DeclName,
    /// Внутри заголовка функции.
    FnHeader,
    /// В теле `STRUCT`: имена полей.
    StructBody,
    /// Ключ литерала структуры (`Имя { ключ: …`).
    StructKey,
    /// Аннотация типа (после `:` вне литерала структуры, после `->` вне
    /// пункта `MATCH`).
    TypePosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Global,
    Block,
    FnBody,
    StructLit,
    StructDecl,
    MatchArms,
    Arm,
    Paren,
    Bracket,
    ForHead,
    MatchHead,
    StructHead,
    /// Заголовок `IF`/`WHILE`: условие, за которым ждём `{` тела.
    CondHead,
}

impl FrameKind {
    /// Область, открытая `{`.
    fn is_brace(self) -> bool {
        matches!(
            self,
            FrameKind::Block
                | FrameKind::FnBody
                | FrameKind::StructLit
                | FrameKind::StructDecl
                | FrameKind::MatchArms
        )
    }

    /// Заголовок конструкции: ждёт своего `{`.
    fn is_head(self) -> bool {
        matches!(
            self,
            FrameKind::ForHead | FrameKind::MatchHead | FrameKind::StructHead | FrameKind::CondHead
        )
    }
}

#[derive(Debug)]
struct Frame {
    kind: FrameKind,
    vars: Vec<VarInfo>,
    /// Переменная цикла `FOR` и индекс её токена (только у `ForHead`).
    for_var: Option<(String, usize)>,
    /// Индекс `{`, открывшей область (только у `StructLit` и `StructDecl`).
    open_at: Option<usize>,
}

impl Frame {
    fn new(kind: FrameKind) -> Self {
        Frame {
            kind,
            vars: Vec::new(),
            for_var: None,
            open_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Ждём имя.
    Name,
    /// Имя есть; ждём `:` или `=`.
    AfterName,
    /// Прочитали `:`; ждём имя типа.
    Type,
    /// Прочитали имя типа; ждём `=`.
    AfterType,
    /// Идёт инициализатор.
    Init,
}

/// Объявление `LET`/`VAR`/`CONST`, имя которого ещё не видно: оно попадает в
/// область, когда оператор закончен (в собственном инициализаторе имя не
/// видно — как у `semantic::Analyzer`).
#[derive(Debug)]
struct Pending {
    kind: VarKind,
    name: String,
    /// Индекс токена имени (с момента, как имя прочитано).
    at: usize,
    /// Индекс ключевого слова `LET`/`VAR`/`CONST`.
    start: usize,
    /// Аннотация типа как написана.
    ty: Option<String>,
    /// Глубина стека областей в момент объявления: объявляемое имя попадёт в
    /// `frames[depth - 1]`.
    depth: usize,
    stage: Stage,
    /// Последний токен инициализатора на этой глубине — конец операнда.
    last_operand_end: bool,
}

/// Оператор `LET`/`VAR`/`CONST` верхнего уровня, дошедший до инициализатора
/// (T019: структура файла).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopVariable {
    pub kind: VarKind,
    pub name: String,
    /// Индекс ключевого слова.
    pub start: usize,
    /// Индекс токена имени.
    pub at: usize,
    /// Индекс последнего токена оператора (включая необязательную `;`).
    pub end: usize,
    /// Аннотация типа как написана.
    pub ty: Option<String>,
}

fn is_statement_start(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Let
            | TokenKind::Var
            | TokenKind::Const
            | TokenKind::If
            | TokenKind::While
            | TokenKind::For
            | TokenKind::Return
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::Print
            | TokenKind::Struct
            | TokenKind::Import
    )
}

fn is_operand_start(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::IntLit(_)
            | TokenKind::FloatLit(_)
            | TokenKind::StringLit(_)
            | TokenKind::Ident(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Nil
            | TokenKind::Not
            | TokenKind::Match
            | TokenKind::Fn
    )
}

fn is_operand_end(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::IntLit(_)
            | TokenKind::FloatLit(_)
            | TokenKind::StringLit(_)
            | TokenKind::Ident(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Nil
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
    )
}

/// Вид литерала структуры, который распознаёт лукахед `{`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiteralForm {
    /// `Имя { }`.
    Empty,
    /// `Имя { поле: …`.
    Fields,
}

/// `{` под индексом `at` открывает литерал структуры? То же правило, что у
/// парсера (`is_struct_lit_lookahead`): перед `{` стоит идентификатор, а
/// после неё — `}` или `идентификатор :`.
fn struct_literal_form(toks: &[TokenInfo], at: usize) -> Option<LiteralForm> {
    let after_ident = at > 0 && matches!(toks[at - 1].kind, TokenKind::Ident(_));
    if !after_ident {
        return None;
    }
    match toks.get(at + 1).map(|t| &t.kind) {
        Some(TokenKind::RBrace) => Some(LiteralForm::Empty),
        Some(TokenKind::Ident(_)) => {
            matches!(toks.get(at + 2).map(|t| &t.kind), Some(TokenKind::Colon))
                .then_some(LiteralForm::Fields)
        }
        _ => None,
    }
}

fn is_struct_literal(toks: &[TokenInfo], at: usize) -> bool {
    struct_literal_form(toks, at).is_some()
}

/// Последний из первых `end` токенов — одиночная `.` (не вторая точка `..`):
/// после неё идёт поле или метод, а не операнд выражения. `..` — диапазон
/// `FOR` (`0..len(a)`), после него снова операнд.
pub fn single_dot_before(toks: &[TokenInfo], end: usize) -> bool {
    end >= 1
        && matches!(toks[end - 1].kind, TokenKind::Dot)
        && !(end >= 2 && matches!(toks[end - 2].kind, TokenKind::Dot))
}

/// Что известно о коде перед курсором.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub blocked: Option<Blocked>,
    /// Видимые имена: внутренние области раньше внешних, в области — позднее
    /// объявленные раньше; имя — один раз (ближайшее объявление).
    pub variables: Vec<VarInfo>,
}

/// `{` под индексом `at` открывает литерал структуры там, где верхняя область —
/// `top`? Литерал с полями (`Имя { поле: …`) — всегда литерал; пустой
/// `Имя { }` в заголовке `IF`/`WHILE`/`FOR`/`MATCH` — почти наверняка тело,
/// которое набирают (редактор сам закрыл скобку): парсер всё равно не принял
/// бы `IF x { }`, он увидел бы литерал структуры и потерял тело.
fn opens_struct_literal(toks: &[TokenInfo], at: usize, top: FrameKind) -> bool {
    let in_head = matches!(
        top,
        FrameKind::ForHead | FrameKind::MatchHead | FrameKind::CondHead
    );
    match struct_literal_form(toks, at) {
        Some(LiteralForm::Fields) => true,
        Some(LiteralForm::Empty) => !in_head,
        None => false,
    }
}

/// Состояние сканера областей.
struct Scanner<'a> {
    toks: &'a [TokenInfo],
    frames: Vec<Frame>,
    pendings: Vec<Pending>,
    in_header: bool,
    /// Записывать ли завершённые объявления верхнего уровня (`top_level`).
    record: bool,
    top_level: Vec<TopVariable>,
}

impl<'a> Scanner<'a> {
    fn new(toks: &'a [TokenInfo]) -> Self {
        Scanner {
            toks,
            frames: vec![Frame::new(FrameKind::Global)],
            pendings: Vec::new(),
            in_header: false,
            record: false,
            top_level: Vec::new(),
        }
    }

    fn top(&self) -> FrameKind {
        self.frames
            .last()
            .map(|f| f.kind)
            .unwrap_or(FrameKind::Global)
    }

    /// Оператор объявления закончен: имя попадает в область объявления.
    /// `end` — индекс последнего токена оператора.
    fn complete_pending(&mut self, end: usize) {
        if let Some(p) = self.pendings.pop() {
            if self.record && p.depth == 1 {
                self.top_level.push(TopVariable {
                    kind: p.kind,
                    name: p.name.clone(),
                    start: p.start,
                    at: p.at,
                    end,
                    ty: p.ty.clone(),
                });
            }
            if let Some(frame) = self.frames.get_mut(p.depth - 1) {
                frame.vars.push(VarInfo {
                    name: p.name,
                    kind: p.kind,
                    at: p.at,
                });
            }
        }
    }

    /// Токены кончились: оператор верхнего уровня, дошедший до инициализатора,
    /// считается законченным на последнем токене (в том числе когда документ
    /// оборвался внутри заголовка функции-значения: `LET f = FN(x`).
    fn finish(&mut self) {
        if !self.record {
            return;
        }
        let Some(end) = self.toks.len().checked_sub(1) else {
            return;
        };
        let at_top = matches!(
            self.pendings.first(),
            Some(p) if p.depth == 1 && p.stage == Stage::Init
        );
        if at_top {
            // нижнее объявление стека — верхнего уровня; те, что выше, лежат
            // внутри его инициализатора и до конца текста не закончены
            self.pendings.truncate(1);
            self.complete_pending(end);
        }
    }

    /// Оператор-объявление, ждущий конца инициализатора, заканчивается перед
    /// токеном `k`: `;`, начало нового оператора или (после операнда) начало
    /// нового операнда.
    fn pending_ends_before(&self, k: usize) -> bool {
        let kind = &self.toks[k].kind;
        match self.pendings.last() {
            Some(p) if p.depth == self.frames.len() && p.stage == Stage::Init => match kind {
                TokenKind::Semicolon => true,
                kind if is_statement_start(kind) => true,
                TokenKind::LBrace => p.last_operand_end && !is_struct_literal(self.toks, k),
                kind if is_operand_start(kind) => p.last_operand_end,
                _ => false,
            },
            _ => false,
        }
    }

    /// После закрытия областей объявления, чья область исчезла, не
    /// завершаются.
    fn drop_dead_pendings(&mut self) {
        let depth = self.frames.len();
        self.pendings.retain(|p| p.depth <= depth);
    }

    /// Ключевое слово оператора не может стоять в заголовке `FOR`/`MATCH`/
    /// `STRUCT`/`IF`/`WHILE`: незавершённый заголовок брошен.
    fn abandon_heads(&mut self) {
        while self.frames.last().is_some_and(|f| f.kind.is_head()) {
            self.frames.pop();
        }
        self.drop_dead_pendings();
    }

    fn close_brace(&mut self) {
        if let Some(i) = self.frames.iter().rposition(|f| f.kind.is_brace()) {
            self.frames.truncate(i);
        }
        self.drop_dead_pendings();
    }

    /// `)`/`]` закрывают ближайшую свою скобку, не выходя за ближайшую
    /// фигурную область.
    fn close_delimiter(&mut self, wanted: FrameKind) {
        for i in (0..self.frames.len()).rev() {
            let kind = self.frames[i].kind;
            if kind == wanted {
                self.frames.truncate(i);
                break;
            }
            if kind.is_brace() || kind == FrameKind::Global {
                break;
            }
        }
        self.drop_dead_pendings();
    }

    fn open_brace(&mut self, at: usize) {
        let top = self.top();
        let struct_lit = opens_struct_literal(self.toks, at, top);
        match top {
            FrameKind::ForHead if !struct_lit => {
                let var = self.frames.pop().and_then(|f| f.for_var);
                let mut body = Frame::new(FrameKind::Block);
                if let Some((name, at)) = var {
                    body.vars.push(VarInfo {
                        name,
                        kind: VarKind::ForVar,
                        at,
                    });
                }
                self.frames.push(body);
            }
            FrameKind::MatchHead if !struct_lit => {
                self.frames.pop();
                self.frames.push(Frame::new(FrameKind::MatchArms));
            }
            FrameKind::CondHead if !struct_lit => {
                self.frames.pop();
                self.frames.push(Frame::new(FrameKind::Block));
            }
            FrameKind::StructHead => {
                self.frames.pop();
                let mut frame = Frame::new(FrameKind::StructDecl);
                frame.open_at = Some(at);
                self.frames.push(frame);
            }
            _ => {
                let kind = if struct_lit {
                    FrameKind::StructLit
                } else {
                    FrameKind::Block
                };
                let mut frame = Frame::new(kind);
                if struct_lit {
                    frame.open_at = Some(at);
                }
                self.frames.push(frame);
            }
        }
    }

    /// Токен `->` в пунктах `MATCH`: отделяет образец от результата.
    /// Образец-имя (кроме `_`) связывает значение в теле пункта.
    fn arrow(&mut self, at: usize) {
        if self.top() != FrameKind::MatchArms {
            return;
        }
        let mut arm = Frame::new(FrameKind::Arm);
        if at >= 1 {
            if let TokenKind::Ident(name) = &self.toks[at - 1].kind {
                let starts_pattern = at < 2
                    || matches!(self.toks[at - 2].kind, TokenKind::LBrace | TokenKind::Comma);
                if name != "_" && starts_pattern {
                    arm.vars.push(VarInfo {
                        name: name.clone(),
                        kind: VarKind::MatchBind,
                        at: at - 1,
                    });
                }
            }
        }
        self.frames.push(arm);
    }

    /// Разбирает токены `[0, limit)`. Если токены кончились внутри заголовка
    /// функции (курсор в заголовке), ставит `in_header` и останавливается.
    fn run(&mut self, limit: usize) {
        let toks = self.toks;
        let mut k = 0;
        while k < limit {
            let kind = &toks[k].kind;

            // 1) объявление ждёт имя, тип или `=`
            let depth = self.frames.len();
            if let Some(p) = self.pendings.last_mut() {
                if p.depth == depth && p.stage != Stage::Init {
                    let consumed = match (p.stage, kind) {
                        (Stage::Name, TokenKind::Ident(name)) => {
                            p.name = name.clone();
                            p.at = k;
                            p.stage = Stage::AfterName;
                            true
                        }
                        (Stage::AfterName, TokenKind::Colon) => {
                            p.stage = Stage::Type;
                            true
                        }
                        (Stage::Type, TokenKind::Ident(_) | TokenKind::Nil) => {
                            p.ty = type_name(kind);
                            p.stage = Stage::AfterType;
                            true
                        }
                        (Stage::AfterName | Stage::AfterType, TokenKind::Assign) => {
                            p.stage = Stage::Init;
                            p.last_operand_end = false;
                            true
                        }
                        _ => false,
                    };
                    if consumed {
                        k += 1;
                        continue;
                    }
                    // оборванное объявление: имя в область не вносится
                    self.pendings.pop();
                }
            }

            // 2) конец оператора-объявления перед этим токеном (`;` входит в
            // оператор, остальные токены уже принадлежат следующему)
            if self.pending_ends_before(k) {
                let end = if matches!(kind, TokenKind::Semicolon) {
                    k
                } else {
                    k.saturating_sub(1)
                };
                self.complete_pending(end);
            }

            // 3) сам токен
            match kind {
                TokenKind::Let | TokenKind::Var | TokenKind::Const => {
                    self.abandon_heads();
                    let kind = match kind {
                        TokenKind::Let => VarKind::Let,
                        TokenKind::Var => VarKind::Var,
                        _ => VarKind::Const,
                    };
                    self.pendings.push(Pending {
                        kind,
                        name: String::new(),
                        at: 0,
                        start: k,
                        ty: None,
                        depth: self.frames.len(),
                        stage: Stage::Name,
                        last_operand_end: false,
                    });
                }
                TokenKind::For => {
                    self.abandon_heads();
                    let mut head = Frame::new(FrameKind::ForHead);
                    if k + 1 < limit {
                        if let TokenKind::Ident(name) = &toks[k + 1].kind {
                            head.for_var = Some((name.clone(), k + 1));
                        }
                    }
                    self.frames.push(head);
                }
                TokenKind::Match => self.frames.push(Frame::new(FrameKind::MatchHead)),
                TokenKind::Struct => {
                    self.abandon_heads();
                    self.frames.push(Frame::new(FrameKind::StructHead));
                }
                TokenKind::If | TokenKind::While => {
                    self.abandon_heads();
                    self.frames.push(Frame::new(FrameKind::CondHead));
                }
                TokenKind::Return
                | TokenKind::Break
                | TokenKind::Continue
                | TokenKind::Print
                | TokenKind::Import => self.abandon_heads(),
                TokenKind::Fn => {
                    let header = parse_header(&toks[..limit], k);
                    match header.end {
                        HeaderEnd::Complete(open) => {
                            let mut body = Frame::new(FrameKind::FnBody);
                            body.vars = header
                                .params
                                .iter()
                                .map(|p| VarInfo {
                                    name: p.name.clone(),
                                    kind: VarKind::Param,
                                    at: p.at,
                                })
                                .collect();
                            self.frames.push(body);
                            k = open + 1;
                            continue;
                        }
                        HeaderEnd::Incomplete => {
                            self.in_header = true;
                            return;
                        }
                        HeaderEnd::Malformed(end) => {
                            k = end.max(k + 1);
                            continue;
                        }
                    }
                }
                TokenKind::LParen => self.frames.push(Frame::new(FrameKind::Paren)),
                TokenKind::LBracket => self.frames.push(Frame::new(FrameKind::Bracket)),
                TokenKind::RParen => self.close_delimiter(FrameKind::Paren),
                TokenKind::RBracket => self.close_delimiter(FrameKind::Bracket),
                TokenKind::LBrace => self.open_brace(k),
                TokenKind::RBrace => self.close_brace(),
                TokenKind::Arrow => self.arrow(k),
                TokenKind::Comma if self.top() == FrameKind::Arm => {
                    self.frames.pop();
                    self.drop_dead_pendings();
                }
                _ => {}
            }

            // 4) последний токен инициализатора на глубине объявления
            let depth = self.frames.len();
            if let Some(p) = self.pendings.last_mut() {
                if p.depth == depth && p.stage == Stage::Init {
                    p.last_operand_end = is_operand_end(kind);
                }
            }
            k += 1;
        }
    }
}

/// Что стоит за идентификатором (T019: переход к определению). Индексы —
/// токенов-имён в объявлениях.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Определения нет: не идентификатор, после `.`, аннотация типа и т. п.
    Nothing,
    /// Само имя в объявлении — результатом служит оно же.
    Declaration,
    /// Ключ литерала структуры `Имя { ключ: …`: имя структуры.
    Field { structure: String },
    /// Имя перед `{` литерала структуры.
    Structure,
    /// Вызов `имя(…)`; `variable` — ближайшая видимая переменная с этим именем.
    Call { variable: Option<usize> },
    /// Имя в позиции значения; `variable` — ближайшая видимая переменная.
    Value { variable: Option<usize> },
}

/// Ближайшее объявление имени среди областей (внутренние раньше внешних, в
/// области — позднее объявленные раньше). Тело `STRUCT` пропускается: слова
/// в нём — имена полей, а `LET` внутри (так бывает только в испорченном
/// тексте) ничего не объявляет — `lookup_at` на таком имени отвечает «ничего»,
/// и определение не должно вести туда, где определения нет (T019).
fn visible_variable(frames: &[Frame], name: &str) -> Option<usize> {
    frames
        .iter()
        .rev()
        .filter(|f| f.kind != FrameKind::StructDecl)
        .find_map(|f| f.vars.iter().rev().find(|v| v.name == name))
        .map(|v| v.at)
}

/// Документ, разобранный для подсказок.
#[derive(Debug, Clone)]
pub struct Document {
    pub tokens: Vec<TokenInfo>,
    pub text_mode: bool,
    pub globals: Globals,
}

/// Всё, что подсказки знают о позиции курсора.
#[derive(Debug, Clone)]
pub struct CursorInfo {
    pub lex: Lex,
    pub prefix: Prefix,
    pub context: Context,
}

impl Document {
    pub fn new(text: &str) -> Self {
        let Tokenized { tokens, text_mode } = tokenize(text);
        let globals = collect_globals(&tokens);
        Document {
            tokens,
            text_mode,
            globals,
        }
    }

    /// Индекс слова (ключевое слово или идентификатор) у позиции: токен,
    /// содержащий позицию (`начало <= позиция < конец`); если такого слова
    /// нет — слово, ЗАКАНЧИВАЮЩЕЕСЯ в позиции.
    pub fn word_at(&self, pos: Pos) -> Option<usize> {
        // первый токен, конец которого строго правее позиции
        let i = self.tokens.partition_point(|t| t.end() <= pos);
        if let Some(t) = self.tokens.get(i) {
            if t.start() <= pos && t.is_word() {
                return Some(i);
            }
        }
        if i > 0 && self.tokens[i - 1].end() == pos && self.tokens[i - 1].is_word() {
            return Some(i - 1);
        }
        None
    }

    /// Что известно у курсора: лексическое состояние, префикс и (если не
    /// текстовый режим) контекст по токенам.
    pub fn cursor_info(&self, text: &str, pos: Pos) -> CursorInfo {
        let lex = lexical_state(text, pos);
        let prefix = prefix_at(text, pos);
        let context = if self.text_mode || lex != Lex::Code {
            Context {
                blocked: None,
                variables: Vec::new(),
            }
        } else {
            self.context_at(pos, &prefix)
        };
        CursorInfo {
            lex,
            prefix,
            context,
        }
    }

    /// Контекст по токенам, закончившимся не правее курсора. Слово, которое
    /// набирают у курсора (непустой префикс), не входит в контекст, но
    /// считается началом операнда: оно закрывает незавершённое объявление.
    fn context_at(&self, pos: Pos, prefix: &Prefix) -> Context {
        let mut limit = self.tokens.partition_point(|t| t.end() <= pos);
        let has_partial = matches!(prefix.kind, PrefixKind::Ascii | PrefixKind::Glyph);
        if has_partial
            && limit > 0
            && self.tokens[limit - 1].start() == prefix.start
            && self.tokens[limit - 1].end() == pos
        {
            limit -= 1;
        }

        let mut scanner = Scanner::new(&self.tokens);
        scanner.run(limit);

        if !scanner.in_header {
            // объявление, оператор которого уже закончился к курсору: слово у
            // курсора после операнда или перевод строки после операнда
            let newline_after = limit > 0 && self.tokens[limit - 1].end().0 < pos.0;
            let depth = scanner.frames.len();
            let complete = matches!(
                scanner.pendings.last(),
                Some(p) if p.depth == depth
                    && p.stage == Stage::Init
                    && p.last_operand_end
                    && (has_partial || newline_after)
            );
            if complete {
                scanner.complete_pending(limit.saturating_sub(1));
            }
        }

        let blocked = self.blocked_at(&scanner, limit);
        let mut variables: Vec<VarInfo> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for frame in scanner.frames.iter().rev() {
            for var in frame.vars.iter().rev() {
                if seen.insert(&var.name) {
                    variables.push(var.clone());
                }
            }
        }
        Context { blocked, variables }
    }

    fn blocked_at(&self, scanner: &Scanner, limit: usize) -> Option<Blocked> {
        if scanner.in_header {
            return Some(Blocked::FnHeader);
        }
        let top = scanner.top();
        if top == FrameKind::StructDecl {
            return Some(Blocked::StructBody);
        }
        let last = limit.checked_sub(1).map(|i| &self.tokens[i].kind)?;
        match last {
            TokenKind::Dot => single_dot_before(&self.tokens, limit).then_some(Blocked::AfterDot),
            TokenKind::Let
            | TokenKind::Var
            | TokenKind::Const
            | TokenKind::Struct
            | TokenKind::For
            | TokenKind::Fn
            | TokenKind::Mut => Some(Blocked::DeclName),
            TokenKind::Colon if top != FrameKind::StructLit => Some(Blocked::TypePosition),
            TokenKind::Arrow if !matches!(top, FrameKind::Arm | FrameKind::MatchArms) => {
                Some(Blocked::TypePosition)
            }
            TokenKind::LBrace | TokenKind::Comma if top == FrameKind::StructLit => {
                Some(Blocked::StructKey)
            }
            _ => None,
        }
    }

    /// Операторы `LET`/`VAR`/`CONST` верхнего уровня, дошедшие до `=`
    /// (T019: структура файла), в порядке появления.
    pub fn top_level_variables(&self) -> Vec<TopVariable> {
        let mut scanner = Scanner::new(&self.tokens);
        scanner.record = true;
        scanner.run(self.tokens.len());
        scanner.finish();
        scanner.top_level
    }

    /// Для каждой `{` из `opens` (индексы токенов по возрастанию) — индекс
    /// парной `}`; `None`, если она не закрыта. Один проход со стеком: поиск
    /// на каждое объявление был бы квадратичным на вложенных объявлениях.
    pub fn matching_closers(&self, opens: &[usize]) -> Vec<Option<usize>> {
        let mut closers = vec![None; opens.len()];
        let mut wanted = 0;
        // для каждой открытой `{` — номер в `opens`, если он нужен
        let mut stack: Vec<Option<usize>> = Vec::new();
        for (i, tok) in self.tokens.iter().enumerate() {
            match tok.kind {
                TokenKind::LBrace => {
                    if opens.get(wanted) == Some(&i) {
                        stack.push(Some(wanted));
                        wanted += 1;
                    } else {
                        stack.push(None);
                    }
                }
                TokenKind::RBrace => {
                    if let Some(Some(n)) = stack.pop() {
                        closers[n] = Some(i);
                    }
                }
                _ => {}
            }
        }
        closers
    }

    /// Что значит идентификатор под индексом `i` (T019): объявление, ключ или
    /// имя литерала структуры, вызов, значение — или ничего. Область видимости
    /// — та же, что у автодополнения.
    pub fn lookup_at(&self, i: usize) -> Lookup {
        let toks = &self.tokens;
        let Some(TokenKind::Ident(name)) = toks.get(i).map(|t| &t.kind) else {
            return Lookup::Nothing;
        };

        // 1) заголовок функции: имя функции и параметры объявляются здесь же,
        // всё остальное в заголовке (типы) определения не имеет. Слово, на
        // котором заголовок «сломался» (`FN f(a b)` — второе `b`), в него уже не
        // входит; ответ на нём — «ничего» — даёт сканер ниже: токены до слова
        // кончаются внутри заголовка. Граница `<`/`<=` поэтому на ответ не влияет.
        if let Some(fn_at) = toks[..i]
            .iter()
            .rposition(|t| matches!(t.kind, TokenKind::Fn))
        {
            let header = parse_header(toks, fn_at);
            let inside = match header.end {
                HeaderEnd::Complete(open) => i < open,
                HeaderEnd::Malformed(end) => i < end,
                HeaderEnd::Incomplete => true,
            };
            if inside {
                let is_name = header.name.is_some() && header.saw_paren && i == fn_at + 1;
                let is_param = header.params.iter().any(|p| p.at == i);
                return if is_name || is_param {
                    Lookup::Declaration
                } else {
                    Lookup::Nothing
                };
            }
        }

        let mut scanner = Scanner::new(toks);
        scanner.run(i);

        // 2) роль слова по структуре кода у него (если токены до слова кончились
        // внутри заголовка функции — а так бывает у слова, на котором заголовок
        // «сломался», — `blocked_at` отвечает `FnHeader`, и ответ «ничего»)
        match self.blocked_at(&scanner, i) {
            Some(Blocked::AfterDot | Blocked::TypePosition | Blocked::FnHeader) => {
                return Lookup::Nothing;
            }
            Some(Blocked::StructBody) => {
                // поле: между `{` и словом только идентификаторы и запятые
                // (как в `collect_declarations`), а `{` — у `STRUCT Имя {`
                let open = scanner.frames.last().and_then(|f| f.open_at);
                let is_field = open.is_some_and(|open| {
                    // перед `{` — имя, перед именем — `STRUCT`
                    let before = open
                        .checked_sub(2)
                        .map(|k| (&toks[k].kind, &toks[k + 1].kind));
                    matches!(before, Some((TokenKind::Struct, TokenKind::Ident(_))))
                        && toks[open + 1..i]
                            .iter()
                            .all(|t| matches!(t.kind, TokenKind::Ident(_) | TokenKind::Comma))
                });
                return if is_field {
                    Lookup::Declaration
                } else {
                    Lookup::Nothing
                };
            }
            Some(Blocked::StructKey) => {
                let structure = scanner
                    .frames
                    .last()
                    .and_then(|f| f.open_at)
                    .and_then(|open| open.checked_sub(1))
                    .and_then(|before| match &toks[before].kind {
                        TokenKind::Ident(name) => Some(name.clone()),
                        _ => None,
                    });
                return match structure {
                    Some(structure) => Lookup::Field { structure },
                    None => Lookup::Nothing,
                };
            }
            Some(Blocked::DeclName) => {
                let declares = match toks[i - 1].kind {
                    // имя прочтёт объявление, которое ждёт имя на этой глубине
                    TokenKind::Let | TokenKind::Var | TokenKind::Const => matches!(
                        scanner.pendings.last(),
                        Some(p) if p.depth == scanner.frames.len() && p.stage == Stage::Name
                    ),
                    TokenKind::Struct => {
                        matches!(toks.get(i + 1).map(|t| &t.kind), Some(TokenKind::LBrace))
                    }
                    TokenKind::For => true,
                    // `FN`/`MUT`: заголовок функции разобран выше
                    _ => false,
                };
                return if declares {
                    Lookup::Declaration
                } else {
                    Lookup::Nothing
                };
            }
            None => {}
        }

        let next = toks.get(i + 1).map(|t| &t.kind);

        // 3) привязка `MATCH`: образец-имя (кроме `_`) перед `->`
        if matches!(next, Some(TokenKind::Arrow))
            && scanner.top() == FrameKind::MatchArms
            && name != "_"
            && (i < 1 || matches!(toks[i - 1].kind, TokenKind::LBrace | TokenKind::Comma))
        {
            return Lookup::Declaration;
        }

        // 4) слово начинает следующий оператор: объявление до него закончено
        if scanner.pending_ends_before(i) {
            scanner.complete_pending(i.saturating_sub(1));
        }
        let variable = visible_variable(&scanner.frames, name);

        if matches!(next, Some(TokenKind::LBrace))
            && opens_struct_literal(toks, i + 1, scanner.top())
        {
            return Lookup::Structure;
        }
        if matches!(next, Some(TokenKind::LParen)) {
            return Lookup::Call { variable };
        }
        Lookup::Value { variable }
    }
}

/// Ключевые слова, чья мнемоника начинается с `prefix` (без учёта регистра);
/// `prefix` — ASCII.
pub fn keywords_with_ascii_prefix(prefix: &str) -> Vec<&'static vocabulary::KeywordDoc> {
    let upper = prefix.to_ascii_uppercase();
    vocabulary::KEYWORDS
        .iter()
        .filter(|k| k.mnemonic.starts_with(&upper))
        .collect()
}

/// Ключевые слова, чья мнемоника начинается с декодированного префикса из
/// букв SGA.
pub fn keywords_with_glyph_prefix(prefix: &str) -> Vec<&'static vocabulary::KeywordDoc> {
    let decoded = decode_word(prefix);
    vocabulary::KEYWORDS
        .iter()
        .filter(|k| k.mnemonic.starts_with(&decoded))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Заменяет `<<СЛОВО>>` словом из букв SGA.
    fn enc(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some(i) = rest.find("<<") {
            out.push_str(&rest[..i]);
            let tail = &rest[i + 2..];
            let j = tail.find(">>").expect("незакрытый <<");
            out.push_str(&crate::sga_alphabet::encode_word(&tail[..j]));
            rest = &tail[j + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Текст без маркера курсора `|` и позиция курсора.
    fn parse(src: &str) -> (String, Pos) {
        let encoded = enc(src);
        let bar = encoded.find('|').expect("маркер курсора `|`");
        let before = &encoded[..bar];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        let text = format!("{}{}", before, &encoded[bar + 1..]);
        (text, (line, col))
    }

    fn info(src: &str) -> CursorInfo {
        let (text, pos) = parse(src);
        Document::new(&text).cursor_info(&text, pos)
    }

    fn vis(src: &str) -> Vec<String> {
        info(src)
            .context
            .variables
            .iter()
            .map(|v| v.name.clone())
            .collect()
    }

    fn blocked(src: &str) -> Option<Blocked> {
        info(src).context.blocked
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── токенизация ────────────────────────────────────────────────

    #[test]
    fn tokenize_valid_text_drops_eof_and_keeps_positions() {
        let t = tokenize("ab cd\n  ef");
        assert!(!t.text_mode);
        let seen: Vec<(String, Pos, Pos)> = t
            .tokens
            .iter()
            .map(|t| (format!("{:?}", t.kind), t.start(), t.end()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("Ident(\"ab\")".to_string(), (1, 1), (1, 3)),
                ("Ident(\"cd\")".to_string(), (1, 4), (1, 6)),
                ("Ident(\"ef\")".to_string(), (2, 3), (2, 5)),
            ]
        );
    }

    #[test]
    fn tokenize_empty_text_has_no_tokens() {
        let t = tokenize("");
        assert!(t.tokens.is_empty() && !t.text_mode);
    }

    #[test]
    fn a_lexical_error_is_blanked_and_later_tokens_keep_their_positions() {
        let t = tokenize("a @ b");
        assert!(!t.text_mode);
        let names: Vec<_> = t.tokens.iter().map(|t| (&t.kind, t.start())).collect();
        assert_eq!(
            names,
            vec![
                (&TokenKind::Ident("a".into()), (1, 1)),
                (&TokenKind::Ident("b".into()), (1, 5)),
            ]
        );
    }

    #[test]
    fn errors_before_and_after_the_interesting_place_do_not_hide_the_tokens() {
        let t = tokenize("# x\nfoo $ bar\n@");
        let idents: Vec<_> = t
            .tokens
            .iter()
            .filter_map(|t| match &t.kind {
                TokenKind::Ident(n) => Some((n.clone(), t.start())),
                _ => None,
            })
            .collect();
        assert_eq!(
            idents,
            vec![
                ("x".to_string(), (1, 3)),
                ("foo".to_string(), (2, 1)),
                ("bar".to_string(), (2, 7)),
            ]
        );
    }

    #[test]
    fn an_unterminated_string_only_loses_the_rest_of_the_text() {
        let t = tokenize("foo \"abc\nbar");
        assert!(!t.text_mode);
        assert_eq!(t.tokens.len(), 1);
        assert_eq!(t.tokens[0].kind, TokenKind::Ident("foo".into()));
    }

    #[test]
    fn an_unknown_sga_word_is_blanked() {
        let text = format!("a {} b", crate::sga_alphabet::encode_word("XYZ"));
        let t = tokenize(&text);
        assert!(!t.text_mode);
        let b = t.tokens.last().expect("токен b");
        assert_eq!(b.kind, TokenKind::Ident("b".into()));
        assert_eq!(b.start(), (1, 7), "позиция b не сдвинулась");
    }

    #[test]
    fn sixteen_errors_are_tolerated_and_the_seventeenth_means_text_mode() {
        let sixteen = "@ ".repeat(MAX_RELEX) + "end";
        let t = tokenize(&sixteen);
        assert!(!t.text_mode, "{MAX_RELEX} ошибок терпимы");
        assert_eq!(t.tokens.len(), 1);
        let seventeen = "@ ".repeat(MAX_RELEX + 1) + "end";
        let t = tokenize(&seventeen);
        assert!(t.text_mode, "на {} ошибке — текстовый режим", MAX_RELEX + 1);
        assert!(t.tokens.is_empty());
    }

    #[test]
    fn blank_range_keeps_line_breaks_and_length() {
        assert_eq!(blank_range("ab\ncd\nef", (1, 2), (3, 2)), "a \n  \n f");
        let text = "x \"a\nb\" y";
        let blanked = blank_range(text, (1, 3), (2, 3));
        assert_eq!(blanked.chars().count(), text.chars().count());
        assert_eq!(blanked.matches('\n').count(), 1);
        assert_eq!(blanked, "x   \n   y");
    }

    #[test]
    fn blank_range_widens_an_empty_range_to_one_character() {
        assert_eq!(blank_range("abc", (1, 2), (1, 2)), "a c");
        assert_eq!(blank_range("abc", (1, 2), (1, 1)), "a c");
    }

    // ── лексическое состояние ──────────────────────────────────────

    fn lex_at(src: &str) -> Lex {
        let (text, pos) = parse(src);
        lexical_state(&text, pos)
    }

    #[test]
    fn lexical_state_in_code_strings_and_comments() {
        assert_eq!(lex_at("x = |"), Lex::Code);
        assert_eq!(
            lex_at("x = \"|"),
            Lex::Str,
            "сразу после открывающей кавычки"
        );
        assert_eq!(lex_at("x = \"ab|c\""), Lex::Str);
        assert_eq!(
            lex_at("x = \"abc|\""),
            Lex::Str,
            "перед закрывающей кавычкой"
        );
        assert_eq!(lex_at("x = \"abc\"|"), Lex::Code, "после закрывающей");
        assert_eq!(lex_at("x = // note|"), Lex::Comment);
        assert_eq!(lex_at("x = //|"), Lex::Comment);
        assert_eq!(lex_at("x = /|/"), Lex::Code, "между двумя косыми чертами");
        assert_eq!(lex_at("// c\n|"), Lex::Code, "комментарий кончился на \\n");
        assert_eq!(lex_at("// c|\nx"), Lex::Comment, "конец строки комментария");
    }

    #[test]
    fn lexical_state_handles_escapes_and_nested_markers() {
        assert_eq!(lex_at(r#"x = "a\"b|""#), Lex::Str, "экранированная кавычка");
        assert_eq!(
            lex_at(r#"x = "a\\"|"#),
            Lex::Code,
            "\\\\ не экранирует кавычку"
        );
        assert_eq!(lex_at(r#"x = "a // b|""#), Lex::Str, "// внутри строки");
        assert_eq!(
            lex_at("// say \"hi|"),
            Lex::Comment,
            "кавычка в комментарии"
        );
        assert_eq!(lex_at("a / / b|"), Lex::Code, "две отдельные косые черты");
        assert_eq!(
            lex_at("\"a\nb|\n\""),
            Lex::Str,
            "строка на нескольких строках"
        );
        assert_eq!(
            lex_at("\"unclosed\n|"),
            Lex::Str,
            "незакрытая строка до конца"
        );
    }

    #[test]
    fn lexical_state_for_a_position_past_the_end_is_the_final_state() {
        assert_eq!(lexical_state("\"abc", (9, 9)), Lex::Str);
        assert_eq!(lexical_state("// c", (9, 9)), Lex::Comment);
        assert_eq!(lexical_state("", (1, 1)), Lex::Code);
    }

    #[test]
    fn lexical_state_agrees_with_the_lexer_about_comment_and_string_boundaries() {
        // после комментария и строки обычный код снова лексируется
        let text = "a // \"not a string\nb \"s // not a comment\" c";
        let tokens = tokenize(text).tokens;
        let idents: Vec<_> = tokens
            .iter()
            .filter_map(|t| match &t.kind {
                TokenKind::Ident(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(idents, ["a", "b", "c"]);
        assert_eq!(lexical_state(text, (2, 9)), Lex::Str);
        assert_eq!(lexical_state(text, (2, 32)), Lex::Code);
    }

    // ── префикс ───────────────────────────────────────────────────

    fn prefix(src: &str) -> (PrefixKind, String, Pos) {
        let (text, pos) = parse(src);
        let p = prefix_at(&text, pos);
        (p.kind, p.text, p.start)
    }

    #[test]
    fn prefix_is_the_ascii_word_left_of_the_cursor() {
        assert_eq!(prefix("pri|"), (PrefixKind::Ascii, "pri".into(), (1, 1)));
        assert_eq!(
            prefix("x = str_sp|"),
            (PrefixKind::Ascii, "str_sp".into(), (1, 5))
        );
        assert_eq!(prefix("  a1|"), (PrefixKind::Ascii, "a1".into(), (1, 3)));
        assert_eq!(prefix("_|"), (PrefixKind::Ascii, "_".into(), (1, 1)));
        assert_eq!(
            prefix("foo(ba|r)"),
            (PrefixKind::Ascii, "ba".into(), (1, 5))
        );
    }

    #[test]
    fn prefix_is_empty_after_a_separator_or_at_line_start() {
        assert_eq!(prefix("|"), (PrefixKind::Empty, "".into(), (1, 1)));
        assert_eq!(prefix("x = |"), (PrefixKind::Empty, "".into(), (1, 5)));
        assert_eq!(prefix("foo(|"), (PrefixKind::Empty, "".into(), (1, 5)));
        assert_eq!(prefix("a\n|b"), (PrefixKind::Empty, "".into(), (2, 1)));
        assert_eq!(prefix("p.|"), (PrefixKind::Empty, "".into(), (1, 3)));
    }

    #[test]
    fn prefix_starting_with_a_digit_is_not_a_word() {
        assert_eq!(prefix("12|").0, PrefixKind::Digit);
        assert_eq!(prefix("1a|").0, PrefixKind::Digit);
        assert_eq!(prefix("x = 3.5|").0, PrefixKind::Digit);
        assert_eq!(
            prefix("a1|").0,
            PrefixKind::Ascii,
            "цифра внутри слова — слово"
        );
    }

    #[test]
    fn prefix_of_sga_letters_is_a_glyph_prefix() {
        let (kind, text, start) = prefix("x = <<LE>>|");
        assert_eq!(kind, PrefixKind::Glyph);
        assert_eq!(text, crate::sga_alphabet::encode_word("LE"));
        assert_eq!(start, (1, 5));
        // ASCII-слово и слово из букв — разные префиксы
        assert_eq!(prefix("ab<<LE>>|").0, PrefixKind::Glyph);
    }

    #[test]
    fn prefix_counts_columns_in_characters_not_bytes() {
        let (kind, text, start) = prefix("я😀 ab|");
        assert_eq!(
            (kind, text.as_str(), start),
            (PrefixKind::Ascii, "ab", (1, 4))
        );
    }

    // ── глобальные объявления ──────────────────────────────────────

    fn globals_of(src: &str) -> Globals {
        collect_globals(&tokenize(&enc(src)).tokens)
    }

    #[test]
    fn functions_are_collected_with_params_mut_types_and_return() {
        let g = globals_of(
            "<<FN>> add(a, <<MUT>> b: int, c: <<NIL>>) -> float { }\n<<FN>> noargs() { }",
        );
        assert_eq!(g.functions.len(), 2);
        let add = &g.functions[0];
        assert_eq!(add.name, "add");
        assert_eq!(
            add.params,
            vec![
                ParamInfo {
                    name: "a".into(),
                    mutable: false,
                    ty: None,
                    at: 3
                },
                ParamInfo {
                    name: "b".into(),
                    mutable: true,
                    ty: Some("int".into()),
                    at: 6
                },
                ParamInfo {
                    name: "c".into(),
                    mutable: false,
                    ty: Some("NIL".into()),
                    at: 10
                },
            ]
        );
        assert_eq!(add.ret.as_deref(), Some("float"));
        assert_eq!(add.line, 1);
        assert_eq!(add.signature(), "add(a, MUT b: int, c: NIL) -> float");
        assert_eq!(g.functions[1].signature(), "noargs()");
        assert_eq!(g.functions[1].line, 2);
    }

    #[test]
    fn lambdas_are_not_functions_and_structs_have_fields() {
        let g = globals_of(
            "<<LET>> f = <<FN>>(x) { <<RETURN>> x; };\n<<STRUCT>> Point { x, y }\n<<STRUCT>> Empty { }",
        );
        assert!(g.functions.is_empty());
        assert_eq!(g.structs.len(), 2);
        assert_eq!(g.structs[0].name, "Point");
        assert_eq!(g.structs[0].fields, strs(&["x", "y"]));
        assert_eq!(g.structs[0].line, 2);
        assert_eq!(g.structs[1].fields, Vec::<String>::new());
    }

    #[test]
    fn the_first_declaration_of_a_name_wins() {
        let g = globals_of(
            "<<FN>> f(a) { }\n<<FN>> f(a, b) { }\n<<STRUCT>> S { x }\n<<STRUCT>> S { y }",
        );
        assert_eq!(g.functions.len(), 1);
        assert_eq!(g.functions[0].params.len(), 1);
        assert_eq!(g.structs.len(), 1);
        assert_eq!(g.structs[0].fields, strs(&["x"]));
        assert_eq!(g.function("f").map(|f| f.line), Some(1));
        assert!(g.function("g").is_none());
    }

    #[test]
    fn half_written_declarations_are_collected_as_far_as_they_go() {
        // заголовок не дописан: имя и параметры до курсора известны
        let g = globals_of("<<FN>> half(a, <<MUT>> b");
        assert_eq!(g.functions.len(), 1);
        assert_eq!(g.functions[0].signature(), "half(a, MUT b)");
        // без `(` это ещё не объявление функции; структура без `{` — тоже
        assert!(globals_of("<<FN>> nameonly").functions.is_empty());
        assert!(globals_of("<<STRUCT>> NoBrace").structs.is_empty());
        // функция ниже незакрытой скобки не пропадает
        let g = globals_of("<<IF>> <<TRUE>> {\n<<FN>> later() { }");
        assert_eq!(g.functions.len(), 1);
    }

    #[test]
    fn collect_globals_survives_any_token_soup() {
        for src in [
            "<<FN>>",
            "<<FN>> (",
            "<<FN>> f (",
            "<<FN>> f ( )",
            "<<FN>> f ( ) ->",
            "<<FN>> f ( : ) { }",
            "<<FN>> f ( a : )",
            "<<FN>> f ( a b )",
            "<<STRUCT>>",
            "<<STRUCT>> S",
            "<<STRUCT>> S {",
            "<<STRUCT>> S { x , , y ,",
            "<<FN>> <<FN>> <<FN>> f ( ( ( {",
        ] {
            let _ = globals_of(src);
        }
    }

    // ── области видимости ─────────────────────────────────────────

    #[test]
    fn a_global_variable_is_visible_after_its_statement_only() {
        assert_eq!(vis("<<LET>> x = 1; |"), strs(&["x"]));
        assert_eq!(vis("|\n<<LET>> x = 1;"), Vec::<String>::new());
        assert_eq!(
            vis("<<LET>> x = |"),
            Vec::<String>::new(),
            "в своём инициализаторе имя не видно"
        );
        assert_eq!(vis("<<LET>> x = 1 + |"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> x = foo(|"), Vec::<String>::new());
        assert_eq!(
            vis("<<LET>> x = fo|"),
            Vec::<String>::new(),
            "слово в инициализаторе — не конец оператора"
        );
    }

    #[test]
    fn a_statement_without_a_semicolon_ends_before_the_next_statement() {
        assert_eq!(
            vis("<<LET>> x = 1\n|"),
            strs(&["x"]),
            "перевод строки после операнда"
        );
        assert_eq!(
            vis("<<LET>> x = 1 y|"),
            strs(&["x"]),
            "слово после операнда"
        );
        assert_eq!(
            vis("<<LET>> x = 1\n<<PRINT>>(|)"),
            strs(&["x"]),
            "ключевое слово оператора"
        );
        assert_eq!(
            vis("<<LET>> x = 1 |"),
            Vec::<String>::new(),
            "на той же строке выражение может продолжиться"
        );
        assert_eq!(
            vis("<<LET>> x = 1 +\n|"),
            Vec::<String>::new(),
            "после оператора выражение не закончено"
        );
        assert_eq!(vis("<<LET>> x = foo(1)\n|"), strs(&["x"]));
        assert_eq!(vis("<<LET>> x = [1, 2]\n|"), strs(&["x"]));
        assert_eq!(vis("<<LET>> x = 1\n<<LET>> y = 2\n|"), strs(&["y", "x"]));
    }

    #[test]
    fn a_continuing_expression_does_not_end_the_statement() {
        // `(`, `[`, `-`, `.` продолжают выражение, а не начинают оператор
        assert_eq!(vis("<<LET>> x = a\n(|"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> x = a\n[|"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> x = a\n- |"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> x = a\n.b|"), Vec::<String>::new());
    }

    #[test]
    fn blocks_open_and_close_scopes() {
        assert_eq!(vis("{ <<LET>> a = 1; | }"), strs(&["a"]));
        assert_eq!(vis("{ <<LET>> a = 1; } |"), Vec::<String>::new());
        assert_eq!(
            vis("<<LET>> g = 1; { <<LET>> a = 2; | }"),
            strs(&["a", "g"]),
            "внутренние раньше внешних"
        );
        assert_eq!(
            vis("<<IF>> <<TRUE>> { <<LET>> a = 1; } <<LET>> b = 2; |"),
            strs(&["b"])
        );
        assert_eq!(
            vis("<<WHILE>> <<TRUE>> { <<LET>> a = 1; } <<ELSE>> { | }"),
            Vec::<String>::new()
        );
        assert_eq!(
            vis("<<IF>> <<TRUE>> { } <<ELSE>> { <<LET>> e = 1; | }"),
            strs(&["e"])
        );
    }

    #[test]
    fn a_closing_brace_closes_the_innermost_block() {
        assert_eq!(
            vis("{ <<LET>> a = 1; { <<LET>> b = 2; } | }"),
            strs(&["a"]),
            "`}}` закрыла внутренний блок, внешний открыт"
        );
        assert_eq!(
            vis("{ <<LET>> a = 1; { <<LET>> b = 2; { <<LET>> c = 3; } | } | }"),
            strs(&["b", "a"]),
            "второй уровень вложенности"
        );
        assert_eq!(
            vis("{ <<LET>> a = 1; { <<LET>> b = 2; } } |"),
            Vec::<String>::new()
        );
        assert_eq!(
            vis("<<FN>> f(p) { <<IF>> p { <<LET>> a = 1; } | }"),
            strs(&["p"]),
            "блок внутри тела функции"
        );
    }

    #[test]
    fn shadowing_keeps_the_innermost_declaration_once() {
        let (text, pos) = parse("<<LET>> a = 1; { <<VAR>> a = 2; | }");
        let ctx = Document::new(&text).cursor_info(&text, pos).context;
        assert_eq!(
            ctx.variables,
            vec![VarInfo {
                name: "a".into(),
                kind: VarKind::Var,
                at: 7
            }]
        );
    }

    #[test]
    fn declaration_kinds_are_remembered() {
        let (text, pos) = parse("<<LET>> a = 1; <<VAR>> b = 2; <<CONST>> c = 3; |");
        let ctx = Document::new(&text).cursor_info(&text, pos).context;
        let kinds: Vec<_> = ctx
            .variables
            .iter()
            .map(|v| (v.name.as_str(), v.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("c", VarKind::Const),
                ("b", VarKind::Var),
                ("a", VarKind::Let)
            ]
        );
    }

    #[test]
    fn a_typed_declaration_is_declared_after_its_statement() {
        assert_eq!(vis("<<LET>> x: int = 1; |"), strs(&["x"]));
        assert_eq!(vis("<<LET>> x: <<NIL>> = <<NIL>>; |"), strs(&["x"]));
        assert_eq!(vis("<<LET>> x: int = |"), Vec::<String>::new());
    }

    #[test]
    fn function_parameters_are_visible_in_the_body_only() {
        // в области позднее объявленное имя идёт раньше
        assert_eq!(
            vis("<<FN>> f(a, <<MUT>> b: int) -> int { | }"),
            strs(&["b", "a"])
        );
        assert_eq!(vis("<<FN>> f(a) { } |"), Vec::<String>::new());
        assert_eq!(vis("<<FN>> f(a) { <<LET>> b = 1; | }"), strs(&["b", "a"]));
        assert_eq!(
            vis("<<FN>> f(a) { <<LET>> b = 1; } <<LET>> c = 2; |"),
            strs(&["c"])
        );
    }

    #[test]
    fn a_function_body_sees_only_globals_declared_before_it() {
        assert_eq!(
            vis("<<LET>> g = 1; <<FN>> f() { | } <<LET>> h = 2;"),
            strs(&["g"])
        );
        assert_eq!(
            vis("<<FN>> f() { } <<LET>> h = 2; <<LET>> g = 3; |"),
            strs(&["g", "h"])
        );
        // на глобальное объявление без `;` перед `FN` тоже действует правило конца оператора
        assert_eq!(vis("<<LET>> g = 1 <<FN>> f() { | }"), strs(&["g"]));
    }

    #[test]
    fn lambda_parameters_and_captured_variables() {
        assert_eq!(
            vis("<<LET>> k = 1; <<LET>> f = <<FN>>(a, b) { | };"),
            strs(&["b", "a", "k"]),
            "сама `f` в своём инициализаторе не видна"
        );
        assert_eq!(
            vis("<<LET>> f = <<FN>>(a) { <<RETURN>> a; }; |"),
            strs(&["f"])
        );
        assert_eq!(
            vis("<<LET>> f = <<FN>>(a) { }\n|"),
            strs(&["f"]),
            "`}}` — конец операнда"
        );
        assert_eq!(
            vis("<<FN>> make(n) { <<RETURN>> <<FN>>(y) { | }; }"),
            strs(&["y", "n"])
        );
    }

    #[test]
    fn a_for_variable_lives_in_the_loop_body() {
        assert_eq!(vis("<<FOR>> i <<IN>> 0..3 { | }"), strs(&["i"]));
        assert_eq!(
            vis("<<FOR>> i <<IN>> 0..| { }"),
            Vec::<String>::new(),
            "в диапазоне переменной нет"
        );
        assert_eq!(vis("<<FOR>> i <<IN>> 0..3 { } |"), Vec::<String>::new());
        assert_eq!(
            vis("<<FOR>> i <<IN>> 0..3 { <<LET>> a = i; | }"),
            strs(&["a", "i"])
        );
        assert_eq!(
            vis("<<FOR>> i <<IN>> 0..3 { <<FOR>> j <<IN>> 0..i { | } }"),
            strs(&["j", "i"])
        );
    }

    #[test]
    fn a_match_binding_lives_in_its_own_arm() {
        assert_eq!(vis("<<MATCH>> 1 { n -> | }"), strs(&["n"]));
        assert_eq!(vis("<<MATCH>> 1 { n -> 1, | }"), Vec::<String>::new());
        assert_eq!(vis("<<MATCH>> 1 { 1 -> |"), Vec::<String>::new());
        assert_eq!(
            vis("<<MATCH>> 1 { _ -> | }"),
            Vec::<String>::new(),
            "`_` ничего не связывает"
        );
        assert_eq!(vis("<<MATCH>> 1 { 1 -> 2, n -> n + | }"), strs(&["n"]));
        assert_eq!(
            vis("<<MATCH>> | { n -> 1 }"),
            Vec::<String>::new(),
            "в выражении выбора привязки нет"
        );
        assert_eq!(vis("<<MATCH>> 1 { n -> 1 } |"), Vec::<String>::new());
        assert_eq!(
            vis("<<LET>> k = 1; <<MATCH>> k { n -> | }"),
            strs(&["n", "k"])
        );
    }

    #[test]
    fn match_arms_with_nested_commas_and_lambdas() {
        assert_eq!(vis("<<MATCH>> 1 { n -> foo(1, 2), m -> | }"), strs(&["m"]));
        assert_eq!(vis("<<MATCH>> 1 { n -> [1, 2], m -> | }"), strs(&["m"]));
        assert_eq!(
            vis("<<MATCH>> 1 { n -> <<FN>>(a) { | } }"),
            strs(&["a", "n"])
        );
        assert_eq!(
            vis("<<MATCH>> 1 { n -> <<MATCH>> n { m -> | } }"),
            strs(&["m", "n"])
        );
        assert_eq!(vis("<<LET>> r = <<MATCH>> 1 { n -> 1 }\n|"), strs(&["r"]));
    }

    #[test]
    fn struct_literals_and_declarations_add_no_names() {
        assert_eq!(vis("<<STRUCT>> P { x, y } |"), Vec::<String>::new());
        assert_eq!(
            vis("<<LET>> a = 1; <<LET>> p = P { x: a, y: | };"),
            strs(&["a"])
        );
        assert_eq!(vis("<<LET>> p = P { x: 1 }; |"), strs(&["p"]));
        assert_eq!(vis("<<LET>> p = P { }; |"), strs(&["p"]), "пустой литерал");
    }

    #[test]
    fn a_brace_not_after_an_identifier_is_a_block_never_a_struct_literal() {
        // `{ }` после слова языка, `;`, `{` или `)` — блок, а не литерал
        // структуры: в нём не «ключ поля», а обычный оператор
        assert_eq!(blocked("<<IF>> x { } <<ELSE>> { | }"), None);
        assert_eq!(blocked("<<LET>> a = 1;\n{ | }"), None);
        assert_eq!(blocked("{ { | } }"), None);
        assert_eq!(blocked("<<LET>> g = <<FN>>(a) { | }"), None);
        assert_eq!(blocked("<<FN>> f() { | }"), None);
        // а после идентификатора и с полем внутри — литерал: ключ
        assert_eq!(blocked("P { | }"), Some(Blocked::StructKey));
        assert_eq!(
            blocked("<<LET>> p = P { x: 1, | }"),
            Some(Blocked::StructKey)
        );
        // литерал с полем: `имя { ключ :` — через `Ident Ident :`
        assert_eq!(vis("<<LET>> a = 1; <<ELSE>> { x: | }"), strs(&["a"]));
        // пока за `{` нет ни `}`, ни `ключ :`, лукахед литерала не видит и первый ключ
        // не блокируется (TD-027); `}` или двоеточие после ключа — блокируют
        assert_eq!(blocked("<<LET>> p = P { | "), None);
        assert_eq!(blocked("<<LET>> p = P { ab| }"), None);
        assert_eq!(
            blocked("<<LET>> p = P { ab|: 1 }"),
            Some(Blocked::StructKey)
        );
    }

    #[test]
    fn unbalanced_closers_close_the_scopes_they_enclose() {
        // `}` закрывает и незакрытые скобки внутри блока
        assert_eq!(vis("{ <<LET>> a = 1; foo( } |"), Vec::<String>::new());
        // лишняя `}` на верхнем уровне ничего не ломает
        assert_eq!(vis("} } <<LET>> a = 1; |"), strs(&["a"]));
        // лишняя `)` не выходит за фигурную область
        assert_eq!(vis("{ <<LET>> a = 1; ) | }"), strs(&["a"]));
        // ... даже если снаружи блока есть незакрытая скобка того же вида
        assert_eq!(vis("f({ <<LET>> a = 1; ) |"), strs(&["a"]));
        assert_eq!(vis("f[{ <<LET>> a = 1; ] |"), strs(&["a"]));
        // `]` и `)` закрывают свои скобки
        assert_eq!(vis("<<LET>> a = [1, (2)]; |"), strs(&["a"]));
    }

    #[test]
    fn an_unclosed_block_keeps_its_names_visible() {
        assert_eq!(vis("<<FN>> f(a) { <<LET>> b = 1; |"), strs(&["b", "a"]));
        assert_eq!(
            vis("<<IF>> <<TRUE>> { <<LET>> a = 1;\n<<LET>> b = 2\n|"),
            strs(&["b", "a"])
        );
    }

    #[test]
    fn a_pending_declaration_dies_with_its_block() {
        assert_eq!(vis("{ <<LET>> a = 1 } |"), Vec::<String>::new());
        assert_eq!(
            vis("<<FN>> f() { <<LET>> a = 1 } <<LET>> b = 2; |"),
            strs(&["b"])
        );
    }

    #[test]
    fn broken_declarations_do_not_declare_anything() {
        assert_eq!(vis("<<LET>> ; |"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> 5; |"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> x <<LET>> y = 1; |"), strs(&["y"]));
        assert_eq!(vis("<<LET>> x : ; |"), Vec::<String>::new());
    }

    #[test]
    fn an_abandoned_declaration_does_not_capture_later_names() {
        // `LET x y = 1;` — не объявление `x`: после лишнего слова разбор не
        // продолжается и `=` не достаётся брошенному объявлению
        assert_eq!(vis("<<LET>> x y = 1; |"), Vec::<String>::new());
        assert_eq!(vis("<<LET>> x y = 1;\n<<LET>> z = 2; |"), strs(&["z"]));
        // брошенное `LET` не превращает последующее присваивание в объявление
        assert_eq!(
            vis("<<LET>> <<LET>> b = 1;\nx = 5;\n|"),
            strs(&["b"]),
            "`x = 5;` — присваивание, а не объявление"
        );
        assert_eq!(
            vis("<<VAR>> <<CONST>> c: int = 1;\nx = 5;\n|"),
            strs(&["c"])
        );
    }

    #[test]
    fn a_statement_keyword_abandons_an_unfinished_head() {
        // Шапка конструкции без `{`, а за ней другой оператор: шапка брошена,
        // имя из следующего объявления остаётся в объемлющей области, а
        // следующая `{` — обычный блок, а не тело брошенной конструкции.
        for head in [
            "<<IF>>",
            "<<WHILE>>",
            "<<FOR>> i",
            "<<MATCH>> v",
            "<<STRUCT>> P",
        ] {
            for decl in ["<<LET>> y = 1;", "<<VAR>> y = 1;", "<<CONST>> y = 1;"] {
                assert_eq!(
                    vis(&format!("{head}\n{decl}\n{{ }}\n|")),
                    strs(&["y"]),
                    "{head} / {decl}"
                );
            }
        }
        for stmt in [
            "<<PRINT>>(1);",
            "<<RETURN>>;",
            "<<BREAK>>;",
            "<<CONTINUE>>;",
            "<<IMPORT>> \"m.sga\";",
        ] {
            assert_eq!(
                vis(&format!("<<FOR>> i\n{stmt}\n{{ | }}")),
                Vec::<String>::new(),
                "после брошенного FOR переменная цикла не видна: {stmt}"
            );
            assert_eq!(
                vis(&format!("<<MATCH>> v\n{stmt}\n{{ a -> | }}")),
                Vec::<String>::new(),
                "после брошенного MATCH `a ->` ничего не связывает: {stmt}"
            );
            assert_eq!(
                blocked(&format!("<<STRUCT>> P\n{stmt}\n{{ | }}")),
                None,
                "после брошенного STRUCT `{{` — обычный блок: {stmt}"
            );
        }
        // а незабытая шапка остаётся шапкой
        assert_eq!(vis("<<FOR>> i <<IN>> x { | }"), strs(&["i"]));
        assert_eq!(blocked("<<STRUCT>> P { | }"), Some(Blocked::StructBody));
    }

    // ── места без подсказок ───────────────────────────────────────

    #[test]
    fn after_a_single_dot_nothing_is_offered_but_after_two_dots_it_is() {
        assert_eq!(blocked("p.|"), Some(Blocked::AfterDot));
        assert_eq!(blocked("p.fo|"), Some(Blocked::AfterDot));
        assert_eq!(blocked("p.x.|"), Some(Blocked::AfterDot));
        assert_eq!(blocked("0.|"), Some(Blocked::AfterDot));
        assert_eq!(blocked("0..|"), None);
        assert_eq!(blocked("<<FOR>> i <<IN>> 0..n|"), None);
        assert_eq!(blocked("<<FOR>> i <<IN>> 0..|"), None);
        assert_eq!(
            blocked("p.m(|"),
            None,
            "внутри скобок вызова метода подсказки есть"
        );
    }

    #[test]
    fn declaration_names_are_not_completed() {
        for src in [
            "<<LET>> |",
            "<<LET>> na|",
            "<<VAR>> |",
            "<<CONST>> |",
            "<<STRUCT>> |",
            "<<STRUCT>> Po|",
            "<<FOR>> |",
            "<<FOR>> i|",
            "<<FN>> |",
            "<<FN>> fo|",
            "<<MUT>> |",
        ] {
            assert!(
                matches!(blocked(src), Some(Blocked::DeclName | Blocked::FnHeader)),
                "{src}: {:?}",
                blocked(src)
            );
        }
        // а после имени уже можно
        assert_eq!(blocked("<<LET>> x |"), None);
        assert_eq!(blocked("<<FOR>> i |"), None);
    }

    #[test]
    fn a_function_header_is_not_completed() {
        for src in [
            "<<FN>> f(|",
            "<<FN>> f(a, |",
            "<<FN>> f(a: |",
            "<<FN>> f(<<MUT>> |",
            "<<FN>> f(a) |",
            "<<FN>> f(a) -> |",
            "<<FN>> f(a) -> in|",
            "<<FN>> (|",
            "<<LET>> g = <<FN>>(a, |",
        ] {
            assert_eq!(blocked(src), Some(Blocked::FnHeader), "{src}");
        }
        assert_eq!(blocked("<<FN>> f(a) { | }"), None);
        assert_eq!(blocked("<<FN>> f(a) -> int { | }"), None);
    }

    #[test]
    fn the_body_of_a_struct_declaration_is_not_completed() {
        assert_eq!(blocked("<<STRUCT>> P { | }"), Some(Blocked::StructBody));
        assert_eq!(blocked("<<STRUCT>> P { x, | }"), Some(Blocked::StructBody));
        assert_eq!(blocked("<<STRUCT>> P { x, y| }"), Some(Blocked::StructBody));
        assert_eq!(blocked("<<STRUCT>> P { x } |"), None);
        assert_eq!(blocked("<<STRUCT>> P |"), None, "до `{{` — ещё не тело");
    }

    #[test]
    fn struct_literal_keys_are_not_completed_but_values_are() {
        assert_eq!(
            blocked("<<LET>> p = P { x: 1, |}"),
            Some(Blocked::StructKey)
        );
        assert_eq!(blocked("<<LET>> p = P { |}"), Some(Blocked::StructKey));
        assert_eq!(
            blocked("<<LET>> p = P { x: 1, y|: 2 }"),
            Some(Blocked::StructKey)
        );
        assert_eq!(
            blocked("<<LET>> p = P { x: |}"),
            None,
            "значение — выражение"
        );
        assert_eq!(blocked("<<LET>> p = P { x: 1 + |}"), None);
        assert_eq!(
            blocked("<<LET>> p = P { x: foo(1, |) }"),
            None,
            "запятая в скобках вызова"
        );
        assert_eq!(blocked("<<LET>> p = P { x: [1, |] }"), None);
    }

    #[test]
    fn type_annotations_are_not_completed() {
        assert_eq!(blocked("<<LET>> x: |"), Some(Blocked::TypePosition));
        assert_eq!(blocked("<<LET>> x: in|"), Some(Blocked::TypePosition));
        assert_eq!(blocked("<<VAR>> x: |"), Some(Blocked::TypePosition));
        assert_eq!(
            blocked("<<FN>> f() { <<LET>> x: | }"),
            Some(Blocked::TypePosition)
        );
        assert_eq!(blocked("<<LET>> x: int = |"), None);
        // `->` вне пункта `MATCH` — тоже аннотация (заголовок функции разобран выше)
        assert_eq!(blocked("a -> |"), Some(Blocked::TypePosition));
        assert_eq!(
            blocked("<<MATCH>> 1 { 1 -> | }"),
            None,
            "в пункте `MATCH` после `->` — выражение"
        );
        assert_eq!(blocked("<<MATCH>> 1 { n -> | }"), None);
    }

    #[test]
    fn ordinary_positions_are_not_blocked() {
        for src in [
            "|",
            "<<PRINT>>(|)",
            "x = |",
            "<<IF>> |",
            "<<RETURN>> |",
            "foo(a, |",
            "[1, |",
            "{ | }",
            "<<LET>> x = 1; |",
            "<<ELSE>> |",
            "<<WHILE>> |",
            "a + |",
        ] {
            assert_eq!(blocked(src), None, "{src}");
        }
    }

    // ── устойчивость ─────────────────────────────────────────────

    #[test]
    fn text_mode_and_non_code_states_give_an_empty_context() {
        let text = "@ ".repeat(MAX_RELEX + 1) + "x";
        let doc = Document::new(&text);
        assert!(doc.text_mode);
        let info = doc.cursor_info(&text, (1, 3));
        assert!(info.context.variables.is_empty() && info.context.blocked.is_none());
        let (text, pos) = parse("<<LET>> a = 1; \"str|");
        let info = Document::new(&text).cursor_info(&text, pos);
        assert_eq!(info.lex, Lex::Str);
        assert!(info.context.variables.is_empty());
    }

    #[test]
    fn word_at_prefers_the_token_containing_the_position() {
        let doc = Document::new("foo(bar)");
        // позиция внутри слова, на его начале и сразу за концом
        assert_eq!(doc.word_at((1, 1)), Some(0));
        assert_eq!(doc.word_at((1, 2)), Some(0));
        assert_eq!(
            doc.word_at((1, 4)),
            Some(0),
            "сразу за словом `foo` стоит `(`"
        );
        assert_eq!(doc.word_at((1, 5)), Some(2), "начало `bar`");
        assert_eq!(doc.word_at((1, 8)), Some(2), "сразу за `bar`");
        assert_eq!(doc.word_at((1, 9)), None, "после `)`");
        // слово, начинающееся там же, где кончается другое, побеждает
        let text = format!("ab{}", crate::sga_alphabet::encode_word("LET"));
        let doc = Document::new(&text);
        assert_eq!(doc.word_at((1, 3)), Some(1));
    }

    #[test]
    fn word_at_knows_keywords_and_ignores_literals_and_operators() {
        let doc = Document::new(&enc("<<LET>> x = 12 + \"s\""));
        assert_eq!(doc.word_at((1, 1)), Some(0), "ключевое слово");
        assert_eq!(doc.word_at((1, 5)), Some(1), "идентификатор");
        assert_eq!(doc.word_at((1, 9)), None, "число");
        assert_eq!(doc.word_at((1, 12)), None, "плюс");
        assert_eq!(doc.word_at((1, 15)), None, "строка");
    }

    #[test]
    fn keyword_mnemonics_are_recovered_from_token_kinds() {
        for m in crate::lexer::token::KEYWORD_MNEMONICS {
            let kind = crate::lexer::token::keyword_from_mnemonic(m).expect("слово");
            assert_eq!(keyword_mnemonic(&kind), Some(*m));
        }
        assert_eq!(keyword_mnemonic(&TokenKind::Ident("x".into())), None);
        assert_eq!(keyword_mnemonic(&TokenKind::Plus), None);
    }

    #[test]
    fn keyword_lookup_by_prefix_ignores_case_and_decodes_glyphs() {
        let names = |v: Vec<&vocabulary::KeywordDoc>| -> Vec<&'static str> {
            v.into_iter().map(|k| k.mnemonic).collect()
        };
        assert_eq!(names(keywords_with_ascii_prefix("le")), ["LET"]);
        assert_eq!(names(keywords_with_ascii_prefix("LE")), ["LET"]);
        assert_eq!(names(keywords_with_ascii_prefix("Le")), ["LET"]);
        assert_eq!(
            names(keywords_with_ascii_prefix("c")),
            ["CONST", "CONTINUE"]
        );
        assert_eq!(keywords_with_ascii_prefix("").len(), 23);
        assert!(keywords_with_ascii_prefix("zz").is_empty());
        let glyphs = crate::sga_alphabet::encode_word("CON");
        assert_eq!(
            names(keywords_with_glyph_prefix(&glyphs)),
            ["CONST", "CONTINUE"]
        );
    }

    #[test]
    fn the_scanner_never_panics_on_arbitrary_token_soup() {
        // перебор коротких последовательностей из «опасных» токенов, курсор
        // в каждой позиции
        let pieces = [
            "<<LET>>",
            "<<FN>>",
            "<<FOR>>",
            "<<MATCH>>",
            "<<STRUCT>>",
            "<<IF>>",
            "<<MUT>>",
            "x",
            "(",
            ")",
            "[",
            "]",
            "{",
            "}",
            ",",
            ":",
            "->",
            "=",
            ";",
            ".",
            "..",
            "1",
            "\"s\"",
            "<<IN>>",
        ];
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..400 {
            let n = (next() % 14 + 1) as usize;
            let mut text = String::new();
            for _ in 0..n {
                text.push_str(&enc(pieces[(next() as usize) % pieces.len()]));
                text.push(' ');
            }
            let doc = Document::new(&text);
            let lines = text.matches('\n').count() + 1;
            for line in 1..=lines {
                for col in 1..=text.chars().count() + 2 {
                    let _ = doc.cursor_info(&text, (line, col));
                    let _ = doc.word_at((line, col));
                }
            }
        }
    }
}
