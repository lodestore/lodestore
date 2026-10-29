use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::ToSchema;

use crate::alerts::model::AlertType;
use crate::config::{ChannelSpec, Secret, SmtpSecurity};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Telegram,
    Slack,
    Email,
    Webhook,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::Slack => "slack",
            ChannelKind::Email => "email",
            ChannelKind::Webhook => "webhook",
        }
    }

    pub fn parse(text: &str) -> Option<ChannelKind> {
        Some(match text {
            "telegram" => ChannelKind::Telegram,
            "slack" => ChannelKind::Slack,
            "email" => ChannelKind::Email,
            "webhook" => ChannelKind::Webhook,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSource {
    Ui,
    Config,
}

impl ChannelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelSource::Ui => "ui",
            ChannelSource::Config => "config",
        }
    }

    pub fn parse(text: &str) -> Option<ChannelSource> {
        match text {
            "ui" => Some(ChannelSource::Ui),
            "config" => Some(ChannelSource::Config),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SmtpMode {
    Starttls,
    Tls,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ChannelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SmtpMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelSecrets {
    #[serde(default)]
    pub bearer: Option<Secret>,
    #[serde(default)]
    pub bot_token: Option<Secret>,
    #[serde(default)]
    pub slack_url: Option<Secret>,
    #[serde(default)]
    pub smtp_password: Option<Secret>,
}

impl ChannelSecrets {
    pub fn any(&self) -> bool {
        self.bearer.is_some()
            || self.bot_token.is_some()
            || self.slack_url.is_some()
            || self.smtp_password.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub source: ChannelSource,
    pub enabled: bool,
    pub types: Vec<AlertType>,
    pub settings: ChannelSettings,
    pub secrets: ChannelSecrets,
}

pub fn split_spec(spec: &ChannelSpec) -> (ChannelKind, ChannelSettings, ChannelSecrets) {
    match spec {
        ChannelSpec::Telegram { bot_token, chat_id } => (
            ChannelKind::Telegram,
            ChannelSettings {
                chat_id: Some(chat_id.clone()),
                ..ChannelSettings::default()
            },
            ChannelSecrets {
                bot_token: Some(bot_token.clone()),
                ..ChannelSecrets::default()
            },
        ),
        ChannelSpec::Webhook { url, secret } => (
            ChannelKind::Webhook,
            ChannelSettings {
                url: Some(url.clone()),
                ..ChannelSettings::default()
            },
            ChannelSecrets {
                bearer: secret.clone(),
                ..ChannelSecrets::default()
            },
        ),
        ChannelSpec::Slack { webhook_url } => (
            ChannelKind::Slack,
            ChannelSettings::default(),
            ChannelSecrets {
                slack_url: Some(webhook_url.clone()),
                ..ChannelSecrets::default()
            },
        ),
        ChannelSpec::Email {
            smtp_host,
            port,
            security,
            username,
            password,
            from,
            to,
        } => (
            ChannelKind::Email,
            ChannelSettings {
                smtp_host: Some(smtp_host.clone()),
                port: Some(*port),
                security: Some(match security {
                    SmtpSecurity::Starttls => SmtpMode::Starttls,
                    SmtpSecurity::Tls => SmtpMode::Tls,
                }),
                username: username.clone(),
                from: Some(from.clone()),
                to: to.clone(),
                ..ChannelSettings::default()
            },
            ChannelSecrets {
                smtp_password: password.clone(),
                ..ChannelSecrets::default()
            },
        ),
    }
}

macro_rules! select {
    ($($tail:literal)?) => {
        concat!(
            "SELECT id, name, kind, source, enabled, types_json, config_json, secret_id \
             FROM notification_channels"
            $(, " ", $tail)?
        )
    };
}

pub async fn load_channel(state: &AppState, id: &str) -> anyhow::Result<Option<Channel>> {
    let row = sqlx::query(select!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await
        .context("reading a notification channel")?;
    match row {
        Some(row) => Ok(Some(from_row(state, &row).await?)),
        None => Ok(None),
    }
}

pub async fn load_enabled(state: &AppState) -> anyhow::Result<Vec<Channel>> {
    let rows = sqlx::query(select!("WHERE enabled = 1 ORDER BY name"))
        .fetch_all(&state.db.pool)
        .await
        .context("reading notification channels")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(from_row(state, row).await?);
    }
    Ok(out)
}

async fn from_row(state: &AppState, row: &AnyRow) -> anyhow::Result<Channel> {
    let name: String = row.try_get("name")?;
    let kind: String = row.try_get("kind")?;
    let source: String = row.try_get("source")?;
    let enabled: i64 = row.try_get("enabled")?;
    let types_json: String = row.try_get("types_json")?;
    let config_json: String = row.try_get("config_json")?;
    let secret_id: Option<String> = row.try_get("secret_id")?;
    let kind = ChannelKind::parse(&kind)
        .with_context(|| format!("channel {name} has an unknown kind {kind:?}"))?;
    let source = ChannelSource::parse(&source)
        .with_context(|| format!("channel {name} has an unknown source {source:?}"))?;
    let secrets = match (source, secret_id) {
        (ChannelSource::Config, _) => state
            .config
            .notifications
            .channels
            .iter()
            .find(|c| c.name == name)
            .map(|c| split_spec(&c.kind).2)
            .with_context(|| format!("channel {name} is no longer in the server config"))?,
        (ChannelSource::Ui, Some(secret_id)) => {
            let bytes = state.sealer.get_secret(&secret_id).await?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("decoding the secrets of channel {name}"))?
        }
        (ChannelSource::Ui, None) => ChannelSecrets::default(),
    };
    Ok(Channel {
        id: row.try_get("id")?,
        kind,
        source,
        enabled: enabled != 0,
        types: serde_json::from_str(&types_json)
            .with_context(|| format!("decoding the alert types of channel {name}"))?,
        settings: serde_json::from_str(&config_json)
            .with_context(|| format!("decoding the settings of channel {name}"))?,
        secrets,
        name,
    })
}
