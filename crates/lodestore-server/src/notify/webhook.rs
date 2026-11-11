use lodestore_policy::jiff::Timestamp;
use serde_json::{Value, json};

use super::channels::Channel;
use super::message::{Message, MessageKind};
use super::{DeliveryError, net};
use crate::state::AppState;

const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

fn time(ms: i64) -> String {
    Timestamp::from_second(ms.div_euclid(1000))
        .map(|t| t.to_string())
        .unwrap_or_else(|_| ZERO_TIME.to_string())
}

pub fn payload(receiver: &str, external_url: &str, m: &Message) -> Value {
    let status = if m.kind == MessageKind::Resolved {
        "resolved"
    } else {
        "firing"
    };
    let alertname = m.labels.get("alertname").cloned().unwrap_or_default();
    let annotations = json!({ "summary": m.title, "description": m.detail });
    json!({
        "version": "4",
        "groupKey": format!("lodestore:{}", m.fingerprint),
        "truncatedAlerts": 0,
        "status": status,
        "receiver": receiver,
        "groupLabels": { "alertname": alertname },
        "commonLabels": m.labels,
        "commonAnnotations": annotations,
        "externalURL": external_url.trim_end_matches('/'),
        "alerts": [{
            "status": status,
            "labels": m.labels,
            "annotations": annotations,
            "startsAt": time(m.started_at),
            "endsAt": m.ended_at.map_or_else(|| ZERO_TIME.to_string(), time),
            "generatorURL": m.link,
            "fingerprint": m.fingerprint,
        }],
    })
}

pub async fn send(
    state: &AppState,
    channel: &Channel,
    message: &Message,
) -> Result<(), DeliveryError> {
    let url = channel
        .settings
        .url
        .as_deref()
        .ok_or_else(|| DeliveryError::new("The webhook has no URL."))?;
    let body = serde_json::to_vec(&payload(&channel.name, &state.config.public_url, message))
        .map_err(|e| DeliveryError::new(e.to_string()))?;
    let bearer = channel.secrets.bearer.as_ref().map(|s| s.expose());
    let response = net::post_json(&state.config.notifications, url, body, bearer).await?;
    net::expect_success(response).await
}
