//! Словарь подсказок языкового сервера (T018, M007): описания ключевых слов и
//! встроенных функций для `textDocument/hover` и `textDocument/completion`.
//!
//! Здесь живут только ТЕКСТЫ. Перечень имён принадлежит компилятору:
//! ключевые слова — `lexer::token::KEYWORD_MNEMONICS` (одна таблица с
//! лексером), встроенные функции — `runtime::BUILTINS` (одна таблица с
//! `is_builtin`, `semantic::builtin_arity` и VM). Словарь сверяется с ними
//! тестами — по составу, ПОРЯДКУ и арности: новое слово языка или новый
//! builtin без описания ломает сборку тестов, а не остаётся без подсказки
//! молча (так закрывается расхождение «подсказки знают одно, компилятор —
//! другое», TD-013). Арность в описании не хранится отдельно: она ВЫВОДИТСЯ из
//! списка параметров (`BuiltinDoc::arity`) и должна совпасть с реестром.
//! Типы результатов проверяются выполнением настоящих вызовов
//! (`tests/lsp_assist_test.rs`).
//!
//! Тексты — на русском, как вся документация проекта. Всё, что в них является
//! кодом (имена, значения, пример синтаксиса), заключено в обратные кавычки:
//! подсказка показывается как Markdown, и неэкранированное `_` в `str_split`
//! превратилось бы в курсив.

/// Описание ключевого слова.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeywordDoc {
    /// ASCII-мнемоника слова (`LET`); в файле слово записано буквами SGA.
    pub mnemonic: &'static str,
    /// Очень короткое название (для списка автодополнения), без точки.
    pub title: &'static str,
    /// Синтаксис в записи МНЕМОНИКАМИ, как в EBNF `docs/LANGUAGE_SPEC.md`;
    /// несколько форм — по одной на строку.
    pub syntax: &'static str,
    /// Описание: что делает слово, какие у него ограничения.
    pub summary: &'static str,
}

/// Параметр встроенной функции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Param {
    pub name: &'static str,
    /// `true` — параметр можно опустить.
    pub optional: bool,
}

const fn req(name: &'static str) -> Param {
    Param {
        name,
        optional: false,
    }
}

const fn opt(name: &'static str) -> Param {
    Param {
        name,
        optional: true,
    }
}

/// Описание встроенной функции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinDoc {
    pub name: &'static str,
    pub params: &'static [Param],
    /// Тип результата в записи аннотаций SGA (`int`, `string`, `array`, `nil`,
    /// `bool`, `float`, `any`); несколько возможных — через ` | `.
    pub returns: &'static str,
    pub summary: &'static str,
}

impl BuiltinDoc {
    /// Допустимое число аргументов `(min, max)`, выведенное из списка
    /// параметров; обязано совпасть с `runtime::builtin_arity`.
    pub fn arity(&self) -> (usize, usize) {
        (
            self.params.iter().filter(|p| !p.optional).count(),
            self.params.len(),
        )
    }

    /// Параметры в записи с необязательными частями в квадратных скобках:
    /// `array[, value]`, `[start, ]end`.
    pub fn params_text(&self) -> String {
        render_params(self.params)
    }

    /// Подпись: `имя(параметры) -> результат`.
    pub fn signature(&self) -> String {
        format!("{}({}) -> {}", self.name, self.params_text(), self.returns)
    }

    /// Допустимые типы результата — части `returns`, разделённые ` | `.
    pub fn return_alternatives(&self) -> impl Iterator<Item = &'static str> {
        self.returns.split(" | ")
    }
}

/// Список параметров в записи с необязательными частями в квадратных скобках.
/// Умеет две формы: необязательные параметры в конце (`a[, b]`) и один
/// необязательный в начале (`[a, ]b`) — других в реестре нет (проверяется
/// тестом).
fn render_params(params: &[Param]) -> String {
    let mut out = String::new();
    // предыдущий необязательный параметр уже вывел запятую внутри скобок
    let mut comma_done = false;
    for (i, p) in params.iter().enumerate() {
        if p.optional {
            if i == 0 && params.len() > 1 {
                out.push_str(&format!("[{}, ]", p.name));
                comma_done = true;
            } else if i == 0 {
                out.push_str(&format!("[{}]", p.name));
            } else {
                out.push_str(&format!("[, {}]", p.name));
            }
        } else {
            if i > 0 && !comma_done {
                out.push_str(", ");
            }
            comma_done = false;
            out.push_str(p.name);
        }
    }
    out
}

/// Описание ключевого слова по мнемонике.
pub fn keyword(mnemonic: &str) -> Option<&'static KeywordDoc> {
    KEYWORDS.iter().find(|k| k.mnemonic == mnemonic)
}

/// Описание встроенной функции по имени.
pub fn builtin(name: &str) -> Option<&'static BuiltinDoc> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// Ключевые слова — в порядке `lexer::token::KEYWORD_MNEMONICS`.
pub const KEYWORDS: &[KeywordDoc] = &[
    KeywordDoc {
        mnemonic: "LET",
        title: "неизменяемая переменная",
        syntax: "LET имя [: тип] = выражение;",
        summary: "Объявляет переменную, которой нельзя присвоить новое значение. Неизменяемость глубокая: через `LET`-переменную нельзя менять и содержимое массива или struct (`имя[i] = …`, `push(имя, …)`, `имя.поле = …`) — это ошибка компиляции. Переменная видна от конца объявления до конца блока.",
    },
    KeywordDoc {
        mnemonic: "VAR",
        title: "изменяемая переменная",
        syntax: "VAR имя [: тип] = выражение;",
        summary: "Объявляет переменную, которой можно присваивать (`имя = …`) и через которую можно менять содержимое массива или struct. Только `VAR`-переменную можно передать в параметр, объявленный с `MUT`.",
    },
    KeywordDoc {
        mnemonic: "CONST",
        title: "константа",
        syntax: "CONST имя [: тип] = выражение;",
        summary: "Объявляет константу. В версии 0.1 ведёт себя как `LET`: присвоить значение повторно нельзя.",
    },
    KeywordDoc {
        mnemonic: "FN",
        title: "функция",
        syntax: "FN имя([MUT] параметр [: тип], …) [-> тип] { … }\nFN(параметр, …) { … }",
        summary: "Объявляет функцию. `FN имя(…)` — именованная функция: допустима только на верхнем уровне файла и видна во всём файле, в том числе выше места объявления. `FN(…)` без имени — выражение-замыкание: значение, которое можно сохранить в переменную, передать как аргумент и вызвать; оно захватывает видимые переменные по значению (копия на момент создания).",
    },
    KeywordDoc {
        mnemonic: "RETURN",
        title: "возврат из функции",
        syntax: "RETURN [выражение];",
        summary: "Завершает функцию и возвращает значение; без выражения возвращается `NIL`. Вне функции — ошибка компиляции.",
    },
    KeywordDoc {
        mnemonic: "IF",
        title: "условие",
        syntax: "IF условие { … } [ELSE IF условие { … }] [ELSE { … }]",
        summary: "Выполняет блок, если условие истинно. Условие не обязано быть `bool`: ложными считаются `FALSE`, `NIL`, `0`, `0.0`, пустая строка и пустой массив, остальное — истинным.",
    },
    KeywordDoc {
        mnemonic: "ELSE",
        title: "ветка «иначе»",
        syntax: "IF условие { … } ELSE { … }\nIF условие { … } ELSE IF условие { … }",
        summary: "Ветка `IF`, которая выполняется, когда условие ложно. После `ELSE` идёт блок или следующий `IF`.",
    },
    KeywordDoc {
        mnemonic: "WHILE",
        title: "цикл по условию",
        syntax: "WHILE условие { … }",
        summary: "Повторяет блок, пока условие истинно (истинность — как у `IF`). Выход из цикла — `BREAK`, переход к следующему проходу — `CONTINUE`.",
    },
    KeywordDoc {
        mnemonic: "FOR",
        title: "цикл по диапазону",
        syntax: "FOR имя IN начало..конец { … }",
        summary: "Перебирает целые от `начало` (включительно) до `конец` (исключая): `0..3` даёт 0, 1, 2. Если `начало` не меньше `конца`, тело не выполняется. Переменная цикла видна только в теле.",
    },
    KeywordDoc {
        mnemonic: "IN",
        title: "часть цикла FOR",
        syntax: "FOR имя IN начало..конец { … }",
        summary: "Отделяет переменную цикла `FOR` от диапазона.",
    },
    KeywordDoc {
        mnemonic: "TRUE",
        title: "логическая истина",
        syntax: "TRUE",
        summary: "Литерал типа `bool`: истина. `PRINT` печатает его как `истина`.",
    },
    KeywordDoc {
        mnemonic: "FALSE",
        title: "логическая ложь",
        syntax: "FALSE",
        summary: "Литерал типа `bool`: ложь. `PRINT` печатает его как `ложь`.",
    },
    KeywordDoc {
        mnemonic: "STRUCT",
        title: "тип-запись",
        syntax: "STRUCT Имя { поле, поле, … }",
        summary: "Объявляет тип-запись с именованными полями без аннотаций типов. Допустим только на верхнем уровне файла; имя видно во всём файле. Экземпляр создаётся литералом `Имя { поле: значение, … }`, поле читается через точку (`p.x`). Экземпляр — ссылочный тип, как массив. Метод — обычная функция `Имя_метод(self, …)`, вызывается как `p.метод(…)`.",
    },
    KeywordDoc {
        mnemonic: "PRINT",
        title: "вывод на экран",
        syntax: "PRINT(выражение, …);",
        summary: "Печатает значения через пробел и завершает вывод переводом строки; без аргументов печатается пустая строка. `PRINT` — ключевое слово, а не функция: передать его как значение нельзя.",
    },
    KeywordDoc {
        mnemonic: "BREAK",
        title: "выход из цикла",
        syntax: "BREAK;",
        summary: "Немедленно завершает ближайший цикл `WHILE` или `FOR`. Вне цикла — ошибка компиляции.",
    },
    KeywordDoc {
        mnemonic: "CONTINUE",
        title: "следующий проход цикла",
        syntax: "CONTINUE;",
        summary: "Переходит к следующему проходу ближайшего цикла `WHILE` или `FOR`. Вне цикла — ошибка компиляции.",
    },
    KeywordDoc {
        mnemonic: "AND",
        title: "логическое И",
        syntax: "выражение AND выражение",
        summary: "Логическое «и»: результат — `bool`. Оба операнда вычисляются всегда: сокращённого вычисления нет.",
    },
    KeywordDoc {
        mnemonic: "OR",
        title: "логическое ИЛИ",
        syntax: "выражение OR выражение",
        summary: "Логическое «или»: результат — `bool`. Оба операнда вычисляются всегда: сокращённого вычисления нет.",
    },
    KeywordDoc {
        mnemonic: "NOT",
        title: "логическое НЕ",
        syntax: "NOT выражение",
        summary: "Логическое отрицание: результат — `bool`. Операнд сначала приводится к истинности, как в `IF`: `NOT 0` — `TRUE`, `NOT 5` — `FALSE`.",
    },
    KeywordDoc {
        mnemonic: "NIL",
        title: "отсутствие значения",
        syntax: "NIL",
        summary: "Литерал типа `nil` — «ничего»; его возвращает функция без `RETURN`. В аннотации типа пишется так же: `-> NIL`.",
    },
    KeywordDoc {
        mnemonic: "MUT",
        title: "изменяемый параметр",
        syntax: "FN имя(MUT параметр, …) { … }",
        summary: "Помечает параметр функции как изменяемое заимствование: тело может менять переданный массив или struct, и вызывающая сторона увидит изменения. Аргумент обязан быть `VAR`-переменной — передать `LET` нельзя (ошибка компиляции). Без `MUT` параметр неизменяем.",
    },
    KeywordDoc {
        mnemonic: "IMPORT",
        title: "подключение модуля",
        syntax: "IMPORT \"путь/к/файлу.sga\";",
        summary: "Подключает другой файл: его верхнеуровневые объявления подставляются на место `IMPORT`. Допустим только на верхнем уровне; путь отсчитывается от каталога текущего файла, а `pkg:имя/путь` — файл пакета из `sga.toml`. Работает при запуске файла (`sga run`). Языковой сервер импорты не разрешает, поэтому для файла с `IMPORT` проверка имён и типов не выполняется.",
    },
    KeywordDoc {
        mnemonic: "MATCH",
        title: "сопоставление с образцом",
        syntax: "MATCH выражение { образец -> выражение, … }",
        summary: "Выражение: сравнивает значение с образцами по порядку и даёт результат первого подошедшего пункта. Образец — литерал, `_` (подходит любому значению) или имя (подходит любому значению и связывает его; имя видно только в своём пункте). Последний пункт обязан быть `_` или именем.",
    },
];

/// Встроенные функции — в порядке `runtime::BUILTINS`.
pub const BUILTINS: &[BuiltinDoc] = &[
    BuiltinDoc {
        name: "len",
        params: &[req("value")],
        returns: "int",
        summary: "Длина: число символов строки (не байт), число элементов массива или число полей struct. Для остальных типов — ошибка выполнения.",
    },
    BuiltinDoc {
        name: "push",
        params: &[req("array"), opt("value")],
        returns: "nil",
        summary: "Добавляет значение в конец массива на месте; без второго аргумента добавляется `NIL`. Массив должен быть `VAR`-переменной (через `LET` менять нельзя — ошибка компиляции). Возвращает `NIL`.",
    },
    BuiltinDoc {
        name: "keys",
        params: &[req("value")],
        returns: "array",
        summary: "Для struct — массив имён полей в алфавитном порядке; для массива — его индексы `[0, 1, …]`.",
    },
    BuiltinDoc {
        name: "range",
        params: &[opt("start"), req("end")],
        returns: "array",
        summary: "Массив целых от `start` (включительно) до `end` (исключая); `range(end)` — то же, что `range(0, end)`. Пустой, если `start` не меньше `end`. Больше 10 000 000 элементов — ошибка выполнения.",
    },
    BuiltinDoc {
        name: "to_string",
        params: &[req("value")],
        returns: "string",
        summary: "Текстовое представление любого значения — в том виде, в каком его печатает `PRINT`.",
    },
    BuiltinDoc {
        name: "to_int",
        params: &[req("value")],
        returns: "int",
        summary: "Целое из `int`, `float` (дробная часть отбрасывается), `string` (пробелы по краям не мешают; не число — ошибка выполнения) или `bool` (`TRUE` — 1, `FALSE` — 0).",
    },
    BuiltinDoc {
        name: "to_float",
        params: &[req("value")],
        returns: "float",
        summary: "Число с плавающей точкой из `int`, `float` или `string` (не число — ошибка выполнения).",
    },
    BuiltinDoc {
        name: "type_of",
        params: &[req("value")],
        returns: "string",
        summary: "Имя типа значения: `int`, `float`, `bool`, `string`, `array`, `nil`, `closure`; для экземпляра struct — имя его типа (например `Point`).",
    },
    BuiltinDoc {
        name: "sqrt",
        params: &[req("x")],
        returns: "float",
        summary: "Квадратный корень числа (`int` или `float`); результат всегда `float`, для отрицательного числа — `NaN`.",
    },
    BuiltinDoc {
        name: "floor",
        params: &[req("x")],
        returns: "int",
        summary: "Округление вниз до целого: `floor(-2.5)` — `-3`.",
    },
    BuiltinDoc {
        name: "ceil",
        params: &[req("x")],
        returns: "int",
        summary: "Округление вверх до целого: `ceil(-2.5)` — `-2`.",
    },
    BuiltinDoc {
        name: "abs",
        params: &[req("x")],
        returns: "int | float",
        summary: "Модуль числа; тип результата тот же, что у аргумента (`int` даёт `int`, `float` даёт `float`).",
    },
    BuiltinDoc {
        name: "min",
        params: &[req("a"), req("b")],
        returns: "int | float",
        summary: "Меньшее из двух чисел. Два `int` дают `int`, если хотя бы одно число `float` — результат `float`.",
    },
    BuiltinDoc {
        name: "max",
        params: &[req("a"), req("b")],
        returns: "int | float",
        summary: "Большее из двух чисел. Два `int` дают `int`, если хотя бы одно число `float` — результат `float`.",
    },
    BuiltinDoc {
        name: "pow",
        params: &[req("base"), req("exp")],
        returns: "int | float",
        summary: "Возведение в степень. Два `int` дают `int` (при переполнении — ошибка выполнения; отрицательный целый показатель — тоже ошибка, приведите оба аргумента через `to_float`), иначе результат `float`.",
    },
    BuiltinDoc {
        name: "str_split",
        params: &[req("s"), req("sep")],
        returns: "array",
        summary: "Разбивает строку по разделителю и возвращает массив строк; `str_split(\"a,b\", \",\")` — `[a, b]`.",
    },
    BuiltinDoc {
        name: "str_contains",
        params: &[req("s"), req("sub")],
        returns: "bool",
        summary: "Содержит ли строка `s` подстроку `sub`.",
    },
    BuiltinDoc {
        name: "str_trim",
        params: &[req("s")],
        returns: "string",
        summary: "Строка без пробельных символов в начале и в конце.",
    },
    BuiltinDoc {
        name: "str_starts_with",
        params: &[req("s"), req("prefix")],
        returns: "bool",
        summary: "Начинается ли строка `s` с `prefix`.",
    },
    BuiltinDoc {
        name: "str_ends_with",
        params: &[req("s"), req("suffix")],
        returns: "bool",
        summary: "Заканчивается ли строка `s` на `suffix`.",
    },
    BuiltinDoc {
        name: "str_replace",
        params: &[req("s"), req("from"), req("to")],
        returns: "string",
        summary: "Заменяет ВСЕ вхождения `from` в строке `s` на `to`.",
    },
    BuiltinDoc {
        name: "str_upper",
        params: &[req("s")],
        returns: "string",
        summary: "Строка в верхнем регистре.",
    },
    BuiltinDoc {
        name: "str_lower",
        params: &[req("s")],
        returns: "string",
        summary: "Строка в нижнем регистре.",
    },
    BuiltinDoc {
        name: "json_stringify",
        params: &[req("value")],
        returns: "string",
        summary: "Сериализует значение в JSON-текст. Поля struct идут по алфавиту; замыкание сериализовать нельзя (ошибка выполнения).",
    },
    BuiltinDoc {
        name: "json_parse",
        params: &[req("s")],
        returns: "any",
        summary: "Разбирает JSON-текст в значение. Типа «словарь» в версии 0.1 нет: JSON-объект становится массивом пар `[ключ, значение]`. Некорректный JSON — ошибка выполнения.",
    },
    BuiltinDoc {
        name: "read_file",
        params: &[req("path")],
        returns: "string",
        summary: "Читает текстовый файл (UTF-8) целиком. Путь отсчитывается от каталога запускаемого файла, выйти за его границы нельзя. Работает только при запуске файла (`sga run файл`).",
    },
    BuiltinDoc {
        name: "write_file",
        params: &[req("path"), req("content")],
        returns: "nil",
        summary: "Записывает строку в файл, перезаписывая его. По умолчанию запрещено: нужен флаг `sga run --allow-write`. Путь отсчитывается от каталога запускаемого файла; каталоги не создаются.",
    },
    BuiltinDoc {
        name: "read_line",
        params: &[],
        returns: "string | nil",
        summary: "Читает одну строку из стандартного ввода без завершающего перевода строки; `NIL` — ввод закончился.",
    },
    BuiltinDoc {
        name: "read_stdin",
        params: &[],
        returns: "string",
        summary: "Читает весь оставшийся стандартный ввод одной строкой.",
    },
    BuiltinDoc {
        name: "args",
        params: &[],
        returns: "array",
        summary: "Аргументы командной строки программы (`sga run файл арг1 арг2`) — массив строк без пути к файлу; без аргументов — пустой массив.",
    },
    BuiltinDoc {
        name: "time_now_ms",
        params: &[],
        returns: "int",
        summary: "Календарное время: миллисекунды с 1970-01-01 UTC по системным часам (при переводе часов значение может скачнуть).",
    },
    BuiltinDoc {
        name: "time_monotonic_ms",
        params: &[],
        returns: "int",
        summary: "Монотонное время: миллисекунды с момента запуска VM; не убывает и не зависит от перевода системных часов — для измерения интервалов.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::token::KEYWORD_MNEMONICS;
    use crate::runtime;

    #[test]
    fn keywords_cover_exactly_the_lexer_table_in_the_same_order() {
        let documented: Vec<&str> = KEYWORDS.iter().map(|k| k.mnemonic).collect();
        assert_eq!(
            documented, KEYWORD_MNEMONICS,
            "словарь ключевых слов разошёлся с таблицей лексера"
        );
    }

    #[test]
    fn builtins_cover_exactly_the_runtime_registry_in_the_same_order() {
        let documented: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
        let registry: Vec<&str> = runtime::BUILTINS.iter().map(|b| b.name).collect();
        assert_eq!(
            documented, registry,
            "словарь builtin разошёлся с реестром runtime"
        );
    }

    #[test]
    fn derived_arity_equals_the_compiler_arity_for_every_builtin() {
        for b in BUILTINS {
            assert_eq!(
                Some(b.arity()),
                runtime::builtin_arity(b.name),
                "{}: арность из параметров описания ≠ реестр",
                b.name
            );
            assert_eq!(
                Some(b.arity()),
                crate::semantic::builtin_arity(b.name),
                "{}: арность описания ≠ проверка semantic",
                b.name
            );
        }
    }

    #[test]
    fn lookups_find_every_entry_and_nothing_else() {
        for k in KEYWORDS {
            assert_eq!(keyword(k.mnemonic), Some(k));
        }
        for b in BUILTINS {
            assert_eq!(builtin(b.name), Some(b));
        }
        for name in ["", "let", "Let", "LETS", "len", "PRINTLN", "XYZ"] {
            assert_eq!(keyword(name), None, "{name:?}");
        }
        for name in ["", "LEN", "Len", "print", "str_", "lenn", "main"] {
            assert_eq!(builtin(name), None, "{name:?}");
        }
    }

    #[test]
    fn signatures_render_optional_parameters_in_brackets() {
        let sig = |name: &str| builtin(name).expect("описан").signature();
        assert_eq!(sig("len"), "len(value) -> int");
        assert_eq!(sig("push"), "push(array[, value]) -> nil");
        assert_eq!(sig("range"), "range([start, ]end) -> array");
        assert_eq!(sig("str_replace"), "str_replace(s, from, to) -> string");
        assert_eq!(sig("args"), "args() -> array");
        assert_eq!(sig("read_line"), "read_line() -> string | nil");
        assert_eq!(sig("min"), "min(a, b) -> int | float");
    }

    #[test]
    fn params_text_handles_the_supported_optional_shapes() {
        assert_eq!(render_params(&[]), "");
        assert_eq!(render_params(&[opt("a")]), "[a]");
        assert_eq!(render_params(&[req("a")]), "a");
        assert_eq!(render_params(&[req("a"), req("b")]), "a, b");
        assert_eq!(render_params(&[req("a"), opt("b")]), "a[, b]");
        assert_eq!(render_params(&[opt("a"), req("b")]), "[a, ]b");
        assert_eq!(render_params(&[req("a"), req("b"), opt("c")]), "a, b[, c]");
        assert_eq!(
            render_params(&[req("a"), opt("b"), opt("c")]),
            "a[, b][, c]"
        );
    }

    #[test]
    fn optional_parameters_are_only_trailing_or_a_single_leading_one() {
        // ровно эти две формы умеет рисовать `params_text`
        for b in BUILTINS {
            let first_optional = b.params.iter().position(|p| p.optional);
            if let Some(i) = first_optional {
                let leading = i == 0 && b.params.iter().skip(1).all(|p| !p.optional);
                let trailing = b.params.iter().skip(i).all(|p| p.optional);
                assert!(leading || trailing, "{}: непредусмотренная форма", b.name);
            }
        }
    }

    #[test]
    fn return_types_are_known_type_names() {
        const KNOWN: &[&str] = &["int", "float", "bool", "string", "array", "nil", "any"];
        for b in BUILTINS {
            let alternatives: Vec<&str> = b.return_alternatives().collect();
            assert!(!alternatives.is_empty(), "{}", b.name);
            for alt in &alternatives {
                assert!(KNOWN.contains(alt), "{}: тип результата {alt:?}", b.name);
            }
            if alternatives.contains(&"any") {
                assert_eq!(alternatives.len(), 1, "{}: `any` — единственный", b.name);
            }
        }
    }

    #[test]
    fn parameter_names_are_distinct_lowercase_identifiers() {
        for b in BUILTINS {
            for (i, p) in b.params.iter().enumerate() {
                assert!(
                    !p.name.is_empty()
                        && p.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "{}: параметр {:?}",
                    b.name,
                    p.name
                );
                assert!(
                    b.params.iter().skip(i + 1).all(|o| o.name != p.name),
                    "{}: повтор параметра {}",
                    b.name,
                    p.name
                );
            }
        }
    }

    fn assert_hygienic_text(label: &str, text: &str) {
        assert!(!text.is_empty(), "{label}: пусто");
        assert_eq!(text, text.trim(), "{label}: пробелы по краям");
        assert!(!text.contains('\t'), "{label}: табуляция");
        assert!(!text.contains("  "), "{label}: двойной пробел");
        assert_eq!(
            text.matches('`').count() % 2,
            0,
            "{label}: непарные обратные кавычки (сломают Markdown)"
        );
    }

    #[test]
    fn every_text_is_clean_markdown_source() {
        for k in KEYWORDS {
            assert_hygienic_text(&format!("{} title", k.mnemonic), k.title);
            assert!(!k.title.ends_with('.'), "{}: точка в названии", k.mnemonic);
            assert!(
                k.title.chars().count() <= 40,
                "{}: длинное название",
                k.mnemonic
            );
            assert_hygienic_text(&format!("{} summary", k.mnemonic), k.summary);
            assert!(k.summary.ends_with('.'), "{}: нет точки", k.mnemonic);
            assert!(
                k.summary.chars().count() <= 700,
                "{}: слишком длинное описание",
                k.mnemonic
            );
            // синтаксис — код: пробелы по краям строк и пустые строки не нужны
            assert!(!k.syntax.is_empty(), "{}: пустой синтаксис", k.mnemonic);
            for line in k.syntax.lines() {
                assert_eq!(line, line.trim(), "{}: пробелы в синтаксисе", k.mnemonic);
                assert!(!line.is_empty(), "{}: пустая строка синтаксиса", k.mnemonic);
            }
            assert!(
                k.syntax
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|w| w == k.mnemonic),
                "{}: синтаксис не содержит самого слова",
                k.mnemonic
            );
        }
        for b in BUILTINS {
            assert_hygienic_text(&format!("{} summary", b.name), b.summary);
            assert!(b.summary.ends_with('.'), "{}: нет точки", b.name);
            assert!(
                b.summary.chars().count() <= 700,
                "{}: слишком длинное описание",
                b.name
            );
        }
    }

    #[test]
    fn identifiers_in_summaries_are_always_inside_code_spans() {
        // неэкранированное `_` Markdown превращает в курсив: имя builtin с
        // подчёркиванием вне обратных кавычек — ошибка текста
        for text in KEYWORDS
            .iter()
            .map(|k| k.summary)
            .chain(BUILTINS.iter().map(|b| b.summary))
        {
            let mut inside = false;
            for ch in text.chars() {
                if ch == '`' {
                    inside = !inside;
                } else if ch == '_' && !inside {
                    panic!("подчёркивание вне кода в тексте: {text}");
                }
            }
        }
    }
}
