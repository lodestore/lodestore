use serde_json::{Value, json};

use super::channels::Channel;
use super::message::{Message, text};
use super::{DeliveryError, net};
use crate::state::AppState;

pub const TELEGRAM_API: &str = "https://api.telegram.org";
const TEXT_LIMIT: usize = 4096;

pub fn request(token: &str, chat_id: &str, body: &str) -> (String, Value) {
    request_at(TELEGRAM_API, token, chat_id, body)
}

pub fn request_at(base: &str, token: &str, chat_id: &str, body: &str) -> (String, Value) {
    let text: String = body.chars().take(TEXT_LIMIT).collect();
    (
        format!("{base}/bot{token}/sendMessage"),
        json!({ "chat_id": chat_id, "text": text }),
    )
}

pub fn parse_response(status: u16, body: &str) -> Result<(), DeliveryError> {
    match serde_json::from_str::<Value>(body) {
        Ok(reply) if reply["ok"] == true => Ok(()),
        Ok(reply) => {
            let description = reply["description"].as_str().unwrap_or("request rejected");
            Err(DeliveryError::new(format!(
                "Telegram: {}",
                net::clip(description)
            )))
        }
        Err(_) => Err(DeliveryError::new(format!(
            "HTTP {status}: {}",
            net::clip(body)
        ))),
    }
}

pub async fn send_to(
    allow_private: bool,
    base: &str,
    token: &str,
    chat_id: &str,
    body: &str,
) -> Result<(), DeliveryError> {
    let (url, payload) = request_at(base, token, chat_id, body);
    let payload = serde_json::to_vec(&payload).map_err(|e| DeliveryError::new(e.to_string()))?;
    let response = net::post_json(allow_private, &url, payload, None).await?;
    let status = response.status().as_u16();
    let reply = response.text().await.map_err(net::transport_error)?;
    parse_response(status, &reply)
}

pub async fn send(
    state: &AppState,
    channel: &Channel,
    message: &Message,
) -> Result<(), DeliveryError> {
    let token = channel
        .secrets
        .bot_token
        .as_ref()
        .ok_or_else(|| DeliveryError::new("The Telegram channel has no bot token."))?;
    let chat_id = channel
        .settings
        .chat_id
        .as_deref()
        .ok_or_else(|| DeliveryError::new("The Telegram channel has no chat ID."))?;
    send_to(
        state.config.notifications.allow_private_targets,
        TELEGRAM_API,
        token.expose(),
        chat_id,
        &text(message),
    )
    .await
}
