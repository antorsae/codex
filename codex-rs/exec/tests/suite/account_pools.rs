use anyhow::Result;
use core_test_support::account_pools;
use core_test_support::account_pools::QuotaFailure;
use core_test_support::test_codex_exec::test_codex_exec;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::MockServer;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_exec_continues_after_quota_and_emits_switch_json() -> Result<()> {
    let backend = MockServer::start().await;
    let home = TempDir::new()?;
    account_pools::setup(&home, &backend).await?;
    let requests = account_pools::mount_recovery(&backend, QuotaFailure::Http).await;
    let fixture = test_codex_exec();
    let result = fixture
        .cmd_with_server(&backend)
        .env("CODEX_HOME", home.path())
        .env("CODEX_SQLITE_HOME", home.path())
        .args([
            "--pool",
            "work",
            "--skip-git-repo-check",
            "--json",
            "--dangerously-bypass-approvals-and-sandbox",
        ])
        .args(["-c", "cli_auth_credentials_store=\"file\""])
        .arg("-c")
        .arg(format!("chatgpt_base_url={:?}", backend.uri()))
        .args(["-m", "gpt-5.5", "Check once and finish."])
        .timeout(std::time::Duration::from_secs(45))
        .assert()
        .success()
        .get_output()
        .clone();
    let events: Vec<Value> = String::from_utf8(result.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "account.pool.updated"
                && event["event"]["type"] == "switched"
                && event["event"]["account"] == "b")
    );
    assert!(events.iter().any(|event| event["type"] == "turn.completed"));
    account_pools::assert_recovery(&requests);
    Ok(())
}
