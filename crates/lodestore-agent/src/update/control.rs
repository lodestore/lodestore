//! What the session does with an `UpdateAssign`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lodestore_proto::agent::v1::agent_message::Body as AgentBody;
use lodestore_proto::agent::v1::{AgentMessage, UpdateAssign, UpdateState, UpdateStatus};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::UpdateOutcome;
use super::guard::{self, RollbackResult};
use crate::Paths;

pub const SWITCHING_WAIT: Duration = Duration::from_secs(5);
const EXIT_AFTER_SWITCH: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Ignore,
    Reply(UpdateStatus),
}

pub fn status(update_id: &str, state: UpdateState, reason: &str) -> UpdateStatus {
    UpdateStatus {
        update_id: update_id.to_string(),
        state: state.into(),
        reason: reason.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Downloading,
    Installing,
}

/// The one update this process is working on. It lives in `UpdateRuntime`, so
/// a reconnect sees an install the previous session left running.
#[derive(Debug, Default)]
pub struct Flight(Mutex<Option<Stage>>);

impl Flight {
    fn stage(&self) -> MutexGuard<'_, Option<Stage>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `false` means the download was cancelled. After `true` the task owes the
    /// sessions an outcome, so a session that ends can no longer cancel it.
    pub fn finish_download(&self) -> bool {
        let mut stage = self.stage();
        if *stage == Some(Stage::Downloading) {
            *stage = Some(Stage::Installing);
            return true;
        }
        false
    }

    /// `true` means nothing is installing and the caller may drop the download.
    pub fn cancel_download(&self) -> bool {
        let mut stage = self.stage();
        if *stage == Some(Stage::Downloading) {
            *stage = None;
            return true;
        }
        false
    }
}

pub struct UpdateControl {
    flight: Arc<Flight>,
    rolled_back: Option<RollbackResult>,
}

impl UpdateControl {
    /// The rollback mark is read from disk: it outlives the process until the
    /// server acknowledges it.
    pub fn load(paths: &Paths, flight: Arc<Flight>) -> UpdateControl {
        UpdateControl {
            flight,
            rolled_back: guard::rolled_back(paths),
        }
    }

    pub fn on_assign(&mut self, assign: &UpdateAssign, busy: bool) -> Action {
        if self.flight.stage().is_some() {
            return Action::Ignore;
        }
        if let Some(mark) = self
            .rolled_back
            .as_ref()
            .filter(|mark| mark.update_id == assign.update_id)
        {
            return Action::Reply(status(
                &mark.update_id,
                UpdateState::RolledBack,
                &mark.reason,
            ));
        }
        if busy {
            return Action::Reply(status(&assign.update_id, UpdateState::Busy, ""));
        }
        *self.flight.stage() = Some(Stage::Downloading);
        Action::Start
    }

    pub fn on_ack(&mut self, paths: &Paths, update_id: &str) {
        if let Err(e) = guard::clear_rollback(paths, update_id) {
            tracing::warn!(error = %e, "could not clear the rollback mark");
        }
        if self
            .rolled_back
            .as_ref()
            .is_some_and(|m| m.update_id == update_id)
        {
            self.rolled_back = None;
        }
    }

    pub fn finish(&mut self) {
        *self.flight.stage() = None;
    }
}

/// Queues `status` and waits until the stream has taken it, but not longer
/// than `wait`: the process is about to exit.
pub async fn send_switching(
    outbound: &mpsc::Sender<AgentMessage>,
    status: UpdateStatus,
    wait: Duration,
) {
    let message = AgentMessage {
        body: Some(AgentBody::UpdateStatus(status)),
    };
    if outbound.send(message).await.is_err() {
        return;
    }
    let drained = async {
        while outbound.capacity() != outbound.max_capacity() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let _ = tokio::time::timeout(wait, drained).await;
}

/// Hands the outcome to the session that is up. After a switch the process
/// leaves even when no session takes it, so systemd starts the new binary;
/// a session that does gets the time to report the switch first.
pub async fn deliver(
    done: &mpsc::Sender<(String, UpdateOutcome)>,
    update_id: String,
    outcome: UpdateOutcome,
    shutdown: &CancellationToken,
) {
    let switched = outcome == UpdateOutcome::Switched;
    let _ = done.send((update_id, outcome)).await;
    if switched {
        tokio::time::sleep(EXIT_AFTER_SWITCH).await;
        shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(id: &str) -> UpdateAssign {
        UpdateAssign {
            update_id: id.to_string(),
            target_version: "99.0.0".into(),
            arch: "x86_64".into(),
            size: 10,
            sha256: String::new(),
        }
    }

    fn paths(dir: &tempfile::TempDir) -> Paths {
        Paths {
            config_dir: dir.path().join("etc"),
            state_dir: dir.path().join("var"),
        }
    }

    fn mark(paths: &Paths, id: &str, reason: &str) {
        std::fs::create_dir_all(paths.update_dir()).unwrap();
        let json = serde_json::json!({ "update_id": id, "reason": reason });
        std::fs::write(paths.update_dir().join("result.json"), json.to_string()).unwrap();
    }

    #[test]
    fn busy_host_answers_busy_before_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut control = UpdateControl::load(&paths(&dir), Arc::default());
        let action = control.on_assign(&assign("u1"), true);
        assert_eq!(action, Action::Reply(status("u1", UpdateState::Busy, "")));
        assert!(matches!(
            control.on_assign(&assign("u1"), false),
            Action::Start
        ));
    }

    #[test]
    fn second_assign_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut control = UpdateControl::load(&paths(&dir), Arc::default());
        assert_eq!(control.on_assign(&assign("u1"), false), Action::Start);
        assert_eq!(control.on_assign(&assign("u1"), false), Action::Ignore);
        assert_eq!(control.on_assign(&assign("u2"), true), Action::Ignore);
        control.finish();
        assert_eq!(control.on_assign(&assign("u2"), false), Action::Start);
    }

    #[test]
    fn rolled_back_update_is_not_installed_again() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        mark(&paths, "X", "did not connect");
        let mut control = UpdateControl::load(&paths, Arc::default());
        assert_eq!(
            control.on_assign(&assign("X"), false),
            Action::Reply(status("X", UpdateState::RolledBack, "did not connect"))
        );
        assert_eq!(control.on_assign(&assign("Y"), false), Action::Start);
    }

    #[test]
    fn rollback_marker_survives_restart_until_ack() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        mark(&paths, "X", "did not connect");
        let mut control = UpdateControl::load(&paths, Arc::default());
        assert!(matches!(
            control.on_assign(&assign("X"), false),
            Action::Reply(_)
        ));

        let mut control = UpdateControl::load(&paths, Arc::default());
        assert!(matches!(
            control.on_assign(&assign("X"), false),
            Action::Reply(_)
        ));
        control.on_ack(&paths, "Y");
        assert!(paths.update_dir().join("result.json").exists());
        control.on_ack(&paths, "X");
        assert!(!paths.update_dir().join("result.json").exists());
        assert_eq!(control.on_assign(&assign("X"), false), Action::Start);
    }

    #[test]
    fn a_new_session_does_not_start_over_a_running_install() {
        let dir = tempfile::tempdir().unwrap();
        let flight = Arc::<Flight>::default();
        let mut first = UpdateControl::load(&paths(&dir), Arc::clone(&flight));
        assert_eq!(first.on_assign(&assign("u1"), false), Action::Start);
        assert!(flight.finish_download());
        assert!(!flight.cancel_download());

        let mut second = UpdateControl::load(&paths(&dir), Arc::clone(&flight));
        assert_eq!(second.on_assign(&assign("u1"), false), Action::Ignore);
        assert_eq!(second.on_assign(&assign("u2"), false), Action::Ignore);
        second.finish();
        assert_eq!(second.on_assign(&assign("u2"), false), Action::Start);
    }

    #[test]
    fn a_cancelled_download_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let flight = Arc::<Flight>::default();
        let mut control = UpdateControl::load(&paths(&dir), Arc::clone(&flight));
        assert_eq!(control.on_assign(&assign("u1"), false), Action::Start);
        assert!(flight.cancel_download());
        assert!(!flight.finish_download());
        assert_eq!(control.on_assign(&assign("u1"), false), Action::Start);
    }

    #[tokio::test(start_paused = true)]
    async fn a_switch_ends_the_process_when_no_session_reads_it() {
        let (done, _unread) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        deliver(&done, "u1".into(), UpdateOutcome::Switched, &shutdown).await;
        assert!(shutdown.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_update_leaves_the_process_running() {
        let (done, mut outcomes) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let outcome = UpdateOutcome::Rejected("no".into());
        deliver(&done, "u1".into(), outcome.clone(), &shutdown).await;
        assert!(!shutdown.is_cancelled());
        assert_eq!(outcomes.recv().await, Some(("u1".to_string(), outcome)));
    }

    #[tokio::test(start_paused = true)]
    async fn switching_waits_up_to_5s_for_send() {
        let (tx, mut rx) = mpsc::channel(8);
        let started = tokio::time::Instant::now();
        let message = status("u1", UpdateState::Switching, "");
        send_switching(&tx, message.clone(), SWITCHING_WAIT).await;
        assert!(started.elapsed() >= SWITCHING_WAIT);

        let (tx, mut reader) = mpsc::channel(8);
        let drain = tokio::spawn(async move { reader.recv().await });
        let started = tokio::time::Instant::now();
        send_switching(&tx, message, SWITCHING_WAIT).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        let sent = drain.await.unwrap().unwrap();
        assert!(matches!(sent.body, Some(AgentBody::UpdateStatus(s)) if s.update_id == "u1"));
        assert!(rx.try_recv().is_ok());
    }
}
