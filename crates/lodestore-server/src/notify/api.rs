//! Notification channels and alert thresholds, managed by the administrator's
//! session. Channel secrets are sealed and never returned.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use sqlx::any::AnyRow;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::channels::{
    Channel, ChannelKind, ChannelSecrets, ChannelSettings, ChannelSource, SmtpMode, load_channel,
    split_spec,
};
use super::send_test;
use crate::alerts::model::{
    AlertType, CERT_DAYS_KEY, CERT_DAYS_MAX, OFFLINE_AFTER_CEILING, OFFLINE_AFTER_FLOOR,
    OFFLINE_AFTER_KEY, load_thresholds,
};
use crate::auth::AuthUser;
use crate::auth::account::admin_email;
use crate::db::now_ms;
use crate::error::{ApiError, ErrorBody};
use crate::maintenance::gc::api::FieldError;
use crate::state::AppState;
use lodestore_policy::{format_duration, parse_duration};

const SECRET_KIND: &str = "notification-channel";
const CONFIG_READ_ONLY: &str =
    "This channel is defined in the server configuration. Change it there and restart the server.";
const ALL_TYPES: [AlertType; 6] = [
    AlertType::BackupFailed,
    AlertType::NoRecentBackup,
    AlertType::AgentOffline,
    AlertType::CheckFailed,
    AlertType::CertificateExpiring,
    AlertType::AgentUpdateFailed,
];

fn all_types() -> Vec<AlertType> {
    ALL_TYPES.to_vec()
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ChannelSettingsDto {
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub port: Option<i64>,
    #[serde(default)]
    pub security: Option<SmtpMode>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub recipients: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LastDelivery {
    pub ok: bool,
    pub at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChannelDto {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ChannelKind,
    pub enabled: bool,
    pub alert_types: Vec<AlertType>,
    pub source: ChannelSource,
    pub settings: ChannelSettingsDto,
    /// Whether a secret is stored; the secret itself is never returned.
    pub secret_set: bool,
    pub last_delivery: Option<LastDelivery>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ChannelInput {
    #[serde(rename = "type")]
    pub kind: ChannelKind,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Left out means all five alert types.
    #[serde(default = "all_types")]
    pub alert_types: Vec<AlertType>,
    #[serde(default)]
    pub settings: ChannelSettingsDto,
    /// The bot token (Telegram), webhook URL (Slack), bearer secret (webhook) or SMTP
    /// password (email). Left out or empty keeps the stored secret.
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ChannelTestInput {
    /// A saved channel; with no `channel` it is tested as saved.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// A form draft, tested without saving. With `channel_id`, an empty secret falls
    /// back to the one saved for that channel.
    #[serde(default)]
    pub channel: Option<ChannelInput>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChannelTestResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AlertThresholds {
    /// How long a host stays silent before it counts as offline, such as `15m`.
    pub offline_after: String,
    /// How many days before a certificate expires the alert fires.
    pub cert_days: u32,
    pub offline_floor: String,
    pub cert_days_max: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AlertThresholdsInput {
    /// A duration such as `15m` or `1h`, not below the floor. Left out keeps the saved one.
    #[serde(default)]
    pub offline_after: Option<String>,
    /// A whole number of days from 1 to 60. Left out keeps the saved one.
    #[serde(default)]
    pub cert_days: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct FieldErrors {
    pub errors: Vec<FieldError>,
}

pub enum ChannelFailure {
    Api(ApiError),
    Fields(Vec<FieldError>),
}

impl<E: Into<ApiError>> From<E> for ChannelFailure {
    fn from(error: E) -> Self {
        ChannelFailure::Api(error.into())
    }
}

impl IntoResponse for ChannelFailure {
    fn into_response(self) -> Response {
        match self {
            ChannelFailure::Api(error) => error.into_response(),
            ChannelFailure::Fields(errors) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(FieldErrors { errors }),
            )
                .into_response(),
        }
    }
}

fn field(path: &str, message: &str) -> FieldError {
    FieldError {
        path: path.to_string(),
        message: message.to_string(),
    }
}

fn secret_key(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Webhook => "bearer",
        ChannelKind::Telegram => "bot_token",
        ChannelKind::Slack => "slack_url",
        ChannelKind::Email => "smtp_password",
    }
}

fn secrets_json(kind: ChannelKind, secret: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ secret_key(kind): secret })).unwrap_or_default()
}

fn secrets_of(kind: ChannelKind, secret: &str) -> ChannelSecrets {
    serde_json::from_value(json!({ secret_key(kind): secret })).unwrap_or_default()
}

struct Prepared {
    name: String,
    kind: ChannelKind,
    enabled: bool,
    types: Vec<AlertType>,
    settings: ChannelSettings,
    secret: Option<String>,
}

fn present(text: Option<&String>) -> Option<String> {
    text.map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn prepare(
    input: ChannelInput,
    has_secret: bool,
    recipient_needed: bool,
    admin_has_email: bool,
) -> Result<Prepared, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = input.name.trim().to_string();
    if name.is_empty() {
        errors.push(field("name", "Enter a name for the channel."));
    }
    let mut types = Vec::new();
    for kind in input.alert_types {
        if !types.contains(&kind) {
            types.push(kind);
        }
    }
    if types.is_empty() {
        errors.push(field("alert_types", "Choose at least one alert type."));
    }
    let secret = present(input.secret.as_ref());
    let given = input.settings;
    let mut settings = ChannelSettings::default();
    match input.kind {
        ChannelKind::Telegram => {
            if secret.is_none() && !has_secret {
                errors.push(field("secret", "Enter a bot token."));
            }
            settings.chat_id = present(given.chat_id.as_ref());
            if settings.chat_id.is_none() {
                errors.push(field("settings.chat_id", "Enter a chat ID."));
            }
        }
        ChannelKind::Slack => {
            let bad = match &secret {
                Some(url) => !url.starts_with("https://"),
                None => !has_secret,
            };
            if bad {
                errors.push(field(
                    "settings.url",
                    "Enter a webhook URL starting with https://.",
                ));
            }
        }
        ChannelKind::Webhook => {
            settings.url = present(given.url.as_ref());
            if !settings
                .url
                .as_deref()
                .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"))
            {
                errors.push(field(
                    "settings.url",
                    "Enter a URL starting with http:// or https://.",
                ));
            }
        }
        ChannelKind::Email => {
            settings.smtp_host = present(given.smtp_host.as_ref());
            if settings.smtp_host.is_none() {
                errors.push(field("settings.smtp_host", "Enter the SMTP host."));
            }
            let security = given.security.unwrap_or(SmtpMode::Starttls);
            settings.security = Some(security);
            let port = given.port.unwrap_or(match security {
                SmtpMode::Starttls => 587,
                SmtpMode::Tls => 465,
            });
            match u16::try_from(port) {
                Ok(port) if port != 0 => settings.port = Some(port),
                _ => errors.push(field("settings.port", "Enter a port from 1 to 65535.")),
            }
            settings.username = present(given.username.as_ref());
            settings.from = present(given.from.as_ref());
            if !settings
                .from
                .as_deref()
                .is_some_and(|f| f.parse::<lettre::message::Mailbox>().is_ok())
            {
                errors.push(field(
                    "settings.from",
                    "Enter a sender address such as lodestore@example.com.",
                ));
            }
            settings.to = given
                .recipients
                .iter()
                .map(|r| r.trim())
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect();
            if settings
                .to
                .iter()
                .any(|r| r.parse::<lettre::Address>().is_err())
            {
                errors.push(field(
                    "settings.recipients",
                    "Enter addresses such as ops@example.com.",
                ));
            } else if recipient_needed && settings.to.is_empty() && !admin_has_email {
                errors.push(field("settings.recipients", super::email::NO_RECIPIENT));
            }
        }
    }
    if errors.is_empty() {
        Ok(Prepared {
            name,
            kind: input.kind,
            enabled: input.enabled,
            types,
            settings,
            secret,
        })
    } else {
        Err(errors)
    }
}

fn same_origin(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
            (Ok(a), Ok(b)) => a.origin() == b.origin(),
            _ => a == b,
        },
        (a, b) => a == b,
    }
}

fn same_target(a: &ChannelSettings, b: &ChannelSettings) -> bool {
    same_origin(a.url.as_deref(), b.url.as_deref())
        && a.smtp_host == b.smtp_host
        && a.port == b.port
        && a.security == b.security
        && a.username == b.username
}

fn secret_again() -> FieldError {
    field(
        "secret",
        "Enter the secret again to use it with a different address.",
    )
}

fn settings_dto(settings: &ChannelSettings) -> ChannelSettingsDto {
    ChannelSettingsDto {
        chat_id: settings.chat_id.clone(),
        url: settings.url.clone(),
        smtp_host: settings.smtp_host.clone(),
        port: settings.port.map(i64::from),
        security: settings.security,
        username: settings.username.clone(),
        from: settings.from.clone(),
        recipients: settings.to.clone(),
    }
}

macro_rules! select {
    ($($tail:literal)?) => {
        concat!(
            "SELECT id, name, kind, source, enabled, types_json, config_json, secret_id, \
             last_ok, last_at, last_error FROM notification_channels"
            $(, " ", $tail)?
        )
    };
}

fn dto_of(state: &AppState, row: &AnyRow) -> anyhow::Result<ChannelDto> {
    use anyhow::Context;
    let name: String = row.try_get("name")?;
    let kind: String = row.try_get("kind")?;
    let source: String = row.try_get("source")?;
    let kind = ChannelKind::parse(&kind).with_context(|| format!("unknown kind {kind:?}"))?;
    let source =
        ChannelSource::parse(&source).with_context(|| format!("unknown source {source:?}"))?;
    let secret_id: Option<String> = row.try_get("secret_id")?;
    let secret_set = match source {
        ChannelSource::Ui => secret_id.is_some(),
        ChannelSource::Config => state
            .config
            .notifications
            .channels
            .iter()
            .find(|c| c.name == name)
            .is_some_and(|c| split_spec(&c.kind).2.any()),
    };
    let last_ok: Option<i64> = row.try_get("last_ok")?;
    let last_at: Option<i64> = row.try_get("last_at")?;
    let enabled: i64 = row.try_get("enabled")?;
    let types: String = row.try_get("types_json")?;
    let settings: String = row.try_get("config_json")?;
    Ok(ChannelDto {
        id: row.try_get("id")?,
        name,
        kind,
        enabled: enabled != 0,
        alert_types: serde_json::from_str(&types).context("decoding the alert types")?,
        source,
        settings: settings_dto(&serde_json::from_str(&settings).context("decoding settings")?),
        secret_set,
        last_delivery: match (last_ok, last_at) {
            (Some(ok), Some(at)) => Some(LastDelivery {
                ok: ok != 0,
                at,
                error: row.try_get("last_error")?,
            }),
            _ => None,
        },
    })
}

async fn channel_dto(state: &AppState, id: &str) -> Result<ChannelDto, ApiError> {
    let row = sqlx::query(select!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("Channel not found.".into()))?;
    Ok(dto_of(state, &row)?)
}

async fn name_taken(state: &AppState, name: &str, except: Option<&str>) -> Result<bool, ApiError> {
    let row = sqlx::query("SELECT id FROM notification_channels WHERE name = $1")
        .bind(name)
        .fetch_optional(&state.db.pool)
        .await?;
    Ok(match row {
        Some(row) => Some(row.try_get::<String, _>("id")?.as_str()) != except,
        None => false,
    })
}

fn name_taken_error() -> ChannelFailure {
    ChannelFailure::Fields(vec![field(
        "name",
        "A channel with this name already exists.",
    )])
}

#[utoipa::path(
    get,
    tag = "Notifications",
    operation_id = "list_channels",
    summary = "List the notification channels",
    description = "Channels created here and channels defined in the server configuration. \
        Secrets are never returned; `secret_set` says whether one is stored.",
    security(("session" = [])),
    path = "/api/v1/notifications/channels",
    responses(
        (status = 200, description = "The channels by name", body = Vec<ChannelDto>),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn list_channels(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<Vec<ChannelDto>>, ApiError> {
    let rows = sqlx::query(select!("ORDER BY name"))
        .fetch_all(&state.db.pool)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(dto_of(&state, row)?);
    }
    Ok(Json(out))
}

#[utoipa::path(
    post,
    tag = "Notifications",
    operation_id = "create_channel",
    summary = "Add a notification channel",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/notifications/channels",
    request_body = ChannelInput,
    responses(
        (status = 201, description = "Created", body = ChannelDto),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 422, description = "A field is not valid", body = FieldErrors),
    )
)]
async fn create_channel(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(input): Json<ChannelInput>,
) -> Result<(StatusCode, Json<ChannelDto>), ChannelFailure> {
    let admin = admin_email(&state).await?.is_some();
    let prepared = prepare(input, false, true, admin).map_err(ChannelFailure::Fields)?;
    if name_taken(&state, &prepared.name, None).await? {
        return Err(name_taken_error());
    }
    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = state.db.pool.begin().await?;
    let secret_id = match &prepared.secret {
        Some(secret) => Some(
            state
                .sealer
                .put_secret_on(&mut tx, SECRET_KIND, &secrets_json(prepared.kind, secret))
                .await?,
        ),
        None => None,
    };
    let now = now_ms();
    sqlx::query(
        "INSERT INTO notification_channels (id, name, kind, source, enabled, types_json, \
         config_json, secret_id, created_at, updated_at) \
         VALUES ($1, $2, $3, 'ui', $4, $5, $6, $7, $8, $8)",
    )
    .bind(&id)
    .bind(&prepared.name)
    .bind(prepared.kind.as_str())
    .bind(i64::from(prepared.enabled))
    .bind(serde_json::to_string(&prepared.types).map_err(anyhow::Error::from)?)
    .bind(serde_json::to_string(&prepared.settings).map_err(anyhow::Error::from)?)
    .bind(secret_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(channel_dto(&state, &id).await?)))
}

#[utoipa::path(
    get,
    tag = "Notifications",
    operation_id = "get_channel",
    summary = "Get a notification channel",
    security(("session" = [])),
    path = "/api/v1/notifications/channels/{id}",
    params(("id" = String, Path, description = "Channel id")),
    responses(
        (status = 200, description = "The channel", body = ChannelDto),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 404, description = "No such channel", body = ErrorBody),
    )
)]
async fn get_channel(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<ChannelDto>, ApiError> {
    Ok(Json(channel_dto(&state, &id).await?))
}

async fn record_test(state: &AppState, id: &str, error: Option<&str>) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE notification_channels SET last_ok = $1, last_at = $2, last_error = $3 \
         WHERE id = $4",
    )
    .bind(i64::from(error.is_none()))
    .bind(now_ms())
    .bind(error)
    .bind(id)
    .execute(&state.db.pool)
    .await?;
    Ok(())
}

#[utoipa::path(
    post,
    tag = "Notifications",
    operation_id = "test_channel",
    summary = "Send a test message",
    description = "Tests a saved channel (`channel_id`) or a form draft (`channel`) without \
        saving it. The stored secret is reused only while the address stays the same. A failed \
        delivery answers 200 with `ok: false` and the error.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/notifications/channels/test",
    request_body = ChannelTestInput,
    responses(
        (status = 200, description = "The outcome of the delivery", body = ChannelTestResult),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 404, description = "No such channel", body = ErrorBody),
        (status = 422, description = "A field of the draft is not valid", body = FieldErrors),
    )
)]
async fn test_channel(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(input): Json<ChannelTestInput>,
) -> Result<Json<ChannelTestResult>, ChannelFailure> {
    let saved = match &input.channel_id {
        Some(id) => Some(
            load_channel(&state, id)
                .await?
                .ok_or_else(|| ApiError::NotFound("Channel not found.".into()))?,
        ),
        None => None,
    };
    let (channel, record) = match input.channel {
        None => {
            let channel = saved.ok_or_else(|| {
                ApiError::Validation("Choose a saved channel or send a draft.".into())
            })?;
            let id = channel.id.clone();
            (channel, Some(id))
        }
        Some(draft) => {
            let kept = saved.filter(|s| s.kind == draft.kind && s.source == ChannelSource::Ui);
            let has_secret = kept.as_ref().is_some_and(|s| s.secrets.any());
            let prepared =
                prepare(draft, has_secret, false, false).map_err(ChannelFailure::Fields)?;
            if prepared.secret.is_none()
                && let Some(kept) = &kept
                && kept.secrets.any()
                && !same_target(&kept.settings, &prepared.settings)
            {
                return Err(ChannelFailure::Fields(vec![secret_again()]));
            }
            let secrets = match &prepared.secret {
                Some(secret) => secrets_of(prepared.kind, secret),
                None => kept.map(|s| s.secrets).unwrap_or_default(),
            };
            let channel = Channel {
                id: String::new(),
                name: prepared.name,
                kind: prepared.kind,
                source: ChannelSource::Ui,
                enabled: prepared.enabled,
                types: prepared.types,
                settings: prepared.settings,
                secrets,
            };
            (channel, None)
        }
    };
    let result = send_test(&state, &channel).await;
    if let Some(id) = record {
        let error = result.as_ref().err().map(ToString::to_string);
        record_test(&state, &id, error.as_deref()).await?;
    }
    Ok(Json(match result {
        Ok(()) => ChannelTestResult {
            ok: true,
            message: "Test message sent.".into(),
        },
        Err(error) => ChannelTestResult {
            ok: false,
            message: error.to_string(),
        },
    }))
}

struct Stored {
    kind: ChannelKind,
    source: ChannelSource,
    secret_id: Option<String>,
    settings: ChannelSettings,
}

async fn stored(state: &AppState, id: &str) -> Result<Stored, ApiError> {
    let row = sqlx::query(
        "SELECT kind, source, secret_id, config_json FROM notification_channels WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("Channel not found.".into()))?;
    let kind: String = row.try_get("kind")?;
    let source: String = row.try_get("source")?;
    let settings: String = row.try_get("config_json")?;
    Ok(Stored {
        settings: serde_json::from_str(&settings).map_err(anyhow::Error::from)?,
        kind: ChannelKind::parse(&kind)
            .ok_or_else(|| anyhow::anyhow!("unknown channel kind {kind:?}"))?,
        source: ChannelSource::parse(&source)
            .ok_or_else(|| anyhow::anyhow!("unknown channel source {source:?}"))?,
        secret_id: row.try_get("secret_id")?,
    })
}

#[utoipa::path(
    put,
    tag = "Notifications",
    operation_id = "update_channel",
    summary = "Change a notification channel",
    description = "The type cannot change. Without a `secret` the stored one stays while the \
        address stays the same. Channels from the server configuration answer 409.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/notifications/channels/{id}",
    params(("id" = String, Path, description = "Channel id")),
    request_body = ChannelInput,
    responses(
        (status = 200, description = "Saved", body = ChannelDto),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 404, description = "No such channel", body = ErrorBody),
        (status = 409, description = "The channel is defined in the server configuration", body = ErrorBody),
        (status = 422, description = "A field is not valid", body = FieldErrors),
    )
)]
async fn update_channel(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
    Json(input): Json<ChannelInput>,
) -> Result<Json<ChannelDto>, ChannelFailure> {
    let current = stored(&state, &id).await?;
    if current.source == ChannelSource::Config {
        return Err(ApiError::Conflict(CONFIG_READ_ONLY.into()).into());
    }
    if input.kind != current.kind {
        return Err(ChannelFailure::Fields(vec![field(
            "type",
            "The type of a channel can't be changed. Add a new channel instead.",
        )]));
    }
    let admin = admin_email(&state).await?.is_some();
    let prepared =
        prepare(input, current.secret_id.is_some(), true, admin).map_err(ChannelFailure::Fields)?;
    if prepared.secret.is_none()
        && current.secret_id.is_some()
        && !same_target(&current.settings, &prepared.settings)
    {
        return Err(ChannelFailure::Fields(vec![secret_again()]));
    }
    if name_taken(&state, &prepared.name, Some(&id)).await? {
        return Err(name_taken_error());
    }
    let mut tx = state.db.pool.begin().await?;
    let secret_id = match &prepared.secret {
        Some(secret) => Some(
            state
                .sealer
                .put_secret_on(&mut tx, SECRET_KIND, &secrets_json(prepared.kind, secret))
                .await?,
        ),
        None => current.secret_id.clone(),
    };
    sqlx::query(
        "UPDATE notification_channels SET name = $1, enabled = $2, types_json = $3, \
         config_json = $4, secret_id = $5, updated_at = $6 WHERE id = $7",
    )
    .bind(&prepared.name)
    .bind(i64::from(prepared.enabled))
    .bind(serde_json::to_string(&prepared.types).map_err(anyhow::Error::from)?)
    .bind(serde_json::to_string(&prepared.settings).map_err(anyhow::Error::from)?)
    .bind(&secret_id)
    .bind(now_ms())
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    if prepared.secret.is_some()
        && let Some(old) = &current.secret_id
    {
        sqlx::query("DELETE FROM secrets WHERE id = $1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(channel_dto(&state, &id).await?))
}

#[utoipa::path(
    delete,
    tag = "Notifications",
    operation_id = "delete_channel",
    summary = "Delete a notification channel",
    description = "Its pending and past deliveries go with it. Channels from the server \
        configuration answer 409.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/notifications/channels/{id}",
    params(("id" = String, Path, description = "Channel id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 404, description = "No such channel", body = ErrorBody),
        (status = 409, description = "The channel is defined in the server configuration", body = ErrorBody),
    )
)]
async fn delete_channel(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let current = stored(&state, &id).await?;
    if current.source == ChannelSource::Config {
        return Err(ApiError::Conflict(CONFIG_READ_ONLY.into()));
    }
    let mut tx = state.db.pool.begin().await?;
    sqlx::query("DELETE FROM alert_deliveries WHERE channel_id = $1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM notification_channels WHERE id = $1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    if let Some(secret_id) = &current.secret_id {
        sqlx::query("DELETE FROM secrets WHERE id = $1")
            .bind(secret_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn thresholds_dto(offline_after: std::time::Duration, cert_days: u32) -> AlertThresholds {
    AlertThresholds {
        offline_after: format_duration(offline_after),
        cert_days,
        offline_floor: format_duration(OFFLINE_AFTER_FLOOR),
        cert_days_max: CERT_DAYS_MAX,
    }
}

#[utoipa::path(
    get,
    tag = "Notifications",
    operation_id = "get_alert_thresholds",
    summary = "Get the alert thresholds",
    security(("session" = [])),
    path = "/api/v1/notifications/thresholds",
    responses(
        (status = 200, description = "The thresholds in force", body = AlertThresholds),
        (status = 401, description = "Not signed in", body = ErrorBody),
    )
)]
async fn get_alert_thresholds(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<AlertThresholds>, ApiError> {
    let t = load_thresholds(&state).await?;
    Ok(Json(thresholds_dto(t.offline_after, t.cert_days)))
}

#[utoipa::path(
    put,
    tag = "Notifications",
    operation_id = "put_alert_thresholds",
    summary = "Change the alert thresholds",
    description = "Takes effect from the next alert evaluation.",
    security(("session" = [], "csrf" = [])),
    path = "/api/v1/notifications/thresholds",
    request_body = AlertThresholdsInput,
    responses(
        (status = 200, description = "Saved", body = AlertThresholds),
        (status = 401, description = "Not signed in", body = ErrorBody),
        (status = 422, description = "A value is not valid", body = FieldErrors),
    )
)]
async fn put_alert_thresholds(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(input): Json<AlertThresholdsInput>,
) -> Result<Json<AlertThresholds>, ChannelFailure> {
    let mut errors = Vec::new();
    let mut offline_after = None;
    if let Some(text) = &input.offline_after {
        match parse_duration(text.trim()) {
            Some(d) if d > OFFLINE_AFTER_CEILING => errors.push(field(
                "offline_after",
                &format!(
                    "Use a duration of {} or less.",
                    format_duration(OFFLINE_AFTER_CEILING)
                ),
            )),
            Some(d) if d >= OFFLINE_AFTER_FLOOR => offline_after = Some(d),
            _ => errors.push(field(
                "offline_after",
                &format!(
                    "Use a duration of {} or more, such as 15m or 1h.",
                    format_duration(OFFLINE_AFTER_FLOOR)
                ),
            )),
        }
    }
    let mut cert_days = None;
    if let Some(days) = input.cert_days {
        match u32::try_from(days)
            .ok()
            .filter(|d| (1..=CERT_DAYS_MAX).contains(d))
        {
            Some(d) => cert_days = Some(d),
            None => errors.push(field(
                "cert_days",
                &format!("Enter a whole number of days from 1 to {CERT_DAYS_MAX}."),
            )),
        }
    }
    if !errors.is_empty() {
        return Err(ChannelFailure::Fields(errors));
    }
    let mut tx = state.db.pool.begin().await?;
    for (key, value) in [
        (
            OFFLINE_AFTER_KEY,
            offline_after.map(|d| d.as_secs().to_string()),
        ),
        (CERT_DAYS_KEY, cert_days.map(|d| d.to_string())),
    ] {
        if let Some(value) = value {
            sqlx::query(
                "INSERT INTO settings (key, value) VALUES ($1, $2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    let t = load_thresholds(&state).await?;
    Ok(Json(thresholds_dto(t.offline_after, t.cert_days)))
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_channels, create_channel))
        .routes(routes!(get_channel, update_channel, delete_channel))
        .routes(routes!(test_channel))
        .routes(routes!(get_alert_thresholds, put_alert_thresholds))
}
