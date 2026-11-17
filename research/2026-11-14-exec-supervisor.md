# Exec supervisor: клиент остаётся живым, чтобы панельный shell не забрал stdin

Дата: 2026-11-14. Репозиторий: `C:\Users\WorkShop1\proj\jcode-src`.
Коммиты: `b28373673` (фикс), `cec14bfbb` (тесты правила глубины).
Ветка: `fix/transport-access-denied-means-present`.

## Дефект

В herdr-панели (Windows, ConPTY) панельный `pwsh` запускает `jcode.exe` как
native-команду и **ждёт** её. При `/restart`, `/reload`, `/rebuild`, `/update`
клиент спавнил преемника и сразу делал `std::process::exit(0)`
(`crates/jcode-base/src/platform.rs`, `replace_process_reparented`).

Последовательность:

1. старый клиент спавнит преемника и выходит;
2. панельный `pwsh` просыпается, возвращается к prompt и начинает читать
   консольный ввод;
3. преемник подключается к серверу и рисует TUI, но ввод уже забрал `pwsh`.

Симптом: TUI нарисован, клавиши echoed терминалом, composer пуст.

herdr исправить не может: он пишет ввод байтами в pipe ConPTY и не сбрасывает
cooked/raw режимы консоли, поэтому выигрывает тот, кто читает буфер ввода в
момент. Механизма «вернуть ввод преемнику» у мультиплексора нет.

## Фикс

Исходящий клиент **не выходит** сразу после spawn преемника, а остаётся жить
как idle-супервизор:

| Свойство | Как обеспечено |
|---|---|
| не читает stdin | crossterm `EventStream` дропается при возврате из TUI, reader-thread останавливается |
| не держит серверное соединение | сокет закрывается вместе с TUI, преемник подключается без takeover-конфликта |
| не пишет в терминал | режимы консоли остаются теми, что ждёт преемник |
| не отпускает панель | панель по-прежнему занята клиентом |

Ожидание опирается на **два независимых сигнала**, и супервизор выходит, только
когда оба говорят «клиента нет»:

- `successor_alive` — жив ли PID преемника;
- `console_has_client` — есть ли в консоли панели другой экземпляр
  образа преемника (`GetConsoleProcessList` + имя образа).

Второй сигнал нужен для **цепочек**: когда преемник сам пере-исполняется, его
PID умирает, но следующий клиент уже в консоли. Ожидание только по PID
выпустило бы супервизор посреди цепочки, и панельный shell снова забрал бы
ввод у живого клиента.

Глубина цепочки передаётся через `JCODE_SUPERVISOR_DEPTH`. Супервизором может
быть только клиент, запущенный панельным shell (глубина 0): преемник
(глубина 1) выходит сразу после своего spawn — его завершение не будит shell,
потому что shell ждёт именно клиента глубины 0.

Таймаут супервизора — 12 часов: это страховка от нечитаемой консоли, он не
должен прерывать живую сессию.

## Файлы

- `crates/jcode-base/src/exec_supervisor.rs` — новый модуль: глубина, решение
  о супервизоре, ожидание по двум сигналам.
- `crates/jcode-base/src/platform.rs` — `replace_process_reparented`: spawn,
  передача глубины, `supervise_replacement`.
- `crates/jcode-base/src/platform_reparent.rs` — `parent_is_console_shell`,
  `console_has_other_process_with_name`.
- `crates/jcode-base/src/lib.rs` — `pub mod exec_supervisor`.

## Совместимость с herdr 0.9.5

Проверено по исходникам herdr (`D:\herdr-src`, read-only) в
`~/.jcode/scratch/herdr_two_clients_detect.md`:

- herdr **не распознаёт `jcode` как агента по имени процесса**
  (`src/detect/mod.rs:200-247`: список агентов не содержит `jcode`). Поэтому
  foreground job панели остаётся `pwsh` и при одном, и при двух, и при десяти
  `jcode.exe` в панели. Два процесса в панели безопасны.
- `PaneDied` (и удаление панели) вызывается только по смерти **прямого**
  потомка панели — `pwsh` (`src/pane.rs:2737-2768`). Смерть `jcode.exe` его не
  вызывает, панель не закрывается и не перезапускается.
- Регистрация jcode в herdr снимается по признаку «панель вернулась к
  шеллу» (`src/pane.rs:386-412`, `src/platform/windows.rs:1692-1708`): пока
  жив супервизор (потомок pwsh), список потомков непуст, панель не считается
  вернувшейся к шеллу — регистрация агента сохраняется. Это ровно то поведение,
  которое нужно: панель остаётся агентной, пока в ней работает клиент.

## Проверки

- `cargo check -p jcode-base --profile selfdev` — чисто.
- `cargo test -p jcode-base --profile selfdev --lib -- exec_supervisor platform_reparent`
  на финальном HEAD (`cec14bfbb`): **29 passed, 0 failed** (10 `exec_supervisor`
  + 19 `platform_reparent`), плюс 1 интеграционный тест
  (`reparented_child_gets_valid_standard_handles`).
- `selfdev build target=tui` — успешно; бинарь содержит строки
  `supervising replacement`, `exiting without supervision` и
  `JCODE_SUPERVISOR_DEPTH`.
- `harbor_check.py` — `checks=29 failures=0 desync_failures=0`,
  `ecosystem OK: client and daemon both run cec14bfbb-dirty-cc06d2aa916f`.
- `harbor_reconcile.py --dry-run --json` — `ok: true`, `check_rc: 0`,
  `desynced: false`, `needs_build: []`, `needs_apply: []`, `landed=9`.

## Живая приёмка в herdr-панели

Стенд: изолированный сервер (`jcode server --socket <path>`) + изолированная
рабочая поверхность herdr (`herdr workspace create`, без `pane split`),
клиент запускается в панели, проба ввода — `herdr pane send-text` без Enter,
затем `herdr pane read --source visible --format text`.

Критичная деталь методики: **перед каждой пробой composer надо очищать**
(`ctrl+u`). Иначе маркер склеивается со следующей командой
(`ZZBASE_..._END/restart`) и `/restart` уходит как обычный текст, а не как
команда. Первый прогон приёмки это показал: `after_in_composer=True`, но
ре-эксп не происходил вовсе.

Отрицательный контроль: опубликованная сборка **до** фикса. Важно, что версия
`6189ea428-dirty-d78357851735` — грязная сборка того же дерева и в ней
**уже есть** код супервизора (`findstr` по бинарю находит `supervising
replacement`), поэтому она бесполезна как контроль. Подходящий бинарь —
`6189ea428-dirty-ea4dbe7a5abf` (14.11 22:43), в нём этой строки нет.

Проверка бинаря на наличие фикса — обязательный шаг, иначе стенд не
различает состояния:

```
findstr /M /C:"supervising replacement" <build>\jcode.exe
```

## Освобождение панели

Супервизор держит панельный шелл в ожидании, поэтому он обязан выйти, когда
клиента в консоли больше нет. Иначе панель осталась бы заблокированной.

Проверяется так: в панели создаётся пара (супервизор + преемник) через
`/restart`, затем убивается только преемник. Супервизор должен заметить это
сам (по двум сигналам: PID преемника мёртв и клиента в консоли больше нет) и
выйти, вернув панель промпту шелла.

Прогон (`_accept_exec_supervisor_release.ps1`), workspace `w6W`, pane `w6W:p1`:

```
client_pid=57136 shell_pid=41408
== issuing /restart to create the supervisor ==
pair_count_after_restart=2
supervisor_pid=57136 successor_pid=56100
== killing the successor client ==
supervisor_exited_on_its_own=True
shell_prompt_visible=True
```

Вердикт: супервизор вышел сам, панель вернула промпт шелла. Панель не
застревает.

Важная деталь сценария: супервизор существует только ПОСЛЕ exec, поэтому
«просто убить клиента» проверку не создаёт — нужен сначала `/restart`.

## Живая приёмка: результат

Полный отчёт: `~/.jcode/scratch/acceptance_exec_supervisor.md`.

| | бинарь с фиксом | бинарь без фикса (`6189ea428-dirty-ea4dbe7a5abf`) |
|---|---|---|
| baseline ввод в composer | True | True |
| ввод после `/restart` в composer | **True** | **False** |
| что на экране | `1> ZZAFTER_1166891569_END` | `^UZZNEGAFTER_1264422_ND` (cooked-echo pwsh) |
| супервизор в логе | `supervising replacement 36744` | отсутствует |

Стенд различает состояния, поэтому позитивный результат доказывает фикс, а не
совпадение.

## Живая приёмка: цепочка двух `/restart` в одной панели

Открытый вопрос из первой приёмки: переживает ли супервизор цепочку, когда
преемник сам пере-исполняется.

Прогон `_accept_chain2.ps1`, workspace `w6Z`, pane `w6Z:p1`, панельный шелл
`pwsh.exe` = pid 37192.

| момент | процессы панели (родитель 37192) | ввод |
|---|---|---|
| до рестарта | `2004` (клиент) | `ZZBEFORE...` в composer |
| после 1-го `/restart` | `2004` (супервизор, жив) + `44096` (преемник) | `ZZAFTERFIRST...` в composer |
| после 2-го `/restart` | `2004` (супервизор, жив) + `56120` (новый преемник) | `ZZAFTERSECOND...` в composer |

`44096` (преемник глубины 1) после второго `/restart` вышел сразу — по правилу
глубины. Супервизор `2004` пережил оба рестарта.

Лог клиента (`jcode-2026-11-15.log`):

```
[01:36:54.064] replace_process: replacement process 44096 reparented to our parent
[01:36:54.090] replace_process: supervising replacement 44096 to keep the pane shell waiting
[01:37:47.690] replace_process: replacement process 56120 reparented to our parent
[01:37:47.690] replace_process: exiting without supervision of replacement 56120
```

Это ровно задуманная картина: супервизор один на всю цепочку, преемник
глубины 1 не супервизорит, его выход не будит панельный шелл.

Вердикт: **ЦЕПОЧКА РАБОТАЕТ**. Все три пробы — ввод в composer, shell-echo
(`^U...`) отсутствует. Отчёт: `~/.jcode/scratch/chain2-report.txt`.

Методическая деталь: `Select-String` на этом логе (68 МБ) молча не находит
строки, а `findstr` находит. Для больших логов jcode проверять надо через
`findstr`, иначе «не нашлось» — это артефакт инструмента, а не отсутствие
события.

Методическая деталь №2: общий подсчёт `jcode.exe` по всей машине даёт
ложные «пары» от других сессий. Структуру панели надо читать по
`ParentProcessId` конкретного панельного шелла.

## Harbor: состояние на 2026-11-15

Оба коммита зарегистрированы и живут в рантайме:

| запись | коммит | состояние | сборка |
|---|---|---|---|
| `fix_b28373673_keep` | `b28373673` | landed | `b28373673-dirty-4ab034498148` |
| `fix_cec14bfbb_cover` | `cec14bfbb` | landed | `cec14bfbb-dirty-cc06d2aa916f` |

Каналы синхронны: `current == shared-server == cec14bfbb-dirty-cc06d2aa916f`,
`desynced: false`, `needs_build: []`, `needs_apply: []`, `landed=9`.

Первая сборка `cec14bfbb` была помечена `superseded` («source drift detected»):
дерево менялось, пока шла сборка. После пересборки на стабильном дереве —
`completed` + `published`.

### `harbor_check.py` давал FAIL — и это было не про фикс

Промежуточное состояние (после apply сборки):

```
checks=29 failures=2 desync_failures=0
FAIL fix_b28373673_keep: contributor session_dragon_... recovery status=pending
FAIL fix_cec14bfbb_cover: contributor session_dragon_... recovery status=pending
```

`desync_failures=0` — экосистема была согласована, оба фикса live. Причина
FAIL: reload, который Harbor сам выполнил при apply, прервал
сессию-контрибьютора. Сервер создал durable-запись восстановления
(`~/.jcode/reload-recovery/session_dragon_*.json`, `status=pending`).

Механика подтверждения (`crates/jcode-app-core/src/server/reload_recovery.rs`):

- запись помечается `delivered`, когда клиент принимает сообщение, содержащее
  текст продолжения (`client_lifecycle.rs:3757`
  `mark_delivered_if_matching_continuation`);
- у клиента напоминание висит в очереди
  (`queued_messages.hidden_count = 1`, 202 байта) и уходит, как только сессия
  перестаёт быть busy (`remote.rs:442-461`: отправка только при
  `!app.is_processing`);
- `resume_all_sessions` тоже умеет подтверждать запись, но требует, чтобы
  сессия была **не** mid-turn: вызов при 7 занятых сессиях дал `skipped=7`.

Итог: запись стала `delivered` (`delivered_at: 2026-11-15T03:06:33Z`), и
`harbor_check.py` дал `checks=29 failures=0 desync_failures=0`,
`harbor_reconcile.py` — `ok: true, check_rc: 0`.

Вывод: `recovery status=pending` в Harbor-приёмке — это НЕ дефект фикса. Это
следствие того, что Harbor сам перезагрузил сервер под контрибьютором. Проверять
этот счётчик нужно, но читать его как «фикс не приземлился» — неверно: о приземлении
говорят `published OK`, `ancestry OK`, `live OK` по конкретному коммиту.

## Не проверено (по итогам приёмки)

- `/reload`: в отличие от `/restart`, он требует более нового бинаря и не
  детерминирован. Приёмка шла на `/restart` — общем для обоих путей exec.
- Цепочка из двух последовательных `/restart` в одной панели проверена живым
  прогоном: супервизор пережил оба рестарта, ввод на всех трёх пробах попал в
  composer.
- Ctrl+C в супервизоре.
- Ввод в панели без herdr (обычный терминал), где родитель - иной процесс.
- Поведение на Unix: код супервизора там недостижим (`exec()` заменяет
  процесс), ветка `#[cfg(not(windows))]` не компилировалась под Linux.
