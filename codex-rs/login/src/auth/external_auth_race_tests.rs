use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
enum PendingOperation {
    Reload,
    GuardedReload,
    Refresh,
}

#[derive(Clone, Copy, Debug)]
enum PendingOutcome {
    Success,
    TransientFailure,
    PermanentFailure,
}

#[derive(Clone, Copy, Debug)]
enum Replacement {
    Install,
    Reinstall,
    Clear,
}

struct PausedExternalAuth {
    auth: CodexAuth,
    outcome: PendingOutcome,
    pause_next: AtomicBool,
    entered: Notify,
    resume: Notify,
}

impl PausedExternalAuth {
    async fn response(&self) -> std::io::Result<CodexAuth> {
        if self.pause_next.swap(/*val*/ false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.resume.notified().await;
            match self.outcome {
                PendingOutcome::Success => {}
                PendingOutcome::TransientFailure | PendingOutcome::PermanentFailure => {
                    return Err(std::io::Error::other("old provider failed"));
                }
            }
        }
        Ok(self.auth.clone())
    }
}

impl ExternalAuth for PausedExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(self.response())
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(self.response())
    }

    fn classify_error(&self, error: std::io::Error) -> RefreshTokenError {
        match self.outcome {
            PendingOutcome::PermanentFailure => permanent_external_auth_error(error.to_string()),
            PendingOutcome::Success | PendingOutcome::TransientFailure => {
                RefreshTokenError::Transient(error)
            }
        }
    }
}

fn provider(account: &'static str, outcome: PendingOutcome) -> Arc<PausedExternalAuth> {
    let headers = http::HeaderMap::from_iter([
        (
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer external"),
        ),
        (
            http::HeaderName::from_static("chatgpt-account-id"),
            http::HeaderValue::from_static(account),
        ),
    ]);
    Arc::new(PausedExternalAuth {
        auth: CodexAuth::Headers(AuthHeaders::new(headers)),
        outcome,
        pause_next: AtomicBool::new(/*v*/ false),
        entered: Notify::new(),
        resume: Notify::new(),
    })
}

#[tokio::test]
async fn superseded_external_auth_cannot_publish_credentials_or_failures() {
    for operation in [
        PendingOperation::Reload,
        PendingOperation::GuardedReload,
        PendingOperation::Refresh,
    ] {
        for outcome in [
            PendingOutcome::Success,
            PendingOutcome::TransientFailure,
            PendingOutcome::PermanentFailure,
        ] {
            for replacement in [
                Replacement::Install,
                Replacement::Reinstall,
                Replacement::Clear,
            ] {
                let manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("seed"));
                let old = provider("workspace-a", outcome);
                manager.set_external_auth(old.clone()).await.unwrap();
                old.pause_next.store(/*val*/ true, Ordering::SeqCst);
                let pending_manager = manager.clone();
                let pending = tokio::spawn(async move {
                    match operation {
                        PendingOperation::Reload => {
                            assert!(!pending_manager.reload().await);
                        }
                        PendingOperation::GuardedReload => {
                            assert!(matches!(
                                pending_manager
                                    .reload_if_account_id_matches(Some("workspace-a"))
                                    .await,
                                ReloadOutcome::ReloadedChanged
                            ));
                        }
                        PendingOperation::Refresh => {
                            pending_manager
                                .refresh_token_from_authority()
                                .await
                                .unwrap();
                        }
                    }
                });
                old.entered.notified().await;
                let expected_auth = match replacement {
                    Replacement::Install => {
                        let new = provider("workspace-b", PendingOutcome::Success);
                        manager.set_external_auth(new.clone()).await.unwrap();
                        Some(new.auth.clone())
                    }
                    Replacement::Reinstall => {
                        manager.set_external_auth(old.clone()).await.unwrap();
                        Some(old.auth.clone())
                    }
                    Replacement::Clear => {
                        manager.clear_external_auth();
                        None
                    }
                };
                let changes = manager.auth_change_state_receiver();
                let expected_changes = *changes.borrow();
                old.resume.notify_one();
                pending.await.unwrap();
                assert_eq!(
                    (
                        manager.auth_cached(),
                        manager.refresh_failure_for_auth(&old.auth),
                        *changes.borrow(),
                    ),
                    (expected_auth, None, expected_changes),
                    "{operation:?}, {outcome:?}, {replacement:?}",
                );
            }
        }
    }
}
