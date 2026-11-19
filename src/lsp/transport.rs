//! Транспорт LSP (T016, M007): сообщения как фреймы `Content-Length`.
//!
//! Фрейм: заголовки (`Имя: значение\r\n`), пустая строка `\r\n`, затем ровно
//! `Content-Length` байт тела (JSON в UTF-8). Читатель строг и ограничен:
//! плохой фрейм — не «пропустить и продолжить», а ошибка, после которой
//! читать поток дальше нельзя (граница следующего сообщения уже неизвестна),
//! поэтому вызывающий завершает работу. Пределы защищают от клиента,
//! который шлёт бесконечный заголовок, тысячи заголовков или объявляет тело
//! в гигабайты: ничего подобного читатель не выделяет заранее.

use std::fmt;
use std::io::{self, BufRead, Read, Write};

/// Максимальная длина одной строки заголовка в байтах (без `\n`).
pub const MAX_HEADER_LINE: usize = 8 * 1024;
/// Максимальное число строк заголовков в одном сообщении.
pub const MAX_HEADERS: usize = 32;
/// Максимальный размер тела сообщения в байтах.
pub const MAX_BODY: usize = 32 * 1024 * 1024;

/// Ошибка чтения фрейма. После любой из них поток считается рассинхронизированным.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// Поток оборвался в заголовках или в теле (уточнение в тексте).
    UnexpectedEof(&'static str),
    HeaderLineTooLong,
    TooManyHeaders,
    /// Строка заголовка без `:`.
    MalformedHeader,
    MissingContentLength,
    DuplicateContentLength,
    /// Значение Content-Length не состоит только из десятичных цифр.
    InvalidContentLength,
    /// Content-Length больше `MAX_BODY` (или не помещается в `u64`).
    BodyTooLarge,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "ошибка ввода-вывода: {e}"),
            FrameError::UnexpectedEof(where_) => write!(f, "неожиданный конец потока {where_}"),
            FrameError::HeaderLineTooLong => {
                write!(f, "строка заголовка длиннее {MAX_HEADER_LINE} байт")
            }
            FrameError::TooManyHeaders => write!(f, "больше {MAX_HEADERS} заголовков в сообщении"),
            FrameError::MalformedHeader => write!(f, "строка заголовка без ':'"),
            FrameError::MissingContentLength => {
                write!(f, "в сообщении нет заголовка Content-Length")
            }
            FrameError::DuplicateContentLength => {
                write!(f, "заголовок Content-Length повторяется")
            }
            FrameError::InvalidContentLength => write!(
                f,
                "значение Content-Length должно состоять только из десятичных цифр"
            ),
            FrameError::BodyTooLarge => {
                write!(f, "Content-Length больше допустимых {MAX_BODY} байт")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Читает одну строку (без `\n` и без одного замыкающего `\r`), не накапливая
/// больше `MAX_HEADER_LINE` байт. `Ok(None)` — поток кончился ровно на
/// границе строки (ни одного байта до EOF).
fn read_line<R: BufRead>(input: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let buf = match input.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        };
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(FrameError::UnexpectedEof("посреди строки заголовка"))
            };
        }
        match buf.iter().position(|&b| b == b'\n') {
            Some(newline) => {
                if line.len() + newline > MAX_HEADER_LINE {
                    return Err(FrameError::HeaderLineTooLong);
                }
                line.extend_from_slice(&buf[..newline]);
                input.consume(newline + 1);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            None => {
                let n = buf.len();
                if line.len() + n > MAX_HEADER_LINE {
                    return Err(FrameError::HeaderLineTooLong);
                }
                line.extend_from_slice(buf);
                input.consume(n);
            }
        }
    }
}

/// Отрезает пробелы и табуляции по краям. Не `<[u8]>::trim_ascii`: он
/// стабилен только с Rust 1.80 (README обещает ≥ 1.75) и срезал бы заодно
/// `\x0C`, `\r` и `\n`, которых в значении быть не должно.
fn trim_spaces_and_tabs(mut value: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = value {
        if matches!(first, b' ' | b'\t') {
            value = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = value {
        if matches!(last, b' ' | b'\t') {
            value = rest;
        } else {
            break;
        }
    }
    value
}

/// Значение Content-Length: пробелы и табуляции по краям допустимы, внутри —
/// только цифры (без знака, без `0x`, без разделителей).
fn parse_content_length(value: &[u8]) -> Result<usize, FrameError> {
    let value = trim_spaces_and_tabs(value);
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(FrameError::InvalidContentLength);
    }
    let mut n: u64 = 0;
    for &digit in value {
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(digit - b'0')))
            .ok_or(FrameError::BodyTooLarge)?;
    }
    if n > MAX_BODY as u64 {
        return Err(FrameError::BodyTooLarge);
    }
    Ok(n as usize)
}

/// Читает следующее сообщение. `Ok(None)` — поток кончился на границе
/// сообщений (штатное завершение). Тело возвращается как есть (байты):
/// проверка UTF-8 и JSON — дело вызывающего.
pub fn read_message<R: BufRead>(input: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut content_length: Option<usize> = None;
    let mut headers = 0usize;
    let mut first_line = true;
    loop {
        let line = match read_line(input)? {
            Some(line) => line,
            None if first_line => return Ok(None),
            None => return Err(FrameError::UnexpectedEof("в заголовках сообщения")),
        };
        first_line = false;
        if line.is_empty() {
            break; // пустая строка — конец заголовков
        }
        headers += 1;
        if headers > MAX_HEADERS {
            return Err(FrameError::TooManyHeaders);
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or(FrameError::MalformedHeader)?;
        if line[..colon].eq_ignore_ascii_case(b"content-length") {
            if content_length.is_some() {
                return Err(FrameError::DuplicateContentLength);
            }
            content_length = Some(parse_content_length(&line[colon + 1..])?);
        }
        // остальные заголовки (Content-Type и т.п.) игнорируются
    }
    let length = content_length.ok_or(FrameError::MissingContentLength)?;
    // `take` + `read_to_end` растят буфер по мере поступления байт: клиент,
    // объявивший 32 МиБ и приславший три байта, не заставит выделить 32 МиБ.
    let mut body = Vec::new();
    input
        .by_ref()
        .take(length as u64)
        .read_to_end(&mut body)
        .map_err(FrameError::Io)?;
    if body.len() != length {
        return Err(FrameError::UnexpectedEof("в теле сообщения"));
    }
    Ok(Some(body))
}

/// Пишет сообщение одним фреймом и сбрасывает буфер. Длина — в БАЙТАХ тела.
pub fn write_message<W: Write>(output: &mut W, body: &str) -> io::Result<()> {
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body.as_bytes());
    output.write_all(&frame)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_all(bytes: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        let mut input = Cursor::new(bytes.to_vec());
        let mut messages = Vec::new();
        while let Some(body) = read_message(&mut input)? {
            messages.push(body);
        }
        Ok(messages)
    }

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    fn err_of(bytes: &[u8]) -> FrameError {
        match read_all(bytes) {
            Ok(m) => panic!("ожидалась ошибка фрейма, прочитано: {m:?}"),
            Err(e) => e,
        }
    }

    // ── чтение: допустимое ───────────────────────────────────────────

    #[test]
    fn reads_a_single_message() {
        assert_eq!(read_all(&frame("{}")).unwrap(), vec![b"{}".to_vec()]);
    }

    #[test]
    fn reads_consecutive_messages_and_ends_cleanly() {
        let mut bytes = frame(r#"{"a":1}"#);
        bytes.extend(frame(r#"{"b":2}"#));
        bytes.extend(frame("[]"));
        assert_eq!(
            read_all(&bytes).unwrap(),
            vec![
                br#"{"a":1}"#.to_vec(),
                br#"{"b":2}"#.to_vec(),
                b"[]".to_vec()
            ]
        );
    }

    #[test]
    fn empty_stream_is_a_clean_end() {
        assert!(read_all(b"").unwrap().is_empty());
    }

    #[test]
    fn header_name_is_case_insensitive() {
        for name in [
            "content-length",
            "CONTENT-LENGTH",
            "Content-length",
            "cOnTeNt-LeNgTh",
        ] {
            let bytes = format!("{name}: 2\r\n\r\n{{}}").into_bytes();
            assert_eq!(read_all(&bytes).unwrap(), vec![b"{}".to_vec()], "{name}");
        }
    }

    #[test]
    fn extra_headers_are_ignored_in_any_order() {
        let bytes = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nX-Other: 1\r\nContent-Length: 2\r\nContent-Type: again\r\n\r\n{}";
        assert_eq!(read_all(bytes).unwrap(), vec![b"{}".to_vec()]);
    }

    #[test]
    fn whitespace_around_the_value_and_leading_zeros_are_fine() {
        assert_eq!(
            read_all(b"Content-Length:2\r\n\r\n{}").unwrap(),
            vec![b"{}".to_vec()]
        );
        assert_eq!(
            read_all(b"Content-Length: \t 2 \t\r\n\r\n{}").unwrap(),
            vec![b"{}".to_vec()]
        );
        assert_eq!(
            read_all(b"Content-Length: 0002\r\n\r\n{}").unwrap(),
            vec![b"{}".to_vec()]
        );
    }

    #[test]
    fn length_is_counted_in_bytes_not_characters() {
        let body = r#"{"s":"привет 😀"}"#;
        assert_ne!(body.len(), body.chars().count());
        assert_eq!(
            read_all(&frame(body)).unwrap(),
            vec![body.as_bytes().to_vec()]
        );
    }

    #[test]
    fn bare_line_feeds_are_tolerated() {
        let bytes = b"Content-Length: 2\n\n{}Content-Length: 2\r\n\n{}";
        assert_eq!(
            read_all(bytes).unwrap(),
            vec![b"{}".to_vec(), b"{}".to_vec()]
        );
    }

    #[test]
    fn empty_body_is_a_message() {
        assert_eq!(
            read_all(b"Content-Length: 0\r\n\r\n").unwrap(),
            vec![Vec::<u8>::new()]
        );
    }

    #[test]
    fn body_bytes_are_returned_verbatim_even_if_not_utf8() {
        let mut bytes = b"Content-Length: 3\r\n\r\n".to_vec();
        bytes.extend([0xFF, 0xFE, 0x00]);
        assert_eq!(read_all(&bytes).unwrap(), vec![vec![0xFF, 0xFE, 0x00]]);
    }

    // ── чтение: ошибки ───────────────────────────────────────────────

    #[test]
    fn missing_content_length_is_an_error() {
        assert!(matches!(
            err_of(b"Content-Type: x\r\n\r\n{}"),
            FrameError::MissingContentLength
        ));
        // сразу пустая строка
        assert!(matches!(
            err_of(b"\r\n{}"),
            FrameError::MissingContentLength
        ));
    }

    #[test]
    fn duplicate_content_length_is_an_error_even_with_equal_values() {
        assert!(matches!(
            err_of(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}"),
            FrameError::DuplicateContentLength
        ));
        assert!(matches!(
            err_of(b"Content-Length: 2\r\ncontent-length: 3\r\n\r\n{}"),
            FrameError::DuplicateContentLength
        ));
    }

    #[test]
    fn spaces_and_tabs_around_the_content_length_value_are_tolerated() {
        for value in ["7", " 7", "7 ", "\t7\t", " \t 7 \t ", "007"] {
            let bytes = format!("Content-Length:{value}\r\n\r\n{{\"a\":1}}").into_bytes();
            assert_eq!(
                read_all(&bytes).unwrap(),
                vec![b"{\"a\":1}".to_vec()],
                "{value:?}"
            );
        }
        // другие пробельные символы — не часть допустимого значения
        for value in ["7\x0C", "\x0C7", "7\x0B", "\x0B7", "7\r", "\r7"] {
            let bytes = format!("Content-Length: {value}\r\n\r\n{{\"a\":1}}").into_bytes();
            assert!(
                matches!(err_of(&bytes), FrameError::InvalidContentLength),
                "значение {value:?}"
            );
        }
    }

    #[test]
    fn non_digit_content_length_values_are_errors() {
        for value in [
            "abc", "-5", "+5", "", "   ", "5 5", "0x10", "1e3", "２", "5,0", "٣",
        ] {
            let bytes = format!("Content-Length: {value}\r\n\r\n{{}}").into_bytes();
            assert!(
                matches!(err_of(&bytes), FrameError::InvalidContentLength),
                "значение {value:?}"
            );
        }
    }

    #[test]
    fn oversized_content_length_is_rejected_before_reading_the_body() {
        let over = MAX_BODY + 1;
        let bytes = format!("Content-Length: {over}\r\n\r\n").into_bytes();
        assert!(matches!(err_of(&bytes), FrameError::BodyTooLarge));
        // не помещается даже в u64 — тоже отказ, а не паника
        let bytes = b"Content-Length: 99999999999999999999999999\r\n\r\n";
        assert!(matches!(err_of(bytes), FrameError::BodyTooLarge));
    }

    #[test]
    fn content_length_that_wraps_around_u64_is_rejected_not_taken_modulo() {
        // 2^64, 2^64 + 1, 2^64 + 5 и 2^65 при арифметике «по модулю» дали бы
        // 0, 1, 5 и 0 — крошечные допустимые длины; обязан быть отказ.
        for value in [
            "18446744073709551616",
            "18446744073709551617",
            "18446744073709551621",
            "36893488147419103232",
        ] {
            let bytes = format!("Content-Length: {value}\r\n\r\n{{}}").into_bytes();
            assert!(
                matches!(err_of(&bytes), FrameError::BodyTooLarge),
                "значение {value}"
            );
        }
        // наибольшее u64 не переполняет, но тоже слишком велико
        let bytes = b"Content-Length: 18446744073709551615\r\n\r\n";
        assert!(matches!(err_of(bytes), FrameError::BodyTooLarge));
    }

    #[test]
    fn content_length_at_the_limit_is_accepted_but_a_short_body_is_an_error() {
        // Объявлено ровно MAX_BODY, пришло три байта: читатель не выделяет
        // 32 МиБ заранее и сообщает об обрыве.
        let bytes = format!("Content-Length: {MAX_BODY}\r\n\r\nabc").into_bytes();
        assert!(matches!(
            err_of(&bytes),
            FrameError::UnexpectedEof("в теле сообщения")
        ));
    }

    #[test]
    fn truncated_body_is_an_error() {
        assert!(matches!(
            err_of(b"Content-Length: 10\r\n\r\n{}"),
            FrameError::UnexpectedEof("в теле сообщения")
        ));
        assert!(matches!(
            err_of(b"Content-Length: 1\r\n\r\n"),
            FrameError::UnexpectedEof("в теле сообщения")
        ));
    }

    #[test]
    fn truncated_headers_are_errors() {
        // обрыв посреди строки заголовка
        assert!(matches!(
            err_of(b"Content-Len"),
            FrameError::UnexpectedEof("посреди строки заголовка")
        ));
        // обрыв после полной строки заголовка, до пустой строки
        assert!(matches!(
            err_of(b"Content-Length: 2\r\n"),
            FrameError::UnexpectedEof("в заголовках сообщения")
        ));
        // у первого сообщения всё хорошо, у второго обрыв в заголовках
        let mut bytes = frame("{}");
        bytes.extend(b"Content-Length: 2\r\n");
        assert!(matches!(
            err_of(&bytes),
            FrameError::UnexpectedEof("в заголовках сообщения")
        ));
    }

    #[test]
    fn header_without_a_colon_is_an_error() {
        assert!(matches!(
            err_of(b"Content-Length 2\r\n\r\n{}"),
            FrameError::MalformedHeader
        ));
    }

    #[test]
    fn header_name_must_match_exactly() {
        // пробел перед двоеточием — уже другое имя: Content-Length не найден
        assert!(matches!(
            err_of(b"Content-Length : 2\r\n\r\n{}"),
            FrameError::MissingContentLength
        ));
    }

    #[test]
    fn header_line_length_limit() {
        let value = "x".repeat(MAX_HEADER_LINE - "X: ".len());
        let ok_line = format!("X: {value}"); // ровно MAX_HEADER_LINE
        assert_eq!(ok_line.len(), MAX_HEADER_LINE);
        let bytes = format!("{ok_line}\r\nContent-Length: 2\r\n\r\n{{}}").into_bytes();
        // строка с \r в конце — MAX_HEADER_LINE + 1 байт до \n: это уже превышение
        assert!(matches!(err_of(&bytes), FrameError::HeaderLineTooLong));
        // без \r — ровно предел, допустимо
        let bytes = format!("{ok_line}\nContent-Length: 2\r\n\r\n{{}}").into_bytes();
        assert_eq!(read_all(&bytes).unwrap(), vec![b"{}".to_vec()]);
        // на байт длиннее — отказ
        let bytes = format!("X{ok_line}\nContent-Length: 2\r\n\r\n{{}}").into_bytes();
        assert!(matches!(err_of(&bytes), FrameError::HeaderLineTooLong));
    }

    #[test]
    fn endless_header_without_newline_is_rejected_without_buffering_it_all() {
        // Бесконечный заголовок без перевода строки: читатель останавливается
        // на пределе, не дочитывая поток до конца.
        struct Endless {
            produced: usize,
        }
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'a');
                self.produced += buf.len();
                Ok(buf.len())
            }
        }
        let mut input = io::BufReader::new(Endless { produced: 0 });
        let err = read_message(&mut input).unwrap_err();
        assert!(matches!(err, FrameError::HeaderLineTooLong), "{err}");
        assert!(
            input.get_ref().produced < 4 * MAX_HEADER_LINE + 64 * 1024,
            "прочитано слишком много: {}",
            input.get_ref().produced
        );
    }

    #[test]
    fn header_count_limit() {
        let mut ok = String::new();
        for i in 0..MAX_HEADERS - 1 {
            ok.push_str(&format!("X-{i}: v\r\n"));
        }
        ok.push_str("Content-Length: 2\r\n\r\n{}"); // ровно MAX_HEADERS заголовков
        assert_eq!(read_all(ok.as_bytes()).unwrap(), vec![b"{}".to_vec()]);

        let mut too_many = String::new();
        for i in 0..MAX_HEADERS {
            too_many.push_str(&format!("X-{i}: v\r\n"));
        }
        too_many.push_str("Content-Length: 2\r\n\r\n{}"); // MAX_HEADERS + 1
        assert!(matches!(
            err_of(too_many.as_bytes()),
            FrameError::TooManyHeaders
        ));
    }

    #[test]
    fn read_errors_from_the_stream_are_reported() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "сломано"))
            }
        }
        let mut input = io::BufReader::new(Broken);
        assert!(matches!(read_message(&mut input), Err(FrameError::Io(_))));
    }

    #[test]
    fn interrupted_reads_are_retried() {
        struct Flaky {
            data: Cursor<Vec<u8>>,
            interrupt_next: bool,
        }
        impl Read for Flaky {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.interrupt_next {
                    self.interrupt_next = false;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.interrupt_next = true;
                // по одному байту: каждое второе чтение прерывается
                let n = buf.len().min(1);
                self.data.read(&mut buf[..n])
            }
        }
        let mut input = io::BufReader::with_capacity(
            1,
            Flaky {
                data: Cursor::new(frame("{}")),
                interrupt_next: true,
            },
        );
        assert_eq!(read_message(&mut input).unwrap(), Some(b"{}".to_vec()));
    }

    #[test]
    fn error_messages_are_readable() {
        let texts: Vec<String> = [
            FrameError::UnexpectedEof("в теле сообщения"),
            FrameError::HeaderLineTooLong,
            FrameError::TooManyHeaders,
            FrameError::MalformedHeader,
            FrameError::MissingContentLength,
            FrameError::DuplicateContentLength,
            FrameError::InvalidContentLength,
            FrameError::BodyTooLarge,
            FrameError::Io(io::Error::other("x")),
        ]
        .iter()
        .map(|e| e.to_string())
        .collect();
        for text in &texts {
            assert!(!text.is_empty());
        }
        assert!(texts[4].contains("Content-Length"));
    }

    // ── запись ───────────────────────────────────────────────────────

    #[test]
    fn writes_a_frame_with_the_length_in_bytes() {
        let mut out = Vec::new();
        write_message(&mut out, r#"{"s":"привет"}"#).unwrap();
        let body = r#"{"s":"привет"}"#;
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
        );
        // 8 байт ASCII-обрамления (`{"s":"` и `"}`) + 6 кириллических букв по
        // 2 байта; в символах длина другая — заголовок обязан считать БАЙТЫ.
        assert_eq!(body.len(), 8 + 12);
        assert_ne!(body.len(), body.chars().count());
    }

    #[test]
    fn written_frames_are_readable_back() {
        let mut out = Vec::new();
        for body in ["{}", r#"{"x":"😀"}"#, ""] {
            write_message(&mut out, body).unwrap();
        }
        assert_eq!(
            read_all(&out).unwrap(),
            vec![b"{}".to_vec(), r#"{"x":"😀"}"#.as_bytes().to_vec(), vec![]]
        );
    }

    #[test]
    fn write_flushes_and_reports_errors() {
        struct Counting {
            written: Vec<u8>,
            flushes: usize,
        }
        impl Write for Counting {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut out = Counting {
            written: Vec::new(),
            flushes: 0,
        };
        write_message(&mut out, "{}").unwrap();
        assert_eq!(out.flushes, 1, "каждый фрейм сбрасывается");

        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_message(&mut Closed, "{}").is_err());
    }
}
