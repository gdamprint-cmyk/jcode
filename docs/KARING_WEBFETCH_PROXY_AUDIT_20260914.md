# Karing proxy and Jcode webfetch audit

**Дата среза:** 14 сентября 2026  
**Режим:** read-only, без изменения proxy/system settings.

## Вывод

В текущем Windows-сеансе Karing действительно настроен как системный HTTP/HTTPS proxy:

- process/user environment содержит `HTTP_PROXY`, `HTTPS_PROXY` и `ALL_PROXY` на loopback proxy endpoint `127.0.0.1:3067`;
- Internet Settings имеют `ProxyEnable=1` и тот же loopback endpoint;
- WinHTTP отдельно показывает `Direct access`, поэтому WinHTTP и Internet/WinINet settings не совпадают;
- процессы `karing.exe` и `karingService.exe` работают, порт `127.0.0.1:3067` слушает `karingService`.

Исходный код Jcode показывает, что `webfetch` использует общий `reqwest` client с feature `system-proxy`, а не жёстко прописанный Karing URL. Поэтому точная формулировка такая: **webfetch не интегрирован с Karing напрямую, но в этом сеансе имеет условия, при которых reqwest может использовать системный proxy и proxy environment variables.** Сам webfetch не возвращает telemetry о фактически выбранном маршруте, поэтому это не 100% packet-level proof.

## Read-only probes

Без proxy и через Karing к безопасному `https://example.com/`:

- direct: HTTP 200, exit 0;
- explicit Karing `127.0.0.1:3067`: HTTP 200, exit 0.

Для Reddit JSON без авторизации:

- direct: HTTP 403;
- explicit Karing: HTTP 403.

Native Jcode `webfetch` к `https://example.com/` успешно вернул HTTP content. Ранее native webfetch к Reddit также получил HTTP 403, а GitHub и Arctic Shift успешно отдавали материалы. Это показывает, что 403 является свойством Reddit access/policy/route в этой комбинации, но **не доказывает**, что Karing виноват или что webfetch обошёл Karing.

## Риск и решение

- Karing не отключался и настройки не менялись.
- Не следует автоматически подменять proxy для webfetch: это может отправить bearer/API credentials через неожиданный маршрут.
- Для публичных источников текущий маршрут рабочий.
- Для Reddit более сильный результат уже получен через Arctic Shift, который отдаёт полные посты/комментарии и provenance. Поэтому нет необходимости ослаблять сетевую безопасность ради повторных 403.
- Если понадобится packet-level attribution, следующий безопасный тест должен запускаться отдельно, с harmless endpoint и явным process-scoped proxy override, без секретов и без изменения глобальных настроек.

## Источники кода

- `crates/jcode-app-core/src/tool/webfetch.rs`: webfetch использует `shared_http_client()`.
- `crates/jcode-provider-core/src/lib.rs`: reqwest client с `system-proxy`.
- `crates/jcode-base/src/auth/env_facts.rs`: проверка proxy env только как факт наличия.

## Acceptance

Текущий результат достаточен для практического решения: Karing активен, explicit proxy работает, webfetch работает с публичными HTTPS-источниками, Reddit продолжает отдавать 403 и через direct, и через Karing. Исследование Reddit продолжено через Arctic Shift и не зависит от неподтверждённого предположения о маршруте webfetch.
