# Полная миграция Jcode на другой компьютер

Дата подготовки: 2026-09-14
Статус: план готов по фактической конфигурации WorkShop1; фактический перенос еще не выполнялся.
Цель: перенести Jcode, Cline, память, сессии, TODO, правила, MCP, локальные сервисы и рабочие проекты так, чтобы на новом компьютере сохранились связи и функции, а секреты не утекли в архив, git, логи или отчеты.

## 0. Главный вывод

Полная миграция возможна, но это не копирование одного exe или одной папки. Нужны пять слоев:

1. Jcode и его пользовательское состояние.
2. Cline и его рабочее состояние.
3. Проекты, git worktrees и незакоммиченные изменения.
4. Локальная инфраструктура: Docker, OmniRouteV2, Karing, Honcho, Ollama, SearXNG, Infigraph.
5. Внешние интеграции: n8n Cloud, Telegram, MCP, браузеры, SSH и MAIN/STANDBY-роль.

Нативный `/handoff` для переноса живой сессии между хостами пока не реализован. `REMOTE_HANDOFF.md` описывает дизайн, но интерфейсы `/handoff`, `/attach` и event-cursor replay не являются готовой функцией. Поэтому безопасная миграция делается после остановки активных процессов и контрольного snapshot, а не во время выполняемого turn.

## 1. Правила безопасности

- Не удалять исходный компьютер, Docker volumes или старые резервные копии до завершения acceptance-проверки и срока удержания rollback-копии.
- Не переносить ключи в этот документ, git, командную строку, PowerShell history, stdout или TODO.
- Секреты переносить отдельно через password manager, vault, зашифрованный канал или повторную авторизацию.
- Перед переносом считать секрет скомпрометированным, если он когда-либо хранился в raw-конфиге, транскрипте или незащищенном backup-файле. Такие credentials должны быть вынесены в vault и при необходимости ротированы.
- Не запускать одновременно две MAIN-роли Cline-инфраструктуры. Это может создать split-brain, дубли Telegram polling и конкурирующие записи памяти.
- Не тестировать production Telegram отправкой сообщений без заранее согласованных тестовых bot token, chat ID и user ID.
- Ничего не подменять по названию провайдера: `OmniRouteV2` и `OpenRouter` должны рассматриваться как разные уровни. Локальный OmniRoute endpoint не означает прямой вызов OpenRouter API.

## 2. Снимок исходной машины

Перед остановкой процессов создать приватный manifest с версиями, путями, хешами и размерами. В manifest не включать содержимое секретных файлов.

### 2.1 Версии и системные сведения

Сохранить результаты следующих проверок в приватный файл:

```powershell
jcode --version
Get-Command jcode
$PSVersionTable.PSVersion
[Environment]::OSVersion.Version
[Environment]::GetEnvironmentVariable('JCODE_HOME','User')
[Environment]::GetEnvironmentVariable('JCODE_RUNTIME_DIR','User')
docker version
wsl --status
```

Также зафиксировать:

- архитектуру Windows, сейчас это x86_64;
- текущий Jcode: v0.84.0, commit `57d587899`;
- установочные пути `%LOCALAPPDATA%\jcode\...`;
- профиль пользователя и доступный объем диска;
- наличие Git, Rust, PowerShell и Visual Studio Build Tools, если нужен source build.

### 2.2 Инвентарь текущего состояния

По текущему аудиту:

| Объект | Текущий путь | Оценка | Действие |
|---|---|---:|---|
| Jcode state | `C:\Users\WorkShop1\.jcode` | около 310 MB | переносить state после остановки Jcode; caches можно переносить отдельно |
| Cline state | `C:\Users\WorkShop1\.cline` | около 6.5 GB | переносить по allowlist и отдельными крупными архивами |
| Jcode repo | `C:\Users\WorkShop1\proj\jcode` | около 11.93 GB | clone/копия exact commit плюс отдельный snapshot worktree |
| Cline source | `C:\Users\WorkShop1\proj\cline-source` | около 3.04 GB | переносить только если нужен локальный source/development |
| OmniRoute source | `C:\Users\WorkShop1\proj\omniroute-source` | около 1.19 GB | переносить при необходимости разработки/локального аудита |
| Telegram research | `C:\Users\WorkShop1\proj\telegram-research` | около 58.5 MB | переносить как проектный архив |
| n8n MCP state | `C:\Users\WorkShop1\.n8n-mcp` | небольшой локальный state | проверить allowlist и секреты перед переносом |
| Docker data | настроено на D: через junction | крупный | переносить через Docker volume export, не копировать внутреннюю БД Docker вручную |
| Cline local data | `C:\Users\WorkShop1\.cline\data` | sessions, kanban, cron, logs, workspaces | переносить после quiesce |

Текущий `C:\Users\WorkShop1\proj` не содержит подтвержденных копий старых каталогов `D:\watcher`, `D:\Notes\10-Projects\watcher-v5`, `D:\tg_bot_replacer`, `D:\order-page-project` и `D:\print-layout-app`. Они должны быть найдены на исходном диске, другом компьютере или в backup до объявления полной миграции завершенной.

### 2.3 Git и незакоммиченные изменения

На момент подготовки плана репозиторий `proj\jcode` был грязным. Эти изменения не принадлежат автоматически текущему плану и не должны случайно попасть в commit:

```text
M crates/jcode-app-core/src/channel.rs
M crates/jcode-base/src/config.rs
M crates/jcode-base/src/config/default_file.rs
M crates/jcode-base/src/config/env_overrides.rs
M crates/jcode-base/src/telegram.rs
M crates/jcode-config-types/src/lib.rs
?? .infigraph/
?? .infigraphignore
```

Перед миграцией нужно сохранить отдельно:

```powershell
git status --short
 git diff --binary > ..\jcode-worktree.patch
 git ls-files --others --exclude-standard > ..\jcode-untracked.txt
```

Вторая команда выше намеренно показана без автоматического применения. На target сначала восстановить exact committed revision, затем просмотреть patch и untracked list. Если требуется буквальная копия рабочего дерева, архивировать весь worktree после остановки процессов, не заменяя им git backup.

## 3. Что переносить из Jcode

### 3.1 Переносимый слой

Сохранить после остановки всех Jcode clients/servers:

- `%USERPROFILE%\.jcode\config.toml`;
- `%USERPROFILE%\.jcode\config.env`, но без вывода значений;
- `prompt-overlay.md`;
- `todo.json`;
- `memory\` включая graph, projects и global memory;
- session records и transcript history;
- пользовательские project/config metadata;
- локальные scripts и безопасные настройки, необходимые для автозапуска;
- диагностические отчеты, если они не содержат secrets.

`JCODE_HOME` можно использовать для другого корня состояния. `JCODE_RUNTIME_DIR` следует создавать заново на target, поскольку там находятся sockets и ephemeral runtime-файлы, а не долговечная история.

### 3.2 Что не надо переносить как источник истины

- `%LOCALAPPDATA%\jcode\bin\jcode.exe` и старые versioned binaries: target лучше установить из проверенного официального release.
- sockets, locks, PID-файлы, временные каталоги и открытые handles.
- stale caches, если они не нужны для ускорения.
- старые логи с секретами или персональными данными, если они не нужны для расследования.

Jcode устанавливается официальным Windows installer, который проверяет `SHA256SUMS`. После установки проверить `jcode --version`, путь и hash бинарника.

### 3.3 Credentials Jcode

Для каждого provider profile составить только список имен и способ восстановления:

- credential source: vault/password manager, environment или `jcode login`;
- endpoint;
- profile name;
- model;
- необходимый scope.

Значения ключей не копировать в manifest. На target выполнить повторную авторизацию или безопасно восстановить credentials, затем `jcode auth-test`.

Критический профиль текущей конфигурации:

```text
profile: OmniRouteV2
endpoint: http://127.0.0.1:20128/v1
model: auto/smart
default provider: OmniRouteV2
```

После миграции это должно остаться именно именованным профилем `OmniRouteV2`. Внешние JSON/log labels не должны ошибочно превращать его в `OpenRouter`; внутренний OpenAI-compatible transport slot не меняет фактический локальный маршрут.

## 4. Что переносить из Cline

### 4.1 Глобальный слой

Канонические пути, подтвержденные картой конфигурации Cline:

- `C:\Users\WorkShop1\.cline\rules\`;
- `C:\Users\WorkShop1\.cline\skills\`;
- `C:\Users\WorkShop1\.cline\hooks\`;
- `C:\Users\WorkShop1\.cline\agents\`;
- `C:\Users\WorkShop1\.cline\workflows\`;
- `C:\Users\WorkShop1\.cline\plugins\`;
- `C:\Users\WorkShop1\.cline\data\settings\cline_mcp_settings.json`;
- `C:\Users\WorkShop1\.cline\data\settings\providers.json`;
- `C:\Users\WorkShop1\.cline\data\settings\global-settings.json`;
- `C:\Users\WorkShop1\.cline\data\sessions\`;
- `C:\Users\WorkShop1\.cline\data\kanban\`;
- `C:\Users\WorkShop1\.cline\data\cron\`;
- `C:\Users\WorkShop1\.cline\data\workspaces\chat\`;
- `C:\Users\WorkShop1\.cline\data\memory\`, если каталог присутствует.

В workspace-слое проверить и переносить только реально используемые:

- `<workspace>\.clinerules\`;
- `<workspace>\.cline\rules\`;
- `<workspace>\.cline\skills\`;
- `<workspace>\.cline\hooks\`;
- `<workspace>\AGENTS.md`;
- workspace `.cline` memory-bank.

Memory-bank не является встроенным механизмом Cline. Он загружается благодаря правилу `memory-bank-habit.md`, поэтому нельзя переносить memory-bank без соответствующего правила.

### 4.2 Junction для правил

VS Code-панель использует `Documents\Cline\Rules`, а CLI использует `~\.cline\rules`. На текущей машине эти пути были сведены junction-ссылкой, чтобы не получить две расходящиеся копии.

На target:

1. перенести каноническую папку `~\.cline\rules`;
2. закрыть Cline и VS Code;
3. создать junction `Documents\Cline\Rules` на каноническую папку;
4. проверить, что списки и содержимое одинаковы;
5. не создавать вторую независимую копию правил.

Если имя пользователя или диск изменится, нужно перепроверить все абсолютные workspace paths. Сессия может открыться, но не найти исходный workspace, если его путь изменился.

### 4.3 Cline sessions и Kanban

Скопировать после quiesce:

- session metadata и messages;
- Kanban board и карточки;
- cron/autopilot definitions;
- workspace metadata;
- memory-bank;
- audit logs только если они нужны и очищены от secrets.

Ожидаемый результат: в новом Cline открываются недавние сессии, сохраняются карточки, статусы и связи с workspace. Это нужно проверить на одной read-only сессии до восстановления всех cron/autopilot процессов.

### 4.4 MCP

На текущей машине зафиксированы два основных источника MCP-конфига:

- CLI: `~\.cline\data\settings\cline_mcp_settings.json`;
- VS Code globalStorage: `globalStorage\saoudrizwan.claude-dev\settings\cline_mcp_settings.json`.

В некоторых сессиях был набор из honcho, infigraph, chrome-devtools и n8n-mcp. Конфиг переносится как шаблон с environment placeholders. Команды и бинарники нужно установить на target заново и проверить `initialize`/`tools/list` для каждого сервера.

Особое внимание: n8n API credential ранее находился непосредственно в JSON. Перед созданием миграционного архива его нужно вынести в vault или защищенную переменную и убрать plaintext из обоих конфигов. В этот план и в отчеты значение не включается.

## 5. Проекты и рабочие каталоги

### 5.1 Канонический способ

Для каждого проекта:

1. получить remote URL и exact commit;
2. клонировать репозиторий на target;
3. восстановить `.env` только из vault-шаблона;
4. отдельно применить uncommitted patch после просмотра diff;
5. восстановить project-local rules, skills, hooks и MCP references;
6. выполнить build/test/smoke.

Если проекта нет в git, нужен зашифрованный архив исходной папки и отдельный manifest с хешами. Не считать transcript ссылкой на существующий код.

### 5.2 Приоритет восстановления

1. `proj\jcode` и его рабочее дерево.
2. `proj\omniroute` и `proj\omniroute-source`.
3. `proj\cline-source`, только если нужен локальный source development.
4. `proj\telegram-research`.
5. Исторические `D:\watcher`, `D:\Notes\10-Projects\watcher-v5`, `D:\tg_bot_replacer`.
6. `D:\order-page-project` и `D:\print-layout-app`.

Последние пять путей на текущем компьютере не подтверждены. Их нужно восстановить из старого D:-диска, другого компьютера или backup, иначе функциональность watcher/site нельзя обещать.

## 6. Локальная инфраструктура

### 6.1 Docker и volumes

Не копировать вручную внутреннюю папку Docker Desktop. Для stateful контейнеров использовать проверенный export/import named volumes:

1. остановить приложения, которые пишут в volumes;
2. получить список `docker volume ls` и compose-проектов;
3. для каждого volume сделать архив из временного контейнера;
4. перенести архив на target через зашифрованный носитель;
5. создать volume с тем же именем;
6. импортировать данные;
7. поднять compose-контур и проверить health endpoints.

На текущей машине важны как минимум:

- `omniroute_data`;
- Honcho PostgreSQL/Redis state;
- SearXNG cache/config state;
- Docker network `local_default` и Redis service `local-redis-1`.

Точные compose-файлы и имена нужно получить из текущего inventory, а не угадывать по контейнеру. Docker Desktop должен быть запущен до проверки volumes, но не во время их небезопасного копирования.

### 6.2 OmniRouteV2

Восстановить локальный OmniRoute-контур:

- container/service `omniroute-1`;
- localhost-only endpoint `127.0.0.1:20128`;
- volume `omniroute_data`;
- Redis `local-redis-1:6379` с тем же namespace/prefix;
- proxy path через Karing `127.0.0.1:3067`, если он нужен для upstream;
- необходимые environment settings через secure config, без ключей в командной строке.

Порядок запуска:

1. Karing/proxy;
2. Docker Desktop и Redis;
3. OmniRoute;
4. Jcode server/client;
5. Cline/Telegram connector.

Проверки:

- health и `/v1/models` на localhost;
- bounded completion через `auto/smart`;
- `jcode provider current` показывает requested/resolved `OmniRouteV2` и model `auto/smart`;
- логи OmniRoute показывают запрос на `127.0.0.1:20128`, а не прямой вызов OpenRouter;
- provider doctor не следует запускать с несуществующим provider id `omniroutev2` как OpenAI-compatible provider. Для Jcode использовать фактический named profile `OmniRouteV2`.

### 6.3 Honcho, Ollama и Infigraph

- Honcho PostgreSQL/Redis volumes переносить до запуска deriver.
- Ollama models переносить только если объем и время оправданы; иначе скачать заново и проверить embedding model.
- Infigraph binary устанавливать на target отдельно.
- `.infigraph` indexes не копировать во время активных watchers или lock contention. Лучше выполнить cold copy с остановленными процессами либо регенерировать индексы после восстановления репозиториев.
- После восстановления проверить `tools/list`, доступность репозиториев и отсутствие stale absolute paths.

### 6.4 Karing

Перенести приложение и его конфигурацию через штатный export или повторную настройку. Сохранить только имена профилей, режимы и локальные endpoints в manifest. Не переносить токены в открытом виде.

До старта Jcode проверить, что Karing слушает `127.0.0.1:3067`. Только после этого запускать Hub и Telegram connector.

## 7. Внешние интеграции

### 7.1 n8n Cloud

Данные workflow находятся на Beget Cloud, а не в локальном Jcode state. На target восстановить:

- URL instance;
- workflow IDs и названия;
- n8n API credential через vault;
- MCP-конфигурацию;
- локальные audit scripts только без ключей.

Начальная проверка read-only: health endpoint, workflow metadata и execution history. Live Telegram acceptance выполнять только после muted-проверки и согласования тестовых получателей.

### 7.2 Telegram

Перенести connector settings как шаблон. Bot token, chat ID и user ID получать только из secure source. Не запускать два polling процесса одновременно. Сначала запускать connector в foreground/staged режиме, затем проверять autostart.

### 7.3 Браузеры и Agent Bridge

Browser profiles содержат cookies и tokens. Безопасный вариант: перенести только нужные настройки/extensions и войти заново. Полный перенос профиля разрешен только на зашифрованный target и после остановки браузеров.

Firefox Agent Bridge и Chrome DevTools MCP устанавливать заново, затем проверить handshake и один harmless browser action. Перенос профиля не считается доказательством работоспособности bridge.

### 7.4 SSH и MAIN/STANDBY

На текущей инфраструктуре есть MAIN/STANDBY scripts и роль на VPS:

- `switch-role.ps1`;
- `force-main.ps1`;
- `start-main.ps1`;
- `start-standby.ps1`;
- `reconcile.ps1`;
- `sync-memory.ps1`;
- VPS `role.json` и lock.

Перенос выполняется так:

1. перевести исходный host в STANDBY;
2. дождаться остановки OmniRoute, Telegram connector и прочих MAIN-сервисов;
3. установить и проверить target в STANDBY;
4. проверить SSH/VPS connectivity;
5. только после проверки назначить target MAIN;
6. выполнить reconcile и убедиться, что нет старого живого MAIN.

Не копировать VPS lock, локальные sockets или PID-файлы. При аварии использовать `force-main` только с явным подтверждением, что прежний MAIN выключен или недостижим.

## 8. Пошаговый runbook

### Фаза A: подготовка source

- [ ] Заморозить новые изменения и предупредить о коротком maintenance window.
- [ ] Завершить или остановить активные Jcode/Cline turns.
- [ ] Остановить Hub, Telegram connector, cron/autopilot, watchers и MCP processes.
- [ ] Остановить Docker applications после graceful shutdown.
- [ ] Сохранить версии, git status, manifests и список volume.
- [ ] Перевести MAIN в STANDBY.
- [ ] Создать rollback snapshot на отдельном зашифрованном носителе.

### Фаза B: установка target

- [ ] Проверить Windows x64/ARM64 и место на диске.
- [ ] Установить Git, PowerShell prerequisites, Docker Desktop/WSL, при необходимости Rust/Build Tools.
- [ ] Установить официальный Jcode release и проверить SHA-256.
- [ ] Создать того же Windows user или настроить `JCODE_HOME`.
- [ ] Не запускать Jcode до восстановления state и проверки permissions.

### Фаза C: восстановление файлов

- [ ] Восстановить Jcode state.
- [ ] Восстановить Cline global/workspace state.
- [ ] Создать `Documents\Cline\Rules` junction.
- [ ] Восстановить проекты и exact git commits.
- [ ] Применить uncommitted patch только после review.
- [ ] Установить MCP binaries и создать config с placeholders.
- [ ] Восстановить Docker volumes и compose files.
- [ ] Восстановить OmniRoute/Karing/Honcho/Ollama/Infigraph.
- [ ] Восстановить Startup scripts только после ручного запуска и проверки.

### Фаза D: credentials и запуск

- [ ] Заполнить secrets через vault или `jcode login`.
- [ ] Проверить provider auth без вывода значений.
- [ ] Поднять Karing, Docker, OmniRoute, Jcode, Cline и MCP в указанном порядке.
- [ ] Запустить Telegram connector ровно одним экземпляром.
- [ ] Включить MAIN только после acceptance в STANDBY.

### Фаза E: acceptance и rollback window

- [ ] Выполнить все проверки из следующего раздела.
- [ ] Сохранить результаты и timestamps без секретов.
- [ ] Оставить source read-only на согласованный срок.
- [ ] Только после этого удалить временные архивы, если это отдельно согласовано.

## 9. Acceptance checklist

### Jcode и память

- [ ] `jcode --version` соответствует исходной версии или upgrade явно принят.
- [ ] launcher и hash бинарника проверены.
- [ ] `todo.json` содержит актуальные pending/completed статусы.
- [ ] memory recall возвращает ключевые non-secret notes.
- [ ] prompt overlay загружается.
- [ ] Последняя Jcode session открывается и содержит transcript continuity.

### OmniRouteV2

- [ ] `jcode provider current` разрешает `OmniRouteV2`.
- [ ] selected model равна `auto/smart`.
- [ ] bounded smoke возвращает ожидаемый marker.
- [ ] Docker logs подтверждают localhost OmniRoute route.
- [ ] В user-facing результате нет ложной подмены `OmniRouteV2` на `OpenRouter`.

### Cline

- [ ] Новая чистая workspace-сессия видит глобальные rules.
- [ ] Skills корректно парсятся и видны после полного restart.
- [ ] hooks запускаются с ожидаемым audit/guard поведением.
- [ ] Kanban и cron восстановлены, но cron сначала disabled/staged.
- [ ] CLI MCP и VS Code MCP имеют согласованный список серверов.
- [ ] Каждый MCP server проходит initialize/tools-list.
- [ ] Recent Cline session и memory-bank открываются без broken workspace paths.

### Инфраструктура

- [ ] Docker volumes монтируются и health endpoints отвечают.
- [ ] Redis/Postgres state и Honcho проходят read-only sanity checks.
- [ ] Karing слушает локальный порт до запуска клиентов.
- [ ] Infigraph не держит stale lock и видит восстановленные repositories.
- [ ] Telegram имеет ровно один polling owner.
- [ ] MAIN/STANDBY status на VPS и target согласованы.

### Проекты и интеграции

- [ ] Jcode repo соответствует exact commit.
- [ ] Uncommitted changes восстановлены отдельно и проверены diff.
- [ ] n8n Cloud отвечает, workflow metadata читается, credentials работают.
- [ ] n8n muted workflow matrix проходит без Telegram side effects.
- [ ] watcher запускается только после восстановления его исходников и share.
- [ ] Browser bridge проходит harmless smoke.

## 10. Откат

Если target не проходит acceptance:

1. не включать его в MAIN;
2. остановить его Hub, connector, cron и Docker приложения;
3. оставить source в рабочем или прежнем STANDBY-состоянии;
4. вернуть роль на source через штатный `switch-role` или согласованный `force-main`;
5. восстановить source из read-only snapshot только при необходимости;
6. зафиксировать failed check и исправить target отдельно.

Удаление source или очистка старых volumes не является частью первой миграции.

## 11. Известные ограничения

- Нельзя гарантировать перенос живого mid-turn, открытых file handles и background bash процессов. Их нужно завершить, checkpoint или повторно запустить.
- Uncommitted worktree state не заменяет commit и переносится отдельным diff/archive шагом.
- Cline/Jcode session history может сохраниться, но абсолютные пути workspace и host-owned runtime могут потребовать repair.
- Старые D:-проекты watcher/site/order/print-layout не найдены на текущем компьютере. Без источника или backup их функциональность переносить нечем.
- Browser cookies, n8n credentials, Telegram tokens и provider keys нельзя считать безопасно мигрированными простым копированием файлов.
- Сетевые папки, Docker Desktop, Karing и внешний VPS должны быть доступны независимо от локальной копии Jcode.

## 12. Критерий завершения миграции

Миграция считается завершенной только когда одновременно выполнены следующие условия:

1. Jcode запускается на target с сохраненной историей, памятью, TODO и prompt overlay.
2. `OmniRouteV2` с `auto/smart` работает через локальный OmniRoute endpoint.
3. Cline видит rules, skills, hooks, MCP, Kanban и recent sessions.
4. Docker/Honcho/OmniRoute/Infigraph проходят проверки.
5. n8n Cloud и staged Telegram контур проходят acceptance без неожиданных реальных отправок.
6. MAIN/STANDBY переключение проверено без split-brain.
7. Все найденные рабочие каталоги либо восстановлены и проверены, либо явно внесены в список отсутствующих артефактов.
8. Source остается доступным для rollback до отдельного решения пользователя.
