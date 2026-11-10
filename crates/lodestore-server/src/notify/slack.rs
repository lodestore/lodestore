use serde_json::json;

use super::channels::Channel;
use super::message::{Message, text};
use super::{DeliveryError, net};
use crate::state::AppState;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub async fn send(
    state: &AppState,
    channel: &Channel,
    message: &Message,
) -> Result<(), DeliveryError> {
    let url = channel
        .secrets
        .slack_url
        .as_ref()
        .ok_or_else(|| DeliveryError::new("The Slack channel has no webhook URL."))?;
    let body = serde_json::to_vec(&json!({ "text": escape(&text(message)) }))
        .map_err(|e| DeliveryError::new(e.to_string()))?;
    let response = net::post_json(&state.config.notifications, url.expose(), body, None).await?;
    net::expect_success(response).await
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn mentions_and_links_are_neutralised() {
        assert_eq!(
            escape("<!channel> <https://evil|Open> a & b"),
            "&lt;!channel&gt; &lt;https://evil|Open&gt; a &amp; b"
        );
    }
}
