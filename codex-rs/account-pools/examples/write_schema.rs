//! Generate the user-only accounts.json schema on stdout.
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let schema = schemars::schema_for!(codex_protocol::account_pool::AccountPoolConfig);
    let mut output = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &schema)?;
    writeln!(output)?;
    Ok(())
}
