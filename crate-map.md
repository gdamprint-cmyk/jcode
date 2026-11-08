# jcode workspace — crate dependency map

Generated from `cargo metadata --format-version 1` against `C:/Users/WorkShop1/proj/jcode-src`.

- **Crates:** 89 workspace members (root crate `jcode` + 88 in `crates/`)
- **Internal edges (runtime/build):** 263
- **Cycles (runtime/build):** 0 — clean DAG
- **Longest chain (spine):** 9 crates / 8 edges

## 1. Spine — longest dependency chain (top → foundation)

Edge `A → B` = A depends on B. Each link is a real `path` dependency (verified against `cargo metadata`).

jcode → jcode-companion → jcode-voice → jcode-base → jcode-protocol → jcode-provider-core → jcode-logging → jcode-storage → jcode-core

## 2. Orphans / roots — entry crates (no internal dependents)

- `jcode` — CLI binary + library (entry point)
- `jcode-sdk` — public SDK crate (consumed outside the workspace)

## 3. Foundation leaves — no internal runtime/build deps (39)

- `jcode-agent-runtime`
- `jcode-ambient-types`
- `jcode-applet-types`
- `jcode-auth-types`
- `jcode-azure-auth`
- `jcode-background-types`
- `jcode-build-meta`
- `jcode-codemode`
- `jcode-command-risk`
- `jcode-config-types`
- `jcode-core`
- `jcode-desktop-open`
- `jcode-embedding`
- `jcode-fuzzy`
- `jcode-gateway-types`
- `jcode-import-core`
- `jcode-memory-types`
- `jcode-message-types`
- `jcode-notify-email`
- `jcode-overnight-core`
- `jcode-pdf`
- `jcode-plan`
- `jcode-provider-metadata`
- `jcode-render-core`
- `jcode-schema-dialect`
- `jcode-selfdev-types`
- `jcode-side-panel-types`
- `jcode-task-types`
- `jcode-terminal-image`
- `jcode-terminal-launch`
- `jcode-terminal-prompt`
- `jcode-tool-types`
- `jcode-transport`
- `jcode-tui-account-picker`
- `jcode-tui-anim`
- `jcode-tui-tool-display`
- `jcode-tui-workspace`
- `jcode-update-core`
- `jcode-usage-types`

## 4. Layers (L0 = top entry → L8 = deepest foundation)

- **L0** (2): `jcode`, `jcode-sdk`
- **L1** (5): `jcode-companion`, `jcode-harness-api-server`, `jcode-provider-doctor`, `jcode-provider-grok-build-runtime`, `jcode-tui`
- **L2** (22): `jcode-app-core`, `jcode-fuzzy`, `jcode-productivity-core`, `jcode-provider-anthropic-runtime`, `jcode-provider-antigravity-runtime`, `jcode-provider-copilot-runtime`, `jcode-provider-cursor-runtime`, `jcode-provider-gemini-runtime`, `jcode-provider-openai-runtime`, `jcode-provider-openrouter-runtime`, `jcode-terminal-prompt`, `jcode-tui-account-picker`, `jcode-tui-anim`, `jcode-tui-core`, `jcode-tui-messages`, `jcode-tui-permissions`, `jcode-tui-render`, `jcode-tui-session-picker`, `jcode-tui-tool-display`, `jcode-tui-usage-overlay`, `jcode-tui-visual-debug`, `jcode-voice`
- **L3** (16): `jcode-ambient-types`, `jcode-base`, `jcode-build-support`, `jcode-codemode`, `jcode-command-risk`, `jcode-desktop-open`, `jcode-harness-api`, `jcode-notify-email`, `jcode-overnight-core`, `jcode-pdf`, `jcode-provider-anthropic`, `jcode-setup-hints`, `jcode-swarm-core`, `jcode-tui-markdown`, `jcode-tui-style`, `jcode-update-core`
- **L4** (22): `jcode-auth-types`, `jcode-azure-auth`, `jcode-background-types`, `jcode-compaction-core`, `jcode-embedding`, `jcode-gateway-types`, `jcode-import-core`, `jcode-memory-types`, `jcode-protocol`, `jcode-provider-antigravity`, `jcode-provider-bedrock`, `jcode-provider-copilot`, `jcode-provider-openai`, `jcode-provider-openrouter`, `jcode-render-core`, `jcode-telemetry-core`, `jcode-terminal-image`, `jcode-terminal-launch`, `jcode-todo-policy`, `jcode-tool-core`, `jcode-transport`, `jcode-tui-mermaid`
- **L5** (15): `jcode-agent-runtime`, `jcode-applet-types`, `jcode-batch-types`, `jcode-build-meta`, `jcode-config-types`, `jcode-plan`, `jcode-provider-core`, `jcode-provider-env`, `jcode-provider-gemini`, `jcode-selfdev-types`, `jcode-session-types`, `jcode-side-panel-types`, `jcode-task-types`, `jcode-tool-types`, `jcode-tui-workspace`
- **L6** (5): `jcode-logging`, `jcode-message-types`, `jcode-provider-metadata`, `jcode-schema-dialect`, `jcode-usage-types`
- **L7** (1): `jcode-storage`
- **L8** (1): `jcode-core`

## 5. Hottest shared crates — by internal fan-in

Fan-in = #internal crates depending on it. High fan-in = change-amplification / rebuild blast radius.

- `jcode-message-types` — 24 dependents · leaf
- `jcode-provider-core` — 16 dependents · 4 own deps
- `jcode-base` — 13 dependents · 41 own deps
- `jcode-core` — 13 dependents · leaf
- `jcode-logging` — 12 dependents · 2 own deps
- `jcode-schema-dialect` — 10 dependents · leaf
- `jcode-storage` — 8 dependents · 1 own dep
- `jcode-session-types` — 8 dependents · 1 own dep
- `jcode-selfdev-types` — 7 dependents · leaf
- `jcode-build-meta` — 6 dependents · leaf

## 6. Cycles

None in the runtime/build graph. The only self-reference — `jcode-schema-dialect → jcode-schema-dialect` — is a `[dev-dependencies]` entry (the `test-support` feature, so its integration tests compile the crate as an external dependency). It is not a runtime/build edge and creates no cycle.

## 7. Families (grouped by first segment after `jcode-`)

- **provider** (19): `jcode-provider-anthropic`, `jcode-provider-anthropic-runtime`, `jcode-provider-antigravity`, `jcode-provider-antigravity-runtime`, `jcode-provider-bedrock`, `jcode-provider-copilot`, `jcode-provider-copilot-runtime`, `jcode-provider-core`, `jcode-provider-cursor-runtime`, `jcode-provider-doctor`, `jcode-provider-env`, `jcode-provider-gemini`, `jcode-provider-gemini-runtime`, `jcode-provider-grok-build-runtime`, `jcode-provider-metadata`, `jcode-provider-openai`, `jcode-provider-openai-runtime`, `jcode-provider-openrouter`, `jcode-provider-openrouter-runtime`
- **tui** (15): `jcode-tui`, `jcode-tui-account-picker`, `jcode-tui-anim`, `jcode-tui-core`, `jcode-tui-markdown`, `jcode-tui-mermaid`, `jcode-tui-messages`, `jcode-tui-permissions`, `jcode-tui-render`, `jcode-tui-session-picker`, `jcode-tui-style`, `jcode-tui-tool-display`, `jcode-tui-usage-overlay`, `jcode-tui-visual-debug`, `jcode-tui-workspace`
- **terminal** (3): `jcode-terminal-image`, `jcode-terminal-launch`, `jcode-terminal-prompt`
- **build** (2): `jcode-build-meta`, `jcode-build-support`
- **tool** (2): `jcode-tool-core`, `jcode-tool-types`
- **harness** (2): `jcode-harness-api`, `jcode-harness-api-server`
- **jcode** (1): `jcode`
- **companion** (1): `jcode-companion`
- **base** (1): `jcode-base`
- **agent** (1): `jcode-agent-runtime`
- **applet** (1): `jcode-applet-types`
- **auth** (1): `jcode-auth-types`
- **azure** (1): `jcode-azure-auth`
- **background** (1): `jcode-background-types`
- **batch** (1): `jcode-batch-types`
- **message** (1): `jcode-message-types`
- **compaction** (1): `jcode-compaction-core`
- **config** (1): `jcode-config-types`
- **core** (1): `jcode-core`
- **embedding** (1): `jcode-embedding`
- **gateway** (1): `jcode-gateway-types`
- **import** (1): `jcode-import-core`
- **logging** (1): `jcode-logging`
- **storage** (1): `jcode-storage`
- **memory** (1): `jcode-memory-types`
- **plan** (1): `jcode-plan`
- **protocol** (1): `jcode-protocol`
- **schema** (1): `jcode-schema-dialect`
- **usage** (1): `jcode-usage-types`
- **selfdev** (1): `jcode-selfdev-types`
- **session** (1): `jcode-session-types`
- **side** (1): `jcode-side-panel-types`
- **render** (1): `jcode-render-core`
- **task** (1): `jcode-task-types`
- **telemetry** (1): `jcode-telemetry-core`
- **todo** (1): `jcode-todo-policy`
- **transport** (1): `jcode-transport`
- **desktop** (1): `jcode-desktop-open`
- **voice** (1): `jcode-voice`
- **app** (1): `jcode-app-core`
- **ambient** (1): `jcode-ambient-types`
- **codemode** (1): `jcode-codemode`
- **command** (1): `jcode-command-risk`
- **notify** (1): `jcode-notify-email`
- **overnight** (1): `jcode-overnight-core`
- **pdf** (1): `jcode-pdf`
- **setup** (1): `jcode-setup-hints`
- **swarm** (1): `jcode-swarm-core`
- **update** (1): `jcode-update-core`
- **fuzzy** (1): `jcode-fuzzy`
- **productivity** (1): `jcode-productivity-core`
- **sdk** (1): `jcode-sdk`

## 8. Edge list (A → B = A depends on B) — runtime/build only

- `jcode` → `jcode-build-meta`, `jcode-companion`, `jcode-harness-api-server`, `jcode-provider-anthropic-runtime`, `jcode-provider-antigravity-runtime`, `jcode-provider-copilot-runtime`, `jcode-provider-cursor-runtime`, `jcode-provider-doctor`, `jcode-provider-gemini-runtime`, `jcode-provider-grok-build-runtime`, `jcode-provider-openai-runtime`, `jcode-provider-openrouter-runtime`, `jcode-selfdev-types`, `jcode-terminal-prompt`, `jcode-tui`, `jcode-tui-session-picker`, `jcode-tui-style`, `jcode-voice`
- `jcode-app-core` → `jcode-agent-runtime`, `jcode-ambient-types`, `jcode-applet-types`, `jcode-base`, `jcode-build-meta`, `jcode-build-support`, `jcode-codemode`, `jcode-command-risk`, `jcode-core`, `jcode-desktop-open`, `jcode-harness-api`, `jcode-import-core`, `jcode-message-types`, `jcode-notify-email`, `jcode-overnight-core`, `jcode-pdf`, `jcode-plan`, `jcode-provider-core`, `jcode-schema-dialect`, `jcode-selfdev-types`, `jcode-session-types`, `jcode-setup-hints`, `jcode-swarm-core`, `jcode-task-types`, `jcode-tool-core`, `jcode-tool-types`, `jcode-update-core`
- `jcode-base` → `jcode-agent-runtime`, `jcode-applet-types`, `jcode-auth-types`, `jcode-azure-auth`, `jcode-background-types`, `jcode-batch-types`, `jcode-build-meta`, `jcode-compaction-core`, `jcode-config-types`, `jcode-core`, `jcode-embedding`, `jcode-gateway-types`, `jcode-import-core`, `jcode-logging`, `jcode-memory-types`, `jcode-message-types`, `jcode-plan`, `jcode-protocol`, `jcode-provider-antigravity`, `jcode-provider-bedrock`, `jcode-provider-copilot`, `jcode-provider-core`, `jcode-provider-env`, `jcode-provider-gemini`, `jcode-provider-metadata`, `jcode-provider-openai`, `jcode-provider-openrouter`, `jcode-render-core`, `jcode-selfdev-types`, `jcode-session-types`, `jcode-side-panel-types`, `jcode-storage`, `jcode-task-types`, `jcode-telemetry-core`, `jcode-terminal-image`, `jcode-terminal-launch`, `jcode-todo-policy`, `jcode-tool-core`, `jcode-tool-types`, `jcode-transport`, `jcode-usage-types`
- `jcode-batch-types` → `jcode-message-types`
- `jcode-build-support` → `jcode-core`, `jcode-selfdev-types`, `jcode-storage`
- `jcode-compaction-core` → `jcode-message-types`
- `jcode-companion` → `jcode-base`, `jcode-desktop-open`, `jcode-transport`, `jcode-voice`
- `jcode-harness-api` → `jcode-applet-types`, `jcode-session-types`, `jcode-side-panel-types`, `jcode-todo-policy`, `jcode-usage-types`
- `jcode-harness-api-server` → `jcode-harness-api`, `jcode-session-types`, `jcode-todo-policy`, `jcode-transport`
- `jcode-logging` → `jcode-core`, `jcode-storage`
- `jcode-productivity-core` → `jcode-storage`
- `jcode-protocol` → `jcode-applet-types`, `jcode-batch-types`, `jcode-config-types`, `jcode-message-types`, `jcode-plan`, `jcode-provider-core`, `jcode-selfdev-types`, `jcode-session-types`, `jcode-side-panel-types`
- `jcode-provider-anthropic` → `jcode-logging`, `jcode-message-types`, `jcode-provider-core`, `jcode-schema-dialect`
- `jcode-provider-anthropic-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-anthropic`, `jcode-provider-core`
- `jcode-provider-antigravity` → `jcode-provider-gemini`, `jcode-schema-dialect`
- `jcode-provider-antigravity-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-antigravity`, `jcode-provider-core`, `jcode-provider-gemini`, `jcode-schema-dialect`
- `jcode-provider-bedrock` → `jcode-core`, `jcode-logging`, `jcode-message-types`, `jcode-provider-core`, `jcode-provider-env`, `jcode-storage`
- `jcode-provider-copilot` → `jcode-message-types`, `jcode-schema-dialect`
- `jcode-provider-copilot-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-copilot`, `jcode-provider-core`, `jcode-provider-openai`
- `jcode-provider-core` → `jcode-logging`, `jcode-message-types`, `jcode-schema-dialect`, `jcode-usage-types`
- `jcode-provider-cursor-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-core`
- `jcode-provider-doctor` → `jcode-base`, `jcode-provider-anthropic-runtime`, `jcode-provider-antigravity-runtime`, `jcode-provider-copilot-runtime`, `jcode-provider-cursor-runtime`, `jcode-provider-gemini-runtime`, `jcode-provider-openai-runtime`, `jcode-provider-openrouter-runtime`
- `jcode-provider-env` → `jcode-core`, `jcode-logging`, `jcode-provider-metadata`, `jcode-storage`
- `jcode-provider-gemini` → `jcode-message-types`, `jcode-schema-dialect`
- `jcode-provider-gemini-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-core`, `jcode-provider-gemini`, `jcode-schema-dialect`
- `jcode-provider-grok-build-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-core`, `jcode-provider-openrouter-runtime`
- `jcode-provider-openai` → `jcode-core`, `jcode-logging`, `jcode-message-types`, `jcode-provider-core`
- `jcode-provider-openai-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-core`, `jcode-provider-openai`, `jcode-schema-dialect`
- `jcode-provider-openrouter` → `jcode-core`, `jcode-logging`, `jcode-message-types`, `jcode-schema-dialect`
- `jcode-provider-openrouter-runtime` → `jcode-base`, `jcode-message-types`, `jcode-provider-core`, `jcode-provider-openrouter`
- `jcode-sdk` → `jcode-applet-types`, `jcode-harness-api`, `jcode-provider-metadata`, `jcode-todo-policy`, `jcode-transport`
- `jcode-session-types` → `jcode-message-types`
- `jcode-setup-hints` → `jcode-build-meta`, `jcode-config-types`, `jcode-core`, `jcode-logging`, `jcode-storage`, `jcode-terminal-launch`
- `jcode-storage` → `jcode-core`
- `jcode-swarm-core` → `jcode-plan`
- `jcode-telemetry-core` → `jcode-build-meta`, `jcode-core`, `jcode-logging`, `jcode-provider-core`, `jcode-selfdev-types`, `jcode-storage`, `jcode-usage-types`
- `jcode-todo-policy` → `jcode-task-types`
- `jcode-tool-core` → `jcode-agent-runtime`, `jcode-message-types`, `jcode-tool-types`
- `jcode-tui` → `jcode-app-core`, `jcode-build-meta`, `jcode-config-types`, `jcode-core`, `jcode-fuzzy`, `jcode-logging`, `jcode-productivity-core`, `jcode-provider-core`, `jcode-selfdev-types`, `jcode-session-types`, `jcode-terminal-image`, `jcode-terminal-prompt`, `jcode-tui-account-picker`, `jcode-tui-anim`, `jcode-tui-core`, `jcode-tui-markdown`, `jcode-tui-mermaid`, `jcode-tui-messages`, `jcode-tui-permissions`, `jcode-tui-render`, `jcode-tui-session-picker`, `jcode-tui-style`, `jcode-tui-tool-display`, `jcode-tui-usage-overlay`, `jcode-tui-visual-debug`, `jcode-tui-workspace`, `jcode-voice`
- `jcode-tui-core` → `jcode-memory-types`
- `jcode-tui-markdown` → `jcode-render-core`, `jcode-tui-mermaid`, `jcode-tui-workspace`
- `jcode-tui-mermaid` → `jcode-tui-workspace`
- `jcode-tui-messages` → `jcode-config-types`, `jcode-message-types`, `jcode-session-types`, `jcode-tui-markdown`
- `jcode-tui-permissions` → `jcode-base`, `jcode-core`, `jcode-tui-style`
- `jcode-tui-render` → `jcode-tui-style`
- `jcode-tui-session-picker` → `jcode-message-types`, `jcode-session-types`
- `jcode-tui-style` → `jcode-logging`
- `jcode-tui-usage-overlay` → `jcode-usage-types`
- `jcode-tui-visual-debug` → `jcode-logging`
- `jcode-voice` → `jcode-base`

## 9. Dev-only internal edges (test-time only; excluded from the DAG in §1–§8)

- `jcode` (dev) → `jcode-harness-api`, `jcode-provider-core`, `jcode-tui`
- `jcode-app-core` (dev) → `jcode-base`
- `jcode-base` (dev) → `jcode-provider-openrouter-runtime`, `jcode-terminal-prompt`
- `jcode-companion` (dev) → `jcode-base`
- `jcode-harness-api-server` (dev) → `jcode-background-types`, `jcode-base`
- `jcode-provider-anthropic-runtime` (dev) → `jcode-base`
- `jcode-provider-antigravity-runtime` (dev) → `jcode-base`
- `jcode-provider-copilot-runtime` (dev) → `jcode-base`
- `jcode-provider-cursor-runtime` (dev) → `jcode-base`
- `jcode-provider-doctor` (dev) → `jcode-base`, `jcode-provider-metadata`
- `jcode-provider-gemini-runtime` (dev) → `jcode-base`, `jcode-provider-anthropic`, `jcode-provider-antigravity`, `jcode-provider-openai`, `jcode-provider-openrouter`
- `jcode-provider-openai-runtime` (dev) → `jcode-base`
- `jcode-provider-openrouter-runtime` (dev) → `jcode-base`, `jcode-provider-metadata`
- `jcode-schema-dialect` (dev) → `jcode-schema-dialect`
- `jcode-todo-policy` (dev) → `jcode-core`
- `jcode-tui` (dev) → `jcode-app-core`
- `jcode-tui-mermaid` (dev) → `jcode-plan`
- `jcode-voice` (dev) → `jcode-base`

## 10. Architecture notes

- `jcode-base` is the broadest god-crate: **41 internal deps**, depended on by `jcode-app-core`, `jcode-tui`, every `provider-*-runtime`, `jcode-provider-doctor`, `jcode-companion`, `jcode-voice`, `jcode-harness-api-server`, `jcode-productivity-core`.
- `jcode-app-core` (27 deps) and `jcode-tui` (27 deps) carry the application core and the full presentation layer respectively.
- Provider family splits into 10 protocol crates (`provider-*`, keyed off `jcode-provider-core` + `jcode-schema-dialect`/`jcode-message-types`) and 8 runtime crates (`provider-*-runtime`, all routing through `jcode-base`).
- `jcode-provider-doctor` is the single diagnostics entry point: it depends on all 8 `provider-*-runtime` crates.
- Deepest shared spine: `jcode-core ← jcode-storage ← jcode-logging` — reached by `jcode-provider-core`, so these three are the highest-blast-radius leaves for a build-time change.
