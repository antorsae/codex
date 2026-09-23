//! A real process exit releases locks but leaves the reset intent and backend state intact.

use crate::AccountBackend;
use crate::redemption::RedemptionMode;
use crate::tests::account;
use crate::tests::populated_store;
use crate::tests::store;
use crate::tests::usage;
use crate::tests::with_credits;
use codex_backend_client::ConsumeRateLimitResetCreditCode;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::path::PathBuf;

struct FileBackend {
    home: PathBuf,
    crash: bool,
}

impl AccountBackend for FileBackend {
    async fn usage(&self, account: &ManagedAccount, _model: Option<&str>) -> ManagedAccountUsage {
        let remaining = if self.home.join("usable").exists() {
            90.0
        } else {
            0.0
        };
        with_credits(
            usage(
                &account.alias,
                remaining,
                remaining,
                chrono::Utc::now().timestamp(),
            ),
            2,
        )
    }

    async fn redeem(
        &self,
        _account: &ManagedAccount,
        key: &str,
        credit: &str,
    ) -> anyhow::Result<ConsumeRateLimitResetCreditCode> {
        // The mock backend applies by idempotency key, even when the client dies without a response.
        let applied = self.home.join("applied.json");
        let value = (key.to_owned(), credit.to_owned());
        if applied.exists() {
            let prior: (String, String) = serde_json::from_slice(&std::fs::read(&applied)?)?;
            assert_eq!(prior, value);
        } else {
            crate::storage::write_json(&applied, &value)?;
        }
        let mut calls = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("calls"))?;
        writeln!(calls, "{key}:{credit}")?;
        calls.sync_all()?;
        if self.crash {
            std::process::exit(42);
        }
        std::fs::write(self.home.join("usable"), b"ready")?;
        Ok(ConsumeRateLimitResetCreditCode::AlreadyRedeemed)
    }
}

#[tokio::test]
async fn reset_recovers_after_process_death_and_concurrent_resumers() -> anyhow::Result<()> {
    const HOME_ENV: &str = "CODEX_POOL_TEST_RESET_HOME";
    if let Some(home) = std::env::var_os(HOME_ENV) {
        let home = PathBuf::from(home);
        let store = store(&home);
        let backend = FileBackend {
            home,
            crash: std::env::var_os("CODEX_POOL_TEST_RESET_CRASH").is_some(),
        };
        let value = crate::redemption::redeem(
            &store,
            &backend,
            &account("a"),
            Some("model"),
            "credit-0",
            RedemptionMode::Automatic,
        )
        .await?;
        assert_eq!(value.ordinary_usage_allowed, Some(true));
        return Ok(());
    }
    let (home, store) = populated_store().await;
    let child = |crash: bool| {
        let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "process_tests::reset_recovers_after_process_death_and_concurrent_resumers",
            "--nocapture",
        ])
        .env(HOME_ENV, home.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
        if crash {
            cmd.env("CODEX_POOL_TEST_RESET_CRASH", "1");
        }
        cmd
    };
    let failed = child(/*crash*/ true).output().await?;
    assert_eq!(failed.status.code(), Some(42));
    let intent: serde_json::Value = serde_json::from_slice(&std::fs::read(
        store.credential_home(&account("a")).join("redemption.json"),
    )?)?;
    assert_eq!(intent["completed"], false);
    let first = child(/*crash*/ false).spawn()?;
    let second = child(/*crash*/ false).spawn()?;
    for result in [
        first.wait_with_output().await?,
        second.wait_with_output().await?,
    ] {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let calls = std::fs::read_to_string(home.path().join("calls"))?;
    let calls: Vec<_> = calls.lines().collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    assert!(calls[0].starts_with(intent["key"].as_str().unwrap()));
    Ok(())
}
