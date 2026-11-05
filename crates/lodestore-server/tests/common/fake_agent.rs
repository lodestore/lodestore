use std::time::Duration;

use lodestore_agent::{NetOptions, Paths, connect, tls};
use lodestore_proto::PROTOCOL_VERSION;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::enrollment_client::EnrollmentClient;
use lodestore_proto::agent::v1::server_message::Body as ServerBody;
use lodestore_proto::agent::v1::{
    AgentMessage, GetTrustAnchorRequest, Hello, JobFinished, JobOutcome, JobReport, JobReportState,
    JobSpec, JobStats, ServerMessage, Versions,
};
use lodestore_server::db::now_ms;
use prost::Message;
use sqlx::AnyPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tonic::transport::Channel;

use super::TestServer;

pub struct Host {
    pub id: String,
    pub paths: Paths,
}

pub async fn channel(ts: &TestServer, paths: &Paths) -> Channel {
    let address = ts.grpc_address();
    let bootstrap = connect::channel(
        &address,
        "127.0.0.1",
        tls::bootstrap_config().unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("bootstrap channel");
    let ca_pem = EnrollmentClient::new(bootstrap)
        .get_trust_anchor(GetTrustAnchorRequest {})
        .await
        .expect("trust anchor")
        .into_inner()
        .ca_pem;
    let cert = std::fs::read_to_string(paths.cert()).unwrap();
    let key = std::fs::read_to_string(paths.key()).unwrap();
    connect::channel(
        &address,
        "127.0.0.1",
        tls::pinned_config(&ca_pem, Some((cert.as_str(), key.as_str()))).unwrap(),
        &NetOptions::default(),
    )
    .await
    .expect("agent channel")
}

pub struct Fake {
    pub tx: mpsc::Sender<AgentMessage>,
    pub rx: Streaming<ServerMessage>,
}

impl Fake {
    pub async fn connect(ts: &TestServer, host: &Host, jobs: Vec<JobReport>) -> Fake {
        let (tx, rx) = mpsc::channel(64);
        Fake::open(ts, host, tx, rx, jobs).await
    }

    pub async fn connect_with(ts: &TestServer, host: &Host, capabilities: &[&str]) -> Fake {
        let (tx, rx) = mpsc::channel(64);
        Fake::open_with(ts, host, tx, rx, vec![], capabilities).await
    }

    pub async fn open(
        ts: &TestServer,
        host: &Host,
        tx: mpsc::Sender<AgentMessage>,
        rx: mpsc::Receiver<AgentMessage>,
        jobs: Vec<JobReport>,
    ) -> Fake {
        Fake::open_with(ts, host, tx, rx, jobs, &[]).await
    }

    pub async fn open_with(
        ts: &TestServer,
        host: &Host,
        tx: mpsc::Sender<AgentMessage>,
        rx: mpsc::Receiver<AgentMessage>,
        jobs: Vec<JobReport>,
        capabilities: &[&str],
    ) -> Fake {
        tx.send(AgentMessage {
            body: Some(AgentBody::Hello(Hello {
                versions: Some(Versions {
                    min: PROTOCOL_VERSION,
                    max: PROTOCOL_VERSION,
                }),
                agent_version: "test".to_string(),
                jobs,
                capabilities: capabilities.iter().map(|c| (*c).to_string()).collect(),
            })),
        })
        .await
        .unwrap();
        let inbound = AgentClient::new(channel(ts, &host.paths).await)
            .session(ReceiverStream::new(rx))
            .await
            .expect("session")
            .into_inner();
        Fake { tx, rx: inbound }
    }

    pub async fn send(&self, body: AgentBody) {
        self.tx
            .send(AgentMessage { body: Some(body) })
            .await
            .unwrap();
    }

    pub async fn recv<T>(&mut self, pick: impl Fn(ServerBody) -> Option<T>) -> T {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(15), self.rx.message())
                .await
                .expect("timed out waiting for a server message")
                .expect("stream error")
                .expect("the server closed the stream");
            if let Some(found) = message.body.and_then(&pick) {
                return found;
            }
        }
    }

    pub async fn drain(&mut self, window: Duration) -> Vec<ServerBody> {
        let deadline = tokio::time::Instant::now() + window;
        let mut out = Vec::new();
        while let Ok(Ok(Some(message))) = tokio::time::timeout_at(deadline, self.rx.message()).await
        {
            out.extend(message.body);
        }
        out
    }
}

pub fn spec() -> JobSpec {
    JobSpec {
        policy_name: "etc".to_string(),
        paths: vec!["/etc".to_string()],
        ..Default::default()
    }
}

pub async fn insert_job(pool: &AnyPool, id: &str, host: &str, source: &str, state: &str, at: i64) {
    sqlx::query(
        "INSERT INTO jobs (id, host_id, source, state, spec, queued_at, assigned_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $6)",
    )
    .bind(id)
    .bind(host)
    .bind(source)
    .bind(state)
    .bind(spec().encode_to_vec())
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
}

pub fn finished(job_id: &str, outcome: JobOutcome, snapshot: &str) -> JobFinished {
    JobFinished {
        job_id: job_id.to_string(),
        outcome: outcome as i32,
        snapshot_id: snapshot.to_string(),
        stats: Some(JobStats {
            files_new: 1,
            ..Default::default()
        }),
        finished_at_ms: now_ms(),
        ..Default::default()
    }
}

pub fn report(job_id: &str, state: JobReportState) -> JobReport {
    JobReport {
        job_id: job_id.to_string(),
        state: state as i32,
        ..Default::default()
    }
}

pub fn is_assign(body: ServerBody) -> Option<String> {
    match body {
        ServerBody::JobAssign(a) => Some(a.job_id),
        _ => None,
    }
}

pub fn is_ack(body: ServerBody) -> Option<String> {
    match body {
        ServerBody::JobFinishedAck(a) => Some(a.job_id),
        _ => None,
    }
}
