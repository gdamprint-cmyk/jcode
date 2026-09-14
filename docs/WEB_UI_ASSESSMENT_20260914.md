# Оценка Web UI и готового удалённого клиента Jcode

Дата среза: 14 сентября 2026.

## Итоговое решение

Готового браузерного Web UI в репозитории Jcode не обнаружено. Поэтому новый web-интерфейс с нуля не создавался и gateway не включался.

При этом в репозитории уже есть реализация нативного iOS-клиента и серверный WebSocket gateway. Это не Web UI, а отдельный тонкий клиент для телефона. Его можно рассматривать как готовый путь удалённого доступа, но только после завершения доставки iOS-приложения и отдельной сетевой проверки.

## Что подтверждено

### 1. Встроенный gateway

`crates/jcode-base/src/gateway.rs` реализует:

- WebSocket endpoint `/ws`;
- HTTP `GET /health`;
- HTTP `POST /pair` с короткоживущим pairing code;
- токены устройств с проверкой до завершения WebSocket handshake;
- отзыв устройств;
- совместимый с TUI newline-delimited JSON protocol.

Gateway выключен по умолчанию. Конфигурация по умолчанию использует порт `7643` и bind `0.0.0.0`, поэтому без ограничения интерфейса или приватной сети его нельзя считать безопасным для публичного открытия.

Документация рекомендует Tailscale или доверенную LAN. В коде есть предупреждение о риске wildcard bind. Предпочтительный практический режим для телефона: приватный tailnet, pairing code с коротким TTL, bearer token после pairing, отзыв устройства при потере телефона.

### 2. Готовый iOS-клиент

В `ios/` присутствует полноценная SwiftUI-заготовка `JCodeMobile` и platform-free библиотека `JCodeKit`, включая:

- подключение к gateway;
- pairing и хранение credentials;
- WebSocket transport;
- session reducer и отображение transcript;
- reconnect при возврате приложения на передний план;
- обработку отозванного токена с прекращением reconnect loop и запросом повторного pairing;
- unit tests, E2E mock gateway и protocol smoke test.

Локальный `ios/PRODUCTION_CHECKLIST.md` показывает PASS для unit tests, E2E, protocol smoke, reconnect, privacy manifest, permission strings и simulator compile check. Доставка в TestFlight помечена BLOCKED из-за cloud signing/App Store Connect permissions. Это значит, что исходники и тестовый контур готовы, но удобная установка на iPhone ещё зависит от владельца Apple Developer аккаунта.

### 3. SDK

TypeScript SDK Jcode может подключаться к уже запущенному серверу и использоваться для собственного dashboard или UI. Это toolkit, а не готовое браузерное приложение. Создавать dashboard только на основании наличия SDK не следует, так как это будет уже новая разработка, которую пользователь просил не делать без готового решения.

### 4. Веб-сайт

`jcode.sh` является статическим сайтом документации и продукта. Он не является web-клиентом для управления живыми сессиями.

## Оценка рисков

| Компонент | Статус | Риск |
|---|---|---|
| Browser Web UI | Не найден | Нельзя безопасно настроить отсутствующий клиент |
| WebSocket gateway | Есть, выключен | При `0.0.0.0` возможна лишняя сетевой экспозиция |
| iOS client source | Есть, тестовый контур есть | На Windows нельзя локально проверить Swift/Xcode |
| TestFlight | Не подтверждён | В upstream checklist заблокирован cloud signing permissions |
| Pairing | Есть | Pairing code и device token нужно считать чувствительными |
| Tailscale | Рекомендуемый транспорт | Нужна проверка фактического tailnet и firewall перед включением |
| HTTP CORS | `*` для gateway HTTP responses | При публичном bind нельзя считать это полноценной защитой |

## Почему сейчас ничего не включалось

1. Запрошен готовый Web UI, а готового web frontend не найдено.
2. Включение gateway изменяет сетевую поверхность машины и при default bind слушает на всех интерфейсах.
3. Доступный клиент сейчас подтверждён как iOS source/test harness, но не как установленное на устройстве приложение.
4. Включение без проверки Tailscale/firewall и без конечного клиента не улучшило бы доступность, но создало бы лишнюю сетевую поверхность.

## Если позже понадобится телефон

Безопасный следующий план:

1. Подтвердить, что iOS-клиент реально установлен через TestFlight или собран владельцем Apple Developer аккаунта.
2. Проверить Tailscale interface и адрес read-only способом.
3. Включить gateway только для доверенного tailnet или конкретного bind address, не оставляя публичный wildcard без необходимости.
4. Выполнить pairing через штатный `jcode pair`.
5. Проверить `/health`, pairing, WebSocket connect, session resume и revoke.
6. После проверки снова убедиться, что локальные OmniRoute, SearXNG, Docker и Jcode server functionality не изменились.
7. При проблеме отозвать устройство и выключить gateway.

На текущем компьютере и в рамках запроса оптимальное действие: Web UI не настраивать, готовый iOS/gateway путь сохранить как отдельную опцию.

## Источники

- Local source: `crates/jcode-base/src/gateway.rs`
- Local source: `ios/`
- Local source: `ios/PRODUCTION_CHECKLIST.md`
- Local source: `ios/TestHarness/README.md`
- Official docs: https://jcode.sh/docs
- Official SDK docs: https://jcode.sh/sdk
- Official architecture: https://raw.githubusercontent.com/1jehuang/jcode/master/docs/SERVER_ARCHITECTURE.md
- Official iOS checklist: https://raw.githubusercontent.com/1jehuang/jcode/master/ios/PRODUCTION_CHECKLIST.md
- Official iOS workflow: https://raw.githubusercontent.com/1jehuang/jcode/master/.github/workflows/ios-testflight.yml
