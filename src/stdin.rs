//! Разбор стандартного ввода для `read_line()` и `read_stdin()` — T013,
//! M006 (`std/io`). Полный Feature Contract — см. PROJECT_STATUS.md.
//!
//! Здесь только чистые функции над `BufRead`: они ничего не знают ни о VM,
//! ни о том, откуда пришли байты (stdin процесса, заранее заданный буфер,
//! тестовый читатель). Благодаря этому границы формата — `\r\n`, EOF,
//! невалидный UTF-8, ошибка читателя — проверяются юнит-тестами без запуска
//! процесса. Связка с VM (состояние читателя, перехват вызова) — в
//! `vm::Vm::call_read_line` / `vm::Vm::call_read_stdin`.
//!
//! Ввод читается как БАЙТЫ и декодируется строго (`String::from_utf8`):
//! строки SGA — это UTF-8, поэтому тихая lossy-подстановка U+FFFD исказила
//! бы данные без единого сигнала. Невалидный UTF-8 — ошибка; байты, которые
//! уже прочитаны, считаются потреблёнными (поток не откатывается назад).

use std::io::BufRead;

/// Следующая строка ввода БЕЗ завершающего перевода строки; `None`, если
/// ввод закончился (не осталось ни одного байта).
///
/// Правила (контракт T013):
///  - читаются байты до первого `\n` включительно либо до EOF;
///  - отбрасывается ОДИН завершающий `\n` и, только если `\n` был, ОДИН
///    `\r` непосредственно перед ним — так `\r\n` и `\n` дают одно и то же;
///  - `\r`, не стоящий перед `\n` (внутри строки или на самом конце ввода),
///    остаётся данными: `"a\r"` в конце ввода — строка `"a\r"`;
///  - последняя строка без завершающего `\n` — полноценная строка, а не
///    потеря данных; пустая строка `""` (ввод `"\n"`) отличима от конца
///    ввода (`None`);
///  - не-UTF-8 → `Err`; прочитанная строка потреблена, следующий вызов
///    продолжит со следующей.
pub(crate) fn read_line(input: &mut dyn BufRead) -> Result<Option<String>, String> {
    let mut bytes = Vec::new();
    let read = input
        .read_until(b'\n', &mut bytes)
        .map_err(|e| format!("ошибка чтения stdin: {}", e))?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    decode(bytes).map(Some)
}

/// ВЕСЬ остаток ввода дословно (переводы строк не нормализуются); пустая
/// строка, если ввод уже исчерпан. После вызова ввод исчерпан в любом
/// случае — и при успехе, и при ошибке декодирования.
pub(crate) fn read_all(input: &mut dyn BufRead) -> Result<String, String> {
    let mut bytes = Vec::new();
    input
        .read_to_end(&mut bytes)
        .map_err(|e| format!("ошибка чтения stdin: {}", e))?;
    decode(bytes)
}

/// Строгое декодирование UTF-8: сообщение называет UTF-8 и сообщает, после
/// скольких валидных байт фрагмента встретилась неверная последовательность.
fn decode(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|e| {
        format!(
            "ввод не является валидным UTF-8 (неверная последовательность после {} валидных \
             байт прочитанного фрагмента); строки SGA — это UTF-8, поэтому такие данные \
             прочитать нельзя, подстановка символа замены не выполняется",
            e.utf8_error().valid_up_to()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, BufReader, Cursor, Read};

    fn some(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    /// Читает строки до конца ввода; последний элемент — `None` (EOF).
    /// Паникует на ошибке — для валидных входов она недопустима.
    fn collect_lines(input: &[u8]) -> Vec<Option<String>> {
        let mut reader = Cursor::new(input.to_vec());
        let mut out = Vec::new();
        loop {
            let line = read_line(&mut reader).expect("валидный ввод не должен давать ошибку");
            let finished = line.is_none();
            out.push(line);
            if finished {
                return out;
            }
        }
    }

    #[test]
    fn lf_lines_are_returned_one_by_one_then_eof() {
        assert_eq!(collect_lines(b"a\nb\n"), vec![some("a"), some("b"), None]);
    }

    #[test]
    fn last_line_without_terminator_is_still_a_line() {
        assert_eq!(collect_lines(b"a\nb"), vec![some("a"), some("b"), None]);
    }

    #[test]
    fn crlf_is_the_same_as_lf_and_empty_line_is_not_eof() {
        assert_eq!(
            collect_lines(b"a\r\n\r\nb\n"),
            vec![some("a"), some(""), some("b"), None]
        );
        assert_eq!(collect_lines(b"\n"), vec![some(""), None]);
        assert_eq!(collect_lines(b"\r\n"), vec![some(""), None]);
    }

    #[test]
    fn lone_cr_is_data_inside_a_line_and_at_the_end_of_input() {
        assert_eq!(collect_lines(b"a\rb\n"), vec![some("a\rb"), None]);
        assert_eq!(collect_lines(b"a\r"), vec![some("a\r"), None]);
    }

    #[test]
    fn only_one_cr_before_lf_is_stripped() {
        assert_eq!(collect_lines(b"a\r\r\n"), vec![some("a\r"), None]);
    }

    #[test]
    fn empty_input_is_eof_immediately_and_stays_eof() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_line(&mut reader), Ok(None));
        assert_eq!(read_line(&mut reader), Ok(None));
    }

    #[test]
    fn invalid_utf8_line_is_an_error_naming_utf8_and_the_line_is_consumed() {
        let mut reader = Cursor::new(b"ok\nab\xffcd\nnext\n".to_vec());
        assert_eq!(read_line(&mut reader), Ok(some("ok")));
        let err = read_line(&mut reader).unwrap_err();
        assert!(err.contains("UTF-8"), "получено: {err}");
        assert!(err.contains("после 2 валидных"), "получено: {err}");
        assert!(
            !err.contains('\u{FFFD}'),
            "подстановка U+FFFD недопустима: {err}"
        );
        assert_eq!(
            read_line(&mut reader),
            Ok(some("next")),
            "строка с ошибкой потреблена, чтение продолжается со следующей"
        );
    }

    #[test]
    fn multibyte_characters_survive_a_reader_with_one_byte_buffer() {
        // Граница внутреннего буфера падает внутрь каждого многобайтного
        // символа: декодирование должно идти по собранной строке целиком, а
        // не по кускам буфера.
        let text = "привет\n日本語 🦀\r\nконец";
        let mut reader = BufReader::with_capacity(1, Cursor::new(text.as_bytes().to_vec()));
        assert_eq!(read_line(&mut reader), Ok(some("привет")));
        assert_eq!(read_line(&mut reader), Ok(some("日本語 🦀")));
        assert_eq!(read_line(&mut reader), Ok(some("конец")));
        assert_eq!(read_line(&mut reader), Ok(None));
    }

    #[test]
    fn read_all_returns_everything_verbatim_and_exhausts_the_input() {
        let mut reader = Cursor::new(b"a\r\nb\n\nc".to_vec());
        assert_eq!(read_all(&mut reader), Ok("a\r\nb\n\nc".to_string()));
        assert_eq!(read_all(&mut reader), Ok(String::new()));
        assert_eq!(read_line(&mut reader), Ok(None));
    }

    #[test]
    fn read_all_on_empty_input_is_an_empty_string() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_all(&mut reader), Ok(String::new()));
    }

    #[test]
    fn read_all_after_read_line_returns_the_remainder() {
        let mut reader = Cursor::new(b"first\r\nsecond\r\nthird".to_vec());
        assert_eq!(read_line(&mut reader), Ok(some("first")));
        assert_eq!(read_all(&mut reader), Ok("second\r\nthird".to_string()));
    }

    #[test]
    fn read_all_handles_multibyte_text_with_one_byte_buffer() {
        let text = "日本語\nпривет 🦀";
        let mut reader = BufReader::with_capacity(1, Cursor::new(text.as_bytes().to_vec()));
        assert_eq!(read_all(&mut reader), Ok(text.to_string()));
    }

    #[test]
    fn read_all_invalid_utf8_is_an_error_and_the_input_is_consumed() {
        let mut reader = Cursor::new(b"ab\xff".to_vec());
        let err = read_all(&mut reader).unwrap_err();
        assert!(err.contains("UTF-8"), "получено: {err}");
        assert!(err.contains("после 2 валидных"), "получено: {err}");
        assert_eq!(read_line(&mut reader), Ok(None), "ввод исчерпан");
    }

    /// Читатель, у которого любая попытка чтения заканчивается ошибкой.
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "boom"))
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "boom"))
        }

        fn consume(&mut self, _amt: usize) {}
    }

    #[test]
    fn reader_errors_are_reported_as_errors_not_panics() {
        let err = read_line(&mut FailingReader).unwrap_err();
        assert!(err.contains("boom"), "получено: {err}");
        let err = read_all(&mut FailingReader).unwrap_err();
        assert!(err.contains("boom"), "получено: {err}");
    }
}
