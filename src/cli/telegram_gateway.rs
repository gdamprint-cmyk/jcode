//! Full Telegram client for persistent jcode sessions.
//!
//! This is deliberately separate from the ambient notification channel. It owns
//! Telegram long-polling and talks to the jcode daemon through the authenticated
//! gateway WebSocket. The bridge keeps only chat/topic -> session IDs on disk.

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::protocol::{Request, ServerEvent};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org/bot";
const DEFAULT_GATEWAY_URL: &str = "ws://127.0.0.1:7643/ws";
const DEFAULT_STATE_FILE: &str = "telegram/gateway_sessions.json";
const MAX_TELEGRAM_CHARS: usize = 4000;
const MAX_SESSION_ID_CHARS: usize = 128;
const MAX_MODEL_CHARS: usize = 256;
const MAX_TITLE_CHARS: usize = 100;
const STREAM_EDIT_INTERVAL: Duration = Duration::from_millis(900);
const STREAM_EDIT_MIN_DELTA: usize = 160;
const CONTINUATION_SUFFIX: &str = "\n\n… ответ продолжается в следующем сообщении …";

#[derive(Clone, Debug)]
struct BridgeConfig {
    bot_token: String,
    gateway_url: String,
    gateway_token: String,
    working_dir: String,
    allowed_user_ids: HashSet<i64>,
    allow_group_chats: bool,
    state_path: PathBuf,
}

impl BridgeConfig {
    fn from_env() -> Result<Self> {
        // The existing ambient reply loop would compete for getUpdates. Full
        // Telegram sessions must be the sole poller, so fail closed rather than
        // silently losing updates between two independent offsets.
        if crate::config::config().safety.telegram_reply_enabled {
            bail!(
                "Telegram full-session bridge requires [safety] telegram_reply_enabled = false; "
                    .to_string()
                    + "ambient/channel polling must be disabled first"
            );
        }

        let bot_token = required_env("JCODE_TELEGRAM_BOT_TOKEN")?;
        let gateway_token = required_env("JCODE_TELEGRAM_GATEWAY_TOKEN")?;
        if gateway_token.len() != 64 || !gateway_token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!(
                "JCODE_TELEGRAM_GATEWAY_TOKEN must be the 64-character hex device token from `jcode pair`"
            );
        }

        let gateway_url = env::var("JCODE_TELEGRAM_GATEWAY_URL")
            .unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_string());
        if !(gateway_url.starts_with("ws://") || gateway_url.starts_with("wss://")) {
            bail!("JCODE_TELEGRAM_GATEWAY_URL must use ws:// or wss://");
        }

        let allowed_user_ids = env::var("JCODE_TELEGRAM_ALLOWED_USER_IDS")
            .unwrap_or_default()
            .split([',', ';', ' ', '\n', '\t'])
            .filter_map(|value| value.trim().parse::<i64>().ok())
            .collect::<HashSet<_>>();
        if allowed_user_ids.is_empty() {
            bail!(
                "JCODE_TELEGRAM_ALLOWED_USER_IDS is required and must contain at least one numeric user ID"
            );
        }

        let working_dir = env::var("JCODE_TELEGRAM_WORKING_DIR")
            .or_else(|_| env::current_dir().map(|path| path.to_string_lossy().into_owned()))
            .context("resolve Telegram bridge working directory")?;
        if !Path::new(&working_dir).is_dir() {
            bail!("JCODE_TELEGRAM_WORKING_DIR is not an existing directory");
        }

        let state_path = env::var_os("JCODE_TELEGRAM_GATEWAY_STATE")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".jcode").join(DEFAULT_STATE_FILE)))
            .context("resolve Telegram bridge state path")?;

        Ok(Self {
            bot_token,
            gateway_url,
            gateway_token,
            working_dir,
            allowed_user_ids,
            allow_group_chats: env_bool("JCODE_TELEGRAM_ALLOW_GROUP_CHATS"),
            state_path,
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is required"))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(value)
}

fn env_bool(name: &str) -> bool {
    matches!(
        env::var(name).ok().as_deref().map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "on")
    )
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    result: Option<T>,
}

#[derive(Clone)]
struct TelegramApi {
    client: reqwest::Client,
    bot_token: String,
}

impl TelegramApi {
    fn new(bot_token: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("jcode-telegram-gateway")
            .build()
            .context("create Telegram HTTP client")?;
        Ok(Self { client, bot_token })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}{}/{}", TELEGRAM_API_BASE, self.bot_token, method);
        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Telegram {method} request failed"))?;
        let status = response.status();
        let body: TelegramResponse<T> = response
            .json()
            .await
            .with_context(|| format!("decode Telegram {method} response"))?;
        if !body.ok {
            bail!(
                "Telegram {method} failed (HTTP {status}): {}",
                body.description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        body.result
            .ok_or_else(|| anyhow::anyhow!("Telegram {method} returned no result"))
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<BotUpdate>> {
        let mut payload = json!({
            "timeout": 30,
            "allowed_updates": ["message"],
        });
        if let Some(offset) = offset {
            payload["offset"] = json!(offset);
        }
        self.call("getUpdates", payload).await
    }

    async fn send_message(&self, chat_id: i64, thread_id: Option<i64>, text: &str) -> Result<i64> {
        let mut payload = json!({
            "chat_id": chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        if let Some(thread_id) = thread_id {
            payload["message_thread_id"] = json!(thread_id);
        }
        Ok(self
            .call::<SentMessage>("sendMessage", payload)
            .await?
            .message_id)
    }

    async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        self.call::<serde_json::Value>(
            "editMessageText",
            json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": text,
                "disable_web_page_preview": true,
            }),
        )
        .await
        .map(|_| ())
    }

    async fn send_chat_action(&self, chat_id: i64, thread_id: Option<i64>) -> Result<()> {
        let mut payload = json!({
            "chat_id": chat_id,
            "action": "typing",
        });
        if let Some(thread_id) = thread_id {
            payload["message_thread_id"] = json!(thread_id);
        }
        self.call::<bool>("sendChatAction", payload)
            .await
            .map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
struct BotUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<BotMessage>,
}

#[derive(Debug, Deserialize)]
struct BotMessage {
    chat: BotChat,
    #[serde(default)]
    from: Option<BotUser>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    message_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BotChat {
    id: i64,
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BotUser {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct SentMessage {
    message_id: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionBinding {
    chat_id: i64,
    #[serde(default)]
    thread_id: Option<i64>,
    session_id: String,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct BridgeState {
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    bindings: BTreeMap<String, SessionBinding>,
}

impl BridgeState {
    fn key(chat_id: i64, thread_id: Option<i64>) -> String {
        format!("{chat_id}:{}", thread_id.unwrap_or_default())
    }

    fn binding(&self, chat_id: i64, thread_id: Option<i64>) -> Option<SessionBinding> {
        self.bindings.get(&Self::key(chat_id, thread_id)).cloned()
    }
}

fn load_state(path: &Path) -> Result<BridgeState> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BridgeState::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read Telegram state {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("decode Telegram state {}", path.display()))
}

fn save_state(path: &Path, state: &BridgeState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create Telegram state directory {}", parent.display()))?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state).context("encode Telegram state")?;
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("write Telegram state temp file {}", temporary.display()))?;
    atomic_replace(&temporary, path)
        .with_context(|| format!("replace Telegram state {}", path.display()))
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

type GatewaySocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct GatewayConnection {
    socket: GatewaySocket,
    next_id: u64,
}

impl GatewayConnection {
    async fn connect(config: &BridgeConfig, client_instance_id: &str) -> Result<Self> {
        let mut request = config
            .gateway_url
            .clone()
            .into_client_request()
            .context("build gateway WebSocket request")?;
        let authorization = format!("Bearer {}", config.gateway_token);
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&authorization).context("build gateway authorization header")?,
        );
        request.headers_mut().insert(
            "x-jcode-client-instance",
            HeaderValue::from_str(client_instance_id)
                .context("build gateway client instance header")?,
        );
        let (socket, _) = connect_async(request)
            .await
            .context("connect to jcode gateway")?;
        Ok(Self { socket, next_id: 1 })
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    async fn send(&mut self, request: Request) -> Result<()> {
        let payload = serde_json::to_string(&request).context("encode gateway request")?;
        self.socket
            .send(Message::Text(payload.into()))
            .await
            .context("send gateway request")
    }

    async fn next_event(&mut self) -> Result<ServerEvent> {
        loop {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("gateway WebSocket closed"))?
                .context("receive gateway WebSocket frame")?;
            match message {
                Message::Text(text) => {
                    return serde_json::from_str(text.as_ref())
                        .context("decode gateway server event");
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .await
                        .context("reply to gateway ping")?;
                }
                Message::Close(_) => bail!("gateway WebSocket closed"),
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }
}

struct Bridge {
    config: BridgeConfig,
    bot: TelegramApi,
    state: Mutex<BridgeState>,
}

impl Bridge {
    fn new(config: BridgeConfig) -> Result<Self> {
        let bot = TelegramApi::new(config.bot_token.clone())?;
        let state = load_state(&config.state_path)?;
        Ok(Self {
            config,
            bot,
            state: Mutex::new(state),
        })
    }

    async fn save_state(&self) -> Result<()> {
        let state = self.state.lock().await.clone();
        save_state(&self.config.state_path, &state)
    }

    fn authorized(&self, message: &BotMessage) -> bool {
        if !self.config.allow_group_chats && message.chat.kind.as_deref() != Some("private") {
            return false;
        }
        message
            .from
            .as_ref()
            .is_some_and(|user| self.config.allowed_user_ids.contains(&user.id))
    }

    async fn run(&self) -> Result<()> {
        eprintln!(
            "Telegram full-session bridge active. Gateway: {}. State: {}",
            self.config.gateway_url,
            self.config.state_path.display()
        );
        let mut reconnect_attempt = 0u32;
        loop {
            let offset = self.state.lock().await.offset;
            match self.bot.get_updates(offset).await {
                Ok(updates) => {
                    reconnect_attempt = 0;
                    for update in updates {
                        let next_offset = update.update_id.saturating_add(1);
                        if let Some(message) = update.message {
                            if self.authorized(&message) {
                                if let Err(error) = self.handle_message(&message).await {
                                    let _ = self
                                        .send_text(
                                            message.chat.id,
                                            message.message_thread_id,
                                            &format!("❌ Bridge error: {error}"),
                                        )
                                        .await;
                                }
                            }
                        }
                        self.state.lock().await.offset = Some(next_offset);
                        self.save_state().await?;
                    }
                }
                Err(error) => {
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    let exponent = reconnect_attempt.min(8);
                    let delay = (2u64.saturating_pow(exponent)).min(300);
                    eprintln!("Telegram polling error; retrying in {delay}s: {error}");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }

    async fn send_text(&self, chat_id: i64, thread_id: Option<i64>, text: &str) -> Result<()> {
        let chunks = split_text(text, MAX_TELEGRAM_CHARS);
        if chunks.is_empty() {
            self.bot.send_message(chat_id, thread_id, " ").await?;
        } else {
            for chunk in chunks {
                self.bot.send_message(chat_id, thread_id, &chunk).await?;
            }
        }
        Ok(())
    }

    async fn bind_session(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        session_id: String,
        title: Option<String>,
    ) -> Result<()> {
        let key = BridgeState::key(chat_id, thread_id);
        self.state.lock().await.bindings.insert(
            key,
            SessionBinding {
                chat_id,
                thread_id,
                session_id,
                title,
            },
        );
        self.save_state().await
    }

    async fn current_binding(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
    ) -> Option<SessionBinding> {
        self.state.lock().await.binding(chat_id, thread_id)
    }

    async fn connect_session(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        target_session_id: Option<&str>,
    ) -> Result<(GatewayConnection, String)> {
        let client_instance_id = format!("telegram-{}", BridgeState::key(chat_id, thread_id));
        let mut connection = GatewayConnection::connect(&self.config, &client_instance_id).await?;
        let subscribe_id = connection.next_id();
        connection
            .send(Request::Subscribe {
                id: subscribe_id,
                working_dir: Some(self.config.working_dir.clone()),
                selfdev: None,
                target_session_id: target_session_id.map(str::to_string),
                client_instance_id: Some(client_instance_id),
                client_has_local_history: false,
                allow_session_takeover: true,
                crash_on_disconnect: false,
                continue_on_disconnect: true,
                terminal_env: Vec::new(),
            })
            .await?;

        let mut session_id = target_session_id.unwrap_or_default().to_string();
        loop {
            match connection.next_event().await? {
                ServerEvent::SessionId {
                    session_id: assigned,
                } => session_id = assigned,
                ServerEvent::Error { id, message, .. } if id == subscribe_id => bail!(message),
                ServerEvent::Done { id } if id == subscribe_id => break,
                _ => {}
            }
        }
        if session_id.is_empty() {
            bail!("gateway did not assign a session ID");
        }
        Ok((connection, session_id))
    }

    async fn handle_message(&self, message: &BotMessage) -> Result<()> {
        let text = message.text.as_deref().unwrap_or_default().trim();
        if text.is_empty() {
            return Ok(());
        }
        if validate_prompt(text).is_err() {
            return self
                .send_text(
                    message.chat.id,
                    message.message_thread_id,
                    "❌ Сообщение слишком длинное. Ограничение bridge: 4000 символов.",
                )
                .await;
        }
        match parse_command(text) {
            BridgeCommand::Help => {
                self.send_text(message.chat.id, message.message_thread_id, HELP_TEXT)
                    .await
            }
            BridgeCommand::New => {
                let (_, session_id) = self
                    .connect_session(message.chat.id, message.message_thread_id, None)
                    .await?;
                self.bind_session(
                    message.chat.id,
                    message.message_thread_id,
                    session_id.clone(),
                    None,
                )
                .await?;
                self.send_text(
                    message.chat.id,
                    message.message_thread_id,
                    &format!("✅ Новая jcode-сессия: `{session_id}`"),
                )
                .await
            }
            BridgeCommand::Sessions => self.list_sessions(message).await,
            BridgeCommand::Resume(session_id) => self.resume_session(message, &session_id).await,
            BridgeCommand::Model(model) => self.set_model(message, &model).await,
            BridgeCommand::Rename(title) => self.rename_session(message, &title).await,
            BridgeCommand::Status => self.session_status(message).await,
            BridgeCommand::Cancel => self.cancel_session(message).await,
            BridgeCommand::Continue => {
                self.send_prompt(
                    message,
                    "Продолжи работу с текущей сессией с безопасной точки.",
                )
                .await
            }
            BridgeCommand::Text(prompt) => self.send_prompt(message, &prompt).await,
        }
    }

    async fn list_sessions(&self, message: &BotMessage) -> Result<()> {
        let state = self.state.lock().await;
        let bindings = state
            .bindings
            .values()
            .filter(|binding| {
                binding.chat_id == message.chat.id && binding.thread_id == message.message_thread_id
            })
            .collect::<Vec<_>>();
        if bindings.is_empty() {
            return self
                .send_text(
                    message.chat.id,
                    message.message_thread_id,
                    "Нет сессий, созданных этим Telegram bridge. Для подключения существующей используй `/resume SESSION_ID`.",
                )
                .await;
        }
        let mut output = String::from("Сессии этого чата:\n");
        for (index, binding) in bindings.iter().enumerate() {
            let title = binding.title.as_deref().unwrap_or("без названия");
            output.push_str(&format!(
                "{}. `{}` · {}\n",
                index + 1,
                binding.session_id,
                title
            ));
        }
        drop(state);
        self.send_text(message.chat.id, message.message_thread_id, &output)
            .await
    }

    async fn resume_session(&self, message: &BotMessage, session_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        let (_, assigned) = self
            .connect_session(message.chat.id, message.message_thread_id, Some(session_id))
            .await?;
        self.bind_session(
            message.chat.id,
            message.message_thread_id,
            assigned.clone(),
            None,
        )
        .await?;
        self.send_text(
            message.chat.id,
            message.message_thread_id,
            &format!("✅ Подключена сессия `{assigned}`"),
        )
        .await
    }

    async fn set_model(&self, message: &BotMessage, model: &str) -> Result<()> {
        validate_model(model)?;
        let Some(binding) = self
            .current_binding(message.chat.id, message.message_thread_id)
            .await
        else {
            return self
                .send_text(
                    message.chat.id,
                    message.message_thread_id,
                    "Сначала создай или возобнови сессию: `/new` или `/resume SESSION_ID`.",
                )
                .await;
        };
        let (mut connection, _) = self
            .connect_session(
                message.chat.id,
                message.message_thread_id,
                Some(&binding.session_id),
            )
            .await?;
        let id = connection.next_id();
        connection
            .send(Request::SetModel {
                id,
                model: model.to_string(),
            })
            .await?;
        loop {
            match connection.next_event().await? {
                ServerEvent::ModelChanged {
                    id: event_id,
                    model,
                    error,
                    ..
                } if event_id == id => {
                    let response = error.map_or_else(
                        || format!("✅ Модель: `{model}`"),
                        |error| format!("❌ Модель не сменена: {error}"),
                    );
                    return self
                        .send_text(message.chat.id, message.message_thread_id, &response)
                        .await;
                }
                ServerEvent::Error {
                    id: event_id,
                    message: gateway_error,
                    ..
                } if event_id == id => {
                    return self
                        .send_text(
                            message.chat.id,
                            message.message_thread_id,
                            &format!("❌ {gateway_error}"),
                        )
                        .await;
                }
                _ => {}
            }
        }
    }

    async fn rename_session(&self, message: &BotMessage, title: &str) -> Result<()> {
        validate_title(title)?;
        let Some(binding) = self
            .current_binding(message.chat.id, message.message_thread_id)
            .await
        else {
            return self
                .send_text(
                    message.chat.id,
                    message.message_thread_id,
                    "Нет активной Telegram-сессии.",
                )
                .await;
        };
        let (mut connection, _) = self
            .connect_session(
                message.chat.id,
                message.message_thread_id,
                Some(&binding.session_id),
            )
            .await?;
        let id = connection.next_id();
        connection
            .send(Request::RenameSession {
                id,
                title: Some(title.to_string()),
            })
            .await?;
        loop {
            match connection.next_event().await? {
                ServerEvent::SessionRenamed {
                    session_id,
                    display_title,
                    ..
                } if session_id == binding.session_id => {
                    self.bind_session(
                        message.chat.id,
                        message.message_thread_id,
                        session_id,
                        Some(display_title.clone()),
                    )
                    .await?;
                    return self
                        .send_text(
                            message.chat.id,
                            message.message_thread_id,
                            &format!("✅ Название: {display_title}"),
                        )
                        .await;
                }
                ServerEvent::Error {
                    id: event_id,
                    message: gateway_error,
                    ..
                } if event_id == id => {
                    return self
                        .send_text(
                            message.chat.id,
                            message.message_thread_id,
                            &format!("❌ {gateway_error}"),
                        )
                        .await;
                }
                _ => {}
            }
        }
    }

    async fn session_status(&self, message: &BotMessage) -> Result<()> {
        let Some(binding) = self
            .current_binding(message.chat.id, message.message_thread_id)
            .await
        else {
            return self
                .send_text(
                    message.chat.id,
                    message.message_thread_id,
                    "Нет активной Telegram-сессии.",
                )
                .await;
        };
        let (mut connection, _) = self
            .connect_session(
                message.chat.id,
                message.message_thread_id,
                Some(&binding.session_id),
            )
            .await?;
        let id = connection.next_id();
        connection.send(Request::GetState { id }).await?;
        loop {
            match connection.next_event().await? {
                ServerEvent::State {
                    id: event_id,
                    session_id,
                    message_count,
                    is_processing,
                } if event_id == id => {
                    let status = if is_processing {
                        "выполняется"
                    } else {
                        "ожидает ввода"
                    };
                    return self.send_text(message.chat.id, message.message_thread_id, &format!("Сессия `{session_id}`\nСтатус: {status}\nСообщений: {message_count}")).await;
                }
                ServerEvent::Error {
                    id: event_id,
                    message: gateway_error,
                    ..
                } if event_id == id => {
                    return self
                        .send_text(
                            message.chat.id,
                            message.message_thread_id,
                            &format!("❌ {gateway_error}"),
                        )
                        .await;
                }
                _ => {}
            }
        }
    }

    async fn cancel_session(&self, message: &BotMessage) -> Result<()> {
        let Some(binding) = self
            .current_binding(message.chat.id, message.message_thread_id)
            .await
        else {
            return self
                .send_text(
                    message.chat.id,
                    message.message_thread_id,
                    "Нет активной Telegram-сессии.",
                )
                .await;
        };
        let (mut connection, _) = self
            .connect_session(
                message.chat.id,
                message.message_thread_id,
                Some(&binding.session_id),
            )
            .await?;
        let id = connection.next_id();
        connection.send(Request::Cancel { id }).await?;
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match connection.next_event().await? {
                    ServerEvent::Interrupted => {
                        return Ok::<String, anyhow::Error>(
                            "⏹ Выполнение остановлено.".to_string(),
                        );
                    }
                    ServerEvent::Done { id: event_id } if event_id == id => {
                        return Ok("⏹ Запрос отмены принят.".to_string());
                    }
                    ServerEvent::Error {
                        id: event_id,
                        message: gateway_error,
                        ..
                    } if event_id == id => {
                        return Ok::<String, anyhow::Error>(
                            "❌ Отмена не выполнена: ".to_string() + &gateway_error,
                        );
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            Ok(
                "⏳ Команда отмены отправлена, состояние обновится при следующем событии."
                    .to_string(),
            )
        })?;
        self.send_text(message.chat.id, message.message_thread_id, &result)
            .await
    }

    async fn send_prompt(&self, message: &BotMessage, prompt: &str) -> Result<()> {
        let progress_id = self
            .bot
            .send_message(
                message.chat.id,
                message.message_thread_id,
                "⏳ Подключение к jcode…",
            )
            .await?;
        let binding = self
            .current_binding(message.chat.id, message.message_thread_id)
            .await;
        let target = binding.as_ref().map(|binding| binding.session_id.as_str());
        let (mut connection, session_id) = match self
            .connect_session(message.chat.id, message.message_thread_id, target)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = self
                    .bot
                    .edit_message(
                        message.chat.id,
                        progress_id,
                        &format!("❌ Не удалось подключить сессию: {error}"),
                    )
                    .await;
                return Ok(());
            }
        };
        if binding
            .as_ref()
            .is_none_or(|binding| binding.session_id != session_id)
        {
            self.bind_session(
                message.chat.id,
                message.message_thread_id,
                session_id.clone(),
                None,
            )
            .await?;
        }

        let typing_bot = self.bot.clone();
        let typing_chat = message.chat.id;
        let typing_thread = message.message_thread_id;
        let typing_task = tokio::spawn(async move {
            loop {
                let _ = typing_bot
                    .send_chat_action(typing_chat, typing_thread)
                    .await;
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });

        let request_id = connection.next_id();
        connection
            .send(Request::Message {
                id: request_id,
                content: prompt.to_string(),
                images: Vec::new(),
                system_reminder: None,
                active_skill: None,
                no_reply: false,
            })
            .await?;

        let mut output = String::new();
        let mut status = String::new();
        let mut last_edit = Instant::now();
        let mut last_render = String::new();
        let result = loop {
            match connection.next_event().await? {
                ServerEvent::TextDelta { text } => {
                    output.push_str(&text);
                    maybe_edit_stream(
                        &self.bot,
                        message.chat.id,
                        progress_id,
                        &output,
                        &status,
                        &mut last_edit,
                        &mut last_render,
                        false,
                    )
                    .await;
                }
                ServerEvent::TextReplace { text } => {
                    output = text;
                    maybe_edit_stream(
                        &self.bot,
                        message.chat.id,
                        progress_id,
                        &output,
                        &status,
                        &mut last_edit,
                        &mut last_render,
                        true,
                    )
                    .await;
                }
                ServerEvent::ToolStart { name, .. } | ServerEvent::ToolExec { name, .. } => {
                    status = format!("🔧 Выполняю `{name}`…");
                    maybe_edit_stream(
                        &self.bot,
                        message.chat.id,
                        progress_id,
                        &output,
                        &status,
                        &mut last_edit,
                        &mut last_render,
                        true,
                    )
                    .await;
                }
                ServerEvent::ToolDone { error, .. } => {
                    status = error.map_or_else(
                        || "🔧 Инструмент завершён".to_string(),
                        |error| format!("⚠️ Инструмент: {error}"),
                    );
                    maybe_edit_stream(
                        &self.bot,
                        message.chat.id,
                        progress_id,
                        &output,
                        &status,
                        &mut last_edit,
                        &mut last_render,
                        true,
                    )
                    .await;
                }
                ServerEvent::SessionRenamed {
                    session_id: event_session_id,
                    display_title,
                    ..
                } if event_session_id == session_id => {
                    self.bind_session(
                        message.chat.id,
                        message.message_thread_id,
                        session_id.clone(),
                        Some(display_title),
                    )
                    .await?;
                }
                ServerEvent::Done { id } if id == request_id => break Ok(()),
                ServerEvent::Error { id, message, .. } if id == request_id => {
                    break Err(anyhow::anyhow!(message));
                }
                ServerEvent::Interrupted => break Err(anyhow::anyhow!("выполнение остановлено")),
                ServerEvent::ProviderGuardrail { message, .. } => {
                    break Err(anyhow::anyhow!(message));
                }
                _ => {}
            }
        };
        typing_task.abort();

        match result {
            Ok(()) => {
                let final_text = if output.trim().is_empty() {
                    if status.is_empty() {
                        "✅ Готово.".to_string()
                    } else {
                        status
                    }
                } else {
                    output
                };
                let chunks = split_text(&final_text, MAX_TELEGRAM_CHARS);
                if let Some(first) = chunks.first() {
                    let _ = self
                        .bot
                        .edit_message(message.chat.id, progress_id, first)
                        .await;
                    for chunk in chunks.iter().skip(1) {
                        self.bot
                            .send_message(message.chat.id, message.message_thread_id, chunk)
                            .await?;
                    }
                }
            }
            Err(error) => {
                let _ = self
                    .bot
                    .edit_message(message.chat.id, progress_id, &format!("❌ {error}"))
                    .await;
            }
        }
        Ok(())
    }
}

async fn maybe_edit_stream(
    bot: &TelegramApi,
    chat_id: i64,
    message_id: i64,
    output: &str,
    status: &str,
    last_edit: &mut Instant,
    last_render: &mut String,
    force: bool,
) {
    let rendered = render_stream(output, status);
    if rendered == *last_render {
        return;
    }
    if !force
        && last_edit.elapsed() < STREAM_EDIT_INTERVAL
        && rendered
            .chars()
            .count()
            .abs_diff(last_render.chars().count())
            < STREAM_EDIT_MIN_DELTA
    {
        return;
    }
    if bot
        .edit_message(chat_id, message_id, &rendered)
        .await
        .is_ok()
    {
        *last_edit = Instant::now();
        *last_render = rendered;
    }
}

fn render_stream(output: &str, status: &str) -> String {
    let body = if output.is_empty() {
        if status.is_empty() {
            "⏳ Работаю…"
        } else {
            status
        }
    } else {
        output
    };
    truncate_text(body, MAX_TELEGRAM_CHARS)
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars <= CONTINUATION_SUFFIX.chars().count() {
        return text.chars().take(max_chars).collect();
    }
    let prefix = text
        .chars()
        .take(max_chars - CONTINUATION_SUFFIX.chars().count())
        .collect::<String>();
    format!("{prefix}{CONTINUATION_SUFFIX}")
}

fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.chars().count() >= max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn validate_session_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_SESSION_ID_CHARS
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid session ID");
    }
    Ok(())
}

fn validate_model(value: &str) -> Result<()> {
    if value.is_empty()
        || value.chars().count() > MAX_MODEL_CHARS
        || value.chars().any(char::is_control)
    {
        bail!("invalid model name");
    }
    Ok(())
}

fn validate_title(value: &str) -> Result<()> {
    if value.is_empty()
        || value.chars().count() > MAX_TITLE_CHARS
        || value.chars().any(char::is_control)
    {
        bail!("invalid session title");
    }
    Ok(())
}

fn validate_prompt(value: &str) -> Result<()> {
    if value.is_empty() || value.chars().count() > MAX_TELEGRAM_CHARS {
        bail!("prompt exceeds Telegram bridge limit");
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum BridgeCommand {
    Help,
    New,
    Sessions,
    Resume(String),
    Model(String),
    Rename(String),
    Status,
    Cancel,
    Continue,
    Text(String),
}

fn parse_command(text: &str) -> BridgeCommand {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return BridgeCommand::Text(trimmed.to_string());
    }
    let mut words = trimmed.split_whitespace();
    let command = words.next().unwrap_or_default();
    let command = command.split('@').next().unwrap_or(command);
    match command {
        "/start" | "/help" => BridgeCommand::Help,
        "/new" => BridgeCommand::New,
        "/sessions" => BridgeCommand::Sessions,
        "/resume" => words.next().map_or(BridgeCommand::Help, |value| {
            BridgeCommand::Resume(value.to_string())
        }),
        "/model" => words.next().map_or(BridgeCommand::Help, |value| {
            BridgeCommand::Model(value.to_string())
        }),
        "/rename" => BridgeCommand::Rename(words.collect::<Vec<_>>().join(" ")),
        "/status" => BridgeCommand::Status,
        "/cancel" => BridgeCommand::Cancel,
        "/continue" => BridgeCommand::Continue,
        _ => BridgeCommand::Help,
    }
}

const HELP_TEXT: &str = "jcode Telegram full-session bridge\n\n/new — новая постоянная сессия\n/sessions — сессии этого чата\n/resume SESSION_ID — подключить существующую\n/model MODEL — сменить модель\n/rename TITLE — переименовать\n/status — состояние\n/cancel — остановить текущий turn\n/continue — продолжить работу\n/help — эта справка\n\nОбычный текст отправляется в активную сессию. Ambient уведомления и этот bridge — разные процессы.";

pub async fn run() -> Result<()> {
    let config = BridgeConfig::from_env()?;
    Bridge::new(config)?.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parser_handles_commands_and_bot_suffix() {
        assert_eq!(parse_command("/new"), BridgeCommand::New);
        assert_eq!(parse_command("/status@my_bot"), BridgeCommand::Status);
        assert_eq!(
            parse_command("/resume abc-123"),
            BridgeCommand::Resume("abc-123".to_string())
        );
        assert_eq!(
            parse_command("hello jcode"),
            BridgeCommand::Text("hello jcode".to_string())
        );
        assert_eq!(parse_command("/unknown"), BridgeCommand::Help);
    }

    #[test]
    fn split_text_preserves_unicode_and_boundaries() {
        let text = "я".repeat(9);
        let chunks = split_text(&text, 4);
        assert_eq!(chunks, vec!["яяяя", "яяяя", "я"]);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn truncate_text_stays_within_telegram_limit() {
        let text = "я".repeat(MAX_TELEGRAM_CHARS + 100);
        let truncated = truncate_text(&text, MAX_TELEGRAM_CHARS);
        assert_eq!(truncated.chars().count(), MAX_TELEGRAM_CHARS);
        assert!(truncated.ends_with(CONTINUATION_SUFFIX));
    }

    #[test]
    fn state_file_replaces_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "jcode_telegram_gateway_state_{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        save_state(&path, &BridgeState::default()).expect("write initial state");
        let mut updated = BridgeState::default();
        updated.offset = Some(42);
        save_state(&path, &updated).expect("replace state");

        assert_eq!(
            load_state(&path).expect("read replaced state").offset,
            Some(42)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validators_fail_closed() {
        assert!(validate_session_id("good-session_01").is_ok());
        assert!(validate_session_id("../secret").is_err());
        assert!(validate_session_id(&"x".repeat(MAX_SESSION_ID_CHARS + 1)).is_err());
        assert!(validate_model("provider/model").is_ok());
        assert!(validate_model("bad\nmodel").is_err());
        assert!(validate_title("A useful title").is_ok());
        assert!(validate_title("bad\nname").is_err());
        assert!(validate_prompt("короткий prompt").is_ok());
        assert!(validate_prompt(&"я".repeat(MAX_TELEGRAM_CHARS + 1)).is_err());
    }
}
