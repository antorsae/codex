//! Native account and pool commands. Credentials never enter command output.

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use codex_account_pools::AccountLogin;
use codex_account_pools::AccountStore;
use codex_account_pools::ManagedBackend;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::AccountSelection;
use codex_utils_cli::CliConfigOverrides;

#[derive(Debug, Parser)]
pub struct AccountCli {
    #[command(subcommand)]
    action: AccountCommand,
    #[arg(long, global = true)]
    json: bool,
    /// Refresh read-only output every 60 seconds until interrupted.
    #[arg(long, global = true)]
    watch: bool,
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Sign in to another ChatGPT subscription without replacing the current login.
    Add {
        alias: String,
        #[arg(long)]
        device_auth: bool,
    },
    /// Import the current stored ChatGPT login.
    Import {
        alias: String,
    },
    List,
    /// Set the default account for future sessions, or clear explicit defaults.
    Select {
        alias: Option<String>,
        #[arg(long, conflicts_with = "alias")]
        clear: bool,
    },
    Remove {
        alias: String,
    },
    Usage {
        alias: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Explicitly use an existing banked reset; never purchases credits.
    Redeem {
        alias: String,
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Parser)]
pub struct PoolCli {
    #[command(subcommand)]
    action: PoolCommand,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    watch: bool,
}

#[derive(Debug, Subcommand)]
enum PoolCommand {
    Create(PoolDefinition),
    List,
    Inspect {
        name: String,
    },
    /// Replace the ordered membership and weekly reset policy.
    Update(PoolDefinition),
    /// Select a default pool for future sessions.
    Select {
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        clear: bool,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, clap::Args)]
struct PoolDefinition {
    name: String,
    #[arg(required = true, num_args = 1.., value_delimiter = ',')]
    accounts: Vec<String>,
    /// Wait for weekly recovery instead of automatically spending banked resets.
    #[arg(long)]
    no_auto_reset: bool,
}

impl From<PoolDefinition> for AccountPool {
    fn from(value: PoolDefinition) -> Self {
        Self {
            name: value.name,
            accounts: value.accounts,
            redeem_weekly_resets: !value.no_auto_reset,
        }
    }
}

async fn store(overrides: CliConfigOverrides) -> Result<AccountStore> {
    let config = codex_core::config::Config::load_with_cli_overrides(
        overrides.parse_overrides().map_err(anyhow::Error::msg)?,
    )
    .await?;
    Ok(AccountStore::from_config(&config))
}

impl AccountCli {
    pub async fn run(self, overrides: CliConfigOverrides) -> Result<()> {
        let store = store(overrides).await?;
        if self.watch
            && !matches!(
                self.action,
                AccountCommand::List | AccountCommand::Usage { .. }
            )
        {
            anyhow::bail!("--watch is available for list and usage only");
        }
        match self.action {
            AccountCommand::Add { alias, device_auth } => {
                let login = AccountLogin::new(store, alias)?;
                if device_auth {
                    codex_login::run_device_code_login(login.options()).await?;
                } else {
                    let server = codex_login::run_login_server(login.options())?;
                    eprintln!("Complete sign-in in your browser: {}", server.auth_url);
                    let cancel = server.cancel_handle();
                    tokio::select! {
                        result = server.block_until_done() => result?,
                        _ = tokio::signal::ctrl_c() => { cancel.shutdown(); return Ok(()); }
                    }
                }
                output(&login.finish().await?, self.json)?;
            }
            AccountCommand::Import { alias } => {
                let home = store
                    .root()
                    .parent()
                    .context("Missing Codex home")?
                    .to_path_buf();
                output(&store.import(&alias, &home).await?, self.json)?;
            }
            AccountCommand::Select { alias, clear } => {
                if alias.is_none() && !clear {
                    anyhow::bail!("Provide an account alias or --clear");
                }
                store
                    .select_default(alias.map(AccountSelection::Account))
                    .await?;
                output(&store.read()?, self.json)?;
            }
            AccountCommand::Remove { alias } => {
                store.remove_account(&alias).await?;
                output(&store.read()?, self.json)?;
            }
            AccountCommand::Redeem { alias, model } => {
                output(&store.redeem(&alias, model.as_deref()).await?, self.json)?;
            }
            AccountCommand::List => loop {
                output(&store.read()?, self.json)?;
                if !self.watch || interrupted().await {
                    break;
                }
            },
            AccountCommand::Usage { alias, model } => loop {
                let mut accounts = store.read()?.accounts;
                if let Some(alias) = &alias {
                    accounts.retain(|account| &account.alias == alias);
                    if accounts.is_empty() {
                        anyhow::bail!("Unknown account: {alias}");
                    }
                }
                let backend = ManagedBackend::new(store.clone());
                let mut usage = Vec::new();
                for account in &accounts {
                    tokio::select! {
                        value = backend.usage(account, model.as_deref()) => usage.push(value),
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                    }
                }
                if self.json {
                    output(&usage, /*json*/ true)?;
                } else {
                    for account in usage {
                        println!("{}", account.display_summary());
                    }
                }
                if !self.watch || interrupted().await {
                    break;
                }
            },
        }
        Ok(())
    }
}

impl PoolCli {
    pub async fn run(self, overrides: CliConfigOverrides) -> Result<()> {
        let store = store(overrides).await?;
        if self.watch && !matches!(self.action, PoolCommand::List | PoolCommand::Inspect { .. }) {
            anyhow::bail!("--watch is available for list and inspect only");
        }
        match self.action {
            PoolCommand::Create(definition) => {
                store.put_pool(definition.into(), /*create*/ true).await?
            }
            PoolCommand::Update(definition) => {
                store.put_pool(definition.into(), /*create*/ false).await?
            }
            PoolCommand::Select { name, clear } => {
                if name.is_none() && !clear {
                    anyhow::bail!("Provide a pool name or --clear");
                }
                store
                    .select_default(name.map(AccountSelection::Pool))
                    .await?;
            }
            PoolCommand::Remove { name } => store.remove_pool(&name).await?,
            PoolCommand::Inspect { name } => {
                loop {
                    let pool = store
                        .read()?
                        .pools
                        .into_iter()
                        .find(|pool| pool.name == name)
                        .context("Unknown pool")?;
                    output(&pool, self.json)?;
                    if !self.watch || interrupted().await {
                        break;
                    }
                }
                return Ok(());
            }
            PoolCommand::List => {}
        }
        loop {
            output(&store.read()?, self.json)?;
            if !self.watch || interrupted().await {
                break;
            }
        }
        Ok(())
    }
}

fn output(value: &impl serde::Serialize, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

async fn interrupted() -> bool {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => true,
        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => false,
    }
}
