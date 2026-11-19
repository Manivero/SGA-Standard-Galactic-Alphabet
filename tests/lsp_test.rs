//! Сквозные тесты языкового сервера `sga lsp` (T016, M007): настоящий
//! бинарник по pipe'ам.
//!
//! Фреймы читаются СОБСТВЕННЫМ разборщиком этого файла, а не
//! `sga::lsp::transport`: если бы читатель и писатель сервера ошибались
//! одинаково, юнит-тесты этого не заметили бы. Любой байт stdout вне фрейма
//! `Content-Length: N\r\n\r\n<N байт>` — провал теста. Все ожидания
//! ограничены таймаутами: зависший сервер даёт падение теста, а не
//! бесконечное ожидание.
//!
//! Ограничение среды (записано явно): настоящий VS Code здесь не запускается,
//! клиентом служит сам тест; поведение клиента расширения — задача T020.

use sga::lsp::json::{self, Json};
use sga::sga_alphabet::encode_word as kw;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Предел ожидания одного шага (ответ сервера, завершение процесса).
const STEP: Duration = Duration::from_secs(30);

/// Предел для шагов с огромными документами (отладочная сборка медленнее).
const HUGE_STEP: Duration = Duration::from_secs(240);

// ── независимый разбор stdout сервера ────────────────────────────────

enum Item {
    /// Тело фрейма (UTF-8).
    Frame(String),
    /// Байты, не являющиеся фреймом: сервер нарушил правило «в stdout только
    /// фреймы».
    Stray(String),
    /// stdout закрыт на границе фреймов.
    End,
}

fn stray(tx: &Sender<Item>, what: impl Into<String>) {
    let _ = tx.send(Item::Stray(what.into()));
}

/// Читает stdout: строго `Content-Length: <цифры>\r\n\r\n` + тело ровно
/// такой длины. Первое же отклонение — `Item::Stray` и конец чтения.
fn read_frames(stdout: impl Read, tx: Sender<Item>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length: Option<usize> = None;
        let mut first_line = true;
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) if first_line => {
                    let _ = tx.send(Item::End);
                    return;
                }
                Ok(0) => return stray(&tx, "конец потока посреди заголовков"),
                Ok(_) => {}
                Err(e) => return stray(&tx, format!("ошибка чтения stdout: {e}")),
            }
            first_line = false;
            if line == b"\r\n" {
                break;
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            let digits = text
                .strip_suffix("\r\n")
                .and_then(|l| l.strip_prefix("Content-Length: "));
            match digits {
                Some(d)
                    if length.is_none()
                        && !d.is_empty()
                        && d.len() < 10
                        && d.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    length = d.parse().ok();
                }
                _ => return stray(&tx, format!("недопустимая строка заголовка {text:?}")),
            }
        }
        let Some(length) = length else {
            return stray(&tx, "заголовки без Content-Length");
        };
        let mut body = vec![0u8; length];
        if let Err(e) = reader.read_exact(&mut body) {
            return stray(&tx, format!("тело фрейма оборвано: {e}"));
        }
        match String::from_utf8(body) {
            Ok(text) => {
                if tx.send(Item::Frame(text)).is_err() {
                    return;
                }
            }
            Err(_) => return stray(&tx, "тело фрейма не UTF-8"),
        }
    }
}

// ── запущенный сервер ────────────────────────────────────────────────

struct Exit {
    /// Код возврата; `None` — процесс убит сигналом (например, `abort`
    /// при переполнении стека).
    code: Option<i32>,
    stderr: String,
    /// Фреймы, которые тест не прочитал.
    rest: Vec<String>,
}

struct Lsp {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Item>,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<String>>,
    exit_code: Option<Option<i32>>,
}

impl Lsp {
    fn start(args: &[&str]) -> Lsp {
        Lsp::start_in(args, None)
    }

    fn start_in(args: &[&str], dir: Option<&Path>) -> Lsp {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sga"));
        command
            .arg("lsp")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let mut child = command.spawn().expect("не удалось запустить `sga lsp`");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let mut stderr = child.stderr.take().expect("stderr");
        let (tx, frames) = mpsc::channel();
        let reader = std::thread::spawn(move || read_frames(stdout, tx));
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Lsp {
            child,
            stdin,
            frames,
            reader: Some(reader),
            stderr: Some(stderr),
            exit_code: None,
        }
    }

    /// Записывает байты в stdin сервера; `false` — сервер уже закрыл pipe.
    fn try_send_raw(&mut self, bytes: &[u8]) -> bool {
        let Some(stdin) = self.stdin.as_mut() else {
            return false;
        };
        stdin.write_all(bytes).and_then(|()| stdin.flush()).is_ok()
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        assert!(self.try_send_raw(bytes), "сервер закрыл stdin раньше срока");
    }

    fn send(&mut self, body: &str) {
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        self.send_raw(&frame);
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn recv_within(&mut self, timeout: Duration) -> Json {
        match self.frames.recv_timeout(timeout) {
            Ok(Item::Frame(body)) => json::parse(&body)
                .unwrap_or_else(|e| panic!("сервер прислал не JSON ({e}): {body}")),
            Ok(Item::Stray(what)) => panic!("в stdout попало постороннее: {what}"),
            Ok(Item::End) => panic!("stdout закрыт, а сообщение ожидалось"),
            Err(RecvTimeoutError::Timeout) => {
                panic!("таймаут {timeout:?}: сообщение от сервера не пришло")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("читатель stdout завершился"),
        }
    }

    fn recv(&mut self) -> Json {
        self.recv_within(STEP)
    }

    /// Ждёт завершения процесса, НЕ закрывая stdin (сервер, который не
    /// вышел сам, а ждёт ввода, — провал). `None` не бывает: по таймауту
    /// процесс убивается и тест падает.
    fn wait_within(&mut self, timeout: Duration) -> Option<i32> {
        if let Some(code) = self.exit_code {
            return code;
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => {
                    self.exit_code = Some(status.code());
                    return status.code();
                }
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("сервер не завершился за {timeout:?}");
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn wait(&mut self) -> Option<i32> {
        self.wait_within(STEP)
    }

    /// Закрывает stdin, дожидается завершения и собирает остатки.
    fn finish(mut self) -> Exit {
        self.finish_within(STEP)
    }

    fn finish_within(&mut self, timeout: Duration) -> Exit {
        self.close_stdin();
        let code = self.wait_within(timeout);
        if let Some(reader) = self.reader.take() {
            reader.join().expect("читатель stdout");
        }
        let stderr = self
            .stderr
            .take()
            .map(|t| t.join().expect("читатель stderr"))
            .unwrap_or_default();
        let mut rest = Vec::new();
        while let Ok(item) = self.frames.try_recv() {
            match item {
                Item::Frame(body) => rest.push(body),
                Item::Stray(what) => panic!("в stdout попало постороннее: {what}"),
                Item::End => {}
            }
        }
        Exit { code, stderr, rest }
    }

    /// `initialize` + `initialized`, с проверкой ответа.
    fn initialize(&mut self) {
        self.initialize_with("{}");
    }

    /// То же с возможностями клиента (`capabilities`) в JSON.
    fn initialize_with(&mut self, capabilities: &str) {
        self.send(&request(
            1,
            "initialize",
            &format!(r#"{{"processId":null,"capabilities":{capabilities}}}"#),
        ));
        let reply = self.recv();
        assert_eq!(reply.get("id"), Some(&Json::Int(1)), "{reply}");
        assert!(reply.get("result").is_some(), "{reply}");
        self.send(&notification("initialized", "{}"));
    }

    /// `shutdown` с проверкой ответа `null`.
    fn shutdown(&mut self, id: i64) {
        self.send(&request(id, "shutdown", "null"));
        let reply = self.recv();
        assert_eq!(reply.get("id"), Some(&Json::Int(id)), "{reply}");
        assert_eq!(reply.get("result"), Some(&Json::Null), "{reply}");
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        if self.exit_code.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ── сообщения ────────────────────────────────────────────────────────

fn quoted(text: &str) -> String {
    Json::string(text).to_string()
}

fn request(id: i64, method: &str, params: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#)
}

fn notification(method: &str, params: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params}}}"#)
}

fn did_open(uri: &str, version: i64, text: &str) -> String {
    notification(
        "textDocument/didOpen",
        &format!(
            r#"{{"textDocument":{{"uri":{},"languageId":"sga","version":{version},"text":{}}}}}"#,
            quoted(uri),
            quoted(text)
        ),
    )
}

fn did_change(uri: &str, version: i64, text: &str) -> String {
    notification(
        "textDocument/didChange",
        &format!(
            r#"{{"textDocument":{{"uri":{},"version":{version}}},"contentChanges":[{{"text":{}}}]}}"#,
            quoted(uri),
            quoted(text)
        ),
    )
}

fn did_close(uri: &str) -> String {
    notification(
        "textDocument/didClose",
        &format!(r#"{{"textDocument":{{"uri":{}}}}}"#, quoted(uri)),
    )
}

/// Параметры уведомления `publishDiagnostics` или провал теста.
fn publish_params(message: &Json) -> &Json {
    assert_eq!(
        message.get("method").and_then(Json::as_str),
        Some("textDocument/publishDiagnostics"),
        "ожидалась публикация диагностик: {message}"
    );
    assert!(message.get("id").is_none(), "уведомление без id: {message}");
    message.get("params").expect("params")
}

fn diagnostics_of(params: &Json) -> &[Json] {
    params
        .get("diagnostics")
        .and_then(Json::as_array)
        .expect("diagnostics")
}

/// `(строка начала, символ начала, строка конца, символ конца)`, с нуля.
fn range_of(diagnostic: &Json) -> (i64, i64, i64, i64) {
    let range = diagnostic.get("range").expect("range");
    let number = |side: &str, field: &str| {
        range
            .get(side)
            .and_then(|p| p.get(field))
            .and_then(Json::as_i64)
            .unwrap_or_else(|| panic!("нет range.{side}.{field}: {diagnostic}"))
    };
    (
        number("start", "line"),
        number("start", "character"),
        number("end", "line"),
        number("end", "character"),
    )
}

fn error_code(message: &Json) -> i64 {
    message
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("в сообщении нет error.code: {message}"))
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sga_lsp_test_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("временный каталог");
    dir
}

// ── сценарии ─────────────────────────────────────────────────────────

#[test]
fn a_whole_session_runs_to_a_clean_exit_with_code_zero() {
    let mut lsp = Lsp::start(&[]);
    lsp.send(&request(
        1,
        "initialize",
        r#"{"processId":null,"capabilities":{}}"#,
    ));
    let reply = lsp.recv();
    assert_eq!(reply.get("id"), Some(&Json::Int(1)));
    let result = reply.get("result").expect("result");
    assert_eq!(
        result.get("capabilities"),
        Some(
            &json::parse(
                r#"{"textDocumentSync":{"openClose":true,"change":1},"hoverProvider":true,"completionProvider":{"resolveProvider":false},"definitionProvider":true,"documentSymbolProvider":true}"#
            )
            .unwrap()
        ),
        "объявляться должны ровно синхронизация документов, hover, автодополнение, \
         переход к определению и символы документа: {reply}"
    );
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(Json::as_str),
        Some("sga-lsp")
    );
    lsp.send(&notification("initialized", "{}"));

    // документ с лексической ошибкой
    let uri = "file:///work/a.sga";
    lsp.send(&did_open(uri, 1, "x = @;"));
    let published = lsp.recv();
    let params = publish_params(&published);
    assert_eq!(params.get("uri").and_then(Json::as_str), Some(uri));
    assert_eq!(params.get("version"), Some(&Json::Int(1)));
    let diagnostics = diagnostics_of(params);
    assert_eq!(diagnostics.len(), 1, "{published}");
    let d = &diagnostics[0];
    assert_eq!(range_of(d), (0, 4, 0, 5), "{d}");
    assert_eq!(d.get("severity"), Some(&Json::Int(1)));
    assert_eq!(d.get("code").and_then(Json::as_str), Some("lexer"));
    assert_eq!(d.get("source").and_then(Json::as_str), Some("sga"));
    assert!(
        d.get("message")
            .and_then(Json::as_str)
            .is_some_and(|m| m.contains('@')),
        "{d}"
    );

    // исправленный текст: диагностики исчезают
    lsp.send(&did_change(uri, 2, &format!("{} x = 1;", kw("LET"))));
    let published = lsp.recv();
    let params = publish_params(&published);
    assert_eq!(params.get("version"), Some(&Json::Int(2)));
    assert!(diagnostics_of(params).is_empty(), "{published}");

    // синтаксическая ошибка парсера
    lsp.send(&did_change(uri, 3, &format!("{} 12345 = 1;", kw("LET"))));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("parser")
    );
    assert_eq!(range_of(&diagnostics[0]), (0, 4, 0, 9));

    // закрытие очищает диагностики
    lsp.send(&did_close(uri));
    let published = lsp.recv();
    let params = publish_params(&published);
    assert_eq!(params.get("uri").and_then(Json::as_str), Some(uri));
    assert!(diagnostics_of(params).is_empty(), "{published}");

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.wait(), Some(0), "exit после shutdown -> код 0");
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert_eq!(exit.stderr, "", "штатная сессия ничего не пишет в stderr");
    assert!(exit.rest.is_empty(), "лишние сообщения: {:?}", exit.rest);
}

#[test]
fn the_stdio_flag_is_accepted_and_changes_nothing() {
    let mut lsp = Lsp::start(&["--stdio"]);
    lsp.initialize();
    lsp.send(&did_open("file:///a.sga", 1, "@"));
    let published = lsp.recv();
    assert_eq!(diagnostics_of(publish_params(&published)).len(), 1);
    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert_eq!(exit.stderr, "");
}

#[test]
fn an_unknown_flag_is_a_usage_error_and_no_server_starts() {
    let mut lsp = Lsp::start(&["--bogus"]);
    // stdin остаётся открытым: настоящий сервер ждал бы ввода, а процесс с
    // ошибкой использования обязан завершиться сам и сразу.
    assert_eq!(lsp.wait(), Some(1));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(1));
    assert!(exit.stderr.contains("--bogus"), "{}", exit.stderr);
    assert!(exit.stderr.contains("--stdio"), "{}", exit.stderr);
    assert!(
        exit.stderr.contains("использование: sga lsp"),
        "{}",
        exit.stderr
    );
    assert!(
        exit.rest.is_empty(),
        "сервер не должен ничего писать в stdout"
    );

    // лишний аргумент рядом с допустимым флагом — тоже ошибка
    let mut lsp = Lsp::start(&["--stdio", "extra"]);
    assert_eq!(lsp.wait(), Some(1));
    let exit = lsp.finish();
    assert!(exit.rest.is_empty());
}

#[test]
fn closing_stdin_ends_the_server_with_the_code_by_lifecycle_state() {
    // EOF до всякого сообщения — без shutdown -> 1
    let lsp = Lsp::start(&[]);
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(1));
    assert!(exit.rest.is_empty());

    // после initialize, но без shutdown -> 1
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(1));

    // после shutdown, без exit -> 0
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.shutdown(2);
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert_eq!(exit.stderr, "");

    // exit без shutdown -> 1
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.wait(), Some(1));
}

#[test]
fn a_truncated_body_is_a_framing_error_with_an_explanation() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.send_raw(b"Content-Length: 100\r\n\r\n{\"jsonrpc\":\"2.0\"");
    lsp.close_stdin();
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(1));
    assert!(
        exit.stderr.starts_with("sga-lsp: "),
        "строки stderr с префиксом: {}",
        exit.stderr
    );
    assert!(exit.stderr.contains("ошибка фрейминга"), "{}", exit.stderr);
    assert!(exit.stderr.contains("в теле сообщения"), "{}", exit.stderr);
    assert!(exit.rest.is_empty());
}

#[test]
fn framing_violations_stop_the_server_at_once_with_code_one() {
    let many_headers = "X-A: 1\r\n".repeat(33);
    let long_header = format!("X-Long: {}\r\n", "a".repeat(9000));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("нет Content-Length", b"X-Other: 1\r\n\r\n{}".to_vec()),
        (
            "нечисловой Content-Length",
            b"Content-Length: abc\r\n\r\n".to_vec(),
        ),
        (
            "отрицательный Content-Length",
            b"Content-Length: -5\r\n\r\n".to_vec(),
        ),
        (
            "дубль Content-Length",
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ),
        // 32 МиБ + 1: тело не присылается совсем — сервер обязан отказать,
        // не дожидаясь его (stdin остаётся открытым)
        (
            "Content-Length больше предела",
            b"Content-Length: 33554433\r\n\r\n".to_vec(),
        ),
        ("строка заголовка длиннее 8 КиБ", long_header.into_bytes()),
        (
            "больше 32 заголовков",
            format!("{many_headers}Content-Length: 2\r\n\r\n{{}}").into_bytes(),
        ),
        (
            "строка заголовка без двоеточия",
            b"nonsense\r\n\r\n".to_vec(),
        ),
    ];
    for (name, bytes) in cases {
        let mut lsp = Lsp::start(&[]);
        lsp.send_raw(&bytes);
        assert_eq!(lsp.wait(), Some(1), "случай «{name}»");
        let exit = lsp.finish();
        assert!(
            exit.stderr.contains("ошибка фрейминга"),
            "случай «{name}»: {}",
            exit.stderr
        );
        assert!(
            exit.rest.is_empty(),
            "случай «{name}»: в stdout что-то записано: {:?}",
            exit.rest
        );
    }
}

#[test]
fn bad_messages_get_error_replies_and_the_server_keeps_working() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();

    // не JSON -> -32700, id null
    lsp.send("{");
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32700, "{reply}");
    assert_eq!(reply.get("id"), Some(&Json::Null));

    // JSON, но не объект -> -32600
    lsp.send("[]");
    assert_eq!(error_code(&lsp.recv()), -32600);

    // нет method -> -32600; method не строка -> -32600; id неверного типа -> -32600
    lsp.send(r#"{"jsonrpc":"2.0","id":3}"#);
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32600);
    assert_eq!(reply.get("id"), Some(&Json::Int(3)));
    lsp.send(r#"{"jsonrpc":"2.0","id":4,"method":5}"#);
    assert_eq!(error_code(&lsp.recv()), -32600);
    lsp.send(r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#);
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32600);
    assert_eq!(reply.get("id"), Some(&Json::Null));

    // ответ клиента игнорируется; неизвестное уведомление игнорируется;
    // уведомление неверной формы — только запись в stderr
    lsp.send(r#"{"jsonrpc":"2.0","id":9,"result":null}"#);
    lsp.send(&notification("$/cancelRequest", r#"{"id":1}"#));
    lsp.send(&notification("textDocument/didOpen", "{}"));
    lsp.send(&notification(
        "textDocument/didChange",
        r#"{"textDocument":{}}"#,
    ));

    // неизвестные запросы: -32601 с тем же id (целым и строковым)
    lsp.send(&request(7, "unknown/request", "{}"));
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32601, "{reply}");
    assert_eq!(reply.get("id"), Some(&Json::Int(7)));
    lsp.send(r#"{"jsonrpc":"2.0","id":"abc","method":"unknown/request"}"#);
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32601);
    assert_eq!(reply.get("id"), Some(&Json::string("abc")));

    // хук паники существует только в тестовой сборке библиотеки: в настоящем
    // бинарнике это обычный неизвестный метод / игнорируемое уведомление
    lsp.send(&request(10, "$/sga/testPanic", "null"));
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32601, "{reply}");
    assert_eq!(reply.get("id"), Some(&Json::Int(10)));
    lsp.send(&notification("$/sga/testPanic", "null"));

    // не-UTF-8 тело -> -32700
    lsp.send_raw(b"Content-Length: 4\r\n\r\n\xff\xfe\xfd\xfc");
    assert_eq!(error_code(&lsp.recv()), -32700);

    // сервер по-прежнему отвечает по существу
    lsp.send(&did_open("file:///a.sga", 1, "// привет 😀\n@"));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(range_of(&diagnostics[0]), (1, 0, 1, 1));

    lsp.shutdown(8);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert!(exit.rest.is_empty(), "{:?}", exit.rest);
    // всё, что сервер сообщает о себе, — в stderr и с префиксом
    assert!(!exit.stderr.is_empty());
    for line in exit.stderr.lines() {
        assert!(
            line.starts_with("sga-lsp: "),
            "строка без префикса: {line:?}"
        );
    }
    assert!(
        exit.stderr.contains("некорректные параметры"),
        "{}",
        exit.stderr
    );
}

#[test]
fn lifecycle_rules_hold_over_the_wire() {
    let mut lsp = Lsp::start(&[]);

    // до initialize: запрос -> -32002, уведомление игнорируется
    lsp.send(&request(1, "shutdown", "null"));
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32002, "{reply}");
    assert_eq!(reply.get("id"), Some(&Json::Int(1)));
    lsp.send(&did_open("file:///a.sga", 1, "@"));

    lsp.send(&request(2, "initialize", r#"{"processId":null}"#));
    let reply = lsp.recv();
    assert_eq!(
        reply.get("id"),
        Some(&Json::Int(2)),
        "следующим сообщением должен быть ответ initialize, а не публикация \
         диагностик для didOpen до initialize: {reply}"
    );
    assert!(reply.get("result").is_some());

    // повторный initialize -> -32600
    lsp.send(&request(3, "initialize", "{}"));
    assert_eq!(error_code(&lsp.recv()), -32600);

    lsp.shutdown(4);

    // после shutdown: запрос -> -32600, уведомления игнорируются
    lsp.send(&request(5, "textDocument/hover", "{}"));
    assert_eq!(error_code(&lsp.recv()), -32600);
    lsp.send(&did_open("file:///b.sga", 1, "@"));
    lsp.send(&request(6, "shutdown", "null"));
    let reply = lsp.recv();
    assert_eq!(
        reply.get("id"),
        Some(&Json::Int(6)),
        "didOpen после shutdown не должен ничего публиковать: {reply}"
    );
    assert_eq!(error_code(&reply), -32600);

    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.wait(), Some(0));
}

#[test]
fn diagnostic_positions_are_utf16_code_units() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();

    // символ вне BMP внутри строки ПЕРЕД ошибкой — две единицы UTF-16:
    // `x = "😀"; @` — ошибка на 10-м символе, но на 11-й единице UTF-16
    lsp.send(&did_open("file:///emoji.sga", 1, "x = \"😀\"; @"));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(range_of(&diagnostics[0]), (0, 10, 0, 11), "{published}");

    // слово из SGA-букв — по одной единице UTF-16 на букву (BMP)
    lsp.send(&did_open(
        "file:///glyphs.sga",
        1,
        &format!("{} x = 1; @", kw("LET")),
    ));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(range_of(&diagnostics[0]), (0, 11, 0, 12), "{published}");

    // CRLF: ошибка на второй строке
    lsp.send(&did_open("file:///crlf.sga", 1, "x = 1;\r\ny = @;"));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(range_of(&diagnostics[0]), (1, 4, 1, 5), "{published}");

    // конец текста: `Eof` после завершающего перевода строки — пустой
    // диапазон на последней, пустой строке
    lsp.send(&did_open(
        "file:///eof.sga",
        1,
        &format!("{} x\n", kw("LET")),
    ));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("parser")
    );
    assert_eq!(range_of(&diagnostics[0]), (1, 0, 1, 0), "{published}");

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.finish().code, Some(0));
}

#[test]
fn two_documents_are_independent() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.send(&did_open("file:///a.sga", 1, "@"));
    let a = lsp.recv();
    assert_eq!(diagnostics_of(publish_params(&a)).len(), 1);
    lsp.send(&did_open(
        "file:///b.sga",
        1,
        &format!("{} x = 1;", kw("LET")),
    ));
    let b = lsp.recv();
    assert_eq!(
        publish_params(&b).get("uri").and_then(Json::as_str),
        Some("file:///b.sga")
    );
    assert!(diagnostics_of(publish_params(&b)).is_empty());

    // правка b не трогает a: публикуется только для b
    lsp.send(&did_change("file:///b.sga", 2, "#"));
    let b2 = lsp.recv();
    assert_eq!(
        publish_params(&b2).get("uri").and_then(Json::as_str),
        Some("file:///b.sga")
    );
    assert_eq!(diagnostics_of(publish_params(&b2)).len(), 1);

    // закрытый документ забыт: didChange для него игнорируется (ответа нет)
    lsp.send(&did_close("file:///a.sga"));
    let closed = lsp.recv();
    assert!(diagnostics_of(publish_params(&closed)).is_empty());
    lsp.send(&did_change("file:///a.sga", 5, "@"));
    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert!(exit.rest.is_empty(), "{:?}", exit.rest);
    assert!(
        exit.stderr.contains("неоткрытого документа"),
        "{}",
        exit.stderr
    );
}

#[test]
fn the_server_only_analyzes_and_never_runs_the_document() {
    // Каталог процесса — пустой временный: любой файл, созданный документом,
    // был бы виден. ОСТОРОЖНО с выводом: запись файлов у программ и так
    // запрещена без `--allow-write`, поэтому отсутствие `pwned.txt` само по
    // себе мало что доказывает. Настоящие свидетельства — три других: `PRINT`
    // документа не появляется в stdout (читатель фреймов считает любой
    // лишний байт провалом), `read_stdin()` не «съедает» ввод LSP (следующее
    // сообщение обрабатывается) и бесконечный цикл не вешает сервер.
    let dir = fresh_dir("analysis_only");
    let mut lsp = Lsp::start_in(&[], Some(&dir));
    lsp.initialize();

    let program = format!(
        "{print}(\"маркер-из-документа\");\n\
         write_file(\"pwned.txt\", \"x\");\n\
         {let_kw} s = read_stdin();\n\
         {while_kw} {true_kw} {{ }}\n",
        print = kw("PRINT"),
        let_kw = kw("LET"),
        while_kw = kw("WHILE"),
        true_kw = kw("TRUE"),
    );
    let uri = "file:///danger.sga";
    lsp.send(&did_open(uri, 1, &program));
    let published = lsp.recv();
    assert!(
        diagnostics_of(publish_params(&published)).is_empty(),
        "синтаксически документ корректен: {published}"
    );

    // сервер жив, не завис в цикле и ввод не потреблён: ответ на правку
    lsp.send(&did_change(uri, 2, &format!("{program}@")));
    let published = lsp.recv();
    assert_eq!(diagnostics_of(publish_params(&published)).len(), 1);

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert_eq!(exit.stderr, "");
    assert!(exit.rest.is_empty(), "{:?}", exit.rest);
    let created: Vec<_> = std::fs::read_dir(&dir)
        .expect("каталог")
        .map(|e| e.expect("запись").file_name())
        .collect();
    std::fs::remove_dir_all(&dir).ok();
    assert!(created.is_empty(), "документ создал файлы: {created:?}");
}

#[test]
fn a_document_of_half_a_million_operators_gets_a_depth_warning_without_crashing() {
    // ≈1 МБ текста `LET x = 1+1+…+1;` — на пределе размера документа. AST —
    // цепочка глубиной 500 000 узлов: парсер строит её циклом, а освобождается
    // она рекурсивно, поэтому анализ идёт в потоке с большим стеком (TD-018).
    // semantic и типы такое дерево не получают (T017: предел глубины), вместо
    // них — одно предупреждение на весь оператор. Падение процесса
    // (переполнение стека -> abort -> код `None`) — провал.
    let mut text = format!("{} x = 1", kw("LET"));
    text.push_str(&"+1".repeat(500_000));
    text.push(';');
    assert!(text.len() <= 1024 * 1024, "{}", text.len());

    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.send(&did_open("file:///big.sga", 1, &text));
    let published = lsp.recv_within(HUGE_STEP);
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{}", diagnostics.len());
    assert_eq!(diagnostics[0].get("severity"), Some(&Json::Int(2)));
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("limit")
    );
    let length = text.chars().count() as i64;
    assert_eq!(range_of(&diagnostics[0]), (0, 0, 0, length));

    // и ошибка в конце такого документа находится
    text.pop();
    text.push_str("@;");
    lsp.send(&did_change("file:///big.sga", 2, &text));
    let published = lsp.recv_within(HUGE_STEP);
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{}", diagnostics.len());
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("lexer")
    );

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish_within(HUGE_STEP);
    assert_eq!(exit.code, Some(0), "stderr: {}", exit.stderr);
}

#[test]
fn semantic_and_type_errors_are_published_over_the_wire_with_utf16_ranges() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    let uri = "file:///names.sga";

    // необъявленное имя после эмодзи в той же строке: `b` — 19-й символ (с
    // нуля), 20-я единица UTF-16
    let text = format!("{} s = \"😀\"; {}(b);", kw("LET"), kw("PRINT"));
    lsp.send(&did_open(uri, 1, &text));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    let d = &diagnostics[0];
    assert_eq!(d.get("severity"), Some(&Json::Int(1)), "{d}");
    assert_eq!(d.get("code").and_then(Json::as_str), Some("semantic"));
    assert_eq!(d.get("source").and_then(Json::as_str), Some("sga"));
    assert_eq!(range_of(d), (0, 20, 0, 21), "{d}");
    assert_eq!(
        d.get("message").and_then(Json::as_str),
        Some("неопределённая переменная 'b'")
    );

    // ошибка типов: строковый литерал с эмодзи внутри — диапазон в единицах
    // UTF-16 (кавычки + две единицы эмодзи)
    let text = format!("{} x: int = \"😀\";", kw("LET"));
    lsp.send(&did_change(uri, 2, &text));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("type")
    );
    assert_eq!(diagnostics[0].get("severity"), Some(&Json::Int(1)));
    assert_eq!(range_of(&diagnostics[0]), (0, 13, 0, 17), "{published}");

    // многострочный узел: литерал структуры на трёх строках
    let text = format!(
        "{} P {{ x }}\n{} p = P {{\n    y: 1\n}};\n",
        kw("STRUCT"),
        kw("LET")
    );
    lsp.send(&did_change(uri, 3, &text));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(range_of(&diagnostics[0]), (1, 8, 3, 1), "{published}");

    // приоритет стадий: ошибка типов раньше в тексте, но semantic — раньше
    // по порядку стадий
    let text = format!("{} x: int = \"s\";\n{}(b);", kw("LET"), kw("PRINT"));
    lsp.send(&did_change(uri, 4, &text));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("semantic")
    );
    assert_eq!(range_of(&diagnostics[0]), (1, 6, 1, 7));

    // исправленный текст — диагностик нет
    let text = format!("{} x: int = 1;\n{}(x);", kw("LET"), kw("PRINT"));
    lsp.send(&did_change(uri, 5, &text));
    let published = lsp.recv();
    assert!(diagnostics_of(publish_params(&published)).is_empty());

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0));
    assert_eq!(exit.stderr, "");
}

#[test]
fn a_document_with_import_gets_one_information_notice_over_the_wire() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    let uri = "file:///with_import.sga";

    // Имя из подключаемого модуля и ошибка типов ниже: без разрешения импортов
    // их показать нельзя, остаётся одно замечание на первом IMPORT.
    let text = format!(
        "{}(1);\n  {import} \"math.sga\";\n{import} \"other.sga\";\n{} x: int = \"s\";\n{}(from_math);",
        kw("PRINT"),
        kw("LET"),
        kw("PRINT"),
        import = kw("IMPORT"),
    );
    lsp.send(&did_open(uri, 1, &text));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    let d = &diagnostics[0];
    assert_eq!(d.get("severity"), Some(&Json::Int(3)), "Information: {d}");
    assert_eq!(d.get("code").and_then(Json::as_str), Some("limit"));
    assert_eq!(range_of(d), (1, 2, 1, 20), "первый IMPORT: {d}");
    assert!(
        d.get("message")
            .and_then(Json::as_str)
            .is_some_and(|m| m.contains("IMPORT")),
        "{d}"
    );

    // синтаксическая ошибка важнее замечания
    lsp.send(&did_change(uri, 2, &format!("{text}\n@")));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("lexer")
    );

    // без IMPORT тот же текст анализируется полностью: имя из «модуля» теперь
    // необъявленное (ровно та ложная ошибка, ради которой замечание об IMPORT
    // подавляет анализ), и semantic находит его раньше ошибки типов
    let without = text.replace(&kw("IMPORT"), "// ");
    lsp.send(&did_change(uri, 3, &without));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("semantic"),
        "{published}"
    );
    assert_eq!(range_of(&diagnostics[0]), (4, 6, 4, 15), "{published}");
    assert_eq!(
        diagnostics[0].get("message").and_then(Json::as_str),
        Some("неопределённая переменная 'from_math'")
    );

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.finish().code, Some(0));
}

#[test]
fn the_depth_limit_decides_between_analysis_and_a_warning_over_the_wire() {
    let limit = sga::diagnostics::MAX_ANALYSIS_DEPTH;
    // `LET x = zz+1+…+1;`: глубина дерева = число слагаемых + 2 (оператор,
    // выражения-суммы и самый глубокий операнд `zz`)
    let chain = |links: usize| format!("{} x = zz{};", kw("LET"), "+1".repeat(links));
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    let uri = "file:///edge.sga";

    // ровно на пределе анализ выполняется и находит необъявленное `zz` на дне
    lsp.send(&did_open(uri, 1, &chain(limit - 2)));
    let published = lsp.recv_within(HUGE_STEP);
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("semantic"),
        "{published}"
    );
    assert_eq!(diagnostics[0].get("severity"), Some(&Json::Int(1)));

    // на одно слагаемое глубже — анализа нет, одно предупреждение
    let deeper = chain(limit - 1);
    lsp.send(&did_change(uri, 2, &deeper));
    let published = lsp.recv_within(HUGE_STEP);
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    let d = &diagnostics[0];
    assert_eq!(d.get("severity"), Some(&Json::Int(2)), "{d}");
    assert_eq!(d.get("code").and_then(Json::as_str), Some("limit"));
    assert_eq!(range_of(d), (0, 0, 0, deeper.chars().count() as i64));
    assert!(
        d.get("message")
            .and_then(Json::as_str)
            .is_some_and(|m| m.contains(&limit.to_string())),
        "{d}"
    );

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish_within(HUGE_STEP);
    assert_eq!(exit.code, Some(0), "stderr: {}", exit.stderr);
    assert_eq!(exit.stderr, "");
}

#[test]
fn one_mebibyte_documents_of_every_deep_construct_do_not_crash_the_server() {
    // По каждой конструкции, у которой парсер не ограничивает длину цепочки
    // (она строится циклом): документ почти в 1 МиБ. semantic и типы не
    // должны получать такое дерево (стек растёт с глубиной), а освобождение
    // дерева идёт на большом стеке потока сервера. Падение процесса
    // (переполнение стека -> abort -> код `None`) — провал.
    const LIMIT: usize = 1024 * 1024;
    let let_kw = kw("LET");
    let chain = |bottom: &str, unit: &str| {
        let head = format!("{let_kw} a = [1]; {let_kw} x = {bottom}");
        let links = (LIMIT - head.len() - 1) / unit.len();
        format!("{head}{};", unit.repeat(links))
    };
    let cases = [
        ("сложение", chain("1", "+1")),
        ("вычитание", chain("1", "-1")),
        ("умножение", chain("1", "*1")),
        ("сравнение", chain("1", "<1")),
        ("индексы", chain("a", "[0]")),
        ("вызовы методов", chain("a", ".m()")),
        ("поля", chain("a", ".b")),
        ("вызовы результата", chain("a", "()")),
    ];
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    for (version, (name, text)) in cases.iter().enumerate() {
        assert!(text.len() <= LIMIT, "{name}: {}", text.len());
        assert!(text.len() > LIMIT - 8, "{name}: документ слишком короткий");
        lsp.send(&did_change_or_open(version as i64, text));
        let published = lsp.recv_within(HUGE_STEP);
        let diagnostics = diagnostics_of(publish_params(&published));
        assert_eq!(diagnostics.len(), 1, "{name}: {}", diagnostics.len());
        assert_eq!(
            diagnostics[0].get("severity"),
            Some(&Json::Int(2)),
            "{name}: ожидалось предупреждение о глубине"
        );
        assert_eq!(
            diagnostics[0].get("code").and_then(Json::as_str),
            Some("limit"),
            "{name}"
        );
    }

    // цепочки унарных операторов парсер ограничивает сам: ошибка парсера
    let unary = format!("{let_kw} x = {}1;", "-".repeat(LIMIT - 64));
    lsp.send(&did_change_or_open(100, &unary));
    let published = lsp.recv_within(HUGE_STEP);
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].get("code").and_then(Json::as_str),
        Some("parser")
    );

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish_within(HUGE_STEP);
    assert_eq!(exit.code, Some(0), "stderr: {}", exit.stderr);
    assert_eq!(exit.stderr, "");
}

/// Первое сообщение документа — `didOpen`, следующие — `didChange`.
fn did_change_or_open(version: i64, text: &str) -> String {
    if version == 0 {
        did_open("file:///deep.sga", 1, text)
    } else {
        did_change("file:///deep.sga", version + 1, text)
    }
}

#[test]
fn a_document_over_the_size_limit_gets_one_warning_instead_of_analysis() {
    const LIMIT: usize = 1024 * 1024;
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();

    // ровно на пределе — анализируется (комментарий: диагностик нет)
    let at_limit = format!("// {}", "a".repeat(LIMIT - 3));
    assert_eq!(at_limit.len(), LIMIT);
    lsp.send(&did_open("file:///edge.sga", 1, &at_limit));
    let published = lsp.recv_within(HUGE_STEP);
    assert!(diagnostics_of(publish_params(&published)).is_empty());

    // на байт больше — анализ пропущен, одно предупреждение «limit»; текст
    // содержит ошибку, которой не должно быть видно
    let over = format!("@{}", "a".repeat(LIMIT));
    assert_eq!(over.len(), LIMIT + 1);
    lsp.send(&did_open("file:///over.sga", 1, &over));
    let published = lsp.recv_within(HUGE_STEP);
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1, "{published}");
    let d = &diagnostics[0];
    assert_eq!(d.get("severity"), Some(&Json::Int(2)), "предупреждение");
    assert_eq!(d.get("code").and_then(Json::as_str), Some("limit"));
    assert_eq!(range_of(d), (0, 0, 0, 0));
    assert!(
        d.get("message")
            .and_then(Json::as_str)
            .is_some_and(|m| m.contains("предел")),
        "{d}"
    );

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.finish_within(HUGE_STEP).code, Some(0));
}

#[test]
fn diagnostic_messages_are_bounded_in_length() {
    // Парсер вставляет в сообщение Debug-представление токена целиком;
    // строка в 200 КБ не должна превращаться в диагностику в 200 КБ.
    let text = format!("{} \"{}\" = 1;", kw("LET"), "a".repeat(200_000));
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.send(&did_open("file:///long.sga", 1, &text));
    let published = lsp.recv();
    let diagnostics = diagnostics_of(publish_params(&published));
    assert_eq!(diagnostics.len(), 1);
    let message = diagnostics[0]
        .get("message")
        .and_then(Json::as_str)
        .expect("message");
    assert_eq!(message.chars().count(), 1000);
    assert!(message.ends_with('…'));
    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.finish().code, Some(0));
}

#[test]
fn the_usage_text_of_the_cli_mentions_lsp() {
    let out = Command::new(env!("CARGO_BIN_EXE_sga"))
        .output()
        .expect("запуск sga без аргументов");
    assert_eq!(out.status.code(), Some(1));
    let usage = String::from_utf8_lossy(&out.stderr);
    assert!(usage.contains("lsp [--stdio]"), "{usage}");
    assert!(usage.contains("языковой сервер"), "{usage}");
}

// ── hover и автодополнение (T018) ────────────────────────────────────

fn position_request(id: i64, method: &str, uri: &str, line: i64, character: i64) -> String {
    request(
        id,
        method,
        &format!(
            r#"{{"textDocument":{{"uri":{}}},"position":{{"line":{line},"character":{character}}}}}"#,
            quoted(uri)
        ),
    )
}

fn labels_of(items: &Json) -> Vec<String> {
    items
        .as_array()
        .unwrap_or_else(|| panic!("ожидался массив: {items}"))
        .iter()
        .map(|i| {
            i.get("label")
                .and_then(Json::as_str)
                .expect("label")
                .to_string()
        })
        .collect()
}

#[test]
fn hover_and_completion_answer_over_the_wire_for_a_document_being_typed() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    let uri = "file:///work/typing.sga";

    // документ, который набирают: недописанный вызов, скобка не закрыта
    let text = format!("{} xs = [1, 2];\n{}(x", kw("LET"), kw("PRINT"));
    lsp.send(&did_open(uri, 1, &text));
    let published = lsp.recv();
    assert_eq!(diagnostics_of(publish_params(&published)).len(), 1);

    // hover на ключевом слове — слово из букв SGA, подсказка с мнемоникой
    lsp.send(&position_request(10, "textDocument/hover", uri, 0, 1));
    let reply = lsp.recv();
    assert_eq!(reply.get("id"), Some(&Json::Int(10)), "{reply}");
    let result = reply.get("result").expect("result");
    let contents = result.get("contents").expect("contents");
    assert_eq!(
        contents.get("kind").and_then(Json::as_str),
        Some("markdown")
    );
    let value = contents.get("value").and_then(Json::as_str).expect("value");
    assert!(
        value.contains("**`LET`**") && value.contains(&kw("LET")),
        "{value}"
    );
    assert_eq!(range_of(result), (0, 0, 0, 3));

    // автодополнение у недописанного имени: переменная выше по тексту
    lsp.send(&position_request(11, "textDocument/completion", uri, 1, 7));
    let reply = lsp.recv();
    assert_eq!(reply.get("id"), Some(&Json::Int(11)), "{reply}");
    let items = reply.get("result").expect("result");
    assert_eq!(labels_of(items), ["xs"], "{reply}");
    let item = &items.as_array().unwrap()[0];
    assert_eq!(item.get("kind"), Some(&Json::Int(6)));
    assert_eq!(item.get("detail").and_then(Json::as_str), Some("LET"));

    // правка текста: следующий запрос видит новый текст; префикс `le`
    // даёт builtin и ключевое слово, вставляемое буквами SGA
    lsp.send(&did_change(uri, 2, &format!("{}(le", kw("PRINT"))));
    let _ = lsp.recv();
    lsp.send(&position_request(12, "textDocument/completion", uri, 0, 8));
    let reply = lsp.recv();
    let items = reply.get("result").expect("result");
    assert_eq!(labels_of(items), ["len", "LET"], "{reply}");
    let let_item = &items.as_array().unwrap()[1];
    let edit = let_item.get("textEdit").expect("textEdit");
    assert_eq!(
        edit.get("newText").and_then(Json::as_str),
        Some(kw("LET").as_str())
    );
    assert_eq!(range_of(edit), (0, 6, 0, 8));

    // строка и комментарий: подсказок нет — пустой массив, не null
    lsp.send(&did_change(uri, 3, "\"abc // x\" // note"));
    let _ = lsp.recv();
    for (id, character) in [(13, 3), (14, 15)] {
        lsp.send(&position_request(
            id,
            "textDocument/completion",
            uri,
            0,
            character,
        ));
        let reply = lsp.recv();
        assert_eq!(
            reply.get("result"),
            Some(&Json::Array(Vec::new())),
            "{reply}"
        );
    }

    // чужой документ -> null; неверные параметры -> -32602
    lsp.send(&position_request(
        15,
        "textDocument/hover",
        "file:///none.sga",
        0,
        0,
    ));
    assert_eq!(lsp.recv().get("result"), Some(&Json::Null));
    lsp.send(&request(16, "textDocument/hover", r#"{"textDocument":{}}"#));
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32602, "{reply}");
    assert_eq!(reply.get("id"), Some(&Json::Int(16)));

    lsp.shutdown(17);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.wait(), Some(0));
    let exit = lsp.finish();
    assert_eq!(
        exit.stderr, "",
        "запросы подсказок ничего не пишут в stderr"
    );
    assert!(exit.rest.is_empty(), "лишние сообщения: {:?}", exit.rest);
}

#[test]
fn hover_and_completion_positions_are_utf16_over_the_wire() {
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    let uri = "file:///work/utf16.sga";
    // эмодзи — две единицы UTF-16; слово `len` стоит на единицах 5..8
    lsp.send(&did_open(uri, 1, "\"😀\" len(1)\r\n"));
    let _ = lsp.recv();
    lsp.send(&position_request(2, "textDocument/hover", uri, 0, 6));
    let reply = lsp.recv();
    let result = reply.get("result").expect("result");
    assert_eq!(range_of(result), (0, 5, 0, 8), "{reply}");
    // та же строка, позиция посреди эмодзи: строка — подсказки нет
    lsp.send(&position_request(3, "textDocument/hover", uri, 0, 2));
    assert_eq!(lsp.recv().get("result"), Some(&Json::Null));
    // префикс после эмодзи: диапазон правки в единицах UTF-16
    lsp.send(&did_change(
        uri,
        2,
        &format!("{} s = \"😀\"; le", kw("LET")),
    ));
    let _ = lsp.recv();
    lsp.send(&position_request(4, "textDocument/completion", uri, 0, 16));
    let reply = lsp.recv();
    let items = reply
        .get("result")
        .and_then(Json::as_array)
        .expect("массив");
    let let_item = items
        .iter()
        .find(|i| i.get("label").and_then(Json::as_str) == Some("LET"))
        .expect("LET");
    assert_eq!(
        range_of(let_item.get("textEdit").expect("textEdit")),
        (0, 14, 0, 16)
    );
    lsp.shutdown(5);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.finish().code, Some(0));
}

// ── навигация: символы документа и переход к определению (T019) ──────

const HIERARCHICAL_SYMBOLS: &str =
    r#"{"textDocument":{"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}}"#;

fn symbol_request(id: i64, uri: &str) -> String {
    request(
        id,
        "textDocument/documentSymbol",
        &format!(r#"{{"textDocument":{{"uri":{}}}}}"#, quoted(uri)),
    )
}

/// `(строка, символ)`-диапазон как `строка:символ-строка:символ`.
fn span_text(range: &Json) -> String {
    let coordinate = |side: &str, key: &str| {
        range
            .get(side)
            .and_then(|p| p.get(key))
            .and_then(Json::as_i64)
            .unwrap_or_else(|| panic!("нет {side}.{key}: {range}"))
    };
    format!(
        "{}:{}-{}:{}",
        coordinate("start", "line"),
        coordinate("start", "character"),
        coordinate("end", "line"),
        coordinate("end", "character")
    )
}

/// Иерархический ответ как список строк `глубина|имя|вид|detail|range|selectionRange`.
fn outline_of(symbols: &[Json], depth: usize, out: &mut Vec<String>) {
    for symbol in symbols {
        out.push(format!(
            "{depth}|{}|{}|{}|{}|{}",
            symbol.get("name").and_then(Json::as_str).expect("name"),
            symbol.get("kind").and_then(Json::as_i64).expect("kind"),
            symbol.get("detail").and_then(Json::as_str).unwrap_or(""),
            span_text(symbol.get("range").expect("range")),
            span_text(symbol.get("selectionRange").expect("selectionRange")),
        ));
        if let Some(children) = symbol.get("children").and_then(Json::as_array) {
            outline_of(children, depth + 1, out);
        }
    }
}

/// Ответ `definition` как `строка:символ-строка:символ` или `None` для `null`.
fn definition_of(reply: &Json, uri: &str) -> Option<String> {
    let result = reply.get("result").expect("result");
    if *result == Json::Null {
        return None;
    }
    assert_eq!(
        result.get("uri").and_then(Json::as_str),
        Some(uri),
        "{reply}"
    );
    Some(span_text(result.get("range").expect("range")))
}

fn navigation_source() -> String {
    format!(
        "{st} P {{ x, y }}\n{f} add(a, {m} b: int) -> int {{\n    {r} a;\n}}\n{v} v = 2;\n{l} total = add(1, v);\n",
        st = kw("STRUCT"),
        f = kw("FN"),
        m = kw("MUT"),
        r = kw("RETURN"),
        v = kw("VAR"),
        l = kw("LET"),
    )
}

#[test]
fn symbols_and_definitions_answer_over_the_wire_in_both_symbol_forms() {
    let uri = "file:///work/nav.sga";
    let source = navigation_source();

    // клиент с иерархией: DocumentSymbol[] с детьми и detail
    let mut lsp = Lsp::start(&[]);
    lsp.initialize_with(HIERARCHICAL_SYMBOLS);
    lsp.send(&did_open(uri, 1, &source));
    let _ = lsp.recv(); // диагностики
    lsp.send(&symbol_request(10, uri));
    let reply = lsp.recv();
    assert_eq!(reply.get("id"), Some(&Json::Int(10)), "{reply}");
    let symbols = reply
        .get("result")
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("ожидался массив: {reply}"));
    assert_eq!(symbols.len(), 4, "{reply}");
    let mut outline = Vec::new();
    outline_of(symbols, 0, &mut outline);
    assert_eq!(
        outline,
        [
            "0|P|23||0:0-0:17|0:7-0:8",
            "1|x|8||0:11-0:12|0:11-0:12",
            "1|y|8||0:14-0:15|0:14-0:15",
            "0|add|12|(a, MUT b: int) -> int|1:0-3:1|1:3-1:6",
            "0|v|13|VAR|4:0-4:10|4:4-4:5",
            "0|total|13|LET|5:0-5:22|5:4-5:9",
        ]
    );

    // переход к определению: вызов -> функция, аргумент -> переменная,
    // параметр в теле, само объявление; без определения — null
    let probes: [(i64, i64, i64, Option<&str>); 10] = [
        (20, 5, 13, Some("1:3-1:6")),   // `add` в вызове
        (21, 5, 19, Some("4:4-4:5")),   // аргумент `v`
        (22, 2, 11, Some("1:7-1:8")),   // `a` в `RETURN a;`
        (23, 5, 5, Some("5:4-5:9")),    // само объявление `total`
        (24, 0, 11, Some("0:11-0:12")), // поле `x` в теле структуры
        (25, 5, 1, None),               // ключевое слово
        (26, 1, 18, None),              // аннотация типа `int`
        (27, 5, 18, None),              // запятая между аргументами
        (29, 5, 15, Some("1:3-1:6")),   // курсор сразу за словом `add` — это слово
        (28, 9, 0, None),               // строка за концом текста
    ];
    for (id, line, character, want) in probes {
        lsp.send(&position_request(
            id,
            "textDocument/definition",
            uri,
            line,
            character,
        ));
        let reply = lsp.recv();
        assert_eq!(reply.get("id"), Some(&Json::Int(id)), "{reply}");
        assert_eq!(
            definition_of(&reply, uri).as_deref(),
            want,
            "definition с {line}:{character}"
        );
    }

    // правка документа: запросы видят новый текст
    lsp.send(&did_change(uri, 2, &format!("{} z = 1;\nz;", kw("LET"))));
    let _ = lsp.recv();
    lsp.send(&symbol_request(30, uri));
    let reply = lsp.recv();
    let mut outline = Vec::new();
    outline_of(
        reply
            .get("result")
            .and_then(Json::as_array)
            .expect("массив"),
        0,
        &mut outline,
    );
    assert_eq!(outline, ["0|z|13|LET|0:0-0:10|0:4-0:5"]);
    lsp.send(&position_request(31, "textDocument/definition", uri, 1, 0));
    assert_eq!(definition_of(&lsp.recv(), uri).as_deref(), Some("0:4-0:5"));

    // закрытый и неоткрытый документ — null, не ошибка
    lsp.send(&did_close(uri));
    let _ = lsp.recv();
    lsp.send(&symbol_request(32, uri));
    assert_eq!(lsp.recv().get("result"), Some(&Json::Null));
    lsp.send(&position_request(33, "textDocument/definition", uri, 0, 0));
    assert_eq!(lsp.recv().get("result"), Some(&Json::Null));

    // неверные параметры — -32602, и сервер продолжает работать
    lsp.send(&request(34, "textDocument/documentSymbol", "{}"));
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32602, "{reply}");
    lsp.send(&request(
        35,
        "textDocument/definition",
        &format!(r#"{{"textDocument":{{"uri":{}}}}}"#, quoted(uri)),
    ));
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32602, "{reply}");
    lsp.send(&position_request(36, "textDocument/definition", uri, -1, 0));
    // отрицательная координата — тоже -32602
    let reply = lsp.recv();
    assert_eq!(error_code(&reply), -32602, "{reply}");

    lsp.shutdown(40);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.wait(), Some(0));
    let exit = lsp.finish();
    assert_eq!(exit.stderr, "", "навигация ничего не пишет в stderr");
    assert!(exit.rest.is_empty(), "лишние сообщения: {:?}", exit.rest);

    // клиент без иерархии: плоский SymbolInformation[] с containerName
    let mut lsp = Lsp::start(&[]);
    lsp.initialize();
    lsp.send(&did_open(uri, 1, &source));
    let _ = lsp.recv();
    lsp.send(&symbol_request(10, uri));
    let reply = lsp.recv();
    let list = reply
        .get("result")
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("ожидался массив: {reply}"));
    let flat: Vec<String> = list
        .iter()
        .map(|entry| {
            assert!(entry.get("children").is_none() && entry.get("range").is_none());
            let location = entry.get("location").expect("location");
            assert_eq!(location.get("uri").and_then(Json::as_str), Some(uri));
            format!(
                "{}|{}|{}|{}",
                entry.get("name").and_then(Json::as_str).expect("name"),
                entry.get("kind").and_then(Json::as_i64).expect("kind"),
                entry
                    .get("containerName")
                    .and_then(Json::as_str)
                    .unwrap_or(""),
                span_text(location.get("range").expect("range"))
            )
        })
        .collect();
    assert_eq!(
        flat,
        [
            "P|23||0:0-0:17",
            "x|8|P|0:11-0:12",
            "y|8|P|0:14-0:15",
            "add|12||1:0-3:1",
            "v|13||4:0-4:10",
            "total|13||5:0-5:22",
        ]
    );
    lsp.shutdown(11);
    lsp.send(&notification("exit", "null"));
    assert_eq!(lsp.finish().code, Some(0));
}

#[test]
fn navigation_positions_are_utf16_and_survive_crlf_over_the_wire() {
    let uri = "file:///work/utf16-nav.sga";
    // два эмодзи (по две единицы UTF-16) до функции в той же строке
    let source = format!(
        "{l} s = \"😀😀\"; {f} f(x) {{ {r} x; }}\r\n{l} t = 1;\r\n",
        l = kw("LET"),
        f = kw("FN"),
        r = kw("RETURN"),
    );
    let mut lsp = Lsp::start(&[]);
    lsp.initialize_with(HIERARCHICAL_SYMBOLS);
    lsp.send(&did_open(uri, 1, &source));
    let _ = lsp.recv();
    lsp.send(&symbol_request(10, uri));
    let reply = lsp.recv();
    let mut outline = Vec::new();
    outline_of(
        reply
            .get("result")
            .and_then(Json::as_array)
            .expect("массив"),
        0,
        &mut outline,
    );
    assert_eq!(
        outline,
        [
            // `LET s = "😀😀";` — 13 символов, 15 единиц; `s` до эмодзи
            "0|s|13|LET|0:0-0:15|0:4-0:5",
            // функция начинается после эмодзи: 14 символов + 2 = 16 единиц
            "0|f|12|(x)|0:16-0:37|0:19-0:20",
            "0|t|13|LET|1:0-1:10|1:4-1:5",
        ]
    );
    // `x` в `RETURN x;`: 31-й символ, 33-я единица; цель — параметр (единица 21)
    lsp.send(&position_request(11, "textDocument/definition", uri, 0, 33));
    assert_eq!(
        definition_of(&lsp.recv(), uri).as_deref(),
        Some("0:21-0:22")
    );
    // позиция посреди суррогатной пары не падает и ничего не находит
    lsp.send(&position_request(12, "textDocument/definition", uri, 0, 10));
    assert_eq!(definition_of(&lsp.recv(), uri), None);
    // `t` во второй строке документа с CRLF
    lsp.send(&position_request(13, "textDocument/definition", uri, 1, 4));
    assert_eq!(definition_of(&lsp.recv(), uri).as_deref(), Some("1:4-1:5"));
    lsp.shutdown(14);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish();
    assert_eq!(exit.code, Some(0), "stderr: {}", exit.stderr);
    assert_eq!(exit.stderr, "", "навигация ничего не пишет в stderr");
    assert!(exit.rest.is_empty(), "лишние сообщения: {:?}", exit.rest);
}

/// Глубина и общее число символов иерархического ответа (обход по уровням).
fn tree_size(list: &[Json]) -> (usize, usize) {
    let mut level: Vec<&Json> = list.iter().collect();
    let (mut depth, mut total) = (0, 0);
    while !level.is_empty() {
        depth += 1;
        total += level.len();
        level = level
            .iter()
            .filter_map(|symbol| symbol.get("children").and_then(Json::as_array))
            .flatten()
            .collect();
    }
    (depth, total)
}

/// Огромный документ для проверки навигации: что ждём в ответ.
struct BigDocument {
    name: &'static str,
    text: String,
    /// Число символов в ответе `documentSymbol`.
    symbols: usize,
    /// Запрос `definition` (строка и символ с нуля) и его ответ.
    query: (i64, i64),
    want: Option<String>,
}

#[test]
fn one_mebibyte_documents_get_navigation_answers_without_crashing_the_server() {
    // Документы почти в 1 МиБ той формы, на которой ломается всё, что
    // рекурсивно по размеру или квадратично: лестница незакрытых функций,
    // сотни тысяч вложенных блоков, десятки тысяч функций, структура с
    // огромным числом полей. Падение процесса (переполнение стека -> abort ->
    // код `None`) — провал.
    const LIMIT: usize = 1024 * 1024;
    let fill = |head: &str, piece: &dyn Fn(usize) -> String, tail: &str| {
        let mut text = String::from(head);
        let mut count = 0;
        loop {
            let next = piece(count);
            if text.len() + next.len() + tail.len() > LIMIT {
                break;
            }
            text.push_str(&next);
            count += 1;
        }
        text.push_str(tail);
        (text, count)
    };

    let fn_kw = kw("FN");
    let (ladder, ladder_count) = fill("", &|_| format!("{fn_kw} a() {{ "), "");

    let braces_tail = format!(" {} x = 1; x;", kw("LET"));
    let braces = format!("{}{braces_tail}", "{".repeat(LIMIT - braces_tail.len()));
    let braces_decl = LIMIT - braces_tail.len() + 1 + 3 + 1;

    let (functions, function_count) = fill(
        "",
        &|i| {
            format!(
                "{fn_kw} f{i}(a, {} b: int) -> int {{ {} a; }}\n",
                kw("MUT"),
                kw("RETURN")
            )
        },
        "",
    );
    let middle = function_count / 2;
    let functions = format!("{functions}f{middle}(1, 2);\n");
    let middle_len = format!("f{middle}").len();

    let struct_head = format!("{} S {{ ", kw("STRUCT"));
    let struct_tail = format!("}}\n{} s = S {{ f0: 1 }};\n", kw("LET"));
    let (fields, field_count) = fill(&struct_head, &|i| format!("f{i}, "), &struct_tail);
    let field_col = struct_head.chars().count();

    let cases = [
        BigDocument {
            name: "лестница незакрытых функций",
            text: ladder,
            symbols: ladder_count,
            query: (0, 3),
            want: Some("0:3-0:4".to_string()),
        },
        BigDocument {
            name: "сотни тысяч вложенных блоков",
            text: braces,
            symbols: 0,
            query: (0, (braces_decl + 7) as i64),
            want: Some(format!("0:{braces_decl}-0:{}", braces_decl + 1)),
        },
        BigDocument {
            name: "десятки тысяч функций",
            text: functions,
            symbols: function_count,
            query: (function_count as i64, 1),
            want: Some(format!("{middle}:3-{middle}:{}", 3 + middle_len)),
        },
        BigDocument {
            name: "структура с огромным числом полей",
            text: fields,
            symbols: 1 + field_count + 1,
            query: (1, (kw("LET").chars().count() + " s = S { ".len()) as i64),
            want: Some(format!("0:{field_col}-0:{}", field_col + 2)),
        },
    ];

    let mut lsp = Lsp::start(&[]);
    lsp.initialize_with(HIERARCHICAL_SYMBOLS);
    let uri = "file:///deep.sga";
    for (version, case) in cases.iter().enumerate() {
        let BigDocument {
            name,
            text,
            symbols,
            query: (line, character),
            want,
        } = case;
        assert!(text.len() <= LIMIT, "{name}: {}", text.len());
        assert!(text.len() > LIMIT - 64, "{name}: документ слишком короткий");
        lsp.send(&did_change_or_open(version as i64, text));
        let _ = lsp.recv_within(HUGE_STEP); // диагностики: их содержание — дело T016/T017

        let began = Instant::now();
        lsp.send(&symbol_request(100 + version as i64, uri));
        let reply = lsp.recv_within(HUGE_STEP);
        let list = reply
            .get("result")
            .and_then(Json::as_array)
            .unwrap_or_else(|| panic!("{name}: ожидался массив"));
        let (depth, total) = tree_size(list);
        assert!(depth <= 16, "{name}: глубина ответа {depth}");
        assert_eq!(total, *symbols, "{name}: число символов");
        eprintln!("{name}: documentSymbol за {:?}", began.elapsed());

        lsp.send(&position_request(
            200 + version as i64,
            "textDocument/definition",
            uri,
            *line,
            *character,
        ));
        let reply = lsp.recv_within(HUGE_STEP);
        assert_eq!(
            definition_of(&reply, uri),
            *want,
            "{name}: definition с {line}:{character}"
        );
    }

    lsp.shutdown(2);
    lsp.send(&notification("exit", "null"));
    let exit = lsp.finish_within(HUGE_STEP);
    assert_eq!(exit.code, Some(0), "stderr: {}", exit.stderr);
    assert_eq!(exit.stderr, "");
}
