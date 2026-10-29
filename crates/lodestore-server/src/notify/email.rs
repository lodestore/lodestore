use std::time::Duration;

use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use super::channels::{Channel, ChannelSecrets, ChannelSettings, SmtpMode};
use super::message::{Message, MessageKind, text};
use super::{DeliveryError, net};
use crate::auth::account::admin_email;
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(15);

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html(m: &Message) -> String {
    let head = match m.kind {
        MessageKind::Test => format!("{}. {}", m.title, m.detail),
        kind => format!("{}: {}", kind.label(), m.title),
    };
    let detail = match m.kind {
        MessageKind::Test => String::new(),
        _ => format!("<p>{}</p>", escape(&m.detail)),
    };
    format!(
        "<p>{}</p>{detail}<p><a href=\"{}\">Open in Lodestore</a></p>",
        escape(&head),
        escape(&m.link)
    )
}

fn mailbox(address: &str) -> Result<Mailbox, DeliveryError> {
    address
        .parse()
        .map_err(|_| DeliveryError::new(format!("The address {address} is not valid.")))
}

pub const NO_RECIPIENT: &str = "Enter a recipient or set an admin email in Settings, User.";

fn subject(message: &Message) -> String {
    match message.kind {
        MessageKind::Test => "[Lodestore] Test message".to_string(),
        kind => format!("[Lodestore] {}: {}", kind.label(), message.title),
    }
}

pub fn build(channel: &Channel, message: &Message) -> Result<lettre::Message, DeliveryError> {
    build_for(channel, message, None)
}

pub fn build_for(
    channel: &Channel,
    message: &Message,
    admin_email: Option<&str>,
) -> Result<lettre::Message, DeliveryError> {
    let settings = &channel.settings;
    let recipients: Vec<&str> = if settings.to.is_empty() {
        admin_email.into_iter().collect()
    } else {
        settings.to.iter().map(String::as_str).collect()
    };
    if recipients.is_empty() {
        return Err(DeliveryError::new(NO_RECIPIENT));
    }
    let from = settings
        .from
        .as_deref()
        .ok_or_else(|| DeliveryError::new("The email channel has no sender address."))?;
    let mut builder = lettre::Message::builder()
        .from(mailbox(from)?)
        .subject(subject(message));
    for recipient in recipients {
        builder = builder.to(mailbox(recipient)?);
    }
    builder
        .multipart(MultiPart::alternative_plain_html(
            text(message),
            html(message),
        ))
        .map_err(|e| DeliveryError::new(format!("The message cannot be built: {e}")))
}

fn smtp_error(error: &lettre::transport::smtp::Error, secrets: &ChannelSecrets) -> DeliveryError {
    let mut text = format!("SMTP: {}", net::error_chain(error));
    let password = secrets.smtp_password.as_ref().map(|p| p.expose());
    if let Some(password) = password.filter(|p| !p.is_empty()) {
        text = text.replace(password, "***");
    }
    DeliveryError::new(text)
}

pub fn transport(
    settings: &ChannelSettings,
    secrets: &ChannelSecrets,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, DeliveryError> {
    let host = settings
        .smtp_host
        .as_deref()
        .ok_or_else(|| DeliveryError::new("The email channel has no SMTP server."))?;
    let relay = match settings.security.unwrap_or(SmtpMode::Starttls) {
        SmtpMode::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        SmtpMode::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
    };
    let mut builder = relay
        .map_err(|e| smtp_error(&e, secrets))?
        .timeout(Some(TIMEOUT));
    if let Some(port) = settings.port {
        builder = builder.port(port);
    }
    if let Some(username) = &settings.username {
        let password = secrets.smtp_password.as_ref().map_or("", |p| p.expose());
        builder = builder.credentials(Credentials::new(username.clone(), password.to_string()));
    }
    Ok(builder.build())
}

pub async fn deliver(
    transport: &AsyncSmtpTransport<Tokio1Executor>,
    message: lettre::Message,
    secrets: &ChannelSecrets,
) -> Result<(), DeliveryError> {
    transport
        .send(message)
        .await
        .map(drop)
        .map_err(|e| smtp_error(&e, secrets))
}

pub async fn send(
    state: &AppState,
    channel: &Channel,
    message: &Message,
) -> Result<(), DeliveryError> {
    let admin = if channel.settings.to.is_empty() {
        admin_email(state)
            .await
            .map_err(|e| DeliveryError::new(format!("{e:#}")))?
    } else {
        None
    };
    let email = build_for(channel, message, admin.as_deref())?;
    let transport = transport(&channel.settings, &channel.secrets)?;
    let host = channel.settings.smtp_host.as_deref().unwrap_or_default();
    let port = channel
        .settings
        .port
        .unwrap_or(match channel.settings.security {
            Some(SmtpMode::Tls) => 465,
            _ => 587,
        });
    net::check_host(host, port, state.config.notifications.allow_private_targets).await?;
    deliver(&transport, email, &channel.secrets).await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;
    use crate::alerts::model::AlertType;
    use crate::notify::channels::{ChannelKind, ChannelSource};
    use crate::notify::message::{AlertRow, for_event};

    #[derive(Default)]
    struct Captured {
        mail_from: Vec<String>,
        rcpt_to: Vec<String>,
        data: Option<String>,
    }

    async fn smtp_receiver() -> (u16, Arc<Mutex<Captured>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Captured::default()));
        let shared = captured.clone();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            write.write_all(b"220 localhost ESMTP\r\n").await.unwrap();
            let mut data: Option<String> = None;
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(body) = data.as_mut() {
                    if line == "." {
                        shared.lock().unwrap().data = data.take();
                        write.write_all(b"250 queued\r\n").await.unwrap();
                    } else {
                        body.push_str(&line);
                        body.push_str("\r\n");
                    }
                    continue;
                }
                let upper = line.to_ascii_uppercase();
                let reply: &[u8] = if upper.starts_with("MAIL FROM:") {
                    shared.lock().unwrap().mail_from.push(line.clone());
                    b"250 ok\r\n"
                } else if upper.starts_with("RCPT TO:") {
                    shared.lock().unwrap().rcpt_to.push(line.clone());
                    b"250 ok\r\n"
                } else if upper == "DATA" {
                    data = Some(String::new());
                    b"354 go ahead\r\n"
                } else if upper == "QUIT" {
                    write.write_all(b"221 bye\r\n").await.unwrap();
                    break;
                } else {
                    b"250 localhost\r\n"
                };
                write.write_all(reply).await.unwrap();
            }
        });
        (port, captured)
    }

    fn channel(port: Option<u16>, to: &[&str]) -> Channel {
        Channel {
            id: "c1".to_string(),
            name: "mail".to_string(),
            kind: ChannelKind::Email,
            source: ChannelSource::Config,
            enabled: true,
            types: vec![AlertType::BackupFailed],
            settings: ChannelSettings {
                smtp_host: Some("127.0.0.1".to_string()),
                port,
                security: Some(SmtpMode::Starttls),
                from: Some("Lodestore <lodestore@example.test>".to_string()),
                to: to.iter().map(ToString::to_string).collect(),
                ..ChannelSettings::default()
            },
            secrets: ChannelSecrets::default(),
        }
    }

    fn backup_failed() -> AlertRow {
        AlertRow {
            kind: AlertType::BackupFailed,
            key: "backup_failed:h1:daily".to_string(),
            severity: "critical".to_string(),
            host_id: Some("h1".to_string()),
            hostname: Some("web-1".to_string()),
            repository_id: None,
            repository: None,
            policy: Some("daily".to_string()),
            check_level: None,
            cert: None,
            title: "Backup failed on web-1".to_string(),
            detail: "Policy daily: disk <full> & \"slow\".".to_string(),
            started_at: 1_000,
            resolved_at: Some(2_000),
        }
    }

    #[test]
    fn email_message_parts() {
        let channel = channel(None, &["ops@example.test"]);
        let firing = for_event(
            "https://lodestore.example.test/",
            &backup_failed(),
            MessageKind::Firing,
            0,
        );
        let raw = String::from_utf8(build(&channel, &firing).unwrap().formatted()).unwrap();
        assert!(raw.contains("Subject: [Lodestore] FIRING: Backup failed on web-1"));
        assert!(raw.contains("To: ops@example.test"));
        assert!(raw.contains("From: Lodestore <lodestore@example.test>"));
        assert!(raw.contains("Content-Type: multipart/alternative"));
        assert!(raw.contains("Content-Type: text/plain"));
        assert!(raw.contains("Content-Type: text/html"));
        assert!(raw.contains("Open in Lodestore: https://lodestore.example.test/hosts/h1"));
        assert!(html(&firing).contains("disk &lt;full&gt; &amp; &quot;slow&quot;."));
        assert!(!raw.contains("<img"));

        let resolved = for_event(
            "https://lodestore.example.test",
            &backup_failed(),
            MessageKind::Resolved,
            0,
        );
        let raw = String::from_utf8(build(&channel, &resolved).unwrap().formatted()).unwrap();
        assert!(raw.contains("Subject: [Lodestore] RESOLVED: Backup failed on web-1"));

        let none = Channel {
            settings: ChannelSettings {
                to: vec![],
                ..channel.settings.clone()
            },
            ..channel
        };
        assert_eq!(
            build(&none, &firing).unwrap_err().to_string(),
            "Enter a recipient or set an admin email in Settings, User."
        );
    }

    #[tokio::test]
    async fn email_delivered_over_smtp() {
        let (port, captured) = smtp_receiver().await;
        let channel = channel(Some(port), &["ops@example.test", "oncall@example.test"]);
        let message = for_event(
            "https://lodestore.example.test",
            &backup_failed(),
            MessageKind::Firing,
            0,
        );
        let email = build(&channel, &message).unwrap();
        let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(port)
            .timeout(Some(Duration::from_secs(5)))
            .build();
        deliver(&transport, email, &channel.secrets).await.unwrap();

        let captured = captured.lock().unwrap();
        assert_eq!(captured.mail_from.len(), 1);
        assert!(captured.mail_from[0].contains("lodestore@example.test"));
        assert_eq!(captured.rcpt_to.len(), 2);
        assert!(captured.rcpt_to[0].contains("ops@example.test"));
        assert!(captured.rcpt_to[1].contains("oncall@example.test"));
        let data = captured.data.as_deref().expect("a message body");
        assert!(data.contains("Subject: [Lodestore] FIRING: Backup failed on web-1"));
        assert!(data.contains("Content-Type: text/plain"));
        assert!(data.contains("Content-Type: text/html"));
        assert!(data.contains("FIRING: Backup failed on web-1"));
    }
}
