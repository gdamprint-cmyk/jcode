# OmniRoute настройка — отчёт 2026-09-14

## Статус: завершено

### Live-проверка после восстановления списка задач

- `config.toml` содержит `default_provider = "OmniRouteV2"` и `default_model = "auto/smart"`.
- Профиль `OmniRouteV2` использует локальный endpoint `http://127.0.0.1:20128/v1` и отдельный private credential-файл Jcode, а не `OPENROUTER_API_KEY`.
- `jcode provider current` подтвердил: requested provider `auto`, resolved provider `OmniRouteV2`, selected model `auto/smart`.
- Два bounded `jcode run` без явного выбора профиля вернули ожидаемые smoke-маркеры.
- Docker-логи подтвердили оба запроса на локальном OmniRoute: `POST /v1/chat/completions | auto/smart`.

Важно: Jcode внутренне использует compatibility slot с меткой `OpenRouter` для named OpenAI-compatible profiles. Это внутренняя реализация runtime и не означает обращение к OpenRouter API. Фактический маршрут в этой настройке идёт в OmniRouteV2 на `127.0.0.1:20128`.

### Что настроено

| Компонент | Значение | Статус |
|---|---|---|
| Модель по умолчанию | `auto/smart` (Jcode config.toml) | ✅ |
| Платные openrouter модели | `hidePaidModels: true` | ✅ |
| Compression стек | `session-dedup → ccr → headroom → rtk → caveman` | ✅ |
| RTK | enabled, intensity=standard, grouping=true | ✅ |
| Caveman | enabled, intensity=full, compressRoles=[user,assistant] | ✅ |
| Auto-Combo | `most-fit-available` (27 моделей, 6 провайдеров) | ⚠️ не работает через API-ключ |
| Провайдеры | cohere, groq, mistral, cloudflare-ai, opencode, felo-web | ✅ |

### Подробности

**Модель по умолчанию:**
- `config.toml`: `default_model = "auto/smart"` (два поля)
- OmniRoute не имеет поля "модель по умолчанию" — модель задаётся клиентом при каждом запросе
- `auto/smart`: quality-first + 10% exploration rate

**Платные модели:**
- `hidePaidModels: True` — платные openrouter модели исключены из пулов
- 72 openrouter модели, все `:free` — платные уже скрыты
- В кандидатах только `:free` модели

**Compression стек:**
- 5 engines: session-dedup, ccr, headroom, rtk, caveman
- Ожидаемая экономия: 78.4-94.6% (согласно документации OmniRoute)
- LLMLingua-2 не включена — требует установки `@atjsh/llmlingua-2`
- OmniGlyph не включён — контекст-как-изображение, неприменимо

**Комбо most-fit-available:**
- Создан через `POST /api/combos` с 27 моделями из 6 провайдеров
- Стратегия: `auto` с весами: taskFit=0.30, quality=0.20, health=0.15, quota=0.10
- Не работает через API-ключ: `combo/` преfix возвращает 401 "No active credentials for provider: combo."
- DELETE не поддерживается (405 Method Not Allowed)
- Решение: использовать `auto/smart` как основной, комбо оставить как резерв

### Проблемы

1. **`combo/` преfix не работает** — OmniRoute требует отдельную учётную запись для провайдера `combo.`
2. **DELETE комбо не поддерживается** — можно только скрыть через `isHidden: true`
3. **`defaultModel` не сохраняется** — OmniRoute не имеет этого поля, модель задаётся клиентом
4. **Telegram тесты не компилируются** — `dlltool.exe` не найден (системная проблема, не связанная с кодом)

### Далее

- [ ] Telegram: regression tests (parser, config, channel)
- [ ] Telegram: staged acceptance
- [ ] Firefox Agent Bridge
- [ ] Связь с активной сессией Cline
- [ ] Deep research скилл: проверить интеграцию с Jcode
