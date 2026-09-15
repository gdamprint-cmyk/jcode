# Telegram gateway session bridge

**Status:** implemented, opt-in, not started automatically.

The full Telegram client is intentionally separate from Jcode ambient mode:

```text
Telegram Bot API (long polling)
        |
        v
jcode telegram gateway  --  authenticated WebSocket  --  jcode gateway/daemon
                                                            |
                                                            v
                                                   persistent jcode sessions
```

The bridge owns the Telegram `getUpdates` poller and maps each private chat, or
Telegram topic when present, to one persistent jcode session. Ambient remains an
internal background runner and its existing Telegram channel must not run at the
same time because Telegram allows only one consumer for a bot's update stream.

## Prerequisites

1. Keep the ambient Telegram reply channel disabled:

   ```toml
   [safety]
   telegram_reply_enabled = false
   ```

2. Pair a gateway device with `jcode pair`. Obtain the device token through the
   existing pairing flow. Do not put that token in shell history, repository files,
   TODOs, logs, issue reports, or chat messages.

3. Configure the bridge process using environment variables from a secure local
   secret source:

   | Variable | Required | Meaning |
   |---|---:|---|
   | `JCODE_TELEGRAM_BOT_TOKEN` | yes | Telegram Bot API token |
   | `JCODE_TELEGRAM_GATEWAY_TOKEN` | yes | 64-character hex token from `jcode pair` |
   | `JCODE_TELEGRAM_ALLOWED_USER_IDS` | yes | comma/space-separated Telegram user IDs |
   | `JCODE_TELEGRAM_GATEWAY_URL` | no | defaults to `ws://127.0.0.1:7643/ws` |
   | `JCODE_TELEGRAM_WORKING_DIR` | no | defaults to the current directory |
   | `JCODE_TELEGRAM_GATEWAY_STATE` | no | defaults to `~/.jcode/telegram/gateway_sessions.json` |
   | `JCODE_TELEGRAM_ALLOW_GROUP_CHATS` | no | opt-in only, defaults to deny |

   The bridge refuses to start without an explicit user allowlist. Private chats
   are the default. Group and topic operation is an explicit opt-in and should be
   enabled only after a separate access review.

## Run

Start it as a foreground process so its lifecycle is visible and controllable:

```text
jcode telegram gateway
```

It is not started by ordinary Jcode startup, ambient startup, or the gateway
server. Run it under a supervisor only after the first manual harmless test.

Supported commands:

- `/new` create and bind a new session
- `/sessions` list sessions known by this bridge for the current chat/topic
- `/resume SESSION_ID` attach an existing session ID
- `/model MODEL_ID` change the active session model
- `/rename TITLE` rename the active session
- `/status` show session state
- `/cancel` request cancellation
- `/continue` ask the active session to continue from a safe point
- `/help` show help
- Any other text is sent to the active session

Responses are streamed through throttled Telegram message edits, typing status is
refreshed while a request is running, and final responses are split below the Bot
API message limit. The state file is written using a temporary file and rename.
The durable offset prevents normal restart replay after an update has been
processed.

## Safety and current boundaries

- Authorization requires an allowlisted `from.id`; a private-chat default also
  rejects group and supergroup updates unless explicitly enabled.
- The bridge never prints bot or gateway tokens. Network errors are intentionally
  generic with respect to the Bot API URL.
- The bridge uses the gateway's bearer-token WebSocket protocol. It does not expose
  a new listener and does not bypass gateway pairing.
- Tool events are displayed as progress only. The current gateway wire protocol has
  no tool-approval request variant, so the bridge does not invent approval buttons
  or pretend that a Telegram click authorized a tool. Approval support requires a
  protocol-level extension and a separate security review.
- `/sessions` lists bindings known to this bridge, not every session in the daemon.
  `/resume SESSION_ID` is the explicit way to attach a known existing ID.
- The bridge currently processes one Telegram update at a time and does not provide
  a per-chat command queue or rate limiter. Keep the allowlist narrow and do not
  expose it to untrusted group traffic.
- A real Telegram end-to-end test is intentionally not part of build validation:
  it polls the user's bot and opens a real gateway session. Perform it only in a
  deliberate test window with credentials supplied through a secure local source.

## Rollback

Stop the bridge process. No Jcode configuration is changed by starting it. To
remove its local chat bindings, stop it and delete only the bridge state file after
confirming that its contents contain no other application data. Never delete the
Jcode session store or gateway pairing file as part of Telegram rollback.
