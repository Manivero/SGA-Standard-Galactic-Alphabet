//! Языковой сервер SGA (LSP): JSON-RPC по stdio, жизненный цикл, хранилище
//! документов, публикация диагностик, `hover`, автодополнение, структура
//! файла (`documentSymbol`) и переход к определению (`definition`)
//! (T016–T019, M007). Подробности и список неподдерживаемого — в
//! `docs/LSP.md`.
//!
//! Сервер ТОЛЬКО АНАЛИЗИРУЕТ текст: программы не исполняются, файлы не
//! читаются и не пишутся, стандартный ввод программы не используется
//! (`PRINT`, `write_file`, `read_stdin()` и бесконечные циклы в документе
//! безвредны — их никто не запускает). Диагностики даёт `diagnostics`.
//!
//! Устройство: `Server::handle_message` — чистая функция «тело сообщения ->
//! исходящие сообщения» без ввода-вывода (проверяется в памяти);
//! `serve_with` — цикл чтения фреймов (`transport`) вокруг неё;
//! `run_stdio` — то же на stdin/stdout процесса в потоке с большим стеком.
//! В stdout попадают ТОЛЬКО фреймы протокола, всё остальное — в stderr с
//! префиксом `sga-lsp:`.

pub mod assist;
pub mod json;
pub mod nav;
pub mod position;
pub mod scan;
pub mod transport;
pub mod vocabulary;

use crate::diagnostics::{self, Code, Diagnostic, Severity};
use json::Json;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::panic::{self, AssertUnwindSafe};

/// Документы больше этого размера (в байтах UTF-8) не анализируются: вместо
/// диагностик сервер публикует одно предупреждение. Предел связан со стеком
/// анализа: AST из длинной цепочки операторов (`1+1+...`) освобождается
/// рекурсивно, и на каждые два байта такой цепочки уходит порядка 110 байт
/// стека в отладочной сборке (≈ 64 в release; TD-018); 1 МиБ при стеке
/// `STACK_SIZES[0]` оставляет запас ≈ 4,6× (на запасном `STACK_SIZES[1]` —
/// ≈ 1,14× в отладочной сборке). Реальные программы SGA — килобайты.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// Размеры стека потока анализа, которые пробуем запросить по порядку.
/// Память резервируется виртуально и фактически занимается лишь по мере
/// использования; если система отказала, берётся следующий размер, а затем
/// — текущий поток (с записью в stderr).
const STACK_SIZES: [usize; 2] = [256 * 1024 * 1024, 64 * 1024 * 1024];

// Коды ошибок JSON-RPC / LSP.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const SERVER_NOT_INITIALIZED: i64 = -32002;

/// Анализатор текста документа. В сервере по умолчанию —
/// `diagnostics::analyze_source`; подменяется в тестах (проверка изоляции
/// паник и пределов).
pub type Analyzer = Box<dyn Fn(&str) -> Vec<Diagnostic>>;

static NULL_JSON: Json = Json::Null;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// До запроса `initialize`.
    Uninitialized,
    /// После `initialize`, до `shutdown`.
    Running,
    /// После `shutdown`: допустим только `exit`.
    ShuttingDown,
}

struct RpcError {
    code: i64,
    message: String,
}

fn rpc_error(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

/// Состояние сервера: жизненный цикл и открытые документы.
pub struct Server {
    analyzer: Analyzer,
    state: State,
    /// Открытые документы: uri -> текст.
    documents: HashMap<String, String>,
    /// Код возврата процесса после получения `exit`.
    exit_code: Option<i32>,
    /// Записи для stderr, накопленные при обработке (забирает `take_log`).
    log: Vec<String>,
    /// Клиент в `initialize` объявил `hierarchicalDocumentSymbolSupport`:
    /// `documentSymbol` отвечает деревом `DocumentSymbol[]`, иначе плоским
    /// `SymbolInformation[]`.
    hierarchical_symbols: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// Сервер с настоящим анализатором (`diagnostics::analyze_source`).
    pub fn new() -> Self {
        Self::with_analyzer(Box::new(diagnostics::analyze_source))
    }

    /// Сервер с подставленным анализатором.
    pub fn with_analyzer(analyzer: Analyzer) -> Self {
        Server {
            analyzer,
            state: State::Uninitialized,
            documents: HashMap::new(),
            exit_code: None,
            log: Vec::new(),
            hierarchical_symbols: false,
        }
    }

    /// `Some(код)` после уведомления `exit`: 0, если до него был `shutdown`,
    /// иначе 1. Цикл чтения на этом останавливается.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Код возврата, если поток ввода кончился без `exit`: 0 после
    /// `shutdown`, иначе 1.
    pub fn eof_code(&self) -> i32 {
        if self.state == State::ShuttingDown {
            0
        } else {
            1
        }
    }

    /// Забирает накопленные записи для stderr.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// Текст открытого документа (для тестов и отладки).
    pub fn document_text(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(String::as_str)
    }

    /// Обрабатывает одно сообщение (тело фрейма) и возвращает сообщения,
    /// которые нужно отправить клиенту, по порядку.
    pub fn handle_message(&mut self, body: &[u8]) -> Vec<Json> {
        let text = match std::str::from_utf8(body) {
            Ok(text) => text,
            Err(e) => {
                return vec![error_response(
                    Json::Null,
                    PARSE_ERROR,
                    &format!("тело сообщения не является валидным UTF-8: {e}"),
                )]
            }
        };
        match json::parse(text) {
            Ok(value) => self.handle_value(&value),
            Err(e) => vec![error_response(
                Json::Null,
                PARSE_ERROR,
                &format!("тело сообщения не является корректным JSON: {e}"),
            )],
        }
    }

    /// Классификация по JSON-RPC 2.0: запрос (`method` + `id`), уведомление
    /// (`method` без `id`), ответ клиента (`result`/`error`, игнорируется).
    fn handle_value(&mut self, msg: &Json) -> Vec<Json> {
        if !matches!(msg, Json::Object(_)) {
            return vec![error_response(
                Json::Null,
                INVALID_REQUEST,
                "сообщение JSON-RPC должно быть объектом",
            )];
        }
        let id = msg.get("id");
        match msg.get("method") {
            Some(Json::Str(method)) => match id {
                None => self.handle_notification(method, msg.get("params")),
                Some(id) if is_valid_id(id) => {
                    self.handle_request(id.clone(), method, msg.get("params"))
                }
                Some(_) => vec![error_response(
                    Json::Null,
                    INVALID_REQUEST,
                    "поле id должно быть целым числом или строкой",
                )],
            },
            Some(_) => vec![error_response(
                echo_id(id),
                INVALID_REQUEST,
                "поле method должно быть строкой",
            )],
            None if msg.get("result").is_some() || msg.get("error").is_some() => Vec::new(),
            None => vec![error_response(
                echo_id(id),
                INVALID_REQUEST,
                "в сообщении нет поля method",
            )],
        }
    }

    fn handle_request(&mut self, id: Json, method: &str, params: Option<&Json>) -> Vec<Json> {
        match guarded(|| self.route_request(method, params)) {
            Ok(Ok(result)) => vec![ok_response(id, result)],
            Ok(Err(e)) => vec![error_response(id, e.code, &e.message)],
            Err(panic_message) => {
                self.log.push(format!(
                    "паника при обработке запроса '{method}': {panic_message}"
                ));
                vec![error_response(
                    id,
                    INTERNAL_ERROR,
                    "внутренняя ошибка сервера",
                )]
            }
        }
    }

    /// Таблица запросов с учётом состояния жизненного цикла.
    fn route_request(&mut self, method: &str, params: Option<&Json>) -> Result<Json, RpcError> {
        match (self.state, method) {
            (State::Uninitialized, "initialize") => {
                self.state = State::Running;
                self.hierarchical_symbols = client_wants_hierarchical_symbols(params);
                Ok(initialize_result())
            }
            (State::Uninitialized, _) => Err(rpc_error(
                SERVER_NOT_INITIALIZED,
                "сервер не инициализирован: первым должен идти запрос initialize",
            )),
            (State::Running, "initialize") => {
                Err(rpc_error(INVALID_REQUEST, "сервер уже инициализирован"))
            }
            (State::Running, "shutdown") => {
                self.state = State::ShuttingDown;
                Ok(Json::Null)
            }
            (State::Running, "textDocument/hover") => self.hover_request(params),
            (State::Running, "textDocument/completion") => self.completion_request(params),
            (State::Running, "textDocument/documentSymbol") => self.document_symbol_request(params),
            (State::Running, "textDocument/definition") => self.definition_request(params),
            // Только для тестов: вызывает панику внутри обработки запроса,
            // чтобы проверить, что она превращается в ответ -32603.
            #[cfg(test)]
            (State::Running, "$/sga/testPanic") => panic!("тестовая паника запроса"),
            (State::Running, other) => Err(rpc_error(
                METHOD_NOT_FOUND,
                format!("метод '{other}' не поддерживается"),
            )),
            (State::ShuttingDown, _) => Err(rpc_error(
                INVALID_REQUEST,
                "сервер завершает работу: допустимо только уведомление exit",
            )),
        }
    }

    /// Текст открытого документа для запроса подсказок; `None` — документ не
    /// открыт или больше `MAX_DOCUMENT_BYTES` (как и диагностики, подсказки на
    /// таком тексте не строятся).
    fn text_for_query(&self, uri: &str) -> Option<&str> {
        self.documents
            .get(uri)
            .map(String::as_str)
            .filter(|text| text.len() <= MAX_DOCUMENT_BYTES)
    }

    /// `textDocument/hover`: `Hover` или `null`.
    fn hover_request(&self, params: Option<&Json>) -> Result<Json, RpcError> {
        let query = position_params(params)?;
        let Some(text) = self.text_for_query(query.uri) else {
            return Ok(Json::Null);
        };
        let pos = position::from_lsp(text, query.line, query.character);
        let doc = scan::Document::new(text);
        Ok(match assist::hover(&doc, pos) {
            Some(hover) => hover_json(text, &hover),
            None => Json::Null,
        })
    }

    /// `textDocument/completion`: массив `CompletionItem` (пустой — подсказок
    /// нет) или `null`.
    fn completion_request(&self, params: Option<&Json>) -> Result<Json, RpcError> {
        let query = position_params(params)?;
        let Some(text) = self.text_for_query(query.uri) else {
            return Ok(Json::Null);
        };
        let pos = position::from_lsp(text, query.line, query.character);
        let doc = scan::Document::new(text);
        Ok(completion_json(text, &assist::complete(&doc, text, pos)))
    }

    /// `textDocument/documentSymbol`: `DocumentSymbol[]` (клиент объявил
    /// иерархию) или `SymbolInformation[]`; `null` — документ не открыт или
    /// слишком велик; пустой массив — объявлений нет.
    fn document_symbol_request(&self, params: Option<&Json>) -> Result<Json, RpcError> {
        let uri = document_uri(params)?;
        let Some(text) = self.text_for_query(uri) else {
            return Ok(Json::Null);
        };
        let doc = scan::Document::new(text);
        let symbols = nav::document_symbols(&doc);
        // тысячи символов — тысячи позиций: таблица строк вместо просмотра
        // текста с начала на каждую (`position::LineTable`)
        let lines = position::LineTable::new(text);
        Ok(if self.hierarchical_symbols {
            Json::Array(
                symbols
                    .iter()
                    .map(|symbol| document_symbol_json(&lines, symbol))
                    .collect(),
            )
        } else {
            let mut flat = Vec::new();
            flat_symbols_json(&lines, uri, &symbols, None, &mut flat);
            Json::Array(flat)
        })
    }

    /// `textDocument/definition`: один `Location` или `null`.
    fn definition_request(&self, params: Option<&Json>) -> Result<Json, RpcError> {
        let query = position_params(params)?;
        let Some(text) = self.text_for_query(query.uri) else {
            return Ok(Json::Null);
        };
        let pos = position::from_lsp(text, query.line, query.character);
        let doc = scan::Document::new(text);
        Ok(match nav::definition(&doc, pos) {
            Some(location) => Json::object([
                ("uri", Json::string(query.uri)),
                ("range", range_json(text, location.start, location.end)),
            ]),
            None => Json::Null,
        })
    }

    fn handle_notification(&mut self, method: &str, params: Option<&Json>) -> Vec<Json> {
        if method == "exit" {
            self.exit_code = Some(if self.state == State::ShuttingDown {
                0
            } else {
                1
            });
            return Vec::new();
        }
        // До initialize и после shutdown уведомления отбрасываются.
        if self.state != State::Running {
            return Vec::new();
        }
        let params = params.unwrap_or(&NULL_JSON);
        match guarded(|| self.dispatch_notification(method, params)) {
            Ok(out) => out,
            Err(panic_message) => {
                self.log.push(format!(
                    "паника при обработке уведомления '{method}': {panic_message}"
                ));
                Vec::new()
            }
        }
    }

    fn dispatch_notification(&mut self, method: &str, params: &Json) -> Vec<Json> {
        match method {
            // Только для тестов: паника внутри обработки уведомления (вне
            // анализатора) не должна завершать сервер.
            #[cfg(test)]
            "$/sga/testPanic" => panic!("тестовая паника уведомления"),
            "textDocument/didOpen" => self.did_open(params),
            "textDocument/didChange" => self.did_change(params),
            "textDocument/didClose" => self.did_close(params),
            // initialized, didSave, workspace/*, $/cancelRequest, $/setTrace и
            // всё неизвестное — молча игнорируется (по спецификации
            // неизвестные уведомления ответа не требуют).
            _ => Vec::new(),
        }
    }

    fn bad_params(&mut self, method: &str, why: &str) -> Vec<Json> {
        self.log.push(format!(
            "{method}: некорректные параметры ({why}), уведомление проигнорировано"
        ));
        Vec::new()
    }

    fn did_open(&mut self, params: &Json) -> Vec<Json> {
        let Some(doc) = params.get("textDocument") else {
            return self.bad_params("didOpen", "нет textDocument");
        };
        let (Some(uri), Some(text)) = (
            doc.get("uri").and_then(Json::as_str),
            doc.get("text").and_then(Json::as_str),
        ) else {
            return self.bad_params("didOpen", "нет textDocument.uri или textDocument.text");
        };
        let version = doc.get("version").and_then(Json::as_i64);
        let diagnostics = self.analyze(text);
        let message = publish_diagnostics(uri, version, text, &diagnostics);
        self.documents.insert(uri.to_string(), text.to_string());
        vec![message]
    }

    fn did_change(&mut self, params: &Json) -> Vec<Json> {
        let Some(doc) = params.get("textDocument") else {
            return self.bad_params("didChange", "нет textDocument");
        };
        let Some(uri) = doc.get("uri").and_then(Json::as_str) else {
            return self.bad_params("didChange", "нет textDocument.uri");
        };
        if !self.documents.contains_key(uri) {
            self.log.push(format!(
                "didChange для неоткрытого документа '{uri}' проигнорирован"
            ));
            return Vec::new();
        }
        let Some(changes) = params.get("contentChanges").and_then(Json::as_array) else {
            return self.bad_params("didChange", "contentChanges не массив");
        };
        let Some(last) = changes.last() else {
            return self.bad_params("didChange", "contentChanges пуст");
        };
        if last.get("range").is_some_and(|range| !range.is_null()) {
            self.log.push(format!(
                "didChange для '{uri}': инкрементальное изменение (range) не поддерживается — сервер объявил полную синхронизацию; изменение проигнорировано, документ не изменён"
            ));
            return Vec::new();
        }
        let Some(text) = last.get("text").and_then(Json::as_str) else {
            return self.bad_params("didChange", "в последнем изменении нет text");
        };
        let version = doc.get("version").and_then(Json::as_i64);
        let diagnostics = self.analyze(text);
        let message = publish_diagnostics(uri, version, text, &diagnostics);
        self.documents.insert(uri.to_string(), text.to_string());
        vec![message]
    }

    fn did_close(&mut self, params: &Json) -> Vec<Json> {
        let Some(uri) = params
            .get("textDocument")
            .and_then(|doc| doc.get("uri"))
            .and_then(Json::as_str)
        else {
            return self.bad_params("didClose", "нет textDocument.uri");
        };
        if self.documents.remove(uri).is_none() {
            self.log.push(format!(
                "didClose для неоткрытого документа '{uri}' проигнорирован"
            ));
            return Vec::new();
        }
        vec![publish_diagnostics(uri, None, "", &[])]
    }

    /// Диагностики документа. Слишком большой документ не анализируется
    /// (одно предупреждение); паника анализатора не завершает сервер —
    /// превращается в одну диагностику `internal` и запись в stderr.
    fn analyze(&mut self, text: &str) -> Vec<Diagnostic> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return vec![whole_document_diagnostic(
                Severity::Warning,
                Code::Limit,
                format!(
                    "анализ пропущен: документ ({} байт) больше предела {} байт",
                    text.len(),
                    MAX_DOCUMENT_BYTES
                ),
            )];
        }
        match guarded(|| (self.analyzer)(text)) {
            Ok(diagnostics) => diagnostics,
            Err(panic_message) => {
                self.log
                    .push(format!("паника анализатора: {panic_message}"));
                vec![whole_document_diagnostic(
                    Severity::Error,
                    Code::Internal,
                    "внутренняя ошибка анализатора (паника); подробности — в stderr сервера"
                        .to_string(),
                )]
            }
        }
    }
}

/// Диагностика без привязки к месту: пустой диапазон в начале документа.
fn whole_document_diagnostic(severity: Severity, code: Code, message: String) -> Diagnostic {
    Diagnostic {
        severity,
        code,
        message,
        start_line: 1,
        start_col: 1,
        end_line: 1,
        end_col: 1,
    }
}

/// Выполняет `f`, превращая панику в `Err(сообщение)`.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "паника без сообщения".to_string()
        }
    })
}

fn is_valid_id(id: &Json) -> bool {
    matches!(id, Json::Int(_) | Json::Str(_))
}

/// `id` для ответа об ошибке: из сообщения, если это целое/строка, иначе null.
fn echo_id(id: Option<&Json>) -> Json {
    match id {
        Some(id) if is_valid_id(id) => id.clone(),
        _ => Json::Null,
    }
}

fn ok_response(id: Json, result: Json) -> Json {
    Json::object([
        ("jsonrpc", Json::string("2.0")),
        ("id", id),
        ("result", result),
    ])
}

fn error_response(id: Json, code: i64, message: &str) -> Json {
    Json::object([
        ("jsonrpc", Json::string("2.0")),
        ("id", id),
        (
            "error",
            Json::object([
                ("code", Json::Int(code)),
                ("message", Json::string(message)),
            ]),
        ),
    ])
}

fn notification(method: &str, params: Json) -> Json {
    Json::object([
        ("jsonrpc", Json::string("2.0")),
        ("method", Json::string(method)),
        ("params", params),
    ])
}

/// Объявил ли клиент в `initialize`, что понимает иерархические символы
/// документа (`textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`).
/// Параметры без этого поля, не объект или отсутствующие — «не объявил».
fn client_wants_hierarchical_symbols(params: Option<&Json>) -> bool {
    matches!(
        params
            .and_then(|p| p.get("capabilities"))
            .and_then(|c| c.get("textDocument"))
            .and_then(|t| t.get("documentSymbol"))
            .and_then(|d| d.get("hierarchicalDocumentSymbolSupport")),
        Some(Json::Bool(true))
    )
}

/// Результат `initialize`: объявляются ТОЛЬКО реализованные возможности —
/// синхронизация документов (открытие/закрытие, полный текст при изменении),
/// `hover`, автодополнение (без `resolve`: описания приходят сразу), переход
/// к определению и структура документа.
fn initialize_result() -> Json {
    Json::object([
        (
            "capabilities",
            Json::object([
                (
                    "textDocumentSync",
                    Json::object([("openClose", Json::Bool(true)), ("change", Json::Int(1))]),
                ),
                ("hoverProvider", Json::Bool(true)),
                (
                    "completionProvider",
                    Json::object([("resolveProvider", Json::Bool(false))]),
                ),
                ("definitionProvider", Json::Bool(true)),
                ("documentSymbolProvider", Json::Bool(true)),
            ]),
        ),
        (
            "serverInfo",
            Json::object([
                ("name", Json::string("sga-lsp")),
                ("version", Json::string(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
    ])
}

fn position_json(position: position::Position) -> Json {
    Json::object([
        ("line", Json::Int(i64::from(position.line))),
        ("character", Json::Int(i64::from(position.character))),
    ])
}

/// Диагностика в формате LSP. Позиции переводятся из координат лексера
/// (символы) в позиции LSP (единицы UTF-16) относительно текста документа.
fn diagnostic_json(text: &str, d: &Diagnostic) -> Json {
    let start = position::to_lsp(text, d.start_line, d.start_col);
    let end = position::to_lsp(text, d.end_line, d.end_col);
    Json::object([
        (
            "range",
            Json::object([("start", position_json(start)), ("end", position_json(end))]),
        ),
        (
            "severity",
            Json::Int(match d.severity {
                Severity::Error => 1,
                Severity::Warning => 2,
                Severity::Information => 3,
            }),
        ),
        ("code", Json::string(d.code.as_str())),
        ("source", Json::string("sga")),
        ("message", Json::string(d.message.as_str())),
    ])
}

/// Параметры запроса «по позиции»: документ и позиция курсора в терминах
/// LSP (строка с нуля, `character` с нуля в единицах UTF-16).
struct PositionQuery<'a> {
    uri: &'a str,
    line: u32,
    character: u32,
}

fn invalid_params(message: impl Into<String>) -> RpcError {
    rpc_error(INVALID_PARAMS, message)
}

/// Разбирает `{ textDocument: { uri } }`; остальное — `-32602`.
fn document_uri(params: Option<&Json>) -> Result<&str, RpcError> {
    params
        .filter(|p| matches!(p, Json::Object(_)))
        .ok_or_else(|| invalid_params("params должен быть объектом"))?
        .get("textDocument")
        .and_then(|doc| doc.get("uri"))
        .and_then(Json::as_str)
        .ok_or_else(|| invalid_params("нет textDocument.uri (строка)"))
}

/// Разбирает `{ textDocument: { uri }, position: { line, character } }`.
/// Координаты — целые числа >= 0 (слишком большие прижимаются к `u32::MAX`
/// и дальше — к концу строки/текста); остальное — `-32602`.
fn position_params(params: Option<&Json>) -> Result<PositionQuery<'_>, RpcError> {
    let uri = document_uri(params)?;
    let position = params
        .and_then(|p| p.get("position"))
        .filter(|p| matches!(p, Json::Object(_)))
        .ok_or_else(|| invalid_params("нет position (объект)"))?;
    let coordinate = |name: &str| match position.get(name) {
        Some(Json::Int(n)) if *n >= 0 => Ok(u32::try_from(*n).unwrap_or(u32::MAX)),
        _ => Err(invalid_params(format!(
            "position.{name} должно быть целым числом >= 0"
        ))),
    };
    Ok(PositionQuery {
        uri,
        line: coordinate("line")?,
        character: coordinate("character")?,
    })
}

fn range_json(text: &str, start: scan::Pos, end: scan::Pos) -> Json {
    Json::object([
        (
            "start",
            position_json(position::to_lsp(text, start.0, start.1)),
        ),
        ("end", position_json(position::to_lsp(text, end.0, end.1))),
    ])
}

/// То же по таблице строк — для ответов с множеством позиций.
fn table_range_json(lines: &position::LineTable, start: scan::Pos, end: scan::Pos) -> Json {
    Json::object([
        ("start", position_json(lines.to_lsp(start.0, start.1))),
        ("end", position_json(lines.to_lsp(end.0, end.1))),
    ])
}

fn markdown_json(value: &str) -> Json {
    Json::object([
        ("kind", Json::string("markdown")),
        ("value", Json::string(value)),
    ])
}

fn hover_json(text: &str, hover: &assist::Hover) -> Json {
    Json::object([
        ("contents", markdown_json(&hover.markdown)),
        ("range", range_json(text, hover.start, hover.end)),
    ])
}

/// Число `SymbolKind` протокола.
fn symbol_kind(kind: nav::SymbolKind) -> i64 {
    match kind {
        nav::SymbolKind::Function => 12,
        nav::SymbolKind::Variable => 13,
        nav::SymbolKind::Constant => 14,
        nav::SymbolKind::Field => 8,
        nav::SymbolKind::Struct => 23,
    }
}

/// `DocumentSymbol` с детьми (глубина ограничена `nav::MAX_SYMBOL_DEPTH`, так
/// что рекурсия здесь неглубока).
fn document_symbol_json(lines: &position::LineTable, symbol: &nav::Symbol) -> Json {
    let mut fields = vec![("name".to_string(), Json::string(symbol.name.as_str()))];
    if let Some(detail) = &symbol.detail {
        fields.push(("detail".to_string(), Json::string(detail.as_str())));
    }
    fields.push(("kind".to_string(), Json::Int(symbol_kind(symbol.kind))));
    fields.push((
        "range".to_string(),
        table_range_json(lines, symbol.start, symbol.end),
    ));
    fields.push((
        "selectionRange".to_string(),
        table_range_json(lines, symbol.name_start, symbol.name_end),
    ));
    if !symbol.children.is_empty() {
        fields.push((
            "children".to_string(),
            Json::Array(
                symbol
                    .children
                    .iter()
                    .map(|child| document_symbol_json(lines, child))
                    .collect(),
            ),
        ));
    }
    Json::Object(fields)
}

/// Дерево символов как плоский `SymbolInformation[]` в порядке обхода в
/// глубину; у вложенных — `containerName`.
fn flat_symbols_json(
    lines: &position::LineTable,
    uri: &str,
    symbols: &[nav::Symbol],
    container: Option<&str>,
    out: &mut Vec<Json>,
) {
    for symbol in symbols {
        let mut fields = vec![
            ("name".to_string(), Json::string(symbol.name.as_str())),
            ("kind".to_string(), Json::Int(symbol_kind(symbol.kind))),
            (
                "location".to_string(),
                Json::object([
                    ("uri", Json::string(uri)),
                    ("range", table_range_json(lines, symbol.start, symbol.end)),
                ]),
            ),
        ];
        if let Some(container) = container {
            fields.push(("containerName".to_string(), Json::string(container)));
        }
        out.push(Json::Object(fields));
        flat_symbols_json(lines, uri, &symbol.children, Some(&symbol.name), out);
    }
}

/// Число `CompletionItemKind` протокола.
fn completion_kind(kind: assist::ItemKind) -> i64 {
    match kind {
        assist::ItemKind::Function => 3,
        assist::ItemKind::Variable => 6,
        assist::ItemKind::Keyword => 14,
        assist::ItemKind::Constant => 21,
        assist::ItemKind::Struct => 22,
    }
}

fn completion_json(text: &str, completion: &assist::Completion) -> Json {
    let range = range_json(text, completion.start, completion.end);
    Json::Array(
        completion
            .items
            .iter()
            .map(|item| {
                let mut fields = vec![
                    ("label".to_string(), Json::string(item.label.as_str())),
                    ("kind".to_string(), Json::Int(completion_kind(item.kind))),
                ];
                if let Some(detail) = &item.detail {
                    fields.push(("detail".to_string(), Json::string(detail.as_str())));
                }
                if let Some(documentation) = &item.documentation {
                    fields.push(("documentation".to_string(), markdown_json(documentation)));
                }
                fields.push((
                    "sortText".to_string(),
                    Json::string(item.sort_text.as_str()),
                ));
                fields.push((
                    "filterText".to_string(),
                    Json::string(item.filter_text.as_str()),
                ));
                fields.push((
                    "textEdit".to_string(),
                    Json::object([
                        ("range", range.clone()),
                        ("newText", Json::string(item.new_text.as_str())),
                    ]),
                ));
                Json::Object(fields)
            })
            .collect(),
    )
}

fn publish_diagnostics(
    uri: &str,
    version: Option<i64>,
    text: &str,
    diagnostics: &[Diagnostic],
) -> Json {
    let mut params = vec![("uri".to_string(), Json::string(uri))];
    if let Some(version) = version {
        params.push(("version".to_string(), Json::Int(version)));
    }
    params.push((
        "diagnostics".to_string(),
        Json::Array(
            diagnostics
                .iter()
                .map(|d| diagnostic_json(text, d))
                .collect(),
        ),
    ));
    notification("textDocument/publishDiagnostics", Json::Object(params))
}

/// Запись в stderr с префиксом `sga-lsp:`. Ошибки записи игнорируются: сервер
/// не должен падать из-за закрытого stderr (в отличие от `eprintln!`).
pub fn stderr_log(line: &str) {
    let _ = writeln!(std::io::stderr(), "sga-lsp: {line}");
}

/// Цикл сервера: читает фреймы из `input`, обрабатывает, пишет ответы в
/// `output`; записи для stderr отдаёт в `log`. Возвращает код возврата
/// процесса: 0 — штатно (`exit` после `shutdown`, либо конец ввода после
/// `shutdown`); 1 — `exit` без `shutdown`, конец ввода без `shutdown`, ошибка
/// фрейминга (поток рассинхронизирован, продолжать нельзя) или ошибка записи
/// ответа клиенту.
pub fn serve_with<R: BufRead, W: Write>(
    server: &mut Server,
    input: &mut R,
    output: &mut W,
    log: &mut dyn FnMut(&str),
) -> i32 {
    loop {
        let body = match transport::read_message(input) {
            Ok(Some(body)) => body,
            Ok(None) => return server.eof_code(),
            Err(e) => {
                log(&format!("ошибка фрейминга: {e}"));
                return 1;
            }
        };
        let replies = server.handle_message(&body);
        for line in server.take_log() {
            log(&line);
        }
        for reply in replies {
            if let Err(e) = transport::write_message(output, &reply.to_string()) {
                log(&format!("не удалось записать ответ клиенту: {e}"));
                return 1;
            }
        }
        if let Some(code) = server.exit_code() {
            return code;
        }
    }
}

/// `serve_with` с настоящим анализатором и записью в stderr.
pub fn serve<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> i32 {
    serve_with(&mut Server::new(), input, output, &mut stderr_log)
}

/// Выполняет `work` в потоке с большим стеком (`STACK_SIZES`), пробуя размеры
/// по порядку; если поток создать не удалось — в текущем потоке. Паника
/// потока -> код 1. Большой стек нужен анализу: предел глубины рекурсии
/// парсера рассчитан на 2 МиБ, а освобождение AST из очень длинной цепочки
/// операторов рекурсивно (TD-018); к тому же главный поток Windows имеет
/// стек в 1 МиБ.
pub fn run_with_large_stack(work: impl Fn() -> i32 + Sync) -> i32 {
    std::thread::scope(|scope| {
        for size in STACK_SIZES {
            let spawned = std::thread::Builder::new()
                .name("sga-lsp".to_string())
                .stack_size(size)
                .spawn_scoped(scope, &work);
            match spawned {
                Ok(handle) => {
                    return handle.join().unwrap_or_else(|_| {
                        stderr_log("рабочий поток аварийно завершился");
                        1
                    })
                }
                Err(e) => stderr_log(&format!(
                    "не удалось создать поток со стеком {} МиБ: {e}",
                    size / (1024 * 1024)
                )),
            }
        }
        stderr_log("работа продолжается в текущем потоке (стек меньше расчётного)");
        work()
    })
}

/// Сервер на stdin/stdout процесса (`sga lsp`); возвращает код возврата.
pub fn run_stdio() -> i32 {
    run_with_large_stack(|| {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut input = stdin.lock();
        let mut output = stdout.lock();
        serve(&mut input, &mut output)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sga_alphabet::encode_word;
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    // ── помощники ────────────────────────────────────────────────────

    fn parse_msg(msg: &str) -> Json {
        json::parse(msg).expect("тестовое сообщение должно быть JSON")
    }

    fn request(id: Json, method: &str, params: Json) -> String {
        Json::object([
            ("jsonrpc", Json::string("2.0")),
            ("id", id),
            ("method", Json::string(method)),
            ("params", params),
        ])
        .to_string()
    }

    fn notify(method: &str, params: Json) -> String {
        Json::object([
            ("jsonrpc", Json::string("2.0")),
            ("method", Json::string(method)),
            ("params", params),
        ])
        .to_string()
    }

    fn initialize_msg(id: i64) -> String {
        request(
            Json::Int(id),
            "initialize",
            Json::object([("processId", Json::Null)]),
        )
    }

    fn did_open(uri: &str, version: i64, text: &str) -> String {
        notify(
            "textDocument/didOpen",
            Json::object([(
                "textDocument",
                Json::object([
                    ("uri", Json::string(uri)),
                    ("languageId", Json::string("sga")),
                    ("version", Json::Int(version)),
                    ("text", Json::string(text)),
                ]),
            )]),
        )
    }

    fn did_change(uri: &str, version: i64, text: &str) -> String {
        notify(
            "textDocument/didChange",
            Json::object([
                (
                    "textDocument",
                    Json::object([("uri", Json::string(uri)), ("version", Json::Int(version))]),
                ),
                (
                    "contentChanges",
                    Json::Array(vec![Json::object([("text", Json::string(text))])]),
                ),
            ]),
        )
    }

    fn did_close(uri: &str) -> String {
        notify(
            "textDocument/didClose",
            Json::object([("textDocument", Json::object([("uri", Json::string(uri))]))]),
        )
    }

    fn send(server: &mut Server, msg: &str) -> Vec<Json> {
        server.handle_message(msg.as_bytes())
    }

    /// Сервер, прошедший initialize/initialized.
    fn started() -> Server {
        let mut server = Server::new();
        let out = send(&mut server, &initialize_msg(1));
        assert_eq!(out.len(), 1, "initialize должен давать один ответ");
        let out = send(&mut server, &notify("initialized", Json::object([])));
        assert!(out.is_empty(), "initialized не вызывает ответа: {out:?}");
        server
    }

    fn error_code(msg: &Json) -> i64 {
        msg.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Json::as_i64)
            .unwrap_or_else(|| panic!("в сообщении нет error.code: {msg}"))
    }

    /// Единственное исходящее сообщение: publishDiagnostics; возвращает params.
    fn only_publish(out: &[Json]) -> &Json {
        assert_eq!(out.len(), 1, "ожидалось одно сообщение: {out:?}");
        assert_eq!(
            out[0].get("method").and_then(Json::as_str),
            Some("textDocument/publishDiagnostics"),
            "{}",
            out[0]
        );
        assert_eq!(out[0].get("jsonrpc").and_then(Json::as_str), Some("2.0"));
        assert!(out[0].get("id").is_none(), "уведомление без id");
        out[0].get("params").expect("params")
    }

    fn diags_of(params: &Json) -> &[Json] {
        params
            .get("diagnostics")
            .and_then(Json::as_array)
            .expect("diagnostics")
    }

    fn lsp_range(d: &Json) -> ((i64, i64), (i64, i64)) {
        let pos = |name: &str| {
            let p = d.get("range").and_then(|r| r.get(name)).expect("range");
            (
                p.get("line").and_then(Json::as_i64).expect("line"),
                p.get("character")
                    .and_then(Json::as_i64)
                    .expect("character"),
            )
        };
        (pos("start"), pos("end"))
    }

    fn let_kw() -> String {
        encode_word("LET")
    }

    // ── жизненный цикл ───────────────────────────────────────────────

    #[test]
    fn initialize_answers_with_exactly_the_declared_capabilities() {
        let mut server = Server::new();
        let out = send(&mut server, &initialize_msg(7));
        assert_eq!(out.len(), 1);
        let reply = &out[0];
        assert_eq!(reply.get("jsonrpc").and_then(Json::as_str), Some("2.0"));
        assert_eq!(reply.get("id"), Some(&Json::Int(7)));
        assert!(reply.get("error").is_none());
        let result = reply.get("result").expect("result");
        assert_eq!(
            result.get("capabilities"),
            Some(&parse_msg(
                r#"{"textDocumentSync":{"openClose":true,"change":1},"hoverProvider":true,"completionProvider":{"resolveProvider":false},"definitionProvider":true,"documentSymbolProvider":true}"#
            )),
            "объявляется РОВНО синхронизация документов, hover, автодополнение, \
             переход к определению и символы документа"
        );
        let info = result.get("serverInfo").expect("serverInfo");
        assert_eq!(info.get("name").and_then(Json::as_str), Some("sga-lsp"));
        assert_eq!(
            info.get("version").and_then(Json::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn string_ids_are_echoed_verbatim() {
        let mut server = Server::new();
        let out = send(
            &mut server,
            &request(Json::string("abc-1"), "initialize", Json::Null),
        );
        assert_eq!(out[0].get("id"), Some(&Json::string("abc-1")));
    }

    #[test]
    fn shutdown_returns_null_and_exit_after_it_gives_zero() {
        let mut server = started();
        let out = send(&mut server, &request(Json::Int(2), "shutdown", Json::Null));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("id"), Some(&Json::Int(2)));
        assert_eq!(out[0].get("result"), Some(&Json::Null));
        assert!(out[0].get("error").is_none());
        assert_eq!(server.exit_code(), None, "до exit код не определён");
        assert!(send(&mut server, &notify("exit", Json::Null)).is_empty());
        assert_eq!(server.exit_code(), Some(0));
    }

    #[test]
    fn exit_without_shutdown_gives_one() {
        let mut server = started();
        send(&mut server, &notify("exit", Json::Null));
        assert_eq!(server.exit_code(), Some(1));

        // и до initialize тоже
        let mut fresh = Server::new();
        send(&mut fresh, &notify("exit", Json::Null));
        assert_eq!(fresh.exit_code(), Some(1));
    }

    #[test]
    fn end_of_input_code_depends_on_shutdown() {
        let mut server = started();
        assert_eq!(server.eof_code(), 1);
        send(&mut server, &request(Json::Int(2), "shutdown", Json::Null));
        assert_eq!(server.eof_code(), 0);
        assert_eq!(Server::new().eof_code(), 1);
    }

    #[test]
    fn requests_before_initialize_are_server_not_initialized() {
        let mut server = Server::new();
        for (id, method) in [
            (Json::Int(1), "textDocument/hover"),
            (Json::Int(2), "textDocument/completion"),
            (Json::Int(5), "textDocument/documentSymbol"),
            (Json::Int(6), "textDocument/definition"),
            (Json::string("s"), "shutdown"),
            (Json::Int(3), "workspace/symbol"),
        ] {
            let out = send(&mut server, &request(id.clone(), method, Json::Null));
            assert_eq!(out.len(), 1, "{method}");
            assert_eq!(error_code(&out[0]), -32002, "{method}");
            assert_eq!(out[0].get("id"), Some(&id), "{method}");
        }
        // после этого initialize по-прежнему работает
        let out = send(&mut server, &initialize_msg(4));
        assert!(out[0].get("result").is_some());
    }

    #[test]
    fn notifications_before_initialize_are_dropped() {
        let mut server = Server::new();
        assert!(send(&mut server, &did_open("file:///a.sga", 1, "x")).is_empty());
        assert_eq!(server.document_text("file:///a.sga"), None);
        send(&mut server, &initialize_msg(1));
        // документ не был запомнен: изменение для него игнорируется
        assert!(send(&mut server, &did_change("file:///a.sga", 2, "y")).is_empty());
        assert_eq!(server.document_text("file:///a.sga"), None);
    }

    #[test]
    fn second_initialize_is_an_invalid_request() {
        let mut server = started();
        let out = send(&mut server, &initialize_msg(9));
        assert_eq!(error_code(&out[0]), -32600);
        assert_eq!(out[0].get("id"), Some(&Json::Int(9)));
    }

    #[test]
    fn requests_after_shutdown_are_invalid_and_notifications_are_dropped() {
        let mut server = started();
        send(&mut server, &request(Json::Int(2), "shutdown", Json::Null));
        for method in [
            "shutdown",
            "initialize",
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/documentSymbol",
            "textDocument/definition",
            "whatever",
        ] {
            let out = send(&mut server, &request(Json::Int(5), method, Json::Null));
            assert_eq!(error_code(&out[0]), -32600, "{method}");
            assert_eq!(out[0].get("id"), Some(&Json::Int(5)));
        }
        assert!(send(&mut server, &did_open("file:///a.sga", 1, "x")).is_empty());
        assert_eq!(server.document_text("file:///a.sga"), None);
    }

    #[test]
    fn unknown_requests_are_method_not_found_with_the_same_id() {
        let mut server = started();
        for id in [
            Json::Int(42),
            Json::string("req-ü"),
            Json::Int(-1),
            Json::Int(0),
        ] {
            let out = send(
                &mut server,
                &request(id.clone(), "textDocument/references", Json::Null),
            );
            assert_eq!(out.len(), 1);
            assert_eq!(error_code(&out[0]), -32601);
            assert_eq!(out[0].get("id"), Some(&id));
            assert!(out[0].get("result").is_none());
        }
    }

    #[test]
    fn unknown_notifications_get_no_reply() {
        let mut server = started();
        for method in [
            "$/cancelRequest",
            "$/setTrace",
            "workspace/didChangeConfiguration",
            "workspace/didChangeWatchedFiles",
            "textDocument/didSave",
            "initialized",
            "custom/anything",
        ] {
            let out = send(&mut server, &notify(method, Json::object([])));
            assert!(out.is_empty(), "{method}: {out:?}");
        }
        assert!(
            server.take_log().is_empty(),
            "молча — значит и без записей в журнал"
        );
    }

    // ── плохие сообщения ─────────────────────────────────────────────

    #[test]
    fn invalid_json_is_a_parse_error_with_null_id_and_the_server_continues() {
        let mut server = started();
        for body in ["{", "not json", "", "{\"a\":}", "[1,]"] {
            let out = send(&mut server, body);
            assert_eq!(out.len(), 1, "{body:?}");
            assert_eq!(error_code(&out[0]), -32700, "{body:?}");
            assert_eq!(out[0].get("id"), Some(&Json::Null), "{body:?}");
        }
        // сервер жив и отвечает на следующий корректный запрос
        let out = send(&mut server, &request(Json::Int(2), "shutdown", Json::Null));
        assert_eq!(out[0].get("result"), Some(&Json::Null));
    }

    #[test]
    fn non_utf8_body_is_a_parse_error() {
        let mut server = started();
        let out = server.handle_message(&[b'{', 0xFF, 0xFE, b'}']);
        assert_eq!(out.len(), 1);
        assert_eq!(error_code(&out[0]), -32700);
        assert_eq!(out[0].get("id"), Some(&Json::Null));
        assert!(out[0]
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Json::as_str)
            .is_some_and(|m| m.contains("UTF-8")));
    }

    /// Текст `error.message` ответа.
    fn error_message(reply: &Json) -> &str {
        reply
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("в ответе нет error.message: {reply}"))
    }

    #[test]
    fn structurally_invalid_messages_are_invalid_requests() {
        let mut server = started();
        // не объект: причина названа в сообщении (все варианты дают -32600 с
        // `id: null`, различаются они только текстом)
        for body in ["[]", "[{\"method\":\"x\"}]", "1", "\"s\"", "null", "true"] {
            let out = send(&mut server, body);
            assert_eq!(out.len(), 1, "{body}");
            assert_eq!(error_code(&out[0]), -32600, "{body}");
            assert_eq!(out[0].get("id"), Some(&Json::Null), "{body}");
            assert!(
                error_message(&out[0]).contains("объектом"),
                "{body}: {}",
                out[0]
            );
        }
        // нет method и нет result/error: id возвращается, если он допустим
        let out = send(&mut server, r#"{"jsonrpc":"2.0","id":7}"#);
        assert_eq!(error_code(&out[0]), -32600);
        assert_eq!(out[0].get("id"), Some(&Json::Int(7)));
        assert!(
            error_message(&out[0]).contains("нет поля method"),
            "{}",
            out[0]
        );
        let out = send(&mut server, "{}");
        assert_eq!(error_code(&out[0]), -32600);
        assert_eq!(out[0].get("id"), Some(&Json::Null));
        // method не строка
        for method in ["5", "null", "true", "[]", "{}"] {
            let out = send(&mut server, &format!(r#"{{"id":"x","method":{method}}}"#));
            assert_eq!(error_code(&out[0]), -32600, "method={method}");
            assert_eq!(
                out[0].get("id"),
                Some(&Json::string("x")),
                "method={method}"
            );
            assert!(
                error_message(&out[0]).contains("method должно быть строкой"),
                "method={method}: {}",
                out[0]
            );
        }
        // id недопустимого типа при наличии method
        for id in ["null", "true", "false", "{}", "[]", "1.5", "1e3"] {
            let out = send(
                &mut server,
                &format!(r#"{{"id":{id},"method":"shutdown"}}"#),
            );
            assert_eq!(out.len(), 1, "id={id}");
            assert_eq!(error_code(&out[0]), -32600, "id={id}");
            assert_eq!(out[0].get("id"), Some(&Json::Null), "id={id}");
            assert!(
                error_message(&out[0]).contains("id должно быть"),
                "id={id}: {}",
                out[0]
            );
        }
        // ни одно из этих сообщений не остановило сервер и не сделало shutdown
        assert_eq!(server.eof_code(), 1);
        let out = send(&mut server, &request(Json::Int(2), "shutdown", Json::Null));
        assert_eq!(out[0].get("result"), Some(&Json::Null));
    }

    #[test]
    fn client_responses_are_ignored() {
        let mut server = started();
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":"a","result":{"x":1}}"#,
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"m"}}"#,
        ] {
            assert!(send(&mut server, body).is_empty(), "{body}");
        }
    }

    #[test]
    fn malformed_document_notifications_are_logged_and_ignored() {
        let mut server = started();
        let bad = [
            notify("textDocument/didOpen", Json::Null),
            notify("textDocument/didOpen", Json::object([])),
            notify(
                "textDocument/didOpen",
                Json::object([(
                    "textDocument",
                    Json::object([("uri", Json::string("file:///a"))]),
                )]),
            ),
            notify(
                "textDocument/didOpen",
                Json::object([("textDocument", Json::object([("text", Json::string("x"))]))]),
            ),
            notify(
                "textDocument/didOpen",
                Json::object([(
                    "textDocument",
                    Json::object([("uri", Json::Int(5)), ("text", Json::string("x"))]),
                )]),
            ),
            notify("textDocument/didChange", Json::Null),
            notify("textDocument/didClose", Json::object([])),
            notify(
                "textDocument/didClose",
                Json::object([("textDocument", Json::object([]))]),
            ),
            // didOpen в виде не-объекта: params — строка
            r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":"oops"}"#.to_string(),
            // совсем без params
            r#"{"jsonrpc":"2.0","method":"textDocument/didOpen"}"#.to_string(),
        ];
        for msg in &bad {
            let out = send(&mut server, msg);
            assert!(out.is_empty(), "{msg}: {out:?}");
            assert!(
                !server.take_log().is_empty(),
                "{msg}: должна быть запись в журнал"
            );
        }
        // didChange с открытым документом, но без text / contentChanges
        send(&mut server, &did_open("file:///a.sga", 1, "x"));
        for params in [
            Json::object([(
                "textDocument",
                Json::object([("uri", Json::string("file:///a.sga"))]),
            )]),
            Json::object([
                (
                    "textDocument",
                    Json::object([("uri", Json::string("file:///a.sga"))]),
                ),
                ("contentChanges", Json::Array(vec![])),
            ]),
            Json::object([
                (
                    "textDocument",
                    Json::object([("uri", Json::string("file:///a.sga"))]),
                ),
                ("contentChanges", Json::Array(vec![Json::object([])])),
            ]),
            Json::object([
                (
                    "textDocument",
                    Json::object([("uri", Json::string("file:///a.sga"))]),
                ),
                ("contentChanges", Json::string("не массив")),
            ]),
        ] {
            let out = send(&mut server, &notify("textDocument/didChange", params));
            assert!(out.is_empty(), "{out:?}");
            assert!(!server.take_log().is_empty());
        }
        assert_eq!(
            server.document_text("file:///a.sga"),
            Some("x"),
            "документ цел"
        );
        // сервер продолжает работать
        let out = send(&mut server, &did_change("file:///a.sga", 2, "y"));
        assert_eq!(only_publish(&out).get("version"), Some(&Json::Int(2)));
    }

    // ── документы и диагностики ──────────────────────────────────────

    #[test]
    fn did_open_publishes_diagnostics_for_that_document() {
        let mut server = started();
        let text = format!("{} 5 = 1;", let_kw()); // число вместо имени
        let out = send(&mut server, &did_open("file:///a.sga", 3, &text));
        let params = only_publish(&out);
        assert_eq!(
            params.get("uri").and_then(Json::as_str),
            Some("file:///a.sga")
        );
        assert_eq!(params.get("version"), Some(&Json::Int(3)));
        let diags = diags_of(params);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.get("severity"), Some(&Json::Int(1)));
        assert_eq!(d.get("source").and_then(Json::as_str), Some("sga"));
        assert_eq!(d.get("code").and_then(Json::as_str), Some("parser"));
        assert_eq!(
            lsp_range(d),
            ((0, 4), (0, 5)),
            "число `5`: колонка 5 -> character 4"
        );
        let message = d.get("message").and_then(Json::as_str).expect("message");
        assert!(message.contains("идентификатор"), "{message}");
        assert!(!message.starts_with('['), "без префикса позиции: {message}");
        assert_eq!(server.document_text("file:///a.sga"), Some(text.as_str()));
    }

    #[test]
    fn lexer_error_is_published_with_its_code_and_range() {
        let mut server = started();
        let out = send(&mut server, &did_open("file:///a.sga", 1, "x = @;"));
        let d = &diags_of(only_publish(&out))[0];
        assert_eq!(d.get("code").and_then(Json::as_str), Some("lexer"));
        assert_eq!(lsp_range(d), ((0, 4), (0, 5)));
    }

    #[test]
    fn semantic_and_type_errors_are_published_with_their_codes_and_node_ranges() {
        let mut server = started();
        let uri = "file:///a.sga";
        // необъявленное имя: диапазон — сам идентификатор `b`
        let text = format!("{} a = 1; {}(b);", let_kw(), encode_word("PRINT"));
        let out = send(&mut server, &did_open(uri, 1, &text));
        let diags = diags_of(only_publish(&out));
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.get("severity"), Some(&Json::Int(1)));
        assert_eq!(d.get("code").and_then(Json::as_str), Some("semantic"));
        assert_eq!(d.get("source").and_then(Json::as_str), Some("sga"));
        assert_eq!(lsp_range(d), ((0, 17), (0, 18)));
        assert_eq!(
            d.get("message").and_then(Json::as_str),
            Some("неопределённая переменная 'b'"),
            "без префикса позиции и названия стадии"
        );

        // ошибка типов: диапазон — строковый литерал
        let text = format!("{} x: int = \"s\";", let_kw());
        let out = send(&mut server, &did_change(uri, 2, &text));
        let diags = diags_of(only_publish(&out));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].get("severity"), Some(&Json::Int(1)));
        assert_eq!(diags[0].get("code").and_then(Json::as_str), Some("type"));
        assert_eq!(lsp_range(&diags[0]), ((0, 13), (0, 16)));

        // исправленный текст — снова пусто
        let text = format!("{} x: int = 1;", let_kw());
        let out = send(&mut server, &did_change(uri, 3, &text));
        assert!(diags_of(only_publish(&out)).is_empty());
    }

    #[test]
    fn semantic_ranges_are_utf16_and_multiline_nodes_span_lines() {
        let mut server = started();
        // эмодзи — один символ, но две единицы UTF-16: `b` на 19-м символе
        // (с нуля), то есть на 20-й единице
        let text = format!("{} s = \"😀\"; {}(b);", let_kw(), encode_word("PRINT"));
        let out = send(&mut server, &did_open("file:///emoji.sga", 1, &text));
        let diags = diags_of(only_publish(&out));
        assert_eq!(
            diags[0].get("code").and_then(Json::as_str),
            Some("semantic")
        );
        assert_eq!(lsp_range(&diags[0]), ((0, 20), (0, 21)));

        // литерал структуры на трёх строках: диапазон тоже на трёх строках;
        // эмодзи в средней строке не влияет на колонки начала и конца — они
        // лежат на других строках, а позиции считаются отдельно в каждой
        let text = format!(
            "{} P {{ x }}\n{} p = P {{\n    y: \"😀\"\n}};",
            encode_word("STRUCT"),
            let_kw()
        );
        let out = send(&mut server, &did_open("file:///multi.sga", 1, &text));
        let diags = diags_of(only_publish(&out));
        assert_eq!(
            diags[0].get("code").and_then(Json::as_str),
            Some("semantic")
        );
        assert_eq!(lsp_range(&diags[0]), ((1, 8), (3, 1)));
    }

    #[test]
    fn an_import_notice_is_published_as_information() {
        let mut server = started();
        let text = format!("{} \"m.sga\";\nunknown_name;", encode_word("IMPORT"));
        let out = send(&mut server, &did_open("file:///a.sga", 1, &text));
        let diags = diags_of(only_publish(&out));
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.get("severity"), Some(&Json::Int(3)), "Information");
        assert_eq!(d.get("code").and_then(Json::as_str), Some("limit"));
        assert_eq!(lsp_range(d), ((0, 0), (0, 15)), "первый IMPORT целиком");
        assert!(
            d.get("message")
                .and_then(Json::as_str)
                .is_some_and(|m| m.contains("IMPORT")),
            "{d}"
        );
    }

    #[test]
    fn every_severity_and_code_reaches_the_wire() {
        use crate::diagnostics::{Code, Diagnostic, Severity};
        let table = [
            (Severity::Error, Code::Lexer, 1, "lexer"),
            (Severity::Error, Code::Parser, 1, "parser"),
            (Severity::Error, Code::Semantic, 1, "semantic"),
            (Severity::Error, Code::Type, 1, "type"),
            (Severity::Warning, Code::Limit, 2, "limit"),
            (Severity::Information, Code::Limit, 3, "limit"),
            (Severity::Error, Code::Internal, 1, "internal"),
        ];
        let mut server = Server::with_analyzer(Box::new(move |_| {
            table
                .iter()
                .map(|&(severity, code, _, _)| Diagnostic {
                    severity,
                    code,
                    message: "m".to_string(),
                    start_line: 1,
                    start_col: 1,
                    end_line: 1,
                    end_col: 1,
                })
                .collect()
        }));
        send(&mut server, &initialize_msg(1));
        let out = send(&mut server, &did_open("file:///a.sga", 1, "x"));
        let diags = diags_of(only_publish(&out));
        assert_eq!(diags.len(), table.len());
        for (d, &(_, _, severity, code)) in diags.iter().zip(table.iter()) {
            assert_eq!(d.get("severity"), Some(&Json::Int(severity)), "{d}");
            assert_eq!(d.get("code").and_then(Json::as_str), Some(code), "{d}");
        }
    }

    #[test]
    fn valid_document_publishes_an_empty_list() {
        let mut server = started();
        let out = send(
            &mut server,
            &did_open("file:///a.sga", 1, &format!("{} x = 1;", let_kw())),
        );
        let params = only_publish(&out);
        assert_eq!(params.get("diagnostics"), Some(&Json::Array(vec![])));
    }

    #[test]
    fn did_change_replaces_the_text_and_republishes() {
        let mut server = started();
        let uri = "file:///a.sga";
        let broken = format!("{} 5 = 1;", let_kw());
        let fixed = format!("{} x = 1;", let_kw());
        let out = send(&mut server, &did_open(uri, 1, &broken));
        assert_eq!(diags_of(only_publish(&out)).len(), 1);

        // исправление: ошибки исчезают
        let out = send(&mut server, &did_change(uri, 2, &fixed));
        let params = only_publish(&out);
        assert_eq!(params.get("version"), Some(&Json::Int(2)));
        assert_eq!(params.get("diagnostics"), Some(&Json::Array(vec![])));
        assert_eq!(server.document_text(uri), Some(fixed.as_str()));

        // снова поломали
        let out = send(&mut server, &did_change(uri, 3, "x = @;"));
        let params = only_publish(&out);
        assert_eq!(params.get("version"), Some(&Json::Int(3)));
        assert_eq!(diags_of(params).len(), 1);
        assert_eq!(server.document_text(uri), Some("x = @;"));
    }

    #[test]
    fn did_close_clears_diagnostics_and_forgets_the_document() {
        let mut server = started();
        let uri = "file:///a.sga";
        send(&mut server, &did_open(uri, 1, "x = @;"));
        let out = send(&mut server, &did_close(uri));
        let params = only_publish(&out);
        assert_eq!(params.get("uri").and_then(Json::as_str), Some(uri));
        assert_eq!(params.get("diagnostics"), Some(&Json::Array(vec![])));
        assert_eq!(server.document_text(uri), None);

        // последующие didChange/didClose для забытого документа — без ответа
        assert!(send(&mut server, &did_change(uri, 2, "y = @;")).is_empty());
        assert!(send(&mut server, &did_close(uri)).is_empty());
        assert_eq!(server.document_text(uri), None);
        assert!(
            !server.take_log().is_empty(),
            "игнорирование записано в журнал"
        );
    }

    #[test]
    fn documents_are_independent() {
        let mut server = started();
        let (a, b) = ("file:///a.sga", "file:///b.sga");
        let out = send(&mut server, &did_open(a, 1, "x = @;"));
        assert_eq!(diags_of(only_publish(&out)).len(), 1);
        let out = send(
            &mut server,
            &did_open(b, 1, &format!("{} x = 1;", let_kw())),
        );
        assert!(diags_of(only_publish(&out)).is_empty());

        // изменение b не трогает a (публикуется только uri изменённого)
        let out = send(&mut server, &did_change(b, 2, "y = #;"));
        let params = only_publish(&out);
        assert_eq!(params.get("uri").and_then(Json::as_str), Some(b));
        assert_eq!(diags_of(params).len(), 1);
        assert_eq!(server.document_text(a), Some("x = @;"));
        assert_eq!(server.document_text(b), Some("y = #;"));

        // закрытие a не забывает b
        send(&mut server, &did_close(a));
        assert_eq!(server.document_text(a), None);
        assert_eq!(server.document_text(b), Some("y = #;"));
    }

    #[test]
    fn reopening_an_open_document_replaces_it() {
        let mut server = started();
        let uri = "file:///a.sga";
        send(&mut server, &did_open(uri, 1, "x = @;"));
        let out = send(
            &mut server,
            &did_open(uri, 5, &format!("{} x = 1;", let_kw())),
        );
        let params = only_publish(&out);
        assert_eq!(params.get("version"), Some(&Json::Int(5)));
        assert!(diags_of(params).is_empty());
    }

    #[test]
    fn incremental_change_is_refused_and_leaves_the_document_alone() {
        let mut server = started();
        let uri = "file:///a.sga";
        let original = format!("{} x = 1;", let_kw());
        send(&mut server, &did_open(uri, 1, &original));
        let msg = notify(
            "textDocument/didChange",
            Json::object([
                (
                    "textDocument",
                    Json::object([("uri", Json::string(uri)), ("version", Json::Int(2))]),
                ),
                (
                    "contentChanges",
                    Json::Array(vec![Json::object([
                        (
                            "range",
                            Json::object([
                                (
                                    "start",
                                    Json::object([
                                        ("line", Json::Int(0)),
                                        ("character", Json::Int(0)),
                                    ]),
                                ),
                                (
                                    "end",
                                    Json::object([
                                        ("line", Json::Int(0)),
                                        ("character", Json::Int(1)),
                                    ]),
                                ),
                            ]),
                        ),
                        ("text", Json::string("@")),
                    ])]),
                ),
            ]),
        );
        let out = send(&mut server, &msg);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(server.document_text(uri), Some(original.as_str()));
        let log = server.take_log();
        assert!(
            log.iter().any(|l| l.contains("инкрементальн")),
            "причина должна быть в журнале: {log:?}"
        );
        // сервер жив: полный текст принимается
        let out = send(&mut server, &did_change(uri, 3, "@"));
        assert_eq!(diags_of(only_publish(&out)).len(), 1);
    }

    #[test]
    fn range_null_means_a_full_replacement() {
        // `range: null` — не инкрементальное изменение (поле как бы отсутствует)
        let mut server = started();
        let uri = "file:///a.sga";
        send(&mut server, &did_open(uri, 1, "x"));
        let msg = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":2}},"contentChanges":[{{"range":null,"text":"@"}}]}}}}"#
        );
        let out = send(&mut server, &msg);
        assert_eq!(diags_of(only_publish(&out)).len(), 1);
        assert_eq!(server.document_text(uri), Some("@"));
    }

    #[test]
    fn the_last_content_change_wins() {
        let mut server = started();
        let uri = "file:///a.sga";
        send(&mut server, &did_open(uri, 1, "x"));
        let msg = notify(
            "textDocument/didChange",
            Json::object([
                (
                    "textDocument",
                    Json::object([("uri", Json::string(uri)), ("version", Json::Int(2))]),
                ),
                (
                    "contentChanges",
                    Json::Array(vec![
                        Json::object([("text", Json::string("первый"))]),
                        Json::object([("text", Json::string("@"))]),
                    ]),
                ),
            ]),
        );
        let out = send(&mut server, &msg);
        assert_eq!(diags_of(only_publish(&out)).len(), 1);
        assert_eq!(server.document_text(uri), Some("@"));
    }

    #[test]
    fn version_is_reported_only_when_the_client_sent_it() {
        let mut server = started();
        let uri = "file:///a.sga";
        let msg = notify(
            "textDocument/didOpen",
            Json::object([(
                "textDocument",
                Json::object([("uri", Json::string(uri)), ("text", Json::string("@"))]),
            )]),
        );
        let out = send(&mut server, &msg);
        assert!(only_publish(&out).get("version").is_none());

        // didClose публикует без версии
        let out = send(&mut server, &did_close(uri));
        assert!(only_publish(&out).get("version").is_none());

        // не целая версия — как отсутствующая
        let msg = notify(
            "textDocument/didOpen",
            Json::object([(
                "textDocument",
                Json::object([
                    ("uri", Json::string(uri)),
                    ("version", Json::string("3")),
                    ("text", Json::string("@")),
                ]),
            )]),
        );
        let out = send(&mut server, &msg);
        assert!(only_publish(&out).get("version").is_none());
    }

    #[test]
    fn published_positions_are_utf16_and_zero_based() {
        let mut server = started();
        // `LET s = "😀" @`: эмодзи (2 единицы UTF-16) стоит ДО ошибки на той же строке
        let text = format!("{} s = \"😀\" @", let_kw());
        let out = send(&mut server, &did_open("file:///a.sga", 1, &text));
        let d = &diags_of(only_publish(&out))[0];
        assert_eq!(d.get("code").and_then(Json::as_str), Some("lexer"));
        // символов до `@` — 12 (колонка 13), единиц UTF-16 — 13
        assert_eq!(lsp_range(d), ((0, 13), (0, 14)));

        // ошибка на третьей строке: нумерация строк с нуля
        let out = send(
            &mut server,
            &did_change("file:///a.sga", 2, "x = 1;\ny = 2;\n  @"),
        );
        let d = &diags_of(only_publish(&out))[0];
        assert_eq!(lsp_range(d), ((2, 2), (2, 3)));
    }

    #[test]
    fn parser_error_at_end_of_text_is_an_empty_range_at_the_end() {
        let mut server = started();
        let text = format!("{} x", let_kw());
        let out = send(&mut server, &did_open("file:///a.sga", 1, &text));
        let d = &diags_of(only_publish(&out))[0];
        assert_eq!(lsp_range(d), ((0, 5), (0, 5)));

        // после завершающего перевода строки — пустая последняя строка
        let out = send(
            &mut server,
            &did_change("file:///a.sga", 2, &format!("{text}\n")),
        );
        let d = &diags_of(only_publish(&out))[0];
        assert_eq!(lsp_range(d), ((1, 0), (1, 0)));
    }

    #[test]
    fn crlf_document_ranges_are_clamped_to_the_visible_line() {
        let mut server = started();
        let out = send(
            &mut server,
            &did_open("file:///a.sga", 1, "s = \"abc\r\nnext"),
        );
        let d = &diags_of(only_publish(&out))[0];
        // конец — после `\r`; клиент обрежет до конца строки
        assert_eq!(lsp_range(d), ((0, 4), (0, 9)));
    }

    // ── пределы и паники ─────────────────────────────────────────────

    fn counting_server() -> (Server, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut server = Server::with_analyzer(Box::new(move |_| {
            counter.set(counter.get() + 1);
            Vec::new()
        }));
        send(&mut server, &initialize_msg(1));
        (server, calls)
    }

    #[test]
    fn oversized_document_is_not_analyzed_and_gets_a_limit_warning() {
        let (mut server, calls) = counting_server();
        let text = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        let out = send(&mut server, &did_open("file:///big.sga", 1, &text));
        assert_eq!(calls.get(), 0, "анализатор не должен вызываться");
        let diags = diags_of(only_publish(&out));
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].get("severity"),
            Some(&Json::Int(2)),
            "предупреждение"
        );
        assert_eq!(diags[0].get("code").and_then(Json::as_str), Some("limit"));
        assert_eq!(lsp_range(&diags[0]), ((0, 0), (0, 0)));
        let message = diags[0]
            .get("message")
            .and_then(Json::as_str)
            .expect("message");
        assert!(message.contains("больше предела"), "{message}");
        // документ при этом запомнен: исправление до предела снова анализируется
        assert_eq!(
            server.document_text("file:///big.sga").map(str::len),
            Some(text.len())
        );
        let out = send(&mut server, &did_change("file:///big.sga", 2, "x"));
        assert_eq!(calls.get(), 1);
        assert!(diags_of(only_publish(&out)).is_empty());
    }

    #[test]
    fn limit_is_measured_in_bytes_and_the_boundary_is_inclusive() {
        let (mut server, calls) = counting_server();
        let exactly = "x".repeat(MAX_DOCUMENT_BYTES);
        let out = send(&mut server, &did_open("file:///a.sga", 1, &exactly));
        assert_eq!(calls.get(), 1, "ровно предел анализируется");
        assert!(diags_of(only_publish(&out)).is_empty());

        // 2-байтовые символы: мало символов, но байт больше предела
        let by_bytes = "я".repeat(MAX_DOCUMENT_BYTES / 2 + 1);
        assert!(by_bytes.chars().count() < MAX_DOCUMENT_BYTES);
        let out = send(&mut server, &did_change("file:///a.sga", 2, &by_bytes));
        assert_eq!(calls.get(), 1, "не анализируется");
        assert_eq!(
            diags_of(only_publish(&out))[0]
                .get("code")
                .and_then(Json::as_str),
            Some("limit")
        );
    }

    #[test]
    fn analyzer_panic_becomes_an_internal_diagnostic_and_the_server_survives() {
        let mut server = Server::with_analyzer(Box::new(|_| panic!("анализатор сломался")));
        send(&mut server, &initialize_msg(1));
        let out = send(&mut server, &did_open("file:///a.sga", 1, "что угодно"));
        let params = only_publish(&out);
        let diags = diags_of(params);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].get("code").and_then(Json::as_str),
            Some("internal")
        );
        assert_eq!(diags[0].get("severity"), Some(&Json::Int(1)));
        assert_eq!(lsp_range(&diags[0]), ((0, 0), (0, 0)));
        let log = server.take_log();
        assert!(
            log.iter().any(|l| l.contains("анализатор сломался")),
            "причина паники должна попасть в журнал: {log:?}"
        );
        // повторная паника на didChange тоже изолирована
        let out = send(&mut server, &did_change("file:///a.sga", 2, "ещё"));
        assert_eq!(
            diags_of(only_publish(&out))[0]
                .get("code")
                .and_then(Json::as_str),
            Some("internal")
        );
        // и сервер отвечает на следующий запрос
        let out = send(&mut server, &request(Json::Int(9), "shutdown", Json::Null));
        assert_eq!(out[0].get("id"), Some(&Json::Int(9)));
        assert_eq!(out[0].get("result"), Some(&Json::Null));
    }

    #[test]
    fn analyzer_panic_with_a_string_payload_is_reported_too() {
        let mut server =
            Server::with_analyzer(Box::new(|text| panic!("плохой текст: {}", text.len())));
        send(&mut server, &initialize_msg(1));
        send(&mut server, &did_open("file:///a.sga", 1, "abc"));
        let log = server.take_log();
        assert!(log.iter().any(|l| l.contains("плохой текст: 3")), "{log:?}");
    }

    #[test]
    fn request_panic_becomes_internal_error_with_the_request_id() {
        let mut server = started();
        for id in [Json::Int(11), Json::string("p")] {
            let out = send(
                &mut server,
                &request(id.clone(), "$/sga/testPanic", Json::Null),
            );
            assert_eq!(out.len(), 1);
            assert_eq!(error_code(&out[0]), -32603);
            assert_eq!(out[0].get("id"), Some(&id));
        }
        let log = server.take_log();
        assert!(
            log.iter().any(|l| l.contains("тестовая паника запроса")),
            "{log:?}"
        );
        // состояние не испорчено: shutdown по-прежнему работает
        let out = send(&mut server, &request(Json::Int(12), "shutdown", Json::Null));
        assert_eq!(out[0].get("result"), Some(&Json::Null));
    }

    #[test]
    fn notification_panic_is_logged_and_does_not_stop_the_server() {
        let mut server = started();
        let out = send(&mut server, &notify("$/sga/testPanic", Json::Null));
        assert!(out.is_empty(), "{out:?}");
        let log = server.take_log();
        assert!(
            log.iter()
                .any(|l| l.contains("паника при обработке уведомления")
                    && l.contains("тестовая паника уведомления")),
            "{log:?}"
        );
        // сервер жив: документы по-прежнему принимаются, shutdown работает
        let out = send(&mut server, &did_open("file:///a.sga", 1, "@"));
        assert_eq!(diags_of(only_publish(&out)).len(), 1);
        let out = send(&mut server, &request(Json::Int(3), "shutdown", Json::Null));
        assert_eq!(out[0].get("result"), Some(&Json::Null));
    }

    #[test]
    fn the_test_panic_hook_is_not_a_production_method() {
        // до initialize хук недоступен (там -32002), как и любой метод
        let mut server = Server::new();
        let out = send(
            &mut server,
            &request(Json::Int(1), "$/sga/testPanic", Json::Null),
        );
        assert_eq!(error_code(&out[0]), -32002);
    }

    #[test]
    fn guarded_extracts_messages_from_all_payload_kinds() {
        assert_eq!(guarded(|| 5), Ok(5));
        assert_eq!(
            guarded(|| -> i32 { panic!("статика") }),
            Err("статика".to_string())
        );
        assert_eq!(
            guarded(|| -> i32 { panic!("{}-{}", "формат", 1) }),
            Err("формат-1".to_string())
        );
        assert_eq!(
            guarded(|| -> i32 { std::panic::panic_any(42u8) }),
            Err("паника без сообщения".to_string())
        );
    }

    // ── цикл чтения/записи ───────────────────────────────────────────

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    fn frames(bodies: &[String]) -> Vec<u8> {
        bodies.iter().flat_map(|b| frame(b)).collect()
    }

    /// Разбирает выходной поток на сообщения; любой лишний байт — провал.
    fn parse_output(bytes: &[u8]) -> Vec<Json> {
        let mut input = Cursor::new(bytes.to_vec());
        let mut out = Vec::new();
        while let Some(body) = transport::read_message(&mut input).expect("выход — только фреймы")
        {
            out.push(parse_msg(std::str::from_utf8(&body).expect("UTF-8")));
        }
        out
    }

    struct Run {
        code: i32,
        output: Vec<u8>,
        log: Vec<String>,
    }

    fn run_serve(input: &[u8]) -> Run {
        let mut server = Server::new();
        let mut output = Vec::new();
        let mut log = Vec::new();
        let mut sink = |line: &str| log.push(line.to_string());
        let code = serve_with(
            &mut server,
            &mut Cursor::new(input.to_vec()),
            &mut output,
            &mut sink,
        );
        Run { code, output, log }
    }

    #[test]
    fn a_whole_session_runs_to_a_clean_exit() {
        let uri = "file:///a.sga";
        let session = frames(&[
            initialize_msg(1),
            notify("initialized", Json::object([])),
            did_open(uri, 1, "x = @;"),
            did_change(uri, 2, &format!("{} x = 1;", let_kw())),
            did_close(uri),
            request(Json::Int(2), "shutdown", Json::Null),
            notify("exit", Json::Null),
        ]);
        let run = run_serve(&session);
        assert_eq!(run.code, 0);
        assert!(run.log.is_empty(), "{:?}", run.log);
        let out = parse_output(&run.output);
        assert_eq!(
            out.len(),
            5,
            "initialize + 3 публикации + shutdown: {out:?}"
        );
        assert_eq!(out[0].get("id"), Some(&Json::Int(1)));
        assert_eq!(diags_of(out[1].get("params").unwrap()).len(), 1);
        assert!(diags_of(out[2].get("params").unwrap()).is_empty());
        assert!(diags_of(out[3].get("params").unwrap()).is_empty());
        assert_eq!(out[4].get("id"), Some(&Json::Int(2)));
        assert_eq!(out[4].get("result"), Some(&Json::Null));
    }

    #[test]
    fn exit_without_shutdown_returns_one() {
        let run = run_serve(&frames(&[initialize_msg(1), notify("exit", Json::Null)]));
        assert_eq!(run.code, 1);
        assert_eq!(parse_output(&run.output).len(), 1);
    }

    #[test]
    fn end_of_input_returns_zero_only_after_shutdown() {
        let after = run_serve(&frames(&[
            initialize_msg(1),
            request(Json::Int(2), "shutdown", Json::Null),
        ]));
        assert_eq!(after.code, 0);
        let before = run_serve(&frames(&[initialize_msg(1)]));
        assert_eq!(before.code, 1);
        let empty = run_serve(b"");
        assert_eq!(empty.code, 1);
        assert!(empty.output.is_empty());
        assert!(empty.log.is_empty(), "чистый EOF — не ошибка фрейминга");
    }

    #[test]
    fn nothing_after_exit_is_processed() {
        let run = run_serve(&frames(&[
            initialize_msg(1),
            request(Json::Int(2), "shutdown", Json::Null),
            notify("exit", Json::Null),
            request(Json::Int(3), "shutdown", Json::Null),
            did_open("file:///a.sga", 1, "@"),
        ]));
        assert_eq!(run.code, 0);
        assert_eq!(
            parse_output(&run.output).len(),
            2,
            "только initialize и shutdown"
        );
    }

    #[test]
    fn a_framing_error_stops_the_server_with_code_one() {
        let mut input = frames(&[initialize_msg(1)]);
        input.extend(b"Content-Type: x\r\n\r\n{}"); // нет Content-Length
        input.extend(frames(&[request(Json::Int(2), "shutdown", Json::Null)]));
        let run = run_serve(&input);
        assert_eq!(run.code, 1);
        assert_eq!(
            parse_output(&run.output).len(),
            1,
            "после ошибки фрейма ничего не обрабатывается"
        );
        assert!(
            run.log
                .iter()
                .any(|l| l.contains("фрейминга") && l.contains("Content-Length")),
            "{:?}",
            run.log
        );
    }

    #[test]
    fn a_truncated_body_stops_the_server_with_code_one() {
        let mut input = frames(&[initialize_msg(1)]);
        input.extend(b"Content-Length: 100\r\n\r\n{\"jsonrpc\"");
        let run = run_serve(&input);
        assert_eq!(run.code, 1);
        assert!(
            run.log.iter().any(|l| l.contains("в теле сообщения")),
            "{:?}",
            run.log
        );
    }

    #[test]
    fn a_bad_json_body_does_not_stop_the_server() {
        let run = run_serve(&frames(&[
            initialize_msg(1),
            "это не json".to_string(),
            request(Json::Int(2), "shutdown", Json::Null),
            notify("exit", Json::Null),
        ]));
        assert_eq!(run.code, 0);
        let out = parse_output(&run.output);
        assert_eq!(out.len(), 3);
        assert_eq!(error_code(&out[1]), -32700);
        assert_eq!(out[2].get("id"), Some(&Json::Int(2)));
    }

    #[test]
    fn a_write_failure_stops_the_server_with_code_one() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut server = Server::new();
        let mut log = Vec::new();
        let mut sink = |line: &str| log.push(line.to_string());
        let code = serve_with(
            &mut server,
            &mut Cursor::new(frames(&[initialize_msg(1), initialize_msg(2)])),
            &mut Closed,
            &mut sink,
        );
        assert_eq!(code, 1);
        assert!(log.iter().any(|l| l.contains("записать")), "{log:?}");
    }

    #[test]
    fn server_log_lines_reach_the_sink() {
        let run = run_serve(&frames(&[
            initialize_msg(1),
            did_change("file:///never-opened.sga", 1, "x"),
        ]));
        assert!(
            run.log.iter().any(|l| l.contains("неоткрытого документа")),
            "{:?}",
            run.log
        );
    }

    #[test]
    fn stdout_side_carries_frames_only() {
        // Все исходящие байты разбираются как фреймы без остатка (parse_output
        // паникует на любом постороннем байте) — в т.ч. при плохих сообщениях.
        let run = run_serve(&frames(&[
            initialize_msg(1),
            "{".to_string(),
            "[]".to_string(),
            did_open("file:///a.sga", 1, "x = @; // привет 😀"),
            r#"{"id":1,"method":"unknown/request"}"#.to_string(),
            request(Json::Int(2), "shutdown", Json::Null),
        ]));
        let out = parse_output(&run.output);
        // 1: ответ initialize; 2: `{` -> -32700; 3: `[]` -> -32600;
        // 4: публикация диагностик; 5: неизвестный запрос -> -32601;
        // 6: ответ shutdown.
        assert_eq!(out.len(), 6, "{out:?}");
        assert!(out[0].get("result").is_some(), "{}", out[0]);
        assert_eq!(error_code(&out[1]), -32700);
        assert_eq!(error_code(&out[2]), -32600);
        assert_eq!(
            out[3].get("method").and_then(Json::as_str),
            Some("textDocument/publishDiagnostics")
        );
        assert_eq!(error_code(&out[4]), -32601);
        assert_eq!(out[5].get("id"), Some(&Json::Int(2)));
    }

    // ── поток с большим стеком ───────────────────────────────────────

    #[test]
    fn the_work_runs_on_a_dedicated_thread_with_a_large_stack() {
        let code = run_with_large_stack(|| {
            if std::thread::current().name() != Some("sga-lsp") {
                return 10;
            }
            // 4 МиБ на стеке: на потоке тестового харнесса (2 МиБ) это
            // переполнило бы стек и уронило процесс.
            let buffer = [7u8; 4 * 1024 * 1024];
            std::hint::black_box(&buffer);
            if buffer[buffer.len() - 1] == 7 {
                0
            } else {
                11
            }
        });
        assert_eq!(code, 0);
    }

    #[test]
    fn a_panicking_worker_gives_exit_code_one() {
        let code = run_with_large_stack(|| panic!("рабочий поток упал"));
        assert_eq!(code, 1);
    }

    #[test]
    fn a_huge_operator_chain_gets_a_depth_warning_without_crashing_on_the_large_stack() {
        // `LET x = 1+1+...+1` из 200 000 слагаемых (400 КБ): AST глубиной в
        // 200 000 узлов глубже `MAX_ANALYSIS_DEPTH`, поэтому semantic и типы
        // не запускаются (их стек растёт с глубиной), а сам AST освобождается
        // рекурсивно — на стеке тестового потока (2 МиБ) он не поместился бы,
        // в потоке сервера помещается. Результат — одно предупреждение.
        let code = run_with_large_stack(|| {
            let mut server = Server::new();
            server.handle_message(initialize_msg(1).as_bytes());
            let text = format!("{} x = 1{};", encode_word("LET"), "+1".repeat(200_000));
            assert!(text.len() < MAX_DOCUMENT_BYTES);
            let out = server.handle_message(did_open("file:///chain.sga", 1, &text).as_bytes());
            let params = out.first().and_then(|message| message.get("params"));
            let diagnostics = params
                .and_then(|p| p.get("diagnostics"))
                .and_then(Json::as_array)
                .unwrap_or(&[]);
            let warning = diagnostics.len() == 1
                && out.len() == 1
                && diagnostics[0].get("severity").and_then(Json::as_i64) == Some(2)
                && diagnostics[0].get("code").and_then(Json::as_str) == Some("limit");
            if warning {
                0
            } else {
                1
            }
        });
        assert_eq!(code, 0);
    }

    // ── hover и completion (T018) ────────────────────────────────────

    fn position_request(id: i64, method: &str, uri: &str, line: i64, character: i64) -> String {
        request(
            Json::Int(id),
            method,
            Json::object([
                ("textDocument", Json::object([("uri", Json::string(uri))])),
                (
                    "position",
                    Json::object([
                        ("line", Json::Int(line)),
                        ("character", Json::Int(character)),
                    ]),
                ),
            ]),
        )
    }

    /// `textDocument/documentSymbol` — параметры только с документом.
    fn symbol_request(id: i64, uri: &str) -> String {
        request(
            Json::Int(id),
            "textDocument/documentSymbol",
            Json::object([("textDocument", Json::object([("uri", Json::string(uri))]))]),
        )
    }

    /// `result` ответа `documentSymbol` для `file:///a.sga`.
    fn symbols_of(server: &mut Server) -> Json {
        let out = send(server, &symbol_request(8, "file:///a.sga"));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].get("id"), Some(&Json::Int(8)));
        assert!(out[0].get("error").is_none(), "{}", out[0]);
        out[0]
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("нет result: {}", out[0]))
    }

    /// Сервер с открытым документом `file:///a.sga`.
    fn opened(text: &str) -> Server {
        let mut server = started();
        send(&mut server, &did_open("file:///a.sga", 1, text));
        server
    }

    /// `result` ответа на запрос по позиции в `file:///a.sga`.
    fn query(server: &mut Server, method: &str, line: i64, character: i64) -> Json {
        let out = send(
            server,
            &position_request(7, method, "file:///a.sga", line, character),
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].get("id"), Some(&Json::Int(7)));
        assert!(out[0].get("error").is_none(), "{}", out[0]);
        out[0]
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("нет result: {}", out[0]))
    }

    fn hover_of(text: &str, line: i64, character: i64) -> Json {
        query(&mut opened(text), "textDocument/hover", line, character)
    }

    fn items_of(result: &Json) -> &[Json] {
        result
            .as_array()
            .unwrap_or_else(|| panic!("не массив: {result}"))
    }

    fn item_named<'a>(items: &'a [Json], label: &str) -> &'a Json {
        items
            .iter()
            .find(|i| i.get("label").and_then(Json::as_str) == Some(label))
            .unwrap_or_else(|| panic!("нет пункта {label}"))
    }

    fn range_of(value: &Json) -> ((i64, i64), (i64, i64)) {
        lsp_range(value)
    }

    #[test]
    fn hover_over_a_keyword_returns_markdown_and_the_word_range() {
        let text = format!("{} x = 1;", let_kw());
        for character in 0..=3 {
            let result = hover_of(&text, 0, character);
            let contents = result.get("contents").expect("contents");
            assert_eq!(
                contents.get("kind").and_then(Json::as_str),
                Some("markdown")
            );
            let value = contents.get("value").and_then(Json::as_str).expect("value");
            assert!(value.contains("**`LET`**"), "{value}");
            assert!(value.contains(&let_kw()), "написание буквами SGA");
            assert_eq!(range_of(&result), ((0, 0), (0, 3)), "символ {character}");
        }
    }

    #[test]
    fn hover_over_a_builtin_call_returns_its_signature() {
        let text = format!("{}(len(x));", encode_word("PRINT"));
        let result = hover_of(&text, 0, 7);
        let value = result
            .get("contents")
            .and_then(|c| c.get("value"))
            .and_then(Json::as_str)
            .expect("value");
        let signature = vocabulary::builtin("len").expect("len").signature();
        assert!(value.contains(&signature), "{value}");
        assert_eq!(range_of(&result), ((0, 6), (0, 9)));
    }

    #[test]
    fn hover_without_content_is_a_null_result_not_an_error() {
        let text = "LET x = 42 + 7; len";
        let mut server = opened(text);
        for character in [0, 4, 8, 10, 12, 14] {
            let out = send(
                &mut server,
                &position_request(3, "textDocument/hover", "file:///a.sga", 0, character),
            );
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].get("result"), Some(&Json::Null), "{character}");
            assert!(out[0].get("error").is_none());
        }
    }

    #[test]
    fn requests_for_an_unopened_or_closed_document_give_null() {
        let mut server = started();
        for method in [
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/definition",
        ] {
            let out = send(
                &mut server,
                &position_request(3, method, "file:///none.sga", 0, 0),
            );
            assert_eq!(out[0].get("result"), Some(&Json::Null), "{method}");
        }
        let out = send(&mut server, &symbol_request(4, "file:///none.sga"));
        assert_eq!(out[0].get("result"), Some(&Json::Null), "documentSymbol");
        send(&mut server, &did_open("file:///a.sga", 1, "len(1)"));
        assert!(query(&mut server, "textDocument/hover", 0, 1)
            .get("contents")
            .is_some());
        assert_eq!(symbols_of(&mut server), Json::Array(vec![]));
        send(&mut server, &did_close("file:///a.sga"));
        for method in [
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/definition",
        ] {
            assert_eq!(query(&mut server, method, 0, 1), Json::Null, "{method}");
        }
        assert_eq!(symbols_of(&mut server), Json::Null, "documentSymbol");
    }

    #[test]
    fn an_oversized_document_gets_null_and_the_limit_is_inclusive() {
        let at_limit = format!("len({})", " ".repeat(MAX_DOCUMENT_BYTES - 5));
        assert_eq!(at_limit.len(), MAX_DOCUMENT_BYTES);
        let mut server = opened(&at_limit);
        assert!(query(&mut server, "textDocument/hover", 0, 1)
            .get("contents")
            .is_some());
        assert!(query(&mut server, "textDocument/completion", 0, 0)
            .as_array()
            .is_some());
        // на границе навигация работает: результата `null` быть не должно
        assert_eq!(symbols_of(&mut server), Json::Array(vec![]));
        assert_eq!(
            query(&mut server, "textDocument/definition", 0, 1),
            Json::Null,
            "builtin `len` места в документе не имеет"
        );

        let over = format!("{at_limit} ");
        let mut server = opened(&over);
        assert_eq!(query(&mut server, "textDocument/hover", 0, 1), Json::Null);
        assert_eq!(
            query(&mut server, "textDocument/completion", 0, 0),
            Json::Null
        );
        assert_eq!(
            query(&mut server, "textDocument/definition", 0, 1),
            Json::Null
        );
        assert_eq!(symbols_of(&mut server), Json::Null, "documentSymbol");
    }

    #[test]
    fn navigation_works_exactly_at_the_size_limit_and_is_off_above_it() {
        // граница включительно и для навигации, а не только для подсказок:
        // на документе ровно в предел символы и переход есть, на байт больше — `null`
        let head = format!("{} v = 1; ", let_kw());
        let at_limit = format!("{head}{}", " ".repeat(MAX_DOCUMENT_BYTES - head.len()));
        assert_eq!(at_limit.len(), MAX_DOCUMENT_BYTES);
        let mut server = opened(&at_limit);
        let symbols = symbols_of(&mut server);
        assert_eq!(items_of(&symbols).len(), 1, "{symbols}");
        let use_site = format!("{head}v");
        let over = format!(
            "{use_site}{}",
            " ".repeat(MAX_DOCUMENT_BYTES + 1 - use_site.len())
        );
        assert_eq!(over.len(), MAX_DOCUMENT_BYTES + 1);
        let mut server = opened(&over);
        assert_eq!(symbols_of(&mut server), Json::Null);
        let use_column = i64::try_from(head.chars().count()).expect("small");
        assert_eq!(
            query(&mut server, "textDocument/definition", 0, use_column),
            Json::Null
        );
    }

    #[test]
    fn malformed_query_params_are_invalid_params_with_the_same_id() {
        let uri = Json::object([("uri", Json::string("file:///a.sga"))]);
        let pos =
            |line: Json, character: Json| Json::object([("line", line), ("character", character)]);
        let good_pos = pos(Json::Int(0), Json::Int(0));
        let bad_params: Vec<(&str, Json)> = vec![
            ("null", Json::Null),
            ("массив", Json::Array(vec![])),
            ("пустой объект", Json::object([])),
            (
                "нет position",
                Json::object([("textDocument", uri.clone())]),
            ),
            (
                "нет textDocument",
                Json::object([("position", good_pos.clone())]),
            ),
            (
                "uri не строка",
                Json::object([
                    ("textDocument", Json::object([("uri", Json::Int(1))])),
                    ("position", good_pos.clone()),
                ]),
            ),
            (
                "textDocument не объект",
                Json::object([
                    ("textDocument", Json::string("file:///a.sga")),
                    ("position", good_pos.clone()),
                ]),
            ),
            (
                "position не объект",
                Json::object([("textDocument", uri.clone()), ("position", Json::Int(0))]),
            ),
            (
                "нет line",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", Json::object([("character", Json::Int(0))])),
                ]),
            ),
            (
                "нет character",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", Json::object([("line", Json::Int(0))])),
                ]),
            ),
            (
                "line отрицательная",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", pos(Json::Int(-1), Json::Int(0))),
                ]),
            ),
            (
                "character отрицательный",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", pos(Json::Int(0), Json::Int(-1))),
                ]),
            ),
            (
                "line дробная",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", pos(Json::Float(1.5), Json::Int(0))),
                ]),
            ),
            (
                "character строка",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", pos(Json::Int(0), Json::string("0"))),
                ]),
            ),
            (
                "line null",
                Json::object([
                    ("textDocument", uri.clone()),
                    ("position", pos(Json::Null, Json::Int(0))),
                ]),
            ),
        ];
        let mut server = opened("len(1)");
        for method in [
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/definition",
        ] {
            for (what, params) in &bad_params {
                let out = send(
                    &mut server,
                    &request(Json::string("id-ü"), method, params.clone()),
                );
                assert_eq!(out.len(), 1, "{method}: {what}");
                assert_eq!(error_code(&out[0]), -32602, "{method}: {what}");
                assert_eq!(out[0].get("id"), Some(&Json::string("id-ü")));
                assert!(out[0].get("result").is_none());
            }
        }
        // запрос без params вообще
        let msg = Json::object([
            ("jsonrpc", Json::string("2.0")),
            ("id", Json::Int(5)),
            ("method", Json::string("textDocument/hover")),
        ])
        .to_string();
        assert_eq!(error_code(&send(&mut server, &msg)[0]), -32602);
        // после ошибок сервер работает
        assert!(query(&mut server, "textDocument/hover", 0, 1)
            .get("contents")
            .is_some());
    }

    #[test]
    fn huge_coordinates_are_clamped_to_the_end_instead_of_failing() {
        let mut server = opened("ab\nle");
        for (line, character) in [
            (i64::MAX, i64::MAX),
            (0, i64::MAX),
            (99, 0),
            (1, 4_294_967_296),
        ] {
            let hover = query(&mut server, "textDocument/hover", line, character);
            assert_eq!(hover, Json::Null);
            let items = query(&mut server, "textDocument/completion", line, character);
            assert!(items.as_array().is_some(), "{line}:{character}");
            assert_eq!(
                query(&mut server, "textDocument/definition", line, character),
                Json::Null,
                "{line}:{character}"
            );
        }
        // конец текста: префикс `le`
        let items = query(&mut server, "textDocument/completion", i64::MAX, i64::MAX);
        assert_eq!(
            range_of(
                item_named(items_of(&items), "LET")
                    .get("textEdit")
                    .expect("textEdit")
            ),
            ((1, 0), (1, 2))
        );
    }

    #[test]
    fn completion_returns_items_with_sort_filter_and_text_edit() {
        let mut server = opened("le");
        let result = query(&mut server, "textDocument/completion", 0, 2);
        let items = items_of(&result);
        let labels: Vec<&str> = items
            .iter()
            .map(|i| i.get("label").and_then(Json::as_str).expect("label"))
            .collect();
        assert_eq!(labels, ["len", "LET"]);

        let let_item = item_named(items, "LET");
        assert_eq!(let_item.get("kind"), Some(&Json::Int(14)));
        assert_eq!(
            let_item.get("sortText").and_then(Json::as_str),
            Some("4LET")
        );
        assert_eq!(
            let_item.get("filterText").and_then(Json::as_str),
            Some("LET")
        );
        assert_eq!(
            let_item.get("detail").and_then(Json::as_str),
            Some("неизменяемая переменная")
        );
        let edit = let_item.get("textEdit").expect("textEdit");
        assert_eq!(
            edit.get("newText").and_then(Json::as_str),
            Some(let_kw().as_str())
        );
        assert_eq!(range_of(edit), ((0, 0), (0, 2)));
        let doc = let_item.get("documentation").expect("documentation");
        assert_eq!(doc.get("kind").and_then(Json::as_str), Some("markdown"));
        assert!(doc
            .get("value")
            .and_then(Json::as_str)
            .is_some_and(|v| v.contains("LET имя [: тип] = выражение;")));

        let len_item = item_named(items, "len");
        assert_eq!(len_item.get("kind"), Some(&Json::Int(3)));
        assert_eq!(
            len_item.get("sortText").and_then(Json::as_str),
            Some("3len")
        );
        assert_eq!(
            len_item.get("detail").and_then(Json::as_str),
            Some(
                vocabulary::builtin("len")
                    .expect("len")
                    .signature()
                    .as_str()
            )
        );
        assert_eq!(
            len_item
                .get("textEdit")
                .and_then(|e| e.get("newText"))
                .and_then(Json::as_str),
            Some("len")
        );
    }

    #[test]
    fn completion_kinds_match_the_protocol_numbers() {
        let text = format!(
            "{} cc = 1;\n{} vv = 1;\n{} fun() {{ }}\n{} Pt {{ x }}\n",
            encode_word("CONST"),
            encode_word("VAR"),
            encode_word("FN"),
            encode_word("STRUCT")
        );
        let mut server = opened(&text);
        let result = query(&mut server, "textDocument/completion", 4, 0);
        let items = items_of(&result);
        let kind = |label: &str| item_named(items, label).get("kind").cloned();
        assert_eq!(kind("cc"), Some(Json::Int(21)), "CONST");
        assert_eq!(kind("vv"), Some(Json::Int(6)), "VAR");
        assert_eq!(kind("fun"), Some(Json::Int(3)), "функция");
        assert_eq!(kind("Pt"), Some(Json::Int(22)), "структура");
        assert_eq!(kind("len"), Some(Json::Int(3)), "builtin");
        assert_eq!(kind("LET"), Some(Json::Int(14)), "ключевое слово");
        // у пунктов без описания нет и поля documentation
        assert!(item_named(items, "cc").get("documentation").is_none());
    }

    #[test]
    fn completion_where_nothing_can_be_typed_is_an_empty_array_not_null() {
        let mut server = opened("\"abc\" // note");
        for character in [2, 10] {
            assert_eq!(
                query(&mut server, "textDocument/completion", 0, character),
                Json::Array(Vec::new()),
                "{character}"
            );
        }
    }

    #[test]
    fn positions_are_utf16_for_astral_characters_in_both_directions() {
        // `"😀" len(1)`: эмодзи занимает две единицы UTF-16
        let mut server = opened("\"😀\" len(1)");
        let hover = query(&mut server, "textDocument/hover", 0, 5);
        assert_eq!(range_of(&hover), ((0, 5), (0, 8)));
        assert_eq!(query(&mut server, "textDocument/hover", 0, 4), Json::Null);
        // внутри суррогатной пары — строка, подсказки нет
        assert_eq!(query(&mut server, "textDocument/hover", 0, 2), Json::Null);

        // диапазон правки после эмодзи: `le` занимает единицы 14..16
        let text = format!("{} s = \"😀\"; le", let_kw());
        let mut server = opened(&text);
        let result = query(&mut server, "textDocument/completion", 0, 16);
        let edit = item_named(items_of(&result), "LET")
            .get("textEdit")
            .expect("textEdit");
        assert_eq!(range_of(edit), ((0, 14), (0, 16)));
    }

    #[test]
    fn crlf_documents_are_addressed_by_line() {
        let mut server = opened("x\r\nlen(1)\r\n");
        let hover = query(&mut server, "textDocument/hover", 1, 0);
        assert_eq!(range_of(&hover), ((1, 0), (1, 3)));
        let items = query(&mut server, "textDocument/completion", 1, 2);
        assert!(items_of(&items)
            .iter()
            .any(|i| i.get("label").and_then(Json::as_str) == Some("len")));
    }

    #[test]
    fn the_next_request_sees_the_latest_document_text() {
        let mut server = opened("len(1)");
        assert!(query(&mut server, "textDocument/hover", 0, 1)
            .get("contents")
            .is_some());
        send(&mut server, &did_change("file:///a.sga", 2, "x(1)"));
        assert_eq!(query(&mut server, "textDocument/hover", 0, 1), Json::Null);
        // отказанное изменение (range) документ не меняет — подсказки прежние
        send(
            &mut server,
            &notify(
                "textDocument/didChange",
                Json::object([
                    (
                        "textDocument",
                        Json::object([
                            ("uri", Json::string("file:///a.sga")),
                            ("version", Json::Int(3)),
                        ]),
                    ),
                    (
                        "contentChanges",
                        Json::Array(vec![Json::object([
                            ("range", Json::Null),
                            ("text", Json::string("len(1)")),
                        ])]),
                    ),
                ]),
            ),
        );
        assert!(query(&mut server, "textDocument/hover", 0, 1)
            .get("contents")
            .is_some());
    }

    #[test]
    fn queries_do_not_publish_anything_and_do_not_touch_the_documents() {
        let mut server = opened("len(1)");
        for method in [
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/definition",
        ] {
            let out = send(
                &mut server,
                &position_request(3, method, "file:///a.sga", 0, 1),
            );
            assert_eq!(out.len(), 1, "{method}");
            assert!(out[0].get("method").is_none(), "ответ, не уведомление");
        }
        let out = send(&mut server, &symbol_request(4, "file:///a.sga"));
        assert_eq!(out.len(), 1, "documentSymbol");
        assert!(out[0].get("method").is_none(), "ответ, не уведомление");
        assert_eq!(server.document_text("file:///a.sga"), Some("len(1)"));
        assert!(server.take_log().is_empty());
    }

    #[test]
    fn the_server_still_answers_queries_after_a_panicking_request() {
        // паника в `route_request` -> -32603 с `id` запроса (общий механизм
        // `guarded`, см. `$/sga/testPanic`); запросы подсказок идут тем же путём
        let mut server = opened("len(1)");
        let out = send(
            &mut server,
            &request(Json::Int(8), "$/sga/testPanic", Json::Null),
        );
        assert_eq!(error_code(&out[0]), -32603);
        assert!(query(&mut server, "textDocument/hover", 0, 1)
            .get("contents")
            .is_some());
    }

    // ── навигация (T019): documentSymbol и definition ────────────────

    /// Заменяет `<<СЛОВО>>` словом из букв SGA.
    fn enc(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some(i) = rest.find("<<") {
            out.push_str(&rest[..i]);
            let tail = &rest[i + 2..];
            let j = tail.find(">>").expect("незакрытый <<");
            out.push_str(&encode_word(&tail[..j]));
            rest = &tail[j + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Клиент, объявивший иерархические символы документа.
    const HIERARCHICAL: &str =
        r#"{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}}"#;

    /// Сервер после `initialize` с заданными `capabilities` клиента (текст
    /// JSON) и открытым документом `file:///a.sga`.
    fn opened_by_client(capabilities: &str, text: &str) -> Server {
        let mut server = Server::new();
        let params = parse_msg(&format!(
            r#"{{"processId":null,"capabilities":{capabilities}}}"#
        ));
        let out = send(&mut server, &request(Json::Int(1), "initialize", params));
        assert!(out[0].get("result").is_some(), "{}", out[0]);
        send(&mut server, &notify("initialized", Json::object([])));
        send(&mut server, &did_open("file:///a.sga", 1, text));
        server
    }

    /// Диапазон `start..end` в виде JSON-текста (для ожидаемых ответов).
    fn rng(from: (i64, i64), to: (i64, i64)) -> String {
        format!(
            r#"{{"start":{{"line":{},"character":{}}},"end":{{"line":{},"character":{}}}}}"#,
            from.0, from.1, to.0, to.1
        )
    }

    /// Файл из структуры, функции и переменной на пяти строках.
    fn outline_source() -> String {
        enc("<<STRUCT>> P { x, y }\n<<FN>> f(a, <<MUT>> b: int) -> int {\n    <<RETURN>> a;\n}\n<<LET>> v = 1;")
    }

    #[test]
    fn a_hierarchical_client_gets_document_symbols_with_children() {
        let mut server = opened_by_client(HIERARCHICAL, &outline_source());
        let expected = format!(
            r#"[{{"name":"P","kind":23,"range":{p},"selectionRange":{pn},"children":[{{"name":"x","kind":8,"range":{x},"selectionRange":{x}}},{{"name":"y","kind":8,"range":{y},"selectionRange":{y}}}]}},{{"name":"f","detail":"(a, MUT b: int) -> int","kind":12,"range":{f},"selectionRange":{fname}}},{{"name":"v","detail":"LET","kind":13,"range":{v},"selectionRange":{vname}}}]"#,
            p = rng((0, 0), (0, 17)),
            pn = rng((0, 7), (0, 8)),
            x = rng((0, 11), (0, 12)),
            y = rng((0, 14), (0, 15)),
            f = rng((1, 0), (3, 1)),
            fname = rng((1, 3), (1, 4)),
            v = rng((4, 0), (4, 10)),
            vname = rng((4, 4), (4, 5)),
        );
        assert_eq!(symbols_of(&mut server), parse_msg(&expected));
    }

    #[test]
    fn a_client_without_hierarchy_gets_flat_symbol_information() {
        let flat = |capabilities: &str| {
            let mut server = opened_by_client(capabilities, &outline_source());
            symbols_of(&mut server)
        };
        let uri = "file:///a.sga";
        let expected = format!(
            r#"[{{"name":"P","kind":23,"location":{{"uri":"{uri}","range":{p}}}}},{{"name":"x","kind":8,"location":{{"uri":"{uri}","range":{x}}},"containerName":"P"}},{{"name":"y","kind":8,"location":{{"uri":"{uri}","range":{y}}},"containerName":"P"}},{{"name":"f","kind":12,"location":{{"uri":"{uri}","range":{f}}}}},{{"name":"v","kind":13,"location":{{"uri":"{uri}","range":{v}}}}}]"#,
            p = rng((0, 0), (0, 17)),
            x = rng((0, 11), (0, 12)),
            y = rng((0, 14), (0, 15)),
            f = rng((1, 0), (3, 1)),
            v = rng((4, 0), (4, 10)),
        );
        let expected = parse_msg(&expected);
        // клиент не сообщил о возможностях вовсе, сообщил пустой набор и
        // явно отказался от иерархии — во всех случаях плоский список
        assert_eq!(flat("{}"), expected);
        assert_eq!(
            flat(
                r#"{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":false}}}"#
            ),
            expected
        );
        let mut plain = opened(&outline_source());
        assert_eq!(symbols_of(&mut plain), expected);
    }

    #[test]
    fn the_hierarchy_flag_is_true_only_for_an_explicit_boolean_true() {
        let cases = [
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}}}"#,
                true,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":false}}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":"true"}}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":1}}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":null}}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":[true]}}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":{}}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":true}}}"#,
                false,
            ),
            (
                r#"{"capabilities":{"textDocument":{"documentSymbol":null}}}"#,
                false,
            ),
            (r#"{"capabilities":{"textDocument":{}}}"#, false),
            (r#"{"capabilities":{"textDocument":"x"}}"#, false),
            (r#"{"capabilities":{}}"#, false),
            (r#"{"capabilities":[]}"#, false),
            (r#"{"capabilities":null}"#, false),
            (r#"{"processId":null}"#, false),
            (r#"{}"#, false),
            (r#"[]"#, false),
            (r#"null"#, false),
            (r#"7"#, false),
        ];
        for (params, expected) in cases {
            assert_eq!(
                client_wants_hierarchical_symbols(Some(&parse_msg(params))),
                expected,
                "{params}"
            );
        }
        assert!(!client_wants_hierarchical_symbols(None));
    }

    #[test]
    fn the_symbol_form_is_fixed_by_initialize_and_survives_later_edits() {
        let mut server = opened_by_client(HIERARCHICAL, "");
        assert_eq!(symbols_of(&mut server), Json::Array(vec![]));
        send(
            &mut server,
            &did_change("file:///a.sga", 2, &enc("<<STRUCT>> P { x }")),
        );
        let symbols = symbols_of(&mut server);
        let list = items_of(&symbols);
        assert_eq!(list.len(), 1, "{symbols}");
        assert_eq!(
            list[0]
                .get("children")
                .and_then(Json::as_array)
                .map(<[Json]>::len),
            Some(1),
            "иерархия остаётся и после didChange: {symbols}"
        );
        assert!(
            list[0].get("location").is_none(),
            "это DocumentSymbol, а не SymbolInformation"
        );
        // повторный initialize ничего не меняет (отказ), форма прежняя
        let again = send(&mut server, &initialize_msg(9));
        assert_eq!(error_code(&again[0]), -32600);
        assert!(items_of(&symbols_of(&mut server))[0]
            .get("children")
            .is_some());
    }

    #[test]
    fn symbol_children_are_omitted_when_there_are_none() {
        let mut server = opened_by_client(HIERARCHICAL, &enc("<<STRUCT>> E { }\n<<FN>> f() { }"));
        let symbols = symbols_of(&mut server);
        let list = items_of(&symbols);
        assert_eq!(list.len(), 2, "{symbols}");
        for symbol in list {
            assert!(symbol.get("children").is_none(), "{symbol}");
        }
        assert!(list[0].get("detail").is_none(), "у структуры detail нет");
        assert_eq!(list[1].get("detail").and_then(Json::as_str), Some("()"));
    }

    #[test]
    fn symbol_columns_are_utf16_code_units_and_survive_crlf() {
        // эмодзи в строке — два кодовых блока UTF-16 при одном символе;
        // вторая переменная начинается правее на один блок, чем «в символах»
        let text = enc("<<LET>> s = \"😀\"; <<LET>> t = 2;\r\n<<LET>> u = 3;\r\n");
        let mut server = opened_by_client(HIERARCHICAL, &text);
        let expected = format!(
            r#"[{{"name":"s","detail":"LET","kind":13,"range":{s},"selectionRange":{sn}}},{{"name":"t","detail":"LET","kind":13,"range":{t},"selectionRange":{tn}}},{{"name":"u","detail":"LET","kind":13,"range":{u},"selectionRange":{un}}}]"#,
            s = rng((0, 0), (0, 13)),
            sn = rng((0, 4), (0, 5)),
            t = rng((0, 14), (0, 24)),
            tn = rng((0, 18), (0, 19)),
            u = rng((1, 0), (1, 10)),
            un = rng((1, 4), (1, 5)),
        );
        assert_eq!(symbols_of(&mut server), parse_msg(&expected));
    }

    #[test]
    fn definition_is_one_location_in_the_same_document() {
        let text = enc("<<LET>> x = 1;\n<<PRINT>>(x);");
        let mut server = opened(&text);
        let expected = format!(
            r#"{{"uri":"file:///a.sga","range":{}}}"#,
            rng((0, 4), (0, 5))
        );
        // использование и сам объявляющий идентификатор
        for (line, character) in [(1, 6), (1, 7), (0, 4), (0, 5)] {
            assert_eq!(
                query(&mut server, "textDocument/definition", line, character),
                parse_msg(&expected),
                "{line}:{character}"
            );
        }
        for (line, character) in [(1, 5), (1, 8), (0, 3), (0, 6), (0, 0)] {
            assert_eq!(
                query(&mut server, "textDocument/definition", line, character),
                Json::Null,
                "{line}:{character}"
            );
        }
    }

    #[test]
    fn definition_columns_are_utf16_and_crlf_safe() {
        // цель стоит правее эмодзи (два блока UTF-16), использование — на
        // следующей строке после CRLF
        let text = enc("<<LET>> s = \"😀\"; <<LET>> t = 2;\r\n<<PRINT>>(t);\r\n");
        let mut server = opened(&text);
        let expected = format!(
            r#"{{"uri":"file:///a.sga","range":{}}}"#,
            rng((0, 18), (0, 19))
        );
        assert_eq!(
            query(&mut server, "textDocument/definition", 1, 6),
            parse_msg(&expected)
        );
        // и в обратную сторону: курсор на цели после эмодзи в координатах UTF-16
        assert_eq!(
            query(&mut server, "textDocument/definition", 0, 18),
            parse_msg(&expected)
        );
        // курсор «внутри» эмодзи слова не задевает
        assert_eq!(
            query(&mut server, "textDocument/definition", 0, 10),
            Json::Null
        );
    }

    #[test]
    fn definition_works_on_text_that_does_not_parse() {
        // лишний `)` и незавершённая строка ниже: парсер здесь ничего не даст,
        // а переход по уже набранному обязан работать
        let text = enc("<<FN>> f(a) { <<RETURN>> a; }\n<<LET>> r = f(1)) +\n<<LET>> q = r");
        let mut server = opened(&text);
        let call = query(&mut server, "textDocument/definition", 1, 9);
        assert_eq!(
            call,
            parse_msg(&format!(
                r#"{{"uri":"file:///a.sga","range":{}}}"#,
                rng((0, 3), (0, 4))
            ))
        );
        let value = query(&mut server, "textDocument/definition", 2, 13);
        assert_eq!(
            value,
            parse_msg(&format!(
                r#"{{"uri":"file:///a.sga","range":{}}}"#,
                rng((1, 4), (1, 5))
            ))
        );
    }

    #[test]
    fn navigation_follows_the_latest_text_and_unknown_names_give_null() {
        let mut server = opened(&enc("<<LET>> a = 1;\n<<PRINT>>(a);"));
        assert!(query(&mut server, "textDocument/definition", 1, 7)
            .get("range")
            .is_some());
        send(
            &mut server,
            &did_change("file:///a.sga", 2, &enc("<<PRINT>>(a);")),
        );
        assert_eq!(
            query(&mut server, "textDocument/definition", 0, 7),
            Json::Null,
            "после правки объявления нет"
        );
        assert_eq!(symbols_of(&mut server), Json::Array(vec![]));
    }

    #[test]
    fn extra_document_symbol_params_are_tolerated_and_a_position_is_not_required() {
        let mut server = opened(&enc("<<LET>> a = 1;"));
        let params = Json::object([
            (
                "textDocument",
                Json::object([
                    ("uri", Json::string("file:///a.sga")),
                    ("version", Json::Int(3)),
                ]),
            ),
            ("workDoneToken", Json::Int(5)),
            ("partialResultToken", Json::string("p")),
            ("position", Json::Int(0)),
        ]);
        let out = send(
            &mut server,
            &request(Json::string("s-1"), "textDocument/documentSymbol", params),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("id"), Some(&Json::string("s-1")));
        assert_eq!(
            out[0]
                .get("result")
                .and_then(Json::as_array)
                .map(<[Json]>::len),
            Some(1),
            "{}",
            out[0]
        );
    }

    #[test]
    fn malformed_document_symbol_params_are_invalid_params_with_the_same_id() {
        let uri = Json::object([("uri", Json::string("file:///a.sga"))]);
        let bad_params: Vec<(&str, Json)> = vec![
            ("null", Json::Null),
            ("массив", Json::Array(vec![])),
            ("число", Json::Int(1)),
            ("пустой объект", Json::object([])),
            (
                "нет textDocument",
                Json::object([("position", Json::object([]))]),
            ),
            (
                "textDocument не объект",
                Json::object([("textDocument", Json::string("file:///a.sga"))]),
            ),
            (
                "textDocument без uri",
                Json::object([("textDocument", Json::object([]))]),
            ),
            (
                "uri не строка",
                Json::object([("textDocument", Json::object([("uri", Json::Int(1))]))]),
            ),
            (
                "uri null",
                Json::object([("textDocument", Json::object([("uri", Json::Null)]))]),
            ),
        ];
        let mut server = opened("len(1)");
        for (what, params) in &bad_params {
            let out = send(
                &mut server,
                &request(
                    Json::string("id-ü"),
                    "textDocument/documentSymbol",
                    params.clone(),
                ),
            );
            assert_eq!(out.len(), 1, "{what}");
            assert_eq!(error_code(&out[0]), -32602, "{what}");
            assert_eq!(out[0].get("id"), Some(&Json::string("id-ü")), "{what}");
            assert!(out[0].get("result").is_none(), "{what}");
        }
        // запрос без params вообще
        let msg = Json::object([
            ("jsonrpc", Json::string("2.0")),
            ("id", Json::Int(5)),
            ("method", Json::string("textDocument/documentSymbol")),
        ])
        .to_string();
        assert_eq!(error_code(&send(&mut server, &msg)[0]), -32602);
        // корректный запрос сразу после ошибок отвечает
        let ok = send(
            &mut server,
            &request(
                Json::Int(6),
                "textDocument/documentSymbol",
                Json::object([("textDocument", uri)]),
            ),
        );
        assert_eq!(ok[0].get("result"), Some(&Json::Array(vec![])));
    }

    /// Наибольшая глубина дерева `DocumentSymbol` и общее число символов.
    fn symbol_tree_stats(list: &[Json]) -> (usize, usize) {
        let mut deepest = 0;
        let mut total = 0;
        let mut level: Vec<&Json> = list.iter().collect();
        while !level.is_empty() {
            deepest += 1;
            total += level.len();
            level = level
                .iter()
                .filter_map(|symbol| symbol.get("children").and_then(Json::as_array))
                .flatten()
                .collect();
        }
        (deepest, total)
    }

    #[test]
    fn a_ladder_of_unclosed_declarations_gives_a_bounded_answer() {
        // 5 000 вложенных `FN a() {` без закрывающих скобок: ответ перечисляет
        // все объявления, но дерево не глубже `MAX_SYMBOL_DEPTH` — и
        // сериализация, и освобождение памяти обходятся без глубокой рекурсии
        let text = enc(&"<<FN>> a() {\n".repeat(5000));
        let mut server = opened_by_client(HIERARCHICAL, &text);
        let symbols = symbols_of(&mut server);
        let (deepest, total) = symbol_tree_stats(items_of(&symbols));
        assert_eq!(total, 5000);
        assert!(deepest <= nav::MAX_SYMBOL_DEPTH, "глубина {deepest}");
        assert!(deepest > 1, "вложенность всё же передаётся");
        assert!(symbols.to_string().len() > 5000);

        let mut flat = opened(&text);
        let listed = symbols_of(&mut flat);
        let list = items_of(&listed);
        assert_eq!(list.len(), 5000);
        assert!(list[0].get("containerName").is_none());
        assert_eq!(
            list[1].get("containerName").and_then(Json::as_str),
            Some("a")
        );
    }
}
