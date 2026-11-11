//! The one server channel that restores and update downloads read per RPC.
//!
//! The session loop swaps it once a renewed certificate is on disk and before the next session opens, so a job
//! that started before the renew never presents the old certificate after the server has stopped accepting it.

use std::future::Future;
use std::sync::Mutex;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::identity::Paths;
use crate::{AgentError, NetOptions};

pub struct SharedChannel {
    tx: watch::Sender<Option<Channel>>,
    paths: Paths,
    net: Mutex<NetOptions>,
    // a channel keeps its connection task on the runtime that built it; jobs run on runtimes of their own
    runtime: Option<Handle>,
}

impl SharedChannel {
    pub fn new(paths: &Paths) -> Self {
        SharedChannel {
            tx: watch::channel(None).0,
            paths: paths.clone(),
            net: Mutex::new(NetOptions::default()),
            runtime: Handle::try_current().ok(),
        }
    }

    pub fn set_net(&self, net: NetOptions) {
        *self.net.lock().unwrap_or_else(|e| e.into_inner()) = net;
    }

    pub fn replace(&self, channel: Channel) {
        self.tx.send_replace(Some(channel));
    }

    pub async fn get(&self) -> Result<Channel, AgentError> {
        let current = self.tx.borrow().clone();
        match current {
            Some(channel) => Ok(channel),
            None => self.rebuild().await,
        }
    }

    /// A channel from the certificate on disk, which a renew has already replaced.
    pub async fn rebuild(&self) -> Result<Channel, AgentError> {
        let net = self.net.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let paths = self.paths.clone();
        let build = async move { crate::session::agent_channel(&paths, &net).await };
        let channel = match &self.runtime {
            Some(runtime) => runtime
                .spawn(build)
                .await
                .map_err(|e| AgentError::Connect(e.to_string()))??,
            None => build.await?,
        };
        self.replace(channel.clone());
        Ok(channel)
    }

    /// Runs one RPC on the current channel; when the server calls the certificate stale, the channel is rebuilt
    /// from disk once and the RPC repeated.
    pub async fn call<T, Fut>(&self, rpc: impl Fn(Channel) -> Fut) -> Result<T, Status>
    where
        Fut: Future<Output = Result<T, Status>>,
    {
        retry_stale(
            || async {
                let channel = self
                    .get()
                    .await
                    .map_err(|e| Status::unavailable(e.to_string()))?;
                rpc(channel).await
            },
            || async {
                if let Err(error) = self.rebuild().await {
                    tracing::warn!(%error, "could not rebuild the server channel");
                }
            },
        )
        .await
    }
}

pub(crate) fn is_stale(status: &Status) -> bool {
    status.code() == Code::PermissionDenied
        && status
            .metadata()
            .get(lodestore_proto::ERROR_METADATA_KEY)
            .and_then(|v| v.to_str().ok())
            == Some(lodestore_proto::ERROR_CERT_STALE)
}

pub(crate) async fn retry_stale<T, Fut, RebuildFut>(
    mut call: impl FnMut() -> Fut,
    mut rebuild: impl FnMut() -> RebuildFut,
) -> Result<T, Status>
where
    Fut: Future<Output = Result<T, Status>>,
    RebuildFut: Future<Output = ()>,
{
    match call().await {
        Err(status) if is_stale(&status) => {
            rebuild().await;
            call().await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn stale() -> Status {
        let mut status = Status::permission_denied("stale");
        status.metadata_mut().insert(
            lodestore_proto::ERROR_METADATA_KEY,
            lodestore_proto::ERROR_CERT_STALE.parse().unwrap(),
        );
        status
    }

    #[tokio::test]
    async fn stale_rpc_rebuilds_the_channel_once() {
        let (calls, rebuilds) = (Cell::new(0), Cell::new(0));
        let rebuild = || async { rebuilds.set(rebuilds.get() + 1) };

        let answers = [Err(stale()), Ok(7)];
        let result = retry_stale(
            || {
                calls.set(calls.get() + 1);
                let answer = answers[calls.get() - 1].clone();
                async move { answer }
            },
            rebuild,
        )
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!((calls.get(), rebuilds.get()), (2, 1));

        let (calls, rebuilds) = (Cell::new(0), Cell::new(0));
        let result: Result<u8, Status> = retry_stale(
            || {
                calls.set(calls.get() + 1);
                async { Err(stale()) }
            },
            || async { rebuilds.set(rebuilds.get() + 1) },
        )
        .await;
        assert!(is_stale(&result.unwrap_err()));
        assert_eq!((calls.get(), rebuilds.get()), (2, 1), "no third attempt");

        let (calls, rebuilds) = (Cell::new(0), Cell::new(0));
        let result: Result<u8, Status> = retry_stale(
            || {
                calls.set(calls.get() + 1);
                async { Err(Status::permission_denied("revoked")) }
            },
            || async { rebuilds.set(rebuilds.get() + 1) },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            (calls.get(), rebuilds.get()),
            (1, 0),
            "other errors are not retried"
        );
    }
}
