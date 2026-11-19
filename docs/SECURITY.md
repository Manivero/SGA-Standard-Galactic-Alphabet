# SECURITY — модель безопасности SGA v0.1

Все пункты ниже проверены автоматическими тестами
(`tests/integration_test.rs`, `cargo test`) — это не декларация, а
наблюдаемое поведение.

## Реализовано и проверено

| Свойство | Механизм | Тест |
|---|---|---|
| **Memory safety** | Гарантируется самим Rust (без `unsafe` в кодовой базе VM/компилятора) | `grep -r unsafe src/` -> пусто |
| **Целочисленное переполнение** | `checked_add/sub/mul/div` -> управляемая `RuntimeError`, не panic/UB | `test_integer_overflow_is_runtime_error_not_panic` |
| **Деление на ноль** | Явная проверка перед `Div`/`Mod`, управляемая ошибка | `test_division_by_zero_is_runtime_error` |
| **Выход за границы массива** | `normalize_index()` проверяет `0 <= i < len` перед доступом | `test_array_out_of_bounds_is_runtime_error` |
| **Immutable by default** | `LET`/`CONST` помечают переменную немутируемой; попытка `StoreVar` отклоняется и на этапе семантического анализа (`src/semantic/mod.rs`), и повторно в VM (defense-in-depth, `src/vm/mod.rs::assign`). Это **глубокая** неизменяемость, а не только защита привязки: `let arr = [1,2,3]; arr[0] = 5;` и `push(arr, 4)` тоже отклоняются (`semantic::Analyzer::check_mutation_target`) — выбор в пользу модели Rust/Swift, а не shallow-модели JS/Kotlin, см. `docs/LANGUAGE_SPEC.md`, §3 | `test_immutable_assignment_is_rejected`, `test_immutable_array_index_assignment_is_rejected`, `test_immutable_array_push_is_rejected` |
| **Immutable by default (структуры)** | Та же глубокая неизменяемость распространена на присваивание полю struct: `let p = Point{...}; p.x = 1;` отклоняется на этапе семантики (`semantic::Analyzer::mutation_root`, ветка `Expr::FieldAccess`) — структуры участвуют в Ownership-модели на тех же правах, что и массивы | `test_struct_field_assign_through_let_is_rejected`, `test_struct_field_assign_through_var_is_allowed` |
| **Ownership/Borrowing на границе вызова функции** | Параметры функций immutable по умолчанию; мутация требует `MUT` в сигнатуре **и** `VAR`-аргумента на месте вызова (`semantic::Analyzer`, проверка по сигнатуре функции + повторно в `vm::call` как defense-in-depth) | `test_immutability_bypass_through_function_parameter_is_now_fixed`, `test_passing_immutable_variable_to_mut_parameter_is_rejected`, `test_mut_parameter_with_var_argument_allows_mutation_across_call_boundary` |
| **Использование неинициализированных/неопределённых имён** | Semantic analyzer отклоняет до исполнения | `test_undefined_variable_is_rejected` |
| **Type safety (динамическая)** | Бинарные операции проверяют совместимость типов в runtime (`arith()`/`compare()` в `src/vm/mod.rs`), несовместимые типы -> `RuntimeError`, не "тихая" порча данных | покрыто косвенно через `arith`/`compare` match-конструкции, которые не имеют catch-all "приведения" |
| **Type safety (статическая, градуальная)** | Аннотации типов (`: int`, `-> string`, ...) проверяются отдельным проходом `src/typechecker/mod.rs` ДО выполнения; без аннотации поведение остаётся полностью динамическим (обратная совместимость) | `test_typed_function_param_and_return_mismatch_is_rejected`, `test_typed_var_decl_with_mismatched_value_is_type_error`, `test_untyped_var_can_still_change_type_freely` |
| **Защита от stack overflow рекурсии (вызовы функций)** | `Vm::max_call_depth = 200`, превышение -> управляемая ошибка | `test_recursion_depth_limit_triggers_before_stack_overflow`, `test_recursion_depth_limit_is_safe_under_default_sized_stack` |
| **Защита от stack overflow в рекурсивном спуске парсера** | До T001 (2026-08-09): `((((...1...))))` с несколькими сотнями вложенных `(` давало неперехватываемый `fatal runtime error: stack overflow` и `abort()` процесса — НЕ Rust panic, `catch_unwind` не помогает. Причина: `MAX_PARSE_DEPTH=256` считал по одному инкременту на вызов `parse_expr`, но один такой вызов — это ~10 вложенных нативных Rust-кадров (вся цепочка precedence-climbing), так что 256 "уровней" защиты реально исчерпывали 2‑МиБ стек (дефолтный размер стека НЕ главного потока в Rust — именно такой даёт тестовый харнесс `cargo test`) ЗАДОЛГО до срабатывания проверки. Эмпирически измерено (см. `src/parser/mod.rs`, doc-комментарий `MAX_PARSE_DEPTH`): порог реального переполнения — между 50–60 уровней при 1 МиБ стека, 104–108 при 2 МиБ, 420–440 при 8 МиБ. Исправлено: `MAX_PARSE_DEPTH` понижена до эмпирически безопасного `80`, тест переведён на детерминированный `stack_size(2 МиБ)` (по аналогии с тестами `max_call_depth` ниже). **Дополнение T016 (M007, 2026-10-24):** защита покрывала скобки, блоки и выражения, но не цепочки унарных `-`/`NOT` (`- - - … x`) и присваиваний (`a = b = … 1;`): они рекурсируют по `parse_unary`/`parse_assignment`, минуя `parse_expr`, и несколько тысяч операторов подряд роняли процесс тем же неперехватываемым `stack overflow` — недопустимо для языкового сервера, который разбирает любой текст из редактора. Теперь каждый шаг такой цепочки проходит через тот же `guarded_recursion` и делит с остальным разбором общий бюджет в 80 уровней: на верхнем уровне допустимо до 78 операторов подряд, больше — `ParseError` («превышена максимальная глубина вложенности унарных операторов (80)» либо «… присваиваний (80)»). Это изменение поведения только для экстремальных цепочек. Ветка унарных операторов вынесена из `parse_unary` в `parse_prefix_operator`, чтобы не раздувать кадр, который стоит на пути КАЖДОЙ вложенной скобки: замер на отладочной сборке (минимальный стек для разбора 200 вложенных конструкций) не вырос, а чуть уменьшился — скобки 1937→1873 КиБ, `MATCH` 2032→1937, лямбды 1619→1588. **Дополнение T017 (M007, 2026-10-29):** позиции в AST сделали `Expr` и `Stmt` структурами `{ kind, span }` (+8 байт на узел), а размеры значений, которые парсер возвращает по значению, входят в кадры рекурсивного спуска, поэтому замер (отладочная сборка, минимальный стек для 200 вложенных конструкций, из 2048 КиБ, которые даёт тест) повторён: скобки 1873→953 КиБ, вызов `f(` 1873→1111, массив 1873→1048, `MATCH` 1937→1238, лямбды 1588→1238, фигурные скобки 1270→1365 (+7,5 %), унарные операторы 127→127, присваивания 222→222. Запас у скобок вырос с 8,5 % до 53 %, `MAX_PARSE_DEPTH` не менялась | `test_deeply_nested_parens_returns_parse_error_not_stack_overflow`, `test_moderately_nested_parens_still_parses_successfully` (negative-тест на false positive); T016: `parser::tests::unary_chain_is_accepted_up_to_exactly_the_depth_limit`, `not_chain_is_limited_like_the_minus_chain`, `assignment_chain_is_accepted_up_to_exactly_the_depth_limit`, `very_long_chains_are_a_parse_error_not_a_stack_overflow`, `mixed_unary_and_paren_nesting_shares_one_depth_budget` |
| **Bytecode verifier** | `Chunk`/`OpCode`/`CompiledProgram` публичны, поэтому потребитель крейта может сконструировать байткод напрямую, в обход компилятора; `Vm::new` верифицирует все ссылки на пул констант и все цели переходов (включая тела вложенных замыканий) ДО исполнения и отклоняет повреждённый байткод как `RuntimeError`, а не падает в Rust panic | `test_vm_rejects_chunk_with_out_of_bounds_const_index_instead_of_panicking`, `test_vm_rejects_chunk_with_jump_target_past_end`, `test_vm_rejects_corrupted_function_chunk_not_just_main` |
| **Защита от scope-stack underflow в `DefineVar`** | `verify_chunk` не проверяет статически баланс `PushScope`/`PopScope` (ненадёжно при наличии `Jump`/`JumpIfFalse` — порядок исполнения динамический). До фикса вручную сконструированный `Chunk` с лишним `PopScope` (без соответствующего `PushScope`) опускал `scopes` до пустого вектора, и следующий `DefineVar` падал на `scopes.last_mut().unwrap()` — неуправляемая Rust panic. Заменено на `.ok_or_else(...)?`, в точности тот же паттерн, что уже используется для каждого `stack.pop()` в этой функции | `test_vm_defines_var_after_scope_underflow_returns_runtime_error_not_panic` |
| **IMPORT confinement (path traversal / absolute path)** | `IMPORT` была единственной операцией файлового I/O во всём пайплайне до T009 (см. следующую строку — `read_file` переиспользует ровно эту же границу и логику, не отдельную). Резолвится в `module_resolver::resolve_imports`. До фикса `Path::join` с абсолютным аргументом (документированное поведение std: абсолютный путь полностью заменяет базу) и относительные `../../`-цепочки позволяли `IMPORT "/etc/passwd";` или `IMPORT "../../secret.sga";` читать и инлайнить произвольный файл с правами процесса — за пределами каталога входного файла программы. Подтверждено эмпирически PoC (планированный секрет в файле вне проекта успешно "утёк" через `print()`). Исправлено: абсолютные пути отклоняются безусловно (до `.join`); резолвленный путь обязан остаться внутри каталога входного файла программы — проверяется через `std::fs::canonicalize` (резолвит symlink) с лексическим fallback'ом для случаев, когда путь ещё не существует на диске (in-memory loader в тестах). Легитимные `..`, которые НЕ выходят за эту границу (вложенные модули внутри одного проекта), продолжают резолвиться как раньше. **Поправка T015 (2026-10-18):** до неё эта граница НЕ работала при запуске программы по голому имени файла (`sga run main.sga`) — см. строку «Корень песочницы никогда не бывает пустым» ниже | `test_absolute_path_import_is_rejected`, `test_path_traversal_escaping_entry_root_is_rejected_on_real_fs`, `test_path_traversal_within_entry_root_still_works_on_real_fs`, `test_cli_import_cannot_escape_the_program_directory_for_any_entry_spelling` |
| **`read_file` confinement (T009, M003, 2026-08-16)** | Второй builtin с файловым I/O после `IMPORT` — **переиспользует ту же confinement-границу и ту же проверку** (`module_resolver::is_within_root`, `module_resolver::compute_confinement_root`, обе сделаны `pub` специально для этого — единый источник истины, не отдельная копия логики). Абсолютные пути отклоняются безусловно; `..`, выходящий за каталог входного файла программы, отклоняется. Программы, запущенные через `run_source` (без файлового пути — нет базы, от которой считать confinement), получают понятную `RuntimeError` при вызове `read_file`, а не тихо читают cwd процесса или падают необъяснимо. Только ЧТЕНИЕ: запись — отдельный builtin `write_file` с явным разрешением (T015, см. строку ниже), удаление явно вне scope (см. `docs/ROADMAP.md`); чтение разрешения не требует | `test_read_file_absolute_path_is_rejected`, `test_read_file_parent_escape_is_rejected`, `test_read_file_without_file_context_is_runtime_error`, `test_read_file_reads_sibling_file_within_root`, `test_read_file_in_subdirectory_within_root_works` |
| **`pkg:`-импорты: confinement пакета и поиск манифеста (T011, M005, 2026-10-10)** | Третья точка файлового ввода-вывода на этапе резолвинга (после `IMPORT` и `read_file`). `IMPORT "pkg:имя/путь"` резолвится только через `[dependencies]` БЛИЖАЙШЕГО `sga.toml` (поиск от каталога входного файла вверх по дереву; побеждает ближайший, без слияния). Путь пакета считается от каталога манифеста; файл внутри пакета обязан остаться в каталоге пакета — проверка `module_resolver::is_within_root`, та же, что у `IMPORT` и `read_file` (не копия): `pkg:mylib/../secret.sga` отклоняется. У пакета СВОЙ confinement-root: `IMPORT "../../x.sga"` из файла пакета не может выйти ни за каталог пакета, ни в каталог основного проекта; транзитивные `pkg:` запрещены. **Граница доверия (осознанная):** каталог зависимости, объявленный в `sga.toml`, НЕ ограничен каталогом проекта (`path = "../mylib"` по построению указывает наружу) — автору манифеста доверяют. Из-за поиска вверх доверенными становятся и каталоги-предки запускаемого файла: чужой `sga.toml` в предке (например, `/tmp/sga.toml`) может переназначить `pkg:`-имя, если ближе манифеста нет — то же свойство, что у `Cargo.toml`/`package.json`; запускайте `pkg:`-программы только из каталогов, предкам которых доверяете. Ошибки манифеста всплывают лишь при `pkg:`-импорте, поэтому посторонний или повреждённый `sga.toml` не ломает программы без `pkg:`. | `test_pkg_import_dotdot_escaping_package_root_is_rejected`, `test_pkg_internal_import_confined_to_package_root_not_main_project_root`, `test_pkg_transitive_pkg_import_inside_package_is_rejected`, `test_pkg_nearest_manifest_wins_over_ancestor_manifest`, `test_pkg_nearest_manifest_is_not_merged_with_ancestor_manifest`, `test_malformed_manifest_does_not_affect_programs_without_pkg_imports` |
| **Аргументы запуска: `args()` (T012, M006, 2026-10-11)** | Builtin отдаёт программе ТОЛЬКО строки, которые передал запустивший (`sga run файл арг...` / `RunOptions::with_args`); он не читает окружение процесса, файлы или сеть, поэтому отдельного разрешения не требует. Строки SGA — UTF-8: не-UTF-8 аргумент отклоняется в CLI понятной ошибкой с кодом возврата 1 (раньше `env::args()` завершал процесс паникой, код 101). Каждый вызов возвращает НОВЫЙ массив — программа не может «испортить» значение для последующих вызовов. Всё после пути файла — данные программы: флаги самого `sga` из этой части командной строки не подхватываются | `test_cli_run_with_non_utf8_argument_is_clear_error_not_panic`, `test_cli_run_passes_arguments_after_file_verbatim_to_program`, `test_args_each_call_returns_independent_array`, `test_vm_args_call_with_arguments_via_handbuilt_bytecode_is_runtime_error` |
| **Стандартный ввод: `read_line()` / `read_stdin()` (T013, M006, 2026-10-14)** | Builtin'ы читают ТОЛЬКО тот ввод, который запускающая сторона явно подключила: CLI `sga run` всегда отдаёт программе stdin процесса (это канал запустившего — отдельного разрешения не требует, как и `args()`; ничего, кроме поданного на вход, программа получить не может), а библиотечные `run_source`/`run_source_file` по умолчанию дают ПУСТОЙ ввод — встраивающий код не «протекает» stdin хоста в недоверенную программу, пока явно не вызовет `RunOptions::with_process_stdin()`. Ввод — строгий UTF-8: не-UTF-8 → управляемая `RuntimeError` (код возврата 1) без паники и без молчаливой подмены символов; ошибка чтения читателя — тоже `RuntimeError`. Чтение ленивое: stdin процесса не трогается, пока программа сама не вызовет builtin, поэтому программа, которая не читает ввод, не блокируется на открытом pipe/терминале. Это НЕ файловый I/O: confinement-границы `IMPORT`/`read_file` не применяются и не нужны. Размер ввода и длина строки не ограничены — см. «Явно НЕ реализовано» ниже | `test_stdin_is_empty_by_default`, `test_read_line_invalid_utf8_is_runtime_error_not_panic_or_lossy`, `test_read_stdin_invalid_utf8_is_runtime_error_not_panic_or_lossy`, `test_cli_run_invalid_utf8_on_stdin_is_clear_error_not_panic`, `test_cli_run_stdin_io_error_is_runtime_error_not_panic`, `test_cli_run_program_that_does_not_read_stdin_does_not_block_on_open_pipe`, `test_vm_stdin_calls_with_arguments_via_handbuilt_bytecode_are_runtime_errors` |
| **Время: `time_now_ms()` / `time_monotonic_ms()` (T014, M006, 2026-10-15)** | Builtin'ы читают ТОЛЬКО часы процесса (`SystemTime::now()` и `Instant`): это не файловый и не сетевой ввод-вывод, поэтому confinement-границы `IMPORT`/`read_file` не применяются и отдельного разрешения не нужно (как и у `args()`). Арифметика (`src/clock.rs`) не паникует: время до 1970-01-01 UTC и значение, не помещающееся в `int`, — управляемая `RuntimeError` (код возврата 1), а не усечение; монотонные часы не убывают, а «момент раньше старта» сатурируется в 0. Что стоит знать при запуске чужих программ: (а) программы, читающие время, НЕДЕТЕРМИНИРОВАНЫ — один и тот же вход даёт разные результаты, воспроизводимый запуск невозможен, пока встраивающий код не может подменить часы (TD-014); (б) программа получает замер времени с точностью до миллисекунды — это не новая возможность (замер доступен и подсчётом итераций цикла), но более точная; границей безопасности она не является; (в) `time_now_ms()` следует за системными часами и может скачком уйти вперёд или назад при их переводе — для интервалов предназначен `time_monotonic_ms()` | `test_time_now_ms_is_an_int_between_system_clock_readings`, `test_time_monotonic_ms_starts_near_zero_and_never_exceeds_elapsed_time`, `test_time_monotonic_ms_is_non_decreasing_over_10000_calls`, `test_time_monotonic_ms_busy_wait_advances_with_real_time`, `test_vm_time_calls_with_arguments_via_handbuilt_bytecode_are_runtime_errors`, юнит-тесты `src/clock.rs` (`unix_millis_before_the_epoch_is_an_error_naming_1970`, `unix_millis_beyond_the_int_range_is_an_error_not_a_panic`) |
| **Корень песочницы никогда не бывает пустым (T015, M006, 2026-10-18)** | **Найденный и исправленный дефект уже существовавшей защиты.** `compute_confinement_root` берёт каталог входного файла как `Path::parent()`; для файла в текущем каталоге (`sga run main.sga`, `sga run ./main.sga`) это ПУСТОЙ путь. Лексический fallback `is_within_root` сравнивал нормализованные пути как есть, и для пустого корня `starts_with` истинно для любого относительного пути, включая `../secret.txt`. Эффект на настоящем бинарнике (проверено до правок): `read_file("../secret.txt")` и `IMPORT "../x.sga"` читали файл ВНЕ каталога программы, `read_file("linkdir/secret.txt")` — через symlink на каталог вне проекта; при запуске по абсолютному пути или по `proj/main.sga` граница работала верно, поэтому прежние тесты (с абсолютными путями) дефекта не видели, а этот документ утверждал, что граница работает. Исправлено двумя независимыми слоями: (1) `compute_confinement_root` никогда не возвращает пустой путь (пустой каталог → `.`; единый источник корня для `IMPORT`, `pkg:`, `read_file` и `write_file`); (2) лексический fallback приводит кандидата и корень к абсолютным путям по текущему каталогу (`std::env::current_dir`) и при невозможности это сделать отказывает (fail-closed). Сигнатуры публичных функций не менялись. Это изменение поведения в сторону безопасности: программы, которые при запуске по голому имени полагались на выход из каталога, перестали работать — осознанно. Остаточное свойство: лексический fallback не раскрывает symlink'и в ещё не существующих путях (TD-015) | `test_cli_read_file_cannot_escape_the_program_directory_for_any_entry_spelling`, `test_cli_import_cannot_escape_the_program_directory_for_any_entry_spelling`, `test_cli_read_file_through_a_symlink_to_an_outside_directory_is_rejected_for_any_entry_spelling`, юнит-тесты `src/module_resolver.rs` (`confinement_root_of_a_file_in_the_current_directory_is_dot_not_empty`, `parent_escape_is_outside_the_current_directory_for_existing_and_missing_candidates`, `lexical_check_fails_closed_for_relative_paths_when_the_current_directory_is_unknown`) |
| **`write_file`: запись файлов только с явным разрешением (T015, M006, 2026-10-18)** | Первая операция SGA-программы, которая ИЗМЕНЯЕТ файловую систему, и первая permission-граница SGA. (1) **Запрещена по умолчанию**: разрешение выдаёт только запускающая сторона — `sga run --allow-write <файл>` (флаг обязан стоять ДО пути файла; после пути это обычный аргумент программы и разрешения не даёт) либо `RunOptions::with_allow_write(true)` / `Vm::set_allow_write(true)` в библиотеке. Источника разрешения внутри программы нет: ни сама программа, ни импортированные модули и `pkg:`-пакеты, ни `sga.toml`, ни переменные окружения его выдать или расширить не могут. Проверка разрешения идёт ДО проверок пути. (2) **Граница — каталог входного файла** (та же, что у `read_file` и `IMPORT`: `compute_confinement_root`): абсолютный путь отклоняется ДО обращения к ФС; пустой путь, NUL и путь, оканчивающийся разделителем, `.` или `..`, отклоняются; родительский каталог цели берётся как `корень.join(путь)` БЕЗ лексической нормализации — поэтому `link/..` разрешается физически, а не «внутри» — обязан СУЩЕСТВОВАТЬ (каталоги не создаются) и после `canonicalize` лежать внутри канонизированного корня; для путей за пределами корня сообщение одно и то же независимо от того, существует ли каталог (запись не служит «оракулом существования» чужих путей). (3) **Цель**: существующий symlink (в т.ч. висячий), каталог и не-обычный файл (fifo, сокет, устройство) отклоняются; обычный файл перезаписывается целиком (с усечением); новый файл создаётся через `create_new` (не следует по symlink'у, появившемуся между проверкой и записью). Содержимое — байты UTF-8 строки дословно, перевод строки не добавляется. (4) Без файлового контекста (`run_source`, а не `run_source_file`) вызов — `RuntimeError` даже при выданном разрешении: без известного корня запись нельзя ограничить. Все отказы — управляемые `RuntimeError`, без panic. Что разрешение НЕ ограничивает (всё или ничего для каталога программы, гонки TOCTOU, жёсткие ссылки, неатомарность, нет лимита размера) — см. «Явно НЕ реализовано» ниже, TD-016 | `test_write_file_is_forbidden_by_default_and_names_both_ways_to_allow_it`, `test_write_file_permission_check_comes_before_any_path_check`, `test_write_file_absolute_path_is_rejected_even_when_it_points_into_the_root`, `test_write_file_parent_escape_is_rejected`, `test_write_file_dotdot_after_a_symlink_to_an_outside_directory_is_rejected`, `test_write_file_through_a_directory_symlink_pointing_outside_is_rejected`, `test_write_file_to_a_symlink_target_is_rejected_whatever_it_points_to`, `test_write_file_without_a_file_context_fails_even_with_permission`, `test_write_file_permission_covers_imported_modules_and_writes_into_the_entry_directory`, `test_write_file_permission_covers_every_file_in_the_directory_including_the_manifest`, `test_cli_allow_write_after_the_file_is_a_program_argument_and_grants_nothing`, `test_cli_without_the_flag_refuses_to_write_and_says_how_to_allow_it`, `test_cli_bare_and_dot_slash_entry_names_confine_writes_to_the_program_directory`, юнит-тесты `src/fs_sandbox.rs` (20, в т.ч. `a_directory_outside_the_root_is_refused_with_one_message_whether_or_not_it_exists`, `a_file_that_appeared_after_the_check_is_not_overwritten`, `a_symlink_that_appeared_after_the_check_is_not_followed`) |
| **Языковой сервер `sga lsp`: только анализ текста, ограниченный и изолированный ввод (T016, M007, 2026-10-24)** | Сервер принимает от редактора документы произвольного происхождения, поэтому его модель — «читать, но не исполнять». (1) **Программы не исполняются никогда**: диагностики дают лексер, парсер, semantic и typechecker (`diagnostics::analyze_source`; с T017 — все стадии, которые `sga run` проходит ДО исполнения, и ни одной после), а оптимизатор, кодогенератор, VM и builtin'ы в серверный путь не входят, поэтому `write_file`, `read_file`, `read_stdin()`, `PRINT` и бесконечные циклы в открытом документе безвредны; `IMPORT` не резолвится (в файле с `IMPORT` semantic и typechecker не запускаются — сервер не читает файлы, а имена из модулей были бы ложными ошибками; вместо них одно замечание), файловая система не затрагивается, стандартный ввод процесса принадлежит протоколу и программе не отдаётся. (2) **Входной поток ограничен**: строка заголовка ≤ 8 КиБ, ≤ 32 заголовков, тело ≤ 32 МиБ (заявленная длина больше — ошибка фрейминга сразу; память под заявленную длину заранее не выделяется, буфер растёт по мере прихода байтов — клиент, объявивший 32 МиБ и приславший три байта, не заставит выделить 32 МиБ; таймаутов чтения нет: время жизни stdio-сервера определяет породивший его процесс); значение `Content-Length` из не-цифр, дубликат, переполнение `u64` — ошибка. После ошибки фрейминга границы следующего сообщения неизвестны, поэтому сервер завершается (код 1), а не «пропускает и читает дальше». Собственный JSON-разбор (без внешних крейтов) ограничен вложенностью 128 — иначе глубокий `[[[[…` клиента ронял бы процесс `stack overflow`. (3) **Работа ограничена**: документ ≤ 1 МиБ (больше — одно предупреждение `limit`, анализ не выполняется), сообщение диагностики ≤ 1000 символов (сообщения парсера содержат `Debug` токена, и строка в мегабайт иначе уходила бы клиенту на каждое нажатие клавиши); **глубина дерева для semantic и typechecker ≤ 2000 уровней** (T017: обе стадии рекурсивны — ≈ 5,9 и ≈ 6,1 КиБ стека на уровень в отладочной сборке, а парсер не ограничивает длину цепочек `1+1+…`, `a[0][0]…`, `f()()…`; дерево глубже получает одно предупреждение `limit` вместо анализа, иначе документ в 1 МиБ ронял бы процесс неперехватываемым `stack overflow`; проверка глубины итеративна и сама стек не растит). (4) **Паника не завершает сервер**: `catch_unwind` вокруг анализатора (→ диагностика `internal`), обработки запроса (→ `-32603`) и обработки уведомления (→ запись в stderr). (5) **Стек**: анализ идёт в потоке `sga-lsp` со стеком 256 МиБ (при отказе системы — 64 МиБ, затем текущий поток с записью в stderr), потому что AST из длинной цепочки операторов освобождается рекурсивно (TD-018): документ в 1 МиБ из `1+1+…` занимает ≈ 56 МиБ стека в отладочной сборке (≈ 32 МиБ в release) — запас на основном стеке ≈ 4,6× (≈ 8×), на запасном 64 МиБ — лишь ≈ 1,14× (≈ 2×). (6) **stdout — только фреймы протокола**, всё остальное в stderr с префиксом `sga-lsp:`. (7) **Подсказки `hover`/`completion` (T018, M007, 2026-11-05)** строятся по тексту открытого документа теми же средствами защиты: ничего не исполняется и не читается; документ больше 1 МиБ — ответ `null` (ровно 1 МиБ — подсказки есть); паника при обработке запроса — `-32603`; параметры запроса проверяются строго (`-32602`), а огромные координаты прижимаются к концу текста. Документ, который сейчас набирают, синтаксически неверен, поэтому подсказки не опираются на парсер: лексические ошибки заменяются пробелами и текст лексируется заново не более 16 раз, дальше — «текстовый режим» без токенов (стоимость запроса не растёт от числа ошибок). Разбор — один проход по токенам со стеком областей в куче, рекурсии по размеру документа нет (проверено на 18 документах по 1 МиБ в потоке со стеком 512 КиБ: переменные, функции, структуры, скобки трёх видов, цепочки операторов, мусор, незакрытая строка, миллион пустых строк и др.). Замер до написания теста нашёл три квадратичных места (проверка повторных объявлений функций и структур, сбор видимых имён): на документах из десятков тысяч объявлений запрос занимал 3–7 с в отладочной сборке; после замены на хеш-множества самый медленный запрос — 0,84 с в отладочной сборке и 0,27 с в release, а тест держит бюджет 3 с на запрос. (8) **Навигация `documentSymbol`/`definition` (T019, M007, 2026-11-12)** устроена так же: по тексту открытого документа, по токенам и сканеру областей, без исполнения, чтения файлов и разбора импортов; документ больше 1 МиБ — `null` (ровно 1 МиБ — ответ есть); параметры проверяются строго (`-32602`). Новые поверхности и их защита: (а) ответ `documentSymbol` — дерево, а на испорченном тексте оно может быть вложено по размеру документа (`FN a() { FN a() { …` без закрывающих скобок), поэтому глубина ответа ограничена 16 уровнями (`MAX_SYMBOL_DEPTH`): более глубокий символ становится ребёнком самого глубокого разрешённого родителя, дерево строится без рекурсии, а его сериализация и освобождение не глубже 16 уровней; (б) парные скобки находятся одним проходом со стеком в куче, а не поиском на каждое объявление (вложенные объявления не дают квадрата); (в) перевод позиций в UTF-16 для ответа строится таблицей строк (`position::LineTable`) за один проход: собственный замер на документе из 16 тысяч функций нашёл квадратичный перевод позиций (19 с на запрос), после исправления самый медленный ответ на документах в 1 МиБ — 1,44 с в отладочной сборке (структура со 128 тысячами полей, в ответе 128 850 символов) и 0,52 с в release, самый долгий `definition` — 0,32 с и 0,14 с, а время растёт линейно (документ вчетверо больше — ответ в 3,7–4,9 раза дольше, допуск теста — десятикратный рост плюс 50 мс запаса); (г) проверено на 18 документах по 1 МиБ в потоке со стеком 512 КиБ (оба вида ответа `documentSymbol`, `definition` в начале, середине и конце; бюджет 5 с на запрос, самый медленный — 0,71 с в отладочной сборке и 0,23 с в release), на семи формах с самым крупным или самым вложенным ответом и на четырёх формах настоящим процессом по stdio (код 0, stderr пуст). Кэша нет — каждый запрос заново разбирает текст, поэтому частота запросов клиента не ограничена (TD-020). Что НЕ ограничено: число одновременно открытых документов, суммарная память документов, частота сообщений клиента (TD-020); клиент, которому вы доверяете запуск `sga lsp`, и так может запустить любой процесс — сервер защищает от содержимого документов, а не от злонамеренного клиента | `lsp_test`: `the_server_only_analyzes_and_never_runs_the_document`, `framing_violations_stop_the_server_at_once_with_code_one`, `a_document_over_the_size_limit_gets_one_warning_instead_of_analysis`, `a_document_of_half_a_million_operators_gets_a_depth_warning_without_crashing`, `diagnostic_messages_are_bounded_in_length`, `bad_messages_get_error_replies_and_the_server_keeps_working`; T017: `one_mebibyte_documents_of_every_deep_construct_do_not_crash_the_server`, `the_depth_limit_decides_between_analysis_and_a_warning_over_the_wire`, `a_document_with_import_gets_one_information_notice_over_the_wire`, `diagnostics::tests::analysis_at_the_depth_limit_fits_into_a_quarter_of_the_fallback_stack`; юнит-тесты `src/lsp/` (`analyzer_panic_becomes_an_internal_diagnostic_and_the_server_survives`, `request_panic_becomes_internal_error_with_the_request_id`, `notification_panic_is_logged_and_does_not_stop_the_server`, `stdout_side_carries_frames_only`, `the_work_runs_on_a_dedicated_thread_with_a_large_stack`, тесты пределов `transport`/`json`); T018: `lsp_assist_test`: `requests_on_a_one_mebibyte_document_are_fast_and_do_not_recurse`, `hover_and_completion_survive_damaged_programs`; `lsp_test`: `hover_and_completion_answer_over_the_wire_for_a_document_being_typed`, `hover_and_completion_positions_are_utf16_over_the_wire`; юнит-тесты `lsp::tests`: `an_oversized_document_gets_null_and_the_limit_is_inclusive`, `malformed_query_params_are_invalid_params_with_the_same_id`, `huge_coordinates_are_clamped_to_the_end_instead_of_failing`, `the_server_still_answers_queries_after_a_panicking_request`, `queries_do_not_publish_anything_and_do_not_touch_the_documents`; `lsp::scan::tests::text_mode_and_non_code_states_give_an_empty_context`; T019: `lsp_assist_test`: `navigation_on_a_one_mebibyte_document_is_fast_and_does_not_recurse`; `lsp_nav_test`: `one_mebibyte_shapes_are_served_on_a_small_stack_within_the_budget`, `document_symbol_time_grows_linearly_with_the_document`, `damaged_programs_keep_the_navigation_invariants`, `every_prefix_of_a_program_keeps_the_navigation_invariants`; `lsp_test`: `one_mebibyte_documents_get_navigation_answers_without_crashing_the_server`; юнит-тесты: `lsp::tests::a_ladder_of_unclosed_declarations_gives_a_bounded_answer`, `lsp::tests::navigation_works_exactly_at_the_size_limit_and_is_off_above_it`, `lsp::tests::malformed_document_symbol_params_are_invalid_params_with_the_same_id`, `lsp::nav::tests::nesting_is_capped_so_a_deep_answer_stays_shallow`, `lsp::position::tests::the_line_table_handles_a_long_line_with_astral_characters_in_constant_time` |

## Явно НЕ реализовано в v0.1 (см. ROADMAP)

- **Null safety на уровне типов**: `NIL` существует как значение, но нет
  типобезопасного `Option<T>`, различающего "может быть nil" и "не может".
  Любая переменная может стать `Nil` в runtime без статической проверки.
- **Safe concurrency**: нет потоков/async — соответственно, и нет
  гонок данных, но и нет самой конкурентности.
- **Sandbox execution / permission system для самих SGA-программ**: у
  SGA-программ теперь ЕСТЬ файловый I/O — `read_file` (T009, M003,
  2026-08-16, см. строку "read_file confinement" выше) и `write_file`
  (T015, M006, 2026-10-18, см. строку «`write_file`» выше), но это НЕ
  полноценная permission-система: единственная граница — фиксированный
  каталог входного файла программы (та же, что у `IMPORT`), без
  возможности разрешить/запретить конкретные поддиректории тоньше этого;
  единственное разрешение — одно, бинарное, на запись (`--allow-write`:
  всё или ничего для каталога программы), чтение разрешения не требует;
  без удаления/переименования файлов и создания каталогов, без сетевого
  I/O (`std/net`, `std/http` не реализованы). Разграничение видов I/O в
  этом документе: `IMPORT` —
  I/O самого компилятора/резолвера (случается ДО исполнения программы,
  во время резолвинга модулей); `read_file` — I/O САМОЙ программы
  (случается ВО ВРЕМЯ исполнения, по явному вызову из SGA-кода) — оба
  используют одну и ту же confinement-границу, но это две разные точки
  входа в пайплайне. `write_file` (T015) — тоже I/O самой программы во время
  исполнения, но единственный, изменяющий файловую систему, поэтому он
  дополнительно закрыт явным разрешением запустившего (`--allow-write`), а не
  только границей каталога. `pkg:`-импорты (T011) читают файлы ВНЕ каталога
  входного файла по построению (каталог зависимости объявлен в
  `sga.toml`) — их граница это каталог пакета, см. строку "`pkg:`-импорты"
  выше; `read_file` из SGA-программы по-прежнему ограничен каталогом
  входного файла и каталоги пакетов не расширяют его границу. `args()`
  (T012) — не I/O программы, а данные от запустившего (см. строку
  «Аргументы запуска» выше), поэтому в это разграничение не входит.
  `read_line()`/`read_stdin()` (T013) — I/O самой программы во время
  исполнения, но не файловый: это тот поток, который запустивший подключил
  к процессу (CLI — stdin процесса, библиотека — пустой ввод или заданные
  байты), см. строку «Стандартный ввод» выше. `time_now_ms()`/
  `time_monotonic_ms()` (T014) — чтение часов процесса: не файловый и не
  сетевой I/O, границы не касаются, см. строку «Время» выше.
- **Защита от бесконечных циклов / instruction budget в VM**: у VM есть
  лимит на ГЛУБИНУ вызовов (`max_call_depth = 200`, см. выше), но нет
  лимита на ОБЩЕЕ количество исполненных инструкций — `WHILE ИСТИНА {}`
  выполняется неограниченно долго, потребляя 100% CPU одного потока без
  тайм-аута. Не является уязвимостью при локальном однопользовательском
  запуске (тот же процесс, что и сам пользователь, мог бы написать
  `while true {}` и на любом другом языке), но становится DoS-вектором
  в любом сценарии, где SGA-программы из недоверенного источника
  исполняются в общем процессе/сервисе (онлайн-площадка, CI). Открытый
  пункт — реализация требует решений по умолчанию по бюджету, обратной
  совместимости API `Vm::new`/`Vm::run` и поведению при превышении
  лимита (см. docs/ROADMAP.md).
- **Лимит на размер ввода и потребление памяти**: `read_stdin()` (T013) и
  `read_file` (T009) читают данные целиком, без верхней границы, а
  `read_line()` не ограничивает длину строки. Бесконечный источник на входе
  `read_stdin()` читался бы, пока не кончится память (не воспроизводилось —
  измерены только конечные вводы). Измерено на release-бинарнике: ввод
  128 МиБ — пик ≈259 МиБ у `read_stdin()`, у `read_line()` на строке в
  128 МиБ и у `read_file` на файле в 128 МиБ (≈2× от размера: буфер растёт
  удвоением); построчный цикл `read_line()` по 128 МиБ (2 млн строк) — ≈11
  МиБ, т.е. память не зависит от размера ввода. Как и отсутствие
  instruction budget, не уязвимость при локальном запуске собственных
  программ, но DoS-вектор для недоверенных программ в общем процессе.
- **Границы записи файлов (`write_file`, T015) — что разрешение НЕ
  ограничивает.** (а) Разрешение `--allow-write` бинарное и выдаётся на
  программу ЦЕЛИКОМ и на весь каталог входного файла: программа с
  разрешением может перезаписать любой обычный файл в этом каталоге и его
  подкаталогах — в том числе собственный исходник и лежащий в том же
  каталоге манифест `sga.toml`; код `pkg:`-зависимостей и импортированных
  модулей получает ту же возможность (они инлайнятся в одну программу). Нет
  списка разрешённых путей или подкаталогов. (б) Гонки между проверкой и
  записью (TOCTOU, TD-016): другой процесс с доступом к каталогу может
  подменить каталог или существующий файл symlink'ом уже после проверки. Для
  НОВОГО файла окно сужено — `create_new` не следует по symlink'у,
  появившемуся в этом месте, — но для перезаписи существующего файла и для
  промежуточных каталогов защиты нет: полная защита требует
  `openat`/`O_NOFOLLOW`, то есть платформенного кода, которого нет в
  переносимом `std`. (в) Жёсткие ссылки не обнаруживаются: файл, имеющий
  вторую ссылку вне каталога программы, будет изменён вместе с ней. (г)
  Запись не атомарна и не вызывает `fsync`: сбой посреди записи оставляет
  частично записанный файл, а перезапись сначала усекает файл. (д) Нет
  лимита на размер содержимого и квот: программа с разрешением может
  заполнить диск (тот же класс, что отсутствие instruction budget и лимита
  ввода выше). (е) Имена, особенные для Windows (зарезервированные имена,
  потоки ADS), не проверяются, а тесты с symlink'ами выполняются только на
  unix — Windows не входил в проверку T015. Не уязвимость при запуске СВОИХ
  программ с осознанно выданным разрешением; для недоверенных программ
  запускайте их без разрешения (по умолчанию) или в отдельном каталоге.
- **Статическая типобезопасность**: типы проверяются только в runtime.

## Известные ограничения, важные для безопасности

- **Глубокая неизменяемость массивов проверяется только на этапе
  семантического анализа, не повторно в VM** (в отличие от обычного
  `StoreVar`, который проверяется и в `semantic`, и в `vm::assign`).
  Причина: к моменту выполнения `IndexAssign`/`push` в VM значение уже
  снято со стека как анонимный `Value::Array` — имя переменной и её
  флаг `mutable`, через которые оно было загружено, к этому моменту уже
  недоступны (стек хранит только значения, не привязки). Поэтому защита
  работает как отказ **компиляции**, а не как runtime-проверка — это и
  сильнее (ошибка ловится раньше).
- Поиск этой границы выявил и закрытую дыру: до фикса в этом же разделе
  кода `let arr = [1,2,3]; arr[0] = 99;` компилировалось и исполнялось
  без единой ошибки. Исправлено и закрыто регрессионными тестами (см.
  выше) — это конкретный пример, а не общая фраза.
- **ЗАКРЫТО (Ownership/Borrowing, см. docs/ROADMAP.md):** ранее граница
  вызова функции "обнуляла" immutability аргумента — параметры были
  всегда мутабельны, и `let frozen = [...]; mutate(frozen);` мог тихо
  менять `frozen` через тело `mutate`. Теперь параметры immutable по
  умолчанию; мутация требует явного `MUT` в сигнатуре **и** `VAR` на
  месте вызова — проверяется и в `semantic`, и повторно в `vm::call`
  (defense-in-depth, как и для прочих immutability-гарантий). См.
  `test_immutability_bypass_through_function_parameter_is_now_fixed`,
  `test_passing_immutable_variable_to_mut_parameter_is_rejected`.

- `And`/`Or` не делают short-circuit на уровне байткода (см.
  `docs/VM_SPEC.md`) — компилятор сначала компилирует оба операнда.
  Это не уязвимость, но может удивить при наличии побочных эффектов.
- **Самореференцирующиеся массивы** (`numbers[0] = numbers;`,
  `push(arr, arr)`) — это было реально протестировано на этапе разработки
  и **обнаружило настоящий баг**: до фикса печать такого массива (`PRINT`)
  вызывала неограниченную рекурсию в `Display` и аварийное завершение
  процесса (`stack overflow`, не управляемая ошибка). Исправлено введением
  ограничения глубины рекурсии (`MAX_DISPLAY_DEPTH = 64`) в
  `src/runtime/mod.rs::fmt_value` — после 64 уровней вложенности печать
  завершается `[...]}` вместо ухода в бесконечную рекурсию. Сама
  циклическая структура данных при этом не запрещена семантически (это
  было бы отдельной фичей — borrow-checker/occurs-check), запрещено
  только её бесконтрольное обхождение при выводе.

## Регрессия, найденная и закрытая при слиянии веток (см. MIGRATION_REPORT.md)

При объединении ветки с замыканиями/модулями с веткой Ownership/Borrowing
обнаружилось, что в ветке с замыканиями `Param`/`FunctionDef` лишились
поля `mutable`/`param_mut`, а `Vm::call` хардкодил `(значение, true)`
для ВСЕХ параметров вызываемой функции независимо от сигнатуры — то
есть **именно та дыра immutability bypass, что описана выше как
"ЗАКРЫТО", была повторно открыта** в момент добавления замыканий (не
специально, а как побочный эффект удаления поля из структуры данных,
которое больше нигде явно не использовалось в той ветке). При слиянии
`param_mut` восстановлен и в `ast::Param`, и в `runtime::FunctionDef`, и
в `vm::Vm::call`; добавлен дополнительный тест
`test_vm_enforces_param_immutability_even_if_semantic_is_bypassed`,
проверяющий защиту на уровне VM напрямую через сконструированный
байткод (минуя semantic), чтобы повторное открытие этой же дыры было
обнаружено даже в случае будущей регрессии semantic-уровня.

## Слияние со structs-веткой (этот раунд, см. MIGRATION_REPORT.md)

- **Identity-equality для reference-типов**: `Value::Array` в этой
  кодовой базе никогда не имел identity-equality в операторе `==`/`!=`
  (`values_equal` падал в `_ => false` для любой пары `Array` —
  настоящая, независимая от слияния регрессия). Найдена при переносе
  той же логики для нового `Value::Struct` из донорской ветки. Исправлено
  одновременно для обоих типов: `(Array(a), Array(b)) => Rc::ptr_eq(a,
  b)`, аналогично для `Struct`. Семантика: `[1,2]==[1,2]` -> `ложь` (два
  разных объекта), `let a=[1,2]; let b=a; a==b` -> `истина` (один
  объект) — стандартная reference-семантика, как в Python/JS/Java для
  объектов (НЕ value-семантика `==` для примитивов/строк).
- **Глубокая неизменяемость структур** реализована по той же модели, что
  и для массивов (см. выше): `p.field = ...` через `LET`-связанную
  переменную отклоняется на этапе semantic
  (`semantic::Analyzer::mutation_root`, расширен на `Expr::FieldAccess`)
  — НЕ повторно в VM, по той же причине, что и для массивов (к моменту
  `SetField` в VM привязка к имени переменной уже потеряна).
- **Известное ограничение: borrow-checking для `.method()` — только на
  уровне тела функции, не на уровне места вызова.** Для обычных функций
  `check_call_borrows` (semantic) статически проверяет, что аргумент,
  передаваемый в `MUT`-параметр, является `var`-связанным именем. Для
  `.method()`-вызовов это невозможно реализовать тем же способом: имя
  вызываемой функции (`{TypeName}_{method}`) зависит от рантайм-типа
  получателя, а структуры не имеют статического типа в v0.1 (нет
  `TypeAnnotation::Struct`, см. `docs/LANGUAGE_SPEC.md`, §7). Это
  означает, что `LET p = Point{...}; p.move(1,1);`, где `Point_move`
  объявлен с `MUT self`, **компилируется** (в отличие от аналогичного
  случая с обычной функцией и явным `MUT`-параметром, который был бы
  отклонён `check_call_borrows`) — однако сама мутация `self.x = ...`
  внутри `Point_move` всё равно требует, чтобы `Point_move` была
  объявлена с `MUT self`; без этого она отклоняется НЕЗАВИСИМО от
  способа вызова. Практический эффект: защита смещена с "места вызова"
  на "сигнатуру метода" — слабее, чем для обычных функций, но не
  отсутствует полностью. Закрытие этого ограничения требует
  статических типов для structs (см. `docs/ROADMAP.md`) и выходит за
  рамки v0.1.



Значение `Vm::max_call_depth` было эмпирически проверено (а не
выбрано "на глаз") путём запуска неограниченно рекурсивной SGA-программы
в потоке с явно заданным размером стека. Результат:

| Размер стека потока | `max_call_depth = 2000` | `max_call_depth = 200` |
|---|---|---|
| 8 МиБ (типичный `ulimit -s` на Linux) | **реальный stack overflow процесса** (SIGABRT) — управляемая `RuntimeError` НЕ успевает сработать | управляемая `RuntimeError`, процесс жив |
| 16 МиБ | реальный stack overflow процесса | управляемая `RuntimeError`, процесс жив |
| 64 МиБ | управляемая `RuntimeError`, процесс жив | управляемая `RuntimeError`, процесс жив |

Таким образом, исходное значение `2000` (унаследованное от родительской
ветки без изменений) было неэффективной защитой при реалистичном для
production размере стека потока: к моменту, когда счётчик глубины
вызовов добрался бы до 2000, нативный стек Rust уже был исчерпан, и
управляемая ошибка просто не успевала сработать — защита существовала
только на бумаге. Значение `200` подтверждено безопасным даже при 8 МиБ
(см. `test_recursion_depth_limit_is_safe_under_default_sized_stack`).
Это измерение справедливо для **debug**-сборки на данной машине/версии
Rust; стоимость кадра стека зависит от платформы, версии компилятора и
профиля сборки (`release` обычно компактнее за счёт инлайнинга), поэтому
встраивающим SGA в собственный сервис рекомендуется ВСЕГДА запускать
непроверенный SGA-код в потоке с явно заданным, заведомо щедрым размером
стека (например, через `std::thread::Builder::stack_size`), а не
рассчитывать только на `max_call_depth` как единственную линию защиты.
