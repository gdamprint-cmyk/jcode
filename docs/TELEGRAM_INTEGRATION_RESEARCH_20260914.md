# Telegram integration research and safe adoption plan

**Дата среза:** 14 сентября 2026.  
**Репозиторий:** Jcode  
**Статус:** исследование завершено; Telegram не включался и не настраивался.

## Итоговое решение

В Jcode уже есть рабочий Telegram transport/channel adapter. Поэтому копировать целиком сторонний фреймворк или писать интеграцию с нуля не нужно. Наиболее рациональный путь:

1. сохранить текущую архитектуру Jcode `MessageChannel`;
2. адаптировать только проверенные security/reliability-паттерны из зрелых реализаций;
3. до включения remote replies добавить sender-level authorization, безопасную обработку offset, bounded reconnect и тесты;
4. использовать **long polling**, а не webhook;
5. ограничить первую эксплуатацию одним private chat, exact `chat_id` + exact Telegram `user_id`, без групп и без remote shell/admin commands.

Сейчас изменения кода и конфигурации не выполнялись, потому что отсутствуют отдельные Telegram bot token, разрешённый chat ID и user ID. Секрет OmniRoute для Telegram не использовался.

## Метод и качество evidence

Сначала были выполнены targeted-запросы через локальный SearXNG. На срезе 14 сентября 2026 все Telegram-запросы вернули `No results found`, поэтому не делались выводы по snippets. После одного упрощённого повторного запроса канал был признан непригодным для этой ветки исследования.

Чтобы достичь того же или лучшего результата без подмены evidence, использованы:

- официальная документация Telegram Bot API;
- GitHub API, README, исходники и тесты зрелых open-source реализаций;
- локальный read-only анализ Jcode;
- локальные source checkouts `cc-connect` и `nanoclaw`.

Классификация:

- **A: подтверждено исходником/тестом или официальной документацией**;
- **B: заявлено проектом, но не полностью подтверждено независимым runtime-тестом**;
- **C: мнение, маркетинговое утверждение или неподтверждённое предположение**.

## Официальный Telegram путь

Источники:

- [Bot API](https://core.telegram.org/bots/api)
- [getUpdates](https://core.telegram.org/bots/api#getupdates)
- [setWebhook](https://core.telegram.org/bots/api#setwebhook)
- [deleteWebhook](https://core.telegram.org/bots/api#deletewebhook)
- [Webhook guide](https://core.telegram.org/bots/webhooks)
- [Bot features and privacy mode](https://core.telegram.org/bots/features)
- [Bot FAQ](https://core.telegram.org/bots/faq)

Подтверждённые факты:

- Bot API работает по HTTPS.
- `getUpdates` и webhook взаимоисключающие.
- Для polling нужно передавать `offset = last_update_id + 1`, чтобы подтверждать обработанные updates.
- Updates Telegram хранит ограниченное время, поэтому после длительного простоя гарантии бесконечной очереди нет.
- Webhook требует публично доступный HTTPS endpoint, корректный TLS и дополнительную сетевую поверхность.
- Secret path для webhook полезен, но не заменяет authorization и сетевые ограничения.
- Privacy mode влияет на сообщения в группах, но не является authorization механизмом.
- Наличие bot token не означает, что любой пользователь должен иметь право отправлять команды агенту.

Для локального Jcode long polling лучше: он требует только исходящие HTTPS-запросы, не открывает listener и проще восстанавливается после перезапуска или временной сетевой ошибки.

## Сравнение найденных реализаций

### cc-connect

Репозиторий: [chenhg5/cc-connect](https://github.com/chenhg5/cc-connect)  
Telegram source: [platform/telegram](https://github.com/chenhg5/cc-connect/tree/main/platform/telegram)  
Guide: [docs/telegram.md](https://github.com/chenhg5/cc-connect/blob/main/docs/telegram.md)

**A/B findings:**

- Go Telegram adapter использует `go-telegram/bot` и отдельный HTTP client.
- Поддерживает long polling, optional HTTP/SOCKS5 proxy, reconnect/backoff и lifecycle handling.
- В коде есть `allow_from`, проверка доступа и отдельная `admin_from` для privileged commands.
- Есть тестируемые абстракции bot factory, timer и handler.
- Поддерживаются Telegram topics/session keys, progress updates и message editing.

**Риски и ограничения:**

- В официальном guide прямо указано, что отсутствие `allow_from` разрешает всех пользователей и лишь пишет предупреждение.
- `admin_from` легко положить не в тот уровень конфигурации, что создаёт ложное ощущение защиты.
- Реализация значительно шире потребностей Jcode и включает операции, которые Jcode не должен импортировать без отдельного threat model.
- В guide отмечено, что chat-ID whitelisting отсутствует или запланирован, поэтому одного user allowlist недостаточно для безопасной групповой модели.
- Отсутствие найденного отдельного SECURITY.md не является доказательством небезопасности, но снижает прозрачность security contract.

**Вывод:** хороший источник reliability-паттернов и тестируемого polling loop, но не готовый код для прямого копирования в Jcode.

### NanoClaw

Репозиторий: [nanocoai/nanoclaw](https://github.com/nanocoai/nanoclaw)  
Security model: [docs/SECURITY.md](https://github.com/nanocoai/nanoclaw/blob/main/docs/SECURITY.md)

**A/B findings:**

- Проект использует channel adapter seam, persistent sessions, sender/group policies и тесты inbound policy.
- Unregistered senders отделены от owners/admins и group members.
- Входящие сообщения считаются недоверенным input даже от допущенного пользователя.
- Основная граница защиты: per-session Docker container, non-root, explicit mounts и отсутствие доступа к host project root.
- Есть mount allowlist, realpath/symlink validation, blocked patterns для credentials и optional egress lockdown.
- В документации отдельно отмечено, что egress lockdown выключен по умолчанию, поэтому default нельзя автоматически считать secure-by-default.

**Ограничения для Jcode:**

- Это самостоятельный агентский runtime, а не маленький Telegram adapter.
- Его container/credential model нельзя перенести в Jcode одним копированием файла.
- В main checkout Telegram adapter добавляется/живет через channel skill path, поэтому README claim и конкретный trunk source нельзя смешивать.

**Вывод:** лучший источник принципов privilege separation, session isolation, mount security и prompt-injection boundaries. Не является drop-in реализацией для Jcode.

### Dialog Reddit/Telegram-style hosted MCP

Исследованный hosted вариант: [dialog-tools/reddit-research-mcp](https://github.com/dialog-tools/reddit-research-mcp).

Он полезен как пример citation/provenance и opt-in hosted OAuth, но Telegram bridge для Jcode не предоставляет. Hosted MCP расширяет trust boundary и не нужен для первой локальной Telegram-интеграции.

## Что уже есть в Jcode

### Transport и wiring

- `crates/jcode-app-core/src/channel.rs` содержит `MessageChannel`, `ChannelRegistry`, Telegram registration, outbound send и reply loop.
- `crates/jcode-base/src/telegram.rs` содержит `sendMessage` и `getUpdates` wrapper.
- HTTP-клиент берётся через `crate::provider::shared_http_client()`, то есть использует общий reqwest/system-proxy путь Jcode. Это согласуется с ранее проверенным Karing audit, но не доказывает, что каждый Telegram proxy scenario будет работать без live probe.
- Reply mode по умолчанию выключен.
- В конфиге уже есть `telegram_enabled`, `telegram_bot_token`, `telegram_chat_id`, `telegram_reply_enabled`.

### Подтверждённые gaps

1. Проверяется только `message.chat.id`. `message.from.id` не извлекается и не проверяется.
2. В группе любой участник разрешённого chat может отправить prompt или permission reply.
3. Offset хранится только в памяти reply loop. После restart возможна повторная доставка или потеря ожидаемого состояния в зависимости от момента остановки.
4. Нет явного graceful cancellation для reply loop через `MessageChannel` API.
5. При сетевой ошибке используется фиксированный sleep 10 секунд вместо bounded exponential backoff с jitter.
6. ACK echo-ит пользовательский текст в Markdown. Это создаёт риск некорректного форматирования и ненужного отражения потенциально чувствительного текста.
7. Bot token входит в URL Bot API. В текущих ошибках полный URL не формируется в сообщении, но transport/proxy diagnostics должны оставаться redacted.
8. Permission reply связывается с request ID, но до hardening не привязан к sender-level authorization и channel mode.
9. Нет отдельного лимита длины входящего текста и явной rate control policy.
10. Реализация является ambient notification/reply adapter, а не полноценным remote session protocol. Для remote lifecycle правильнее использовать существующий Jcode server/gateway seam, если когда-либо потребуется полноценный удалённый клиент.

## Рекомендуемый безопасный scope

### Phase 1: hardening существующего Jcode adapter

Без изменения server protocol и без добавления групп:

- добавить optional exact `telegram_user_id` или список разрешённых user IDs;
- требовать user allowlist при `telegram_reply_enabled=true`;
- по умолчанию разрешать только private chat; group mode сделать явным отдельным opt-in gate;
- проверять `chat_id` и `from.id` до parsing permission/prompt;
- не echo-ить исходный пользовательский текст в ACK;
- перейти на plain text либо корректно экранировать MarkdownV2;
- ввести max message length и безопасное усечение;
- добавить bounded exponential reconnect с jitter и cancellation;
- определить restart semantics offset: durable offset либо явный startup drain с документированным discard policy;
- логировать только update ID, chat/user IDs в безопасной форме и размеры, не token/text payload;
- оставить remote shell/admin/write actions вне Telegram scope;
- permission approval принимать только от allowlisted sender и только для действующего request ID.

### Phase 2: acceptance

В изолированном тестовом конфиге:

- unit tests для allowed/denied user, wrong chat, group default deny;
- tests для malformed update, missing sender, non-text update и oversized text;
- tests для duplicate update ID, offset advancement и restart behavior;
- reconnect/backoff test с simulated 403/429/5xx/timeout;
- permission approval/denial test с чужим sender ID;
- secret-redaction test для URL, errors и logs;
- live `getMe`/`getUpdates` smoke только после отдельного token gate;
- temporary network loss recovery test;
- rollback test, не меняющий рабочую конфигурацию пользователя.

### Phase 3: optional expansion

Только после успешной Phase 1/2:

- group/topic support with explicit membership policy;
- bounded per-user and per-chat rate limits;
- persistent session binding if user really needs multiple sessions;
- webhook only when a real public HTTPS deployment is intentionally provisioned and audited.

## Security decision

**Стороннюю реализацию напрямую не устанавливать и не запускать.**

Решение основано на границе риска:

- cc-connect даёт хорошие polling/reconnect patterns, но его privileged command surface слишком широк для копирования;
- NanoClaw даёт сильную модель isolation, но требует переноса целого runtime и Docker boundary;
- текущий Jcode seam уже достаточен, поэтому адаптация меньшего объёма снижает supply-chain и integration risk.

Следовательно, следующий кодовый шаг должен быть не «подключить Telegram любой ценой», а точечный hardening текущего adapter с тестами и обратимым staged acceptance. До получения Telegram credentials и явного operational gate конфигурация не меняется.

## Остаточная неопределённость

- Runtime Telegram live probe не проводился: токен и разрешённые IDs отсутствуют.
- SearXNG не дал результатов, поэтому GitHub/API/официальные источники использованы как более релевантный и воспроизводимый эквивалент, а не представлены как Reddit evidence.
- Claims сторонних README не считаются независимой эксплуатационной статистикой.
- Надёжность proxy path для Telegram должна быть проверена отдельным harmless `getMe`/negative probe после появления credentials, с redaction и без публикации token.
