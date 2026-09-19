use crate::logging;
use serde::Deserialize;

const API_BASE: &str = "https://api.telegram.org/bot";

fn redact_bot_token(text: &str, bot_token: &str) -> String {
    if bot_token.is_empty() {
        text.to_string()
    } else {
        text.replace(bot_token, "<redacted>")
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramMessage {
    pub text: Option<String>,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    #[serde(rename = "date")]
    pub _date: i64,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TelegramCallbackQuery {
    pub id: String,
    pub from: TelegramUser,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
    #[serde(default)]
    pub data: Option<String>,
}

pub fn default_inline_keyboard() -> serde_json::Value {
    serde_json::json!({
        "inline_keyboard": [[
            { "text": "Status", "callback_data": "jcode:status" },
            { "text": "Continue", "callback_data": "jcode:continue" },
            { "text": "Help", "callback_data": "jcode:help" }
        ]]
    })
}

pub async fn send_message(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let url = format!("{}{}/sendMessage", API_BASE, bot_token);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "disable_web_page_preview": true,
        }))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Telegram request failed: {}",
                redact_bot_token(&error.to_string(), bot_token)
            )
        })?;

    let status = resp.status();
    let body: TelegramResponse<serde_json::Value> = resp.json().await?;

    if !body.ok {
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            redact_bot_token(&body.description.unwrap_or_default(), bot_token)
        );
    }

    logging::info("Telegram notification sent");
    Ok(())
}

pub async fn send_message_with_inline_keyboard(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    reply_markup: serde_json::Value,
) -> anyhow::Result<()> {
    let url = format!("{}{}/sendMessage", API_BASE, bot_token);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "disable_web_page_preview": true,
            "reply_markup": reply_markup,
        }))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Telegram request failed: {}",
                redact_bot_token(&error.to_string(), bot_token)
            )
        })?;

    let status = resp.status();
    let body: TelegramResponse<serde_json::Value> = resp.json().await?;
    if !body.ok {
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            redact_bot_token(&body.description.unwrap_or_default(), bot_token)
        );
    }
    Ok(())
}

pub async fn send_chat_action(
    client: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    action: &str,
) -> anyhow::Result<()> {
    let url = format!("{}{}/sendChatAction", API_BASE, bot_token);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "action": action,
        }))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Telegram chat action failed: {}",
                redact_bot_token(&error.to_string(), bot_token)
            )
        })?;
    let status = resp.status();
    let body: TelegramResponse<serde_json::Value> = resp.json().await?;
    if !body.ok {
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            redact_bot_token(&body.description.unwrap_or_default(), bot_token)
        );
    }
    Ok(())
}

pub async fn answer_callback_query(
    client: &reqwest::Client,
    bot_token: &str,
    callback_query_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let url = format!("{}{}/answerCallbackQuery", API_BASE, bot_token);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "callback_query_id": callback_query_id,
            "text": text,
            "show_alert": false,
        }))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Telegram callback answer failed: {}",
                redact_bot_token(&error.to_string(), bot_token)
            )
        })?;
    let status = resp.status();
    let body: TelegramResponse<serde_json::Value> = resp.json().await?;
    if !body.ok {
        anyhow::bail!(
            "Telegram API error ({}): {}",
            status,
            redact_bot_token(&body.description.unwrap_or_default(), bot_token)
        );
    }
    Ok(())
}

pub async fn get_updates(
    client: &reqwest::Client,
    bot_token: &str,
    offset: Option<i64>,
    timeout_secs: u64,
) -> anyhow::Result<Vec<Update>> {
    let url = format!("{}{}/getUpdates", API_BASE, bot_token);
    let mut params = serde_json::json!({
        "timeout": timeout_secs,
        "allowed_updates": ["message", "callback_query"],
    });

    if let Some(off) = offset {
        params["offset"] = serde_json::json!(off);
    }

    let resp = client
        .post(&url)
        .json(&params)
        .timeout(std::time::Duration::from_secs(timeout_secs + 5))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Telegram request failed: {}",
                redact_bot_token(&error.to_string(), bot_token)
            )
        })?;

    let body: TelegramResponse<Vec<Update>> = resp.json().await?;

    if !body.ok {
        anyhow::bail!(
            "Telegram getUpdates error: {}",
            redact_bot_token(&body.description.unwrap_or_default(), bot_token)
        );
    }

    Ok(body.result.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Telegram hardening helpers (added 2026-09-14)
// ---------------------------------------------------------------------------

/// Plain ACK sent immediately after receiving a Telegram message, before any
/// processing starts. This prevents the Telegram server from retrying delivery
/// while the agent is still working on the previous message.
pub fn plain_ack() -> &'static str {
    "OK"
}

/// Bounded reconnect: exponential backoff capped at TELEGRAM_MAX_RECONNECT_ATTEMPTS.
/// After the cap is reached, the function returns None instead of retrying forever.
pub fn next_reconnect_delay(attempt: u64) -> Option<u64> {
    if attempt >= TELEGRAM_MAX_RECONNECT_ATTEMPTS as u64 {
        return None;
    }
    let delay = TELEGRAM_RECONNECT_BASE_SECS
        * 2u64.pow(attempt.min((TELEGRAM_MAX_RECONNECT_ATTEMPTS - 1) as u64) as u32);
    Some(delay.min(300))
}

/// Webhook mode: Telegram pushes updates to a URL. Polling mode: Jcode
/// periodically calls getUpdates. Use this to switch between modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramMode {
    Polling,
    Webhook,
}

impl TelegramMode {
    pub fn from_config(enabled: bool, webhook_url: Option<&str>) -> Self {
        if enabled && webhook_url.is_some() && !webhook_url.unwrap().is_empty() {
            TelegramMode::Webhook
        } else {
            TelegramMode::Polling
        }
    }
}

const TELEGRAM_MAX_INBOUND_CHARS: usize = 4000;
const TELEGRAM_MAX_RECONNECT_ATTEMPTS: u32 = 10;
const TELEGRAM_RECONNECT_BASE_SECS: u64 = 2;

fn telegram_text_is_acceptable(text: &str) -> bool {
    text.chars().count() <= TELEGRAM_MAX_INBOUND_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_update() {
        let json = r#"{
            "update_id": 123,
            "message": {
                "text": "hello",
                "chat": {"id": 456},
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        assert_eq!(update.update_id, 123);
        assert_eq!(update.message.unwrap().text.unwrap(), "hello");
    }

    #[test]
    fn test_parse_sender_and_chat_type() {
        let json = r#"{
            "update_id": 123,
            "message": {
                "text": "hello",
                "chat": {"id": 456, "type": "private"},
                "from": {"id": 789},
                "date": 1700000000
            }
        }"#;
        let update: Update = serde_json::from_str(json).unwrap();
        let message = update.message.unwrap();
        assert_eq!(message.chat.kind.as_deref(), Some("private"));
        assert_eq!(message.from.unwrap().id, 789);
    }

    #[test]
    fn test_parse_response() {
        let json = r#"{"ok": true, "result": []}"#;
        let resp: TelegramResponse<Vec<Update>> = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.result.unwrap().is_empty());
    }

    #[test]
    fn test_redact_bot_token() {
        assert_eq!(
            redact_bot_token(
                "https://api.telegram.org/botsecret-token/getUpdates",
                "secret-token"
            ),
            "https://api.telegram.org/bot<redacted>/getUpdates"
        );
    }

    #[test]
    fn test_plain_ack() {
        assert_eq!(plain_ack(), "OK");
    }

    #[test]
    fn test_next_reconnect_delay_bounded() {
        // First few attempts should return increasing delays
        assert_eq!(next_reconnect_delay(0), Some(2));
        assert_eq!(next_reconnect_delay(1), Some(4));
        assert_eq!(next_reconnect_delay(2), Some(8));
        assert_eq!(next_reconnect_delay(3), Some(16));
        assert_eq!(next_reconnect_delay(4), Some(32));
        assert_eq!(next_reconnect_delay(5), Some(64));
        assert_eq!(next_reconnect_delay(6), Some(128));
        assert_eq!(next_reconnect_delay(7), Some(256));
        assert_eq!(next_reconnect_delay(8), Some(300)); // capped at 300
        assert_eq!(next_reconnect_delay(9), Some(300)); // capped at 300
        // After max attempts, returns None
        assert_eq!(next_reconnect_delay(10), None);
        assert_eq!(next_reconnect_delay(11), None);
        assert_eq!(next_reconnect_delay(100), None);
    }

    #[test]
    fn test_telegram_mode_from_config() {
        assert_eq!(
            TelegramMode::from_config(false, None),
            TelegramMode::Polling
        );
        assert_eq!(TelegramMode::from_config(true, None), TelegramMode::Polling);
        assert_eq!(
            TelegramMode::from_config(true, Some("")),
            TelegramMode::Polling
        );
        assert_eq!(
            TelegramMode::from_config(true, Some("https://example.com/webhook")),
            TelegramMode::Webhook
        );
    }
}
